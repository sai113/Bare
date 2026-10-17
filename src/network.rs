//! Secure DNS (DNS over HTTPS) and proxy settings, applied to Chromium at startup and whenever
//! they change in Settings.

use cef::*;

/// A DNS-over-HTTPS provider: (id, name, URL template).
pub const DNS_PROVIDERS: [(&str, &str, &str); 8] = [
    ("cloudflare", "Cloudflare (1.1.1.1)", "https://chromium.dns.cloudflare.com/dns-query"),
    ("google", "Google (Public DNS)", "https://dns.google/dns-query{?dns}"),
    ("quad9", "Quad9 (9.9.9.9)", "https://dns.quad9.net/dns-query"),
    ("adguard", "AdGuard (also blocks ads)", "https://dns.adguard-dns.com/dns-query"),
    ("mullvad", "Mullvad", "https://dns.mullvad.net/dns-query"),
    ("nextdns", "NextDNS", "https://chromium.dns.nextdns.io"),
    ("cleanbrowsing", "CleanBrowsing (Family Filter)", "https://doh.cleanbrowsing.org/doh/family-filter{?dns}"),
    ("opendns", "OpenDNS", "https://doh.opendns.com/dns-query{?dns}"),
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Dns {
    /// Plain DNS from the operating system.
    Off,
    /// Like Chrome's default: secure DNS with the system's provider when it supports it.
    Automatic,
    /// Always use this provider (id from `DNS_PROVIDERS`).
    Provider(String),
    /// Always use this DoH URL template.
    Custom(String),
}

impl Dns {
    pub fn parse(s: &str) -> Dns {
        match s.trim() {
            "off" => Dns::Off,
            "" | "automatic" => Dns::Automatic,
            s if s.starts_with("custom:") => {
                let url = &s["custom:".len()..];
                if valid_doh(url) { Dns::Custom(url.to_string()) } else { Dns::Automatic }
            }
            s if DNS_PROVIDERS.iter().any(|(id, ..)| *id == s) => Dns::Provider(s.to_string()),
            _ => Dns::Automatic,
        }
    }

    pub fn serialize(&self) -> String {
        match self {
            Dns::Off => "off".into(),
            Dns::Automatic => "automatic".into(),
            Dns::Provider(id) => id.clone(),
            Dns::Custom(url) => format!("custom:{url}"),
        }
    }

    /// Chromium's `dns_over_https.mode` and `dns_over_https.templates`.
    fn chromium_prefs(&self) -> (&'static str, String) {
        match self {
            Dns::Off => ("off", String::new()),
            Dns::Automatic => ("automatic", String::new()),
            Dns::Provider(id) => {
                let template = DNS_PROVIDERS.iter().find(|(p, ..)| p == id).map_or("", |(_, _, t)| t);
                ("secure", template.to_string())
            }
            Dns::Custom(url) => ("secure", url.clone()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Proxy {
    /// Use the operating system's proxy settings (Chrome's behaviour).
    System,
    /// Connect directly.
    Direct,
    /// A fixed server, e.g. "socks5://127.0.0.1:9050", plus hosts that bypass it.
    Manual { server: String, bypass: String },
    /// A proxy auto-config (PAC) script URL.
    Pac(String),
}

impl Proxy {
    pub fn parse(mode: &str, bypass: &str) -> Proxy {
        let mode = mode.trim();
        if mode == "direct" {
            Proxy::Direct
        } else if let Some(server) = mode.strip_prefix("manual:") {
            if valid_proxy_server(server) {
                Proxy::Manual { server: server.into(), bypass: bypass.trim().into() }
            } else {
                Proxy::System
            }
        } else if let Some(url) = mode.strip_prefix("pac:") {
            if valid_pac(url) { Proxy::Pac(url.into()) } else { Proxy::System }
        } else {
            Proxy::System
        }
    }

    /// (mode string, bypass list) as stored in settings.
    pub fn serialize(&self) -> (String, String) {
        match self {
            Proxy::System => ("system".into(), String::new()),
            Proxy::Direct => ("direct".into(), String::new()),
            Proxy::Manual { server, bypass } => (format!("manual:{server}"), bypass.clone()),
            Proxy::Pac(url) => (format!("pac:{url}"), String::new()),
        }
    }
}

fn host_part(rest: &str) -> &str {
    rest.split(['/', '?', '#']).next().unwrap_or("")
}

/// A DoH template: https:// with a host.
pub fn valid_doh(url: &str) -> bool {
    url.strip_prefix("https://").is_some_and(|rest| {
        let host = host_part(rest);
        !host.is_empty() && !host.contains(char::is_whitespace)
    })
}

/// `[scheme://]host:port`, scheme one of http, https, socks4, socks5.
pub fn valid_proxy_server(s: &str) -> bool {
    let s = s.trim();
    let rest = match s.split_once("://") {
        Some((scheme, rest)) if ["http", "https", "socks4", "socks5"].contains(&scheme) => rest,
        Some(_) => return false,
        None => s,
    };
    let Some((host, port)) = rest.rsplit_once(':') else { return false };
    let host_ok = !host.is_empty() && !host.contains(['/', ' ', '@']);
    host_ok && port.parse::<u16>().is_ok_and(|p| p > 0)
}

pub fn valid_pac(url: &str) -> bool {
    ["https://", "http://", "file:///"]
        .iter()
        .any(|p| url.strip_prefix(p).is_some_and(|rest| !host_part(rest).is_empty() || p.starts_with("file")))
        && !url.contains(char::is_whitespace)
}

fn string_value(s: &str) -> Option<Value> {
    value_create().inspect(|v| {
        v.set_string(Some(&CefString::from(s)));
    })
}

/// Applies secure DNS (a browser-wide Chromium setting). Returns false if Chromium refused.
pub fn apply_dns(dns: &Dns) -> Result<(), String> {
    let prefs = preference_manager_get_global().ok_or("no global preferences")?;
    let (mode, template) = dns.chromium_prefs();
    for (name, value) in [("dns_over_https.templates", template.as_str()), ("dns_over_https.mode", mode)] {
        let mut v = string_value(value).ok_or("no value")?;
        let mut error = CefString::from("");
        if prefs.set_preference(Some(&CefString::from(name)), Some(&mut v), Some(&mut error)) != 1 {
            return Err(format!("{name}: {}", error.to_string()));
        }
    }
    Ok(())
}

/// Applies the proxy to a request context.
pub fn apply_proxy(ctx: &RequestContext, proxy: &Proxy) -> Result<(), String> {
    let dict = dictionary_value_create().ok_or("no dictionary")?;
    let set = |k: &str, v: &str| {
        dict.set_string(Some(&CefString::from(k)), Some(&CefString::from(v)));
    };
    match proxy {
        Proxy::System => set("mode", "system"),
        Proxy::Direct => set("mode", "direct"),
        Proxy::Manual { server, bypass } => {
            set("mode", "fixed_servers");
            set("server", server);
            set("bypass_list", bypass);
        }
        Proxy::Pac(url) => {
            set("mode", "pac_script");
            set("pac_url", url);
        }
    }
    let mut value = value_create().ok_or("no value")?;
    let mut dict = dict;
    value.set_dictionary(Some(&mut dict));
    // Always pass an error string: in release builds the call fails without one.
    let mut error = CefString::from("");
    if ctx.set_preference(Some(&CefString::from("proxy")), Some(&mut value), Some(&mut error)) != 1 {
        return Err(error.to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dns_round_trip() {
        for d in [Dns::Off, Dns::Automatic, Dns::Provider("quad9".into()), Dns::Custom("https://doh.example/dns-query".into())] {
            assert_eq!(Dns::parse(&d.serialize()), d);
        }
        assert_eq!(Dns::parse("nonsense"), Dns::Automatic);
        assert_eq!(Dns::parse("custom:http://plain.example/"), Dns::Automatic);
        assert_eq!(Dns::Provider("cloudflare".into()).chromium_prefs(), ("secure", DNS_PROVIDERS[0].2.to_string()));
        assert_eq!(Dns::Off.chromium_prefs().0, "off");
    }

    #[test]
    fn proxy_round_trip() {
        for p in [
            Proxy::System,
            Proxy::Direct,
            Proxy::Manual { server: "socks5://127.0.0.1:9050".into(), bypass: "localhost;*.lan".into() },
            Proxy::Pac("https://corp.example/proxy.pac".into()),
        ] {
            let (mode, bypass) = p.serialize();
            assert_eq!(Proxy::parse(&mode, &bypass), p);
        }
        assert_eq!(Proxy::parse("manual:not a proxy", ""), Proxy::System);
    }

    #[test]
    fn validation() {
        assert!(valid_proxy_server("proxy.corp:8080"));
        assert!(valid_proxy_server("http://10.0.0.1:3128"));
        assert!(valid_proxy_server("socks5://[::1]:1080"));
        assert!(!valid_proxy_server("ftp://x:21"));
        assert!(!valid_proxy_server("proxy.corp"));
        assert!(!valid_proxy_server("proxy.corp:99999"));
        assert!(!valid_proxy_server("user@proxy:80"));
        assert!(valid_doh("https://dns.example/dns-query{?dns}"));
        assert!(!valid_doh("http://dns.example/dns-query"));
        assert!(!valid_doh("https://"));
        assert!(valid_pac("http://wpad/wpad.dat"));
        assert!(!valid_pac("javascript:alert(1)"));
    }
}
