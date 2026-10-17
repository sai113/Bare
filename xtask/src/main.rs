//! Build helper.
//!
//! - `cargo xtask package`: builds Bare in release mode and assembles a runnable app folder in
//!   `dist/Bare` with the Chromium (CEF) runtime and filter lists next to it.
//! - `cargo xtask keygen`: creates the update-signing key pair (once per project).
//! - `cargo xtask release`: packages, archives and signs a release, writing `dist/latest.txt`.
//! - `cargo xtask engine <CEF version>`: downloads that Chromium engine from CEF's build server
//!   and builds `dist/engine-test` (this Bare on that engine) to try it out. With `--approve`, signs
//!   `engines.txt` so installed copies of Bare offer it in Settings.

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, ExitCode},
};

use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha256};

#[cfg(windows)]
#[path = "../../src/winres.rs"]
mod winres;

#[path = "../../src/engine_files.rs"]
mod engine_files;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const USAGE: &str = "usage: cargo xtask <package | keygen | release | engine <CEF version> [--approve] [--beta]>";

fn main() -> ExitCode {
    let task = env::args().nth(1);
    let result = match task.as_deref() {
        Some("package") => package(),
        Some("keygen") => keygen(),
        Some("release") => release(),
        Some("engine") => engine(),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::FAILURE;
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn package() -> Result<()> {
    let root = root();
    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    // Windows: the sandboxed DLL; elsewhere: the executable.
    let target_kind = if cfg!(windows) { "--lib" } else { "--bin=bare" };
    let status = Command::new(cargo)
        .current_dir(&root)
        .args(["build", "--release", "-p", "bare", target_kind])
        .status()?;
    if !status.success() {
        return Err("cargo build failed".into());
    }

    let cef = cef_dir(&root)?;
    let out = root.join("dist").join("Bare");
    if out.exists() {
        fs::remove_dir_all(&out)?;
    }
    fs::create_dir_all(&out)?;

    // Chromium runtime: libraries, resource packs, locales and license notices.
    let mut engine_names = Vec::new();
    for entry in fs::read_dir(&cef)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if path.is_dir() {
            if name == "locales" {
                copy_dir(&path, &out.join(&name))?;
                engine_names.push(name);
            }
            continue;
        }
        let skip = name.ends_with(".lib")
            || name.ends_with(".exe")
            || name == "archive.json"
            || name == "CMakeLists.txt";
        if !skip {
            fs::copy(&path, out.join(&name))?;
            engine_names.push(name);
        }
    }
    // Record the engine and its files, so an engine update knows what it replaces.
    if cfg!(windows) {
        engine_names.push("Bare.exe".into());
    }
    engine_names.sort();
    let archive_info: serde_json::Value = serde_json::from_str(&fs::read_to_string(cef.join("archive.json"))?)?;
    let engine = archive_info["name"]
        .as_str()
        .and_then(engine_files::version_of_archive)
        .ok_or("can't tell the CEF version from archive.json")?;
    engine_files::write_manifest(&out, &engine, &engine_names)?;
    fs::copy(root.join("LICENSE"), out.join("LICENSE.txt"))?;
    // Mozilla Readability (reader mode) is built into Bare.dll.
    fs::copy(root.join("assets/readability/LICENSE.md"), out.join("LICENSE-readability.txt"))?;

    // Filter lists are loaded at runtime from data/ next to the executable.
    let data = out.join("data");
    fs::create_dir_all(&data)?;
    for list in [
        "easylist.txt",
        "easyprivacy.txt",
        "hagezi-pro.txt",
        "bare-extra.txt",
        "blocklist.txt",
        "fanboy-social.txt",
        "bare-social.txt",
        "easylist-cookie.txt",
        "bare-cookies.txt",
        "LISTS.md",
    ] {
        fs::copy(root.join("data").join(list), data.join(list))?;
    }

    let release = root.join("target").join("release");
    if cfg!(windows) {
        // CEF's sandbox launcher, renamed: Bare.exe loads Bare.dll in every process.
        let exe = out.join("Bare.exe");
        fs::copy(cef.join("bootstrap.exe"), &exe)?;
        fs::copy(release.join("bare.dll"), out.join("Bare.dll"))?;
        #[cfg(windows)]
        {
            // Antivirus scanners briefly lock the freshly copied exe; retry a few times.
            let (ico, version) = (fs::read(root.join("assets").join("bare.ico"))?, version(&root)?);
            let mut attempt = 0;
            while let Err(e) = winres::brand(&exe, &ico, &version) {
                attempt += 1;
                if attempt == 5 {
                    return Err(e.into());
                }
                std::thread::sleep(std::time::Duration::from_secs(2));
            }
        }
    } else {
        fs::copy(release.join("bare"), out.join("bare"))?;
    }

    let size: u64 = walk(&out)?.iter().map(|p| p.metadata().map(|m| m.len()).unwrap_or(0)).sum();
    println!("Packaged {} ({:.0} MB)", out.display(), size as f64 / 1e6);
    Ok(())
}

/// The CEF runtime downloaded by `cef-dll-sys` into vendor/cef/<version>/cef_<os>_<arch>.
fn cef_dir(root: &Path) -> Result<PathBuf> {
    let os = match env::consts::OS {
        "macos" => "macos",
        os => os,
    };
    let arch = match env::consts::ARCH {
        "aarch64" => "arm64",
        a => a,
    };
    let wanted = format!("cef_{os}_{arch}");
    let base = root.join("vendor").join("cef");
    let mut versions: Vec<PathBuf> = fs::read_dir(&base)
        .map_err(|_| "CEF not downloaded yet: run `cargo build --release` first")?
        .filter_map(|e| e.ok().map(|e| e.path().join(&wanted)))
        .filter(|p| p.join("archive.json").exists())
        .collect();
    versions.sort();
    versions.pop().ok_or_else(|| format!("no {wanted} folder under vendor/cef").into())
}

fn version(root: &Path) -> Result<String> {
    let manifest = fs::read_to_string(root.join("Cargo.toml"))?;
    let package = manifest.split("[package]").nth(1).ok_or("no [package] in Cargo.toml")?;
    package
        .lines()
        .find_map(|l| l.trim().strip_prefix("version = \"")?.strip_suffix('"').map(String::from))
        .ok_or_else(|| "no package version in Cargo.toml".into())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

/// The private signing key lives outside the repository.
fn key_path() -> PathBuf {
    if let Some(p) = env::var_os("BARE_SIGNING_KEY_FILE") {
        return PathBuf::from(p);
    }
    let home = env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from).unwrap_or_default();
    home.join(".bare").join("update-signing.key")
}

fn keygen() -> Result<()> {
    let key_file = key_path();
    if key_file.exists() {
        return Err(format!("{} already exists; refusing to overwrite your signing key", key_file.display()).into());
    }
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|e| format!("no secure randomness: {e}"))?;
    let key = SigningKey::from_bytes(&seed);
    fs::create_dir_all(key_file.parent().ok_or("bad key path")?)?;
    fs::write(&key_file, hex(&seed))?;
    let public = root().join("data").join("update-key.pub");
    fs::write(&public, format!("{}\n", hex(&key.verifying_key().to_bytes())))?;
    println!("Private signing key: {}", key_file.display());
    println!("  Keep it secret and back it up. Anyone with it can push updates to every Bare user;");
    println!("  if you lose it, existing installs can never be updated again.");
    println!("Public key written to {} (commit this file).", public.display());
    Ok(())
}

fn release() -> Result<()> {
    let template = env::var("BARE_DOWNLOAD_URL").map_err(|_| {
        "set BARE_DOWNLOAD_URL to where release archives will be hosted, e.g.\n  \
         https://github.com/OWNER/REPO/releases/download/v{version}/{file}"
    })?;
    if env::var("BARE_UPDATE_URL").is_err() {
        return Err("set BARE_UPDATE_URL to where latest.txt will be published, e.g.\n  \
                    https://github.com/OWNER/REPO/releases/latest/download/latest.txt"
            .into());
    }
    let root = root();
    let key = signing_key(&root)?;
    // Checked before building anything: every release needs its notes.
    let notes = release_notes(&root, &version(&root)?)?;

    package()?;
    let version = version(&root)?;
    let platform = format!("{}-{}", env::consts::OS, env::consts::ARCH);
    let ext = if cfg!(windows) { "zip" } else { "tar.gz" };
    let file = format!("Bare-{version}-{platform}.{ext}");
    let dist = root.join("dist");
    let archive = dist.join(&file);
    let _ = fs::remove_file(&archive);
    let mut tar = Command::new("tar");
    if cfg!(windows) {
        tar.args(["-a", "-c", "-f"]);
    } else {
        tar.args(["-c", "-z", "-f"]);
    }
    // Name the top-level files and folders rather than ".": entries like "./Bare.exe" make
    // Windows Explorer reject the zip as invalid.
    let app = dist.join("Bare");
    let mut entries: Vec<_> = fs::read_dir(&app)?.filter_map(|e| e.ok().map(|e| e.file_name())).collect();
    entries.sort();
    if !tar.arg(&archive).arg("-C").arg(&app).args(&entries).status()?.success() {
        return Err("creating the archive failed".into());
    }

    let sha = hex(&Sha256::digest(fs::read(&archive)?));
    let url = template.replace("{version}", &version).replace("{file}", &file);
    // Must match `update::signed_message` in the app.
    let message = format!("bare-update\n{version}\n{platform}\n{url}\n{sha}");
    let signature = hex(&key.sign(message.as_bytes()).to_bytes());

    // Keep other platforms' entries for the same version.
    let manifest_path = dist.join("latest.txt");
    let mut lines: Vec<String> = fs::read_to_string(&manifest_path)
        .ok()
        .filter(|t| t.lines().any(|l| l == format!("version={version}")))
        .map(|t| t.lines().filter(|l| !l.starts_with("version=") && !l.starts_with(&format!("{platform}."))).map(String::from).collect())
        .unwrap_or_default();
    lines.insert(0, format!("version={version}"));
    lines.push(format!("{platform}.url={url}"));
    lines.push(format!("{platform}.sha256={sha}"));
    lines.push(format!("{platform}.signature={signature}"));
    fs::write(&manifest_path, lines.join("\n") + "\n")?;

    let notes_path = dist.join("release-notes.md");
    fs::write(&notes_path, &notes)?;
    println!("Release {version} for {platform}:");
    println!("  paste {} into the release description", notes_path.display());
    println!("  upload {} to {url}", archive.display());
    println!("  publish {} at $BARE_UPDATE_URL", manifest_path.display());
    // Carry the approved engine forward, so engines.txt stays at the latest release's address.
    let approved = root.join("data").join("engines.txt");
    if approved.exists() {
        fs::copy(&approved, dist.join("engines.txt"))?;
        println!("  attach {} (the approved Chromium engine)", dist.join("engines.txt").display());
    }

    if cfg!(windows) {
        match build_installer(&root, &version)? {
            Some(setup) => println!("  attach {} for people installing Bare", setup.display()),
            None => println!("  (Inno Setup not found: skipped BareSetup.exe. Install it with `winget install JRSoftware.InnoSetup`.)"),
        }
    }
    Ok(())
}

/// The GitHub release description for `version`: its section of CHANGELOG.md, how to get it,
/// and a link comparing it with the previous version.
fn release_notes(root: &Path, version: &str) -> Result<String> {
    let changelog = fs::read_to_string(root.join("CHANGELOG.md")).map_err(|_| "CHANGELOG.md is missing")?;
    let heading = |l: &str| l.strip_prefix("## ").map(|v| v.split_whitespace().next().unwrap_or("").to_string());
    let mut lines = changelog.lines().skip_while(|l| heading(l).as_deref() != Some(version));
    if lines.next().is_none() {
        return Err(format!("CHANGELOG.md has no \"## {version}\" section: write what's new in {version} first").into());
    }
    let mut section = Vec::new();
    let mut previous = None;
    for line in lines {
        if let Some(v) = heading(line) {
            previous = Some(v);
            break;
        }
        section.push(line);
    }
    let section = section.join("\n").trim().to_string();
    if section.is_empty() {
        return Err(format!("the \"## {version}\" section of CHANGELOG.md is empty").into());
    }
    let manifest = fs::read_to_string(root.join("Cargo.toml"))?;
    let repo = manifest
        .lines()
        .find_map(|l| l.trim().strip_prefix("repository = \"")?.strip_suffix('"').map(String::from))
        .ok_or("no repository in Cargo.toml")?;
    let mut notes = format!(
        "{section}\n\n**New to Bare?** Download `BareSetup-{version}.exe` below. **Already using it?** Bare updates itself within a day, or right away from Settings \u{2192} About \u{2192} Check now.\n"
    );
    if let Some(previous) = previous {
        notes.push_str(&format!("\n**Full Changelog**: {repo}/compare/v{previous}...v{version}\n"));
    }
    Ok(notes)
}

/// The update-signing key, checked against the public key built into Bare.
fn signing_key(root: &Path) -> Result<SigningKey> {
    let seed: [u8; 32] = fs::read_to_string(key_path())
        .ok()
        .and_then(|t| unhex(t.trim()))
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| format!("no signing key at {} (run `cargo xtask keygen`)", key_path().display()))?;
    let key = SigningKey::from_bytes(&seed);
    let public = fs::read_to_string(root.join("data").join("update-key.pub"))?;
    if public.trim() != hex(&key.verifying_key().to_bytes()) {
        return Err("data/update-key.pub doesn't match your signing key".into());
    }
    Ok(key)
}

const CEF_BUILDS: &str = "https://cef-builds.spotifycdn.com/";

/// `cargo xtask engine <CEF version> [--approve] [--beta]`
///
/// Without `--approve`: downloads the engine from CEF's build server (checking CEF's SHA-1)
/// and builds dist/engine-test, the packaged Bare running on that engine, to try out.
/// With `--approve`: signs engines.txt (data/ and dist/) so installed copies offer it.
fn engine() -> Result<()> {
    let args: Vec<String> = env::args().skip(2).collect();
    let wanted = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .ok_or("usage: cargo xtask engine <CEF version, e.g. 156.0.7> [--approve] [--beta]")?;
    let approve = args.iter().any(|a| a == "--approve");
    let allow_beta = args.iter().any(|a| a == "--beta");
    let root = root();
    let dist = root.join("dist");
    let platform = format!("{}-{}", env::consts::OS, env::consts::ARCH);
    let cef_platform = match (env::consts::OS, env::consts::ARCH) {
        ("windows", "x86_64") => "windows64",
        ("windows", "aarch64") => "windowsarm64",
        ("linux", "x86_64") => "linux64",
        ("linux", "aarch64") => "linuxarm64",
        ("macos", "aarch64") => "macosarm64",
        ("macos", "x86_64") => "macosx64",
        _ => return Err("no CEF builds for this platform".into()),
    };

    // Find the build in CEF's index.
    let index = Command::new("curl").args(["-fsSL", &format!("{CEF_BUILDS}index.json")]).output()?;
    if !index.status.success() {
        return Err("couldn't download CEF's build index".into());
    }
    let index: serde_json::Value = serde_json::from_slice(&index.stdout)?;
    let build = index[cef_platform]["versions"]
        .as_array()
        .ok_or("unexpected CEF index format")?
        .iter()
        .find(|v| v["cef_version"].as_str().is_some_and(|c| c == wanted || engine_files::cef_number(c) == wanted))
        .ok_or_else(|| format!("CEF {wanted} isn't on CEF's build server for {cef_platform}"))?;
    let channel = build["channel"].as_str().unwrap_or("stable");
    let file = build["files"]
        .as_array()
        .and_then(|files| files.iter().find(|f| f["type"] == "minimal"))
        .ok_or("that build has no minimal distribution")?;
    let (name, sha1, size) = (
        file["name"].as_str().ok_or("bad CEF index")?,
        file["sha1"].as_str().ok_or("bad CEF index")?.to_ascii_lowercase(),
        file["size"].as_u64().unwrap_or(0),
    );
    let engine = engine_files::version_of_archive(name).ok_or("unexpected CEF archive name")?;
    // '+' must be escaped: the CDN would read it as a space.
    let url = format!("{CEF_BUILDS}{}", name.replace('+', "%2B"));
    println!("CEF {engine} ({channel}), {} MB", size / 1_000_000);

    // Download once, verified against CEF's published SHA-1.
    let cache = dist.join("engines");
    fs::create_dir_all(&cache)?;
    let archive = cache.join(name);
    let sha1_of = |p: &Path| -> Result<String> { Ok(hex(&sha1::Sha1::digest(fs::read(p)?))) };
    if !archive.exists() || sha1_of(&archive)? != sha1 {
        println!("Downloading {url}");
        let partial = cache.join(format!("{name}.part"));
        if !Command::new("curl").args(["-fL", "--retry", "3", "-o"]).arg(&partial).arg(&url).status()?.success() {
            return Err("download failed".into());
        }
        fs::rename(&partial, &archive)?;
        if sha1_of(&archive)? != sha1 {
            fs::remove_file(&archive)?;
            return Err("the download doesn't match CEF's published SHA-1".into());
        }
    }
    let sha256 = hex(&Sha256::digest(fs::read(&archive)?));

    if approve {
        if channel != "stable" && !allow_beta {
            return Err(format!("CEF {engine} is a {channel} build; add --beta to approve it anyway").into());
        }
        let key = signing_key(&root)?;
        // Must match `update::signed_message_for(Kind::Engine, ..)` in the app.
        let message = format!("bare-engine\n{engine}\n{platform}\n{url}\n{sha256}");
        let signature = hex(&key.sign(message.as_bytes()).to_bytes());
        let text = format!(
            "engine={engine}\n{platform}.url={url}\n{platform}.sha256={sha256}\n{platform}.size={size}\n{platform}.signature={signature}\n"
        );
        fs::write(root.join("data").join("engines.txt"), &text)?;
        fs::write(dist.join("engines.txt"), &text)?;
        println!("Approved Chromium {} for {platform}.", engine_files::chromium(&engine));
        println!("  Upload {} to your latest GitHub release (replacing the old one).", dist.join("engines.txt").display());
        println!("  Bare offers it under Settings > About; users download it from CEF's server.");
        println!("  Commit data/engines.txt so later releases keep offering it.");
        return Ok(());
    }

    // Build dist/engine-test: the packaged Bare, with its engine swapped for this one.
    let app = dist.join("Bare");
    if !app.join(engine_files::MANIFEST).exists() {
        package()?;
    }
    let test = dist.join("engine-test");
    if test.exists() {
        fs::remove_dir_all(&test)?;
    }
    copy_dir(&app, &test)?;
    if let Some((_, files)) = engine_files::read_manifest(&test) {
        for f in files {
            let p = test.join(f);
            if p.is_dir() { fs::remove_dir_all(p)? } else { fs::remove_file(p)? }
        }
    }
    let unpacked = cache.join("unpacked");
    if unpacked.exists() {
        fs::remove_dir_all(&unpacked)?;
    }
    fs::create_dir_all(&unpacked)?;
    if !Command::new("tar").arg("-xjf").arg(&archive).arg("-C").arg(&unpacked).status()?.success() {
        return Err("couldn't unpack the engine".into());
    }
    let cef_root = fs::read_dir(&unpacked)?.filter_map(|e| e.ok().map(|e| e.path())).find(|p| p.is_dir()).ok_or("empty archive")?;
    let names = engine_files::assemble(&cef_root, &test)?;
    #[cfg(windows)]
    winres::brand(&test.join("Bare.exe"), &fs::read(root.join("assets").join("bare.ico"))?, &version(&root)?)?;
    engine_files::write_manifest(&test, &engine, &names)?;
    fs::remove_dir_all(&unpacked)?;
    let launcher = if cfg!(windows) { "Try Bare (test profile).cmd" } else { "try-bare.sh" };
    if cfg!(windows) {
        // A separate profile: a newer Chromium upgrades the profile it opens.
        fs::write(test.join(launcher), "@echo off\r\nset \"LOCALAPPDATA=%~dp0test-profile\"\r\nstart \"\" \"%~dp0Bare.exe\" %*\r\n")?;
    } else {
        fs::write(test.join(launcher), "#!/bin/sh\nXDG_DATA_HOME=\"$(dirname \"$0\")/test-profile\" exec \"$(dirname \"$0\")/bare\" \"$@\"\n")?;
    }
    println!("Built {} with Chromium {}.", test.display(), engine_files::chromium(&engine));
    println!("  Try it with \"{}\" (uses its own test profile, not your real one).", test.join(launcher).display());
    if channel != "stable" {
        println!("  Note: this is a {channel} build.");
    }
    println!("  If it works, approve it: cargo xtask engine {wanted} --approve{}", if channel != "stable" { " --beta" } else { "" });
    Ok(())
}

/// Builds dist/BareSetup-<version>.exe from installer/bare.iss with Inno Setup's compiler.
fn build_installer(root: &Path, version: &str) -> Result<Option<PathBuf>> {
    let local = env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_default();
    let candidates = [
        env::var_os("ISCC").map(PathBuf::from),
        Some(local.join(r"Programs\Inno Setup 6\ISCC.exe")),
        env::var_os("ProgramFiles(x86)").map(|p| PathBuf::from(p).join(r"Inno Setup 6\ISCC.exe")),
        env::var_os("ProgramFiles").map(|p| PathBuf::from(p).join(r"Inno Setup 6\ISCC.exe")),
    ];
    let Some(iscc) = candidates.into_iter().flatten().find(|p| p.exists()) else { return Ok(None) };
    let dist = root.join("dist");
    let setup = dist.join(format!("BareSetup-{version}.exe"));
    // Antivirus scanners sometimes lock the new setup exe while Inno Setup stamps it; retry.
    for attempt in 1..=4 {
        let _ = fs::remove_file(&setup);
        let status = Command::new(&iscc)
            .arg("/Q")
            .arg(format!("/DAppVersion={version}"))
            .arg(format!("/DSourceDir={}", dist.join("Bare").display()))
            .arg(format!("/DRepoDir={}", root.display()))
            .arg(format!("/O{}", dist.display()))
            .arg(root.join("installer").join("bare.iss"))
            .status()?;
        if status.success() && setup.exists() {
            return Ok(Some(setup));
        }
        if attempt < 4 {
            println!("  installer build failed (attempt {attempt}), retrying…");
            std::thread::sleep(std::time::Duration::from_secs(3));
        }
    }
    Err("building the installer failed".into())
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &dest)?;
        } else {
            fs::copy(entry.path(), dest)?;
        }
    }
    Ok(())
}

fn walk(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            files.extend(walk(&path)?);
        } else {
            files.push(path);
        }
    }
    Ok(files)
}
