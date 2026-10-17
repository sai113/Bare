//! Download tracking for the downloads panel. Lives on the UI thread.

use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    process::Command,
};

use cef::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum State {
    InProgress,
    Paused,
    Complete,
    Canceled,
    Failed,
}

impl State {
    pub fn id(self) -> &'static str {
        match self {
            State::InProgress => "progress",
            State::Paused => "paused",
            State::Complete => "complete",
            State::Canceled => "canceled",
            State::Failed => "failed",
        }
    }
}

pub struct Download {
    pub id: u32,
    pub name: String,
    pub path: String,
    pub url: String,
    pub received: i64,
    pub total: i64,
    pub percent: i32,
    pub speed: i64,
    pub state: State,
    callback: Option<DownloadItemCallback>,
}

thread_local! {
    static DOWNLOADS: RefCell<Vec<Download>> = const { RefCell::new(Vec::new()) };
}

pub fn with<R>(f: impl FnOnce(&mut Vec<Download>) -> R) -> R {
    DOWNLOADS.with(|d| f(&mut d.borrow_mut()))
}

/// The user's Downloads folder.
pub fn folder() -> PathBuf {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from).unwrap_or_default();
    home.join("Downloads")
}

/// A path in `dir` for `name` that doesn't overwrite an existing file: "a.zip", "a (1).zip", ...
pub fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let name = sanitize(name);
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.clone(), String::new()),
    };
    let mut path = dir.join(&name);
    let mut n = 1;
    while path.exists() {
        path = dir.join(format!("{stem} ({n}){ext}"));
        n += 1;
    }
    path
}

/// Strips path separators and characters Windows doesn't allow in file names.
fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control() { '_' } else { c })
        .collect();
    let cleaned = cleaned.trim().trim_matches('.').to_string();
    if cleaned.is_empty() { "download".into() } else { cleaned }
}

/// Records progress reported by CEF.
pub fn update(item: &DownloadItem, callback: Option<&mut DownloadItemCallback>) {
    let s = |v: &CefStringUserfree| CefString::from(v).to_string();
    let path = s(&item.full_path());
    let state = if item.is_complete() == 1 {
        State::Complete
    } else if item.is_canceled() == 1 {
        State::Canceled
    } else if item.is_interrupted() == 1 {
        State::Failed
    } else if item.is_paused() == 1 {
        State::Paused
    } else {
        State::InProgress
    };
    let name = Path::new(&path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| s(&item.suggested_file_name()));
    let entry = Download {
        id: item.id(),
        name,
        path,
        url: s(&item.url()),
        received: item.received_bytes(),
        total: item.total_bytes(),
        percent: item.percent_complete(),
        speed: item.current_speed(),
        state,
        callback: if matches!(state, State::InProgress | State::Paused) { callback.map(|c| c.clone()) } else { None },
    };
    with(|list| match list.iter_mut().find(|d| d.id == entry.id) {
        Some(d) => *d = entry,
        None => list.insert(0, entry),
    });
}

pub fn cancel(id: u32) {
    with(|list| {
        if let Some(cb) = list.iter().find(|d| d.id == id).and_then(|d| d.callback.as_ref()) {
            cb.cancel();
        }
    });
}

pub fn pause_or_resume(id: u32) {
    with(|list| {
        if let Some(d) = list.iter().find(|d| d.id == id) {
            if let Some(cb) = &d.callback {
                if d.state == State::Paused { cb.resume() } else { cb.pause() }
            }
        }
    });
}

/// Removes finished downloads from the list (files stay on disk).
pub fn clear_finished() {
    with(|list| list.retain(|d| matches!(d.state, State::InProgress | State::Paused)));
}

/// Opens a finished download with its default app.
pub fn open(id: u32) {
    if let Some(path) = finished_path(id) {
        open_with_system(&path, false);
    }
}

/// Shows a finished download in the file manager.
pub fn show_in_folder(id: u32) {
    if let Some(path) = finished_path(id) {
        open_with_system(&path, true);
    }
}

fn finished_path(id: u32) -> Option<String> {
    with(|list| list.iter().find(|d| d.id == id && d.state == State::Complete).map(|d| d.path.clone()))
        .filter(|p| Path::new(p).exists())
}

fn open_with_system(path: &str, reveal: bool) {
    #[cfg(windows)]
    {
        if reveal {
            // The path quoted as Explorer expects it, so a comma in a file name can't be read
            // as another Explorer switch. (Windows file names can't contain quotes.)
            use std::os::windows::process::CommandExt;
            let _ = Command::new("explorer.exe").raw_arg(format!("/select,\"{path}\"")).spawn();
        } else {
            // Opened the way double-clicking it would, with Windows' usual SmartScreen /
            // Mark-of-the-Web checks for downloaded programs.
            crate::system::shell_open(path);
        }
        return;
    }
    #[allow(unreachable_code)]
    let result = if cfg!(target_os = "macos") {
        let mut c = Command::new("open");
        if reveal {
            c.arg("-R");
        }
        c.arg(path).spawn()
    } else {
        let target = if reveal { Path::new(path).parent().map_or(path.into(), |p| p.to_string_lossy().into_owned()) } else { path.into() };
        Command::new("xdg-open").arg(target).spawn()
    };
    let _ = result;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_unique_names() {
        let dir = std::env::temp_dir().join(format!("bare-dl-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(unique_path(&dir, "a.zip"), dir.join("a.zip"));
        std::fs::write(dir.join("a.zip"), b"").unwrap();
        assert_eq!(unique_path(&dir, "a.zip"), dir.join("a (1).zip"));
        assert_eq!(unique_path(&dir, "..\\..\\evil.exe"), dir.join("_.._evil.exe"));
        assert_eq!(unique_path(&dir, "con:<>"), dir.join("con___"));
        assert_eq!(unique_path(&dir, "..."), dir.join("download"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
