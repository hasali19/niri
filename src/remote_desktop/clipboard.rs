//! Clipboard integration for `org.freedesktop.RemoteDesktop1.Clipboard`.
//!
//! Two directions:
//!
//! * remote -> Wayland: `SetSelection` makes the session the selection owner. When a Wayland client
//!   reads it, [`crate::niri::State::send_selection`] parks the client's fd and emits
//!   `SelectionTransfer`; the service answers with `SelectionWrite`, which hands the fd back.
//! * Wayland -> remote: `SelectionRead` pipes the current Wayland selection to the service, and
//!   `SelectionOwnerChanged` tells it when the Wayland side changes.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::time::Duration;

use calloop::RegistrationToken;
use smithay::wayland::selection::SelectionTarget;

use crate::dbus::remote_desktop::ClipboardType;

/// How long a parked transfer fd is kept before we give up on the service answering.
pub const TRANSFER_TIMEOUT: Duration = Duration::from_secs(30);

/// Clipboard state of one session.
#[derive(Default)]
pub struct SessionClipboard {
    /// Mime types this session currently offers, per selection.
    pub offered: HashMap<ClipboardType, Vec<String>>,
    /// Wayland clients waiting for the service to write the selection data.
    pub pending: HashMap<u32, PendingTransfer>,
    next_serial: u32,
}

pub struct PendingTransfer {
    /// The fd the Wayland client wants the data written into.
    ///
    /// Taken by `SelectionWrite`; `None` afterwards, while we wait for `SelectionWriteDone`.
    pub fd: Option<OwnedFd>,
    pub timeout: Option<RegistrationToken>,
}

impl SessionClipboard {
    pub fn next_serial(&mut self) -> u32 {
        self.next_serial = self.next_serial.wrapping_add(1);
        if self.next_serial == 0 {
            self.next_serial = 1;
        }
        self.next_serial
    }

    pub fn owns(&self, ty: ClipboardType) -> bool {
        self.offered.contains_key(&ty)
    }
}

impl From<ClipboardType> for SelectionTarget {
    fn from(value: ClipboardType) -> Self {
        match value {
            ClipboardType::Clipboard => SelectionTarget::Clipboard,
            ClipboardType::Primary => SelectionTarget::Primary,
        }
    }
}

pub fn clipboard_type_of(target: SelectionTarget) -> ClipboardType {
    match target {
        SelectionTarget::Clipboard => ClipboardType::Clipboard,
        SelectionTarget::Primary => ClipboardType::Primary,
    }
}
