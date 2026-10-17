//! Chromium engine updates, separate from Bare's own releases.
//!
//! The engine (CEF: libcef.dll, its resources and the sandbox launcher) can be replaced without
//! a new Bare build: Bare.dll talks to CEF through its versioned C API, and newer CEF builds
//! keep supporting older API versions. The project publishes `engines.txt`, signed with the
//! same key as Bare's updates:
//!
//! ```text
//! engine=156.0.7+g47dbc1d+chromium-156.0.8078.17
//! windows-x86_64.url=https://cef-builds.spotifycdn.com/cef_binary_…_windows64_minimal.tar.bz2
//! windows-x86_64.sha256=<hex>
//! windows-x86_64.size=<bytes>        (shown to the user; not signed)
//! windows-x86_64.signature=<hex>     (signs "bare-engine\n{engine}\n{platform}\n{url}\n{sha256}")
//! ```
//!
//! An engine is offered only when the signature verifies and it's newer than the running one.
//! It downloads straight from CEF's build server and must match the signed SHA-256. It's then
//! unpacked and laid out like a packaged Bare (bootstrap.exe becomes a branded Bare.exe) in the
//! staging folder Bare's own updates use, and copied over the app after Bare exits.

use std::{
    cell::RefCell,
    fs::{self, File},
    os::raw::c_int,
    path::Path,
    process::Command,
    sync::Mutex,
};

use cef::*;
use sha2::{Digest, Sha256};

use crate::{
    engine_files::{self, chromium, cef_number},
    ui,
    update::{self, Kind, Release, Sink},
};

/// Where `engines.txt` is published. Set at build time (see .cargo/config.toml).
const MANIFEST_URL: Option<&str> = option_env!("BARE_ENGINE_URL");

#[derive(Clone, PartialEq, Debug)]
pub enum Status {
    Disabled(&'static str),
    Idle,
    Checking,
    UpToDate,
    NonePublished,
    Available { engine: String, size: u64 },
    Downloading { engine: String, percent: i32 },
    Unpacking { engine: String },
    Ready { engine: String },
    Failed(String),
}

impl Status {
    pub fn describe(&self) -> String {
        match self {
            Status::Disabled(why) => (*why).into(),
            Status::Idle => "Not checked yet this session.".into(),
            Status::Checking => "Checking for a newer engine…".into(),
            Status::UpToDate => "This is the newest engine approved for Bare.".into(),
            Status::NonePublished => "No engine updates have been published yet.".into(),
            Status::Available { engine, size } if *size > 0 => {
                format!("Chromium {} is available ({} MB download).", chromium(engine), size / 1_000_000)
            }
            Status::Available { engine, .. } => format!("Chromium {} is available.", chromium(engine)),
            Status::Downloading { engine, percent } => format!("Downloading Chromium {}… {percent}%", chromium(engine)),
            Status::Unpacking { engine } => format!("Unpacking Chromium {}…", chromium(engine)),
            Status::Ready { engine } => format!("Chromium {} is ready. Restart Bare to switch to it.", chromium(engine)),
            Status::Failed(e) => format!("Engine update failed: {e}"),
        }
    }

    /// The button the Settings page shows: "check", "update", "restart" or none.
    pub fn action(&self) -> &'static str {
        match self {
            Status::Available { .. } => "update",
            Status::Ready { .. } => "restart",
            Status::Disabled(_) | Status::Checking | Status::Downloading { .. } | Status::Unpacking { .. } => "",
            _ => "check",
        }
    }
}

thread_local! {
    static STATUS: RefCell<Status> = const { RefCell::new(Status::Idle) };
    /// The verified engine on offer.
    static OFFER: RefCell<Option<Release>> = const { RefCell::new(None) };
}

/// Result of unpacking, handed from the worker thread back to the UI thread.
static STAGED: Mutex<Option<Result<String, String>>> = Mutex::new(None);

unsafe extern "C" {
    /// libcef's own version (cef_version_info.h): 0-2 CEF major/minor/patch, 4-7 Chromium.
    fn cef_version_info(entry: c_int) -> c_int;
}

/// The engine Bare is running on: "154.0.34+chromium-154.0.8037.98".
pub fn running() -> String {
    // SAFETY: a plain getter exported by libcef, which Bare.dll is linked against.
    let v = |entry| unsafe { cef_version_info(entry) };
    format!("{}.{}.{}+chromium-{}.{}.{}.{}", v(0), v(1), v(2), v(4), v(5), v(6), v(7))
}

pub fn status() -> Status {
    STATUS.with(|s| s.borrow().clone())
}

fn set_status(s: Status) {
    STATUS.with(|cell| *cell.borrow_mut() = s);
    ui::with_ui(|ui| ui.on_update_status());
}

pub fn is_ready() -> bool {
    matches!(status(), Status::Ready { .. })
}

/// Downloading, unpacking or waiting to be installed.
pub fn busy() -> bool {
    matches!(status(), Status::Downloading { .. } | Status::Unpacking { .. } | Status::Ready { .. })
}

fn disabled_reason() -> Option<&'static str> {
    if !cfg!(windows) {
        return Some("Engine updates are only available on Windows for now.");
    }
    if MANIFEST_URL.is_none_or(str::is_empty) {
        return Some("Engine updates aren't configured for this build.");
    }
    update::disabled_reason()
}

/// Called at startup (after `update::init`): picks up an engine staged earlier.
pub fn init() {
    if let Some(reason) = disabled_reason() {
        STATUS.with(|s| *s.borrow_mut() = Status::Disabled(reason));
        return;
    }
    let Some(dir) = update::update_dir() else { return };
    let Ok(staged) = fs::read_to_string(dir.join("staging-engine.txt")) else { return };
    let staged = staged.trim().to_string();
    if newer_than_running(&staged) && dir.join("staging").exists() {
        STATUS.with(|s| *s.borrow_mut() = Status::Ready { engine: staged });
    } else {
        // Already installed (or broken): nothing to do.
        update::clear_staging();
    }
}

fn newer_than_running(engine: &str) -> bool {
    update::newer(cef_number(engine), cef_number(&running()))
}

/// Looks for a newer approved engine. Never downloads by itself.
pub fn check() {
    if matches!(status(), Status::Disabled(_) | Status::Checking) || busy() {
        return;
    }
    set_status(Status::Checking);
    let url = MANIFEST_URL.unwrap_or_default();
    update::fetch_with_progress(url, Sink::Memory(Vec::new()), |_| {}, |result| match result {
        Ok(Sink::Memory(bytes)) => on_manifest(&String::from_utf8_lossy(&bytes)),
        Ok(_) => {}
        Err(e) if e.contains("HTTP 404") => set_status(Status::NonePublished),
        Err(e) => set_status(Status::Failed(e)),
    });
}

fn on_manifest(text: &str) {
    let platform = update::platform();
    let release = match update::parse_signed(Kind::Engine, text, &platform, &update::public_key()) {
        Ok(r) => r,
        Err(e) => return set_status(Status::Failed(e)),
    };
    if !newer_than_running(&release.version) {
        return set_status(Status::UpToDate);
    }
    let size_key = format!("{platform}.size=");
    let size = text.lines().find_map(|l| l.trim().strip_prefix(&size_key)?.parse().ok()).unwrap_or(0);
    let engine = release.version.clone();
    OFFER.with(|o| *o.borrow_mut() = Some(release));
    set_status(Status::Available { engine, size });
}

/// Downloads, verifies and stages the engine on offer.
pub fn download() {
    let Some(release) = OFFER.with(|o| o.borrow().clone()) else { return };
    if !matches!(status(), Status::Available { .. }) {
        return;
    }
    if matches!(update::status(), update::Status::Downloading { .. } | update::Status::Ready { .. }) {
        return set_status(Status::Failed("restart Bare to finish installing its update first".into()));
    }
    let Some(dir) = update::update_dir() else { return };
    let _ = fs::create_dir_all(&dir);
    let archive = dir.join("engine.tar.bz2");
    let file = match File::create(&archive) {
        Ok(f) => f,
        Err(e) => return set_status(Status::Failed(format!("can't write the download: {e}"))),
    };
    let engine = release.version.clone();
    set_status(Status::Downloading { engine: engine.clone(), percent: 0 });
    let progress = |percent| {
        if let Status::Downloading { engine, .. } = status() {
            STATUS.with(|s| *s.borrow_mut() = Status::Downloading { engine, percent });
            ui::with_ui(|ui| ui.on_update_status());
        }
    };
    update::fetch_with_progress(&release.url, Sink::File(file, Sha256::new(), 0), progress, move |result| {
        let digest = match result {
            Ok(Sink::File(file, hasher, _)) => {
                drop(file);
                hasher.finalize()
            }
            Ok(_) => return,
            Err(e) => {
                let _ = fs::remove_file(&archive);
                return set_status(Status::Failed(e));
            }
        };
        if digest.as_slice() != release.sha256 {
            let _ = fs::remove_file(&archive);
            return set_status(Status::Failed("the download doesn't match the signed checksum".into()));
        }
        // Unpacking ~165 MB of bzip2 takes a while: do it off the UI thread.
        set_status(Status::Unpacking { engine: engine.clone() });
        std::thread::spawn(move || {
            let result = stage(&archive, &dir, &engine).map(|()| engine);
            let _ = fs::remove_file(&archive);
            *STAGED.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
            post_task(ThreadId::UI, Some(&mut StagedTask::new()));
        });
    });
}

wrap_task! {
    struct StagedTask;

    impl Task {
        fn execute(&self) {
            match STAGED.lock().unwrap_or_else(|e| e.into_inner()).take() {
                Some(Ok(engine)) => set_status(Status::Ready { engine }),
                Some(Err(e)) => set_status(Status::Failed(e)),
                None => {}
            }
        }
    }
}

/// Unpacks the verified archive and lays the engine out in `<profile>/update/staging`, with
/// the list of old engine files to remove. Runs on a worker thread.
fn stage(archive: &Path, dir: &Path, engine: &str) -> Result<(), String> {
    update::clear_staging();
    let unpacked = dir.join("engine-unpacked");
    let _ = fs::remove_dir_all(&unpacked);
    fs::create_dir_all(&unpacked).map_err(|e| e.to_string())?;
    let result = (|| {
        let mut tar = Command::new("tar");
        tar.arg("-xjf").arg(archive).arg("-C").arg(&unpacked);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            tar.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        if !tar.status().is_ok_and(|s| s.success()) {
            return Err("couldn't unpack the engine".to_string());
        }
        // The archive holds one folder, cef_binary_<version>_<platform>_minimal.
        let root = fs::read_dir(&unpacked)
            .map_err(|e| e.to_string())?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .find(|p| p.is_dir())
            .ok_or("the engine archive is empty")?;
        let staging = dir.join("staging");
        let names = engine_files::assemble(&root, &staging).map_err(|e| format!("couldn't lay out the engine: {e}"))?;
        #[cfg(windows)]
        brand(&staging.join("Bare.exe"))?;
        engine_files::write_manifest(&staging, engine, &names).map_err(|e| e.to_string())?;
        let old = update::app_dir().and_then(|a| engine_files::read_manifest(&a)).map(|(_, files)| files).unwrap_or_default();
        fs::write(dir.join("staging-delete.txt"), engine_files::stale(&old, &names).join("\n")).map_err(|e| e.to_string())?;
        fs::write(dir.join("staging-engine.txt"), engine).map_err(|e| e.to_string())
    })();
    let _ = fs::remove_dir_all(&unpacked);
    if result.is_err() {
        update::clear_staging();
    }
    result
}

/// Gives the new engine's launcher Bare's icon and version, as `cargo xtask package` does.
#[cfg(windows)]
fn brand(exe: &Path) -> Result<(), String> {
    const ICO: &[u8] = include_bytes!("../assets/bare.ico");
    // Antivirus scanners briefly lock freshly written executables; retry a few times.
    let mut attempt = 0;
    loop {
        match crate::winres::brand(exe, ICO, update::VERSION) {
            Ok(()) => return Ok(()),
            Err(e) if attempt == 4 => return Err(format!("couldn't set up Bare.exe: {e}")),
            Err(_) => attempt += 1,
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses() {
        let a = Status::Available { engine: "156.0.7+g1+chromium-156.0.8078.17".into(), size: 167_000_000 };
        assert_eq!(a.describe(), "Chromium 156.0.8078.17 is available (167 MB download).");
        assert_eq!(a.action(), "update");
        assert_eq!(Status::Ready { engine: "156.0.7+g1+chromium-156.0.8078.17".into() }.action(), "restart");
        assert_eq!(Status::UpToDate.action(), "check");
        assert_eq!(Status::Unpacking { engine: "x".into() }.action(), "");
    }

    #[test]
    fn engine_manifests_are_not_app_manifests() {
        use ed25519_dalek::{Signer, SigningKey};
        let key = SigningKey::from_bytes(&[7; 32]);
        let public = Some(key.verifying_key());
        let (engine, platform, url, sha) = ("156.0.7+g1+chromium-156.0.8078.17", "windows-x86_64", "https://cef.test/e.tar.bz2", "cd".repeat(32));
        let sign = |kind| {
            let sig = key.sign(update::signed_message_for(kind, engine, platform, url, &sha).as_bytes());
            sig.to_bytes().iter().map(|b| format!("{b:02x}")).collect::<String>()
        };
        let text = |sig: &str| format!("engine={engine}\n{platform}.url={url}\n{platform}.sha256={sha}\n{platform}.size=1\n{platform}.signature={sig}\n");
        let r = update::parse_signed(Kind::Engine, &text(&sign(Kind::Engine)), platform, &public).unwrap();
        assert_eq!(r.version, engine);
        // A signature made for a Bare release doesn't validate an engine, and vice versa.
        assert!(update::parse_signed(Kind::Engine, &text(&sign(Kind::App)), platform, &public).is_err());
        let as_app = text(&sign(Kind::Engine)).replace("engine=", "version=");
        assert!(update::parse_signed(Kind::App, &as_app, platform, &public).is_err());
    }
}
