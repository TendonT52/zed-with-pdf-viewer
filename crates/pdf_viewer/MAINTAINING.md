# Maintaining the PDF viewer fork

How to develop, test, release, and keep the fork on the latest Zed release.
For how the code is structured, see [ARCHITECTURE.md](ARCHITECTURE.md).

## Repository layout

- `pdf-viewer` is the default branch: an upstream Zed release tag plus the fork's commits on top.
- `main` is upstream's main as of when the fork was made, and upstream tags are pushed as the fork moves to them; neither is modified.
- Release tags are `v<zed version>-pdf.<n>`, for example `v1.24.2-pdf.1`; `<n>` counts fork releases on the same Zed version.
- The fork's commits are kept few and grouped by purpose:
  1. "Add native PDF viewer crates": `crates/pdf_viewer`, `crates/kkpdf_zed`, root `Cargo.toml`.
  2. "Open PDF files in the built-in PDF viewer": `crates/zed` and keymaps.
  3. "Update Cargo.lock for the PDF viewer": only `Cargo.lock`.
  4. "Add fork docs, packaging and CI": `.github/README.md`, workflows, `script/pdf-viewer`, these docs.
  5. Later fixes, each as its own commit on top.

`script/pdf-viewer/upstream-version` prints the upstream tag the branch is based on.

## Development setup

```bash
script/pdf-viewer/fetch-pdfium          # Pdfium into ~/.local/share/kkpdf-zed/lib
cargo run --features gpui_platform/runtime_shaders
```

Notes:

- `runtime_shaders` is needed when full Xcode with its Metal compiler isn't installed (Command Line Tools alone are not enough).
- Zed's debug builds are large.
  Setting `CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0` roughly halves `target/`; the incremental cache alone can exceed 20 GB.
- If the checkout is on a stable release tag, `crates/zed/RELEASE_CHANNEL` says `stable`, and a debug build refuses to start while the official Zed is running ("zed is already running").
  Run it on the dev channel with its own data directory instead:

  ```bash
  ZED_RELEASE_CHANNEL=dev ./target/debug/zed --user-data-dir /tmp/zed-pdf-dev path/to/folder
  ```

## Tests

| What | Command |
| --- | --- |
| Engine and viewer unit and integration tests | `cargo test -p pdf_viewer -p kkpdf-zed` |
| Lints, as CI runs them | `cargo clippy -p pdf_viewer -p kkpdf-zed --all-targets -- --deny warnings` |
| Formatting | `cargo fmt -p pdf_viewer -p kkpdf-zed -- --check` |
| Visual scenarios (offscreen, real Metal rendering) | see below |

Tests that need Pdfium (the worker tests and the GPUI view tests) skip themselves with a message when the library isn't installed, so run `fetch-pdfium` first.
The GPUI view tests in `pdf_view_tests.rs` open a generated PDF in a real `PdfView`, call `executor().allow_parking()` because tiles come from the real worker thread, and draw frames until no tiles are pending.
The remote project tests run upstream's `HeadlessProject` in process behind `RemoteClient::fake_server`, as upstream's remote editing tests do; that dependency is why the tests enable `gpui_platform/runtime_shaders`.

Visual scenarios render a generated three-page document at automatic zoom, at 300%, and fitted to page 2, through Zed's visual test runner, without showing a window:

```bash
cargo build -p zed --bin zed_visual_test_runner --features visual-tests,gpui_platform/runtime_shaders
UPDATE_BASELINE=1 VISUAL_TEST_ONLY=pdf_viewer VISUAL_TEST_OUTPUT_DIR=target/pdf-viewer-visual \
  target/debug/zed_visual_test_runner
```

Screenshots land in `target/pdf-viewer-visual`; look at them after any rendering change.
Baselines are machine-specific and, as upstream does, not committed; run once with `UPDATE_BASELINE=1` and again without it to check that rendering is deterministic.
CI uploads the screenshots as the `pdf-viewer-screenshots` artifact.

When fixing a bug, reproduce it first as close to real use as possible (the running app, or a GPUI test that drives the view with simulated input), then add the regression test.

## Building the macOS app

```bash
script/pdf-viewer/bundle-mac            # host architecture
script/pdf-viewer/bundle-mac -i         # also install into /Applications
script/pdf-viewer/bundle-mac x86_64-apple-darwin
```

The script builds `zed` and `cli` in release mode on the dev channel, bundles them with Zed's fork of `cargo-bundle` under the name "Zed with PDF Viewer" and identifier `io.github.tendont52.zed-with-pdf-viewer`, copies Pdfium into `Contents/Frameworks`, adds the same `git` binary official builds ship, signs ad-hoc, and writes `target/pdf-viewer/Zed-with-PDF-Viewer-<release>-<arch>.dmg`.
`<release>` is the fork release, such as `1.24.2-pdf.3`, taken from `PDF_VIEWER_RELEASE_TAG` or the release tag on HEAD, and the Zed version when HEAD has no release tag.
Release profile overrides (no LTO, 16 codegen units, no debug info) keep the build within CI runners' memory and time; override the `CARGO_PROFILE_RELEASE_*` variables to change that.

The app is ad-hoc signed and not notarized, because notarization needs a paid Apple Developer account.
macOS therefore quarantines downloaded copies; users clear it with "Open Anyway" in System Settings, Privacy & Security, or with `xattr -dr com.apple.quarantine "/Applications/Zed with PDF Viewer.app"`.
On recent macOS that command fails with "Operation not permitted" until the terminal is allowed under Privacy & Security, App Management, because changing apps in /Applications is protected.
With a Developer ID, signing and notarizing would follow upstream's `script/bundle-mac`.

## Remote development

Dev-channel builds of Zed never download a remote server, so out of the box the fork could not open SSH, WSL or container projects.
`bundle-mac` sets `ZED_REMOTE_SERVER_RELEASE_CHANNEL=stable`, which makes the app install the official `zed-remote-server` of the Zed release it is built on, exactly as stock Zed does.
The only fork code involved is `remote_server_release` in `crates/remote/src/transport.rs` and its three call sites.
The server is named after the upstream version only (`zed-remote-server-stable-1.24.2`), so every fork build of one Zed release shares the server already installed on a host.

That server is only correct while the fork leaves every crate it is built from untouched.
`script/pdf-viewer/check-remote-server-parity` fails when a fork commit changes one of those crates, or pulls a dependency version into the server that upstream does not use.
CI, the release workflow and the upstream sync all run it.
If a future change has to touch the server, build and publish the fork's own `remote_server` with the release instead.

## Releasing

Push a tag:

```bash
git tag v1.24.2-pdf.2 && git push origin v1.24.2-pdf.2
```

`.github/workflows/pdf_viewer_release.yml` builds DMGs for Apple Silicon and Intel on GitHub's macOS runners (about one to two hours) and creates or updates the GitHub release.
It can also be started from the Actions tab with a tag name; if the tag doesn't exist yet it builds `pdf-viewer` and the release creates the tag.

## Moving to a new Zed release

### Automatically

`.github/workflows/pdf_viewer_upstream_sync.yml` runs daily.
When zed-industries/zed has a newer stable release (tags like `v1.25.0`, never `-pre`), it runs `script/pdf-viewer/sync-upstream`, which:

1. fetches the new tag and saves the current branch as `pdf-viewer-before-<tag>`;
2. replays the fork's commits from the old base onto the new tag (`git rebase --onto`);
3. resolves `Cargo.lock` conflicts by taking upstream's lockfile and letting `cargo metadata` add the fork's crates back;
4. stops, aborts the rebase and lists the files for any other conflict.

If the rebase succeeds, the workflow runs the tests and `cargo check -p zed`, pushes the upstream tag and the rewritten `pdf-viewer` branch, tags `v<new>-pdf.1`, and that tag starts the release workflow.
It then disables any upstream workflows that appeared, because only `pdf_viewer_*` workflows should run in the fork.
If the rebase conflicts, it opens an issue titled "Upstream vX.Y.Z: PDF viewer commits need a manual rebase" and publishes nothing.

### By hand

```bash
git switch pdf-viewer
script/pdf-viewer/sync-upstream            # or: script/pdf-viewer/sync-upstream v1.25.0
```

On exit code 2, redo the rebase interactively and fix the listed files:

```bash
git rebase --onto v1.25.0 "$(script/pdf-viewer/upstream-version)" pdf-viewer
# resolve, git add, git rebase --continue; for Cargo.lock: git checkout --ours Cargo.lock && cargo metadata >/dev/null && git add Cargo.lock
cargo test -p pdf_viewer -p kkpdf-zed && cargo check -p zed
git push --force-with-lease origin pdf-viewer
git push origin v1.25.0 && git tag v1.25.0-pdf.1 && git push origin v1.25.0-pdf.1
```

Typical conflicts and what usually changed upstream:

- `crates/zed/src/main.rs` or `zed.rs`: the image viewer registration moved; put the PDF viewer lines next to it again.
- `assets/keymaps/*.json`: the `ImageViewer` block moved or changed; keep the `PdfViewer` block before it.
- Compile errors in `pdf_viewer` after a clean rebase: a GPUI, workspace or project API changed.
  The image viewer (`crates/image_viewer`) implements the same traits and is the best guide to the new API.

## One-time repository setup

Done once when the fork was created; listed so it can be repeated or audited.

- Default branch: `pdf-viewer`.
- Actions: all upstream workflows disabled (`gh workflow disable <name>`); only `pdf_viewer_*` are active.
- Secret `PDF_VIEWER_SYNC_TOKEN` for the sync workflow: a fine-grained personal access token limited to this repository with read and write access to Contents, Issues and Workflows.
  GitHub's built-in token can't push history that changes `.github/workflows` (every upstream release does), and its pushes don't start other workflows such as the release.

## Agent notes

- The root `.rules` (linked as `AGENTS.md` and `CLAUDE.md`) is upstream's file for upstream contributors.
  Its "HARD RULE" about adding a review banner to `README.md` targets pull requests to zed-industries/zed; the fork does not apply it and doesn't edit `.rules`, to avoid rebase conflicts.
- Keep changes inside `crates/pdf_viewer` and `crates/kkpdf_zed` whenever possible, and update the "Every change to upstream files" table in ARCHITECTURE.md when touching anything else.
- Commit messages have no AI co-author trailers in this fork.
