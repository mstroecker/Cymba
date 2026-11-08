# Nearby notifications

When Cymba tells the user that a known pair is nearby again, with a Connect button
(#196). Run only against unit tests and the fake world: see
[Hardware checks](#hardware-checks).

`nearby::Returns` works it out on the tokio side, from the snapshot stream, the time,
whether the user is at the desktop and the resumes from sleep; `ui::notifications` sends it from the main context
as a `GNotification`, through `nearby::Gate` for the switch, as the low battery
notifications are ([notifications.md](notifications.md)). Which notice comes when, in
one place: [When a notice shows](#when-a-notice-shows).

## When a notice shows

**The first row that matches decides;** "any" matches every value. A notice is shown
once per return; what each column means is linked below the table. Followed row by
row, in this order, by `nearby::tests::the_decision_table`.

| Case | Return | A pod out of the case | Connected here | At the desktop | The switch | Host named (bytes 4-6) | Byte 8 | Outcome |
|---|---|---|---|---|---|---|---|---|
| No return | None | any | any | any | any | any | any | No notice |
| Both pods in the case | any | No | any | any | any | any | any | No notice |
| Connected here | any | any | Yes | any | any | any | any | No notice |
| The switch off | any | any | any | any | Off | any | any | No notice |
| The user away | any | Yes | No | No | On | any | any | Held until the user is back |
| Named another host | any | Yes | No | Yes | On | Another | any | Shown at once |
| In use elsewhere | any | Yes | No | Yes | On | any | In use | Shown at once |
| Named this computer | any | Yes | No | Yes | On | This computer | Other | Shown after `nearby::SHOW_AFTER` |
| Host named unknown | any | Yes | No | Yes | On | Unknown | Other | Shown after `nearby::SHOW_AFTER` |

- **Return:** after `nearby::QUIET` unheard, after a resume from sleep, or a pull-out
  ([A return](#a-return), [A pull-out](#a-pull-out)). None: no return under way - the
  reading is none of these, the return was notified or ended by a connection here, or
  its pair has gone unheard for `QUIET` since.
- **A pod out of the case:** from the last live advertisement ([What is
  notified](#what-is-notified)). Both pods back in the case leaves the return under way,
  and a pod out later in it is notified.
- **Connected here:** attached or on AAP, in the snapshot. It ends the return.
- **At the desktop:** [At the desktop](#at-the-desktop).
- **The switch:** as it stands when the notice would be shown
  ([The switch](#the-switch)).
- **Host named and byte 8:** from the last live advertisement, both unconfirmed
  ([When the pods say they will not come](#when-the-pods-say-they-will-not-come);
  bytes 4-6:
  [protocol/ble/proximity-pairing.md](../protocol/ble/proximity-pairing.md#open-questions)).
  Host named: this computer when decrypted bytes 4-6 end one of this computer's
  adapters' addresses, another when they end none, unknown when the adapters are not
  known or there are none, or the reading names no host. Byte 8: in use for `0x05`, `0x06` or `0x07`,
  other for any other value. A reading heard while the pair was connected here, cached
  until it left, counts as unknown and other, whatever it says. One case escapes: the
  clock set back while the pair is connected. A reading heard before then may now read as
  heard after now. The BLE cache's next prune drops it ([state.md](state.md#the-wall-clock)).
  That prune runs on every advertisement that counts, and at least each minute
  ([state.md](state.md#the-ble-cache)). The disconnect must also fall inside that
  stretch, between the clock set back and the prune. Then a reading heard while connected
  can read as heard after the pair left, and skip the wait.
- **Held:** shown once the user is back, if the return still qualifies then: at once if
  it has qualified for `SHOW_AFTER` by then or the pods say they will not come, else
  once it has ([At the desktop](#at-the-desktop)).
- **Shown after `SHOW_AFTER`:** once the return has qualified, unbroken, for
  `SHOW_AFTER` ([What is notified](#what-is-notified)).

**While a notice is on show,** the first row that matches decides. Followed row by row,
in this order, by `nearby::tests::the_withdrawal_table`.

| Case | What happens | The notice |
|---|---|---|
| `SHOWN_FOR` over | `nearby::SHOWN_FOR` has passed since it was shown | Withdrawn |
| Shown after now | It was shown after now, the clock set back since | Withdrawn at the next update, within `SHOWN_FOR` |
| Connected here | The pair connects here, by the button or otherwise | Withdrawn at once |
| The switch off | The switch is turned off | Withdrawn at once |
| The quit | The app quits | Withdrawn |
| A new return that waits | A return after `QUIET`, or a pull-out, not shown at once | Withdrawn at once; the new notice follows as the table above says |
| A new return shown at once | A return after `QUIET`, or a pull-out, shown at once | Replaced by the new notice |
| A resume's return | A resume, and its return | Left on show: replaced if the resume's notice shows within its `SHOWN_FOR` |

How each is carried out: [The notification](#the-notification).

## A return

A return is one of three, each notified as [When a notice shows](#when-a-notice-shows) says:

| Return | When |
|---|---|
| After a quiet stretch | A known pair's first live advertisement after `nearby::QUIET` unheard, or never heard this run |
| After a resume from sleep | The first live reading heard since the sleep began; below |
| A pull-out | A pod out of the case after both were in it: [A pull-out](#a-pull-out) |

**After a quiet stretch: the first advertisement of a known pair after it has gone
unheard for `nearby::QUIET`.** Longer than a lid shut for a moment, short enough that coming back
from lunch counts. Not `BLE_CACHE_TTL`: the cache keeps a reading for a day, far too long
to mark a pair as gone ([state.md](state.md#the-ble-cache)).

**Every resume from sleep makes a return pending for every known pair,** however short
the sleep, though the pair was heard just before the machine slept. Carrying the pods in
from the commute and opening the laptop at work offers to take them over.

- **`presence` takes each resume in** from logind's `PrepareForSleep`
  ([protocol/linux/desktop.md](../protocol/linux/desktop.md#logind)) as
  `presence::Resumes`: a count, so that latest-wins loses none, and when the last sleep
  began - when its `PrepareForSleep(true)` came, or, with none before the resume, when
  the resume came. `Returns` takes it as an argument.
- **With a return pending, a live reading heard since the sleep began, and within
  `QUIET` of now, is the return,** whenever it was taken in. An advertisement heard
  after the thaw but taken in before logind's resume is not lost: the reading already in
  the snapshot when the resume is taken in counts. That holds when logind's
  `PrepareForSleep(true)` came before the sleep; with none, the sleep is dated at the
  resume, and a reading heard before the resume is not the return. A reading from before
  the sleep, still the last in the snapshot, is no return of any kind, nor one that ends
  a `QUIET` stretch and is taken in only with the resume; for a sleep begun after now,
  see **On the wall clock** below.
- **The pending return takes the place of one held from before the sleep,** so that a
  return held while the user was away is not shown at the unlock from a reading taken
  before the sleep.
- **A pending return is used up only** when it is notified, or when a connection to this
  computer begins after the resume: a snapshot with the pair connected, after one
  without it.
- **A pair shown connected after the resume uses nothing up.** The snapshot from before
  the sleep, taken in again at the resume, and the one a link not yet dropped sends
  after it, leave the return pending: the pair may have left while the machine slept.
  Once the pair is detached, its next reading that qualifies is notified. A reading the
  cache kept while a link outlived the resume does not qualify once it is `QUIET` old,
  so a detach hours later is no return by itself; a reading heard after that detach
  still is.
- **A held return that no longer qualifies stays pending,** as a held return after
  `QUIET` stays: not notified while it does not qualify, notified when a pod is out of
  the case later ([At the desktop](#at-the-desktop)).
- **A resume lands on the lock screen** on GNOME
  ([protocol/linux/desktop.md](../protocol/linux/desktop.md#logind)), so the return is
  held until the user has unlocked ([At the desktop](#at-the-desktop)).

**`QUIET` must stay well above the longest a pair in range goes unheard.** A reading comes
only when a pod's payload changes or a new address is heard
([notifications.md](notifications.md#what-is-warned-about), on `notify::FRESH`), and an
idle pair keeps its payload for minutes; its address rotates every few minutes
([protocol/ble/addresses.md](../protocol/ble/addresses.md#rotation)). A `QUIET` near
those would take a pair that never left for a return.

What "heard" is, per pair:

| What | Counts as heard |
|---|---|
| A live advertisement | At its time, the BLE reading's `last_seen` |
| The reading an earlier run saved | At its saved time, but never a return by itself ([readings.md](readings.md)) |
| A snapshot with the pair connected here, or the first without it after one | Until now: an advertisement of a pair on AAP brings a snapshot only when it changes what is on show ([state.md](state.md#snapshots)), so the time it disconnects counts as heard. Across a sleep that is wrong, and the resume's pending return makes up for it |
| None of these yet this run | Unheard: the first live advertisement is a return |

- **Keyed by the real MAC.** A pod's new address resolves to the same pair
  ([identification.md](identification.md)), so rotation is never a return.
- **On the wall clock,** as the BLE cache is ([state.md](state.md#the-wall-clock)). A
  reading heard after now is no return and is not taken as heard. A time heard that is
  after now, the clock set back since, is forgotten, as the cache forgets such a
  reading: the pair counts as unheard, and its next advertisement is a return. A sleep
  begun after now holds no return off: on that clock a reading before it may be one. The
  resume's return stays pending.
- **The saved readings come from the snapshot taken before scanning starts.** The first
  live advertisement replaces the saved reading in the cache, so a later snapshot no
  longer has its time.

## A pull-out

**Taking a pod out of the case is a return,** each time: the user's choice, so that the
offer comes whenever the pods come out, an iPhone nearby or not. A pair whose live
readings said both pods were in the case, then a pod out, while not connected here, has
returned. Put back and taken out again is a new return.

- **Independent of `QUIET` and of resumes.** A pull-out that is also the return after a
  quiet stretch or a resume gives one notice, and uses a pending resume up as any notice
  does.
- **In the case for `nearby::IN_CASE_SETTLE` first.** The pods must have read as in the
  case from the first live reading that said so, for at least `IN_CASE_SETTLE`, by the
  reading with a pod out. A brief run of readings with both pods in the case between two
  with a pod out is then no pull-out: the pods have said both are in the case with one
  worn, which is open
  ([protocol/ble/proximity-pairing.md](../protocol/ble/proximity-pairing.md#open-questions)).
- **On the wall clock, not a count of readings.** A reading comes only when a pod's
  payload changes or a new address is heard
  ([protocol/ble/proximity-pairing.md](../protocol/ble/proximity-pairing.md#update-frequency)),
  so a count says nothing of how long the pods were in the case. A time in the case that
  is after now, the clock set back since, is forgotten, as a time heard is.
- **`IN_CASE_SETTLE` is a guess:** longer than a stray reading is assumed to stand, and
  shorter than the pods are assumed to rest in the case between two uses. Neither is
  measured ([Hardware checks](#hardware-checks)).
- **Only live readings count.** A restored reading says nothing of the case. At startup,
  a pair whose first live reading has a pod out is the quiet stretch's case, measured
  from the restored reading's time, not a pull-out: a pull-out needs a live reading with
  both pods in the case first.
- **Not while connected here.** A snapshot with the pair connected here ends the stay in
  the case: a pod taken out while connected, and the disconnect after it, are no
  pull-out. A stay begun after the disconnect starts no earlier than the first snapshot
  without the connection: a reading with both pods in the case cached while connected,
  still the last at the disconnect, starts it at the disconnect, not at its own time.
- **Not from before a sleep.** While a resume's return is pending, a reading from before
  the sleep is no pull-out, as it is no return of any kind ([A return](#a-return)).
- **The lid alone is no pull-out:** opened or closed, the pods stay in the case.
- **The stay is measured from the first reading the notifier takes in,** or from the
  disconnect before it if that is later. The snapshots are latest-wins
  ([state.md](state.md#snapshots)), so a reading passed over can shorten the stay
  measured, never lengthen it. It can also join two stays into one: a reading with a pod
  out passed over between them leaves one stay, measured from the first.
- **What `IN_CASE_SETTLE` does not catch:** a stray reading with both pods in the case that
  stays the last for `IN_CASE_SETTLE` or more, as the pods' payloads do not change,
  makes the next reading with a pod out a pull-out, and a notice follows if the pair is
  not connected here. In the #184 run the pair was connected here, which ends the stay
  ([Hardware checks](#hardware-checks)).

## What is notified

A return qualifies while a pod is out of the case, the pair is not connected here -
neither [attached](aap-links.md#which-pairs) nor on AAP - and it has been heard within
`QUIET`; what comes of it: [When a notice shows](#when-a-notice-shows).

- **A pod out of the case** - worn, in hand or on the table - is the last live
  advertisement's status: not both pods in the case, status bit 2
  ([protocol/ble/proximity-pairing.md](../protocol/ble/proximity-pairing.md#byte-3-status-byte));
  `PodState::out_of_case`.
- **`SHOW_AFTER` is a wait for a connect of the pods' own.** That pods taken out of the
  case connect here by themselves, so that a notice shown at once would flash and be
  withdrawn, is an assumption, a guess not seen here; so is the value
  ([Hardware checks](#hardware-checks)). Every return waits, not only a pull-out, unless
  the pods say they will not come
  ([below](#when-the-pods-say-they-will-not-come)).
- **Each return waits on its own.** A new return - after `QUIET`, a pull-out, or a
  resume - starts its wait afresh, whatever the return it replaces had waited.
- **The wait starts again** whenever a snapshot finds the return not qualifying: both pods
  back in the case, the pair connected here, or unheard for `QUIET`. A connection here
  ends the return. A time it began that is after now, the clock set back since, is
  forgotten, and the wait starts again from now.
- **The wait counts from the snapshot, not the reading.** A return can begin to qualify
  without a new reading: at a detach, a cached reading can be a resume's return. It
  waits `SHOW_AFTER` all the same, as the pair may connect here again meanwhile, whatever
  that reading says of another host or of use: it was heard while the pair was connected
  here ([below](#when-the-pods-say-they-will-not-come)).
- **`nearby::run` wakes for it,** as for a withdrawal ([The notification](#the-notification)),
  sleeping at most `SHOW_AFTER` at a time. It does not wake for a return held while the
  user is away: the user's return wakes it.

- **The lid opened alone notifies nothing;** a pod taken out later in the same return
  does. Only a live advertisement says where the pods are: a restored reading, and a
  reading over AAP, have no `out_of_case`.
- **Once per return, never per advertisement.** After the notice, nothing more until the
  next return: the pair unheard for `QUIET` again, a resume from sleep, or a pull-out.
- **Connected here, the return ends:** a pair that connects and disconnects again is not
  notified until its next return. A resume's pending return ends only when a connection
  begins after the resume ([A return](#a-return)).
- **No filter for a phone:** the pods are notified whether or not they are connected to
  an iPhone. A choice: a notification each time the user sits down wearing pods that play
  from the phone may be unwanted. What the advertisement says of another host only
  spares the wait ([below](#when-the-pods-say-they-will-not-come)).

### When the pods say they will not come

**A return is shown without waiting `SHOW_AFTER` when the pods' last live advertisement,
heard since the pair was last connected here, says they will not connect here by
themselves.** The wait is there for pods that connect
here on their own; for pods used elsewhere it only delays the offer. The user's choice.
Either reading is enough:

| Reading | Says | Where it comes from |
|---|---|---|
| The host named is another | Decrypted bytes 4-6, the last three bytes of a host's address, end none of this computer's adapters' addresses | Seen on one Pro 3 (#71), unconfirmed: [protocol/ble/proximity-pairing.md](../protocol/ble/proximity-pairing.md#decrypted-format); their byte order, and whether they name the host last used or the host connected, are [open](../protocol/ble/proximity-pairing.md#open-questions); `PodState::last_host` |
| In use elsewhere | Cleartext byte 8 is `0x05` Music, `0x06` Call or `0x07` Ringing, in a reading heard since the pair was last connected here | LibrePods' names, unconfirmed here: [protocol/ble/proximity-pairing.md](../protocol/ble/proximity-pairing.md#byte-8-connection-state); `PodState::connection_state` |

- **Both readings are unconfirmed** ([Hardware checks](#hardware-checks), #194). Wrong,
  they show a notice for pods that then connect here by themselves: it flashes, and is
  withdrawn at the connect, which is what the wait is for.
- **Only a reading heard since the pair was last connected here counts:** the first
  snapshot without the connection after one with it marks when it left. A reading heard
  while it was connected, still the last at the detach, may be this computer's own use:
  the #184 run saw byte 8 `0x05` from pods worn and connected here
  ([protocol/ble/proximity-pairing.md](../protocol/ble/proximity-pairing.md#the-184-run)).
  A time it left that is after now, the clock set back since, keeps the wait.
- **The wait is kept otherwise:** for `0x04` Idle - which the #71 captures saw for an
  advertisement or two while pods just out of the case paged the host they last used
  ([protocol/ble/proximity-pairing.md](../protocol/ble/proximity-pairing.md#byte-8-connection-state))
  - and every other value; for a reading that names no host; and while this computer's
  adapters are not known, or there are none: every host would read as another, while an
  adapter may be away only for a moment, as when it is replugged.
- **Only the last three bytes are compared,** as only those are advertised. In the order
  the address is written, as bytes 7-9 carry the pair's own MAC
  ([protocol/ble/proximity-pairing.md](../protocol/ble/proximity-pairing.md#decryption-validation)):
  for bytes 4-6, a guess, which is
  [open](../protocol/ble/proximity-pairing.md#open-questions).
- **This computer's adapters' addresses** are each adapter's `Address`, powered or not,
  read with the adapters from BlueZ's `GetManagedObjects` and `InterfacesAdded` - no
  call of their own ([bluez.md](bluez.md#adapters)). Attachment tells the coordinator
  after every change to the adapters (`Coordinator::set_adapter_addresses`), and the
  snapshot carries them (`Snapshot::adapter_addresses`). Not known while BlueZ is off
  the bus or attachment is not running, nor while an adapter's address does not read, as
  the one missing could be the host named.
- **A notice that skipped the wait says why in the log,** on its `nearby again` line:
  "advertised as naming another host", or "advertised as in use". One shown once it
  had qualified for `SHOW_AFTER` anyway, as at the user's return, says nothing of it. No
  address is logged.

## At the desktop

**A return while the user is away is held,** and notified once the user is at the
desktop again, if it still qualifies then, with no resume from sleep since
([A return](#a-return)); when: [When a notice shows](#when-a-notice-shows). A held return
that no longer qualifies is not notified; a pod taken out again later in the same return
is.

**"At the desktop" is logind's session** (`presence`): `Active`, and neither `IdleHint`
nor `LockedHint`. What logind and GNOME do with these:
[protocol/linux/desktop.md](../protocol/linux/desktop.md#logind).

- **The session is the one `session/auto` names,** followed at its own path, as logind
  signals nothing at `auto`.
- **Every call goes to logind's unique name,** read first with `GetNameOwner`, without
  auto-start, each within `presence::CALL_LIMIT`. A signal counts only from that name,
  and `NameOwnerChanged` only from the bus.
- **Where logind cannot tell, the user is at the desktop:** logind not on the bus, no
  session, a call not answered. A desktop that sets neither hint leaves the user at the
  desktop while the session is in the foreground.
- **With no session found, resumes are still taken in:** `presence` goes on reading
  logind's signals, and looks for the session again on `bluez::Backoff`.
- **The session's changes and a resume come in one stream,** every signal of logind's,
  so that they are taken in the order logind sent them; a resume counts only from
  logind's unique name and at the manager's path.
- **They reach the notifier as one value,** `presence::Presence`: whether the user is at
  the desktop and the resumes, each as of the same signal. Whatever wakes
  `nearby::run`, it reads the latest of that value and of the snapshots before it
  judges, so a lock taken in beside a resume or a new reading is never judged with the
  user still at the desktop.
- **`presence::run` sets itself up again** on `bluez::Backoff` when the bus or logind
  fails, and when logind's name changes hands; while logind is away it waits for the
  name, with no polling.

Why logind rather than GNOME's own services: it is on the system bus, not GNOME's
alone, and GNOME sets both hints there
([protocol/linux/desktop.md](../protocol/linux/desktop.md#logind)). Each of GNOME's tells one of the two, and only
on GNOME, and its screensaver service is started on demand
([protocol/linux/desktop.md](../protocol/linux/desktop.md#gnomes-own)), which the app
would have to take care never to do.

## The notification

- **One per pair,** with the ID `nearby-<MAC>`, so a new one replaces the old.
- **Titled "<name> nearby",** the pair named by `Snapshot::display_name`
  ([names.md](names.md#everywhere-else)); the body is "Not connected to this computer".
- **Its Connect button runs `app.connect`** with the pair's MAC as its target, as the
  window's banner does ([ui.md](ui.md#the-connect-banner)). A failed connect from it is
  logged alone: only the window's press toasts. Outside a sandbox, GNOME Shell activates
  it through the running app
  ([protocol/linux/desktop.md](../protocol/linux/desktop.md#notifications)); in a Flatpak
  the notification goes through the portal, and that is untested
  ([Hardware checks](#hardware-checks)).
- **A click on the notification itself** activates the app, which brings up the window,
  as for the low battery notifications.
- **When it is withdrawn:** [When a notice shows](#when-a-notice-shows); how, below.
- **`nearby::SHOWN_FOR` is the user's choice,** not a measurement. A connect wanted
  later is the window's banner's ([ui.md](ui.md#the-connect-banner)). A return held
  while away gets its `SHOWN_FOR`
  from when it is shown. `nearby::run` wakes for it, and for a return due to be shown
  ([What is notified](#what-is-notified)), on tokio's clock, which does not count a
  suspend ([protocol/linux/clocks.md](../protocol/linux/clocks.md)), and `Returns`
  judges both on the wall clock. `run` sleeps at most `SHOWN_FOR` at a time for a
  withdrawal, so a notice shown after now, the clock set back since, goes within
  `SHOWN_FOR`.
- **A new return that waits withdraws the notice on show,** as nothing would otherwise
  withdraw it while the new return waits or is held. One shown at once replaces it, under
  the same ID.
- **At the quit, in GApplication's `shutdown`,** while the app is still registered
  ([protocol/linux/desktop.md](../protocol/linux/desktop.md#gapplication)). A crash
  leaves a notice on show.
- **After the app has quit, Connect does nothing:** the app is not D-Bus activatable,
  and GNOME Shell's call to it fails. A click on the notification itself starts the app,
  as its launcher entry would, without connecting
  ([protocol/linux/desktop.md](../protocol/linux/desktop.md#notifications)). Accepted:
  the app normally runs in the background with its tray icon, and its quit withdraws the
  notice, so this happens only after a crash.

## The switch

Preferences' Nearby notifications switch is the app action `app.nearby-notifications`
([ui.md](ui.md#settings)), read by the delivery loop on the main context, not the
notifier:

- **Off** drops what comes meanwhile; what is on show:
  [When a notice shows](#when-a-notice-shows).
- **On** shows nothing that came while it was off: a nearby notice is of its moment, and
  the notifier sends each return once.

## Hardware checks

- Decrypted bytes 4-6 and byte 8 on a pull-out, the pods last used with an iPhone nearby,
  once with it playing and once idle, and once with this computer last used: whether
  bytes 4-6 end the iPhone's address and then this computer's adapter's, in the order the
  address is written, and whether byte 8 reads `0x05` while the iPhone plays. Both
  decide whether the notice skips `nearby::SHOW_AFTER`; the log's `nearby again` line
  says whether it did, and `examples/decrypt_probe.rs` shows the plaintext of a captured
  payload ([development/probes.md](../development/probes.md#decrypt_probe)). #194 asks
  for the same runs.
- Pods last used with this computer, taken out of the case: whether they connect here
  before `SHOW_AFTER`, so that no notice shows.
- Decrypted bytes 4-6 after a disconnect from this computer, the pods out of the case
  and not connected elsewhere: whether they still end this computer's adapter's address
  ([protocol/ble/proximity-pairing.md](../protocol/ble/proximity-pairing.md#open-questions)).
  If they name another host by then, the notice skips `nearby::SHOW_AFTER` for pods last
  used here; the log's `nearby again` line says it did.

- A suspend and resume with the pods worn, not connected here: a notice after unlocking.
- A suspend with the pair connected here, and a resume with the pods on another host and
  out of the case: a notice after unlocking.
- A short suspend within `nearby::SHOWN_FOR` of a notice, then the pods back in the case
  before the unlock: whether the old notice leaves GNOME Shell.
- In the Flatpak, the Connect button: GLib sends the notification through the portal
  there, not to GNOME Shell's `org.gtk.Notifications`
  ([protocol/linux/desktop.md](../protocol/linux/desktop.md#notifications)), so whether
  it runs `app.connect` is untested.
- The notice disappears `nearby::SHOWN_FOR` after it is shown in GNOME Shell, and whether
  a withdrawn notice also leaves the message tray.
- A pair back after `QUIET`, a pod taken out: notified once; the Connect button
  connects it, and the notice goes.
- A pod taken out of the case, the pair not connected here, an iPhone nearby: notified
  each time, also when put back for a minute and taken out again.
- Pods that connect here by themselves when taken out of the case: how long after the
  first `BLE parsable raw:` line with a pod out attachment's `AirPods connected:` line
  comes, against `nearby::SHOW_AFTER`. A notice shown and withdrawn within a few seconds
  says the wait is too short. The same after a resume from sleep with the pods worn: how
  long after the unlock they connect here, if they do, against the resume's notice.
- One pod worn and the other in the open case, not connected here, with the lid opened
  and closed: whether a reading with status bit 2 set comes from the pod in the case
  ([protocol/ble/proximity-pairing.md](../protocol/ble/proximity-pairing.md#open-questions)),
  how long it stays the last reading, and whether a pull-out is notified for it. The
  status byte in the pods' `BLE parsable raw:` lines, at `RUST_LOG=cymba=trace`, beside
  the `nearby again` line, tells.
- A pod taken out within `nearby::IN_CASE_SETTLE` of the lid opening, the app started
  since the pods were last heard and less than `QUIET` after its saved reading: no
  reading from before has them in the case, so no notice. Whether a user meets that.
- On a Pro 3, whether the first reading with both pods in the case comes only at the lid
  opening, the pods silent while it is closed
  ([protocol/ble/proximity-pairing.md](../protocol/ble/proximity-pairing.md#update-frequency)):
  a pull-out within `nearby::IN_CASE_SETTLE` of it then gives no notice, unless a reading
  from before the lid closed is the stay's first. How often a user takes a pod out that
  fast, with the pods last heard out of the case, decides whether `IN_CASE_SETTLE` is too
  long. The pods' `BLE parsable raw:` lines from the lid opening to the first with a pod
  out tell.
- A link dropped while the last cached advertisement has status bit 2 set with a pod
  worn: the stay starts at the disconnect, so a pod out within `nearby::IN_CASE_SETTLE`
  of it is no pull-out, and one later is. Whether such a reading is cached at a
  disconnect: the `BLE parsable raw:` lines before attachment's `AirPods disconnected:`
  line tell.
- A pair connected at login: whether attachment attaches it before the first
  advertisement reaches the notifier, or a notice shows for a moment and is withdrawn.
  On one run attachment attached a connected pair at once
  ([ui.md](ui.md#the-connect-banner)), but nothing waits for it. The snapshot has no
  connection state from BlueZ but attachment's (`Snapshot::attached`), so until the
  attach the pair reads as not connected here, never seen to leave: its advertisement
  can skip `nearby::SHOW_AFTER` - byte 8 `0x05` from its own playing here, say - and the
  notice can show at once and go at the attach, with no wait to cover it
  ([When the pods say they will not come](#when-the-pods-say-they-will-not-come)).
- The screen locked while a pod is taken out: held, and notified on unlocking.
- In the Flatpak, logind's `session/auto` resolving to the user's graphical session,
  the caller being `xdg-dbus-proxy`: unverified
  ([protocol/linux/desktop.md](../protocol/linux/desktop.md#open-questions)). And a
  resume there, whether `session/auto` resolves or not: `PrepareForSleep` reaching the
  app through the proxy, and the resume counted.
- A resume with the pair connected here before the sleep: whether a snapshot that still
  has it attached goes out before the detach, as the link's drop (`drop_link`) is
  broadcast.
- A short suspend with the pods worn: whether the first advertisement comes before
  logind's resume. The log's `resumed from sleep` line, beside the pair's first
  `BLE decryptable:` line, tells.
- A reading just before the freeze: the app runs on for a moment after
  `PrepareForSleep(true)`, as GNOME Shell delays the sleep until it has locked
  ([protocol/linux/desktop.md](../protocol/linux/desktop.md#logind)), and an
  advertisement then is heard after the sleep began. At the unlock it can give a notice for pods that have gone since,
  withdrawn after `nearby::SHOWN_FOR`. Check with a sleep of a few minutes with a pod
  out, the pods taken away before waking: compare the log's `going to sleep` line with
  the pair's last `BLE decryptable:` line before the suspend.
- When `LockedHint` is taken in: before the freeze, or after the thaw beside the resume.
- A connection that begins between the thaw and `presence` taking in
  `PrepareForSleep(false)`: taken in before the resume, it does not use the pending
  return up, and the pair's next reading that qualifies after it is detached is
  notified; taken in in the same pass as the resume, it does use it up. The log's
  `resumed from sleep` line against attachment's `AirPods connected:` line for the pair
  tells.
- When `PrepareForSleep(true)` is taken in: before the freeze, or only at the thaw, which
  would date the sleep at the thaw.
- Whether a link survives a short (s2idle) suspend.
- In the Flatpak with no session found at first: whether `presence` finds it later.
- A connection kept up through a resume, then detached by attachment setting itself up
  again: a reading within `QUIET` counts as the resume's return, so a pair still
  connected may be notified for a moment, and the notice is withdrawn when it is
  attached again. A detach later than `QUIET` after the last reading is not, until the
  next reading after it.
- A pair idle in range for over an hour - worn, playing from an iPhone - gives no second
  notice: a reading at least every `QUIET`.
- Status bit 2 as a pod out of the case on the Pro 2 (USB-C): read only on a Pro 3. A
  status that sets bit 2 with a pod worn, which the Pro 3 sent in the #184 run
  ([protocol/ble/proximity-pairing.md](../protocol/ble/proximity-pairing.md#open-questions)),
  delays the notice to the next advertisement, and may start a pull-out
  ([A pull-out](#a-pull-out)).
