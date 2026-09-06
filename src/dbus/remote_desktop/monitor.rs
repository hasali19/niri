//! `org.freedesktop.RemoteDesktop1.Monitor`.

use std::collections::HashMap;

use zbus::object_server::SignalEmitter;
use zbus::zvariant::OwnedValue;
use zbus::{fdo, interface, ObjectServer};

use super::{
    call_niri, get_u32, to_owned_value, CursorMode, MonitorInfo, RemoteDesktopToNiri, VERSION,
};
use crate::utils::RemoteDesktopSessionId;

#[derive(Clone)]
pub struct Monitor {
    /// Session this object belongs to. The same monitor is exposed under every session.
    session: RemoteDesktopSessionId,
    info: MonitorInfo,
    to_niri: calloop::channel::Sender<RemoteDesktopToNiri>,
}

impl Monitor {
    pub fn new(
        session: RemoteDesktopSessionId,
        info: MonitorInfo,
        to_niri: calloop::channel::Sender<RemoteDesktopToNiri>,
    ) -> Self {
        Self {
            session,
            info,
            to_niri,
        }
    }

    fn is_owned(&self) -> bool {
        self.info.owner == Some(self.session)
    }
}

#[interface(name = "org.freedesktop.RemoteDesktop1.Monitor")]
impl Monitor {
    async fn remove(&self, #[zbus(object_server)] _server: &ObjectServer) -> fdo::Result<()> {
        if !self.is_owned() {
            return Err(fdo::Error::AccessDenied(
                "this monitor belongs to another session".to_owned(),
            ));
        }

        debug!(monitor = %self.info.id, "Monitor.Remove");

        // The compositor answers with MonitorRemoved, which emits Removed and drops the objects
        // under every session, so nothing is done here.
        call_niri(&self.to_niri, |reply| {
            RemoteDesktopToNiri::RemoveVirtualMonitor {
                session: self.session,
                monitor: self.info.id,
                reply,
            }
        })
        .await
    }

    #[zbus(property)]
    async fn cursor_modes(&self) -> Vec<u32> {
        vec![
            CursorMode::Hidden as u32,
            CursorMode::Embedded as u32,
            CursorMode::Metadata as u32,
        ]
    }

    async fn open_pipe_wire_stream(
        &self,
        options: HashMap<String, OwnedValue>,
    ) -> fdo::Result<(String, HashMap<String, OwnedValue>)> {
        let cursor_mode = match get_u32(&options, "cursor-mode") {
            Some(value) => CursorMode::from_dbus(value)
                .ok_or_else(|| fdo::Error::InvalidArgs(format!("bad cursor mode: {value}")))?,
            None => CursorMode::default(),
        };

        debug!(monitor = %self.info.id, ?cursor_mode, "Monitor.OpenPipeWireStream");

        let stream = call_niri(&self.to_niri, |reply| {
            RemoteDesktopToNiri::OpenPipeWireStream {
                session: self.session,
                monitor: self.info.id,
                cursor_mode,
                reply,
            }
        })
        .await?;

        let mut info = HashMap::new();
        info.insert("node-id".to_owned(), to_owned_value(stream.node_id));
        info.insert("serial".to_owned(), to_owned_value(stream.serial));

        Ok((stream.stream_id, info))
    }

    async fn close_pipe_wire_stream(&self, id: String) -> fdo::Result<()> {
        debug!(monitor = %self.info.id, id, "Monitor.ClosePipeWireStream");

        call_niri(&self.to_niri, |reply| {
            RemoteDesktopToNiri::ClosePipeWireStream {
                session: self.session,
                monitor: self.info.id,
                stream_id: id,
                reply,
            }
        })
        .await
    }

    #[zbus(property)]
    async fn attributes(&self) -> HashMap<String, OwnedValue> {
        let mut attributes = HashMap::new();
        attributes.insert("owned".to_owned(), to_owned_value(self.is_owned()));
        attributes.insert("name".to_owned(), to_owned_value(self.info.name.clone()));
        attributes
    }

    #[zbus(signal)]
    pub async fn removed(
        ctxt: &SignalEmitter<'_>,
        options: HashMap<String, OwnedValue>,
    ) -> zbus::Result<()>;

    #[zbus(property)]
    async fn version(&self) -> u32 {
        VERSION
    }
}
