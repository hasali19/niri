//! Server side of the `org.freedesktop.RemoteDesktop1` D-Bus API.
//!
//! This is the API described by
//! <https://gitlab.freedesktop.org/xdg/xdg-specs/-/merge_requests/115>. Unlike the screen cast and
//! remote desktop portals, it is meant for remote desktop *services* driving a session where the
//! primary mode of access is remote, so there is no user present to approve anything. Because of
//! that it is off by default and only started when the config has a `remote-desktop` section.
//!
//! The object tree looks like this:
//!
//! ```text
//! /org/freedesktop/RemoteDesktop1                            RemoteDesktop1
//! /org/freedesktop/RemoteDesktop1/Session/u{n}               Session + ObjectManager
//! /org/freedesktop/RemoteDesktop1/Session/u{n}/Monitor/u{m}  Monitor
//! /org/freedesktop/RemoteDesktop1/Session/u{n}/Clipboard     Clipboard
//! ```
//!
//! Monitors are exposed under every live session, since the spec wants a session to also see
//! virtual monitors created by other sessions. `Attributes.owned` tells them apart.

pub mod clipboard;
pub mod monitor;
pub mod session;

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::sync::{Arc, Mutex};

use zbus::fdo::{self, RequestNameFlags};
use zbus::names::{BusName, OwnedUniqueName, UniqueName};
use zbus::object_server::SignalEmitter;
use zbus::zvariant::NoneValue;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};
use zbus::{interface, Connection, ObjectServer};

use self::clipboard::Clipboard;
use self::monitor::Monitor;
use self::session::Session;
use super::Start;
use crate::utils::{RemoteDesktopMonitorId, RemoteDesktopSessionId};

pub const BUS_NAME: &str = "org.freedesktop.RemoteDesktop";
pub const PATH: &str = "/org/freedesktop/RemoteDesktop1";

/// Version of every interface in this module that we implement.
pub const VERSION: u32 = 1;

pub fn session_path(session: RemoteDesktopSessionId) -> OwnedObjectPath {
    OwnedObjectPath::try_from(format!("{PATH}/Session/u{session}")).unwrap()
}

pub fn monitor_path(
    session: RemoteDesktopSessionId,
    monitor: RemoteDesktopMonitorId,
) -> OwnedObjectPath {
    OwnedObjectPath::try_from(format!("{PATH}/Session/u{session}/Monitor/u{monitor}")).unwrap()
}

pub fn clipboard_path(session: RemoteDesktopSessionId) -> OwnedObjectPath {
    OwnedObjectPath::try_from(format!("{PATH}/Session/u{session}/Clipboard")).unwrap()
}

/// Clipboard selection that a `RemoteDesktop1.Clipboard` method or signal refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ClipboardType {
    #[default]
    Clipboard = 1,
    Primary = 2,
}

impl ClipboardType {
    pub fn from_dbus(value: u32) -> Option<Self> {
        match value {
            1 => Some(Self::Clipboard),
            2 => Some(Self::Primary),
            _ => None,
        }
    }
}

/// How the cursor is drawn into a monitor's PipeWire stream.
///
/// Note that these values are shifted by one compared to the ones in
/// `org.gnome.Mutter.ScreenCast`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CursorMode {
    Hidden = 1,
    Embedded = 2,
    #[default]
    Metadata = 3,
}

impl CursorMode {
    pub fn from_dbus(value: u32) -> Option<Self> {
        match value {
            1 => Some(Self::Hidden),
            2 => Some(Self::Embedded),
            3 => Some(Self::Metadata),
            _ => None,
        }
    }
}

#[cfg(feature = "xdp-gnome-screencast")]
impl From<CursorMode> for super::mutter_screen_cast::CursorMode {
    fn from(value: CursorMode) -> Self {
        match value {
            CursorMode::Hidden => Self::Hidden,
            CursorMode::Embedded => Self::Embedded,
            CursorMode::Metadata => Self::Metadata,
        }
    }
}

/// Everything the D-Bus side needs to know about one virtual monitor.
#[derive(Debug, Clone)]
pub struct MonitorInfo {
    pub id: RemoteDesktopMonitorId,
    /// niri output connector name, e.g. `HEADLESS-1`.
    pub name: String,
    /// Session that created it, if any. Monitors made through `niri msg` have no owner.
    pub owner: Option<RemoteDesktopSessionId>,
}

/// Result of `Monitor.OpenPipeWireStream`.
#[derive(Debug, Clone)]
pub struct StreamInfo {
    pub stream_id: String,
    pub node_id: u32,
    pub serial: u64,
}

type Reply<T> = async_channel::Sender<Result<T, String>>;

/// Messages from the D-Bus threads into the compositor.
pub enum RemoteDesktopToNiri {
    CreateSession {
        id: RemoteDesktopSessionId,
        persistent: bool,
        takes_control: bool,
        disable_animations: bool,
        /// Monitors that already exist, so the new session can expose them right away.
        reply: Reply<Vec<MonitorInfo>>,
    },
    DestroySession {
        id: RemoteDesktopSessionId,
        /// Also drop persistent state (monitors) belonging to the session.
        remove: bool,
    },
    SetDisableAnimations {
        id: RemoteDesktopSessionId,
        value: bool,
    },
    CreateVirtualMonitor {
        session: RemoteDesktopSessionId,
        name: Option<String>,
        persistent: bool,
        reply: Reply<MonitorInfo>,
    },
    RemoveVirtualMonitor {
        session: RemoteDesktopSessionId,
        monitor: RemoteDesktopMonitorId,
        reply: Reply<()>,
    },
    OpenPipeWireStream {
        session: RemoteDesktopSessionId,
        monitor: RemoteDesktopMonitorId,
        cursor_mode: CursorMode,
        reply: Reply<StreamInfo>,
    },
    ClosePipeWireStream {
        session: RemoteDesktopSessionId,
        monitor: RemoteDesktopMonitorId,
        stream_id: String,
        reply: Reply<()>,
    },
    ConnectToEis {
        session: RemoteDesktopSessionId,
        socket: OwnedFd,
        reply: Reply<()>,
    },
    EnableClipboard {
        session: RemoteDesktopSessionId,
        reply: Reply<()>,
    },
    DisableClipboard {
        session: RemoteDesktopSessionId,
    },
    SetSelection {
        session: RemoteDesktopSessionId,
        ty: ClipboardType,
        mime_types: Vec<String>,
        reply: Reply<()>,
    },
    SelectionRead {
        session: RemoteDesktopSessionId,
        ty: ClipboardType,
        mime_type: String,
        reply: Reply<OwnedFd>,
    },
    SelectionWrite {
        session: RemoteDesktopSessionId,
        serial: u32,
        reply: Reply<OwnedFd>,
    },
    SelectionWriteDone {
        session: RemoteDesktopSessionId,
        serial: u32,
        success: bool,
    },
}

/// Messages from the compositor back to the D-Bus threads.
///
/// These are handled by a task on the zbus executor, which owns all object server manipulation so
/// the compositor thread never has to block on D-Bus.
pub enum NiriToRemoteDesktop {
    MonitorAdded(MonitorInfo),
    MonitorRemoved(RemoteDesktopMonitorId),
    SessionControlChanged {
        session: RemoteDesktopSessionId,
        has_control: bool,
    },
    /// The session went away for a reason other than `Session.Destroy()`.
    SessionDestroyed(RemoteDesktopSessionId),
    SelectionOwnerChanged {
        session: RemoteDesktopSessionId,
        ty: ClipboardType,
        mime_types: Vec<String>,
        session_is_owner: bool,
    },
    SelectionTransfer {
        session: RemoteDesktopSessionId,
        ty: ClipboardType,
        mime_type: String,
        serial: u32,
    },
}

/// Object-server bookkeeping shared between the interfaces and the event task.
#[derive(Default)]
pub struct Registry {
    /// Live sessions and the unique name of the peer that created each one.
    pub sessions: HashMap<RemoteDesktopSessionId, OwnedUniqueName>,
    /// Live monitors. Each one has an object under every session.
    pub monitors: Vec<MonitorInfo>,
}

#[derive(Clone)]
pub struct RemoteDesktop {
    to_niri: calloop::channel::Sender<RemoteDesktopToNiri>,
    registry: Arc<Mutex<Registry>>,
    enable_input: bool,
    enable_clipboard: bool,
}

impl RemoteDesktop {
    pub fn new(
        to_niri: calloop::channel::Sender<RemoteDesktopToNiri>,
        enable_input: bool,
        enable_clipboard: bool,
    ) -> Self {
        Self {
            to_niri,
            registry: Arc::new(Mutex::new(Registry::default())),
            enable_input,
            enable_clipboard,
        }
    }

    pub fn registry(&self) -> Arc<Mutex<Registry>> {
        self.registry.clone()
    }
}

#[interface(name = "org.freedesktop.RemoteDesktop1")]
impl RemoteDesktop {
    async fn create_session(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        #[zbus(header)] hdr: zbus::message::Header<'_>,
        options: HashMap<String, OwnedValue>,
        session_options: HashMap<String, OwnedValue>,
    ) -> fdo::Result<OwnedObjectPath> {
        let Some(owner) = hdr
            .sender()
            .map(|name| OwnedUniqueName::from(name.to_owned()))
        else {
            return Err(fdo::Error::Failed("message has no sender".to_owned()));
        };

        let persistent = get_bool(&options, "persistent").unwrap_or(false);
        let takes_control = get_bool(&options, "takes-control").unwrap_or(false);
        let disable_animations = get_bool(&session_options, "disable-animations").unwrap_or(false);

        let id = RemoteDesktopSessionId::next();
        debug!(%id, persistent, takes_control, "CreateSession");

        let monitors = call_niri(&self.to_niri, |reply| RemoteDesktopToNiri::CreateSession {
            id,
            persistent,
            takes_control,
            disable_animations,
            reply,
        })
        .await?;

        let session = Session::new(
            id,
            owner.clone(),
            persistent,
            takes_control,
            disable_animations,
            self.to_niri.clone(),
            self.registry.clone(),
            self.enable_input,
            self.enable_clipboard,
        );

        let path = session_path(id);
        // The ObjectManager has to go on first: adding the session interface afterwards is what
        // makes the session itself show up in GetManagedObjects for anyone watching the parent.
        if let Err(err) = server.at(&path, zbus::fdo::ObjectManager).await {
            let _ = self
                .to_niri
                .send(RemoteDesktopToNiri::DestroySession { id, remove: false });
            return Err(fdo::Error::Failed(format!(
                "error creating session object manager: {err:?}"
            )));
        }

        match server.at(&path, session).await {
            Ok(true) => (),
            Ok(false) => {
                return Err(fdo::Error::Failed("session path already exists".to_owned()));
            }
            Err(err) => {
                let _ = self
                    .to_niri
                    .send(RemoteDesktopToNiri::DestroySession { id, remove: false });
                return Err(fdo::Error::Failed(format!(
                    "error creating session object: {err:?}"
                )));
            }
        }

        self.registry.lock().unwrap().sessions.insert(id, owner);

        for info in monitors {
            add_monitor_object(server, &self.to_niri, id, &info).await;
        }

        Ok(path)
    }

    #[zbus(property)]
    async fn version(&self) -> u32 {
        VERSION
    }

    #[zbus(property)]
    async fn capabilities(&self) -> HashMap<String, OwnedValue> {
        let mut caps = HashMap::new();
        // Persistence is process-lifetime only: sessions and monitors survive their peer
        // disconnecting and reconnecting, but not a niri restart.
        caps.insert("persistence".to_owned(), to_owned_value(true));
        caps
    }
}

impl Start for RemoteDesktop {
    fn start(self) -> anyhow::Result<zbus::blocking::Connection> {
        let conn = zbus::blocking::Connection::session()?;
        let flags = RequestNameFlags::AllowReplacement
            | RequestNameFlags::ReplaceExisting
            | RequestNameFlags::DoNotQueue;

        conn.object_server().at(PATH, self)?;
        conn.request_name_with_flags(BUS_NAME, flags)?;

        Ok(conn)
    }
}

/// Starts the tasks that own the compositor -> D-Bus direction.
///
/// Run after `Start::start`, because both tasks need the finished connection.
pub fn spawn_tasks(
    conn: &zbus::blocking::Connection,
    to_niri: calloop::channel::Sender<RemoteDesktopToNiri>,
    from_niri: async_channel::Receiver<NiriToRemoteDesktop>,
    registry: Arc<Mutex<Registry>>,
) {
    let inner = conn.inner().clone();

    let events = {
        let conn = inner.clone();
        let registry = registry.clone();
        let to_niri = to_niri.clone();
        async move {
            while let Ok(msg) = from_niri.recv().await {
                handle_niri_event(&conn, &to_niri, &registry, msg).await;
            }
        }
    };
    inner
        .executor()
        .spawn(events, "remote desktop events")
        .detach();

    let peers = {
        let conn = inner.clone();
        async move {
            if let Err(err) = watch_peers(&conn, &to_niri, &registry).await {
                warn!("error watching remote desktop peers: {err:?}");
            }
        }
    };
    inner
        .executor()
        .spawn(peers, "remote desktop peer watcher")
        .detach();
}

/// Drops sessions whose owning peer disconnected from the bus.
async fn watch_peers(
    conn: &Connection,
    to_niri: &calloop::channel::Sender<RemoteDesktopToNiri>,
    registry: &Arc<Mutex<Registry>>,
) -> zbus::Result<()> {
    use futures_util::StreamExt as _;

    let proxy = fdo::DBusProxy::new(conn).await?;
    let mut changes = proxy
        .receive_name_owner_changed_with_args(&[(2, UniqueName::null_value())])
        .await?;

    while let Some(change) = changes.next().await {
        let Ok(args) = change.args() else { continue };
        let Some(name) = &**args.old_owner() else {
            continue;
        };

        let gone: Vec<_> = {
            let registry = registry.lock().unwrap();
            registry
                .sessions
                .iter()
                .filter(|(_, owner)| owner.as_str() == name.as_str())
                .map(|(id, _)| *id)
                .collect()
        };

        for id in gone {
            debug!(%id, "remote desktop peer disconnected");
            // The compositor decides what a disconnect means: a persistent session keeps its
            // monitors and bound input capabilities, a transient one is torn down. Either way the
            // D-Bus objects go, and the compositor tells us if the session itself is gone.
            let _ = to_niri.send(RemoteDesktopToNiri::DestroySession { id, remove: false });
            remove_session_objects(conn, registry, id).await;
        }
    }

    Ok(())
}

async fn handle_niri_event(
    conn: &Connection,
    to_niri: &calloop::channel::Sender<RemoteDesktopToNiri>,
    registry: &Arc<Mutex<Registry>>,
    msg: NiriToRemoteDesktop,
) {
    let server = conn.object_server();

    match msg {
        NiriToRemoteDesktop::MonitorAdded(info) => {
            let sessions: Vec<_> = registry.lock().unwrap().sessions.keys().copied().collect();
            registry.lock().unwrap().monitors.push(info.clone());

            for session in sessions {
                add_monitor_object(server, to_niri, session, &info).await;
            }
        }
        NiriToRemoteDesktop::MonitorRemoved(monitor) => {
            let sessions: Vec<_> = registry.lock().unwrap().sessions.keys().copied().collect();
            registry
                .lock()
                .unwrap()
                .monitors
                .retain(|info| info.id != monitor);

            for session in sessions {
                let path = monitor_path(session, monitor);
                if let Ok(emitter) = SignalEmitter::new(conn, &path) {
                    if let Err(err) = Monitor::removed(&emitter, HashMap::new()).await {
                        warn!("error emitting Monitor.Removed: {err:?}");
                    }
                }
                if let Err(err) = server.remove::<Monitor, _>(&path).await {
                    warn!("error removing monitor object: {err:?}");
                }
            }
        }
        NiriToRemoteDesktop::SessionControlChanged {
            session,
            has_control,
        } => {
            let path = session_path(session);
            let Ok(iface) = server.interface::<_, Session>(&path).await else {
                return;
            };
            let emitter = iface.signal_emitter().clone();
            let res = {
                let session = iface.get().await;
                session.set_has_control(has_control);
                session.attributes_changed(&emitter).await
            };
            if let Err(err) = res {
                warn!("error notifying Session.Attributes change: {err:?}");
            }
        }
        NiriToRemoteDesktop::SessionDestroyed(session) => {
            let owner = registry.lock().unwrap().sessions.get(&session).cloned();
            let path = session_path(session);
            if let Ok(mut emitter) = SignalEmitter::new(conn, &path) {
                if let Some(owner) = owner {
                    emitter = emitter.set_destination(BusName::Unique(owner.into()));
                }
                if let Err(err) = Session::destroyed(&emitter, HashMap::new()).await {
                    warn!("error emitting Session.Destroyed: {err:?}");
                }
            }
            remove_session_objects(conn, registry, session).await;
        }
        NiriToRemoteDesktop::SelectionOwnerChanged {
            session,
            ty,
            mime_types,
            session_is_owner,
        } => {
            let path = clipboard_path(session);
            let Some(emitter) = session_emitter(conn, registry, session, &path) else {
                return;
            };

            let mut options = HashMap::new();
            options.insert("mime-types".to_owned(), to_owned_value(mime_types));
            options.insert("clipboard-type".to_owned(), to_owned_value(ty as u32));
            options.insert(
                "session-is-owner".to_owned(),
                to_owned_value(session_is_owner),
            );

            if let Err(err) = Clipboard::selection_owner_changed(&emitter, options).await {
                warn!("error emitting Clipboard.SelectionOwnerChanged: {err:?}");
            }
        }
        NiriToRemoteDesktop::SelectionTransfer {
            session,
            ty,
            mime_type,
            serial,
        } => {
            let path = clipboard_path(session);
            let Some(emitter) = session_emitter(conn, registry, session, &path) else {
                return;
            };

            let mut options = HashMap::new();
            options.insert("clipboard-type".to_owned(), to_owned_value(ty as u32));

            if let Err(err) =
                Clipboard::selection_transfer(&emitter, &mime_type, serial, options).await
            {
                warn!("error emitting Clipboard.SelectionTransfer: {err:?}");
            }
        }
    }
}

/// A signal emitter addressed only at the peer that owns the session.
fn session_emitter<'a>(
    conn: &Connection,
    registry: &Arc<Mutex<Registry>>,
    session: RemoteDesktopSessionId,
    path: &'a ObjectPath<'a>,
) -> Option<SignalEmitter<'a>> {
    let owner = registry.lock().unwrap().sessions.get(&session).cloned()?;
    let emitter = SignalEmitter::new(conn, path).ok()?;
    Some(emitter.set_destination(BusName::Unique(owner.into())))
}

async fn add_monitor_object(
    server: &ObjectServer,
    to_niri: &calloop::channel::Sender<RemoteDesktopToNiri>,
    session: RemoteDesktopSessionId,
    info: &MonitorInfo,
) {
    let path = monitor_path(session, info.id);
    let monitor = Monitor::new(session, info.clone(), to_niri.clone());
    if let Err(err) = server.at(&path, monitor).await {
        warn!("error creating monitor object: {err:?}");
    }
}

pub async fn remove_session_objects(
    conn: &Connection,
    registry: &Arc<Mutex<Registry>>,
    session: RemoteDesktopSessionId,
) {
    let server = conn.object_server();

    let monitors: Vec<_> = registry
        .lock()
        .unwrap()
        .monitors
        .iter()
        .map(|info| info.id)
        .collect();
    for monitor in monitors {
        let _ = server
            .remove::<Monitor, _>(&monitor_path(session, monitor))
            .await;
    }

    let _ = server
        .remove::<Clipboard, _>(&clipboard_path(session))
        .await;

    let path = session_path(session);
    let _ = server.remove::<Session, _>(&path).await;
    let _ = server.remove::<zbus::fdo::ObjectManager, _>(&path).await;

    registry.lock().unwrap().sessions.remove(&session);
}

/// Sends a request to the compositor and waits for its reply.
pub async fn call_niri<T, F>(
    to_niri: &calloop::channel::Sender<RemoteDesktopToNiri>,
    make_msg: F,
) -> fdo::Result<T>
where
    F: FnOnce(Reply<T>) -> RemoteDesktopToNiri,
{
    let (tx, rx) = async_channel::bounded(1);
    if let Err(err) = to_niri.send(make_msg(tx)) {
        warn!("error sending message to niri: {err:?}");
        return Err(fdo::Error::Failed("internal error".to_owned()));
    }

    match rx.recv().await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(err)) => Err(fdo::Error::Failed(err)),
        Err(err) => {
            warn!("error receiving message from niri: {err:?}");
            Err(fdo::Error::Failed("internal error".to_owned()))
        }
    }
}

pub fn get_bool(options: &HashMap<String, OwnedValue>, key: &str) -> Option<bool> {
    bool::try_from(options.get(key)?).ok()
}

pub fn get_u32(options: &HashMap<String, OwnedValue>, key: &str) -> Option<u32> {
    u32::try_from(options.get(key)?).ok()
}

pub fn get_string(options: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    String::try_from(options.get(key)?.clone()).ok()
}

pub fn get_string_vec(options: &HashMap<String, OwnedValue>, key: &str) -> Option<Vec<String>> {
    Vec::<String>::try_from(options.get(key)?.clone()).ok()
}

pub fn to_owned_value<'a, T: Into<Value<'a>>>(value: T) -> OwnedValue {
    // Values built from owned Rust data never contain a borrowed fd, so this cannot fail.
    OwnedValue::try_from(value.into()).unwrap()
}
