//! What renderers fetch: each track handed to one gets a random path on a
//! small HTTP server of ours. Local files are served from disk (Range
//! included), cue and SACD tracks and DSD files as PCM WAV, network streams
//! (plugin URLs, radios) relayed: the renderer never sees their URL, which
//! may be on our loopback or carry credentials.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use crate::http::{self, Request};
use crate::media;

/// Tracks served at once: the current one, the next, and a few the
/// renderer may still be reading.
const KEEP: usize = 6;
const UPSTREAM_CONNECT: Duration = Duration::from_secs(10);
const UPSTREAM_READ: Duration = Duration::from_secs(30);

/// How a track reaches the renderer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    File,
    /// Decoded here: cue spans, SACD tracks, DSD files.
    Wav,
    Relay,
}

pub struct Served {
    /// Path on our server (`/<token>/track.<ext>`).
    pub path: String,
    pub uri: String,
    /// MIME type announced to the renderer.
    pub mime: String,
    kind: Kind,
    /// HTTP status of the last refused upstream fetch (relayed streams).
    pub upstream_error: Mutex<Option<u16>>,
}

struct Server {
    port: u16,
    items: Mutex<VecDeque<Arc<Served>>>,
}

static SERVER: LazyLock<Option<Arc<Server>>> = LazyLock::new(start);

fn start() -> Option<Arc<Server>> {
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)))
        .map_err(|e| tracing::warn!("upnp output: no media server: {e}"))
        .ok()?;
    let port = listener.local_addr().ok()?.port();
    let server = Arc::new(Server {
        port,
        items: Mutex::default(),
    });
    let s = server.clone();
    std::thread::Builder::new()
        .name("ricercar-out-http".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let s = s.clone();
                let _ = std::thread::Builder::new()
                    .name("ricercar-out-conn".into())
                    .spawn(move || s.handle(stream));
            }
        })
        .ok()?;
    tracing::info!("upnp output: media server on port {port}");
    Some(server)
}

/// Path secret of a served track.
fn token() -> Option<String> {
    let mut b = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .ok()?;
    Some(b.iter().map(|x| format!("{x:02x}")).collect())
}

fn ext_of(path: &str) -> Option<String> {
    let name = path.rsplit('/').next()?;
    let (_, ext) = name.rsplit_once('.')?;
    (ext.len() <= 5 && ext.chars().all(|c| c.is_ascii_alphanumeric()))
        .then(|| ext.to_ascii_lowercase())
}

/// How `uri` is served, and its MIME type.
fn classify(uri: &str) -> Option<(Kind, String)> {
    if uri.starts_with("http://") || uri.starts_with("https://") {
        let path = uri.split(['?', '#']).next().unwrap_or(uri);
        return Some((
            Kind::Relay,
            upstream_mime(uri).unwrap_or_else(|| {
                ext_of(path)
                    .map_or("audio/mpeg", |e| media::mime_of(&format!("x.{e}")))
                    .to_string()
            }),
        ));
    }
    if !uri.starts_with("file://") {
        return None;
    }
    let fragment = uri.split_once('#').map(|(_, f)| f);
    let path = ricercar_core::meta::uri_to_path(uri)?;
    let ext = ext_of(&path.to_string_lossy()).unwrap_or_default();
    let decoded = fragment.is_some_and(|f| f.starts_with("t=") || f.starts_with("sacd="))
        || matches!(ext.as_str(), "dsf" | "dff" | "iso");
    if decoded {
        Some((Kind::Wav, "audio/wav".into()))
    } else {
        Some((
            Kind::File,
            media::mime_of(&path.to_string_lossy()).to_string(),
        ))
    }
}

/// Content type the stream's server announces (a `HEAD`), when it does.
fn upstream_mime(url: &str) -> Option<String> {
    let r = ureq::head(url)
        .timeout(Duration::from_secs(5))
        .call()
        .ok()?;
    let t = r
        .header("content-type")?
        .split(';')
        .next()?
        .trim()
        .to_ascii_lowercase();
    (t.starts_with("audio/") || t == "application/ogg").then_some(t)
}

/// Serve `uri`; `None` when it cannot be (another scheme, no server).
pub fn register(uri: &str) -> Option<Arc<Served>> {
    let server = SERVER.as_ref()?;
    let (kind, mime) = classify(uri)?;
    let ext = match kind {
        Kind::Wav => "wav".to_string(),
        _ => media_ext(&mime),
    };
    let served = Arc::new(Served {
        path: format!("/{}/track.{ext}", token()?),
        uri: uri.to_string(),
        mime,
        kind,
        upstream_error: Mutex::new(None),
    });
    let mut items = server.items.lock().unwrap_or_else(|e| e.into_inner());
    items.push_back(served.clone());
    while items.len() > KEEP {
        items.pop_front();
    }
    Some(served)
}

fn media_ext(mime: &str) -> String {
    match mime {
        "audio/flac" | "audio/x-flac" => "flac",
        "audio/mpeg" | "audio/mp3" => "mp3",
        "audio/wav" | "audio/x-wav" | "audio/wave" => "wav",
        "audio/aiff" | "audio/x-aiff" => "aiff",
        "audio/mp4" | "audio/x-m4a" | "audio/aac" => "m4a",
        "audio/ogg" | "application/ogg" => "ogg",
        _ => "bin",
    }
    .to_string()
}

/// `http://<our address facing the renderer>:<port><path>`.
pub fn url_for(served: &Served, renderer: SocketAddr) -> Option<String> {
    let server = SERVER.as_ref()?;
    // A connected UDP socket sends nothing but tells which address routes there.
    let probe = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    probe.connect(renderer).ok()?;
    let ip = probe.local_addr().ok()?.ip();
    Some(format!("http://{ip}:{}{}", server.port, served.path))
}

/// protocolInfo of the `<res>` the renderer is given.
pub fn protocol_info(served: &Served) -> String {
    match served.kind {
        Kind::Wav => media::cue_protocol_info(),
        _ => media::protocol_info(&served.mime),
    }
}

impl Server {
    fn find(&self, path: &str) -> Option<Arc<Served>> {
        self.items
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|s| s.path == path)
            .cloned()
    }

    fn handle(&self, mut stream: TcpStream) {
        let _ = stream.set_write_timeout(Some(http::WRITE_TIMEOUT));
        let Some(req) = http::read_request(&mut stream) else {
            return;
        };
        let served = match req.method.as_str() {
            "GET" | "HEAD" => self.find(req.route()),
            _ => None,
        };
        let Some(s) = served else {
            return http::not_found(&mut stream);
        };
        match s.kind {
            Kind::File => match ricercar_core::meta::uri_to_path(&s.uri) {
                Some(p) => media::serve_file(&p, &req, &mut stream),
                None => http::not_found(&mut stream),
            },
            Kind::Wav => media::serve_wav(&s.uri, req.method == "HEAD", &mut stream),
            Kind::Relay => relay(&s, &req, &mut stream),
        }
    }
}

/// The stream relayed as its server sends it, ranges included.
fn relay(s: &Served, req: &Request, stream: &mut TcpStream) {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(UPSTREAM_CONNECT)
        .timeout_read(UPSTREAM_READ)
        .build();
    let mut up = agent.request(if req.method == "HEAD" { "HEAD" } else { "GET" }, &s.uri);
    if let Some(r) = req.header("range") {
        up = up.set("Range", r);
    }
    let resp = match up.call() {
        Ok(r) => r,
        Err(ureq::Error::Status(code, _)) => {
            tracing::info!(
                "upnp output: {}: HTTP {code}",
                ricercar_audio::redact_url(&s.uri)
            );
            *s.upstream_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(code);
            return http::write_response(stream, code, "text/plain", b"");
        }
        Err(e) => {
            tracing::info!("upnp output: {}: {e}", ricercar_audio::redact_url(&s.uri));
            return http::write_response(stream, 502, "text/plain", b"");
        }
    };
    let mut headers = vec![
        ("CONTENT-TYPE", s.mime.clone()),
        ("transferMode.dlna.org", "Streaming".to_string()),
        ("contentFeatures.dlna.org", media::dlna_features(&s.mime)),
    ];
    for (name, ours) in [
        ("content-length", "CONTENT-LENGTH"),
        ("content-range", "CONTENT-RANGE"),
        ("accept-ranges", "ACCEPT-RANGES"),
    ] {
        if let Some(v) = resp.header(name) {
            headers.push((ours, v.to_string()));
        }
    }
    if !http::write_head(stream, resp.status(), &headers) || req.method == "HEAD" {
        return;
    }
    let _ = std::io::copy(&mut resp.into_reader(), stream);
    let _ = stream.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_is_decoded_here() {
        let k = classify;
        assert_eq!(
            k("file:///m/a.flac"),
            Some((Kind::File, "audio/flac".into()))
        );
        assert_eq!(
            k("file:///m/album.flac#t=0,215.5"),
            Some((Kind::Wav, "audio/wav".into()))
        );
        assert_eq!(
            k("file:///m/disc.iso#sacd=2"),
            Some((Kind::Wav, "audio/wav".into()))
        );
        assert_eq!(k("file:///m/a.dsf"), Some((Kind::Wav, "audio/wav".into())));
        assert_eq!(k("plugin://qobuz/track/1"), None);
        assert_eq!(k("/bare/path.flac"), None);
    }

    #[test]
    fn extensions() {
        assert_eq!(ext_of("/a/b.FLAC").as_deref(), Some("flac"));
        assert_eq!(ext_of("/a/getFile"), None);
        assert_eq!(media_ext("audio/x-flac"), "flac");
    }
}
