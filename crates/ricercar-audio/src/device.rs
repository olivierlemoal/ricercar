use std::collections::BTreeMap;
use std::ops::RangeInclusive;

use crate::dsd::native::NativeFormat;
use crate::error::{AudioError, Result};
use crate::fmt::Container;

/// Playback devices advertised to the user.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    /// ALSA device name, or `null` / `file:<path>`
    pub name: String,
    pub description: String,
    pub kind: DeviceKind,
    /// Long name of the sound card (from `/proc/asound/cards`), for `hw:`.
    pub card_name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    /// `hw:` card — direct, exclusive, no mixer in the path.
    Hardware,
    /// Any other ALSA name (plughw, default, pipewire's alsa bridge...).
    Virtual,
    /// Sink discarding data (testing / --device null).
    Null,
    /// Sink writing raw container bytes to a file (testing / bit-exact checks).
    File,
    /// UPnP renderer on the network (`upnp:<uuid>`): it fetches and decodes
    /// the stream itself, out of our sight.
    Network,
}

impl DeviceKind {
    /// True when we can guarantee no resampling or mixing happens on our side
    /// and the kernel hands samples to the codec untouched.
    pub fn is_bit_perfect(self) -> bool {
        matches!(
            self,
            DeviceKind::Hardware | DeviceKind::Null | DeviceKind::File
        )
    }
}

pub fn classify(name: &str) -> DeviceKind {
    if name == "null" {
        DeviceKind::Null
    } else if name.starts_with("file:") {
        DeviceKind::File
    } else if name.starts_with("hw:") {
        DeviceKind::Hardware
    } else if name.starts_with("upnp:") {
        DeviceKind::Network
    } else {
        DeviceKind::Virtual
    }
}

/// Card index -> (short name, long name), from `/proc/asound/cards`:
///
/// ```text
///  1 [K7             ]: USB-Audio - FiiO K7
///                       FiiO FiiO K7 at usb-0000:00:14.0-2, high speed
/// ```
pub fn parse_cards(text: &str) -> BTreeMap<u32, (String, String)> {
    let mut out = BTreeMap::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let t = line.trim_start();
        let Some((idx, rest)) = t.split_once(' ') else {
            continue;
        };
        let Ok(idx) = idx.parse::<u32>() else {
            continue;
        };
        let Some((_, head)) = rest.split_once("]: ") else {
            continue;
        };
        let short = head
            .split_once(" - ")
            .map_or(head, |(_, s)| s)
            .trim()
            .to_string();
        let long = match lines.peek() {
            Some(next) if next.starts_with("  ") => {
                let l = next.trim().to_string();
                lines.next();
                l
            }
            _ => short.clone(),
        };
        out.insert(idx, (short, long));
    }
    out
}

/// Playback PCMs from `/proc/asound/pcm` as (card, device, name):
/// `00-03: HDMI 0 : HDMI 0 : playback 1`.
pub fn parse_pcms(text: &str) -> Vec<(u32, u32, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Some((ids, rest)) = line.split_once(": ") else {
            continue;
        };
        if !rest.contains("playback") {
            continue;
        }
        let Some((card, dev)) = ids.split_once('-') else {
            continue;
        };
        let (Ok(card), Ok(dev)) = (card.trim().parse::<u32>(), dev.trim().parse::<u32>()) else {
            continue;
        };
        let desc = rest.split(" : ").next().unwrap_or("").trim().to_string();
        out.push((card, dev, desc));
    }
    out.sort_by_key(|&(c, d, _)| (c, d));
    out
}

/// Names of ALSA PCMs declared in the configuration (no device is opened).
fn alsa_pcm_hints() -> Option<Vec<(String, Option<String>)>> {
    let hints = alsa::device_name::HintIter::new_str(None, "pcm").ok()?;
    Some(
        hints
            .filter(|h| h.direction != Some(alsa::Direction::Capture))
            .filter_map(|h| {
                let desc = h.desc.map(|d| d.lines().next().unwrap_or("").to_string());
                h.name.map(|n| (n, desc))
            })
            .collect(),
    )
}

/// Build the device list from the raw sources; pure, for testability.
pub fn build_device_list(
    cards: &str,
    pcms: &str,
    hints: Option<&[(String, Option<String>)]>,
) -> Vec<DeviceInfo> {
    let cards = parse_cards(cards);
    let mut out = vec![DeviceInfo {
        name: "null".into(),
        description: "Null sink (discards audio, for tests)".into(),
        kind: DeviceKind::Null,
        card_name: None,
    }];
    for (card, dev, desc) in parse_pcms(pcms) {
        let names = cards.get(&card);
        let description = match names {
            Some((short, _)) if !desc.is_empty() => format!("{short}: {desc}"),
            Some((short, _)) => short.clone(),
            None => desc,
        };
        out.push(DeviceInfo {
            name: format!("hw:{card},{dev}"),
            description,
            kind: DeviceKind::Hardware,
            card_name: names.map(|(_, long)| long.clone()),
        });
    }
    let virtuals = [
        (
            "default",
            "ALSA default (may be resampled by PipeWire/Pulse)",
        ),
        ("pipewire", "PipeWire (mixed/resampled by the sound server)"),
        ("pulse", "PulseAudio (mixed/resampled by the sound server)"),
    ];
    for (name, fallback) in virtuals {
        // Without hints, still offer `default`: it exists on any sane setup.
        let hint = match hints {
            Some(h) => match h.iter().find(|(n, _)| n == name) {
                Some(found) => Some(found.1.clone()),
                None => continue,
            },
            None if name == "default" => None,
            None => continue,
        };
        let description = match hint.flatten() {
            Some(d) if !d.is_empty() => format!("{d} (not bit-perfect)"),
            _ => fallback.to_string(),
        };
        out.push(DeviceInfo {
            name: name.into(),
            description,
            kind: DeviceKind::Virtual,
            card_name: None,
        });
    }
    out
}

/// Enumerate playback devices: `null`, every `hw:` PCM (bit-perfect), then
/// `default`/`pipewire`/`pulse` when ALSA declares them. Never opens a device.
pub fn list_devices() -> Vec<DeviceInfo> {
    let cards = std::fs::read_to_string("/proc/asound/cards").unwrap_or_default();
    let pcms = std::fs::read_to_string("/proc/asound/pcm").unwrap_or_default();
    let hints = alsa_pcm_hints();
    let mut out = build_device_list(&cards, &pcms, hints.as_deref());
    if out.iter().any(|d| d.name == "pipewire") {
        out.extend(pipewire_sinks());
    }
    out
}

/// PipeWire outputs that are not ALSA cards (Bluetooth headsets and
/// speakers, network sinks…), reached through ALSA's `pipewire` plugin.
fn pipewire_sinks() -> Vec<DeviceInfo> {
    pactl(&["-f", "json", "list", "sinks"])
        .map(|out| parse_pipewire_sinks(&out))
        .unwrap_or_default()
}

/// Run `pactl` and return what it printed, when it succeeded.
fn pactl(args: &[&str]) -> Option<String> {
    std::process::Command::new("pactl")
        .args(args)
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
}

/// Every sink the sound server has right now (ALSA cards included), as
/// `(node name, description)`. Empty without PipeWire/PulseAudio.
pub fn pipewire_sink_names() -> Vec<(String, String)> {
    pactl(&["-f", "json", "list", "sinks"])
        .map(|out| parse_sink_names(&out))
        .unwrap_or_default()
}

/// The sink PipeWire plays to when no node is given (or the given one is
/// gone): `pactl get-default-sink`. Runs a process: keep it off the UI thread.
pub fn pipewire_default_sink() -> Option<String> {
    pactl(&["get-default-sink"]).and_then(|out| parse_default_sink(&out))
}

/// `pactl get-default-sink` output → the node name.
pub fn parse_default_sink(out: &str) -> Option<String> {
    let name = out.lines().next()?.trim();
    (!name.is_empty() && !name.contains(char::is_whitespace) && !name.contains('"'))
        .then(|| name.to_string())
}

/// Keep a stream on the PipeWire output it was opened on.
///
/// WirePlumber treats a stream linked to the default sink as following the
/// default from then on (PulseAudio behaviour): with the chosen speaker as
/// default, connecting other Bluetooth headphones made them the default and
/// moved playback to them. `node.dont-move` makes it ignore that, so the
/// stream stays on the `NODE` it names; a stream without one still plays to
/// the default, and one whose node is gone still falls back to it.
///
/// PipeWire's ALSA plugin reads extra stream properties from
/// `PIPEWIRE_ALSA`; one already set by the user is left alone. Call at the
/// start of `main`, before any thread exists.
pub fn pin_pipewire_streams() {
    if std::env::var_os("PIPEWIRE_ALSA").is_none() {
        // SAFETY: called before any other thread is started.
        unsafe { std::env::set_var("PIPEWIRE_ALSA", "{ node.dont-move = true }") };
    }
}

/// The node inside a `pipewire:NODE="<name>"` device name.
pub fn pipewire_node(device: &str) -> Option<&str> {
    device
        .strip_prefix("pipewire:NODE=")
        .map(|n| n.trim_matches('"'))
        .filter(|n| !n.is_empty())
}

/// `pactl -f json list sinks` → `(node name, description)` of every sink.
pub fn parse_sink_names(json: &str) -> Vec<(String, String)> {
    let Ok(serde_json::Value::Array(sinks)) = serde_json::from_str(json) else {
        return Vec::new();
    };
    sinks
        .iter()
        .filter_map(|s| {
            let name = s.get("name")?.as_str()?;
            if name.is_empty() {
                return None;
            }
            let desc = s
                .get("description")
                .and_then(|d| d.as_str())
                .filter(|d| !d.is_empty())
                .unwrap_or(name);
            Some((name.to_string(), desc.to_string()))
        })
        .collect()
}

/// `pactl -f json list sinks` → `pipewire:NODE="<name>"` devices. Sinks of
/// ALSA cards are left out: their `hw:` device plays them bit-perfect.
pub fn parse_pipewire_sinks(json: &str) -> Vec<DeviceInfo> {
    parse_sink_names(json)
        .into_iter()
        .filter_map(|(name, desc)| {
            // Quotes would break the ALSA argument.
            if name.starts_with("alsa_output.") || name.contains('"') {
                return None;
            }
            let via = if name.starts_with("bluez_output.") {
                "Bluetooth"
            } else {
                "PipeWire"
            };
            Some(DeviceInfo {
                name: format!("pipewire:NODE=\"{name}\""),
                description: format!("{desc} ({via}, not bit-perfect)"),
                kind: DeviceKind::Virtual,
                card_name: None,
            })
        })
        .collect()
}

/// What a device accepts, for a "device capabilities" panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceCaps {
    pub formats: Vec<Container>,
    pub rates: Vec<u32>,
    pub channels: RangeInclusive<u32>,
    /// ALSA DSD formats the driver offers (native DSD).
    pub dsd: Vec<NativeFormat>,
}

/// Rates worth reporting (the ones music actually comes in).
pub const PROBE_RATES: [u32; 14] = [
    8000, 11025, 16000, 22050, 32000, 44100, 48000, 88200, 96000, 176400, 192000, 352800, 384000,
    768000,
];

impl DeviceCaps {
    /// Assemble caps from per-format / per-rate acceptance tests.
    pub fn from_tests(
        format_ok: impl Fn(Container) -> bool,
        rate_ok: impl Fn(u32) -> bool,
        channels: RangeInclusive<u32>,
    ) -> DeviceCaps {
        DeviceCaps {
            formats: Container::ALL
                .into_iter()
                .filter(|&c| format_ok(c))
                .collect(),
            rates: PROBE_RATES.into_iter().filter(|&r| rate_ok(r)).collect(),
            channels,
            dsd: Vec::new(),
        }
    }

    /// Everything goes (null / file sinks).
    pub fn unrestricted() -> DeviceCaps {
        DeviceCaps {
            dsd: NativeFormat::ALL.to_vec(),
            ..DeviceCaps::from_tests(|_| true, |_| true, 1..=32)
        }
    }

    pub fn supports_rate(&self, rate: u32) -> bool {
        self.rates.contains(&rate)
    }
}

pub(crate) fn alsa_format(c: Container) -> alsa::pcm::Format {
    use alsa::pcm::Format;
    match c {
        Container::S16 => Format::S16LE,
        Container::S24 => Format::S24LE,
        Container::S24_3 => Format::S243LE,
        Container::S32 => Format::S32LE,
    }
}

/// Containers a hw configuration space accepts.
pub(crate) fn supported_containers(hw: &alsa::pcm::HwParams) -> Vec<Container> {
    Container::ALL
        .into_iter()
        .filter(|&c| hw.test_format(alsa_format(c)).is_ok())
        .collect()
}

/// Query what an ALSA device accepts. Opens the PCM (non-blocking, so a busy
/// device errors out instead of hanging) but never starts it: nothing is
/// played. `null` and `file:` sinks, and network renderers (which decode
/// themselves), report unrestricted caps.
pub fn probe_device(name: &str) -> Result<DeviceCaps> {
    if matches!(
        classify(name),
        DeviceKind::Null | DeviceKind::File | DeviceKind::Network
    ) {
        return Ok(DeviceCaps::unrestricted());
    }
    let err = |e: alsa::Error| AudioError::Alsa {
        device: name.to_string(),
        source: e,
    };
    let pcm = alsa::PCM::new(name, alsa::Direction::Playback, true).map_err(err)?;
    let hw = alsa::pcm::HwParams::any(&pcm).map_err(err)?;
    hw.set_access(alsa::pcm::Access::RWInterleaved)
        .map_err(err)?;
    let channels = hw.get_channels_min().map_err(err)?..=hw.get_channels_max().map_err(err)?;
    let formats = supported_containers(&hw);
    let dsd = NativeFormat::ALL
        .into_iter()
        .filter(|f| hw.test_format(f.alsa()).is_ok())
        .collect();
    Ok(DeviceCaps {
        dsd,
        ..DeviceCaps::from_tests(
            |c| formats.contains(&c),
            |r| hw.test_rate(r).is_ok(),
            channels,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CARDS: &str = " 0 [Generic        ]: HDA-Intel - HD-Audio Generic
                      HD-Audio Generic at 0xfcd80000 irq 110
 1 [K7             ]: USB-Audio - FiiO K7
                      FiiO FiiO K7 at usb-0000:00:14.0-2, high speed
";
    const PCMS: &str = "01-00: USB Audio : USB Audio : playback 1 : capture 1
00-03: HDMI 0 : HDMI 0 : playback 1
00-00: Generic Analog : Generic Analog : playback 1 : capture 1
00-02: Generic Alt : Generic Alt : capture 1
";

    #[test]
    fn cards_long_and_short_names() {
        let c = parse_cards(CARDS);
        assert_eq!(c[&0].0, "HD-Audio Generic");
        assert_eq!(c[&1].0, "FiiO K7");
        assert_eq!(c[&1].1, "FiiO FiiO K7 at usb-0000:00:14.0-2, high speed");
    }

    #[test]
    fn device_list_is_ordered_and_classified() {
        let hints: Vec<(String, Option<String>)> = vec![
            ("pulse".into(), Some("PulseAudio Sound Server".into())),
            ("default".into(), Some("Default ALSA Output".into())),
            ("sysdefault:CARD=K7".into(), None),
        ];
        let l = build_device_list(CARDS, PCMS, Some(&hints));
        let names: Vec<&str> = l.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(
            names,
            ["null", "hw:0,0", "hw:0,3", "hw:1,0", "default", "pulse"]
        );
        assert_eq!(l[1].description, "HD-Audio Generic: Generic Analog");
        assert_eq!(
            l[3].card_name.as_deref(),
            Some("FiiO FiiO K7 at usb-0000:00:14.0-2, high speed")
        );
        for d in &l {
            assert_eq!(d.kind, classify(&d.name), "{}", d.name);
        }
        assert!(!l[4].kind.is_bit_perfect());
    }

    #[test]
    fn default_offered_without_hints() {
        let l = build_device_list("", "", None);
        let names: Vec<&str> = l.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["null", "default"]);
    }

    #[test]
    fn pipewire_sinks_skip_alsa_cards() {
        let json = r#"[
            {"name":"alsa_output.pci-0000_e3_00.6.HiFi__Speaker__sink","description":"Speaker"},
            {"name":"bluez_output.00_11_22_33_44_55.1","description":"WH-1000XM4"},
            {"name":"tunnel.office","description":"Office"},
            {"name":"bad\"name","description":"x"}
        ]"#;
        let l = parse_pipewire_sinks(json);
        let names: Vec<&str> = l.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "pipewire:NODE=\"bluez_output.00_11_22_33_44_55.1\"",
                "pipewire:NODE=\"tunnel.office\""
            ]
        );
        assert_eq!(l[0].description, "WH-1000XM4 (Bluetooth, not bit-perfect)");
        assert_eq!(l[0].kind, DeviceKind::Virtual);
        assert!(parse_pipewire_sinks("not json").is_empty());
        // Every sink, ALSA cards included, for readable names.
        let all = parse_sink_names(json);
        assert_eq!(all.len(), 4);
        assert_eq!(
            all[0],
            (
                "alsa_output.pci-0000_e3_00.6.HiFi__Speaker__sink".to_string(),
                "Speaker".to_string()
            )
        );
        let no_desc = parse_sink_names(r#"[{"name":"tunnel.x"},{"name":""}]"#);
        assert_eq!(no_desc, [("tunnel.x".to_string(), "tunnel.x".to_string())]);
    }

    #[test]
    fn default_sink_output() {
        assert_eq!(
            parse_default_sink("bluez_output.00_11_22_33_44_55.1\n").as_deref(),
            Some("bluez_output.00_11_22_33_44_55.1")
        );
        assert_eq!(
            parse_default_sink("  tunnel.office  ").as_deref(),
            Some("tunnel.office")
        );
        assert_eq!(parse_default_sink(""), None);
        assert_eq!(parse_default_sink("\n"), None);
        assert_eq!(parse_default_sink("bad \"name\""), None);
    }

    #[test]
    fn pipewire_node_names() {
        assert_eq!(
            pipewire_node("pipewire:NODE=\"bluez_output.00_11_22_33_44_55.1\""),
            Some("bluez_output.00_11_22_33_44_55.1")
        );
        assert_eq!(
            pipewire_node("pipewire:NODE=tunnel.office"),
            Some("tunnel.office")
        );
        assert_eq!(pipewire_node("pipewire:NODE=\"\""), None);
        assert_eq!(pipewire_node("pipewire"), None);
        assert_eq!(pipewire_node("hw:1,0"), None);
    }

    #[test]
    fn live_listing_never_fails() {
        let l = list_devices();
        assert_eq!(l[0].name, "null");
    }

    #[test]
    fn caps_from_tests() {
        let caps = DeviceCaps::from_tests(
            |c| matches!(c, Container::S16 | Container::S32),
            |r| r % 44100 == 0,
            2..=2,
        );
        assert_eq!(caps.formats, [Container::S16, Container::S32]);
        assert_eq!(caps.rates, [44100, 88200, 176400, 352800]);
        assert!(caps.supports_rate(88200) && !caps.supports_rate(96000));
        assert_eq!(probe_device("null").unwrap(), DeviceCaps::unrestricted());
        assert_eq!(
            probe_device("file:/nonexistent").unwrap().formats,
            Container::ALL
        );
    }
}
