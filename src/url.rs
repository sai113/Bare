//! Address-bar parsing and HTTPS-only rules. Pure functions, unit tested.

use std::collections::HashSet;

pub const BLANK: &str = "about:blank";
#[cfg(test)]
const SEARCH: &str = "https://duckduckgo.com/?q=";

/// Turns whatever the user typed into a URL to load.
/// Only http(s) and about:blank are accepted; anything else (javascript:, file:, data:, ...)
/// is treated as a search query (appended to `search`) so the address bar can never run code.
pub fn resolve(input: &str, search: &str) -> String {
    let s = input.trim();
    if s.is_empty() || s.eq_ignore_ascii_case(BLANK) {
        return BLANK.into();
    }
    let lower = s.to_ascii_lowercase();
    if lower.starts_with("https://") || lower.starts_with("http://") {
        return s.into();
    }
    if !s.contains(char::is_whitespace) && !lower.contains(':') || looks_like_host_port(&lower) {
        let host = host_of_bare(&lower);
        if is_local(host) {
            return format!("http://{s}");
        }
        if host.contains('.') && !host.starts_with('.') && !host.ends_with('.') {
            return format!("https://{s}");
        }
    }
    format!("{search}{}", encode(s))
}

/// `example.com:8080/path` – a bare host with a numeric port.
fn looks_like_host_port(s: &str) -> bool {
    match s.split_once(':') {
        Some((host, rest)) => {
            let port: String = rest.chars().take_while(char::is_ascii_digit).collect();
            !host.is_empty() && !port.is_empty() && !s.contains(char::is_whitespace)
        }
        None => false,
    }
}

fn host_of_bare(s: &str) -> &str {
    let end = s.find(['/', '?', '#']).unwrap_or(s.len());
    let authority = &s[..end];
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    if authority.starts_with('[') {
        return authority.find(']').map_or(authority, |i| &authority[..=i]);
    }
    authority.split(':').next().unwrap_or(authority)
}

/// Host part of an http(s) URL, lowercased by the caller.
pub fn host(url: &str) -> Option<&str> {
    let (_, rest) = url.split_once("://")?;
    Some(host_of_bare(rest))
}

pub fn is_local(host: &str) -> bool {
    host == "localhost" || host.ends_with(".localhost") || host == "127.0.0.1" || host == "[::1]"
}

/// Does a cookie for `cookie_domain` ("example.com", ".example.com", "www.example.com") reach
/// pages on `host`? True for the host itself and its parent domains.
pub fn cookie_applies(host: &str, cookie_domain: &str) -> bool {
    let host = host.to_ascii_lowercase();
    let domain = cookie_domain.trim_start_matches('.').to_ascii_lowercase();
    !domain.is_empty() && (host == domain || host.ends_with(&format!(".{domain}")))
}

/// Pop-ups and `target=_blank` links open as tabs that Bare loads itself, which skips
/// Chromium's checks on where a page may send you. So only web addresses, blank pages, `blob:`
/// links (generated files) and `view-source:` of web pages (the context menu's View source) are
/// allowed: never local files, `data:` pages (a phishing trick Chrome blocks), `javascript:` or
/// browser-internal pages.
pub fn popup_allowed(url: &str) -> bool {
    let url = url.trim().to_ascii_lowercase();
    url.is_empty()
        || url == "about:blank"
        || ["https://", "http://", "blob:https://", "blob:http://", "view-source:https://", "view-source:http://"].iter().any(|p| url.starts_with(p))
}

/// HTTPS-only mode: returns the https:// version of a plain-http URL, unless the host is
/// local or the user explicitly allowed it this session.
pub fn upgrade(url: &str, allowed: &HashSet<String>) -> Option<String> {
    if url.len() < 7 || !url[..7].eq_ignore_ascii_case("http://") {
        return None;
    }
    let lower = url.to_ascii_lowercase();
    let host = host(&lower)?;
    if is_local(host) || allowed.contains(host) {
        return None;
    }
    Some(format!("https://{}", &url[7..]))
}

pub fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// A `file:///` URL for a local path (for files opened from Explorer, never the address bar).
pub fn file_url(path: &std::path::Path) -> String {
    let p = path.to_string_lossy().replace('\\', "/");
    let mut out = String::from("file:///");
    for c in p.trim_start_matches('/').chars() {
        match c {
            '%' => out.push_str("%25"),
            ' ' => out.push_str("%20"),
            '#' => out.push_str("%23"),
            '?' => out.push_str("%3F"),
            c => out.push(c),
        }
    }
    out
}

/// Decodes `%XX` escapes and `+` (as space).
pub fn decode(s: &str) -> String {
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match (b[i], b.get(i + 1).copied().and_then(hex), b.get(i + 2).copied().and_then(hex)) {
            (b'%', Some(h), Some(l)) => {
                out.push(h << 4 | l);
                i += 3;
            }
            (b'+', ..) => {
                out.push(b' ');
                i += 1;
            }
            (c, ..) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The `key=value` pairs of a URL's query string, decoded.
pub fn query(url: &str) -> Vec<(String, String)> {
    let Some((_, q)) = url.split_once('?') else { return Vec::new() };
    let q = q.split('#').next().unwrap_or(q);
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (decode(k), decode(v))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookies_for_a_site() {
        assert!(cookie_applies("www.example.com", ".example.com"));
        assert!(cookie_applies("www.example.com", "www.example.com"));
        assert!(cookie_applies("WWW.Example.com", "example.com"));
        assert!(!cookie_applies("www.example.com", "accounts.example.com"));
        assert!(!cookie_applies("www.example.com", "ample.com"));
        assert!(!cookie_applies("example.com", "other.com"));
        assert!(!cookie_applies("example.com", "."));
    }

    #[test]
    fn popups() {
        for ok in ["https://a.test/x", "HTTP://a.test", "", "about:blank", "blob:https://a.test/1234"] {
            assert!(popup_allowed(ok), "{ok}");
        }
        for bad in ["file:///C:/Users/me/secret.txt", "data:text/html,<h1>Bank login</h1>", "javascript:alert(1)", " data:,x", "chrome://settings", "view-source:file:///C:/x", "blob:null/123"] {
            assert!(!popup_allowed(bad), "{bad}");
        }
    }

    #[test]
    fn resolves_urls_and_searches() {
        assert_eq!(resolve("example.com", SEARCH), "https://example.com");
        assert_eq!(resolve("example.com/a?b=1", SEARCH), "https://example.com/a?b=1");
        assert_eq!(resolve("example.com:8443/x", SEARCH), "https://example.com:8443/x");
        assert_eq!(resolve("https://a.org", SEARCH), "https://a.org");
        assert_eq!(resolve("localhost:3000", SEARCH), "http://localhost:3000");
        assert_eq!(resolve("  ", SEARCH), BLANK);
        assert_eq!(resolve("rust lang", SEARCH), "https://duckduckgo.com/?q=rust+lang");
        assert_eq!(resolve("hello", SEARCH), "https://duckduckgo.com/?q=hello");
        assert_eq!(resolve("c++", SEARCH), "https://duckduckgo.com/?q=c%2B%2B");
    }

    #[test]
    fn address_bar_never_runs_code() {
        for evil in ["javascript:alert(1)", "file:///C:/Windows", "data:text/html,<b>x</b>"] {
            assert!(resolve(evil, SEARCH).starts_with(SEARCH), "{evil}");
        }
    }

    #[test]
    fn query_strings() {
        assert_eq!(decode("a%20b+c%2B%zz%"), "a b c+%zz%");
        assert_eq!(query("https://x.test/p?a=1&b=hello+w%C3%B6rld&c#frag"), vec![("a".into(), "1".into()), ("b".into(), "hello wörld".into()), ("c".into(), "".into())]);
        assert!(query("https://x.test/").is_empty());
        assert_eq!(decode(&encode("a b/&=ü")), "a b/&=ü");
    }

    #[test]
    fn file_urls() {
        let p = std::path::Path::new(r"C:\Users\me\My Docs\a#1 100%.html");
        assert_eq!(file_url(p), "file:///C:/Users/me/My%20Docs/a%231%20100%25.html");
    }

    #[test]
    fn https_only() {
        let mut allowed = HashSet::new();
        assert_eq!(upgrade("http://a.com/x", &allowed).as_deref(), Some("https://a.com/x"));
        assert_eq!(upgrade("HTTP://A.com", &allowed).as_deref(), Some("https://A.com"));
        assert_eq!(upgrade("https://a.com", &allowed), None);
        assert_eq!(upgrade("http://localhost:8080", &allowed), None);
        assert_eq!(upgrade("http://user@127.0.0.1/", &allowed), None);
        allowed.insert("a.com".into());
        assert_eq!(upgrade("http://a.com:81/", &allowed), None);
    }
}
