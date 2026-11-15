//! The AirPods' microphone over AAP, offered as an input on PipeWire, while
//! A2DP plays on in stereo (#209). Experimental, and off unless the
//! Preferences switch turns it on: it switches the pair's card off and back
//! after every stop, the workaround #212 is to replace.
//!
//! With the switch on and a pair attached on AAP, a source named
//! "AirPods Microphone" is up, and is made the default input. Only while
//! something records from it does the pair's microphone run: started when a
//! recording begins, stopped [`STOP_AFTER`] after the last one ends.
//! Conversation Awareness is turned off for the stream, and on again after
//! it, if it was on. After every stop the pair's card is switched off and
//! back, and the players the switch paused are played again: on a Pro 3 one
//! pod stayed silent after a stop until then. `docs/app/mic.md` says how and
//! why, and what is not known.
//!
//! [`Stream`] decides when to start, stop and turn Conversation Awareness
//! off and on, and [`DefaultInput`] when to make the source the default
//! input and put the user's back, each from what it is told and the time,
//! as `volume::Ducking` does, and each is tested on its own. [`Mic::run`]
//! carries them out, in a task of its own beside attachment: commands to
//! the pods through `Coordinator::command`, the source and the default
//! input through the audio side - PipeWire's thread, `pipewire` - and the
//! card switch's BlueZ and MPRIS calls in a task of their own.

#[cfg(feature = "mic")]
pub mod decoder;
#[cfg(feature = "mic")]
pub mod pipewire;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bluer::Address;
use tokio::sync::{mpsc, oneshot, watch};
use zbus::Connection;

use crate::bluez::Backoff;
use crate::media::{self, Bus, Playback, Player};
use crate::podstate::{Command, Coordinator, MicAudio, MicEvent, Snapshot};
use crate::volume::{self, Connect};

/// The source's node name, which the default input names.
pub const NODE_NAME: &str = "cymba.airpods_mic";

/// The source's name, as the desktop shows it.
pub const NODE_DESCRIPTION: &str = "AirPods Microphone";

/// The default input, in PipeWire's `default` metadata.
pub const DEFAULT_SOURCE_KEY: &str = "default.configured.audio.source";

/// How long after the last recording ends the stream stops: so that an app
/// that closes the input and opens it again at once - a call switching
/// devices, a browser asking which inputs there are - does not stop the
/// stream and switch the card each time. A choice.
pub const STOP_AFTER: Duration = Duration::from_secs(3);

/// How many audio packets wait for the task at most: two seconds' worth. One
/// more is dropped by the read loop, which never waits.
pub const AUDIO_QUEUE: usize = 64;

/// How many times the card is switched off and back after a stop, for the
/// music to come back to the pair, as the probe does.
pub const SWITCH_TRIES: u32 = 3;

/// How long a new A2DP transport is given to come after a card switch, a
/// player the switch paused to report the pause, and A2DP to play after
/// the player was played again: the probe's.
pub const RESUME_WAIT: Duration = Duration::from_secs(5);

/// How often the transport and the players are read meanwhile.
pub const RESUME_POLL: Duration = Duration::from_millis(100);

/// The longest the stream's stop, Conversation Awareness put back and the
/// default input put back may take as the app quits: a choice, short enough
/// that a quit never seems to hang.
pub const QUIT_LIMIT: Duration = Duration::from_secs(2);

/// Whether the microphone can be offered, for the Preferences switch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    /// Not known yet: PipeWire not asked.
    Unknown,
    Available,
    /// Why not, in a sentence for the switch's row.
    Unavailable(String),
}

/// What the task asks of the audio side.
#[derive(Debug)]
pub enum AudioRequest {
    /// Put the source up, or take it down.
    Source(bool),
    /// An audio packet, for the source to play.
    Audio(Vec<u8>),
    /// Make the default input the node of this name, or clear it.
    SetDefault(Option<String>),
    /// Switch the pair's card off and back; the result goes to `done`.
    SwitchCard {
        mac: Address,
        done: oneshot::Sender<Result<()>>,
    },
    /// Answer `done` once everything asked before is through.
    Sync(oneshot::Sender<()>),
    /// End.
    Quit,
}

/// What the audio side tells the task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioEvent {
    /// Connected, with the default input as it reads: the node it names.
    Ready { default: Option<String> },
    /// The default input changed: the node it names now.
    Default(Option<String>),
    /// Whether anything records from the source.
    Recording(bool),
    /// The audio side ended, and why.
    Gone(String),
}

/// The audio side, as the task holds it.
pub trait AudioSide: Send + Sync {
    /// Hands `request` over: false once the audio side has ended.
    fn ask(&self, request: AudioRequest) -> bool;
}

impl AudioSide for mpsc::UnboundedSender<AudioRequest> {
    fn ask(&self, request: AudioRequest) -> bool {
        self.send(request).is_ok()
    }
}

/// Starts an audio side that tells `events` - PipeWire's thread in the app -
/// or says why there can be none: no decoder, say, which no retry helps.
pub type StartAudio = Box<
    dyn FnMut(mpsc::UnboundedSender<AudioEvent>) -> Result<Arc<dyn AudioSide>, String>
        + Send
        + Sync,
>;

/// Why the microphone can never be offered by this build, if it cannot.
pub fn missing() -> Option<String> {
    if cfg!(feature = "mic") {
        None
    } else {
        Some("this build has no microphone support".into())
    }
}

/// The app's audio side: FFmpeg's decoder checked, then PipeWire's thread.
pub fn start_audio() -> StartAudio {
    Box::new(|events| {
        #[cfg(feature = "mic")]
        {
            decoder::available().map_err(|e| format!("{e:#}"))?;
            Ok(Arc::from(pipewire::start(events)))
        }
        #[cfg(not(feature = "mic"))]
        {
            drop(events);
            Err("this build has no microphone support".into())
        }
    })
}

/// What [`Stream`] asks to be done, in this order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Do {
    /// Turn the pair's Conversation Awareness on or off.
    ConversationAwareness { mac: Address, on: bool },
    /// Start the pair's microphone.
    Start(Address),
    /// Stop it, and switch the pair's card off and back after.
    Stop(Address),
}

/// What the pairs are to the stream now.
#[derive(Debug, Clone, Copy)]
pub struct View<'a> {
    /// The pair the source is for: the switch on, the microphone available,
    /// and the pair attached and on AAP.
    pub pair: Option<Address>,
    /// Whether anything records from the source.
    pub recording: bool,
    /// Every pair with a link, with its Conversation Awareness as the link
    /// has it: `None` until reported.
    pub links: &'a HashMap<Address, Option<bool>>,
}

/// Where the stream is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    /// Started, and recorded from.
    Running {
        mac: Address,
    },
    /// Started, nothing recording since; stops at `until`.
    Lingering {
        mac: Address,
        until: Instant,
    },
    /// Stopped; the card switch is under way.
    Switching {
        mac: Address,
    },
}

/// Conversation Awareness the stream turned off, to turn on again after.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TurnedOff {
    mac: Address,
    /// Whether the link has read off since: until then the command may not
    /// have landed, and a reading of on is not the user's.
    seen_off: bool,
}

/// One stream of the microphone at a time: when it starts and stops, and
/// what is done around it. Takes what the pairs are, whether anything
/// records, and the time, and says what to do; the caller does it, and
/// says when the card switch after a stop is done.
#[derive(Debug, Clone)]
pub struct Stream {
    state: State,
    turned_off: Option<TurnedOff>,
    /// [`STOP_AFTER`] but in a test, which may ask for less.
    stop_after: Duration,
}

impl Default for Stream {
    fn default() -> Self {
        Self {
            state: State::Idle,
            turned_off: None,
            stop_after: STOP_AFTER,
        }
    }
}

impl Stream {
    /// What `view` at `now` asks for.
    ///
    /// - Idle, something recording and a pair: Conversation Awareness off
    ///   if it reads on, then the start.
    /// - Running, nothing recording: the stop is due [`STOP_AFTER`] from now;
    ///   something recording again meanwhile, it is not.
    /// - Due, or the pair no longer the source's: the stop, and the card
    ///   switch after it - unless the pair's link went, which ended the
    ///   stream already.
    /// - Once the stream is over, Conversation Awareness on again, if the
    ///   stream turned it off and it still reads off. Read on after it read
    ///   off, someone turned it on: it is left alone.
    pub fn update(&mut self, view: View<'_>, now: Instant) -> Vec<Do> {
        let mut out = Vec::new();
        match self.state {
            State::Running { mac } | State::Lingering { mac, .. } if view.pair != Some(mac) => {
                if view.links.contains_key(&mac) {
                    out.push(Do::Stop(mac));
                    self.state = State::Switching { mac };
                } else {
                    self.state = State::Idle;
                }
            }
            State::Running { mac } if !view.recording => {
                self.state = State::Lingering {
                    mac,
                    until: now + self.stop_after,
                };
            }
            State::Lingering { mac, .. } if view.recording => {
                self.state = State::Running { mac };
            }
            State::Lingering { mac, until } if now >= until => {
                out.push(Do::Stop(mac));
                self.state = State::Switching { mac };
            }
            State::Idle if view.recording => {
                if let Some(mac) = view.pair.filter(|mac| view.links.contains_key(mac)) {
                    if self.turned_off.is_none() && view.links[&mac] == Some(true) {
                        out.push(Do::ConversationAwareness { mac, on: false });
                        self.turned_off = Some(TurnedOff {
                            mac,
                            seen_off: false,
                        });
                    }
                    out.push(Do::Start(mac));
                    self.state = State::Running { mac };
                }
            }
            _ => {}
        }
        let streaming = self.streaming();
        if let Some(turned_off) = &mut self.turned_off {
            let mac = turned_off.mac;
            let streaming = streaming == Some(mac);
            match view.links.get(&mac) {
                Some(Some(false)) => {
                    turned_off.seen_off = true;
                    if !streaming {
                        out.push(Do::ConversationAwareness { mac, on: true });
                        self.turned_off = None;
                    }
                }
                Some(Some(true)) if turned_off.seen_off => self.turned_off = None,
                _ => {}
            }
        }
        out
    }

    /// The pair's link went: a stream on it is over, with no stop to send.
    pub fn link_gone(&mut self, mac: Address) {
        if self.streaming() == Some(mac) {
            self.state = State::Idle;
        }
    }

    /// The card switch after the stop is done, however it went.
    pub fn switched(&mut self) {
        if matches!(self.state, State::Switching { .. }) {
            self.state = State::Idle;
        }
    }

    /// Turning Conversation Awareness off failed: there is nothing to turn
    /// on again.
    pub fn not_turned_off(&mut self, mac: Address) {
        if self.turned_off.is_some_and(|t| t.mac == mac) {
            self.turned_off = None;
        }
    }

    /// The pair whose microphone runs: started, and not yet stopped.
    pub fn streaming(&self) -> Option<Address> {
        match self.state {
            State::Running { mac } | State::Lingering { mac, .. } => Some(mac),
            State::Idle | State::Switching { .. } => None,
        }
    }

    /// When the stop is due, if it is.
    pub fn next_due(&self) -> Option<Instant> {
        match self.state {
            State::Lingering { until, .. } => Some(until),
            _ => None,
        }
    }

    /// Whether the card switch after a stop is under way.
    pub fn is_switching(&self) -> bool {
        matches!(self.state, State::Switching { .. })
    }
}

/// What [`DefaultInput`] asks to be done to the default input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DefaultChange {
    /// Make it the source.
    Source,
    /// Put back the one before: the node it named, or none.
    PutBack(Option<String>),
}

/// What making the source the default input remembers.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Made {
    /// The default input before: the node it named, or none.
    before: Option<String>,
    /// Whether the default input has read the source since.
    applied: bool,
}

/// The default input while the source is up: made the source, and the one
/// before put back after - unless the user chose another meanwhile, whose
/// choice wins and stays.
#[derive(Debug, Clone, Default)]
pub struct DefaultInput {
    made: Option<Made>,
    /// The user moved the default input off the source: it is not made the
    /// source again until the source has gone and come back.
    held_off: bool,
}

impl DefaultInput {
    /// What the default input reading `current` (the node it names) asks
    /// for, with the source up or not.
    ///
    /// - Up, and not made yet: make it the source, remembering `current`.
    /// - Read the source once, and then another: the user's choice. Nothing
    ///   is put back, and the source is not made the default input again
    ///   while it stays up.
    /// - Down, made: put back what was before.
    pub fn update(&mut self, up: bool, current: Option<&str>) -> Option<DefaultChange> {
        if let Some(made) = &mut self.made {
            if current == Some(NODE_NAME) {
                made.applied = true;
            } else if made.applied {
                self.made = None;
                self.held_off = up;
                return None;
            }
        }
        match (up, self.made.take()) {
            (true, Some(made)) => {
                self.made = Some(made);
                None
            }
            (true, None) if self.held_off => None,
            (true, None) => {
                self.made = Some(Made {
                    before: current.filter(|c| *c != NODE_NAME).map(String::from),
                    applied: current == Some(NODE_NAME),
                });
                Some(DefaultChange::Source)
            }
            (false, made) => {
                self.held_off = false;
                made.map(|made| DefaultChange::PutBack(made.before))
            }
        }
    }

    /// Whether the source was made the default input, and not yet put back.
    pub fn is_made(&self) -> bool {
        self.made.is_some()
    }
}

/// The pair the source is for in `snapshot`: the first attached pair with a
/// link, in the order the links came up.
pub fn source_pair(snapshot: &Snapshot) -> Option<Address> {
    snapshot
        .connected_macs
        .iter()
        .copied()
        .find(|mac| snapshot.attached.contains(mac))
}

/// Every pair with a link in `snapshot`, with its Conversation Awareness.
fn links(snapshot: &Snapshot) -> HashMap<Address, Option<bool>> {
    snapshot
        .connected_macs
        .iter()
        .map(|&mac| {
            let setting = snapshot
                .states
                .get(&mac)
                .and_then(|state| state.conversation_awareness);
            (mac, setting)
        })
        .collect()
}

/// What [`MicControl`] asks of [`Mic::run`].
enum ControlRequest {
    /// Say, once every event told before is taken, and what it brought is
    /// asked of the audio side, whether the stream is idle.
    Settled(oneshot::Sender<bool>),
    /// Stop the stream, put Conversation Awareness and the default input
    /// back, take the source down, and say when done.
    Quit(oneshot::Sender<()>),
}

/// What the task's own work tells it.
enum Internal {
    /// The card switch after a stop is done.
    Switched,
    /// Turning Conversation Awareness off failed.
    NotTurnedOff(Address),
}

/// The buses the card switch calls on, each connected when first needed,
/// and again after a connection failed.
struct Buses {
    bluez: tokio::sync::Mutex<Bus>,
    session: tokio::sync::Mutex<Bus>,
}

/// Asks [`Mic::run`] how it stands, and to put everything back at the quit.
#[derive(Clone)]
pub struct MicControl {
    requests: mpsc::UnboundedSender<ControlRequest>,
    availability: watch::Receiver<Availability>,
}

impl MicControl {
    /// Whether the stream is idle - not started, not switching the card -
    /// once every event told before this call has been taken, and what it
    /// brought asked of the audio side and of the pods: for a test to see
    /// what came of an event. `false` once the task is gone.
    pub async fn settled(&self) -> bool {
        let (done, answer) = oneshot::channel();
        if self.requests.send(ControlRequest::Settled(done)).is_err() {
            return false;
        }
        answer.await.unwrap_or(false)
    }

    /// Stops a stream that runs, puts Conversation Awareness and the default
    /// input back, and takes the source down: for the quit, which bounds it
    /// with [`QUIT_LIMIT`]. No card switch: it would hold the quit up.
    pub async fn shut_down(&self) {
        let (done, answer) = oneshot::channel();
        if self.requests.send(ControlRequest::Quit(done)).is_ok() {
            let _ = answer.await;
        }
    }

    /// Whether the microphone can be offered, as it changes.
    pub fn availability(&self) -> watch::Receiver<Availability> {
        self.availability.clone()
    }
}

/// Offers a pair's microphone as an input while the switch is on. Its task
/// is [`Mic::run`].
pub struct Mic {
    coordinator: Arc<Coordinator>,
    snapshots: watch::Receiver<Snapshot>,
    events: mpsc::UnboundedReceiver<MicEvent>,
    audio_in: mpsc::Receiver<MicAudio>,
    switch: watch::Receiver<bool>,
    availability: watch::Sender<Availability>,
    /// Why the microphone cannot be offered whatever PipeWire says: the
    /// build has no support, or no decoder loads. `None` when it can.
    missing: Option<String>,
    start_audio: StartAudio,
    requests: mpsc::UnboundedReceiver<ControlRequest>,
    internal: (
        mpsc::UnboundedSender<Internal>,
        mpsc::UnboundedReceiver<Internal>,
    ),
    /// The buses the card switch calls on: BlueZ's, the players'.
    buses: Arc<Buses>,
    limit: Duration,
    /// [`RESUME_WAIT`] but in a test, which may ask for less.
    resume_wait: Duration,
    /// The audio side, while it runs, and what it tells.
    audio: Option<Arc<dyn AudioSide>>,
    audio_events: mpsc::UnboundedReceiver<AudioEvent>,
    audio_events_tx: mpsc::UnboundedSender<AudioEvent>,
    /// Whether the audio side has said it is ready.
    ready: bool,
    /// When to start the audio side again, after it ended.
    restart_at: Option<tokio::time::Instant>,
    backoff: Backoff,
    started_at: Option<Instant>,
    /// What the audio side last said.
    recording: bool,
    default: Option<String>,
    source_up: bool,
    stream: Stream,
    default_input: DefaultInput,
    dropped_at_start: u64,
    /// Set once everything was put back for the quit: nothing is started
    /// again.
    quitting: bool,
}

impl Mic {
    /// Offers `coordinator`'s pairs' microphone while `switch` is on,
    /// through the audio side `start_audio` starts, unless `missing` says
    /// why it cannot be; the card switch reads the transport on the bus
    /// `bluez` connects to, and the players on the bus `session` connects
    /// to.
    pub fn new(
        coordinator: Arc<Coordinator>,
        switch: watch::Receiver<bool>,
        missing: Option<String>,
        start_audio: StartAudio,
        bluez: Connect,
        session: Connect,
    ) -> (Self, MicControl) {
        let (requests_tx, requests) = mpsc::unbounded_channel();
        let (audio_events_tx, audio_events) = mpsc::unbounded_channel();
        let initial = match &missing {
            Some(reason) => Availability::Unavailable(reason.clone()),
            None => Availability::Unknown,
        };
        let (availability, availability_rx) = watch::channel(initial);
        let mic = Self {
            snapshots: coordinator.subscribe(),
            events: coordinator.mic_events(),
            audio_in: coordinator.mic_audio(AUDIO_QUEUE),
            coordinator,
            switch,
            availability,
            missing,
            start_audio,
            requests,
            internal: mpsc::unbounded_channel(),
            buses: Arc::new(Buses {
                bluez: tokio::sync::Mutex::new(Bus::new("BlueZ", bluez)),
                session: tokio::sync::Mutex::new(Bus::new("the media players", session)),
            }),
            limit: media::CALL_LIMIT,
            resume_wait: RESUME_WAIT,
            audio: None,
            audio_events,
            audio_events_tx,
            ready: false,
            restart_at: None,
            backoff: Backoff::default(),
            started_at: None,
            recording: false,
            default: None,
            source_up: false,
            stream: Stream::default(),
            default_input: DefaultInput::default(),
            dropped_at_start: 0,
            quitting: false,
        };
        (
            mic,
            MicControl {
                requests: requests_tx,
                availability: availability_rx,
            },
        )
    }

    /// Gives each BlueZ and MPRIS call `limit` in place of
    /// [`media::CALL_LIMIT`]: for a test on a loaded machine.
    #[cfg(feature = "harness")]
    pub fn with_call_limit(mut self, limit: Duration) -> Self {
        self.limit = limit;
        self
    }

    /// Stops the stream `stop_after` after the last recording, and waits
    /// `resume_wait` at each step of the card switch, in place of
    /// [`STOP_AFTER`] and [`RESUME_WAIT`]: for a test, which need not wait
    /// seconds for what it does not watch.
    #[cfg(feature = "harness")]
    pub fn with_waits(mut self, stop_after: Duration, resume_wait: Duration) -> Self {
        self.stream.stop_after = stop_after;
        self.resume_wait = resume_wait;
        self
    }

    /// Runs until the coordinator goes. The audio side is started while the
    /// switch is on, and again after it ended, waiting longer each time.
    ///
    /// Events come first, then audio, what the audio side tells, the
    /// snapshots, the switch and the task's own work, then the timers, and
    /// requests last: [`MicControl::settled`] answers for every event told
    /// before it.
    pub async fn run(mut self) {
        let mut snapshots_open = true;
        let mut switch_open = true;
        let mut requests_open = true;
        loop {
            self.settle(Instant::now());
            let due = self.stream.next_due().map(tokio::time::Instant::from_std);
            let restart = self.restart_at;
            tokio::select! {
                biased;
                event = self.events.recv() => match event {
                    Some(event) => self.handle(event),
                    None => return,
                },
                Some(audio) = self.audio_in.recv() => {
                    if self.stream.streaming() == Some(audio.mac)
                        && let Some(side) = &self.audio
                    {
                        side.ask(AudioRequest::Audio(audio.packet));
                    }
                }
                Some(event) = self.audio_events.recv() => self.heard(event),
                changed = self.snapshots.changed(), if snapshots_open => {
                    if changed.is_err() {
                        snapshots_open = false;
                    }
                }
                changed = self.switch.changed(), if switch_open => {
                    if changed.is_err() {
                        switch_open = false;
                    }
                }
                Some(internal) = self.internal.1.recv() => match internal {
                    Internal::Switched => self.stream.switched(),
                    Internal::NotTurnedOff(mac) => self.stream.not_turned_off(mac),
                },
                () = sleep_until(due), if due.is_some() => {}
                () = sleep_until(restart), if restart.is_some() => {
                    self.restart_at = None;
                }
                request = self.requests.recv(), if requests_open => match request {
                    Some(ControlRequest::Settled(answer)) => {
                        self.settle(Instant::now());
                        self.sync_audio().await;
                        let idle = self.stream.streaming().is_none()
                            && !self.stream.is_switching();
                        let _ = answer.send(idle);
                    }
                    Some(ControlRequest::Quit(done)) => {
                        self.shut_down().await;
                        let _ = done.send(());
                    }
                    None => requests_open = false,
                },
            }
        }
    }

    fn handle(&mut self, event: MicEvent) {
        match event {
            MicEvent::LinkUp { .. } => {}
            MicEvent::Answer { mac, answer } => {
                tracing::debug!("the microphone of {mac} answered {answer:?}");
            }
            MicEvent::LinkGone { mac } => {
                if self.stream.streaming() == Some(mac) {
                    tracing::info!("The microphone of {mac} stopped: its link went");
                }
                self.stream.link_gone(mac);
            }
        }
    }

    fn heard(&mut self, event: AudioEvent) {
        match event {
            AudioEvent::Ready { default } => {
                self.ready = true;
                self.default = default;
                self.backoff = Backoff::default();
                self.availability.send_replace(Availability::Available);
            }
            AudioEvent::Default(default) => self.default = default,
            AudioEvent::Recording(recording) => {
                if recording != self.recording {
                    tracing::info!(
                        "The microphone's source is {}",
                        if recording {
                            "recorded from"
                        } else {
                            "no longer recorded from"
                        }
                    );
                }
                self.recording = recording;
            }
            AudioEvent::Gone(reason) => {
                tracing::warn!("The microphone's audio side ended: {reason}");
                let ran = self.started_at.take().map(|at| at.elapsed());
                let wait = match ran {
                    Some(ran) if self.ready => self.backoff.ended(ran),
                    _ => self.backoff.failed(),
                };
                self.availability
                    .send_replace(Availability::Unavailable(format!(
                        "PipeWire could not be reached: {reason}"
                    )));
                self.audio = None;
                self.ready = false;
                self.recording = false;
                self.source_up = false;
                self.restart_at = Some(tokio::time::Instant::now() + wait);
            }
        }
    }

    /// Whether the switch is on.
    fn on(&self) -> bool {
        *self.switch.borrow()
    }

    /// Brings everything in line with what the task knows now.
    fn settle(&mut self, now: Instant) {
        if self.quitting {
            return;
        }
        let on = self.on();
        // The audio side runs while the switch is on, for the source and
        // for whether anything records.
        if on && self.missing.is_none() && self.audio.is_none() && self.restart_at.is_none() {
            match (self.start_audio)(self.audio_events_tx.clone()) {
                Ok(side) => {
                    self.started_at = Some(now);
                    self.audio = Some(side);
                }
                Err(reason) => {
                    tracing::warn!("The microphone cannot be offered: {reason}");
                    self.availability
                        .send_replace(Availability::Unavailable(reason.clone()));
                    self.missing = Some(reason);
                }
            }
        }
        // Cloned out: the borrow is a read lock every broadcast waits for.
        let snapshot = self.snapshots.borrow().clone();
        let links = links(&snapshot);
        let pair = source_pair(&snapshot).filter(|_| on && self.ready);

        // The source is up while there is a pair for it.
        let up = pair.is_some();
        if up != self.source_up
            && let Some(side) = &self.audio
        {
            side.ask(AudioRequest::Source(up));
            self.source_up = up;
            if !up {
                self.recording = false;
            }
        }

        let actions = self.stream.update(
            View {
                pair,
                recording: self.recording && self.source_up,
                links: &links,
            },
            now,
        );
        for action in actions {
            self.carry_out(action);
        }

        if self.ready
            && let Some(change) = self.default_input.update(up, self.default.as_deref())
        {
            self.change_default(change);
        }

        // Once the switch is off, and nothing is left to put back, the
        // audio side ends.
        if !on
            && !self.source_up
            && !self.default_input.is_made()
            && !self.stream.is_switching()
            && let Some(side) = self.audio.take()
        {
            side.ask(AudioRequest::Quit);
            self.ready = false;
            self.restart_at = None;
            if self.missing.is_none() {
                self.availability.send_replace(Availability::Unknown);
            }
        }
    }

    fn change_default(&mut self, change: DefaultChange) {
        let Some(side) = &self.audio else {
            return;
        };
        match change {
            DefaultChange::Source => {
                tracing::info!(
                    "The default input is now the microphone's source, in place of {}",
                    self.default.as_deref().unwrap_or("none")
                );
                side.ask(AudioRequest::SetDefault(Some(NODE_NAME.into())));
            }
            DefaultChange::PutBack(before) => {
                tracing::info!(
                    "The default input put back to {}",
                    before.as_deref().unwrap_or("none")
                );
                side.ask(AudioRequest::SetDefault(before));
            }
        }
    }

    fn carry_out(&mut self, action: Do) {
        match action {
            Do::ConversationAwareness { mac, on } => {
                tracing::info!(
                    "Turning Conversation Awareness of {mac} {} for the microphone",
                    if on { "on again" } else { "off" }
                );
                let done = self
                    .coordinator
                    .command(mac, Command::ConversationAwareness(on));
                let internal = self.internal.0.clone();
                tokio::spawn(async move {
                    if let Err(e) = done.await {
                        tracing::warn!("{e:#}");
                        if !on {
                            let _ = internal.send(Internal::NotTurnedOff(mac));
                        }
                    }
                });
            }
            Do::Start(mac) => {
                tracing::info!("Starting the microphone of {mac}: something records");
                self.dropped_at_start = self.coordinator.mic_audio_dropped();
                let done = self.coordinator.command(mac, Command::Microphone(true));
                tokio::spawn(async move {
                    if let Err(e) = done.await {
                        tracing::warn!("{e:#}");
                    }
                });
            }
            Do::Stop(mac) => {
                let dropped = self.coordinator.mic_audio_dropped() - self.dropped_at_start;
                tracing::info!(
                    "Stopping the microphone of {mac}; {dropped} audio packet(s) dropped, \
                     this side behind"
                );
                let done = self.coordinator.command(mac, Command::Microphone(false));
                let internal = self.internal.0.clone();
                let switch = self.card_switch(mac);
                tokio::spawn(async move {
                    // After the stop, whether it was sent or not: a stop
                    // that failed may still have reached the pods.
                    if let Err(e) = done.await {
                        tracing::warn!("{e:#}");
                    }
                    if let Some(switch) = switch {
                        switch.await;
                    }
                    let _ = internal.send(Internal::Switched);
                });
            }
        }
    }

    /// The card switch after a stop: `None` without an audio side to ask.
    fn card_switch(&self, mac: Address) -> Option<impl Future<Output = ()> + Send + use<>> {
        let side = self.audio.clone()?;
        let (buses, limit, wait) = (self.buses.clone(), self.limit, self.resume_wait);
        Some(async move {
            // One switch at a time: a stop comes only after the last
            // switch is done.
            let mut bluez = buses.bluez.lock().await;
            let mut session = buses.session.lock().await;
            match switch_card(mac, &*side, &mut bluez, &mut session, limit, wait).await {
                Ok(()) => tracing::info!("The card of {mac} is back, and plays"),
                Err(e) => tracing::warn!("The card switch of {mac} after the stop: {e:#}"),
            }
        })
    }

    /// Asks the audio side to answer once everything asked before is
    /// through.
    async fn sync_audio(&self) {
        let Some(side) = &self.audio else {
            return;
        };
        let (done, answer) = oneshot::channel();
        if side.ask(AudioRequest::Sync(done)) {
            let _ = answer.await;
        }
    }

    /// Stops a stream that runs, puts Conversation Awareness and the default
    /// input back, and takes the source down - see [`MicControl::shut_down`].
    async fn shut_down(&mut self) {
        self.quitting = true;
        let mut waits = Vec::new();
        if let Some(mac) = self.stream.streaming() {
            tracing::info!("Stopping the microphone of {mac}: the app quits");
            waits.push(self.coordinator.command(mac, Command::Microphone(false)));
            if let Some(turned_off) = self.stream.turned_off.take() {
                waits.push(
                    self.coordinator
                        .command(turned_off.mac, Command::ConversationAwareness(true)),
                );
            }
        }
        if let Some(DefaultChange::PutBack(before)) =
            self.default_input.update(false, self.default.as_deref())
        {
            self.change_default(DefaultChange::PutBack(before));
        }
        if let Some(side) = &self.audio {
            side.ask(AudioRequest::Source(false));
        }
        for wait in waits {
            if let Err(e) = wait.await {
                tracing::warn!("{e:#}");
            }
        }
        self.sync_audio().await;
    }
}

/// `sleep_until(at)`, or never.
async fn sleep_until(at: Option<tokio::time::Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// The path of the pair's A2DP transport, with whether it is `active`, read
/// within `limit`.
async fn a2dp(
    system: &Connection,
    mac: Address,
    limit: Duration,
) -> Result<Option<(String, bool)>> {
    let objects = tokio::time::timeout(limit, volume::bluez_objects(system))
        .await
        .with_context(|| format!("BlueZ did not answer within {limit:?}"))??;
    Ok(volume::a2dp_transport_in(&objects, mac).map(|(path, active)| (path.to_string(), active)))
}

/// Calls `check` every [`RESUME_POLL`] until it says yes, or `wait` from
/// `from` has passed; says which.
async fn wait_for<F: Future<Output = bool>>(
    from: tokio::time::Instant,
    wait: Duration,
    mut check: impl FnMut() -> F,
) -> bool {
    loop {
        if check().await {
            return true;
        }
        if tokio::time::Instant::now() >= from + wait {
            return false;
        }
        tokio::time::sleep(RESUME_POLL).await;
    }
}

/// Switches the pair's card off and back, waits for a new A2DP transport,
/// and plays again every player the switch paused; with one playing, waits
/// for the pair's A2DP to play too, and switches again if it does not, up
/// to [`SWITCH_TRIES`] switches. A read that fails counts as not yet.
async fn switch_card(
    mac: Address,
    side: &dyn AudioSide,
    bluez: &mut Bus,
    session: &mut Bus,
    limit: Duration,
    resume_wait: Duration,
) -> Result<()> {
    // The players playing before, each to be played again if the switch
    // pauses it. Without them, the switch goes on all the same.
    let players = match session.connection(limit).await {
        Ok(conn) => {
            let listed = media::list_players(&conn, limit).await;
            session.check(&listed);
            listed.map(|players| (conn, players))
        }
        Err(e) => Err(e),
    };
    let (session_conn, playing) = match players {
        Ok((conn, players)) => {
            let playing: Vec<Player> = players
                .into_iter()
                .filter(|player| player.status == Playback::Playing)
                .collect();
            (Some(conn), playing)
        }
        Err(e) => {
            tracing::warn!("no player will be played again after the card switch: {e:#}");
            (None, Vec::new())
        }
    };
    let system = bluez.connection(limit).await?;
    for attempt in 1..=SWITCH_TRIES {
        let before = a2dp(&system, mac, limit).await;
        bluez.check(&before);
        let before = before.ok().flatten().map(|(path, _)| path);
        let (done, switched) = oneshot::channel();
        anyhow::ensure!(
            side.ask(AudioRequest::SwitchCard { mac, done }),
            "the audio side has ended"
        );
        switched
            .await
            .context("the audio side ended during the switch")??;
        let switched = tokio::time::Instant::now();
        // Whether playing before A2DP is set up again leaves both pods
        // silent is open (protocol/aap/microphone.md, Open questions):
        // a new transport first - one that went and came back, or another.
        let mut went = before.is_none();
        let mut new = false;
        while !new {
            match a2dp(&system, mac, limit).await {
                Ok(Some((path, _))) => new = went || Some(&path) != before.as_ref(),
                Ok(None) => went = true,
                Err(_) => {}
            }
            if new || tokio::time::Instant::now() >= switched + resume_wait {
                break;
            }
            tokio::time::sleep(RESUME_POLL).await;
        }
        tracing::info!(
            "Card switch {attempt} of {mac}: {}",
            if new {
                "a new A2DP transport"
            } else {
                "no new A2DP transport in time"
            }
        );
        let Some(conn) = &session_conn else {
            return Ok(());
        };
        if playing.is_empty() {
            return Ok(());
        }
        futures_util::future::join_all(
            playing
                .iter()
                .map(|player| play_again(conn, player, switched, limit, resume_wait)),
        )
        .await;
        let played = tokio::time::Instant::now();
        let plays = wait_for(played, resume_wait, || async {
            matches!(a2dp(&system, mac, limit).await, Ok(Some((_, true))))
        })
        .await;
        if plays {
            return Ok(());
        }
        tracing::info!("A2DP of {mac} does not play after card switch {attempt}");
    }
    anyhow::bail!("A2DP did not play after {SWITCH_TRIES} card switches")
}

/// Waits up to [`RESUME_WAIT`] from `switched` for `player`, playing before
/// the switch, to report the pause the switch causes, and plays it again
/// only then: played before it reported the pause, a player stayed paused
/// (protocol/aap/microphone.md, Run 3). One still playing, stopped or gone
/// is left as it is.
async fn play_again(
    conn: &Connection,
    player: &Player,
    switched: tokio::time::Instant,
    limit: Duration,
    resume_wait: Duration,
) {
    let status = loop {
        let read = media::list_players(conn, limit).await.ok();
        let status = read.and_then(|players| {
            players
                .into_iter()
                .find(|p| p.id == player.id)
                .map(|p| p.status)
        });
        if status != Some(Playback::Playing)
            || tokio::time::Instant::now() >= switched + resume_wait
        {
            break status;
        }
        tokio::time::sleep(RESUME_POLL).await;
    };
    if status == Some(Playback::Paused) {
        match media::call(conn, player, "Play", limit).await {
            Ok(()) => tracing::info!("Played {} again after the card switch", player.name),
            Err(e) => tracing::warn!("{e:#}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAIR: Address = Address::new([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);
    const OTHER: Address = Address::new([0x02, 0x00, 0x00, 0x00, 0x00, 0x02]);

    /// The pairs with a link, each with its Conversation Awareness.
    fn links(pairs: &[(Address, Option<bool>)]) -> HashMap<Address, Option<bool>> {
        pairs.iter().copied().collect()
    }

    fn view(
        pair: Option<Address>,
        recording: bool,
        links: &HashMap<Address, Option<bool>>,
    ) -> View<'_> {
        View {
            pair,
            recording,
            links,
        }
    }

    /// A recording starts the stream; once it ends, the stream stops
    /// `STOP_AFTER` later, not before, and the card switch follows; the
    /// next recording waits for the switch to be done.
    #[test]
    fn starts_with_a_recording_and_stops_after_the_last() {
        let mut stream = Stream::default();
        let t0 = Instant::now();
        let linked = links(&[(PAIR, Some(false))]);

        assert_eq!(stream.update(view(Some(PAIR), false, &linked), t0), []);
        assert_eq!(
            stream.update(view(Some(PAIR), true, &linked), t0),
            [Do::Start(PAIR)]
        );
        assert_eq!(stream.streaming(), Some(PAIR));

        let ended = t0 + Duration::from_secs(10);
        assert_eq!(stream.update(view(Some(PAIR), false, &linked), ended), []);
        assert_eq!(stream.next_due(), Some(ended + STOP_AFTER));
        let almost = ended + STOP_AFTER - Duration::from_millis(1);
        assert_eq!(stream.update(view(Some(PAIR), false, &linked), almost), []);
        assert_eq!(
            stream.update(view(Some(PAIR), false, &linked), ended + STOP_AFTER),
            [Do::Stop(PAIR)]
        );
        assert_eq!(stream.streaming(), None);
        assert!(stream.is_switching());

        let later = ended + STOP_AFTER + Duration::from_secs(1);
        assert_eq!(stream.update(view(Some(PAIR), true, &linked), later), []);
        stream.switched();
        assert_eq!(
            stream.update(view(Some(PAIR), true, &linked), later),
            [Do::Start(PAIR)]
        );
    }

    /// A recording back within `STOP_AFTER` keeps the stream: no stop, no
    /// second start.
    #[test]
    fn a_recording_back_in_time_keeps_the_stream() {
        let mut stream = Stream::default();
        let t0 = Instant::now();
        let linked = links(&[(PAIR, None)]);
        stream.update(view(Some(PAIR), true, &linked), t0);
        stream.update(view(Some(PAIR), false, &linked), t0);
        let back = t0 + STOP_AFTER / 2;
        assert_eq!(stream.update(view(Some(PAIR), true, &linked), back), []);
        assert_eq!(stream.next_due(), None);
        assert_eq!(
            stream.update(view(Some(PAIR), true, &linked), back + STOP_AFTER),
            []
        );
        assert_eq!(stream.streaming(), Some(PAIR));
    }

    /// Nothing starts without a pair for the source, or for a pair with no
    /// link.
    #[test]
    fn no_pair_no_stream() {
        let mut stream = Stream::default();
        let now = Instant::now();
        let none = links(&[]);
        assert_eq!(stream.update(view(None, true, &none), now), []);
        assert_eq!(stream.update(view(Some(PAIR), true, &none), now), []);
        assert_eq!(stream.streaming(), None);
    }

    /// The switch turned off, or the pair detached, with its link still up,
    /// stops the stream at once, and switches the card; the link gone ends
    /// it with nothing to send and no switch.
    #[test]
    fn the_pair_gone_from_the_source_stops_the_stream() {
        let now = Instant::now();
        let linked = links(&[(PAIR, None)]);
        let mut stream = Stream::default();
        stream.update(view(Some(PAIR), true, &linked), now);
        assert_eq!(
            stream.update(view(None, true, &linked), now),
            [Do::Stop(PAIR)]
        );
        assert!(stream.is_switching());

        let mut stream = Stream::default();
        stream.update(view(Some(PAIR), true, &linked), now);
        stream.update(view(Some(PAIR), false, &linked), now);
        assert_eq!(
            stream.update(view(Some(OTHER), false, &linked), now),
            [Do::Stop(PAIR)]
        );

        let mut stream = Stream::default();
        stream.update(view(Some(PAIR), true, &linked), now);
        stream.link_gone(PAIR);
        assert_eq!(stream.streaming(), None);
        assert!(!stream.is_switching());
        let gone = links(&[]);
        assert_eq!(stream.update(view(None, true, &gone), now), []);
    }

    /// Another pair's link going leaves the stream.
    #[test]
    fn another_link_going_leaves_the_stream() {
        let now = Instant::now();
        let linked = links(&[(PAIR, None), (OTHER, None)]);
        let mut stream = Stream::default();
        stream.update(view(Some(PAIR), true, &linked), now);
        stream.link_gone(OTHER);
        assert_eq!(stream.streaming(), Some(PAIR));
    }

    /// On at the start: off before the start, and on again with the stop,
    /// once the link has read it off.
    #[test]
    fn conversation_awareness_goes_off_for_the_stream_and_back_on() {
        let mut stream = Stream::default();
        let t0 = Instant::now();
        let on = links(&[(PAIR, Some(true))]);
        let off = links(&[(PAIR, Some(false))]);
        assert_eq!(
            stream.update(view(Some(PAIR), true, &on), t0),
            [
                Do::ConversationAwareness {
                    mac: PAIR,
                    on: false
                },
                Do::Start(PAIR),
            ]
        );
        // Still on, as the command has not landed: not the user's.
        assert_eq!(stream.update(view(Some(PAIR), true, &on), t0), []);
        assert_eq!(stream.update(view(Some(PAIR), true, &off), t0), []);
        stream.update(view(Some(PAIR), false, &off), t0);
        assert_eq!(
            stream.update(view(Some(PAIR), false, &off), t0 + STOP_AFTER),
            [
                Do::Stop(PAIR),
                Do::ConversationAwareness {
                    mac: PAIR,
                    on: true
                },
            ]
        );
        // Once only.
        stream.switched();
        assert_eq!(
            stream.update(view(Some(PAIR), false, &off), t0 + STOP_AFTER),
            []
        );
    }

    /// Off, or unknown, at the start: left alone throughout.
    #[test]
    fn conversation_awareness_off_or_unknown_is_left_alone() {
        for setting in [Some(false), None] {
            let mut stream = Stream::default();
            let t0 = Instant::now();
            let linked = links(&[(PAIR, setting)]);
            assert_eq!(
                stream.update(view(Some(PAIR), true, &linked), t0),
                [Do::Start(PAIR)]
            );
            stream.update(view(Some(PAIR), false, &linked), t0);
            assert_eq!(
                stream.update(view(Some(PAIR), false, &linked), t0 + STOP_AFTER),
                [Do::Stop(PAIR)]
            );
        }
    }

    /// Turned on again by someone else while the stream ran: left as it
    /// is after the stop.
    #[test]
    fn conversation_awareness_turned_on_meanwhile_is_the_users() {
        let mut stream = Stream::default();
        let t0 = Instant::now();
        let on = links(&[(PAIR, Some(true))]);
        let off = links(&[(PAIR, Some(false))]);
        stream.update(view(Some(PAIR), true, &on), t0);
        stream.update(view(Some(PAIR), true, &off), t0);
        stream.update(view(Some(PAIR), true, &on), t0);
        stream.update(view(Some(PAIR), false, &on), t0);
        assert_eq!(
            stream.update(view(Some(PAIR), false, &on), t0 + STOP_AFTER),
            [Do::Stop(PAIR)]
        );
    }

    /// Turned off for a stream whose link went: on again once a new link
    /// reads it off; and a new stream before then sends nothing but the
    /// start, as it is off already.
    #[test]
    fn conversation_awareness_goes_back_on_over_the_next_link() {
        let t0 = Instant::now();
        let on = links(&[(PAIR, Some(true))]);
        let off = links(&[(PAIR, Some(false))]);
        let none = links(&[]);
        let unread = links(&[(PAIR, None)]);

        let mut stream = Stream::default();
        stream.update(view(Some(PAIR), true, &on), t0);
        stream.update(view(Some(PAIR), true, &off), t0);
        stream.link_gone(PAIR);
        assert_eq!(stream.update(view(None, false, &none), t0), []);
        assert_eq!(stream.update(view(Some(PAIR), false, &unread), t0), []);
        assert_eq!(
            stream.update(view(Some(PAIR), false, &off), t0),
            [Do::ConversationAwareness {
                mac: PAIR,
                on: true
            }]
        );

        let mut stream = Stream::default();
        stream.update(view(Some(PAIR), true, &on), t0);
        stream.update(view(Some(PAIR), true, &off), t0);
        stream.link_gone(PAIR);
        assert_eq!(
            stream.update(view(Some(PAIR), true, &unread), t0),
            [Do::Start(PAIR)]
        );
    }

    /// Turning it off failed: nothing is turned on after.
    #[test]
    fn conversation_awareness_not_turned_off_is_not_turned_on() {
        let mut stream = Stream::default();
        let t0 = Instant::now();
        let on = links(&[(PAIR, Some(true))]);
        let off = links(&[(PAIR, Some(false))]);
        stream.update(view(Some(PAIR), true, &on), t0);
        stream.not_turned_off(PAIR);
        stream.update(view(Some(PAIR), false, &off), t0);
        assert_eq!(
            stream.update(view(Some(PAIR), false, &off), t0 + STOP_AFTER),
            [Do::Stop(PAIR)]
        );
    }

    /// Up, the source is made the default input, the one before
    /// remembered; down, that one is put back - or none cleared.
    #[test]
    fn the_default_input_is_made_and_put_back() {
        let mut default = DefaultInput::default();
        assert_eq!(
            default.update(true, Some("alsa_input.mic")),
            Some(DefaultChange::Source)
        );
        // Not read back yet, then read back: nothing more.
        assert_eq!(default.update(true, Some("alsa_input.mic")), None);
        assert_eq!(default.update(true, Some(NODE_NAME)), None);
        assert_eq!(
            default.update(false, Some(NODE_NAME)),
            Some(DefaultChange::PutBack(Some("alsa_input.mic".into())))
        );
        assert!(!default.is_made());
        assert_eq!(default.update(false, Some("alsa_input.mic")), None);

        let mut default = DefaultInput::default();
        assert_eq!(default.update(true, None), Some(DefaultChange::Source));
        assert_eq!(
            default.update(false, None),
            Some(DefaultChange::PutBack(None))
        );
    }

    /// The user choosing another input while the source is up wins: nothing
    /// is put back, and the source is not made the default again until it
    /// has gone and come back.
    #[test]
    fn the_users_choice_wins() {
        let mut default = DefaultInput::default();
        default.update(true, Some("alsa_input.mic"));
        default.update(true, Some(NODE_NAME));
        assert_eq!(default.update(true, Some("usb_headset")), None);
        assert!(!default.is_made());
        assert_eq!(default.update(true, Some("usb_headset")), None);
        assert_eq!(default.update(false, Some("usb_headset")), None);
        assert_eq!(
            default.update(true, Some("usb_headset")),
            Some(DefaultChange::Source)
        );
    }

    /// A default left naming the source - a quit that did not put it back -
    /// is not remembered as the one before.
    #[test]
    fn the_source_is_never_the_one_before() {
        let mut default = DefaultInput::default();
        assert_eq!(
            default.update(true, Some(NODE_NAME)),
            Some(DefaultChange::Source)
        );
        assert_eq!(
            default.update(false, Some(NODE_NAME)),
            Some(DefaultChange::PutBack(None))
        );
    }
}
