use std::time::Duration;

use smithay::backend::input::{ButtonState, InputTime};
use smithay::input::pointer::{
    AxisFrame, ButtonEvent, CursorIcon, CursorImageStatus, GestureHoldBeginEvent,
    GestureHoldEndEvent, GesturePinchBeginEvent, GesturePinchEndEvent, GesturePinchUpdateEvent,
    GestureSwipeBeginEvent, GestureSwipeEndEvent, GestureSwipeUpdateEvent,
    GrabStartData as PointerGrabStartData, MotionEvent, PointerGrab, PointerInnerHandle,
    RelativeMotionEvent,
};
use smithay::input::tablet::tool::{TabletToolGrab, TabletToolInnerHandle};
use smithay::input::tablet::TabletSeatHandler;
use smithay::input::touch::{
    self, GrabStartData as TouchGrabStartData, TouchGrab, TouchInnerHandle,
};
use smithay::input::{tablet, SeatHandler};
use smithay::output::Output;
use smithay::utils::{Logical, Point, SERIAL_COUNTER};

use crate::input::AnyStartData;
use crate::layout::workspace::WorkspaceId;
use crate::niri::State;

/// Grab for dragging a workspace by its handle in the overview.
pub struct WorkspaceDragGrab {
    start_data: AnyStartData<State>,
    cancelled: bool,

    // Accumulated and applied in frame().
    new_location: Point<f64, Logical>,
    event_timestamp: Option<Duration>,
}

impl WorkspaceDragGrab {
    pub fn new(
        state: &mut State,
        start_data: AnyStartData<State>,
        output: Output,
        pos_within_output: Point<f64, Logical>,
        workspace_id: WorkspaceId,
    ) -> Option<Self> {
        if !state
            .niri
            .layout
            .workspace_drag_begin(workspace_id, output, pos_within_output)
        {
            return None;
        }

        if !start_data.is_touch() {
            state
                .niri
                .cursor_manager
                .set_cursor_image(CursorImageStatus::Named(CursorIcon::Grabbing));
        }

        // FIXME: only redraw the relevant output.
        state.niri.queue_redraw_all();

        let location = start_data.location();
        Some(Self {
            start_data,
            cancelled: false,
            new_location: location,
            event_timestamp: None,
        })
    }

    fn on_ungrab(&mut self, data: &mut State) {
        data.niri.layout.workspace_drag_end(!self.cancelled);

        if !self.start_data.is_touch() {
            data.niri
                .cursor_manager
                .set_cursor_image(CursorImageStatus::default_named());
        }

        // FIXME: only redraw the relevant outputs.
        data.niri.queue_redraw_all();
    }

    fn on_frame(&mut self, data: &mut State) -> bool {
        if self.event_timestamp.take().is_none() {
            return true;
        }

        let Some((output, pos_within_output)) = data.niri.output_under(self.new_location) else {
            return true;
        };
        let output = output.clone();

        if !data
            .niri
            .layout
            .workspace_drag_update(output, pos_within_output)
        {
            return false;
        }

        // FIXME: only redraw the previous and the new output.
        data.niri.queue_redraw_all();
        true
    }
}

impl PointerGrab<State> for WorkspaceDragGrab {
    fn motion(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        _focus: Option<(<State as SeatHandler>::PointerFocus, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        // While the grab is active, no client has pointer focus.
        handle.motion(data, None, event);

        self.new_location = event.location;
        self.event_timestamp = Some(Duration::from_micros(event.time.micros()));
    }

    fn relative_motion(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        _focus: Option<(<State as SeatHandler>::PointerFocus, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        // While the grab is active, no client has pointer focus.
        handle.relative_motion(data, None, event);
    }

    fn button(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);

        if event.state == ButtonState::Released
            && !handle
                .current_pressed()
                .contains(&self.start_data.unwrap_pointer().button)
        {
            // The button that initiated the grab was released.
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    fn axis(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        details: AxisFrame,
    ) {
        handle.axis(data, details);
    }

    fn frame(&mut self, data: &mut State, handle: &mut PointerInnerHandle<'_, State>) {
        handle.frame(data);

        if !self.on_frame(data) {
            // The gesture is no longer ongoing.
            handle.unset_grab(
                self,
                data,
                SERIAL_COUNTER.next_serial(),
                InputTime::now(),
                true,
            );
        }
    }

    fn gesture_swipe_begin(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureSwipeBeginEvent,
    ) {
        handle.gesture_swipe_begin(data, event);
    }

    fn gesture_swipe_update(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureSwipeUpdateEvent,
    ) {
        handle.gesture_swipe_update(data, event);
    }

    fn gesture_swipe_end(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureSwipeEndEvent,
    ) {
        handle.gesture_swipe_end(data, event);
    }

    fn gesture_pinch_begin(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GesturePinchBeginEvent,
    ) {
        handle.gesture_pinch_begin(data, event);
    }

    fn gesture_pinch_update(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GesturePinchUpdateEvent,
    ) {
        handle.gesture_pinch_update(data, event);
    }

    fn gesture_pinch_end(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GesturePinchEndEvent,
    ) {
        handle.gesture_pinch_end(data, event);
    }

    fn gesture_hold_begin(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureHoldBeginEvent,
    ) {
        handle.gesture_hold_begin(data, event);
    }

    fn gesture_hold_end(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureHoldEndEvent,
    ) {
        handle.gesture_hold_end(data, event);
    }

    fn start_data(&self) -> &PointerGrabStartData<State> {
        self.start_data.unwrap_pointer()
    }

    fn unset(&mut self, data: &mut State) {
        self.on_ungrab(data);
    }
}

impl TouchGrab<State> for WorkspaceDragGrab {
    fn down(
        &mut self,
        data: &mut State,
        handle: &mut TouchInnerHandle<'_, State>,
        _focus: Option<(<State as SeatHandler>::TouchFocus, Point<f64, Logical>)>,
        event: &touch::DownEvent,
    ) {
        handle.down(data, None, event);
    }

    fn up(
        &mut self,
        data: &mut State,
        handle: &mut TouchInnerHandle<'_, State>,
        event: &touch::UpEvent,
    ) {
        handle.up(data, event);

        if event.slot == self.start_data.unwrap_touch().slot {
            handle.unset_grab(self, data);
        }
    }

    fn motion(
        &mut self,
        data: &mut State,
        handle: &mut TouchInnerHandle<'_, State>,
        _focus: Option<(<State as SeatHandler>::TouchFocus, Point<f64, Logical>)>,
        event: &touch::MotionEvent,
    ) {
        handle.motion(data, None, event);

        if event.slot != self.start_data.unwrap_touch().slot {
            return;
        }

        self.new_location = event.location;
        self.event_timestamp = Some(Duration::from_micros(event.time.micros()));
    }

    fn frame(&mut self, data: &mut State, handle: &mut TouchInnerHandle<'_, State>) {
        handle.frame(data);

        if !self.on_frame(data) {
            // The gesture is no longer ongoing.
            handle.unset_grab(self, data);
        }
    }

    fn cancel(&mut self, data: &mut State, handle: &mut TouchInnerHandle<'_, State>) {
        self.cancelled = true;
        handle.cancel(data);
        handle.unset_grab(self, data);
    }

    fn shape(
        &mut self,
        data: &mut State,
        handle: &mut TouchInnerHandle<'_, State>,
        event: &touch::ShapeEvent,
    ) {
        handle.shape(data, event);
    }

    fn orientation(
        &mut self,
        data: &mut State,
        handle: &mut TouchInnerHandle<'_, State>,
        event: &touch::OrientationEvent,
    ) {
        handle.orientation(data, event);
    }

    fn start_data(&self) -> &TouchGrabStartData<State> {
        self.start_data.unwrap_touch()
    }

    fn unset(&mut self, data: &mut State) {
        self.on_ungrab(data);
    }
}

impl TabletToolGrab<State> for WorkspaceDragGrab {
    fn start_data(&self) -> &tablet::tool::GrabStartData<State> {
        self.start_data.unwrap_tablet_tool()
    }

    fn proximity_out(
        &mut self,
        data: &mut State,
        handle: &mut TabletToolInnerHandle<'_, State>,
        event: &tablet::tool::ProximityOutEvent,
    ) {
        handle.proximity_out(data, event);
        handle.unset_grab(self, data, event.serial, event.time, false);
    }

    fn motion(
        &mut self,
        data: &mut State,
        handle: &mut TabletToolInnerHandle<'_, State>,
        _focus: Option<(<State as TabletSeatHandler>::ToolFocus, Point<f64, Logical>)>,
        event: &tablet::tool::MotionEvent,
    ) {
        handle.motion(data, None, event);

        self.new_location = event.location;
        self.event_timestamp = Some(Duration::from_micros(event.time.micros()));
    }

    fn down(
        &mut self,
        data: &mut State,
        handle: &mut TabletToolInnerHandle<'_, State>,
        event: &tablet::tool::DownEvent,
    ) {
        handle.down(data, event);
    }

    fn up(
        &mut self,
        data: &mut State,
        handle: &mut TabletToolInnerHandle<'_, State>,
        event: &tablet::tool::UpEvent,
    ) {
        handle.up(data, event);
        handle.unset_grab(self, data, event.serial, event.time, true);
    }

    fn button(
        &mut self,
        data: &mut State,
        handle: &mut TabletToolInnerHandle<'_, State>,
        event: &tablet::tool::ButtonEvent,
    ) {
        handle.button(data, event);
    }

    fn axis(
        &mut self,
        data: &mut State,
        handle: &mut TabletToolInnerHandle<'_, State>,
        frame: tablet::tool::AxisFrame,
    ) {
        handle.axis(data, frame);
    }

    fn frame(
        &mut self,
        data: &mut State,
        handle: &mut TabletToolInnerHandle<'_, State>,
        time: InputTime,
    ) {
        handle.frame(data, time);

        if !self.on_frame(data) {
            // The gesture is no longer ongoing.
            handle.unset_grab(
                self,
                data,
                SERIAL_COUNTER.next_serial(),
                InputTime::now(),
                true,
            );
        }
    }

    fn unset(&mut self, data: &mut State) {
        self.on_ungrab(data);
    }
}
