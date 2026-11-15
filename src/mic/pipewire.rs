//! The microphone's PipeWire side: the source node apps record from, the
//! default input, and the pair's card switched off and back. On a thread of
//! its own, running PipeWire's loop, as PipeWire's objects never leave the
//! thread that made them; the task in `crate::mic` asks it things on a queue
//! whose pipe wakes PipeWire's loop, and hears from it on a tokio channel.
//!
//! - **The source** is a stream with `media.class` `Audio/Source`, named
//!   [`super::NODE_NAME`]: it plays the decoded samples at
//!   [`mic::SAMPLE_RATE`], one channel, and PipeWire converts them to what
//!   each app records. Its state tells whether anything records from it:
//!   `Streaming` while something does.
//! - **The default input** is the `default` metadata's
//!   `default.configured.audio.source`, the setting GNOME's Sound panel and
//!   `pactl set-default-source` change. Every change to it is told.
//! - **The card** is the pair's `bluez5` device, by `api.bluez5.address`: its
//!   `Profile` set to the profile named `off`, and once it reports that, back
//!   to the one it had.
//!
//! `docs/app/mic.md` says why each is done as it is.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::io::{Cursor, Read, Write};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use bluer::Address;
use pipewire as pw;
use pw::device::{Device, DeviceListener};
use pw::metadata::{Metadata, MetadataListener};
use pw::properties::properties;
use pw::spa;
use pw::spa::param::ParamType;
use pw::spa::pod::deserialize::PodDeserializer;
use pw::spa::pod::serialize::PodSerializer;
use pw::spa::pod::{Object, Pod, Property, PropertyFlags, Value};
use pw::spa::support::system::IoFlags;
use pw::stream::{StreamFlags, StreamListener, StreamRc, StreamState};
use pw::types::ObjectType;
use tokio::sync::{mpsc, oneshot};

use super::decoder::Decoder;
use super::{
    AudioEvent, AudioRequest, AudioSide, DEFAULT_SOURCE_KEY, NODE_DESCRIPTION, NODE_NAME,
    SWITCH_STEP_LIMIT,
};
use crate::aap::mic;

/// The samples kept waiting for the source at most: 0.2 s. Older ones go,
/// so a stream that fell behind catches up rather than lag for good.
const MAX_BUFFERED: usize = mic::SAMPLE_RATE as usize / 5;

/// The samples gathered before the source plays any, after it started or
/// ran dry: two audio packets' worth, so that packets that do not come
/// evenly do not run it dry at once (the spacing a guess:
/// `docs/protocol/aap/microphone.md`, Audio: `0x58 01 00`). A choice.
const PREFILL: usize = 2 * 4 * mic::FRAME_SAMPLES;

/// The audio packets waiting in the queue at most: 16, about 0.5 s of
/// audio at four frames of 480 samples a packet
/// (`docs/protocol/aap/microphone.md`). More than [`MAX_BUFFERED`] keeps once
/// decoded, so a thread that is only slow loses nothing the source would
/// play; one that hangs does not grow the queue without end. One more is dropped, and
/// the task counts it. A choice.
const MAX_QUEUED_AUDIO: usize = 16;

/// How often a card switch under way is looked at, for its limit.
const SWITCH_CHECK: Duration = Duration::from_millis(500);

/// What the task asks, waiting for the thread, until the thread ends. Not a
/// `pw::channel` (`docs/protocol/linux/desktop.md#pipewire`): the thread
/// takes the requests out before it serves them, so a decode never holds up
/// the task.
type Shared = Arc<Mutex<Option<Queue>>>;

struct Queue {
    requests: VecDeque<AudioRequest>,
    /// The audio packets among `requests`.
    audio: usize,
    /// A byte written as the queue stops being empty wakes the thread's
    /// loop; the thread reads it as it takes the queue.
    wake: std::io::PipeWriter,
}

impl Queue {
    /// Every request waiting, for the thread to serve.
    fn take(&mut self) -> VecDeque<AudioRequest> {
        self.audio = 0;
        std::mem::take(&mut self.requests)
    }
}

/// The thread's handle on the task's side.
struct Requests(Shared);

impl AudioSide for Requests {
    fn ask(&self, request: AudioRequest) -> bool {
        let mut shared = lock(&self.0);
        let Some(queue) = shared.as_mut() else {
            return false;
        };
        let audio = matches!(request, AudioRequest::Audio(_));
        if audio && queue.audio >= MAX_QUEUED_AUDIO {
            return false;
        }
        if queue.requests.is_empty() && queue.wake.write_all(&[1]).is_err() {
            return false;
        }
        queue.requests.push_back(request);
        queue.audio += usize::from(audio);
        true
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Marks the side ended as the thread ends, however it ends - a panic too -
/// and only then says so with [`AudioEvent::Gone`]. The queue taken, every
/// request still in it goes: each `oneshot` in one fails.
struct Ending {
    requests: Shared,
    events: mpsc::UnboundedSender<AudioEvent>,
}

impl Ending {
    /// Ends the side, the first time, saying `reason`.
    fn end(&self, reason: String) {
        let queue = lock(&self.requests).take();
        if queue.is_some() {
            drop(queue);
            let _ = self.events.send(AudioEvent::Gone(reason));
        }
    }
}

impl Drop for Ending {
    fn drop(&mut self) {
        self.end("PipeWire's thread failed".into());
    }
}

/// Starts the PipeWire side on a thread of its own, telling `events`. It
/// ends, with an [`AudioEvent::Gone`], when PipeWire cannot be reached or
/// goes, or when asked to with [`AudioRequest::Quit`].
pub fn start(events: mpsc::UnboundedSender<AudioEvent>) -> Box<dyn AudioSide> {
    start_at(None, events)
}

/// [`start`], connecting to the PipeWire at `remote` - a socket's path, or
/// its name in the runtime directory - in place of the user's, unless
/// `PIPEWIRE_REMOTE` names one, which wins
/// (`docs/protocol/linux/desktop.md#pipewire`).
pub fn start_at(
    remote: Option<String>,
    events: mpsc::UnboundedSender<AudioEvent>,
) -> Box<dyn AudioSide> {
    let (woken, wake) = match std::io::pipe() {
        Ok(pipe) => pipe,
        Err(e) => {
            let _ = events.send(AudioEvent::Gone(format!(
                "no pipe for PipeWire's thread: {e}"
            )));
            return Box::new(Requests(Arc::new(Mutex::new(None))));
        }
    };
    let requests: Shared = Arc::new(Mutex::new(Some(Queue {
        requests: VecDeque::new(),
        audio: 0,
        wake,
    })));
    let ending = Ending {
        requests: requests.clone(),
        events,
    };
    let taken = requests.clone();
    let spawned = std::thread::Builder::new()
        .name("cymba-pipewire".into())
        .spawn(move || {
            let served = serve(remote, taken, woken, &ending.events);
            ending.end(match served {
                Ok(()) => "the connection to PipeWire ended".to_string(),
                Err(e) => format!("{e:#}"),
            });
        });
    // Not spawned, the closure is dropped, and `Ending` with it.
    if let Err(e) = spawned {
        tracing::warn!("No thread for PipeWire: {e}");
    }
    Box::new(Requests(requests))
}

/// The samples on their way to the source.
#[derive(Default)]
struct Ring {
    samples: VecDeque<f32>,
    /// Whether the source plays from it: not until [`PREFILL`] samples are
    /// in, after it started or ran dry.
    primed: bool,
}

impl Ring {
    fn push(&mut self, samples: &[f32]) {
        self.samples.extend(samples);
        let over = self.samples.len().saturating_sub(MAX_BUFFERED);
        self.samples.drain(..over);
        if self.samples.len() >= PREFILL {
            self.primed = true;
        }
    }

    /// Fills `out` with what is waiting, silence for the rest.
    fn fill(&mut self, out: &mut [f32]) {
        let mut taken = 0;
        if self.primed {
            let n = out.len().min(self.samples.len());
            for (slot, sample) in out.iter_mut().zip(self.samples.drain(..n)) {
                *slot = sample;
                taken += 1;
            }
            if taken < out.len() {
                self.primed = false;
            }
        }
        out[taken..].fill(0.0);
    }

    fn clear(&mut self) {
        self.samples.clear();
        self.primed = false;
    }
}

/// The source, while it is up.
struct Source {
    // The listener goes first, so the stream's going tells nothing.
    _listener: StreamListener<()>,
    stream: StreamRc,
}

/// A `bluez5` device: a pair's card.
struct Card {
    address: String,
    device: Device,
    _listener: DeviceListener,
}

/// What a card reports of its profiles, kept as its params come.
#[derive(Default)]
struct Profiles {
    /// Its profile now, by index.
    current: Option<i32>,
    /// Each profile's name, by index.
    names: HashMap<i32, String>,
}

/// A switch of a card off and back, under way.
struct Switch {
    card: u32,
    off: i32,
    back: i32,
    /// Whether the card was set back already: the off was reported.
    returning: bool,
    /// When the step under way began.
    since: std::time::Instant,
    done: oneshot::Sender<Result<()>>,
}

/// Everything the thread keeps between callbacks. Each callback borrows it
/// only for itself, and never calls into PipeWire while borrowed, but for
/// requests PipeWire only queues.
#[derive(Default)]
struct State {
    source: Option<Source>,
    decoder: Option<Decoder>,
    /// Frames that did not decode since the source last started streaming.
    undecoded: u64,
    /// The `default` metadata, while it is there.
    metadata: Option<BoundMetadata>,
    /// The sync asked after the metadata was bound: once it is answered,
    /// every property the metadata had then has been read.
    metadata_sync: Option<i32>,
    /// Whether the metadata's properties have been read: until then, and
    /// without it, the default input is not known.
    metadata_read: bool,
    cards: HashMap<u32, Card>,
    profiles: Rc<RefCell<HashMap<u32, Profiles>>>,
    switch: Option<Switch>,
    /// Told once the first two round trips are done.
    ready: bool,
    /// The default input as last read.
    configured: Option<String>,
}

impl State {
    /// The default input as read, for [`AudioEvent::Ready`]: `None` until
    /// the metadata's properties are read.
    fn default_input(&self) -> Option<Option<String>> {
        self.metadata_read.then(|| self.configured.clone())
    }
}

/// The `default` metadata, bound.
struct BoundMetadata {
    /// Its global's ID, for its removal.
    id: u32,
    // The listener goes first, so the proxy's going tells nothing.
    _listener: MetadataListener,
    metadata: Metadata,
}

/// Connects to PipeWire and serves the task until the connection ends or
/// the task asks it to quit. Every reply still owed fails as it returns.
fn serve(
    remote: Option<String>,
    requests: Shared,
    woken: std::io::PipeReader,
    events: &mpsc::UnboundedSender<AudioEvent>,
) -> Result<()> {
    pw::init();
    let main_loop = pw::main_loop::MainLoopRc::new(None).context("no PipeWire loop")?;
    let context = pw::context::ContextRc::new(&main_loop, None).context("no PipeWire context")?;
    let core = context
        .connect_rc(remote.map(|remote| properties! { *pw::keys::REMOTE_NAME => remote }))
        .context("PipeWire could not be reached")?;
    let registry = core.get_registry_rc().context("no PipeWire registry")?;
    let state = Rc::new(RefCell::new(State::default()));
    let ring = Rc::new(RefCell::new(Ring::default()));

    // The connection gone ends the loop, and the thread with it.
    let first_sync = Rc::new(std::cell::Cell::new(None));
    let second_sync = Rc::new(std::cell::Cell::new(None));
    let _core_listener = {
        let quit = main_loop.downgrade();
        let (state, events, core_weak) = (state.clone(), events.clone(), core.downgrade());
        let (first, second) = (first_sync.clone(), second_sync.clone());
        core.add_listener_local()
            .error(move |id, _seq, res, message| {
                tracing::warn!("PipeWire error on object {id}: {message} ({res})");
                if id == pw::sys::PW_ID_CORE
                    && let Some(main_loop) = quit.upgrade()
                {
                    main_loop.quit();
                }
            })
            .done(move |id, seq| {
                if id != pw::sys::PW_ID_CORE {
                    return;
                }
                // Every global announced, then every property of the
                // metadata bound meanwhile: ready. The metadata's
                // properties read, whenever it came: told once ready.
                if state.borrow().metadata_sync == Some(seq.seq()) {
                    let mut state = state.borrow_mut();
                    state.metadata_sync = None;
                    state.metadata_read = true;
                    if state.ready {
                        let _ = events.send(AudioEvent::Default(state.default_input()));
                    }
                } else if first.get() == Some(seq.seq()) {
                    first.set(None);
                    if let Some(core) = core_weak.upgrade() {
                        second.set(core.sync(0).ok().map(|s| s.seq()));
                    }
                } else if second.get() == Some(seq.seq()) {
                    second.set(None);
                    let mut state = state.borrow_mut();
                    state.ready = true;
                    let _ = events.send(AudioEvent::Ready {
                        default: state.default_input(),
                    });
                } else if let Some(waiting) =
                    PENDING_SYNCS.with(|p| p.borrow_mut().remove(&seq.seq()))
                {
                    let _ = waiting.send(());
                }
            })
            .register()
    };

    let _registry_listener = {
        let registry_weak = registry.downgrade();
        let (state, events, core) = (state.clone(), events.clone(), core.clone());
        let (removed_state, removed_events) = (state.clone(), events.clone());
        registry
            .add_listener_local()
            .global(move |global| {
                let Some(registry) = registry_weak.upgrade() else {
                    return;
                };
                let props = global.props.as_ref();
                match global.type_ {
                    ObjectType::Metadata
                        if props.and_then(|p| p.get("metadata.name")) == Some("default") =>
                    {
                        bind_metadata(&registry, &core, global, &state, &events);
                    }
                    ObjectType::Device
                        if props.and_then(|p| p.get("device.api")) == Some("bluez5") =>
                    {
                        if let Some(address) = props.and_then(|p| p.get("api.bluez5.address")) {
                            bind_card(&registry, global, address, &state);
                        }
                    }
                    _ => {}
                }
            })
            .global_remove(move |id| {
                let gone = metadata_removed(id, &removed_state, &removed_events);
                // Out of the borrow, as anything going may tell.
                drop(gone);
                let mut state = removed_state.borrow_mut();
                state.cards.remove(&id);
                state.profiles.borrow_mut().remove(&id);
                if state.switch.as_ref().is_some_and(|s| s.card == id)
                    && let Some(switch) = state.switch.take()
                {
                    let _ = switch.done.send(Err(anyhow::anyhow!("the card went")));
                }
            })
            .register()
    };
    first_sync.set(Some(core.sync(0).context("PipeWire did not answer")?.seq()));

    // A switch that takes too long is given up, and the card set back:
    // looked at every so often, as a timer cannot be armed from a request.
    let timer = {
        let state = state.clone();
        main_loop.loop_().add_timer(move |_| give_up_switch(&state))
    };
    timer
        .update_timer(Some(SWITCH_CHECK), Some(SWITCH_CHECK))
        .into_sync_result()
        .context("no timer for the card switch")?;

    let _requests = {
        let quit = main_loop.downgrade();
        let (state, ring, core, events) =
            (state.clone(), ring.clone(), core.clone(), events.clone());
        let serve_one = move |request| match request {
            AudioRequest::Source(up) => set_source(up, &state, &ring, &core, &events),
            AudioRequest::Audio(packet) => decode(&packet, &state, &ring),
            AudioRequest::SetDefault(name) => set_default(name.as_deref(), &state),
            AudioRequest::SwitchCard { mac, done } => start_switch(mac, done, &state),
            AudioRequest::SetBackCard => set_back_switch(&state),
            AudioRequest::Sync(done) => match core.sync(0) {
                Ok(seq) => PENDING_SYNCS.with(|p| {
                    p.borrow_mut().insert(seq.seq(), done);
                }),
                Err(_) => drop(done),
            },
            AudioRequest::Quit => {
                if let Some(main_loop) = quit.upgrade() {
                    main_loop.quit();
                }
            }
        };
        // The byte read and the queue taken under the lock; each request
        // served once it is let go.
        main_loop.loop_().add_io(
            woken,
            IoFlags::IN,
            move |woken: &mut std::io::PipeReader| {
                let taken = {
                    let mut shared = lock(&requests);
                    let _ = woken.read(&mut [0]);
                    shared.as_mut().map(Queue::take).unwrap_or_default()
                };
                taken.into_iter().for_each(&serve_one);
            },
        )
    };

    main_loop.run();
    // Taken down before the connection, which the stream needs, and out of
    // the borrow, as its going tells its state.
    let source = state.borrow_mut().source.take();
    drop(source);
    if let Some(switch) = state.borrow_mut().switch.take() {
        let _ = switch
            .done
            .send(Err(anyhow::anyhow!("PipeWire's thread ended")));
    }
    PENDING_SYNCS.with(|pending| pending.borrow_mut().clear());
    Ok(())
}

thread_local! {
    /// Syncs asked for by [`AudioRequest::Sync`], by sequence number.
    static PENDING_SYNCS: RefCell<HashMap<i32, oneshot::Sender<()>>> = RefCell::default();
}

/// Binds the `default` metadata, in place of one bound before, and asks a
/// sync, whose answer says its properties are read. Once ready, the default
/// input is told then, and at every change after.
fn bind_metadata(
    registry: &pw::registry::Registry,
    core: &pw::core::CoreRc,
    global: &pw::registry::GlobalObject<&spa::utils::dict::DictRef>,
    state: &Rc<RefCell<State>>,
    events: &mpsc::UnboundedSender<AudioEvent>,
) {
    let metadata: Metadata = match registry.bind(global) {
        Ok(metadata) => metadata,
        Err(e) => {
            tracing::warn!("PipeWire's default metadata could not be bound: {e}");
            return;
        }
    };
    let (changed, events) = (state.clone(), events.clone());
    let listener = metadata
        .add_listener_local()
        .property(move |subject, key, _type, value| {
            if subject != pw::sys::PW_ID_CORE {
                return 0;
            }
            let mut state = changed.borrow_mut();
            match key {
                Some(DEFAULT_SOURCE_KEY) => {
                    state.configured = value.and_then(node_name);
                }
                // All of the subject's keys cleared.
                None => state.configured = None,
                Some(_) => return 0,
            }
            if state.ready && state.metadata_read {
                let _ = events.send(AudioEvent::Default(state.default_input()));
            }
            0
        })
        .register();
    let sync = match core.sync(0) {
        Ok(seq) => Some(seq.seq()),
        Err(e) => {
            tracing::warn!("PipeWire did not take a sync after the default metadata: {e}");
            None
        }
    };
    let before = {
        let mut state = state.borrow_mut();
        state.metadata_read = false;
        state.metadata_sync = sync;
        state.configured = None;
        state.metadata.replace(BoundMetadata {
            id: global.id,
            _listener: listener,
            metadata,
        })
    };
    // Out of the borrow, as anything going may tell.
    drop(before);
}

/// The global `id` removed: if it was the `default` metadata, the default
/// input is not known until another comes. Hands back the metadata, to be
/// dropped out of the state's borrow.
fn metadata_removed(
    id: u32,
    state: &Rc<RefCell<State>>,
    events: &mpsc::UnboundedSender<AudioEvent>,
) -> Option<BoundMetadata> {
    let mut state = state.borrow_mut();
    if state.metadata.as_ref().is_none_or(|bound| bound.id != id) {
        return None;
    }
    state.metadata_read = false;
    state.metadata_sync = None;
    state.configured = None;
    if state.ready {
        let _ = events.send(AudioEvent::Default(None));
    }
    state.metadata.take()
}

/// The node name in a default's value, `{ "name": "..." }`.
fn node_name(value: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(value).ok()?;
    value.get("name")?.as_str().map(String::from)
}

/// Binds a pair's card, and keeps what it reports of its profiles.
fn bind_card(
    registry: &pw::registry::Registry,
    global: &pw::registry::GlobalObject<&spa::utils::dict::DictRef>,
    address: &str,
    state: &Rc<RefCell<State>>,
) {
    let device: Device = match registry.bind(global) {
        Ok(device) => device,
        Err(e) => {
            tracing::debug!("A Bluetooth card of PipeWire's could not be bound: {e}");
            return;
        }
    };
    let id = global.id;
    let profiles = state.borrow().profiles.clone();
    profiles.borrow_mut().entry(id).or_default();
    let (reported, switching) = (profiles.clone(), state.clone());
    let listener = device
        .add_listener_local()
        .param(move |_seq, kind, _index, _next, param| {
            let Some((index, name)) = param.and_then(profile_of) else {
                return;
            };
            {
                let mut profiles = reported.borrow_mut();
                let card = profiles.entry(id).or_default();
                match kind {
                    ParamType::EnumProfile => {
                        if let Some(name) = name {
                            card.names.insert(index, name);
                        }
                    }
                    ParamType::Profile => card.current = Some(index),
                    _ => return,
                }
            }
            if kind == ParamType::Profile {
                advance_switch(id, index, &switching);
            }
        })
        .register();
    device.subscribe_params(&[ParamType::EnumProfile, ParamType::Profile]);
    state.borrow_mut().cards.insert(
        id,
        Card {
            address: address.to_string(),
            device,
            _listener: listener,
        },
    );
}

/// A profile param's index, and its name where it has one.
fn profile_of(param: &Pod) -> Option<(i32, Option<String>)> {
    let (_, value) = PodDeserializer::deserialize_any_from(param.as_bytes()).ok()?;
    let Value::Object(object) = value else {
        return None;
    };
    let mut index = None;
    let mut name = None;
    for property in object.properties {
        match (property.key, property.value) {
            (spa::sys::SPA_PARAM_PROFILE_index, Value::Int(i)) => index = Some(i),
            (spa::sys::SPA_PARAM_PROFILE_name, Value::String(n)) => name = Some(n),
            _ => {}
        }
    }
    Some((index?, name))
}

/// Sets `card`'s profile to the one at `index`, unsaved.
fn set_profile(card: &Card, index: i32) -> Result<()> {
    let value = Value::Object(Object {
        type_: spa::sys::SPA_TYPE_OBJECT_ParamProfile,
        id: spa::sys::SPA_PARAM_Profile,
        properties: vec![Property {
            key: spa::sys::SPA_PARAM_PROFILE_index,
            flags: PropertyFlags::empty(),
            value: Value::Int(index),
        }],
    });
    let bytes = PodSerializer::serialize(Cursor::new(Vec::new()), &value)
        .context("the profile did not serialize")?
        .0
        .into_inner();
    let pod = Pod::from_bytes(&bytes).context("the profile is no pod")?;
    card.device.set_param(ParamType::Profile, 0, pod);
    Ok(())
}

/// Begins switching the card of `mac` off and back.
fn start_switch(mac: Address, done: oneshot::Sender<Result<()>>, state: &Rc<RefCell<State>>) {
    let mut state = state.borrow_mut();
    if state.switch.is_some() {
        let _ = done.send(Err(anyhow::anyhow!("a card switch is under way already")));
        return;
    }
    let address = mac.to_string();
    let found = state
        .cards
        .iter()
        .find(|(_, card)| card.address.eq_ignore_ascii_case(&address))
        .map(|(&id, _)| id);
    let Some(id) = found else {
        let _ = done.send(Err(anyhow::anyhow!("PipeWire has no card for {mac}")));
        return;
    };
    let profiles = state.profiles.clone();
    let profiles = profiles.borrow();
    let card = profiles.get(&id);
    let off = card.and_then(|c| {
        c.names
            .iter()
            .find(|(_, name)| name.as_str() == "off")
            .map(|(&i, _)| i)
    });
    let back = card.and_then(|c| c.current);
    let (Some(off), Some(back)) = (off, back) else {
        let _ = done.send(Err(anyhow::anyhow!(
            "the card of {mac} reports no off profile, or no profile now"
        )));
        return;
    };
    if back == off {
        let _ = done.send(Err(anyhow::anyhow!("the card of {mac} is off already")));
        return;
    }
    if let Err(e) = set_profile(&state.cards[&id], off) {
        let _ = done.send(Err(e));
        return;
    }
    tracing::info!("Switching the card of {mac} off, and back to profile {back}");
    state.switch = Some(Switch {
        card: id,
        off,
        back,
        returning: false,
        since: std::time::Instant::now(),
        done,
    });
}

/// Takes a card's profile as reported: off sets it back, back is done.
fn advance_switch(card: u32, index: i32, state: &Rc<RefCell<State>>) {
    let mut state = state.borrow_mut();
    let Some(switch) = state.switch.as_mut().filter(|s| s.card == card) else {
        return;
    };
    if !switch.returning && index == switch.off {
        switch.returning = true;
        switch.since = std::time::Instant::now();
        let back = switch.back;
        if let Err(e) = set_profile(&state.cards[&card], back)
            && let Some(switch) = state.switch.take()
        {
            let _ = switch.done.send(Err(e));
        }
    } else if switch.returning
        && index == switch.back
        && let Some(switch) = state.switch.take()
    {
        let _ = switch.done.send(Ok(()));
    }
}

/// A switch that took longer than [`SWITCH_STEP_LIMIT`]: the card is set
/// back, so it is never left off, and the switch fails.
fn give_up_switch(state: &Rc<RefCell<State>>) {
    let mut state = state.borrow_mut();
    if state
        .switch
        .as_ref()
        .is_none_or(|switch| switch.since.elapsed() < SWITCH_STEP_LIMIT)
    {
        return;
    }
    let Some(switch) = state.switch.take() else {
        return;
    };
    if let Some(card) = state.cards.get(&switch.card) {
        let _ = set_profile(card, switch.back);
    }
    let step = if switch.returning { "back" } else { "off" };
    let _ = switch.done.send(Err(anyhow::anyhow!(
        "the card did not report its switch {step} within {SWITCH_STEP_LIMIT:?}"
    )));
}

/// Sets the card of a switch under way back, and fails the switch.
fn set_back_switch(state: &Rc<RefCell<State>>) {
    let mut state = state.borrow_mut();
    let Some(switch) = state.switch.take() else {
        return;
    };
    if let Some(card) = state.cards.get(&switch.card)
        && let Err(e) = set_profile(card, switch.back)
    {
        tracing::warn!("The card could not be set back: {e:#}");
    }
    let _ = switch
        .done
        .send(Err(anyhow::anyhow!("the card was set back for the quit")));
}

/// Puts the source up, or takes it down.
fn set_source(
    up: bool,
    state: &Rc<RefCell<State>>,
    ring: &Rc<RefCell<Ring>>,
    core: &pw::core::CoreRc,
    events: &mpsc::UnboundedSender<AudioEvent>,
) {
    if !up {
        let source = state.borrow_mut().source.take();
        if let Some(source) = source {
            let _ = source.stream.disconnect();
            tracing::info!("The microphone's source is down");
        }
        ring.borrow_mut().clear();
        let _ = events.send(AudioEvent::Recording(false));
        return;
    }
    if state.borrow().source.is_some() {
        return;
    }
    if state.borrow().decoder.is_none() {
        match Decoder::new() {
            Ok(decoder) => state.borrow_mut().decoder = Some(decoder),
            Err(e) => {
                tracing::warn!("The microphone's source stays down: {e:#}");
                return;
            }
        }
    }
    // Built outside the borrow: connecting tells the state at once.
    match source(core, ring, state, events) {
        Ok(source) => {
            state.borrow_mut().source = Some(source);
            tracing::info!("The microphone's source is up, as {NODE_NAME}");
        }
        Err(e) => tracing::warn!("The microphone's source could not be put up: {e:#}"),
    }
}

/// The source's stream, connected.
fn source(
    core: &pw::core::CoreRc,
    ring: &Rc<RefCell<Ring>>,
    state: &Rc<RefCell<State>>,
    events: &mpsc::UnboundedSender<AudioEvent>,
) -> Result<Source> {
    let stream = StreamRc::new(
        core.clone(),
        NODE_DESCRIPTION,
        properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_CLASS => "Audio/Source",
            *pw::keys::NODE_NAME => NODE_NAME,
            *pw::keys::NODE_DESCRIPTION => NODE_DESCRIPTION,
            *pw::keys::AUDIO_CHANNELS => "1",
            "audio.position" => "MONO",
            "device.icon-name" => "audio-headset",
        },
    )
    .context("no stream")?;

    let (streaming, playing, undecoded) = (events.clone(), ring.clone(), state.clone());
    let listener = stream
        .add_local_listener_with_user_data(())
        .state_changed(move |_, _, old, new| {
            tracing::debug!("The microphone's source went from {old:?} to {new:?}");
            if let StreamState::Error(e) = &new {
                tracing::warn!("The microphone's source failed: {e}");
            }
            let recording = matches!(new, StreamState::Streaming);
            if recording != matches!(old, StreamState::Streaming) {
                // A new recording starts with the samples that come next.
                playing.borrow_mut().clear();
                let lost = std::mem::take(&mut undecoded.borrow_mut().undecoded);
                if lost > 0 {
                    tracing::info!("{lost} of the microphone's frames did not decode");
                }
                let _ = streaming.send(AudioEvent::Recording(recording));
            }
        })
        .process({
            let ring = ring.clone();
            move |stream, _| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let requested = usize::try_from(buffer.requested()).unwrap_or(0);
                let data = &mut buffer.datas_mut()[0];
                let stride = std::mem::size_of::<f32>();
                let written = match data.data() {
                    Some(bytes) => {
                        let room = bytes.len() / stride;
                        let frames = if requested == 0 {
                            room
                        } else {
                            requested.min(room)
                        };
                        let mut samples = vec![0.0f32; frames];
                        ring.borrow_mut().fill(&mut samples);
                        for (chunk, sample) in bytes.chunks_exact_mut(stride).zip(&samples) {
                            chunk.copy_from_slice(&sample.to_le_bytes());
                        }
                        frames
                    }
                    None => 0,
                };
                let chunk = data.chunk_mut();
                *chunk.offset_mut() = 0;
                *chunk.stride_mut() = stride as i32;
                *chunk.size_mut() = u32::try_from(written * stride).unwrap_or(0);
            }
        })
        .register()
        .context("no stream listener")?;

    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(mic::SAMPLE_RATE);
    info.set_channels(1);
    let mut position = [0; spa::param::audio::MAX_CHANNELS];
    position[0] = spa::sys::SPA_AUDIO_CHANNEL_MONO;
    info.set_position(position);
    let format = PodSerializer::serialize(
        Cursor::new(Vec::new()),
        &Value::Object(Object {
            type_: spa::sys::SPA_TYPE_OBJECT_Format,
            id: spa::sys::SPA_PARAM_EnumFormat,
            properties: info.into(),
        }),
    )
    .context("the format did not serialize")?
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&format).context("the format is no pod")?];
    stream
        .connect(
            spa::utils::Direction::Output,
            None,
            StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .context("the stream did not connect")?;
    Ok(Source {
        stream,
        _listener: listener,
    })
}

/// Decodes an audio packet's frames into the ring, while the source is up.
fn decode(packet: &[u8], state: &Rc<RefCell<State>>, ring: &Rc<RefCell<Ring>>) {
    let mut state = state.borrow_mut();
    if state.source.is_none() {
        return;
    }
    let State {
        decoder, undecoded, ..
    } = &mut *state;
    let Some(decoder) = decoder.as_mut() else {
        return;
    };
    let mut samples = Vec::with_capacity(4 * mic::FRAME_SAMPLES);
    for unit in mic::access_units(packet) {
        if decoder.decode(unit.frame, &mut samples).is_err() {
            *undecoded += 1;
        }
    }
    ring.borrow_mut().push(&samples);
}

/// Sets the default input to the node `name`, or clears it.
fn set_default(name: Option<&str>, state: &Rc<RefCell<State>>) {
    let state = state.borrow();
    let Some(BoundMetadata { metadata, .. }) = &state.metadata else {
        tracing::warn!("PipeWire has no default metadata: the default input stays as it is");
        return;
    };
    match name {
        Some(name) => {
            let value = serde_json::json!({ "name": name }).to_string();
            metadata.set_property(
                pw::sys::PW_ID_CORE,
                DEFAULT_SOURCE_KEY,
                Some("Spa:String:JSON"),
                Some(&value),
            );
        }
        None => metadata.set_property(pw::sys::PW_ID_CORE, DEFAULT_SOURCE_KEY, None, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A private PipeWire daemon for a test, in a directory of its own: its
    /// socket there, no session manager, the `default` metadata and a dummy
    /// driver. Never the user's PipeWire. Killed when dropped.
    struct Daemon {
        child: std::process::Child,
        dir: tempfile::TempDir,
    }

    /// What the daemon loads: the protocol, metadata, streams and links.
    const DAEMON_CONF: &str = r#"
context.properties = { core.daemon = true, core.name = pipewire-0, support.dbus = false }
context.spa-libs = {
    audio.convert.* = audioconvert/libspa-audioconvert
    support.* = support/libspa-support
}
context.modules = [
    { name = libpipewire-module-protocol-native }
    { name = libpipewire-module-access }
    { name = libpipewire-module-metadata }
    { name = libpipewire-module-spa-node-factory }
    { name = libpipewire-module-client-node }
    { name = libpipewire-module-adapter }
    { name = libpipewire-module-link-factory }
]
context.objects = [
    { factory = metadata args = { metadata.name = default } }
    { factory = spa-node-factory
      args = { factory.name = support.node.driver node.name = Dummy-Driver priority.driver = 20000 } }
]
"#;

    /// The `default` metadata in [`DAEMON_CONF`], the daemon's own.
    const DAEMON_METADATA: &str = "    { factory = metadata args = { metadata.name = default } }\n";

    impl Daemon {
        /// The daemon, up; `None` where PipeWire's daemon is not installed.
        fn start() -> Option<Self> {
            Self::start_with(DAEMON_CONF)
        }

        /// The daemon with no `default` metadata of its own, up.
        fn start_without_metadata() -> Option<Self> {
            assert!(DAEMON_CONF.contains(DAEMON_METADATA));
            Self::start_with(&DAEMON_CONF.replace(DAEMON_METADATA, ""))
        }

        /// The daemon loading `conf`, up.
        fn start_with(conf_text: &str) -> Option<Self> {
            let dir = tempfile::tempdir().unwrap();
            let conf = dir.path().join("daemon.conf");
            std::fs::write(&conf, conf_text).unwrap();
            let child = Self::command_in(&dir, "pipewire")
                .arg("-c")
                .arg(&conf)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            let child = match child {
                Ok(child) => child,
                Err(e) => {
                    untried(&format!("no PipeWire daemon to test with: {e}"));
                    return None;
                }
            };
            let daemon = Self { child, dir };
            let socket = daemon.dir.path().join("pipewire-0");
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while !socket.exists() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the daemon did not come up"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
            Some(daemon)
        }

        /// `program`, reaching this daemon and no other.
        fn command_in(dir: &tempfile::TempDir, program: &str) -> std::process::Command {
            let mut command = std::process::Command::new(program);
            command
                .env("PIPEWIRE_RUNTIME_DIR", dir.path())
                .env_remove("PIPEWIRE_REMOTE")
                .env("DBUS_SESSION_BUS_ADDRESS", "disabled:");
            command
        }

        fn command(&self, program: &str) -> std::process::Command {
            Self::command_in(&self.dir, program)
        }

        /// Runs `command` against the daemon until it succeeds, for 10 s at
        /// most, and returns what it printed.
        async fn run(&self, command: &[&str]) -> String {
            match self.try_run(command).await {
                Ok(out) => out,
                Err(failed) => panic!("{command:?}: {failed}"),
            }
        }

        /// [`Daemon::run`], saying how the last try failed rather than
        /// failing the test.
        async fn try_run(&self, command: &[&str]) -> Result<String, String> {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                let done = self
                    .command(command[0])
                    .args(&command[1..])
                    .output()
                    .unwrap();
                if done.status.success() && done.stderr.is_empty() {
                    return Ok(String::from_utf8_lossy(&done.stdout).into_owned());
                }
                if std::time::Instant::now() >= deadline {
                    return Err(format!("{done:?}"));
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }

        /// The ID of the node named `name`, once it is there.
        async fn node_id(&self, name: &str) -> String {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                let listed = self.run(&["pw-cli", "ls", "Node"]).await;
                let mut id = None;
                for line in listed.lines() {
                    if let Some(rest) = line.trim().strip_prefix("id ") {
                        id = rest.split(',').next().map(str::to_string);
                    }
                    if line.contains(&format!("node.name = \"{name}\"")) {
                        return id.expect("an ID before the name");
                    }
                }
                assert!(std::time::Instant::now() < deadline, "no node {name}");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }

        /// The daemon's socket, for [`start_at`].
        fn socket(&self) -> String {
            self.dir.path().join("pipewire-0").display().to_string()
        }
    }

    impl Drop for Daemon {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    /// Says why part of a test is not tried: on CI (`CI` set) that fails the
    /// test, so CI never skips it unseen; elsewhere it is only said.
    fn untried(why: &str) {
        assert!(std::env::var_os("CI").is_none(), "untried on CI: {why}");
        eprintln!("untried: {why}");
    }

    /// The next event, failing the test if none comes in time.
    async fn next(events: &mut mpsc::UnboundedReceiver<AudioEvent>) -> AudioEvent {
        tokio::time::timeout(Duration::from_secs(10), events.recv())
            .await
            .expect("an event in time")
            .expect("the thread is there")
    }

    /// Answers once everything asked before is through.
    async fn sync(side: &dyn AudioSide) {
        let (done, answer) = oneshot::channel();
        assert!(side.ask(AudioRequest::Sync(done)));
        tokio::time::timeout(Duration::from_secs(10), answer)
            .await
            .expect("a sync in time")
            .expect("the thread is there");
    }

    /// Against a PipeWire of its own: ready with no default input; the
    /// source is recorded from while a recorder is linked to it, and no
    /// longer once it goes - where this PipeWire's ports can be linked
    /// without a session manager, which on CI they must be; the default
    /// input set to the source and cleared, each told back; a switch of a
    /// card PipeWire does not have fails; asked to quit, the side ends, a
    /// sync asked right after fails rather than waiting forever, and once
    /// the side has said it is gone, nothing more is taken.
    #[test]
    fn serves_a_pipewire_of_its_own() {
        // It would win over the daemon's socket, and may name the user's.
        if std::env::var_os("PIPEWIRE_REMOTE").is_some() {
            untried("PIPEWIRE_REMOTE is set, which the test's own client would connect to");
            return;
        }
        let Some(daemon) = Daemon::start() else {
            return;
        };
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let (events_tx, mut events) = mpsc::unbounded_channel();
            let side = start_at(Some(daemon.socket()), events_tx);
            assert_eq!(
                next(&mut events).await,
                AudioEvent::Ready {
                    default: Some(None)
                }
            );

            assert!(side.ask(AudioRequest::Source(true)));
            sync(&*side).await;

            let record = daemon.dir.path().join("recorded.wav");
            let mut recorder = daemon
                .command("pw-record")
                .args(["--channels", "1", "-P", "{ node.name = cymba-test-record }"])
                .arg(&record)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("pw-record");
            // With no session manager, each node's ports are set up here, as
            // WirePlumber would, and the two linked. The recorder's node
            // comes up in its own time: each step is tried until it takes.
            for (node, direction) in [(NODE_NAME, "Output"), ("cymba-test-record", "Input")] {
                let config = format!(
                    "{{ direction: {direction}, mode: dsp, format: {{ mediaType: audio, \
                     mediaSubtype: raw, format: F32P, rate: 48000, channels: 1, \
                     position: [ MONO ] }} }}"
                );
                let id = daemon.node_id(node).await;
                daemon
                    .run(&["pw-cli", "set-param", &id, "PortConfig", &config])
                    .await;
            }
            // By port: whether every pw-link links nodes is not known. A
            // PipeWire whose ports this setup does not bring up leaves the
            // recording untried, which fails the test on CI.
            let output = format!("{NODE_NAME}:capture_MONO");
            let linked = daemon
                .try_run(&["pw-link", &output, "cymba-test-record:input_MONO"])
                .await;
            match linked {
                Ok(_) => {
                    assert_eq!(next(&mut events).await, AudioEvent::Recording(true));
                    recorder.kill().unwrap();
                    recorder.wait().unwrap();
                    assert_eq!(next(&mut events).await, AudioEvent::Recording(false));
                }
                Err(failed) => {
                    recorder.kill().unwrap();
                    recorder.wait().unwrap();
                    untried(&format!("the recording: no link here ({failed})"));
                }
            }

            assert!(side.ask(AudioRequest::SetDefault(Some(NODE_NAME.into()))));
            assert_eq!(
                next(&mut events).await,
                AudioEvent::Default(Some(Some(NODE_NAME.into())))
            );
            assert!(side.ask(AudioRequest::SetDefault(None)));
            assert_eq!(next(&mut events).await, AudioEvent::Default(Some(None)));

            let (done, switched) = oneshot::channel();
            let mac = Address::new([0x02, 0, 0, 0, 0, 1]);
            assert!(side.ask(AudioRequest::SwitchCard { mac, done }));
            let refused = switched.await.unwrap().unwrap_err();
            assert!(format!("{refused}").contains("no card"), "{refused}");

            assert!(side.ask(AudioRequest::Source(false)));
            assert!(side.ask(AudioRequest::Quit));
            // Taken before the thread ends, or refused: either way it fails.
            let (done, answer) = oneshot::channel();
            side.ask(AudioRequest::Sync(done));
            tokio::time::timeout(Duration::from_secs(10), answer)
                .await
                .expect("the sync fails in time")
                .expect_err("no sync after the quit");
            loop {
                if let AudioEvent::Gone(_) = next(&mut events).await {
                    break;
                }
            }
            let (done, answer) = oneshot::channel();
            assert!(!side.ask(AudioRequest::Sync(done)));
            answer.await.expect_err("nothing answers once gone");
        });
    }

    /// A `default` metadata another client exports to PipeWire, as
    /// WirePlumber does (`docs/protocol/linux/desktop.md#pipewire`): kept in
    /// that client, there until dropped, which disconnects the client.
    struct ClientMetadata {
        quit: pw::channel::Sender<()>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl ClientMetadata {
        /// The metadata, exported to the daemon at `socket`, once PipeWire
        /// has it.
        // pipewire-rs has no safe way to make a metadata of a client's own
        // and export it: libpipewire's own calls, as WirePlumber makes them.
        #[allow(unsafe_code)]
        fn make(socket: String) -> Self {
            let (quit, quitting) = pw::channel::channel();
            let (made, has) = std::sync::mpsc::channel();
            let thread = std::thread::spawn(move || {
                pw::init();
                let main_loop = pw::main_loop::MainLoopRc::new(None).unwrap();
                let context = pw::context::ContextRc::new(&main_loop, None).unwrap();
                let core = context
                    .connect_rc(Some(properties! { *pw::keys::REMOTE_NAME => socket }))
                    .unwrap();
                let props = properties! { "metadata.name" => "default" };
                // SAFETY: the context and the core are live; the metadata is
                // destroyed below, after the core that exported it, and
                // before the context that made it.
                let metadata = unsafe {
                    pw::sys::pw_context_create_metadata(
                        context.as_raw_ptr(),
                        c"default".as_ptr(),
                        std::ptr::null_mut(),
                        0,
                    )
                };
                assert!(!metadata.is_null(), "no metadata");
                // SAFETY: as above; `props` is copied by the call.
                let exported = unsafe {
                    pw::sys::pw_core_export(
                        core.as_raw_ptr(),
                        c"PipeWire:Interface:Metadata".as_ptr(),
                        props.dict().as_raw_ptr().cast(),
                        pw::sys::pw_impl_metadata_get_implementation(metadata).cast(),
                        0,
                    )
                };
                assert!(!exported.is_null(), "the metadata not exported");
                let seq = core.sync(0).unwrap();
                let _done = core
                    .add_listener_local()
                    .done(move |id, done| {
                        if id == pw::sys::PW_ID_CORE && done == seq {
                            let _ = made.send(());
                        }
                    })
                    .register();
                let _quitting = quitting.attach(main_loop.loop_(), {
                    let main_loop = main_loop.downgrade();
                    move |()| {
                        if let Some(main_loop) = main_loop.upgrade() {
                            main_loop.quit();
                        }
                    }
                });
                main_loop.run();
                drop(_done);
                drop(core);
                // SAFETY: made above, destroyed once.
                unsafe { pw::sys::pw_impl_metadata_destroy(metadata) };
            });
            has.recv_timeout(Duration::from_secs(10))
                .expect("the metadata made in time");
            Self {
                quit,
                thread: Some(thread),
            }
        }
    }

    impl Drop for ClientMetadata {
        fn drop(&mut self) {
            let _ = self.quit.send(());
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    /// Against a PipeWire with no `default` metadata at first, as before
    /// WirePlumber makes it: ready with the default input not read; once
    /// another client makes the metadata, with no default input in it, the
    /// default input is told as none, and a change to it after as that;
    /// once that client goes, and the metadata with it, the default input
    /// is told as not read.
    #[test]
    fn reads_the_default_input_once_the_metadata_comes() {
        if std::env::var_os("PIPEWIRE_REMOTE").is_some() {
            untried("PIPEWIRE_REMOTE is set, which the test's own client would connect to");
            return;
        }
        let Some(daemon) = Daemon::start_without_metadata() else {
            return;
        };
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let (events_tx, mut events) = mpsc::unbounded_channel();
            let side = start_at(Some(daemon.socket()), events_tx);
            assert_eq!(next(&mut events).await, AudioEvent::Ready { default: None });

            let metadata = ClientMetadata::make(daemon.socket());
            assert_eq!(next(&mut events).await, AudioEvent::Default(Some(None)));
            assert!(side.ask(AudioRequest::SetDefault(Some(NODE_NAME.into()))));
            assert_eq!(
                next(&mut events).await,
                AudioEvent::Default(Some(Some(NODE_NAME.into())))
            );

            drop(metadata);
            assert_eq!(next(&mut events).await, AudioEvent::Default(None));
            assert!(side.ask(AudioRequest::Quit));
            loop {
                if let AudioEvent::Gone(_) = next(&mut events).await {
                    break;
                }
            }
        });
    }

    /// Nothing plays until two packets' worth is in; then it plays, in
    /// order, and silence follows once it runs dry, until it is primed
    /// again.
    #[test]
    fn the_ring_waits_for_two_packets() {
        let mut ring = Ring::default();
        let mut out = [1.0f32; 4];
        ring.push(&[0.5; PREFILL - 1]);
        ring.fill(&mut out);
        assert_eq!(out, [0.0; 4]);
        ring.push(&[0.25]);
        ring.fill(&mut out);
        assert_eq!(out, [0.5; 4]);

        let mut rest = vec![1.0f32; PREFILL];
        ring.fill(&mut rest);
        assert_eq!(rest[PREFILL - 5..PREFILL - 4], [0.25]);
        assert!(rest[PREFILL - 4..].iter().all(|&s| s == 0.0));
        ring.push(&[0.75; 8]);
        ring.fill(&mut out);
        assert_eq!(out, [0.0; 4], "not primed again yet");
    }

    /// Past 0.2 s, the oldest samples go.
    #[test]
    fn the_ring_keeps_the_newest() {
        let mut ring = Ring::default();
        ring.push(&vec![0.1; MAX_BUFFERED]);
        ring.push(&[0.9; 10]);
        assert_eq!(ring.samples.len(), MAX_BUFFERED);
        assert_eq!(ring.samples.back(), Some(&0.9));
        assert_eq!(ring.samples.front(), Some(&0.1));
    }

    /// Sixteen audio packets wait at most; the seventeenth is refused, while
    /// any other request is still taken. Once the thread has taken the
    /// queue, audio is taken again.
    #[test]
    fn the_queue_holds_sixteen_audio_packets() {
        let (_woken, wake) = std::io::pipe().unwrap();
        let shared: Shared = Arc::new(Mutex::new(Some(Queue {
            requests: VecDeque::new(),
            audio: 0,
            wake,
        })));
        let side = Requests(shared.clone());
        for _ in 0..16 {
            assert!(side.ask(AudioRequest::Audio(vec![0; 4])));
        }
        assert!(!side.ask(AudioRequest::Audio(vec![0; 4])));
        assert!(side.ask(AudioRequest::SetDefault(None)));
        assert!(side.ask(AudioRequest::Source(false)));
        let taken = lock(&shared).as_mut().unwrap().take();
        assert_eq!(taken.len(), 18);
        assert!(side.ask(AudioRequest::Audio(vec![0; 4])));
    }

    /// A default's value names its node; anything else names none.
    #[test]
    fn reads_the_node_a_default_names() {
        assert_eq!(
            node_name(r#"{ "name": "alsa_input.pci-0000_00_1f.3.analog-stereo" }"#).as_deref(),
            Some("alsa_input.pci-0000_00_1f.3.analog-stereo")
        );
        assert_eq!(node_name("not json"), None);
        assert_eq!(node_name(r#"{ "other": 1 }"#), None);
    }
}
