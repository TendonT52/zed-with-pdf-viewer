# PDF viewer architecture

This document explains how the PDF viewer is built and wired into Zed, so a new contributor or agent session can change it confidently.
For testing, releases and moving to new Zed releases, see [MAINTAINING.md](MAINTAINING.md).

## The fork in one picture

```
zed (upstream, untouched except for the wiring below)
 ├─ crates/zed ............... registers the viewer and its toolbar (3 lines)
 ├─ assets/keymaps/*.json .... "PdfViewer" key context bindings
 │
 ├─ crates/pdf_viewer ........ NEW: the editor integration (GPUI views, layout, tiles)
 │    └─ depends on ↓
 └─ crates/kkpdf_zed ......... NEW: vendored PDF engine (Pdfium bindings, worker thread)
          └─ loads at runtime ↓
      libpdfium.dylib / .so / .dll (Google Pdfium, downloaded by script/pdf-viewer/fetch-pdfium)
```

Everything PDF-specific lives in the two new crates.
The rest of Zed is touched only where registration is unavoidable, which keeps rebases onto new Zed releases cheap.

## Every change to upstream files

Keep this list current; it is exactly the surface that can conflict when moving to a new Zed release.

| File | Change |
| --- | --- |
| `Cargo.toml` | Workspace members `crates/kkpdf_zed` and `crates/pdf_viewer`; workspace dependencies `kkpdf-zed` and `pdf_viewer`. |
| `Cargo.lock` | Regenerated; kept in its own commit (see MAINTAINING.md). |
| `crates/zed/Cargo.toml` | Dependency on `pdf_viewer`; `pdf_viewer/test-support` in the `visual-tests` feature. |
| `crates/zed/src/main.rs` | `pdf_viewer::init(cx)` right after `image_viewer::init(cx)`. |
| `crates/zed/src/zed.rs` | Adds `PdfViewToolbarControls` to every pane toolbar, right after the image viewer's toolbar. |
| `crates/zed/src/visual_test_runner.rs` | `pdf_viewer::init`, writes `sample.pdf` into the test project, and `run_pdf_viewer_visual_tests`, selected with `VISUAL_TEST_ONLY=pdf_viewer`. |
| `assets/keymaps/default-{macos,linux,windows}.json` | One `"context": "PdfViewer"` block each, placed before the `ImageViewer` block. |
| `.github/README.md`, `.github/workflows/pdf_viewer_*.yml`, `script/pdf-viewer/*` | Fork-only files; upstream never has files at these paths. |

## How a PDF gets opened

1. The user opens a path (project panel, file finder, CLI).
   The workspace asks every registered `ProjectItem` type, most recently registered first, whether it can open the path.
   `pdf_viewer::init` registers `PdfView` after the image viewer, so it is asked before the text editor would try and fail on binary content.
2. `PdfItem::try_open` (`src/pdf_item.rs`) claims paths ending in `.pdf` (case-insensitive).
   In a local worktree it loads the bytes through `Worktree::load_binary_file`.
   In a remote (SSH, WSL, container) project it downloads the file with `Project::download_file` into a private temporary directory, waits until the project has written it with the size the remote worktree reports, reads it and deletes the directory.
   This uses only requests the official remote server already handles, so the fork's server stays identical to upstream.
3. The bytes go to the Pdfium worker (`kkpdf_zed::PdfWorker::open`), which parses the document once and keeps it open.
   It replies with a `DocumentInfo`: an id, the size of every page in points, and the title from the metadata.
4. The workspace builds a `PdfView` (`src/pdf_view.rs`) for the `PdfItem` and shows it in a tab.
   If anything fails (missing Pdfium library, corrupt file), `for_broken_project_item` shows Zed's standard `InvalidItemView` with the error.

`PdfItem` is shared by all views of the same file (split panes).
It subscribes to its worktree; when the file changes on disk it waits 150 ms for writes to settle, re-reads (or, in remote projects, downloads again) and re-parses it, swaps in the new `DocumentInfo`, bumps `generation`, closes the old document in the worker and emits `PdfItemEvent::Reloaded`.
The project tracks downloads by path, so a reload waits for the previous download of the file to finish before starting its own.
A file that fails to parse mid-write is ignored; the next filesystem event retries.

## The engine: `crates/kkpdf_zed`

The engine is a vendored copy of [kk376/kkpdf-zed](https://github.com/kk376/kkpdf-zed); `NOTICE.md` lists what changed.
The viewer only uses the `worker` module and `PageDimensions`.
The original modules (`view.rs`, `cache.rs`, `rasterizer.rs`, `pdfium/*`) are kept as upstream wrote them.

### Why a dedicated worker thread

Pdfium is not thread-safe, and the original engine re-parsed the whole file on every render.
`src/worker.rs` runs one long-lived thread, `pdfium-worker`, that owns:

- the `Pdfium` instance, bound once from the shared library by `pdfium::loader::discover_and_bind_pdfium`;
- every open document, keyed by `DocumentId`;
- per document, the eight most recently used loaded pages, so neighboring tiles of one page don't re-parse its content stream.

Callers talk to it through a mutex-protected queue and a condition variable.
Open and close jobs are served first and in order.
Render jobs are served newest first, because the newest request matches what is on screen now.
`PdfWorker::render` returns a `RenderTicket`; dropping it cancels the job if it has not started, which is how the viewer abandons tiles that scrolled away.
Results come back through a callback, which the viewer forwards into a `futures::channel::oneshot`.

### Tiles

A `TileRequest` asks for a rectangle of a page rendered at a scale in device pixels per PDF point.
The worker renders it with a transform matrix (`scale, 0, 0, scale, -x, -y`) into a bitmap the size of the tile, so memory per tile is bounded no matter how far the user zooms in.
Bitmaps come back in BGRA order, which is what GPUI's texture atlas expects, so no pixel swizzling happens anywhere.

Two Pdfium details matter here:

- `pdfium-render` reverses the byte order by default (to produce RGBA for the `image` crate); the worker turns that off with `set_reverse_byte_order(false)`.
- Pdfium cannot draw interactive form fields while a custom transform is applied.
  For documents with a form, each page is flattened in memory before its first tile is rendered, which bakes field appearances into the page.
  The file on disk is never modified.

### Finding Pdfium

`pdfium/loader.rs` searches, in order: `PDFIUM_LIB_PATH` (a file), `PDFIUM_LIB_DIR` (a directory), libraries shipped with the app (next to the executable, in `lib/`, or in a macOS bundle's `Contents/Frameworks`), the system linker path, `~/.local/share/kkpdf-zed/lib`, `~/.local/lib`, common system directories, and finally `crates/kkpdf_zed/lib` in debug builds.
Bundled copies win over system copies so a released app always uses the Pdfium version it was tested with.

## The viewer: `crates/pdf_viewer`

| File | Responsibility |
| --- | --- |
| `pdf_viewer.rs` | `init` registers the project item and serialization; re-exports. |
| `pdf_item.rs` | `PdfItem`: loading, file watching, reloading, `project::ProjectItem`. |
| `pdf_view.rs` | `PdfView`: actions, input handling, layout and tile scheduling, painting, `Item`, `ProjectItem`, `SerializableItem`. |
| `layout.rs` | Pure geometry: page positions, zoom modes, page anchors, the tile grid. Unit tested without GPUI windows. |
| `scroll.rs` | `PdfScrollHandle`, the scroll state shared with Zed's scrollbars. |
| `toolbar.rs` | `PdfViewToolbarControls`: page number and zoom editors, navigation and zoom buttons. |
| `persistence.rs` | The `pdf_viewers` table in Zed's workspace database. |
| `test_support.rs` | Generated PDFs for tests and visual tests (behind `test-support`). |
| `pdf_view_tests.rs` | GPUI tests that drive a real `PdfView` with the real Pdfium worker. |

### Coordinates and zoom

- Zoom is logical pixels per PDF point; 1.0 shows a page at its physical size.
- `PageLayout` stacks pages vertically with a 12 px gap and 16 px padding, centered horizontally, and widens the content when pages are wider than the viewport.
- `ZoomMode` is `Auto` (fit width, capped at 150%), `FitWidth`, `FitPage` or `Fixed(zoom)`, resolved against the viewport on every layout, so fit modes follow window resizes.
- Positions that must survive relayout (zoom, resize, reload, restore) are stored as a `PageAnchor`: a page index plus a point in that page's PDF points.
  `PdfView::pending_anchor` pairs an anchor with the viewport point it should end up under; it is applied after the next layout.
  This is how zooming keeps the point under the cursor fixed and how session restore returns to the right spot.
  A view resting at the very top stays at the top when the zoom changes, because the padding above the first page doesn't scale.

### Scrolling

The view does its own scrolling instead of using a GPUI `overflow_scroll` div, because Cmd + scroll and pinch must zoom rather than scroll, and because the content is virtual (only visible pages are painted).
`PdfScrollHandle` implements `ui::ScrollableHandle`, so Zed's standard `Scrollbars` draw and drag it; `tracked_entity` makes scrollbar drags re-render the view.
Offsets follow GPUI's convention: they become more negative as you scroll down or right.

### Rendering pipeline

All layout and scheduling happens in `PdfView::prepaint_pages`, called from the custom `PagesElement`'s `prepaint`, because that is where the element's real bounds and the window's scale factor are known.
Each frame it:

1. Resolves the zoom, recomputes the `PageLayout` when needed, updates the scroll handle, and applies a pending anchor.
2. Picks the raster zoom.
   While the zoom keeps changing (pinch, resize in fit mode) the raster zoom lags behind by 120 ms, so existing tiles are scaled on the GPU instead of re-rendered every frame.
   A timer re-renders the view once the zoom settles.
3. For every page within half a screen of the viewport, computes which 1024 × 1024 device-pixel tiles of the page raster intersect that region and requests the missing ones from the worker.
   Tiles are keyed by document generation, page, raster scale, column and row.
4. For visible pages, collects what to paint: placeholder tiles from other scales or document generations first (lowest resolution first), then current tiles on top.
   Placeholders stop being drawn once all current tiles of a page are present, which is what makes zoom and live reload flicker-free.
5. Cancels pending tiles that are no longer wanted and evicts the least recently used tiles once the cache exceeds 384 MB, calling `Window::drop_image` so the GPU atlas memory is released too.

`PagesElement::paint` draws, per page: a drop shadow, a white page, the tiles clipped to the page, and a 1 px border.
Page origins are snapped to device pixels, and neighboring tiles share edges computed from the same formula, so there are no seams.

### Toolbar, actions and keys

`PdfViewToolbarControls` follows the image viewer's toolbar pattern: it is a `ToolbarItemView` that shows itself (in `PrimaryRight`) only when the active item is a `PdfView`, and observes the view to stay current.
The page number and zoom percentage turn into small single-line editors when clicked.
`GoToPage` sets a flag on the view that the toolbar picks up to start editing the page number.
Buttons dispatch actions through the view's focus handle, so tooltips show the real keybindings.
Actions live in the `pdf_viewer` namespace; key bindings use the `PdfViewer` key context set on the view's root element.

### Persistence

`SerializableItem` stores the absolute path and a small JSON state (`zoom_mode` and the page anchor at the top of the viewport) in the `pdf_viewers` table.
The view emits `PdfViewEvent::StateChanged` 500 ms after the user stops scrolling or zooming, which tells the workspace to serialize.
Unknown JSON fields are ignored, so older saved states keep loading.

## Decisions and alternatives

- **Vendored engine instead of a git dependency.**
  The fork needs engine changes (the worker, loader order) and a fork build must not break when another repository changes.
  Upstreaming the worker to kk376/kkpdf-zed would let the vendored copy shrink back to the original.
- **Tiles instead of whole-page bitmaps.**
  A page at 400% on a Retina display is over 100 MB as one bitmap; tiles keep memory proportional to the screen.
- **Original colors only.**
  A theme-aware dark mode was prototyped and then dropped at the project owner's request, before the first commit; the viewer always shows pages as the author made them.
  The prototype recolored tiles in the worker: near-neutral pixels were mapped onto the theme's background-to-foreground gradient, colored pixels kept their hue while their perceived brightness was inverted, and pixels inside image objects (found with `PdfPage::objects`) were left alone.
  Mapping by HSL lightness instead of perceived brightness left dark blue headings unreadable.
- **Dev release channel for the app.**
  Release builds of this fork use Zed's `dev` channel so Zed's auto-updater never replaces them with stock Zed, with their own bundle identifier so they install next to the official app.

## Known limitations and ideas

- No text selection, copy, or search inside PDFs yet.
  The engine already extracts text with bounding boxes (`PdfiumEngine::extract_document_details`), which is the starting point.
- No outline or thumbnail sidebar, no clickable links.
- In remote projects every reload downloads the whole file again, and session restore doesn't reopen remote PDFs, because they have no local path to save.
- Only macOS builds are published; Linux and Windows builds compile the same code but are untested.
