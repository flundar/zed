pub mod actions;
pub mod autoscroll;
pub mod scroll_amount;

use std::{
    cmp::Ordering,
    time::{Duration, Instant},
};

use gpui::{
    geometry::vector::{vec2f, Vector2F},
    AppContext, Axis, Task, ViewContext,
};
use language::{Bias, Point};
use util::ResultExt;
use workspace::WorkspaceId;

use crate::{
    display_map::{DisplaySnapshot, ToDisplayPoint},
    hover_popover::{hide_hover, HideHover},
    persistence::DB,
    Anchor, DisplayPoint, Editor, EditorMode, Event, MultiBufferSnapshot, ToPoint,
};

use self::{
    autoscroll::{Autoscroll, AutoscrollStrategy},
    scroll_amount::ScrollAmount,
};

pub const SCROLL_EVENT_SEPARATION: Duration = Duration::from_millis(28);
const SCROLLBAR_SHOW_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Default)]
pub struct ScrollbarAutoHide(pub bool);

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrollAnchor {
    pub offset: Vector2F,
    pub top_anchor: Anchor,
}

impl ScrollAnchor {
    fn new() -> Self {
        Self {
            offset: Vector2F::zero(),
            top_anchor: Anchor::min(),
        }
    }

    pub fn scroll_position(&self, snapshot: &DisplaySnapshot) -> Vector2F {
        let mut scroll_position = self.offset;
        if self.top_anchor != Anchor::min() {
            let scroll_top = self.top_anchor.to_display_point(snapshot).row() as f32;
            scroll_position.set_y(scroll_top + scroll_position.y());
        } else {
            scroll_position.set_y(0.);
        }
        scroll_position
    }

    pub fn top_row(&self, buffer: &MultiBufferSnapshot) -> u32 {
        self.top_anchor.to_point(buffer).row
    }
}

#[derive(Clone, Copy, Debug)]
pub struct OngoingScroll {
    last_event: Instant,
    axis: Option<Axis>,
}

impl OngoingScroll {
    fn new() -> Self {
        Self {
            last_event: Instant::now() - SCROLL_EVENT_SEPARATION,
            axis: None,
        }
    }

    pub fn filter(&self, delta: &mut Vector2F) -> Option<Axis> {
        const UNLOCK_PERCENT: f32 = 1.9;
        const UNLOCK_LOWER_BOUND: f32 = 6.;
        let mut axis = self.axis;

        let x = delta.x().abs();
        let y = delta.y().abs();
        let duration = Instant::now().duration_since(self.last_event);
        if duration > SCROLL_EVENT_SEPARATION {
            //New ongoing scroll will start, determine axis
            axis = if x <= y {
                Some(Axis::Vertical)
            } else {
                Some(Axis::Horizontal)
            };
        } else if x.max(y) >= UNLOCK_LOWER_BOUND {
            //Check if the current ongoing will need to unlock
            match axis {
                Some(Axis::Vertical) => {
                    if x > y && x >= y * UNLOCK_PERCENT {
                        axis = None;
                    }
                }

                Some(Axis::Horizontal) => {
                    if y > x && y >= x * UNLOCK_PERCENT {
                        axis = None;
                    }
                }

                None => {}
            }
        }

        match axis {
            Some(Axis::Vertical) => *delta = vec2f(0., delta.y()),
            Some(Axis::Horizontal) => *delta = vec2f(delta.x(), 0.),
            None => {}
        }

        axis
    }
}

pub struct ScrollManager {
    vertical_scroll_margin: f32,
    anchor: ScrollAnchor,
    ongoing: OngoingScroll,
    autoscroll_request: Option<(Autoscroll, bool)>,
    last_autoscroll: Option<(Vector2F, f32, f32, AutoscrollStrategy)>,
    show_scrollbars: bool,
    hide_scrollbar_task: Option<Task<()>>,
    visible_line_count: Option<f32>,
}

impl ScrollManager {
    pub fn new() -> Self {
        ScrollManager {
            vertical_scroll_margin: 3.0,
            anchor: ScrollAnchor::new(),
            ongoing: OngoingScroll::new(),
            autoscroll_request: None,
            show_scrollbars: true,
            hide_scrollbar_task: None,
            last_autoscroll: None,
            visible_line_count: None,
        }
    }

    pub fn clone_state(&mut self, other: &Self) {
        self.anchor = other.anchor;
        self.ongoing = other.ongoing;
    }

    pub fn anchor(&self) -> ScrollAnchor {
        self.anchor
    }

    pub fn ongoing_scroll(&self) -> OngoingScroll {
        self.ongoing
    }

    pub fn update_ongoing_scroll(&mut self, axis: Option<Axis>) {
        self.ongoing.last_event = Instant::now();
        self.ongoing.axis = axis;
    }

    pub fn scroll_position(&self, snapshot: &DisplaySnapshot) -> Vector2F {
        self.anchor.scroll_position(snapshot)
    }

    fn set_scroll_position(
        &mut self,
        scroll_position: Vector2F,
        map: &DisplaySnapshot,
        local: bool,
        workspace_id: Option<i64>,
        cx: &mut ViewContext<Editor>,
    ) {
        let (new_anchor, top_row) = if scroll_position.y() <= 0. {
            (
                ScrollAnchor {
                    top_anchor: Anchor::min(),
                    offset: scroll_position.max(vec2f(0., 0.)),
                },
                0,
            )
        } else {
            let scroll_top_buffer_point =
                DisplayPoint::new(scroll_position.y() as u32, 0).to_point(&map);
            let top_anchor = map
                .buffer_snapshot
                .anchor_at(scroll_top_buffer_point, Bias::Right);

            (
                ScrollAnchor {
                    top_anchor,
                    offset: vec2f(
                        scroll_position.x(),
                        scroll_position.y() - top_anchor.to_display_point(&map).row() as f32,
                    ),
                },
                scroll_top_buffer_point.row,
            )
        };

        self.set_anchor(new_anchor, top_row, local, workspace_id, cx);
    }

    fn set_anchor(
        &mut self,
        anchor: ScrollAnchor,
        top_row: u32,
        local: bool,
        workspace_id: Option<i64>,
        cx: &mut ViewContext<Editor>,
    ) {
        self.anchor = anchor;
        cx.emit(Event::ScrollPositionChanged { local });
        self.show_scrollbar(cx);
        self.autoscroll_request.take();
        if let Some(workspace_id) = workspace_id {
            let item_id = cx.view_id();

            cx.background()
                .spawn(async move {
                    DB.save_scroll_position(
                        item_id,
                        workspace_id,
                        top_row,
                        anchor.offset.x(),
                        anchor.offset.y(),
                    )
                    .await
                    .log_err()
                })
                .detach()
        }
        cx.notify();
    }

    pub fn show_scrollbar(&mut self, cx: &mut ViewContext<Editor>) {
        if !self.show_scrollbars {
            self.show_scrollbars = true;
            cx.notify();
        }

        if cx.default_global::<ScrollbarAutoHide>().0 {
            self.hide_scrollbar_task = Some(cx.spawn(|editor, mut cx| async move {
                cx.background().timer(SCROLLBAR_SHOW_INTERVAL).await;
                editor
                    .update(&mut cx, |editor, cx| {
                        editor.scroll_manager.show_scrollbars = false;
                        cx.notify();
                    })
                    .log_err();
            }));
        } else {
            self.hide_scrollbar_task = None;
        }
    }

    pub fn scrollbars_visible(&self) -> bool {
        self.show_scrollbars
    }

    pub fn has_autoscroll_request(&self) -> bool {
        self.autoscroll_request.is_some()
    }

    pub fn clamp_scroll_left(&mut self, max: f32) -> bool {
        if max < self.anchor.offset.x() {
            self.anchor.offset.set_x(max);
            true
        } else {
            false
        }
    }
}

impl Editor {
    pub fn vertical_scroll_margin(&mut self) -> usize {
        self.scroll_manager.vertical_scroll_margin as usize
    }

    pub fn set_vertical_scroll_margin(&mut self, margin_rows: usize, cx: &mut ViewContext<Self>) {
        self.scroll_manager.vertical_scroll_margin = margin_rows as f32;
        cx.notify();
    }

    pub fn visible_line_count(&self) -> Option<f32> {
        self.scroll_manager.visible_line_count
    }

    pub(crate) fn set_visible_line_count(&mut self, lines: f32) {
        self.scroll_manager.visible_line_count = Some(lines)
    }

    pub fn set_scroll_position(&mut self, scroll_position: Vector2F, cx: &mut ViewContext<Self>) {
        self.set_scroll_position_internal(scroll_position, true, cx);
    }

    pub(crate) fn set_scroll_position_internal(
        &mut self,
        scroll_position: Vector2F,
        local: bool,
        cx: &mut ViewContext<Self>,
    ) {
        let map = self.display_map.update(cx, |map, cx| map.snapshot(cx));

        hide_hover(self, &HideHover, cx);
        self.scroll_manager.set_scroll_position(
            scroll_position,
            &map,
            local,
            self.workspace_id,
            cx,
        );
    }

    pub fn scroll_position(&self, cx: &mut ViewContext<Self>) -> Vector2F {
        let display_map = self.display_map.update(cx, |map, cx| map.snapshot(cx));
        self.scroll_manager.anchor.scroll_position(&display_map)
    }

    pub fn set_scroll_anchor(&mut self, scroll_anchor: ScrollAnchor, cx: &mut ViewContext<Self>) {
        hide_hover(self, &HideHover, cx);
        let top_row = scroll_anchor
            .top_anchor
            .to_point(&self.buffer().read(cx).snapshot(cx))
            .row;
        self.scroll_manager
            .set_anchor(scroll_anchor, top_row, true, self.workspace_id, cx);
    }

    pub(crate) fn set_scroll_anchor_remote(
        &mut self,
        scroll_anchor: ScrollAnchor,
        cx: &mut ViewContext<Self>,
    ) {
        hide_hover(self, &HideHover, cx);
        let top_row = scroll_anchor
            .top_anchor
            .to_point(&self.buffer().read(cx).snapshot(cx))
            .row;
        self.scroll_manager
            .set_anchor(scroll_anchor, top_row, false, self.workspace_id, cx);
    }

    pub fn scroll_screen(&mut self, amount: &ScrollAmount, cx: &mut ViewContext<Self>) {
        if matches!(self.mode, EditorMode::SingleLine) {
            cx.propagate_action();
            return;
        }

        if self.take_rename(true, cx).is_some() {
            return;
        }

        if amount.move_context_menu_selection(self, cx) {
            return;
        }

        let cur_position = self.scroll_position(cx);
        let new_pos = cur_position + vec2f(0., amount.lines(self) - 1.);
        self.set_scroll_position(new_pos, cx);
    }

    /// Returns an ordering. The newest selection is:
    ///     Ordering::Equal => on screen
    ///     Ordering::Less => above the screen
    ///     Ordering::Greater => below the screen
    pub fn newest_selection_on_screen(&self, cx: &mut AppContext) -> Ordering {
        let snapshot = self.display_map.update(cx, |map, cx| map.snapshot(cx));
        let newest_head = self
            .selections
            .newest_anchor()
            .head()
            .to_display_point(&snapshot);
        let screen_top = self
            .scroll_manager
            .anchor
            .top_anchor
            .to_display_point(&snapshot);

        if screen_top > newest_head {
            return Ordering::Less;
        }

        if let Some(visible_lines) = self.visible_line_count() {
            if newest_head.row() < screen_top.row() + visible_lines as u32 {
                return Ordering::Equal;
            }
        }

        Ordering::Greater
    }

    pub fn read_scroll_position_from_db(
        &mut self,
        item_id: usize,
        workspace_id: WorkspaceId,
        cx: &mut ViewContext<Editor>,
    ) {
        let scroll_position = DB.get_scroll_position(item_id, workspace_id);
        if let Ok(Some((top_row, x, y))) = scroll_position {
            let top_anchor = self
                .buffer()
                .read(cx)
                .snapshot(cx)
                .anchor_at(Point::new(top_row as u32, 0), Bias::Left);
            let scroll_anchor = ScrollAnchor {
                offset: Vector2F::new(x, y),
                top_anchor,
            };
            self.set_scroll_anchor(scroll_anchor, cx);
        }
    }
}
