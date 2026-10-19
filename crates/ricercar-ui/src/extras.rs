//! Radio, settings and integrations.

use std::rc::Rc;

use ricercar_core::config::{DsdOutput, ReplayGain, Theme as ThemeCfg};
use ricercar_core::writer::Durability;
use ricercar_core::{PlayContext, TrackInfo};
use ricercar_online::radio::{RadioBrowser, Station, StationQuery};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use crate::app::{THUMB, Ui, post, with_ui};
use crate::images::Source;
use crate::text::{t, tf};
use crate::{DeviceRow, StationRow};

pub const ACCENTS: [&str; 7] = [
    "#d4a35a", "#e0785a", "#d66a9c", "#8b7cf6", "#4f9cf0", "#3fb8a8", "#7cc46a",
];

#[derive(Default)]
pub struct Extras {
    results: Vec<Station>,
    favs: Vec<Station>,
    radio: Option<RadioBrowser>,
    radio_loaded: bool,
    lastfm_token: Option<String>,
    /// Latest radio query: older answers arriving late are dropped.
    radio_serial: u64,
    /// Pending debounced edits (radio query, settings text, LB token).
    debounce: std::collections::HashMap<&'static str, u64>,
    stations: Option<Rc<VecModel<StationRow>>>,
    /// Network status revision shown in the settings.
    network_rev: Option<u64>,
    /// Interface names behind the settings choice ("" first: all).
    net_ifaces: Vec<String>,
    /// Devices whose capabilities panel is open, being probed, or failed.
    pub expanded: std::collections::HashSet<String>,
    probing: std::collections::HashSet<String>,
    probe_errors: std::collections::HashMap<String, String>,
    /// UPnP renderers found by the last network search (`None` before the
    /// first answer), and a search in flight.
    renderers: Option<Vec<ricercar_daemon::network_outputs::Renderer>>,
    searching: bool,
    /// Library sample rates by library revision.
    histogram: Option<(u64, Vec<ricercar_core::RateCount>)>,
    fav_model: Option<Rc<VecModel<StationRow>>>,
}

fn luminance(c: [f64; 3]) -> f64 {
    let lin = |v: f64| {
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(c[0]) + 0.7152 * lin(c[1]) + 0.0722 * lin(c[2])
}

fn contrast(a: [f64; 3], b: [f64; 3]) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// An accent usable as text on the theme background (WCAG 4.5:1): lifted
/// toward white on dark, pushed toward black on light, hue preserved.
pub fn readable_accent(rgb: [u8; 3], dark: bool) -> slint::Color {
    let bg = if dark {
        [0x0f, 0x0f, 0x11]
    } else {
        [0xf6, 0xf5, 0xf2]
    };
    let bg = bg.map(|v| v as f64 / 255.0);
    let target = if dark { 1.0 } else { 0.0 };
    let mut c = rgb.map(|v| v as f64 / 255.0);
    for _ in 0..40 {
        if contrast(c, bg) >= 4.6 {
            break;
        }
        c = c.map(|v| v + (target - v) * 0.08);
    }
    let [r, g, b] = c.map(|v| (v.clamp(0.0, 1.0) * 255.0).round() as u8);
    slint::Color::from_rgb_u8(r, g, b)
}

pub fn apply_cover_accent(ui: &Ui) {
    let dark = ui.ctx.config.read().unwrap().ui.theme == ThemeCfg::Dark;
    let c = match ui.st.borrow().cover_rgb {
        Some(rgb) => readable_accent(rgb, dark),
        None => slint::Color::from_argb_u8(0, 0, 0, 0),
    };
    ui.window.global::<crate::Theme>().set_cover_accent(c);
}

fn parse_hex(s: &str) -> Option<slint::Color> {
    let s = s.trim_start_matches('#');
    let v = u32::from_str_radix(s, 16).ok()?;
    (s.len() == 6).then(|| slint::Color::from_rgb_u8((v >> 16) as u8, (v >> 8) as u8, v as u8))
}

// ------------------------------------------------------------------ radio

fn favs_path() -> std::path::PathBuf {
    ricercar_core::config::data_dir().join("radio.json")
}

fn station_row(ui: &Ui, s: &Station, fav: bool) -> StationRow {
    let key = if s.favicon.is_empty() {
        String::new()
    } else {
        s.favicon.clone()
    };
    let cover = if key.is_empty() {
        Default::default()
    } else {
        ui.cover(&key, Source::Url(key.clone()), None, THUMB)
    };
    let mut sub = vec![];
    if !s.country.is_empty() {
        sub.push(s.country.clone());
    }
    if !s.codec.is_empty() {
        sub.push(if s.bitrate > 0 {
            format!("{} {} kbit/s", s.codec, s.bitrate)
        } else {
            s.codec.clone()
        });
    }
    StationRow {
        uuid: s.uuid.clone().into(),
        name: s.name.trim().to_string().into(),
        sub: sub.join(" · ").into(),
        tags: s
            .tags
            .iter()
            .take(4)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ")
            .into(),
        cover,
        ckey: key.into(),
        fav,
    }
}

fn show_stations(ui: &Ui) {
    let (results, favs) = {
        let ex = ui.extras.borrow();
        (ex.results.clone(), ex.favs.clone())
    };
    let fav_ids: std::collections::HashSet<&str> = favs.iter().map(|s| s.uuid.as_str()).collect();
    let rows: Vec<_> = results
        .iter()
        .map(|s| station_row(ui, s, fav_ids.contains(s.uuid.as_str())))
        .collect();
    let frows: Vec<_> = favs.iter().map(|s| station_row(ui, s, true)).collect();
    let app = ui.app();
    let m = Rc::new(VecModel::from(rows));
    let fm = Rc::new(VecModel::from(frows));
    app.set_stations(ModelRc::from(m.clone()));
    app.set_radio_favs(ModelRc::from(fm.clone()));
    let mut ex = ui.extras.borrow_mut();
    ex.stations = Some(m);
    ex.fav_model = Some(fm);
}

pub fn refill_station_covers(ui: &Ui) {
    let ex = ui.extras.borrow();
    let loader = ui.loader.borrow();
    for m in [&ex.stations, &ex.fav_model].into_iter().flatten() {
        for i in 0..m.row_count() {
            let mut r = m.row_data(i).unwrap();
            if !r.ckey.is_empty()
                && r.cover.size().width == 0
                && let Some(img) = loader.peek(&r.ckey, THUMB)
            {
                r.cover = img;
                m.set_row_data(i, r);
            }
        }
    }
}

/// Run `f` once typing has paused for `ms` (per `key`).
fn debounce(ui: &Rc<Ui>, key: &'static str, ms: u64, f: impl FnOnce(&Rc<Ui>) + 'static) {
    let serial = {
        let mut ex = ui.extras.borrow_mut();
        let s = ex.debounce.entry(key).or_insert(0);
        *s += 1;
        *s
    };
    slint::Timer::single_shot(std::time::Duration::from_millis(ms), move || {
        with_ui(|ui| {
            if ui.extras.borrow().debounce.get(key) == Some(&serial) {
                f(ui);
            }
        })
    });
}

fn radio_text_search(ui: &Rc<Ui>, q: &str) {
    let q = q.trim().to_string();
    if q.is_empty() {
        radio_query(ui, StationQuery::default(), t("Top stations").into());
    } else {
        let query = StationQuery {
            name: Some(q.clone()),
            limit: 80,
            ..Default::default()
        };
        radio_query(ui, query, format!("{} · {q}", t("Stations")));
    }
}

fn radio_query(ui: &Rc<Ui>, q: StationQuery, label: String) {
    ui.app().set_radio_status(t("Searching…").into());
    let serial = {
        let mut ex = ui.extras.borrow_mut();
        ex.radio_serial += 1;
        ex.radio_serial
    };
    let radio = ui.extras.borrow().radio.clone();
    std::thread::spawn(move || {
        let radio = radio.unwrap_or_else(RadioBrowser::discover);
        let res = if q.name.is_none() && q.tag.is_none() {
            radio.top_clicked(60)
        } else {
            radio.search(&q)
        };
        post(move |ui| {
            ui.extras.borrow_mut().radio = Some(radio);
            if ui.extras.borrow().radio_serial != serial {
                return;
            }
            let app = ui.app();
            match res {
                Ok(list) => {
                    app.set_radio_status(
                        if list.is_empty() {
                            t("No station found").to_string()
                        } else {
                            label
                        }
                        .into(),
                    );
                    ui.extras.borrow_mut().results = list;
                    show_stations(ui);
                }
                Err(e) => {
                    tracing::warn!("radio: {e}");
                    app.set_radio_status(t("Radio Browser is unreachable").into());
                }
            }
        });
    });
}

pub fn load_radio(ui: &Rc<Ui>) {
    if !ui.ctx.config.read().unwrap().online.radio {
        return;
    }
    if ui.extras.borrow().radio_loaded {
        show_stations(ui);
        return;
    }
    ui.extras.borrow_mut().radio_loaded = true;
    let favs: Vec<Station> = std::fs::read(favs_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    ui.extras.borrow_mut().favs = favs;
    let tags = [
        "jazz",
        "classical",
        "ambient",
        "electronic",
        "rock",
        "chillout",
        "news",
        "lounge",
        "baroque",
        "soul",
        "funk",
        "hiphop",
        "world",
        "80s",
    ];
    let model: Vec<SharedString> = tags.iter().map(|s| (*s).into()).collect();
    ui.app().set_radio_tags(ModelRc::new(VecModel::from(model)));
    show_stations(ui);
    radio_query(ui, StationQuery::default(), t("Top stations").into());
}

fn play_station(ui: &Rc<Ui>, s: &Station) {
    let info = TrackInfo {
        uri: s.url_resolved.clone(),
        title: s.name.trim().to_string(),
        artist: Some(
            [
                s.country.clone(),
                s.tags.first().cloned().unwrap_or_default(),
            ]
            .into_iter()
            .filter(|x| !x.is_empty())
            .collect::<Vec<_>>()
            .join(" · "),
        ),
        cover: (!s.favicon.is_empty()).then(|| s.favicon.clone()),
        codec: (!s.codec.is_empty()).then(|| s.codec.clone()),
        live: true,
        ..Default::default()
    };
    ui.ctx.ctl.play_tracks(vec![info], 0, PlayContext::Radio);
    let radio = ui.extras.borrow().radio.clone();
    let uuid = s.uuid.clone();
    std::thread::spawn(move || {
        // Radio Browser asks clients to report plays.
        if let Some(r) = radio {
            let _ = r.count_click(&uuid);
        }
    });
}

/// radio.json holds the user's picks: durable, written off the UI thread.
fn save_favs(ui: &Ui) {
    let favs = ui.extras.borrow().favs.clone();
    if let Ok(json) = serde_json::to_vec_pretty(&favs) {
        ui.ctx.io.submit(&favs_path(), json, Durability::Durable);
    }
}

// ------------------------------------------------------------------ settings

pub fn load_settings(ui: &Rc<Ui>) {
    let cfg = ui.ctx.config.read().unwrap().clone();
    let app = ui.app();
    refresh_devices(ui);
    probe_devices(ui);
    find_renderers(ui);
    app.set_replaygain(match cfg.audio.replaygain {
        ReplayGain::Off => 0,
        ReplayGain::Track => 1,
        ReplayGain::Album => 2,
        ReplayGain::Auto => 3,
    });
    app.set_preamp(cfg.audio.preamp_db);
    app.set_restore_session(cfg.audio.restore_session);
    app.set_continuous_playback(cfg.audio.continuous_playback);
    app.set_fixed_volume(cfg.audio.fixed_volume);
    app.set_watch_library(cfg.library.watch);
    app.set_renderer_name(cfg.network.name.clone().into());
    app.set_renderer_enabled(cfg.network.renderer);
    app.set_server_enabled(cfg.network.media_server);
    refresh_interfaces(ui);
    refresh_network_status(ui);
    app.set_lb_token(cfg.scrobble.listenbrainz_token.clone().into());
    if cfg.scrobble.listenbrainz_token.is_empty() {
        app.set_lb_status(t("Not connected").into());
    } else if app.get_lb_status().is_empty() {
        check_listenbrainz(ui, cfg.scrobble.listenbrainz_token.clone());
    }
    app.set_lastfm_key(cfg.scrobble.lastfm_api_key.clone().into());
    app.set_lastfm_secret(cfg.scrobble.lastfm_secret.clone().into());
    app.set_lastfm_status(
        if cfg.scrobble.lastfm_session.is_empty() {
            String::new()
        } else {
            format!("{} {}", t("Connected as"), cfg.scrobble.lastfm_user)
        }
        .into(),
    );
    app.set_lyrics_online(cfg.online.lyrics);
    app.set_covers_online(cfg.online.cover_art);
    app.set_plugins_online(cfg.online.plugin_catalog);
    app.set_updates_online(cfg.online.updates);
    app.set_notifications(cfg.ui.notifications);
    app.set_close_to_tray(cfg.ui.close_to_tray);
    app.set_theme_dark(cfg.ui.theme == ThemeCfg::Dark);
    app.set_ui_language(crate::text::language_index(&cfg.ui.language));
    app.set_adaptive_colors(cfg.ui.adaptive_colors);
    app.set_estimate_word_timing(cfg.ui.estimate_word_timing);
    app.set_accent_choice(
        ACCENTS
            .iter()
            .position(|a| a.eq_ignore_ascii_case(&cfg.ui.accent))
            .unwrap_or(0) as i32,
    );
    refresh_library_rows(ui);
}

/// Choices for the network interface: all, then each interface that is up
/// with its IPv4 address; the configured one stays listed when it is gone.
fn interface_choices(
    current: &str,
    up: &[(String, std::net::Ipv4Addr)],
) -> (Vec<String>, Vec<String>, usize) {
    let mut names = vec![String::new()];
    let mut labels = vec![t("All interfaces").to_string()];
    for (name, ip) in up {
        names.push(name.clone());
        labels.push(format!("{name} · {ip}"));
    }
    if !current.is_empty() && !names.iter().any(|n| n == current) {
        names.push(current.to_string());
        labels.push(format!("{current} ({})", t("not available")));
    }
    let selected = names.iter().position(|n| n == current).unwrap_or(0);
    (names, labels, selected)
}

/// The network interface choice, read afresh (settings page opened).
pub fn refresh_interfaces(ui: &Ui) {
    let current = ui.ctx.config.read().unwrap().network.interface.clone();
    let (names, labels, selected) =
        interface_choices(&current, &ricercar_daemon::network_interfaces());
    let app = ui.app();
    let labels: Vec<SharedString> = labels.into_iter().map(Into::into).collect();
    app.set_net_ifaces(ModelRc::new(VecModel::from(labels)));
    app.set_net_iface(selected as i32);
    ui.extras.borrow_mut().net_ifaces = names;
}

/// The status line under the network toggles; follows restarts live.
pub fn refresh_network_status(ui: &Ui) {
    use ricercar_daemon::NetworkStatus;
    let rev = ui.ctx.network_revision();
    ui.extras.borrow_mut().network_rev = Some(rev);
    let net = ui.ctx.config.read().unwrap().network.clone();
    let name = net.name;
    let on = if net.interface.is_empty() {
        String::new()
    } else {
        format!(" · {}", net.interface)
    };
    let text = match ui.ctx.network_status() {
        NetworkStatus::Running { port } => format!(
            "{} “{name}” · {} {port}{on}",
            t("Visible on the network as"),
            t("port")
        ),
        NetworkStatus::Starting => t("Starting…").into(),
        NetworkStatus::Off => t("Network sharing is off").into(),
        NetworkStatus::InterfaceMissing(i) => {
            tf("Interface {name} is not available", &[("name", &i)])
        }
        NetworkStatus::Failed(e) => format!("{}: {e}", t("Network unavailable")),
    };
    ui.app().set_network_status(text.into());
}

/// Called from the player tick: cheap unless the status changed.
pub fn poll_network_status(ui: &Ui) {
    if ui.extras.borrow().network_rev != Some(ui.ctx.network_revision()) {
        refresh_network_status(ui);
    }
}

fn add_root(ui: &Ui, dir: std::path::PathBuf) {
    ui.ctx.update_config(|c| {
        if !c.library.roots.contains(&dir) {
            c.library.roots.push(dir.clone());
        }
    });
    refresh_library_rows(ui);
    ui.toast(t("Folder added; indexing…"), false);
}

/// Whether a file chooser portal answers on the session bus (starting it if
/// needed). Without one, rfd reports "nothing picked" as for a cancel.
pub(crate) fn file_chooser_available() -> bool {
    let Ok(bus) = zbus::blocking::Connection::session() else {
        return false;
    };
    bus.call_method(
        Some("org.freedesktop.portal.Desktop"),
        "/org/freedesktop/portal/desktop",
        Some("org.freedesktop.DBus.Properties"),
        "Get",
        &("org.freedesktop.portal.FileChooser", "version"),
    )
    .is_ok()
}

/// Ask for a music folder with the portal's folder picker, then `done`;
/// without a picker, the dialog where the path is typed.
pub(crate) fn pick_folder(done: fn(&Ui, std::path::PathBuf)) {
    std::thread::spawn(move || {
        let picker = file_chooser_available();
        let asked = std::time::Instant::now();
        let dir = if picker {
            rfd::FileDialog::new().pick_folder()
        } else {
            None
        };
        match dir {
            Some(dir) => post(move |ui| done(ui, dir)),
            // rfd answers "nothing picked" at once when the portal
            // fails; nobody cancels a real dialog that fast.
            None if !picker || asked.elapsed() < std::time::Duration::from_millis(400) => {
                post(|ui| {
                    let app = ui.app();
                    app.set_root_path(SharedString::new());
                    app.set_root_path_open(true);
                })
            }
            None => {}
        }
    });
}

/// A typed folder path: `~` expanded, must be an existing directory.
fn typed_folder(typed: &str) -> Option<std::path::PathBuf> {
    let typed = typed.trim();
    let path = match typed.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => {
            std::path::PathBuf::from(std::env::var_os("HOME")?).join(rest.trim_start_matches('/'))
        }
        _ => std::path::PathBuf::from(typed),
    };
    (path.is_absolute() && path.is_dir()).then_some(path)
}

pub fn refresh_library_rows(ui: &Ui) {
    let roots: Vec<SharedString> = ui
        .ctx
        .config
        .read()
        .unwrap()
        .library
        .roots
        .iter()
        .map(|r| crate::setup::shown_folder(r).into())
        .collect();
    ui.app().set_roots(ModelRc::new(VecModel::from(roots)));
}

fn chips(v: Vec<(String, bool)>) -> ModelRc<crate::CapChip> {
    let rows: Vec<crate::CapChip> = v
        .into_iter()
        .map(|(label, ok)| crate::CapChip {
            label: label.into(),
            ok,
        })
        .collect();
    ModelRc::new(VecModel::from(rows))
}

/// The output our engine holds open right now (never probed).
fn device_in_use(ui: &Ui, name: &str) -> bool {
    ui.ctx.ctl.device_name() == name && ui.ctx.ctl.engine_chain().format.is_some()
}

/// Library sample rates, cached per library revision.
fn rate_histogram(ui: &Ui) -> Vec<ricercar_core::RateCount> {
    let rev = ui.ctx.lib.revision();
    let cached = ui.extras.borrow().histogram.clone();
    match cached {
        Some((r, h)) if r == rev => h,
        _ => {
            let h = ui.ctx.lib.rate_histogram();
            ui.extras.borrow_mut().histogram = Some((rev, h.clone()));
            h
        }
    }
}

pub fn refresh_devices(ui: &Ui) {
    // A card plugged in or out may now sit behind another hw: name.
    crate::outputs::forget_cards();
    let current = ui.ctx.ctl.device_name();
    let cache = ui.saved_state.borrow().dac_caps.clone();
    let dsd = ui.ctx.config.read().unwrap().audio.dsd.clone();
    let hist = rate_histogram(ui);
    let devices = ricercar_audio::device::list_devices();
    // Keep the names of PipeWire outputs for when they are not connected.
    let listed: Vec<(String, String)> = devices
        .iter()
        .filter_map(|d| {
            let node = ricercar_audio::device::pipewire_node(&d.name)?;
            Some((node.to_string(), d.description.clone()))
        })
        .collect();
    crate::outputs::remember(ui, &listed);
    let mut rows: Vec<DeviceRow> = devices
        .into_iter()
        .filter(|d| d.kind != ricercar_audio::DeviceKind::Null)
        .map(|d| {
            let hardware = d.kind == ricercar_audio::DeviceKind::Hardware;
            let caps = cache.get(&d.name).filter(|_| hardware);
            let ex = ui.extras.borrow();
            let status = if ex.probing.contains(&d.name) {
                t("Reading capabilities…").to_string()
            } else if let Some(e) = ex.probe_errors.get(&d.name) {
                format!("{}: {e}", t("Could not read the device"))
            } else if hardware && device_in_use(ui, &d.name) {
                if caps.is_some() {
                    t("In use: capabilities from the last check.").into()
                } else {
                    t("In use: capabilities are read once playback stops.").into()
                }
            } else {
                String::new()
            };
            let lib_note = caps
                .map(|c| {
                    crate::dac::unsupported(&hist, c)
                        .into_iter()
                        .map(|(rate, n)| crate::text::unsupported_albums(n, rate))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            DeviceRow {
                hardware,
                absent: false,
                shared: d.kind == ricercar_audio::DeviceKind::Virtual,
                network: false,
                badge: "".into(),
                detail: "".into(),
                selected: d.name == current,
                desc: d
                    .description
                    .trim_end_matches(" (not bit-perfect)")
                    .to_string()
                    .into(),
                expanded: hardware && ex.expanded.contains(&d.name),
                caps_known: caps.is_some(),
                rates: chips(caps.map(crate::dac::rate_chips).unwrap_or_default()),
                formats: chips(caps.map(crate::dac::container_chips).unwrap_or_default()),
                channels: caps
                    .map(crate::dac::channels_label)
                    .unwrap_or_default()
                    .into(),
                caps_status: status.into(),
                lib_note: lib_note.into(),
                dsd_choice: match dsd.get(&d.name).filter(|_| hardware) {
                    Some(DsdOutput::Dop) => 1,
                    Some(DsdOutput::Native) => 2,
                    _ => 0,
                },
                // Native only when the last probe saw DSD formats.
                dsd_native: caps.is_some_and(|c| !c.dsd.is_empty())
                    || dsd.get(&d.name) == Some(&DsdOutput::Native),
                dsd_badge: match dsd.get(&d.name).filter(|_| hardware) {
                    Some(DsdOutput::Dop) => t("DSD: DoP").into(),
                    Some(DsdOutput::Native) => t("DSD: NATIVE").into(),
                    _ => "".into(),
                },
                name: d.name.into(),
            }
        })
        .collect();
    // UPnP renderers on the network, as the last search found them: they
    // fetch and decode the stream themselves (no capabilities, no DSD).
    let renderers = ui.extras.borrow().renderers.clone();
    let unnamed = t("UPnP renderer");
    rows.extend(renderers.iter().flatten().map(|r| {
        DeviceRow {
            selected: r.device == current,
            network: true,
            badge: "UPNP".into(),
            detail: if r.model.is_empty() {
                unnamed.to_string()
            } else {
                r.model.clone()
            }
            .into(),
            desc: r.name.as_str().into(),
            name: r.device.as_str().into(),
            ..Default::default()
        }
    }));
    // Devices offered by plugins: they fetch the plugin's items themselves.
    rows.extend(ui.ctx.plugins.outputs().into_iter().map(|o| {
        DeviceRow {
            selected: o.device == current,
            network: true,
            // Badges are capitals; empty keeps the generic NETWORK one.
            badge: o.badge.to_uppercase().into(),
            detail: if o.detail.is_empty() {
                o.plugin_name
            } else {
                o.detail
            }
            .into(),
            desc: o.name.into(),
            name: o.device.into(),
            ..Default::default()
        }
    }));
    if !rows.iter().any(|r| r.selected) {
        let null = current == "null";
        let network =
            ricercar_audio::device::classify(&current) == ricercar_audio::DeviceKind::Network;
        // A plugin's device it does not list now: not connected.
        let plugin = ricercar_core::plugin::parse_output_device(&current)
            .map(|(id, _)| crate::plugins::name_of(ui, id));
        // A PipeWire output that is not connected now: its remembered name.
        let node = ricercar_audio::device::pipewire_node(&current);
        let absent = node.map(|n| {
            let saved = ui.saved_state.borrow();
            crate::outputs::absent_name(n, saved.device_name(n))
        });
        rows.push(if network {
            // A renderer the last search did not find (none ran yet: not
            // claimed absent).
            DeviceRow {
                desc: crate::outputs::describe(ui, &current).label.into(),
                absent: plugin.is_some() || renderers.is_some(),
                badge: if plugin.is_none() {
                    "UPNP".into()
                } else {
                    "".into()
                },
                detail: plugin.unwrap_or_else(|| unnamed.to_string()).into(),
                network: true,
                name: current.into(),
                selected: true,
                ..Default::default()
            }
        } else {
            DeviceRow {
                desc: if null {
                    t("Null sink").into()
                } else {
                    absent.clone().unwrap_or_else(|| current.clone())
                }
                .into(),
                absent: absent.is_some(),
                name: if null {
                    t("Discards audio (testing)").into()
                } else {
                    current
                }
                .into(),
                selected: true,
                ..Default::default()
            }
        });
    }
    crate::setup::refresh(ui, &rows);
    ui.app().set_devices(ModelRc::new(VecModel::from(rows)));
}

/// The device's capabilities are being read right now.
pub fn probing(ui: &Ui, name: &str) -> bool {
    ui.extras.borrow().probing.contains(name)
}

/// Read what every hw: device accepts, off the UI thread. The device we
/// play on is skipped (its cached capabilities stay), and the headless tours
/// never open real hardware.
pub fn probe_devices(ui: &Rc<Ui>) {
    if std::env::var_os("RICERCAR_SNAPSHOT").is_some() {
        return;
    }
    let names: Vec<String> = ricercar_audio::device::list_devices()
        .into_iter()
        .filter(|d| d.kind == ricercar_audio::DeviceKind::Hardware)
        .map(|d| d.name)
        .filter(|n| !device_in_use(ui, n) && !ui.extras.borrow().probing.contains(n))
        .collect();
    if names.is_empty() {
        return;
    }
    {
        let mut ex = ui.extras.borrow_mut();
        for n in &names {
            ex.probing.insert(n.clone());
            ex.probe_errors.remove(n);
        }
    }
    refresh_devices(ui);
    std::thread::Builder::new()
        .name("ricercar-dac-probe".into())
        .spawn(move || {
            for name in names {
                let r = ricercar_audio::probe_device(&name);
                post(move |ui| {
                    ui.extras.borrow_mut().probing.remove(&name);
                    match r {
                        Ok(caps) => {
                            let c = crate::dac::CachedCaps::from_caps(
                                &caps,
                                ricercar_core::library::now_unix(),
                            );
                            store_caps(ui, &name, c);
                        }
                        Err(e) => {
                            tracing::info!("probe {name}: {e}");
                            ui.extras
                                .borrow_mut()
                                .probe_errors
                                .insert(name, short_alsa(&e));
                        }
                    }
                    refresh_devices(ui);
                });
            }
        })
        .expect("spawn probe");
}

/// How long a search for UPnP renderers listens for answers.
const RENDERER_SEARCH: std::time::Duration = std::time::Duration::from_secs(2);

/// Look for UPnP renderers on the network, off the UI thread (the search
/// blocks for a couple of seconds); the device list is rebuilt from the
/// answer. One search at a time, and the headless tours never search.
pub fn find_renderers(ui: &Ui) {
    if std::env::var_os("RICERCAR_SNAPSHOT").is_some() {
        return;
    }
    {
        let mut ex = ui.extras.borrow_mut();
        if ex.searching {
            return;
        }
        ex.searching = true;
    }
    let spawned = std::thread::Builder::new()
        .name("ricercar-renderers".into())
        .spawn(|| {
            let found = ricercar_daemon::network_outputs::discover(RENDERER_SEARCH);
            post(move |ui| {
                {
                    let mut ex = ui.extras.borrow_mut();
                    ex.searching = false;
                    ex.renderers = Some(found);
                }
                refresh_devices(ui);
            });
        });
    if spawned.is_err() {
        ui.extras.borrow_mut().searching = false;
    }
}

/// "alsa error on device 'hw:1,0': ALSA function 'snd_pcm_open' failed with
/// error 'EBUSY: Device or resource busy'" → "Device or resource busy".
fn short_alsa(e: &ricercar_audio::AudioError) -> String {
    let s = e.to_string();
    s.rsplit(": ")
        .next()
        .unwrap_or(&s)
        .trim_end_matches('\'')
        .to_string()
}

/// Remember a device's capabilities (ui-state.json).
pub fn store_caps(ui: &Ui, name: &str, caps: crate::dac::CachedCaps) {
    ui.saved_state
        .borrow_mut()
        .dac_caps
        .insert(name.to_string(), caps);
    crate::plugins::push_output(ui);
    let st = ui.saved_state.borrow().clone();
    if std::env::var_os("RICERCAR_SNAPSHOT").is_none() {
        crate::app::queue_state_save(ui, &st);
    }
}

/// Capabilities of the active output, when it is a probed hw: device.
pub fn active_caps(ui: &Ui) -> Option<crate::dac::CachedCaps> {
    let name = ui.ctx.ctl.device_name();
    (ricercar_audio::device::classify(&name) == ricercar_audio::DeviceKind::Hardware)
        .then(|| ui.saved_state.borrow().dac_caps.get(&name).cloned())
        .flatten()
}

/// Apply look & feel from the config to the Theme global.
pub fn apply_theme(ui: &Ui) {
    let cfg = ui.ctx.config.read().unwrap().ui.clone();
    let theme = ui.window.global::<crate::Theme>();
    theme.set_dark(cfg.theme == ThemeCfg::Dark);
    theme.set_adaptive(cfg.adaptive_colors);
    let dark = cfg.theme == ThemeCfg::Dark;
    if let Some(c) = parse_hex(&cfg.accent) {
        theme.set_base_accent(readable_accent([c.red(), c.green(), c.blue()], dark));
    }
    apply_cover_accent(ui);
    ui.app().set_theme_dark(cfg.theme == ThemeCfg::Dark);
}

/// Switch to the language of the config, live: the .slint side switches at
/// once; strings built on the Rust side come back with the page and the
/// settings (which also refill the setup assistant).
pub fn apply_language(ui: &Rc<Ui>) {
    let lang = ui.ctx.config.read().unwrap().ui.language.clone();
    let before = crate::text::language();
    if crate::text::set_language(&lang) != before {
        ui.reload_page();
        load_settings(ui);
    }
}

fn settings_changed(ui: &Rc<Ui>) {
    let app = ui.app();
    let rg = match app.get_replaygain() {
        1 => ReplayGain::Track,
        2 => ReplayGain::Album,
        3 => ReplayGain::Auto,
        _ => ReplayGain::Off,
    };
    let preamp = (app.get_preamp() * 10.0).round() / 10.0;
    let accent = ACCENTS[(app.get_accent_choice().max(0) as usize).min(ACCENTS.len() - 1)];
    ui.ctx.update_config(|c| {
        c.audio.replaygain = rg;
        c.audio.preamp_db = preamp;
        c.audio.restore_session = app.get_restore_session();
        c.audio.continuous_playback = app.get_continuous_playback();
        c.audio.fixed_volume = app.get_fixed_volume();
        c.library.watch = app.get_watch_library();
        let name = app.get_renderer_name().trim().to_string();
        if !name.is_empty() {
            c.network.name = name;
        }
        c.network.renderer = app.get_renderer_enabled();
        c.network.media_server = app.get_server_enabled();
        c.online.lyrics = app.get_lyrics_online();
        c.online.cover_art = app.get_covers_online();
        c.online.plugin_catalog = app.get_plugins_online();
        c.online.updates = app.get_updates_online();
        c.ui.notifications = app.get_notifications();
        c.ui.close_to_tray = app.get_close_to_tray();
        c.ui.theme = if app.get_theme_dark() {
            ThemeCfg::Dark
        } else {
            ThemeCfg::Light
        };
        c.ui.adaptive_colors = app.get_adaptive_colors();
        c.ui.estimate_word_timing = app.get_estimate_word_timing();
        c.ui.accent = accent.into();
        c.ui.language = crate::text::language_pref(app.get_ui_language()).into();
    });
    apply_language(ui);
    ui.ctx.ctl.set_replaygain(rg, preamp);
    ui.loader.borrow().set_online(app.get_covers_online());
    apply_theme(ui);
    crate::player::word_estimate_changed(ui);
}

// ------------------------------------------------------------------ scrobbling

fn check_listenbrainz(ui: &Rc<Ui>, token: String) {
    ui.app().set_lb_status(t("Checking…").into());
    std::thread::spawn(move || {
        let r = ricercar_online::listenbrainz::ListenBrainz::new(token).validate_token();
        post(move |ui| {
            let s = match r {
                Ok(Some(user)) => format!("{} {user}", t("Connected as")),
                Ok(None) => t("Invalid token").into(),
                Err(e) => format!("{}: {e}", t("Network error")),
            };
            ui.app().set_lb_status(s.into());
        });
    });
}

fn lastfm_connect(ui: &Rc<Ui>) {
    let app = ui.app();
    let key = app.get_lastfm_key().trim().to_string();
    let secret = app.get_lastfm_secret().trim().to_string();
    if key.is_empty() || secret.is_empty() {
        ui.toast(
            t("Enter the API key and shared secret of your Last.fm API account first."),
            true,
        );
        return;
    }
    ui.ctx.update_config(|c| {
        c.scrobble.lastfm_api_key = key.clone();
        c.scrobble.lastfm_secret = secret.clone();
    });
    std::thread::spawn(move || {
        let lf = ricercar_online::lastfm::LastFm::new(key, secret);
        let r = lf.get_token();
        post(move |ui| match r {
            Ok(token) => {
                let url = lf.auth_url(&token);
                crate::sys::xdg_open(&url);
                ui.extras.borrow_mut().lastfm_token = Some(token);
                ui.app().set_lastfm_pending(true);
                ui.toast(
                    t("Approve ricercar in your browser, then click “I approved it”."),
                    false,
                );
            }
            Err(e) => ui.toast(format!("Last.fm: {e}"), true),
        });
    });
}

fn lastfm_finish(ui: &Rc<Ui>) {
    let Some(token) = ui.extras.borrow_mut().lastfm_token.take() else {
        return;
    };
    let (key, secret) = {
        let c = ui.ctx.config.read().unwrap();
        (
            c.scrobble.lastfm_api_key.clone(),
            c.scrobble.lastfm_secret.clone(),
        )
    };
    std::thread::spawn(move || {
        let r = ricercar_online::lastfm::LastFm::new(key, secret).get_session(&token);
        post(move |ui| {
            ui.app().set_lastfm_pending(false);
            match r {
                Ok(s) => {
                    ui.ctx.update_config(|c| {
                        c.scrobble.lastfm_session = s.key.clone();
                        c.scrobble.lastfm_user = s.name.clone();
                    });
                    ui.app()
                        .set_lastfm_status(format!("{} {}", t("Connected as"), s.name).into());
                }
                Err(e) => ui.toast(format!("{}: {e}", t("Authorization failed")), true),
            }
        });
    });
}

// ------------------------------------------------------------------ wiring

/// Play on `name` from now on (settings list, setup assistant).
pub fn select_device(ui: &Ui, name: &str) {
    let desc = ui
        .app()
        .get_devices()
        .iter()
        .find(|d| d.name == name)
        .map(|d| d.desc.to_string());
    if let Some(desc) = &desc {
        crate::outputs::remember_picked(ui, name, desc);
    }
    ui.ctx.update_config(|c| c.audio.device = name.to_string());
    crate::plugins::push_output(ui);
    refresh_devices(ui);
    let shown = match (&desc, ricercar_audio::device::pipewire_node(name)) {
        (Some(desc), Some(_)) => crate::outputs::readable(desc),
        // A UPnP renderer: its friendly name, not `upnp:<uuid>`.
        (Some(desc), None)
            if ricercar_audio::device::classify(name) == ricercar_audio::DeviceKind::Network =>
        {
            desc.as_str()
        }
        _ => name,
    };
    ui.toast(format!("{}: {shown}", t("Output device")), false);
}

pub fn wire(ui: &Rc<Ui>) {
    apply_theme(ui);
    {
        let c = ui.ctx.config.read().unwrap().audio.clone();
        ui.ctx.ctl.set_replaygain(c.replaygain, c.preamp_db);
    }
    let app = ui.app();
    app.on_settings_changed(|| with_ui(settings_changed));
    app.on_select_device(|name| with_ui(|ui| select_device(ui, &name)));
    app.on_select_net_iface(|i| {
        with_ui(|ui| {
            let name = ui
                .extras
                .borrow()
                .net_ifaces
                .get(i.max(0) as usize)
                .cloned();
            if let Some(name) = name {
                ui.ctx.update_config(|c| c.network.interface = name.clone());
            }
            refresh_interfaces(ui);
            refresh_network_status(ui);
        })
    });
    app.on_refresh_devices(|| {
        with_ui(|ui| {
            refresh_devices(ui);
            probe_devices(ui);
            find_renderers(ui);
        })
    });
    app.on_copy_report(|| with_ui(|ui| crate::diag::copy_report(ui)));
    app.on_open_logs(crate::diag::open_logs);
    app.on_set_device_dsd(|name, choice| {
        with_ui(|ui| {
            let name = name.to_string();
            // DoP and native are for hw: devices only.
            let hardware =
                ricercar_audio::device::classify(&name) == ricercar_audio::DeviceKind::Hardware;
            let out = match choice {
                1 if hardware => DsdOutput::Dop,
                2 if hardware => DsdOutput::Native,
                _ => DsdOutput::Pcm,
            };
            ui.ctx.update_config(|c| {
                if out == DsdOutput::Pcm {
                    c.audio.dsd.remove(&name);
                } else {
                    c.audio.dsd.insert(name.clone(), out);
                }
            });
            refresh_devices(ui);
        })
    });
    app.on_toggle_device_caps(|name| {
        with_ui(|ui| {
            let name = name.to_string();
            {
                let mut ex = ui.extras.borrow_mut();
                if !ex.expanded.remove(&name) {
                    ex.expanded.insert(name);
                }
            }
            refresh_devices(ui);
        })
    });
    app.on_add_root(|| pick_folder(add_root));
    app.on_add_root_path(|typed| {
        with_ui(|ui| match typed_folder(typed.as_str()) {
            Some(dir) => {
                ui.app().set_root_path_open(false);
                // The setup assistant asked for it.
                if ui.app().get_setup_open() {
                    crate::setup::add_folder(ui, dir);
                } else {
                    add_root(ui, dir);
                }
            }
            None => ui.toast(t("This folder does not exist."), true),
        })
    });
    app.on_remove_root(|i| {
        with_ui(|ui| {
            ui.ctx.update_config(|c| {
                let i = i.max(0) as usize;
                if i < c.library.roots.len() {
                    c.library.roots.remove(i);
                }
            });
            refresh_library_rows(ui);
            ui.toast(t("Folder removed"), false);
        })
    });
    app.on_rescan(|| {
        with_ui(|ui| {
            ui.loader.borrow_mut().forget_missing();
            ui.ctx.rescan();
        })
    });
    app.on_lb_save(|| {
        with_ui(|ui| {
            let token = ui.app().get_lb_token().trim().to_string();
            ui.ctx
                .update_config(|c| c.scrobble.listenbrainz_token = token.clone());
            if token.is_empty() {
                ui.app().set_lb_status(t("Not connected").into());
            } else {
                check_listenbrainz(ui, token);
            }
        })
    });
    app.on_lastfm_connect(|| with_ui(lastfm_connect));
    app.on_lastfm_finish(|| with_ui(lastfm_finish));
    app.on_lastfm_disconnect(|| {
        with_ui(|ui| {
            ui.ctx.update_config(|c| {
                c.scrobble.lastfm_session.clear();
                c.scrobble.lastfm_user.clear();
            });
            ui.app().set_lastfm_status("".into());
        })
    });

    app.on_radio_search(|q| {
        with_ui(|ui| {
            ui.extras.borrow_mut().debounce.remove("radio");
            radio_text_search(ui, &q);
        })
    });
    app.on_radio_edited(|q| {
        with_ui(|ui| {
            let q = q.to_string();
            // Radio Browser is a shared community service: wait for a pause
            // and at least two letters before querying.
            if q.trim().chars().count() == 1 {
                return;
            }
            debounce(ui, "radio", 400, move |ui| radio_text_search(ui, &q));
        })
    });
    app.on_text_setting_edited(|| with_ui(|ui| debounce(ui, "settings", 700, settings_changed)));
    app.on_lb_edited(|| {
        with_ui(|ui| {
            debounce(ui, "lb", 900, |ui| {
                let token = ui.app().get_lb_token().trim().to_string();
                // ListenBrainz tokens are UUIDs: validate once one is complete.
                if token.is_empty() || token.len() >= 32 {
                    ui.app().invoke_lb_save();
                }
            })
        })
    });
    app.on_radio_tag(|tag| {
        with_ui(|ui| {
            let query = StationQuery {
                tag: Some(tag.to_string()),
                limit: 80,
                ..Default::default()
            };
            radio_query(ui, query, format!("{} · {tag}", t("Stations")));
        })
    });
    app.on_radio_play(|list, i| {
        with_ui(|ui| {
            let s = {
                let ex = ui.extras.borrow();
                let v = if list == "favs" {
                    &ex.favs
                } else {
                    &ex.results
                };
                v.get(i.max(0) as usize).cloned()
            };
            if let Some(s) = s {
                play_station(ui, &s);
            }
        })
    });
    app.on_radio_fav(|list, i| {
        with_ui(|ui| {
            {
                let mut ex = ui.extras.borrow_mut();
                let s = if list == "favs" {
                    ex.favs.get(i.max(0) as usize).cloned()
                } else {
                    ex.results.get(i.max(0) as usize).cloned()
                };
                if let Some(s) = s {
                    if let Some(pos) = ex.favs.iter().position(|f| f.uuid == s.uuid) {
                        ex.favs.remove(pos);
                    } else {
                        ex.favs.push(s);
                    }
                }
            }
            save_favs(ui);
            show_stations(ui);
        })
    });
}

#[cfg(test)]
mod tests {
    use super::{interface_choices, typed_folder};

    #[test]
    fn interface_choice_list() {
        let up = [
            ("eth0".to_string(), "192.168.1.20".parse().unwrap()),
            ("wg0".to_string(), "10.8.0.2".parse().unwrap()),
        ];
        let (names, labels, sel) = interface_choices("", &up);
        assert_eq!(names, ["", "eth0", "wg0"]);
        assert_eq!(labels[1], "eth0 · 192.168.1.20");
        assert_eq!(sel, 0);
        let (_, _, sel) = interface_choices("wg0", &up);
        assert_eq!(sel, 2);
        // Configured but gone: still listed, selected.
        let (names, labels, sel) = interface_choices("eth1", &up);
        assert_eq!(names.last().unwrap(), "eth1");
        assert_eq!(labels.last().unwrap(), "eth1 (not available)");
        assert_eq!(sel, 3);
    }

    #[test]
    fn typed_folder_paths() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().to_str().unwrap();
        assert_eq!(
            typed_folder(&format!("  {p} ")).as_deref(),
            Some(dir.path())
        );
        assert_eq!(typed_folder(&format!("{p}/missing")), None);
        assert_eq!(typed_folder("relative/path"), None);
        assert_eq!(typed_folder(""), None);
    }
}
