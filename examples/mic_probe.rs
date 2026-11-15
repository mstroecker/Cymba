//! Asks a pair of AirPods for their microphone over AAP and shows what comes
//! back: how the audio is framed, how fast the frames come, whether the system
//! FFmpeg decodes them, and what happens to A2DP - for #209. The packets are
//! LibrePods' (its PR #655); what a Pro 3 did with them is in
//! `docs/protocol/aap/microphone.md`.
//!
//! The run opens the session the app opens and reads for a few seconds, sends
//! the start, reads for `SECONDS`, sends the stop and reads a few seconds more.
//! It prints the first audio packets' headers, and each of their access units'
//! timestamp and length, every other packet as it comes, then a summary:
//! packets, access units, their sizes, the timestamp steps, the rate the
//! frames came at and the header bytes that changed. The A2DP transport is
//! read at the connection, after the start, at the stop, after it, and after
//! the switch below. Play something to the pair during the run.
//!
//! Every packet that is not the microphone's is printed by what it is: a
//! battery, ear or primary pod report, a noise mode, and so on. Of a packet
//! that names devices - `0x0C`, `0x0E`, `0x2E` - each address is named
//! instead: the pair, this host (any adapter's address), no device for all
//! zeros, or "address 1", "2" and on by the order they first came in. Any
//! other packet is printed as its opcode and length.
//!
//! After the stop the run switches the pair's card to the `off` profile and
//! back to the one it had, with `pactl`, and plays again every MPRIS player
//! that was playing before, once a new A2DP transport has come, or
//! `RESUME_WAIT` has passed, and the player reports the pause the switch
//! causes: on a Pro 3, one pod went
//! silent after the stop until the card was switched (`microphone.md`, A2DP).
//! With a player playing, it waits for the pair's A2DP to play, and switches
//! again if it does not, up to `SWITCH_TRIES` switches. It reads on through the
//! switches and a moment after them. With `--no-reset` it
//! leaves the pair as the stop left it.
//!
//! The access units go to `<OUT>.mka`, a Matroska file with the
//! AudioSpecificConfig as its codec private data. If `ffmpeg` is on the path,
//! the run decodes it with the system FFmpeg's own `aac` decoder into
//! `<OUT>-48k.wav` and `<OUT>-64k.wav` - the same samples, labeled with each
//! rate - and prints FFmpeg's errors, the samples decoded and their level; with
//! `ffprobe`, also what it makes of the stream.
//!
//! Quit Cymba first, so the probe's AAP session is the only one.
//!
//! Usage: cargo run --example mic_probe -- <MAC> <OUT> [SECONDS] [--no-reset]
//!
//! SECONDS is how long to record, 10 by default. The files hold whatever the
//! microphone heard: never attach them to an issue.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, bail};
use bluer::Address;
use cymba::aap::conversation::describe_level;
use cymba::aap::{Client, Message, decode, keys, mic};
use cymba::hex;
use cymba::media::{self, Playback};
use cymba::volume::find_a2dp;
use tokio::time::Instant;
use zbus::Connection;

// The packets that start and stop the stream, LibrePods', as the app sends
// them.
use cymba::aap::mic::{START as START_AUDIO, STOP as STOP_AUDIO};

/// The bytes before the first access unit.
const HEADER_LEN: usize = mic::AUDIO_HEADER_LEN;

/// The AudioSpecificConfig LibrePods gives the decoder: AAC-ELD, 48 kHz,
/// mono, 480 samples a frame. The frames come at 64 kHz's pace on a Pro 3:
/// `docs/protocol/aap/microphone.md`, The audio.
const ASC: [u8; 4] = mic::AUDIO_SPECIFIC_CONFIG;

/// Samples in a frame, as the AudioSpecificConfig says.
const FRAME_SAMPLES: u64 = mic::FRAME_SAMPLES as u64;

/// The rate the AudioSpecificConfig gives.
const ASC_RATE: u64 = 48_000;

/// How many audio packets' headers are printed.
const SHOWN: usize = 3;

/// How long to record, unless the command line says.
const DEFAULT_WINDOW: Duration = Duration::from_secs(10);

/// How many times the card is switched off and back, for the music to come
/// back to the pair.
const SWITCH_TRIES: u32 = 3;

/// How long one `pactl` call may take before it is killed.
const PACTL_LIMIT: Duration = Duration::from_secs(5);

/// How long the whole reset may take before it is given up: every switch's
/// two `pactl` calls and three waits, with room to spare.
const RESET_LIMIT: Duration = Duration::from_secs(90);

/// How long to read after the card switch and the player played again, for
/// what the pods send as the music comes back, or does not.
const AFTER_SWITCH: Duration = Duration::from_secs(5);

/// How long a new A2DP transport is given to come after the card switch, a
/// player that was playing to report its pause, and A2DP to play after the
/// player was played again, before each is left alone.
const RESUME_WAIT: Duration = Duration::from_secs(5);

/// How often the transport and the player are read meanwhile.
const RESUME_POLL: Duration = Duration::from_millis(100);

/// How long to read before the start: the startup dump, and the pods as they
/// were before the microphone.
const BEFORE_START: Duration = Duration::from_secs(3);

/// How long to read after the stop, for the answer, audio still coming, and
/// what the pods send as one goes silent.
const AFTER_STOP: Duration = Duration::from_secs(5);

/// How long after the start the A2DP transport is read again.
const A2DP_CHECK: Duration = Duration::from_secs(2);

/// Every access unit of an audio packet: the timestamp and the bytes. Also
/// the bytes left over that make no whole unit.
fn access_units(packet: &[u8]) -> (Vec<(u32, &[u8])>, usize) {
    let mut read = mic::access_units(packet);
    let units = read
        .by_ref()
        .map(|unit| (unit.timestamp, unit.frame))
        .collect();
    (units, read.leftover())
}

/// What was seen of the audio packets.
#[derive(Default)]
struct Tally {
    packets: usize,
    /// Packets with bytes after the last whole access unit.
    leftover: usize,
    sizes: BTreeMap<usize, usize>,
    units_per_packet: BTreeMap<usize, usize>,
    unit_sizes: Vec<usize>,
    ts_steps: BTreeMap<i64, usize>,
    last_ts: Option<u32>,
    first_header: Option<Vec<u8>>,
    /// Header positions that differed from the first packet's.
    header_changes: BTreeMap<usize, usize>,
    first_at: Option<Instant>,
    /// Access units in the first packet, which came at `first_at`.
    first_units: usize,
    last_at: Option<Instant>,
    units: Vec<Vec<u8>>,
}

impl Tally {
    fn add(&mut self, packet: &[u8], at: Instant) {
        self.packets += 1;
        *self.sizes.entry(packet.len()).or_default() += 1;
        self.first_at.get_or_insert(at);
        self.last_at = Some(at);
        let header = &packet[..HEADER_LEN.min(packet.len())];
        match &self.first_header {
            None => self.first_header = Some(header.to_vec()),
            Some(first) => {
                for (i, (a, b)) in first.iter().zip(header).enumerate() {
                    if a != b {
                        *self.header_changes.entry(i).or_default() += 1;
                    }
                }
            }
        }
        let (units, leftover) = access_units(packet);
        if self.packets == 1 {
            self.first_units = units.len();
        }
        if leftover > 0 {
            self.leftover += 1;
        }
        *self.units_per_packet.entry(units.len()).or_default() += 1;
        for (ts, unit) in units {
            if let Some(last) = self.last_ts {
                *self
                    .ts_steps
                    .entry(i64::from(ts) - i64::from(last))
                    .or_default() += 1;
            }
            self.last_ts = Some(ts);
            self.unit_sizes.push(unit.len());
            self.units.push(unit.to_vec());
        }
    }

    fn print(&self) {
        println!("audio packets: {}", self.packets);
        if self.packets == 0 {
            return;
        }
        println!("  sizes (bytes: count): {}", counts(&self.sizes));
        println!(
            "  access units per packet (units: count): {}",
            counts(&self.units_per_packet)
        );
        println!(
            "  packets with bytes after the last whole unit: {}",
            self.leftover
        );
        let n = self.unit_sizes.len();
        if n > 0 {
            let min = self.unit_sizes.iter().min().expect("not empty");
            let max = self.unit_sizes.iter().max().expect("not empty");
            let mean = self.unit_sizes.iter().sum::<usize>() as f64 / n as f64;
            println!("access units: {n}, {min} to {max} bytes, {mean:.1} on average");
        }
        let mut steps: Vec<_> = self.ts_steps.iter().collect();
        steps.sort_by(|a, b| b.1.cmp(a.1));
        let steps: Vec<_> = steps
            .iter()
            .take(6)
            .map(|(step, count)| format!("{step}: {count}"))
            .collect();
        println!(
            "  timestamp steps (step: count, most common first): {}",
            steps.join(", ")
        );
        if let Some(first) = &self.first_header {
            println!("first header: {}", hex(first));
        }
        if self.header_changes.is_empty() {
            println!("  no header byte changed from the first packet's");
        } else {
            println!(
                "  header bytes that changed (offset: packets): {}",
                counts(&self.header_changes)
            );
        }
        if let (Some(first), Some(last)) = (self.first_at, self.last_at) {
            let secs = (last - first).as_secs_f64();
            if secs > 0.0 {
                // The units that came after the first packet's arrival, up to
                // the last's.
                let rate = (n - self.first_units) as f64 / secs;
                println!(
                    "frames came at {rate:.1} a second over {secs:.2} s: {:.0} samples a second at {FRAME_SAMPLES} a frame",
                    rate * FRAME_SAMPLES as f64
                );
            }
        }
    }
}

fn counts<K: std::fmt::Display>(map: &BTreeMap<K, usize>) -> String {
    map.iter()
        .map(|(k, v)| format!("{k}: {v}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The A2DP transport of the pair, in a line.
async fn a2dp(conn: &Option<Connection>, mac: Address) -> String {
    let Some(conn) = conn else {
        return "not read: no system bus".into();
    };
    match find_a2dp(conn, mac).await {
        Ok(Some(a2dp)) if a2dp.transport.playing => "playing".into(),
        Ok(Some(_)) => "there, not playing".into(),
        Ok(None) => "none".into(),
        Err(e) => format!("not read: {e:#}"),
    }
}

/// Runs `pactl` with `args`, untranslated, and returns what it printed. Killed
/// if it takes longer than [`PACTL_LIMIT`].
async fn pactl(args: &[&str]) -> anyhow::Result<String> {
    let mut child = Command::new("pactl")
        .env("LC_ALL", "C")
        .args(args)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .context("failed to run pactl")?;
    // Read meanwhile, so a long listing never fills the pipe and stalls it.
    let mut stdout = child.stdout.take().context("pactl has no output")?;
    let reader = std::thread::spawn(move || {
        use std::io::Read;
        let mut out = String::new();
        stdout.read_to_string(&mut out).map(|_| out)
    });
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().context("failed to wait for pactl")? {
            break status;
        }
        if started.elapsed() >= PACTL_LIMIT {
            let _ = child.kill();
            let _ = child.wait();
            bail!("pactl {} took over {PACTL_LIMIT:?}", args.join(" "));
        }
        tokio::time::sleep(RESUME_POLL).await;
    };
    let out = reader
        .join()
        .map_err(|_| anyhow::anyhow!("pactl's reader panicked"))?
        .context("failed to read pactl's output")?;
    anyhow::ensure!(status.success(), "pactl {}: {status}", args.join(" "));
    Ok(out)
}

/// The pair's card in PipeWire's PulseAudio server, and its active profile,
/// as `pactl` lists them.
async fn card(mac: Address) -> anyhow::Result<(String, String)> {
    let card = format!("bluez_card.{}", mac.to_string().replace(':', "_"));
    let text = pactl(&["list", "cards"]).await?;
    let name = format!("Name: {card}");
    let profile = text
        .lines()
        .skip_while(|line| line.trim() != name)
        .find_map(|line| line.trim().strip_prefix("Active Profile: "))
        .with_context(|| format!("no card {card} with an active profile"))?
        .to_string();
    Ok((card, profile))
}

/// The path of the pair's A2DP transport, `None` while there is none.
async fn transport(system: &Connection, mac: Address) -> anyhow::Result<Option<String>> {
    Ok(find_a2dp(system, mac)
        .await?
        .map(|a2dp| a2dp.path.to_string()))
}

/// Switches the pair's card off and back to its profile, and plays again every
/// MPRIS player that was playing before the switch stopped it. With a player
/// playing, it then waits for the pair's A2DP to play too, and switches again
/// if it does not, up to [`SWITCH_TRIES`] switches in all.
async fn reset(mac: Address) -> anyhow::Result<()> {
    // Only the replay needs the session bus.
    let session = Connection::session().await.ok();
    let system = Connection::system()
        .await
        .context("failed to connect to the system bus")?;
    let playing: Vec<_> = match &session {
        Some(session) => media::list_players(session, media::CALL_LIMIT)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|player| player.status == Playback::Playing)
            .collect(),
        None => {
            println!("  no session bus: no player played again");
            Vec::new()
        }
    };
    let (card, profile) = card(mac).await?;
    for attempt in 1..=SWITCH_TRIES {
        let before = transport(&system, mac).await?;
        pactl(&["set-card-profile", &card, "off"]).await?;
        pactl(&["set-card-profile", &card, &profile]).await?;
        let switched = Instant::now();
        let since = || switched.elapsed().as_secs_f64();
        println!("switch {attempt}: {card} off and back to {profile}");
        // Whether playing before A2DP is set up again leaves both pods silent
        // is open (microphone.md, Open questions): wait for a new transport
        // first - one that went and came back, or another path.
        let mut went = before.is_none();
        loop {
            // A read that fails counts as not yet.
            match transport(&system, mac).await {
                Ok(Some(path)) if went || Some(&path) != before.as_ref() => {
                    println!("  a new A2DP transport {:.2} s after the switch", since());
                    break;
                }
                Ok(Some(_)) | Err(_) => {}
                Ok(None) => went = true,
            }
            if switched.elapsed() >= RESUME_WAIT {
                println!(
                    "  no new A2DP transport {:.2} s after the switch ({})",
                    since(),
                    if matches!(transport(&system, mac).await, Ok(Some(_))) {
                        "the old one stayed"
                    } else {
                        "none"
                    }
                );
                break;
            }
            tokio::time::sleep(RESUME_POLL).await;
        }
        let Some(session) = &session else {
            return Ok(());
        };
        if playing.is_empty() {
            println!("  nothing was playing: nothing to play again or to check");
            return Ok(());
        }
        for player in &playing {
            resume(session, player, switched).await;
        }
        // Whether the music reaches the pair: the transport plays.
        let played = Instant::now();
        loop {
            if find_a2dp(&system, mac)
                .await
                .ok()
                .flatten()
                .is_some_and(|a2dp| a2dp.transport.playing)
            {
                println!(
                    "  A2DP playing {:.2} s after the switch, {:.2} s after the play",
                    since(),
                    played.elapsed().as_secs_f64()
                );
                return Ok(());
            }
            if played.elapsed() >= RESUME_WAIT {
                println!(
                    "  A2DP not playing {:.2} s after the play",
                    played.elapsed().as_secs_f64()
                );
                break;
            }
            tokio::time::sleep(RESUME_POLL).await;
        }
    }
    println!("A2DP not playing after {SWITCH_TRIES} switches: switch by hand");
    Ok(())
}

/// Waits up to [`RESUME_WAIT`] from `switched` for `player` to report the
/// pause the card switch causes, and plays it again only then: played at
/// once, Spotify still reported Playing and paused only after (microphone.md,
/// run 3). A read that fails counts as not yet.
async fn resume(session: &Connection, player: &media::Player, switched: Instant) {
    let deadline = switched + RESUME_WAIT;
    let status = loop {
        let read = media::list_players(session, media::CALL_LIMIT).await;
        let status = read
            .as_ref()
            .ok()
            .map(|players| players.iter().find(|p| p.id == player.id).map(|p| p.status));
        match status {
            None | Some(Some(Playback::Playing)) if Instant::now() < deadline => {
                tokio::time::sleep(RESUME_POLL).await;
            }
            other => break other.flatten(),
        }
    };
    let after = switched.elapsed().as_secs_f64();
    match status {
        None => println!("  {} went away, or does not read", player.name),
        Some(Playback::Playing) => {
            println!(
                "  {} still playing {after:.2} s after the switch",
                player.name
            );
        }
        Some(status) => {
            println!("  {} {status:?} {after:.2} s after the switch", player.name);
            match media::call(session, player, "Play", media::CALL_LIMIT).await {
                Ok(()) => println!("  played {} again", player.name),
                Err(e) => println!("  {} not played again: {e:#}", player.name),
            }
        }
    }
}

// Matroska, as little of it as FFmpeg needs to read the access units with the
// AudioSpecificConfig: an EBML header, the track, and clusters of blocks.

fn ebml_uint(v: u64) -> Vec<u8> {
    let bytes = v.to_be_bytes();
    let skip = bytes.iter().take_while(|&&b| b == 0).count().min(7);
    bytes[skip..].to_vec()
}

/// An element: its ID as written, its size as an 8-byte vint, its body.
fn element(id: u32, body: &[u8]) -> Vec<u8> {
    let id = id.to_be_bytes();
    let skip = id.iter().take_while(|&&b| b == 0).count();
    let mut out = id[skip..].to_vec();
    out.push(0x01);
    out.extend_from_slice(&(body.len() as u64).to_be_bytes()[1..]);
    out.extend_from_slice(body);
    out
}

fn matroska(units: &[Vec<u8>]) -> Vec<u8> {
    // A frame's length in ms at the AudioSpecificConfig's rate.
    let frame_ms = FRAME_SAMPLES * 1000 / ASC_RATE;
    let header = [
        element(0x4286, &ebml_uint(1)),
        element(0x42F7, &ebml_uint(1)),
        element(0x42F2, &ebml_uint(4)),
        element(0x42F3, &ebml_uint(8)),
        element(0x4282, b"matroska"),
        element(0x4287, &ebml_uint(4)),
        element(0x4285, &ebml_uint(2)),
    ]
    .concat();
    let info = element(
        0x1549_A966,
        &[
            element(0x2A_D7B1, &ebml_uint(1_000_000)),
            element(0x4D80, b"cymba mic_probe"),
            element(0x5741, b"cymba mic_probe"),
        ]
        .concat(),
    );
    let audio = element(
        0xE1,
        &[
            element(0xB5, &(ASC_RATE as f64).to_be_bytes()),
            element(0x9F, &ebml_uint(1)),
        ]
        .concat(),
    );
    let track = element(
        0xAE,
        &[
            element(0xD7, &ebml_uint(1)),
            element(0x73C5, &ebml_uint(1)),
            element(0x83, &ebml_uint(2)),
            element(0x86, b"A_AAC"),
            element(0x63A2, &ASC),
            audio,
        ]
        .concat(),
    );
    let mut segment = [info, element(0x1654_AE6B, &track)].concat();
    // A block's time is 16 bits relative to its cluster's: a cluster per
    // 1000 frames keeps it well inside.
    for (c, chunk) in units.chunks(1000).enumerate() {
        let start = c as u64 * 1000 * frame_ms;
        let mut cluster = element(0xE7, &ebml_uint(start));
        for (i, unit) in chunk.iter().enumerate() {
            let rel = (i as u64 * frame_ms) as i16;
            let mut block = vec![0x81];
            block.extend_from_slice(&rel.to_be_bytes());
            block.push(0x80);
            block.extend_from_slice(unit);
            cluster.extend(element(0xA3, &block));
        }
        segment.extend(element(0x1F43_B675, &cluster));
    }
    [
        element(0x1A45_DFA3, &header),
        element(0x1853_8067, &segment),
    ]
    .concat()
}

/// `path` with `suffix` after its file name.
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

/// Decodes `mka` with the system FFmpeg into a WAV file labeled with `rate`,
/// and prints what came of it.
fn decode_wav(mka: &Path, rate: u64) -> anyhow::Result<()> {
    let wav = with_suffix(&mka.with_extension(""), &format!("-{}k.wav", rate / 1000));
    let mut cmd = Command::new("ffmpeg");
    cmd.args([
        "-hide_banner",
        "-nostdin",
        "-y",
        "-v",
        "error",
        "-c:a",
        "aac",
        "-i",
    ])
    .arg(mka);
    if rate != ASC_RATE {
        cmd.args(["-af", &format!("asetrate={rate}")]);
    }
    let out = cmd
        .args(["-c:a", "pcm_s16le"])
        .arg(&wav)
        .output()
        .context("failed to run ffmpeg")?;
    let errors = String::from_utf8_lossy(&out.stderr);
    let lines: Vec<_> = errors.lines().collect();
    println!(
        "{}: ffmpeg {}, {} error lines",
        wav.display(),
        out.status,
        lines.len()
    );
    for line in lines.iter().take(5) {
        println!("  {line}");
    }
    if !out.status.success() {
        return Ok(());
    }
    let bytes = std::fs::read(&wav).with_context(|| format!("failed to read {}", wav.display()))?;
    // FFmpeg's WAV header: the format chunk, its channel count 10 bytes in,
    // then the data chunk.
    let (Some(fmt), Some(data)) = (
        bytes.windows(4).position(|w| w == b"fmt "),
        bytes.windows(4).position(|w| w == b"data"),
    ) else {
        bail!("no format or data chunk in {}", wav.display());
    };
    let channels = u16::from_le_bytes([bytes[fmt + 10], bytes[fmt + 11]]).max(1);
    let samples: Vec<i16> = bytes[data + 8..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&b| i16::from_le_bytes(b))
        .collect();
    let peak = samples.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
    let rms = (samples.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>()
        / samples.len().max(1) as f64)
        .sqrt();
    let db = |v: f64| 20.0 * (v.max(1.0) / 32768.0).log10();
    let frames = samples.len() / usize::from(channels);
    println!(
        "  {frames} samples, {channels} channel(s), {:.2} s at {rate} Hz; peak {:.1} dBFS, RMS {:.1} dBFS",
        frames as f64 / rate as f64,
        db(f64::from(peak)),
        db(rms)
    );
    Ok(())
}

/// What `ffprobe` makes of the stream: codec, profile, rate, channels.
fn probe(mka: &Path) -> anyhow::Result<()> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_name,profile,sample_rate,channels",
            "-of",
            "compact=p=0",
        ])
        .arg(mka)
        .output()
        .context("failed to run ffprobe")?;
    println!(
        "ffprobe: {}{}",
        String::from_utf8_lossy(&out.stdout).trim(),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(())
}

/// Where the run is.
#[derive(Clone, Copy)]
enum Phase {
    /// Reading what the pods send before the start.
    Before,
    /// Recording, from the start; `checked` once the A2DP transport was read.
    Recording { start: Instant, checked: bool },
    /// Reading on after the stop, sent at this time.
    AfterStop(Instant),
    /// Waiting for the card switch, begun at this time.
    Resetting(Instant),
    /// Reading on after the switch, done at this time.
    Settling(Instant),
}

enum Event {
    Packet(anyhow::Result<Vec<u8>>),
    /// The phase's time is up.
    Due,
    Reset(anyhow::Result<()>),
}

/// Every adapter's address, to tell this host in the packets that name
/// devices.
async fn host_addresses(bus: &Option<Connection>) -> Vec<Address> {
    let Some(bus) = bus else {
        return Vec::new();
    };
    match cymba::bluez::adapters(bus).await {
        Ok(adapters) => adapters.into_iter().filter_map(|a| a.address).collect(),
        Err(_) => Vec::new(),
    }
}

/// What a packet that is not the microphone's is, with every address in it
/// named rather than printed: the pair, this host, or another by the order
/// it first came in.
struct Names {
    pair: [u8; 6],
    hosts: Vec<[u8; 6]>,
    others: Vec<[u8; 6]>,
}

impl Names {
    fn new(pair: Address, hosts: Vec<Address>) -> Self {
        Self {
            pair: pair.0,
            hosts: hosts.into_iter().map(|a| a.0).collect(),
            others: Vec::new(),
        }
    }

    /// An address, most significant byte first.
    fn name(&mut self, address: [u8; 6]) -> String {
        if address == [0; 6] {
            return "no device (all zeros)".into();
        }
        if address == self.pair {
            return "the pair".into();
        }
        if self.hosts.contains(&address) {
            return "this host".into();
        }
        let n = match self.others.iter().position(|a| *a == address) {
            Some(n) => n,
            None => {
                self.others.push(address);
                self.others.len() - 1
            }
        };
        format!("address {}", n + 1)
    }

    /// An address least significant byte first, as `0x0C` and `0x0E` carry it.
    fn name_reversed(&mut self, bytes: &[u8]) -> String {
        let mut address: [u8; 6] = bytes.try_into().expect("6 bytes");
        address.reverse();
        self.name(address)
    }

    fn describe(&mut self, packet: &[u8]) -> String {
        match decode(packet) {
            Ok(Message::Battery(b)) => return format!("battery {b:?}"),
            Ok(Message::Ear(e)) => {
                return format!("ear: primary {:?}, secondary {:?}", e.primary, e.secondary);
            }
            Ok(Message::Primary(p)) => return format!("primary pod {p:?}"),
            Ok(Message::NoiseMode(m)) => return format!("noise mode {m}"),
            Ok(Message::ConversationAwareness(on)) => {
                return format!("Conversation Awareness {}", if on { "on" } else { "off" });
            }
            Ok(Message::SpeechLevel(l)) => return format!("speech level {}", describe_level(l)),
            Ok(Message::DeviceInfo(_)) => return "0x1D device information".into(),
            Ok(Message::Keys(_)) => return "0x31 keys, not printed".into(),
            _ => {}
        }
        let Some(&opcode) = packet.get(4) else {
            return format!("{} bytes, no opcode", packet.len());
        };
        match opcode {
            0x0C if packet.len() >= 14 => format!(
                "0x0C names {}, ending {}",
                self.name_reversed(&packet[6..12]),
                hex(&packet[12..14])
            ),
            0x0E if packet.len() >= 13 => format!(
                "0x0E names {}, type {:02x}",
                self.name_reversed(&packet[6..12]),
                packet[12]
            ),
            0x2E if packet.len() >= 9 => {
                let count = usize::from(packet[8]);
                let entries: Vec<_> = packet[9..]
                    .as_chunks::<8>()
                    .0
                    .iter()
                    .take(count)
                    .map(|e| {
                        let address: [u8; 6] = e[..6].try_into().expect("6 bytes");
                        format!("{} {}", self.name(address), hex(&e[6..]))
                    })
                    .collect();
                format!("0x2E hosts: {}", entries.join(", "))
            }
            _ => format!("0x{opcode:02X}, {} bytes", packet.len()),
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "cymba=info".into()),
        )
        .init();

    let all: Vec<String> = std::env::args().skip(1).collect();
    let no_reset = all.iter().any(|a| a == "--no-reset");
    let args: Vec<&String> = all.iter().filter(|a| *a != "--no-reset").collect();
    let usage = "usage: mic_probe <MAC> <OUT> [SECONDS] [--no-reset]";
    let arg = args.first().context(usage)?;
    let mac: Address = arg
        .parse()
        .with_context(|| format!("not a MAC address: {arg}"))?;
    let out = PathBuf::from(args.get(1).context(usage)?.as_str());
    let window = match args.get(2) {
        Some(seconds) => Duration::from_secs(
            seconds
                .parse()
                .with_context(|| format!("not a number of seconds: {seconds}"))?,
        ),
        None => DEFAULT_WINDOW,
    };
    let mka = with_suffix(&out, ".mka");
    if let Some(dir) = mka.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
    }

    let bus = Connection::system().await.ok();
    let hosts = host_addresses(&bus).await;
    println!("connecting AAP to {mac}...");
    // The session the app opens, the features included, without the keys.
    let client = Client::new(mac).open().await?;
    let opened = Instant::now();
    let since = |at: Instant| (at - opened).as_secs_f64();
    println!(
        "{:7.3} s  connected; A2DP: {}",
        since(Instant::now()),
        a2dp(&bus, mac).await
    );
    let mut names = Names::new(mac, hosts);

    let mut tally = Tally::default();
    let mut after_stop = 0;
    let mut phase = Phase::Before;
    let mut started = None;
    let mut resetting: Option<tokio::task::JoinHandle<anyhow::Result<()>>> = None;
    loop {
        let deadline = match phase {
            Phase::Before => opened + BEFORE_START,
            Phase::Recording { start, checked } => {
                let stop = start + window;
                if checked {
                    stop
                } else {
                    (start + A2DP_CHECK).min(stop)
                }
            }
            Phase::AfterStop(stopped) => stopped + AFTER_STOP,
            Phase::Resetting(began) => began + RESET_LIMIT,
            Phase::Settling(done) => done + AFTER_SWITCH,
        };
        let event = tokio::select! {
            read = client.read_packet() => Event::Packet(read),
            () = tokio::time::sleep_until(deadline) => Event::Due,
            done = async { resetting.as_mut().expect("resetting").await }, if resetting.is_some() => {
                resetting = None;
                Event::Reset(done.map_err(anyhow::Error::from).and_then(|r| r))
            }
        };
        let now = Instant::now();
        match event {
            Event::Packet(Err(e)) => {
                println!("read failed: {e:#}");
                break;
            }
            Event::Packet(Ok(packet)) => {
                if mic::is_audio_packet(&packet) {
                    if !matches!(phase, Phase::Recording { .. }) {
                        after_stop += 1;
                        continue;
                    }
                    if tally.packets < SHOWN {
                        let (units, leftover) = access_units(&packet);
                        println!(
                            "{:7.3} s  audio, {} bytes, header {}",
                            since(now),
                            packet.len(),
                            hex(&packet[..HEADER_LEN.min(packet.len())])
                        );
                        for (ts, unit) in &units {
                            println!("           unit ts {ts} ({ts:#010x}), {} bytes", unit.len());
                        }
                        if leftover > 0 {
                            println!("           {leftover} bytes after the last whole unit");
                        }
                    }
                    tally.add(&packet, now);
                } else if packet.get(4..6) == Some(&[0x58, 0x00]) {
                    // Not audio: the answer to the start or the stop.
                    println!(
                        "{:7.3} s  0x58  {}",
                        since(now),
                        keys::redacted_hex(&packet)
                    );
                } else {
                    println!("{:7.3} s  {}", since(now), names.describe(&packet));
                }
            }
            Event::Due => match phase {
                Phase::Before => {
                    client.send_unchecked(&START_AUDIO).await?;
                    started = Some(now);
                    println!(
                        "{:7.3} s  sent start  {}; recording for {} s - speak",
                        since(now),
                        hex(&START_AUDIO),
                        window.as_secs()
                    );
                    phase = Phase::Recording {
                        start: now,
                        checked: false,
                    };
                }
                Phase::Recording { start, checked } if !checked && now < start + window => {
                    println!("{:7.3} s  A2DP: {}", since(now), a2dp(&bus, mac).await);
                    phase = Phase::Recording {
                        start,
                        checked: true,
                    };
                }
                Phase::Recording { .. } => {
                    println!("{:7.3} s  A2DP: {}", since(now), a2dp(&bus, mac).await);
                    client.send_unchecked(&STOP_AUDIO).await?;
                    let sent = Instant::now();
                    println!("{:7.3} s  sent stop   {}", since(sent), hex(&STOP_AUDIO));
                    phase = Phase::AfterStop(sent);
                }
                Phase::AfterStop(_) => {
                    println!("{:7.3} s  A2DP: {}", since(now), a2dp(&bus, mac).await);
                    if no_reset {
                        break;
                    }
                    println!("{:7.3} s  switching the card off and back", since(now));
                    resetting = Some(tokio::spawn(reset(mac)));
                    phase = Phase::Resetting(now);
                }
                Phase::Resetting(_) => {
                    if let Some(reset) = resetting.take() {
                        reset.abort();
                    }
                    println!("{:7.3} s  reset given up after {RESET_LIMIT:?}", since(now));
                    phase = Phase::Settling(now);
                }
                Phase::Settling(_) => {
                    println!("{:7.3} s  A2DP: {}", since(now), a2dp(&bus, mac).await);
                    break;
                }
            },
            Event::Reset(done) => {
                if let Err(e) = done {
                    println!("{:7.3} s  reset stopped: {e:#}", since(now));
                }
                phase = Phase::Settling(now);
            }
        }
    }

    // A loop that ended mid-reset - the link gone - lets the switch finish,
    // so the card is not left off.
    if let Some(reset) = resetting.take() {
        match tokio::time::timeout(RESET_LIMIT, reset).await {
            Ok(Ok(Ok(()))) => println!("reset finished after the link went"),
            Ok(Ok(Err(e))) => println!("reset stopped: {e:#}"),
            Ok(Err(e)) => println!("reset ended: {e}"),
            Err(_) => println!("reset given up after {RESET_LIMIT:?}"),
        }
    }

    println!();
    if let (Some(first), Some(start)) = (tally.first_at, started) {
        println!(
            "first audio {:.3} s after the start",
            (first - start).as_secs_f64()
        );
    }
    tally.print();
    println!("audio packets outside the recording: {after_stop}");
    drop(client);

    if tally.units.is_empty() {
        println!("no audio: nothing written");
        return Ok(());
    }
    std::fs::write(&mka, matroska(&tally.units))
        .with_context(|| format!("failed to write {}", mka.display()))?;
    println!("wrote {}", mka.display());
    if let Err(e) = probe(&mka) {
        println!("{e:#}");
    }
    for rate in [ASC_RATE, 64_000] {
        if let Err(e) = decode_wav(&mka, rate) {
            println!("{e:#}");
        }
    }
    Ok(())
}
