//! DSD: DSF and DFF readers, and the three ways DSD reaches a device.
//!
//! - `dop`: DSD over PCM, 16 DSD bits per channel in 24-bit PCM frames
//!   marked 0x05 / 0xFA, at a sixteenth of the DSD rate. Bit-perfect.
//! - `native`: ALSA's DSD sample formats (DSD_U8, DSD_U16, DSD_U32).
//!   Bit-perfect.
//! - `pcm`: decimated to 24-bit PCM (see `pcm.rs`). Not bit-perfect.
//!
//! Readers implement `source::DsdSource`; `play::DsdPlayback` turns any of
//! them into the engine's i32 samples for the chosen mode.

use std::io;
use std::path::Path;

use crate::device::DeviceKind;

pub mod dff;
pub mod dop;
pub mod dsf;
#[doc(hidden)]
pub mod fixture;
pub mod id3;
pub mod native;
pub mod pcm;
pub mod play;
pub mod sacd;
#[allow(clippy::len_without_is_empty)]
pub mod source;

pub use source::{DsdFormat, DsdSource};

/// Idle pattern: as many ones as zeros, heard as silence.
pub const SILENCE: u8 = 0x69;

/// How DSD reaches an output, chosen per device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum DsdMode {
    /// Converted to 24-bit PCM: plays anywhere, not bit-perfect.
    #[default]
    Pcm,
    /// DSD over PCM: needs a DAC that recognises the markers.
    Dop,
    /// ALSA DSD sample formats: needs a driver that offers them.
    Native,
}

impl DsdMode {
    pub const ALL: [DsdMode; 3] = [DsdMode::Pcm, DsdMode::Dop, DsdMode::Native];

    pub fn label(self) -> &'static str {
        match self {
            DsdMode::Pcm => "pcm",
            DsdMode::Dop => "dop",
            DsdMode::Native => "native",
        }
    }

    pub fn from_label(s: &str) -> Option<DsdMode> {
        DsdMode::ALL.into_iter().find(|m| m.label() == s)
    }

    /// The DSD bits reach the DAC untouched.
    pub fn is_bit_perfect(self) -> bool {
        self != DsdMode::Pcm
    }

    /// DoP and native DSD only ever go to an output that hands bytes to
    /// the DAC untouched: a `hw:` card, or the null and file sinks. A sound
    /// server or plug device would mix or resample them into noise, so it
    /// gets PCM; so does a network renderer, which is sent PCM.
    pub fn guarded(self, kind: DeviceKind) -> DsdMode {
        match kind {
            DeviceKind::Virtual | DeviceKind::Network => DsdMode::Pcm,
            _ => self,
        }
    }
}

/// DSD bit rates (2.8224 MHz and up, multiples of 44.1 kHz) as stored in a
/// sample-rate column, so callers can tell them from PCM rates.
pub fn is_dsd_rate(rate: u32) -> bool {
    rate >= 2_822_400 && rate.is_multiple_of(44_100)
}

/// "DSD64", "DSD128"…
pub fn rate_label(rate: u32) -> String {
    format!("DSD{}", rate / 44_100)
}

/// `.dsf` and `.dff`, by extension (of a path or a URI, fragment allowed).
pub fn is_dsd_path(path: &str) -> bool {
    let ext = path
        .rsplit_once('.')
        .map(|(_, e)| e.split(['#', '?']).next().unwrap_or(e).to_ascii_lowercase());
    matches!(ext.as_deref(), Some("dsf" | "dff"))
}

/// A DSD file or a track of a SACD image (path, key or URI).
pub fn is_dsd_uri(s: &str) -> bool {
    is_dsd_path(s) || sacd::split_key(s).is_some()
}

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// Refuse what the pipeline does not handle yet.
fn check_format(f: DsdFormat) -> io::Result<()> {
    if f.channels == 0 {
        return Err(bad("no channels"));
    }
    if f.channels > 2 {
        return Err(bad(format!(
            "{} channels: only mono and stereo DSD are supported for now",
            f.channels
        )));
    }
    if !is_dsd_rate(f.rate) {
        return Err(bad(format!("unsupported DSD rate {} Hz", f.rate)));
    }
    Ok(())
}

/// Open a DSF or DFF file, recognised by its first bytes.
pub fn open_path(path: &Path) -> io::Result<Box<dyn DsdSource>> {
    use std::io::{Read, Seek};
    let mut f = std::fs::File::open(path)?;
    let mut magic = [0u8; 4];
    f.read_exact(&mut magic)?;
    f.rewind()?;
    match &magic {
        b"DSD " => Ok(Box::new(dsf::DsfReader::new(f)?)),
        b"FRM8" => Ok(Box::new(dff::DffReader::new(f)?)),
        _ => Err(bad("not a DSF or DFF file")),
    }
}

/// What the library needs to know about a DSD file.
#[derive(Debug, Clone, PartialEq)]
pub struct FileInfo {
    pub format: DsdFormat,
    /// Bytes per channel.
    pub len: u64,
    /// "DSF", "DFF" or "DST" (DFF with DST compression).
    pub codec: &'static str,
    pub tags: Vec<(String, String)>,
    pub picture: Option<(Vec<u8>, String)>,
}

impl FileInfo {
    pub fn duration_ms(&self) -> u64 {
        self.len * 8 * 1000 / self.format.rate.max(1) as u64
    }
}

/// Format, length and tags of a DSD file, without decoding it.
pub fn probe(path: &Path) -> io::Result<FileInfo> {
    use std::io::{Read, Seek};
    let mut f = std::fs::File::open(path)?;
    let mut magic = [0u8; 4];
    f.read_exact(&mut magic)?;
    f.rewind()?;
    let (format, len, codec, raw_id3, mut tags) = match &magic {
        b"DSD " => {
            let r = dsf::DsfReader::new(f)?;
            (
                r.format(),
                r.len().unwrap_or(0),
                "DSF",
                r.id3().to_vec(),
                r.tags(),
            )
        }
        b"FRM8" => {
            let r = dff::DffReader::new_unchecked(f)?;
            let codec = if r.is_dst() { "DST" } else { "DFF" };
            (
                r.format(),
                r.len().unwrap_or(0),
                codec,
                r.id3().to_vec(),
                r.tags(),
            )
        }
        _ => return Err(bad("not a DSF or DFF file")),
    };
    let picture = id3::parse(&raw_id3).and_then(|t| t.picture);
    tags.dedup_by(|a, b| a.0 == b.0);
    Ok(FileInfo {
        format,
        len,
        codec,
        tags,
        picture,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_keeps_bits_off_shared_devices() {
        for m in DsdMode::ALL {
            assert_eq!(m.guarded(DeviceKind::Virtual), DsdMode::Pcm);
            assert_eq!(m.guarded(DeviceKind::Hardware), m);
            assert_eq!(m.guarded(DeviceKind::File), m);
        }
        for name in ["default", "pipewire", "plughw:1,0", "pulse", "dmix"] {
            let kind = crate::device::classify(name);
            assert_eq!(DsdMode::Dop.guarded(kind), DsdMode::Pcm, "{name}");
            assert_eq!(DsdMode::Native.guarded(kind), DsdMode::Pcm, "{name}");
        }
        assert_eq!(
            DsdMode::Dop.guarded(crate::device::classify("hw:2,0")),
            DsdMode::Dop
        );
    }

    #[test]
    fn labels_and_rates() {
        for m in DsdMode::ALL {
            assert_eq!(DsdMode::from_label(m.label()), Some(m));
        }
        assert!(is_dsd_rate(2_822_400) && is_dsd_rate(11_289_600));
        assert!(!is_dsd_rate(384_000) && !is_dsd_rate(768_000));
        assert_eq!(rate_label(5_644_800), "DSD128");
        assert!(is_dsd_path("/a/b.DSF") && is_dsd_path("x.dff") && !is_dsd_path("x.flac"));
        assert!(is_dsd_path("file:///a/b.dff#t=1,2") && !is_dsd_path("file:///a.dsf/b.wav"));
        assert!(is_dsd_uri("file:///a/disc.iso#sacd=3") && is_dsd_uri("/a/disc.iso#sacd=12"));
        assert!(!is_dsd_uri("file:///a/disc.iso") && !is_dsd_uri("/a/b.iso#sacd=x"));
        assert_eq!(
            sacd::split_key("/m/#1/d.iso#sacd=7"),
            Some(("/m/#1/d.iso", 7))
        );
        assert_eq!(sacd::split_key("/m/d.iso#sacd=0"), None);
    }
}
