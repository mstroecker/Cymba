//! The nearby notification (#196): a known pair heard again after a quiet
//! stretch or a resume from sleep, or taken out of its case, a pod out of the
//! case and not connected here, offered with a Connect button.
//!
//! [`Returns`] works out what to show and withdraw from a snapshot, the time,
//! whether the user is at the desktop and the resumes from sleep - pure, and
//! tested without GTK; [`run`] hands that to the GTK main context, where
//! `ui::notifications` sends it as a `GNotification`, through a [`Gate`] for
//! the switch. When, and why: `docs/app/nearby.md`, whose decision table
//! `tests::the_decision_table` follows row by row.

use std::collections::{BTreeSet, HashMap};
use std::time::{Duration, SystemTime};

use bluer::Address;
use tokio::sync::watch;

use crate::notify::{Action, Notice};
use crate::podstate::{self, DataSource, PodState, Snapshot};
use crate::presence::{Presence, Resumes};

/// How long a pair goes unheard before its next advertisement is a return:
/// longer than a lid shut for a moment, short enough that coming back from
/// lunch counts (`docs/app/nearby.md#a-return`).
pub const QUIET: Duration = Duration::from_secs(30 * 60);

/// How long both pods must have read as in the case, from the first live
/// reading that said so, before a pod out of the case is a pull-out. A guess:
/// longer than the readings that set status bit 2 with a pod worn stand,
/// which is open (`docs/app/nearby.md#a-pull-out`).
pub const IN_CASE_SETTLE: Duration = Duration::from_secs(10);

/// How long a return must have qualified before it is shown, so that a pair
/// that connects here on its own meanwhile shows nothing - unless the pods
/// say they will not ([`elsewhere`]). A guess, not a measurement
/// (`docs/app/nearby.md#what-is-notified`).
pub const SHOW_AFTER: Duration = Duration::from_secs(5);

/// Cleartext byte 8's Music, Call and Ringing: the pods in use, by
/// LibrePods' names, unconfirmed here
/// (`docs/protocol/ble/proximity-pairing.md#byte-8-connection-state`).
const IN_USE: [u8; 3] = [0x05, 0x06, 0x07];

/// Why the pods will not connect here by themselves, as the advertisement
/// behind a live reading says, so that a return is shown without waiting
/// [`SHOW_AFTER`]; [`Returns::update`] takes it only from a reading heard
/// since the pair was last connected here. Both readings are unconfirmed
/// (`docs/app/nearby.md#when-the-pods-say-they-will-not-come`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Elsewhere {
    /// The host decrypted bytes 4-6 name is none of `adapters`.
    AnotherHost,
    /// Byte 8 is one of [`IN_USE`].
    InUse,
}

/// What `state` says of the pods being used elsewhere, with `adapters` this
/// computer's adapters' addresses: `None`, and the wait kept, unless it is a
/// live reading that names a host none of `adapters` ends in, or says
/// the pods are in use. Only the last three bytes of an address are
/// compared, as only those are advertised. No adapter counts as none known:
/// every host would read as another, while the adapter may be away only for
/// a moment, as when it is replugged.
fn elsewhere(state: &PodState, adapters: Option<&BTreeSet<Address>>) -> Option<Elsewhere> {
    if state.source != DataSource::Ble || state.restored {
        return None;
    }
    let another = state
        .last_host
        .zip(adapters.filter(|adapters| !adapters.is_empty()))
        .is_some_and(|(host, adapters)| adapters.iter().all(|adapter| adapter[3..] != host));
    if another {
        return Some(Elsewhere::AnotherHost);
    }
    state
        .connection_state
        .is_some_and(|byte| IN_USE.contains(&byte))
        .then_some(Elsewhere::InUse)
}

/// How long a nearby notice stays on show: a minute, the user's choice
/// rather than a measurement. A connect wanted later is the window's banner's
/// (`docs/app/nearby.md#the-notification`).
pub const SHOWN_FOR: Duration = Duration::from_secs(60);

/// The ID a pair's nearby notification goes by: one per pair, so that it
/// replaces and withdraws its own.
pub fn notification_id(mac: Address) -> String {
    format!("nearby-{mac}")
}

/// Where a pair stands in a return.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Stage {
    /// No return under way: none yet, connected here, or a notice's
    /// [`SHOWN_FOR`] over.
    #[default]
    None,
    /// Heard after [`QUIET`] unheard, for the first time this run, or pulled
    /// out of the case, not yet notified: waiting for a pod out of the case,
    /// for [`SHOW_AFTER`], and for the user at the desktop.
    Waiting,
    /// Notified at `since`, on show until [`SHOWN_FOR`] has passed; nothing
    /// more until the next return. `skipped` says why, when it was shown
    /// before it had qualified for [`SHOW_AFTER`].
    Shown {
        since: SystemTime,
        skipped: Option<Elsewhere>,
    },
}

#[derive(Debug, Default)]
struct Pair {
    /// When the pair was last heard, on the wall clock: its last live
    /// advertisement, the reading a run before saved, the last snapshot
    /// that had it connected here, or the first without it after one.
    /// `None` while none of these is known.
    heard: Option<SystemTime>,
    /// Whether the last snapshot had it connected here.
    connected: bool,
    /// When it was last seen to leave this computer: the first snapshot
    /// without the connection after one with it. `None` while it has not
    /// been connected here this run.
    apart: Option<SystemTime>,
    /// A resume's return, not yet used up: when the sleep before it began.
    /// A live reading heard since then is the return.
    pending: Option<SystemTime>,
    /// When the live readings began to say both pods are in the case, since
    /// the last that said otherwise and the last snapshot connected here:
    /// the first such reading's time, or the first snapshot without the
    /// connection if that is later. `None` while they do not.
    in_case: Option<SystemTime>,
    /// When the return under way began to qualify for a notice, unbroken
    /// since: the time of the first update that found it qualifying. `None`
    /// while it does not, and again when a new return begins.
    qualified: Option<SystemTime>,
    stage: Stage,
}

/// Each known pair's return, so that a pair is notified once per return,
/// never per advertisement, and only while it qualifies
/// (`docs/app/nearby.md`). Keyed by the real MAC, which a pod's rotating
/// address resolves to, so a new address is never a return.
///
/// Pure: the time, whether the user is at the desktop and the resumes from
/// sleep come in as arguments.
#[derive(Debug, Default)]
pub struct Returns {
    pairs: HashMap<Address, Pair>,
    /// The count of resumes taken in.
    resumes: u64,
    /// Whether the user was away at the last update.
    away: bool,
}

impl Returns {
    /// Takes in a snapshot, as of `now`, with the user at the desktop or
    /// away and the resumes from sleep so far, and returns what to show and
    /// what to withdraw. Called again with the same snapshot when the user
    /// comes back, a resume comes, or a notice is due to be shown or to go,
    /// which shows a return held meanwhile if it still qualifies.
    pub fn update(
        &mut self,
        snapshot: &Snapshot,
        now: SystemTime,
        at_desktop: bool,
        resumes: Resumes,
    ) -> Vec<Action> {
        self.away = !at_desktop;
        // A pair last seen connected is visited even when the snapshot no
        // longer lists it, so that its disconnect counts as heard.
        // So is a pair whose notice is on show, so that it is withdrawn in
        // time, and one whose return qualified, so that the wait goes with
        // its reading and wakes `run` no more.
        let was_connected = self
            .pairs
            .iter()
            .filter(|(_, pair)| {
                pair.connected
                    || pair.qualified.is_some()
                    || matches!(pair.stage, Stage::Shown { .. })
            })
            .map(|(mac, _)| mac);
        let macs: BTreeSet<Address> = snapshot
            .states
            .keys()
            .chain(&snapshot.attached)
            .chain(&snapshot.connected_macs)
            .chain(was_connected)
            .copied()
            .collect();
        // A new resume makes a return pending for every known pair, in
        // place of one waiting from before the sleep, and its wait: after
        // it, only a reading heard since the sleep began is the return.
        let new_resume = resumes.count > self.resumes;
        self.resumes = resumes.count;
        if new_resume && let Some(slept) = resumes.slept {
            for mac in &macs {
                self.pairs.entry(*mac).or_default();
            }
            for pair in self.pairs.values_mut() {
                pair.pending = Some(slept);
                pair.qualified = None;
                if pair.stage == Stage::Waiting {
                    pair.stage = Stage::None;
                }
            }
        }
        let mut actions = Vec::new();
        for mac in macs {
            let pair = self.pairs.entry(mac).or_default();
            // Kept only by a judgment below that finds the return still
            // qualifying, and only for the return that began it.
            let mut qualified = pair.qualified.take();
            // Heard after now, the clock set back since: no telling when, as
            // for the BLE cache (`docs/app/state.md#the-wall-clock`). The
            // same for when the pods went into the case.
            if pair
                .heard
                .is_some_and(|heard| podstate::age(heard, now).is_none())
            {
                pair.heard = None;
            }
            if pair
                .in_case
                .is_some_and(|since| podstate::age(since, now).is_none())
            {
                pair.in_case = None;
            }
            // Its minute over - or shown after now, the clock set back since.
            if let Stage::Shown { since, .. } = pair.stage
                && podstate::age(since, now).is_none_or(|age| age >= SHOWN_FOR)
            {
                pair.stage = Stage::None;
                actions.push(Action::Withdraw(notification_id(mac)));
            }
            if snapshot.is_attached(mac) || snapshot.is_connected(mac) {
                pair.heard = later(pair.heard, now);
                // A pull-out counts only while not connected here.
                pair.in_case = None;
                // A connection that begins uses a pending return up; one that
                // was there before the resume - a snapshot from before the
                // sleep, or a link not yet dropped - leaves it.
                if !std::mem::replace(&mut pair.connected, true) {
                    pair.pending = None;
                }
                if matches!(std::mem::take(&mut pair.stage), Stage::Shown { .. }) {
                    actions.push(Action::Withdraw(notification_id(mac)));
                }
                continue;
            }
            if std::mem::take(&mut pair.connected) {
                pair.heard = later(pair.heard, now);
                pair.apart = Some(now);
            }
            let Some(state) = snapshot.states.get(&mac) else {
                continue;
            };
            let Some(seen) = state.last_seen.filter(|_| state.source == DataSource::Ble) else {
                continue;
            };
            if state.restored {
                // Never a return by itself: only the time it gives.
                pair.heard = pair.heard.or(Some(seen));
                continue;
            }
            // Heard after now, the clock set back: no telling how long the
            // pair was quiet, nor whether it is still heard.
            if podstate::age(seen, now).is_none() {
                continue;
            }
            // Both pods in the case since `in_case`, for at least
            // `IN_CASE_SETTLE` by this reading, and a pod out now. A stay
            // starts no earlier than the pair was last heard before its
            // first reading: one cached while connected here starts at the
            // first snapshot without the connection, which counts as heard.
            let pulled_out = match state.out_of_case {
                Some(false) => {
                    let start = pair.heard.map_or(seen, |heard| heard.max(seen));
                    pair.in_case.get_or_insert(start);
                    false
                }
                Some(true) => pair.in_case.take().is_some_and(|since| {
                    podstate::age(since, seen).is_some_and(|stood| stood >= IN_CASE_SETTLE)
                }),
                None => false,
            };
            // With a resume's return pending, a reading from before the
            // sleep is no return of any kind. A sleep begun after now, the
            // clock set back since, is no telling, as for `heard`.
            let before_sleep = pair
                .pending
                .filter(|&slept| podstate::age(slept, now).is_some())
                .is_some_and(|slept| seen < slept);
            let after_quiet = pair
                .heard
                .is_none_or(|heard| podstate::age(heard, seen).is_some_and(|gap| gap >= QUIET));
            let mut replaced = Stage::None;
            if !before_sleep && (after_quiet || pulled_out) {
                replaced = std::mem::replace(&mut pair.stage, Stage::Waiting);
                qualified = None;
            }
            if pair.heard.is_none_or(|heard| seen > heard) {
                pair.heard = Some(seen);
            }
            let heard_lately = pair
                .heard
                .and_then(|heard| podstate::age(heard, now))
                .is_some_and(|age| age < QUIET);
            // Heard since the sleep began, and within `QUIET` of now: the
            // resume's return, whenever the reading was taken in. An older
            // one is a reading cached while a link that outlived the resume
            // was up.
            let resumed = pair.pending.is_some_and(|slept| seen >= slept)
                && podstate::age(seen, now).is_some_and(|age| age < QUIET);
            let mut show = false;
            let mut skipped = None;
            if (pair.stage == Stage::Waiting || resumed)
                && heard_lately
                && state.out_of_case == Some(true)
            {
                // From the first update that found it qualifying, whatever
                // began it: a reading, or a snapshot such as a detach. A time
                // after now, the clock set back since, is forgotten: the wait
                // starts again.
                let since = qualified
                    .filter(|&since| podstate::age(since, now).is_some())
                    .unwrap_or(now);
                // No wait where the pods say they will not connect here, in a
                // reading heard since the pair was last connected here: one
                // cached while it was may be this computer's own use, as byte
                // 8's `0x05` was in the #184 run
                // (`docs/protocol/ble/proximity-pairing.md#the-184-run`). A
                // time it left after now, the clock set back since, keeps the
                // wait: no reading taken in is heard after now.
                let apart = pair.apart.is_none_or(|apart| seen > apart);
                let said = elsewhere(state, snapshot.adapter_addresses.as_ref()).filter(|_| apart);
                let wait = match said {
                    Some(_) => Duration::ZERO,
                    None => SHOW_AFTER,
                };
                let stood = podstate::age(since, now);
                show = at_desktop && stood.is_some_and(|stood| stood >= wait);
                skipped = said.filter(|_| stood.is_some_and(|stood| stood < SHOW_AFTER));
                if !show {
                    pair.qualified = Some(since);
                }
            }
            if show {
                pair.stage = Stage::Shown {
                    since: now,
                    skipped,
                };
                pair.pending = None;
                actions.push(Action::Show(notice(mac, snapshot)));
            } else if matches!(replaced, Stage::Shown { .. }) {
                // A notice on show that the new return replaced, and that no
                // notice replaces now: nothing would withdraw it later.
                actions.push(Action::Withdraw(notification_id(mac)));
            }
        }
        actions
    }

    /// When the first notice on show is due to be withdrawn: [`update`]
    /// withdraws it once called at or after that.
    ///
    /// [`update`]: Returns::update
    pub fn next_withdrawal(&self) -> Option<SystemTime> {
        self.pairs
            .values()
            .filter_map(|pair| match pair.stage {
                Stage::Shown { since, .. } => Some(since + SHOWN_FOR),
                _ => None,
            })
            .min()
    }

    /// Why `mac`'s notice on show was shown before it had qualified for
    /// [`SHOW_AFTER`], if it was: for the log.
    fn skipped(&self, mac: Address) -> Option<Elsewhere> {
        match self.pairs.get(&mac)?.stage {
            Stage::Shown { skipped, .. } => skipped,
            _ => None,
        }
    }

    /// When the first return that qualifies will have qualified for
    /// [`SHOW_AFTER`]: [`update`] shows it once called at or after that, if
    /// it still qualifies. `None` while the user was away at the last
    /// update, as a return held then is shown when they are back.
    ///
    /// [`update`]: Returns::update
    pub fn next_showing(&self) -> Option<SystemTime> {
        if self.away {
            return None;
        }
        self.pairs
            .values()
            .filter_map(|pair| pair.qualified)
            .map(|since| since + SHOW_AFTER)
            .min()
    }
}

/// The later of `heard` and `now`.
fn later(heard: Option<SystemTime>, now: SystemTime) -> Option<SystemTime> {
    Some(heard.map_or(now, |heard| heard.max(now)))
}

fn notice(mac: Address, snapshot: &Snapshot) -> Notice {
    let name = snapshot
        .display_name(mac)
        .unwrap_or_else(|| "AirPods".to_string());
    Notice {
        mac,
        id: notification_id(mac),
        title: format!("{name} nearby"),
        body: "Not connected to this computer".to_string(),
        critical: false,
        connect: true,
    }
}

/// What [`run`] asks for, and what of it is carried out while Preferences'
/// switch is on or off: the delivery loop's, `ui::notifications::deliver_nearby`,
/// on the main context (`docs/app/nearby.md#the-switch`).
///
/// Unlike the low battery notices' `ui::notifications::Shelf`, turning the
/// switch on shows nothing: a nearby notice is of the moment it was sent,
/// and [`Returns`] sends each return once.
/// Turning it off withdraws what is on show, and so does the app's quit.
///
/// Pure: what to carry out is returned, for the delivery loop to do.
#[derive(Debug)]
pub struct Gate {
    enabled: bool,
    /// The IDs of the notices on show: shown and not withdrawn since.
    shown: BTreeSet<String>,
}

impl Gate {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            shown: BTreeSet::new(),
        }
    }

    /// Takes what the nearby notifier asks for, and returns it if it is to be
    /// carried out: a notice only while the switch is on, a withdrawal only
    /// of one on show.
    pub fn take(&mut self, action: Action) -> Option<Action> {
        match &action {
            Action::Show(notice) => {
                if !self.enabled {
                    return None;
                }
                self.shown.insert(notice.id.clone());
            }
            Action::Withdraw(id) => {
                if !self.shown.remove(id) {
                    return None;
                }
            }
        }
        Some(action)
    }

    /// Turns the switch on or off, and returns what that takes: off, every
    /// notice on show withdrawn.
    pub fn set_enabled(&mut self, enabled: bool) -> Vec<Action> {
        self.enabled = enabled;
        if enabled {
            return Vec::new();
        }
        self.withdraw_all()
    }

    /// Every notice on show, withdrawn: at the quit.
    pub fn withdraw_all(&mut self) -> Vec<Action> {
        std::mem::take(&mut self.shown)
            .into_iter()
            .map(Action::Withdraw)
            .collect()
    }
}

/// Watches `updates`, starting from `first`, and `presence` for as long as
/// the app runs, and sends what to show and withdraw to `notices`, for the
/// GTK main context to carry out. Returns when the snapshots or `notices`
/// close.
///
/// `first` is the snapshot taken as the app subscribed, before scanning
/// started: it holds the readings an earlier run saved, which the first live
/// advertisement replaces. Whatever wakes it, `run` reads the latest of
/// both before it judges, so that a stale value never meets a fresh one:
/// a lock and a resume come in one `Presence`, in logind's order.
pub async fn run(
    first: Snapshot,
    updates: watch::Receiver<Snapshot>,
    presence: watch::Receiver<Presence>,
    notices: async_channel::Sender<Action>,
) {
    run_on(SystemTime::now, first, updates, presence, notices).await;
}

/// [`run`], reading the wall clock from `clock`.
async fn run_on(
    clock: impl Fn() -> SystemTime,
    first: Snapshot,
    mut updates: watch::Receiver<Snapshot>,
    mut presence: watch::Receiver<Presence>,
    notices: async_channel::Sender<Action>,
) {
    let mut returns = Returns::default();
    let mut snapshot = first;
    let mut told = *presence.borrow_and_update();
    // Once the sender is gone, nothing tells otherwise.
    let mut following = true;
    // `first` is judged first, before any later snapshot, so that the
    // readings an earlier run saved give their times.
    loop {
        let actions = returns.update(&snapshot, clock(), told.at_desktop, told.resumes);
        for action in actions {
            match &action {
                Action::Show(notice) => {
                    let said = match returns.skipped(notice.mac) {
                        Some(Elsewhere::AnotherHost) => "; advertised as naming another host",
                        Some(Elsewhere::InUse) => "; advertised as in use",
                        None => "",
                    };
                    tracing::info!("{} nearby again{said}", notice.mac);
                }
                Action::Withdraw(id) => tracing::info!("withdrawing {id}"),
            }
            if notices.send(action).await.is_err() {
                return;
            }
        }
        // Woken for the next withdrawal or showing on tokio's clock, which
        // does not count a suspend (`docs/protocol/linux/clocks.md`);
        // `update` judges both on the wall clock. At most `SHOWN_FOR` and
        // `SHOW_AFTER` away, so that a time after now, the clock set back
        // since, is judged then.
        let until = |due: SystemTime, most: Duration| {
            due.duration_since(clock()).unwrap_or_default().min(most)
        };
        let withdrawal = returns.next_withdrawal().map(|due| until(due, SHOWN_FOR));
        let showing = returns.next_showing().map(|due| until(due, SHOW_AFTER));
        let wake = withdrawal.into_iter().chain(showing).min();
        tokio::select! {
            changed = presence.changed(), if following => {
                if changed.is_err() {
                    following = false;
                    told.at_desktop = true;
                }
            }
            () = tokio::time::sleep(wake.unwrap_or_default()), if wake.is_some() => {}
            changed = updates.changed() => {
                if changed.is_err() {
                    return;
                }
            }
        }
        // The latest of both, whatever woke `run`.
        if following {
            told = *presence.borrow_and_update();
        }
        // Cloned out, whatever woke `run`: the borrow is a read lock every
        // broadcast waits for, and the sends above are awaited.
        snapshot = updates.borrow_and_update().clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bluez::DeviceNames;
    use crate::podstate::PodState;

    const PAIR: Address = Address::new([0x00, 0x00, 0x5E, 0x00, 0x53, 0x01]);
    const OTHER: Address = Address::new([0x00, 0x00, 0x5E, 0x00, 0x53, 0x02]);
    const MINUTE: Duration = Duration::from_secs(60);
    const SECOND: Duration = Duration::from_secs(1);

    /// No resume from sleep yet.
    const AWAKE: Resumes = Resumes {
        count: 0,
        slept: None,
    };

    /// A fixed time, so the expected values below are worked out by hand.
    fn t0() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000)
    }

    /// A live advertisement heard at `seen`, a pod out of the case or not.
    fn heard(seen: SystemTime, out_of_case: bool) -> PodState {
        let mut state = PodState::empty(DataSource::Ble);
        state.last_seen = Some(seen);
        state.out_of_case = Some(out_of_case);
        state
    }

    /// The reading an earlier run saved, heard at `seen`.
    fn restored(seen: SystemTime) -> PodState {
        let mut state = PodState::empty(DataSource::Ble);
        state.last_seen = Some(seen);
        state.restored = true;
        state
    }

    fn snapshot(states: impl IntoIterator<Item = (Address, PodState)>) -> Snapshot {
        Snapshot {
            states: states.into_iter().collect(),
            ..Snapshot::default()
        }
    }

    /// One pair, `state`.
    fn of(state: PodState) -> Snapshot {
        snapshot([(PAIR, state)])
    }

    /// One pair, `state`, attached here.
    fn attached(state: PodState) -> Snapshot {
        let mut snap = of(state);
        snap.attached.insert(PAIR);
        snap
    }

    fn shown(actions: &[Action]) -> Vec<(&str, &str)> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Show(n) => Some((n.id.as_str(), n.title.as_str())),
                Action::Withdraw(_) => None,
            })
            .collect()
    }

    const NEARBY: (&str, &str) = ("nearby-00:00:5E:00:53:01", "AirPods nearby");

    /// Takes `snap` in at `now`, asserting that nothing is shown then, and
    /// again `SHOW_AFTER` later: what the two send together. A return that
    /// begins to qualify at `now` is shown at the second.
    fn settled(
        returns: &mut Returns,
        snap: &Snapshot,
        now: SystemTime,
        at_desktop: bool,
        resumes: Resumes,
    ) -> Vec<Action> {
        let mut actions = returns.update(snap, now, at_desktop, resumes);
        assert!(shown(&actions).is_empty(), "shown at once: {actions:?}");
        actions.extend(returns.update(snap, now + SHOW_AFTER, at_desktop, resumes));
        actions
    }

    /// A pair whose reading an earlier run saved a minute before `t0()`,
    /// taken in at `t0()`: known, and not returned.
    fn known() -> Returns {
        let mut returns = Returns::default();
        let saved = of(restored(t0() - MINUTE));
        assert!(returns.update(&saved, t0(), true, AWAKE).is_empty());
        returns
    }

    /// Heard at `t0()`, then quiet: at `t0() + quiet` the next one comes.
    /// Returns what that next advertisement sends, by `SHOW_AFTER` later.
    fn after_quiet(quiet: Duration) -> Vec<Action> {
        let mut returns = Returns::default();
        settled(&mut returns, &of(heard(t0(), true)), t0(), true, AWAKE);
        let next = t0() + quiet;
        settled(&mut returns, &of(heard(next, true)), next, true, AWAKE)
    }

    /// The first live advertisement of a run, with no reading saved, is a
    /// return: shown `SHOW_AFTER` later, with a Connect button for the pair,
    /// titled "AirPods" as nothing names it.
    #[test]
    fn a_pair_never_heard_returns_with_its_first_advertisement() {
        let mut returns = Returns::default();
        let actions = settled(&mut returns, &of(heard(t0(), true)), t0(), true, AWAKE);
        assert_eq!(shown(&actions), [NEARBY]);
        let Action::Show(notice) = &actions[0] else {
            unreachable!()
        };
        assert_eq!(notice.mac, PAIR);
        assert!(notice.connect);
        assert_eq!(notice.body, "Not connected to this computer");
    }

    /// Unheard for 30 minutes is a return; for a second less it is not.
    #[test]
    fn a_return_takes_the_whole_quiet_stretch() {
        assert_eq!(shown(&after_quiet(30 * MINUTE)), [NEARBY]);
        assert!(shown(&after_quiet(30 * MINUTE - SECOND)).is_empty());
        assert!(shown(&after_quiet(5 * MINUTE)).is_empty());
    }

    /// Notified once per return: advertisements that follow, a new address
    /// among them - the same real MAC - show nothing; only a new quiet
    /// stretch does.
    #[test]
    fn once_per_return() {
        let mut returns = Returns::default();
        assert_eq!(
            shown(&settled(
                &mut returns,
                &of(heard(t0(), true)),
                t0(),
                true,
                AWAKE
            )),
            [NEARBY]
        );
        for minutes in [1, 7, 20, 49] {
            let at = t0() + minutes * MINUTE;
            let mut rotated = heard(at, true);
            rotated.current_ble_mac = Some(Address::new([0x4A, 0, 0, 0, 0, minutes as u8]));
            let actions = settled(&mut returns, &of(rotated), at, true, AWAKE);
            assert!(shown(&actions).is_empty(), "{minutes} min: {actions:?}");
        }
        // Unheard from minute 49 to 79: a return.
        let back = t0() + 79 * MINUTE;
        assert_eq!(
            shown(&settled(
                &mut returns,
                &of(heard(back, true)),
                back,
                true,
                AWAKE
            )),
            [NEARBY]
        );
    }

    /// The reading a run before saved is never a return by itself; the first
    /// live advertisement is measured from its time.
    #[test]
    fn a_restored_reading_gives_its_time_alone() {
        let start = t0() + 10 * MINUTE;

        // Saved 10 minutes before the app started: heard again at once is
        // no return.
        let mut returns = Returns::default();
        assert!(
            returns
                .update(&of(restored(t0())), start, true, AWAKE)
                .is_empty()
        );
        assert!(settled(&mut returns, &of(heard(start, true)), start, true, AWAKE).is_empty());

        // Saved 40 minutes before: a return.
        let start = t0() + 40 * MINUTE;
        let mut returns = Returns::default();
        assert!(
            returns
                .update(&of(restored(t0())), start, true, AWAKE)
                .is_empty()
        );
        assert_eq!(
            shown(&settled(
                &mut returns,
                &of(heard(start, true)),
                start,
                true,
                AWAKE
            )),
            [NEARBY]
        );
    }

    /// Lid open with both pods in the case notifies nothing; a pod taken out
    /// later in the same return does, once, though it is a pull-out too:
    /// the first advertisement of the run, in the case at `t0()`, is the
    /// return, and the pod out at minute 4 shows one notice.
    #[test]
    fn only_with_a_pod_out_of_the_case() {
        let mut returns = Returns::default();
        assert!(settled(&mut returns, &of(heard(t0(), false)), t0(), true, AWAKE).is_empty());
        let later = t0() + 3 * MINUTE;
        assert!(settled(&mut returns, &of(heard(later, false)), later, true, AWAKE).is_empty());
        let out = t0() + 4 * MINUTE;
        assert_eq!(
            shown(&settled(
                &mut returns,
                &of(heard(out, true)),
                out,
                true,
                AWAKE
            )),
            [NEARBY]
        );
        let still_out = out + MINUTE;
        assert!(
            shown(&settled(
                &mut returns,
                &of(heard(still_out, true)),
                still_out,
                true,
                AWAKE
            ))
            .is_empty()
        );
    }

    /// A pair attached or on AAP is not notified, and one shown is withdrawn
    /// once it connects; the time it was connected counts as heard, so
    /// disconnecting is no return.
    #[test]
    fn not_while_connected_here() {
        for connect in [
            |s: &mut Snapshot| {
                s.attached.insert(PAIR);
            },
            |s: &mut Snapshot| s.connected_macs.push(PAIR),
        ] {
            let mut returns = Returns::default();
            let mut connected = of(heard(t0(), true));
            connect(&mut connected);
            assert!(settled(&mut returns, &connected, t0(), true, AWAKE).is_empty());

            // Connected for an hour with no snapshot, then disconnected:
            // the cached advertisement shows again, heard a minute before
            // and 59 minutes after the last one this saw.
            let gone = t0() + 60 * MINUTE;
            assert!(
                settled(
                    &mut returns,
                    &of(heard(gone - MINUTE, true)),
                    gone,
                    true,
                    AWAKE
                )
                .is_empty()
            );
            let next = gone + MINUTE;
            assert!(settled(&mut returns, &of(heard(next, true)), next, true, AWAKE).is_empty());

            // Shown, then connected: withdrawn, once.
            let mut returns = Returns::default();
            assert_eq!(
                shown(&settled(
                    &mut returns,
                    &of(heard(t0(), true)),
                    t0(),
                    true,
                    AWAKE
                )),
                [NEARBY]
            );
            let mut connected = of(heard(t0(), true));
            connect(&mut connected);
            let at = t0() + MINUTE;
            assert_eq!(
                returns.update(&connected, at, true, AWAKE),
                [Action::Withdraw(notification_id(PAIR))]
            );
            assert!(returns.update(&connected, at, true, AWAKE).is_empty());
        }
    }

    /// A return held while the user is away asks for no wake; shown once
    /// the user is back, it is not shown again.
    #[test]
    fn a_held_return_asks_for_no_wake() {
        let mut returns = Returns::default();
        let snap = of(heard(t0(), true));
        assert!(returns.update(&snap, t0(), false, AWAKE).is_empty());
        assert_eq!(returns.next_showing(), None);
        let back = t0() + 10 * MINUTE;
        assert_eq!(shown(&returns.update(&snap, back, true, AWAKE)), [NEARBY]);
        assert!(returns.update(&snap, back, true, AWAKE).is_empty());
    }

    /// A held return is not shown when, by the time the user is back, the
    /// pair has been unheard for the quiet stretch, has both pods back in
    /// the case, or is connected here.
    #[test]
    fn held_and_no_longer_qualifying() {
        // Unheard since: 30 minutes is too long.
        let mut returns = Returns::default();
        let snap = of(heard(t0(), true));
        returns.update(&snap, t0(), false, AWAKE);
        assert!(
            returns
                .update(&snap, t0() + 30 * MINUTE, true, AWAKE)
                .is_empty()
        );

        // Back in the case.
        let mut returns = Returns::default();
        returns.update(&of(heard(t0(), true)), t0(), false, AWAKE);
        let at = t0() + MINUTE;
        returns.update(&of(heard(at, false)), at, false, AWAKE);
        assert!(
            returns
                .update(&of(heard(at, false)), at + MINUTE, true, AWAKE)
                .is_empty()
        );

        // Connected here, then disconnected again before the user is back.
        let mut returns = Returns::default();
        returns.update(&of(heard(t0(), true)), t0(), false, AWAKE);
        let mut connected = of(heard(t0(), true));
        connected.attached.insert(PAIR);
        returns.update(&connected, t0() + MINUTE, false, AWAKE);
        let at = t0() + 2 * MINUTE;
        returns.update(&of(heard(at, true)), at, false, AWAKE);
        assert!(
            returns
                .update(&of(heard(at, true)), at + MINUTE, true, AWAKE)
                .is_empty()
        );
    }

    /// Put back and pulled out again is a new return, each time: pulled out
    /// at minute 1 and shown, back in at 3 (the notice gone), out at 4,
    /// shown again; back in at 4:30 and out at 4:45, while that notice is
    /// on show, withdraws it at once and shows the new one `SHOW_AFTER`
    /// later.
    #[test]
    fn put_back_and_pulled_out_again_returns_again() {
        let mut returns = known();
        let at = |seconds: u64| t0() + Duration::from_secs(seconds);
        returns.update(&of(heard(at(0), false)), at(0), true, AWAKE);
        assert_eq!(
            shown(&settled(
                &mut returns,
                &of(heard(at(60), true)),
                at(60),
                true,
                AWAKE
            )),
            [NEARBY]
        );
        assert_eq!(
            returns.update(&of(heard(at(180), false)), at(180), true, AWAKE),
            [Action::Withdraw(notification_id(PAIR))]
        );
        assert_eq!(
            shown(&settled(
                &mut returns,
                &of(heard(at(240), true)),
                at(240),
                true,
                AWAKE
            )),
            [NEARBY]
        );
        assert!(
            returns
                .update(&of(heard(at(270), false)), at(270), true, AWAKE)
                .is_empty()
        );
        let actions = settled(
            &mut returns,
            &of(heard(at(285), true)),
            at(285),
            true,
            AWAKE,
        );
        assert_eq!(actions[0], Action::Withdraw(notification_id(PAIR)));
        assert_eq!(shown(&actions), [NEARBY]);
    }

    /// Both pods in the case for less than `IN_CASE_SETTLE` between two
    /// readings with a pod out is no pull-out: a single in-case reading at
    /// minute 1, the next 9 s later with a pod out, shows nothing. In the
    /// case for exactly `IN_CASE_SETTLE`, from minute 2 to 2:10, is one.
    #[test]
    fn a_short_stretch_in_the_case_is_no_pull_out() {
        let mut returns = known();
        let at = |seconds: u64| t0() + Duration::from_secs(seconds);
        for (seconds, out) in [(0, true), (60, false), (69, true)] {
            let snap = of(heard(at(seconds), out));
            let actions = settled(&mut returns, &snap, at(seconds), true, AWAKE);
            assert!(actions.is_empty(), "{seconds} s: {actions:?}");
        }
        returns.update(&of(heard(at(120), false)), at(120), true, AWAKE);
        assert_eq!(
            shown(&settled(
                &mut returns,
                &of(heard(at(130), true)),
                at(130),
                true,
                AWAKE
            )),
            [NEARBY]
        );
    }

    /// A return is shown only once it has qualified for `SHOW_AFTER`, and
    /// `next_showing` says when. Pulled out at minute 1: nothing a second
    /// before `SHOW_AFTER`, shown at it. The same pull-out with the pair
    /// attached 2 s after it, or both pods back in the case then, is never
    /// shown.
    #[test]
    fn a_return_waits_before_it_is_shown() {
        let pulled_out = || {
            let mut returns = known();
            returns.update(&of(heard(t0(), false)), t0(), true, AWAKE);
            let out = t0() + MINUTE;
            let snap = of(heard(out, true));
            assert!(returns.update(&snap, out, true, AWAKE).is_empty());
            assert_eq!(returns.next_showing(), Some(out + SHOW_AFTER));
            (returns, snap, out)
        };

        let (mut returns, snap, out) = pulled_out();
        let early = out + SHOW_AFTER - SECOND;
        assert!(returns.update(&snap, early, true, AWAKE).is_empty());
        assert_eq!(
            shown(&returns.update(&snap, out + SHOW_AFTER, true, AWAKE)),
            [NEARBY]
        );
        assert_eq!(returns.next_showing(), None);

        let (mut returns, _, out) = pulled_out();
        let connected = attached(heard(out, true));
        let soon = out + 2 * SECOND;
        assert!(returns.update(&connected, soon, true, AWAKE).is_empty());
        assert_eq!(returns.next_showing(), None);
        assert!(
            returns
                .update(&connected, out + SHOW_AFTER, true, AWAKE)
                .is_empty()
        );

        let (mut returns, _, out) = pulled_out();
        let soon = out + 2 * SECOND;
        let back_in = of(heard(soon, false));
        assert!(returns.update(&back_in, soon, true, AWAKE).is_empty());
        assert!(
            returns
                .update(&back_in, out + SHOW_AFTER, true, AWAKE)
                .is_empty()
        );
    }

    /// A pod taken out while the pair is connected here is no return, nor
    /// is the disconnect after it: known, both pods in the case at `t0()`,
    /// attached at 30 s, a pod out at minute 1, detached at minute 2 with
    /// that reading the last.
    #[test]
    fn a_pull_out_while_connected_here_is_no_return() {
        let mut returns = known();
        let in_case = of(heard(t0(), false));
        assert!(settled(&mut returns, &in_case, t0(), true, AWAKE).is_empty());
        let connected = t0() + 30 * SECOND;
        let snap = attached(heard(t0(), false));
        assert!(settled(&mut returns, &snap, connected, true, AWAKE).is_empty());
        let out = t0() + MINUTE;
        let snap = attached(heard(out, true));
        assert!(settled(&mut returns, &snap, out, true, AWAKE).is_empty());
        let gone = t0() + 2 * MINUTE;
        assert!(settled(&mut returns, &of(heard(out, true)), gone, true, AWAKE).is_empty());
    }

    /// A stay in the case begun from a reading cached while connected here
    /// starts at the first snapshot without the connection. Known, attached
    /// at `t0()` with a pod out; a reading with both pods in the case cached
    /// at minute 1; detached at minute 2 with it the last: a pod out 2 s
    /// later is no pull-out, as the stay is 2 s, not 62. Detached the same
    /// way, a pod out `IN_CASE_SETTLE` after the detach is one.
    #[test]
    fn a_stay_cached_while_connected_starts_at_the_disconnect() {
        let detached = |out_after: Duration| {
            let mut returns = known();
            returns.update(&attached(heard(t0(), true)), t0(), true, AWAKE);
            let cached = heard(t0() + MINUTE, false);
            returns.update(&attached(cached.clone()), t0() + MINUTE, true, AWAKE);
            let gone = t0() + 2 * MINUTE;
            assert!(returns.update(&of(cached), gone, true, AWAKE).is_empty());
            let out = gone + out_after;
            settled(&mut returns, &of(heard(out, true)), out, true, AWAKE)
        };
        assert!(detached(2 * SECOND).is_empty());
        assert_eq!(shown(&detached(IN_CASE_SETTLE)), [NEARBY]);
    }

    /// A new return waits `SHOW_AFTER` of its own, whatever the one it
    /// replaces had waited. Away at `t0()`, a pod out, held; a reading
    /// `QUIET` later, the user at the desktop: nothing at once, the notice
    /// `SHOW_AFTER` later. The same with a resume in place of `QUIET`:
    /// asleep from minute 1, a reading at minute 5.
    #[test]
    fn a_new_return_waits_afresh() {
        let mut returns = Returns::default();
        returns.update(&of(heard(t0(), true)), t0(), false, AWAKE);
        let back = t0() + QUIET;
        let actions = settled(&mut returns, &of(heard(back, true)), back, true, AWAKE);
        assert_eq!(shown(&actions), [NEARBY]);

        let mut returns = Returns::default();
        returns.update(&of(heard(t0(), true)), t0(), false, AWAKE);
        let sleep = resumed(1, t0() + MINUTE);
        let after = t0() + 5 * MINUTE;
        let actions = settled(&mut returns, &of(heard(after, true)), after, true, sleep);
        assert_eq!(shown(&actions), [NEARBY]);
    }

    /// A return waiting out `SHOW_AFTER` whose pair leaves the snapshot asks
    /// for no wake: pulled out at minute 1 on the lock screen, held; the
    /// reading gone from the snapshot by the unlock at minute 3.
    #[test]
    fn a_waiting_return_gone_from_the_snapshot_asks_for_no_wake() {
        let mut returns = known();
        returns.update(&of(heard(t0(), false)), t0(), false, AWAKE);
        let out = t0() + MINUTE;
        assert!(
            returns
                .update(&of(heard(out, true)), out, false, AWAKE)
                .is_empty()
        );
        let unlocked = t0() + 3 * MINUTE;
        assert!(
            returns
                .update(&Snapshot::default(), unlocked, true, AWAKE)
                .is_empty()
        );
        assert_eq!(returns.next_showing(), None);
    }

    /// A time the return began to qualify that is after now - the clock set
    /// back since - is forgotten, and the wait starts again. A pod out at
    /// `t0()`, taken in at 3 s; the clock set back to 1 s; shown at 6 s,
    /// `SHOW_AFTER` after that, where measured from 3 s it would not be.
    #[test]
    fn a_wait_begun_after_now_starts_again() {
        let mut returns = known();
        let at = |seconds: u64| t0() + Duration::from_secs(seconds);
        returns.update(
            &of(heard(at(0) - MINUTE, false)),
            at(0) - MINUTE,
            true,
            AWAKE,
        );
        let snap = of(heard(at(0), true));
        assert!(returns.update(&snap, at(3), true, AWAKE).is_empty());
        assert!(returns.update(&snap, at(1), true, AWAKE).is_empty());
        assert_eq!(shown(&returns.update(&snap, at(6), true, AWAKE)), [NEARBY]);
    }

    /// A pull-out that is also the return after `QUIET` shows one notice:
    /// saved at `t0()`, the app started 40 minutes later with the pods in
    /// the case, a pod out a minute after that; nothing more for the reading
    /// that follows.
    #[test]
    fn a_pull_out_after_quiet_shows_one_notice() {
        let start = t0() + 40 * MINUTE;
        let mut returns = Returns::default();
        returns.update(&of(restored(t0())), start, true, AWAKE);
        assert!(settled(&mut returns, &of(heard(start, false)), start, true, AWAKE).is_empty());
        let out = start + MINUTE;
        let actions = settled(&mut returns, &of(heard(out, true)), out, true, AWAKE);
        assert_eq!(shown(&actions), [NEARBY]);
        let next = out + MINUTE;
        let actions = settled(&mut returns, &of(heard(next, true)), next, true, AWAKE);
        assert!(shown(&actions).is_empty());
    }

    /// A pull-out that is also a resume's return shows one notice, and uses
    /// that return up: known, both pods in the case at `t0()`, asleep from
    /// minute 1, a pod out at minute 5 after the resume; the reading with a
    /// pod out at minute 7 withdraws the notice, its `SHOWN_FOR` over, and
    /// shows nothing.
    #[test]
    fn a_pull_out_after_a_resume_shows_one_notice() {
        let mut returns = known();
        returns.update(&of(heard(t0(), false)), t0(), true, AWAKE);
        let sleep = resumed(1, t0() + MINUTE);
        let out = t0() + 5 * MINUTE;
        assert_eq!(
            shown(&settled(
                &mut returns,
                &of(heard(out, true)),
                out,
                true,
                sleep
            )),
            [NEARBY]
        );
        let later = out + 2 * MINUTE;
        let actions = settled(&mut returns, &of(heard(later, true)), later, true, sleep);
        assert_eq!(actions, [Action::Withdraw(notification_id(PAIR))]);
    }

    /// A reading heard after now - the clock set back since - is no return,
    /// and is not taken as heard.
    #[test]
    fn a_reading_heard_after_now_is_no_return() {
        let mut returns = Returns::default();
        let ahead = heard(t0() + 2 * SECOND, true);
        assert!(returns.update(&of(ahead), t0(), true, AWAKE).is_empty());

        // Heard at t0(), then a reading from 2 s after now: nothing, and the
        // pair is still last heard at t0().
        let mut returns = Returns::default();
        settled(&mut returns, &of(heard(t0(), true)), t0(), true, AWAKE);
        let now = t0() + 40 * MINUTE;
        let ahead = heard(now + 2 * SECOND, true);
        assert!(shown(&returns.update(&of(ahead), now, true, AWAKE)).is_empty());
        assert_eq!(
            shown(&settled(
                &mut returns,
                &of(heard(now, true)),
                now,
                true,
                AWAKE
            )),
            [NEARBY]
        );
    }

    /// `count` resumes, the last after a sleep that began at `slept`.
    fn resumed(count: u64, slept: SystemTime) -> Resumes {
        Resumes {
            count,
            slept: Some(slept),
        }
    }

    /// Every resume from sleep is a return, however short: heard half a
    /// minute before a sleep that began at minute 2.5, the pair is notified
    /// `SHOW_AFTER` after its first advertisement after the resume at
    /// minute 3, once; a second sleep from minute 4.5 to 5, a second return.
    #[test]
    fn a_resume_is_a_return() {
        let mut returns = Returns::default();
        let at = |seconds: u64| t0() + Duration::from_secs(seconds);
        assert_eq!(
            shown(&settled(
                &mut returns,
                &of(heard(at(0), true)),
                at(0),
                true,
                AWAKE
            )),
            [NEARBY]
        );
        // The notice withdrawn by minute 2.
        assert!(
            shown(&settled(
                &mut returns,
                &of(heard(at(120), true)),
                at(120),
                true,
                AWAKE
            ))
            .is_empty()
        );
        let first = resumed(1, at(150));
        assert_eq!(
            shown(&settled(
                &mut returns,
                &of(heard(at(180), true)),
                at(180),
                true,
                first
            )),
            [NEARBY]
        );
        assert!(
            shown(&settled(
                &mut returns,
                &of(heard(at(240), true)),
                at(240),
                true,
                first
            ))
            .is_empty()
        );
        let second = resumed(2, at(270));
        assert_eq!(
            shown(&settled(
                &mut returns,
                &of(heard(at(300), true)),
                at(300),
                true,
                second
            )),
            [NEARBY]
        );
    }

    /// The reading the snapshot holds from before the sleep is no return:
    /// heard at `t0()` and notified, asleep from minute 2, resumed at
    /// minute 3 with that reading still the last; the one heard at minute 4
    /// is.
    #[test]
    fn a_reading_from_before_the_sleep_is_no_return() {
        let mut returns = Returns::default();
        let before = of(heard(t0(), true));
        assert_eq!(
            shown(&settled(&mut returns, &before, t0(), true, AWAKE)),
            [NEARBY]
        );
        let sleep = resumed(1, t0() + 2 * MINUTE);
        let resume = t0() + 3 * MINUTE;
        assert!(shown(&settled(&mut returns, &before, resume, true, sleep)).is_empty());
        let after = t0() + 4 * MINUTE;
        assert_eq!(
            shown(&settled(
                &mut returns,
                &of(heard(after, true)),
                after,
                true,
                sleep
            )),
            [NEARBY]
        );
    }

    /// A connection to this computer that begins after the resume uses its
    /// return up: asleep from minute 1, resumed at minute 3 with the pair
    /// newly attached, detached at minute 4, its advertisement at minute 5
    /// is no return.
    #[test]
    fn a_connection_after_the_resume_uses_the_return_up() {
        let mut returns = Returns::default();
        returns.update(&of(heard(t0(), false)), t0(), true, AWAKE);
        let sleep = resumed(1, t0() + MINUTE);
        let resume = t0() + 3 * MINUTE;
        assert!(
            returns
                .update(&attached(heard(t0(), false)), resume, true, sleep)
                .is_empty()
        );
        let gone = t0() + 4 * MINUTE;
        assert!(
            returns
                .update(&Snapshot::default(), gone, true, sleep)
                .is_empty()
        );
        let next = t0() + 5 * MINUTE;
        assert!(settled(&mut returns, &of(heard(next, true)), next, true, sleep).is_empty());
    }

    /// A pair still shown connected after the resume - the snapshot from
    /// before the sleep, and the one its link's drop sends - uses nothing
    /// up. Connected Friday 18:00 (`t0()`), asleep from 18:10; resumed
    /// Monday 09:00, 63 hours after `t0()`, onto the lock screen; the
    /// link's drop 2 s later still attached; the detach 5 s after the
    /// resume; unlocked at 09:00:30; the pods' advertisement a minute after
    /// the detach is notified `SHOW_AFTER` later, though the pair counts as
    /// heard until 5 s after the resume.
    #[test]
    fn a_connection_from_before_the_sleep_leaves_the_return_pending() {
        let mut returns = Returns::default();
        let mut connected = Snapshot::default();
        connected.attached.insert(PAIR);
        assert!(returns.update(&connected, t0(), true, AWAKE).is_empty());

        let sleep = resumed(1, t0() + 10 * MINUTE);
        let monday = t0() + 63 * 60 * MINUTE;
        let secs = |s: u64| monday + Duration::from_secs(s);
        assert!(returns.update(&connected, monday, false, sleep).is_empty());
        assert!(returns.update(&connected, secs(2), false, sleep).is_empty());
        let detached = Snapshot::default();
        assert!(returns.update(&detached, secs(5), false, sleep).is_empty());
        assert!(returns.update(&detached, secs(30), true, sleep).is_empty());
        assert_eq!(
            shown(&settled(
                &mut returns,
                &of(heard(secs(65), true)),
                secs(65),
                true,
                sleep
            )),
            [NEARBY]
        );
    }

    /// A reading cached while a link outlived the resume is not its return
    /// once the link goes, `QUIET` or more later. Asleep from 08:30
    /// (`t0()`) with the pair connected; resumed at 09:00 with the link
    /// still up; a reading cached at 09:30; detached at 12:00, the reading
    /// 2.5 hours old: nothing.
    #[test]
    fn an_old_reading_is_not_the_resume_return() {
        let mut returns = Returns::default();
        let mut connected = Snapshot::default();
        connected.attached.insert(PAIR);
        let at = |minutes: u32| t0() + minutes * MINUTE;
        assert!(returns.update(&connected, at(0), true, AWAKE).is_empty());
        let sleep = resumed(1, at(0));
        assert!(returns.update(&connected, at(30), true, sleep).is_empty());
        assert!(
            returns
                .update(&attached(heard(at(60), true)), at(60), true, sleep)
                .is_empty()
        );
        let detached = of(heard(at(60), true));
        assert!(settled(&mut returns, &detached, at(210), true, sleep).is_empty());
    }

    /// A return that begins to qualify at a snapshot rather than a reading
    /// waits `SHOW_AFTER` all the same. Asleep from 08:10 (08:00 as
    /// `t0()`) with the pair connected; resumed at 08:30 with the link
    /// still up; a reading cached at 08:31; detached at 08:40, the reading
    /// 9 minutes old and the resume's return: nothing at the detach, shown
    /// `SHOW_AFTER` after it.
    #[test]
    fn a_return_begun_at_a_detach_waits_too() {
        let mut returns = Returns::default();
        let at = |minutes: u32| t0() + minutes * MINUTE;
        returns.update(&attached(heard(at(0), true)), at(0), true, AWAKE);
        let sleep = resumed(1, at(10));
        returns.update(&attached(heard(at(0), true)), at(30), true, sleep);
        let cached = heard(at(31), true);
        returns.update(&attached(cached.clone()), at(31), true, sleep);
        let actions = settled(&mut returns, &of(cached), at(40), true, sleep);
        assert_eq!(shown(&actions), [NEARBY]);
    }

    /// An advertisement heard after the sleep began but taken in before the
    /// resume is the return once the resume is taken in. Notified at
    /// `t0()`, withdrawn by minute 2; asleep from minute 3; the
    /// advertisement heard at minute 10, 2 s before logind's resume; held
    /// on the lock screen, shown at the unlock a minute later.
    #[test]
    fn a_reading_taken_in_before_the_resume_is_its_return() {
        let mut returns = Returns::default();
        assert_eq!(
            shown(&settled(
                &mut returns,
                &of(heard(t0(), true)),
                t0(),
                true,
                AWAKE
            )),
            [NEARBY]
        );
        let later = t0() + 2 * MINUTE;
        assert_eq!(
            returns.update(&of(heard(later, true)), later, true, AWAKE),
            [Action::Withdraw(notification_id(PAIR))]
        );
        let thawed = t0() + 10 * MINUTE;
        let snap = of(heard(thawed, true));
        assert!(returns.update(&snap, thawed, false, AWAKE).is_empty());
        let sleep = resumed(1, t0() + 3 * MINUTE);
        let resume = thawed + 2 * SECOND;
        assert!(returns.update(&snap, resume, false, sleep).is_empty());
        let unlocked = resume + MINUTE;
        assert_eq!(
            shown(&returns.update(&snap, unlocked, true, sleep)),
            [NEARBY]
        );
        assert!(returns.update(&snap, unlocked, true, sleep).is_empty());
    }

    /// A return held from before a sleep is not shown from a reading taken
    /// before the sleep. First heard at 12:40 (12:00 as `t0()`) while
    /// away, held; last heard at 12:45; asleep from 12:45:30; resumed at
    /// 13:00 and unlocked at 13:05 with the 12:45 reading still the last:
    /// nothing. The reading at 13:06 is the resume's return.
    #[test]
    fn a_return_held_before_a_sleep_needs_a_reading_after_it() {
        let mut returns = Returns::default();
        let at =
            |minutes: u32, seconds: u64| t0() + minutes * MINUTE + Duration::from_secs(seconds);
        assert!(
            returns
                .update(&of(heard(at(40, 0), true)), at(40, 0), false, AWAKE)
                .is_empty()
        );
        let last = of(heard(at(45, 0), true));
        assert!(returns.update(&last, at(45, 0), false, AWAKE).is_empty());
        let sleep = resumed(1, at(45, 30));
        assert!(returns.update(&last, at(60, 0), false, sleep).is_empty());
        assert!(settled(&mut returns, &last, at(65, 0), true, sleep).is_empty());
        assert_eq!(
            shown(&settled(
                &mut returns,
                &of(heard(at(66, 0), true)),
                at(66, 0),
                true,
                sleep
            )),
            [NEARBY]
        );
    }

    /// A reading from before the sleep that ends a quiet stretch, or a stay
    /// in the case, taken in only with the resume, is no return. 12:00 as
    /// `t0()`: notified at 12:00, both pods in the case at 12:02, by when
    /// the notice's `SHOWN_FOR` is over; a pod out at 12:39, 37 minutes later, and
    /// asleep from 12:40; resumed at 13:00 with the 12:39 reading taken in
    /// only then, unlocked at 13:05: nothing. The reading at 13:06 is the
    /// resume's return.
    #[test]
    fn a_reading_before_the_sleep_taken_in_after_it_is_no_return() {
        let mut returns = Returns::default();
        let at = |minutes: u32| t0() + minutes * MINUTE;
        assert_eq!(
            shown(&settled(
                &mut returns,
                &of(heard(at(0), true)),
                at(0),
                true,
                AWAKE
            )),
            [NEARBY]
        );
        assert_eq!(
            returns.update(&of(heard(at(2), false)), at(2), true, AWAKE),
            [Action::Withdraw(notification_id(PAIR))]
        );
        let sleep = resumed(1, at(40));
        let before = of(heard(at(39), true));
        assert!(returns.update(&before, at(60), false, sleep).is_empty());
        assert!(settled(&mut returns, &before, at(65), true, sleep).is_empty());
        assert_eq!(
            shown(&settled(
                &mut returns,
                &of(heard(at(66), true)),
                at(66),
                true,
                sleep
            )),
            [NEARBY]
        );
    }

    /// A resume within a notice's minute, held while away, leaves that
    /// notice to its minute: shown at `t0()`, asleep from 30 s, the
    /// advertisement at 45 s while away and the pods back in the case at
    /// 50 s show nothing, the notice goes at a minute, and nothing comes at
    /// the unlock. At the desktop, the resume's notice replaces it
    /// `SHOW_AFTER` after the advertisement at 45 s.
    #[test]
    fn a_resume_within_the_minute_leaves_the_notice_to_its_minute() {
        let secs = |s: u64| t0() + Duration::from_secs(s);
        let sleep = resumed(1, secs(30));
        // Heard `SHOW_AFTER` before `t0()`, and shown at `t0()`.
        let first = of(heard(t0() - SHOW_AFTER, true));
        let mut returns = Returns::default();
        returns.update(&first, t0() - SHOW_AFTER, true, AWAKE);
        assert_eq!(shown(&returns.update(&first, t0(), true, AWAKE)), [NEARBY]);
        let after = of(heard(secs(45), true));
        assert!(returns.update(&after, secs(45), false, sleep).is_empty());
        let in_case = of(heard(secs(50), false));
        assert!(returns.update(&in_case, secs(50), false, sleep).is_empty());
        assert_eq!(
            returns.update(&in_case, secs(60), false, sleep),
            [Action::Withdraw(notification_id(PAIR))]
        );
        assert_eq!(returns.next_withdrawal(), None);
        assert!(returns.update(&in_case, secs(120), true, sleep).is_empty());

        let mut returns = Returns::default();
        returns.update(&first, t0() - SHOW_AFTER, true, AWAKE);
        returns.update(&first, t0(), true, AWAKE);
        let actions = settled(&mut returns, &after, secs(45), true, sleep);
        assert_eq!(shown(&actions), [NEARBY]);
        assert_eq!(returns.next_withdrawal(), Some(secs(50) + MINUTE));
    }

    /// This computer's adapter in the table tests below.
    const HOST: Address = Address::new([0x00, 0x00, 0x5E, 0x00, 0x53, 0xF0]);
    /// Its last three bytes, as an advertisement's bytes 4-6 name it.
    const HERE: [u8; 3] = [0x00, 0x53, 0xF0];
    /// A host that is not this computer.
    const ELSEWHERE: [u8; 3] = [0x00, 0x53, 0xE0];

    /// The decision table's trigger column (`docs/app/nearby.md`).
    #[derive(Debug, Clone, Copy)]
    enum Trigger {
        None,
        AfterQuiet,
        Resume,
        PullOut,
    }

    /// Its host named (bytes 4-6) column.
    #[derive(Debug, Clone, Copy)]
    enum Host {
        ThisComputer,
        Another,
        Unknown,
    }

    /// Its outcome column.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Outcome {
        NoNotice,
        Held,
        AfterWait,
        AtOnce,
    }

    /// One row of the decision table: the label, then the columns in the
    /// table's order.
    struct Row {
        label: &'static str,
        trigger: Trigger,
        pod_out: bool,
        connected: bool,
        at_desktop: bool,
        switch_on: bool,
        host: Host,
        in_use: bool,
        outcome: Outcome,
    }

    /// When the reading a row is about is heard and taken in.
    fn trigger_at() -> SystemTime {
        t0() + 2 * MINUTE
    }

    /// `Returns` just before `trigger`'s reading, at [`trigger_at`], and the
    /// resumes to judge it with. Each starts from a reading an earlier run
    /// saved: no return by itself.
    fn before(trigger: Trigger) -> (Returns, Resumes) {
        let at = trigger_at();
        let saved = |heard: SystemTime| {
            let mut returns = Returns::default();
            assert!(
                returns
                    .update(&of(restored(heard)), heard, true, AWAKE)
                    .is_empty()
            );
            returns
        };
        match trigger {
            // Heard a minute before: no stretch, no sleep, no stay in the case.
            Trigger::None => (saved(at - MINUTE), AWAKE),
            Trigger::AfterQuiet => (saved(at - QUIET), AWAKE),
            // Asleep from a minute before.
            Trigger::Resume => (saved(at - 2 * MINUTE), resumed(1, at - MINUTE)),
            // Both pods in the case from a minute before.
            Trigger::PullOut => {
                let mut returns = saved(at - 2 * MINUTE);
                let in_case = of(heard(at - MINUTE, false));
                assert!(
                    returns
                        .update(&in_case, at - MINUTE, true, AWAKE)
                        .is_empty()
                );
                (returns, AWAKE)
            }
        }
    }

    /// The snapshot with a row's reading: heard at [`trigger_at`], the host
    /// named and byte 8 as the row has them, attached here if it is.
    fn row_snapshot(row: &Row) -> Snapshot {
        let mut state = heard(trigger_at(), row.pod_out);
        state.last_host = Some(match row.host {
            Host::Another => ELSEWHERE,
            Host::ThisComputer | Host::Unknown => HERE,
        });
        // Idle, which the #71 captures saw while pods paged the host they last used.
        state.connection_state = Some(if row.in_use { 0x05 } else { 0x04 });
        let mut snap = of(state);
        if !matches!(row.host, Host::Unknown) {
            snap.adapter_addresses = Some(BTreeSet::from([HOST]));
        }
        if row.connected {
            snap.attached.insert(PAIR);
        }
        snap
    }

    /// Whether `snap`, taken in at `now`, shows a notice through `gate`.
    fn shows(
        returns: &mut Returns,
        gate: &mut Gate,
        snap: &Snapshot,
        now: SystemTime,
        at_desktop: bool,
        resumes: Resumes,
    ) -> bool {
        returns
            .update(snap, now, at_desktop, resumes)
            .into_iter()
            .filter_map(|action| gate.take(action))
            .any(|action| matches!(action, Action::Show(_)))
    }

    /// What a row's inputs come to: shown when the reading is taken in, or
    /// `SHOW_AFTER` later, at the desktop; held while away, and shown when
    /// the user is back then; or never shown.
    fn outcome(row: &Row) -> Outcome {
        let (mut returns, resumes) = before(row.trigger);
        let mut gate = Gate::new(row.switch_on);
        let snap = row_snapshot(row);
        let at = trigger_at();
        let later = at + SHOW_AFTER;
        let mut shows_at =
            |now, at_desktop| shows(&mut returns, &mut gate, &snap, now, at_desktop, resumes);
        if shows_at(at, row.at_desktop) {
            return Outcome::AtOnce;
        }
        if row.at_desktop {
            return match shows_at(later, true) {
                true => Outcome::AfterWait,
                false => Outcome::NoNotice,
            };
        }
        assert!(!shows_at(later, false), "{}: shown while away", row.label);
        match shows_at(later, true) {
            true => Outcome::Held,
            false => Outcome::NoNotice,
        }
    }

    /// `docs/app/nearby.md`'s decision table, row by row and in its order:
    /// each row's inputs, with a value picked for each "any", give the
    /// row's outcome.
    #[test]
    fn the_decision_table() {
        use {Host::*, Outcome::*, Trigger::*};
        let rows = [
            Row {
                label: "no return",
                trigger: Trigger::None,
                pod_out: true,
                connected: false,
                at_desktop: true,
                switch_on: true,
                host: Another,
                in_use: true,
                outcome: NoNotice,
            },
            Row {
                label: "both pods in the case",
                trigger: AfterQuiet,
                pod_out: false,
                connected: false,
                at_desktop: true,
                switch_on: true,
                host: Another,
                in_use: true,
                outcome: NoNotice,
            },
            Row {
                label: "connected here",
                trigger: AfterQuiet,
                pod_out: true,
                connected: true,
                at_desktop: true,
                switch_on: true,
                host: Another,
                in_use: true,
                outcome: NoNotice,
            },
            Row {
                label: "the switch off",
                trigger: PullOut,
                pod_out: true,
                connected: false,
                at_desktop: true,
                switch_on: false,
                host: Another,
                in_use: true,
                outcome: NoNotice,
            },
            Row {
                label: "the user away",
                trigger: Resume,
                pod_out: true,
                connected: false,
                at_desktop: false,
                switch_on: true,
                host: Another,
                in_use: true,
                outcome: Held,
            },
            Row {
                label: "named another host",
                trigger: AfterQuiet,
                pod_out: true,
                connected: false,
                at_desktop: true,
                switch_on: true,
                host: Another,
                in_use: false,
                outcome: AtOnce,
            },
            Row {
                label: "in use elsewhere",
                trigger: PullOut,
                pod_out: true,
                connected: false,
                at_desktop: true,
                switch_on: true,
                host: ThisComputer,
                in_use: true,
                outcome: AtOnce,
            },
            Row {
                label: "named this computer",
                trigger: Resume,
                pod_out: true,
                connected: false,
                at_desktop: true,
                switch_on: true,
                host: ThisComputer,
                in_use: false,
                outcome: AfterWait,
            },
            Row {
                label: "host named unknown",
                trigger: PullOut,
                pod_out: true,
                connected: false,
                at_desktop: true,
                switch_on: true,
                host: Unknown,
                in_use: false,
                outcome: AfterWait,
            },
        ];
        for row in &rows {
            assert_eq!(outcome(row), row.outcome, "{}", row.label);
        }
    }

    /// The withdrawal table's outcome column.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Notice {
        Withdrawn,
        Replaced,
        Kept,
    }

    /// `docs/app/nearby.md`'s withdrawal table, row by row and in its
    /// order: a notice shown at `SHOW_AFTER` after the pair's first
    /// advertisement, then each row's event, gives the row's outcome then.
    #[test]
    fn the_withdrawal_table() {
        let first = heard(t0(), true);
        let since = t0() + SHOW_AFTER;
        let at = move |seconds: u64| since + Duration::from_secs(seconds);
        let through = |actions: Vec<Action>, gate: &mut Gate| -> Vec<Action> {
            actions.into_iter().filter_map(|a| gate.take(a)).collect()
        };
        // Both pods in the case at 1 s, a pod out at 20 s naming `host` as
        // the last: a pull-out.
        let pulled_out = |host: [u8; 3]| {
            move |returns: &mut Returns, gate: &mut Gate| {
                let in_case = of(heard(at(1), false));
                let mut actions = returns.update(&in_case, at(1), true, AWAKE);
                let mut out = heard(at(20), true);
                out.last_host = Some(host);
                let mut snap = of(out);
                snap.adapter_addresses = Some(BTreeSet::from([HOST]));
                actions.extend(returns.update(&snap, at(20), true, AWAKE));
                through(actions, gate)
            }
        };
        type Event = Box<dyn Fn(&mut Returns, &mut Gate) -> Vec<Action>>;
        let rows: [(&str, Event, Notice); 8] = [
            (
                "`SHOWN_FOR` over",
                Box::new(move |returns, gate| {
                    let snap = of(heard(t0(), true));
                    through(returns.update(&snap, since + SHOWN_FOR, true, AWAKE), gate)
                }),
                Notice::Withdrawn,
            ),
            (
                "shown after now",
                Box::new(move |returns, gate| {
                    let snap = of(heard(t0(), true));
                    through(returns.update(&snap, since - SECOND, true, AWAKE), gate)
                }),
                Notice::Withdrawn,
            ),
            (
                "connected here",
                Box::new(move |returns, gate| {
                    let snap = attached(heard(t0(), true));
                    through(returns.update(&snap, at(1), true, AWAKE), gate)
                }),
                Notice::Withdrawn,
            ),
            (
                "the switch off",
                Box::new(|_, gate| gate.set_enabled(false)),
                Notice::Withdrawn,
            ),
            (
                "the quit",
                Box::new(|_, gate| gate.withdraw_all()),
                Notice::Withdrawn,
            ),
            (
                "a new return that waits",
                Box::new(pulled_out(HERE)),
                Notice::Withdrawn,
            ),
            (
                "a new return shown at once",
                Box::new(pulled_out(ELSEWHERE)),
                Notice::Replaced,
            ),
            (
                "a resume's return",
                Box::new(move |returns, gate| {
                    let snap = of(heard(at(2), true));
                    let sleep = resumed(1, at(1));
                    through(returns.update(&snap, at(2), true, sleep), gate)
                }),
                Notice::Kept,
            ),
        ];
        for (label, event, expected) in rows {
            let mut returns = Returns::default();
            let mut gate = Gate::new(true);
            let snap = of(first.clone());
            assert!(through(returns.update(&snap, t0(), true, AWAKE), &mut gate).is_empty());
            let actions = through(returns.update(&snap, since, true, AWAKE), &mut gate);
            assert_eq!(shown(&actions), [NEARBY], "{label}: shown first");
            let actions = event(&mut returns, &mut gate);
            let withdrawn = actions.contains(&Action::Withdraw(notification_id(PAIR)));
            let notice = match (withdrawn, shown(&actions).is_empty()) {
                (true, _) => Notice::Withdrawn,
                (false, false) => Notice::Replaced,
                (false, true) => Notice::Kept,
            };
            assert_eq!(notice, expected, "{label}: {actions:?}");
        }
    }

    /// Whether a pull-out naming `last_host`, with `byte_8`, is shown at
    /// once, this computer's adapters' addresses `adapters`.
    fn at_once(last_host: Option<[u8; 3]>, byte_8: u8, adapters: &[Address]) -> bool {
        let (mut returns, resumes) = before(Trigger::PullOut);
        let mut state = heard(trigger_at(), true);
        state.last_host = last_host;
        state.connection_state = Some(byte_8);
        let mut snap = of(state);
        snap.adapter_addresses = Some(adapters.iter().copied().collect());
        !shown(&returns.update(&snap, trigger_at(), true, resumes)).is_empty()
    }

    /// Byte 8 skips the wait for Music, Call and Ringing alone, not for
    /// Disconnected, Idle, Hanging Up, `0x08` or LibrePods' stand-in `0xFF`.
    /// The host named is this computer's when it ends any adapter's address,
    /// whatever that address's first three bytes; a reading that names none,
    /// and no adapter, keep the wait.
    #[test]
    fn what_skips_the_wait() {
        for byte_8 in [0x00, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0xFF] {
            let expected = matches!(byte_8, 0x05..=0x07);
            assert_eq!(
                at_once(Some(HERE), byte_8, &[HOST]),
                expected,
                "{byte_8:#04x}"
            );
        }
        let second = Address::new([0x00, 0x00, 0x5E, 0x00, 0x53, 0xF1]);
        let other_prefix = Address::new([0x11, 0x22, 0x33, 0x00, 0x53, 0xF0]);
        assert!(!at_once(Some(HERE), 0x04, &[second, HOST]));
        assert!(!at_once(Some(HERE), 0x04, &[other_prefix]));
        assert!(at_once(Some(HERE), 0x04, &[second]));
        assert!(!at_once(None, 0x04, &[HOST]));
        assert!(!at_once(Some(HERE), 0x04, &[]));
    }

    /// A reading cached while the pair was connected here skips no wait,
    /// whatever it says: connected at 08:00 (`t0()`), asleep from 08:10,
    /// resumed at 08:30 with the link still up, a reading saying another
    /// host and Music cached at 08:31, detached at 08:40: nothing at the
    /// detach, shown `SHOW_AFTER` after it. The same reading heard a second
    /// after the detach is shown at once.
    #[test]
    fn a_reading_cached_while_connected_here_skips_no_wait() {
        let at = |minutes: u32| t0() + minutes * MINUTE;
        let mut cached = heard(at(31), true);
        cached.last_host = Some(ELSEWHERE);
        cached.connection_state = Some(0x05);
        let with_adapter = |mut snap: Snapshot| {
            snap.adapter_addresses = Some(BTreeSet::from([HOST]));
            snap
        };
        let detached = |state: PodState| {
            let mut returns = Returns::default();
            returns.update(&attached(heard(at(0), true)), at(0), true, AWAKE);
            let sleep = resumed(1, at(10));
            returns.update(&attached(heard(at(0), true)), at(30), true, sleep);
            let snap = with_adapter(attached(cached.clone()));
            returns.update(&snap, at(31), true, sleep);
            let gone = at(40);
            assert!(
                returns
                    .update(&with_adapter(of(cached.clone())), gone, true, sleep)
                    .is_empty()
            );
            (returns, sleep, with_adapter(of(state)))
        };

        let (mut returns, sleep, snap) = detached(cached.clone());
        let later = at(40) + SHOW_AFTER;
        assert_eq!(shown(&returns.update(&snap, later, true, sleep)), [NEARBY]);

        let mut fresh = cached.clone();
        fresh.last_seen = Some(at(40) + SECOND);
        let (mut returns, sleep, snap) = detached(fresh);
        let now = at(40) + SECOND;
        assert_eq!(shown(&returns.update(&snap, now, true, sleep)), [NEARBY]);
    }

    /// A notice says why it skipped the wait only where it did: shown at once
    /// for another host, with a reason; held while away for `SHOW_AFTER` and
    /// more, shown at the user's return, with none, as it waited all the
    /// same.
    #[test]
    fn only_a_skipped_wait_has_a_reason() {
        let snap = || {
            let mut state = heard(trigger_at(), true);
            state.last_host = Some(ELSEWHERE);
            let mut snap = of(state);
            snap.adapter_addresses = Some(BTreeSet::from([HOST]));
            snap
        };
        let (mut returns, resumes) = before(Trigger::AfterQuiet);
        let actions = returns.update(&snap(), trigger_at(), true, resumes);
        assert_eq!(shown(&actions), [NEARBY]);
        assert_eq!(returns.skipped(PAIR), Some(Elsewhere::AnotherHost));

        let (mut returns, resumes) = before(Trigger::AfterQuiet);
        assert!(
            returns
                .update(&snap(), trigger_at(), false, resumes)
                .is_empty()
        );
        let back = trigger_at() + SHOW_AFTER;
        assert_eq!(
            shown(&returns.update(&snap(), back, true, resumes)),
            [NEARBY]
        );
        assert_eq!(returns.skipped(PAIR), None);
    }

    /// Off from the start, nothing is sent, and turning the switch on brings
    /// back nothing sent meanwhile; a withdrawal of a notice not on show goes
    /// nowhere.
    #[test]
    fn the_switch_off_sends_nothing_and_on_brings_nothing_back() {
        let mut gate = Gate::new(false);
        let show = Action::Show(notice(PAIR, &Snapshot::default()));
        assert_eq!(gate.take(show.clone()), None);
        assert_eq!(gate.set_enabled(true), []);
        assert_eq!(gate.take(Action::Withdraw(notification_id(PAIR))), None);
        assert_eq!(gate.take(show.clone()), Some(show));
    }

    /// The quit withdraws the notices on show, once, and only those.
    #[test]
    fn the_quit_withdraws_what_is_on_show() {
        let mut gate = Gate::new(true);
        gate.take(Action::Show(notice(PAIR, &Snapshot::default())));
        gate.take(Action::Show(notice(OTHER, &Snapshot::default())));
        gate.take(Action::Withdraw(notification_id(OTHER)));
        assert_eq!(
            gate.withdraw_all(),
            [Action::Withdraw(notification_id(PAIR))]
        );
        assert_eq!(gate.withdraw_all(), []);
    }

    /// The switch off withdraws the notices on show, and only those, once;
    /// a withdrawal of one on show goes through.
    #[test]
    fn the_switch_off_withdraws_what_is_on_show() {
        let mut gate = Gate::new(true);
        gate.take(Action::Show(notice(PAIR, &Snapshot::default())));
        gate.take(Action::Show(notice(OTHER, &Snapshot::default())));
        let withdraw = Action::Withdraw(notification_id(OTHER));
        assert_eq!(gate.take(withdraw.clone()), Some(withdraw));
        assert_eq!(
            gate.set_enabled(false),
            [Action::Withdraw(notification_id(PAIR))]
        );
        assert_eq!(gate.set_enabled(false), []);
        assert_eq!(gate.set_enabled(true), []);
    }

    /// Long enough for any notice `run` would show: past `SHOW_AFTER`, on
    /// tokio's clock, which these tests pause, so that it passes at once
    /// once nothing else is left to run.
    const WAIT: Duration = Duration::from_secs(30);

    /// A wall clock for `run_on` that starts at `base` and runs with tokio's
    /// clock, paused in these tests: `run`'s sleeps pass at once, and the
    /// wall clock with them.
    fn paused_clock(base: SystemTime) -> impl Fn() -> SystemTime {
        let start = tokio::time::Instant::now();
        move || base + start.elapsed()
    }

    /// What `run` sends within `WAIT`, if anything.
    async fn within_wait(notices: &async_channel::Receiver<Action>) -> Option<Action> {
        tokio::time::timeout(WAIT, notices.recv())
            .await
            .ok()
            .map(|action| action.expect("the notifier runs"))
    }

    /// `run` takes a resume in: the advertisement heard after the sleep
    /// began is a return, after the first notice.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn run_takes_a_resume_in() {
        let base = SystemTime::now();
        let clock = paused_clock(base);
        let first = of(heard(base - 2 * SECOND, true));
        // As in the app, `first` is the snapshot the channel holds.
        let (snapshots, updates) = watch::channel(first.clone());
        let (resume, presence) = watch::channel(told(true, AWAKE));
        let (tx, notices) = async_channel::unbounded();
        let running = tokio::spawn(run_on(clock, first, updates, presence, tx));
        let first = within_wait(&notices).await.expect("the first notice");
        assert_eq!(shown(&[first]), [NEARBY]);

        let now = base + SHOW_AFTER;
        snapshots.send_replace(of(heard(now, true)));
        resume.send_replace(told(true, resumed(1, now - SECOND)));
        let next = within_wait(&notices)
            .await
            .expect("the notice after the resume");
        assert_eq!(shown(&[next]), [NEARBY]);
        running.abort();
    }

    /// What `presence` tells, as one value.
    fn told(at_desktop: bool, resumes: Resumes) -> Presence {
        Presence {
            at_desktop,
            resumes,
        }
    }

    /// Lets a `run` spawned on this `current_thread` runtime take in what
    /// was sent to it, and send what it will. `yield_now` puts this task at
    /// the back of the queue (tokio 1.54.1), behind `run`, woken by the
    /// send; `run` then goes on until its `select!` waits again, as its
    /// sends to an unbounded channel never wait.
    async fn settle() {
        tokio::task::yield_now().await;
    }

    /// `run` judges `first` before a later snapshot already waiting: the
    /// saved reading from 10 minutes ago gives its time, so the live
    /// reading after it is no return, where judged alone it would be one.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn run_judges_the_first_snapshot_first() {
        let base = SystemTime::now();
        let first = of(restored(base - 10 * MINUTE));
        let (snapshots, updates) = watch::channel(first.clone());
        snapshots.send_replace(of(heard(base, true)));
        let (_logind, presence) = watch::channel(told(true, AWAKE));
        let (tx, notices) = async_channel::unbounded();
        let running = tokio::spawn(run_on(paused_clock(base), first, updates, presence, tx));
        assert_eq!(
            within_wait(&notices).await,
            None,
            "the saved reading was passed over"
        );
        running.abort();
    }

    /// `run` reads the lock and the resume as one value, as logind sent
    /// them: a reading already taken in, heard after the sleep began, with
    /// the screen locked and the resume taken in together, is held, and
    /// shown at the unlock.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn run_takes_a_lock_and_a_resume_in_together() {
        let base = SystemTime::now();
        let (snapshots, updates) = watch::channel(Snapshot::default());
        let (logind, presence) = watch::channel(told(true, AWAKE));
        let (tx, notices) = async_channel::unbounded();
        let first = of(restored(base - 10 * SECOND));
        let running = tokio::spawn(run_on(paused_clock(base), first, updates, presence, tx));
        settle().await;
        // Heard 9 s after the saved reading: no return by itself.
        snapshots.send_replace(of(heard(base - SECOND, true)));
        assert_eq!(
            within_wait(&notices).await,
            None,
            "a return without a resume"
        );

        let sleep = resumed(1, base - 5 * SECOND);
        logind.send_replace(told(false, sleep));
        assert_eq!(
            within_wait(&notices).await,
            None,
            "shown on the lock screen"
        );
        logind.send_replace(told(true, sleep));
        let action = within_wait(&notices).await.expect("the notice");
        assert_eq!(shown(&[action]), [NEARBY]);
        running.abort();
    }

    /// `run` reads the latest of both the snapshot and `presence` whatever
    /// woke it: a pod taken out and the screen locked, both before `run`
    /// wakes, is held, and shown at the unlock. In the case for less than
    /// `IN_CASE_SETTLE`, so the return is the first advertisement's.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn run_takes_a_lock_and_a_snapshot_in_together() {
        let base = SystemTime::now();
        let (snapshots, updates) = watch::channel(Snapshot::default());
        let (logind, presence) = watch::channel(told(true, AWAKE));
        let (tx, notices) = async_channel::unbounded();
        let first = of(heard(base - 3 * SECOND, false));
        let running = tokio::spawn(run_on(paused_clock(base), first, updates, presence, tx));
        assert_eq!(
            within_wait(&notices).await,
            None,
            "shown with both pods in the case"
        );

        snapshots.send_replace(of(heard(base - SECOND, true)));
        logind.send_replace(told(false, AWAKE));
        assert_eq!(
            within_wait(&notices).await,
            None,
            "shown on the lock screen"
        );
        logind.send_replace(told(true, AWAKE));
        let action = within_wait(&notices).await.expect("the notice");
        assert_eq!(shown(&[action]), [NEARBY]);
        running.abort();
    }

    /// `run` wakes for a return due to be shown, with nothing else to wake
    /// it: a reading heard as it starts is held back, and shown
    /// `SHOW_AFTER` later on its clock with no new snapshot or presence.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn run_wakes_to_show_a_return() {
        let base = SystemTime::now();
        let first = of(heard(base, true));
        let (_snapshots, updates) = watch::channel(first.clone());
        let (_logind, presence) = watch::channel(told(true, AWAKE));
        let (tx, notices) = async_channel::unbounded();
        let running = tokio::spawn(run_on(paused_clock(base), first, updates, presence, tx));
        let started = tokio::time::Instant::now();
        let action = within_wait(&notices).await.expect("the notice");
        assert_eq!(shown(&[action]), [NEARBY]);
        assert_eq!(started.elapsed(), SHOW_AFTER);
        running.abort();
    }

    /// A notice is withdrawn once a minute has passed since it was shown, not
    /// a second before, once, and `next_withdrawal` says when.
    #[test]
    fn a_notice_is_withdrawn_a_minute_after_it_is_shown() {
        let mut returns = Returns::default();
        // Heard `SHOW_AFTER` before `t0()`, and shown at `t0()`.
        let snap = of(heard(t0() - SHOW_AFTER, true));
        returns.update(&snap, t0() - SHOW_AFTER, true, AWAKE);
        assert_eq!(shown(&returns.update(&snap, t0(), true, AWAKE)), [NEARBY]);
        assert_eq!(returns.next_withdrawal(), Some(t0() + MINUTE));
        let early = t0() + 59 * SECOND;
        assert!(returns.update(&snap, early, true, AWAKE).is_empty());
        let withdrawn = [Action::Withdraw(notification_id(PAIR))];
        assert_eq!(returns.update(&snap, t0() + MINUTE, true, AWAKE), withdrawn);
        assert_eq!(returns.next_withdrawal(), None);
        assert!(
            returns
                .update(&snap, t0() + 2 * MINUTE, true, AWAKE)
                .is_empty()
        );
    }

    /// A return held while away gets its minute from when it is shown.
    #[test]
    fn a_held_notice_gets_its_minute_from_when_it_is_shown() {
        let mut returns = Returns::default();
        let snap = of(heard(t0(), true));
        assert!(returns.update(&snap, t0(), false, AWAKE).is_empty());
        let back = t0() + 10 * MINUTE;
        assert_eq!(shown(&returns.update(&snap, back, true, AWAKE)), [NEARBY]);
        let early = back + 59 * SECOND;
        assert!(returns.update(&snap, early, true, AWAKE).is_empty());
        assert_eq!(
            returns.update(&snap, back + MINUTE, true, AWAKE),
            [Action::Withdraw(notification_id(PAIR))]
        );
    }

    /// A pair connected here, then gone from the snapshot altogether - no
    /// reading cached - counts as heard when it went: its advertisement
    /// `QUIET` after that is a return.
    #[test]
    fn a_pair_gone_from_the_snapshot_is_no_longer_connected() {
        let mut returns = Returns::default();
        let mut connected = Snapshot::default();
        connected.attached.insert(PAIR);
        assert!(returns.update(&connected, t0(), true, AWAKE).is_empty());
        let gone = t0() + MINUTE;
        assert!(
            returns
                .update(&Snapshot::default(), gone, true, AWAKE)
                .is_empty()
        );
        let back = gone + QUIET;
        assert_eq!(
            shown(&settled(
                &mut returns,
                &of(heard(back, true)),
                back,
                true,
                AWAKE
            )),
            [NEARBY]
        );
    }

    /// `run` takes in the user's return to the desktop as it takes in a
    /// snapshot: a return held while away is sent once the user is back.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn run_sends_a_held_return_once_the_user_is_back() {
        let base = SystemTime::now();
        let first = of(heard(base, true));
        // As in the app, `first` is the snapshot the channel holds.
        let (_snapshots, updates) = watch::channel(first.clone());
        let (desk, presence) = watch::channel(told(false, AWAKE));
        let (tx, notices) = async_channel::unbounded();
        let running = tokio::spawn(run_on(paused_clock(base), first, updates, presence, tx));
        assert_eq!(within_wait(&notices).await, None, "sent while away");

        desk.send_replace(told(true, AWAKE));
        let action = within_wait(&notices).await.expect("the notice");
        assert_eq!(shown(&[action]), [NEARBY]);
        running.abort();
    }

    /// A time heard that is after now - the clock set back an hour since -
    /// is forgotten, as the cache forgets such a reading: the next
    /// advertisement is a return, where measured from it, it would be none.
    #[test]
    fn a_time_heard_after_now_is_forgotten() {
        let mut returns = Returns::default();
        settled(&mut returns, &of(heard(t0(), true)), t0(), true, AWAKE);
        let set_back = t0() - 60 * MINUTE;
        assert_eq!(
            shown(&settled(
                &mut returns,
                &of(heard(set_back, true)),
                set_back,
                true,
                AWAKE
            )),
            [NEARBY]
        );
    }

    /// A sleep begun after now, the clock set back since, holds no return
    /// off. 12:00 as `t0()`: connected at 12:00, asleep from 12:10, resumed
    /// at 12:30 with the link still up, detached at 12:40; the clock set
    /// back to 11:00, the advertisement heard then is a return, as the
    /// time heard, 12:40, is forgotten.
    #[test]
    fn a_sleep_begun_after_now_holds_no_return_off() {
        let mut returns = Returns::default();
        let at = |minutes: u32| t0() + minutes * MINUTE;
        let mut connected = Snapshot::default();
        connected.attached.insert(PAIR);
        assert!(returns.update(&connected, at(0), true, AWAKE).is_empty());
        let sleep = resumed(1, at(10));
        assert!(returns.update(&connected, at(30), true, sleep).is_empty());
        assert!(
            returns
                .update(&Snapshot::default(), at(40), true, sleep)
                .is_empty()
        );
        let set_back = t0() - 60 * MINUTE;
        assert_eq!(
            shown(&settled(
                &mut returns,
                &of(heard(set_back, true)),
                set_back,
                true,
                sleep
            )),
            [NEARBY]
        );
    }

    /// Each pair's return is its own, named as `Snapshot::display_name`
    /// names it.
    #[test]
    fn each_pair_returns_on_its_own() {
        let mut returns = Returns::default();
        let mut snap = snapshot([(PAIR, heard(t0(), true)), (OTHER, heard(t0(), false))]);
        snap.device_names.insert(
            PAIR,
            DeviceNames {
                alias: "Marcel’s AirPods".into(),
                name: None,
            },
        );
        assert_eq!(
            shown(&settled(&mut returns, &snap, t0(), true, AWAKE)),
            [("nearby-00:00:5E:00:53:01", "Marcel’s AirPods nearby")]
        );
        let at = t0() + MINUTE;
        snap.states.insert(OTHER, heard(at, true));
        assert_eq!(
            shown(&settled(&mut returns, &snap, at, true, AWAKE)),
            [("nearby-00:00:5E:00:53:02", "AirPods nearby")]
        );
    }

    /// The pipeline from a fake pair's advertisements to the notice: a pod
    /// taken out of the open case is shown, named by BlueZ's alias, with a
    /// Connect button, and the pair connecting withdraws it.
    #[cfg(feature = "harness")]
    mod pipeline {
        use futures_util::StreamExt;

        use super::*;
        use crate::attachment::Attachment;
        use crate::ble::parser::PodSide;
        use crate::ble::scanner::Scanner;
        use crate::harness::pods::PRO_3;
        use crate::harness::{PrivateBus, World};
        use crate::keystore::{Keystore, PairKeys};
        use crate::podstate::Coordinator;

        const TIMEOUT: Duration = Duration::from_secs(15);

        async fn next(notices: &async_channel::Receiver<Action>) -> Action {
            tokio::time::timeout(TIMEOUT, notices.recv())
                .await
                .expect("timed out waiting for a notice")
                .expect("the notifier runs")
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_pod_out_of_the_case_is_shown_and_connecting_withdraws_it() {
            let bus = PrivateBus::start().unwrap();
            let sockets = tempfile::Builder::new()
                .prefix("cymba-aap")
                .tempdir()
                .unwrap();
            let world = World::new(bus.address(), sockets.path()).await.unwrap();
            world.add_pods(PAIR, PRO_3, "Fake AirPods").await.unwrap();
            world.publish().await.unwrap();

            // The pair's keys, as though it had connected once before; no
            // readings saved beside them.
            let data = tempfile::tempdir().unwrap();
            let keystore = Keystore::with_dir(data.path().to_path_buf()).unwrap();
            let keys = world.keys(PAIR).await.unwrap();
            keystore
                .save(&[(PAIR, PairKeys::new(keys.enc_key, Some(keys.irk)))].into())
                .unwrap();
            let coordinator =
                Coordinator::with_aap_socket_dir(Some(keystore), sockets.path().to_path_buf());

            let mut updates = coordinator.subscribe();
            let first = updates.borrow_and_update().clone();
            let (_logind, presence) = watch::channel(Presence::default());
            let (tx, notices) = async_channel::unbounded();
            let watching = tokio::spawn(run(first, updates, presence, tx));

            let attachment = Attachment::new(
                bus.connect_for_providers().await.unwrap(),
                coordinator.clone(),
            )
            .await
            .unwrap();
            let attaching = tokio::spawn(attachment.run());

            let scanner = Scanner::with_connection(bus.connect().await.unwrap());
            let adverts = scanner.advertisements().await.unwrap();
            scanner.start_discovery().await.unwrap();
            let fed = coordinator.clone();
            let mut heard = coordinator.subscribe();
            let scanning = tokio::spawn(async move {
                tokio::pin!(adverts);
                while let Some(advert) = adverts.next().await {
                    fed.handle_advertisement(advert.data, advert.ble_mac).await;
                }
            });

            // Heard first with both pods in the open case.
            tokio::time::timeout(TIMEOUT, heard.wait_for(|s| s.states.contains_key(&PAIR)))
                .await
                .expect("timed out waiting for the pair")
                .unwrap();
            world
                .with_pods(PAIR, |pods| pods.set_in_case(PodSide::Left, false))
                .await
                .unwrap();
            let Action::Show(notice) = next(&notices).await else {
                panic!("expected a notice");
            };
            assert_eq!(notice.id, notification_id(PAIR));
            assert_eq!(notice.title, "Fake AirPods nearby");
            assert!(notice.connect);

            world.connect(PAIR).await.unwrap();
            assert_eq!(
                next(&notices).await,
                Action::Withdraw(notification_id(PAIR))
            );

            scanning.abort();
            attaching.abort();
            watching.abort();
        }
    }
}
