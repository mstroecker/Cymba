# The desktop

What the desktop around BlueZ does that a client has to know: PipeWire following the
transport's volume, MPRIS, the path a press on a pod takes to a player, and UPower's
warning level for a Bluetooth battery, how GApplication keeps an app to one instance,
and how it serves its actions on the bus, where GLib sends a notification and what GNOME
Shell does with its button, what logind and GNOME say of whether the user is at the desktop, and what
Flatpak's bus proxy lets through. Checked on hardware where a run below says so; MPRIS
is from its specification, UPower also from its source, GLib, GNOME Shell,
gnome-session, logind, Flatpak and the bus proxy from their documentation and source, and the
key path is only as understood, not traced.

The app acts on these in `volume`, `media`, `notify`, `nearby`, `presence` and
`ui::connect`: [app/volume.md](../../app/volume.md),
[app/media-pause.md](../../app/media-pause.md),
[app/notifications.md](../../app/notifications.md),
[app/nearby.md](../../app/nearby.md). On GApplication's, at its startup:
[app/from-linuxpods.md](../../app/from-linuxpods.md#when); on its actions, for a connect:
[app/ui.md](../../app/ui.md#the-connect-banner),
[app/bluez.md](../../app/bluez.md#connecting-a-pair).

## The volume

**PipeWire's sink follows the transport's `Volume`.** Set on the pair's A2DP transport,
`Volume` moved the pair's sink within 0.5 s (Pro 3, 2026-11-01, #150; run below).

The transport's `Volume`: [bluez.md](bluez.md#mediatransport1volume).

## MPRIS

Media players are controlled over MPRIS, on the session bus
([MPRIS D-Bus Interface Specification](https://specifications.freedesktop.org/mpris-spec/latest/)
2.2, cited in #167):

- **Players** own names starting `org.mpris.MediaPlayer2.`, and serve
  `org.mpris.MediaPlayer2.Player` at `/org/mpris/MediaPlayer2`.
- **`PlaybackStatus`** reads `Playing`, `Paused` or `Stopped`.
- **`Pause` and `Play`** each go one way only: `Pause` on a paused player and `Play` on
  a playing one change nothing.
- **`PlayPause` is a toggle**: sent to a player that is not playing, it starts it.

## A press on a pod

As understood - not traced here (#167):

1. A press on a pod travels as an AVRCP play/pause passthrough.
2. bluetoothd turns it into a key press on an input device of its own,
   `<name> (AVRCP)`, through uinput.
3. GNOME's media keys service (`org.gnome.SettingsDaemon.MediaKeys`) hands the key to the
   one player it takes as current, over MPRIS.

A key is a toggle, as `PlayPause` is. Synthesizing a key of one's own needs write access
to `/dev/uinput`, which takes root or a udev rule, and the shell then picks the player
that gets it (#167).

Why the app does not go this way to pause the media:
[app/media-pause.md](../../app/media-pause.md).

## UPower

**GNOME never warns of a low Bluetooth battery.** Seen on a Pro 3 (2026-10-26, #126, run
below), and true of every battery that bluetoothd reports as `Battery1`:

- **UPower gives a warning level only to a device that is discharging**
  (`up_daemon_compute_warning_level` in UPower 1.91.5's `src/up-daemon.c`; #126).
- **Its BlueZ backend reads `Percentage` alone** (UPower 1.91.5,
  `src/linux/up-device-bluez.c`; #126). The device's state stays unknown, so its
  `WarningLevel` stays none.
- **`Battery1` carries no state** in BlueZ 5.87. BlueZ master adds an experimental
  `Charging`, which UPower does not read (#126).
- **With `WarningLevel` none, `gsd-power` sends nothing** (#126).
- **UPower's default thresholds** are 20% (low) and 5% (critical): `PercentageLow` and
  `PercentageCritical` in UPower 1.91.5's `etc/UPower.conf` (#126).

## GApplication

From GLib 2.90.1's GApplication documentation (`gio/gapplication.c`) and the sources
named, not a run here:

- **One instance per application ID, per session.** The ID is claimed as a well-known
  name on the user's session bus. A second launch with the same ID finds it taken: it
  sends its activation or command line to the primary instance, and `run` returns.
- **`startup` is emitted on the primary instance only,** right after it registers. A
  second launch never emits it.
- **Two IDs are two applications.** Nothing in GApplication keeps an app under one ID
  from running beside the same program under another.
- **Its actions are on the session bus.** GApplication exports its action group at its
  object path (`g_dbus_connection_export_action_group`, `gio/gapplicationimpl-dbus.c`) as
  `org.gtk.Actions` (`gio/gactiongroupexporter.c`), so any process on the session bus can
  activate one.
- **An activation says nothing of who asked.** `GSimpleAction::activate` carries only
  the action and its parameter (`gio/gsimpleaction.c`).
- **In the primary instance, an activation runs before the call returns.**
  `g_application_activate_action` hands it straight to the local action group; only a
  remote instance sends it over D-Bus (`gio/gapplication.c`, GLib 2.90.1).
- **`shutdown` is emitted while the app is still registered:** `g_application_run`
  emits it, then flushes and destroys its D-Bus side (`gio/gapplication.c`, GLib 2.90.1,
  read 2026-11-08, #196).
- **It also serves `org.freedesktop.Application`** at its object path, whose
  `ActivateAction` activates one of its actions (`gio/gapplicationimpl-dbus.c`, GLib
  2.90.1). The running app answered introspection there with it (GLib 2.90.1,
  2026-11-08, #196).

## Notifications

**Where GLib sends a `GNotification`** depends on the sandbox (GLib 2.90.1, read
2026-11-08, #196):

- **In a Flatpak,** to the notification portal: `AddNotification` on
  `org.freedesktop.portal.Notification` at `org.freedesktop.portal.Desktop`. The portal
  backend outranks GNOME Shell's (priority 110 to 100,
  `gio/gportalnotificationbackend.c`), and is used inside a Flatpak, where
  `/.flatpak-info` exists, or with `GIO_USE_PORTALS=1` (`gio/gportalsupport.c`,
  `gio/gsandbox.c`). The app's Flatpak runtime, org.gnome.Platform 50, builds GLib
  2.88.4 (gnome-build-meta, branch `gnome-50`, `elements/sdk/glib.inc`, read
  2026-11-08); these files were read in 2.90.1, not in 2.88.4.
- **Outside a sandbox,** to GNOME Shell: `AddNotification` on `org.gtk.Notifications`,
  used while that name has an owner (priority 100, `gio/ggtknotificationbackend.c`).

What GNOME Shell does with a notification sent to it on `org.gtk.Notifications`, the
unsandboxed path, from GNOME Shell 51.1's `js/ui/notificationDaemon.js` (read from the
installed build's resources, 2026-11-08, #196) and its `src/shell-app.c` (GNOME Shell
51.1, read 2026-11-08, #196). What the portal does with one is
[open](#open-questions):

- **A button whose action starts with `app.`** is activated through the app:
  `org.freedesktop.Application.ActivateAction` on the bus name that is the app's ID, at
  the object path made from it, with the button's target.
- **That call may auto-start the app** (no `NO_AUTO_START` flag): it starts one that is
  D-Bus activatable, and fails for one that is not and is not running.
- **A click on the notification itself, with no default action,** activates the app as
  its launcher entry would.
- **A new notification with the ID of one on show replaces it.**

## logind

From systemd 262's `org.freedesktop.login1(5)` and `loginctl(1)`, and runs here:

- **`/org/freedesktop/login1/session/auto`** names the caller's session, or, for a
  caller in none, the user's graphical session. `loginctl(1)` says so of the command
  line's `auto`; for the bus path it rests on a run: a process of the user's
  `user@.service`, outside any session, read the graphical session's ID there
  (systemd 262, GNOME 51.1, 2026-11-08, #196).
- **No signal is sent at `auto` or `self`:** only at the session's own path, which the
  manager's `GetSession` returns for its ID.
- **`Active`, `IdleHint` and `LockedHint`** are the session's: in the foreground of its
  seat, idle, and locked. Each is signaled with `PropertiesChanged` and its value when it
  changes: `org.freedesktop.login1(5)` gives them no `EmitsChangedSignal` annotation, so
  they take the default, and `busctl introspect` of the session flagged each
  `emits-change` (systemd 262, 2026-11-08, #196).
- **logind sets neither hint itself for a graphical session:** a graphical session's
  `IdleHint` is the one `SetIdleHint` last set, false until then, and `LockedHint` is set
  by `SetLockedHint` alone (systemd 262, `src/login/logind-session.c`, read 2026-11-08,
  #196). The man page leaves them to the desktop: `SetIdleHint` "by the session object",
  `SetLockedHint` "by the desktop environment".
- **The manager signals `PrepareForSleep(b start)`** at `/org/freedesktop/login1`, on
  `org.freedesktop.login1.Manager`: "right before (with the argument "true") or after
  (with the argument "false")" the system goes to suspend or hibernate
  (`org.freedesktop.login1(5)`, systemd 262).

What GNOME sets:

- **GNOME Shell sets `LockedHint`** whenever its screen shield locks or unlocks
  (`_setLocked` in `js/ui/screenShield.js`, GNOME Shell 51.1). It finds its session by
  `XDG_SESSION_ID`, or else by `session/auto` (`js/misc/loginManager.js`).
- **GNOME Shell locks the screen at `PrepareForSleep(true)`** when
  `org.gnome.desktop.screensaver lock-enabled` is on, and wakes the screen at `false`
  (`_prepareForSleep` in `js/ui/screenShield.js`, GNOME Shell 51.1): a resume lands on
  the lock screen.
- **GNOME Shell delays the sleep until it has locked.** While its session is active,
  the screen unlocked and locking enabled, it holds a `delay` inhibitor for `sleep`
  (`_syncInhibitor` in `js/ui/screenShield.js`, `inhibit` in `js/misc/loginManager.js`,
  GNOME Shell 51.1, read from the installed build's resources, 2026-11-08), and lets
  it go once the screen shield is up. logind holds a sleep back for a `delay` inhibitor
  until it is released, at most `InhibitDelayMaxSec`, 5 s by default
  (`org.freedesktop.login1(5)`, `logind.conf(5)`, systemd 262). So processes go on
  running for a moment after `PrepareForSleep(true)`.
- **gnome-session sets `IdleHint`** from its presence: idle while the presence status is
  idle, else not. Its idle timeout is `org.gnome.desktop.session idle-delay`, and an idle
  inhibitor turns idle detection off (gnome-session 51.0, `gnome-session/gsm-manager.c`,
  read 2026-11-08, #196).

### GNOME's own

Seen on GNOME 51.1 (2026-11-08, #196), from introspection and the bus's service files:

- **`org.gnome.ScreenSaver`** (`GetActive`, `ActiveChanged`) says whether the screen
  shield is up. It is owned by a `gjs` process of its own,
  `/usr/share/gnome-shell/org.gnome.ScreenSaver`, and the session bus has a service file
  for the name, so a call to it while it is not running starts it.
- **`org.gnome.Mutter.IdleMonitor`**, owned by `gnome-shell`, says how long the user has
  been idle (`GetIdletime`), and calls back after a stretch idle or at the next activity
  (`AddIdleWatch`, `AddUserActiveWatch`).

## Flatpak's bus proxy

In a Flatpak, the app's buses are reached through `xdg-dbus-proxy`, which filters by
well-known name (`xdg-dbus-proxy(1)`, 0.1.9, unless another source is named):

- **By default the proxy lets a client reach only the bus itself and its own unique
  name;** every other client is invisible.
- **Flatpak adds to that on the session bus:** the app may own its ID, the names under
  it, and its ID under `org.mpris.MediaPlayer2`, and may talk to those, to the bus and to
  the portals, `org.freedesktop.portal.*` (`flatpak-metadata(5)`, Flatpak 1.18.4,
  "Session Bus Policy").
- **The system bus is not there at all** unless the manifest grants a name on it
  (`flatpak-metadata(5)`, Flatpak 1.18.4, "System Bus Policy").
- **The manifest grants more:** `--talk-name`, `--system-talk-name`. A name ending in
  `.*` grants every name under it.
- **A name it may talk to is listed by `ListNames`,** its owner given by `GetNameOwner`,
  and its `NameOwnerChanged` seen; the app may call it and receives its broadcast signals.
- **A unique name gets the policy of every well-known name it owns,** so a call to the
  unique name that owns a granted name goes through.

## Runs

**The volume: Pro 3, 2026-11-01 (#150).** The transport's `Volume` was set from 32 to 20
and back with `busctl`, which was not the transport's owner. PipeWire's sink followed
within 0.5 s: 25% to 16% to 25%.

**UPower: Pro 3, connected, 2026-10-26 (#126).** A battery provider reported the pair to
bluetoothd at 9%, then 4%. UPower 1.91.5 listed it as headphones at that level, with
state unknown and `WarningLevel` none, and `gsd-power` sent nothing.

## Open questions

- In the Flatpak, logind sees `xdg-dbus-proxy` as the caller of `session/auto`: whether it
  still resolves to the user's graphical session (#196).
- In the Flatpak, what becomes of a notification's button: GLib sends the notification
  to the notification portal there, not to GNOME Shell
  ([Notifications](#notifications)). Whether the button still activates the app's
  action has not been read or run (#196).
- How gnome-session's presence status decides idle (`gnome-session/gsm-presence.c`): not
  read (#196).
- logind's hints on a desktop other than GNOME: whether it sets `IdleHint` and
  `LockedHint` at all (#196).
- gnome-session setting `IdleHint` was not seen: in the session of the #196 run
  `idle-delay` was 0, never idle, and `IdleSinceHint` read 0. Nor was `LockedHint` seen
  to change.
- A press on a pod: confirm the path above - `evtest` shows the key, and the MPRIS
  player changes state (#167).
- GNOME's volume slider, which follows the sink, was not watched in the volume run
  (#150).
