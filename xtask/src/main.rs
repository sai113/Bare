//! Build helper.
//!
//! - `cargo xtask package`: builds Bare in release mode and assembles a runnable app folder in
//!   `dist/Bare` with the Chromium (CEF) runtime and filter lists next to it.
//! - `cargo xtask keygen`: creates the update-signing key pair (once per project).
//! - `cargo xtask release`: packages, archives and signs a release, writing `dist/latest.txt`.

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, ExitCode},
};

use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha256};

#[cfg(windows)]
mod winres;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const USAGE: &str = "usage: cargo xtask <package | keygen | release>";

fn main() -> ExitCode {
    let task = env::args().nth(1);
    let result = match task.as_deref() {
        Some("package") => package(),
        Some("keygen") => keygen(),
        Some("release") => release(),
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
    for entry in fs::read_dir(&cef)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if path.is_dir() {
            if name == "locales" {
                copy_dir(&path, &out.join(name))?;
            }
            continue;
        }
        let skip = name.ends_with(".lib")
            || name.ends_with(".exe")
            || name == "archive.json"
            || name == "CMakeLists.txt";
        if !skip {
            fs::copy(&path, out.join(&name))?;
        }
    }
    fs::copy(root.join("LICENSE"), out.join("LICENSE.txt"))?;

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
            let (ico, version) = (root.join("assets").join("bare.ico"), version(&root)?);
            let mut attempt = 0;
            while let Err(e) = winres::brand(&exe, &ico, &version) {
                attempt += 1;
                if attempt == 5 {
                    return Err(e);
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
    if !tar.arg(&archive).arg("-C").arg(dist.join("Bare")).arg(".").status()?.success() {
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

    println!("Release {version} for {platform}:");
    println!("  upload {} to {url}", archive.display());
    println!("  publish {} at $BARE_UPDATE_URL", manifest_path.display());
    Ok(())
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
