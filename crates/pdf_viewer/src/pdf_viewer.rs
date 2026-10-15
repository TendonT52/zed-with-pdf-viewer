//! Native PDF viewer: opens `.pdf` files in an editor tab, rendered by the
//! kkpdf-zed engine on a dedicated Pdfium worker thread.

mod layout;
mod pdf_item;
mod pdf_view;
mod persistence;
mod scroll;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
mod toolbar;

use gpui::App;

pub use layout::ZoomMode;
pub use pdf_item::{PdfItem, PdfItemEvent};
pub use pdf_view::*;
pub use toolbar::PdfViewToolbarControls;

pub fn init(cx: &mut App) {
    workspace::register_project_item::<PdfView>(cx);
    workspace::register_serializable_item::<PdfView>(cx);
}
