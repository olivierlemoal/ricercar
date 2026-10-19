//! Playback devices offered by plugins (`outputs` capability). An output
//! `plugin:<plugin id>/<output id>` is driven with `output.*` requests: the
//! device is handed the plugin's own item refs and fetches the audio
//! itself; its `output.state` notifications become the engine events the
//! controller expects from any output.

use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use ricercar_audio::RemoteEngine;
use ricercar_audio::player::{
    EndReason, EngineCommand, EngineEvent, PlayerShared, TransportStatus,
};
use serde::Deserialize;
use serde_json::{Value, json};

use super::host::{Availability, PluginHost};
use super::{PluginError, parse_output_device, parse_plugin_uri, valid_output_id};

/// An item the device has not started by then failed.
const START_TIMEOUT: Duration = Duration::from_secs(30);
/// A stop this close to the end of the item is its end.
const END_MARGIN_MS: u64 = 5_000;
/// No `output.state` for this long while playing: the device is lost.
const SILENT_AFTER: Duration = Duration::from_secs(20);
/// Checks between commands and notifications.
const TICK: Duration = Duration::from_secs(1);
/// How long a play order waits for a starting plugin and its list.
const READY_WAIT: Duration = Duration::from_secs(15);
/// A failed `output.select` is tried again after this long.
const RESELECT_AFTER: Duration = Duration::from_secs(30);
/// Most outputs kept per plugin.
const MAX_OUTPUTS: usize = 64;

/// A playback device of a running plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginOutput {
    /// `plugin:<plugin id>/<output id>`.
    pub device: String,
    pub name: String,
    /// The plugin's second line for it (empty when none).
    pub detail: String,
    /// How the device is reached, shown as its badge (empty when none).
    pub badge: String,
    /// Display name of the plugin.
    pub plugin_name: String,
}

/// An entry of `outputs.changed`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(super) struct Listed {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub badge: Option<String>,
}

fn clip(s: &str, max: usize) -> String {
    s.trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(max)
        .collect()
}

/// The outputs of an `outputs.changed`: entries with a valid id, first of
/// a duplicate kept; an empty name reads as the id.
pub(super) fn parse_outputs(v: Option<&Value>) -> Vec<Listed> {
    let mut out: Vec<Listed> = Vec::new();
    for e in v.and_then(Value::as_array).into_iter().flatten() {
        let Ok(mut o) = serde_json::from_value::<Listed>(e.clone()) else {
            continue;
        };
        if !valid_output_id(&o.id) || out.iter().any(|x| x.id == o.id) {
            continue;
        }
        o.name = clip(&o.name, 120);
        if o.name.is_empty() {
            o.name = o.id.clone();
        }
        o.detail = o.detail.map(|d| clip(&d, 200)).filter(|d| !d.is_empty());
        o.badge = o.badge.map(|b| clip(&b, 24)).filter(|b| !b.is_empty());
        out.push(o);
        if out.len() == MAX_OUTPUTS {
            break;
        }
    }
    out
}

/// `status` of `output.state`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum DevStatus {
    Loading,
    Playing,
    Paused,
    #[default]
    Stopped,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub(super) struct StateError {
    pub message: String,
    pub code: Option<i64>,
}

/// An `output.state` notification. Numbers are read leniently (a float
/// is no reason to drop the whole state).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub(super) struct OutState {
    pub output: String,
    pub status: DevStatus,
    #[serde(rename = "ref")]
    pub reference: Option<String>,
    pub pos_ms: Option<f64>,
    pub dur_ms: Option<f64>,
    pub volume: Option<f64>,
    pub muted: Option<bool>,
    pub error: Option<StateError>,
}

fn whole(v: Option<f64>) -> Option<u64> {
    v.filter(|x| x.is_finite() && *x >= 0.0).map(|x| x as u64)
}

impl OutState {
    fn pos(&self) -> Option<u64> {
        whole(self.pos_ms)
    }
    fn dur(&self) -> Option<u64> {
        whole(self.dur_ms).filter(|&d| d > 0)
    }
    fn percent(&self) -> Option<u32> {
        whole(self.volume.map(f64::round)).map(|v| v.min(100) as u32)
    }
}

/// Text of a failed call: the plugin's own message when it gave one.
fn error_text(e: &PluginError) -> String {
    match e {
        PluginError::Other { message, .. } if !message.is_empty() => message.clone(),
        e => e.to_string(),
    }
}

/// The item of ours the device is on.
#[derive(Debug, Clone)]
struct Cur {
    uri: String,
    reference: String,
    started: bool,
    since: Instant,
    pos_ms: u64,
    dur_ms: Option<u64>,
    /// `Buffering { waiting: true }` was reported last.
    waiting: bool,
    pause_on_start: bool,
    seek_on_start: Option<u64>,
}

/// Device work a state change calls for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Act {
    /// Play this queue URI (the item that follows one that ended).
    Play(String),
    Pause,
    Seek(u64),
}

/// The engine's view of the device, from the orders given and the states
/// reported, without any I/O: events and device work pile up for the
/// driver to take.
#[derive(Debug)]
pub(super) struct Machine {
    plugin: String,
    /// Output name, for messages.
    pub name: String,
    cur: Option<Cur>,
    /// What the controller armed to follow (any URI).
    next: Option<String>,
    /// Ref last given in `output.next`.
    next_ref: Option<String>,
    status: TransportStatus,
    volume: Option<(u32, bool)>,
    last_state: Instant,
    /// Error of the previous state: one repeated without a ref is stale.
    last_error: Option<StateError>,
    events: Vec<EngineEvent>,
    acts: Vec<Act>,
}

impl Machine {
    pub fn new(plugin: &str, name: &str) -> Machine {
        Machine {
            plugin: plugin.to_string(),
            name: name.to_string(),
            cur: None,
            next: None,
            next_ref: None,
            status: TransportStatus::Stopped,
            volume: None,
            last_state: Instant::now(),
            last_error: None,
            events: Vec::new(),
            acts: Vec::new(),
        }
    }

    /// The ref of `uri` when it is an item of this output's plugin.
    pub fn own_ref(&self, uri: &str) -> Option<String> {
        parse_plugin_uri(uri)
            .filter(|(id, _)| *id == self.plugin)
            .map(|(_, r)| r)
    }

    pub fn busy(&self) -> bool {
        self.cur.is_some()
    }

    pub fn take(&mut self) -> (Vec<EngineEvent>, Vec<Act>) {
        (
            std::mem::take(&mut self.events),
            std::mem::take(&mut self.acts),
        )
    }

    /// The shared player state as this view has it.
    pub fn project(&self, st: &mut PlayerShared) {
        st.status = self.status;
        st.track_uri = self.cur.as_ref().map(|c| c.uri.clone());
        st.next_uri = self.next.clone();
        st.pos_ms = self.cur.as_ref().map_or(0, |c| c.pos_ms);
        st.dur_ms = self.cur.as_ref().and_then(|c| c.dur_ms);
        st.seekable = self.cur.as_ref().is_some_and(|c| c.started);
        st.stream_title = None;
        if let Some((v, _)) = self.volume {
            st.chain.volume = v;
        }
    }

    pub fn set_status(&mut self, status: TransportStatus) {
        if self.status != status {
            self.status = status;
            self.events.push(EngineEvent::Status { status });
        }
    }

    fn go_stopped(&mut self) {
        self.cur = None;
        self.set_status(TransportStatus::Stopped);
    }

    /// A Load: the item playing ends. True when the device holds a next
    /// item to clear.
    pub fn replace(&mut self) -> bool {
        self.next = None;
        if let Some(c) = self.cur.take() {
            self.events.push(EngineEvent::TrackEnded {
                uri: c.uri,
                reason: EndReason::Replaced,
            });
        }
        self.next_ref.take().is_some()
    }

    /// The device accepted to play `uri` (`reference`).
    pub fn begin(&mut self, uri: String, reference: String) {
        self.cur = Some(Cur {
            uri: uri.clone(),
            reference,
            started: false,
            since: Instant::now(),
            pos_ms: 0,
            dur_ms: None,
            waiting: true,
            pause_on_start: false,
            seek_on_start: None,
        });
        self.last_state = Instant::now();
        self.events
            .push(EngineEvent::Buffering { uri, waiting: true });
    }

    /// `uri` could not be handed to the device.
    pub fn load_failed(&mut self, message: String, uri: String) {
        self.events.push(EngineEvent::Error {
            message,
            uri: Some(uri),
            http_status: None,
        });
        self.next = None;
        self.go_stopped();
    }

    /// What follows the current item. Returns the ref to send in
    /// `output.next` when it changed.
    pub fn set_next(&mut self, uri: Option<String>) -> Option<Option<String>> {
        let reference = uri.as_deref().and_then(|u| self.own_ref(u));
        self.next = uri;
        (self.next_ref != reference).then(|| {
            self.next_ref = reference.clone();
            reference
        })
    }

    /// True when the device is to be paused now.
    pub fn pause(&mut self) -> bool {
        match self.cur.as_mut() {
            Some(c) if !c.started => {
                c.pause_on_start = true;
                false
            }
            Some(_) => self.status == TransportStatus::Playing,
            None => false,
        }
    }

    /// True when the device is to resume now.
    pub fn resume(&mut self) -> bool {
        match self.cur.as_mut() {
            Some(c) if !c.started => {
                c.pause_on_start = false;
                false
            }
            Some(_) => self.status == TransportStatus::Paused,
            None => false,
        }
    }

    /// A Stop. True when the device has something to stop.
    pub fn stop(&mut self) -> bool {
        self.next = None;
        self.next_ref = None;
        let Some(c) = self.cur.take() else {
            self.go_stopped();
            return false;
        };
        self.events.push(EngineEvent::TrackEnded {
            uri: c.uri,
            reason: EndReason::Stopped,
        });
        self.go_stopped();
        true
    }

    /// True when the device is to seek now.
    pub fn seek(&mut self, ms: u64) -> bool {
        match self.cur.as_mut() {
            Some(c) if !c.started => {
                c.seek_on_start = Some(ms);
                false
            }
            Some(_) => true,
            None => false,
        }
    }

    pub fn seeked(&mut self, ms: u64) {
        if let Some(c) = self.cur.as_mut() {
            c.pos_ms = ms;
        }
        self.position();
    }

    /// Volume and mute just ordered (the device's own come with its state).
    pub fn ordered_volume(&mut self, percent: Option<u32>, muted: Option<bool>) {
        let (p, m) = self.volume.unwrap_or((100, false));
        self.volume = Some((percent.unwrap_or(p), muted.unwrap_or(m)));
    }

    fn device_volume(&mut self, s: &OutState) {
        let known = self.volume;
        let now = match (s.percent(), s.muted, known) {
            (Some(p), m, _) => (p, m.or(known.map(|k| k.1)).unwrap_or(false)),
            (None, Some(m), Some((p, _))) => (p, m),
            _ => return,
        };
        if known != Some(now) {
            self.volume = Some(now);
            self.events.push(EngineEvent::Volume {
                percent: now.0,
                muted: now.1,
            });
        }
    }

    fn position(&mut self) {
        let Some(c) = &self.cur else { return };
        self.events.push(EngineEvent::Position {
            pos_ms: c.pos_ms,
            dur_ms: c.dur_ms,
        });
    }

    fn set_waiting(&mut self, waiting: bool) {
        let Some(c) = self.cur.as_mut() else { return };
        if c.waiting != waiting {
            c.waiting = waiting;
            let uri = c.uri.clone();
            self.events.push(EngineEvent::Buffering { uri, waiting });
        }
    }

    /// An `output.state` of this output.
    pub fn on_state(&mut self, s: &OutState) {
        self.last_state = Instant::now();
        let repeated = s.error.is_some() && s.error == self.last_error;
        self.last_error = s.error.clone();
        self.device_volume(s);
        let Some(c) = self.cur.as_ref() else { return };
        let named = s.reference.as_ref();
        // An error naming the item, or a new one naming none.
        if let Some(e) = &s.error
            && named.map_or(!repeated, |r| *r == c.reference)
        {
            let what = if e.message.trim().is_empty() {
                PluginError::from_code(e.code.unwrap_or(-32000), "", None).to_string()
            } else {
                clip(&e.message, 200)
            };
            return self.failed(format!("{}: {what}", self.name));
        }
        if !c.started {
            // Only a state naming the item says it started.
            match s.status {
                DevStatus::Playing | DevStatus::Paused if named == Some(&c.reference) => {
                    self.started(s)
                }
                _ if c.since.elapsed() > START_TIMEOUT => {
                    let message = format!("{}: the device did not start the track", self.name);
                    self.failed(message)
                }
                _ => {}
            }
            return;
        }
        let ours = named.is_none_or(|r| *r == c.reference);
        let to_next = named.is_some() && named == self.next_ref.as_ref();
        // The device went on to the next item by itself (gapless), or
        // played the same item again (repeat one).
        let looped = ours
            && self.next_ref.as_ref() == Some(&c.reference)
            && c.dur_ms.is_some_and(|d| c.pos_ms + END_MARGIN_MS >= d)
            && s.pos().is_some_and(|p| p < END_MARGIN_MS);
        if (to_next && !ours) || looped {
            return self.advance(s);
        }
        if !ours {
            tracing::info!("plugin output {}: plays something else now", self.name);
            return self.stopped(EndReason::Stopped);
        }
        let Some(c) = self.cur.as_mut() else { return };
        // A stopped device may reset its position: keep the last one seen.
        if matches!(s.status, DevStatus::Playing | DevStatus::Paused) {
            if let Some(p) = s.pos() {
                c.pos_ms = p;
            }
            if let Some(d) = s.dur() {
                c.dur_ms = Some(d);
            }
        }
        match s.status {
            DevStatus::Playing | DevStatus::Paused => {
                self.set_waiting(false);
                self.set_status(if s.status == DevStatus::Playing {
                    TransportStatus::Playing
                } else {
                    TransportStatus::Paused
                });
                self.position();
            }
            DevStatus::Loading => self.set_waiting(true),
            DevStatus::Stopped => {
                let ended = c.dur_ms.is_some_and(|d| c.pos_ms + END_MARGIN_MS >= d);
                if ended {
                    self.finished();
                } else {
                    self.stopped(EndReason::Stopped);
                }
            }
            DevStatus::Unknown => {}
        }
    }

    /// Periodic checks; `plugin_up`: the plugin process still runs.
    pub fn tick(&mut self, plugin_up: bool) {
        let Some(c) = &self.cur else { return };
        if !plugin_up {
            return self.lost(format!(
                "{}: the plugin stopped; playback stopped",
                self.name
            ));
        }
        if !c.started && c.since.elapsed() > START_TIMEOUT {
            let message = format!("{}: the device did not start the track", self.name);
            return self.failed(message);
        }
        if c.started
            && self.status == TransportStatus::Playing
            && self.last_state.elapsed() > SILENT_AFTER
        {
            self.lost(format!(
                "{}: no news from the device; playback stopped",
                self.name
            ));
        }
    }

    fn started(&mut self, s: &OutState) {
        let Some(c) = self.cur.as_mut() else { return };
        c.started = true;
        c.pos_ms = s.pos().unwrap_or(0);
        c.dur_ms = s.dur();
        c.waiting = false;
        let (uri, pause, seek) = (c.uri.clone(), c.pause_on_start, c.seek_on_start.take());
        self.events.push(EngineEvent::Buffering {
            uri: uri.clone(),
            waiting: false,
        });
        self.events
            .push(EngineEvent::TrackStarted { uri, format: None });
        let status = if s.status == DevStatus::Paused {
            TransportStatus::Paused
        } else {
            TransportStatus::Playing
        };
        self.set_status(status);
        if let Some(ms) = seek {
            self.acts.push(Act::Seek(ms));
        }
        if pause && status == TransportStatus::Playing {
            self.acts.push(Act::Pause);
        }
        self.position();
    }

    /// The device moved on to the next item itself.
    fn advance(&mut self, s: &OutState) {
        let (Some(c), Some(uri), Some(reference)) =
            (self.cur.take(), self.next.take(), self.next_ref.take())
        else {
            return;
        };
        self.events.push(EngineEvent::TrackEnded {
            uri: c.uri,
            reason: EndReason::Finished,
        });
        self.begin(uri, reference);
        self.on_state(s);
    }

    /// The current item failed: shown, and the next one plays if it had
    /// started (one that never started is skipped by the controller).
    fn failed(&mut self, message: String) {
        let Some(c) = self.cur.take() else { return };
        tracing::warn!("{message}");
        self.events.push(EngineEvent::Error {
            message,
            uri: Some(c.uri.clone()),
            http_status: None,
        });
        self.events.push(EngineEvent::TrackEnded {
            uri: c.uri,
            reason: EndReason::Error,
        });
        self.next_ref = None;
        match self.next.take().filter(|_| c.started) {
            Some(next) => self.acts.push(Act::Play(next)),
            None => self.go_stopped(),
        }
    }

    /// The device or the plugin is gone.
    fn lost(&mut self, message: String) {
        let Some(c) = self.cur.take() else { return };
        tracing::warn!("{message}");
        self.events.push(EngineEvent::Error {
            message,
            uri: None,
            http_status: None,
        });
        self.events.push(EngineEvent::TrackEnded {
            uri: c.uri,
            reason: EndReason::Error,
        });
        self.next = None;
        self.next_ref = None;
        self.go_stopped();
    }

    fn stopped(&mut self, reason: EndReason) {
        let Some(c) = self.cur.take() else { return };
        self.events
            .push(EngineEvent::TrackEnded { uri: c.uri, reason });
        self.next = None;
        self.next_ref = None;
        self.go_stopped();
    }

    /// Played to its end; the device did not go on by itself: the next
    /// item, if any, is ordered.
    fn finished(&mut self) {
        let Some(c) = self.cur.take() else { return };
        self.events.push(EngineEvent::TrackEnded {
            uri: c.uri,
            reason: EndReason::Finished,
        });
        self.next_ref = None;
        match self.next.take() {
            Some(next) => self.acts.push(Act::Play(next)),
            None => self.go_stopped(),
        }
    }
}

// ---------------------------------------------------------------- driver

/// Engine thread of one plugin output, until its command channel closes.
pub(super) fn drive(host: PluginHost, engine: RemoteEngine) {
    let Some((plugin, output)) =
        parse_output_device(&engine.device).map(|(p, o)| (p.to_string(), o.to_string()))
    else {
        return refuse(engine);
    };
    let (tx, states) = crossbeam_channel::unbounded();
    host.attach_output_engine(&engine.device, tx.clone());
    let name = host
        .output_name(&engine.device)
        .unwrap_or_else(|| output.clone());
    let mut d = Driver {
        m: Machine::new(&plugin, &name),
        host,
        e: engine,
        plugin,
        output,
        states,
        select_failed: None,
        last_tick: Instant::now(),
    };
    d.select_if_listed();
    d.run();
    d.host.detach_output_engine(&d.e.device, &tx);
    if d.host.unmark_selected(&d.plugin, &d.output) {
        let _ = d
            .host
            .call(&d.plugin, "output.release", json!({ "output": d.output }));
    }
}

/// An output name that is not one: every track fails.
pub(super) fn refuse(engine: RemoteEngine) {
    for cmd in engine.commands.iter() {
        if let EngineCommand::Load { .. } = cmd {
            engine.hub.publish(EngineEvent::Error {
                message: format!("{}: no such output", engine.device),
                uri: None,
                http_status: None,
            });
        }
    }
}

struct Driver {
    host: PluginHost,
    e: RemoteEngine,
    plugin: String,
    output: String,
    m: Machine,
    states: Receiver<OutState>,
    select_failed: Option<Instant>,
    last_tick: Instant,
}

impl Driver {
    fn run(&mut self) {
        let mut stash: Option<EngineCommand> = None;
        loop {
            if let Some(cmd) = stash.take() {
                stash = self.command(cmd);
                continue;
            }
            let wait = TICK.saturating_sub(self.last_tick.elapsed());
            crossbeam_channel::select! {
                recv(self.e.commands) -> c => match c {
                    Ok(cmd) => stash = self.command(cmd),
                    Err(_) => break,
                },
                recv(self.states) -> s => if let Ok(s) = s {
                    self.m.on_state(&s);
                    self.flush();
                },
                default(wait) => {}
            }
            if self.last_tick.elapsed() >= TICK {
                self.last_tick = Instant::now();
                self.tick();
            }
        }
    }

    fn plugin_name(&self) -> String {
        self.host
            .status(&self.plugin)
            .map_or_else(|| self.plugin.clone(), |s| s.name)
    }

    fn refresh_name(&mut self) {
        if let Some(n) = self.host.output_name(&self.e.device) {
            self.m.name = n;
        }
    }

    /// A request to the plugin about this output; the error as shown.
    fn request(&self, method: &str, mut params: Value) -> Result<(), String> {
        params["output"] = json!(self.output);
        self.host
            .call(&self.plugin, method, params)
            .map(|_| ())
            .map_err(|e| format!("{}: {}", self.m.name, error_text(&e)))
    }

    /// Publish what the machine has to say, with the shared state updated
    /// first; then do the device work it asks for.
    fn flush(&mut self) {
        loop {
            let (events, acts) = self.m.take();
            if events.is_empty() && acts.is_empty() {
                return;
            }
            self.m
                .project(&mut self.e.state.lock().unwrap_or_else(|e| e.into_inner()));
            for ev in events {
                self.e.hub.publish(ev);
            }
            for act in acts {
                match act {
                    Act::Play(uri) => self.start(uri),
                    Act::Pause => {
                        if self.ordered("output.pause", json!({})) {
                            self.m.set_status(TransportStatus::Paused);
                        }
                    }
                    Act::Seek(ms) => self.seek(ms),
                }
            }
        }
    }

    /// A transport order whose failure is only logged.
    fn ordered(&self, method: &str, params: Value) -> bool {
        match self.request(method, params) {
            Ok(()) => true,
            Err(e) => {
                tracing::info!("plugin output: {method}: {e}");
                false
            }
        }
    }

    fn command(&mut self, cmd: EngineCommand) -> Option<EngineCommand> {
        let mut stash = None;
        match cmd {
            EngineCommand::Load { uri, .. } => {
                if self.m.replace() {
                    self.ordered("output.next", json!({ "ref": null }));
                }
                self.flush();
                self.start(uri);
            }
            EngineCommand::EnqueueNext { uri, .. } => {
                if uri.is_empty() {
                    self.next(None);
                } else if !self.m.busy() {
                    // Nothing plays: it plays now.
                    self.start(uri);
                } else {
                    self.next(Some(uri));
                }
            }
            EngineCommand::Pause => {
                if self.m.pause() && self.ordered("output.pause", json!({})) {
                    self.m.set_status(TransportStatus::Paused);
                }
            }
            EngineCommand::Resume => {
                if self.m.resume() && self.ordered("output.resume", json!({})) {
                    self.m.set_status(TransportStatus::Playing);
                }
            }
            EngineCommand::Stop => {
                if self.m.stop() {
                    self.ordered("output.stop", json!({}));
                }
            }
            EngineCommand::Seek { ms } => self.seek(ms),
            EngineCommand::SetVolume { .. } | EngineCommand::SetMute { .. } => {
                // One request for a burst (a slider being dragged).
                let (mut percent, mut muted) = (None, None);
                let mut take = |c: &EngineCommand| match *c {
                    EngineCommand::SetVolume { percent: p } => {
                        percent = Some(p.min(100));
                        true
                    }
                    EngineCommand::SetMute { muted: m } => {
                        muted = Some(m);
                        true
                    }
                    _ => false,
                };
                take(&cmd);
                while let Ok(c) = self.e.commands.try_recv() {
                    if !take(&c) {
                        stash = Some(c);
                        break;
                    }
                }
                self.m.ordered_volume(percent, muted);
                let mut params = json!({});
                if let Some(p) = percent {
                    params["percent"] = json!(p);
                }
                if let Some(m) = muted {
                    params["muted"] = json!(m);
                }
                self.ordered("output.volume", params);
            }
            // The device decodes: no gain stage nor DSD output of ours.
            EngineCommand::SetGain { .. } | EngineCommand::SetDsdMode { .. } => {}
        }
        self.flush();
        stash
    }

    fn next(&mut self, uri: Option<String>) {
        if let Some(reference) = self.m.set_next(uri) {
            self.ordered("output.next", json!({ "ref": reference }));
        }
        self.flush();
    }

    fn seek(&mut self, ms: u64) {
        if self.m.seek(ms) && self.ordered("output.seek", json!({ "ms": ms })) {
            self.m.seeked(ms);
        }
        self.flush();
    }

    /// Hand `uri` to the device.
    fn start(&mut self, uri: String) {
        self.refresh_name();
        match self.order_play(&uri) {
            Ok(reference) => self.m.begin(uri, reference),
            Err(message) => {
                tracing::warn!("{message}");
                self.m.load_failed(message, uri);
            }
        }
        self.flush();
    }

    fn order_play(&mut self, uri: &str) -> Result<String, String> {
        let Some(reference) = self.m.own_ref(uri) else {
            return Err(format!(
                "{} plays {} tracks only",
                self.m.name,
                self.plugin_name()
            ));
        };
        self.ready()?;
        self.request("output.play", json!({ "ref": reference, "start_ms": 0 }))?;
        Ok(reference)
    }

    /// The plugin runs and lists the output, and the output is selected.
    /// A plugin starting, or that has not sent its list yet, is waited
    /// for a little.
    fn ready(&mut self) -> Result<(), String> {
        let deadline = Instant::now() + READY_WAIT;
        loop {
            match self.host.output_availability(&self.plugin, &self.output) {
                Availability::Listed => break,
                Availability::Pending if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(200));
                }
                Availability::Absent => {
                    return Err(format!("{}: not connected", self.m.name));
                }
                Availability::Pending | Availability::Off => {
                    return Err(format!(
                        "{}: {}: {}",
                        self.m.name,
                        self.plugin_name(),
                        PluginError::NotRunning
                    ));
                }
            }
        }
        self.refresh_name();
        self.select()
    }

    fn select(&mut self) -> Result<(), String> {
        if self.host.is_selected(&self.plugin, &self.output) {
            return Ok(());
        }
        self.request("output.select", json!({}))?;
        self.host.mark_selected(&self.plugin, &self.output);
        self.select_failed = None;
        Ok(())
    }

    /// At start-up, and after a plugin restart: select the output once it
    /// is listed.
    fn select_if_listed(&mut self) {
        if self
            .select_failed
            .is_some_and(|t| t.elapsed() < RESELECT_AFTER)
            || self.host.is_selected(&self.plugin, &self.output)
            || self.host.output_availability(&self.plugin, &self.output) != Availability::Listed
        {
            return;
        }
        self.refresh_name();
        if let Err(e) = self.select() {
            tracing::info!("plugin output: output.select: {e}");
            self.select_failed = Some(Instant::now());
        }
    }

    fn tick(&mut self) {
        self.select_if_listed();
        let up = self.host.is_running(&self.plugin);
        self.m.tick(up);
        self.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "plugin://q/track%2F1";
    const B: &str = "plugin://q/track%2F2";

    fn state(status: DevStatus, reference: Option<&str>, pos: u64, dur: u64) -> OutState {
        OutState {
            output: "o".into(),
            status,
            reference: reference.map(Into::into),
            pos_ms: Some(pos as f64),
            dur_ms: Some(dur as f64),
            ..Default::default()
        }
    }

    fn playing(m: &mut Machine, uri: &str) {
        let r = m.own_ref(uri).unwrap();
        m.begin(uri.into(), r.clone());
        m.on_state(&state(DevStatus::Playing, Some(&r), 0, 200_000));
        m.take();
    }

    fn ended(ev: &[EngineEvent]) -> Vec<(String, EndReason)> {
        ev.iter()
            .filter_map(|e| match e {
                EngineEvent::TrackEnded { uri, reason } => Some((uri.clone(), *reason)),
                _ => None,
            })
            .collect()
    }

    fn started(ev: &[EngineEvent]) -> Vec<String> {
        ev.iter()
            .filter_map(|e| match e {
                EngineEvent::TrackStarted { uri, .. } => Some(uri.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn refs_of_its_own_plugin_only() {
        let m = Machine::new("q", "Salon");
        assert_eq!(m.own_ref(A).as_deref(), Some("track/1"));
        assert_eq!(m.own_ref("plugin://other/track%2F1"), None);
        assert_eq!(m.own_ref("file:///m/a.flac"), None);
    }

    #[test]
    fn starts_on_the_first_state_naming_the_item() {
        let mut m = Machine::new("q", "Salon");
        m.begin(A.into(), "track/1".into());
        let (ev, _) = m.take();
        assert!(matches!(
            &ev[..],
            [EngineEvent::Buffering { waiting: true, .. }]
        ));
        // A state of what played before, then loading: still waiting.
        m.on_state(&state(DevStatus::Playing, Some("track/9"), 5_000, 9_000));
        m.on_state(&state(DevStatus::Loading, Some("track/1"), 0, 0));
        assert!(started(&m.take().0).is_empty());
        m.on_state(&state(DevStatus::Playing, Some("track/1"), 1_000, 200_000));
        let (ev, _) = m.take();
        assert_eq!(started(&ev), [A]);
        assert!(ev.contains(&EngineEvent::Status {
            status: TransportStatus::Playing
        }));
        assert!(ev.iter().any(|e| matches!(
            e,
            EngineEvent::Position {
                pos_ms: 1_000,
                dur_ms: Some(200_000)
            }
        )));
    }

    #[test]
    fn gapless_advance_to_the_next_ref() {
        let mut m = Machine::new("q", "Salon");
        playing(&mut m, A);
        assert_eq!(m.set_next(Some(B.into())), Some(Some("track/2".into())));
        // Same next again: nothing to send.
        assert_eq!(m.set_next(Some(B.into())), None);
        m.on_state(&state(DevStatus::Playing, Some("track/2"), 300, 180_000));
        let (ev, acts) = m.take();
        assert_eq!(ended(&ev), [(A.to_string(), EndReason::Finished)]);
        assert_eq!(started(&ev), [B]);
        assert!(acts.is_empty());
    }

    #[test]
    fn stop_near_the_end_is_the_end() {
        let mut m = Machine::new("q", "Salon");
        playing(&mut m, A);
        m.on_state(&state(
            DevStatus::Playing,
            Some("track/1"),
            197_000,
            200_000,
        ));
        m.on_state(&state(DevStatus::Stopped, Some("track/1"), 0, 200_000));
        let (ev, acts) = m.take();
        assert_eq!(ended(&ev), [(A.to_string(), EndReason::Finished)]);
        assert!(ev.contains(&EngineEvent::Status {
            status: TransportStatus::Stopped
        }));
        assert!(acts.is_empty());

        // With a next item the device did not take: the host orders it.
        playing(&mut m, A);
        m.set_next(Some("file:///m/b.flac".into()));
        m.on_state(&state(DevStatus::Playing, None, 198_000, 200_000));
        m.on_state(&state(DevStatus::Stopped, None, 0, 0));
        let (ev, acts) = m.take();
        assert_eq!(ended(&ev), [(A.to_string(), EndReason::Finished)]);
        assert_eq!(acts, [Act::Play("file:///m/b.flac".into())]);
    }

    #[test]
    fn early_stop_or_other_ref_is_stopped_by_someone_else() {
        let mut m = Machine::new("q", "Salon");
        playing(&mut m, A);
        m.on_state(&state(DevStatus::Stopped, Some("track/1"), 30_000, 200_000));
        assert_eq!(ended(&m.take().0), [(A.to_string(), EndReason::Stopped)]);

        playing(&mut m, A);
        m.on_state(&state(DevStatus::Playing, Some("track/77"), 0, 100_000));
        assert_eq!(ended(&m.take().0), [(A.to_string(), EndReason::Stopped)]);
        assert!(!m.busy());
    }

    #[test]
    fn an_error_fails_the_item() {
        let mut m = Machine::new("q", "Salon");
        m.begin(A.into(), "track/1".into());
        m.take();
        let mut s = state(DevStatus::Stopped, Some("track/1"), 0, 0);
        s.error = Some(StateError {
            message: "not streamable".into(),
            code: Some(-32003),
        });
        m.on_state(&s);
        let (ev, acts) = m.take();
        assert!(ev.contains(&EngineEvent::Error {
            message: "Salon: not streamable".into(),
            uri: Some(A.into()),
            http_status: None,
        }));
        assert_eq!(ended(&ev), [(A.to_string(), EndReason::Error)]);
        assert!(acts.is_empty());
        assert!(!m.busy());

        // The same error, naming no item, still reported: not about the
        // next one.
        let mut stale = s.clone();
        stale.reference = None;
        m.on_state(&stale);
        m.begin(B.into(), "track/2".into());
        m.take();
        m.on_state(&stale);
        assert!(m.busy());
    }

    #[test]
    fn device_volume_is_mirrored_once() {
        let mut m = Machine::new("q", "Salon");
        let mut s = state(DevStatus::Stopped, None, 0, 0);
        s.volume = Some(35.0);
        s.muted = Some(false);
        m.on_state(&s);
        m.on_state(&s);
        assert_eq!(
            m.take().0,
            [EngineEvent::Volume {
                percent: 35,
                muted: false
            }]
        );
    }

    #[test]
    fn outputs_list_is_checked() {
        let v = json!([
            {"id": "6f1c", "name": " Salon ", "detail": "Marantz NR1200", "badge": "Qobuz Connect"},
            {"id": "6f1c", "name": "dup"},
            {"id": "bad id"},
            {"id": "x2", "badge": "  "},
            {"name": "no id"}
        ]);
        let o = parse_outputs(Some(&v));
        assert_eq!(o.len(), 2);
        assert_eq!(o[0].name, "Salon");
        assert_eq!(o[0].detail.as_deref(), Some("Marantz NR1200"));
        assert_eq!(o[0].badge.as_deref(), Some("Qobuz Connect"));
        assert_eq!(
            (
                o[1].name.as_str(),
                o[1].detail.as_deref(),
                o[1].badge.as_deref()
            ),
            ("x2", None, None)
        );
    }
}
