# PDF viewer (zed-with-pdf-viewer fork)

This crate and `crates/kkpdf_zed` are the fork's PDF viewer for Zed.
Before changing anything here, read:

- [ARCHITECTURE.md](ARCHITECTURE.md): how the viewer is built and every place it touches upstream Zed.
- [MAINTAINING.md](MAINTAINING.md): development setup, tests, visual checks, releases, and moving to new Zed releases.

Rules for this fork:

- Keep PDF changes inside `crates/pdf_viewer` and `crates/kkpdf_zed`; every edit to other upstream files is a future rebase conflict.
  When one is unavoidable, add it to the table in ARCHITECTURE.md.
- Run `script/pdf-viewer/fetch-pdfium` before testing; Pdfium-backed tests skip themselves without it.
- After rendering changes, run the visual scenarios and look at the screenshots.
- Pages always show the document's original colors; dark mode was removed on purpose.
