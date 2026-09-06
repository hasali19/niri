//! A smithay input backend for events coming from an EIS client.
//!
//! Injected events go through [`State::process_input_event`] like any other input, so remote input
//! triggers keybinds, wakes monitors, notifies the idle notifier and updates focus-follows-mouse
//! exactly like local input does. This is the same approach `wlr-virtual-pointer` takes in
//! [`crate::protocols::virtual_pointer`], extended with keyboard and touch.

use std::cell::RefCell;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::rc::Rc;

use smithay::backend::input::{
    AbsolutePositionEvent, Axis, AxisRelativeDirection, AxisSource, ButtonState, Device,
    DeviceCapability, Event, InputBackend, KeyState, KeyboardKeyEvent, Keycode, PointerAxisEvent,
    PointerButtonEvent, PointerMotionAbsoluteEvent, PointerMotionEvent, TouchCancelEvent,
    TouchDownEvent, TouchEvent, TouchFrameEvent, TouchMotionEvent, TouchSlot, TouchUpEvent,
    UnusedEvent,
};
use smithay::input::pointer::AxisFrame;
use smithay::output::Output;
use smithay::utils::{Logical, Point, Size};

use crate::input::backend_ext::NiriInputDevice;
use crate::niri::State;

/// Input backend for events emitted by an EIS client.
#[derive(Debug)]
pub struct EiInputBackend;

/// One EIS device (a pointer, keyboard or touchscreen bound by the client).
#[derive(Debug, Clone)]
pub struct EiDevice {
    inner: Rc<EiDeviceInner>,
}

#[derive(Debug)]
struct EiDeviceInner {
    id: u64,
    name: String,
    capabilities: Vec<DeviceCapability>,
    /// Output the absolute event being dispatched right now maps into.
    ///
    /// EIS clients send absolute coordinates in one flat region that covers all outputs, so which
    /// output an event lands on is a property of the event, not the device. niri asks the device,
    /// so this is set immediately before dispatching each event.
    output: RefCell<Option<Output>>,
}

impl EiDevice {
    pub fn new(id: u64, name: String, capabilities: Vec<DeviceCapability>) -> Self {
        Self {
            inner: Rc::new(EiDeviceInner {
                id,
                name,
                capabilities,
                output: RefCell::new(None),
            }),
        }
    }

    pub fn set_output(&self, output: Option<Output>) {
        *self.inner.output.borrow_mut() = output;
    }

    /// Stable per-device key, for keeping per-device state on the side.
    pub fn key(&self) -> u64 {
        self.inner.id
    }
}

impl PartialEq for EiDevice {
    fn eq(&self, other: &Self) -> bool {
        self.inner.id == other.inner.id
    }
}

impl Eq for EiDevice {}

impl Hash for EiDevice {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.inner.id.hash(state);
    }
}

impl Device for EiDevice {
    fn id(&self) -> String {
        format!("eis device {}", self.inner.id)
    }

    fn name(&self) -> String {
        self.inner.name.clone()
    }

    fn has_capability(&self, capability: DeviceCapability) -> bool {
        self.inner.capabilities.contains(&capability)
    }

    fn usb_id(&self) -> Option<(u32, u32)> {
        None
    }

    fn syspath(&self) -> Option<PathBuf> {
        None
    }
}

impl NiriInputDevice for EiDevice {
    fn output(&self, _state: &State) -> Option<Output> {
        self.inner.output.borrow().clone()
    }
}

/// Position of an absolute event, already resolved to a coordinate space.
///
/// `position` is expressed in `space`, which is the untransformed size of the target: either an
/// output (for touch, where niri needs a specific output) or the bounding rectangle of all outputs
/// (for the absolute pointer, which niri maps in global coordinates).
#[derive(Debug, Clone, Copy)]
pub struct AbsolutePosition {
    pub position: Point<f64, Logical>,
    pub space: Size<i32, Logical>,
}

macro_rules! impl_event {
    ($ty:ident) => {
        impl Event<EiInputBackend> for $ty {
            fn time(&self) -> u64 {
                self.time
            }

            fn device(&self) -> EiDevice {
                self.device.clone()
            }
        }
    };
}

macro_rules! impl_absolute {
    ($ty:ident) => {
        impl AbsolutePositionEvent<EiInputBackend> for $ty {
            fn x(&self) -> f64 {
                self.pos.position.x
            }

            fn y(&self) -> f64 {
                self.pos.position.y
            }

            fn x_transformed(&self, width: i32) -> f64 {
                if self.pos.space.w == 0 {
                    return 0.;
                }
                self.pos.position.x * f64::from(width) / f64::from(self.pos.space.w)
            }

            fn y_transformed(&self, height: i32) -> f64 {
                if self.pos.space.h == 0 {
                    return 0.;
                }
                self.pos.position.y * f64::from(height) / f64::from(self.pos.space.h)
            }
        }
    };
}

#[derive(Debug)]
pub struct EiPointerMotionEvent {
    pub device: EiDevice,
    /// Microseconds.
    pub time: u64,
    pub dx: f64,
    pub dy: f64,
}

impl_event!(EiPointerMotionEvent);

impl PointerMotionEvent<EiInputBackend> for EiPointerMotionEvent {
    fn delta_x(&self) -> f64 {
        self.dx
    }

    fn delta_y(&self) -> f64 {
        self.dy
    }

    fn delta_x_unaccel(&self) -> f64 {
        self.dx
    }

    fn delta_y_unaccel(&self) -> f64 {
        self.dy
    }
}

#[derive(Debug)]
pub struct EiPointerMotionAbsoluteEvent {
    pub device: EiDevice,
    pub time: u64,
    pub pos: AbsolutePosition,
}

impl_event!(EiPointerMotionAbsoluteEvent);
impl_absolute!(EiPointerMotionAbsoluteEvent);
impl PointerMotionAbsoluteEvent<EiInputBackend> for EiPointerMotionAbsoluteEvent {}

#[derive(Debug)]
pub struct EiButtonEvent {
    pub device: EiDevice,
    pub time: u64,
    pub button: u32,
    pub state: ButtonState,
}

impl_event!(EiButtonEvent);

impl PointerButtonEvent<EiInputBackend> for EiButtonEvent {
    fn button_code(&self) -> u32 {
        self.button
    }

    fn state(&self) -> ButtonState {
        self.state
    }
}

#[derive(Debug)]
pub struct EiPointerAxisEvent {
    pub device: EiDevice,
    pub time: u64,
    pub frame: AxisFrame,
}

impl_event!(EiPointerAxisEvent);

fn tuple_axis<T>(tuple: (T, T), axis: Axis) -> T {
    match axis {
        Axis::Horizontal => tuple.0,
        Axis::Vertical => tuple.1,
    }
}

impl PointerAxisEvent<EiInputBackend> for EiPointerAxisEvent {
    fn amount(&self, axis: Axis) -> Option<f64> {
        Some(tuple_axis(self.frame.axis, axis))
    }

    fn amount_v120(&self, axis: Axis) -> Option<f64> {
        self.frame
            .v120
            .map(|v120| f64::from(tuple_axis(v120, axis)))
    }

    fn source(&self) -> AxisSource {
        self.frame.source.unwrap_or(AxisSource::Continuous)
    }

    fn relative_direction(&self, axis: Axis) -> AxisRelativeDirection {
        tuple_axis(self.frame.relative_direction, axis)
    }
}

#[derive(Debug)]
pub struct EiKeyboardKeyEvent {
    pub device: EiDevice,
    pub time: u64,
    /// Evdev key code, as sent by the client.
    pub key: u32,
    pub state: KeyState,
    pub count: u32,
}

impl_event!(EiKeyboardKeyEvent);

impl KeyboardKeyEvent<EiInputBackend> for EiKeyboardKeyEvent {
    fn key_code(&self) -> Keycode {
        // libei sends evdev codes; xkb keycodes are offset by 8.
        Keycode::new(self.key + 8)
    }

    fn state(&self) -> KeyState {
        self.state
    }

    fn count(&self) -> u32 {
        self.count
    }
}

#[derive(Debug)]
pub struct EiTouchDownEvent {
    pub device: EiDevice,
    pub time: u64,
    pub slot: TouchSlot,
    pub pos: AbsolutePosition,
}

impl_event!(EiTouchDownEvent);
impl_absolute!(EiTouchDownEvent);

impl TouchEvent<EiInputBackend> for EiTouchDownEvent {
    fn slot(&self) -> TouchSlot {
        self.slot
    }
}

impl TouchDownEvent<EiInputBackend> for EiTouchDownEvent {}

#[derive(Debug)]
pub struct EiTouchMotionEvent {
    pub device: EiDevice,
    pub time: u64,
    pub slot: TouchSlot,
    pub pos: AbsolutePosition,
}

impl_event!(EiTouchMotionEvent);
impl_absolute!(EiTouchMotionEvent);

impl TouchEvent<EiInputBackend> for EiTouchMotionEvent {
    fn slot(&self) -> TouchSlot {
        self.slot
    }
}

impl TouchMotionEvent<EiInputBackend> for EiTouchMotionEvent {}

#[derive(Debug)]
pub struct EiTouchUpEvent {
    pub device: EiDevice,
    pub time: u64,
    pub slot: TouchSlot,
}

impl_event!(EiTouchUpEvent);

impl TouchEvent<EiInputBackend> for EiTouchUpEvent {
    fn slot(&self) -> TouchSlot {
        self.slot
    }
}

impl TouchUpEvent<EiInputBackend> for EiTouchUpEvent {}

#[derive(Debug)]
pub struct EiTouchCancelEvent {
    pub device: EiDevice,
    pub time: u64,
    pub slot: TouchSlot,
}

impl_event!(EiTouchCancelEvent);

impl TouchEvent<EiInputBackend> for EiTouchCancelEvent {
    fn slot(&self) -> TouchSlot {
        self.slot
    }
}

impl TouchCancelEvent<EiInputBackend> for EiTouchCancelEvent {}

#[derive(Debug)]
pub struct EiTouchFrameEvent {
    pub device: EiDevice,
    pub time: u64,
}

impl_event!(EiTouchFrameEvent);
impl TouchFrameEvent<EiInputBackend> for EiTouchFrameEvent {}

impl InputBackend for EiInputBackend {
    type Device = EiDevice;

    type KeyboardKeyEvent = EiKeyboardKeyEvent;
    type PointerAxisEvent = EiPointerAxisEvent;
    type PointerButtonEvent = EiButtonEvent;
    type PointerMotionEvent = EiPointerMotionEvent;
    type PointerMotionAbsoluteEvent = EiPointerMotionAbsoluteEvent;

    type GestureSwipeBeginEvent = UnusedEvent;
    type GestureSwipeUpdateEvent = UnusedEvent;
    type GestureSwipeEndEvent = UnusedEvent;
    type GesturePinchBeginEvent = UnusedEvent;
    type GesturePinchUpdateEvent = UnusedEvent;
    type GesturePinchEndEvent = UnusedEvent;
    type GestureHoldBeginEvent = UnusedEvent;
    type GestureHoldEndEvent = UnusedEvent;

    type TouchDownEvent = EiTouchDownEvent;
    type TouchUpEvent = EiTouchUpEvent;
    type TouchMotionEvent = EiTouchMotionEvent;
    type TouchCancelEvent = EiTouchCancelEvent;
    type TouchFrameEvent = EiTouchFrameEvent;

    type TabletToolAxisEvent = UnusedEvent;
    type TabletToolProximityEvent = UnusedEvent;
    type TabletToolTipEvent = UnusedEvent;
    type TabletToolButtonEvent = UnusedEvent;

    type SwitchToggleEvent = UnusedEvent;

    type SpecialEvent = UnusedEvent;
}
