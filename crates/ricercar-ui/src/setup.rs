//! Setup assistant: the interface language, music folders, plugins from
//! the catalogue (and their sign-in), an output, what its DAC accepts,
//! fixed volume. Opens by itself after a first launch (no config file
//! yet), at every start until finished or skipped (`[ui] setup_done`);
//! Settings opens it again, at the language, with the current values.
//!
//! The language applies (and is saved) at once, like in the settings.
//! Plugins are installed when leaving their step, once the user saw what
//! is downloaded and from where. Folders, output and volume apply at Save.
//! On a fresh install no folder is scanned before that: Save scans the
//! folders chosen, Later the music folder proposed.
//!
//! The output rows come from the settings' device list
//! (`extras::refresh_devices`, which calls `refresh` here), so capabilities
//! read in the background show up as they arrive.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use ricercar_core::config::{Config, PluginConfig};
use ricercar_core::plugin::{PluginStatus, RunState, catalog};
use slint::{Model, ModelRc, SharedString, VecModel};

use crate::app::{Ui, post, with_ui};
use crate::text::t;
use crate::{DeviceRow, SetupLine, SetupPlugin};

const LANGUAGE: i32 = 0;
const PLUGINS: i32 = 2;
const OUTPUT: i32 = 3;
const SUMMARY: i32 = 6;

/// Plugins step: choosing, confirming the downloads, installing.
const CHOOSE: i32 = 0;
const CONFIRM: i32 = 1;
const INSTALL: i32 = 2;

/// What the assistant holds while it is open.
#[derive(Default)]
struct State {
    /// The output picked.
    choice: Option<String>,
    folders: Vec<PathBuf>,
    /// Catalogue entries as read, and the read in flight.
    catalog: Vec<catalog::Entry>,
    serial: u64,
    loading: bool,
    /// Plugins ticked to install.
    checked: BTreeSet<String>,
    /// Plugins installed (or tried) from the assistant, by id.
    outcomes: HashMap<String, Outcome>,
}

#[derive(Debug, Clone, PartialEq)]
enum Outcome {
    Installing,
    Installed,
    Failed(String),
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

/// Open the assistant at startup? When a first launch started it and it
/// was neither finished nor skipped since (`UiConfig::setup_done`).
/// Upgrades never see it by themselves.
pub fn should_show(cfg: &Config) -> bool {
    cfg.ui.setup_pending()
}

/// Folders proposed by the library step: the configured ones, else the
/// music folder of the desktop when it exists.
pub fn initial_folders(configured: &[PathBuf], music: Option<&Path>) -> Vec<PathBuf> {
    if !configured.is_empty() {
        return configured.to_vec();
    }
    music
        .filter(|m| m.is_dir())
        .map(|m| vec![m.to_path_buf()])
        .unwrap_or_default()
}

/// A folder as shown: from `~` when under the home folder.
pub fn folder_label(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// `folder_label` with the home folder of this run.
pub fn shown_folder(path: &Path) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    folder_label(path, home.as_deref().filter(|h| *h != Path::new("/")))
}

/// A catalogue entry as the plugins step offers it.
#[derive(Debug, Clone, PartialEq)]
pub struct Offer {
    pub id: String,
    pub name: String,
    pub description: String,
    pub installed: bool,
    pub checked: bool,
}

/// The entries offered: those installed already (shown ticked), and
/// those that can be installed on this computer, in catalogue order.
pub fn offers(
    entries: &[catalog::Entry],
    installed: &[PluginConfig],
    checked: &BTreeSet<String>,
) -> Vec<Offer> {
    entries
        .iter()
        .filter_map(|e| {
            let installed = installed.iter().any(|p| p.id == e.id);
            (installed || e.installable()).then(|| Offer {
                id: e.id.clone(),
                name: e.name.chars().take(80).collect(),
                description: e.description.chars().take(400).collect(),
                installed,
                checked: installed || checked.contains(&e.id),
            })
        })
        .collect()
}

/// An installed plugin in the summary: its name, and whether it is
/// signed in when it needs a sign-in.
pub struct PluginLine {
    pub name: String,
    pub signed_in: Option<bool>,
}

/// What the last step lists.
pub struct Summary {
    pub language: String,
    pub folders: Vec<PathBuf>,
    pub plugins: Vec<PluginLine>,
    pub output: String,
    pub fixed: bool,
}

/// The summary as label and value lines.
pub fn summary_lines(s: &Summary) -> Vec<(String, String)> {
    let or_none = |v: Vec<String>| {
        if v.is_empty() {
            t("None").to_string()
        } else {
            v.join("\n")
        }
    };
    let folders = s.folders.iter().map(|f| shown_folder(f)).collect();
    let plugins = s
        .plugins
        .iter()
        .map(|p| match p.signed_in {
            Some(true) => format!("{} · {}", p.name, t("Signed in")),
            Some(false) => format!("{} · {}", p.name, t("Not signed in")),
            None => p.name.clone(),
        })
        .collect();
    vec![
        (t("Language").into(), s.language.clone()),
        (t("Music folders").into(), or_none(folders)),
        (t("Plugins").into(), or_none(plugins)),
        (t("Output").into(), s.output.clone()),
        (
            t("Fixed volume").into(),
            if s.fixed { t("On") } else { t("Off") }.into(),
        ),
    ]
}

/// Listed outputs, a PipeWire one chosen earlier, UPnP renderers and
/// plugins' devices; not the placeholder rows of the null and file sinks.
fn offerable(r: &DeviceRow) -> bool {
    r.hardware || r.shared || r.absent || r.network
}

/// Outputs offered: hardware first, then shared ones, then network
/// outputs (renderers, plugins' devices), in list order.
pub fn offered(rows: &[DeviceRow]) -> Vec<DeviceRow> {
    let mut out: Vec<DeviceRow> = rows.iter().filter(|r| offerable(r)).cloned().collect();
    out.sort_by_key(|r| (!r.hardware, r.network));
    for r in &mut out {
        r.selected = false;
        r.expanded = false;
        if r.hardware {
            r.desc = crate::outputs::card_label(&r.desc).into();
        }
    }
    out
}

/// The output selected when the assistant opens: the current one when
/// offered and bit-perfect, else the first USB card (most likely the DAC),
/// else the current one, else the first offered.
pub fn initial_choice(rows: &[DeviceRow], current: &str) -> String {
    let mut rows = rows.iter().filter(|r| offerable(r));
    let current_row = rows.clone().find(|r| r.name == current);
    if let Some(r) = current_row.filter(|r| r.hardware) {
        return r.name.to_string();
    }
    let usb = rows
        .clone()
        .find(|r| r.hardware && r.desc.ends_with(": USB Audio"));
    usb.or(current_row)
        .or(rows.next())
        .map(|r| r.name.to_string())
        .unwrap_or_else(|| current.to_string())
}

pub fn open(ui: &Rc<Ui>) {
    let (fixed, lang, roots) = {
        let cfg = ui.ctx.config.read().unwrap();
        (
            cfg.audio.fixed_volume,
            cfg.ui.language.clone(),
            cfg.library.roots.clone(),
        )
    };
    let music = ricercar_core::config::default_music_dir();
    STATE.with(|s| {
        *s.borrow_mut() = State {
            folders: initial_folders(&roots, music.as_deref()),
            ..State::default()
        }
    });
    let app = ui.app();
    app.set_ui_language(crate::text::language_index(&lang));
    app.set_setup_fixed(fixed);
    app.set_setup_plugins_phase(CHOOSE);
    app.set_setup_plugins_busy(false);
    app.set_setup_plugins_loading(false);
    app.set_setup_plugins_status(SharedString::new());
    app.set_setup_open(true);
    show_folders(ui);
    refresh_plugins(ui);
    go(ui, LANGUAGE);
    // Lists the outputs (and calls `refresh`), then reads the cards and
    // looks for renderers on the network.
    crate::extras::refresh_devices(ui);
    crate::extras::probe_devices(ui);
    crate::extras::find_renderers(ui);
}

/// Show step `step`, reading what it needs.
pub fn go(ui: &Rc<Ui>, step: i32) {
    ui.app().set_setup_step(step);
    if step == PLUGINS {
        let read = STATE.with(|s| {
            let s = s.borrow();
            s.catalog.is_empty() && !s.loading
        });
        if read {
            load_catalog(ui);
        }
    }
    if step == SUMMARY {
        fill_summary(ui);
    }
}

fn next(ui: &Rc<Ui>) {
    let step = ui.app().get_setup_step();
    match step {
        PLUGINS => plugins_next(ui),
        SUMMARY => {}
        _ => go(ui, step + 1),
    }
}

fn back(ui: &Rc<Ui>) {
    let app = ui.app();
    let step = app.get_setup_step();
    if step == PLUGINS && app.get_setup_plugins_phase() != CHOOSE {
        if !app.get_setup_plugins_busy() {
            app.set_setup_plugins_phase(CHOOSE);
            refresh_plugins(ui);
        }
    } else if step > 0 {
        go(ui, step - 1);
    }
}

// ------------------------------------------------------------ library

fn show_folders(ui: &Ui) {
    let rows: Vec<SharedString> = STATE.with(|s| {
        s.borrow()
            .folders
            .iter()
            .map(|f| shown_folder(f).into())
            .collect()
    });
    ui.app()
        .set_setup_folders(ModelRc::new(VecModel::from(rows)));
}

/// A folder picked (or typed) for the library step.
pub fn add_folder(ui: &Ui, dir: PathBuf) {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        if !s.folders.contains(&dir) {
            s.folders.push(dir);
        }
    });
    show_folders(ui);
}

fn remove_folder(ui: &Ui, i: i32) {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        let i = i.max(0) as usize;
        if i < s.folders.len() {
            s.folders.remove(i);
        }
    });
    show_folders(ui);
}

// ------------------------------------------------------------ plugins

/// Read the catalogue the Plugins page reads (when allowed).
fn load_catalog(ui: &Rc<Ui>) {
    let app = ui.app();
    if !ui.ctx.config.read().unwrap().online.plugin_catalog {
        app.set_setup_plugins_status(
            t("The community catalogue is off (Settings → Online extras).").into(),
        );
        return;
    }
    let serial = STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.serial += 1;
        s.loading = true;
        s.serial
    });
    app.set_setup_plugins_loading(true);
    app.set_setup_plugins_status(SharedString::new());
    let (url, local) = crate::plugins::index_source(ui);
    let others = crate::plugins::other_indexes(ui);
    std::thread::spawn(move || {
        let r = catalog::fetch_all(&url, local, &others).map(|(list, _)| list);
        post(move |ui| catalog_read(ui, serial, r));
    });
}

fn catalog_read(ui: &Ui, serial: u64, r: Result<Vec<catalog::Entry>, catalog::CatalogError>) {
    let current = STATE.with(|s| {
        let mut s = s.borrow_mut();
        if s.serial != serial {
            return false;
        }
        s.loading = false;
        if let Ok(list) = &r {
            s.catalog = list.clone();
        }
        true
    });
    if !current {
        return;
    }
    let app = ui.app();
    app.set_setup_plugins_loading(false);
    match r {
        Ok(_) => {
            refresh_plugins(ui);
            if app.get_setup_plugins().row_count() == 0 {
                app.set_setup_plugins_status(t("The catalogue is empty for now.").into());
            }
        }
        Err(e) => {
            tracing::info!("setup: plugin catalogue: {e}");
            app.set_setup_plugins_status(
                t("The plugin catalogue could not be read (offline?). You can add plugins later from the Plugins page.").into(),
            );
        }
    }
}

/// Status of an offered plugin: text, whether it is an error, whether a
/// sign-in can start.
fn plugin_state(
    outcome: Option<&Outcome>,
    installed: bool,
    status: Option<&PluginStatus>,
) -> (String, bool, bool) {
    match (outcome, status) {
        (Some(Outcome::Installing), _) => (t("Installing…").into(), false, false),
        (Some(Outcome::Failed(why)), _) => (
            format!("{}: {why}", t("Could not install the plugin")),
            true,
            false,
        ),
        _ if !installed => (String::new(), false, false),
        (_, Some(s)) if s.state != RunState::Running => {
            let (text, state) = crate::plugins::status_text(s);
            (text, state == 2, false)
        }
        (_, Some(s)) if s.caps.auth && s.signed_in() => (t("Signed in").into(), false, false),
        (_, Some(s)) if s.caps.auth => (t("Not signed in").into(), false, true),
        _ => (t("Installed").into(), false, false),
    }
}

/// Fill the plugins step: every offer, or while installing and after,
/// the plugins installed from here.
pub fn refresh_plugins(ui: &Ui) {
    let app = ui.app();
    if !app.get_setup_open() {
        return;
    }
    let installed = ui.ctx.config.read().unwrap().plugins.clone();
    let statuses = crate::plugins::host(ui).statuses();
    let only_installs = app.get_setup_plugins_phase() == INSTALL;
    let rows: Vec<SetupPlugin> = STATE.with(|s| {
        let s = s.borrow();
        offers(&s.catalog, &installed, &s.checked)
            .into_iter()
            .filter(|o| !only_installs || s.outcomes.contains_key(&o.id))
            .map(|o| {
                let outcome = s.outcomes.get(&o.id);
                let status = statuses.iter().find(|st| st.id == o.id);
                let (status, failed, can_sign_in) = plugin_state(outcome, o.installed, status);
                SetupPlugin {
                    id: o.id.into(),
                    name: o.name.into(),
                    description: o.description.into(),
                    installed: o.installed,
                    checked: o.checked,
                    status: status.into(),
                    failed,
                    can_sign_in,
                }
            })
            .collect()
    });
    app.set_setup_plugins(ModelRc::new(VecModel::from(rows)));
}

/// Entries ticked and not installed yet.
fn to_install(ui: &Ui) -> Vec<catalog::Entry> {
    let installed = ui.ctx.config.read().unwrap().plugins.clone();
    STATE.with(|s| {
        let s = s.borrow();
        s.catalog
            .iter()
            .filter(|e| s.checked.contains(&e.id) && !installed.iter().any(|p| p.id == e.id))
            .cloned()
            .collect()
    })
}

/// Next on the plugins step: on to the output when nothing is to be
/// installed, else first what will be downloaded and from where (the
/// install dialog's text), then the installs.
fn plugins_next(ui: &Rc<Ui>) {
    let app = ui.app();
    match app.get_setup_plugins_phase() {
        CHOOSE => {
            let list = to_install(ui);
            if list.is_empty() {
                go(ui, OUTPUT);
                return;
            }
            let bodies: Vec<String> = list.iter().filter_map(crate::plugins::entry_body).collect();
            app.set_setup_plugins_confirm(bodies.join("\n\n").into());
            app.set_setup_plugins_phase(CONFIRM);
        }
        CONFIRM => install(ui),
        _ if !app.get_setup_plugins_busy() => go(ui, OUTPUT),
        _ => {}
    }
}

/// Install the ticked plugins one after the other, as the Plugins page
/// does (checked against the catalogue's SHA-256, then declared).
fn install(ui: &Rc<Ui>) {
    let list = to_install(ui);
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        for e in &list {
            s.outcomes.insert(e.id.clone(), Outcome::Installing);
        }
    });
    let app = ui.app();
    app.set_setup_plugins_phase(INSTALL);
    app.set_setup_plugins_busy(true);
    refresh_plugins(ui);
    let (_, local) = crate::plugins::index_source(ui);
    let root = catalog::bin_root(&ricercar_core::config::data_dir());
    std::thread::spawn(move || {
        for e in list {
            let r = catalog::install(&e, &root, local);
            post(move |ui| installed(ui, &e.id, r));
        }
        post(|ui| {
            ui.app().set_setup_plugins_busy(false);
            refresh_plugins(ui);
        });
    });
}

fn installed(ui: &Ui, id: &str, r: Result<PluginConfig, catalog::CatalogError>) {
    let outcome = match r {
        Ok(cfg) => {
            crate::plugins::declare(ui, cfg);
            Outcome::Installed
        }
        Err(e) => Outcome::Failed(e.to_string()),
    };
    STATE.with(|s| s.borrow_mut().outcomes.insert(id.to_string(), outcome));
    refresh_plugins(ui);
}

fn check_plugin(ui: &Ui, id: &str, on: bool) {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        if on {
            s.checked.insert(id.to_string());
        } else {
            s.checked.remove(id);
        }
    });
    refresh_plugins(ui);
}

// ------------------------------------------------------------ output

/// Fill the assistant from the device rows of the settings.
pub fn refresh(ui: &Ui, rows: &[DeviceRow]) {
    let app = ui.app();
    if !app.get_setup_open() {
        return;
    }
    let choice = STATE.with(|s| {
        s.borrow_mut()
            .choice
            .get_or_insert_with(|| initial_choice(rows, &ui.ctx.ctl.device_name()))
            .clone()
    });
    let mut offered = offered(rows);
    for r in &mut offered {
        r.selected = r.name == choice.as_str();
    }
    let dev = offered
        .iter()
        .find(|r| r.selected)
        .cloned()
        .unwrap_or_default();
    let (verdict, note) = if dev.hardware {
        let caps = ui.saved_state.borrow().dac_caps.get(&choice).cloned();
        match caps {
            Some(c) => (crate::text::dac_verdict(crate::dac::best(&c)), ""),
            None if crate::extras::probing(ui, &choice) => (String::new(), ""),
            None => (
                String::new(),
                t("Its capabilities could not be read (it may be busy). You can continue anyway."),
            ),
        }
    } else if dev.network {
        (
            String::new(),
            t(
                "A network renderer fetches and decodes the stream itself: ricercar cannot check what reaches its DAC, so it is never shown as bit-perfect.",
            ),
        )
    } else {
        (
            String::new(),
            t(
                "A shared output goes through the system mixer: the sound server may resample. Capabilities are read on hardware outputs only.",
            ),
        )
    };
    app.set_setup_outputs(ModelRc::new(VecModel::from(offered)));
    app.set_setup_dev(dev);
    app.set_setup_verdict(verdict.into());
    app.set_setup_note(note.into());
}

// ------------------------------------------------------------ summary

fn fill_summary(ui: &Ui) {
    let app = ui.app();
    let language = app
        .get_language_names()
        .row_data(app.get_ui_language().max(0) as usize)
        .unwrap_or_default()
        .to_string();
    let statuses = crate::plugins::host(ui).statuses();
    let plugins = ui
        .ctx
        .config
        .read()
        .unwrap()
        .plugins
        .iter()
        .map(|p| {
            let st = statuses.iter().find(|s| s.id == p.id);
            PluginLine {
                name: st.map_or_else(|| p.id.clone(), |s| s.name.clone()),
                signed_in: st.filter(|s| s.caps.auth).map(|s| s.signed_in()),
            }
        })
        .collect();
    let dev = app.get_setup_dev();
    let output = if dev.name.is_empty() {
        String::new()
    } else if dev.network {
        // `upnp:<uuid>`, `plugin:<id>/<output>` say nothing to the reader: its name.
        dev.desc.to_string()
    } else {
        format!("{} ({})", dev.desc, dev.name)
    };
    let s = Summary {
        language,
        folders: STATE.with(|s| s.borrow().folders.clone()),
        plugins,
        output,
        fixed: app.get_setup_fixed(),
    };
    let lines: Vec<SetupLine> = summary_lines(&s)
        .into_iter()
        .map(|(label, value)| SetupLine {
            label: label.into(),
            value: value.into(),
        })
        .collect();
    app.set_setup_summary(ModelRc::new(VecModel::from(lines)));
}

// ------------------------------------------------------------ closing

/// A language picked: saved and applied at once, as in the settings, so
/// the assistant goes on in it (and Later keeps it).
fn pick_language(ui: &Rc<Ui>, index: i32) {
    let pref = crate::text::language_pref(index);
    ui.ctx.update_config(|c| c.ui.language = pref.into());
    ui.app().set_ui_language(index);
    crate::extras::apply_language(ui);
}

fn close(ui: &Ui) {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        // A read in flight is dropped when it lands.
        let serial = s.serial;
        *s = State {
            serial,
            ..State::default()
        };
    });
    ui.app().set_setup_open(false);
}

/// Later: the assistant is not shown again, and the language and plugins
/// already installed stay. Nothing else changes, but for a fresh install
/// with no folder yet: the music folder proposed is scanned, as it would
/// have been without the assistant (not the list as edited: Later does
/// not apply the steps).
fn skip(ui: &Ui) {
    let (ui_cfg, roots) = {
        let c = ui.ctx.config.read().unwrap();
        (c.ui.clone(), c.library.roots.clone())
    };
    let folders = skipped_folders(
        ui_cfg.setup_pending(),
        &roots,
        ricercar_core::config::default_music_dir().as_deref(),
    );
    if ui_cfg.setup_done != Some(true) || folders.is_some() {
        ui.ctx.update_config(|c| {
            c.ui.setup_done = Some(true);
            if let Some(f) = folders {
                c.library.roots = f;
            }
        });
        crate::extras::refresh_library_rows(ui);
    }
    close(ui);
}

/// The folders Later sets: the proposal, when the first launch is pending
/// and no folder is configured yet.
pub fn skipped_folders(
    pending: bool,
    configured: &[PathBuf],
    music: Option<&Path>,
) -> Option<Vec<PathBuf>> {
    let f = initial_folders(configured, music);
    (pending && configured.is_empty() && !f.is_empty()).then_some(f)
}

/// Save the choices: folders (scanned if they changed), output, volume.
fn finish(ui: &Ui) {
    let fixed = ui.app().get_setup_fixed();
    let (choice, folders) = STATE.with(|s| {
        let s = s.borrow();
        (s.choice.clone(), s.folders.clone())
    });
    ui.ctx.update_config(|c| {
        c.audio.fixed_volume = fixed;
        c.library.roots = folders;
        c.ui.setup_done = Some(true);
    });
    ui.app().set_fixed_volume(fixed);
    crate::extras::refresh_library_rows(ui);
    if let Some(name) = choice.filter(|n| *n != ui.ctx.ctl.device_name()) {
        crate::extras::select_device(ui, &name);
    }
    close(ui);
}

pub fn wire(ui: &Rc<Ui>) {
    let app = ui.app();
    app.on_setup_start(|| with_ui(open));
    app.on_setup_language(|i| with_ui(|ui| pick_language(ui, i)));
    app.on_setup_add_folder(|| crate::extras::pick_folder(add_folder));
    app.on_setup_remove_folder(|i| with_ui(|ui| remove_folder(ui, i)));
    app.on_setup_plugin_check(|id, on| with_ui(|ui| check_plugin(ui, &id, on)));
    app.on_setup_pick(|name| {
        with_ui(|ui| {
            STATE.with(|s| s.borrow_mut().choice = Some(name.to_string()));
            crate::extras::refresh_devices(ui);
        })
    });
    app.on_setup_next(|| with_ui(next));
    app.on_setup_back(|| with_ui(back));
    app.on_setup_skip(|| with_ui(|ui| skip(ui)));
    app.on_setup_finish(|| with_ui(|ui| finish(ui)));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, desc: &str, hardware: bool) -> DeviceRow {
        DeviceRow {
            name: name.into(),
            desc: desc.into(),
            hardware,
            shared: !hardware,
            ..Default::default()
        }
    }

    #[test]
    fn shown_until_finished_or_skipped() {
        let mut c = Config::default();
        // No flag: a config from before the assistant, or the daemon's.
        assert!(!should_show(&c));
        // Written by a first launch of the interface: at every start.
        c.ui.setup_done = Some(false);
        assert!(should_show(&c));
        c.ui.setup_done = Some(true);
        assert!(!should_show(&c));
    }

    #[test]
    fn shown_from_the_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c.toml");
        // A file from an older version, with no flag: not shown.
        std::fs::write(&p, "[audio]\nfixed_volume = true\n").unwrap();
        assert!(!should_show(&Config::load(&p)));
        // The first launch wrote it pending: shown again after a quit.
        std::fs::write(&p, "[ui]\nsetup_done = false\n").unwrap();
        assert!(should_show(&Config::load(&p)));
        std::fs::write(&p, "[ui]\nsetup_done = true\n").unwrap();
        assert!(!should_show(&Config::load(&p)));
        // The earlier name.
        std::fs::write(&p, "[ui]\naudio_setup_done = true\n").unwrap();
        assert!(!should_show(&Config::load(&p)));
    }

    #[test]
    fn later_scans_the_proposal() {
        let dir = tempfile::tempdir().unwrap();
        let music = dir.path().join("Music");
        std::fs::create_dir(&music).unwrap();
        // A fresh install skipped: the music folder.
        assert_eq!(
            skipped_folders(true, &[], Some(&music)),
            Some(vec![music.clone()])
        );
        // Nothing to propose, folders already chosen, or opened from the
        // settings: unchanged.
        assert_eq!(skipped_folders(true, &[], None), None);
        let mine = vec![PathBuf::from("/path/to/flac")];
        assert_eq!(skipped_folders(true, &mine, Some(&music)), None);
        assert_eq!(skipped_folders(false, &[], Some(&music)), None);
    }

    #[test]
    fn folder_labels() {
        let home = Path::new("/data/listener");
        let label = |p: &str| folder_label(Path::new(p), Some(home));
        assert_eq!(label("/data/listener/Music"), "~/Music");
        assert_eq!(label("/data/listener"), "~");
        assert_eq!(
            label("/data/listener-two/Music"),
            "/data/listener-two/Music"
        );
        assert_eq!(label("/mnt/flac"), "/mnt/flac");
        assert_eq!(folder_label(Path::new("/mnt/flac"), None), "/mnt/flac");
    }

    #[test]
    fn music_folder_prefill() {
        let dir = tempfile::tempdir().unwrap();
        let music = dir.path().join("Music");
        std::fs::create_dir(&music).unwrap();
        // Nothing configured: the music folder, when it exists.
        assert_eq!(
            initial_folders(&[], Some(&music)),
            std::slice::from_ref(&music)
        );
        assert!(initial_folders(&[], Some(&dir.path().join("missing"))).is_empty());
        assert!(initial_folders(&[], None).is_empty());
        // Folders configured: those, as they are.
        let mine = vec![
            PathBuf::from("/path/to/flac"),
            PathBuf::from("/path/to/dsd"),
        ];
        assert_eq!(initial_folders(&mine, Some(&music)), mine);
    }

    fn entry(id: &str, name: &str, arch: &str) -> catalog::Entry {
        catalog::Entry {
            id: id.into(),
            name: name.into(),
            description: format!("{name}, made up for the tests."),
            repository: format!("https://example.org/{id}"),
            version: "1.0.0".into(),
            protocol: ricercar_core::plugin::PROTOCOL,
            assets: vec![catalog::Asset {
                arch: arch.into(),
                url: format!("https://example.org/{id}/bin"),
                sha256: "0".repeat(64),
            }],
            ..Default::default()
        }
    }

    fn declared(id: &str) -> PluginConfig {
        PluginConfig {
            id: id.into(),
            command: "/path/to/plugin".into(),
            args: Vec::new(),
            enabled: true,
            version: Some("1.0.0".into()),
            host: None,
            source: None,
            settings: Default::default(),
        }
    }

    #[test]
    fn plugin_offers() {
        let here = catalog::current_arch();
        let entries = [
            entry("example-source", "Example source", here),
            entry("other-source", "Other source", here),
            entry("elsewhere", "Not for this computer", "no-such-arch"),
            entry("kept", "Installed already", "no-such-arch"),
        ];
        let checked = BTreeSet::from(["other-source".to_string()]);
        let got = offers(&entries, &[declared("kept")], &checked);
        let ids: Vec<&str> = got.iter().map(|o| o.id.as_str()).collect();
        // Not installable here and not installed: left out.
        assert_eq!(ids, ["example-source", "other-source", "kept"]);
        assert_eq!(got[0].name, "Example source");
        assert_eq!(got[0].description, "Example source, made up for the tests.");
        assert!(!got[0].installed && !got[0].checked);
        assert!(!got[1].installed && got[1].checked);
        // Installed: shown ticked.
        assert!(got[2].installed && got[2].checked);
        assert!(offers(&[], &[declared("kept")], &checked).is_empty());
    }

    #[test]
    fn summary() {
        let s = Summary {
            language: "Français".into(),
            folders: vec![PathBuf::from("/path/to/music")],
            plugins: vec![
                PluginLine {
                    name: "Example source".into(),
                    signed_in: Some(false),
                },
                PluginLine {
                    name: "Other source".into(),
                    signed_in: None,
                },
            ],
            output: "BIG KNOB STUDIO (hw:2,0)".into(),
            fixed: true,
        };
        let lines = summary_lines(&s);
        let labels: Vec<&str> = lines.iter().map(|l| l.0.as_str()).collect();
        assert_eq!(
            labels,
            [
                "Language",
                "Music folders",
                "Plugins",
                "Output",
                "Fixed volume"
            ]
        );
        assert_eq!(lines[0].1, "Français");
        assert_eq!(lines[1].1, "/path/to/music");
        assert_eq!(lines[2].1, "Example source · Not signed in\nOther source");
        assert_eq!(lines[3].1, "BIG KNOB STUDIO (hw:2,0)");
        assert_eq!(lines[4].1, "On");
        let none = summary_lines(&Summary {
            folders: Vec::new(),
            plugins: Vec::new(),
            fixed: false,
            ..s
        });
        assert_eq!((none[1].1.as_str(), none[2].1.as_str()), ("None", "None"));
        assert_eq!(none[4].1, "Off");
    }

    #[test]
    fn plugin_status_lines() {
        let (text, failed, sign_in) = plugin_state(Some(&Outcome::Installing), false, None);
        assert_eq!(
            (text.as_str(), failed, sign_in),
            ("Installing…", false, false)
        );
        let (text, failed, _) =
            plugin_state(Some(&Outcome::Failed("network error".into())), false, None);
        assert!(failed && text.ends_with(": network error"));
        assert_eq!(plugin_state(None, false, None).0, "");
        assert_eq!(
            plugin_state(Some(&Outcome::Installed), true, None).0,
            "Installed"
        );
    }

    #[test]
    fn hardware_first() {
        let rows = [
            row("default", "ALSA default", false),
            row("hw:0,0", "HDA Intel PCH: ALC257 Analog", true),
            row("pipewire", "PipeWire", false),
            row("hw:2,0", "BIG KNOB STUDIO: USB Audio", true),
            DeviceRow {
                name: "Discards audio (testing)".into(),
                desc: "Null sink".into(),
                selected: true,
                ..Default::default()
            },
            DeviceRow {
                name: "upnp:4d2f7c1a-9b3e-4c56-8a01-2b7d9e6f3c40".into(),
                desc: "Salon".into(),
                network: true,
                ..Default::default()
            },
            row("pulse", "PulseAudio", false),
        ];
        let names: Vec<String> = offered(&rows).iter().map(|r| r.name.to_string()).collect();
        assert_eq!(
            names,
            [
                "hw:0,0",
                "hw:2,0",
                "default",
                "pipewire",
                "pulse",
                "upnp:4d2f7c1a-9b3e-4c56-8a01-2b7d9e6f3c40"
            ]
        );
        assert_eq!(offered(&rows)[1].desc, "BIG KNOB STUDIO");
        assert!(offered(&rows).iter().all(|r| !r.selected));
    }

    #[test]
    fn first_choice() {
        let rows = [
            row("hw:0,0", "HDA Intel PCH: ALC257 Analog", true),
            row("hw:2,0", "BIG KNOB STUDIO: USB Audio", true),
            row("default", "ALSA default", false),
        ];
        // A USB card over the default output.
        assert_eq!(initial_choice(&rows, "default"), "hw:2,0");
        // A hardware output already chosen stays.
        assert_eq!(initial_choice(&rows, "hw:0,0"), "hw:0,0");
        // No USB card: the current output.
        assert_eq!(initial_choice(&rows[..1], "hw:9,0"), "hw:0,0");
        assert_eq!(
            initial_choice(&[rows[0].clone(), rows[2].clone()], "default"),
            "default"
        );
        assert_eq!(initial_choice(&[], "default"), "default");
    }
}
