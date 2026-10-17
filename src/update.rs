//! Signed auto-updates.
//!
//! A release publishes `latest.txt`, a manifest signed with the project's Ed25519 key:
//!
//! ```text
//! version=0.3.0
//! windows-x86_64.url=https://.../Bare-0.3.0-windows-x86_64.zip
//! windows-x86_64.sha256=<hex>
//! windows-x86_64.signature=<hex>   (signs "bare-update\n{version}\n{platform}\n{url}\n{sha256}")
//! ```
//!
//! Bare only installs a package whose signature verifies against the public key built into
//! the app, whose SHA-256 matches, and whose version is newer than its own. The package is
//! unpacked into a staging folder and copied over the app after Bare exits.

use std::{
    cell::RefCell,
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    rc::Rc,
    sync::OnceLock,
    time::{SystemTime, UNIX_EPOCH},
};

use cef::*;
use ed25519_dalek::{Signature, VerifyingKey};
use sha2::{Digest, Sha256};

use crate::{settings, ui};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Where `latest.txt` is published. Set at build time: `BARE_UPDATE_URL=https://... cargo xtask package`.
const MANIFEST_URL: Option<&str> = option_env!("BARE_UPDATE_URL");
const PUBLIC_KEY_HEX: &str = include_str!("../data/update-key.pub");
const MAX_MANIFEST: usize = 64 * 1024;
const MAX_PACKAGE: i64 = 2 * 1024 * 1024 * 1024;
const CHECK_EVERY_SECS: u64 = 24 * 60 * 60;
/// The installer's AppId (installer/bare.iss); never changes.
const INSTALLER_APP_ID: &str = "B6FF0361-A243-4B32-97EE-94D5CE3B8EF0";

#[derive(Clone, PartialEq, Debug)]
pub enum Status {
    Disabled(&'static str),
    Idle,
    Checking,
    UpToDate,
    Downloading { version: String, percent: i32 },
    Ready { version: String },
    Failed(String),
}

impl Status {
    pub fn describe(&self) -> String {
        match self {
            Status::Disabled(why) => (*why).into(),
            Status::Idle => "Not checked yet this session.".into(),
            Status::Checking => "Checking for updates…".into(),
            Status::UpToDate => "Bare is up to date.".into(),
            Status::Downloading { version, percent } => format!("Downloading version {version}… {percent}%"),
            Status::Ready { version } => format!("Version {version} is ready. Restart Bare to finish updating."),
            Status::Failed(e) => format!("Update failed: {e}"),
        }
    }
}

thread_local! {
    static STATUS: RefCell<Status> = const { RefCell::new(Status::Idle) };
    static ACTIVE: RefCell<Option<Urlrequest>> = const { RefCell::new(None) };
}

static PROFILE: OnceLock<PathBuf> = OnceLock::new();
static RELAUNCH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn status() -> Status {
    STATUS.with(|s| s.borrow().clone())
}

fn set_status(s: Status) {
    STATUS.with(|cell| *cell.borrow_mut() = s);
    ui::with_ui(|ui| ui.on_update_status());
}

pub fn platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

fn update_dir() -> Option<PathBuf> {
    PROFILE.get().map(|p| p.join("update"))
}

fn app_dir() -> Option<PathBuf> {
    std::env::current_exe().ok()?.parent().map(Path::to_path_buf)
}

/// Called at startup: remembers the profile folder and picks up an update staged earlier.
pub fn init(profile: &Path) {
    let _ = PROFILE.set(profile.to_path_buf());
    if let Some(reason) = disabled_reason() {
        STATUS.with(|s| *s.borrow_mut() = Status::Disabled(reason));
        return;
    }
    let staged = update_dir().and_then(|d| fs::read_to_string(d.join("staging-version.txt")).ok());
    match staged {
        Some(v) if newer(v.trim(), VERSION) => STATUS.with(|s| *s.borrow_mut() = Status::Ready { version: v.trim().into() }),
        Some(_) => clear_staging(),
        None => {}
    }
}

fn disabled_reason() -> Option<&'static str> {
    if MANIFEST_URL.is_none_or(str::is_empty) {
        return Some("Updates aren't configured for this build (no update URL).");
    }
    if PUBLIC_KEY_HEX.trim().is_empty() {
        return Some("Updates aren't configured for this build (no signing key).");
    }
    if !app_dir().is_some_and(|d| writable(&d)) {
        return Some("Bare can't update itself because its folder isn't writable. Download new versions manually.");
    }
    None
}

fn writable(dir: &Path) -> bool {
    let probe = dir.join(".bare-write-test");
    let ok = File::create(&probe).is_ok();
    let _ = fs::remove_file(probe);
    ok
}

/// Daily automatic check, if enabled.
pub fn maybe_check_automatically() {
    if !settings::get().auto_update || status() != Status::Idle {
        return;
    }
    let stamp = update_dir().map(|d| d.join("last-check.txt"));
    let last: u64 = stamp.as_ref().and_then(|p| fs::read_to_string(p).ok()).and_then(|t| t.trim().parse().ok()).unwrap_or(0);
    if now() >= last + CHECK_EVERY_SECS {
        check();
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Fetches the manifest and, if a newer verified release exists, downloads and stages it.
pub fn check() {
    if matches!(status(), Status::Disabled(_) | Status::Checking | Status::Downloading { .. } | Status::Ready { .. }) {
        return;
    }
    if let Some(dir) = update_dir() {
        let _ = fs::create_dir_all(&dir);
        let _ = fs::write(dir.join("last-check.txt"), now().to_string());
    }
    set_status(Status::Checking);
    let url = MANIFEST_URL.unwrap_or_default().to_string();
    fetch(&url, Sink::Memory(Vec::new()), |result| match result {
        Ok(Sink::Memory(bytes)) => on_manifest(&bytes),
        Ok(_) => {}
        Err(e) => set_status(Status::Failed(e)),
    });
}

fn on_manifest(bytes: &[u8]) {
    let text = String::from_utf8_lossy(bytes);
    let release = match parse_manifest(&text, &platform(), &public_key()) {
        Ok(r) => r,
        Err(e) => return set_status(Status::Failed(e)),
    };
    if !newer(&release.version, VERSION) {
        return set_status(Status::UpToDate);
    }
    let Some(dir) = update_dir() else { return };
    let file_path = dir.join(format!("Bare-{}.pkg", release.version));
    let file = match File::create(&file_path) {
        Ok(f) => f,
        Err(e) => return set_status(Status::Failed(format!("can't write the download: {e}"))),
    };
    set_status(Status::Downloading { version: release.version.clone(), percent: 0 });
    let url = release.url.clone();
    fetch(&url, Sink::File(file, Sha256::new(), 0), move |result| {
        let digest = match result {
            Ok(Sink::File(_, hasher, _)) => hasher.finalize(),
            Ok(_) => return,
            Err(e) => {
                let _ = fs::remove_file(&file_path);
                return set_status(Status::Failed(e));
            }
        };
        if digest.as_slice() != release.sha256 {
            let _ = fs::remove_file(&file_path);
            return set_status(Status::Failed("the download doesn't match the signed checksum".into()));
        }
        match stage(&file_path, &dir, &release.version) {
            Ok(()) => set_status(Status::Ready { version: release.version.clone() }),
            Err(e) => set_status(Status::Failed(e)),
        }
        let _ = fs::remove_file(&file_path);
    });
}

/// Unpacks the verified package into `<profile>/update/staging`.
fn stage(package: &Path, dir: &Path, version: &str) -> Result<(), String> {
    clear_staging();
    let staging = dir.join("staging");
    fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
    // Windows 10+ and macOS ship bsdtar, which reads .zip; Linux packages are .tar.gz.
    let ok = Command::new("tar")
        .arg("-xf")
        .arg(package)
        .arg("-C")
        .arg(&staging)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let exe = if cfg!(windows) { "Bare.exe" } else { "bare" };
    if !ok || !staging.join(exe).exists() {
        clear_staging();
        return Err("couldn't unpack the update".into());
    }
    fs::write(dir.join("staging-version.txt"), version).map_err(|e| e.to_string())
}

fn clear_staging() {
    if let Some(dir) = update_dir() {
        let _ = fs::remove_dir_all(dir.join("staging"));
        let _ = fs::remove_file(dir.join("staging-version.txt"));
    }
}

/// Asks for the update to be installed and Bare restarted once the window closes.
pub fn request_restart() {
    RELAUNCH.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Called after CEF has shut down: if an update is staged, starts a helper that waits for
/// Bare to exit, copies the new files over the app, and optionally relaunches it.
pub fn apply_if_staged() {
    let (Some(dir), Some(app)) = (update_dir(), app_dir()) else { return };
    let staging = dir.join("staging");
    if !matches!(status(), Status::Ready { .. }) || !staging.exists() {
        return;
    }
    let relaunch = RELAUNCH.load(std::sync::atomic::Ordering::Relaxed);
    let pid = std::process::id();
    let marker = dir.join("staging-version.txt");
    if cfg!(windows) {
        let script = dir.join("apply-update.cmd");
        let log = dir.join("update.log");
        // Waits for Bare to exit, copies the update (robocopy retries while Chromium's child
        // processes release their files), and keeps the staged copy if anything failed.
        let text = format!(
            "@echo off\r\n\
             echo %date% %time% applying update >> \"{l}\"\r\n\
             powershell -NoProfile -NonInteractive -Command \"Wait-Process -Id {pid} -ErrorAction SilentlyContinue\"\r\n\
             robocopy \"{s}\" \"{a}\" /E /IS /IT /R:60 /W:1 /NP /NFL /NDL >> \"{l}\"\r\n\
             if %errorlevel% geq 8 (echo copy failed: %errorlevel% >> \"{l}\") else (rmdir /S /Q \"{s}\" & del \"{m}\" & echo done >> \"{l}\")\r\n\
             reg query \"{k}\" >nul 2>&1 && reg add \"{k}\" /v DisplayVersion /d \"{v}\" /f >nul 2>&1\r\n\
             {relaunch}del \"%~f0\"\r\n",
            s = staging.display(),
            a = app.display(),
            l = log.display(),
            m = marker.display(),
            // Installed with BareSetup.exe: keep "Settings -> Apps" showing the right version.
            k = format!(r"HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\{{{INSTALLER_APP_ID}}}_is1"),
            v = match status() {
                Status::Ready { version } => version,
                _ => String::new(),
            },
            relaunch = if relaunch { format!("start \"\" \"{}\"\r\n", app.join("Bare.exe").display()) } else { String::new() },
        );
        if fs::write(&script, text).is_ok() {
            spawn_detached(Command::new("cmd").arg("/c").arg(&script));
        }
    } else {
        let script = dir.join("apply-update.sh");
        let text = format!(
            "#!/bin/sh\nwhile kill -0 {pid} 2>/dev/null; do sleep 1; done\ncp -a '{s}/.' '{a}/' && rm -rf '{s}' '{m}'\n{relaunch}rm -f \"$0\"\n",
            s = staging.display(),
            a = app.display(),
            m = marker.display(),
            relaunch = if relaunch { format!("'{}' &\n", app.join("bare").display()) } else { String::new() },
        );
        if fs::write(&script, text).is_ok() {
            spawn_detached(Command::new("sh").arg(&script));
        }
    }
}

fn spawn_detached(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // A hidden console (not DETACHED_PROCESS, which leaves console tools without stdio).
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let _ = cmd.spawn();
}

// ---- manifest ----

#[derive(Debug, PartialEq)]
pub struct Release {
    pub version: String,
    pub url: String,
    pub sha256: [u8; 32],
}

fn public_key() -> Option<VerifyingKey> {
    let bytes: [u8; 32] = from_hex(PUBLIC_KEY_HEX.trim())?.try_into().ok()?;
    VerifyingKey::from_bytes(&bytes).ok()
}

/// The exact bytes the release signature covers.
pub fn signed_message(version: &str, platform: &str, url: &str, sha256_hex: &str) -> String {
    format!("bare-update\n{version}\n{platform}\n{url}\n{sha256_hex}")
}

pub fn parse_manifest(text: &str, platform: &str, key: &Option<VerifyingKey>) -> Result<Release, String> {
    let key = key.as_ref().ok_or("no valid signing key in this build")?;
    let get = |k: &str| {
        text.lines()
            .filter_map(|l| l.split_once('='))
            .find(|(name, _)| name.trim() == k)
            .map(|(_, v)| v.trim().to_string())
    };
    let missing = |what: &str| format!("the update manifest has no {what} for {platform}");
    let version = get("version").ok_or("the update manifest has no version")?;
    let url = get(&format!("{platform}.url")).ok_or_else(|| missing("download"))?;
    let sha_hex = get(&format!("{platform}.sha256")).ok_or_else(|| missing("checksum"))?.to_ascii_lowercase();
    let sig_hex = get(&format!("{platform}.signature")).ok_or_else(|| missing("signature"))?;
    // Test builds (BARE_UPDATE_ALLOW_LOCALHOST set at compile time) may use a local HTTP server.
    let local_test = option_env!("BARE_UPDATE_ALLOW_LOCALHOST").is_some() && url.starts_with("http://127.0.0.1:");
    if !url.starts_with("https://") && !local_test {
        return Err("the update download isn't HTTPS".into());
    }
    let sha256: [u8; 32] = from_hex(&sha_hex).and_then(|b| b.try_into().ok()).ok_or("bad checksum in manifest")?;
    let sig: [u8; 64] = from_hex(&sig_hex).and_then(|b| b.try_into().ok()).ok_or("bad signature in manifest")?;
    key.verify_strict(signed_message(&version, platform, &url, &sha_hex).as_bytes(), &Signature::from_bytes(&sig))
        .map_err(|_| "the update's signature is invalid".to_string())?;
    Ok(Release { version, url, sha256 })
}

/// Is version `a` newer than `b`? Compares dot-separated numbers ("0.10.0" > "0.9.3").
pub fn newer(a: &str, b: &str) -> bool {
    let parse = |v: &str| v.split('.').map(|p| p.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>();
    let (a, b) = (parse(a), parse(b));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x > y;
        }
    }
    false
}

pub fn from_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

// ---- downloading through Chromium's network stack ----

enum Sink {
    Memory(Vec<u8>),
    File(File, Sha256, i64),
}

type Done = Box<dyn FnOnce(Result<Sink, String>)>;

struct Fetch {
    sink: Option<Sink>,
    error: Option<String>,
    done: Option<Done>,
}

fn fetch(url: &str, sink: Sink, done: impl FnOnce(Result<Sink, String>) + 'static) {
    let Some(request) = request_create() else { return set_status(Status::Failed("network unavailable".into())) };
    request.set_url(Some(&CefString::from(url)));
    let state = Rc::new(RefCell::new(Fetch { sink: Some(sink), error: None, done: Some(Box::new(done)) }));
    let mut client = Fetcher::new(state);
    let mut request = request;
    let handle = urlrequest_create(Some(&mut request), Some(&mut client), None);
    ACTIVE.with(|a| *a.borrow_mut() = handle);
}

wrap_urlrequest_client! {
    struct Fetcher {
        state: Rc<RefCell<Fetch>>,
    }

    impl UrlrequestClient {
        fn on_download_data(&self, _request: Option<&mut Urlrequest>, data: *const u8, data_length: usize) {
            if data.is_null() || data_length == 0 {
                return;
            }
            let chunk = unsafe { std::slice::from_raw_parts(data, data_length) };
            let mut st = self.state.borrow_mut();
            let err = match st.sink.as_mut() {
                Some(Sink::Memory(buf)) if buf.len() + chunk.len() > MAX_MANIFEST => Some("the update manifest is too large"),
                Some(Sink::Memory(buf)) => {
                    buf.extend_from_slice(chunk);
                    None
                }
                Some(Sink::File(_, _, total)) if *total + chunk.len() as i64 > MAX_PACKAGE => Some("the update is too large"),
                Some(Sink::File(file, hasher, total)) => {
                    *total += chunk.len() as i64;
                    hasher.update(chunk);
                    file.write_all(chunk).err().map(|_| "couldn't save the update")
                }
                None => None,
            };
            if let Some(e) = err {
                st.error.get_or_insert(e.to_string());
                st.sink = None;
                ACTIVE.with(|a| if let Some(r) = a.borrow().as_ref() { r.cancel() });
            }
        }

        fn on_download_progress(&self, _request: Option<&mut Urlrequest>, current: i64, total: i64) {
            if total > 0 {
                if let Status::Downloading { version, .. } = status() {
                    let percent = (current * 100 / total) as i32;
                    STATUS.with(|s| *s.borrow_mut() = Status::Downloading { version, percent });
                    ui::with_ui(|ui| ui.on_update_status());
                }
            }
        }

        fn on_request_complete(&self, request: Option<&mut Urlrequest>) {
            ACTIVE.with(|a| a.borrow_mut().take());
            let (done, result) = {
                let mut st = self.state.borrow_mut();
                let done = st.done.take();
                let http = request.as_ref().and_then(|r| r.response()).map_or(0, |r| r.status());
                let ok = request.as_ref().is_some_and(|r| r.request_status() == UrlrequestStatus::SUCCESS);
                let result = match (st.error.take(), st.sink.take()) {
                    (Some(e), _) => Err(e),
                    (None, Some(sink)) if ok && http == 200 => Ok(sink),
                    _ if http != 0 && http != 200 => Err(format!("the update server answered HTTP {http}")),
                    _ => Err("couldn't reach the update server".into()),
                };
                (done, result)
            };
            if let Some(done) = done {
                done(result);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn signed(key: &SigningKey, version: &str, platform: &str, url: &str, sha: &str) -> String {
        let sig = key.sign(signed_message(version, platform, url, sha).as_bytes());
        let hex: String = sig.to_bytes().iter().map(|b| format!("{b:02x}")).collect();
        format!("version={version}\n{platform}.url={url}\n{platform}.sha256={sha}\n{platform}.signature={hex}\n")
    }

    #[test]
    fn verifies_signed_manifests() {
        let key = SigningKey::from_bytes(&[7; 32]);
        let public = Some(key.verifying_key());
        let sha = "ab".repeat(32);
        let text = signed(&key, "1.2.3", "windows-x86_64", "https://example.test/Bare.zip", &sha);
        let r = parse_manifest(&text, "windows-x86_64", &public).unwrap();
        assert_eq!(r.version, "1.2.3");
        assert_eq!(r.sha256, [0xab; 32]);

        // Tampering with anything signed is rejected.
        let tampered = text.replace("https://example.test/Bare.zip", "https://evil.test/Bare.zip");
        assert!(parse_manifest(&tampered, "windows-x86_64", &public).is_err());
        assert!(parse_manifest(&text.replace("1.2.3", "9.9.9"), "windows-x86_64", &public).is_err());
        // A different key, a missing platform, or plain HTTP are rejected.
        let other = Some(SigningKey::from_bytes(&[8; 32]).verifying_key());
        assert!(parse_manifest(&text, "windows-x86_64", &other).is_err());
        assert!(parse_manifest(&text, "linux-x86_64", &public).is_err());
        let http = signed(&key, "1.2.3", "windows-x86_64", "http://example.test/Bare.zip", &sha);
        assert!(parse_manifest(&http, "windows-x86_64", &public).is_err());
        assert!(parse_manifest(&text, "windows-x86_64", &None).is_err());
    }

    #[test]
    fn version_order() {
        assert!(newer("0.3.0", "0.2.9"));
        assert!(newer("0.10.0", "0.9.3"));
        assert!(newer("1.0", "0.99.99"));
        assert!(!newer("0.2.0", "0.2.0"));
        assert!(!newer("0.2", "0.2.0"));
        assert!(!newer("0.1.9", "0.2.0"));
    }

    #[test]
    fn hex() {
        assert_eq!(from_hex("00ff10"), Some(vec![0, 255, 16]));
        assert_eq!(from_hex("abc"), None);
        assert_eq!(from_hex("zz"), None);
    }
}
