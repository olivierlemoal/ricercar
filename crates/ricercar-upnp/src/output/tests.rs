//! A controller playing to a ricercar renderer on loopback, through the
//! network output: what the renderer gets, and what comes back.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ricercar_audio::player::TransportStatus;
use ricercar_core::{Controller, Library, PlayContext, TrackInfo};

use crate::RendererHandle;

/// Renderers share our UDN (one config dir): one rig at a time.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn wait_until(secs: u64, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// Serves a fixture over HTTP in slices spread over about four seconds:
/// the test sinks play faster than real time, a slow stream keeps the
/// renderer playing long enough to be seen doing so.
fn slow_stream(name: &str) -> String {
    let body = std::fs::read(fixture_path(name)).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for mut s in listener.incoming().flatten() {
            let body = body.clone();
            std::thread::spawn(move || {
                let mut head = Vec::new();
                let mut b = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") && s.read(&mut b).unwrap_or(0) == 1 {
                    head.push(b[0]);
                }
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: audio/flac\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                if s.write_all(header.as_bytes()).is_err() || head.starts_with(b"HEAD") {
                    return;
                }
                for part in body.chunks(body.len().div_ceil(20)) {
                    if s.write_all(part).is_err() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
            });
        }
    });
    format!("http://127.0.0.1:{port}/{name}")
}

struct Pair {
    /// Plays to the renderer.
    player: Arc<Controller>,
    /// The renderer's own controller.
    renderer: Arc<Controller>,
    _handle: RendererHandle,
}

fn pair() -> Pair {
    super::install();
    let renderer = Arc::new(Controller::new(
        Arc::new(Library::in_memory().unwrap()),
        "null",
    ));
    let handle = crate::start_renderer(renderer.clone(), "ricercar-test").unwrap();
    let location = format!("http://127.0.0.1:{}/device.xml", handle.port);
    let dev = super::search::describe(&location).expect("renderer described");
    super::remember(&dev);
    let player = Arc::new(Controller::new(
        Arc::new(Library::in_memory().unwrap()),
        &format!("upnp:{}", dev.uuid),
    ));
    super::attach(&player);
    Pair {
        player,
        renderer,
        _handle: handle,
    }
}

fn track(uri: &str, title: &str) -> TrackInfo {
    TrackInfo {
        title: title.into(),
        ..TrackInfo::from_uri(uri)
    }
}

fn renderer_item(c: &Controller) -> Option<(String, String)> {
    c.lock()
        .current_item()
        .map(|q| (q.info.uri.clone(), q.info.title.clone()))
}

#[test]
fn plays_a_queue_through_the_renderer() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let p = pair();
    let sources = [
        slow_stream("tone_16_441.flac"),
        slow_stream("tone_24_96.flac"),
    ];
    let tracks = vec![track(&sources[0], "First"), track(&sources[1], "Second")];
    p.player.play_tracks(tracks, 0, PlayContext::None);

    // The renderer fetches from our server (never the source itself), and
    // shows what the queue says.
    let mut first = String::new();
    assert!(wait_until(10, || renderer_item(&p.renderer).is_some_and(
        |(uri, title)| {
            first = uri.clone();
            title == "First"
        }
    )));
    assert!(
        first.ends_with("/track.flac") && !sources.contains(&first),
        "{first}"
    );
    assert!(wait_until(10, || p.player.lock().status == TransportStatus::Playing));
    assert!(
        wait_until(5, || p.player.lock().pos_ms > 0),
        "position follows"
    );

    // At the end of the first track the renderer is handed the second.
    assert!(wait_until(15, || renderer_item(&p.renderer)
        .is_some_and(|(uri, title)| uri != first && title == "Second")));
    assert!(wait_until(10, || p.player.lock().current == Some(1)));
    // Then the queue ends.
    assert!(wait_until(15, || p.player.lock().status == TransportStatus::Stopped));
}

#[test]
fn transport_and_volume_follow_both_ways() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let p = pair();
    p.renderer.set_volume(37);
    // The renderer's own volume is shown, never pushed over it.
    assert!(wait_until(10, || p.player.lock().volume == 37));
    p.player.set_volume(52);
    assert!(wait_until(5, || p.renderer.lock().volume == 52));

    let source = slow_stream("tone_24_96.flac");
    p.player
        .play_tracks(vec![track(&source, "Tone")], 0, PlayContext::None);
    assert!(wait_until(10, || p.player.lock().status == TransportStatus::Playing));
    p.player.pause();
    assert!(wait_until(5, || p.renderer.lock().status == TransportStatus::Paused));
    assert_eq!(p.player.lock().status, TransportStatus::Paused);
    // Resumed from the renderer's side: we follow.
    p.renderer.play();
    assert!(wait_until(5, || p.player.lock().status == TransportStatus::Playing));
    p.player.stop();
    assert!(wait_until(5, || p.renderer.lock().status == TransportStatus::Stopped));
}

#[test]
fn local_files_are_served_with_ranges() {
    let path = fixture_path("tone_16_441.flac");
    let served = super::serve::register(&format!("file://{}", path.display())).unwrap();
    let url = super::serve::url_for(&served, "127.0.0.1:9".parse().unwrap()).unwrap();
    let rest = url.strip_prefix("http://").unwrap();
    let (host, route) = rest.split_at(rest.find('/').unwrap());
    let mut s = TcpStream::connect(host).unwrap();
    write!(
        s,
        "GET {route} HTTP/1.1\r\nHost: {host}\r\nRange: bytes=4-11\r\n\r\n"
    )
    .unwrap();
    let mut resp = Vec::new();
    s.read_to_end(&mut resp).unwrap();
    let split = resp.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&resp[..split]);
    assert!(head.starts_with("HTTP/1.1 206"), "{head}");
    assert_eq!(&resp[split + 4..], &std::fs::read(&path).unwrap()[4..12]);

    // Anything not registered is not there.
    let mut s = TcpStream::connect(host).unwrap();
    write!(
        s,
        "GET /{} HTTP/1.1\r\nHost: {host}\r\n\r\n",
        "0".repeat(32)
    )
    .unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    assert!(resp.starts_with("HTTP/1.1 404"), "{resp}");
}
