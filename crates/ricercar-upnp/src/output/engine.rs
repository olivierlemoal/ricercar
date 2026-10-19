//! Drives one renderer: engine commands become AVTransport and
//! RenderingControl actions, and its state, polled, becomes the engine
//! events the controller expects from any output.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::RecvTimeoutError;
use ricercar_audio::player::{EndReason, EngineCommand, EngineEvent, TransportStatus};
use ricercar_audio::{RemoteEngine, redact_url};
use ricercar_core::TrackInfo;

use super::control::{Failure, arg};
use super::search::Device;
use super::serve::{self, Served};
use crate::didl;

/// State polls while a track plays.
const POLL: Duration = Duration::from_secs(1);
/// …and while waiting for one to start.
const POLL_STARTING: Duration = Duration::from_millis(300);
/// Volume is read every this many polls (and when idle).
const VOLUME_EVERY: u32 = 3;
const IDLE: Duration = Duration::from_secs(3);
/// A track the renderer has not started by then failed.
const START_TIMEOUT: Duration = Duration::from_secs(20);
/// A stop this close to the end of the track is its end.
const END_MARGIN_MS: u64 = 4_000;
/// Unanswered polls in a row before the renderer is looked up again.
const LOST_AFTER: u32 = 5;

struct Track {
    uri: String,
    served: Arc<Served>,
    started: bool,
    since: Instant,
    pos_ms: u64,
    dur_ms: Option<u64>,
    pause_on_start: bool,
    seek_on_start: Option<u64>,
}

struct Out {
    e: RemoteEngine,
    uuid: String,
    dev: Option<Device>,
    cur: Option<Track>,
    next: Option<String>,
    status: TransportStatus,
    volume: Option<(u32, bool)>,
    polls: u32,
    unanswered: u32,
    last_poll: Instant,
}

/// The driver registered with the audio crate for `upnp:` outputs.
pub fn drive(engine: RemoteEngine) {
    let uuid = engine
        .device
        .strip_prefix(super::PREFIX)
        .unwrap_or(&engine.device)
        .to_string();
    let mut o = Out {
        e: engine,
        uuid,
        dev: None,
        cur: None,
        next: None,
        status: TransportStatus::Stopped,
        volume: None,
        polls: 0,
        unanswered: 0,
        last_poll: Instant::now(),
    };
    if o.device().is_some() {
        o.read_volume();
    }
    loop {
        let every = match &o.cur {
            Some(t) if !t.started => POLL_STARTING,
            Some(_) => POLL,
            None => IDLE,
        };
        let wait = every.saturating_sub(o.last_poll.elapsed());
        match o.e.commands.recv_timeout(wait) {
            Ok(cmd) => o.on_cmd(cmd),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if o.last_poll.elapsed() >= every {
            o.poll();
        }
    }
}

/// `H:MM:SS` for a Seek target.
fn hms(ms: u64) -> String {
    let s = ms / 1000;
    format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

fn parse_time(t: &str) -> Option<u64> {
    didl::parse_duration(t).filter(|_| t.contains(':'))
}

impl Out {
    fn publish(&self, ev: EngineEvent) {
        self.e.hub.publish(ev);
    }

    fn set_status(&mut self, status: TransportStatus) {
        if self.status == status {
            return;
        }
        self.status = status;
        self.e
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .status = status;
        self.publish(EngineEvent::Status { status });
    }

    fn device(&mut self) -> Option<&Device> {
        if self.dev.is_none() {
            self.dev = super::locate(&self.uuid);
            if self.dev.is_none() {
                tracing::info!("upnp output: renderer {} not found", self.uuid);
            }
        }
        self.dev.as_ref()
    }

    fn name(&self) -> String {
        self.dev
            .as_ref()
            .map_or_else(|| self.uuid.clone(), |d| d.name.clone())
    }

    /// A failed action: an unreachable renderer is looked up again next time.
    fn note(&mut self, f: &Failure) {
        if let Failure::Unreachable(_) = f {
            super::forget(&self.uuid);
            self.dev = None;
        }
    }

    fn on_cmd(&mut self, cmd: EngineCommand) {
        match cmd {
            EngineCommand::Load { uri, .. } => self.load(uri, true),
            EngineCommand::EnqueueNext { uri, .. } => {
                if uri.is_empty() {
                    self.next = None;
                } else if self.cur.is_none() {
                    return self.load(uri, false);
                } else {
                    self.next = Some(uri);
                }
                self.lock_state().next_uri = self.next.clone();
            }
            EngineCommand::Pause => match self.cur.as_mut() {
                Some(t) if !t.started => t.pause_on_start = true,
                Some(_) if self.status == TransportStatus::Playing => {
                    if self.act("Pause", &[]) {
                        self.set_status(TransportStatus::Paused);
                    }
                }
                _ => {}
            },
            EngineCommand::Resume => match self.cur.as_mut() {
                Some(t) if !t.started => t.pause_on_start = false,
                Some(_) if self.status == TransportStatus::Paused => {
                    if self.act("Play", &[("Speed", "1")]) {
                        self.set_status(TransportStatus::Playing);
                    }
                }
                _ => {}
            },
            EngineCommand::Stop => {
                self.next = None;
                if let Some(t) = self.cur.take() {
                    self.act("Stop", &[]);
                    self.publish(EngineEvent::TrackEnded {
                        uri: t.uri,
                        reason: EndReason::Stopped,
                    });
                }
                self.go_stopped();
            }
            EngineCommand::Seek { ms } => self.seek(ms),
            EngineCommand::SetVolume { percent } => {
                let muted = self.volume.is_some_and(|v| v.1);
                self.set_volume(percent.min(100), muted);
            }
            EngineCommand::SetMute { muted } => {
                let percent = self.volume.map_or(100, |v| v.0);
                self.set_volume(percent, muted);
            }
            // The renderer decodes: no gain stage nor DSD output of ours.
            EngineCommand::SetGain { .. } | EngineCommand::SetDsdMode { .. } => {}
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, ricercar_audio::PlayerShared> {
        self.e.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// An AVTransport action on the current renderer; false when it failed.
    fn act(&mut self, action: &str, args: &[(&str, &str)]) -> bool {
        let Some(dev) = self.dev.clone() else {
            return false;
        };
        match dev.avt(action, args) {
            Ok(_) => true,
            Err(f) => {
                tracing::info!("upnp output: {}: {action}: {f}", dev.name);
                self.note(&f);
                false
            }
        }
    }

    fn set_volume(&mut self, percent: u32, muted: bool) {
        let Some(dev) = self.device().cloned() else {
            return;
        };
        let before = self.volume;
        self.volume = Some((percent, muted));
        if before.map(|v| v.0) != Some(percent) {
            let p = percent.to_string();
            if let Some(Err(f)) = dev.rcs("SetVolume", &[("DesiredVolume", &p)]) {
                tracing::info!("upnp output: {}: SetVolume: {f}", dev.name);
                self.note(&f);
            }
        }
        if before.map(|v| v.1) != Some(muted) {
            let m = if muted { "1" } else { "0" };
            if let Some(Err(f)) = dev.rcs("SetMute", &[("DesiredMute", m)]) {
                tracing::info!("upnp output: {}: SetMute: {f}", dev.name);
                self.note(&f);
            }
        }
        self.lock_state().chain.volume = percent;
    }

    /// The renderer's own volume; reported when it changed.
    fn read_volume(&mut self) {
        let Some(dev) = self.dev.clone() else { return };
        let Some(Ok(v)) = dev.rcs("GetVolume", &[]) else {
            return;
        };
        let Ok(percent) = arg(&v, "CurrentVolume").trim().parse::<u32>() else {
            return;
        };
        let muted = matches!(
            dev.rcs("GetMute", &[])
                .and_then(Result::ok)
                .as_deref()
                .map(|m| arg(m, "CurrentMute").trim()),
            Some("1" | "true" | "True")
        );
        let percent = percent.min(100);
        if self.volume != Some((percent, muted)) {
            self.volume = Some((percent, muted));
            self.lock_state().chain.volume = percent;
            self.publish(EngineEvent::Volume { percent, muted });
        }
    }

    /// Hand `uri` to the renderer. `replace`: a Load (the next slot goes).
    fn load(&mut self, uri: String, replace: bool) {
        if replace {
            self.next = None;
            self.lock_state().next_uri = None;
        }
        if let Some(t) = self.cur.take() {
            self.publish(EngineEvent::TrackEnded {
                uri: t.uri,
                reason: EndReason::Replaced,
            });
        }
        if let Err(message) = self.start(&uri, false) {
            tracing::warn!("{message}");
            self.publish(EngineEvent::Error {
                message,
                uri: None,
                http_status: None,
            });
            self.go_stopped();
        }
    }

    /// `upcoming`: the track follows the one that just ended.
    fn start(&mut self, uri: &str, upcoming: bool) -> Result<(), String> {
        let shown = redact_url(uri);
        let Some(dev) = self.device().cloned() else {
            return Err(format!(
                "{}: renderer not found on the network",
                self.name()
            ));
        };
        let served =
            serve::register(uri).ok_or_else(|| format!("{shown}: cannot be sent to a renderer"))?;
        let url = dev
            .addr()
            .and_then(|a| serve::url_for(&served, a))
            .ok_or_else(|| format!("{}: no route to the renderer", dev.name))?;
        let didl = metadata(uri, upcoming, &url, &served);
        // Some renderers ignore a new URI while they play or pause.
        if self.status != TransportStatus::Stopped {
            let _ = dev.avt("Stop", &[]);
        }
        let set = dev
            .avt(
                "SetAVTransportURI",
                &[("CurrentURI", &url), ("CurrentURIMetaData", &didl)],
            )
            .and_then(|_| dev.avt("Play", &[("Speed", "1")]));
        if let Err(f) = set {
            self.note(&f);
            return Err(format!("{}: {shown}: {f}", dev.name));
        }
        tracing::info!("upnp output: {} plays {shown}", dev.name);
        {
            let mut st = self.lock_state();
            st.track_uri = Some(uri.to_string());
            st.pos_ms = 0;
            st.dur_ms = None;
            st.seekable = false;
            st.stream_title = None;
        }
        self.cur = Some(Track {
            uri: uri.to_string(),
            served,
            started: false,
            since: Instant::now(),
            pos_ms: 0,
            dur_ms: None,
            pause_on_start: false,
            seek_on_start: None,
        });
        self.publish(EngineEvent::Buffering {
            uri: uri.to_string(),
            waiting: true,
        });
        self.last_poll = Instant::now();
        Ok(())
    }

    fn seek(&mut self, ms: u64) {
        let Some(t) = self.cur.as_mut() else { return };
        if !t.started {
            t.seek_on_start = Some(ms);
            return;
        }
        if t.dur_ms.is_none() {
            return; // a live stream
        }
        if self.act("Seek", &[("Unit", "REL_TIME"), ("Target", &hms(ms))]) {
            let dur_ms = self.cur.as_mut().map(|t| {
                t.pos_ms = ms;
                t.dur_ms
            });
            self.lock_state().pos_ms = ms;
            self.publish(EngineEvent::Position {
                pos_ms: ms,
                dur_ms: dur_ms.flatten(),
            });
        }
    }

    fn go_stopped(&mut self) {
        self.cur = None;
        {
            let mut st = self.lock_state();
            st.track_uri = None;
            st.next_uri = None;
            st.pos_ms = 0;
            st.dur_ms = None;
            st.seekable = false;
        }
        self.set_status(TransportStatus::Stopped);
    }

    fn poll(&mut self) {
        self.last_poll = Instant::now();
        self.polls = self.polls.wrapping_add(1);
        if self.cur.is_none() || self.polls.is_multiple_of(VOLUME_EVERY) {
            if self.dev.is_none() && self.cur.is_none() {
                // Idle and unknown: wait for a track before searching again.
                return;
            }
            self.read_volume();
        }
        if self.cur.is_none() {
            return;
        }
        let Some(dev) = self.dev.clone() else {
            return self.lost();
        };
        let transport = dev.avt("GetTransportInfo", &[]);
        let position = dev.avt("GetPositionInfo", &[]);
        let (transport, position) = match (transport, position) {
            (Ok(t), Ok(p)) => (t, p),
            (Err(f), _) | (_, Err(f)) => {
                self.unanswered += 1;
                if self.unanswered >= LOST_AFTER {
                    self.note(&f);
                    self.lost();
                }
                return;
            }
        };
        self.unanswered = 0;
        let state = arg(&transport, "CurrentTransportState").to_string();
        let error = arg(&transport, "CurrentTransportStatus") == "ERROR_OCCURRED";
        let track_uri = arg(&position, "TrackURI").to_string();
        let pos = parse_time(arg(&position, "RelTime"));
        let dur = parse_time(arg(&position, "TrackDuration")).filter(|&d| d > 0);
        self.on_state(&state, error, &track_uri, pos, dur);
    }

    /// The renderer stopped answering while playing.
    fn lost(&mut self) {
        let Some(t) = self.cur.take() else { return };
        let message = format!("{}: renderer not answering; playback stopped", self.name());
        tracing::warn!("{message}");
        self.publish(EngineEvent::Error {
            message,
            uri: None,
            http_status: None,
        });
        self.publish(EngineEvent::TrackEnded {
            uri: t.uri,
            reason: EndReason::Error,
        });
        self.next = None;
        self.go_stopped();
    }

    fn on_state(
        &mut self,
        state: &str,
        error: bool,
        track_uri: &str,
        pos: Option<u64>,
        dur: Option<u64>,
    ) {
        let Some(t) = self.cur.as_mut() else { return };
        let ours = track_uri.is_empty() || track_uri.contains(&t.served.path);
        if !t.started {
            match state {
                "PLAYING" | "PAUSED_PLAYBACK" if ours => self.started(pos, dur),
                _ if error || t.since.elapsed() > START_TIMEOUT => self.failed(),
                // STOPPED, TRANSITIONING, NO_MEDIA_PRESENT: still loading.
                _ => {}
            }
            return;
        }
        if !ours {
            tracing::info!("upnp output: {} plays something else now", self.name());
            return self.stopped(EndReason::Stopped);
        }
        // A stopped renderer may reset its position: keep the last one seen.
        if matches!(state, "PLAYING" | "PAUSED_PLAYBACK") {
            if let Some(p) = pos {
                t.pos_ms = p;
            }
            if dur.is_some() {
                t.dur_ms = dur;
            }
        }
        match state {
            "PLAYING" => {
                self.set_status(TransportStatus::Playing);
                self.position();
            }
            "PAUSED_PLAYBACK" => {
                self.set_status(TransportStatus::Paused);
                self.position();
            }
            "STOPPED" | "NO_MEDIA_PRESENT" => {
                let ended = t.dur_ms.is_some_and(|d| t.pos_ms + END_MARGIN_MS >= d);
                if error {
                    self.failed();
                } else if ended {
                    self.finished();
                } else {
                    self.stopped(EndReason::Stopped);
                }
            }
            _ => {} // TRANSITIONING
        }
    }

    fn position(&self) {
        let Some(t) = &self.cur else { return };
        {
            let mut st = self.lock_state();
            st.pos_ms = t.pos_ms;
            st.dur_ms = t.dur_ms;
        }
        self.publish(EngineEvent::Position {
            pos_ms: t.pos_ms,
            dur_ms: t.dur_ms,
        });
    }

    fn started(&mut self, pos: Option<u64>, dur: Option<u64>) {
        let Some(t) = self.cur.as_mut() else { return };
        t.started = true;
        t.pos_ms = pos.unwrap_or(0);
        t.dur_ms = dur;
        let (uri, pause, seek) = (t.uri.clone(), t.pause_on_start, t.seek_on_start.take());
        {
            let mut st = self.lock_state();
            st.dur_ms = dur;
            st.seekable = dur.is_some();
        }
        self.publish(EngineEvent::Buffering {
            uri: uri.clone(),
            waiting: false,
        });
        self.publish(EngineEvent::TrackStarted { uri, format: None });
        self.set_status(TransportStatus::Playing);
        if let Some(ms) = seek {
            self.seek(ms);
        }
        if pause && self.act("Pause", &[]) {
            self.set_status(TransportStatus::Paused);
        }
        self.position();
    }

    /// The track never started, or the renderer reports an error.
    fn failed(&mut self) {
        let Some(t) = self.cur.take() else { return };
        let http_status = *t
            .served
            .upstream_error
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let shown = redact_url(&t.uri);
        let message = match http_status {
            Some(code) => format!("{}: {shown}: HTTP {code}", self.name()),
            None => format!("{}: could not play {shown}", self.name()),
        };
        tracing::warn!("{message}");
        self.publish(EngineEvent::Error {
            message,
            uri: http_status.map(|_| t.uri.clone()),
            http_status,
        });
        self.publish(EngineEvent::TrackEnded {
            uri: t.uri,
            reason: EndReason::Error,
        });
        self.next = None;
        self.go_stopped();
    }

    fn stopped(&mut self, reason: EndReason) {
        let Some(t) = self.cur.take() else { return };
        self.publish(EngineEvent::TrackEnded { uri: t.uri, reason });
        self.next = None;
        self.go_stopped();
    }

    /// Played to its end: on to the next track, if any.
    fn finished(&mut self) {
        let Some(t) = self.cur.take() else { return };
        self.publish(EngineEvent::TrackEnded {
            uri: t.uri,
            reason: EndReason::Finished,
        });
        match self.next.take() {
            Some(next) => {
                self.lock_state().next_uri = None;
                if let Err(message) = self.start(&next, true) {
                    tracing::warn!("{message}");
                    self.publish(EngineEvent::Error {
                        message,
                        uri: None,
                        http_status: None,
                    });
                    self.go_stopped();
                }
            }
            None => self.go_stopped(),
        }
    }
}

/// DIDL-Lite for the renderer's display: tags of a local file, the queue
/// item's details when the controller is known, else a bare title.
fn metadata(uri: &str, upcoming: bool, url: &str, served: &Served) -> String {
    let mut info = super::queued_info(uri, upcoming).unwrap_or_else(|| {
        if uri.starts_with("file://") {
            TrackInfo::from_uri(uri)
        } else {
            TrackInfo {
                uri: uri.to_string(),
                ..Default::default()
            }
        }
    });
    if info.title.is_empty() {
        info.title = "ricercar".into();
    }
    let art = info
        .cover
        .as_deref()
        .filter(|c| c.starts_with("https://") || c.starts_with("http://"))
        .map(str::to_string);
    let protocol = serve::protocol_info(served);
    didl::ItemXml {
        id: "0",
        parent: "-1",
        info: &info,
        res_url: url,
        protocol_info: &protocol,
        size: None,
        channels: None,
        bitrate: None,
        art_url: art.as_deref(),
    }
    .document()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times() {
        assert_eq!(hms(3_725_400), "1:02:05");
        assert_eq!(parse_time("0:03:25"), Some(205_000));
        assert_eq!(parse_time("NOT_IMPLEMENTED"), None);
        assert_eq!(parse_time(""), None);
    }
}
