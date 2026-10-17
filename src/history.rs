//! Browsing history: kept in `<profile>/history.txt` and used for address-bar suggestions and
//! bare://history. Nothing is recorded in private windows.
//!
//! The file is an append-only log, compacted on start:
//!   `V\t<unix time>\t<url>\t<title>`  a visit
//!   `T\t<url>\t<title>`               a page title learned after the visit
//!   `A\t<unix time>\t<visits>\t<url>\t<title>`  a compacted entry
//!   `D\t<url>`                        a deleted entry

use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

/// Entries kept after compaction (most recent first).
const MAX_ENTRIES: usize = 20_000;

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub url: String,
    pub title: String,
    pub visits: u32,
    pub last: u64,
}

#[derive(Default)]
struct History {
    path: Option<PathBuf>,
    entries: HashMap<String, Entry>,
}

static HISTORY: Mutex<Option<History>> = Mutex::new(None);

fn with<R>(f: impl FnOnce(&mut History) -> R) -> R {
    let mut guard = HISTORY.lock().unwrap_or_else(|e| e.into_inner());
    f(guard.get_or_insert_with(History::default))
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn clean(s: &str) -> String {
    s.replace(['\t', '\r', '\n'], " ").trim().to_string()
}

/// Which URLs belong in history.
pub fn recordable(url: &str) -> bool {
    (url.starts_with("https://") || url.starts_with("http://") || url.starts_with("file:///")) && url.len() < 4096
}

/// Loads `<profile>/history.txt` (and compacts it).
pub fn init(profile: &std::path::Path) {
    let path = profile.join("history.txt");
    let entries = fs::read_to_string(&path).map(|t| parse(&t)).unwrap_or_default();
    let mut history = History { path: Some(path), entries };
    history.trim();
    history.rewrite();
    *HISTORY.lock().unwrap_or_else(|e| e.into_inner()) = Some(history);
}

fn parse(text: &str) -> HashMap<String, Entry> {
    let mut map: HashMap<String, Entry> = HashMap::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        match f.as_slice() {
            ["V", time, url, title] => {
                let e = map.entry(url.to_string()).or_insert_with(|| Entry { url: url.to_string(), title: String::new(), visits: 0, last: 0 });
                e.visits += 1;
                e.last = e.last.max(time.parse().unwrap_or(0));
                if !title.is_empty() {
                    e.title = title.to_string();
                }
            }
            ["T", url, title] => {
                if let Some(e) = map.get_mut(*url) {
                    e.title = title.to_string();
                }
            }
            ["A", time, visits, url, title] => {
                map.insert(url.to_string(), Entry { url: url.to_string(), title: title.to_string(), visits: visits.parse().unwrap_or(1), last: time.parse().unwrap_or(0) });
            }
            ["D", url] => {
                map.remove(*url);
            }
            _ => {}
        }
    }
    map
}

impl History {
    fn trim(&mut self) {
        if self.entries.len() > MAX_ENTRIES {
            let mut all: Vec<Entry> = self.entries.drain().map(|(_, e)| e).collect();
            all.sort_by(|a, b| b.last.cmp(&a.last));
            all.truncate(MAX_ENTRIES);
            self.entries = all.into_iter().map(|e| (e.url.clone(), e)).collect();
        }
    }

    /// Writes one compacted line per entry.
    fn rewrite(&self) {
        let Some(path) = &self.path else { return };
        let mut out = String::new();
        for e in self.entries.values() {
            out.push_str(&format!("A\t{}\t{}\t{}\t{}\n", e.last, e.visits, e.url, clean(&e.title)));
        }
        let _ = fs::write(path, out);
    }

    fn append(&self, line: &str) {
        let Some(path) = &self.path else { return };
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "{line}");
        }
    }
}

pub fn record_visit(url: &str, title: &str) {
    if !recordable(url) {
        return;
    }
    let (url, title, time) = (clean(url), clean(title), now());
    with(|h| {
        let e = h.entries.entry(url.clone()).or_insert_with(|| Entry { url: url.clone(), title: String::new(), visits: 0, last: 0 });
        e.visits += 1;
        e.last = time;
        if !title.is_empty() {
            e.title = title.clone();
        }
        h.append(&format!("V\t{time}\t{url}\t{title}"));
    });
}

pub fn set_title(url: &str, title: &str) {
    let (url, title) = (clean(url), clean(title));
    if title.is_empty() {
        return;
    }
    with(|h| {
        if let Some(e) = h.entries.get_mut(&url) {
            if e.title != title {
                e.title = title.clone();
                h.append(&format!("T\t{url}\t{title}"));
            }
        }
    });
}

pub fn delete(url: &str) {
    with(|h| {
        if h.entries.remove(url).is_some() {
            h.append(&format!("D\t{url}"));
        }
    });
}

pub fn clear() {
    with(|h| {
        h.entries.clear();
        if let Some(path) = &h.path {
            let _ = File::create(path);
        }
    });
}

/// Most recent entries first, optionally filtered by `query` (title or address).
pub fn recent(query: &str, limit: usize) -> Vec<Entry> {
    let q = query.trim().to_lowercase();
    let mut all: Vec<Entry> = with(|h| {
        h.entries.values().filter(|e| q.is_empty() || e.url.to_lowercase().contains(&q) || e.title.to_lowercase().contains(&q)).cloned().collect()
    });
    all.sort_by(|a, b| b.last.cmp(&a.last));
    all.truncate(limit);
    all
}

/// Address-bar suggestions for what's typed, best first.
pub fn suggest(typed: &str, limit: usize) -> Vec<Entry> {
    let now = now();
    let mut scored: Vec<(f64, Entry)> = with(|h| {
        h.entries.values().filter_map(|e| Some((score(e, typed, now)?, e.clone()))).collect()
    });
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    scored.into_iter().take(limit).map(|(_, e)| e).collect()
}

/// The address without "https://" and "www." (what people type).
pub fn display_url(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.strip_prefix("www.").unwrap_or(rest)
}

/// How well `e` matches `typed`, or None. Matches at the start of the site name rank first,
/// then the start of the address or of a word in the title, then anywhere; frequently and
/// recently visited pages rank higher.
pub fn score(e: &Entry, typed: &str, now: u64) -> Option<f64> {
    let q = typed.trim().to_lowercase();
    if q.is_empty() {
        return None;
    }
    let url = display_url(&e.url).to_lowercase();
    let title = e.title.to_lowercase();
    let host = url.split(['/', '?', '#']).next().unwrap_or("");
    let base = if host.starts_with(&q) {
        // Prefer the site's front page over deep links.
        100.0 + if url.trim_end_matches('/') == host { 20.0 } else { 0.0 }
    } else if url.starts_with(&q) {
        80.0
    } else if title.split(|c: char| !c.is_alphanumeric()).any(|w| w.starts_with(&q)) {
        50.0
    } else if url.contains(&q) {
        30.0
    } else if title.contains(&q) {
        20.0
    } else {
        return None;
    };
    let days = now.saturating_sub(e.last) as f64 / 86_400.0;
    let recency = 1.0 / (1.0 + days / 7.0);
    let frequency = (1.0 + e.visits as f64).ln();
    Some(base * (1.0 + frequency) * (0.5 + recency))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(url: &str, title: &str, visits: u32, days_ago: u64) -> Entry {
        Entry { url: url.into(), title: title.into(), visits, last: 1_000_000_000 - days_ago * 86_400 }
    }

    #[test]
    fn ranking() {
        let now = 1_000_000_000;
        let gh = entry("https://github.com/", "GitHub", 30, 0);
        let gh_deep = entry("https://github.com/sai113/Bare/issues", "Issues", 2, 0);
        let gitlab = entry("https://gitlab.com/", "GitLab", 1, 30);
        let blog = entry("https://blog.example/post", "Using git well", 1, 0);
        // Site-name prefix beats a title word; the front page beats a deep link.
        assert!(score(&gh, "git", now) > score(&gh_deep, "git", now));
        assert!(score(&gh_deep, "git", now) > score(&blog, "git", now));
        assert!(score(&gh, "git", now) > score(&gitlab, "git", now));
        // "www." and the scheme are ignored; no match means no suggestion.
        assert!(score(&entry("https://www.youtube.com/", "YouTube", 1, 0), "you", now).is_some());
        assert!(score(&gh, "zzz", now).is_none());
        assert!(score(&gh, "  ", now).is_none());
    }

    #[test]
    fn log_parsing() {
        let text = "A\t100\t3\thttps://a.test/\tA site\n\
                    V\t200\thttps://a.test/\t\n\
                    V\t150\thttps://b.test/\tB\n\
                    T\thttps://b.test/\tB renamed\n\
                    V\t160\thttps://c.test/\tC\n\
                    D\thttps://c.test/\n\
                    garbage line\n";
        let map = parse(text);
        assert_eq!(map.len(), 2);
        assert_eq!(map["https://a.test/"], Entry { url: "https://a.test/".into(), title: "A site".into(), visits: 4, last: 200 });
        assert_eq!(map["https://b.test/"].title, "B renamed");
    }

    #[test]
    fn what_gets_recorded() {
        assert!(recordable("https://example.com/"));
        assert!(recordable("file:///C:/a.html"));
        assert!(!recordable("data:text/html,hi"));
        assert!(!recordable("about:blank"));
        assert!(!recordable("bare://settings"));
        assert_eq!(display_url("https://www.github.com/x"), "github.com/x");
        assert_eq!(clean("a\tb\nc"), "a b c");
    }
}
