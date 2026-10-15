use std::{cell::RefCell, rc::Rc};

use gpui::{Bounds, Pixels, Point, Size, point};
use ui::ScrollableHandle;

#[derive(Debug, Default)]
struct ScrollState {
    /// Distance from the viewport's top-left to the content's top-left. Becomes
    /// more negative as the user scrolls right or down, like [`gpui::ScrollHandle`].
    offset: Point<Pixels>,
    viewport: Bounds<Pixels>,
    content_size: Size<Pixels>,
}

impl ScrollState {
    fn max_offset(&self) -> Point<Pixels> {
        point(
            (self.content_size.width - self.viewport.size.width).max(Pixels::ZERO),
            (self.content_size.height - self.viewport.size.height).max(Pixels::ZERO),
        )
    }

    fn clamp(&self, offset: Point<Pixels>) -> Point<Pixels> {
        let max = self.max_offset();
        point(
            offset.x.clamp(-max.x, Pixels::ZERO),
            offset.y.clamp(-max.y, Pixels::ZERO),
        )
    }
}

/// Scroll state of the page canvas, shared with Zed's scrollbars.
#[derive(Debug, Clone, Default)]
pub struct PdfScrollHandle(Rc<RefCell<ScrollState>>);

impl PdfScrollHandle {
    /// Top-left of the visible content, in content coordinates.
    pub fn scroll_position(&self) -> Point<Pixels> {
        -self.0.borrow().offset
    }

    pub fn set_scroll_position(&self, position: Point<Pixels>) {
        self.set_offset(-position);
    }

    pub fn scroll_by(&self, delta: Point<Pixels>) {
        let offset = self.0.borrow().offset + delta;
        self.set_offset(offset);
    }

    pub fn viewport_bounds(&self) -> Bounds<Pixels> {
        self.0.borrow().viewport
    }

    /// Updates the geometry after layout, keeping the offset within range.
    pub fn set_geometry(&self, viewport: Bounds<Pixels>, content_size: Size<Pixels>) {
        let mut state = self.0.borrow_mut();
        state.viewport = viewport;
        state.content_size = content_size;
        state.offset = state.clamp(state.offset);
    }
}

impl ScrollableHandle for PdfScrollHandle {
    fn max_offset(&self) -> Point<Pixels> {
        self.0.borrow().max_offset()
    }

    fn set_offset(&self, offset: Point<Pixels>) {
        let mut state = self.0.borrow_mut();
        state.offset = state.clamp(offset);
    }

    fn offset(&self) -> Point<Pixels> {
        self.0.borrow().offset
    }

    fn viewport(&self) -> Bounds<Pixels> {
        self.0.borrow().viewport
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{px, size};

    #[test]
    fn offsets_are_clamped_to_the_content() {
        let handle = PdfScrollHandle::default();
        handle.set_geometry(
            Bounds::new(point(px(0.), px(0.)), size(px(100.), px(100.))),
            size(px(300.), px(1000.)),
        );
        handle.scroll_by(point(px(-50.), px(-5000.)));
        assert_eq!(handle.scroll_position(), point(px(50.), px(900.)));
        handle.scroll_by(point(px(500.), px(500.)));
        assert_eq!(handle.scroll_position(), point(px(0.), px(400.)));

        // Shrinking the content pulls the offset back into range.
        handle.set_geometry(handle.viewport_bounds(), size(px(100.), px(200.)));
        assert_eq!(handle.scroll_position(), point(px(0.), px(100.)));
    }
}
