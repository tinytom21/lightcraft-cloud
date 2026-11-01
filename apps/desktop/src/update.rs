//! Looking for a newer version of this app on GitHub, and (on Windows) installing it when the user clicks.
//!
//! * **The check**: shortly after the start and then once a day, one request to the GitHub API for the latest *published* release
//!   (never a draft or a prerelease), with a User-Agent that names this app and nothing else about the user. A check that fails is a
//!   line in the log and nothing more; the result lives in `<state>/update.json`, which is not part of the sync status, so a failed
//!   check can never colour the tray or the Status window's state line. Off with `check_for_updates = false`.
//! * **The offer**: the tray menu and the Status window say a newer version exists, with a link to its release notes. One desktop
//!   notification per version at most. On Linux that is all: the app never replaces itself there.
//! * **The update** (Windows, only after a click, see [`request_install`]): download `lightcraft-cloud-<version>-windows-setup.exe`
//!   and `SHA256SUMS` from that release into `<state>/update/`, check the installer against the SHA-256 in `SHA256SUMS`, wait until no
//!   sync or upload runs, then run the installer silently. The installer stops this app, replaces the files and starts the app again
//!   (`packaging/windows/lightcraft-cloud.iss`). Whatever goes wrong before that leaves the running app as it was.
//!
//! **What this does not protect against.** `SHA256SUMS` comes from the same release as the installer, so the check detects a damaged or
//! cut-off download, not a release published by someone who controls the GitHub account. The installer is not code-signed.
//!
//! **Where updates come from is fixed in the program.** [`Source::github`] has no inputs: no config key, environment variable or command
//! line option can point the app at another server (a test checks that nothing in this file reads the environment). The tests give an
//! [`Updater`] a local source through [`Updater::build`], which only the tests can call.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use lightcraft_cloud_client::home::utc_iso;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::notifier::{Notifier, TITLE};

/// The repository releases come from.
pub const REPO: &str = "tinytom21/lightcraft-cloud";
const API_LATEST: &str = "https://api.github.com/repos/tinytom21/lightcraft-cloud/releases/latest";
/// Every installer and checksum file must live under this.
const DOWNLOAD_PREFIX: &str = "https://github.com/tinytom21/lightcraft-cloud/releases/download/";
/// GitHub serves release files from these hosts after a redirect; nothing else is followed.
const REDIRECT_HOSTS: &[&str] = &["objects.githubusercontent.com", "release-assets.githubusercontent.com", "github-releases.githubusercontent.com"];
/// What the app says about itself in a request.
pub const USER_AGENT: &str = concat!("lightcraft-cloud-desktop/", env!("CARGO_PKG_VERSION"), " (update check; +https://github.com/tinytom21/lightcraft-cloud)");

/// The first check after the start, and the time between checks. A failed check is tried again after [`RETRY_AFTER`].
pub const FIRST_CHECK_AFTER: Duration = Duration::from_secs(90);
pub const CHECK_EVERY: Duration = Duration::from_secs(24 * 3600);
pub const RETRY_AFTER: Duration = Duration::from_secs(3600);

const API_LIMIT: u64 = 1 << 20;
const SUMS_LIMIT: u64 = 64 << 10;
const INSTALLER_LIMIT: u64 = 300 << 20;
const REDIRECTS: usize = 4;

/// The file name of a version's Windows installer.
pub fn setup_name(version: &Version) -> String {
    format!("lightcraft-cloud-{version}-windows-setup.exe")
}

/// The page of a version's release (its notes).
pub fn release_page(tag: &str) -> String {
    format!("https://github.com/{REPO}/releases/tag/{tag}")
}

// ------------------------------------------------------------------ versions

/// A semantic version (`1.2.3`, `1.2.3-rc.1`), compared by the semver rules; build metadata (`+...`) is ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    core: [u64; 3],
    pre: Vec<Ident>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Ident {
    /// (numbers sort before words, and by value)
    Num(u64),
    Text(String),
}

fn number(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) || (s.len() > 1 && s.starts_with('0')) {
        return None;
    }
    s.parse().ok()
}

impl Version {
    /// `1.2.3`, `v1.2.3`, `1.2.3-beta.1`, `1.2.3+build`. Anything else is `None`.
    pub fn parse(s: &str) -> Option<Version> {
        let s = s.trim();
        let s = s.strip_prefix('v').unwrap_or(s);
        let s = s.split('+').next()?;
        let (core, pre) = match s.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (s, None),
        };
        let mut parts = core.split('.');
        let core = [number(parts.next()?)?, number(parts.next()?)?, number(parts.next()?)?];
        if parts.next().is_some() {
            return None;
        }
        let mut idents = Vec::new();
        if let Some(pre) = pre {
            for id in pre.split('.') {
                if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
                    return None;
                }
                idents.push(if id.bytes().all(|b| b.is_ascii_digit()) { Ident::Num(number(id)?) } else { Ident::Text(id.to_string()) });
            }
        }
        Some(Version { core, pre: idents })
    }

    pub fn is_prerelease(&self) -> bool {
        !self.pre.is_empty()
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering::*;
        self.core.cmp(&other.core).then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
            (true, true) => Equal,
            (true, false) => Greater, // (1.0.0 is newer than 1.0.0-rc.1)
            (false, true) => Less,
            (false, false) => self.pre.cmp(&other.pre),
        })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.core[0], self.core[1], self.core[2])?;
        if !self.pre.is_empty() {
            let ids: Vec<String> = self.pre.iter().map(|i| match i {
                Ident::Num(n) => n.to_string(),
                Ident::Text(t) => t.clone(),
            }).collect();
            write!(f, "-{}", ids.join("."))?;
        }
        Ok(())
    }
}

/// This program's version.
pub fn current_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("the crate version is semver")
}

// ------------------------------------------------------------------ the release

#[derive(Debug, Deserialize)]
struct RawRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<RawAsset>,
}

#[derive(Debug, Deserialize)]
struct RawAsset {
    name: String,
    browser_download_url: String,
}

/// A file attached to a release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub url: String,
}

/// The latest published release, as far as the app cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    pub tag: String,
    /// The Windows installer and the checksums, if the release has them.
    pub setup: Option<Asset>,
    pub sums: Option<Asset>,
}

/// Read the GitHub API's answer for "latest release". A draft or a prerelease is refused (the API does not return them, but this does not
/// depend on that), as is a tag that is not a version.
pub fn parse_latest(json: &[u8]) -> Result<Release, String> {
    let raw: RawRelease = serde_json::from_slice(json).map_err(|e| format!("the release information is not understood: {e}"))?;
    if raw.draft || raw.prerelease {
        return Err("the latest release is a draft or a prerelease".into());
    }
    let version = Version::parse(&raw.tag_name).ok_or_else(|| format!("the release tag {:?} is not a version", raw.tag_name))?;
    if version.is_prerelease() {
        return Err("the latest release is a prerelease".into());
    }
    let setup_file = setup_name(&version);
    let find = |name: &str| raw.assets.iter().find(|a| a.name == name).map(|a| Asset { name: a.name.clone(), url: a.browser_download_url.clone() });
    Ok(Release { setup: find(&setup_file), sums: find("SHA256SUMS"), version, tag: raw.tag_name })
}

/// The SHA-256 that `SHA256SUMS` (the format of `sha256sum`) lists for `name`.
pub fn hash_for(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|l| {
        let (hash, file) = l.trim().split_once(char::is_whitespace)?;
        let file = file.trim().trim_start_matches('*');
        (file == name && hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit())).then(|| hash.to_ascii_lowercase())
    })
}

// ------------------------------------------------------------------ where updates come from

/// Where to ask and what to trust. Only [`Source::github`] exists in the program; the tests have one for a local server.
#[derive(Debug, Clone)]
pub struct Source {
    latest: String,
    download_prefix: String,
    redirect_hosts: Vec<String>,
    /// Plain `http://` allowed (the local server of the tests).
    insecure: bool,
}

impl Source {
    /// GitHub, the only source the app has.
    pub fn github() -> Source {
        Source {
            latest: API_LATEST.into(),
            download_prefix: DOWNLOAD_PREFIX.into(),
            redirect_hosts: REDIRECT_HOSTS.iter().map(|h| h.to_string()).collect(),
            insecure: false,
        }
    }

    #[cfg(test)]
    fn local(base: &str) -> Source {
        Source {
            latest: format!("{base}/repos/{REPO}/releases/latest"),
            download_prefix: format!("{base}/{REPO}/releases/download/"),
            redirect_hosts: vec!["127.0.0.1".into()],
            insecure: true,
        }
    }

    /// The address a release file must have: no other is downloaded.
    fn asset_url(&self, tag: &str, name: &str) -> String {
        format!("{}{tag}/{name}", self.download_prefix)
    }

    /// May a redirect go to `url`?
    fn redirect_ok(&self, url: &str) -> bool {
        let Some((scheme, host)) = split_url(url) else { return false };
        (scheme == "https" || (self.insecure && scheme == "http")) && self.redirect_hosts.iter().any(|h| h.eq_ignore_ascii_case(host))
    }
}

/// (scheme, host) of an absolute URL without credentials.
fn split_url(u: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = u.split_once("://")?;
    let auth = rest.split(['/', '?', '#']).next()?;
    if auth.is_empty() || auth.contains('@') {
        return None;
    }
    let host = match auth.rsplit_once(':') {
        Some((h, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => h,
        _ => auth,
    };
    Some((scheme, host))
}

/// The one HTTP client: GET with the redirect rules of the [`Source`].
struct Client<'a> {
    source: &'a Source,
}

impl Client<'_> {
    fn agent(&self, total: Duration) -> ureq::Agent {
        ureq::Agent::config_builder()
            .user_agent(USER_AGENT)
            .max_redirects(0)
            .max_redirects_will_error(false)
            .https_only(!self.source.insecure)
            .timeout_connect(Some(Duration::from_secs(15)))
            .timeout_global(Some(total))
            .build()
            .into()
    }

    /// GET `url`, handing the body to `on_data` in pieces with the total length when the server said it. Redirects are followed only
    /// when `follow` and only to the hosts the source allows. At most `limit` bytes.
    fn get(&self, url: &str, follow: bool, accept: &str, limit: u64, total: Duration, on_data: &mut dyn FnMut(&[u8], Option<u64>) -> Result<(), String>) -> Result<(), String> {
        let agent = self.agent(total);
        let mut url = url.to_string();
        for _ in 0..=REDIRECTS {
            let resp = agent.get(&url).header("Accept", accept).call().map_err(|e| e.to_string())?;
            let status = resp.status().as_u16();
            if (300..400).contains(&status) {
                let to = resp.headers().get("location").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                if !follow || !self.source.redirect_ok(&to) {
                    return Err(format!("the server redirected to an address that is not allowed ({status})"));
                }
                url = to;
                continue;
            }
            if status != 200 {
                return Err(format!("the server answered {status}"));
            }
            let len = resp.headers().get("content-length").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
            if len.is_some_and(|l| l > limit) {
                return Err("the file is larger than expected".into());
            }
            let mut reader = resp.into_body().into_with_config().limit(limit).reader();
            let mut buf = vec![0u8; 64 << 10];
            loop {
                let n = reader.read(&mut buf).map_err(|e| e.to_string())?;
                if n == 0 {
                    return Ok(());
                }
                on_data(&buf[..n], len)?;
            }
        }
        Err("too many redirects".into())
    }

    fn get_bytes(&self, url: &str, follow: bool, accept: &str, limit: u64) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        self.get(url, follow, accept, limit, Duration::from_secs(60), &mut |d, _| {
            out.extend_from_slice(d);
            Ok(())
        })?;
        Ok(out)
    }
}

// ------------------------------------------------------------------ what is kept in the state folder

/// A newer version that exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Offer {
    pub version: String,
    /// The release notes.
    pub page: String,
    /// The app can install it itself (Windows, with the files in the release).
    pub can_install: bool,
}

/// How far an update got.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Phase {
    #[default]
    Idle,
    Downloading {
        done: u64,
        total: Option<u64>,
    },
    Verifying,
    /// Downloaded and checked; a sync or an upload is running, so it waits.
    Waiting,
    /// The installer runs.
    Installing,
    Failed {
        message: String,
    },
}

impl Phase {
    /// An update is under way (not to be interrupted by a new check or request).
    pub fn is_working(&self) -> bool {
        matches!(self, Phase::Downloading { .. } | Phase::Verifying | Phase::Waiting | Phase::Installing)
    }
}

/// `<state>/update.json`: written by the app, read by the Status window and the tray. Separate from `status.json`, so that nothing about
/// updates can touch the sync's health.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateState {
    /// ISO time of the last check that worked.
    pub checked: Option<String>,
    pub latest: Option<Offer>,
    /// The version the user was last told about.
    pub notified: Option<String>,
    pub phase: Phase,
}

/// What the Status window shows about an update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateView {
    pub version: String,
    pub page: String,
    pub can_install: bool,
    pub phase: Phase,
}

impl UpdateState {
    fn path(state_dir: &Path) -> PathBuf {
        state_dir.join("update.json")
    }

    pub fn load(state_dir: &Path) -> UpdateState {
        std::fs::read(Self::path(state_dir)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    fn save(&self, state_dir: &Path) -> Result<(), String> {
        crate::appstate::write_atomic(&Self::path(state_dir), &serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?)
    }

    /// The offer, if it is newer than `current`.
    pub fn view(&self, current: &Version) -> Option<UpdateView> {
        let o = self.latest.as_ref()?;
        (Version::parse(&o.version)? > *current).then(|| UpdateView { version: o.version.clone(), page: o.page.clone(), can_install: o.can_install, phase: self.phase.clone() })
    }
}

fn request_path(state_dir: &Path) -> PathBuf {
    state_dir.join("update-request")
}

/// Ask the running app to install `version` (the Status window and the tray menu do this). The app does it only if that is the version it
/// offers, and only on Windows.
pub fn request_install(state_dir: &Path, version: &str) -> Result<(), String> {
    std::fs::create_dir_all(state_dir).map_err(|e| format!("{}: {e}", state_dir.display()))?;
    let p = request_path(state_dir);
    std::fs::write(&p, version).map_err(|e| format!("{}: {e}", p.display()))
}

/// Where the installer and its checksums are kept while updating.
pub fn update_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("update")
}

/// Remove what an earlier update left behind: files of installers that are not newer than this program (the update was done), and
/// half-downloaded ones.
fn clean_leftovers(state_dir: &Path, current: &Version) {
    let dir = update_dir(state_dir);
    let Ok(entries) = std::fs::read_dir(&dir) else { return };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let newer = name
            .strip_prefix("lightcraft-cloud-")
            .and_then(|r| r.strip_suffix("-windows-setup.exe"))
            .and_then(Version::parse)
            .is_some_and(|v| v > *current);
        if !newer {
            let _ = std::fs::remove_file(e.path());
        }
    }
    let _ = std::fs::remove_dir(&dir); // (only when empty)
}

// ------------------------------------------------------------------ the updater

/// Runs the installer. The real one starts the downloaded setup program and waits for it; tests give it a fake.
pub trait Launcher: Send {
    /// Run `installer` silently and wait. The exit code.
    fn run(&mut self, installer: &Path, log: &Path) -> Result<i32, String>;
}

struct SetupLauncher;

impl Launcher for SetupLauncher {
    fn run(&mut self, installer: &Path, log: &Path) -> Result<i32, String> {
        let mut cmd = std::process::Command::new(installer);
        cmd.args(["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART"]);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // (the installer's own log, in case it fails; CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP: it does not depend on this program's console)
            cmd.raw_arg(format!("/LOG=\"{}\"", log.display())).creation_flags(0x0800_0000 | 0x0000_0200);
        }
        #[cfg(not(windows))]
        let _ = log;
        let status = cmd.status().map_err(|e| e.to_string())?;
        status.code().ok_or_else(|| "the installer was stopped".to_string())
    }
}

/// What the settings say, at each tick.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// `check_for_updates`.
    pub enabled: bool,
    /// `notify`.
    pub notify: bool,
}

struct Ready {
    setup: PathBuf,
}

pub struct Updater {
    source: Source,
    state_dir: PathBuf,
    current: Version,
    /// This system installs updates itself (Windows); elsewhere the app only says that one exists.
    installs: bool,
    state: UpdateState,
    release: Option<Release>,
    ready: Option<Ready>,
    next_check: SystemTime,
    idle: Box<dyn Fn() -> bool + Send>,
    notifier: Box<dyn Notifier>,
    launcher: Box<dyn Launcher>,
    log: Box<dyn Fn(&str) + Send>,
}

impl Updater {
    /// The updater of the running app. `idle`: no sync or upload is running now.
    pub fn new(state_dir: &Path, now: SystemTime, idle: Box<dyn Fn() -> bool + Send>, notifier: Box<dyn Notifier>, log: Box<dyn Fn(&str) + Send>) -> Updater {
        Updater::build(Source::github(), state_dir, now, current_version(), cfg!(windows), idle, notifier, Box::new(SetupLauncher), log)
    }

    /// (The tests use this with a local source; the program only calls it through [`Updater::new`].)
    fn build(
        source: Source,
        state_dir: &Path,
        now: SystemTime,
        current: Version,
        installs: bool,
        idle: Box<dyn Fn() -> bool + Send>,
        notifier: Box<dyn Notifier>,
        launcher: Box<dyn Launcher>,
        log: Box<dyn Fn(&str) + Send>,
    ) -> Updater {
        let mut state = UpdateState::load(state_dir);
        // (what an earlier run left is not this run's: it looks again shortly after the start)
        state.phase = Phase::Idle;
        state.latest = None;
        clean_leftovers(state_dir, &current);
        let _ = std::fs::remove_file(request_path(state_dir));
        Updater { source, state_dir: state_dir.to_path_buf(), current, installs, state, release: None, ready: None, next_check: now + FIRST_CHECK_AFTER, idle, notifier, launcher, log }
    }

    pub fn state(&self) -> &UpdateState {
        &self.state
    }

    fn say(&self, m: impl AsRef<str>) {
        (self.log)(m.as_ref());
    }

    fn save(&self) {
        if let Err(e) = self.state.save(&self.state_dir) {
            self.say(format!("cannot write update.json: {e}"));
        }
    }

    fn set_phase(&mut self, phase: Phase) {
        self.state.phase = phase;
        self.save();
    }

    fn fail(&mut self, message: impl Into<String>) {
        let message = message.into();
        self.say(format!("update: {message}"));
        self.ready = None;
        self.set_phase(Phase::Failed { message });
    }

    /// One look at what is due: a request to install, a download that is ready and may go, a check. Returns whether the state changed.
    /// A download blocks (it saves its progress as it goes).
    pub fn tick(&mut self, now: SystemTime, opts: Options) -> bool {
        let before = self.state.clone();
        if !opts.enabled {
            let _ = std::fs::remove_file(request_path(&self.state_dir));
            if self.state.latest.is_some() || self.state.phase != Phase::Idle {
                self.state.latest = None;
                self.state.phase = Phase::Idle;
                self.release = None;
                self.ready = None;
                self.save();
            }
            return self.state != before;
        }
        self.take_request();
        if self.ready.is_some() && (self.idle)() {
            self.launch();
        }
        if now >= self.next_check && !self.state.phase.is_working() {
            self.check(now, opts.notify);
        }
        self.state != before
    }

    fn take_request(&mut self) {
        let p = request_path(&self.state_dir);
        let Ok(text) = std::fs::read_to_string(&p) else { return };
        let _ = std::fs::remove_file(&p);
        let wanted = text.trim();
        let ok = self.installs
            && !matches!(self.state.phase, Phase::Downloading { .. } | Phase::Verifying | Phase::Installing)
            && self.ready.is_none()
            && self.state.latest.as_ref().is_some_and(|o| o.can_install && o.version == wanted);
        if !ok {
            self.say(format!("update: ignored a request to install {wanted:?}"));
            return;
        }
        if let Some(rel) = self.release.clone() {
            self.download(&rel);
        }
    }

    fn check(&mut self, now: SystemTime, notify: bool) {
        let result = Client { source: &self.source }
            .get_bytes(&self.source.latest, false, "application/vnd.github+json", API_LIMIT)
            .and_then(|b| parse_latest(&b));
        let rel = match result {
            Ok(r) => r,
            Err(e) => {
                // (silent: a line in the log; the sync status is not touched)
                self.say(format!("update check: {e}"));
                self.next_check = now + RETRY_AFTER;
                return;
            }
        };
        self.next_check = now + CHECK_EVERY;
        self.state.checked = Some(utc_iso(now));
        if rel.version > self.current {
            let can_install = self.installs && rel.setup.is_some() && rel.sums.is_some();
            if self.installs && !can_install {
                // (a release that is not complete yet: wait for it)
                self.say(format!("update check: version {} has no installer yet", rel.version));
                self.state.latest = None;
                self.release = None;
                self.save();
                return;
            }
            let version = rel.version.to_string();
            let page = release_page(&rel.tag);
            self.say(format!("update check: version {version} is available"));
            if notify && self.state.notified.as_deref() != Some(&version) {
                self.notifier.notify(TITLE, &if self.installs {
                    format!("Version {version} of LightCraft Cloud is available. Open Status to update.")
                } else {
                    format!("Version {version} of LightCraft Cloud is available: {page}")
                });
                self.state.notified = Some(version.clone());
            }
            self.state.latest = Some(Offer { version, page, can_install });
            self.release = Some(rel);
        } else {
            self.state.latest = None;
            self.release = None;
        }
        self.save();
    }

    /// Download the installer and its checksums, and check the one against the other.
    fn download(&mut self, rel: &Release) {
        let (Some(setup), Some(sums)) = (rel.setup.clone(), rel.sums.clone()) else {
            return self.fail("This release has no installer.");
        };
        let name = setup_name(&rel.version);
        if setup.url != self.source.asset_url(&rel.tag, &name) || sums.url != self.source.asset_url(&rel.tag, "SHA256SUMS") {
            return self.fail("The update was not downloaded: its address is not one this app trusts. Nothing was changed.");
        }
        self.set_phase(Phase::Downloading { done: 0, total: None });
        let client = Client { source: &self.source };
        let sums_text = match client.get_bytes(&sums.url, true, "application/octet-stream", SUMS_LIMIT) {
            Ok(b) => String::from_utf8_lossy(&b).into_owned(),
            Err(e) => return self.fail(format!("Could not download the update ({e}). The current version keeps working; try again later.")),
        };
        let Some(want) = hash_for(&sums_text, &name) else {
            return self.fail("The release has no checksum for the installer, so it was not downloaded. Nothing was changed.");
        };
        let dir = update_dir(&self.state_dir);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return self.fail(format!("Could not make the folder for the download ({}: {e}).", dir.display()));
        }
        let (part, dest) = (dir.join(format!("{name}.part")), dir.join(&name));
        let mut file = match std::fs::File::create(&part) {
            Ok(f) => f,
            Err(e) => return self.fail(format!("Could not write the download ({}: {e}).", part.display())),
        };
        let mut hasher = Sha256::new();
        let (mut done, mut saved) = (0u64, Instant::now());
        let state_dir = self.state_dir.clone();
        let mut progress = self.state.clone();
        let result = Client { source: &self.source }.get(&setup.url, true, "application/octet-stream", INSTALLER_LIMIT, Duration::from_secs(20 * 60), &mut |d, total| {
            hasher.update(d);
            file.write_all(d).map_err(|e| format!("writing the download: {e}"))?;
            done += d.len() as u64;
            if saved.elapsed() >= Duration::from_millis(250) {
                saved = Instant::now();
                progress.phase = Phase::Downloading { done, total };
                let _ = progress.save(&state_dir);
            }
            Ok(())
        });
        let finished = result.and_then(|()| file.sync_all().map_err(|e| e.to_string()));
        drop(file);
        if let Err(e) = finished {
            let _ = std::fs::remove_file(&part);
            return self.fail(format!("Could not download the update ({e}). The current version keeps working; try again later."));
        }
        self.set_phase(Phase::Verifying);
        let got = format!("{:x}", hasher.finalize());
        if got != want {
            let _ = std::fs::remove_file(&part);
            return self.fail("The downloaded installer did not match its checksum, so it was not run. Nothing was changed; the current version keeps working.");
        }
        if let Err(e) = lightcraft_cloud_client::replace_file(&part, &dest) {
            let _ = std::fs::remove_file(&part);
            return self.fail(format!("Could not keep the download ({}: {e}).", dest.display()));
        }
        self.say(format!("update: {name} downloaded and checked"));
        self.ready = Some(Ready { setup: dest });
        self.set_phase(Phase::Waiting);
    }

    /// The installer is here and nothing is syncing: run it. It stops this app, replaces the files and starts the app again; when it returns
    /// with this process still alive, it failed, and the old version is still here.
    fn launch(&mut self) {
        let Some(ready) = self.ready.take() else { return };
        self.set_phase(Phase::Installing);
        self.say(format!("update: running {}", ready.setup.display()));
        let log = update_dir(&self.state_dir).join("setup.log");
        match self.launcher.run(&ready.setup, &log) {
            Ok(0) => {
                // (the installer ends this program as part of its work; getting here means it did not need to)
                self.say("update: the installer finished");
                self.set_phase(Phase::Idle);
            }
            Ok(code) => self.fail(format!("The installer did not finish (code {code}). The current version is still installed and keeps working.")),
            Err(e) => self.fail(format!("Could not start the installer ({e}). The current version keeps working.")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    // ---- versions

    #[test]
    fn versions_compare_by_the_semver_rules() {
        let order = ["0.1.0", "0.2.0-alpha", "0.2.0-alpha.1", "0.2.0-alpha.beta", "0.2.0-beta", "0.2.0-beta.2", "0.2.0-beta.11", "0.2.0-rc.1", "0.2.0", "0.2.1", "0.10.0", "1.0.0", "10.0.0"];
        for w in order.windows(2) {
            assert!(v(w[0]) < v(w[1]), "{} < {}", w[0], w[1]);
            assert!(v(w[1]) > v(w[0]));
        }
        assert!(v("0.9.0") < v("0.10.0"), "numbers, not text");
        assert_eq!(v("v1.2.3"), v("1.2.3"));
        assert_eq!(v("1.2.3+abc").cmp(&v("1.2.3+xyz")), std::cmp::Ordering::Equal, "build metadata is ignored");
        assert_eq!(v("1.2.3-rc.1").to_string(), "1.2.3-rc.1");
        assert!(v("1.0.0-rc.1").is_prerelease() && !v("1.0.0").is_prerelease());
        for bad in ["", "1", "1.2", "1.2.3.4", "01.2.3", "1.2.x", "1.2.3-", "1.2.3-a..b", "latest", "v", "1.2.3-é"] {
            assert!(Version::parse(bad).is_none(), "{bad:?} is not a version");
        }
    }

    // ---- reading the release

    fn release_json(tag: &str, draft: bool, pre: bool, base: &str, with_assets: bool) -> String {
        let ver = tag.trim_start_matches('v');
        let assets = if with_assets {
            format!(
                r#"[{{"name":"lightcraft-cloud-{ver}-windows-setup.exe","browser_download_url":"{base}{REPO}/releases/download/{tag}/lightcraft-cloud-{ver}-windows-setup.exe"}},
                    {{"name":"SHA256SUMS","browser_download_url":"{base}{REPO}/releases/download/{tag}/SHA256SUMS"}},
                    {{"name":"lightcraft-cloud-{ver}-linux-light.tar.gz","browser_download_url":"{base}x/linux"}}]"#
            )
        } else {
            "[]".to_string()
        };
        format!(r#"{{"tag_name":"{tag}","draft":{draft},"prerelease":{pre},"html_url":"https://example.invalid/x","assets":{assets},"body":"notes"}}"#)
    }

    #[test]
    fn the_latest_release_is_read_and_drafts_prereleases_and_nonsense_are_refused() {
        let ok = parse_latest(release_json("v0.3.0", false, false, "https://github.com/", true).as_bytes()).unwrap();
        assert_eq!(ok.version, v("0.3.0"));
        assert_eq!(ok.setup.unwrap().name, "lightcraft-cloud-0.3.0-windows-setup.exe");
        assert_eq!(ok.sums.unwrap().name, "SHA256SUMS");
        // no installer attached: still a release (the Linux app only needs the version), without the assets
        let bare = parse_latest(release_json("v0.3.0", false, false, "", false).as_bytes()).unwrap();
        assert!(bare.setup.is_none() && bare.sums.is_none());
        assert!(parse_latest(release_json("v0.3.0", true, false, "", true).as_bytes()).unwrap_err().contains("draft"));
        assert!(parse_latest(release_json("v0.3.0", false, true, "", true).as_bytes()).unwrap_err().contains("prerelease"));
        assert!(parse_latest(release_json("v0.3.0-rc.1", false, false, "", true).as_bytes()).unwrap_err().contains("prerelease"));
        assert!(parse_latest(release_json("nightly", false, false, "", true).as_bytes()).unwrap_err().contains("not a version"));
        for bad in ["", "not json", "[]", "{}", r#"{"tag_name":5}"#, r#"{"tag_name":"v1.0.0","assets":"x"}"#] {
            assert!(parse_latest(bad.as_bytes()).is_err(), "{bad:?}");
        }
        // an asset of another version's name is not this version's installer
        let wrong = r#"{"tag_name":"v0.3.0","assets":[{"name":"lightcraft-cloud-0.2.0-windows-setup.exe","browser_download_url":"https://x/y"}]}"#;
        assert!(parse_latest(wrong.as_bytes()).unwrap().setup.is_none());
    }

    #[test]
    fn the_checksum_file_is_read_like_sha256sum_writes_it() {
        let h = "a".repeat(64);
        let sums = format!("{h}  lightcraft-cloud-0.3.0-windows-setup.exe\n{}  other.tar.gz\n{}  *star.bin\n", "b".repeat(64), "c".repeat(64));
        assert_eq!(hash_for(&sums, "lightcraft-cloud-0.3.0-windows-setup.exe"), Some(h));
        assert_eq!(hash_for(&sums, "star.bin"), Some("c".repeat(64)));
        assert_eq!(hash_for(&sums, "missing"), None);
        assert_eq!(hash_for("short  lightcraft.exe", "lightcraft.exe"), None, "a hash of the wrong length is no hash");
        assert_eq!(hash_for(&format!("{}  x.exe", "z".repeat(64)), "x.exe"), None);
    }

    // ---- a local server standing in for GitHub

    struct Reply {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    fn reply(status: u16, body: impl Into<Vec<u8>>) -> Reply {
        Reply { status, headers: vec![], body: body.into() }
    }

    struct Server {
        base: String,
        routes: Arc<Mutex<HashMap<String, Arc<dyn Fn() -> Reply + Send + Sync>>>>,
        seen: Arc<Mutex<Vec<(String, String)>>>, // (path, request head)
        stop: Arc<AtomicBool>,
    }

    impl Server {
        fn start() -> Server {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let s = Server { base, routes: Arc::default(), seen: Arc::default(), stop: Arc::default() };
            let (routes, seen, stop) = (s.routes.clone(), s.seen.clone(), s.stop.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((conn, _)) => {
                            let (routes, seen) = (routes.clone(), seen.clone());
                            std::thread::spawn(move || serve(conn, &routes, &seen));
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(5)),
                    }
                }
            });
            s
        }

        fn route(&self, path: &str, f: impl Fn() -> Reply + Send + Sync + 'static) {
            self.routes.lock().unwrap().insert(path.to_string(), Arc::new(f));
        }

        fn count(&self, path: &str) -> usize {
            self.seen.lock().unwrap().iter().filter(|(p, _)| p == path).count()
        }

        fn total(&self) -> usize {
            self.seen.lock().unwrap().len()
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
        }
    }

    type Routes = Arc<Mutex<HashMap<String, Arc<dyn Fn() -> Reply + Send + Sync>>>>;

    fn serve(conn: TcpStream, routes: &Routes, seen: &Arc<Mutex<Vec<(String, String)>>>) {
        conn.set_nonblocking(false).unwrap();
        let mut rd = BufReader::new(conn.try_clone().unwrap());
        let (mut first, mut head) = (String::new(), String::new());
        rd.read_line(&mut first).unwrap();
        loop {
            let mut l = String::new();
            if rd.read_line(&mut l).unwrap_or(0) == 0 || l == "\r\n" {
                break;
            }
            head.push_str(&l);
        }
        let path = first.split_whitespace().nth(1).unwrap_or("").to_string();
        seen.lock().unwrap().push((path.clone(), format!("{first}{head}")));
        let r = routes.lock().unwrap().get(&path).cloned().map(|f| f()).unwrap_or_else(|| reply(404, "not found"));
        let mut out = format!("HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n", r.status, r.body.len());
        for (k, v) in &r.headers {
            out += &format!("{k}: {v}\r\n");
        }
        out += "\r\n";
        let mut conn = conn;
        let _ = conn.write_all(out.as_bytes()).and_then(|()| conn.write_all(&r.body));
    }

    struct Fakes {
        notified: Arc<Mutex<Vec<String>>>,
        launched: Arc<Mutex<Vec<(PathBuf, bool)>>>,
        idle: Arc<AtomicBool>,
        exit: Arc<AtomicUsize>,
        logs: Arc<Mutex<Vec<String>>>,
    }

    struct FakeNotifier(Arc<Mutex<Vec<String>>>);
    impl Notifier for FakeNotifier {
        fn notify(&self, _s: &str, body: &str) {
            self.0.lock().unwrap().push(body.to_string());
        }
    }

    struct FakeLauncher {
        launched: Arc<Mutex<Vec<(PathBuf, bool)>>>,
        idle: Arc<AtomicBool>,
        exit: Arc<AtomicUsize>,
    }
    impl Launcher for FakeLauncher {
        fn run(&mut self, installer: &Path, _log: &Path) -> Result<i32, String> {
            self.launched.lock().unwrap().push((installer.to_path_buf(), self.idle.load(Ordering::SeqCst)));
            Ok(self.exit.load(Ordering::SeqCst) as i32)
        }
    }

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("lc-update-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn updater(server: &Server, state: &Path, current: &str, installs: bool) -> (Updater, Fakes) {
        let f = Fakes { notified: Arc::default(), launched: Arc::default(), idle: Arc::new(AtomicBool::new(true)), exit: Arc::default(), logs: Arc::default() };
        let (idle, logs) = (f.idle.clone(), f.logs.clone());
        let u = Updater::build(
            Source::local(&server.base),
            state,
            SystemTime::UNIX_EPOCH,
            v(current),
            installs,
            Box::new(move || idle.load(Ordering::SeqCst)),
            Box::new(FakeNotifier(f.notified.clone())),
            Box::new(FakeLauncher { launched: f.launched.clone(), idle: f.idle.clone(), exit: f.exit.clone() }),
            Box::new(move |m| logs.lock().unwrap().push(m.to_string())),
        );
        (u, f)
    }

    const LATEST: &str = "/repos/tinytom21/lightcraft-cloud/releases/latest";
    const ON: Options = Options { enabled: true, notify: true };

    fn later(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    /// A release `tag` whose installer is `bytes`, listed with `sums_hash` (the right one when None).
    fn publish(s: &Server, tag: &str, bytes: &[u8], sums_hash: Option<String>) {
        let ver = tag.trim_start_matches('v').to_string();
        let name = format!("lightcraft-cloud-{ver}-windows-setup.exe");
        let base = format!("{}/", s.base);
        let json = release_json(tag, false, false, &base, true);
        s.route(LATEST, move || reply(200, json.clone()));
        let hash = sums_hash.unwrap_or_else(|| format!("{:x}", Sha256::digest(bytes)));
        let sums = format!("{hash}  {name}\n");
        s.route(&format!("/{REPO}/releases/download/{tag}/SHA256SUMS"), move || reply(200, sums.clone()));
        let body = bytes.to_vec();
        s.route(&format!("/{REPO}/releases/download/{tag}/{name}"), move || reply(200, body.clone()));
    }

    // ---- the check

    #[test]
    fn a_newer_release_is_offered_after_the_first_delay_with_a_plain_user_agent_and_nothing_else_sent() {
        let (server, dir) = (Server::start(), temp("offer"));
        publish(&server, "v0.3.0", b"setup", None);
        let (mut u, f) = updater(&server, &dir, "0.2.0", true);
        assert!(!u.tick(later(10), ON), "not yet: the first check comes shortly after the start");
        assert_eq!(server.total(), 0);
        assert!(u.tick(later(100), ON));
        assert_eq!(server.total(), 1);
        let head = server.seen.lock().unwrap()[0].1.clone();
        assert!(head.starts_with(&format!("GET {LATEST} HTTP/1.1")), "{head}");
        let lower = head.to_lowercase();
        assert!(lower.contains("user-agent: lightcraft-cloud-desktop/"), "{head}");
        // only the headers a plain request has: no cookie, no authorization, no identifier
        for h in ["cookie", "authorization", "x-"] {
            assert!(!lower.contains(&format!("\n{h}")), "{head}");
        }
        let offer = u.state().latest.clone().unwrap();
        assert_eq!((offer.version.as_str(), offer.can_install), ("0.3.0", true));
        assert_eq!(offer.page, "https://github.com/tinytom21/lightcraft-cloud/releases/tag/v0.3.0");
        assert_eq!(f.notified.lock().unwrap().len(), 1);
        // a later tick before the next day asks nothing more
        u.tick(later(3600), ON);
        assert_eq!(server.total(), 1);
        // the file is what the Status window reads
        let shown = UpdateState::load(&dir).view(&v("0.2.0")).unwrap();
        assert_eq!((shown.version.as_str(), shown.phase), ("0.3.0", Phase::Idle));
        assert!(UpdateState::load(&dir).view(&v("0.3.0")).is_none(), "not newer than itself");
        assert!(UpdateState::load(&dir).view(&v("0.4.0")).is_none());
        // a day later it looks again
        u.tick(later(100 + 24 * 3600), ON);
        assert_eq!(server.total(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_same_or_an_older_release_is_not_an_update() {
        let (server, dir) = (Server::start(), temp("same"));
        publish(&server, "v0.2.0", b"x", None);
        let (mut u, f) = updater(&server, &dir, "0.2.0", true);
        u.tick(later(100), ON);
        assert_eq!(server.count(LATEST), 1);
        assert!(u.state().latest.is_none() && u.state().checked.is_some());
        assert!(f.notified.lock().unwrap().is_empty());
        let (mut u, _) = updater(&server, &dir, "0.3.0-rc.1", true);
        u.tick(later(100), ON);
        assert!(u.state().latest.is_none(), "0.2.0 is older than 0.3.0-rc.1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_draft_or_a_prerelease_from_the_server_is_never_offered() {
        let (server, dir) = (Server::start(), temp("draft"));
        for (draft, pre, tag) in [(true, false, "v0.9.0"), (false, true, "v0.9.0"), (false, false, "v0.9.0-rc.1")] {
            let json = release_json(tag, draft, pre, "", true);
            server.route(LATEST, move || reply(200, json.clone()));
            let (mut u, f) = updater(&server, &dir, "0.2.0", true);
            u.tick(later(100), ON);
            assert!(u.state().latest.is_none(), "{tag} draft={draft} pre={pre}");
            assert!(f.notified.lock().unwrap().is_empty());
            assert!(f.logs.lock().unwrap().iter().any(|l| l.starts_with("update check:")));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_check_is_silent_and_never_touches_the_sync_status() {
        let (server, dir) = (Server::start(), temp("fail"));
        let status = crate::appstate::AppStatus { configured: true, ..Default::default() };
        status.save(&dir).unwrap();
        let before = std::fs::read(dir.join("status.json")).unwrap();
        for answer in [reply(500, "oops"), reply(404, "no"), reply(200, "{ this is not json"), reply(200, "")] {
            let answer = Arc::new(Mutex::new(Some(answer)));
            server.route(LATEST, move || {
                let a = answer.lock().unwrap().take();
                a.unwrap_or_else(|| reply(500, "again"))
            });
            let (mut u, f) = updater(&server, &dir, "0.2.0", true);
            u.tick(later(100), ON);
            assert!(u.state().latest.is_none() && u.state().checked.is_none());
            assert!(f.notified.lock().unwrap().is_empty(), "no notification for a failed check");
            assert!(f.logs.lock().unwrap().iter().any(|l| l.starts_with("update check:")), "one line in the log");
            assert_eq!(u.state().phase, Phase::Idle);
        }
        // a server that is not there at all
        let dead = { let l = TcpListener::bind("127.0.0.1:0").unwrap(); format!("http://{}", l.local_addr().unwrap()) };
        let gone = Server { base: dead, routes: Arc::default(), seen: Arc::default(), stop: Arc::default() };
        let (mut u, f) = updater(&gone, &dir, "0.2.0", true);
        u.tick(later(100), ON);
        assert!(u.state().latest.is_none());
        assert!(f.logs.lock().unwrap().iter().any(|l| l.starts_with("update check:")));
        // a failed check is tried again within the hour, not in a day
        let before_retry = server.total();
        u.tick(later(100 + RETRY_AFTER.as_secs() + 1), ON);
        assert!(f.logs.lock().unwrap().len() >= 2);
        let _ = before_retry;
        // nothing about the sync changed
        assert_eq!(std::fs::read(dir.join("status.json")).unwrap(), before);
        assert_eq!(crate::appstate::AppStatus::load(&dir).unwrap().health().text, status.health().text);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nothing_is_asked_or_shown_while_updates_are_off() {
        let (server, dir) = (Server::start(), temp("off"));
        publish(&server, "v0.3.0", b"x", None);
        let (mut u, f) = updater(&server, &dir, "0.2.0", true);
        let off = Options { enabled: false, notify: true };
        u.tick(later(100), off);
        u.tick(later(100 + 48 * 3600), off);
        assert_eq!(server.total(), 0, "no request at all");
        assert!(u.state().latest.is_none() && f.notified.lock().unwrap().is_empty());
        // switched on: it asks; switched off again: the offer goes away at once
        u.tick(later(100), ON);
        assert!(u.state().latest.is_some());
        u.tick(later(101), off);
        assert!(u.state().latest.is_none());
        assert!(UpdateState::load(&dir).view(&v("0.2.0")).is_none());
        // a request to install while off does nothing and is not kept
        request_install(&dir, "0.3.0").unwrap();
        u.tick(later(102), off);
        assert!(!request_path(&dir).exists());
        assert!(f.launched.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_version_is_announced_once_even_across_restarts_and_a_new_one_is_announced_again() {
        let (server, dir) = (Server::start(), temp("once"));
        publish(&server, "v0.3.0", b"x", None);
        let (mut u, f) = updater(&server, &dir, "0.2.0", true);
        u.tick(later(100), ON);
        u.tick(later(100 + 24 * 3600), ON); // (the next day's check finds the same release)
        u.tick(later(100 + 48 * 3600), ON);
        assert_eq!(f.notified.lock().unwrap().len(), 1);
        assert!(f.notified.lock().unwrap()[0].contains("0.3.0"));
        // the app is restarted: still once
        let (mut u2, f2) = updater(&server, &dir, "0.2.0", true);
        u2.tick(later(100), ON);
        assert!(f2.notified.lock().unwrap().is_empty());
        assert!(u2.state().latest.is_some(), "but it is still offered");
        // 0.4.0 comes out
        publish(&server, "v0.4.0", b"y", None);
        u2.tick(later(100 + 24 * 3600), ON);
        assert_eq!(f2.notified.lock().unwrap().len(), 1);
        assert!(f2.notified.lock().unwrap()[0].contains("0.4.0"));
        // with notifications off nothing is said, and the version is not used up: switching them on says it once
        let dir2 = temp("once2");
        let (mut u3, f3) = updater(&server, &dir2, "0.2.0", true);
        u3.tick(later(100), Options { enabled: true, notify: false });
        assert!(f3.notified.lock().unwrap().is_empty() && u3.state().latest.is_some());
        u3.tick(later(100 + 24 * 3600), ON);
        assert_eq!(f3.notified.lock().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[test]
    fn on_linux_a_newer_version_is_only_shown_with_its_link_and_nothing_is_downloaded() {
        let (server, dir) = (Server::start(), temp("linux"));
        publish(&server, "v0.3.0", b"x", None);
        let (mut u, f) = updater(&server, &dir, "0.2.0", false);
        u.tick(later(100), ON);
        let offer = u.state().latest.clone().unwrap();
        assert!(!offer.can_install);
        assert!(f.notified.lock().unwrap()[0].contains("https://github.com/tinytom21/lightcraft-cloud/releases/tag/v0.3.0"));
        request_install(&dir, "0.3.0").unwrap();
        u.tick(later(101), ON);
        assert_eq!(server.count(&format!("/{REPO}/releases/download/v0.3.0/SHA256SUMS")), 0);
        assert!(f.launched.lock().unwrap().is_empty());
        assert_eq!(u.state().phase, Phase::Idle);
        // a release with no files at all is still offered there
        let json = release_json("v0.3.0", false, false, "", false);
        server.route(LATEST, move || reply(200, json.clone()));
        let (mut u, _) = updater(&server, &temp("linux2"), "0.2.0", false);
        u.tick(later(100), ON);
        assert!(u.state().latest.is_some());
        // ... and on Windows it is not, until the installer is attached
        let (mut u, _) = updater(&server, &temp("win2"), "0.2.0", true);
        u.tick(later(100), ON);
        assert!(u.state().latest.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- the update

    fn installer_name(v: &str) -> String {
        format!("lightcraft-cloud-{v}-windows-setup.exe")
    }

    #[test]
    fn a_click_downloads_checks_and_runs_the_installer_once_nothing_is_syncing() {
        let (server, dir) = (Server::start(), temp("go"));
        let bytes: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 251) as u8).collect();
        publish(&server, "v0.3.0", &bytes, None);
        let (mut u, f) = updater(&server, &dir, "0.2.0", true);
        u.tick(later(100), ON);
        f.idle.store(false, Ordering::SeqCst); // (a sync is running)
        // nothing happens without a click
        u.tick(later(101), ON);
        assert_eq!(server.count(&format!("/{REPO}/releases/download/v0.3.0/{}", installer_name("0.3.0"))), 0);
        request_install(&dir, "0.3.0").unwrap();
        u.tick(later(102), ON);
        // downloaded and checked, but it waits for the sync
        assert_eq!(u.state().phase, Phase::Waiting);
        assert_eq!(UpdateState::load(&dir).phase, Phase::Waiting);
        assert!(f.launched.lock().unwrap().is_empty());
        assert_eq!(std::fs::read(dir.join("update").join(installer_name("0.3.0"))).unwrap(), bytes);
        assert!(!dir.join("update").join(format!("{}.part", installer_name("0.3.0"))).exists());
        u.tick(later(103), ON);
        u.tick(later(104), ON);
        assert!(f.launched.lock().unwrap().is_empty(), "still syncing: still waiting");
        assert_eq!(u.state().phase, Phase::Waiting);
        // the sync ends
        f.idle.store(true, Ordering::SeqCst);
        u.tick(later(105), ON);
        let launched = f.launched.lock().unwrap().clone();
        assert_eq!(launched.len(), 1);
        assert!(launched[0].0.ends_with(installer_name("0.3.0")));
        assert!(launched[0].1, "it was run while nothing was syncing");
        assert_eq!(u.state().phase, Phase::Idle, "(the real installer ends the app; the fake returns 0)");
        // a second request for the same version while it is not offered again does nothing harmful
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn if_nothing_is_syncing_the_installer_runs_right_after_the_check_of_the_download() {
        let (server, dir) = (Server::start(), temp("now"));
        publish(&server, "v0.3.0", b"installer bytes", None);
        let (mut u, f) = updater(&server, &dir, "0.2.0", true);
        u.tick(later(100), ON);
        request_install(&dir, "0.3.0").unwrap();
        u.tick(later(101), ON);
        assert_eq!(f.launched.lock().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_request_for_another_version_than_the_one_offered_is_ignored() {
        let (server, dir) = (Server::start(), temp("other"));
        publish(&server, "v0.3.0", b"x", None);
        let (mut u, f) = updater(&server, &dir, "0.2.0", true);
        u.tick(later(100), ON);
        for ask in ["0.4.0", "0.2.0", "", "../../evil"] {
            request_install(&dir, ask).unwrap();
            u.tick(later(101), ON);
            assert!(f.launched.lock().unwrap().is_empty(), "{ask:?}");
            assert_eq!(u.state().phase, Phase::Idle);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_download_that_does_not_match_its_checksum_is_refused_and_deleted() {
        let (server, dir) = (Server::start(), temp("hash"));
        publish(&server, "v0.3.0", b"the real installer", Some("0".repeat(64)));
        let (mut u, f) = updater(&server, &dir, "0.2.0", true);
        u.tick(later(100), ON);
        request_install(&dir, "0.3.0").unwrap();
        u.tick(later(101), ON);
        assert!(f.launched.lock().unwrap().is_empty(), "never run");
        match &u.state().phase {
            Phase::Failed { message } => assert!(message.contains("did not match its checksum") && message.contains("keeps working"), "{message}"),
            p => panic!("{p:?}"),
        }
        let left: Vec<_> = std::fs::read_dir(dir.join("update")).map(|d| d.flatten().map(|e| e.file_name()).collect()).unwrap_or_default();
        assert!(left.is_empty(), "nothing of the bad file is kept: {left:?}");
        // the app goes on: it can try again, and the offer is still there
        assert!(u.state().latest.is_some());
        publish(&server, "v0.3.0", b"the real installer", None);
        request_install(&dir, "0.3.0").unwrap();
        u.tick(later(102), ON);
        assert_eq!(f.launched.lock().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_installer_with_no_listed_checksum_is_refused() {
        let (server, dir) = (Server::start(), temp("nosum"));
        publish(&server, "v0.3.0", b"x", None);
        server.route(&format!("/{REPO}/releases/download/v0.3.0/SHA256SUMS"), || reply(200, format!("{}  something-else.exe\n", "a".repeat(64))));
        let (mut u, f) = updater(&server, &dir, "0.2.0", true);
        u.tick(later(100), ON);
        request_install(&dir, "0.3.0").unwrap();
        u.tick(later(101), ON);
        assert!(f.launched.lock().unwrap().is_empty());
        assert!(matches!(&u.state().phase, Phase::Failed { message } if message.contains("no checksum")));
        assert_eq!(server.count(&format!("/{REPO}/releases/download/v0.3.0/{}", installer_name("0.3.0"))), 0, "the installer is not even fetched");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_that_is_not_under_the_releases_address_is_refused_before_anything_is_fetched() {
        let (server, dir) = (Server::start(), temp("url"));
        let name = installer_name("0.3.0");
        let good_sums = format!("{:x}  {name}\n", Sha256::digest(b"x"));
        server.route(&format!("/{REPO}/releases/download/v0.3.0/SHA256SUMS"), move || reply(200, good_sums.clone()));
        server.route("/elsewhere/setup.exe", || reply(200, "x"));
        for url in [
            format!("{}/elsewhere/setup.exe", server.base),
            format!("{}/{REPO}/releases/download/v0.3.0/../../../elsewhere/setup.exe", server.base),
            format!("{}/{REPO}/releases/download/v0.2.0/{name}", server.base),
            "http://127.0.0.1:1/tinytom21/lightcraft-cloud/releases/download/v0.3.0/x.exe".to_string(),
            "https://evil.example/tinytom21/lightcraft-cloud/releases/download/v0.3.0/x.exe".to_string(),
        ] {
            let json = format!(
                r#"{{"tag_name":"v0.3.0","assets":[{{"name":"{name}","browser_download_url":"{url}"}},{{"name":"SHA256SUMS","browser_download_url":"{}/{REPO}/releases/download/v0.3.0/SHA256SUMS"}}]}}"#,
                server.base
            );
            server.route(LATEST, move || reply(200, json.clone()));
            let (mut u, f) = updater(&server, &dir, "0.2.0", true);
            u.tick(later(100), ON);
            request_install(&dir, "0.3.0").unwrap();
            u.tick(later(101), ON);
            assert!(f.launched.lock().unwrap().is_empty(), "{url}");
            match &u.state().phase {
                Phase::Failed { message } => assert!(message.contains("not one this app trusts") && message.contains("Nothing was changed"), "{message}"),
                p => panic!("{url}: {p:?}"),
            }
            assert!(u.state().latest.is_some(), "the running app goes on");
        }
        assert_eq!(server.count("/elsewhere/setup.exe"), 0, "never requested");
        assert_eq!(server.count(&format!("/{REPO}/releases/download/v0.3.0/SHA256SUMS")), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_redirect_is_followed_only_to_a_host_the_source_allows() {
        let (server, dir) = (Server::start(), temp("redir"));
        let bytes = b"installer behind a redirect".to_vec();
        publish(&server, "v0.3.0", &bytes, None);
        let name = installer_name("0.3.0");
        let path = format!("/{REPO}/releases/download/v0.3.0/{name}");
        let target = format!("{}/cdn/blob", server.base);
        server.route(&path, move || Reply { status: 302, headers: vec![("Location".into(), target.clone())], body: vec![] });
        let b = bytes.clone();
        server.route("/cdn/blob", move || reply(200, b.clone()));
        let (mut u, f) = updater(&server, &dir, "0.2.0", true);
        u.tick(later(100), ON);
        request_install(&dir, "0.3.0").unwrap();
        u.tick(later(101), ON);
        assert_eq!(f.launched.lock().unwrap().len(), 1, "GitHub's own redirect target is fine: {:?}", u.state().phase);
        // to another host (here: the same machine under another name; in production anything but GitHub's file hosts) it is not
        for to in ["http://localhost:1/blob", "https://evil.example/blob", "file:///etc/passwd", "/relative/blob", "http://user@127.0.0.1/blob"] {
            let to = to.to_string();
            server.route(&path, move || Reply { status: 302, headers: vec![("Location".into(), to.clone())], body: vec![] });
            let dir2 = temp("redir2");
            let (mut u, f) = updater(&server, &dir2, "0.2.0", true);
            u.tick(later(100), ON);
            request_install(&dir2, "0.3.0").unwrap();
            u.tick(later(101), ON);
            assert!(f.launched.lock().unwrap().is_empty());
            assert!(matches!(&u.state().phase, Phase::Failed { message } if message.contains("not allowed")), "{:?}", u.state().phase);
            let _ = std::fs::remove_dir_all(&dir2);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failing_installer_leaves_the_app_as_it_was_and_says_so() {
        let (server, dir) = (Server::start(), temp("bad-setup"));
        publish(&server, "v0.3.0", b"x", None);
        let (mut u, f) = updater(&server, &dir, "0.2.0", true);
        f.exit.store(5, Ordering::SeqCst);
        u.tick(later(100), ON);
        request_install(&dir, "0.3.0").unwrap();
        u.tick(later(101), ON);
        match &u.state().phase {
            Phase::Failed { message } => assert!(message.contains("code 5") && message.contains("keeps working"), "{message}"),
            p => panic!("{p:?}"),
        }
        assert!(u.state().latest.is_some(), "still offered: the user can try again");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_download_the_server_refuses_fails_and_leaves_no_file() {
        let (server, dir) = (Server::start(), temp("cut"));
        publish(&server, "v0.3.0", b"x", None);
        // the server fails in the middle of the job
        let name = installer_name("0.3.0");
        let path = format!("/{REPO}/releases/download/v0.3.0/{name}");
        server.route(&path, || reply(500, "broken"));
        let (mut u, f) = updater(&server, &dir, "0.2.0", true);
        u.tick(later(100), ON);
        request_install(&dir, "0.3.0").unwrap();
        u.tick(later(101), ON);
        assert!(f.launched.lock().unwrap().is_empty());
        assert!(matches!(&u.state().phase, Phase::Failed { message } if message.contains("Could not download")));
        assert!(std::fs::read_dir(dir.join("update")).map(|d| d.count()).unwrap_or(0) == 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn leftover_installers_are_removed_at_the_start_once_they_are_not_newer() {
        let dir = temp("leftover");
        let up = update_dir(&dir);
        std::fs::create_dir_all(&up).unwrap();
        for n in ["lightcraft-cloud-0.2.0-windows-setup.exe", "lightcraft-cloud-0.1.0-windows-setup.exe", "lightcraft-cloud-0.3.0-windows-setup.exe.part", "setup.log", "SHA256SUMS"] {
            std::fs::write(up.join(n), "x").unwrap();
        }
        std::fs::write(up.join("lightcraft-cloud-0.3.0-windows-setup.exe"), "x").unwrap(); // (a newer one, e.g. a failed install: kept for now)
        let server = Server::start();
        let (_u, _) = updater(&server, &dir, "0.2.0", true);
        let mut left: Vec<String> = std::fs::read_dir(&up).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, vec!["lightcraft-cloud-0.3.0-windows-setup.exe"]);
        let (_u, _) = updater(&server, &dir, "0.3.0", true);
        assert!(!up.exists(), "after the update the folder is gone");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- the source cannot be changed from outside

    #[test]
    fn github_is_the_only_source_and_it_is_fixed_in_the_program() {
        let s = Source::github();
        assert_eq!(s.latest, "https://api.github.com/repos/tinytom21/lightcraft-cloud/releases/latest");
        assert_eq!(s.download_prefix, "https://github.com/tinytom21/lightcraft-cloud/releases/download/");
        assert!(!s.insecure);
        assert!(s.redirect_ok("https://objects.githubusercontent.com/github-production-release-asset/1/2?x=y"));
        assert!(s.redirect_ok("https://release-assets.githubusercontent.com/a"));
        for bad in ["http://objects.githubusercontent.com/a", "https://github.com.evil.example/a", "https://evil.example/objects.githubusercontent.com", "https://objects.githubusercontent.com@evil.example/a", "https://githubusercontent.com/a", "ftp://objects.githubusercontent.com/a", "//objects.githubusercontent.com/a"] {
            assert!(!s.redirect_ok(bad), "{bad}");
        }
        assert_eq!(s.asset_url("v1.0.0", "SHA256SUMS"), "https://github.com/tinytom21/lightcraft-cloud/releases/download/v1.0.0/SHA256SUMS");
    }

    #[test]
    fn no_environment_variable_or_config_key_can_change_where_updates_come_from() {
        // 1. the program text above the tests reads no environment variable and no file but its own state (the build-time version is not a run-time input)
        let text = include_str!("update.rs");
        let production = text.split("#[cfg(test)]\nmod tests").next().unwrap();
        for forbidden in ["std::env::var", "env::var(", "env::args", "var_os", "option_env!", "dotenv"] {
            assert!(!production.contains(forbidden), "update.rs must not read {forbidden}");
        }
        // 2. the only constructors of a Source: `github()` (no inputs) and the test-only `local`
        let makers = production.matches("fn github(").count();
        assert_eq!(makers, 1);
        assert!(production.contains("#[cfg(test)]\n    fn local("), "the local source exists only in test builds");
        assert!(production.contains("fn build(") && !production.contains("pub fn build("), "building an Updater with another source is private");
        // 3. the config has no key for it, and whatever a config file says about updates, the updater's source is the same
        let cfg = include_str!("config.rs");
        let keys = cfg.split("const KNOWN_KEYS").nth(1).unwrap().split("];").next().unwrap();
        for k in ["update_url", "update_source", "update_api", "updates_url", "release_url", "update_server", "update_base"] {
            assert!(!keys.contains(k), "{k}");
        }
        let f: crate::config::File = toml::from_str("update_url = \"http://evil.example\"\nupdate_source = \"x\"\ncheck_for_updates = true\n").unwrap();
        assert!(f.check_for_updates);
        let dir = temp("env");
        let u = Updater::new(&dir, SystemTime::UNIX_EPOCH, Box::new(|| true), Box::new(FakeNotifier(Arc::default())), Box::new(|_| {}));
        assert_eq!(u.source.latest, API_LATEST);
        assert_eq!(u.source.download_prefix, DOWNLOAD_PREFIX);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
