//! User settings, kept in `<profile>/settings.txt` as simple `key=value` lines.

use std::{
    collections::BTreeSet,
    fs,
    path::PathBuf,
    sync::{OnceLock, RwLock, RwLockReadGuard, RwLockWriteGuard},
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Search {
    DuckDuckGo,
    Brave,
    Startpage,
    Google,
    Bing,
}

impl Search {
    pub const ALL: [Search; 5] = [Search::DuckDuckGo, Search::Brave, Search::Startpage, Search::Google, Search::Bing];

    pub fn id(self) -> &'static str {
        match self {
            Search::DuckDuckGo => "duckduckgo",
            Search::Brave => "brave",
            Search::Startpage => "startpage",
            Search::Google => "google",
            Search::Bing => "bing",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Search::DuckDuckGo => "DuckDuckGo",
            Search::Brave => "Brave Search",
            Search::Startpage => "Startpage",
            Search::Google => "Google",
            Search::Bing => "Bing",
        }
    }

    /// The engine's home page, used as the new-tab page.
    pub fn home(self) -> &'static str {
        match self {
            Search::DuckDuckGo => "https://duckduckgo.com/",
            Search::Brave => "https://search.brave.com/",
            Search::Startpage => "https://www.startpage.com/",
            Search::Google => "https://www.google.com/",
            Search::Bing => "https://www.bing.com/",
        }
    }

    /// URL prefix the URL-encoded query is appended to.
    pub fn prefix(self) -> &'static str {
        match self {
            Search::DuckDuckGo => "https://duckduckgo.com/?q=",
            Search::Brave => "https://search.brave.com/search?q=",
            Search::Startpage => "https://www.startpage.com/do/search?q=",
            Search::Google => "https://www.google.com/search?q=",
            Search::Bing => "https://www.bing.com/search?q=",
        }
    }

    pub fn from_id(id: &str) -> Option<Search> {
        Search::ALL.into_iter().find(|s| s.id() == id)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub search: Search,
    /// Network blocking of ads, trackers and malware.
    pub blocking: bool,
    /// Hiding leftover ad elements on pages.
    pub cosmetic: bool,
    /// Optional list: social media buttons, widgets and trackers.
    pub block_social: bool,
    /// Optional list: hide cookie-consent banners.
    pub hide_cookie_banners: bool,
    /// Sites where blocking is turned off.
    pub allowed_sites: BTreeSet<String>,
    /// Ask where to save each download instead of using the Downloads folder.
    pub ask_download_location: bool,
    /// Check for and download updates automatically.
    pub auto_update: bool,
    /// Delete all cookies, cache and site data the next time Bare starts.
    pub wipe_on_start: bool,
    /// Show the bookmarks bar (when there are bookmarks).
    pub bookmarks_bar: bool,
    /// New tabs open the search engine's home page (otherwise a blank page).
    pub new_tab_home: bool,
    /// Offer to save passwords when logging in to sites.
    pub save_passwords: bool,
    /// Secure DNS (DNS over HTTPS).
    pub dns: crate::network::Dns,
    /// Proxy server.
    pub proxy: crate::network::Proxy,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            search: Search::DuckDuckGo,
            blocking: true,
            cosmetic: true,
            block_social: false,
            hide_cookie_banners: false,
            allowed_sites: BTreeSet::new(),
            ask_download_location: false,
            auto_update: true,
            wipe_on_start: false,
            bookmarks_bar: true,
            new_tab_home: true,
            save_passwords: true,
            dns: crate::network::Dns::Automatic,
            proxy: crate::network::Proxy::System,
        }
    }
}

impl Settings {
    pub fn parse(text: &str) -> Settings {
        let mut s = Settings::default();
        let (mut proxy_mode, mut proxy_bypass) = (String::new(), String::new());
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else { continue };
            let on = value.trim() == "true";
            match key.trim() {
                "search" => s.search = Search::from_id(value.trim()).unwrap_or(s.search),
                "blocking" => s.blocking = on,
                "cosmetic" => s.cosmetic = on,
                "block_social" => s.block_social = on,
                "hide_cookie_banners" => s.hide_cookie_banners = on,
                "ask_download_location" => s.ask_download_location = on,
                "auto_update" => s.auto_update = on,
                "wipe_on_start" => s.wipe_on_start = on,
                "bookmarks_bar" => s.bookmarks_bar = on,
                "new_tab_home" => s.new_tab_home = on,
                "save_passwords" => s.save_passwords = on,
                "dns" => s.dns = crate::network::Dns::parse(value),
                "proxy" => proxy_mode = value.trim().to_string(),
                "proxy_bypass" => proxy_bypass = value.trim().to_string(),
                "allowed_sites" => {
                    s.allowed_sites = value.split(',').map(str::trim).filter(|h| !h.is_empty()).map(String::from).collect()
                }
                _ => {}
            }
        }
        s.proxy = crate::network::Proxy::parse(&proxy_mode, &proxy_bypass);
        s
    }

    pub fn serialize(&self) -> String {
        let sites: Vec<&str> = self.allowed_sites.iter().map(String::as_str).collect();
        let (proxy, bypass) = self.proxy.serialize();
        // One value per line: never let user-entered text add lines.
        let line = |s: &str| s.replace(['\r', '\n'], " ");
        format!(
            "search={}\nblocking={}\ncosmetic={}\nblock_social={}\nhide_cookie_banners={}\nallowed_sites={}\nask_download_location={}\nauto_update={}\nwipe_on_start={}\nbookmarks_bar={}\nnew_tab_home={}\nsave_passwords={}\ndns={}\nproxy={}\nproxy_bypass={}\n",
            self.search.id(),
            self.blocking,
            self.cosmetic,
            self.block_social,
            self.hide_cookie_banners,
            sites.join(","),
            self.ask_download_location,
            self.auto_update,
            self.wipe_on_start,
            self.bookmarks_bar,
            self.new_tab_home,
            self.save_passwords,
            line(&self.dns.serialize()),
            line(&proxy),
            line(&bypass),
        )
    }

    /// What a new tab shows.
    pub fn new_tab_url(&self) -> &'static str {
        if self.new_tab_home { self.search.home() } else { crate::url::BLANK }
    }

    /// Optional filter lists to load.
    pub fn extras(&self) -> crate::block::Extras {
        crate::block::Extras { social: self.block_social, cookie_banners: self.hide_cookie_banners }
    }

    /// Whether network and cosmetic blocking apply on `host`.
    pub fn blocks_on(&self, host: &str) -> bool {
        self.blocking && !self.allowed_sites.contains(host)
    }
}

struct Store {
    path: Option<PathBuf>,
    settings: RwLock<Settings>,
}

static STORE: OnceLock<Store> = OnceLock::new();

/// Loads settings from `path` (call once at startup in the browser process).
pub fn init(path: PathBuf) {
    let settings = fs::read_to_string(&path).map(|t| Settings::parse(&t)).unwrap_or_default();
    let _ = STORE.set(Store { path: Some(path), settings: RwLock::new(settings) });
}

fn store() -> &'static Store {
    STORE.get_or_init(|| Store { path: None, settings: RwLock::new(Settings::default()) })
}

pub fn get() -> RwLockReadGuard<'static, Settings> {
    store().settings.read().unwrap_or_else(|e| e.into_inner())
}

/// Changes settings and saves them to disk.
pub fn update(change: impl FnOnce(&mut Settings)) {
    let store = store();
    let text = {
        let mut s: RwLockWriteGuard<Settings> = store.settings.write().unwrap_or_else(|e| e.into_inner());
        change(&mut s);
        s.serialize()
    };
    if let Some(path) = &store.path {
        let _ = fs::write(path, text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let mut s = Settings { search: Search::Brave, cosmetic: false, auto_update: false, block_social: true, bookmarks_bar: false, new_tab_home: false, save_passwords: false, ..Default::default() };
        s.dns = crate::network::Dns::Custom("https://doh.example/dns-query".into());
        s.proxy = crate::network::Proxy::Manual { server: "socks5://127.0.0.1:9050".into(), bypass: "localhost".into() };
        s.allowed_sites.insert("example.com".into());
        s.allowed_sites.insert("news.test".into());
        assert_eq!(Settings::parse(&s.serialize()), s);
    }

    #[test]
    fn defaults_and_garbage() {
        assert_eq!(Settings::parse(""), Settings::default());
        let s = Settings::parse("search=nonsense\nblocking=false\nunknown=1\nno equals sign");
        assert_eq!(s.search, Search::DuckDuckGo);
        assert!(!s.blocking);
    }

    #[test]
    fn site_exceptions() {
        let mut s = Settings::default();
        s.allowed_sites.insert("a.test".into());
        assert!(!s.blocks_on("a.test"));
        assert!(s.blocks_on("b.test"));
        s.blocking = false;
        assert!(!s.blocks_on("b.test"));
    }
}
