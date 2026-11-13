# UI

How the window, Preferences and the tray get what they show, and the GTK traps they
avoid. The window is `ui`, the tray `indicator`. What each feature shows is in its own
document; this one covers what holds for all of them.

## Updates come from snapshots

**Widgets change only from the snapshot stream** ([state.md](state.md#snapshots)), the
scanning hint from the scanner's ([The scanning hint](#the-scanning-hint)), and the
connect banner's title and button and the header's spinner from the connects under way
([The connect banner](#the-connect-banner)), which redraw at once through
`Connecting::follow`, with no render loop of their own. The
window's render loops, one per stream, run on the main context, the only place a widget
may be touched ([architecture.md](architecture.md#two-threads)). Each:

- **One pass per `RENDER_INTERVAL` at most.** tokio's cooperative budget does not reach
  glib's executor: under a stream of updates `changed()` would stay ready, and the loop
  would never yield to GTK. The pause caps rendering, not delivery: the next pass picks up
  whatever is newest.
- **The borrow is cloned out** before anything is awaited: it is a read lock every
  broadcast waits for.
- **Draws the current value at once:** `Coordinator::subscribe` and `ScanControl::state`
  hand out receivers marked changed, so a window built after the value was set still
  draws it.

A clock redraws the window every 30 seconds, so a BLE reading's age moves on with no
snapshot. Under `RECENT` it reads "Last seen just now".

**The exceptions are a command that fails** ([commands.md](commands.md#what-the-caller-hears))
**and a connect that fails** ([The connect banner](#the-connect-banner)). Each result comes
back to the main context; a failed command, and a failed connect of the window's own
press, goes up as a toast: an event, never a control's state. So
the noise control radios keep showing the recorded mode.

## Controls that send commands

Noise Control, Conversation Awareness and the Info group's Name row send commands, so
they are insensitive unless the pair shown is on AAP. BLE is receive-only.

| Control | Sensitive while the pair is on AAP and |
|---|---|
| Noise Control | always |
| The Conversation Awareness switch | the setting has been reported: a switch cannot show unknown |
| The Name row | the pods' name is known: [names.md](names.md#two-names) |

**A snapshot turns the switch under `ControlView::syncing_features`,** so its
`notify::active` does not send the setting back out. A failed command draws the last
snapshot again, which puts the switch back.

The Model, Color and Firmware rows below Name are no commands: [Model and color](#model-and-color),
[firmware.md](firmware.md#what-the-app-does-with-it).

## The connect banner

**A banner under the header bar, with a Connect button, asks BlueZ to connect the pair
shown** (#195): a deliberate page. What the connect does, and what it may do to a pair
another host holds: [bluez.md](bluez.md#connecting-a-pair).

- **Revealed from the snapshot alone** (`control::offers_connect`): while BlueZ has the
  pair paired - it is among BlueZ's names - and the pair is neither
  [attached](aap-links.md#which-pairs) nor on AAP
  ([state.md](state.md#what-a-snapshot-shows)). It goes once attachment has attached the
  pair. The press does not hide it: a pair that connects takes it away, and one that
  fails leaves it.
- **A connect under way shows in the banner and the header** (`control::connect_shown`):
  while one to the pair shown runs, the banner reads "Connecting…" with its button
  hidden, and a spinner spins at the start of the header bar; once it ends, the title and
  the button are back and the spinner goes. "Under way" is `connect::Connecting`, the
  pairs `app.connect` is connecting, whatever ran it - the window's press or another
  process - never the press itself. The window follows it on the main context and draws
  it with the last snapshot, so switching pairs shows each pair's own state. Revealing
  stays the snapshot's alone: a pair attached, or on AAP, before BlueZ answers the
  connect has the spinner with no banner.
- **Connect takes no press while both pods are in the case** (#204): while the state
  the snapshot reports for the pair has both pods charging (`control::in_the_case`:
  `PodState::left_charging` and `right_charging`), the banner reads "Your AirPods are
  in the case" and its button is insensitive. A connect to pods in the closed case got
  no reply from bluetoothd within `bluez::DEVICE_CONNECT_LIMIT`
  ([bluez.md](bluez.md#connecting-a-pair)).
  - **The charging bits, not status bit 2.** Pods in the case charge
    ([protocol/ble/proximity-pairing.md](../protocol/ble/proximity-pairing.md#byte-5-charging-status));
    bit 2 is also set by a pod in the case with its partner worn
    ([protocol/ble/proximity-pairing.md](../protocol/ble/proximity-pairing.md#byte-3-status-byte)).
    Such a pod's first advertisements read its worn partner's level as unknown, with
    the charging bit set in that decrypted byte; the parser keeps the cleartext's
    charging flag for a pod whose level is unknown
    ([identification.md](identification.md#batteries-in-the-clear-and-encrypted)), so
    they read one pod charging, not both.
  - **The reported state, whatever the reading came from** (#206): a live
    advertisement, or a reading restored at startup ([readings.md](readings.md)). The
    banner does not ask where a reading came from. With the lid closed the pods do not
    advertise, so their last reading stands; at startup, a restored reading is the only
    one there is. Read from live advertisements alone, the banner would offer Connect
    for pods in the closed case at every startup, and a press would fail at
    `bluez::DEVICE_CONNECT_LIMIT`.
  - **Pods taken out while the app was not running** read as in the case until they are
    heard: accepted. Pods out of the case advertise, so this lasts until their first
    advertisement. Pods not heard at all may be out of range; whether a connect to them
    fails is a [hardware check](bluez.md#hardware-checks).
  - **A pull-out is not read this way:** the nearby notification counts a stay in the
    case from live readings alone ([nearby.md](nearby.md#a-pull-out)). Over AAP the
    pair is connected, and there is no banner.
  - **A case too empty to charge the pods leaves Connect pressable:** accepted, as a
    press that fails is a toast.
  - **The open case too:** the test reads the charging bits, which are set in the open
    case as in the closed one, and not the lid. Whether pods in the open case answer a
    connect is a [hardware check](bluez.md#hardware-checks); Connect off there as well
    is accepted.
  - **Late both ways:** a closed case's pods go quiet, and their last reading stands
    until `BLE_CACHE_TTL`; a pod taken out reads as out only from its next
    advertisement.
  - **libadwaita keeps the banner's button internal,** so `control::banner_button`
    finds it among the banner's children to set it insensitive.
- **It can show, and its title be wrong, for a pair connected already,** among others in
  these cases:
  - **A pair whose UUIDs do not list the AirPods service yet** - as we expect just
    after a first pairing, before BlueZ's service discovery; not checked. A press
    fails before any `Connect`, as BlueZ has no paired AirPods at its address, and the
    window shows the toast.
  - **Every pair at launch,** until attachment's first sweep attaches it, as
    `update_names` runs before `sweep`. On one run it did not show on screen: with a
    Pro 3 connected (firmware 9A348, BlueZ 5.87, 2026-11-08, 5edd065, #195), attachment
    found the pair connected 48 ms after the app's first log line and attached it,
    starting AAP, 6 ms later; the user saw no banner.
  - **Every paired pair, connected ones included, for the whole wait after BlueZ's
    signals end:** attachment drops every pair as attached, then waits on its backoff
    (`bluez::Backoff`) before it starts over, while BlueZ's names stay.
  - **A pair connected from outside the app,** until attachment's `attach` has run for
    it, and for as long as `attach` cannot read the device.

  In all but the first, a press reaches bluetoothd, which may answer success, connect
  what is left, or refuse it
  ([protocol/linux/bluez.md](../protocol/linux/bluez.md#device1connect)), or, for a pair
  bluetoothd knows over LE, make an LE connect (open:
  [Open questions](../protocol/linux/bluez.md#open-questions)); a refusal is a toast.
- **It runs `app.connect`,** the pair's MAC as the action's string target. An app action
  rather than the window's, so that anything else that runs it runs the same connect. An
  activation while a connect to the pair is under way does nothing.
- **A failure is a toast, "Couldn’t connect the AirPods", only for the window's press;**
  from anywhere else it is logged alone. Every failure is logged. A GAction says
  nothing of who activated it, so the banner runs the action through `connect::press`,
  which marks the press for the activation it makes; the activation runs before
  `press` returns ([protocol/linux/desktop.md](../protocol/linux/desktop.md#gapplication)).

## The scanning hint

**Preferences says while Cymba listens by discovery** (#189). While a powered adapter
has no advertisement monitor (`ScanState::fallback`), a group at the top of Preferences
says that mice and similar devices may not reconnect while the app scans. Why they do
not: [bluez.md](bluez.md#listening-for-the-pods). Hidden, it takes no space.

| Row | Does |
|---|---|
| The guide row | Names the fix, BlueZ's experimental features, and opens the user document [other-devices.md](../user/other-devices.md) on GitHub's `main`, in the browser |
| The pause row | Its button calls `ScanControl::pause_discovery`: the app's discovery stops for `DISCOVERY_PAUSE`. Its title reads "Scanning", or "Paused until" and the local time the pause ends, from `ScanState::paused_until`. Pressed again while paused, the pause runs that long from then. |

The pause row's subtitle says that the pause lets other devices connect. That AirPods
nearby go unheard meanwhile, and that another client's discovery, such as GNOME Settings'
Bluetooth panel, still blocks, are left to the user document
([bluez.md](bluez.md#listening-for-the-pods)).

**A second stream, not part of the snapshot.** The scanner's state is no pair's state,
and the window alone reads it. Carried in the snapshot, every change would wake the tray,
attachment and the notifiers for nothing, and the coordinator would hold state it does
not merge. So the window follows `ScanControl::state` as it follows the snapshots, with
the same loop, and the harness's `Settle` waits for both.

**The pause shows only once the scanner reports it.** The button changes nothing on
screen: the scanner stops its discovery and sends the new state, which draws the pause,
as a command's result comes back with the snapshot.

**The time the pause ends, not a countdown.** A countdown would redraw every second from a
clock, outside either stream. The end is drawn once; when it comes, the scanner sends a
new state, and the row reads "Scanning" again. A pause whose end has passed before that
state arrives already reads "Scanning".

**Not damped.** During a graceful bluetoothd restart, `fallback` can be true for a moment:
a `Release` can come before the adapter watch hears the adapters power off
([bluez.md](bluez.md#hardware-checks)). The hint would then show for that moment. It is
not held back, as:

- the scanner does fall back to discovery then, though its starts fail, so the hint says
  what is so;
- a delay before showing would hold the hint back wherever it is true too, by a length
  no run has measured;
- Preferences is seldom open while bluetoothd restarts: a guess.

## Drop-downs

**Read a selection before mutating the widget.** Splicing a `StringList` makes
`GtkDropDown` emit `selected-notify`, which clobbers the user's choice.

## Settings

**A setting is not device state.** Its switch is bound to an app action and shows that
action's state:

| Preferences switch | Action | Setting | Document |
|---|---|---|---|
| Battery notifications | `app.battery-notifications` | `Settings::battery_notifications` | [notifications.md](notifications.md#the-switch) |
| Nearby notifications | `app.nearby-notifications` | `Settings::nearby_notifications` | [nearby.md](nearby.md#the-switch) |
| Pause when a pod is taken out | `app.pause-on-ear` | `Settings::pause_on_ear` | [media-pause.md](media-pause.md) |

Each is on by default, also for a file from before the setting.

**Kept in `~/.config/cymba/settings.json`,** a file rather than GSettings: a GSettings
schema has to be compiled and installed wherever the app runs, and GIO aborts the process
when it is missing, as it is for a plain `cargo run`.

- **Loaded at startup** by `settings::open`, before any action is created, so a switch
  saved off starts off and sends nothing.
- **Saved by `settings::Saver`** on tokio's blocking pool, never the main loop: the latest
  settings, whatever order the saves run in.
- **A file that does not parse is set aside,** and the defaults used. A field this build
  does not know is kept and written back.
- **A save that fails** keeps the change in memory.

A planned setting not built yet is shown insensitive, with the subtitle "Not available
yet": it neither claims a state nor takes a click.

## Model and color

The color byte and its names:
[protocol/ble/proximity-pairing.md](../protocol/ble/proximity-pairing.md#byte-7-device-color).

The Color row, below the Model row, names the color. A code missing from the table shows
as `Unknown (0x..)`, as the Model row shows a product ID it does not know. Both rows read
"Unknown" and are insensitive while nothing has identified the pair:

- a pair on AAP before its first advertisement;
- a pair with no link and no reading in the [BLE cache](state.md#the-ble-cache): none
  heard, or the last one has left it.

A link keeps the model and color after that: [state.md](state.md#what-a-snapshot-shows).

## The tray

A StatusNotifierItem (`indicator`, through ksni), run by `tray_task` from the snapshots.
It shows the lower of the two pods ([battery.md](battery.md#where-it-goes)), and its menu
sets the noise control mode, logging a command that fails.

## Resources

**Artwork and the app icon are compiled into a GResource** by `build.rs`, from
`assets/resources.gresource.xml`. A missing resource fails silently: `gtk::Image` renders
nothing, and the About dialog shows a placeholder. So the `bundled_resources_resolve`
test asserts that every name the UI loads is in the bundle.

**The tray points the shell at `assets/icons`** in the checkout: the shell draws the tray
icon, and cannot read resources inside the binary.

## Hardware checks

The features' checks are in their documents, the scanning hint's and the connect
banner's in [bluez.md](bluez.md#hardware-checks), but for these:

- **Connect with both pods in the case** (#204): with the pair paired and not connected
  here, both pods in the open case, the banner reads "Your AirPods are in the case" and
  its Connect is insensitive; a pod taken out makes it pressable once its advertisement
  comes, with "Your AirPods are not connected". Byte 5 in the pods'
  `BLE parsable raw:` lines, at `RUST_LOG=cymba=trace`, tells what each advertisement
  said.
- **Started with both pods in the closed case** (#206): quit the app with the pair heard
  in the case and not connected here, close the lid, and start it again within
  `BLE_CACHE_TTL`. The banner should read "Your AirPods are in the case" from the
  restored reading, its Connect insensitive, and the window show both pods charging; a
  pod taken out should make it pressable once its advertisement comes.
- **Pods resting fully charged in the case for hours:** whether both pods keep reading
  charging. In the 2026-11-10 run AAP reported status `05` for both pods while
  their advertisements still sent `b5`, but only for about 3.5 minutes
  ([protocol/aap/battery.md](../protocol/aap/battery.md#status-05)). If the bits clear,
  Connect becomes pressable with the pods still in the case. The window's charging
  icons, which show the flags the banner reads, and byte 5 of the pods'
  `BLE parsable raw:` lines after a few hours in the open case tell.
