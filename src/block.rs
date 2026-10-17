//! Ad and tracker blocking with Brave's adblock engine, which understands the same filter
//! syntax as uBlock Origin and Adblock Plus (EasyList, EasyPrivacy) plus hosts-style lists.
//!
//! Network rules cancel requests before they're sent. Cosmetic rules hide leftover ad
//! elements: site-specific selectors come straight from the lists, and generic ones are
//! matched against the class names and ids actually present on the page.

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock, RwLock},
    thread,
};

use adblock::{
    Engine,
    lists::{FilterFormat, FilterSet, ParseOptions},
    request::Request,
};

/// The active engine. Swapped as a whole when optional lists are switched on or off.
static ENGINE: RwLock<Option<Arc<Engine>>> = RwLock::new(None);
static CACHE_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Filter lists in the app's `data` folder, always on.
const LISTS: [(&str, FilterFormat); 5] = [
    ("easylist.txt", FilterFormat::Standard),
    ("easyprivacy.txt", FilterFormat::Standard),
    ("hagezi-pro.txt", FilterFormat::Standard),
    ("bare-extra.txt", FilterFormat::Standard),
    ("blocklist.txt", FilterFormat::Hosts),
];
/// Optional lists, switched on in Settings.
const SOCIAL_LISTS: [&str; 2] = ["fanboy-social.txt", "bare-social.txt"];
const COOKIE_LISTS: [&str; 2] = ["easylist-cookie.txt", "bare-cookies.txt"];

/// Which optional lists are on.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Extras {
    /// Facebook/Twitter/Pinterest buttons, widgets and trackers.
    pub social: bool,
    /// Cookie-consent banners.
    pub cookie_banners: bool,
}

fn list_names(extras: Extras) -> Vec<(&'static str, FilterFormat)> {
    let mut names = LISTS.to_vec();
    if extras.social {
        names.extend(SOCIAL_LISTS.map(|n| (n, FilterFormat::Standard)));
    }
    if extras.cookie_banners {
        names.extend(COOKIE_LISTS.map(|n| (n, FilterFormat::Standard)));
    }
    names
}

fn engine() -> Option<Arc<Engine>> {
    ENGINE.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Prefix of the console message the cosmetic script uses to report class names and ids.
pub const COSMETIC_PREFIX: &str = "\u{2063}bare-cosmetic\u{2063}";

/// `data` next to the executable (packaged app), else the repo's `data` (development).
fn data_dir() -> Option<PathBuf> {
    let packaged = std::env::current_exe().ok()?.parent()?.join("data");
    if packaged.exists() {
        return Some(packaged);
    }
    let dev = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("data");
    dev.exists().then_some(dev)
}

/// Loads the engine. Call once in the browser process before any page loads. A compiled copy
/// is cached in `cache_dir` so later starts skip parsing ~300k rules.
pub fn init(cache_dir: &Path, extras: Extras) {
    let _ = CACHE_DIR.set(cache_dir.to_path_buf());
    let engine = load(cache_dir, extras);
    *ENGINE.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(engine));
}

/// Switches optional lists on or off. The new engine is built in the background (a couple of
/// seconds the first time); the current one keeps blocking until it's ready.
pub fn reload(extras: Extras) {
    let Some(dir) = CACHE_DIR.get().cloned() else { return };
    thread::spawn(move || {
        let engine = load(&dir, extras);
        *ENGINE.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(engine));
    });
}

/// The engine for this list selection, from the cache if possible.
fn load(cache_dir: &Path, extras: Extras) -> Engine {
    let texts: Vec<(String, FilterFormat)> = data_dir()
        .map(|dir| {
            list_names(extras)
                .into_iter()
                .filter_map(|(name, format)| Some((fs::read_to_string(dir.join(name)).ok()?, format)))
                .collect()
        })
        .unwrap_or_default();
    let cache = cache_dir.join(format!("adblock-{:016x}.dat", fingerprint(&texts)));
    if let Ok(bytes) = fs::read(&cache) {
        let mut engine = Engine::default();
        if engine.deserialize(&bytes).is_ok() {
            return engine;
        }
    }
    let engine = engine_from(texts);
    // One cache per list selection: keep the last few, drop older ones.
    if let Ok(entries) = fs::read_dir(cache_dir) {
        let mut old: Vec<_> = entries
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("adblock-"))
            .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
            .collect();
        old.sort();
        for (_, path) in old.iter().rev().skip(3) {
            let _ = fs::remove_file(path);
        }
    }
    let _ = fs::create_dir_all(cache_dir);
    let _ = fs::write(&cache, engine.serialize());
    engine
}

fn engine_from(texts: Vec<(String, FilterFormat)>) -> Engine {
    let mut set = FilterSet::new(false);
    for (text, format) in texts {
        set.add_filter_list(text, ParseOptions { format, ..Default::default() });
    }
    Engine::new_with_filter_set(set)
}

/// FNV-1a over the list contents and engine version, to key the on-disk cache.
fn fingerprint(texts: &[(String, FilterFormat)]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    let version = "adblock-0.13.3";
    for b in version.bytes().chain(texts.iter().flat_map(|(t, _)| t.bytes())) {
        h = (h ^ b as u64).wrapping_mul(0x100000001b3);
    }
    h
}

/// Should a request of `kind` (e.g. "script", "image") to `url`, made by a page at
/// `source_url`, be blocked?
pub fn should_block(url: &str, source_url: &str, kind: &str) -> bool {
    let Some(engine) = engine() else { return false };
    let Ok(request) = Request::new(url, source_url, kind, "get") else { return false };
    engine.check_network_request(&request).should_block()
}

/// Site-specific hide selectors for `url`, the exceptions that apply to it, and whether
/// generic cosmetic rules are allowed there.
pub fn site_cosmetics(url: &str) -> (Vec<String>, HashSet<String>, bool) {
    let Some(engine) = engine() else { return (Vec::new(), HashSet::new(), false) };
    let r = engine.url_cosmetic_resources(url);
    (r.hide_selectors.into_iter().collect(), r.exceptions, !r.generichide)
}

/// Generic hide selectors that match the given class names and ids.
pub fn generic_selectors(classes: &[&str], ids: &[&str], exceptions: &HashSet<String>) -> Vec<String> {
    let Some(engine) = engine() else { return Vec::new() };
    engine.hidden_class_id_selectors(classes, ids, exceptions)
}

/// JavaScript that hides the given selectors on the current page. Uses a constructed
/// stylesheet, which the page's Content-Security-Policy doesn't block.
pub fn hide_script(selectors: &[String]) -> Option<String> {
    if selectors.is_empty() {
        return None;
    }
    let css: String = selectors.iter().map(|s| format!("{s}{{display:none!important}}\n")).collect();
    Some(format!(
        "(()=>{{try{{const s=new CSSStyleSheet();s.replaceSync({});document.adoptedStyleSheets=[...document.adoptedStyleSheets,s];}}catch(e){{}}}})();",
        js_string(&css)
    ))
}

/// Script injected into every page: reports class names and ids (now and as the page
/// changes) so matching generic hide rules can be applied.
pub fn observer_script() -> String {
    format!(
        r#"(()=>{{if(window.__bareCosmetic)return;window.__bareCosmetic=1;
const P={prefix},seen=new Set(),cls=[],ids=[];let t=0;
const visit=e=>{{if(e.id&&!seen.has('#'+e.id)){{seen.add('#'+e.id);ids.push(e.id);}}
if(e.classList)for(const c of e.classList)if(!seen.has('.'+c)){{seen.add('.'+c);cls.push(c);}}}};
const scan=n=>{{if(n.nodeType!==1)return;visit(n);for(const e of n.querySelectorAll('[class],[id]'))visit(e);}};
const flush=()=>{{t=0;if(cls.length||ids.length){{console.debug(P+cls.splice(0).join(' ')+'\t'+ids.splice(0).join(' '));}}}};
const later=()=>{{if(!t)t=setTimeout(flush,300);}};
const start=()=>{{scan(document.documentElement);flush();
new MutationObserver(ms=>{{for(const m of ms){{if(m.type==='attributes')visit(m.target);else for(const n of m.addedNodes)scan(n);}}later();}})
.observe(document.documentElement,{{childList:true,subtree:true,attributes:true,attributeFilter:['class','id']}});}};
if(document.readyState==='loading')document.addEventListener('DOMContentLoaded',start,{{once:true}});else start();}})();"#,
        prefix = js_string(COSMETIC_PREFIX)
    )
}

/// Escapes a string as a JavaScript string literal (including the quotes).
pub fn js_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '<' => out.push_str("\\u003c"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine(rules: &str) -> Engine {
        engine_from(vec![(rules.to_string(), FilterFormat::Standard)])
    }

    fn blocks(e: &Engine, url: &str, source: &str, kind: &str) -> bool {
        e.check_network_request(&Request::new(url, source, kind, "get").unwrap()).should_block()
    }

    #[test]
    fn network_rules() {
        let e = engine("||ads.example^\n/banner/*$image,third-party\n@@||ads.example/allowed.js");
        assert!(blocks(&e, "https://ads.example/x.js", "https://news.test/", "script"));
        assert!(blocks(&e, "https://sub.ads.example/y.png", "https://news.test/", "image"));
        assert!(!blocks(&e, "https://ads.example/allowed.js", "https://news.test/", "script"));
        // Option filters: images only, third-party only.
        assert!(blocks(&e, "https://cdn.test/banner/1.png", "https://news.test/", "image"));
        assert!(!blocks(&e, "https://cdn.test/banner/1.js", "https://news.test/", "script"));
        assert!(!blocks(&e, "https://news.test/banner/1.png", "https://news.test/", "image"));
    }

    #[test]
    fn hosts_lists() {
        let e = engine_from(vec![("# comment\ntracker.test\n".into(), FilterFormat::Hosts)]);
        assert!(blocks(&e, "https://tracker.test/p", "https://site.test/", "ping"));
        assert!(blocks(&e, "https://a.tracker.test/p", "https://site.test/", "image"));
        assert!(!blocks(&e, "https://site.test/p", "https://site.test/", "image"));
    }

    #[test]
    fn cosmetic_rules() {
        let e = engine("news.test##.sponsored\n##.ad-banner\nnews.test#@#.ad-banner");
        let r = e.url_cosmetic_resources("https://news.test/a");
        assert!(r.hide_selectors.contains(".sponsored"));
        assert!(e.hidden_class_id_selectors(["ad-banner"], Vec::<&str>::new(), &r.exceptions).is_empty());
        let other = e.url_cosmetic_resources("https://other.test/");
        assert_eq!(e.hidden_class_id_selectors(["ad-banner"], Vec::<&str>::new(), &other.exceptions), vec![".ad-banner"]);
    }

    #[test]
    fn scripts_escape_selectors() {
        let s = hide_script(&["a[href=\"x\"]".into()]).unwrap();
        assert!(s.contains(r#"a[href=\"x\"]"#));
        let quoted = js_string("</script>\nline");
        assert!(!quoted.contains('<') && !quoted.contains('\n'));
        assert!(quoted.starts_with('"') && quoted.ends_with('"'));
        assert!(hide_script(&[]).is_none());
    }

    fn shipped_engine(extras: Extras) -> Engine {
        let dir = data_dir().expect("data folder");
        engine_from(list_names(extras).into_iter().map(|(n, f)| (fs::read_to_string(dir.join(n)).unwrap(), f)).collect())
    }

    #[test]
    fn bare_lists() {
        let dir = data_dir().expect("data folder");
        let list = |name: &str| (fs::read_to_string(dir.join(name)).unwrap(), FilterFormat::Standard);
        let extra = engine_from(vec![list("bare-extra.txt")]);
        let page = "https://news.test/";
        for url in ["https://s.youtube.com/api/stats/ads", "https://redirector.googlevideo.com/x", "https://bnc.lt/a", "https://vk.com/rtrg?p=1"] {
            assert!(blocks(&extra, url, page, "xmlhttprequest"), "{url}");
        }
        // Instagram's API is blocked on other sites, never on Instagram itself.
        assert!(blocks(&extra, "https://i.instagram.com/api/v1/x", page, "xmlhttprequest"));
        assert!(!blocks(&extra, "https://i.instagram.com/api/v1/x", "https://www.instagram.com/", "xmlhttprequest"));
        // The rest of vk.com is untouched.
        assert!(!blocks(&extra, "https://vk.com/feed", page, "document"));

        let cookies = engine_from(vec![list("bare-cookies.txt")]);
        assert!(blocks(&cookies, "https://consent.cookiebot.com/uc.js", page, "script"));
        assert!(blocks(&cookies, "https://app.usercentrics.eu/browser-ui/latest/loader.js", page, "script"));
        assert!(!blocks(&cookies, "https://www.cookiebot.com/en/", "https://www.cookiebot.com/", "document"));
    }

    #[test]
    fn optional_social_list() {
        let fb = "https://connect.facebook.net/en_US/sdk.js";
        assert!(!blocks(&shipped_engine(Extras::default()), fb, "https://news.test/", "script"));
        let social = shipped_engine(Extras { social: true, ..Default::default() });
        assert!(blocks(&social, fb, "https://news.test/", "script"));
        // Facebook itself keeps working.
        assert!(!blocks(&social, fb, "https://www.facebook.com/", "script"));
        let pin = "https://widgets.pinterest.com/v3/pidgets/boards/x";
        assert!(blocks(&social, pin, "https://news.test/", "script"));
        assert!(!blocks(&social, pin, "https://www.pinterest.com/", "script"));
    }

    /// Developer check, not run by default: lists hosts the shipped lists would let through.
    /// `BARE_CHECK_HOSTS=hosts.tsv cargo test -p bare --bin bare --no-default-features -- --ignored check_hosts --nocapture`
    /// where each line is `category<TAB>host` (or just `host`).
    #[test]
    #[ignore]
    fn check_hosts() {
        let file = std::env::var("BARE_CHECK_HOSTS").expect("set BARE_CHECK_HOSTS");
        // BARE_CHECK_EXTRAS=social,cookies also loads the optional lists.
        let opt = std::env::var("BARE_CHECK_EXTRAS").unwrap_or_default();
        let extras = Extras { social: opt.contains("social"), cookie_banners: opt.contains("cookies") };
        let e = shipped_engine(extras);
        let (mut total, mut open) = (0, Vec::new());
        for line in fs::read_to_string(file).unwrap().lines() {
            let (category, host) = line.rsplit_once('\t').unwrap_or(("", line));
            let host = host.trim();
            if host.is_empty() {
                continue;
            }
            total += 1;
            if !blocks(&e, &format!("https://{host}/"), "https://example.test/", "xmlhttprequest") {
                open.push(format!("{category}\t{host}"));
            }
        }
        println!("blocked {}/{total}", total - open.len());
        for o in open {
            println!("NOT BLOCKED\t{o}");
        }
    }

    #[test]
    fn shipped_lists_load() {
        let e = shipped_engine(Extras::default());
        // Bare's own list (always on).
        for host in ["us.i.posthog.com", "udc.yahoo.com", "xp.apple.com", "settings-win.data.microsoft.com"] {
            assert!(blocks(&e, &format!("https://{host}/x"), "https://news.test/", "xmlhttprequest"), "{host}");
        }
        // Things that must keep working.
        // (YouTube's video servers; its redirector is blocked, which playback doesn't need.)
        for url in ["https://rr3---sn-a5mekn6s.googlevideo.com/videoplayback", "https://click.mailchimp.com/track/click"] {
            assert!(!blocks(&e, url, "https://news.test/", "xmlhttprequest"), "{url}");
        }
        assert!(blocks(&e, "https://pagead2.googlesyndication.com/pagead/js/adsbygoogle.js", "https://news.test/", "script"));
        assert!(blocks(&e, "https://www.google-analytics.com/analytics.js", "https://news.test/", "script"));
        assert!(!blocks(&e, "https://news.test/app.js", "https://news.test/", "script"));
        // Device and app telemetry (from the d3ward ad-block test), covered by HaGeZi Pro.
        for host in [
            "business-api.tiktok.com",
            "log.byteoversea.com",
            "appmetrica.yandex.ru",
            "metrika.yandex.ru",
            "log.fc.yahoo.com",
            "udcm.yahoo.com",
            "data.mistat.india.xiaomi.com",
            "data.mistat.rus.xiaomi.com",
            "grs.hicloud.com",
            "api-adservices.apple.com",
            "books-analytics-events.apple.com",
        ] {
            assert!(blocks(&e, &format!("https://{host}/x"), "https://news.test/", "xmlhttprequest"), "{host}");
        }
    }
}
