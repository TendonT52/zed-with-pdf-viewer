use std::{path::Path, time::Duration};

use fs::{FakeFs, Fs as _};
use gpui::{Entity, TestAppContext, VisualTestContext, point, px, size};
use project::{Project, ProjectPath};
use settings::SettingsStore;
use util::rel_path::rel_path;

use super::*;
use crate::layout::{CONTENT_PADDING, ZoomMode};
use crate::test_support::multi_page_pdf;

fn init_test(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
    });
}

/// Opens a generated PDF in a `PdfView`, or returns `None` when Pdfium isn't installed.
async fn open_pdf_view(
    pages: usize,
    cx: &mut TestAppContext,
) -> Option<(Entity<PdfView>, &mut VisualTestContext)> {
    if let Err(error) = kkpdf_zed::PdfWorker::global() {
        eprintln!("skipping: {error}");
        return None;
    }
    init_test(cx);
    // The Pdfium worker is a real thread, so let the executor wait for it.
    cx.executor().allow_parking();

    let fs = FakeFs::new(cx.executor());
    fs.create_dir(Path::new("/root")).await.unwrap();
    fs.insert_file("/root/doc.pdf", multi_page_pdf(pages)).await;
    let project = Project::test(fs, [Path::new("/root")], cx).await;
    let worktree_id = cx.update(|cx| project.read(cx).worktrees(cx).next().unwrap().read(cx).id());
    let path = ProjectPath {
        worktree_id,
        path: rel_path("doc.pdf").into(),
    };
    let pdf_item = cx
        .update(|cx| <PdfItem as project::ProjectItem>::try_open(&project, &path, cx))
        .expect("PDF files should be claimed by the PDF viewer")
        .await
        .expect("PDF should open");

    let (view, cx) =
        cx.add_window_view(|window, cx| PdfView::new(pdf_item, project.clone(), window, cx));
    draw(cx);
    Some((view, cx))
}

fn draw(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| {
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

/// Draws until every visible tile has been rendered by the worker.
fn draw_until_rendered(view: &Entity<PdfView>, cx: &mut VisualTestContext) {
    for _ in 0..200 {
        draw(cx);
        if cx.read(|cx| view.read(cx).tiles.pending.is_empty()) {
            draw(cx);
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("tiles never finished rendering");
}

#[gpui::test]
async fn opens_at_the_top_of_the_first_page(cx: &mut TestAppContext) {
    let Some((view, cx)) = open_pdf_view(12, cx).await else {
        return;
    };
    draw_until_rendered(&view, cx);
    // Windows usually settle into their final size over a few frames.
    for window_size in [size(px(300.), px(200.)), size(px(1400.), px(900.))] {
        cx.simulate_resize(window_size);
        draw_until_rendered(&view, cx);
    }
    cx.read(|cx| {
        let view = view.read(cx);
        assert_eq!(view.scroll.scroll_position().y, px(0.));
        assert_eq!(view.current_page(), 0);
    });
}

#[gpui::test]
async fn zooming_keeps_the_point_under_the_cursor(cx: &mut TestAppContext) {
    let Some((view, cx)) = open_pdf_view(5, cx).await else {
        return;
    };
    cx.simulate_resize(size(px(1000.), px(800.)));
    draw_until_rendered(&view, cx);
    view.update(cx, |view, cx| view.go_to_page(2, cx));
    draw(cx);

    let (viewport, anchor_before) = cx.read(|cx| {
        let view = view.read(cx);
        let viewport = view.scroll.viewport_bounds();
        let cursor = viewport.origin + point(px(300.), px(250.));
        let content = view.scroll.scroll_position() + (cursor - viewport.origin);
        (
            viewport,
            view.layout.as_ref().unwrap().anchor_at(content).unwrap(),
        )
    });
    let cursor = viewport.origin + point(px(300.), px(250.));
    view.update(cx, |view, cx| {
        view.set_zoom_mode(ZoomMode::Fixed(2.5), Some(cursor), cx)
    });
    draw_until_rendered(&view, cx);

    cx.read(|cx| {
        let view = view.read(cx);
        let layout = view.layout.as_ref().unwrap();
        assert_eq!(layout.zoom, 2.5);
        let content = view.scroll.scroll_position() + (cursor - viewport.origin);
        let anchor_after = layout.anchor_at(content).unwrap();
        assert_eq!(anchor_after.page_index, anchor_before.page_index);
        assert!((anchor_after.x - anchor_before.x).abs() < 1.);
        assert!((anchor_after.y - anchor_before.y).abs() < 1.);
    });
}

#[gpui::test]
async fn page_navigation(cx: &mut TestAppContext) {
    let Some((view, cx)) = open_pdf_view(8, cx).await else {
        return;
    };
    cx.simulate_resize(size(px(900.), px(700.)));
    draw_until_rendered(&view, cx);

    view.update(cx, |view, cx| view.go_to_page(5, cx));
    draw(cx);
    cx.read(|cx| {
        let view = view.read(cx);
        let page_top = view.layout.as_ref().unwrap().pages[5].top();
        assert_eq!(view.scroll.scroll_position().y, page_top - PAGE_GAP / 2.);
    });

    // Jumping past the end clamps to the last page.
    view.update(cx, |view, cx| view.go_to_page(100, cx));
    draw(cx);
    cx.read(|cx| assert_eq!(view.read(cx).current_page(), 7));

    cx.focus(&view);
    cx.dispatch_action(FirstPage);
    draw(cx);
    cx.read(|cx| assert_eq!(view.read(cx).scroll.scroll_position().y, px(0.)));
}

#[gpui::test]
async fn renders_visible_tiles_and_paints_them(cx: &mut TestAppContext) {
    let Some((view, cx)) = open_pdf_view(3, cx).await else {
        return;
    };
    cx.simulate_resize(size(px(800.), px(600.)));
    draw_until_rendered(&view, cx);
    cx.read(|cx| {
        let view = view.read(cx);
        assert!(view.tiles.failed.is_empty(), "no tile should fail");
        assert!(
            !view.tiles.tiles.is_empty(),
            "visible tiles should be cached"
        );
        let layout = view.layout.as_ref().unwrap();
        assert_eq!(layout.pages[0].top(), CONTENT_PADDING);
    });
}

#[gpui::test]
async fn moving_the_mouse_without_a_button_does_not_pan(cx: &mut TestAppContext) {
    let Some((view, cx)) = open_pdf_view(6, cx).await else {
        return;
    };
    cx.simulate_resize(size(px(900.), px(700.)));
    draw_until_rendered(&view, cx);
    let center = cx.read(|cx| view.read(cx).scroll.viewport_bounds().center());

    // Hovering never scrolls.
    cx.simulate_mouse_move(center, None, gpui::Modifiers::none());
    cx.simulate_mouse_move(
        center - point(px(0.), px(200.)),
        None,
        gpui::Modifiers::none(),
    );
    draw(cx);
    cx.read(|cx| {
        assert_eq!(
            view.read(cx).scroll.scroll_position(),
            point(px(0.), px(0.))
        )
    });

    // Dragging with the button held pans the document.
    cx.simulate_mouse_down(center, gpui::MouseButton::Left, gpui::Modifiers::none());
    cx.simulate_mouse_move(
        center - point(px(0.), px(150.)),
        Some(gpui::MouseButton::Left),
        gpui::Modifiers::none(),
    );
    draw(cx);
    cx.read(|cx| assert_eq!(view.read(cx).scroll.scroll_position().y, px(150.)));

    // If the mouse-up is never delivered, the next plain move ends the drag.
    cx.simulate_mouse_move(
        center - point(px(0.), px(300.)),
        None,
        gpui::Modifiers::none(),
    );
    cx.simulate_mouse_move(center, None, gpui::Modifiers::none());
    draw(cx);
    cx.read(|cx| {
        let view = view.read(cx);
        assert_eq!(view.scroll.scroll_position().y, px(150.));
        assert!(view.drag_position.is_none());
    });
}

#[gpui::test]
async fn opens_a_pdf_opened_on_its_own(cx: &mut TestAppContext) {
    if let Err(error) = kkpdf_zed::PdfWorker::global() {
        eprintln!("skipping: {error}");
        return;
    }
    init_test(cx);
    cx.executor().allow_parking();

    // Opening a single file (Finder, `zed file.pdf`) makes the file itself the
    // worktree root, so its path inside the worktree is empty.
    let fs = FakeFs::new(cx.executor());
    fs.create_dir(Path::new("/root")).await.unwrap();
    fs.insert_file("/root/Lecture Notes.PDF", multi_page_pdf(2))
        .await;
    let project = Project::test(fs, [Path::new("/root/Lecture Notes.PDF")], cx).await;
    let worktree_id = cx.update(|cx| project.read(cx).worktrees(cx).next().unwrap().read(cx).id());
    let path = ProjectPath {
        worktree_id,
        path: util::rel_path::RelPath::empty_arc(),
    };

    let pdf_item = cx
        .update(|cx| <PdfItem as project::ProjectItem>::try_open(&project, &path, cx))
        .expect("a lone PDF should be claimed by the PDF viewer")
        .await
        .expect("a lone PDF should open");
    cx.read(|cx| assert_eq!(pdf_item.read(cx).document().pages.len(), 2));
}
