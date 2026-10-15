//! Pure geometry for the continuous-scroll page layout and its tile grid.
//!
//! Layout coordinates are logical pixels relative to the top-left corner of the
//! scrollable content. Zoom is expressed in logical pixels per PDF point, so a
//! zoom of 1.0 shows a page at its physical size (72 points per inch).

use gpui::{Bounds, Pixels, Point, Size, point, px, size};
use kkpdf_zed::PageDimensions;
use serde::{Deserialize, Serialize};

/// Vertical space between consecutive pages.
pub const PAGE_GAP: Pixels = px(12.);
/// Space between the pages and the edges of the scrollable content.
pub const CONTENT_PADDING: Pixels = px(16.);

pub const MIN_ZOOM: f32 = 0.1;
pub const MAX_ZOOM: f32 = 16.0;
pub const ZOOM_STEP: f32 = 1.2;
/// Automatic zoom fits the page width, but never magnifies beyond this.
pub const AUTO_ZOOM_MAX: f32 = 1.5;

/// How the zoom level is chosen.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum ZoomMode {
    /// Fit the widest page to the viewport width, capped at [`AUTO_ZOOM_MAX`].
    Auto,
    /// Fit the widest page to the viewport width.
    FitWidth,
    /// Fit a whole page inside the viewport.
    FitPage,
    /// A fixed zoom level chosen by the user.
    Fixed(f32),
}

impl ZoomMode {
    /// Resolves the mode to a concrete zoom for a viewport of the given size.
    pub fn resolve(self, pages: &[PageDimensions], viewport: Size<Pixels>) -> f32 {
        let available = |extent: Pixels| f32::from(extent - CONTENT_PADDING * 2.).max(1.);
        let widest = pages.iter().map(|page| page.width).fold(0., f32::max);
        let tallest = pages.iter().map(|page| page.height).fold(0., f32::max);
        let zoom = match self {
            ZoomMode::Fixed(zoom) => zoom,
            _ if widest <= 0. || tallest <= 0. => 1.,
            ZoomMode::FitWidth => available(viewport.width) / widest,
            ZoomMode::Auto => (available(viewport.width) / widest).min(AUTO_ZOOM_MAX),
            ZoomMode::FitPage => {
                (available(viewport.width) / widest).min(available(viewport.height) / tallest)
            }
        };
        zoom.clamp(MIN_ZOOM, MAX_ZOOM)
    }
}

/// Positions of every page for one zoom level and viewport width.
#[derive(Debug, Clone, PartialEq)]
pub struct PageLayout {
    pub zoom: f32,
    pub pages: Vec<Bounds<Pixels>>,
    pub content_size: Size<Pixels>,
}

impl PageLayout {
    pub fn new(pages: &[PageDimensions], zoom: f32, viewport_width: Pixels) -> Self {
        let widest = pages
            .iter()
            .map(|page| px(page.width * zoom))
            .fold(Pixels::ZERO, Pixels::max);
        let content_width = (widest + CONTENT_PADDING * 2.).max(viewport_width);

        let mut y = CONTENT_PADDING;
        let page_bounds = pages
            .iter()
            .map(|page| {
                let page_size = size(px(page.width * zoom), px(page.height * zoom));
                let origin = point((content_width - page_size.width) / 2., y);
                y += page_size.height + PAGE_GAP;
                Bounds::new(origin, page_size)
            })
            .collect::<Vec<_>>();
        let content_height = if page_bounds.is_empty() {
            CONTENT_PADDING * 2.
        } else {
            y - PAGE_GAP + CONTENT_PADDING
        };

        Self {
            zoom,
            pages: page_bounds,
            content_size: size(content_width, content_height),
        }
    }

    /// Indices of pages that intersect the vertical range `top..bottom`.
    pub fn pages_in_range(&self, top: Pixels, bottom: Pixels) -> std::ops::Range<usize> {
        let start = self.pages.partition_point(|page| page.bottom() < top);
        let end = self.pages.partition_point(|page| page.top() <= bottom);
        start..end.max(start)
    }

    /// The page that a content-space y coordinate falls on, or the nearest one.
    pub fn page_at_y(&self, y: Pixels) -> usize {
        self.pages
            .partition_point(|page| page.bottom() + PAGE_GAP / 2. < y)
            .min(self.pages.len().saturating_sub(1))
    }

    /// Converts a content-space point into a page-relative anchor that survives relayout.
    pub fn anchor_at(&self, position: Point<Pixels>) -> Option<PageAnchor> {
        let page_index = self.page_at_y(position.y);
        let page = self.pages.get(page_index)?;
        Some(PageAnchor {
            page_index,
            x: f32::from(position.x - page.left()) / self.zoom,
            y: f32::from(position.y - page.top()) / self.zoom,
        })
    }

    /// Converts a page-relative anchor back into content space.
    pub fn position_of(&self, anchor: PageAnchor) -> Option<Point<Pixels>> {
        let page = self.pages.get(anchor.page_index)?;
        Some(point(
            page.left() + px(anchor.x * self.zoom),
            page.top() + px(anchor.y * self.zoom),
        ))
    }
}

/// A position expressed in PDF points relative to a page's top-left corner.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PageAnchor {
    pub page_index: usize,
    pub x: f32,
    pub y: f32,
}

/// Edge length of a raster tile in device pixels.
pub const TILE_SIZE: u32 = 1024;

/// One tile of a page rasterized at a particular device scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TileRect {
    pub column: u32,
    pub row: u32,
    /// Device-pixel rectangle of the tile within the full page raster.
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Size in device pixels of a page rasterized at `scale` device pixels per point.
pub fn raster_size(page: PageDimensions, scale: f32) -> (u32, u32) {
    (
        (page.width * scale).ceil().max(1.) as u32,
        (page.height * scale).ceil().max(1.) as u32,
    )
}

/// Tiles of a page raster that intersect `visible`, a rectangle in raster pixels.
pub fn tiles_intersecting(
    raster: (u32, u32),
    visible: Bounds<f32>,
) -> impl Iterator<Item = TileRect> {
    let (raster_width, raster_height) = raster;
    let columns = raster_width.div_ceil(TILE_SIZE);
    let rows = raster_height.div_ceil(TILE_SIZE);
    // Tile indices covering `start..end`, clamped to the raster's extent.
    let span = |start: f32, end: f32, extent: u32, count: u32| {
        let start = start.clamp(0., extent as f32);
        let end = end.clamp(0., extent as f32);
        if end <= start {
            return 0..0;
        }
        let first = (start / TILE_SIZE as f32).floor() as u32;
        let last = (end / TILE_SIZE as f32).ceil() as u32;
        first.min(count)..last.min(count)
    };
    let column_range = span(visible.left(), visible.right(), raster_width, columns);
    let row_range = span(visible.top(), visible.bottom(), raster_height, rows);
    row_range.flat_map(move |row| {
        column_range.clone().map(move |column| {
            let x = column * TILE_SIZE;
            let y = row * TILE_SIZE;
            TileRect {
                column,
                row,
                x,
                y,
                width: TILE_SIZE.min(raster_width - x),
                height: TILE_SIZE.min(raster_height - y),
            }
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn letter() -> PageDimensions {
        PageDimensions::new(612., 792.)
    }

    #[test]
    fn layout_stacks_and_centers_pages() {
        let pages = [letter(), PageDimensions::new(306., 396.)];
        let layout = PageLayout::new(&pages, 1., px(1000.));
        assert_eq!(layout.content_size.width, px(1000.));
        assert_eq!(layout.pages[0].origin, point(px(194.), CONTENT_PADDING));
        assert_eq!(layout.pages[1].top(), CONTENT_PADDING + px(792.) + PAGE_GAP);
        assert_eq!(layout.pages[1].left(), px(347.));
        assert_eq!(
            layout.content_size.height,
            CONTENT_PADDING * 2. + px(792. + 396.) + PAGE_GAP
        );
    }

    #[test]
    fn wide_pages_widen_the_content() {
        let layout = PageLayout::new(&[letter()], 2., px(500.));
        assert_eq!(layout.content_size.width, px(1224.) + CONTENT_PADDING * 2.);
        assert_eq!(layout.pages[0].left(), CONTENT_PADDING);
    }

    #[test]
    fn zoom_modes_resolve_against_the_viewport() {
        let pages = [letter()];
        let viewport = size(px(612. + 32.), px(396. + 32.));
        assert_eq!(ZoomMode::FitWidth.resolve(&pages, viewport), 1.);
        assert_eq!(ZoomMode::FitPage.resolve(&pages, viewport), 0.5);
        assert_eq!(ZoomMode::Fixed(3.).resolve(&pages, viewport), 3.);
        let huge = size(px(10_000.), px(10_000.));
        assert_eq!(ZoomMode::Auto.resolve(&pages, huge), AUTO_ZOOM_MAX);
        assert_eq!(ZoomMode::Fixed(1000.).resolve(&pages, huge), MAX_ZOOM);
    }

    #[test]
    fn visible_page_ranges() {
        let pages = vec![letter(); 5];
        let layout = PageLayout::new(&pages, 1., px(800.));
        assert_eq!(layout.pages_in_range(px(0.), px(100.)), 0..1);
        let second_top = layout.pages[1].top();
        // A range that lies entirely in the gap between pages touches neither.
        assert_eq!(
            layout.pages_in_range(second_top - px(8.), second_top - px(4.)),
            1..1
        );
        assert_eq!(
            layout.pages_in_range(second_top - PAGE_GAP - px(5.), second_top + px(5.)),
            0..2
        );
        assert_eq!(layout.pages_in_range(px(1e9), px(2e9)), 5..5);
        assert_eq!(layout.page_at_y(second_top + px(10.)), 1);
        assert_eq!(layout.page_at_y(px(1e9)), 4);
    }

    #[test]
    fn anchors_survive_zoom_changes() {
        let pages = vec![letter(); 3];
        let before = PageLayout::new(&pages, 1., px(800.));
        let position = before.pages[1].origin + point(px(100.), px(200.));
        let anchor = before.anchor_at(position).unwrap();
        assert_eq!((anchor.page_index, anchor.x, anchor.y), (1, 100., 200.));

        let after = PageLayout::new(&pages, 2., px(800.));
        assert_eq!(
            after.position_of(anchor).unwrap(),
            after.pages[1].origin + point(px(200.), px(400.))
        );
    }

    #[test]
    fn tile_grid_covers_only_visible_area() {
        let raster = (2500, 1500);
        let all: Vec<_> =
            tiles_intersecting(raster, Bounds::new(point(0., 0.), size(2500., 1500.))).collect();
        assert_eq!(all.len(), 6);
        let last = all.last().unwrap();
        assert_eq!(
            (last.x, last.y, last.width, last.height),
            (2048, 1024, 452, 476)
        );

        let corner: Vec<_> =
            tiles_intersecting(raster, Bounds::new(point(1100., 10.), size(100., 100.))).collect();
        assert_eq!(corner.len(), 1);
        assert_eq!((corner[0].column, corner[0].row), (1, 0));

        let outside: Vec<_> =
            tiles_intersecting(raster, Bounds::new(point(3000., 0.), size(10., 10.))).collect();
        assert!(outside.is_empty());
    }
}
