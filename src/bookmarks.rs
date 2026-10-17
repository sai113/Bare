//! Bookmarks, kept in `<profile>/bookmarks.json`, shown on the bookmarks bar and in address-bar
//! suggestions. Can be imported from Chrome, Edge and Brave.
//!
//! Folders are paths like "Work/Docs". Bookmarks with an empty folder sit directly on the bar;
//! top-level folders appear on the bar as menus.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Bookmark {
    pub title: String,
    pub url: String,
    #[serde(default)]
    pub folder: String,
}

#[derive(Default, Serialize, Deserialize)]
struct Store {
    #[serde(default)]
    bookmarks: Vec<Bookmark>,
    #[serde(skip)]
    path: Option<PathBuf>,
}

static STORE: Mutex<Option<Store>> = Mutex::new(None);

fn with<R>(f: impl FnOnce(&mut Store) -> R) -> R {
    let mut guard = STORE.lock().unwrap_or_else(|e| e.into_inner());
    f(guard.get_or_insert_with(Store::default))
}

impl Store {
    fn save(&self) {
        if let (Some(path), Ok(json)) = (&self.path, serde_json::to_string_pretty(self)) {
            let _ = fs::write(path, json);
        }
    }
}

pub fn init(profile: &Path) {
    let path = profile.join("bookmarks.json");
    let mut store: Store = fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    store.path = Some(path);
    *STORE.lock().unwrap_or_else(|e| e.into_inner()) = Some(store);
}

pub fn all() -> Vec<Bookmark> {
    with(|s| s.bookmarks.clone())
}

pub fn is_bookmarked(url: &str) -> bool {
    with(|s| s.bookmarks.iter().any(|b| b.url == url))
}

/// Adds a bookmark to the bar (no-op if the address is already bookmarked).
pub fn add(title: &str, url: &str) {
    with(|s| {
        if !s.bookmarks.iter().any(|b| b.url == url) {
            let title = if title.trim().is_empty() { url } else { title.trim() };
            s.bookmarks.push(Bookmark { title: title.to_string(), url: url.to_string(), folder: String::new() });
            s.save();
        }
    });
}

pub fn remove(url: &str) {
    with(|s| {
        s.bookmarks.retain(|b| b.url != url);
        s.save();
    });
}

pub fn rename(url: &str, title: &str) {
    with(|s| {
        if let Some(b) = s.bookmarks.iter_mut().find(|b| b.url == url) {
            b.title = title.trim().to_string();
            s.save();
        }
    });
}

/// Bookmarks matching what's typed in the address bar (title or address).
pub fn suggest(typed: &str, limit: usize) -> Vec<Bookmark> {
    let q = typed.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    with(|s| {
        s.bookmarks
            .iter()
            .filter(|b| crate::history::display_url(&b.url).to_lowercase().starts_with(&q) || b.title.to_lowercase().contains(&q))
            .take(limit)
            .cloned()
            .collect()
    })
}

// ---- the bar ----

#[derive(Clone, Debug, PartialEq)]
pub enum BarItem {
    Link(Bookmark),
    /// A top-level folder and everything inside it (sub-folders flattened with their paths).
    Folder(String, Vec<Bookmark>),
}

/// What the bookmarks bar shows: loose bookmarks and top-level folders, in order.
pub fn bar_items() -> Vec<BarItem> {
    bar_items_of(&all())
}

fn bar_items_of(bookmarks: &[Bookmark]) -> Vec<BarItem> {
    let mut items: Vec<BarItem> = Vec::new();
    for b in bookmarks {
        if b.folder.is_empty() {
            items.push(BarItem::Link(b.clone()));
            continue;
        }
        let top = b.folder.split('/').next().unwrap_or(&b.folder).to_string();
        match items.iter_mut().find(|i| matches!(i, BarItem::Folder(name, _) if *name == top)) {
            Some(BarItem::Folder(_, list)) => list.push(b.clone()),
            _ => items.push(BarItem::Folder(top, vec![b.clone()])),
        }
    }
    items
}

// ---- importing from Chromium-based browsers ----

/// Browsers whose bookmarks can be imported: (id, name, path to their Bookmarks file).
pub fn import_sources() -> Vec<(&'static str, &'static str, PathBuf)> {
    let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_default();
    [
        ("chrome", "Chrome", local.join(r"Google\Chrome\User Data\Default\Bookmarks")),
        ("edge", "Edge", local.join(r"Microsoft\Edge\User Data\Default\Bookmarks")),
        ("brave", "Brave", local.join(r"BraveSoftware\Brave-Browser\User Data\Default\Bookmarks")),
    ]
    .into_iter()
    .filter(|(_, _, p)| p.exists())
    .collect()
}

#[derive(Deserialize)]
struct ChromiumNode {
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    children: Vec<ChromiumNode>,
}

#[derive(Deserialize)]
struct ChromiumFile {
    roots: std::collections::HashMap<String, ChromiumNode>,
}

/// Bookmarks from a Chromium `Bookmarks` file. The bar's contents stay on the bar; "Other"
/// and "Mobile" bookmarks go into folders of those names.
pub fn parse_chromium(json: &str) -> Result<Vec<Bookmark>, String> {
    let file: ChromiumFile = serde_json::from_str(json).map_err(|e| format!("unreadable bookmarks file ({e})"))?;
    let mut out = Vec::new();
    fn walk(node: &ChromiumNode, folder: &str, out: &mut Vec<Bookmark>) {
        for child in &node.children {
            if child.kind == "url" && (child.url.starts_with("http://") || child.url.starts_with("https://")) {
                out.push(Bookmark { title: child.name.clone(), url: child.url.clone(), folder: folder.to_string() });
            } else if child.kind == "folder" {
                let name = child.name.replace('/', "-");
                let path = if folder.is_empty() { name } else { format!("{folder}/{name}") };
                walk(child, &path, out);
            }
        }
    }
    for (root, folder) in [("bookmark_bar", ""), ("other", "Other bookmarks"), ("synced", "Mobile bookmarks")] {
        if let Some(node) = file.roots.get(root) {
            walk(node, folder, &mut out);
        }
    }
    Ok(out)
}

/// Imports from one of `import_sources()`; returns how many new bookmarks were added.
pub fn import(source_id: &str) -> Result<usize, String> {
    let (_, name, path) = import_sources().into_iter().find(|(id, ..)| *id == source_id).ok_or("that browser isn't installed")?;
    let json = fs::read_to_string(&path).map_err(|e| format!("couldn't read {name}'s bookmarks ({e})"))?;
    let found = parse_chromium(&json)?;
    Ok(with(|s| {
        let mut added = 0;
        for b in found {
            if !s.bookmarks.iter().any(|x| x.url == b.url) {
                s.bookmarks.push(b);
                added += 1;
            }
        }
        s.save();
        added
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHROME: &str = r#"{
      "checksum": "x", "version": 1,
      "roots": {
        "bookmark_bar": { "type": "folder", "name": "Bookmarks bar", "children": [
          { "type": "url", "name": "GitHub", "url": "https://github.com/" },
          { "type": "folder", "name": "Work", "children": [
            { "type": "url", "name": "Docs", "url": "https://docs.example/" },
            { "type": "folder", "name": "Old/Archive", "children": [
              { "type": "url", "name": "Wiki", "url": "https://wiki.example/" } ] } ] },
          { "type": "url", "name": "Script", "url": "javascript:alert(1)" } ] },
        "other": { "type": "folder", "name": "Other", "children": [
          { "type": "url", "name": "News", "url": "https://news.example/" } ] },
        "synced": { "type": "folder", "name": "Mobile", "children": [] }
      }
    }"#;

    #[test]
    fn chromium_import() {
        let b = parse_chromium(CHROME).unwrap();
        let summary: Vec<(&str, &str)> = b.iter().map(|b| (b.title.as_str(), b.folder.as_str())).collect();
        assert_eq!(summary, vec![("GitHub", ""), ("Docs", "Work"), ("Wiki", "Work/Old-Archive"), ("News", "Other bookmarks")]);
        assert!(parse_chromium("not json").is_err());
    }

    #[test]
    fn bar_grouping() {
        let b = parse_chromium(CHROME).unwrap();
        let items = bar_items_of(&b);
        assert_eq!(items.len(), 3);
        assert!(matches!(&items[0], BarItem::Link(x) if x.title == "GitHub"));
        assert!(matches!(&items[1], BarItem::Folder(name, list) if name == "Work" && list.len() == 2));
        assert!(matches!(&items[2], BarItem::Folder(name, list) if name == "Other bookmarks" && list.len() == 1));
    }
}
