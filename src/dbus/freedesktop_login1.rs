use futures_util::StreamExt;
use zbus::fdo;
use zbus::names::InterfaceName;

pub enum Login1ToNiri {
    LidClosedChanged(bool),
    /// The login session that niri runs in was removed.
    SessionRemoved,
}

/// Starts watching logind.
///
/// If `session_id` is set, also watches for that login session getting removed (for example, when
/// GDM force-stops it to let the user log in remotely).
pub fn start(
    to_niri: calloop::channel::Sender<Login1ToNiri>,
    session_id: Option<String>,
) -> anyhow::Result<zbus::blocking::Connection> {
    let conn = zbus::blocking::Connection::system()?;

    if let Some(session_id) = session_id {
        let async_conn = conn.inner().clone();
        let to_niri = to_niri.clone();
        let future = async move {
            let proxy = zbus::Proxy::new(
                &async_conn,
                "org.freedesktop.login1",
                "/org/freedesktop/login1",
                "org.freedesktop.login1.Manager",
            )
            .await;
            let proxy = match proxy {
                Ok(x) => x,
                Err(err) => {
                    warn!("error creating login1 Manager proxy: {err:?}");
                    return;
                }
            };

            let mut removed = match proxy.receive_signal("SessionRemoved").await {
                Ok(x) => x,
                Err(err) => {
                    warn!("error subscribing to SessionRemoved: {err:?}");
                    return;
                }
            };

            while let Some(message) = removed.next().await {
                let body = message.body();
                let Ok((id, _path)) =
                    body.deserialize::<(String, zbus::zvariant::OwnedObjectPath)>()
                else {
                    warn!("error parsing SessionRemoved args");
                    continue;
                };

                if id == session_id {
                    if let Err(err) = to_niri.send(Login1ToNiri::SessionRemoved) {
                        warn!("error sending message to niri: {err:?}");
                    }
                    return;
                }
            }
        };

        let task = conn
            .inner()
            .executor()
            .spawn(future, "monitor login1 session removal");
        task.detach();
    }

    let async_conn = conn.inner().clone();
    let future = async move {
        let proxy = fdo::PropertiesProxy::new(
            &async_conn,
            "org.freedesktop.login1",
            "/org/freedesktop/login1",
        )
        .await;
        let proxy = match proxy {
            Ok(x) => x,
            Err(err) => {
                warn!("error creating PropertiesProxy: {err:?}");
                return;
            }
        };

        let mut props_changed = match proxy.receive_properties_changed().await {
            Ok(x) => x,
            Err(err) => {
                warn!("error subscribing to PropertiesChanged: {err:?}");
                return;
            }
        };

        let props = proxy
            .get_all(InterfaceName::try_from("org.freedesktop.login1.Manager").unwrap())
            .await;
        let mut props = match props {
            Ok(x) => x,
            Err(err) => {
                warn!("error receiving initial properties: {err:?}");
                return;
            }
        };

        trace!("initial properties: {props:?}");

        let mut lid_closed = props
            .remove("LidClosed")
            .and_then(|value| bool::try_from(value).ok())
            .unwrap_or_default();

        if let Err(err) = to_niri.send(Login1ToNiri::LidClosedChanged(lid_closed)) {
            warn!("error sending initial lid state to niri: {err:?}");
            return;
        };

        while let Some(signal) = props_changed.next().await {
            let args = match signal.args() {
                Ok(args) => args,
                Err(err) => {
                    warn!("error parsing PropertiesChanged args: {err:?}");
                    return;
                }
            };

            let mut new_lid_closed = lid_closed;
            let mut changed = false;
            for (name, value) in args.changed_properties() {
                trace!("changed property: {name} => {value:?}");
                if *name != "LidClosed" {
                    continue;
                }

                new_lid_closed = bool::try_from(value).unwrap_or(new_lid_closed);
                changed = true;
            }

            if !changed {
                continue;
            }

            if new_lid_closed == lid_closed {
                continue;
            }

            lid_closed = new_lid_closed;
            if let Err(err) = to_niri.send(Login1ToNiri::LidClosedChanged(lid_closed)) {
                warn!("error sending message to niri: {err:?}");
                return;
            };
        }
    };

    let task = conn
        .inner()
        .executor()
        .spawn(future, "monitor login1 property changes");
    task.detach();

    Ok(conn)
}

/// Information about the logind session that niri runs in.
#[derive(Debug)]
pub struct SessionInfo {
    /// Whether the session has a seat. Sessions without a seat (for example, remote logins through
    /// GDM) have no access to monitors or input devices.
    pub has_seat: bool,
    /// Whether the session is a remote login.
    pub remote: bool,
    /// The session class, for example `user` or `greeter`.
    pub class: String,
}

/// Queries logind for the session in `XDG_SESSION_ID`.
pub fn current_session_info() -> anyhow::Result<SessionInfo> {
    use anyhow::Context as _;
    use zbus::zvariant::{ObjectPath, OwnedObjectPath};

    let session_id = std::env::var("XDG_SESSION_ID").context("XDG_SESSION_ID is unset")?;

    let conn =
        zbus::blocking::Connection::system().context("error connecting to the system bus")?;

    let message = conn
        .call_method(
            Some("org.freedesktop.login1"),
            "/org/freedesktop/login1",
            Some("org.freedesktop.login1.Manager"),
            "GetSession",
            &(session_id),
        )
        .context("failed to call GetSession")?;
    let body = message.body();
    let session_path: ObjectPath = body
        .deserialize()
        .context("failed to deserialize GetSession reply")?;

    let proxy = zbus::blocking::fdo::PropertiesProxy::builder(&conn)
        .destination("org.freedesktop.login1")?
        .path(session_path)?
        .build()
        .context("error creating PropertiesProxy")?;
    let iface = InterfaceName::try_from("org.freedesktop.login1.Session").unwrap();

    let (seat, _): (String, OwnedObjectPath) = proxy
        .get(iface.clone(), "Seat")
        .context("error getting Seat")?
        .try_into()
        .context("wrong Seat type")?;
    let remote: bool = proxy
        .get(iface.clone(), "Remote")
        .context("error getting Remote")?
        .try_into()
        .context("wrong Remote type")?;
    let class: String = proxy
        .get(iface, "Class")
        .context("error getting Class")?
        .try_into()
        .context("wrong Class type")?;

    Ok(SessionInfo {
        has_seat: !seat.is_empty(),
        remote,
        class,
    })
}

/// Starts or stops gnome-remote-desktop's handover daemon for a remote login.
///
/// The handover daemon takes over the RDP connection from the gnome-remote-desktop system daemon
/// that the user logged in through, and connects it to this session. In GNOME,
/// gnome-settings-daemon starts and stops it.
pub fn set_remote_desktop_handover_running(running: bool) -> anyhow::Result<()> {
    use anyhow::Context as _;

    let unit = "gnome-remote-desktop-handover.service";
    let method = if running { "StartUnit" } else { "StopUnit" };

    let conn =
        zbus::blocking::Connection::session().context("error connecting to the session bus")?;
    conn.call_method(
        Some("org.freedesktop.systemd1"),
        "/org/freedesktop/systemd1",
        Some("org.freedesktop.systemd1.Manager"),
        method,
        &(unit, "replace"),
    )
    .with_context(|| format!("failed to call {method} for {unit}"))?;

    Ok(())
}
