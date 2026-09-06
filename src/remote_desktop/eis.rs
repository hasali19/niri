//! EIS (Emulated Input Server) side of `Session.ConnectToEIS`.
//!
//! A remote desktop service gets a socket from `ConnectToEIS` and speaks the libei protocol over
//! it. We bind the device capabilities it asks for and turn the events it emits into
//! [`EiInputBackend`] events, which go through the normal niri input pipeline.

use std::collections::HashMap;
use std::ffi::CString;
use std::os::fd::AsFd as _;
use std::os::unix::net::UnixStream;

use anyhow::Context as _;
use reis::calloop::{EisRequestSource, EisRequestSourceEvent};
use reis::eis;
use reis::enumflags2::BitFlags;
use reis::request::{Connection, DeviceCapability, EisRequest};
use smithay::backend::input::{
    Axis, AxisSource, ButtonState, DeviceCapability as SmithayCapability, InputEvent, KeyState,
    TouchSlot,
};
use smithay::input::keyboard::xkb;
use smithay::input::pointer::AxisFrame;
use smithay::utils::{Logical, Point, Rectangle, SealedFile};

use super::input::{
    AbsolutePosition, EiButtonEvent, EiDevice, EiInputBackend, EiKeyboardKeyEvent,
    EiPointerAxisEvent, EiPointerMotionAbsoluteEvent, EiPointerMotionEvent, EiTouchCancelEvent,
    EiTouchDownEvent, EiTouchFrameEvent, EiTouchMotionEvent, EiTouchUpEvent,
};
use crate::niri::State;
use crate::utils::{id::IdCounter, RemoteDesktopSessionId};

/// Capabilities we advertise on the seat.
fn advertised_capabilities() -> BitFlags<DeviceCapability> {
    DeviceCapability::Pointer
        | DeviceCapability::PointerAbsolute
        | DeviceCapability::Keyboard
        | DeviceCapability::Touch
        | DeviceCapability::Scroll
        | DeviceCapability::Button
}

/// Per-connection EIS state, owned by the calloop source's closure.
#[derive(Default)]
pub struct EisConnectionState {
    seat: Option<reis::request::Seat>,
    /// Devices we created, by the reis device they correspond to.
    devices: Vec<(reis::request::Device, EiDevice)>,
    /// Scroll deltas accumulated since the last frame, per device.
    axis_frames: HashMap<u64, AxisFrame>,
    /// Whether a touch event happened since the last frame, per device.
    touch_frames: Vec<EiDevice>,
    /// Capabilities the client bound, remembered so a persistent session can recreate them.
    bound: BitFlags<DeviceCapability>,
}

/// Handle to a live EIS connection, so the session can tear it down.
pub struct EisConnection {
    pub token: calloop::RegistrationToken,
    /// Capabilities the client had bound.
    ///
    /// A persistent session keeps these so the same devices come back on reconnect, as the spec
    /// recommends.
    pub bound: BitFlags<DeviceCapability>,
}

fn next_device_id() -> u64 {
    static COUNTER: IdCounter = IdCounter::new();
    COUNTER.next()
}

impl State {
    /// Takes over the compositor end of a `ConnectToEIS` socket pair.
    pub fn remote_desktop_connect_eis(
        &mut self,
        session: RemoteDesktopSessionId,
        socket: UnixStream,
    ) -> anyhow::Result<()> {
        socket
            .set_nonblocking(true)
            .context("error setting the EIS socket non-blocking")?;
        let context = eis::Context::new(socket).context("error creating an EIS context")?;

        // Serial 1 matches what the reis examples use; it only has to be non-zero.
        let source = EisRequestSource::new(context, 1);

        let mut conn_state = EisConnectionState::default();
        let token = self
            .niri
            .event_loop
            .insert_source(source, move |event, connection, state: &mut State| {
                let action = match event {
                    Ok(event) => conn_state.handle_event(state, connection, session, event),
                    Err(err) => {
                        warn!("error communicating with the EIS client: {err}");
                        calloop::PostAction::Remove
                    }
                };

                if action == calloop::PostAction::Remove {
                    conn_state.destroy(state, session);
                }

                Ok(action)
            })
            .map_err(|err| anyhow::anyhow!("error inserting the EIS source: {err}"))?;

        self.niri.remote_desktop.set_eis(
            session,
            EisConnection {
                token,
                bound: BitFlags::empty(),
            },
        );

        Ok(())
    }
}

impl EisConnectionState {
    fn destroy(&mut self, state: &mut State, session: RemoteDesktopSessionId) {
        self.devices.clear();
        self.seat = None;
        state.niri.remote_desktop.take_eis(session, self.bound);
    }

    fn handle_event(
        &mut self,
        state: &mut State,
        connection: &Connection,
        session: RemoteDesktopSessionId,
        event: EisRequestSourceEvent,
    ) -> calloop::PostAction {
        match event {
            EisRequestSourceEvent::Connected => {
                self.seat = Some(connection.add_seat(Some("niri"), advertised_capabilities()));
            }
            EisRequestSourceEvent::Request(EisRequest::Disconnect) => {
                return calloop::PostAction::Remove;
            }
            EisRequestSourceEvent::Request(EisRequest::Bind(bind)) => {
                self.bind(state, connection, session, bind.capabilities);
            }
            EisRequestSourceEvent::Request(EisRequest::DeviceClosed(closed)) => {
                self.devices.retain(|(device, _)| device != &closed.device);
            }
            EisRequestSourceEvent::Request(EisRequest::Frame(frame)) => {
                self.flush_frame(state, &frame.device, frame.time);
            }
            EisRequestSourceEvent::Request(request) => {
                self.handle_input(state, request);
            }
        }

        let _ = connection.flush();

        calloop::PostAction::Continue
    }

    fn bind(
        &mut self,
        state: &mut State,
        connection: &Connection,
        session: RemoteDesktopSessionId,
        capabilities: BitFlags<DeviceCapability>,
    ) {
        let Some(seat) = self.seat.clone() else {
            return;
        };

        // Unbinding a capability removes the devices that provided it.
        let removed = self.bound & !capabilities;
        if !removed.is_empty() {
            self.devices.retain(|(device, ei_device)| {
                let keep = !removed.iter().any(|cap| device.has_capability(cap));
                if !keep {
                    device.remove();
                    ei_device.set_output(None);
                }
                keep
            });
        }

        self.bound = capabilities;
        state
            .niri
            .remote_desktop
            .set_eis_bound(session, capabilities);

        // A pointer and a keyboard are separate devices; a relative and an absolute pointer are
        // too, because niri handles them through different code paths.
        struct DeviceKind {
            name: &'static str,
            /// The capability that defines this device.
            primary: DeviceCapability,
            /// Capabilities that ride along when the client asked for them.
            extra: BitFlags<DeviceCapability>,
            smithay: SmithayCapability,
        }

        let kinds = [
            DeviceKind {
                name: "niri remote pointer",
                primary: DeviceCapability::Pointer,
                extra: DeviceCapability::Button | DeviceCapability::Scroll,
                smithay: SmithayCapability::Pointer,
            },
            DeviceKind {
                name: "niri remote absolute pointer",
                primary: DeviceCapability::PointerAbsolute,
                extra: DeviceCapability::Button | DeviceCapability::Scroll,
                smithay: SmithayCapability::Pointer,
            },
            DeviceKind {
                name: "niri remote keyboard",
                primary: DeviceCapability::Keyboard,
                extra: BitFlags::empty(),
                smithay: SmithayCapability::Keyboard,
            },
            DeviceKind {
                name: "niri remote touchscreen",
                primary: DeviceCapability::Touch,
                extra: BitFlags::empty(),
                smithay: SmithayCapability::Touch,
            },
        ];

        for kind in kinds {
            if !capabilities.contains(kind.primary) {
                continue;
            }
            if self
                .devices
                .iter()
                .any(|(device, _)| device.has_capability(kind.primary))
            {
                continue;
            }

            let caps = (BitFlags::from(kind.primary) | kind.extra) & capabilities;
            let absolute = matches!(
                kind.primary,
                DeviceCapability::PointerAbsolute | DeviceCapability::Touch
            );
            let regions = if absolute {
                region_list(state)
            } else {
                Vec::new()
            };
            let keymap = if kind.primary == DeviceCapability::Keyboard {
                keymap_file(state)
            } else {
                None
            };

            let device = seat.add_device(
                Some(kind.name),
                eis::device::DeviceType::Virtual,
                caps,
                |device| {
                    // Absolute coordinates are niri's global logical coordinates, so the regions
                    // are just the output geometries.
                    for region in &regions {
                        device.device().region(
                            region.loc.x.max(0) as u32,
                            region.loc.y.max(0) as u32,
                            region.size.w.max(0) as u32,
                            region.size.h.max(0) as u32,
                            1.,
                        );
                    }

                    if let (Some(keyboard), Some((file, size))) =
                        (device.interface::<eis::Keyboard>(), &keymap)
                    {
                        keyboard.keymap(eis::keyboard::KeymapType::Xkb, *size as u32, file.as_fd());
                    }
                },
            );
            device.resumed();

            if connection.context_type() == eis::handshake::ContextType::Receiver {
                // A receiver context means the client wants us to send it events, which is input
                // capture, not what this API does.
                device.remove();
                continue;
            }

            let ei_device =
                EiDevice::new(next_device_id(), kind.name.to_owned(), vec![kind.smithay]);

            state.process_input_event(InputEvent::<EiInputBackend>::DeviceAdded {
                device: ei_device.clone(),
            });

            self.devices.push((device, ei_device));
        }
    }

    fn device_for(&self, request_device: &reis::request::Device) -> Option<&EiDevice> {
        self.devices
            .iter()
            .find(|(device, _)| device == request_device)
            .map(|(_, ei_device)| ei_device)
    }

    fn axis_frame(&mut self, device: &EiDevice, time: u64) -> &mut AxisFrame {
        let key = device.key();
        self.axis_frames
            .entry(key)
            .or_insert_with(|| AxisFrame::new((time / 1000) as u32).source(AxisSource::Continuous))
    }

    fn handle_input(&mut self, state: &mut State, request: EisRequest) {
        macro_rules! device {
            ($request:expr) => {
                match self.device_for(&$request.device) {
                    Some(device) => device.clone(),
                    None => return,
                }
            };
        }

        match request {
            EisRequest::PointerMotion(event) => {
                let device = device!(event);
                device.set_output(None);
                state.process_input_event(InputEvent::<EiInputBackend>::PointerMotion {
                    event: EiPointerMotionEvent {
                        device,
                        time: event.time,
                        dx: f64::from(event.dx),
                        dy: f64::from(event.dy),
                    },
                });
            }
            EisRequest::PointerMotionAbsolute(event) => {
                let device = device!(event);
                let global =
                    Point::from((f64::from(event.dx_absolute), f64::from(event.dy_absolute)));
                let Some(pos) = global_position(state, global) else {
                    return;
                };
                // niri maps the absolute pointer over the bounding rectangle of all outputs, so
                // the device deliberately has no output of its own.
                device.set_output(None);
                state.process_input_event(InputEvent::<EiInputBackend>::PointerMotionAbsolute {
                    event: EiPointerMotionAbsoluteEvent {
                        device,
                        time: event.time,
                        pos,
                    },
                });
            }
            EisRequest::Button(event) => {
                let device = device!(event);
                device.set_output(None);
                let button_state = match event.state {
                    eis::button::ButtonState::Press => ButtonState::Pressed,
                    eis::button::ButtonState::Released => ButtonState::Released,
                };
                state.process_input_event(InputEvent::<EiInputBackend>::PointerButton {
                    event: EiButtonEvent {
                        device,
                        time: event.time,
                        button: event.button,
                        state: button_state,
                    },
                });
            }
            EisRequest::ScrollDelta(event) => {
                let device = device!(event);
                let frame = self.axis_frame(&device, event.time);
                *frame = frame
                    .value(Axis::Horizontal, f64::from(event.dx))
                    .value(Axis::Vertical, f64::from(event.dy));
            }
            EisRequest::ScrollDiscrete(event) => {
                let device = device!(event);
                let frame = self.axis_frame(&device, event.time);
                *frame = frame.source(AxisSource::Wheel);
                if event.discrete_dx != 0 {
                    *frame = frame.v120(Axis::Horizontal, event.discrete_dx);
                }
                if event.discrete_dy != 0 {
                    *frame = frame.v120(Axis::Vertical, event.discrete_dy);
                }
            }
            EisRequest::ScrollStop(event) => {
                let device = device!(event);
                let frame = self.axis_frame(&device, event.time);
                if event.x {
                    *frame = frame.stop(Axis::Horizontal);
                }
                if event.y {
                    *frame = frame.stop(Axis::Vertical);
                }
            }
            EisRequest::ScrollCancel(event) => {
                let device = device!(event);
                let frame = self.axis_frame(&device, event.time);
                if event.x {
                    *frame = frame.stop(Axis::Horizontal);
                }
                if event.y {
                    *frame = frame.stop(Axis::Vertical);
                }
            }
            EisRequest::KeyboardKey(event) => {
                let device = device!(event);
                let key_state = match event.state {
                    eis::keyboard::KeyState::Press => KeyState::Pressed,
                    eis::keyboard::KeyState::Released => KeyState::Released,
                };
                state.process_input_event(InputEvent::<EiInputBackend>::Keyboard {
                    event: EiKeyboardKeyEvent {
                        device,
                        time: event.time,
                        key: event.key,
                        state: key_state,
                        count: 1,
                    },
                });
            }
            EisRequest::TouchDown(event) => {
                let device = device!(event);
                let global = Point::from((f64::from(event.x), f64::from(event.y)));
                let Some(pos) = output_position(state, &device, global) else {
                    return;
                };
                self.touch_frames.push(device.clone());
                state.process_input_event(InputEvent::<EiInputBackend>::TouchDown {
                    event: EiTouchDownEvent {
                        device,
                        time: event.time,
                        slot: TouchSlot::from(Some(event.touch_id)),
                        pos,
                    },
                });
            }
            EisRequest::TouchMotion(event) => {
                let device = device!(event);
                let global = Point::from((f64::from(event.x), f64::from(event.y)));
                let Some(pos) = output_position(state, &device, global) else {
                    return;
                };
                self.touch_frames.push(device.clone());
                state.process_input_event(InputEvent::<EiInputBackend>::TouchMotion {
                    event: EiTouchMotionEvent {
                        device,
                        time: event.time,
                        slot: TouchSlot::from(Some(event.touch_id)),
                        pos,
                    },
                });
            }
            EisRequest::TouchUp(event) => {
                let device = device!(event);
                self.touch_frames.push(device.clone());
                state.process_input_event(InputEvent::<EiInputBackend>::TouchUp {
                    event: EiTouchUpEvent {
                        device,
                        time: event.time,
                        slot: TouchSlot::from(Some(event.touch_id)),
                    },
                });
            }
            EisRequest::TouchCancel(event) => {
                let device = device!(event);
                self.touch_frames.push(device.clone());
                state.process_input_event(InputEvent::<EiInputBackend>::TouchCancel {
                    event: EiTouchCancelEvent {
                        device,
                        time: event.time,
                        slot: TouchSlot::from(Some(event.touch_id)),
                    },
                });
            }
            _ => (),
        }
    }

    /// Emits the events that libei groups into a frame.
    fn flush_frame(
        &mut self,
        state: &mut State,
        request_device: &reis::request::Device,
        time: u64,
    ) {
        let Some(device) = self.device_for(request_device).cloned() else {
            return;
        };

        if let Some(frame) = self.axis_frames.remove(&device.key()) {
            device.set_output(None);
            state.process_input_event(InputEvent::<EiInputBackend>::PointerAxis {
                event: EiPointerAxisEvent {
                    device: device.clone(),
                    time,
                    frame,
                },
            });
        }

        if let Some(index) = self.touch_frames.iter().position(|d| *d == device) {
            self.touch_frames.retain(|d| *d != device);
            let _ = index;
            state.process_input_event(InputEvent::<EiInputBackend>::TouchFrame {
                event: EiTouchFrameEvent { device, time },
            });
        }
    }
}

/// The regions we advertise: one per output, in niri's global logical coordinates.
fn region_list(state: &State) -> Vec<Rectangle<i32, Logical>> {
    state
        .niri
        .global_space
        .outputs()
        .filter_map(|output| state.niri.global_space.output_geometry(output))
        .collect()
}

/// Maps a global logical point into the coordinate space niri uses for absolute pointer events.
fn global_position(state: &State, global: Point<f64, Logical>) -> Option<AbsolutePosition> {
    // `on_pointer_motion_absolute` falls back to the bounding rectangle of all outputs when the
    // device has no output, and that fallback is an exact identity for us: it computes
    // `position_transformed(bounds.size) + bounds.loc`.
    let bounds = state.global_bounding_rectangle()?;
    Some(AbsolutePosition {
        position: global - bounds.loc.to_f64(),
        space: bounds.size,
    })
}

/// Maps a global logical point onto the output that contains it, for touch.
///
/// Touch needs a specific output, since `compute_touch_location` falls back to
/// `output_for_touch()` rather than the global bounding rectangle.
fn output_position(
    state: &State,
    device: &EiDevice,
    global: Point<f64, Logical>,
) -> Option<AbsolutePosition> {
    let (output, _) = state.niri.output_under(global)?;
    let output = output.clone();
    let geo = state.niri.global_space.output_geometry(&output)?;

    // `compute_absolute_location` undoes the output transform:
    //     size = transform.invert().transform_size(geo.size)
    //     pos  = transform.transform_point_in(position_transformed(size), size) + geo.loc
    // so we have to hand it the point in the untransformed space.
    let transform = output.current_transform();
    let space = transform.invert().transform_size(geo.size);
    let local = global - geo.loc.to_f64();
    let position = transform
        .invert()
        .transform_point_in(local, &geo.size.to_f64());

    device.set_output(Some(output));

    Some(AbsolutePosition { position, space })
}

/// The current keymap, as a sealed file suitable for `ei_keyboard.keymap`.
fn keymap_file(state: &mut State) -> Option<(SealedFile, usize)> {
    let keyboard = state.niri.seat.get_keyboard()?;
    let keymap = keyboard.with_xkb_state(state, |context| {
        let xkb = context.xkb().lock().unwrap();
        // Safety: we only produce an owned String, no reference outlives the lock.
        unsafe { xkb.keymap() }.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1)
    });

    let contents = CString::new(keymap).ok()?;
    match SealedFile::with_content(c"niri-remote-desktop-keymap", &contents) {
        Ok(file) => {
            let size = file.size();
            Some((file, size))
        }
        Err(err) => {
            warn!("error creating the EIS keymap file: {err:?}");
            None
        }
    }
}
