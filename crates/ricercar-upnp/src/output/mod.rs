//! UPnP renderers on the network as audio outputs (`upnp:<uuid>`): we are
//! their control point. A renderer gets an URL on our own small HTTP server
//! for each track (local files, plugin streams, radios alike) and plays it
//! itself; AVTransport and RenderingControl drive it, and polling its state
//! feeds the engine events the controller expects from any output.

mod control;
mod engine;
mod search;
mod serve;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::Duration;

use ricercar_core::{Controller, TrackInfo};

pub(crate) use search::Device;

/// Prefix of a network output's device name.
pub const PREFIX: &str = "upnp:";

/// A renderer found on the network.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Renderer {
    /// Output name, `upnp:<uuid>`.
    pub device: String,
    /// Its friendly name (`Living room`).
    pub name: String,
    /// Manufacturer and model (`Marantz MODEL 40n`); may be empty.
    pub model: String,
}

/// Renderers seen so far, by uuid: a later search or the output in use
/// reuses them without searching again.
static KNOWN: LazyLock<Mutex<HashMap<String, Device>>> = LazyLock::new(Mutex::default);

fn remember(dev: &Device) {
    KNOWN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(dev.uuid.clone(), dev.clone());
}

fn known(uuid: &str) -> Option<Device> {
    KNOWN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(uuid)
        .cloned()
}

/// Make `upnp:` outputs playable: call once at startup, before the
/// controller spawns its output.
pub fn install() {
    ricercar_audio::register_remote_driver(PREFIX, Arc::new(engine::drive));
}

/// The controller whose queue names what renderers are sent (titles,
/// artists, covers on their display).
static CONTROLLER: LazyLock<Mutex<Weak<Controller>>> = LazyLock::new(Mutex::default);

pub fn attach(ctl: &Arc<Controller>) {
    *CONTROLLER.lock().unwrap_or_else(|e| e.into_inner()) = Arc::downgrade(ctl);
}

/// The queue item behind `uri`: the one with that URI, else (a plugin item
/// plays from a URL of its own) the current one, or the one after it when
/// `upcoming`.
fn queued_info(uri: &str, upcoming: bool) -> Option<TrackInfo> {
    let ctl = CONTROLLER
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .upgrade()?;
    let st = ctl.lock();
    let by_uri = st.queue.iter().find(|q| q.info.uri == uri);
    let at = st.current.map(|i| i + usize::from(upcoming));
    by_uri
        .or_else(|| at.and_then(|i| st.queue.get(i)))
        .map(|q| q.info.clone())
}

/// Renderers that answer an SSDP search within `wait` (blocks that long)
/// and offer AVTransport, ricercar's own renderer excepted.
pub fn discover(wait: Duration) -> Vec<Renderer> {
    let own = std::fs::read_to_string(crate::config_dir().join("udn")).unwrap_or_default();
    let mut found: Vec<Renderer> = search::search(search::MEDIA_RENDERER, wait)
        .into_iter()
        .filter(|d| d.uuid != own.trim())
        .map(|d| {
            remember(&d);
            Renderer {
                device: format!("{PREFIX}{}", d.uuid),
                name: d.name,
                model: d.model,
            }
        })
        .collect();
    found.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    found
}

/// Friendly name of a renderer met by an earlier search or played to.
pub fn known_name(device: &str) -> Option<String> {
    known(device.strip_prefix(PREFIX)?).map(|d| d.name)
}

/// The renderer behind `uuid`: known already, or searched for by its uuid.
fn locate(uuid: &str) -> Option<Device> {
    if let Some(d) = known(uuid) {
        return Some(d);
    }
    let d = search::search(&format!("uuid:{uuid}"), Duration::from_secs(3))
        .into_iter()
        .find(|d| d.uuid == uuid)?;
    remember(&d);
    Some(d)
}

/// Forget a renderer that stopped answering: the next use searches again
/// (it may have a new address).
fn forget(uuid: &str) {
    KNOWN.lock().unwrap_or_else(|e| e.into_inner()).remove(uuid);
}
