use std::collections::HashMap;

use smithay::backend::input::{
    AbsolutePositionEvent as _, ButtonState, InputEvent, KeyState, KeyboardKeyEvent as _,
};
use smithay::utils::Point;

use super::*;
use crate::dbus::remote_desktop::{
    ClipboardType, CursorMode, NiriToRemoteDesktop, RemoteDesktopToNiri,
};
use crate::remote_desktop::eis::{global_position, output_position};
use crate::remote_desktop::input::{
    EiButtonEvent, EiDevice, EiInputBackend, EiKeyboardKeyEvent, EiPointerMotionAbsoluteEvent,
};
use crate::utils::{RemoteDesktopMonitorId, RemoteDesktopSessionId};

/// Turns the API on for a fixture, and returns the channel the compositor notifies the D-Bus side
/// through.
fn enable(f: &mut Fixture) -> async_channel::Receiver<NiriToRemoteDesktop> {
    let (to_dbus, from_niri) = async_channel::unbounded();
    let state = f.niri_state();
    state.niri.remote_desktop.to_dbus = Some(to_dbus);
    state.niri.remote_desktop.default_mode = Some(niri_ipc::ConfiguredMode {
        width: 1280,
        height: 720,
        refresh: Some(60.),
    });
    from_niri
}

fn create_session(f: &mut Fixture, persistent: bool) -> RemoteDesktopSessionId {
    let id = RemoteDesktopSessionId::next();
    let (reply, rx) = async_channel::bounded(1);
    f.niri_state()
        .on_remote_desktop_msg(RemoteDesktopToNiri::CreateSession {
            id,
            persistent,
            takes_control: true,
            disable_animations: false,
            reply,
        });
    rx.recv_blocking().unwrap().unwrap();
    id
}

fn create_monitor(
    f: &mut Fixture,
    session: RemoteDesktopSessionId,
    name: Option<&str>,
) -> Result<RemoteDesktopMonitorId, String> {
    let (reply, rx) = async_channel::bounded(1);
    f.niri_state()
        .on_remote_desktop_msg(RemoteDesktopToNiri::CreateVirtualMonitor {
            session,
            name: name.map(ToOwned::to_owned),
            persistent: false,
            reply,
        });
    rx.recv_blocking().unwrap().map(|info| info.id)
}

fn has_output(f: &mut Fixture, name: &str) -> bool {
    f.niri().global_space.outputs().any(|o| o.name() == name)
}

#[test]
fn create_and_remove_virtual_monitor() {
    let mut f = Fixture::new();
    let _events = enable(&mut f);
    let session = create_session(&mut f, false);

    let monitor = create_monitor(&mut f, session, Some("Virtual-1")).unwrap();
    assert!(has_output(&mut f, "Virtual-1"));

    // The monitor comes up at the configured default mode.
    let output = f
        .niri()
        .global_space
        .outputs()
        .find(|o| o.name() == "Virtual-1")
        .unwrap()
        .clone();
    let mode = output.current_mode().unwrap();
    assert_eq!(mode.size.w, 1280);
    assert_eq!(mode.size.h, 720);

    let (reply, rx) = async_channel::bounded(1);
    f.niri_state()
        .on_remote_desktop_msg(RemoteDesktopToNiri::RemoveVirtualMonitor {
            session,
            monitor,
            reply,
        });
    rx.recv_blocking().unwrap().unwrap();
    assert!(!has_output(&mut f, "Virtual-1"));
}

#[test]
fn another_session_cannot_remove_a_monitor() {
    let mut f = Fixture::new();
    let _events = enable(&mut f);
    let owner = create_session(&mut f, false);
    let monitor = create_monitor(&mut f, owner, Some("Virtual-1")).unwrap();

    // takes_control terminates the first session, so make the second one non-controlling by
    // creating it directly.
    let other = RemoteDesktopSessionId::next();
    let (reply, rx) = async_channel::bounded(1);
    f.niri_state()
        .on_remote_desktop_msg(RemoteDesktopToNiri::CreateSession {
            id: other,
            persistent: false,
            takes_control: false,
            disable_animations: false,
            reply,
        });
    rx.recv_blocking().unwrap().unwrap();

    let (reply, rx) = async_channel::bounded(1);
    f.niri_state()
        .on_remote_desktop_msg(RemoteDesktopToNiri::RemoveVirtualMonitor {
            session: other,
            monitor,
            reply,
        });
    assert!(rx.recv_blocking().unwrap().is_err());
    assert!(has_output(&mut f, "Virtual-1"));
}

#[test]
fn destroying_a_session_removes_its_monitors() {
    let mut f = Fixture::new();
    let _events = enable(&mut f);
    let session = create_session(&mut f, false);
    create_monitor(&mut f, session, Some("Virtual-1")).unwrap();

    f.niri_state()
        .on_remote_desktop_msg(RemoteDesktopToNiri::DestroySession {
            id: session,
            remove: false,
        });

    assert!(!has_output(&mut f, "Virtual-1"));
}

#[test]
fn a_persistent_session_keeps_its_monitors() {
    let mut f = Fixture::new();
    let _events = enable(&mut f);
    let session = create_session(&mut f, true);

    let (reply, rx) = async_channel::bounded(1);
    f.niri_state()
        .on_remote_desktop_msg(RemoteDesktopToNiri::CreateVirtualMonitor {
            session,
            name: Some("Virtual-1".to_owned()),
            persistent: true,
            reply,
        });
    rx.recv_blocking().unwrap().unwrap();

    f.niri_state()
        .on_remote_desktop_msg(RemoteDesktopToNiri::DestroySession {
            id: session,
            remove: false,
        });
    assert!(has_output(&mut f, "Virtual-1"));

    // ...unless the peer asks for it to go.
    let session = create_session(&mut f, true);
    let (reply, rx) = async_channel::bounded(1);
    f.niri_state()
        .on_remote_desktop_msg(RemoteDesktopToNiri::CreateVirtualMonitor {
            session,
            name: Some("Virtual-2".to_owned()),
            persistent: true,
            reply,
        });
    rx.recv_blocking().unwrap().unwrap();

    f.niri_state()
        .on_remote_desktop_msg(RemoteDesktopToNiri::DestroySession {
            id: session,
            remove: true,
        });
    assert!(!has_output(&mut f, "Virtual-2"));
}

#[test]
fn taking_control_terminates_the_previous_session() {
    let mut f = Fixture::new();
    let events = enable(&mut f);
    let first = create_session(&mut f, false);
    let second = create_session(&mut f, false);

    assert_eq!(f.niri().remote_desktop.controlling, Some(second));
    assert!(!f.niri().remote_desktop.sessions.contains_key(&first));

    let mut destroyed = false;
    while let Ok(msg) = events.try_recv() {
        if matches!(msg, NiriToRemoteDesktop::SessionDestroyed(id) if id == first) {
            destroyed = true;
        }
    }
    assert!(destroyed, "the first session should get a Destroyed signal");
}

#[test]
fn monitors_created_outside_the_api_are_exposed_without_an_owner() {
    let mut f = Fixture::new();
    let _events = enable(&mut f);
    let session = create_session(&mut f, false);

    let name = {
        let state = f.niri_state();
        state
            .backend
            .create_virtual_output(&mut state.niri, 800, 600, 60, Some("sunshine".to_owned()))
            .unwrap()
    };

    f.niri_state().refresh_remote_desktop_monitors();

    let monitor = f
        .niri()
        .remote_desktop
        .monitors
        .iter()
        .find(|m| m.output_name == name)
        .expect("the virtual output should show up as a monitor");
    assert_eq!(monitor.owner, None);
    let monitor_id = monitor.id;

    // Not ours, so we can't remove it through the API.
    let (reply, rx) = async_channel::bounded(1);
    f.niri_state()
        .on_remote_desktop_msg(RemoteDesktopToNiri::RemoveVirtualMonitor {
            session,
            monitor: monitor_id,
            reply,
        });
    assert!(rx.recv_blocking().unwrap().is_err());
}

#[test]
fn disable_animations_option() {
    let mut f = Fixture::new();
    let _events = enable(&mut f);
    let session = create_session(&mut f, false);

    assert!(!f.niri().clock.should_complete_instantly());

    f.niri_state()
        .on_remote_desktop_msg(RemoteDesktopToNiri::SetDisableAnimations {
            id: session,
            value: true,
        });
    assert!(f.niri().clock.should_complete_instantly());

    f.niri_state()
        .on_remote_desktop_msg(RemoteDesktopToNiri::DestroySession {
            id: session,
            remove: false,
        });
    assert!(!f.niri().clock.should_complete_instantly());
}

#[test]
fn absolute_pointer_maps_over_the_whole_layout() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));
    f.add_output(2, (1920, 1080));

    let outputs: Vec<_> = f.niri().global_space.outputs().cloned().collect();
    assert_eq!(outputs.len(), 2);

    let device = EiDevice::new(1, "test".to_owned(), vec![]);
    let state = f.niri_state();

    // A point on the second output.
    let global = Point::from((1400., 300.));
    let pos = global_position(state, global).unwrap();
    let event = EiPointerMotionAbsoluteEvent {
        device: device.clone(),
        time: 0,
        pos,
    };

    // niri maps an absolute event with no device output over the bounding rectangle, so feeding
    // the bounding rectangle's size back must return the original global point.
    let bounds = state.global_bounding_rectangle().unwrap();
    let mapped = event.position_transformed(bounds.size) + bounds.loc.to_f64();
    assert!((mapped.x - global.x).abs() < 0.001, "{mapped:?}");
    assert!((mapped.y - global.y).abs() < 0.001, "{mapped:?}");
}

#[test]
fn touch_maps_onto_the_output_under_the_point() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));
    f.add_output(2, (1920, 1080));

    let second = f.niri_output(2);
    let geo = f.niri().global_space.output_geometry(&second).unwrap();

    let device = EiDevice::new(1, "test".to_owned(), vec![]);
    let state = f.niri_state();

    let global = Point::from((geo.loc.x as f64 + 100., geo.loc.y as f64 + 50.));
    let pos = output_position(state, &device, global).unwrap();

    // The device now points at the output the event landed on...
    use crate::input::backend_ext::NiriInputDevice as _;
    assert_eq!(device.output(state).as_ref(), Some(&second));

    // ...and the position is relative to it.
    assert!((pos.position.x - 100.).abs() < 0.001, "{pos:?}");
    assert!((pos.position.y - 50.).abs() < 0.001, "{pos:?}");
}

#[test]
fn injected_input_goes_through_the_normal_pipeline() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));

    let device = EiDevice::new(1, "test".to_owned(), vec![]);

    // A button press must reach the seat's pointer, which tracks pressed buttons.
    f.niri_state()
        .process_input_event(InputEvent::<EiInputBackend>::PointerButton {
            event: EiButtonEvent {
                device: device.clone(),
                time: 0,
                button: 0x110, // BTN_LEFT
                state: ButtonState::Pressed,
            },
        });
    // A key press must update the keyboard's pressed-keys set.
    let event = EiKeyboardKeyEvent {
        device,
        time: 0,
        key: 30, // KEY_A
        state: KeyState::Pressed,
        count: 1,
    };
    // evdev codes are offset by 8 in xkb space.
    assert_eq!(event.key_code().raw(), 38);

    f.niri_state()
        .process_input_event(InputEvent::<EiInputBackend>::Keyboard { event });

    let pressed = f.niri().seat.get_keyboard().unwrap().pressed_keys();
    assert!(
        pressed.iter().any(|k| k.raw() == 38),
        "the injected key should be pressed on the seat: {pressed:?}"
    );
}

#[test]
fn clipboard_round_trip_types() {
    // The D-Bus clipboard-type values map onto smithay's selection targets.
    use smithay::wayland::selection::SelectionTarget;

    assert_eq!(
        SelectionTarget::from(ClipboardType::Clipboard),
        SelectionTarget::Clipboard
    );
    assert_eq!(
        SelectionTarget::from(ClipboardType::Primary),
        SelectionTarget::Primary
    );
    assert_eq!(ClipboardType::from_dbus(1), Some(ClipboardType::Clipboard));
    assert_eq!(ClipboardType::from_dbus(2), Some(ClipboardType::Primary));
    assert_eq!(ClipboardType::from_dbus(3), None);

    // Cursor modes are shifted by one compared to org.gnome.Mutter.ScreenCast.
    assert_eq!(CursorMode::from_dbus(1), Some(CursorMode::Hidden));
    assert_eq!(CursorMode::from_dbus(3), Some(CursorMode::Metadata));
    assert_eq!(CursorMode::from_dbus(0), None);
    assert_eq!(CursorMode::default(), CursorMode::Metadata);

    let _ = HashMap::<String, String>::new();
}

/// A minimal libei client, enough to bind a keyboard and a pointer and emit a few events.
mod ei_client {
    use std::collections::HashMap;
    use std::os::unix::net::UnixStream;

    use reis::{ei, PendingRequestResult};

    #[derive(Default)]
    struct DeviceData {
        interfaces: HashMap<String, reis::Object>,
    }

    impl DeviceData {
        fn interface<T: reis::Interface>(&self) -> Option<T> {
            self.interfaces.get(T::NAME)?.clone().downcast()
        }
    }

    pub struct Client {
        context: ei::Context,
        seats: HashMap<ei::Seat, HashMap<String, u64>>,
        devices: HashMap<ei::Device, DeviceData>,
        last_serial: u32,
        sequence: u32,
        /// Devices that finished being announced and are emulating.
        pub ready_keyboards: Vec<(ei::Device, ei::Keyboard)>,
        pub ready_pointers: Vec<(ei::Device, ei::Pointer)>,
        pub keymap_size: Option<u32>,
    }

    impl Client {
        pub fn new(socket: UnixStream) -> Self {
            let context = ei::Context::new(socket).unwrap();
            let _handshake = context.handshake();
            context.flush().unwrap();

            Self {
                context,
                seats: HashMap::new(),
                devices: HashMap::new(),
                last_serial: 0,
                sequence: 0,
                ready_keyboards: Vec::new(),
                ready_pointers: Vec::new(),
                keymap_size: None,
            }
        }

        /// Reads and handles everything the server has sent so far.
        pub fn pump(&mut self) {
            if self.context.read().is_err() {
                return;
            }

            while let Some(result) = self.context.pending_event() {
                let PendingRequestResult::Request(event) = result else {
                    continue;
                };
                self.handle(event);
            }

            let _ = self.context.flush();
        }

        fn handle(&mut self, event: ei::Event) {
            match event {
                ei::Event::Handshake(handshake, request) => match request {
                    ei::handshake::Event::HandshakeVersion { .. } => {
                        handshake.handshake_version(1);
                        handshake.name("niri-test");
                        handshake.context_type(ei::handshake::ContextType::Sender);
                        for interface in [
                            "ei_callback",
                            "ei_connection",
                            "ei_seat",
                            "ei_device",
                            "ei_pingpong",
                            "ei_keyboard",
                            "ei_pointer",
                            "ei_pointer_absolute",
                            "ei_button",
                            "ei_scroll",
                        ] {
                            handshake.interface_version(interface, 1);
                        }
                        handshake.finish();
                    }
                    ei::handshake::Event::Connection { serial, .. } => {
                        self.last_serial = serial;
                    }
                    _ => (),
                },
                ei::Event::Connection(_, request) => match request {
                    ei::connection::Event::Seat { seat } => {
                        self.seats.insert(seat, HashMap::new());
                    }
                    ei::connection::Event::Ping { ping } => ping.done(0),
                    _ => (),
                },
                ei::Event::Seat(seat, request) => match request {
                    ei::seat::Event::Capability { mask, interface } => {
                        if let Some(caps) = self.seats.get_mut(&seat) {
                            caps.insert(interface, mask);
                        }
                    }
                    ei::seat::Event::Done => {
                        let Some(caps) = self.seats.get(&seat) else {
                            return;
                        };
                        let mask = caps.values().fold(0, |acc, mask| acc | mask);
                        seat.bind(mask);
                    }
                    ei::seat::Event::Device { device } => {
                        self.devices.insert(device, DeviceData::default());
                    }
                    _ => (),
                },
                ei::Event::Device(device, request) => match request {
                    ei::device::Event::Interface { object } => {
                        if let Some(data) = self.devices.get_mut(&device) {
                            data.interfaces
                                .insert(object.interface().to_owned(), object);
                        }
                    }
                    ei::device::Event::Done => {
                        let Some(data) = self.devices.get(&device) else {
                            return;
                        };
                        let keyboard = data.interface::<ei::Keyboard>();
                        let pointer = data.interface::<ei::Pointer>();

                        self.sequence += 1;
                        device.start_emulating(self.last_serial, self.sequence);

                        if let Some(keyboard) = keyboard {
                            self.ready_keyboards.push((device.clone(), keyboard));
                        }
                        if let Some(pointer) = pointer {
                            self.ready_pointers.push((device.clone(), pointer));
                        }
                    }
                    ei::device::Event::Resumed { serial } => self.last_serial = serial,
                    _ => (),
                },
                ei::Event::Keyboard(_, ei::keyboard::Event::Keymap { size, .. }) => {
                    self.keymap_size = Some(size);
                }
                _ => (),
            }
        }

        /// Presses or releases an evdev key code.
        pub fn key(&mut self, key: u32, pressed: bool, time: u64) {
            let Some((device, keyboard)) = self.ready_keyboards.first().cloned() else {
                panic!("no keyboard device");
            };
            let state = if pressed {
                ei::keyboard::KeyState::Press
            } else {
                ei::keyboard::KeyState::Released
            };
            keyboard.key(key, state);
            device.frame(self.last_serial, time);
            let _ = self.context.flush();
        }

        pub fn move_pointer(&mut self, dx: f32, dy: f32) {
            let Some((device, pointer)) = self.ready_pointers.first().cloned() else {
                panic!("no pointer device");
            };
            pointer.motion_relative(dx, dy);
            device.frame(self.last_serial, 3000);
            let _ = self.context.flush();
        }
    }
}

/// Drives both ends until the closure is happy, or gives up.
fn pump_until(
    f: &mut Fixture,
    client: &mut ei_client::Client,
    mut done: impl FnMut(&mut Fixture, &mut ei_client::Client) -> bool,
) -> bool {
    for _ in 0..200 {
        f.dispatch();
        client.pump();
        f.dispatch();
        if done(f, client) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    false
}

#[test]
fn eis_input_reaches_the_seat() {
    use std::os::unix::net::UnixStream;

    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));
    let _events = enable(&mut f);
    let session = create_session(&mut f, false);

    let (ours, theirs) = UnixStream::pair().unwrap();
    f.niri_state()
        .remote_desktop_connect_eis(session, ours)
        .unwrap();

    let mut client = ei_client::Client::new(theirs);

    // Wait for the seat, the bind round-trip and the devices.
    let bound = pump_until(&mut f, &mut client, |_, client| {
        !client.ready_keyboards.is_empty() && !client.ready_pointers.is_empty()
    });
    assert!(bound, "the client should get a keyboard and a pointer");
    assert!(
        client.keymap_size.is_some_and(|size| size > 0),
        "the keyboard device should come with a keymap"
    );

    // A key press must land on the seat's keyboard, which means it went through
    // State::process_input_event and not straight to a wl_keyboard.
    client.key(30, true, 1000); // KEY_A
    let pressed = pump_until(&mut f, &mut client, |f, _| {
        f.niri()
            .seat
            .get_keyboard()
            .unwrap()
            .pressed_keys()
            .iter()
            .any(|k| k.raw() == 38)
    });
    assert!(pressed, "the injected key should be pressed on the seat");

    client.key(30, false, 2000);
    let released = pump_until(&mut f, &mut client, |f, _| {
        f.niri()
            .seat
            .get_keyboard()
            .unwrap()
            .pressed_keys()
            .is_empty()
    });
    assert!(released, "the injected key should be released again");

    // And pointer motion must move the cursor.
    let before = f.niri().seat.get_pointer().unwrap().current_location();
    client.move_pointer(37., 23.);
    let moved = pump_until(&mut f, &mut client, |f, _| {
        f.niri().seat.get_pointer().unwrap().current_location() != before
    });
    assert!(moved, "the injected motion should move the pointer");

    let after = f.niri().seat.get_pointer().unwrap().current_location();
    assert!(
        (after.x - before.x - 37.).abs() < 1.,
        "{before:?} -> {after:?}"
    );
    assert!(
        (after.y - before.y - 23.).abs() < 1.,
        "{before:?} -> {after:?}"
    );
}

#[test]
fn remote_clipboard_transfer() {
    use std::io::Read as _;
    use std::os::unix::net::UnixStream;

    use smithay::wayland::selection::{SelectionHandler as _, SelectionTarget};

    let mut f = Fixture::new();
    let events = enable(&mut f);
    let session = create_session(&mut f, false);

    let (reply, rx) = async_channel::bounded(1);
    f.niri_state()
        .on_remote_desktop_msg(RemoteDesktopToNiri::EnableClipboard { session, reply });
    rx.recv_blocking().unwrap().unwrap();

    // The remote service offers text.
    let (reply, rx) = async_channel::bounded(1);
    f.niri_state()
        .on_remote_desktop_msg(RemoteDesktopToNiri::SetSelection {
            session,
            ty: ClipboardType::Clipboard,
            mime_types: vec!["text/plain;charset=utf-8".to_owned()],
            reply,
        });
    rx.recv_blocking().unwrap().unwrap();

    // A Wayland client reading the selection reaches send_selection, which parks the fd and asks
    // the service for the data. A socket pair stands in for the client's pipe.
    let (theirs, ours) = UnixStream::pair().unwrap();
    let state = f.niri_state();
    let seat = state.niri.seat.clone();
    state.send_selection(
        SelectionTarget::Clipboard,
        "text/plain;charset=utf-8".to_owned(),
        ours.into(),
        seat,
        &crate::niri::NiriSelection::Remote {
            session,
            target: SelectionTarget::Clipboard,
        },
    );

    let mut serial = None;
    while let Ok(msg) = events.try_recv() {
        if let NiriToRemoteDesktop::SelectionTransfer {
            session: s,
            ty,
            mime_type,
            serial: n,
        } = msg
        {
            assert_eq!(s, session);
            assert_eq!(ty, ClipboardType::Clipboard);
            assert_eq!(mime_type, "text/plain;charset=utf-8");
            serial = Some(n);
        }
    }
    let serial = serial.expect("a SelectionTransfer should have been emitted");

    // The service answers with SelectionWrite and gets the client's fd back.
    let (reply, rx) = async_channel::bounded(1);
    f.niri_state()
        .on_remote_desktop_msg(RemoteDesktopToNiri::SelectionWrite {
            session,
            serial,
            reply,
        });
    let fd = rx.recv_blocking().unwrap().unwrap();

    // Asking twice must not hand out the same fd again.
    let (reply, rx) = async_channel::bounded(1);
    f.niri_state()
        .on_remote_desktop_msg(RemoteDesktopToNiri::SelectionWrite {
            session,
            serial,
            reply,
        });
    assert!(rx.recv_blocking().unwrap().is_err());

    // Writing to it must reach the "client".
    let mut writer = std::fs::File::from(fd);
    use std::io::Write as _;
    writer.write_all(b"hello from the remote").unwrap();
    drop(writer);

    let mut got = String::new();
    let mut reader = theirs;
    reader.read_to_string(&mut got).unwrap();
    assert_eq!(got, "hello from the remote");

    f.niri_state()
        .on_remote_desktop_msg(RemoteDesktopToNiri::SelectionWriteDone {
            session,
            serial,
            success: true,
        });

    // The transfer is finished, so the serial is no longer known.
    let (reply, rx) = async_channel::bounded(1);
    f.niri_state()
        .on_remote_desktop_msg(RemoteDesktopToNiri::SelectionWrite {
            session,
            serial,
            reply,
        });
    assert!(rx.recv_blocking().unwrap().is_err());
}

#[test]
fn a_wayland_selection_notifies_the_service() {
    use smithay::wayland::selection::{SelectionHandler as _, SelectionTarget};

    let mut f = Fixture::new();
    let events = enable(&mut f);
    let session = create_session(&mut f, false);

    let (reply, rx) = async_channel::bounded(1);
    f.niri_state()
        .on_remote_desktop_msg(RemoteDesktopToNiri::EnableClipboard { session, reply });
    rx.recv_blocking().unwrap().unwrap();
    while events.try_recv().is_ok() {}

    let state = f.niri_state();
    let seat = state.niri.seat.clone();
    state.new_selection(SelectionTarget::Primary, None, seat);

    let mut saw = false;
    while let Ok(msg) = events.try_recv() {
        if let NiriToRemoteDesktop::SelectionOwnerChanged {
            session: s,
            ty,
            session_is_owner,
            ..
        } = msg
        {
            assert_eq!(s, session);
            assert_eq!(ty, ClipboardType::Primary);
            assert!(!session_is_owner);
            saw = true;
        }
    }
    assert!(saw, "a SelectionOwnerChanged should have been emitted");
}
