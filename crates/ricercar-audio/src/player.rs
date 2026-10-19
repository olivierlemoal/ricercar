use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver as StdReceiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender as CbSender, TryRecvError};

use crate::buffering::Monitor;
use crate::device::{DeviceInfo, DeviceKind};
use crate::dsd::{DsdFormat, DsdMode, dop};
use crate::error::AudioError;
use crate::fmt::{PcmFormat, Wire};
pub use crate::gain::{GainStage, TrackOpts};
use crate::redact::redact_url;
use crate::sink::{AudioSink, device_info, make_sink};
use crate::stream::TrackSource;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportStatus {
    Stopped,
    Playing,
    Paused,
}

#[derive(Debug)]
pub enum EngineCommand {
    Load {
        uri: String,
        opts: TrackOpts,
    },
    EnqueueNext {
        uri: String,
        opts: TrackOpts,
    },
    Pause,
    Resume,
    Stop,
    Seek {
        ms: u64,
    },
    SetVolume {
        percent: u32,
    },
    /// Global preamp in dB, added to each track's own gain.
    SetGain {
        db: f32,
    },
    SetMute {
        muted: bool,
    },
    /// How DSD tracks opened from now on reach this device.
    SetDsdMode {
        mode: DsdMode,
    },
}

/// Why a track stopped playing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    /// Played to the end.
    Finished,
    /// `Stop` command.
    Stopped,
    /// Another track was loaded over it.
    Replaced,
    /// Decode or device error.
    Error,
}

#[derive(Debug, Clone, PartialEq)]
pub enum EngineEvent {
    Status {
        status: TransportStatus,
    },
    TrackStarted {
        uri: String,
        format: Option<PcmFormat>,
    },
    TrackEnded {
        uri: String,
        reason: EndReason,
    },
    Position {
        pos_ms: u64,
        dur_ms: Option<u64>,
    },
    /// Title announced in-band by an internet radio (ICY metadata).
    StreamTitle {
        title: String,
    },
    /// The engine has been waiting for a network source for a moment
    /// (before its first sample, after a seek, on underrun), or plays again.
    Buffering {
        uri: String,
        waiting: bool,
    },
    /// Byte ranges of a spooled HTTP body downloaded so far (sorted,
    /// half-open), sent when they change, a few times a second at most.
    Buffered {
        uri: String,
        ranges: Vec<(u64, u64)>,
        total: u64,
    },
    Error {
        message: String,
        /// The source that failed to open, when the error is about one.
        uri: Option<String>,
        /// HTTP status of a failed fetch (401, 403, 404, 410…).
        http_status: Option<u16>,
    },
    /// Volume and mute as the output itself has them (a network renderer,
    /// turned from its own knob or remote).
    Volume {
        percent: u32,
        muted: bool,
    },
}

pub struct Subscriber(pub StdReceiver<EngineEvent>);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panicking reader must not take the engine down with it.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Minimal fan-out hub (UI, UPnP and MPRIS watch the same engine).
#[derive(Default)]
pub struct EventHub {
    subs: Mutex<Vec<Sender<EngineEvent>>>,
}

impl EventHub {
    pub fn subscribe(&self) -> Subscriber {
        let (tx, rx) = mpsc::channel();
        lock(&self.subs).push(tx);
        Subscriber(rx)
    }
    pub fn publish(&self, ev: EngineEvent) {
        lock(&self.subs).retain(|s| s.send(ev.clone()).is_ok());
    }
}

/// A DSD track in the chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DsdChain {
    pub source: DsdFormat,
    /// How it reaches the device (after the guard: never DoP or native on
    /// a shared device). DoP and native run at unity gain whatever the
    /// volume, mute, ReplayGain and preamp say.
    pub mode: DsdMode,
}

impl DsdChain {
    /// Volume and gain are bypassed: the bits go out untouched.
    pub fn gain_locked(&self) -> bool {
        self.mode.is_bit_perfect()
    }
}

#[derive(Debug, Clone)]
pub struct ChainInfo {
    pub device: String,
    pub device_kind: DeviceKind,
    pub format: Option<PcmFormat>,
    /// Container actually negotiated with the device (e.g. `S24_3LE`).
    pub container: Option<&'static str>,
    pub volume: u32,
    /// Lossless all the way: native rate, lossless container, unity gain
    /// (volume 100, no ReplayGain/preamp, not muted), and a device kind that
    /// excludes mixing/resampling. DSD: DoP or native (converted PCM never
    /// is).
    pub bit_perfect: bool,
    /// Set while a DSD track plays.
    pub dsd: Option<DsdChain>,
}

#[derive(Debug, Clone)]
pub struct PlayerShared {
    pub status: TransportStatus,
    pub track_uri: Option<String>,
    pub next_uri: Option<String>,
    pub pos_ms: u64,
    pub dur_ms: Option<u64>,
    pub seekable: bool,
    pub chain: ChainInfo,
    /// Current gain stage (volume, preamp, track gain/peak, mute).
    pub gain: GainStage,
    /// Last in-band stream title of the current track (internet radio).
    pub stream_title: Option<String>,
}

pub struct PlayerHandle {
    tx: CbSender<EngineCommand>,
    pub state: Arc<Mutex<PlayerShared>>,
    pub hub: Arc<EventHub>,
    alive: Arc<AtomicBool>,
}

impl PlayerHandle {
    fn send(&self, cmd: EngineCommand) {
        let _ = self.tx.send(cmd);
    }
    pub fn load(&self, uri: impl Into<String>) {
        self.load_with(uri, TrackOpts::default());
    }
    pub fn load_with(&self, uri: impl Into<String>, opts: TrackOpts) {
        self.send(EngineCommand::Load {
            uri: uri.into(),
            opts,
        });
    }
    pub fn enqueue_next(&self, uri: impl Into<String>) {
        self.enqueue_next_with(uri, TrackOpts::default());
    }
    pub fn enqueue_next_with(&self, uri: impl Into<String>, opts: TrackOpts) {
        self.send(EngineCommand::EnqueueNext {
            uri: uri.into(),
            opts,
        });
    }
    pub fn pause(&self) {
        self.send(EngineCommand::Pause);
    }
    pub fn resume(&self) {
        self.send(EngineCommand::Resume);
    }
    pub fn stop(&self) {
        self.send(EngineCommand::Stop);
    }
    pub fn seek_ms(&self, ms: u64) {
        self.send(EngineCommand::Seek { ms });
    }
    pub fn set_volume(&self, percent: u32) {
        self.send(EngineCommand::SetVolume {
            percent: percent.min(100),
        });
    }
    /// Global preamp in dB (0.0 = off).
    pub fn set_gain(&self, db: f32) {
        self.send(EngineCommand::SetGain { db });
    }
    pub fn set_mute(&self, muted: bool) {
        self.send(EngineCommand::SetMute { muted });
    }
    /// DSD output mode for this device; DoP and native fall back to PCM on
    /// anything but a `hw:` device (or the test sinks).
    pub fn set_dsd_mode(&self, mode: DsdMode) {
        self.send(EngineCommand::SetDsdMode { mode });
    }
    pub fn subscribe(&self) -> Subscriber {
        self.hub.subscribe()
    }
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }
}

/// Spawn the audio engine on its own thread; a network output (`upnp:…`,
/// `plugin:…`) gets the driver registered for its prefix with
/// [`register_remote_driver`].
pub fn spawn_player(device_name: &str) -> PlayerHandle {
    let device = device_info(device_name);
    if device.kind == DeviceKind::Network {
        return spawn_remote(device);
    }
    spawn_player_with_sink(make_sink(&device))
}

fn initial_state(device: &DeviceInfo, gain: GainStage) -> PlayerShared {
    PlayerShared {
        status: TransportStatus::Stopped,
        track_uri: None,
        next_uri: None,
        pos_ms: 0,
        dur_ms: None,
        seekable: false,
        chain: chain_of(device, None, None, &gain, None),
        gain,
        stream_title: None,
    }
}

/// What drives a network output: its commands, and the state and events to
/// keep up to date the way the local engine does.
pub struct RemoteEngine {
    pub device: String,
    pub commands: Receiver<EngineCommand>,
    pub state: Arc<Mutex<PlayerShared>>,
    pub hub: Arc<EventHub>,
}

/// Runs a network output until its command channel closes.
pub type RemoteDriver = Arc<dyn Fn(RemoteEngine) + Send + Sync>;

/// Drivers by device-name prefix.
static REMOTE_DRIVERS: Mutex<Vec<(String, RemoteDriver)>> = Mutex::new(Vec::new());

/// Who drives the network outputs whose device name starts with `prefix`
/// (`upnp:`); register before the first one is spawned. A later call for
/// the same prefix replaces the driver.
pub fn register_remote_driver(prefix: &str, driver: RemoteDriver) {
    let mut drivers = lock(&REMOTE_DRIVERS);
    drivers.retain(|(p, _)| p != prefix);
    drivers.push((prefix.to_string(), driver));
}

/// The driver of `device`: the one with the longest matching prefix.
fn remote_driver(device: &str) -> Option<RemoteDriver> {
    lock(&REMOTE_DRIVERS)
        .iter()
        .filter(|(p, _)| device.starts_with(p.as_str()))
        .max_by_key(|(p, _)| p.len())
        .map(|(_, d)| d.clone())
}

fn spawn_remote(device: DeviceInfo) -> PlayerHandle {
    let (tx, rx) = crossbeam_channel::unbounded::<EngineCommand>();
    let hub = Arc::new(EventHub::default());
    let alive = Arc::new(AtomicBool::new(true));
    let state = Arc::new(Mutex::new(initial_state(&device, GainStage::default())));
    let engine = RemoteEngine {
        device: device.name,
        commands: rx,
        state: state.clone(),
        hub: hub.clone(),
    };
    let alive_t = AliveGuard {
        alive: alive.clone(),
        hub: hub.clone(),
        state: state.clone(),
    };
    let driver = remote_driver(&engine.device);
    std::thread::Builder::new()
        .name("ricercar-remote".into())
        .spawn(move || {
            let _alive = alive_t;
            match driver {
                Some(drive) => drive(engine),
                None => no_driver(engine),
            }
        })
        .expect("spawn remote output");
    PlayerHandle {
        tx,
        state,
        hub,
        alive,
    }
}

/// Embedders without network support: every track fails to load.
fn no_driver(engine: RemoteEngine) {
    for cmd in engine.commands.iter() {
        if let EngineCommand::Load { .. } = cmd {
            engine.hub.publish(EngineEvent::Error {
                message: format!("{}: network outputs are not available", engine.device),
                uri: None,
                http_status: None,
            });
        }
    }
}

/// Spawn the engine on a caller-provided sink (embedding, tests).
pub fn spawn_player_with_sink(sink: Box<dyn AudioSink>) -> PlayerHandle {
    let device = sink.device().clone();
    let (tx, rx) = crossbeam_channel::unbounded::<EngineCommand>();
    let hub = Arc::new(EventHub::default());
    let alive = Arc::new(AtomicBool::new(true));
    let gain = GainStage::default();
    let state = Arc::new(Mutex::new(initial_state(&device, gain)));

    let engine = Engine {
        device,
        sink,
        monitor: Monitor::spawn(hub.clone()),
        hub: hub.clone(),
        state: state.clone(),
        status: TransportStatus::Stopped,
        current: None,
        next: None,
        gain,
        pos_frames: 0,
        last_pos_emit: Instant::now(),
        dsd_mode: DsdMode::Pcm,
        wire: Wire::Pcm,
        playing_dsd: None,
        dop_phase: false,
    };
    let alive_t = AliveGuard {
        alive: alive.clone(),
        hub: hub.clone(),
        state: state.clone(),
    };
    std::thread::Builder::new()
        .name("ricercar-engine".into())
        .spawn(move || {
            let _alive = alive_t;
            engine.run(rx);
        })
        .expect("spawn engine");

    PlayerHandle {
        tx,
        state,
        hub,
        alive,
    }
}

/// Clears the alive flag however the engine thread exits; a panic that got
/// past the engine's own recovery is reported as a stop with an error.
struct AliveGuard {
    alive: Arc<AtomicBool>,
    hub: Arc<EventHub>,
    state: Arc<Mutex<PlayerShared>>,
}

impl Drop for AliveGuard {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Relaxed);
        if std::thread::panicking() {
            {
                let mut st = lock(&self.state);
                st.status = TransportStatus::Stopped;
                st.track_uri = None;
                st.next_uri = None;
            }
            self.hub.publish(EngineEvent::Error {
                message: "audio engine stopped after an internal fault".into(),
                uri: None,
                http_status: None,
            });
            self.hub.publish(EngineEvent::Status {
                status: TransportStatus::Stopped,
            });
        }
    }
}

fn panic_message(p: &(dyn std::any::Any + Send)) -> String {
    p.downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| p.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".into())
}

/// Why a track could not be opened, reported when the engine takes it up.
struct OpenFailure {
    message: String,
    uri: Option<String>,
    http_status: Option<u16>,
}

type Opened = std::result::Result<TrackSource, OpenFailure>;

/// Open and prime a track; a panicking demuxer or decoder is an error like
/// any other, not the end of the engine.
fn open_source(uri: &str, dsd: DsdMode) -> Opened {
    let opened = catch_unwind(AssertUnwindSafe(|| {
        TrackSource::open_with(uri, dsd).map(|mut src| {
            let ok = prime(&mut src);
            (src, ok)
        })
    }));
    let shown = redact_url(uri);
    match opened {
        Ok(Ok((src, true))) => Ok(src),
        Ok(Ok((_, false))) => Err(OpenFailure {
            message: format!("cannot decode {shown}"),
            uri: None,
            http_status: None,
        }),
        Ok(Err(e)) => Err(OpenFailure {
            message: format!("{shown}: {e}"),
            uri: Some(uri.to_string()),
            http_status: match &e {
                AudioError::HttpStatus { status, .. } => Some(*status),
                _ => None,
            },
        }),
        Err(p) => Err(OpenFailure {
            message: format!("{shown}: decoder fault: {}", panic_message(p.as_ref())),
            uri: Some(uri.to_string()),
            http_status: None,
        }),
    }
}

fn chain_of(
    device: &DeviceInfo,
    fmt: Option<PcmFormat>,
    container: Option<&'static str>,
    gain: &GainStage,
    dsd: Option<DsdChain>,
) -> ChainInfo {
    let unity = match dsd {
        Some(d) if d.gain_locked() => true,
        Some(_) => false, // converted to PCM
        None => gain.is_unity(),
    };
    ChainInfo {
        device: device.name.clone(),
        device_kind: device.kind,
        format: fmt,
        container,
        volume: gain.volume,
        bit_perfect: unity && device.kind.is_bit_perfect(),
        dsd,
    }
}

enum NextSource {
    Pending(String),
    /// Being opened on a worker thread, off the audio path.
    Opening(String, Receiver<Opened>),
    Open(Box<TrackSource>),
}

const POSITION_EVERY: Duration = Duration::from_millis(250);

struct Engine {
    device: DeviceInfo,
    sink: Box<dyn AudioSink>,
    /// Loading state and download progress of network sources.
    monitor: Arc<Monitor>,
    hub: Arc<EventHub>,
    state: Arc<Mutex<PlayerShared>>,
    /// Authoritative transport status (mirrored into `state`).
    status: TransportStatus,
    current: Option<TrackSource>,
    next: Option<(NextSource, TrackOpts)>,
    /// `gain.track` holds the options of the current track.
    gain: GainStage,
    /// Frames of the current track handed to the sink.
    pos_frames: u64,
    last_pos_emit: Instant,
    /// DSD mode for tracks opened from now on (guarded).
    dsd_mode: DsdMode,
    /// What the sink was opened for.
    wire: Wire,
    /// The current track, when it is DSD.
    playing_dsd: Option<DsdChain>,
    /// DoP marker alternation, carried across tracks.
    dop_phase: bool,
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.monitor.stop();
    }
}

impl Engine {
    fn run(mut self, rx: Receiver<EngineCommand>) {
        loop {
            match catch_unwind(AssertUnwindSafe(|| self.step(&rx))) {
                Ok(true) => {}
                Ok(false) => return self.shutdown(),
                Err(p) => self.recover(&panic_message(p.as_ref())),
            }
        }
    }

    /// One round of the engine loop; false once the handle is gone.
    fn step(&mut self, rx: &Receiver<EngineCommand>) -> bool {
        if self.status == TransportStatus::Playing {
            // Apply everything queued before producing more audio, so a
            // `load` + `seek`/`pause` burst takes effect atomically.
            loop {
                match rx.try_recv() {
                    Ok(cmd) => self.handle(cmd),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return false,
                }
            }
            if self.status == TransportStatus::Playing {
                self.pump();
                if self.last_pos_emit.elapsed() >= POSITION_EVERY {
                    self.emit_position();
                }
            }
        } else {
            // Paused / stopped: sleep on the command channel.
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(cmd) => self.handle(cmd),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return false,
            }
        }
        true
    }

    /// Something panicked mid-step: drop the track and go back to a clean
    /// stopped state, ready for the next command.
    fn recover(&mut self, what: &str) {
        self.error(format!("audio engine fault: {what}; playback stopped"));
        self.next = None;
        self.end_current(EndReason::Error);
        self.go_stopped(false);
    }

    fn shutdown(&mut self) {
        let _ = self.sink.discard();
        self.sink.close();
    }

    fn handle(&mut self, cmd: EngineCommand) {
        match cmd {
            EngineCommand::Load { uri, opts } => self.load(uri, opts),
            EngineCommand::EnqueueNext { uri, opts } => {
                if uri.is_empty() {
                    // Control points clear the next slot this way.
                    self.next = None;
                    lock(&self.state).next_uri = None;
                } else if self.current.is_none() {
                    self.load(uri, opts);
                } else {
                    lock(&self.state).next_uri = Some(uri.clone());
                    self.next = Some((NextSource::Pending(uri), opts));
                }
            }
            EngineCommand::Pause => {
                if self.status == TransportStatus::Playing {
                    self.emit_position();
                    if let Err(e) = self.sink.pause() {
                        return self.sink_failed(e);
                    }
                    self.rewind_dropped();
                    self.monitor.resumed();
                    self.set_status(TransportStatus::Paused);
                }
            }
            EngineCommand::Resume => {
                if self.status == TransportStatus::Paused {
                    if let Err(e) = self.sink.resume() {
                        return self.sink_failed(e);
                    }
                    self.set_status(TransportStatus::Playing);
                    self.emit_position();
                }
            }
            EngineCommand::Stop => {
                self.next = None;
                self.end_current(EndReason::Stopped);
                self.go_stopped(false);
            }
            EngineCommand::Seek { ms } => self.seek(ms),
            EngineCommand::SetVolume { percent } => {
                self.gain.volume = percent.min(100);
                self.refresh_chain();
            }
            EngineCommand::SetGain { db } => {
                self.gain.preamp_db = if db.is_finite() { db } else { 0.0 };
                self.refresh_chain();
            }
            EngineCommand::SetMute { muted } => {
                self.gain.muted = muted;
                self.refresh_chain();
            }
            EngineCommand::SetDsdMode { mode: wanted } => {
                let mode = wanted.guarded(self.device.kind);
                if mode != wanted {
                    tracing::info!(
                        "{}: shared device, DSD is converted to PCM",
                        self.device.name
                    );
                }
                if mode == self.dsd_mode {
                    return;
                }
                self.dsd_mode = mode;
                // A next track opened in the old mode is opened again.
                if let Some((next, opts)) = self.next.take() {
                    let uri = match next {
                        NextSource::Pending(u) | NextSource::Opening(u, _) => u,
                        NextSource::Open(t) => t.uri,
                    };
                    self.next = Some((NextSource::Pending(uri), opts));
                }
            }
        }
    }

    // ------------------------------------------------------------ state

    fn set_status(&mut self, status: TransportStatus) {
        self.status = status;
        lock(&self.state).status = status;
        self.hub.publish(EngineEvent::Status { status });
    }

    fn refresh_chain(&self) {
        let mut st = lock(&self.state);
        let fmt = self.sink.opened_format().or(st.chain.format);
        let container = self
            .sink
            .opened_container()
            .map(|c| c.label())
            .or_else(|| self.sink.opened_dsd().map(|d| d.label()))
            .or(st.chain.container);
        st.chain = chain_of(&self.device, fmt, container, &self.gain, self.playing_dsd);
        st.gain = self.gain;
    }

    fn pos_ms(&self) -> u64 {
        let rate = self
            .sink
            .opened_format()
            .or_else(|| self.current.as_ref().and_then(|c| c.format))
            .map_or(44100, |f| f.sample_rate as u64);
        let audible = self.pos_frames.saturating_sub(self.sink.delay_frames());
        audible * 1000 / rate
    }

    fn emit_position(&mut self) {
        self.last_pos_emit = Instant::now();
        let pos_ms = self.pos_ms();
        let dur_ms = {
            let mut st = lock(&self.state);
            st.pos_ms = pos_ms;
            st.dur_ms
        };
        self.hub.publish(EngineEvent::Position { pos_ms, dur_ms });
    }

    fn error(&self, message: String) {
        tracing::warn!("{message}");
        self.hub.publish(EngineEvent::Error {
            message,
            uri: None,
            http_status: None,
        });
    }

    fn end_current(&mut self, reason: EndReason) {
        self.monitor.unfollow();
        self.monitor.resumed();
        if let Some(src) = self.current.take() {
            self.hub.publish(EngineEvent::TrackEnded {
                uri: src.uri,
                reason,
            });
        }
    }

    /// Release the device and report Stopped. `drain` lets queued audio play
    /// out (natural end of the queue) instead of cutting it.
    fn go_stopped(&mut self, drain: bool) {
        if drain {
            let _ = self.sink.drain();
        } else {
            let _ = self.sink.discard();
        }
        self.sink.close();
        self.current = None;
        self.monitor.unfollow();
        self.monitor.resumed();
        self.playing_dsd = None;
        self.pos_frames = 0;
        {
            let mut st = lock(&self.state);
            st.track_uri = None;
            st.next_uri = None;
            st.pos_ms = 0;
            st.dur_ms = None;
            st.seekable = false;
            st.stream_title = None;
        }
        self.set_status(TransportStatus::Stopped);
    }

    /// The device failed under us (e.g. USB DAC unplugged): stop cleanly.
    fn sink_failed(&mut self, e: AudioError) {
        self.error(match &e {
            AudioError::DeviceGone { .. } => format!("{e}; playback stopped"),
            _ => format!("audio output error: {e}"),
        });
        self.next = None;
        self.end_current(EndReason::Error);
        self.sink.close();
        self.go_stopped(false);
    }

    // ------------------------------------------------------------ tracks

    fn open_track(&self, uri: &str) -> Option<TrackSource> {
        self.accept(open_source(uri, self.dsd_mode))
    }

    fn accept(&self, opened: Opened) -> Option<TrackSource> {
        match opened {
            Ok(src) => Some(src),
            Err(f) => {
                tracing::warn!("{}", f.message);
                self.hub.publish(EngineEvent::Error {
                    message: f.message,
                    uri: f.uri,
                    http_status: f.http_status,
                });
                None
            }
        }
    }

    /// Start opening the pending next track on a worker thread (network
    /// sources may take seconds to connect; the device buffer does not).
    fn preopen_next(&mut self) {
        if let (Some(cur), Some((NextSource::Pending(uri), _))) = (&self.current, &self.next)
            && cur.continues_into(uri)
        {
            // Decoding just goes on into it: nothing to open.
            return;
        }
        let Some((NextSource::Pending(uri), opts)) = self.next.take() else {
            return;
        };
        let (tx, rx) = crossbeam_channel::bounded(1);
        let u = uri.clone();
        let mode = self.dsd_mode;
        let spawned = std::thread::Builder::new()
            .name("ricercar-preopen".into())
            .spawn(move || {
                let _ = tx.send(open_source(&u, mode));
            });
        self.next = Some(match spawned {
            Ok(_) => (NextSource::Opening(uri, rx), opts),
            Err(_) => (NextSource::Pending(uri), opts),
        });
    }

    /// Take up the pre-opened next track once the worker is done.
    fn poll_next(&mut self) {
        let ready = match &self.next {
            Some((NextSource::Opening(_, rx), _)) => match rx.try_recv() {
                Ok(r) => Some(Some(r)),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(None),
            },
            _ => None,
        };
        let Some(result) = ready else { return };
        if let Some((NextSource::Opening(uri, _), opts)) = self.next.take() {
            self.next = match result {
                Some(r) => self
                    .accept(r)
                    .map(|t| (NextSource::Open(Box::new(t)), opts)),
                None => Some((NextSource::Pending(uri), opts)),
            };
        }
    }

    /// Without hardware pause the device threw queued audio away: move the
    /// position back to what was heard, and the source with it when it can
    /// seek, so resuming neither skips audio nor misreports the position.
    fn rewind_dropped(&mut self) {
        let dropped = self.sink.take_dropped_frames();
        if dropped == 0 {
            return;
        }
        self.pos_frames = self.pos_frames.saturating_sub(dropped);
        if let Some(src) = self.current.as_mut().filter(|s| s.seekable) {
            let rate = src.format.map_or(44100, |f| f.sample_rate as u64);
            let ms = self.pos_frames * 1000 / rate;
            match catch_unwind(AssertUnwindSafe(|| src.seek_ms(ms))) {
                Ok(Ok(actual)) => {
                    self.pos_frames = (actual.as_secs_f64() * rate as f64).round() as u64;
                }
                Ok(Err(e)) => tracing::warn!("rewind after pause failed: {e}"),
                Err(p) => {
                    tracing::warn!("rewind after pause failed: {}", panic_message(p.as_ref()))
                }
            }
        }
        self.emit_position();
    }

    /// Make sure the sink runs at `fmt` for `wire`; a fresh open drops what
    /// is queued, otherwise queued audio of the previous format plays out
    /// first. Going between DSD and PCM reopens the device, as a rate
    /// change does.
    fn open_sink(&mut self, fmt: PcmFormat, wire: Wire, fresh: bool) -> Result<(), AudioError> {
        if !fresh && self.sink.opened_format() == Some(fmt) && self.wire == wire {
            return Ok(());
        }
        if fresh {
            let _ = self.sink.discard();
        } else {
            let _ = self.sink.drain();
        }
        self.sink.close();
        self.wire = Wire::Pcm;
        match wire {
            Wire::NativeDsd => self.sink.open_dsd(fmt),
            Wire::Pcm | Wire::Dop => self.sink.open(fmt),
        }?;
        self.wire = wire;
        self.dop_phase = false;
        Ok(())
    }

    /// Open the sink for `src`. DSD converted to PCM steps down a rate when
    /// the DAC refuses the first one.
    fn sink_for(&mut self, src: &mut TrackSource, fresh: bool) -> bool {
        loop {
            let Some(fmt) = src.format else {
                return false;
            };
            match self.open_sink(fmt, src.wire(), fresh) {
                Ok(()) => return true,
                Err(e @ (AudioError::FormatRefused { .. } | AudioError::UnsupportedFormat(_)))
                    if src.lower_dsd_rate() =>
                {
                    tracing::info!("{}: {e}; converting DSD to a lower rate", self.device.name);
                }
                Err(e) => {
                    self.error(format!("{}: {e}", self.device.name));
                    return false;
                }
            }
        }
    }

    fn begin(&mut self, src: TrackSource, opts: TrackOpts) {
        let fmt = src.format;
        self.playing_dsd = src.dsd().map(|(source, mode)| DsdChain { source, mode });
        self.gain.track = opts;
        self.pos_frames = 0;
        {
            let mut st = lock(&self.state);
            st.track_uri = Some(src.uri.clone());
            st.pos_ms = 0;
            st.dur_ms = src.duration_ms;
            st.seekable = src.seekable;
            st.stream_title = None;
        }
        self.refresh_chain();
        self.hub.publish(EngineEvent::TrackStarted {
            uri: src.uri.clone(),
            format: fmt,
        });
        self.monitor.follow(&src.uri, src.spool());
        self.current = Some(src);
    }

    fn load(&mut self, uri: String, opts: TrackOpts) {
        self.next = None;
        lock(&self.state).next_uri = None;
        self.end_current(EndReason::Replaced);
        self.monitor.waiting(&uri);
        let Some(mut src) = self.open_track(&uri) else {
            return self.go_stopped(false);
        };
        if !self.sink_for(&mut src, true) {
            return self.go_stopped(false);
        }
        self.begin(src, opts);
        self.set_status(TransportStatus::Playing);
    }

    fn seek(&mut self, ms: u64) {
        let Some(src) = self.current.as_mut().filter(|s| s.seekable) else {
            return;
        };
        self.monitor.waiting(&src.uri);
        let res = catch_unwind(AssertUnwindSafe(|| src.seek_ms(ms))).unwrap_or_else(|p| {
            Err(AudioError::Decode(format!(
                "decoder fault: {}",
                panic_message(p.as_ref())
            )))
        });
        match res {
            Ok(actual) => {
                let rate = src.format.map_or(44100.0, |f| f.sample_rate as f64);
                self.pos_frames = (actual.as_secs_f64() * rate).round() as u64;
                if let Err(e) = self.sink.discard() {
                    return self.sink_failed(e);
                }
                self.emit_position();
            }
            Err(e) => tracing::warn!("seek to {ms} ms failed: {e}"),
        }
        // Playing, the wait goes on until audio comes out.
        if self.status != TransportStatus::Playing {
            self.monitor.resumed();
        }
    }

    // ------------------------------------------------------------ pumping

    fn pump(&mut self) {
        let Some(src) = self.current.as_mut() else {
            return self.go_stopped(true);
        };

        if let Some(title) = src.take_stream_title() {
            lock(&self.state).stream_title = Some(title.clone());
            self.hub.publish(EngineEvent::StreamTitle { title });
        }

        // Open the next source shortly before the current one ends, so the
        // transition is gapless.
        let near_end = match (src.duration_ms, src.format) {
            (Some(dur), Some(f)) => self.pos_frames * 1000 / f.sample_rate as u64 + 8_000 >= dur,
            _ => false,
        };
        if near_end && matches!(self.next, Some((NextSource::Pending(_), _))) {
            self.preopen_next();
        }
        self.poll_next();

        let Some(src) = self.current.as_mut() else {
            return;
        };
        self.monitor.waiting(&src.uri);
        let pumped = catch_unwind(AssertUnwindSafe(|| src.pump())).unwrap_or_else(|p| {
            Err(AudioError::Decode(format!(
                "decoder fault: {}",
                panic_message(p.as_ref())
            )))
        });
        if let Err(e) = pumped {
            let msg = format!("{}: {e}", redact_url(&src.uri));
            self.error(msg);
            self.end_current(EndReason::Error);
            return self.go_stopped(false);
        }

        if !src.pending.is_empty() {
            self.monitor.resumed();
            let Some(fmt) = src.format else { return };
            // A chained stream may change format mid-track.
            if self.sink.opened_format() != Some(fmt) || self.wire != src.wire() {
                let Some(mut cur) = self.current.take() else {
                    return;
                };
                let ok = self.sink_for(&mut cur, false);
                self.current = Some(cur);
                if !ok {
                    self.end_current(EndReason::Error);
                    return self.go_stopped(false);
                }
                self.refresh_chain();
            }
            let Some(src) = self.current.as_mut() else {
                return;
            };
            let samples = src.pending.make_contiguous();
            let ch = fmt.channels.max(1) as usize;
            match self.wire {
                Wire::Pcm => self.gain.apply(samples, fmt.bits),
                // DSD bits: no gain, ever.
                Wire::Dop => dop::mark(samples, ch, &mut self.dop_phase),
                Wire::NativeDsd => {}
            }
            let frames = (samples.len() / ch) as u64;
            if let Err(e) = self.sink.write_i32(samples) {
                return self.sink_failed(e);
            }
            src.pending.clear();
            self.pos_frames += frames;
        }

        if self
            .current
            .as_ref()
            .is_some_and(|s| s.end_of_stream && s.pending.is_empty())
        {
            self.transition();
        }
    }

    /// Current track finished: continue gaplessly with the next one, if any.
    fn transition(&mut self) {
        if self.continue_same_file() {
            return;
        }
        // DSD converted to PCM: the filters carry on into the next track.
        let conv = self.current.as_mut().and_then(|c| c.take_dsd_converter());
        self.end_current(EndReason::Finished);
        lock(&self.state).next_uri = None;
        let next = match self.next.take() {
            Some((NextSource::Open(t), opts)) => Some((*t, opts)),
            Some((NextSource::Opening(uri, rx), opts)) => {
                self.monitor.waiting(&uri);
                match rx.recv() {
                    Ok(r) => self.accept(r),
                    Err(_) => self.open_track(&uri),
                }
                .map(|t| (t, opts))
            }
            Some((NextSource::Pending(uri), opts)) => {
                self.monitor.waiting(&uri);
                self.open_track(&uri).map(|t| (t, opts))
            }
            None => None,
        };
        let Some((mut src, opts)) = next else {
            return self.go_stopped(true);
        };
        if src.format.is_none() {
            return self.go_stopped(true);
        }
        src.adopt_dsd_converter(conv);
        // Native-rate switch: flush queued audio, then reopen.
        if !self.sink_for(&mut src, false) {
            return self.go_stopped(false);
        }
        self.begin(src, opts);
    }
}

impl Engine {
    /// The next track is the part of the same file that follows the current
    /// one (cue sheets): keep the decoder going, which is sample-exact and
    /// gapless whatever the container.
    fn continue_same_file(&mut self) -> bool {
        let next_uri = match &self.next {
            Some((NextSource::Pending(u) | NextSource::Opening(u, _), _)) => u.clone(),
            Some((NextSource::Open(t), _)) => t.uri.clone(),
            None => return false,
        };
        let Some(cur) = self.current.as_ref() else {
            return false;
        };
        if !cur.span_done() || !cur.continues_into(&next_uri) {
            return false;
        }
        let Some((_, opts)) = self.next.take() else {
            return false;
        };
        lock(&self.state).next_uri = None;
        let Some(mut src) = self.current.take() else {
            return false;
        };
        self.hub.publish(EngineEvent::TrackEnded {
            uri: src.uri.clone(),
            reason: EndReason::Finished,
        });
        src.continue_into(&next_uri);
        self.begin(src, opts);
        true
    }
}

/// Decode until the track format is known.
fn prime(src: &mut TrackSource) -> bool {
    if src.format.is_none() && !src.end_of_stream && src.pump().is_err() {
        return false;
    }
    src.format.is_some()
}
