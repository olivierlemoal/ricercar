//! Playback controller: owns the queue, drives the audio engine, mirrors its
//! state for every front-end (UI, MPRIS, UPnP) and publishes change events.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::{Duration, Instant};

use ricercar_audio::TrackOpts;
use ricercar_audio::device::DeviceKind;
use ricercar_audio::dsd::DsdMode;
use ricercar_audio::player::{ChainInfo, EngineEvent, PlayerHandle, TransportStatus};
use serde::{Deserialize, Serialize};

use crate::config::ReplayGain;
use crate::library::{Library, Track};
use crate::meta;
use crate::plugin::{
    Delivery, PluginError, Purpose, RADIO_LIMIT, Resolved, Resolver, is_plugin_uri,
};
use crate::writer::{Durability, LatestWriter};

/// Metadata about a playable item (library track, pushed stream, radio).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TrackInfo {
    pub uri: String,
    /// Local file backing the item, when there is one.
    pub path: Option<String>,
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub album_id: Option<String>,
    pub duration_ms: u64,
    /// Cover hint: a local track path or an http(s) image URL.
    pub cover: Option<String>,
    pub track_no: Option<u32>,
    pub year: Option<i32>,
    pub genre: Option<String>,
    pub sample_rate: Option<u32>,
    pub bits: Option<u8>,
    pub codec: Option<String>,
    pub rg_track_gain: Option<f32>,
    pub rg_track_peak: Option<f32>,
    pub rg_album_gain: Option<f32>,
    pub rg_album_peak: Option<f32>,
    pub mb_album_id: Option<String>,
    /// Endless stream (internet radio): no duration, no gapless successor.
    pub live: bool,
    /// Plugin tracks: the ref of their album in the plugin, when it gave one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album_ref: Option<String>,
    /// Plugin tracks: the ref of their artist in the plugin.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist_ref: Option<String>,
}

impl From<&Track> for TrackInfo {
    fn from(t: &Track) -> Self {
        if t.is_plugin() {
            return TrackInfo {
                uri: t.uri.clone(),
                title: t.title.clone(),
                artist: t.artist.clone(),
                album: t.album.clone(),
                album_artist: t.album_artist.clone(),
                duration_ms: t.duration_ms,
                cover: t.art.clone(),
                track_no: t.track,
                year: t.year,
                genre: t.genre.clone(),
                sample_rate: t.sample_rate,
                bits: t.bits,
                codec: t.codec.clone(),
                album_ref: t.album_ref.clone(),
                artist_ref: t.artist_ref.clone(),
                ..Default::default()
            };
        }
        TrackInfo {
            uri: t.uri.clone(),
            path: Some(t.path.clone()),
            title: t.title.clone(),
            artist: t.artist.clone(),
            album: t.album.clone(),
            album_artist: t.album_artist.clone(),
            album_id: Some(t.album_id.clone()),
            duration_ms: t.duration_ms,
            cover: Some(t.path.clone()),
            track_no: t.track,
            year: t.year,
            genre: t.genre.clone(),
            sample_rate: t.sample_rate,
            bits: t.bits,
            codec: t.codec.clone(),
            rg_track_gain: t.rg_track_gain,
            rg_track_peak: t.rg_track_peak,
            rg_album_gain: t.rg_album_gain,
            rg_album_peak: t.rg_album_peak,
            mb_album_id: t.mb_album_id.clone(),
            live: false,
            album_ref: None,
            artist_ref: None,
        }
    }
}

impl From<Track> for TrackInfo {
    fn from(t: Track) -> Self {
        TrackInfo::from(&t)
    }
}

impl TrackInfo {
    /// A plugin track's album: (plugin id, album ref), to open with the
    /// plugin's `browse.list`. `None` for other items, or when the plugin
    /// gave no `album_ref` (fall back to a search by name).
    pub fn plugin_album_ref(&self) -> Option<(String, String)> {
        self.plugin_ref(self.album_ref.as_deref())
    }

    /// A plugin track's artist: (plugin id, artist ref).
    pub fn plugin_artist_ref(&self) -> Option<(String, String)> {
        self.plugin_ref(self.artist_ref.as_deref())
    }

    fn plugin_ref(&self, r: Option<&str>) -> Option<(String, String)> {
        let r = r.filter(|r| crate::plugin::valid_ref(r))?;
        let (id, _) = crate::plugin::parse_plugin_uri(&self.uri)?;
        Some((id, r.to_string()))
    }

    /// Minimal info for a bare URI (MPRIS OpenUri, UPnP without DIDL).
    pub fn from_uri(uri: &str) -> TrackInfo {
        if let Some(path) = meta::uri_to_path(uri) {
            let tags = meta::read_tags(&path);
            let p = path.to_string_lossy().into_owned();
            return TrackInfo {
                uri: uri.into(),
                path: Some(p.clone()),
                title: tags.title.unwrap_or_default(),
                artist: tags.artist,
                album: tags.album,
                album_artist: tags.album_artist,
                duration_ms: tags.duration_ms,
                cover: Some(p),
                track_no: tags.track,
                year: tags.year,
                sample_rate: tags.sample_rate,
                bits: tags.bits,
                codec: tags.codec,
                ..Default::default()
            };
        }
        let name = uri
            .split(['?', '#'])
            .next()
            .unwrap_or(uri)
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or(uri);
        TrackInfo {
            uri: uri.into(),
            title: meta::percent_decode(name),
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueueItem {
    /// Stable identity within the session (URIs may repeat in a queue).
    pub id: u64,
    pub info: TrackInfo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Repeat {
    #[default]
    Off,
    All,
    One,
}

impl Repeat {
    pub fn cycle(self) -> Repeat {
        match self {
            Repeat::Off => Repeat::All,
            Repeat::All => Repeat::One,
            Repeat::One => Repeat::Off,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// Queue owned by us (local library playback).
    Local,
    /// Driven by a remote UPnP control point.
    Remote,
    /// Set by a plugin with the `remote_control` capability (its id).
    Plugin(String),
}

/// Where the current queue came from (UI "playing from", ReplayGain auto).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PlayContext {
    #[default]
    None,
    Album(String),
    Artist(String),
    Playlist(i64),
    Radio,
    Mix,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueAt {
    Next,
    End,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CtlEvent {
    TrackChanged,
    StatusChanged(TransportStatus),
    QueueChanged,
    VolumeChanged,
    Seeked(u64),
    /// A play was long enough to count (history, scrobblers).
    Played(Box<TrackInfo>),
    StreamTitle(String),
    /// `buffering` or `buffered` changed.
    Buffering,
    Error(String),
}

#[derive(Debug, Clone)]
pub struct CtlState {
    pub status: TransportStatus,
    pub queue: Vec<QueueItem>,
    pub current: Option<usize>,
    pub shuffle: bool,
    /// Queue ids in their unshuffled order, restored when shuffle goes off.
    pub natural: Vec<u64>,
    pub repeat: Repeat,
    pub origin: Origin,
    pub context: PlayContext,
    pub pos_ms: u64,
    pub dur_ms: u64,
    pub chain: ChainInfo,
    pub volume: u32,
    pub muted: bool,
    /// `[audio] fixed_volume`: volume held at 100 % and unmuted, every
    /// volume or mute change ignored.
    pub fixed_volume: bool,
    pub stream_title: Option<String>,
    /// Bumped on every queue mutation.
    pub queue_rev: u64,
    /// Plugin item whose URL is being resolved before it can load.
    pub resolving: Option<u64>,
    /// How the playing plugin item's stream reaches the engine (from
    /// `track.resolve`); `None` for anything else.
    pub delivery: Option<Delivery>,
    /// Waiting for a network stream: handed to the engine and not playing
    /// yet, or the engine reported a wait (after a seek, on underrun).
    pub buffering: bool,
    /// Parts of the playing HTTP stream received so far, as fractions of
    /// its length (sorted, `start < end`); empty for anything else.
    pub buffered: Vec<(f32, f32)>,
    /// Continuous playback is asking the plugin for more tracks.
    pub radio_pending: bool,
    /// Queue id of the last item the engine played to its very end.
    pub last_finished: Option<u64>,
}

impl CtlState {
    pub fn current_item(&self) -> Option<&QueueItem> {
        self.current.and_then(|i| self.queue.get(i))
    }

    pub fn track(&self) -> Option<TrackInfo> {
        self.current_item().map(|q| q.info.clone())
    }

    pub fn current_uri(&self) -> Option<String> {
        self.current_item().map(|q| q.info.uri.clone())
    }

    pub fn has_next(&self) -> bool {
        match self.current {
            Some(i) => {
                i + 1 < self.queue.len() || (self.repeat == Repeat::All && !self.queue.is_empty())
            }
            None => !self.queue.is_empty(),
        }
    }

    pub fn has_prev(&self) -> bool {
        self.current.is_some_and(|i| i > 0) || self.repeat == Repeat::All
    }

    /// Put `ids` into the natural order right after `anchor` (at the front
    /// when `None`, at the end when the anchor is not there).
    fn natural_insert(&mut self, anchor: Option<u64>, ids: &[u64]) {
        let pos = match anchor {
            None => 0,
            Some(a) => self
                .natural
                .iter()
                .position(|id| *id == a)
                .map_or(self.natural.len(), |i| i + 1),
        };
        self.natural.splice(pos..pos, ids.iter().copied());
    }

    /// Drop the ids that left the queue from the natural order.
    fn natural_retain(&mut self) {
        let queued: HashSet<u64> = self.queue.iter().map(|q| q.id).collect();
        self.natural.retain(|id| queued.contains(id));
    }

    /// Reorder the queue to the natural order; ids it does not know keep
    /// their relative place at the end. The current item stays current.
    fn restore_natural(&mut self) {
        let cur_id = self.current_item().map(|q| q.id);
        let rank: HashMap<u64, usize> = self
            .natural
            .iter()
            .enumerate()
            .map(|(i, id)| (*id, i))
            .collect();
        self.queue
            .sort_by_key(|q| rank.get(&q.id).copied().unwrap_or(usize::MAX));
        self.natural = self.queue.iter().map(|q| q.id).collect();
        self.current = cur_id.and_then(|c| self.queue.iter().position(|q| q.id == c));
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Session {
    queue: Vec<TrackInfo>,
    current: Option<usize>,
    pos_ms: u64,
    volume: u32,
    shuffle: bool,
    repeat: Repeat,
    #[serde(default)]
    context: PlayContext,
    /// Unshuffled order as indexes into `queue`.
    #[serde(default)]
    natural: Option<Vec<usize>>,
}

#[derive(Default)]
struct EventHub {
    subs: Mutex<Vec<Sender<CtlEvent>>>,
}

impl EventHub {
    fn subscribe(&self) -> Receiver<CtlEvent> {
        let (tx, rx) = mpsc::channel();
        self.subs.lock().unwrap().push(tx);
        rx
    }
    fn publish(&self, ev: CtlEvent) {
        self.subs
            .lock()
            .unwrap()
            .retain(|s| s.send(ev.clone()).is_ok());
    }
}

/// Engine-side bookkeeping, shared with the bridge thread.
#[derive(Default)]
struct Pending {
    /// Item handed to the engine as gapless successor.
    armed: Option<(u64, String)>,
    /// Item we asked the engine to load (for auto-skip on failure).
    loading: Option<u64>,
    /// Seek to apply once the given item starts (session restore).
    seek_on_start: Option<(u64, u64)>,
    failures: u32,
    replaygain: ReplayGain,
    /// Plugin items resolved for the loading, playing or armed item.
    resolved: HashMap<u64, Resolved>,
    /// Successors being resolved ahead of time.
    preloading: HashSet<u64>,
    /// Items whose refused URL was already resolved again.
    retried: HashSet<u64>,
    /// Continuous playback: `radio.next` asked (or being asked) after this
    /// item, the last of the queue.
    radio_after: Option<u64>,
}

pub struct Controller {
    pub lib: Arc<Library>,
    player: Arc<Mutex<PlayerHandle>>,
    pub state: Arc<Mutex<CtlState>>,
    pending: Arc<Mutex<Pending>>,
    events: Arc<EventHub>,
    device_name: RwLock<String>,
    next_id: Arc<AtomicU64>,
    alive: Arc<AtomicBool>,
    session_path: Arc<Mutex<Option<PathBuf>>>,
    /// session.json is durable but written off the engine-event thread: a
    /// slow fsync must not hold back TrackStarted at a track change.
    session_writer: Arc<LatestWriter>,
    /// Restored session waiting for the first "play".
    resume_at: Mutex<Option<u64>>,
    rg_preamp: Mutex<f32>,
    resolver: Arc<RwLock<Option<Arc<dyn Resolver>>>>,
    continuous: Arc<AtomicBool>,
    /// Serialises volume and mute changes against the fixed-volume lock.
    volume_gate: Mutex<()>,
    /// DSD output mode per device name (`[audio.dsd]`); PCM if absent.
    dsd_modes: RwLock<BTreeMap<String, DsdMode>>,
}

fn chain_stub(device: &str) -> ChainInfo {
    let kind = ricercar_audio::device::classify(device);
    ChainInfo {
        device: device.into(),
        device_kind: kind,
        format: None,
        container: None,
        volume: 100,
        bit_perfect: kind != DeviceKind::Network,
        dsd: None,
    }
}

/// A network renderer keeps its own volume: we follow it rather than
/// impose ours (a switch to it must not jump the amplifier to our level).
fn own_volume(device: &str) -> bool {
    ricercar_audio::device::classify(device) == DeviceKind::Network
}

/// xorshift64*: good enough for queue shuffles, no extra dependency.
fn shuffle_in_place<T>(v: &mut [T]) {
    let mut s = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E3779B97F4A7C15)
        | 1;
    for i in (1..v.len()).rev() {
        s ^= s >> 12;
        s ^= s << 25;
        s ^= s >> 27;
        let j = (s.wrapping_mul(0x2545F4914F6CDD1D) % (i as u64 + 1)) as usize;
        v.swap(i, j);
    }
}

impl Controller {
    pub fn new(lib: Arc<Library>, device: &str) -> Controller {
        let handle = ricercar_audio::player::spawn_player(device);
        let ctl = Controller {
            lib,
            player: Arc::new(Mutex::new(handle)),
            state: Arc::new(Mutex::new(CtlState {
                status: TransportStatus::Stopped,
                queue: Vec::new(),
                current: None,
                shuffle: false,
                natural: Vec::new(),
                repeat: Repeat::Off,
                origin: Origin::Local,
                context: PlayContext::None,
                pos_ms: 0,
                dur_ms: 0,
                chain: chain_stub(device),
                volume: 100,
                muted: false,
                fixed_volume: false,
                stream_title: None,
                queue_rev: 0,
                resolving: None,
                delivery: None,
                buffering: false,
                buffered: Vec::new(),
                radio_pending: false,
                last_finished: None,
            })),
            pending: Arc::new(Mutex::new(Pending::default())),
            events: Arc::new(EventHub::default()),
            device_name: RwLock::new(device.into()),
            next_id: Arc::new(AtomicU64::new(1)),
            alive: Arc::new(AtomicBool::new(true)),
            session_path: Arc::new(Mutex::new(None)),
            session_writer: Arc::new(LatestWriter::new("ricercar-session-io")),
            resume_at: Mutex::new(None),
            rg_preamp: Mutex::new(0.0),
            resolver: Arc::new(RwLock::new(None)),
            continuous: Arc::new(AtomicBool::new(false)),
            volume_gate: Mutex::new(()),
            dsd_modes: RwLock::new(BTreeMap::new()),
        };
        ctl.spawn_bridge();
        ctl
    }

    pub fn subscribe(&self) -> Receiver<CtlEvent> {
        self.events.subscribe()
    }

    pub fn lock(&self) -> MutexGuard<'_, CtlState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn player(&self) -> MutexGuard<'_, PlayerHandle> {
        self.player.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn new_items(&self, infos: Vec<TrackInfo>) -> Vec<QueueItem> {
        new_items(&self.next_id, infos)
    }

    // ------------------------------------------------------------ engine bridge

    /// Mirror engine events into CtlState, keep the gapless successor armed,
    /// count plays and persist the session.
    fn spawn_bridge(&self) {
        let alive = self.alive.clone();
        let player = self.player.clone();
        let state = self.state.clone();
        let events = self.events.clone();
        let lib = self.lib.clone();
        let session_path = self.session_path.clone();
        let session_writer = self.session_writer.clone();
        let bridge = self.bridge();
        std::thread::Builder::new()
            .name("ricercar-ctl".into())
            .spawn(move || {
                let mut counted_serial = 0u64;
                let mut play_serial = 0u64;
                let mut listened_ms = 0u64;
                let mut last_pos = 0u64;
                let mut last_save = Instant::now();
                let mut saved_rev = u64::MAX;
                while alive.load(Ordering::Relaxed) {
                    let sub = player.lock().unwrap_or_else(|e| e.into_inner()).subscribe();
                    while alive.load(Ordering::Relaxed) {
                        let ev = match sub.0.recv_timeout(Duration::from_millis(500)) {
                            Ok(e) => Some(e),
                            Err(mpsc::RecvTimeoutError::Disconnected) => break,
                            Err(mpsc::RecvTimeoutError::Timeout) => None,
                        };

                        match ev {
                            Some(EngineEvent::TrackStarted { uri, format }) => {
                                play_serial += 1;
                                listened_ms = 0;
                                last_pos = 0;
                                bridge.on_track_started(&uri, format);
                            }
                            Some(EngineEvent::Position { pos_ms, dur_ms }) => {
                                // Count real listening time, not seeks.
                                let delta = pos_ms.saturating_sub(last_pos);
                                if delta < 2_000 {
                                    listened_ms += delta;
                                }
                                last_pos = pos_ms;
                                let played = {
                                    let mut st = bridge.lock_state();
                                    st.pos_ms = pos_ms;
                                    if let Some(d) = dur_ms.filter(|d| *d > 0) {
                                        st.dur_ms = d;
                                    }
                                    let dur = st.dur_ms;
                                    let threshold = (dur / 2).min(240_000);
                                    (counted_serial != play_serial
                                        && dur > 30_000
                                        && listened_ms >= threshold)
                                        .then(|| st.track())
                                        .flatten()
                                };
                                if let Some(info) = played {
                                    counted_serial = play_serial;
                                    lib.record_played(&info);
                                    events.publish(CtlEvent::Played(Box::new(info)));
                                }
                            }
                            Some(EngineEvent::Status { status }) => bridge.on_status(status),
                            Some(EngineEvent::Error {
                                message,
                                uri,
                                http_status,
                            }) => bridge.on_error(message, uri, http_status),
                            Some(EngineEvent::StreamTitle { title }) => {
                                bridge.lock_state().stream_title = Some(title.clone());
                                events.publish(CtlEvent::StreamTitle(title));
                            }
                            Some(EngineEvent::TrackEnded {
                                uri,
                                reason: ricercar_audio::EndReason::Finished,
                            }) => bridge.on_track_finished(&uri),
                            Some(EngineEvent::Buffering { waiting, .. }) => {
                                bridge.set_buffering(Some(waiting), None)
                            }
                            Some(EngineEvent::Buffered { uri, ranges, total }) => {
                                bridge.on_buffered(&uri, &ranges, total)
                            }
                            Some(EngineEvent::Volume { percent, muted }) => {
                                bridge.on_output_volume(percent, muted)
                            }
                            Some(EngineEvent::TrackEnded { .. }) | None => {}
                        }
                        let save_due = last_save.elapsed() >= Duration::from_secs(10);
                        if save_due {
                            last_save = Instant::now();
                            let rev = {
                                let st = bridge.lock_state();
                                st.queue_rev ^ (st.pos_ms / 5000)
                            };
                            if rev != saved_rev {
                                saved_rev = rev;
                                if let Some(p) = session_path.lock().unwrap().clone() {
                                    queue_session_save(&state, &p, &session_writer);
                                }
                            }
                        }
                    }
                }
            })
            .expect("spawn bridge");
    }

    fn bridge(&self) -> Bridge {
        Bridge {
            player: self.player.clone(),
            state: self.state.clone(),
            pending: self.pending.clone(),
            events: self.events.clone(),
            resolver: self.resolver.clone(),
            next_id: self.next_id.clone(),
            continuous: self.continuous.clone(),
        }
    }

    /// Where `plugin://` items get their URLs (the plugin host).
    pub fn set_resolver(&self, r: Arc<dyn Resolver>) {
        *self.resolver.write().unwrap() = Some(r);
    }

    /// Continuous playback (`[audio] continuous_playback`): when the queue
    /// ends on a track of a plugin that declares `radio`, ask it for more
    /// and keep playing.
    pub fn set_continuous(&self, on: bool) {
        if self.continuous.swap(on, Ordering::SeqCst) != on {
            self.pending.lock().unwrap().radio_after = None;
            self.bridge().rearm();
        }
    }

    pub fn continuous(&self) -> bool {
        self.continuous.load(Ordering::SeqCst)
    }

    /// Replace the queue with a radio of the plugin behind `seed` (a
    /// `plugin://` URI of a track, album or artist), `lead` first when
    /// given (usually the seed track itself). Blocking (`radio.next`):
    /// call it off the UI thread. Returns how many tracks were queued.
    pub fn start_radio(&self, seed: &str, lead: Option<TrackInfo>) -> Result<usize, PluginError> {
        let res = self.resolver.read().unwrap().clone();
        let res = res.ok_or(PluginError::NotRunning)?;
        if !res.has_radio(seed) {
            return Err(PluginError::Other {
                code: -32601,
                message: "radio not supported".into(),
            });
        }
        let mut exclude = vec![seed.to_string()];
        exclude.extend(lead.iter().map(|l| l.uri.clone()).filter(|u| u != seed));
        let mut infos: Vec<TrackInfo> = lead.into_iter().collect();
        infos.extend(res.radio_next(seed, &exclude, RADIO_LIMIT)?);
        if infos.is_empty() {
            return Err(PluginError::NotFound);
        }
        let n = infos.len();
        self.play_tracks(infos, 0, PlayContext::None);
        Ok(n)
    }

    /// Display name of the plugin behind a `plugin://` URI.
    pub fn plugin_name(&self, uri: &str) -> Option<String> {
        self.resolver.read().unwrap().as_ref()?.plugin_name(uri)
    }

    /// The audio chain as the engine actually configured it (container,
    /// bit-perfect state including gain and mute).
    pub fn engine_chain(&self) -> ChainInfo {
        let p = self.player();
        let st = p.state.lock().unwrap_or_else(|e| e.into_inner());
        st.chain.clone()
    }

    pub fn device_name(&self) -> String {
        self.device_name.read().unwrap().clone()
    }

    /// Switch output device; playback continues where it was.
    pub fn set_device(&self, name: &str) {
        if name == self.device_name() {
            return;
        }
        let (resume, pos, vol, muted) = {
            let st = self.lock();
            (
                st.status != TransportStatus::Stopped,
                st.pos_ms,
                st.volume,
                st.muted,
            )
        };
        self.player().stop();
        let handle = ricercar_audio::player::spawn_player(name);
        if !own_volume(name) {
            handle.set_volume(vol);
            handle.set_mute(muted);
        }
        handle.set_gain(*self.rg_preamp.lock().unwrap());
        handle.set_dsd_mode(self.dsd_mode_for(name));
        *self.player() = handle;
        *self.device_name.write().unwrap() = name.into();
        {
            let mut st = self.lock();
            st.chain = chain_stub(name);
            st.status = TransportStatus::Stopped;
        }
        *self.pending.lock().unwrap() = Pending::default();
        if resume {
            self.reload_at(pos);
        }
    }

    /// Load the current item again and seek to `pos`.
    fn reload_at(&self, pos: u64) {
        let id = self.lock().current_item().map(|q| q.id);
        if let Some(id) = id {
            self.pending.lock().unwrap().seek_on_start = Some((id, pos));
            self.bridge().load_current();
        }
    }

    /// DSD output mode chosen for `device` (PCM unless set).
    pub fn dsd_mode_for(&self, device: &str) -> DsdMode {
        self.dsd_modes
            .read()
            .unwrap()
            .get(device)
            .copied()
            .unwrap_or_default()
    }

    /// DSD output modes per device (`[audio.dsd]`). A DSD track playing on
    /// the current device restarts where it was in the new mode.
    pub fn set_dsd_modes(&self, modes: BTreeMap<String, DsdMode>) {
        let device = self.device_name();
        let before = self.dsd_mode_for(&device);
        *self.dsd_modes.write().unwrap() = modes;
        let now = self.dsd_mode_for(&device);
        if now == before {
            return;
        }
        self.player().set_dsd_mode(now);
        let (playing_dsd, paused, pos) = {
            let st = self.lock();
            (
                st.status != TransportStatus::Stopped
                    && st
                        .current_uri()
                        .is_some_and(|u| ricercar_audio::dsd::is_dsd_uri(&u)),
                st.status == TransportStatus::Paused,
                st.pos_ms,
            )
        };
        if playing_dsd {
            self.reload_at(pos);
            if paused {
                self.player().pause();
            }
        }
    }

    // ------------------------------------------------------------ queue: play

    /// Replace the queue and start playing `start`.
    pub fn play_tracks(&self, infos: Vec<TrackInfo>, start: usize, ctx: PlayContext) {
        self.play_tracks_in(infos, start, ctx, None);
    }

    /// `play_tracks`; `order` lists the unshuffled order as indexes into
    /// `infos` when they come already shuffled.
    fn play_tracks_in(
        &self,
        infos: Vec<TrackInfo>,
        start: usize,
        ctx: PlayContext,
        order: Option<Vec<usize>>,
    ) {
        if infos.is_empty() {
            return;
        }
        let items = self.new_items(infos);
        {
            let mut st = self.lock();
            let start = start.min(items.len() - 1);
            st.natural = match order {
                Some(o) => o
                    .iter()
                    .filter_map(|i| items.get(*i))
                    .map(|q| q.id)
                    .collect(),
                None => items.iter().map(|q| q.id).collect(),
            };
            st.queue = items;
            st.current = Some(start);
            if st.shuffle {
                let first = st.queue.remove(start);
                shuffle_in_place(&mut st.queue);
                st.queue.insert(0, first);
                st.current = Some(0);
            }
            st.origin = Origin::Local;
            st.context = ctx;
            st.queue_rev += 1;
        }
        *self.resume_at.lock().unwrap() = None;
        self.events.publish(CtlEvent::QueueChanged);
        self.bridge().load_current();
    }

    /// A plugin with the `remote_control` capability replaces the queue.
    pub fn play_from_plugin(&self, plugin_id: &str, infos: Vec<TrackInfo>, start: usize) {
        self.play_tracks(infos, start, PlayContext::None);
        self.lock().origin = Origin::Plugin(plugin_id.to_string());
        self.events.publish(CtlEvent::QueueChanged);
    }

    /// Fresh metadata for a queue item (the URI stays).
    pub fn update_item_info(&self, id: u64, mut info: TrackInfo) {
        {
            let mut st = self.lock();
            let Some(q) = st.queue.iter_mut().find(|q| q.id == id) else {
                return;
            };
            info.uri = q.info.uri.clone();
            if q.info == info {
                return;
            }
            q.info = info;
            st.queue_rev += 1;
        }
        self.events.publish(CtlEvent::QueueChanged);
    }

    /// Replace the queue with a shuffled copy and play it.
    pub fn play_shuffled(&self, infos: Vec<TrackInfo>, ctx: PlayContext) {
        let mut order: Vec<usize> = (0..infos.len()).collect();
        shuffle_in_place(&mut order);
        let mut slots: Vec<Option<TrackInfo>> = infos.into_iter().map(Some).collect();
        let shuffled: Vec<TrackInfo> = order.iter().filter_map(|i| slots[*i].take()).collect();
        // Turning shuffle off later brings back the order as given.
        let mut natural = vec![0; order.len()];
        for (pos, orig) in order.iter().enumerate() {
            natural[*orig] = pos;
        }
        // A shuffled play turns shuffle on, so the mode shown matches the
        // queue and turning it off restores the list's order.
        self.lock().shuffle = true;
        self.play_tracks_in(shuffled, 0, ctx, Some(natural));
    }

    pub fn play_library_tracks(&self, tracks: &[Track], start: usize, ctx: PlayContext) {
        self.play_tracks(tracks.iter().map(TrackInfo::from).collect(), start, ctx);
    }

    pub fn enqueue(&self, infos: Vec<TrackInfo>, at: EnqueueAt) {
        if infos.is_empty() {
            return;
        }
        let items = self.new_items(infos);
        let start_now = {
            let mut st = self.lock();
            let was_empty = st.queue.is_empty();
            let ids: Vec<u64> = items.iter().map(|q| q.id).collect();
            let pos = match (at, st.current) {
                (EnqueueAt::Next, Some(i)) if i < st.queue.len() => {
                    let cur = st.queue[i].id;
                    st.natural_insert(Some(cur), &ids);
                    i + 1
                }
                _ => {
                    st.natural.extend(&ids);
                    st.queue.len()
                }
            };
            st.queue.splice(pos..pos, items);
            st.queue_rev += 1;
            if st.origin == Origin::Remote {
                st.origin = Origin::Local;
            }
            let idle = st.status == TransportStatus::Stopped;
            if was_empty {
                st.current = Some(0);
            }
            was_empty && idle
        };
        self.events.publish(CtlEvent::QueueChanged);
        if start_now {
            self.bridge().load_current();
        } else {
            self.bridge().rearm();
        }
    }

    /// Insert items right after the queue item `after_id` (`0` = at the
    /// front) without starting playback (OpenHome `Playlist.Insert`).
    /// Returns the new item ids, or `None` when `after_id` is not queued.
    pub fn insert_after(&self, after_id: u64, infos: Vec<TrackInfo>) -> Option<Vec<u64>> {
        let items = self.new_items(infos);
        let ids: Vec<u64> = items.iter().map(|q| q.id).collect();
        {
            let mut st = self.lock();
            let pos = if after_id == 0 {
                0
            } else {
                st.queue.iter().position(|q| q.id == after_id)? + 1
            };
            if items.is_empty() {
                return Some(ids);
            }
            let n = items.len();
            st.queue.splice(pos..pos, items);
            st.natural_insert((after_id != 0).then_some(after_id), &ids);
            if let Some(cur) = st.current
                && pos <= cur
            {
                st.current = Some(cur + n);
            }
            st.origin = Origin::Local;
            st.queue_rev += 1;
        }
        self.events.publish(CtlEvent::QueueChanged);
        self.bridge().rearm();
        Some(ids)
    }

    pub fn play_index(&self, i: usize) {
        {
            let mut st = self.lock();
            if i >= st.queue.len() {
                return;
            }
            st.current = Some(i);
        }
        *self.resume_at.lock().unwrap() = None;
        self.bridge().load_current();
    }

    pub fn play_id(&self, id: u64) {
        let idx = self.lock().queue.iter().position(|q| q.id == id);
        if let Some(i) = idx {
            self.play_index(i);
        }
    }

    pub fn remove_ids(&self, ids: &[u64]) {
        let (reload, stop) = {
            let mut st = self.lock();
            let cur_id = st.current_item().map(|q| q.id);
            let playing = st.status != TransportStatus::Stopped;
            let cur_idx = st.current.unwrap_or(0);
            st.queue.retain(|q| !ids.contains(&q.id));
            st.natural_retain();
            st.queue_rev += 1;
            match cur_id {
                Some(cid) if ids.contains(&cid) => {
                    // The playing item went away: continue with what took its place.
                    if cur_idx < st.queue.len() {
                        st.current = Some(cur_idx);
                        (playing, false)
                    } else {
                        st.current = (!st.queue.is_empty()).then(|| st.queue.len() - 1);
                        (false, playing)
                    }
                }
                Some(cid) => {
                    st.current = st.queue.iter().position(|q| q.id == cid);
                    (false, false)
                }
                None => (false, false),
            }
        };
        self.events.publish(CtlEvent::QueueChanged);
        if reload {
            self.bridge().load_current();
        } else if stop {
            self.player().stop();
        } else {
            self.bridge().rearm();
        }
    }

    pub fn move_item(&self, from: usize, to: usize) {
        {
            let mut st = self.lock();
            if from >= st.queue.len() || to >= st.queue.len() || from == to {
                return;
            }
            let cur_id = st.current_item().map(|q| q.id);
            let item = st.queue.remove(from);
            let id = item.id;
            st.queue.insert(to, item);
            // Unshuffled, the natural order follows the move.
            if !st.shuffle {
                let before = to.checked_sub(1).map(|i| st.queue[i].id);
                st.natural.retain(|n| *n != id);
                st.natural_insert(before, &[id]);
            }
            st.current = cur_id.and_then(|c| st.queue.iter().position(|q| q.id == c));
            st.queue_rev += 1;
        }
        self.events.publish(CtlEvent::QueueChanged);
        self.bridge().rearm();
    }

    /// Drop everything after the current item.
    pub fn clear_upcoming(&self) {
        {
            let mut st = self.lock();
            let keep = st.current.map(|i| i + 1).unwrap_or(0);
            st.queue.truncate(keep);
            st.natural_retain();
            st.queue_rev += 1;
        }
        self.events.publish(CtlEvent::QueueChanged);
        self.bridge().rearm();
    }

    pub fn clear_queue(&self) {
        self.player().stop();
        {
            let mut st = self.lock();
            st.queue.clear();
            st.natural.clear();
            st.current = None;
            st.context = PlayContext::None;
            st.queue_rev += 1;
            st.pos_ms = 0;
            st.dur_ms = 0;
        }
        *self.resume_at.lock().unwrap() = None;
        self.events.publish(CtlEvent::QueueChanged);
        self.events.publish(CtlEvent::TrackChanged);
    }

    // ------------------------------------------------------------ modes

    pub fn set_shuffle(&self, on: bool) {
        {
            let mut st = self.lock();
            if st.shuffle == on {
                return;
            }
            st.shuffle = on;
            if on {
                let from = st.current.map(|i| i + 1).unwrap_or(0);
                if from < st.queue.len() {
                    shuffle_in_place(&mut st.queue[from..]);
                }
            } else {
                st.restore_natural();
            }
            st.queue_rev += 1;
        }
        self.events.publish(CtlEvent::QueueChanged);
        self.bridge().rearm();
    }

    pub fn set_repeat(&self, r: Repeat) {
        self.lock().repeat = r;
        self.events.publish(CtlEvent::QueueChanged);
        self.bridge().rearm();
    }

    // ------------------------------------------------------------ transport

    pub fn next(&self) {
        let target = {
            let st = self.lock();
            match st.current {
                Some(i) if i + 1 < st.queue.len() => Some(i + 1),
                Some(_) if st.repeat == Repeat::All && !st.queue.is_empty() => Some(0),
                None if !st.queue.is_empty() => Some(0),
                _ => None,
            }
        };
        if let Some(i) = target {
            self.play_index(i);
        }
    }

    pub fn prev(&self) {
        let (pos, target) = {
            let st = self.lock();
            let t = match st.current {
                Some(i) if i > 0 => Some(i - 1),
                Some(_) if st.repeat == Repeat::All && !st.queue.is_empty() => {
                    Some(st.queue.len() - 1)
                }
                _ => None,
            };
            (st.pos_ms, t)
        };
        match target {
            Some(i) if pos <= 3000 => self.play_index(i),
            _ => self.seek_ms(0),
        }
    }

    pub fn toggle(&self) {
        let status = self.lock().status;
        match status {
            TransportStatus::Playing => self.pause(),
            TransportStatus::Paused => self.resume(),
            TransportStatus::Stopped => self.play(),
        }
    }

    /// Play from the current state: resume, restart the current item, or
    /// start the queue.
    pub fn play(&self) {
        let (status, has_current, empty) = {
            let st = self.lock();
            (st.status, st.current.is_some(), st.queue.is_empty())
        };
        match status {
            TransportStatus::Paused => self.resume(),
            TransportStatus::Playing => {}
            TransportStatus::Stopped if empty => {}
            TransportStatus::Stopped => {
                if !has_current {
                    self.lock().current = Some(0);
                }
                if let Some(pos) = self.resume_at.lock().unwrap().take() {
                    let id = self.lock().current_item().map(|q| q.id);
                    if let Some(id) = id {
                        self.pending.lock().unwrap().seek_on_start = Some((id, pos));
                    }
                }
                self.bridge().load_current();
            }
        }
    }

    pub fn pause(&self) {
        self.player().pause();
    }

    pub fn resume(&self) {
        self.player().resume();
    }

    pub fn stop(&self) {
        // A stop by the user also ends continuous playback.
        self.pending.lock().unwrap().radio_after = None;
        self.player().stop();
        let mut st = self.lock();
        st.pos_ms = 0;
        st.radio_pending = false;
    }

    pub fn seek_ms(&self, ms: u64) {
        let stopped = self.lock().status == TransportStatus::Stopped;
        if stopped {
            // Seeking a restored session: remember it for the first play.
            if self.resume_at.lock().unwrap().is_some() {
                *self.resume_at.lock().unwrap() = Some(ms);
                self.lock().pos_ms = ms;
            }
            return;
        }
        self.player().seek_ms(ms);
        self.lock().pos_ms = ms;
        self.events.publish(CtlEvent::Seeked(ms));
    }

    pub fn seek_relative(&self, delta_ms: i64) {
        let (pos, dur) = {
            let st = self.lock();
            (st.pos_ms as i64, st.dur_ms as i64)
        };
        let target = (pos + delta_ms).max(0);
        let target = if dur > 0 {
            target.min(dur - 500)
        } else {
            target
        };
        self.seek_ms(target.max(0) as u64);
    }

    fn volume_gate(&self) -> MutexGuard<'_, ()> {
        self.volume_gate.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Ignored while the volume is fixed; the event still goes out so
    /// whoever asked shows the real value again.
    pub fn set_volume(&self, percent: u32) {
        let percent = percent.min(100);
        let _gate = self.volume_gate();
        {
            let mut st = self.lock();
            if !st.fixed_volume {
                st.volume = percent;
                drop(st);
                self.player().set_volume(percent);
            }
        }
        self.events.publish(CtlEvent::VolumeChanged);
    }

    /// Ignored while the volume is fixed, like [`Controller::set_volume`].
    pub fn set_muted(&self, muted: bool) {
        let _gate = self.volume_gate();
        {
            let mut st = self.lock();
            if !st.fixed_volume {
                st.muted = muted;
                drop(st);
                self.player().set_mute(muted);
            }
        }
        self.events.publish(CtlEvent::VolumeChanged);
    }

    /// Fixed volume (`[audio] fixed_volume`): on, the volume goes to 100 %
    /// unmuted and every later volume or mute change, from any caller, is
    /// ignored until it is turned off. ReplayGain and preamp are separate.
    pub fn set_fixed_volume(&self, on: bool) {
        let _gate = self.volume_gate();
        {
            let mut st = self.lock();
            if st.fixed_volume == on && (!on || (st.volume == 100 && !st.muted)) {
                return;
            }
            st.fixed_volume = on;
            if on {
                st.volume = 100;
                st.muted = false;
            }
        }
        if on && !own_volume(&self.device_name()) {
            let p = self.player();
            p.set_volume(100);
            p.set_mute(false);
        }
        self.events.publish(CtlEvent::VolumeChanged);
    }

    pub fn volume_fixed(&self) -> bool {
        self.lock().fixed_volume
    }

    /// ReplayGain mode and preamp; applies from the next loaded track.
    pub fn set_replaygain(&self, mode: ReplayGain, preamp_db: f32) {
        self.pending.lock().unwrap().replaygain = mode;
        let pre = if mode == ReplayGain::Off {
            0.0
        } else {
            preamp_db
        };
        self.player().set_gain(pre);
        self.rg_preamp.lock().unwrap().clone_from(&pre);
    }

    // ------------------------------------------------------------ remote (UPnP)

    /// A control point took over the renderer (SetAVTransportURI).
    pub fn set_remote(&self, uri: &str, info: Option<TrackInfo>) {
        let mut info = info.unwrap_or_else(|| TrackInfo::from_uri(uri));
        info.uri = uri.into();
        let items = self.new_items(vec![info]);
        {
            let mut st = self.lock();
            st.natural = items.iter().map(|q| q.id).collect();
            st.queue = items;
            st.current = Some(0);
            st.origin = Origin::Remote;
            st.context = PlayContext::None;
            st.queue_rev += 1;
        }
        *self.resume_at.lock().unwrap() = None;
        self.events.publish(CtlEvent::QueueChanged);
        self.bridge().load_current();
    }

    /// Queue the next track of a remote playlist (SetNextAVTransportURI).
    pub fn remote_next(&self, uri: &str, info: Option<TrackInfo>) {
        {
            let mut st = self.lock();
            let cur = st.current.unwrap_or(0);
            // Keep the list short in long remote sessions: [current, next].
            if cur > 0 {
                st.queue.drain(..cur);
                st.current = Some(0);
            }
            st.queue.truncate(1);
            if !uri.is_empty() {
                let mut info = info.unwrap_or_else(|| TrackInfo::from_uri(uri));
                info.uri = uri.into();
                let items = self.new_items(vec![info]);
                st.queue.extend(items);
            }
            st.natural_retain();
            if let Some(last) = st.queue.last().map(|q| q.id)
                && !st.natural.contains(&last)
            {
                st.natural.push(last);
            }
            st.queue_rev += 1;
        }
        self.events.publish(CtlEvent::QueueChanged);
        self.bridge().rearm();
    }

    // ------------------------------------------------------------ covers

    pub fn cover_for(&self, uri: &str) -> Option<(Vec<u8>, String)> {
        if let Some(path) = meta::uri_to_path(uri) {
            return meta::cover_bytes(&path);
        }
        let hint = self
            .lock()
            .queue
            .iter()
            .find(|q| q.info.uri == uri)
            .and_then(|q| q.info.cover.clone());
        hint.as_deref().and_then(fetch_cover)
    }

    // ------------------------------------------------------------ session

    /// Enable session persistence and restore a previous session (paused).
    pub fn enable_session(&self, path: PathBuf, restore: bool) {
        *self.session_path.lock().unwrap() = Some(path.clone());
        if !restore {
            return;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        let Ok(s) = serde_json::from_str::<Session>(&text) else {
            return;
        };
        let items = self.new_items(s.queue);
        let _gate = self.volume_gate();
        let volume = {
            let mut st = self.lock();
            st.current = s.current.filter(|i| *i < items.len());
            st.dur_ms = st
                .current
                .and_then(|i| items.get(i))
                .map(|q| q.info.duration_ms)
                .unwrap_or(0);
            st.natural = match &s.natural {
                Some(order) => order
                    .iter()
                    .filter_map(|i| items.get(*i))
                    .map(|q| q.id)
                    .collect(),
                // Older sessions: album/track order after the current item.
                None if s.shuffle => {
                    let from = st.current.map(|i| i + 1).unwrap_or(0).min(items.len());
                    let mut rest: Vec<&QueueItem> = items[from..].iter().collect();
                    rest.sort_by(|a, b| {
                        (&a.info.album_id, a.info.track_no)
                            .cmp(&(&b.info.album_id, b.info.track_no))
                    });
                    items[..from].iter().chain(rest).map(|q| q.id).collect()
                }
                None => items.iter().map(|q| q.id).collect(),
            };
            st.queue = items;
            st.pos_ms = s.pos_ms;
            // A saved lower volume does not come back while it is fixed.
            if !st.fixed_volume {
                st.volume = s.volume.min(100);
            }
            st.shuffle = s.shuffle;
            st.repeat = s.repeat;
            st.context = s.context;
            st.queue_rev += 1;
            st.volume
        };
        self.player().set_volume(volume);
        drop(_gate);
        *self.resume_at.lock().unwrap() = Some(s.pos_ms);
        self.events.publish(CtlEvent::QueueChanged);
        self.events.publish(CtlEvent::TrackChanged);
    }

    /// Save the session now, on the calling thread.
    pub fn save_session(&self) {
        if let Some(p) = self.session_path.lock().unwrap().clone()
            && let Some(json) = session_bytes(&self.state)
            && let Err(e) = self
                .session_writer
                .write_now(&p, &json, Durability::Durable)
        {
            tracing::warn!("save session: {e}");
        }
    }

    pub fn shutdown(&self) {
        // Queued saves first, then the last one, synchronously.
        self.session_writer.shutdown();
        self.save_session();
        self.player().stop();
        self.alive.store(false, Ordering::Relaxed);
    }
}

fn new_items(next_id: &AtomicU64, infos: Vec<TrackInfo>) -> Vec<QueueItem> {
    infos
        .into_iter()
        .map(|info| QueueItem {
            id: next_id.fetch_add(1, Ordering::Relaxed),
            info,
        })
        .collect()
}

/// Hand the session to its writer thread: returns at once.
fn queue_session_save(state: &Mutex<CtlState>, path: &std::path::Path, writer: &LatestWriter) {
    if let Some(json) = session_bytes(state) {
        writer.submit(path, json, Durability::Durable);
    }
}

/// The session to save; none while a remote control point drives the queue.
fn session_bytes(state: &Mutex<CtlState>) -> Option<Vec<u8>> {
    let s = {
        let st = state.lock().unwrap_or_else(|e| e.into_inner());
        if st.origin == Origin::Remote {
            return None;
        }
        Session {
            queue: st.queue.iter().map(|q| q.info.clone()).collect(),
            current: st.current,
            pos_ms: st.pos_ms,
            volume: st.volume,
            shuffle: st.shuffle,
            repeat: st.repeat,
            context: st.context.clone(),
            natural: Some({
                let index: HashMap<u64, usize> = st
                    .queue
                    .iter()
                    .enumerate()
                    .map(|(i, q)| (q.id, i))
                    .collect();
                st.natural
                    .iter()
                    .filter_map(|id| index.get(id).copied())
                    .collect()
            }),
        }
    };
    serde_json::to_vec(&s).ok()
}

/// Engine-side half of the controller, shared by the controller, its bridge
/// thread and the threads resolving plugin items.
#[derive(Clone)]
struct Bridge {
    player: Arc<Mutex<PlayerHandle>>,
    state: Arc<Mutex<CtlState>>,
    pending: Arc<Mutex<Pending>>,
    events: Arc<EventHub>,
    resolver: Arc<RwLock<Option<Arc<dyn Resolver>>>>,
    next_id: Arc<AtomicU64>,
    continuous: Arc<AtomicBool>,
}

fn is_network(uri: &str) -> bool {
    uri.starts_with("http://") || uri.starts_with("https://")
}

/// Byte ranges of a body of `total` bytes as fractions of it.
fn fractions(ranges: &[(u64, u64)], total: u64) -> Vec<(f32, f32)> {
    if total == 0 {
        return Vec::new();
    }
    let f = |b: u64| (b.min(total) as f64 / total as f64) as f32;
    ranges
        .iter()
        .map(|&(a, b)| (f(a), f(b)))
        .filter(|(a, b)| a < b)
        .collect()
}

/// HTTP statuses after which a resolved URL is resolved once more.
const RERESOLVE_ON: [u16; 4] = [401, 403, 404, 410];

impl Bridge {
    /// ReplayGain for an item; album gain only when the album plays in order.
    fn opts_for(&self, info: &TrackInfo, mode: ReplayGain) -> TrackOpts {
        let album_ctx = matches!(self.lock_state().context, PlayContext::Album(_));
        let album = match mode {
            ReplayGain::Off => return TrackOpts::default(),
            ReplayGain::Track => false,
            ReplayGain::Album => true,
            ReplayGain::Auto => album_ctx,
        };
        let (gain, peak) = if album && info.rg_album_gain.is_some() {
            (info.rg_album_gain, info.rg_album_peak)
        } else {
            (info.rg_track_gain, info.rg_track_peak)
        };
        TrackOpts {
            gain_db: gain,
            peak,
        }
    }

    fn lock_state(&self) -> MutexGuard<'_, CtlState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock_pending(&self) -> MutexGuard<'_, Pending> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn player(&self) -> MutexGuard<'_, PlayerHandle> {
        self.player.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn resolver(&self) -> Option<Arc<dyn Resolver>> {
        self.resolver.read().unwrap().clone()
    }

    /// ReplayGain values given by a plugin complete the item's own.
    fn with_resolved(info: &TrackInfo, r: &Resolved) -> TrackInfo {
        let mut info = info.clone();
        if let Some(rg) = &r.replaygain {
            info.rg_track_gain = rg.track_gain.or(info.rg_track_gain);
            info.rg_track_peak = rg.track_peak.or(info.rg_track_peak);
            info.rg_album_gain = rg.album_gain.or(info.rg_album_gain);
            info.rg_album_peak = rg.album_peak.or(info.rg_album_peak);
        }
        info
    }

    /// What the engine gets for an item: its URI, or for a plugin item the
    /// URL it resolved to (when known and not expired).
    fn engine_uri(&self, id: u64, info: &TrackInfo) -> Option<String> {
        if !is_plugin_uri(&info.uri) {
            return Some(info.uri.clone());
        }
        let p = self.lock_pending();
        p.resolved
            .get(&id)
            .filter(|r| !r.expired(crate::library::now_unix()))
            .map(|r| r.url.clone())
    }

    fn load_current(&self) {
        let item = {
            let mut st = self.lock_state();
            st.pos_ms = 0;
            st.stream_title = None;
            st.resolving = None;
            st.delivery = None;
            st.buffering = false;
            st.buffered.clear();
            st.dur_ms = st.current_item().map(|q| q.info.duration_ms).unwrap_or(0);
            st.current_item().cloned()
        };
        let Some(item) = item else { return };
        if is_plugin_uri(&item.info.uri) {
            {
                let mut p = self.lock_pending();
                p.armed = None;
                p.loading = Some(item.id);
            }
            self.lock_state().resolving = Some(item.id);
            self.events.publish(CtlEvent::TrackChanged);
            let b = self.clone();
            std::thread::Builder::new()
                .name("ricercar-resolve".into())
                .spawn(move || b.resolve_and_load(item, Purpose::Play))
                .expect("spawn resolve");
            return;
        }
        let opts = {
            let mut p = self.lock_pending();
            p.armed = None;
            p.loading = Some(item.id);
            self.opts_for(&item.info, p.replaygain)
        };
        self.lock_state().buffering = is_network(&item.info.uri);
        self.player().load_with(&item.info.uri, opts);
        self.events.publish(CtlEvent::TrackChanged);
    }

    /// Resolve a plugin item off the controller lock, then load it unless
    /// the user moved on meanwhile.
    fn resolve_and_load(&self, item: QueueItem, purpose: Purpose) {
        let r = match self.resolver() {
            Some(res) => res.resolve(&item.info.uri, purpose),
            None => Err(crate::plugin::PluginError::NotRunning),
        };
        let still_wanted = {
            let st = self.lock_state();
            st.current_item().map(|q| q.id) == Some(item.id)
        } && self.lock_pending().loading == Some(item.id);
        if !still_wanted {
            return;
        }
        self.lock_state().resolving = None;
        match r {
            Ok(res) => {
                let info = Self::with_resolved(&item.info, &res);
                if item.info.duration_ms == 0
                    && let Some(d) = res.duration_ms
                {
                    self.lock_state().dur_ms = d;
                }
                let opts = {
                    let mut p = self.lock_pending();
                    p.resolved.insert(item.id, res.clone());
                    self.opts_for(&info, p.replaygain)
                };
                {
                    let mut st = self.lock_state();
                    st.delivery = Some(res.delivery);
                    st.buffering = is_network(&res.url);
                }
                self.player().load_with(&res.url, opts);
                self.events.publish(CtlEvent::TrackChanged);
            }
            Err(e) => {
                let name = self
                    .resolver()
                    .and_then(|r| r.plugin_name(&item.info.uri))
                    .unwrap_or_default();
                self.on_error(format!("{}: {name}: {e}", item.info.title), None, None);
            }
        }
    }

    /// Hand the engine the item that should follow the current one.
    fn rearm(&self) {
        let (want, last) = {
            let st = self.lock_state();
            // The current item ends the queue: continuous playback may
            // follow it.
            let last = st
                .current
                .filter(|&i| {
                    st.status != TransportStatus::Stopped
                        && st.repeat == Repeat::Off
                        && i + 1 == st.queue.len()
                })
                .and_then(|i| st.queue.get(i))
                .filter(|q| !q.info.live && is_plugin_uri(&q.info.uri))
                .cloned();
            let want = if st.status == TransportStatus::Stopped {
                None
            } else {
                st.current.and_then(|i| {
                    let cur = st.queue.get(i)?;
                    if cur.info.live {
                        return None;
                    }
                    let next = match st.repeat {
                        Repeat::One => Some(cur),
                        Repeat::All => st.queue.get(i + 1).or(st.queue.first()),
                        Repeat::Off => st.queue.get(i + 1),
                    };
                    next.cloned()
                })
            };
            (want, last)
        };
        if let Some(last) = last {
            self.maybe_continue(last);
        }
        // A plugin successor needs its URL first: resolve it (once at a time)
        // and come back here when it is known.
        let want = match want {
            Some(q) => match self.engine_uri(q.id, &q.info) {
                Some(uri) => Some((q.id, uri, q.info)),
                None => {
                    let start = self.lock_pending().preloading.insert(q.id);
                    if start {
                        let b = self.clone();
                        std::thread::Builder::new()
                            .name("ricercar-preload".into())
                            .spawn(move || b.preload(q))
                            .expect("spawn preload");
                    }
                    return;
                }
            },
            None => None,
        };
        let mut p = self.lock_pending();
        if p.armed.as_ref().map(|a| (a.0, &a.1)) != want.as_ref().map(|w| (w.0, &w.1)) {
            // Never hand an empty successor to an idle engine: it would be a no-op anyway.
            if want.is_some() || p.armed.is_some() {
                let (uri, opts) = match &want {
                    Some((id, uri, info)) => {
                        let info = match p.resolved.get(id) {
                            Some(r) => Self::with_resolved(info, r),
                            None => info.clone(),
                        };
                        (uri.clone(), self.opts_for(&info, p.replaygain))
                    }
                    None => (String::new(), TrackOpts::default()),
                };
                self.player().enqueue_next_with(uri, opts);
            }
            p.armed = want.map(|(id, uri, _)| (id, uri));
        }
    }

    /// Continuous playback: ask the plugin of `last` (the last item of the
    /// queue, playing) for tracks to follow it, once per item.
    fn maybe_continue(&self, last: QueueItem) {
        if !self.continuous.load(Ordering::SeqCst) {
            return;
        }
        let Some(res) = self.resolver() else { return };
        if !res.has_radio(&last.info.uri) {
            return;
        }
        {
            let mut p = self.lock_pending();
            if p.radio_after == Some(last.id) {
                return;
            }
            p.radio_after = Some(last.id);
        }
        self.lock_state().radio_pending = true;
        let b = self.clone();
        std::thread::Builder::new()
            .name("ricercar-radio".into())
            .spawn(move || b.continue_radio(res, last))
            .expect("spawn radio");
    }

    /// `radio.next` off the controller lock, then append what came (tracks
    /// of the seed's plugin only) and go on playing. Nothing, or an error:
    /// playback stops at the end of the queue as usual.
    fn continue_radio(&self, res: Arc<dyn Resolver>, seed: QueueItem) {
        let plugin = crate::plugin::parse_plugin_uri(&seed.info.uri).map(|p| p.0);
        let same_plugin = |uri: &str| crate::plugin::parse_plugin_uri(uri).map(|p| p.0) == plugin;
        let exclude: Vec<String> = {
            let st = self.lock_state();
            let mut seen = HashSet::new();
            st.queue
                .iter()
                .rev()
                .map(|q| q.info.uri.clone())
                .filter(|u| same_plugin(u) && seen.insert(u.clone()))
                .take(50)
                .collect()
        };
        let r = res.radio_next(&seed.info.uri, &exclude, RADIO_LIMIT);
        let infos: Vec<TrackInfo> = match r {
            Ok(v) => v
                .into_iter()
                .filter(|t| same_plugin(&t.uri) && !exclude.contains(&t.uri))
                .collect(),
            Err(e) => {
                tracing::info!(
                    "continuous playback after {}: {e}",
                    ricercar_audio::redact_url(&seed.info.uri)
                );
                Vec::new()
            }
        };
        let wanted = self.lock_pending().radio_after == Some(seed.id);
        let start = {
            let mut st = self.lock_state();
            st.radio_pending = false;
            let still_last = st.queue.last().map(|q| q.id) == Some(seed.id)
                && st.current_item().map(|q| q.id) == Some(seed.id);
            if !wanted || !still_last || infos.is_empty() {
                drop(st);
                self.events.publish(CtlEvent::QueueChanged);
                return;
            }
            let at = st.queue.len();
            let items = new_items(&self.next_id, infos);
            st.natural.extend(items.iter().map(|q| q.id));
            st.queue.extend(items);
            st.queue_rev += 1;
            // The seed ended while the plugin was answering.
            let ended = st.status == TransportStatus::Stopped;
            if ended {
                st.current = Some(at);
            }
            ended
        };
        self.events.publish(CtlEvent::QueueChanged);
        if start {
            self.load_current();
        } else {
            self.rearm();
        }
    }

    /// Resolve the gapless successor ahead of time; an URL that comes back
    /// already expired is asked for once more.
    fn preload(&self, item: QueueItem) {
        let Some(res) = self.resolver() else {
            self.lock_pending().preloading.remove(&item.id);
            return;
        };
        let mut r = res.resolve(&item.info.uri, Purpose::Preload);
        if matches!(&r, Ok(x) if x.expired(crate::library::now_unix())) {
            r = res.resolve(&item.info.uri, Purpose::Preload);
        }
        let ok = {
            let mut p = self.lock_pending();
            p.preloading.remove(&item.id);
            match r {
                Ok(x) if !x.expired(crate::library::now_unix()) => {
                    p.resolved.insert(item.id, x);
                    true
                }
                Ok(_) => false,
                Err(e) => {
                    tracing::info!(
                        "preload {}: {e}",
                        ricercar_audio::redact_url(&item.info.uri)
                    );
                    false
                }
            }
        };
        // Loaded on its own turn instead (with the error handling of a load).
        if ok {
            self.rearm();
        }
    }

    /// Queue item the engine is playing when it reports `uri`.
    /// The engine played `uri` to its end: remember which queue item it
    /// was (plugin items play from their resolved URL).
    fn on_track_finished(&self, uri: &str) {
        let by_url: Vec<u64> = self
            .lock_pending()
            .resolved
            .iter()
            .filter(|(_, r)| r.url == uri)
            .map(|(id, _)| *id)
            .collect();
        let mut st = self.lock_state();
        let matches = |q: &QueueItem| q.info.uri == uri || by_url.contains(&q.id);
        let id = st
            .current_item()
            .filter(|q| matches(q))
            .or_else(|| st.queue.iter().find(|q| matches(q)))
            .map(|q| q.id);
        if id.is_some() {
            st.last_finished = id;
        }
    }

    fn on_track_started(&self, uri: &str, format: Option<ricercar_audio::PcmFormat>) {
        let (armed, seek, by_url) = {
            let mut p = self.lock_pending();
            p.loading = None;
            p.failures = 0;
            let by_url: Vec<u64> = p
                .resolved
                .iter()
                .filter(|(_, r)| r.url == uri)
                .map(|(id, _)| *id)
                .collect();
            (p.armed.take(), p.seek_on_start.take(), by_url)
        };
        let started_id = {
            let mut st = self.lock_state();
            let by_armed = armed
                .filter(|(_, u)| u == uri)
                .and_then(|(id, _)| st.queue.iter().position(|q| q.id == id));
            let idx = by_armed.or_else(|| {
                st.current
                    .filter(|&i| {
                        st.queue
                            .get(i)
                            .is_some_and(|q| q.info.uri == uri || by_url.contains(&q.id))
                    })
                    .or_else(|| {
                        st.queue
                            .iter()
                            .position(|q| q.info.uri == uri || by_url.contains(&q.id))
                    })
            });
            if idx.is_some() {
                st.current = idx;
            }
            if format.is_some() {
                st.chain.format = format;
            }
            st.pos_ms = 0;
            st.stream_title = None;
            st.resolving = None;
            // Opened: the first samples are decoded.
            st.buffering = false;
            st.buffered.clear();
            st.dur_ms = st.current_item().map(|q| q.info.duration_ms).unwrap_or(0);
            st.current_item().map(|q| q.id)
        };
        // Resolved URLs are short-lived: keep only the playing one.
        let delivery = {
            let mut p = self.lock_pending();
            p.resolved.retain(|id, _| Some(*id) == started_id);
            p.retried.retain(|id| Some(*id) == started_id);
            started_id
                .and_then(|id| p.resolved.get(&id))
                .map(|r| r.delivery)
        };
        self.lock_state().delivery = delivery;
        if let (Some((id, pos)), Some(sid)) = (seek, started_id)
            && id == sid
            && pos > 0
        {
            self.player().seek_ms(pos);
        }
        self.events.publish(CtlEvent::TrackChanged);
        self.rearm();
    }

    fn on_status(&self, status: TransportStatus) {
        {
            let mut st = self.lock_state();
            if st.status == status {
                return;
            }
            st.status = status;
            if status == TransportStatus::Stopped {
                st.pos_ms = 0;
            }
        }
        if status == TransportStatus::Stopped {
            self.set_buffering(Some(false), Some(Vec::new()));
        }
        if status == TransportStatus::Stopped {
            self.lock_pending().armed = None;
        }
        self.events.publish(CtlEvent::StatusChanged(status));
        if status != TransportStatus::Stopped {
            self.rearm();
        }
    }

    /// The output's own volume (a network renderer) is the one shown.
    fn on_output_volume(&self, percent: u32, muted: bool) {
        {
            let mut st = self.lock_state();
            if (st.volume, st.muted) == (percent, muted) {
                return;
            }
            st.volume = percent.min(100);
            st.muted = muted;
        }
        self.events.publish(CtlEvent::VolumeChanged);
    }

    /// A resolved URL the server refused (expired, revoked…) is resolved
    /// again, once. Returns true when that is under way.
    fn retry_refused(&self, uri: Option<&str>, status: Option<u16>) -> bool {
        let (Some(uri), Some(status)) = (uri, status) else {
            return false;
        };
        if !RERESOLVE_ON.contains(&status) {
            return false;
        }
        let target = {
            let mut p = self.lock_pending();
            let Some(id) = p
                .resolved
                .iter()
                .find(|(_, r)| r.url == uri)
                .map(|(id, _)| *id)
            else {
                return false;
            };
            if !p.retried.insert(id) {
                return false;
            }
            p.resolved.remove(&id);
            if p.armed.as_ref().is_some_and(|a| a.0 == id) {
                p.armed = None;
            }
            id
        };
        let item = self
            .lock_state()
            .queue
            .iter()
            .find(|q| q.id == target)
            .cloned();
        let Some(item) = item else { return false };
        tracing::info!(
            "{}: HTTP {status}, resolving again",
            ricercar_audio::redact_url(uri)
        );
        let is_loading = self.lock_pending().loading == Some(item.id);
        let b = self.clone();
        if is_loading {
            self.lock_state().resolving = Some(item.id);
            std::thread::spawn(move || b.resolve_and_load(item, Purpose::Play));
        } else {
            std::thread::spawn(move || b.rearm());
        }
        true
    }

    /// Update the loading state and the received parts; publish a change.
    fn set_buffering(&self, waiting: Option<bool>, parts: Option<Vec<(f32, f32)>>) {
        let changed = {
            let mut st = self.lock_state();
            let mut changed = false;
            if let Some(w) = waiting.filter(|w| *w != st.buffering) {
                st.buffering = w;
                changed = true;
            }
            if let Some(p) = parts.filter(|p| *p != st.buffered) {
                st.buffered = p;
                changed = true;
            }
            changed
        };
        if changed {
            self.events.publish(CtlEvent::Buffering);
        }
    }

    /// Downloaded ranges of the stream now playing.
    fn on_buffered(&self, uri: &str, ranges: &[(u64, u64)], total: u64) {
        let playing = {
            let st = self.lock_state();
            st.current_item().map(|q| (q.id, q.info.uri == uri))
        };
        let Some((id, same)) = playing else { return };
        let ours = same
            || self
                .lock_pending()
                .resolved
                .get(&id)
                .is_some_and(|r| r.url == uri);
        if ours {
            self.set_buffering(None, Some(fractions(ranges, total)));
        }
    }

    fn on_error(&self, message: String, uri: Option<String>, http_status: Option<u16>) {
        if self.retry_refused(uri.as_deref(), http_status) {
            return;
        }
        // Signed URLs carry their credentials in the query.
        let message = ricercar_audio::redact_urls(&message);
        tracing::warn!("engine: {message}");
        self.events.publish(CtlEvent::Error(message));
        // A track that fails to open is skipped (bounded, to avoid spinning
        // through a queue of unreachable URLs).
        let skip = {
            let mut p = self.lock_pending();
            match p.loading.take() {
                Some(_) if p.failures < 5 => {
                    p.failures += 1;
                    true
                }
                _ => false,
            }
        };
        if skip {
            let next = {
                let st = self.lock_state();
                st.current.filter(|i| i + 1 < st.queue.len()).map(|i| i + 1)
            };
            if let Some(i) = next {
                self.lock_state().current = Some(i);
                self.load_current();
            }
        }
    }
}

fn fetch_cover(url: &str) -> Option<(Vec<u8>, String)> {
    if url.starts_with("file:") || std::path::Path::new(url).exists() {
        let p = if url.starts_with("file:") {
            meta::uri_to_path(url)?
        } else {
            std::path::PathBuf::from(url)
        };
        return meta::cover_bytes(&p);
    }
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return None;
    }
    let resp = ureq::get(url).timeout(Duration::from_secs(8)).call().ok()?;
    let mime = resp
        .header("content-type")
        .unwrap_or("image/jpeg")
        .to_string();
    let mut buf = Vec::new();
    use std::io::Read;
    resp.into_reader()
        .take(16 * 1024 * 1024)
        .read_to_end(&mut buf)
        .ok()?;
    Some((buf, mime))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(n: u32) -> TrackInfo {
        TrackInfo {
            uri: format!("file:///nonexistent/{n}.flac"),
            title: format!("t{n}"),
            track_no: Some(n),
            album_id: Some("a".into()),
            ..Default::default()
        }
    }

    fn ctl() -> Controller {
        Controller::new(Arc::new(Library::in_memory().unwrap()), "null")
    }

    fn titles(c: &Controller) -> Vec<String> {
        c.lock()
            .queue
            .iter()
            .map(|q| q.info.title.clone())
            .collect()
    }

    #[test]
    fn received_ranges_map_to_fractions() {
        assert_eq!(
            fractions(&[(0, 25), (50, 100), (100, 120)], 100),
            [(0.0, 0.25), (0.5, 1.0)]
        );
        assert!(fractions(&[(0, 10)], 0).is_empty());
    }

    #[test]
    fn buffering_state_follows_the_engine_and_resets() {
        let c = stalled();
        let uri = "http://127.0.0.1:9/a.flac";
        c.enqueue(
            vec![TrackInfo {
                uri: uri.into(),
                title: "a".into(),
                ..Default::default()
            }],
            EnqueueAt::End,
        );
        c.lock().current = Some(0);
        let events = c.subscribe();
        let b = c.bridge();
        b.on_buffered(uri, &[(0, 250), (500, 1000)], 1000);
        assert_eq!(c.lock().buffered, [(0.0, 0.25), (0.5, 1.0)]);
        assert!(events.try_iter().any(|e| e == CtlEvent::Buffering));
        // Another stream's progress is not ours.
        b.on_buffered("http://127.0.0.1:9/b.flac", &[(0, 1000)], 1000);
        assert_eq!(c.lock().buffered, [(0.0, 0.25), (0.5, 1.0)]);
        assert!(events.try_iter().next().is_none());
        b.set_buffering(Some(true), None);
        assert!(c.lock().buffering);
        b.set_buffering(Some(false), None);
        assert!(!c.lock().buffering);
        b.set_buffering(Some(true), None);
        c.lock().status = TransportStatus::Playing;
        b.on_status(TransportStatus::Stopped);
        let st = c.lock().clone();
        assert!(!st.buffering && st.buffered.is_empty());
        b.on_buffered(uri, &[(0, 500)], 1000);
        b.on_track_started(uri, None);
        let st = c.lock().clone();
        assert!(!st.buffering && st.buffered.is_empty());
    }

    #[test]
    fn queue_mutations_keep_current() {
        let c = stalled();
        c.enqueue((1..=3).map(album).collect(), EnqueueAt::End);
        assert_eq!(c.lock().current, Some(0));
        c.lock().current = Some(1);
        c.enqueue(vec![album(9)], EnqueueAt::Next);
        assert_eq!(titles(&c), ["t1", "t2", "t9", "t3"]);
        c.move_item(3, 0);
        assert_eq!(titles(&c), ["t3", "t1", "t2", "t9"]);
        assert_eq!(c.lock().current_item().unwrap().info.title, "t2");
        let id = c.lock().queue[0].id;
        c.remove_ids(&[id]);
        assert_eq!(c.lock().current, Some(1));
        c.clear_upcoming();
        assert_eq!(titles(&c), ["t1", "t2"]);
        c.clear_queue();
        assert!(c.lock().queue.is_empty());
    }

    #[test]
    fn shuffle_keeps_current_first_and_restores_order() {
        let c = stalled();
        c.enqueue((1..=30).map(album).collect(), EnqueueAt::End);
        c.lock().context = PlayContext::Album("a".into());
        c.lock().current = Some(4);
        c.set_shuffle(true);
        let t = titles(&c);
        assert_eq!(&t[..5], ["t1", "t2", "t3", "t4", "t5"]);
        assert_ne!(
            &t[5..],
            (6..=30).map(|n| format!("t{n}")).collect::<Vec<_>>()
        );
        c.set_shuffle(false);
        assert_eq!(
            titles(&c),
            (1..=30).map(|n| format!("t{n}")).collect::<Vec<_>>()
        );
        assert_eq!(c.lock().current, Some(4));
    }

    /// Never answers: plugin items stay loading, so nothing fails and
    /// skips ahead while a test looks at the queue.
    struct Stalled;

    impl Resolver for Stalled {
        fn resolve(&self, _uri: &str, _p: Purpose) -> Result<Resolved, PluginError> {
            std::thread::sleep(Duration::from_secs(600));
            Err(PluginError::NotRunning)
        }
        fn plugin_name(&self, _uri: &str) -> Option<String> {
            None
        }
    }

    fn stalled() -> Controller {
        let c = ctl();
        c.set_resolver(Arc::new(Stalled));
        c
    }

    /// A plugin track: no album id, no track number.
    fn plug(title: &str) -> TrackInfo {
        TrackInfo {
            uri: format!("plugin://d/{title}"),
            title: title.into(),
            ..Default::default()
        }
    }

    fn album(n: u32) -> TrackInfo {
        TrackInfo {
            uri: format!("plugin://d/{n}"),
            ..info(n)
        }
    }

    fn names(r: std::ops::RangeInclusive<u32>) -> Vec<String> {
        r.map(|n| format!("t{n}")).collect()
    }

    fn current_title(c: &Controller) -> String {
        c.lock().current_item().unwrap().info.title.clone()
    }

    #[test]
    fn shuffle_play_turns_shuffle_on() {
        let c = ctl();
        assert!(!c.lock().shuffle);
        c.play_shuffled((1..=12).map(info).collect(), PlayContext::Album("a".into()));
        assert!(c.lock().shuffle, "shuffle mode follows a shuffled play");
        c.set_shuffle(false);
        let t = titles(&c);
        let cur = c.lock().current.unwrap();
        assert_eq!(t, (1..=12).map(|n| format!("t{n}")).collect::<Vec<_>>());
        assert_eq!(c.lock().queue[cur].info.title, t[cur]);
    }

    #[test]
    fn album_started_shuffled_unshuffles_to_album_order() {
        let c = stalled();
        c.set_shuffle(true);
        c.play_tracks(
            (1..=12).map(album).collect(),
            4,
            PlayContext::Album("a".into()),
        );
        assert_eq!(c.lock().current, Some(0));
        assert_eq!(current_title(&c), "t5");
        c.set_shuffle(false);
        assert_eq!(titles(&c), names(1..=12));
        assert_eq!(c.lock().current, Some(4));
        assert_eq!(current_title(&c), "t5");
    }

    #[test]
    fn playlist_and_plugin_order_restored_exactly() {
        let order = [
            "zeta", "alpha", "mu", "beta", "omega", "kappa", "delta", "gamma",
        ];
        let tracks = || order.iter().map(|t| plug(t)).collect::<Vec<_>>();
        let c = stalled();
        c.play_tracks(tracks(), 2, PlayContext::Playlist(3));
        c.set_shuffle(true);
        c.set_shuffle(false);
        assert_eq!(titles(&c), order);
        assert_eq!(current_title(&c), "mu");
        // Started shuffled, from a plugin, without a context.
        c.set_shuffle(true);
        c.play_from_plugin("d", tracks(), 5);
        c.set_shuffle(false);
        assert_eq!(titles(&c), order);
        assert_eq!(c.lock().current, Some(5));
        // Shuffle play of a list, then shuffle on and off.
        c.play_shuffled(tracks(), PlayContext::Playlist(3));
        let first = current_title(&c);
        c.set_shuffle(true);
        c.set_shuffle(false);
        assert_eq!(titles(&c), order);
        assert_eq!(current_title(&c), first);
    }

    #[test]
    fn queue_edits_while_shuffled_land_in_natural_place() {
        let edit = |c: &Controller| {
            c.enqueue(vec![plug("next")], EnqueueAt::Next);
            c.enqueue(vec![plug("end")], EnqueueAt::End);
            let t3 = c
                .lock()
                .queue
                .iter()
                .find(|q| q.info.title == "t3")
                .unwrap()
                .id;
            c.insert_after(t3, vec![plug("after3")]).unwrap();
            let gone: Vec<u64> = c
                .lock()
                .queue
                .iter()
                .filter(|q| q.info.title == "t5" || q.info.title == "t6")
                .map(|q| q.id)
                .collect();
            c.remove_ids(&gone);
        };
        let plain = stalled();
        plain.play_tracks(
            (1..=8).map(album).collect(),
            1,
            PlayContext::Album("a".into()),
        );
        edit(&plain);
        let want = titles(&plain);
        assert_eq!(
            want,
            ["t1", "t2", "next", "t3", "after3", "t4", "t7", "t8", "end"]
        );
        let c = stalled();
        c.play_tracks(
            (1..=8).map(album).collect(),
            1,
            PlayContext::Album("a".into()),
        );
        c.set_shuffle(true);
        edit(&c);
        c.set_shuffle(false);
        assert_eq!(titles(&c), want);
        assert_eq!(current_title(&c), "t2");
        assert_eq!(c.lock().current, Some(1));
    }

    #[test]
    fn moves_follow_natural_order_only_unshuffled() {
        let c = stalled();
        c.play_tracks(
            (1..=6).map(album).collect(),
            0,
            PlayContext::Album("a".into()),
        );
        c.move_item(5, 1);
        c.move_item(2, 4);
        let moved = titles(&c);
        assert_eq!(moved, ["t1", "t6", "t3", "t4", "t2", "t5"]);
        c.set_shuffle(true);
        c.move_item(5, 1);
        c.set_shuffle(false);
        assert_eq!(titles(&c), moved);
        assert_eq!(current_title(&c), "t1");
    }

    #[test]
    fn session_keeps_natural_order_while_shuffled() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("session.json");
        let c = stalled();
        c.enable_session(p.clone(), false);
        c.set_shuffle(true);
        c.play_tracks(
            (1..=10).map(album).collect(),
            3,
            PlayContext::Album("a".into()),
        );
        c.save_session();
        let c2 = stalled();
        c2.enable_session(p.clone(), true);
        assert!(c2.lock().shuffle);
        assert_eq!(current_title(&c2), "t4");
        c2.set_shuffle(false);
        assert_eq!(titles(&c2), names(1..=10));
        assert_eq!(c2.lock().current, Some(3));
        // Sessions saved without it: album/track order after the current item.
        let mut v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        v.as_object_mut().unwrap().remove("natural");
        std::fs::write(&p, v.to_string()).unwrap();
        let c3 = stalled();
        c3.enable_session(p, true);
        c3.set_shuffle(false);
        let t = titles(&c3);
        assert_eq!(t[0], "t4");
        let mut rest = names(1..=10);
        rest.retain(|n| n != "t4");
        assert_eq!(&t[1..], rest);
    }

    #[test]
    fn insert_after_positions_and_keeps_current() {
        let c = ctl();
        // Into an empty queue: nothing is selected, nothing starts.
        let a = c.insert_after(0, vec![info(1)]).unwrap();
        assert_eq!(c.lock().current, None);
        let b = c.insert_after(a[0], vec![info(2), info(3)]).unwrap();
        assert_eq!(b.len(), 2);
        assert_eq!(titles(&c), ["t1", "t2", "t3"]);
        c.lock().current = Some(1);
        // Before the current item: current follows its item.
        let f = c.insert_after(0, vec![info(0)]).unwrap();
        assert_eq!(titles(&c), ["t0", "t1", "t2", "t3"]);
        assert_eq!(c.lock().current_item().unwrap().info.title, "t2");
        c.insert_after(b[1], vec![info(4)]).unwrap();
        assert_eq!(titles(&c), ["t0", "t1", "t2", "t3", "t4"]);
        assert_eq!(c.lock().queue[0].id, f[0]);
        assert!(c.insert_after(9_999, vec![info(5)]).is_none());
        assert_eq!(c.lock().queue.len(), 5);
        assert_eq!(c.lock().status, TransportStatus::Stopped);
    }

    #[test]
    fn next_prev_and_repeat() {
        let c = ctl();
        c.enqueue((1..=2).map(info).collect(), EnqueueAt::End);
        let st = c.lock().clone();
        assert!(st.has_next() && !st.has_prev());
        c.lock().current = Some(1);
        assert!(!c.lock().has_next());
        c.set_repeat(Repeat::All);
        assert!(c.lock().has_next());
        assert_eq!(Repeat::Off.cycle(), Repeat::All);
    }

    #[test]
    fn remote_next_keeps_two_items() {
        let c = ctl();
        c.set_remote("http://x/a.flac", None);
        c.remote_next("http://x/b.flac", None);
        assert_eq!(titles(&c), ["a.flac", "b.flac"]);
        c.lock().current = Some(1);
        c.remote_next("http://x/c%20d.flac", None);
        assert_eq!(titles(&c), ["b.flac", "c d.flac"]);
        assert_eq!(c.lock().origin, Origin::Remote);
    }

    #[test]
    fn session_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("session.json");
        let c = ctl();
        c.enable_session(p.clone(), false);
        let mut items: Vec<TrackInfo> = (1..=3).map(info).collect();
        items[1].album_ref = Some("album/7".into());
        items[1].artist_ref = Some("artist/2".into());
        c.enqueue(items, EnqueueAt::End);
        c.set_repeat(Repeat::One);
        {
            let mut st = c.lock();
            st.current = Some(2);
            st.pos_ms = 42_000;
        }
        c.save_session();
        let c2 = ctl();
        c2.enable_session(p, true);
        let st = c2.lock().clone();
        assert_eq!(st.queue.len(), 3);
        assert_eq!(st.current, Some(2));
        assert_eq!(st.pos_ms, 42_000);
        assert_eq!(st.repeat, Repeat::One);
        assert_eq!(st.queue[1].info.album_ref.as_deref(), Some("album/7"));
        assert_eq!(st.queue[1].info.artist_ref.as_deref(), Some("artist/2"));
        // Sessions saved before these fields still load.
        let old: TrackInfo = serde_json::from_str(r#"{"uri":"plugin://d/t","title":"x"}"#).unwrap();
        assert_eq!((old.album_ref, old.artist_ref), (None, None));
    }

    #[test]
    fn periodic_session_saves_do_not_wait_for_the_disk() {
        let gate = Arc::new(std::sync::Barrier::new(2));
        let held = Arc::new(AtomicBool::new(false));
        let written = Arc::new(Mutex::new(Vec::new()));
        let (g, h, w) = (gate.clone(), held.clone(), written.clone());
        let writer = LatestWriter::with_sink("test-session-io", move |_, bytes, durability| {
            assert_eq!(durability, Durability::Durable);
            if !h.swap(true, Ordering::SeqCst) {
                g.wait();
            }
            w.lock().unwrap().push(bytes.to_vec());
            Ok(())
        });
        let c = ctl();
        c.enqueue((1..=3).map(info).collect(), EnqueueAt::End);
        let p = std::path::Path::new("session.json");
        let t = Instant::now();
        // The first save holds the writer (a stuck fsync); the others queue.
        for pos in 0..50 {
            c.lock().pos_ms = pos;
            queue_session_save(&c.state, p, &writer);
        }
        assert!(t.elapsed() < Duration::from_secs(1));
        gate.wait();
        writer.flush();
        let written = written.lock().unwrap();
        let last: Session = serde_json::from_slice(written.last().unwrap()).unwrap();
        assert_eq!(last.pos_ms, 49);
        assert!(written.len() <= 2);
    }

    fn vol(c: &Controller) -> (u32, bool) {
        let st = c.lock();
        (st.volume, st.muted)
    }

    #[test]
    fn fixed_volume_holds_100_and_ignores_changes() {
        let c = ctl();
        c.set_volume(40);
        c.set_muted(true);
        assert_eq!(vol(&c), (40, true));
        let events = c.subscribe();
        c.set_fixed_volume(true);
        assert!(c.volume_fixed());
        assert_eq!(vol(&c), (100, false));
        assert!(
            events
                .try_iter()
                .any(|e| matches!(e, CtlEvent::VolumeChanged))
        );
        c.set_volume(10);
        c.set_muted(true);
        c.set_volume(0);
        assert_eq!(vol(&c), (100, false));
        // Ignored requests still announce the real value.
        assert!(
            events
                .try_iter()
                .any(|e| matches!(e, CtlEvent::VolumeChanged))
        );
        c.set_fixed_volume(false);
        assert!(!c.volume_fixed());
        assert_eq!(vol(&c), (100, false));
        c.set_volume(25);
        c.set_muted(true);
        assert_eq!(vol(&c), (25, true));
    }

    #[test]
    fn fixed_volume_blocks_session_restore_of_a_lower_volume() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("session.json");
        let c = ctl();
        c.enable_session(p.clone(), false);
        c.enqueue((1..=2).map(info).collect(), EnqueueAt::End);
        c.set_volume(30);
        c.save_session();

        let locked = ctl();
        locked.set_fixed_volume(true);
        locked.enable_session(p.clone(), true);
        assert_eq!(locked.lock().queue.len(), 2);
        assert_eq!(vol(&locked), (100, false));

        // Restored first, locked afterwards: the lock wins too.
        let late = ctl();
        late.enable_session(p.clone(), true);
        assert_eq!(vol(&late).0, 30);
        late.set_fixed_volume(true);
        assert_eq!(vol(&late), (100, false));

        let free = ctl();
        free.enable_session(p, true);
        assert_eq!(vol(&free).0, 30);
    }

    #[test]
    fn engine_errors_are_logged_without_url_secrets() {
        // on_error logs and publishes the same masked message; checking the
        // event avoids capturing logs, which tracing's per-callsite cache
        // makes unreliable when tests run in parallel.
        let c = ctl();
        let events = c.subscribe();
        let uri = "http://127.0.0.1:9/x?token=secret";
        c.bridge().on_error(
            format!("{uri}: connection to 127.0.0.1 timed out"),
            Some(uri.into()),
            None,
        );
        let shown = events
            .try_iter()
            .find_map(|e| match e {
                CtlEvent::Error(m) => Some(m),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            shown,
            "http://127.0.0.1:9/x?token=…: connection to 127.0.0.1 timed out"
        );
    }
}
