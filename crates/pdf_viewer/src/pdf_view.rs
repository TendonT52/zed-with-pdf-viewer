use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Context as _;
use editor::{EditorSettings, RevealInFileManager, items::entry_git_aware_label_color};
use file_icons::FileIcons;
use futures::channel::oneshot;
use gpui::{
    AnyElement, App, Bounds, BoxShadow, ContentMask, Context, Corners, Element, ElementId, Entity,
    EventEmitter, FocusHandle, Focusable, Font, GlobalElementId, Hsla, InspectorElementId,
    InteractiveElement, IntoElement, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement, PinchEvent, Pixels, Point, Render, RenderImage, ScrollWheelEvent,
    Style, Styled, Task, WeakEntity, Window, actions, div, fill, hsla, outline, point, px, size,
};
use image::{Frame, RgbaImage};
use kkpdf_zed::{PdfWorker, RenderTicket, RenderedTile, TileRequest};
use language::File as _;
use project::{Project, git_store::GitStoreEvent};
use serde::{Deserialize, Serialize};
use settings::Settings;
use smallvec::smallvec;
use theme_settings::ThemeSettings;
use ui::{ScrollAxes, Scrollbars, WithScrollbar, prelude::*};
use util::{ResultExt as _, paths::PathExt};
use workspace::{
    ItemId, ItemSettings, Pane, ToolbarItemLocation, Workspace, WorkspaceId, delete_unloaded_items,
    invalid_item_view::InvalidItemView,
    item::{HighlightedText, Item, ItemEvent, ProjectItem, SerializableItem, TabContentParams},
};

use crate::{
    layout::{
        MAX_ZOOM, MIN_ZOOM, PAGE_GAP, PageAnchor, PageLayout, TileRect, ZOOM_STEP, ZoomMode,
        raster_size, tiles_intersecting,
    },
    pdf_item::{PdfItem, PdfItemEvent},
    persistence::PdfViewerDb,
    scroll::PdfScrollHandle,
};

actions!(
    pdf_viewer,
    [
        /// Zoom in.
        ZoomIn,
        /// Zoom out.
        ZoomOut,
        /// Show pages at their physical size (100%).
        ActualSize,
        /// Fit the page width to the view.
        FitWidth,
        /// Fit a whole page inside the view.
        FitPage,
        /// Go to the next page.
        NextPage,
        /// Go to the previous page.
        PreviousPage,
        /// Go to the first page.
        FirstPage,
        /// Go to the last page.
        LastPage,
        /// Scroll up a little.
        ScrollUp,
        /// Scroll down a little.
        ScrollDown,
        /// Scroll up by one screen.
        ScrollPageUp,
        /// Scroll down by one screen.
        ScrollPageDown,
        /// Edit the current page number to jump to a page.
        GoToPage,
    ]
);

/// Distance scrolled by a single line of mouse wheel or arrow key.
const LINE_HEIGHT: Pixels = px(40.);
/// How long the zoom must stay unchanged before pages are re-rasterized at the
/// new resolution. Until then the existing tiles are scaled, keeping pinch and
/// resize smooth.
const RASTER_SETTLE: Duration = Duration::from_millis(120);
/// Upper bound on GPU memory spent on cached tiles.
const TILE_CACHE_BYTES: usize = 384 * 1024 * 1024;
/// Delay before persisting the scroll position after the user stops scrolling.
const SERIALIZE_DEBOUNCE: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct TileKey {
    generation: u64,
    page_index: usize,
    /// Raster scale in device pixels per point, multiplied by 1024.
    scale: u32,
    column: u32,
    row: u32,
}

struct CachedTile {
    image: Arc<RenderImage>,
    tile: TileRect,
    scale: f32,
    last_used: u64,
}

struct PendingTile {
    _ticket: RenderTicket,
    _task: Task<()>,
}

#[derive(Default)]
struct TileCache {
    tiles: HashMap<TileKey, CachedTile>,
    pending: HashMap<TileKey, PendingTile>,
    failed: HashSet<TileKey>,
    frame: u64,
}

impl TileCache {
    fn bytes(&self) -> usize {
        self.tiles
            .values()
            .map(|tile| tile.tile.width as usize * tile.tile.height as usize * 4)
            .sum()
    }

    fn clear(&mut self, window: &mut Window) {
        for (_, tile) in self.tiles.drain() {
            window.drop_image(tile.image).log_err();
        }
        self.pending.clear();
        self.failed.clear();
    }

    /// Evicts least recently used tiles that weren't drawn this frame.
    fn evict(&mut self, window: &mut Window) {
        let mut bytes = self.bytes();
        if bytes <= TILE_CACHE_BYTES {
            return;
        }
        let mut candidates: Vec<_> = self
            .tiles
            .iter()
            .filter(|(_, tile)| tile.last_used < self.frame)
            .map(|(key, tile)| (tile.last_used, *key))
            .collect();
        candidates.sort_unstable_by_key(|(last_used, _)| *last_used);
        for (_, key) in candidates {
            if bytes <= TILE_CACHE_BYTES {
                break;
            }
            if let Some(tile) = self.tiles.remove(&key) {
                bytes -= tile.tile.width as usize * tile.tile.height as usize * 4;
                window.drop_image(tile.image).log_err();
            }
        }
    }
}

/// View state persisted across restarts.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SerializedViewState {
    zoom_mode: ZoomMode,
    anchor: Option<PageAnchor>,
}

pub enum PdfViewEvent {
    TitleChanged,
    StateChanged,
}

pub struct PdfView {
    pdf_item: Entity<PdfItem>,
    project: Entity<Project>,
    focus_handle: FocusHandle,
    scroll: PdfScrollHandle,
    zoom_mode: ZoomMode,
    layout: Option<PageLayout>,
    layout_generation: u64,
    /// A page position to bring to a viewport-relative point after the next layout.
    pending_anchor: Option<(PageAnchor, Point<Pixels>)>,
    /// Zoom that tiles are currently rasterized for; trails the displayed zoom.
    raster_zoom: Option<f32>,
    zoom_changed_at: Instant,
    settle_task: Option<Task<()>>,
    serialize_task: Option<Task<()>>,
    drag_position: Option<Point<Pixels>>,
    tiles: TileCache,
    page_edit_requested: bool,
}

impl PdfView {
    pub fn new(
        pdf_item: Entity<PdfItem>,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.subscribe_in(&pdf_item, window, Self::on_pdf_item_event)
            .detach();
        let git_store = project.read(cx).git_store().clone();
        cx.subscribe(&git_store, |_, _, event, cx| {
            if matches!(event, GitStoreEvent::DiffBaseChanged(_)) {
                cx.emit(PdfViewEvent::TitleChanged);
            }
        })
        .detach();
        cx.on_release_in(window, |this, window, _| this.tiles.clear(window))
            .detach();

        Self {
            pdf_item,
            project,
            focus_handle: cx.focus_handle(),
            scroll: PdfScrollHandle::default(),
            zoom_mode: ZoomMode::Auto,
            layout: None,
            layout_generation: 0,
            pending_anchor: None,
            raster_zoom: None,
            zoom_changed_at: Instant::now(),
            settle_task: None,
            serialize_task: None,
            drag_position: None,
            tiles: TileCache::default(),
            page_edit_requested: false,
        }
    }

    fn on_pdf_item_event(
        &mut self,
        _: &Entity<PdfItem>,
        event: &PdfItemEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            PdfItemEvent::FileHandleChanged => cx.emit(PdfViewEvent::TitleChanged),
            // Old tiles stay cached and keep being drawn as placeholders until the
            // re-rendered tiles arrive, so a reload doesn't flash.
            PdfItemEvent::Reloaded => self.tiles.failed.clear(),
        }
        cx.notify();
    }

    /// True while page tiles are still being rasterized for the current view.
    pub fn is_rendering(&self) -> bool {
        !self.tiles.pending.is_empty()
    }

    pub fn page_count(&self, cx: &App) -> usize {
        self.pdf_item.read(cx).document().pages.len()
    }

    /// Zero-based index of the page at the center of the viewport.
    pub fn current_page(&self) -> usize {
        let Some(layout) = &self.layout else {
            return 0;
        };
        let viewport = self.scroll.viewport_bounds();
        layout.page_at_y(self.scroll.scroll_position().y + viewport.size.height / 2.)
    }

    pub fn zoom(&self) -> f32 {
        self.layout.as_ref().map_or(1., |layout| layout.zoom)
    }

    pub fn zoom_mode(&self) -> ZoomMode {
        self.zoom_mode
    }

    pub(crate) fn take_page_edit_request(&mut self) -> bool {
        std::mem::take(&mut self.page_edit_requested)
    }

    /// Captures what is at `viewport_point` so it stays there across a relayout.
    fn anchor_viewport_point(&mut self, viewport_point: Point<Pixels>) {
        if let Some(layout) = &self.layout
            && let Some(anchor) = layout.anchor_at(self.scroll.scroll_position() + viewport_point)
        {
            self.pending_anchor = Some((anchor, viewport_point));
        }
    }

    fn viewport_center(&self) -> Point<Pixels> {
        let viewport = self.scroll.viewport_bounds();
        point(viewport.size.width / 2., viewport.size.height / 2.)
    }

    pub fn set_zoom_mode(
        &mut self,
        zoom_mode: ZoomMode,
        window_point: Option<Point<Pixels>>,
        cx: &mut Context<Self>,
    ) {
        let zoom_mode = match zoom_mode {
            ZoomMode::Fixed(zoom) => ZoomMode::Fixed(zoom.clamp(MIN_ZOOM, MAX_ZOOM)),
            mode => mode,
        };
        if zoom_mode == self.zoom_mode {
            return;
        }
        let viewport = self.scroll.viewport_bounds();
        let viewport_point = window_point
            .map(|point| point - viewport.origin)
            .unwrap_or_else(|| self.viewport_center());
        self.anchor_viewport_point(viewport_point);
        self.zoom_mode = zoom_mode;
        self.state_changed(cx);
        cx.notify();
    }

    fn zoom_by(
        &mut self,
        factor: f32,
        window_point: Option<Point<Pixels>>,
        cx: &mut Context<Self>,
    ) {
        let zoom = self.zoom() * factor;
        self.set_zoom_mode(ZoomMode::Fixed(zoom), window_point, cx);
    }

    pub fn zoom_in(&mut self, _: &ZoomIn, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom_by(ZOOM_STEP, None, cx);
    }

    pub fn zoom_out(&mut self, _: &ZoomOut, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom_by(1. / ZOOM_STEP, None, cx);
    }

    pub fn actual_size(&mut self, _: &ActualSize, _: &mut Window, cx: &mut Context<Self>) {
        self.set_zoom_mode(ZoomMode::Fixed(1.), None, cx);
    }

    pub fn fit_width(&mut self, _: &FitWidth, _: &mut Window, cx: &mut Context<Self>) {
        self.set_zoom_mode(ZoomMode::FitWidth, None, cx);
    }

    pub fn fit_page(&mut self, _: &FitPage, _: &mut Window, cx: &mut Context<Self>) {
        let page = self.current_page();
        self.set_zoom_mode(ZoomMode::FitPage, None, cx);
        self.go_to_page(page, cx);
    }

    pub fn go_to_page(&mut self, page_index: usize, cx: &mut Context<Self>) {
        let last = self.page_count(cx).saturating_sub(1);
        // Applied after layout, so it also works right after a zoom change.
        self.pending_anchor = Some((
            PageAnchor {
                page_index: page_index.min(last),
                x: 0.,
                y: 0.,
            },
            point(Pixels::ZERO, PAGE_GAP / 2.),
        ));
        self.state_changed(cx);
        cx.notify();
    }

    fn next_page(&mut self, _: &NextPage, _: &mut Window, cx: &mut Context<Self>) {
        self.go_to_page(self.current_page() + 1, cx);
    }

    fn previous_page(&mut self, _: &PreviousPage, _: &mut Window, cx: &mut Context<Self>) {
        self.go_to_page(self.current_page().saturating_sub(1), cx);
    }

    fn first_page(&mut self, _: &FirstPage, _: &mut Window, cx: &mut Context<Self>) {
        self.scroll.set_scroll_position(Point::default());
        self.state_changed(cx);
        cx.notify();
    }

    fn last_page(&mut self, _: &LastPage, _: &mut Window, cx: &mut Context<Self>) {
        let x = self.scroll.scroll_position().x;
        self.scroll.set_scroll_position(point(x, px(f32::MAX)));
        self.state_changed(cx);
        cx.notify();
    }

    fn scroll_by(&mut self, delta: Point<Pixels>, cx: &mut Context<Self>) {
        self.pending_anchor = None;
        self.scroll.scroll_by(delta);
        self.state_changed(cx);
        cx.notify();
    }

    fn scroll_up(&mut self, _: &ScrollUp, _: &mut Window, cx: &mut Context<Self>) {
        self.scroll_by(point(Pixels::ZERO, LINE_HEIGHT), cx);
    }

    fn scroll_down(&mut self, _: &ScrollDown, _: &mut Window, cx: &mut Context<Self>) {
        self.scroll_by(point(Pixels::ZERO, -LINE_HEIGHT), cx);
    }

    fn screen_height(&self) -> Pixels {
        (self.scroll.viewport_bounds().size.height - LINE_HEIGHT).max(LINE_HEIGHT)
    }

    fn scroll_page_up(&mut self, _: &ScrollPageUp, _: &mut Window, cx: &mut Context<Self>) {
        self.scroll_by(point(Pixels::ZERO, self.screen_height()), cx);
    }

    fn scroll_page_down(&mut self, _: &ScrollPageDown, _: &mut Window, cx: &mut Context<Self>) {
        self.scroll_by(point(Pixels::ZERO, -self.screen_height()), cx);
    }

    fn go_to_page_action(&mut self, _: &GoToPage, _: &mut Window, cx: &mut Context<Self>) {
        self.page_edit_requested = true;
        cx.notify();
    }

    fn reveal_in_file_manager(
        &mut self,
        _: &RevealInFileManager,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(path) = self.pdf_item.read(cx).abs_path(cx) {
            self.project
                .update(cx, |project, cx| project.reveal_path(&path, cx));
        }
    }

    fn handle_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let delta = event.delta.pixel_delta(LINE_HEIGHT);
        if event.modifiers.platform || event.modifiers.control {
            let factor = (1. + f32::from(delta.y) * 0.01).clamp(0.5, 2.);
            self.zoom_by(factor, Some(event.position), cx);
        } else {
            self.scroll_by(delta, cx);
        }
    }

    fn handle_pinch(&mut self, event: &PinchEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom_by(1. + event.delta, Some(event.position), cx);
    }

    fn handle_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle, cx);
        self.drag_position = Some(event.position);
        cx.notify();
    }

    fn handle_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(previous) = self.drag_position else {
            return;
        };
        // The button can be released outside the window, or while another element
        // has captured the mouse, so a missed mouse-up must not leave us panning.
        if event.pressed_button.is_none() {
            self.end_drag(cx);
            return;
        }
        self.drag_position = Some(event.position);
        self.scroll_by(event.position - previous, cx);
    }

    fn end_drag(&mut self, cx: &mut Context<Self>) {
        if self.drag_position.take().is_some() {
            cx.notify();
        }
    }

    /// Persists the view state once the user has stopped interacting for a moment.
    fn state_changed(&mut self, cx: &mut Context<Self>) {
        self.serialize_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SERIALIZE_DEBOUNCE).await;
            this.update(cx, |_, cx| cx.emit(PdfViewEvent::StateChanged))
                .ok();
        }));
    }

    fn serialized_state(&self) -> SerializedViewState {
        let anchor = self.layout.as_ref().and_then(|layout| {
            layout.anchor_at(self.scroll.scroll_position() + point(Pixels::ZERO, PAGE_GAP / 2.))
        });
        SerializedViewState {
            zoom_mode: self.zoom_mode,
            anchor,
        }
    }

    fn restore_state(&mut self, state: SerializedViewState) {
        self.zoom_mode = state.zoom_mode;
        if let Some(anchor) = state.anchor {
            self.pending_anchor = Some((anchor, point(Pixels::ZERO, PAGE_GAP / 2.)));
        }
    }

    /// Lays out pages for the given viewport and returns what to paint.
    fn prepaint_pages(
        &mut self,
        viewport: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> PagesPaint {
        let item = self.pdf_item.read(cx);
        let document = item.document().clone();
        let generation = item.generation();

        // Relayout, keeping the reading position stable across zoom, resize and reload.
        let zoom = self.zoom_mode.resolve(&document.pages, viewport.size);
        let needs_layout = self.layout.as_ref().is_none_or(|layout| {
            layout.zoom != zoom
                || layout.content_size.width < viewport.size.width
                || layout.pages.len() != document.pages.len()
                || self.layout_generation != generation
        }) || self.scroll.viewport_bounds().size.width != viewport.size.width;
        if needs_layout {
            // A view resting at the very top stays there; the padding above the first
            // page doesn't scale with zoom, so anchoring to it would drift.
            if self.pending_anchor.is_none()
                && self.layout.is_some()
                && self.scroll.scroll_position().y > Pixels::ZERO
            {
                self.anchor_viewport_point(point(viewport.size.width / 2., Pixels::ZERO));
            }
            let layout = PageLayout::new(&document.pages, zoom, viewport.size.width);
            if self.layout.as_ref().is_none_or(|old| old.zoom != zoom) {
                self.zoom_changed_at = Instant::now();
            }
            self.layout = Some(layout);
            self.layout_generation = generation;
        }
        let layout = self.layout.as_ref().expect("layout was just computed");
        self.scroll.set_geometry(viewport, layout.content_size);
        if let Some((anchor, viewport_point)) = self.pending_anchor.take()
            && let Some(position) = layout.position_of(anchor)
        {
            let mut target = position - viewport_point;
            // Don't scroll horizontally when the pages fit the viewport anyway.
            if layout.content_size.width <= viewport.size.width {
                target.x = Pixels::ZERO;
            }
            self.scroll.set_scroll_position(target);
        }

        // Pick the raster zoom, letting it trail the displayed zoom while it changes.
        let raster_zoom = match self.raster_zoom {
            Some(raster_zoom) if raster_zoom != zoom => {
                let elapsed = self.zoom_changed_at.elapsed();
                if elapsed >= RASTER_SETTLE {
                    self.raster_zoom = Some(zoom);
                    zoom
                } else {
                    let wait = RASTER_SETTLE - elapsed;
                    self.settle_task = Some(cx.spawn(async move |this, cx| {
                        cx.background_executor().timer(wait).await;
                        this.update(cx, |_, cx| cx.notify()).ok();
                    }));
                    raster_zoom
                }
            }
            _ => {
                self.raster_zoom = Some(zoom);
                zoom
            }
        };

        let scale_factor = window.scale_factor();
        let raster_scale = raster_zoom * scale_factor;
        let scale_key = (raster_scale * 1024.).round() as u32;
        let scroll_position = self.scroll.scroll_position();
        let visible = Bounds::new(scroll_position, viewport.size);
        // Prefetch half a screen above and below.
        let margin = viewport.size.height / 2.;
        let prefetch_range =
            layout.pages_in_range(visible.top() - margin, visible.bottom() + margin);
        let visible_range = layout.pages_in_range(visible.top(), visible.bottom());

        self.tiles.frame += 1;
        let frame = self.tiles.frame;
        let mut wanted = HashSet::new();
        let mut pages = Vec::new();
        let border = cx.theme().colors().border_variant;

        for page_index in prefetch_range {
            let page_bounds = layout.pages[page_index];
            let dimensions = document.pages[page_index];
            let raster = raster_size(dimensions, raster_scale);
            // Visible part of the page, expanded by the prefetch margin, in raster pixels.
            let region = Bounds::from_corners(
                point(visible.left(), visible.top() - margin),
                point(visible.right(), visible.bottom() + margin),
            );
            if !page_bounds.intersects(&region) {
                continue;
            }
            let region = page_bounds.intersect(&region);
            let to_raster = raster_scale / layout.zoom;
            let raster_region = Bounds::new(
                point(
                    f32::from(region.left() - page_bounds.left()) * to_raster,
                    f32::from(region.top() - page_bounds.top()) * to_raster,
                ),
                size(
                    f32::from(region.size.width) * to_raster,
                    f32::from(region.size.height) * to_raster,
                ),
            );

            let mut complete = true;
            for tile in tiles_intersecting(raster, raster_region) {
                let key = TileKey {
                    generation,
                    page_index,
                    scale: scale_key,
                    column: tile.column,
                    row: tile.row,
                };
                wanted.insert(key);
                if let Some(cached) = self.tiles.tiles.get_mut(&key) {
                    cached.last_used = frame;
                } else {
                    complete = false;
                    if !self.tiles.pending.contains_key(&key) && !self.tiles.failed.contains(&key) {
                        let request = TileRequest {
                            document: document.id,
                            page_index,
                            scale: raster_scale,
                            x: tile.x,
                            y: tile.y,
                            width: tile.width,
                            height: tile.height,
                        };
                        if let Some(pending) = self.request_tile(key, tile, request, cx) {
                            self.tiles.pending.insert(key, pending);
                        }
                    }
                }
            }

            if !visible_range.contains(&page_index) {
                continue;
            }

            // Paint placeholders from other resolutions or document versions
            // first, then the current tiles on top.
            let page_origin = snap_point(
                viewport.origin - scroll_position + page_bounds.origin,
                scale_factor,
            );
            let page_rect = Bounds::new(page_origin, page_bounds.size);
            let mut images: Vec<(u64, bool, Bounds<Pixels>, Arc<RenderImage>)> = Vec::new();
            for (key, cached) in &mut self.tiles.tiles {
                if key.page_index != page_index {
                    continue;
                }
                let is_current = wanted.contains(key);
                if !is_current && complete {
                    continue;
                }
                let to_logical = layout.zoom / cached.scale;
                let left = page_origin.x + px(cached.tile.x as f32 * to_logical);
                let top = page_origin.y + px(cached.tile.y as f32 * to_logical);
                let right =
                    page_origin.x + px((cached.tile.x + cached.tile.width) as f32 * to_logical);
                let bottom =
                    page_origin.y + px((cached.tile.y + cached.tile.height) as f32 * to_logical);
                cached.last_used = frame;
                images.push((
                    if is_current {
                        u64::MAX
                    } else {
                        cached.scale.to_bits() as u64
                    },
                    is_current,
                    Bounds::from_corners(point(left, top), point(right, bottom)),
                    cached.image.clone(),
                ));
            }
            images.sort_by_key(|(order, ..)| *order);
            pages.push(PagePaint {
                bounds: page_rect,
                images: images
                    .into_iter()
                    .map(|(_, _, bounds, image)| (bounds, image))
                    .collect(),
            });
        }

        // Stop rendering tiles that scrolled out of range.
        self.tiles.pending.retain(|key, _| wanted.contains(key));
        self.tiles.evict(window);

        PagesPaint { pages, border }
    }

    fn request_tile(
        &self,
        key: TileKey,
        tile: TileRect,
        request: TileRequest,
        cx: &mut Context<Self>,
    ) -> Option<PendingTile> {
        let worker = PdfWorker::global().log_err()?;
        let (tx, rx) = oneshot::channel();
        let ticket = worker.render(request, move |result| {
            tx.send(result).ok();
        });
        let task = cx.spawn(async move |this, cx| {
            let result = rx.await.context("render was cancelled");
            let image = result.and_then(|result| result).and_then(|rendered| {
                render_image(rendered).context("tile buffer has the wrong size")
            });
            this.update(cx, |this, cx| {
                this.tiles.pending.remove(&key);
                match image {
                    Ok(image) => {
                        this.tiles.tiles.insert(
                            key,
                            CachedTile {
                                image: Arc::new(image),
                                tile,
                                scale: request.scale,
                                last_used: this.tiles.frame,
                            },
                        );
                    }
                    Err(error) => {
                        log::error!("failed to render PDF tile {key:?}: {error:#}");
                        this.tiles.failed.insert(key);
                    }
                }
                cx.notify();
            })
            .ok();
        });
        Some(PendingTile {
            _ticket: ticket,
            _task: task,
        })
    }
}

fn render_image(tile: RenderedTile) -> Option<RenderImage> {
    // GPUI treats image bytes as BGRA, which is what the worker produces.
    let buffer = RgbaImage::from_raw(tile.width, tile.height, tile.bgra)?;
    Some(RenderImage::new(smallvec![Frame::new(buffer)]))
}

fn snap_point(position: Point<Pixels>, scale_factor: f32) -> Point<Pixels> {
    position.map(|coordinate| px((f32::from(coordinate) * scale_factor).round() / scale_factor))
}

struct PagePaint {
    bounds: Bounds<Pixels>,
    images: Vec<(Bounds<Pixels>, Arc<RenderImage>)>,
}

struct PagesPaint {
    pages: Vec<PagePaint>,
    border: Hsla,
}

/// Paints the visible pages; layout and tile scheduling happen in prepaint.
struct PagesElement {
    view: Entity<PdfView>,
}

impl IntoElement for PagesElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for PagesElement {
    type RequestLayoutState = ();
    type PrepaintState = PagesPaint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let style = Style {
            size: size(relative(1.).into(), relative(1.).into()),
            ..Default::default()
        };
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> PagesPaint {
        self.view
            .update(cx, |view, cx| view.prepaint_pages(bounds, window, cx))
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        paint: &mut PagesPaint,
        window: &mut Window,
        _: &mut App,
    ) {
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            for page in &paint.pages {
                window.paint_drop_shadows(
                    page.bounds,
                    Corners::default(),
                    &[BoxShadow {
                        color: hsla(0., 0., 0., 0.18),
                        offset: point(px(0.), px(1.)),
                        blur_radius: px(4.),
                        spread_radius: px(0.),
                        inset: false,
                    }],
                );
                // PDF pages are paper: always drawn on white, as the document intends.
                window.paint_quad(fill(page.bounds, gpui::white()));
                window.with_content_mask(
                    Some(ContentMask {
                        bounds: page.bounds,
                    }),
                    |window| {
                        for (image_bounds, image) in &page.images {
                            window
                                .paint_image(
                                    page.bounds,
                                    *image_bounds,
                                    Corners::default(),
                                    image.clone(),
                                    0,
                                    false,
                                )
                                .log_err();
                        }
                    },
                );
                window.paint_quad(outline(page.bounds, paint.border, Default::default()));
            }
        });
    }
}

impl EventEmitter<PdfViewEvent> for PdfView {}

impl Focusable for PdfView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PdfView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_dragging = self.drag_position.is_some();
        div()
            .track_focus(&self.focus_handle)
            .key_context("PdfViewer")
            .on_action(cx.listener(Self::zoom_in))
            .on_action(cx.listener(Self::zoom_out))
            .on_action(cx.listener(Self::actual_size))
            .on_action(cx.listener(Self::fit_width))
            .on_action(cx.listener(Self::fit_page))
            .on_action(cx.listener(Self::next_page))
            .on_action(cx.listener(Self::previous_page))
            .on_action(cx.listener(Self::first_page))
            .on_action(cx.listener(Self::last_page))
            .on_action(cx.listener(Self::scroll_up))
            .on_action(cx.listener(Self::scroll_down))
            .on_action(cx.listener(Self::scroll_page_up))
            .on_action(cx.listener(Self::scroll_page_down))
            .on_action(cx.listener(Self::go_to_page_action))
            .on_action(cx.listener(Self::reveal_in_file_manager))
            .size_full()
            .bg(cx.theme().colors().background)
            .child(
                div()
                    .id("pdf-pages")
                    .size_full()
                    .overflow_hidden()
                    .cursor(if is_dragging {
                        gpui::CursorStyle::ClosedHand
                    } else {
                        gpui::CursorStyle::Arrow
                    })
                    .on_scroll_wheel(cx.listener(Self::handle_scroll_wheel))
                    .on_pinch(cx.listener(Self::handle_pinch))
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::handle_mouse_down))
                    .on_mouse_down(MouseButton::Middle, cx.listener(Self::handle_mouse_down))
                    .on_mouse_move(cx.listener(Self::handle_mouse_move))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseUpEvent, _, cx| this.end_drag(cx)),
                    )
                    .on_mouse_up(
                        MouseButton::Middle,
                        cx.listener(|this, _: &MouseUpEvent, _, cx| this.end_drag(cx)),
                    )
                    .on_mouse_up_out(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseUpEvent, _, cx| this.end_drag(cx)),
                    )
                    .on_mouse_up_out(
                        MouseButton::Middle,
                        cx.listener(|this, _: &MouseUpEvent, _, cx| this.end_drag(cx)),
                    )
                    .child(PagesElement { view: cx.entity() })
                    .custom_scrollbars(
                        Scrollbars::new(ScrollAxes::Both)
                            .tracked_scroll_handle(&self.scroll)
                            .tracked_entity(cx.entity_id()),
                        window,
                        cx,
                    ),
            )
    }
}

impl Item for PdfView {
    type Event = PdfViewEvent;

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        match event {
            PdfViewEvent::TitleChanged => {
                f(ItemEvent::UpdateTab);
                f(ItemEvent::UpdateBreadcrumbs);
            }
            PdfViewEvent::StateChanged => {}
        }
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        f(self.pdf_item.entity_id(), self.pdf_item.read(cx))
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        let pdf_item = self.pdf_item.read(cx);
        let path = pdf_item
            .abs_path(cx)
            .map(|abs_path| abs_path.compact())
            .unwrap_or_else(|| pdf_item.file().full_path(cx));
        Some(path.to_string_lossy().into_owned().into())
    }

    fn tab_content(&self, params: TabContentParams, _: &Window, cx: &App) -> AnyElement {
        let project_path = self.pdf_item.read(cx).project_path(cx);
        let label_color = if ItemSettings::get_global(cx).git_status {
            let git_status = self
                .project
                .read(cx)
                .git_store()
                .read(cx)
                .display_status_for_project_path(&project_path, cx)
                .map(|status| status.summary())
                .unwrap_or_default();
            self.project
                .read(cx)
                .entry_for_path(&project_path, cx)
                .map(|entry| {
                    entry_git_aware_label_color(git_status, entry.is_ignored, params.selected)
                })
                .unwrap_or_else(|| params.text_color())
        } else {
            params.text_color()
        };

        Label::new(self.tab_content_text(params.detail.unwrap_or_default(), cx))
            .single_line()
            .color(label_color)
            .when(params.preview, |this| this.italic())
            .when(self.pdf_item.read(cx).is_deleted(), |this| {
                this.strikethrough()
            })
            .into_any_element()
    }

    fn tab_content_text(&self, _: usize, cx: &App) -> SharedString {
        self.pdf_item
            .read(cx)
            .file()
            .file_name(cx)
            .to_string()
            .into()
    }

    fn tab_icon(&self, _: &Window, cx: &App) -> Option<Icon> {
        // The full path includes the root name, which is the file name when the PDF
        // was opened on its own and its project path is empty.
        let path = self.pdf_item.read(cx).file().full_path(cx);
        ItemSettings::get_global(cx)
            .file_icons
            .then(|| FileIcons::get_icon(&path, cx))
            .flatten()
            .map(Icon::from_path)
    }

    fn breadcrumb_location(&self, cx: &App) -> ToolbarItemLocation {
        if EditorSettings::get_global(cx).toolbar.breadcrumbs {
            ToolbarItemLocation::PrimaryLeft
        } else {
            ToolbarItemLocation::Hidden
        }
    }

    fn breadcrumbs(&self, cx: &App) -> Option<(Vec<HighlightedText>, Option<Font>)> {
        let project = self.project.read(cx);
        let item = self.pdf_item.read(cx);
        let mut path = item.file().path().to_rel_path_buf();
        // A file opened on its own is the worktree root, so its relative path is
        // empty; the root name is the file name then.
        if (path.is_empty() || project.visible_worktrees(cx).count() > 1)
            && let Some(worktree) = project.worktree_for_id(item.project_path(cx).worktree_id, cx)
        {
            path = worktree.read(cx).root_name().join(&path);
        }
        let mut text = path.display(project.path_style(cx)).to_string();
        if let Some(title) = &item.document().title {
            text.push_str(" - ");
            text.push_str(title);
        }
        Some((
            vec![HighlightedText {
                text: text.into(),
                highlights: vec![],
            }],
            Some(ThemeSettings::get_global(cx).buffer_font.clone()),
        ))
    }

    fn can_split(&self) -> bool {
        true
    }

    fn clone_on_split(
        &self,
        _: Option<WorkspaceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Self>>> {
        let state = self.serialized_state();
        let pdf_item = self.pdf_item.clone();
        let project = self.project.clone();
        Task::ready(Some(cx.new(|cx| {
            let mut view = PdfView::new(pdf_item, project, window, cx);
            view.restore_state(state);
            view
        })))
    }

    fn has_deleted_file(&self, cx: &App) -> bool {
        self.pdf_item.read(cx).is_deleted()
    }

    fn buffer_kind(&self, _: &App) -> workspace::item::ItemBufferKind {
        workspace::item::ItemBufferKind::Singleton
    }
}

impl ProjectItem for PdfView {
    type Item = PdfItem;

    fn for_project_item(
        project: Entity<Project>,
        _: Option<&Pane>,
        item: Entity<Self::Item>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new(item, project, window, cx)
    }

    fn for_broken_project_item(
        abs_path: &Path,
        is_local: bool,
        error: &anyhow::Error,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<InvalidItemView> {
        Some(InvalidItemView::new(abs_path, is_local, error, window, cx))
    }
}

impl SerializableItem for PdfView {
    fn serialized_item_kind() -> &'static str {
        "PdfView"
    }

    fn deserialize(
        project: Entity<Project>,
        _workspace: WeakEntity<Workspace>,
        workspace_id: WorkspaceId,
        item_id: ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<Entity<Self>>> {
        let db = PdfViewerDb::global(cx);
        window.spawn(cx, async move |cx| {
            let (pdf_path, view_state) = db
                .get_pdf(item_id, workspace_id)?
                .context("no PDF path found")?;
            let pdf_item = PdfItem::open_abs_path(project.clone(), pdf_path, cx).await?;
            let state = serde_json::from_str::<SerializedViewState>(&view_state).log_err();
            cx.update(|window, cx| {
                cx.new(|cx| {
                    let mut view = PdfView::new(pdf_item, project, window, cx);
                    if let Some(state) = state {
                        view.restore_state(state);
                    }
                    view
                })
            })
        })
    }

    fn cleanup(
        workspace_id: WorkspaceId,
        alive_items: Vec<ItemId>,
        _: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<()>> {
        let db = PdfViewerDb::global(cx);
        delete_unloaded_items(alive_items, workspace_id, "pdf_viewers", &db, cx)
    }

    fn serialize(
        &mut self,
        workspace: &mut Workspace,
        item_id: ItemId,
        _closing: bool,
        cx: &mut Context<Self>,
    ) -> Option<Task<anyhow::Result<()>>> {
        let workspace_id = workspace.database_id()?;
        // Save the path on the host rather than on this computer, so PDFs in
        // remote projects are restored too.
        let pdf_path = self.pdf_item.read(cx).path_on_host(cx);
        let view_state = serde_json::to_string(&self.serialized_state()).log_err()?;
        let db = PdfViewerDb::global(cx);
        Some(cx.background_spawn(async move {
            db.save_pdf(item_id, workspace_id, pdf_path, view_state)
                .await
        }))
    }

    fn should_serialize(&self, event: &Self::Event) -> bool {
        matches!(event, PdfViewEvent::StateChanged)
    }
}

#[cfg(test)]
#[path = "pdf_view_tests.rs"]
mod tests;
