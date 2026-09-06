//! `org.freedesktop.RemoteDesktop1.Clipboard`.

use std::collections::HashMap;

use zbus::names::{OwnedUniqueName, UniqueName};
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedFd, OwnedValue};
use zbus::{fdo, interface, ObjectServer};

use super::{call_niri, get_string_vec, get_u32, ClipboardType, RemoteDesktopToNiri, VERSION};
use crate::utils::RemoteDesktopSessionId;

#[derive(Clone)]
pub struct Clipboard {
    session: RemoteDesktopSessionId,
    owner: OwnedUniqueName,
    to_niri: calloop::channel::Sender<RemoteDesktopToNiri>,
}

impl Clipboard {
    pub fn new(
        session: RemoteDesktopSessionId,
        owner: OwnedUniqueName,
        to_niri: calloop::channel::Sender<RemoteDesktopToNiri>,
    ) -> Self {
        Self {
            session,
            owner,
            to_niri,
        }
    }

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

fn clipboard_type(options: &HashMap<String, OwnedValue>) -> fdo::Result<ClipboardType> {
    match get_u32(options, "clipboard-type") {
        Some(value) => ClipboardType::from_dbus(value)
            .ok_or_else(|| fdo::Error::InvalidArgs(format!("bad clipboard type: {value}"))),
        None => Ok(ClipboardType::default()),
    }
}

#[interface(name = "org.freedesktop.RemoteDesktop1.Clipboard")]
impl Clipboard {
    async fn destroy(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        #[zbus(header)] hdr: zbus::message::Header<'_>,
    ) -> fdo::Result<()> {
        self.check_sender(&hdr)?;

        debug!(session = %self.session, "Clipboard.Destroy");

        let _ = self.to_niri.send(RemoteDesktopToNiri::DisableClipboard {
            session: self.session,
        });

        let path = super::clipboard_path(self.session);
        let _ = server.remove::<Clipboard, _>(&path).await;

        Ok(())
    }

    async fn set_selection(
        &self,
        #[zbus(header)] hdr: zbus::message::Header<'_>,
        options: HashMap<String, OwnedValue>,
    ) -> fdo::Result<()> {
        self.check_sender(&hdr)?;

        let ty = clipboard_type(&options)?;
        let mime_types = get_string_vec(&options, "mime-types").unwrap_or_default();

        debug!(session = %self.session, ?ty, ?mime_types, "Clipboard.SetSelection");

        call_niri(&self.to_niri, |reply| RemoteDesktopToNiri::SetSelection {
            session: self.session,
            ty,
            mime_types,
            reply,
        })
        .await
    }

    async fn selection_write(
        &self,
        #[zbus(header)] hdr: zbus::message::Header<'_>,
        serial: u32,
        _options: HashMap<String, OwnedValue>,
    ) -> fdo::Result<OwnedFd> {
        self.check_sender(&hdr)?;

        let fd = call_niri(&self.to_niri, |reply| RemoteDesktopToNiri::SelectionWrite {
            session: self.session,
            serial,
            reply,
        })
        .await?;

        Ok(fd.into())
    }

    async fn selection_write_done(
        &self,
        #[zbus(header)] hdr: zbus::message::Header<'_>,
        serial: u32,
        success: bool,
        _options: HashMap<String, OwnedValue>,
    ) -> fdo::Result<()> {
        self.check_sender(&hdr)?;

        let _ = self.to_niri.send(RemoteDesktopToNiri::SelectionWriteDone {
            session: self.session,
            serial,
            success,
        });

        Ok(())
    }

    async fn selection_read(
        &self,
        #[zbus(header)] hdr: zbus::message::Header<'_>,
        mime_type: String,
        options: HashMap<String, OwnedValue>,
    ) -> fdo::Result<OwnedFd> {
        self.check_sender(&hdr)?;

        let ty = clipboard_type(&options)?;
        debug!(session = %self.session, ?ty, mime_type, "Clipboard.SelectionRead");

        let fd = call_niri(&self.to_niri, |reply| RemoteDesktopToNiri::SelectionRead {
            session: self.session,
            ty,
            mime_type,
            reply,
        })
        .await?;

        Ok(fd.into())
    }

    #[zbus(signal)]
    pub async fn selection_owner_changed(
        ctxt: &SignalEmitter<'_>,
        options: HashMap<String, OwnedValue>,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn selection_transfer(
        ctxt: &SignalEmitter<'_>,
        mime_type: &str,
        serial: u32,
        options: HashMap<String, OwnedValue>,
    ) -> zbus::Result<()>;

    #[zbus(property)]
    async fn supported_types(&self) -> Vec<u32> {
        vec![
            ClipboardType::Clipboard as u32,
            ClipboardType::Primary as u32,
        ]
    }

    #[zbus(property)]
    async fn version(&self) -> u32 {
        VERSION
    }
}
