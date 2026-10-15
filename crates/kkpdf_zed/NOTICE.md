# Vendored copy of kkpdf-zed

This crate is a vendored and modified copy of [kk376/kkpdf-zed](https://github.com/kk376/kkpdf-zed)
at commit `e4961e59595dcd8a91fe6dafbd5317fba906d771`, used under its Apache-2.0 OR GPL-3.0-or-later license.

Changes made for zed-with-pdf-viewer:

- `src/worker.rs` (new): a dedicated Pdfium thread that keeps documents parsed in memory and renders page tiles on demand, newest request first, with cancellation.
- `src/pdfium/loader.rs`: libraries bundled with the application (next to the executable, in `lib/`, or in a macOS bundle's `Contents/Frameworks`) take precedence over system-wide copies.
- `tests/worker_tests.rs` (new): integration tests for the worker against the real Pdfium library.

The original modules (`view.rs`, `cache.rs`, `rasterizer.rs`, and so on) are kept unchanged so the crate stays close to upstream.
