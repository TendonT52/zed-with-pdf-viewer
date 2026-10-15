use editor::{Editor, EditorEvent, actions::SelectAll};
use gpui::{
    Action, Context, Entity, EventEmitter, Focusable as _, Subscription, WeakEntity, Window,
};
use ui::{Divider, Tooltip, prelude::*};
use workspace::{ToolbarItemEvent, ToolbarItemLocation, ToolbarItemView, item::ItemHandle};

use crate::{
    layout::ZoomMode,
    pdf_view::{ActualSize, FitPage, FitWidth, NextPage, PdfView, PreviousPage, ZoomIn, ZoomOut},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditTarget {
    Page,
    Zoom,
}

struct InlineEdit {
    target: EditTarget,
    editor: Entity<Editor>,
    _subscription: Subscription,
}

/// Page navigation, zoom and color controls shown above an active PDF.
pub struct PdfViewToolbarControls {
    pdf_view: Option<WeakEntity<PdfView>>,
    edit: Option<InlineEdit>,
    _subscription: Option<Subscription>,
}

impl PdfViewToolbarControls {
    pub fn new() -> Self {
        Self {
            pdf_view: None,
            edit: None,
            _subscription: None,
        }
    }

    fn pdf_view(&self) -> Option<Entity<PdfView>> {
        self.pdf_view.as_ref()?.upgrade()
    }

    fn start_edit(&mut self, target: EditTarget, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pdf_view) = self.pdf_view() else {
            return;
        };
        let initial = match target {
            EditTarget::Page => (pdf_view.read(cx).current_page() + 1).to_string(),
            EditTarget::Zoom => ((pdf_view.read(cx).zoom() * 100.).round() as u32).to_string(),
        };
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(initial, window, cx);
            editor.set_text_style_refinement(gpui::TextStyleRefinement {
                color: Some(cx.theme().colors().text),
                text_align: Some(gpui::TextAlign::Center),
                font_size: Some(TextSize::Small.rems(cx).into()),
                ..Default::default()
            });
            editor.select_all(&SelectAll, window, cx);
            editor
        });
        let subscription =
            cx.subscribe_in(
                &editor,
                window,
                |this, editor, event, window, cx| match event {
                    EditorEvent::Blurred => this.commit_edit(window, cx),
                    EditorEvent::Edited { .. } => {
                        let text = editor.read(cx).text(cx);
                        let digits: String =
                            text.chars().filter(char::is_ascii_digit).take(5).collect();
                        if digits != text {
                            editor.update(cx, |editor, cx| editor.set_text(digits, window, cx));
                        }
                        cx.notify();
                    }
                    _ => {}
                },
            );
        editor.focus_handle(cx).focus(window, cx);
        self.edit = Some(InlineEdit {
            target,
            editor,
            _subscription: subscription,
        });
        cx.notify();
    }

    fn commit_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(edit) = self.edit.take() else {
            return;
        };
        let value = edit.editor.read(cx).text(cx).trim().parse::<u32>().ok();
        if let (Some(value), Some(pdf_view)) = (value.filter(|value| *value > 0), self.pdf_view()) {
            pdf_view.update(cx, |pdf_view, cx| match edit.target {
                EditTarget::Page => pdf_view.go_to_page(value as usize - 1, cx),
                EditTarget::Zoom => {
                    pdf_view.set_zoom_mode(ZoomMode::Fixed(value as f32 / 100.), None, cx)
                }
            });
            // Return focus to the document so keyboard navigation keeps working.
            pdf_view.read(cx).focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }

    fn cancel_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.edit = None;
        if let Some(pdf_view) = self.pdf_view() {
            pdf_view.read(cx).focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }

    fn render_value(
        &self,
        target: EditTarget,
        label: String,
        min_chars: usize,
        tooltip: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if let Some(edit) = self.edit.as_ref().filter(|edit| edit.target == target) {
            let chars = edit.editor.read(cx).text(cx).chars().count().max(min_chars);
            return h_flex()
                .w(px(chars as f32 * 8. + 12.))
                .child(edit.editor.clone())
                .on_action(
                    cx.listener(|this, _: &menu::Confirm, window, cx| this.commit_edit(window, cx)),
                )
                .on_action(
                    cx.listener(|this, _: &menu::Cancel, window, cx| this.cancel_edit(window, cx)),
                )
                .into_any_element();
        }
        h_flex()
            .id(match target {
                EditTarget::Page => "pdf-page-label",
                EditTarget::Zoom => "pdf-zoom-label",
            })
            .px_1()
            .rounded_sm()
            .cursor_pointer()
            .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
            .child(Label::new(label).size(LabelSize::Small))
            .tooltip(Tooltip::text(tooltip))
            .on_click(cx.listener(move |this, _, window, cx| this.start_edit(target, window, cx)))
            .into_any_element()
    }
}

impl Render for PdfViewToolbarControls {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(pdf_view) = self.pdf_view() else {
            return div().into_any_element();
        };
        let view = pdf_view.read(cx);
        let focus_handle = view.focus_handle(cx);
        let page_count = view.page_count(cx);
        let current_page = view.current_page();
        let zoom_mode = view.zoom_mode();
        let zoom_percent = (view.zoom() * 100.).round() as u32;

        let button =
            |id: &'static str, icon: IconName, tooltip: &'static str, action: Box<dyn Action>| {
                let focus_handle = focus_handle.clone();
                let tooltip_action = action.boxed_clone();
                IconButton::new(id, icon)
                    .icon_size(IconSize::Small)
                    .tooltip(move |_, cx| Tooltip::for_action(tooltip, &*tooltip_action, cx))
                    .on_click(move |_, window, cx| {
                        focus_handle.dispatch_action(&*action, window, cx);
                    })
            };

        h_flex()
            .gap_1()
            .child(
                button(
                    "pdf-previous-page",
                    IconName::ChevronUp,
                    "Previous Page",
                    PreviousPage.boxed_clone(),
                )
                .disabled(current_page == 0),
            )
            .child(
                h_flex()
                    .gap_0p5()
                    .child(self.render_value(
                        EditTarget::Page,
                        (current_page + 1).to_string(),
                        page_count.to_string().len(),
                        "Go to Page",
                        cx,
                    ))
                    .child(
                        Label::new(format!("/ {page_count}"))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .child(
                button(
                    "pdf-next-page",
                    IconName::ChevronDown,
                    "Next Page",
                    NextPage.boxed_clone(),
                )
                .disabled(current_page + 1 >= page_count),
            )
            .child(Divider::vertical())
            .child(button(
                "pdf-zoom-out",
                IconName::Dash,
                "Zoom Out",
                ZoomOut.boxed_clone(),
            ))
            .child(self.render_value(
                EditTarget::Zoom,
                format!("{zoom_percent}%"),
                3,
                "Set Zoom",
                cx,
            ))
            .child(button(
                "pdf-zoom-in",
                IconName::Plus,
                "Zoom In",
                ZoomIn.boxed_clone(),
            ))
            .child(
                button(
                    "pdf-fit-width",
                    IconName::ArrowRightLeft,
                    "Fit Width",
                    FitWidth.boxed_clone(),
                )
                .toggle_state(matches!(zoom_mode, ZoomMode::FitWidth)),
            )
            .child(
                button(
                    "pdf-fit-page",
                    IconName::Maximize,
                    "Fit Page",
                    FitPage.boxed_clone(),
                )
                .toggle_state(matches!(zoom_mode, ZoomMode::FitPage)),
            )
            .child(
                button(
                    "pdf-actual-size",
                    IconName::Screen,
                    "Actual Size",
                    ActualSize.boxed_clone(),
                )
                .toggle_state(zoom_mode == ZoomMode::Fixed(1.)),
            )
            .into_any_element()
    }
}

impl EventEmitter<ToolbarItemEvent> for PdfViewToolbarControls {}

impl ToolbarItemView for PdfViewToolbarControls {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        self.pdf_view = None;
        self.edit = None;
        self._subscription = None;

        let Some(pdf_view) = active_pane_item.and_then(|item| item.downcast::<PdfView>()) else {
            return ToolbarItemLocation::Hidden;
        };
        self._subscription =
            Some(
                cx.observe_in(&pdf_view, window, |this, pdf_view, window, cx| {
                    if pdf_view.update(cx, |pdf_view, _| pdf_view.take_page_edit_request()) {
                        this.start_edit(EditTarget::Page, window, cx);
                    }
                    cx.notify();
                }),
            );
        self.pdf_view = Some(pdf_view.downgrade());
        cx.notify();
        ToolbarItemLocation::PrimaryRight
    }
}
