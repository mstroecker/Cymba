# AAP microphone

The microphone over AAP (`0x58`): the packets that start and stop it, the audio packets
the pods send, the AAC-ELD inside them, and what happens to A2DP meanwhile. The packets
come from LibrePods. Four runs on AirPods Pro 3 on 2026-11-15 (#211) confirmed them, with
music playing over A2DP throughout. That AirPods Pro 2 (USB-C) behave the same is
assumed, not checked.

The app does not use the microphone yet (#209). The packets are sent by
`examples/mic_probe.rs`.

Byte offsets count from 0 at the first byte of the packet. Multi-byte numbers are
little-endian.

**Source:** the start and stop packets, the audio layout, the AudioSpecificConfig and
the A2DP switch come from [LibrePods PR #655](https://github.com/librepods-org/librepods/pull/655)
(`linux-rust/src/bluetooth/aacp.rs`, `aacp_audio.rs`, `audio/eld.rs`,
`audio/output.rs`, `ui/window.rs`), for protocol facts only; no code is taken from it.
What the pods did with them comes from the [runs](#runs).

## Packets

Bytes 6-7 tell the two kinds of `0x58` packet apart:

- `00 00`: a control packet, the start or the stop, and their answers.
- `01 00`: audio.

In both kinds, bytes 8-9 are the length of the rest of the packet, after byte 9.

| Packet | From | Bytes |
|---|---|---|
| Start | Client | `04 00 04 00 58 00 00 00 09 00 00 01 82 00 00 00 04 96 00` |
| Answer to the start | Pods | `04 00 04 00 58 00 00 00 03 00 01 01 01` |
| Stop | Client | `04 00 04 00 58 00 00 00 02 00 03 01` |
| Answer to the stop | Pods | `04 00 04 00 58 00 00 00 02 00 04 01` |

- **The start's payload,** `00 01 82 00 00 00 04 96 00`, is LibrePods'. What its bytes
  select is not known.
- **The answers came 0.26-0.37 s after the start and 0.21-0.26 s after the stop.** The
  first audio came 0.32-0.43 s after the start. One or two audio packets came after the
  stop.
- **Each answer's first payload byte is one more than the request's:** `00` answered
  with `01`, `03` with `04`. What the answers' payloads mean is not known.

## Audio: `0x58 01 00`

```
Offset:  0  1  2  3  4  5  6  7  8  9  10 11 12 13 14 15 16 17 18 19 20 21  22 ...
Hex:     04 00 04 00 58 00 01 00 LL LL 58 00 01 00 MM MM 18 4B CC CC CC CC  [access units]
         ~~~~~~~~~~~ ~~~~~ ~~~~~ ~~~~~ ~~~~~~~~~~~ ~~~~~ ~~~~~ ~~~~~~~~~~~  ~~~~~~~~~~~~~~
         Header      Op    Audio Len   Repeated    Len   ?     Clock        4 units
```

- **`LL LL`** is the length after byte 9: the packet's length less 10. **`MM MM`** is the
  length after byte 15: the packet's length less 16. Both held in every header printed,
  and bytes 8 and 14 were the only length bytes that changed in a run.
- **Bytes 16-17 were `18 4B`** in every packet of every run. What they mean is not known.
- **Bytes 18-21 are most likely a clock in milliseconds** (a guess): a 32-bit number that
  went up by 15 to 30 between the first packets of a run, a packet coming every 30 ms on
  average, and by 1 423 110 between runs 1 and 2, which were about 24 minutes apart.
- **Each access unit** is a 4-byte timestamp, a length byte, then that many bytes of
  AAC-ELD:

  ```
  [timestamp, 4 bytes] [length, 1 byte] [AAC-ELD frame, length bytes]
  ```

- **Four units in every packet,** 326 to 353 bytes in all.
- **The AAC-ELD frames were 68 to 78 bytes,** 74.6 to 75.3 on average: about 80 kbit/s
  at the rate below.
- **The timestamp went up by 180 a unit,** in every step of every run. It started at 900
  in runs 1, 3 and 4, and at 1080 in run 2. At 7.5 ms a frame, that is a clock of 24 000
  a second.

## The audio

- **AAC-ELD, mono, 480 samples a frame,** decoded by FFmpeg's built-in `aac` decoder (the
  system FFmpeg, 9.0.2 by `ffmpeg -version`) with LibrePods' AudioSpecificConfig,
  `F8 E6 30 00`. Every frame of every run decoded with no error.
- **The frames come at the pace of 64 kHz,** although the AudioSpecificConfig says
  48 kHz. 133.2 to 133.5 frames came a second, which is 63 913 to 64 098 samples a
  second. Run 1's 614 400 samples last 9.6 s at 64 kHz, which fits in the 10 s
  recording; at 48 kHz they would last 12.8 s, which does not. Whether the samples played
  at 64 kHz sound right has not been checked by ear.
- **The AudioSpecificConfig decodes as:** object type 39 (ER AAC-ELD), sampling frequency
  index 3 (48 kHz), channel configuration 1 (mono), frame length flag 1 (480 samples), and
  no low-delay SBR: worked out by hand from the AudioSpecificConfig and
  ELDSpecificConfig syntax of ISO/IEC 14496-3:2019, and FFmpeg's `ffprobe` reads the
  stream as ELD, 48 kHz, one channel.

## The audio source and the pods

Around the start and the stop the pods report this host's audio source (`0x0E`,
[session.md](session.md#0x0e-the-audio-source)) with another type (runs 2-4):

| When | `0x0E` |
|---|---|
| Before the start, with music playing | This host, type `02` |
| 0-4 ms after the answer to the start | This host, type `22` |
| 0-3 ms after the answer to the stop | This host, type `02` |

A `0x55` of 10 bytes came 21-30 ms after the answer to the stop.

- **No role switch.** The right pod was the primary pod at link-up, and no primary pod
  report (`0x08`) or ear report (`0x06`) came at the start, the stop or the card switch.
- **Conversation Awareness went on.** Speech levels `01` and `02` came while the
  microphone ran, `01` with a noise mode report, and `03`, `0b` and `04` after the stop
  ([conversation-awareness.md](conversation-awareness.md#speech-levels-0x4b)).

## A2DP

- **A2DP played on while the microphone ran.** BlueZ's A2DP transport stayed `active`
  2 s after the start, at the stop and 5 s after it, and the music played on both pods.
- **After the stop, only the right pod played,** the primary pod. The left pod stayed
  silent until one of these:
  - another start: then both pods played until the next stop;
  - the card switched to the `off` profile and back to `a2dp-sink`, with
    `pactl set-card-profile`. The switch also paused the music player.
- **Pausing the music for 10 s** and playing it again did not bring the left pod back.
- **The switch does not always work.** In run 4 the music came back on neither pod: the
  player played, and the A2DP transport was not active 2.5 s after the switch. Switching
  by hand once more brought both pods back.
- **During the switch that worked** (run 3) the pods sent:
  1. a `0x0E` naming an address that was not the pair, this host or the pod the link-up's
     `0x0C` named - possibly all zeros, which that run did not tell apart - type `00`,
     1.6 s after the switch began;
  2. a `0x0E` naming this host, type `02`, 0.2 s later;
  3. a `0x4C` of 11 bytes, then a `0x55` of 10 bytes.

  **In the switch that failed** (run 4) only the first came, naming all zeros, 1.5 s
  after the switch began, and nothing after it in the next second.

LibrePods says one pod's playback drops after a stop on some setups (`ui/window.rs`),
and switches the card off and back to its profile both after the start and after the
stop (`audio/output.rs`).

## Runs

All four ran a `mic_probe` against a Pro 3 with music playing to the pair, on
2026-11-15 (#211), each recording 10 s while the user spoke. Each used the probe as it
stood then: run 1 printed the `0x58` packets alone, runs 2-4 every packet, and the
switch after the stop grew from run 3 to the probe that is committed. `<MAC>` stands for
the pair's address.

### Run 1

`cargo run --example mic_probe -- <MAC> <OUT> 10`, a probe that printed the `0x58`
packets alone. Excerpt:

```
A2DP before the start: playing
  0.000 s  sent start  04 00 04 00 58 00 00 00 09 00 00 01 82 00 00 00 04 96 00; recording for 10 s - speak
  0.368 s  0x58  04 00 04 00 58 00 00 00 03 00 01 01 01
  0.433 s  audio, 343 bytes, header 04 00 04 00 58 00 01 00 4d 01 58 00 01 00 47 01 18 4b 4f 9b ee 01
           unit ts 900 (0x00000384), 74 bytes
           unit ts 1080 (0x00000438), 75 bytes
           unit ts 1260 (0x000004ec), 74 bytes
           unit ts 1440 (0x000005a0), 78 bytes
  0.482 s  audio, 336 bytes, header 04 00 04 00 58 00 01 00 46 01 58 00 01 00 40 01 18 4b 5e 9b ee 01
  0.484 s  audio, 340 bytes, header 04 00 04 00 58 00 01 00 4a 01 58 00 01 00 44 01 18 4b 7b 9b ee 01
  2.001 s  A2DP: playing
A2DP at the end: playing
 10.017 s  sent stop   04 00 04 00 58 00 00 00 02 00 03 01
 10.224 s  0x58  04 00 04 00 58 00 00 00 02 00 04 01

first audio 0.433 s after the start
audio packets: 320
  sizes (bytes: count): 329: 1, 330: 1, 332: 4, ... 351: 3, 352: 1
  access units per packet (units: count): 4: 320
  packets with bytes after the last whole unit: 0
access units: 1280, 68 to 78 bytes, 74.6 on average
  timestamp steps (step: count, most common first): 180: 1279
  header bytes that changed (offset: packets): 8: 294, 14: 294, 18: 317, 19: 313
frames came at 133.5 a second over 9.56 s: 64098 samples a second at 480 a frame
audio packets after the stop: 1
ffprobe: codec_name=aac|profile=ELD|sample_rate=48000|channels=1
captures/2026-11-15/mic-48k.wav: ffmpeg exit status: 0, 0 error lines
  614400 samples, 1 channel(s), 12.80 s at 48000 Hz; peak -24.1 dBFS, RMS -40.6 dBFS
```

The units of the second and third packet are left out. Both pods played through the run
and only the right one after it. A second start brought the left pod back until its
stop. `pactl set-card-profile <card> off`, then `a2dp-sink`, brought it back too, and
paused the player, which was started again by hand. In a later run with `--no-reset`,
pausing the music for 10 s and playing it again left the left pod silent.

### Run 2

`cargo run --example mic_probe -- <MAC> <OUT> 10 --no-reset`, a probe that printed
every packet with the addresses named, about 24 minutes after run 1. Excerpt; of the
startup dump only its `0x08`, `0x0C`, `0x0E` and `0x06` are kept, and no other `0x06`
or `0x08` came in the run:

```
  0.000 s  connected; A2DP: playing
  0.120 s  primary pod Some(Right)
  0.124 s  0x0C names address 1, ending 01 02
  0.124 s  0x0C names this host, ending 01 01
  0.124 s  0x0E names this host, type 02
  0.129 s  ear: primary InEar, secondary InEar
  3.002 s  sent start  04 00 04 00 58 00 00 00 09 00 00 01 82 00 00 00 04 96 00; recording for 10 s - speak
  3.266 s  0x58  04 00 04 00 58 00 00 00 03 00 01 01 01
  3.270 s  0x0E names this host, type 22
  3.319 s  audio, 342 bytes, header 04 00 04 00 58 00 01 00 4c 01 58 00 01 00 46 01 18 4b 55 52 04 02
           unit ts 1080 (0x00000438), 77 bytes
  3.340 s  audio, 345 bytes, header 04 00 04 00 58 00 01 00 4f 01 58 00 01 00 49 01 18 4b 64 52 04 02
  3.375 s  audio, 344 bytes, header 04 00 04 00 58 00 01 00 4e 01 58 00 01 00 48 01 18 4b 81 52 04 02
  5.003 s  A2DP: playing
 11.586 s  speech level 01 (speaking)
 11.588 s  noise mode Transparency
 12.302 s  speech level 02 (speaking)
 13.003 s  A2DP: playing
 13.003 s  sent stop   04 00 04 00 58 00 00 00 02 00 03 01
 13.261 s  0x58  04 00 04 00 58 00 00 00 02 00 04 01
 13.264 s  0x0E names this host, type 02
 13.291 s  0x55, 10 bytes
 18.011 s  A2DP: playing

first audio 0.317 s after the start
audio packets: 324
  sizes (bytes: count): 327: 1, 329: 1, 330: 3, ...
  access units per packet (units: count): 4: 324
access units: 1296, 68 to 78 bytes, 74.9 on average
  timestamp steps (step: count, most common first): 180: 1295
  header bytes that changed (offset: packets): 8: 300, 14: 300, 18: 321, 19: 317
frames came at 133.5 a second over 9.68 s: 64093 samples a second at 480 a frame
audio packets outside the recording: 1
ffprobe: codec_name=aac|profile=ELD|sample_rate=48000|channels=1
captures/2026-11-15/mic3-48k.wav: ffmpeg exit status: 0, 0 error lines
```

Byte 21 of the second and third headers, and the end of the sizes line, were cut off in
the terminal; byte 21 is the first header's, as the changed offsets show. The left pod
was silent after the stop.

### Run 3

The same command without `--no-reset`, after the card was switched by hand to bring both
pods back, with a probe that switched the card once and played the player again at
once. Excerpt, the startup dump left out; as in run 2 it named the right
pod primary, and no other `0x06` or `0x08` came:

```
  3.002 s  sent start  04 00 04 00 58 00 00 00 09 00 00 01 82 00 00 00 04 96 00; recording for 10 s - speak
  3.308 s  0x58  04 00 04 00 58 00 00 00 03 00 01 01 01
  3.308 s  0x0E names this host, type 22
  3.350 s  audio, 339 bytes, header 04 00 04 00 58 00 01 00 49 01 58 00 01 00 43 01 18 4b 8f 8f 05 02
           unit ts 900 (0x00000384), 73 bytes
  3.377 s  audio, 343 bytes, header 04 00 04 00 58 00 01 00 4d 01 58 00 01 00 47 01 18 4b ac 8f 05 02
  3.422 s  audio, 341 bytes, header 04 00 04 00 58 00 01 00 4b 01 58 00 01 00 45 01 18 4b ca 8f 05 02
 13.005 s  sent stop   04 00 04 00 58 00 00 00 02 00 03 01
 13.262 s  0x58  04 00 04 00 58 00 00 00 02 00 04 01
 13.264 s  0x0E names this host, type 02
 13.290 s  0x55, 10 bytes
 15.388 s  speech level 03 (easing back)
 17.280 s  speech level 0b (unknown)
 17.385 s  speech level 04 (easing back)
 18.026 s  A2DP: playing
 18.026 s  switching the card off and back
switched bluez_card.<MAC> off and back to a2dp-sink
played org.mpris.MediaPlayer2.spotify again
 19.617 s  0x0E names address 2, type 00
 19.815 s  0x0E names this host, type 02
 19.820 s  0x4C, 11 bytes
 19.865 s  0x55, 10 bytes
 20.156 s  A2DP: playing

first audio 0.348 s after the start
audio packets: 322
  sizes (bytes: count): 327: 1, 329: 5, 331: 4, ... 350: 6, 351: 2
  access units per packet (units: count): 4: 322
access units: 1288, 68 to 78 bytes, 74.6 on average
  timestamp steps (step: count, most common first): 180: 1287
  header bytes that changed (offset: packets): 8: 291, 14: 291, 18: 321, 19: 318
frames came at 133.2 a second over 9.64 s: 63913 samples a second at 480 a frame
audio packets outside the recording: 2
ffprobe: codec_name=aac|profile=ELD|sample_rate=48000|channels=1
captures/2026-11-15/mic4-48k.wav: ffmpeg exit status: 0, 0 error lines
```

Byte 21 of the second and third headers was cut off in the terminal; it is the first
header's, as the changed offsets show. Both pods played after the switch. Spotify stayed
paused: the probe played it before
it had reported the pause, and the user started it again by hand. The probe did not tell
an address of all zeros apart then.

### Run 4

The same command, with a probe that played the player again once it reported the pause,
and switched once. Excerpt, the startup dump left out; as in run 2 it named the right
pod primary, and no other `0x06` or `0x08` came:

```
  3.002 s  sent start  04 00 04 00 58 00 00 00 09 00 00 01 82 00 00 00 04 96 00; recording for 10 s - speak
  3.306 s  0x58  04 00 04 00 58 00 00 00 03 00 01 01 01
  3.306 s  0x0E names this host, type 22
  3.346 s  audio, 337 bytes, header 04 00 04 00 58 00 01 00 47 01 58 00 01 00 41 01 18 4b 75 34 11 02
           unit ts 900 (0x00000384), 72 bytes
  3.371 s  audio, 333 bytes, header 04 00 04 00 58 00 01 00 43 01 58 00 01 00 3d 01 18 4b 93 34 11 02
  3.413 s  audio, 335 bytes, header 04 00 04 00 58 00 01 00 45 01 58 00 01 00 3f 01 18 4b b1 34 11 02
 13.004 s  sent stop   04 00 04 00 58 00 00 00 02 00 03 01
 13.211 s  0x58  04 00 04 00 58 00 00 00 02 00 04 01
 13.211 s  0x0E names this host, type 02
 13.232 s  0x55, 10 bytes
 18.009 s  A2DP: playing
 18.009 s  switching the card off and back
switched bluez_card.<MAC> off and back to a2dp-sink
org.mpris.MediaPlayer2.spotify Paused 0.21 s after the switch
played org.mpris.MediaPlayer2.spotify again
 19.555 s  0x0E names no device (all zeros), type 00
 20.539 s  A2DP: there, not playing

first audio 0.344 s after the start
audio packets: 322
  sizes (bytes: count): 326: 1, 327: 1, 329: 1, ... 352: 7, 353: 2
  access units per packet (units: count): 4: 322
access units: 1288, 68 to 78 bytes, 75.3 on average
  timestamp steps (step: count, most common first): 180: 1287
  header bytes that changed (offset: packets): 8: 314, 14: 314, 18: 319, 19: 317
frames came at 133.4 a second over 9.63 s: 64024 samples a second at 480 a frame
audio packets outside the recording: 1
ffprobe: codec_name=aac|profile=ELD|sample_rate=48000|channels=1
captures/2026-11-15/mic-mytest-48k.wav: ffmpeg exit status: 0, 0 error lines
```

Byte 21 of the second and third headers was cut off in the terminal; it is the first
header's, as the changed offsets show. Spotify showed itself playing, and neither pod
played. Switching the card off and back
by hand brought both back. In other runs of that probe the music came back on both
pods 3-5 s after the switch. The committed probe, which waits for a new transport and
switches again while A2DP does not play, has no run recorded here.

## Open questions

- What the start's payload selects, and whether other values give another rate, frame
  length or channel count.
- What the answers' payloads mean.
- What type `22` in `0x0E` means, and what `0x55` and `0x4C` carry here.
- Why the secondary pod goes silent after a stop, and what Apple's devices send or do so
  that it does not.
- Whether a packet ends the one-pod state after a stop without switching the A2DP
  profile.
- Why a card switch sometimes leaves both pods silent: whether playing too early after
  it does that, or the pods.
- What bytes 16-17 of an audio packet mean, and where the timestamp's start comes from.
- Whether an audio packet ever runs past 1 KB, as LibrePods' 4096-byte receive buffer
  suggests; the largest seen was 353 bytes.
- Whether the samples played at 64 kHz sound right.
- What the stream does when A2DP stops, when a pod leaves an ear or goes into the case,
  and when the session closes without a stop.
- Whether the noise control mode can be set while the microphone runs.
- Whether AirPods Pro 2 (USB-C) do the same.
