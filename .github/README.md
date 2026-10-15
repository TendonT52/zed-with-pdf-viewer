# Zed with PDF Viewer

[Zed](https://zed.dev), the fast collaborative code editor, with a built-in PDF viewer.

Zed opens images but not PDFs, so papers, exam sheets, datasheets, LaTeX and Typst output all had to live in a separate app.
This fork adds a native PDF tab to Zed.
Pages render with Google's Pdfium engine on a background thread, so scrolling and zooming stay smooth even in long documents.

It tracks upstream Zed: every new stable Zed release is picked up automatically, tested, and published here as a new build.

> This is an unofficial community fork.
> It is not affiliated with or endorsed by Zed Industries.
> For anything not related to PDFs, use [zed-industries/zed](https://github.com/zed-industries/zed).

## Features

- Opens `.pdf` files from the project panel, the file finder, or the command line like any other file.
- Continuous scrolling with sharp text at any zoom level, including Retina displays.
- Zoom with Cmd + scroll, trackpad pinch, or the toolbar, from 10% to 1600%, anchored at the cursor.
- Fit width, fit page, and actual size modes.
- Page navigation: previous and next page buttons, arrow keys, Page Up and Page Down, Space, and jumping to a typed page number (Ctrl+G).
- Live reload: when the PDF changes on disk, for example after LaTeX or Typst recompiles it, the view updates in place and keeps your position.
- Drag to pan, split panes, and session restore of the page and zoom you were at.
- Pages always show the document's original colors.

## Install on macOS

1. Download the DMG for your Mac from the [latest release](https://github.com/TendonT52/zed-with-pdf-viewer/releases/latest): `aarch64` for Apple Silicon, `x86_64` for Intel.
2. Open the DMG and drag **Zed with PDF Viewer** into Applications.
3. The app is not notarized by Apple, so macOS blocks the first launch.
   Run this once in Terminal:

   ```bash
   xattr -dr com.apple.quarantine "/Applications/Zed with PDF Viewer.app"
   ```

   Alternatively, try to open the app, then go to System Settings, Privacy & Security, and click **Open Anyway**.

The app installs next to the official Zed and does not replace it.
Both share your settings, keymaps, themes and extensions in `~/.config/zed`.
It does not auto-update itself; new versions appear on the releases page.

## Keyboard shortcuts

| Action | macOS | Linux and Windows |
| --- | --- | --- |
| Zoom in and out | Cmd + = and Cmd + - | Ctrl + = and Ctrl + - |
| Fit width | Cmd + 0 | Ctrl + 0 |
| Actual size | Cmd + 1 | Ctrl + 1 |
| Fit page | Cmd + Shift + 0 | Ctrl + Shift + 0 |
| Previous and next page | Left and Right | Left and Right |
| First and last page | Home and End, Cmd + Up and Cmd + Down | Home and End, Ctrl + Home and Ctrl + End |
| Go to page | Ctrl + G | Ctrl + G |
| Scroll | Up, Down, Page Up, Page Down, Space, Shift + Space | Same |

All actions are also in the command palette under `pdf viewer:`.

## Build from source

You need Rust (the version in `rust-toolchain.toml` is installed automatically by rustup) and, on macOS, the Xcode command line tools.

```bash
git clone --branch pdf-viewer https://github.com/TendonT52/zed-with-pdf-viewer.git
cd zed-with-pdf-viewer
script/pdf-viewer/fetch-pdfium
cargo run --features gpui_platform/runtime_shaders
```

`fetch-pdfium` downloads a checksum-verified Pdfium library into `~/.local/share/kkpdf-zed/lib`.
`runtime_shaders` is only needed when full Xcode with its Metal compiler is not installed.

To build the macOS app and DMG yourself, run `script/pdf-viewer/bundle-mac`.
The DMG is written to `target/pdf-viewer/`.

The viewer code is cross-platform, but only macOS builds are published and tested so far.

## How it stays current with Zed

The fork is upstream Zed plus a small, self-contained set of commits on the `pdf-viewer` branch.
A daily GitHub Actions job checks for a new stable Zed release, replays the PDF viewer commits onto it, runs the tests, and publishes a new build tagged `v<zed version>-pdf.1`.
When upstream changes collide with the patch, the job opens an issue instead of publishing.

Because the branch is rebased onto each release, its history is rewritten.
If you work from a clone, update it with `git fetch && git reset --hard origin/pdf-viewer`.

## Contributing

Bug reports and pull requests are welcome.
Read [crates/pdf_viewer/ARCHITECTURE.md](../crates/pdf_viewer/ARCHITECTURE.md) for how the viewer is wired into Zed and [crates/pdf_viewer/MAINTAINING.md](../crates/pdf_viewer/MAINTAINING.md) for testing, releases and upstream syncs.
Please keep changes inside the PDF viewer crates where possible, so that moving to new Zed releases stays easy.

## Credits and license

- [Zed](https://github.com/zed-industries/zed) by Zed Industries, licensed under GPL-3.0-or-later (see `LICENSE-GPL`), which this fork is distributed under as well.
- The PDF engine is a modified copy of [kkpdf-zed](https://github.com/kk376/kkpdf-zed) by Kushagra Kumar, licensed under Apache-2.0 or GPL-3.0-or-later.
  See [crates/kkpdf_zed/NOTICE.md](../crates/kkpdf_zed/NOTICE.md) for what changed.
- [Pdfium](https://pdfium.googlesource.com/pdfium/) by Google, licensed under BSD-3-Clause and Apache-2.0, via [pdfium-binaries](https://github.com/bblanchon/pdfium-binaries) and [pdfium-render](https://github.com/ajrcarey/pdfium-render).
