//! Compositor side of the `org.freedesktop.RemoteDesktop1` API.
//!
//! The D-Bus interfaces live in [`crate::dbus::remote_desktop`] and run on zbus's own threads; this
//! module owns everything that needs `State` and runs on the compositor thread.

pub mod clipboard;
pub mod eis;
pub mod input;

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;

use reis::enumflags2::BitFlags;
use reis::request::DeviceCapability;
use smithay::output::Output;
use smithay::reexports::rustix::fs::{fcntl_setfl, OFlags};
use smithay::reexports::rustix::pipe::{pipe_with, PipeFlags};
use smithay::utils::{Physical, Size};
use smithay::wayland::selection::data_device::{
    clear_data_device_selection, request_data_device_client_selection, set_data_device_selection,
};
use smithay::wayland::selection::primary_selection::{
    clear_primary_selection, request_primary_client_selection, set_primary_selection,
};
use smithay::wayland::selection::{SelectionSource, SelectionTarget};

use self::clipboard::{clipboard_type_of, PendingTransfer, SessionClipboard, TRANSFER_TIMEOUT};
use self::eis::EisConnection;
use crate::backend::VirtualOutputMarker;
use crate::dbus::remote_desktop::{
    ClipboardType, CursorMode, MonitorInfo, NiriToRemoteDesktop, RemoteDesktopToNiri, StreamInfo,
};
use crate::niri::{Niri, NiriSelection, State};
use crate::utils::{CastSessionId, CastStreamId, RemoteDesktopMonitorId, RemoteDesktopSessionId};

/// One remote desktop session.
pub struct Session {
    pub persistent: bool,
    pub has_control: bool,
    pub disable_animations: bool,
    /// Live EIS connection, if the service called `ConnectToEIS` and hasn't disconnected.
    pub eis: Option<EisConnection>,
    /// Capabilities the service bound last time, kept across reconnects for persistent sessions.
    pub eis_bound: BitFlags<DeviceCapability>,
    pub clipboard: Option<SessionClipboard>,
    pub streams: Vec<Stream>,
}

/// A virtual monitor, which is always backed by a niri virtual output.
pub struct Monitor {
    pub id: RemoteDesktopMonitorId,
    pub output_name: String,
    /// Session that created it. Virtual outputs made through `niri msg` have no owner.
    pub owner: Option<RemoteDesktopSessionId>,
    pub persistent: bool,
}

impl Monitor {
    fn info(&self) -> MonitorInfo {
        MonitorInfo {
            id: self.id,
            name: self.output_name.clone(),
            owner: self.owner,
        }
    }
}

/// A PipeWire stream of a virtual monitor.
pub struct Stream {
    /// The id handed out over D-Bus.
    pub id: String,
    pub monitor: RemoteDesktopMonitorId,
    pub cast_session_id: CastSessionId,
    pub cast_stream_id: CastStreamId,
    /// The `OpenPipeWireStream` call waiting for the node id.
    pub pending_reply: Option<async_channel::Sender<Result<StreamInfo, String>>>,
}

#[derive(Default)]
pub struct RemoteDesktopState {
    /// Set once the D-Bus service starts.
    pub to_dbus: Option<async_channel::Sender<NiriToRemoteDesktop>>,
    pub sessions: HashMap<RemoteDesktopSessionId, Session>,
    pub monitors: Vec<Monitor>,
    pub controlling: Option<RemoteDesktopSessionId>,
    /// Mode new virtual monitors start with, from the config.
    pub default_mode: Option<niri_ipc::ConfiguredMode>,
}

impl RemoteDesktopState {
    pub fn is_enabled(&self) -> bool {
        self.to_dbus.is_some()
    }

    fn notify(&self, msg: NiriToRemoteDesktop) {
        let Some(to_dbus) = &self.to_dbus else { return };
        if let Err(err) = to_dbus.send_blocking(msg) {
            warn!("error sending message to the remote desktop service: {err:?}");
        }
    }

    /// True if any session asked for animations to be disabled.
    pub fn animations_disabled(&self) -> bool {
        self.sessions.values().any(|s| s.disable_animations)
    }

    pub fn set_eis(&mut self, session: RemoteDesktopSessionId, eis: EisConnection) {
        if let Some(session) = self.sessions.get_mut(&session) {
            session.eis = Some(eis);
        }
    }

    pub fn set_eis_bound(
        &mut self,
        session: RemoteDesktopSessionId,
        bound: BitFlags<DeviceCapability>,
    ) {
        if let Some(session) = self.sessions.get_mut(&session) {
            session.eis_bound = bound;
            if let Some(eis) = &mut session.eis {
                eis.bound = bound;
            }
        }
    }

    /// Forgets the EIS connection after the client went away.
    ///
    /// A persistent session keeps the bound capabilities so the same devices come back on
    /// reconnect, as the spec recommends.
    pub fn take_eis(
        &mut self,
        session: RemoteDesktopSessionId,
        bound: BitFlags<DeviceCapability>,
    ) -> Option<EisConnection> {
        let session = self.sessions.get_mut(&session)?;
        if session.persistent {
            session.eis_bound = bound;
        } else {
            session.eis_bound = BitFlags::empty();
        }
        session.eis.take()
    }

    fn monitor(&self, id: RemoteDesktopMonitorId) -> Option<&Monitor> {
        self.monitors.iter().find(|m| m.id == id)
    }
}

impl State {
    pub fn on_remote_desktop_msg(&mut self, msg: RemoteDesktopToNiri) {
        match msg {
            RemoteDesktopToNiri::CreateSession {
                id,
                persistent,
                takes_control,
                disable_animations,
                reply,
            } => {
                debug!(%id, persistent, takes_control, "creating remote desktop session");

                if takes_control {
                    if let Some(previous) = self.niri.remote_desktop.controlling.take() {
                        self.destroy_remote_desktop_session(previous, false, true);
                    }
                    self.niri.remote_desktop.controlling = Some(id);
                }

                self.niri.remote_desktop.sessions.insert(
                    id,
                    Session {
                        persistent,
                        has_control: takes_control,
                        disable_animations,
                        eis: None,
                        eis_bound: BitFlags::empty(),
                        clipboard: None,
                        streams: Vec::new(),
                    },
                );

                self.refresh_remote_desktop_monitors();
                self.refresh_remote_desktop_animations();

                let monitors = self
                    .niri
                    .remote_desktop
                    .monitors
                    .iter()
                    .map(Monitor::info)
                    .collect();
                let _ = reply.send_blocking(Ok(monitors));
            }
            RemoteDesktopToNiri::DestroySession { id, remove } => {
                self.destroy_remote_desktop_session(id, remove, false);
            }
            RemoteDesktopToNiri::SetDisableAnimations { id, value } => {
                if let Some(session) = self.niri.remote_desktop.sessions.get_mut(&id) {
                    session.disable_animations = value;
                }
                self.refresh_remote_desktop_animations();
            }
            RemoteDesktopToNiri::CreateVirtualMonitor {
                session,
                name,
                persistent,
                reply,
            } => {
                let result = self.create_remote_desktop_monitor(session, name, persistent);
                let _ = reply.send_blocking(result);
            }
            RemoteDesktopToNiri::RemoveVirtualMonitor {
                session,
                monitor,
                reply,
            } => {
                let result = self.remove_remote_desktop_monitor(session, monitor);
                let _ = reply.send_blocking(result);
            }
            RemoteDesktopToNiri::OpenPipeWireStream {
                session,
                monitor,
                cursor_mode,
                reply,
            } => {
                if let Err(err) =
                    self.open_remote_desktop_stream(session, monitor, cursor_mode, &reply)
                {
                    let _ = reply.send_blocking(Err(err));
                }
            }
            RemoteDesktopToNiri::ClosePipeWireStream {
                session,
                monitor,
                stream_id,
                reply,
            } => {
                let result = self.close_remote_desktop_stream(session, monitor, &stream_id);
                let _ = reply.send_blocking(result);
            }
            RemoteDesktopToNiri::ConnectToEis {
                session,
                socket,
                reply,
            } => {
                let result = self.connect_remote_desktop_eis(session, socket);
                let _ = reply.send_blocking(result);
            }
            RemoteDesktopToNiri::EnableClipboard { session, reply } => {
                let result = match self.niri.remote_desktop.sessions.get_mut(&session) {
                    Some(session) => {
                        session.clipboard = Some(SessionClipboard::default());
                        Ok(())
                    }
                    None => Err("no such session".to_owned()),
                };
                let _ = reply.send_blocking(result);
            }
            RemoteDesktopToNiri::DisableClipboard { session } => {
                self.disable_remote_desktop_clipboard(session);
            }
            RemoteDesktopToNiri::SetSelection {
                session,
                ty,
                mime_types,
                reply,
            } => {
                let result = self.set_remote_desktop_selection(session, ty, mime_types);
                let _ = reply.send_blocking(result);
            }
            RemoteDesktopToNiri::SelectionRead {
                session,
                ty,
                mime_type,
                reply,
            } => {
                let result = self.read_remote_desktop_selection(session, ty, mime_type);
                let _ = reply.send_blocking(result);
            }
            RemoteDesktopToNiri::SelectionWrite {
                session,
                serial,
                reply,
            } => {
                let result = self.take_remote_desktop_transfer_fd(session, serial);
                let _ = reply.send_blocking(result);
            }
            RemoteDesktopToNiri::SelectionWriteDone {
                session,
                serial,
                success,
            } => {
                if !success {
                    debug!(%session, serial, "remote desktop clipboard transfer failed");
                }
                self.finish_remote_desktop_transfer(session, serial);
            }
        }
    }

    fn destroy_remote_desktop_session(
        &mut self,
        id: RemoteDesktopSessionId,
        remove: bool,
        notify: bool,
    ) {
        let Some(mut session) = self.niri.remote_desktop.sessions.remove(&id) else {
            return;
        };
        debug!(%id, remove, "destroying remote desktop session");

        if self.niri.remote_desktop.controlling == Some(id) {
            self.niri.remote_desktop.controlling = None;
        }

        for stream in std::mem::take(&mut session.streams) {
            self.niri.stop_remote_desktop_cast(stream.cast_session_id);
        }

        if let Some(eis) = session.eis.take() {
            self.niri.event_loop.remove(eis.token);
        }

        if session.clipboard.is_some() {
            self.clear_remote_desktop_selections(id, &mut session);
        }

        // A persistent session's monitors outlive it unless the peer asked to remove them.
        let drop_monitors = remove || !session.persistent;
        let to_remove: Vec<_> = self
            .niri
            .remote_desktop
            .monitors
            .iter()
            .filter(|m| m.owner == Some(id) && (drop_monitors || !m.persistent))
            .map(|m| (m.id, m.output_name.clone()))
            .collect();

        for (monitor, name) in to_remove {
            if let Err(err) = self.backend.remove_virtual_output(&mut self.niri, &name) {
                warn!("error removing virtual monitor {name}: {err}");
            }
            self.niri
                .remote_desktop
                .monitors
                .retain(|m| m.id != monitor);
            self.niri
                .remote_desktop
                .notify(NiriToRemoteDesktop::MonitorRemoved(monitor));
        }

        // Monitors that stay lose their owner, so nobody can remove them through this API anymore.
        for monitor in &mut self.niri.remote_desktop.monitors {
            if monitor.owner == Some(id) {
                monitor.owner = None;
            }
        }

        if notify {
            self.niri
                .remote_desktop
                .notify(NiriToRemoteDesktop::SessionDestroyed(id));
        }

        self.refresh_remote_desktop_animations();
    }

    fn create_remote_desktop_monitor(
        &mut self,
        session: RemoteDesktopSessionId,
        name: Option<String>,
        persistent: bool,
    ) -> Result<MonitorInfo, String> {
        if !self.niri.remote_desktop.sessions.contains_key(&session) {
            return Err("no such session".to_owned());
        }

        let mode = self
            .niri
            .remote_desktop
            .default_mode
            .unwrap_or(niri_ipc::ConfiguredMode {
                width: 1920,
                height: 1080,
                refresh: Some(60.),
            });
        let refresh = mode.refresh.unwrap_or(60.).round().clamp(1., 1000.) as u32;

        let output_name = self.backend.create_virtual_output(
            &mut self.niri,
            mode.width,
            mode.height,
            refresh,
            name,
        )?;

        let monitor = Monitor {
            id: RemoteDesktopMonitorId::next(),
            output_name,
            owner: Some(session),
            persistent,
        };
        let info = monitor.info();
        self.niri.remote_desktop.monitors.push(monitor);

        // The creating session gets its object from the D-Bus method itself; this tells the others.
        self.niri
            .remote_desktop
            .notify(NiriToRemoteDesktop::MonitorAdded(info.clone()));

        Ok(info)
    }

    fn remove_remote_desktop_monitor(
        &mut self,
        session: RemoteDesktopSessionId,
        monitor: RemoteDesktopMonitorId,
    ) -> Result<(), String> {
        let Some(record) = self.niri.remote_desktop.monitor(monitor) else {
            return Err("no such monitor".to_owned());
        };
        if record.owner != Some(session) {
            return Err("this monitor belongs to another session".to_owned());
        }
        let name = record.output_name.clone();

        self.backend.remove_virtual_output(&mut self.niri, &name)?;
        self.niri
            .remote_desktop
            .monitors
            .retain(|m| m.id != monitor);
        self.niri
            .remote_desktop
            .notify(NiriToRemoteDesktop::MonitorRemoved(monitor));

        Ok(())
    }

    fn open_remote_desktop_stream(
        &mut self,
        session: RemoteDesktopSessionId,
        monitor: RemoteDesktopMonitorId,
        cursor_mode: CursorMode,
        reply: &async_channel::Sender<Result<StreamInfo, String>>,
    ) -> Result<(), String> {
        let Some(record) = self.niri.remote_desktop.monitor(monitor) else {
            return Err("no such monitor".to_owned());
        };
        let output_name = record.output_name.clone();

        let Some(output) = self.niri.output_by_name_match(&output_name).cloned() else {
            return Err("the monitor's output is not connected".to_owned());
        };

        if !self.niri.remote_desktop.sessions.contains_key(&session) {
            return Err("no such session".to_owned());
        }

        let cast_session_id = CastSessionId::next();
        let cast_stream_id = CastStreamId::next();

        self.start_remote_desktop_cast(
            cast_session_id,
            cast_stream_id,
            &output,
            cursor_mode.into(),
        )
        .map_err(|err| format!("error starting the stream: {err:?}"))?;

        let session = self
            .niri
            .remote_desktop
            .sessions
            .get_mut(&session)
            .expect("checked above");
        session.streams.push(Stream {
            id: format!("u{}", cast_stream_id.get()),
            monitor,
            cast_session_id,
            cast_stream_id,
            pending_reply: Some(reply.clone()),
        });

        Ok(())
    }

    fn close_remote_desktop_stream(
        &mut self,
        session: RemoteDesktopSessionId,
        monitor: RemoteDesktopMonitorId,
        stream_id: &str,
    ) -> Result<(), String> {
        let Some(record) = self.niri.remote_desktop.sessions.get_mut(&session) else {
            return Err("no such session".to_owned());
        };

        let Some(index) = record
            .streams
            .iter()
            .position(|s| s.id == stream_id && s.monitor == monitor)
        else {
            return Err("no such stream".to_owned());
        };

        let stream = record.streams.remove(index);
        if let Some(reply) = stream.pending_reply {
            let _ = reply.send_blocking(Err("the stream was closed".to_owned()));
        }
        self.niri.stop_remote_desktop_cast(stream.cast_session_id);

        Ok(())
    }

    fn connect_remote_desktop_eis(
        &mut self,
        session: RemoteDesktopSessionId,
        socket: OwnedFd,
    ) -> Result<(), String> {
        let Some(record) = self.niri.remote_desktop.sessions.get_mut(&session) else {
            return Err("no such session".to_owned());
        };

        // One EIS connection per session; a new one replaces the old.
        if let Some(eis) = record.eis.take() {
            self.niri.event_loop.remove(eis.token);
        }

        self.remote_desktop_connect_eis(session, UnixStream::from(socket))
            .map_err(|err| format!("error setting up the EIS connection: {err:?}"))
    }

    /// Called when the PipeWire node id of a remote desktop stream becomes known.
    pub fn on_remote_desktop_node_id(
        &mut self,
        stream_id: CastStreamId,
        node_id: u32,
        serial: Option<u64>,
    ) {
        if serial.is_none() {
            warn!(%stream_id, node_id, "no object.serial for the stream's node, reporting 0");
        }

        for session in self.niri.remote_desktop.sessions.values_mut() {
            let Some(stream) = session
                .streams
                .iter_mut()
                .find(|s| s.cast_stream_id == stream_id)
            else {
                continue;
            };

            let Some(reply) = stream.pending_reply.take() else {
                return;
            };

            let _ = reply.send_blocking(Ok(StreamInfo {
                stream_id: stream.id.clone(),
                node_id,
                serial: serial.unwrap_or(0),
            }));
            return;
        }
    }

    /// Called when a consumer negotiates a size different from the monitor's current one.
    pub fn on_remote_desktop_resize_target(
        &mut self,
        stream_id: CastStreamId,
        size: Size<u32, Physical>,
    ) {
        let output_name = self
            .niri
            .remote_desktop
            .sessions
            .values()
            .find_map(|session| {
                let stream = session
                    .streams
                    .iter()
                    .find(|s| s.cast_stream_id == stream_id)?;
                let monitor = self.niri.remote_desktop.monitor(stream.monitor)?;
                Some(monitor.output_name.clone())
            });
        let Some(output_name) = output_name else {
            return;
        };

        let Ok(width) = u16::try_from(size.w) else {
            warn!("consumer asked for a {}px wide monitor, ignoring", size.w);
            return;
        };
        let Ok(height) = u16::try_from(size.h) else {
            warn!("consumer asked for a {}px tall monitor, ignoring", size.h);
            return;
        };

        let refresh = self
            .niri
            .output_by_name_match(&output_name)
            .and_then(|output| output.current_mode())
            .map_or(60., |mode| f64::from(mode.refresh) / 1000.);

        debug!(
            output_name,
            width, height, "resizing a virtual monitor to match its stream"
        );

        self.apply_transient_output_config(
            &output_name,
            niri_ipc::OutputAction::CustomMode {
                mode: niri_ipc::ConfiguredMode {
                    width,
                    height,
                    refresh: Some(refresh),
                },
            },
        );
    }

    /// Keeps the monitor list in sync with the virtual outputs that actually exist.
    ///
    /// Virtual outputs created outside this API (`niri msg create-virtual-output`, or the default
    /// headless output) are exposed too, without an owner, so a session can capture them but not
    /// remove or resize them.
    pub fn refresh_remote_desktop_monitors(&mut self) {
        if !self.niri.remote_desktop.is_enabled() {
            return;
        }

        let virtual_outputs: Vec<String> = self
            .niri
            .global_space
            .outputs()
            .filter(|output| VirtualOutputMarker::is_virtual(output))
            .map(Output::name)
            .collect();

        // Gone.
        let mut removed = Vec::new();
        self.niri.remote_desktop.monitors.retain(|monitor| {
            let keep = virtual_outputs.contains(&monitor.output_name);
            if !keep {
                removed.push(monitor.id);
            }
            keep
        });
        for monitor in removed {
            self.niri
                .remote_desktop
                .notify(NiriToRemoteDesktop::MonitorRemoved(monitor));
        }

        // New.
        let mut added = Vec::new();
        for name in virtual_outputs {
            if self
                .niri
                .remote_desktop
                .monitors
                .iter()
                .any(|m| m.output_name == name)
            {
                continue;
            }

            let monitor = Monitor {
                id: RemoteDesktopMonitorId::next(),
                output_name: name,
                owner: None,
                persistent: false,
            };
            added.push(monitor.info());
            self.niri.remote_desktop.monitors.push(monitor);
        }
        for info in added {
            self.niri
                .remote_desktop
                .notify(NiriToRemoteDesktop::MonitorAdded(info));
        }
    }

    /// Applies the `disable-animations` session option on top of the config.
    pub fn refresh_remote_desktop_animations(&mut self) {
        let off = self.niri.config.borrow().animations.off;
        let disabled = off || self.niri.remote_desktop.animations_disabled();
        self.niri.clock.set_complete_instantly(disabled);
    }
}

// Clipboard.
impl State {
    fn set_remote_desktop_selection(
        &mut self,
        session: RemoteDesktopSessionId,
        ty: ClipboardType,
        mime_types: Vec<String>,
    ) -> Result<(), String> {
        let Some(record) = self.niri.remote_desktop.sessions.get_mut(&session) else {
            return Err("no such session".to_owned());
        };
        let Some(clipboard) = &mut record.clipboard else {
            return Err("clipboard integration is not enabled for this session".to_owned());
        };

        if mime_types.is_empty() {
            clipboard.offered.remove(&ty);
            match SelectionTarget::from(ty) {
                SelectionTarget::Clipboard => {
                    clear_data_device_selection(&self.niri.display_handle, &self.niri.seat);
                }
                SelectionTarget::Primary => {
                    clear_primary_selection(&self.niri.display_handle, &self.niri.seat);
                }
            }
            return Ok(());
        }

        clipboard.offered.insert(ty, mime_types.clone());

        let user_data = NiriSelection::Remote {
            session,
            target: SelectionTarget::from(ty),
        };
        match SelectionTarget::from(ty) {
            SelectionTarget::Clipboard => set_data_device_selection(
                &self.niri.display_handle,
                &self.niri.seat,
                mime_types,
                user_data,
            ),
            SelectionTarget::Primary => set_primary_selection(
                &self.niri.display_handle,
                &self.niri.seat,
                mime_types,
                user_data,
            ),
        }

        Ok(())
    }

    fn read_remote_desktop_selection(
        &mut self,
        session: RemoteDesktopSessionId,
        ty: ClipboardType,
        mime_type: String,
    ) -> Result<OwnedFd, String> {
        if !self.niri.remote_desktop.sessions.contains_key(&session) {
            return Err("no such session".to_owned());
        }

        let (read, write) =
            pipe_with(PipeFlags::CLOEXEC).map_err(|err| format!("error creating a pipe: {err}"))?;

        match SelectionTarget::from(ty) {
            SelectionTarget::Clipboard => {
                request_data_device_client_selection(&self.niri.seat, mime_type, write)
                    .map_err(|err| format!("error reading the selection: {err}"))?;
            }
            SelectionTarget::Primary => {
                request_primary_client_selection(&self.niri.seat, mime_type, write)
                    .map_err(|err| format!("error reading the selection: {err}"))?;
            }
        }

        Ok(read)
    }

    fn take_remote_desktop_transfer_fd(
        &mut self,
        session: RemoteDesktopSessionId,
        serial: u32,
    ) -> Result<OwnedFd, String> {
        let Some(record) = self.niri.remote_desktop.sessions.get_mut(&session) else {
            return Err("no such session".to_owned());
        };
        let Some(clipboard) = &mut record.clipboard else {
            return Err("clipboard integration is not enabled for this session".to_owned());
        };
        let Some(transfer) = clipboard.pending.get_mut(&serial) else {
            return Err("no transfer with this serial".to_owned());
        };
        transfer
            .fd
            .take()
            .ok_or_else(|| "the transfer fd was already taken".to_owned())
    }

    fn finish_remote_desktop_transfer(&mut self, session: RemoteDesktopSessionId, serial: u32) {
        let Some(record) = self.niri.remote_desktop.sessions.get_mut(&session) else {
            return;
        };
        let Some(clipboard) = &mut record.clipboard else {
            return;
        };
        if let Some(transfer) = clipboard.pending.remove(&serial) {
            if let Some(token) = transfer.timeout {
                self.niri.event_loop.remove(token);
            }
        }
    }

    fn disable_remote_desktop_clipboard(&mut self, session: RemoteDesktopSessionId) {
        let Some(mut record) = self.niri.remote_desktop.sessions.remove(&session) else {
            return;
        };
        self.clear_remote_desktop_selections(session, &mut record);
        record.clipboard = None;
        self.niri.remote_desktop.sessions.insert(session, record);
    }

    fn clear_remote_desktop_selections(
        &mut self,
        _session: RemoteDesktopSessionId,
        record: &mut Session,
    ) {
        let Some(clipboard) = &mut record.clipboard else {
            return;
        };

        for (ty, _) in std::mem::take(&mut clipboard.offered) {
            match SelectionTarget::from(ty) {
                SelectionTarget::Clipboard => {
                    clear_data_device_selection(&self.niri.display_handle, &self.niri.seat);
                }
                SelectionTarget::Primary => {
                    clear_primary_selection(&self.niri.display_handle, &self.niri.seat);
                }
            }
        }

        for (_, transfer) in std::mem::take(&mut clipboard.pending) {
            if let Some(token) = transfer.timeout {
                self.niri.event_loop.remove(token);
            }
        }
    }

    /// A Wayland client wants to read a selection owned by a remote desktop session.
    ///
    /// Parks the client's fd and asks the service for the data; it answers with `SelectionWrite`.
    pub fn remote_desktop_send_selection(
        &mut self,
        session: RemoteDesktopSessionId,
        ty: ClipboardType,
        mime_type: String,
        fd: OwnedFd,
    ) {
        // The Wayland side sets O_NONBLOCK; the remote service writes to this fd with a plain
        // write(), so clear it like the byte-buffer path does.
        if let Err(err) = fcntl_setfl(&fd, OFlags::empty()) {
            warn!("error clearing flags on selection target fd: {err:?}");
        }

        let Some(record) = self.niri.remote_desktop.sessions.get_mut(&session) else {
            return;
        };
        let Some(clipboard) = &mut record.clipboard else {
            return;
        };

        let serial = clipboard.next_serial();

        // Don't leak the fd if the service never answers.
        let timer = calloop::timer::Timer::from_duration(TRANSFER_TIMEOUT);
        let timeout = self
            .niri
            .event_loop
            .insert_source(timer, move |_, _, state| {
                if let Some(record) = state.niri.remote_desktop.sessions.get_mut(&session) {
                    if let Some(clipboard) = &mut record.clipboard {
                        if clipboard.pending.remove(&serial).is_some() {
                            warn!(serial, "remote desktop clipboard transfer timed out");
                        }
                    }
                }
                calloop::timer::TimeoutAction::Drop
            })
            .ok();

        clipboard.pending.insert(
            serial,
            PendingTransfer {
                fd: Some(fd),
                timeout,
            },
        );

        self.niri
            .remote_desktop
            .notify(NiriToRemoteDesktop::SelectionTransfer {
                session,
                ty,
                mime_type,
                serial,
            });
    }

    /// A Wayland client took over a selection; tell every session with clipboard integration.
    pub fn remote_desktop_new_selection(
        &mut self,
        target: SelectionTarget,
        source: Option<&SelectionSource>,
    ) {
        if !self.niri.remote_desktop.is_enabled() {
            return;
        }

        let ty = clipboard_type_of(target);
        let mime_types = source.map(SelectionSource::mime_types).unwrap_or_default();

        let sessions: Vec<_> = self
            .niri
            .remote_desktop
            .sessions
            .iter()
            .filter(|(_, s)| s.clipboard.is_some())
            .map(|(id, s)| (*id, s.clipboard.as_ref().is_some_and(|c| c.owns(ty))))
            .collect();

        for (session, session_is_owner) in sessions {
            self.niri
                .remote_desktop
                .notify(NiriToRemoteDesktop::SelectionOwnerChanged {
                    session,
                    ty,
                    mime_types: mime_types.clone(),
                    session_is_owner,
                });
        }
    }
}

impl Niri {
    /// Stops a remote desktop cast without going through the Mutter screen cast objects.
    pub fn stop_remote_desktop_cast(&mut self, session_id: CastSessionId) {
        for i in (0..self.casting.casts.len()).rev() {
            if self.casting.casts[i].session_id != session_id {
                continue;
            }

            let cast = self.casting.casts.swap_remove(i);
            if let Err(err) = cast.stream.disconnect() {
                warn!("error disconnecting stream: {err:?}");
            }
        }
    }
}
