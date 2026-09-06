//! `org.freedesktop.RemoteDesktop1.Session`.

use std::collections::HashMap;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use zbus::names::{OwnedUniqueName, UniqueName};
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};
use zbus::{fdo, interface, ObjectServer};

use super::clipboard::Clipboard;
use super::{
    add_monitor_object, call_niri, clipboard_path, get_bool, get_string, remove_session_objects,
    to_owned_value, Registry, RemoteDesktopToNiri, VERSION,
};
use crate::utils::RemoteDesktopSessionId;

#[derive(Clone)]
pub struct Session {
    id: RemoteDesktopSessionId,
    owner: OwnedUniqueName,
    persistent: bool,
    has_control: Arc<AtomicBool>,
    disable_animations: Arc<AtomicBool>,
    destroyed: Arc<AtomicBool>,
    to_niri: calloop::channel::Sender<RemoteDesktopToNiri>,
    registry: Arc<Mutex<Registry>>,
    enable_input: bool,
    enable_clipboard: bool,
}

impl Session {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: RemoteDesktopSessionId,
        owner: OwnedUniqueName,
        persistent: bool,
        has_control: bool,
        disable_animations: bool,
        to_niri: calloop::channel::Sender<RemoteDesktopToNiri>,
        registry: Arc<Mutex<Registry>>,
        enable_input: bool,
        enable_clipboard: bool,
    ) -> Self {
        Self {
            id,
            owner,
            persistent,
            has_control: Arc::new(AtomicBool::new(has_control)),
            disable_animations: Arc::new(AtomicBool::new(disable_animations)),
            destroyed: Arc::new(AtomicBool::new(false)),
            to_niri,
            registry,
            enable_input,
            enable_clipboard,
        }
    }

    pub fn set_has_control(&self, value: bool) {
        self.has_control.store(value, Ordering::SeqCst);
    }

    /// Methods and signals are only for the peer that created the session.
    fn check_sender(&self, hdr: &zbus::message::Header<'_>) -> fdo::Result<()> {
        if hdr.sender().map(UniqueName::as_str) == Some(self.owner.as_str()) {
            Ok(())
        } else {
            Err(fdo::Error::AccessDenied(
                "only the peer that created this session may use it".to_owned(),
            ))
        }
    }
}

#[interface(name = "org.freedesktop.RemoteDesktop1.Session")]
impl Session {
    async fn destroy(
        &self,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] hdr: zbus::message::Header<'_>,
        options: HashMap<String, OwnedValue>,
    ) -> fdo::Result<()> {
        self.check_sender(&hdr)?;

        if self.destroyed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }

        let remove = get_bool(&options, "remove").unwrap_or(false);
        debug!(id = %self.id, remove, "Session.Destroy");

        let _ = self.to_niri.send(RemoteDesktopToNiri::DestroySession {
            id: self.id,
            remove,
        });

        // Destroy() explicitly does not emit Destroyed, so tear the objects down here rather than
        // waiting for the compositor to send SessionDestroyed back.
        remove_session_objects(conn, &self.registry, self.id).await;

        Ok(())
    }

    // zbus would derive `ConnectToEis` from the method name.
    #[zbus(name = "ConnectToEIS")]
    async fn connect_to_eis(
        &self,
        #[zbus(header)] hdr: zbus::message::Header<'_>,
        _options: HashMap<String, OwnedValue>,
    ) -> fdo::Result<zbus::zvariant::OwnedFd> {
        self.check_sender(&hdr)?;

        if !self.enable_input {
            return Err(fdo::Error::NotSupported(
                "input emulation is not enabled; add enable-input to the remote-desktop config \
                 section"
                    .to_owned(),
            ));
        }

        debug!(id = %self.id, "Session.ConnectToEIS");

        let (ours, theirs) = UnixStream::pair()
            .map_err(|err| fdo::Error::Failed(format!("error creating socket pair: {err}")))?;

        call_niri(&self.to_niri, |reply| RemoteDesktopToNiri::ConnectToEis {
            session: self.id,
            socket: ours.into(),
            reply,
        })
        .await?;

        Ok(std::os::fd::OwnedFd::from(theirs).into())
    }

    async fn create_virtual_monitor(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        #[zbus(header)] hdr: zbus::message::Header<'_>,
        options: HashMap<String, OwnedValue>,
    ) -> fdo::Result<OwnedObjectPath> {
        self.check_sender(&hdr)?;

        let persistent = get_bool(&options, "persistent").unwrap_or(false);
        if persistent && !self.persistent {
            return Err(fdo::Error::InvalidArgs(
                "persistent monitors require a persistent session".to_owned(),
            ));
        }

        let name = get_string(&options, "name");
        debug!(id = %self.id, ?name, persistent, "Session.CreateVirtualMonitor");

        let info = call_niri(&self.to_niri, |reply| {
            RemoteDesktopToNiri::CreateVirtualMonitor {
                session: self.id,
                name,
                persistent,
                reply,
            }
        })
        .await?;

        // The compositor broadcasts MonitorAdded to every other session; this one gets its object
        // here so the path is live by the time the method returns.
        let path = super::monitor_path(self.id, info.id);
        add_monitor_object(server, &self.to_niri, self.id, &info).await;

        Ok(path)
    }

    async fn enable_clipboard(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        #[zbus(header)] hdr: zbus::message::Header<'_>,
    ) -> fdo::Result<OwnedObjectPath> {
        self.check_sender(&hdr)?;

        if !self.enable_clipboard {
            return Err(fdo::Error::NotSupported(
                "clipboard integration is not enabled; add enable-clipboard to the remote-desktop \
                 config section"
                    .to_owned(),
            ));
        }

        debug!(id = %self.id, "Session.EnableClipboard");

        call_niri(&self.to_niri, |reply| {
            RemoteDesktopToNiri::EnableClipboard {
                session: self.id,
                reply,
            }
        })
        .await?;

        let path = clipboard_path(self.id);
        let clipboard = Clipboard::new(self.id, self.owner.clone(), self.to_niri.clone());
        match server.at(&path, clipboard).await {
            Ok(_) => Ok(path),
            Err(err) => {
                let _ = self
                    .to_niri
                    .send(RemoteDesktopToNiri::DisableClipboard { session: self.id });
                Err(fdo::Error::Failed(format!(
                    "error creating clipboard object: {err:?}"
                )))
            }
        }
    }

    #[zbus(property)]
    async fn options(&self) -> HashMap<String, OwnedValue> {
        let mut options = HashMap::new();
        options.insert(
            "disable-animations".to_owned(),
            to_owned_value(self.disable_animations.load(Ordering::SeqCst)),
        );
        options
    }

    #[zbus(property)]
    async fn set_options(&self, options: HashMap<String, OwnedValue>) -> zbus::Result<()> {
        if let Some(value) = get_bool(&options, "disable-animations") {
            self.disable_animations.store(value, Ordering::SeqCst);
            let _ = self
                .to_niri
                .send(RemoteDesktopToNiri::SetDisableAnimations { id: self.id, value });
        }
        Ok(())
    }

    #[zbus(property)]
    async fn attributes(&self) -> HashMap<String, OwnedValue> {
        let mut attributes = HashMap::new();
        attributes.insert(
            "has-control".to_owned(),
            to_owned_value(self.has_control.load(Ordering::SeqCst)),
        );
        attributes
    }

    #[zbus(signal)]
    pub async fn destroyed(
        ctxt: &SignalEmitter<'_>,
        options: HashMap<String, OwnedValue>,
    ) -> zbus::Result<()>;

    #[zbus(property)]
    async fn version(&self) -> u32 {
        VERSION
    }
}
