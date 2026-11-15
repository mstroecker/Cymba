# The microphone

How Cymba offers the AirPods' microphone over AAP as an input on PipeWire, "AirPods
Microphone", while A2DP plays on in stereo (#209). Experimental and off by default. None
of it has run on a pair yet: it has run against the fake world, in the pipeline tests,
and against a PipeWire daemon of a test's own ([Hardware checks](#hardware-checks)). The
packets, the audio and what the pods do around them are in
[protocol/aap/microphone.md](../protocol/aap/microphone.md); PipeWire's side is in
[protocol/linux/desktop.md](../protocol/linux/desktop.md#pipewire).

After every stop the app switches the pair's card off and back: on a Pro 3 one pod went
silent after a stop until that was done
([protocol/aap/microphone.md](../protocol/aap/microphone.md#a2dp)). That is a workaround,
which #212 is to replace; it is why the feature is off unless turned on.

`mic::Stream` decides when to start, stop and turn Conversation Awareness off and on, and
`mic::DefaultInput` when to make the source the default input and put the user's back,
each pure and tested on its own, as `volume::Ducking` is. `mic::Mic` carries them out in
a task of its own on the runtime, with PipeWire on a thread of its own
([The PipeWire side](#the-pipewire-side)).

## The switch

The Preferences switch "AirPods microphone" (`Settings::mic_over_aap`,
`app.mic-over-aap`) is off by default ([ui.md](ui.md#settings)). It reaches the task on a
`watch`.

- **On:** the task checks the decoder ([The decoder](#the-decoder)) and starts the
  PipeWire side. With an attached pair on AAP - the first one linked, of several - the
  source is up and made the default input.
- **Off:** a stream that runs is stopped, the default input put back, the source taken
  down, and the PipeWire side ended.
- **Not offered** - this build has no `mic` feature, no decoder loads, or PipeWire cannot
  be reached - the row says why (`mic::Availability`), and is insensitive while the
  switch is off. One that is on can always be turned off.

## When the microphone runs

Only while something records from the source. Its stream's state says so: `Streaming`
while an app records from it ([protocol/linux/desktop.md](../protocol/linux/desktop.md#pipewire)).
Nothing is polled.

| | |
|---|---|
| Something starts recording | Conversation Awareness off, if the link reads it on; then `Command::Microphone(true)`, the start. |
| The last recording ends | The stop is due `mic::STOP_AFTER` later (a choice): an app that closes the input and opens it again, as a call switching devices does, keeps the stream. |
| Something records again in time | Nothing: the stream goes on. |
| The stop is due, or the pair is no longer the source's: the switch off, the pair detached | `Command::Microphone(false)`, the stop; Conversation Awareness on again; then the card switch ([The card switch](#the-card-switch)). |
| The pair's link goes | The stream is over: no stop is sent, and the card is not switched. |
| Something records during the card switch | The start waits until the switch is done. |

**Commands take the one path** (`Coordinator::command`, [commands.md](commands.md)), in
the order the task calls them: Conversation Awareness off before the start, and on after
the stop.

**The pods' answers** come as `MicEvent::Answer` (`Coordinator::mic_events`) and are only
logged: nothing waits for them, as nothing would be done otherwise. Each link coming up
and going comes as an event too, so a link replaced between two snapshots still ends the
stream ([state.md](state.md#events)).

## Conversation Awareness

Turned off for the stream, so the pods neither switch to Transparency nor send speech
levels while the user speaks into the microphone - and lower the music
([volume.md](volume.md)). On a Pro 3 the levels came while the microphone ran
([protocol/aap/microphone.md](../protocol/aap/microphone.md#the-audio-source-and-the-pods)).

- **Only if the link reads it on** at the start. Off or not reported yet, it is left alone.
- **On again once the stream is over,** and only if the link reads it off then. Read on
  after it read off, someone turned it on - the window, or a report from the pods - and it
  is left as it is. A change made on a phone may never reach the app
  ([conversation-awareness.md](conversation-awareness.md#the-setting)).
  Until the link has read it off once, a reading of on is the setting before the command
  landed, and is waited out.
- **Over the next link,** when the link went first: on again once a new link reads it off.
- **A turn off that failed** turns nothing on after.

## The default input

The source is the default input while it is up, so a call app that records "the
microphone" takes it, as a phone call would on the pods. The default input is PipeWire's
`default.configured.audio.source`, the one GNOME's Sound panel sets
([protocol/linux/desktop.md](../protocol/linux/desktop.md#pipewire)).

- **Made the source** as the source comes up, the one before remembered.
- **Put back** as the source goes - the pair detached, the switch off, the quit - to the
  one before, or cleared if there was none.
- **The user's change wins.** Once the default input has read the source, another one is
  the user's choice: nothing is put back, and the source is not made the default input
  again until it has gone and come back, as `volume` leaves a volume the user moved.
- **A default naming the source** when it comes up - a quit that did not put it back - is
  not remembered as the one before.

## The card switch

After every stop: the pair's card switched to its `off` profile and back to the profile
it had, through the card's `Profile` param on PipeWire - what `pactl set-card-profile`
does, which the probe used. Then, as the probe does:

1. **The players playing before** are read (MPRIS, `media::list_players`).
2. **A new A2DP transport** is waited for, for `mic::RESUME_WAIT`: one that went and came
   back, or another path. Whether playing too early leaves both pods silent is open
   ([protocol/aap/microphone.md](../protocol/aap/microphone.md#open-questions)).
3. **Each of those players is played again** once it reports the pause the switch causes,
   with an explicit `Play`, within `mic::RESUME_WAIT`. One still playing, stopped or gone
   is left alone: only what the switch paused is played.
4. **A2DP is waited for to play,** for `mic::RESUME_WAIT`. Not playing, the card is switched
   again, up to `mic::SWITCH_TRIES` switches; on a Pro 3 one switch once left both pods
   silent.

With nothing playing before, the switch is done after step 2.

- **Only after a stop the app sent:** never on a timer of its own, never while nothing
  recorded, never for a link that went.
- **On PipeWire's thread,** each step within a limit (`SWITCH_STEP_LIMIT`, a choice): a
  card that does not report its switch off in time is set back all the same, so it is never
  left off.
- **Calls without auto-start, within `media::CALL_LIMIT`**, the players' side by side, as
  [media-pause.md](media-pause.md#the-task) makes them; the transport is read with
  `volume::a2dp_transport_in`, under the pair's device by `Device` and `UUID`.
- **In a task of its own,** not in the pair's supervisor, which is aborted wherever it is
  ([aap-links.md](aap-links.md#the-supervisor)).

## The audio

- **Handed on, never waited for.** The read loop routes an audio packet by its header
  before anything is logged or decoded, and hands it to the one consumer
  (`Coordinator::mic_audio`) over a channel of `mic::AUDIO_QUEUE` packets. One more is
  dropped and counted (`Coordinator::mic_audio_dropped`), which the stop logs: the link's
  other packets come on the same loop. Nothing logs each audio packet
  ([logging.md](logging.md#aap-packets)).
- **Only the streaming pair's** audio reaches the source; the rest is dropped.
- **Decoded on PipeWire's thread,** each access unit (`aap::mic::access_units`), and
  played at `aap::mic::SAMPLE_RATE`, 64 kHz, the pace the frames came at on a Pro 3,
  not the AudioSpecificConfig's 48 kHz. PipeWire converts it to what each app records.
  Whether it sounds right is open
  ([protocol/aap/microphone.md](../protocol/aap/microphone.md#open-questions)).
- **`MAX_BUFFERED` samples at most wait** for the source (`mic::pipewire`); older ones
  go. After it starts or runs dry, the source plays nothing until `PREFILL` samples are in,
  so packets that come in bursts do not run it dry at once. A frame that does not decode is counted and logged when the
  recording ends.

## The decoder

FFmpeg's own `aac` decoder, from the system's libavcodec - never `libfdk_aac`, and no
FFmpeg of the app's own.

- **Loaded when the switch is first turned on,** not linked: a release binary linked to
  one libavcodec soname would not start on a system with another, or none. The sonames
  from `decoder::NEWEST_MAJOR` down to `decoder::OLDEST_MAJOR` are tried, and the one that loads is
  used if its own `avcodec_version` says the same major. Of FFmpeg's structures only the
  first fields of `AVCodecParameters` and `AVFrame` are touched, which every major in that
  range shares; a newer major is not tried until checked.
- **Opened with the microphone's AudioSpecificConfig** as the codec's extradata. One that
  does not open makes the microphone unavailable, with the reason in Preferences.

## The PipeWire side

The test `mic::pipewire::tests::serves_a_pipewire_of_its_own` runs it against a PipeWire
daemon of its own ([protocol/linux/desktop.md](../protocol/linux/desktop.md#runs)).

On a thread of its own (`mic::pipewire`), running PipeWire's loop: PipeWire's objects
never leave the thread that made them, and its loop blocks. The task asks it things over a
PipeWire channel (`mic::AudioRequest`) and hears from it on a tokio one
(`mic::AudioEvent`).

- **The source** is a stream with `media.class` `Audio/Source`, `node.name`
  `cymba.airpods_mic` (`mic::NODE_NAME`), one channel at 64 kHz.
- **Ready** once two round trips are done: every object announced, then the `default`
  metadata's properties read. The default input read then is the one the task starts from.
- **Ended** when PipeWire goes or cannot be reached: the task starts it again on
  `bluez::Backoff`, while the switch is on, as the tasks on the system bus do.

## Building without it

The `mic` cargo feature, on by default, brings the `pipewire` crate, which needs
PipeWire's headers and libclang to build, and `libloading`. `--no-default-features`
builds without both: the switch then says the build has no microphone support. The
pure parts - `aap::mic`, `mic::Stream`, `mic::DefaultInput` - build and are tested
either way.

## Hardware checks

On a Pro 3, with the switch on and music playing over A2DP:

- The source shows in GNOME's Sound panel as "AirPods Microphone", and is the default
  input while the pair is connected; disconnected, or the switch off, the input before is
  back.
- Recording from it - GNOME Sound Recorder, a call in a browser - starts the microphone
  within about half a second, and the music plays on in stereo on both pods.
- The recording sounds right at 64 kHz: speech at its pitch and speed.
- Conversation Awareness, on before, is off while recording and on again after.
- Whether the pods report the setting (`0x28`) after it is turned off for the stream: a
  late report of on would read as the user's, and leave it off after the stream.
- After a stop, the log's "Card switch N" and "Played ... again after the card switch":
  the players played again, and which transport is taken when BlueZ briefly shows two.
- `mic::STOP_AFTER` after the recording ends the stream stops, the card is switched, the
  music pauses briefly and plays again, on both pods. How often more than one switch is
  needed, and whether `mic::RESUME_WAIT` is long enough.
- Short opens of the input - a browser listing devices, the Sound panel's level meter -
  start and stop the stream, and each stop switches the card: whether that is a nuisance.
- Choosing another input in the Sound panel while the pair is connected: it stays chosen
  after a disconnect.
- What a pod taken out of an ear, or into the case, does to the stream
  ([protocol/aap/microphone.md](../protocol/aap/microphone.md#open-questions)).
- The quit while recording: the stop is sent, with no card switch: whether one pod then
  stays silent until the next stream or reconnect.
- The Flatpak: PipeWire reached through `xdg-run/pipewire-0`, and the runtime's codecs
  extension carrying the `aac` decoder.
- A Pro 2 (USB-C) is assumed to behave the same.
