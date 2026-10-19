//! ricercar-audio — bit-perfect PCM pipeline.
//!
//! Decodes audio (symphonia) to interleaved i32 and hands it to a sink
//! (ALSA / null / file) configured at the *exact* track format: the pipeline
//! never resamples, never mixes, never truncates. If a device cannot play a
//! track at its native rate/depth, it is refused, not degraded. DSD files
//! (`dsd`) go out as DoP, native DSD or, when asked, converted PCM.

mod buffering;
mod client;
pub mod device;
pub mod dsd;
pub mod error;
pub mod fmt;
pub mod gain;
pub mod http;
pub mod player;
pub mod redact;
pub mod sink;
pub mod stream;

pub use device::{DeviceCaps, DeviceInfo, DeviceKind, list_devices, probe_device};
pub use error::{AudioError, Result};
pub use fmt::{Container, PcmFormat, Wire, negotiate};
pub use gain::{GainStage, TrackOpts};
pub use player::{
    ChainInfo, DsdChain, EndReason, EngineCommand, EngineEvent, PlayerHandle, PlayerShared,
    RemoteDriver, RemoteEngine, Subscriber, TransportStatus, register_remote_driver, spawn_player,
    spawn_player_with_sink,
};
pub use redact::{redact_url, redact_urls};
pub use sink::AudioSink;
pub use stream::TrackSource;
