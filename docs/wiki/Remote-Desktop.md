# Remote Desktop

niri implements `org.freedesktop.RemoteDesktop1`, the D-Bus API from
[xdg-specs MR !115](https://gitlab.freedesktop.org/xdg/xdg-specs/-/merge_requests/115). It lets a
*remote desktop service* — the process that speaks RDP, VNC or some other protocol to a remote
user — drive a niri session where the primary way to access the machine is remotely.

This is not screen sharing. Screen sharing and remote assistance of a desktop somebody is sitting
in front of should go through the
[screencast](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.ScreenCast.html)
and
[remote desktop](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.RemoteDesktop.html)
portals, where the user gets to approve each session. This API has no approval step at all, which
is the point: it is for unattended access, where nobody is there to click anything.

> [!WARNING]
> Enabling this gives every peer on your session bus the ability to capture the screen and inject
> input, with no prompt. Only enable it on machines where that is what you want.

## Enabling it

It is off by default and needs two things: a niri built with the `remote-desktop` Cargo feature
(on by default), and a `remote-desktop` section in the config.

```kdl
remote-desktop {
    // Let services connect to the EIS input server and inject input.
    enable-input

    // Let services integrate with the clipboard.
    enable-clipboard

    // Mode that new virtual monitors start with. A service can change it afterwards by
    // negotiating a different size on the PipeWire stream.
    virtual-monitor-default-mode "1920x1080@60"
}
```

An empty `remote-desktop { }` section starts the D-Bus service but allows neither input nor
clipboard: `ConnectToEIS()` and `EnableClipboard()` return a not-supported error.

The service appears on the session bus as `org.freedesktop.RemoteDesktop`, at
`/org/freedesktop/RemoteDesktop1`. It is only started for a session instance (`niri --session`), or
when [`debug { dbus-interfaces-in-non-session-instances }`](./Configuration:-Debug-Options.md) is
set.

## How a service uses it

### Sessions

`org.freedesktop.RemoteDesktop1.CreateSession()` returns a session object path. Two options matter:

- `takes-control` (`b`) — only one session may have control at a time. Creating a controlling
  session terminates whatever controlling session existed before, which gets a `Destroyed` signal.
- `persistent` (`b`) — the session's virtual monitors and bound input devices survive the D-Bus
  peer disconnecting and reconnecting.

Only the peer that created a session may call its methods; anyone else gets `AccessDenied`. When a
peer drops off the bus, its non-persistent sessions are torn down along with their monitors.

niri's persistence is process-lifetime only: a persistent session survives its peer reconnecting,
but not a niri restart. `Capabilities` reports `persistence: true` on that basis.

### Virtual monitors

`Session.CreateVirtualMonitor()` creates a niri [virtual output](./Virtual-Outputs.md) and returns
a `org.freedesktop.RemoteDesktop1.Monitor` object under the session path. The
`org.freedesktop.DBus.ObjectManager` on the session path lists them, and fires
`InterfacesAdded`/`InterfacesRemoved` as they come and go.

Every session sees every virtual output, not just its own — including ones made with
`niri msg create-virtual-output` and the `HEADLESS-1` output the headless backend creates by
default. `Attributes.owned` says whether the monitor belongs to the session looking at it; only its
owner may `Remove()` it.

`Monitor.OpenPipeWireStream()` returns the `node-id` and `serial` of a PipeWire stream carrying the
monitor's contents, so a service can pull frames with `pipewiresrc` or the PipeWire API directly.
`cursor-mode` picks how the cursor is drawn: `1` hidden, `2` embedded in the frames, `3` (the
default) as PipeWire stream metadata.

The stream offers a *range* of sizes rather than a fixed one, so the size the service negotiates
becomes the size of the virtual monitor, as the spec describes. Until something negotiates, the
monitor keeps `virtual-monitor-default-mode`.

### Input

`Session.ConnectToEIS()` returns a socket speaking the
[libei](https://gitlab.freedesktop.org/libinput/libei) protocol. The service binds the device
capabilities it wants — relative pointer, absolute pointer, keyboard, touch — and emits events on
them.

Absolute pointer and touch coordinates are niri's global logical coordinates; the regions niri
advertises on those devices are the output geometries, so a point maps onto whichever output
contains it. Keyboard devices come with niri's current xkb keymap.

Injected events go through the same path as local input, so they trigger keybinds, wake monitors,
notify the idle notifier and update focus-follows-mouse. A remote user can use your `Mod+T` bind.

For a persistent session, unbinding a capability permanently removes it; simply disconnecting keeps
it, so the same devices come back on reconnect.

### Clipboard

`Session.EnableClipboard()` returns a `org.freedesktop.RemoteDesktop1.Clipboard` object. Both the
regular clipboard (type `1`) and the primary selection (type `2`) are supported.

- Remote to local: `SetSelection()` with the mime types the service has data for. When a Wayland
  client reads the selection, the service gets a `SelectionTransfer` signal, answers with
  `SelectionWrite()` to get a file descriptor, writes the data, and calls `SelectionWriteDone()`. A
  transfer the service never answers is dropped after 30 seconds.
- Local to remote: `SelectionOwnerChanged` fires whenever a Wayland client takes the selection, and
  `SelectionRead()` returns a file descriptor to read the data from.

## A minimal session

```
busctl --user call org.freedesktop.RemoteDesktop /org/freedesktop/RemoteDesktop1 \
    org.freedesktop.RemoteDesktop1 CreateSession 'a{sv}a{sv}' \
    2 persistent b true takes-control b true 0
```

Note that `busctl` exits after each call, which drops its bus name and therefore the session; a
real service keeps one connection open for the lifetime of the session.

## Advertising support

niri's `.desktop` file carries `X-RemoteDesktop1-Supported=true`, which the spec uses to say the
session supports this API. It advertises build-time support — the config still has to enable it.

## See also

- [Virtual Outputs](./Virtual-Outputs.md) for the same virtual monitors through `niri msg` and the
  config.
- [Screencasting](./Screencasting.md) for the portal-based path, which is what you want for screen
  sharing with a user present.
- [Security Model](./Security-Model.md) for how niri thinks about privileged protocols in general.
