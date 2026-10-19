//! Readable names for PipeWire outputs (`pipewire:NODE="bluez_output…"`),
//! and what really plays when the chosen one is not connected: PipeWire's
//! ALSA plugin then falls back to its default sink.
//!
//! The sound server is asked off the UI thread (`pactl`), at most every few
//! seconds while playing; names seen once are kept in ui-state.json.

use std::cell::RefCell;
use std::time::{Duration, Instant};

use ricercar_audio::device::{pipewire_default_sink, pipewire_node, pipewire_sink_names};

use crate::app::{Ui, post};
use crate::text::{t, tf};

/// How long a listing of the sound server's sinks is trusted.
const REFRESH: Duration = Duration::from_secs(5);

/// What the sound server reported last.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sinks {
    /// Every sink: (node name, description).
    pub sinks: Vec<(String, String)>,
    /// The node PipeWire plays to when the chosen one is gone.
    pub default_sink: Option<String>,
}

impl Sinks {
    fn description(&self, node: &str) -> Option<&str> {
        self.sinks
            .iter()
            .find(|(n, _)| n == node)
            .map(|(_, d)| d.as_str())
    }

    /// Readable name of the default sink (its node name when not listed).
    fn default_name(&self) -> Option<&str> {
        let node = self.default_sink.as_deref()?;
        Some(self.description(node).unwrap_or(node))
    }
}

/// How the active output is shown.
#[derive(Debug, Clone, PartialEq)]
pub struct Shown {
    /// The chosen device.
    pub label: String,
    /// What actually plays: the default sink when the chosen one is gone.
    pub playing: String,
    /// Device hop of the signal path when the chosen node is not
    /// connected (without the "shared" wording).
    pub absent: Option<String>,
}

/// "WH-1000XM4 (Bluetooth, not bit-perfect)" (a device list entry) → "WH-1000XM4".
pub fn readable(description: &str) -> &str {
    description
        .strip_suffix(" (Bluetooth, not bit-perfect)")
        .or_else(|| description.strip_suffix(" (PipeWire, not bit-perfect)"))
        .unwrap_or(description)
}

/// Name for a node that is not listed: the one remembered, "Bluetooth
/// device", or the node name itself.
pub fn absent_name(node: &str, remembered: Option<&str>) -> String {
    match remembered {
        Some(n) => n.to_string(),
        None if node.starts_with("bluez_output.") => t("Bluetooth device").to_string(),
        None => node.to_string(),
    }
}

/// The label rules. `seen` is the sound server's last listing (`None`
/// before the first answer, or without a sound server), `remembered` the
/// name kept from earlier listings.
pub fn resolve(device: &str, seen: Option<&Sinks>, remembered: Option<&str>) -> Shown {
    let Some(node) = pipewire_node(device) else {
        return Shown {
            label: device.to_string(),
            playing: device.to_string(),
            absent: None,
        };
    };
    let Some(seen) = seen else {
        // Not asked yet: trust the name, claim nothing about the device.
        let label = absent_name(node, remembered);
        return Shown {
            playing: label.clone(),
            label,
            absent: None,
        };
    };
    if let Some(desc) = seen.description(node) {
        return Shown {
            label: desc.to_string(),
            playing: desc.to_string(),
            absent: None,
        };
    }
    let name = absent_name(node, remembered);
    let label = if remembered.is_some() || node.starts_with("bluez_output.") {
        tf("{name} (not connected)", &[("name", &name)])
    } else {
        name.clone()
    };
    let (playing, absent) = match seen.default_name() {
        Some(def) => (
            def.to_string(),
            tf(
                "{output} · default output; {device} not connected",
                &[("output", def), ("device", &name)],
            ),
        ),
        None => (
            t("PipeWire default output").to_string(),
            tf(
                "PipeWire default output; {device} not connected",
                &[("device", &name)],
            ),
        ),
    };
    Shown {
        label,
        playing,
        absent: Some(absent),
    }
}

#[derive(Default)]
struct State {
    seen: Option<Sinks>,
    asked: Option<Instant>,
    busy: bool,
    /// Bumped whenever `seen` changes, so the signal path is rebuilt.
    rev: u64,
}

thread_local! {
    static STATE: RefCell<State> = Default::default();
}

/// Changes whenever the sound server's answer does.
pub fn rev() -> u64 {
    STATE.with(|s| s.borrow().rev)
}

/// "BIG KNOB STUDIO: USB Audio" → "BIG KNOB STUDIO": the card name when
/// its device name is ALSA's generic one for USB cards.
pub fn card_label(description: &str) -> &str {
    description
        .strip_suffix(": USB Audio")
        .unwrap_or(description)
}

thread_local! {
    /// ALSA card names by device name, read once from the device list.
    static CARDS: RefCell<std::collections::HashMap<String, Option<String>>> =
        RefCell::new(std::collections::HashMap::new());
}

/// The card's name for an ALSA hardware device (`hw:3,0`), read from the
/// device list without opening anything; `None` when it isn't listed.
fn hw_name(device: &str) -> Option<String> {
    if !(device.starts_with("hw:") || device.starts_with("plughw:")) {
        return None;
    }
    if std::env::var_os("RICERCAR_SNAPSHOT").is_some() {
        return None;
    }
    CARDS.with(|c| {
        c.borrow_mut()
            .entry(device.to_string())
            .or_insert_with(|| {
                ricercar_audio::device::list_devices()
                    .into_iter()
                    .find(|d| d.name == device)
                    .map(|d| card_label(&d.description).to_string())
            })
            .clone()
    })
}

/// Forget the card names read so far (after a rescan of the devices).
pub fn forget_cards() {
    CARDS.with(|c| c.borrow_mut().clear());
}

/// How to show `device` with what is known now (never blocks).
pub fn describe(ui: &Ui, device: &str) -> Shown {
    if let Some(name) = hw_name(device) {
        return Shown {
            label: name.clone(),
            playing: name,
            absent: None,
        };
    }
    if ricercar_audio::device::classify(device) == ricercar_audio::DeviceKind::Network {
        // A UPnP renderer: the name it gave when last found or used.
        let name = ricercar_daemon::network_outputs::known_name(device)
            .unwrap_or_else(|| t("UPnP renderer").to_string());
        return Shown {
            label: name.clone(),
            playing: name,
            absent: None,
        };
    }
    let saved = ui.saved_state.borrow();
    let remembered = pipewire_node(device).and_then(|n| saved.device_name(n));
    STATE.with(|s| resolve(device, s.borrow().seen.as_ref(), remembered))
}

/// Ask the sound server again (off the UI thread) when `device` is a
/// PipeWire node and the last answer is old. `playing`: keep it fresh;
/// otherwise only a first answer is fetched.
pub fn poll(device: &str, playing: bool) {
    if pipewire_node(device).is_none() || std::env::var_os("RICERCAR_SNAPSHOT").is_some() {
        return;
    }
    let due = STATE.with(|s| {
        let mut s = s.borrow_mut();
        let stale = match s.asked {
            None => true,
            Some(at) => playing && at.elapsed() >= REFRESH,
        };
        if s.busy || !stale {
            return false;
        }
        s.busy = true;
        s.asked = Some(Instant::now());
        true
    });
    if !due {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("ricercar-sinks".into())
        .spawn(|| {
            let sinks = pipewire_sink_names();
            let default_sink = pipewire_default_sink();
            post(move |ui| {
                // No sinks at all: no sound server to ask, nothing to claim.
                let seen = (!sinks.is_empty()).then_some(Sinks {
                    sinks,
                    default_sink,
                });
                if let Some(seen) = &seen {
                    remember(ui, &seen.sinks);
                }
                STATE.with(|s| {
                    let mut s = s.borrow_mut();
                    s.busy = false;
                    if s.seen != seen {
                        s.seen = seen;
                        s.rev += 1;
                    }
                });
            });
        });
    if spawned.is_err() {
        STATE.with(|s| s.borrow_mut().busy = false);
    }
}

/// Keep the readable names of listed PipeWire outputs (ALSA cards have
/// their own `hw:` entry) in ui-state.json.
pub fn remember(ui: &Ui, sinks: &[(String, String)]) {
    let mut changed = false;
    {
        let mut st = ui.saved_state.borrow_mut();
        for (node, desc) in sinks {
            if !node.starts_with("alsa_output.") {
                changed |= st.remember_device(node, readable(desc), false);
            }
        }
    }
    if changed {
        save(ui);
    }
}

/// The user picked `device`: keep its name as the most recent.
pub fn remember_picked(ui: &Ui, device: &str, description: &str) {
    let Some(node) = pipewire_node(device) else {
        return;
    };
    let changed = ui
        .saved_state
        .borrow_mut()
        .remember_device(node, readable(description), true);
    if changed {
        save(ui);
    }
}

fn save(ui: &Ui) {
    let st = ui.saved_state.borrow().clone();
    if std::env::var_os("RICERCAR_SNAPSHOT").is_none() {
        crate::app::queue_state_save(ui, &st);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BT: &str = "pipewire:NODE=\"bluez_output.00_11_22_33_44_55.1\"";

    fn seen(default_sink: Option<&str>) -> Sinks {
        Sinks {
            sinks: vec![
                (
                    "alsa_output.pci-0000_00_1f.3.analog-stereo".into(),
                    "Built-in Audio".into(),
                ),
                ("bluez_output.66_77_88_99_AA_BB.1".into(), "Speaker".into()),
            ],
            default_sink: default_sink.map(Into::into),
        }
    }

    #[test]
    fn other_devices_keep_their_name() {
        let s = resolve("hw:1,0", Some(&seen(None)), None);
        assert_eq!((s.label.as_str(), s.playing.as_str()), ("hw:1,0", "hw:1,0"));
        assert_eq!(s.absent, None);
    }

    #[test]
    fn present_node_reads_its_description() {
        let d = "pipewire:NODE=\"bluez_output.66_77_88_99_AA_BB.1\"";
        let s = resolve(d, Some(&seen(None)), Some("Old name"));
        assert_eq!(s.label, "Speaker");
        assert_eq!(s.playing, "Speaker");
        assert_eq!(s.absent, None);
    }

    #[test]
    fn absent_remembered_node() {
        let s = resolve(
            BT,
            Some(&seen(Some("bluez_output.66_77_88_99_AA_BB.1"))),
            Some("Headset"),
        );
        assert_eq!(s.label, "Headset (not connected)");
        assert_eq!(s.playing, "Speaker");
        assert_eq!(
            s.absent.as_deref(),
            Some("Speaker · default output; Headset not connected")
        );
    }

    #[test]
    fn absent_unknown_bluetooth_node() {
        let s = resolve(BT, Some(&seen(None)), None);
        assert_eq!(s.label, "Bluetooth device (not connected)");
        assert_eq!(s.playing, "PipeWire default output");
        assert_eq!(
            s.absent.as_deref(),
            Some("PipeWire default output; Bluetooth device not connected")
        );
        // A default sink without a listed description shows its node.
        let s = resolve(BT, Some(&seen(Some("tunnel.office"))), None);
        assert_eq!(s.playing, "tunnel.office");
    }

    #[test]
    fn absent_unknown_other_node() {
        let d = "pipewire:NODE=\"tunnel.office\"";
        let s = resolve(
            d,
            Some(&seen(Some("alsa_output.pci-0000_00_1f.3.analog-stereo"))),
            None,
        );
        assert_eq!(s.label, "tunnel.office");
        assert_eq!(s.playing, "Built-in Audio");
        assert_eq!(
            s.absent.as_deref(),
            Some("Built-in Audio · default output; tunnel.office not connected")
        );
    }

    #[test]
    fn before_the_first_answer() {
        let s = resolve(BT, None, Some("Headset"));
        assert_eq!((s.label.as_str(), s.absent), ("Headset", None));
        assert_eq!(resolve(BT, None, None).playing, "Bluetooth device");
        let d = "pipewire:NODE=\"tunnel.office\"";
        assert_eq!(resolve(d, None, None).label, "tunnel.office");
    }

    #[test]
    fn card_names() {
        assert_eq!(card_label("BIG KNOB STUDIO: USB Audio"), "BIG KNOB STUDIO");
        assert_eq!(
            card_label("HD-Audio Generic: HDMI 0"),
            "HD-Audio Generic: HDMI 0"
        );
    }

    #[test]
    fn readable_names() {
        assert_eq!(
            readable("WH-1000XM4 (Bluetooth, not bit-perfect)"),
            "WH-1000XM4"
        );
        assert_eq!(readable("Office (PipeWire, not bit-perfect)"), "Office");
        assert_eq!(readable("Speaker (USB)"), "Speaker (USB)");
    }
}
