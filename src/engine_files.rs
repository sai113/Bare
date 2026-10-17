//! Which files make up Bare's Chromium engine, and the `engine.txt` manifest that records them.
//!
//! Shared by `cargo xtask` (packaging, trying out engines) and the in-app engine updater, so
//! both lay out an engine the same way. Plain std only: no CEF calls.

// Each side uses part of it.
#![allow(dead_code)]

use std::{
    fs, io,
    path::{Path, PathBuf},
};

/// Written next to Bare.exe: which engine is installed, and its files.
pub const MANIFEST: &str = "engine.txt";

/// Files in the app folder that belong to Bare itself and are never replaced or removed by an
/// engine update.
const BARE_FILES: &[&str] = &[
    "Bare.dll",
    "bare",
    "data",
    "LICENSE.txt",
    "LICENSE-readability.txt",
    MANIFEST,
    "unins000.exe",
    "unins000.dat",
];

/// "154.0.34+g14c5a08+chromium-154.0.8037.98" from a CEF archive name
/// ("cef_binary_154.0.34+g14c5a08+chromium-154.0.8037.98_windows64_minimal.tar.bz2").
pub fn version_of_archive(name: &str) -> Option<String> {
    let rest = name.strip_prefix("cef_binary_")?;
    let version = rest.split('_').next()?;
    version.contains("+chromium-").then(|| version.to_string())
}

/// The CEF release number of an engine version ("154.0.34").
pub fn cef_number(engine: &str) -> &str {
    engine.split('+').next().unwrap_or(engine)
}

/// The Chromium version of an engine version ("154.0.8037.98").
pub fn chromium(engine: &str) -> &str {
    engine.split_once("+chromium-").map_or(engine, |(_, c)| c)
}

/// The engine files in an unpacked CEF "minimal" archive, as (source, name in the app folder).
/// CEF's sandbox launcher `bootstrap.exe` becomes `Bare.exe`.
pub fn runtime_files(archive_root: &Path) -> io::Result<Vec<(PathBuf, String)>> {
    let mut files = Vec::new();
    for dir in ["Release", "Resources"] {
        for entry in fs::read_dir(archive_root.join(dir))? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".lib") || name == "bootstrapc.exe" {
                continue;
            }
            let target = if name == "bootstrap.exe" { "Bare.exe".to_string() } else { name };
            files.push((entry.path(), target));
        }
    }
    let credits = archive_root.join("CREDITS.html");
    if credits.exists() {
        files.push((credits, "CREDITS.html".into()));
    }
    let required = if cfg!(windows) { ["libcef.dll", "Bare.exe"] } else { ["libcef.so", "icudtl.dat"] };
    if let Some(missing) = required.iter().find(|r| !files.iter().any(|(_, n)| n == *r)) {
        return Err(io::Error::other(format!("not a CEF runtime: no {missing}")));
    }
    if let Some((_, name)) = files.iter().find(|(_, n)| !valid_name(n)) {
        return Err(io::Error::other(format!("unexpected file in the engine: {name}")));
    }
    Ok(files)
}

/// Copies the engine in `archive_root` into `target`; returns the names it wrote.
pub fn assemble(archive_root: &Path, target: &Path) -> io::Result<Vec<String>> {
    fs::create_dir_all(target)?;
    let mut names = Vec::new();
    for (source, name) in runtime_files(archive_root)? {
        copy_all(&source, &target.join(&name))?;
        names.push(name);
    }
    names.sort();
    Ok(names)
}

fn copy_all(from: &Path, to: &Path) -> io::Result<()> {
    if from.is_dir() {
        fs::create_dir_all(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            copy_all(&entry.path(), &to.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        fs::copy(from, to).map(|_| ())
    }
}

/// A plain file or folder name in the app folder: no paths, and never one of Bare's own files.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\', ':', '\n', '\r', '"', '%'])
        && !BARE_FILES.iter().any(|b| b.eq_ignore_ascii_case(name))
}

pub fn write_manifest(dir: &Path, engine: &str, names: &[String]) -> io::Result<()> {
    fs::write(dir.join(MANIFEST), format!("engine={engine}\nfiles={}\n", names.join(",")))
}

/// (engine version, file names) from an app folder's `engine.txt`.
pub fn read_manifest(dir: &Path) -> Option<(String, Vec<String>)> {
    let text = fs::read_to_string(dir.join(MANIFEST)).ok()?;
    let get = |key: &str| text.lines().find_map(|l| l.strip_prefix(key).and_then(|v| v.strip_prefix('=')).map(str::trim));
    let engine = get("engine")?.to_string();
    let files = get("files")?.split(',').map(str::trim).filter(|n| valid_name(n)).map(String::from).collect();
    Some((engine, files))
}

/// Files of the old engine that the new one no longer has (to delete when installing it).
pub fn stale(old: &[String], new: &[String]) -> Vec<String> {
    old.iter().filter(|o| valid_name(o) && !new.iter().any(|n| n.eq_ignore_ascii_case(o))).cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        let v = version_of_archive("cef_binary_156.0.7+g47dbc1d+chromium-156.0.8078.17_windows64_beta_minimal.tar.bz2").unwrap();
        assert_eq!(v, "156.0.7+g47dbc1d+chromium-156.0.8078.17");
        assert_eq!(cef_number(&v), "156.0.7");
        assert_eq!(chromium(&v), "156.0.8078.17");
        assert_eq!(version_of_archive("bare.zip"), None);
    }

    #[test]
    fn names() {
        assert!(valid_name("libcef.dll") && valid_name("locales"));
        for bad in ["", "..", "../x", "a\\b", "C:x", "Bare.dll", "bare.DLL", "data", "engine.txt", "unins000.exe", "x\"y"] {
            assert!(!valid_name(bad), "{bad}");
        }
        let old = vec!["libcef.dll".to_string(), "dxil.dll".into(), "Bare.dll".into()];
        assert_eq!(stale(&old, &["libcef.dll".into()]), vec!["dxil.dll".to_string()]);
    }

    #[test]
    fn manifest_and_layout() {
        let base = std::env::temp_dir().join(format!("bare-engine-test-{}", std::process::id()));
        let archive = base.join("cef_binary_x");
        for (dir, name) in [("Release", "libcef.dll"), ("Release", "bootstrap.exe"), ("Release", "libcef.lib"), ("Release", "bootstrapc.exe"), ("Resources", "resources.pak")] {
            fs::create_dir_all(archive.join(dir)).unwrap();
            fs::write(archive.join(dir).join(name), name).unwrap();
        }
        fs::create_dir_all(archive.join("Resources/locales")).unwrap();
        fs::write(archive.join("Resources/locales/en-US.pak"), "en").unwrap();
        fs::write(archive.join("CREDITS.html"), "credits").unwrap();
        let app = base.join("app");
        let names = assemble(&archive, &app).unwrap();
        if cfg!(windows) {
            assert_eq!(names, ["Bare.exe", "CREDITS.html", "libcef.dll", "locales", "resources.pak"]);
            assert_eq!(fs::read_to_string(app.join("Bare.exe")).unwrap(), "bootstrap.exe");
            assert!(app.join("locales/en-US.pak").exists() && !app.join("libcef.lib").exists());
        }
        write_manifest(&app, "156.0.7+g1+chromium-156.0.8078.17", &names).unwrap();
        let (engine, files) = read_manifest(&app).unwrap();
        assert_eq!((engine.as_str(), files), ("156.0.7+g1+chromium-156.0.8078.17", names));
        fs::remove_dir_all(&base).unwrap();
    }
}
