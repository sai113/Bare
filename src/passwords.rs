//! The password vault: saved logins, kept in `<profile>/passwords.dat` encrypted with Windows
//! DPAPI (only this Windows account can decrypt it), plus sites the user said never to save for.
//!
//! Logins belong to an exact origin (`https://accounts.example.com`), so a login is only ever
//! offered on the site it was saved on. Pages report submitted logins through console messages
//! starting with [`PREFIX`]; filling happens only when the user picks a login from the 🔑 menu.

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
};

use serde::{Deserialize, Serialize};

use crate::block::js_string;

/// Marks console messages from the login-capture script.
pub const PREFIX: &str = "\u{2063}bare-login\u{2063}";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Login {
    pub origin: String,
    pub username: String,
    pub password: String,
    /// Seconds since 1970 when saved or last changed.
    #[serde(default)]
    pub saved: u64,
}

#[derive(Default, Serialize, Deserialize)]
struct Vault {
    #[serde(default)]
    logins: Vec<Login>,
    /// Origins where Bare never offers to save.
    #[serde(default)]
    never: BTreeSet<String>,
}

struct Store {
    vault: Vault,
    path: Option<PathBuf>,
}

static STORE: Mutex<Option<Store>> = Mutex::new(None);

fn with<R>(f: impl FnOnce(&mut Store) -> R) -> R {
    let mut guard = STORE.lock().unwrap_or_else(|e| e.into_inner());
    f(guard.get_or_insert_with(|| Store { vault: Vault::default(), path: None }))
}

impl Store {
    fn save(&self) {
        let (Some(path), Ok(json)) = (&self.path, serde_json::to_vec(&self.vault)) else { return };
        if let Some(sealed) = protect(&json) {
            // Write then rename, so a crash never leaves a half-written vault.
            let tmp = path.with_extension("tmp");
            if fs::write(&tmp, sealed).is_ok() {
                let _ = fs::rename(&tmp, path);
            }
        }
    }
}

/// Loads the vault from the profile folder.
pub fn init(profile: &Path) {
    let path = profile.join("passwords.dat");
    let vault = match fs::read(&path) {
        Ok(sealed) => match unprotect(&sealed).and_then(|json| serde_json::from_slice(&json).ok()) {
            Some(vault) => vault,
            None => {
                // Another Windows account's vault, or damaged: keep it aside rather than
                // overwrite it with the next save.
                let _ = fs::rename(&path, profile.join("passwords.unreadable.dat"));
                Vault::default()
            }
        },
        Err(_) => Vault::default(),
    };
    *STORE.lock().unwrap_or_else(|e| e.into_inner()) = Some(Store { vault, path: Some(path) });
}

/// `scheme://host[:port]` of a web address (lowercase, default ports dropped), or `None` for
/// anything that isn't http(s).
pub fn origin(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "https" && scheme != "http" {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit('@').next()?.to_ascii_lowercase();
    let default_port = if scheme == "https" { ":443" } else { ":80" };
    let authority = authority.strip_suffix(default_port).unwrap_or(&authority);
    if authority.is_empty() {
        return None;
    }
    Some(format!("{scheme}://{authority}"))
}

/// The host part of an origin, for showing to the user.
pub fn site(origin: &str) -> &str {
    origin.split_once("://").map_or(origin, |(_, h)| h)
}

/// Usernames saved for `origin`.
pub fn usernames(origin: &str) -> Vec<String> {
    with(|s| s.vault.logins.iter().filter(|l| l.origin == origin).map(|l| l.username.clone()).collect())
}

pub fn has_logins(origin: &str) -> bool {
    with(|s| s.vault.logins.iter().any(|l| l.origin == origin))
}

pub fn password(origin: &str, username: &str) -> Option<String> {
    with(|s| s.vault.logins.iter().find(|l| l.origin == origin && l.username == username).map(|l| l.password.clone()))
}

/// Every saved login, by site then username (passwords included; callers decide what to show).
pub fn all() -> Vec<Login> {
    let mut list = with(|s| s.vault.logins.clone());
    list.sort_by(|a, b| (site(&a.origin), &a.username).cmp(&(site(&b.origin), &b.username)));
    list
}

pub fn never_list() -> Vec<String> {
    with(|s| s.vault.never.iter().cloned().collect())
}

/// What to ask after a login form was submitted.
#[derive(Debug, PartialEq)]
pub enum Offer {
    /// Nothing: already saved, or the user said never for this site.
    None,
    Save,
    /// The username is saved with a different password.
    Update,
}

pub fn offer(origin: &str, username: &str, password: &str) -> Offer {
    with(|s| {
        if password.is_empty() || s.vault.never.contains(origin) {
            return Offer::None;
        }
        match s.vault.logins.iter().find(|l| l.origin == origin && l.username == username) {
            Some(l) if l.password == password => Offer::None,
            Some(_) => Offer::Update,
            None => Offer::Save,
        }
    })
}

/// Saves a login (replacing the password if the username is already saved for that site).
pub fn save(origin: &str, username: &str, password: &str) {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    with(|s| {
        match s.vault.logins.iter_mut().find(|l| l.origin == origin && l.username == username) {
            Some(l) => (l.password, l.saved) = (password.to_string(), now),
            None => s.vault.logins.push(Login { origin: origin.into(), username: username.into(), password: password.into(), saved: now }),
        }
        s.save();
    });
}

pub fn remove(origin: &str, username: &str) {
    with(|s| {
        s.vault.logins.retain(|l| !(l.origin == origin && l.username == username));
        s.save();
    });
}

pub fn set_never(origin: &str, never: bool) {
    with(|s| {
        if never {
            s.vault.never.insert(origin.to_string());
        } else {
            s.vault.never.remove(origin);
        }
        s.save();
    });
}

/// A login submitted on a page, as reported by the capture script.
#[derive(Debug, PartialEq, Deserialize)]
pub struct Submitted {
    /// `location.origin` of the page (checked against the tab's address before use).
    pub origin: String,
    #[serde(default)]
    pub username: String,
    pub password: String,
}

/// Parses a console message from the capture script: `None` if it isn't one, otherwise the
/// page load's token it carries and the login (if well-formed).
pub fn parse_report(message: &str) -> Option<(&str, Option<Submitted>)> {
    let (token, json) = message.strip_prefix(PREFIX)?.split_once('\t')?;
    let login = serde_json::from_str::<Submitted>(json).ok().filter(|s| {
        !s.password.is_empty() && s.password.len() <= 1000 && s.username.len() <= 500
    });
    Some((token, login))
}

/// Watches the page for login forms being submitted (a form submit, a click on a button, or
/// Enter in a password field) and reports the username and password once per pair.
///
/// Reports carry `token`, made fresh for each page load and only known to this script in the
/// top-level page, so frames from other sites embedded in the page can't fake a login.
pub fn capture_script(token: &str) -> String {
    format!(
        r#"(function(){{if(window.__bareLogin)return;window.__bareLogin=1;
var P={prefix},last='',log=console.log.bind(console);
function visible(e){{return !!(e.offsetWidth||e.offsetHeight||e.getClientRects().length)}}
function filled(root){{return [].filter.call(root.querySelectorAll('input[type=password]'),function(e){{return e.value&&visible(e)}})}}
function userFor(pw,root){{
  var inputs=[].slice.call(root.querySelectorAll('input')),i=inputs.indexOf(pw),u='';
  for(var j=i-1;j>=0;j--){{var e=inputs[j],t=(e.type||'text').toLowerCase();
    if((t==='text'||t==='email'||t==='tel')&&e.value&&visible(e)){{u=e.value;break}}}}
  if(!u){{var h=document.querySelector('input[autocomplete~=username]');if(h&&h.value)u=h.value}}
  return u.trim()}}
function report(root){{
  var pws=filled(root);if(!pws.length&&root!==document)pws=filled(document);if(!pws.length)return;
  var pw=pws[pws.length-1],form=pw.form||document,u=userFor(pws[0],form);
  var key=u+'\n'+pw.value;if(key===last)return;last=key;
  log(P+JSON.stringify({{origin:location.origin,username:u,password:pw.value}}))}}
document.addEventListener('submit',function(e){{report(e.target)}},true);
document.addEventListener('click',function(e){{var b=e.target&&e.target.closest&&e.target.closest('button,input[type=submit],input[type=button],[role=button]');if(b)report(b.form||document)}},true);
document.addEventListener('keydown',function(e){{if(e.key==='Enter'&&e.target&&e.target.type==='password')report(e.target.form||document)}},true);
}})();"#,
        prefix = js_string(&format!("{PREFIX}{token}\t"))
    )
}

/// Fills `username` and `password` into the page's login form, but only if the page is still
/// on `origin` (it could have navigated since the user clicked).
pub fn fill_script(origin: &str, username: &str, password: &str) -> String {
    format!(
        r#"(function(o,u,p){{if(location.origin!==o)return;
function visible(e){{return !!(e.offsetWidth||e.offsetHeight||e.getClientRects().length)}}
var set=Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,'value').set;
function put(e,v){{e.focus();set.call(e,v);e.dispatchEvent(new Event('input',{{bubbles:true}}));e.dispatchEvent(new Event('change',{{bubbles:true}}))}}
var pw=[].filter.call(document.querySelectorAll('input[type=password]'),visible)[0];
var root=pw&&pw.form||document,inputs=[].slice.call(root.querySelectorAll('input')),user=null;
var from=pw?inputs.indexOf(pw):inputs.length;
for(var j=from-1;j>=0;j--){{var e=inputs[j],t=(e.type||'text').toLowerCase();
  if((t==='text'||t==='email'||t==='tel')&&visible(e)&&!e.readOnly&&!e.disabled){{user=e;break}}}}
if(!user)user=[].filter.call(document.querySelectorAll('input[autocomplete~=username],input[type=email]'),visible)[0]||null;
if(user&&u)put(user,u);if(pw)put(pw,p);}})({},{},{});"#,
        js_string(origin),
        js_string(username),
        js_string(password)
    )
}

// ---- encryption at rest ----

const ENTROPY: &[u8] = b"Bare password vault v1";

/// Encrypts with DPAPI for the current Windows user.
#[cfg(windows)]
fn protect(data: &[u8]) -> Option<Vec<u8>> {
    use windows_sys::Win32::Security::Cryptography::{CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData};
    let input = CRYPT_INTEGER_BLOB { cbData: u32::try_from(data.len()).ok()?, pbData: data.as_ptr() as *mut u8 };
    let entropy = CRYPT_INTEGER_BLOB { cbData: ENTROPY.len() as u32, pbData: ENTROPY.as_ptr() as *mut u8 };
    let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
    // SAFETY: the input blobs point at live slices; DPAPI allocates `out`, freed below.
    let ok = unsafe { CryptProtectData(&input, std::ptr::null(), &entropy, std::ptr::null(), std::ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out) };
    take_blob(ok != 0, out)
}

#[cfg(windows)]
fn unprotect(data: &[u8]) -> Option<Vec<u8>> {
    use windows_sys::Win32::Security::Cryptography::{CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptUnprotectData};
    let input = CRYPT_INTEGER_BLOB { cbData: u32::try_from(data.len()).ok()?, pbData: data.as_ptr() as *mut u8 };
    let entropy = CRYPT_INTEGER_BLOB { cbData: ENTROPY.len() as u32, pbData: ENTROPY.as_ptr() as *mut u8 };
    let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
    // SAFETY: as in `protect`.
    let ok = unsafe { CryptUnprotectData(&input, std::ptr::null_mut(), &entropy, std::ptr::null(), std::ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out) };
    take_blob(ok != 0, out)
}

/// Copies a DPAPI output blob and frees it.
#[cfg(windows)]
fn take_blob(ok: bool, blob: windows_sys::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB) -> Option<Vec<u8>> {
    if blob.pbData.is_null() {
        return None;
    }
    // SAFETY: DPAPI returned `cbData` bytes at `pbData`, allocated with LocalAlloc.
    let bytes = unsafe { std::slice::from_raw_parts(blob.pbData, blob.cbData as usize) }.to_vec();
    unsafe { windows_sys::Win32::Foundation::LocalFree(blob.pbData as _) };
    ok.then_some(bytes)
}

/// No OS key store wired up on other platforms yet: nothing is saved.
#[cfg(not(windows))]
fn protect(_data: &[u8]) -> Option<Vec<u8>> {
    None
}

#[cfg(not(windows))]
fn unprotect(_data: &[u8]) -> Option<Vec<u8>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins() {
        assert_eq!(origin("https://Accounts.Example.com/login?next=/#x").as_deref(), Some("https://accounts.example.com"));
        assert_eq!(origin("https://example.com:443/").as_deref(), Some("https://example.com"));
        assert_eq!(origin("https://example.com:8443/a").as_deref(), Some("https://example.com:8443"));
        assert_eq!(origin("http://localhost:80").as_deref(), Some("http://localhost"));
        assert_eq!(origin("https://user:pw@example.com/").as_deref(), Some("https://example.com"));
        assert_eq!(origin("data:text/html,hi"), None);
        assert_eq!(origin("file:///C:/x.html"), None);
        assert_eq!(origin("https://"), None);
        assert_eq!(site("https://example.com:8443"), "example.com:8443");
    }

    #[test]
    fn reports() {
        let msg = format!("{PREFIX}tok\t{}", r#"{"origin":"https://a.test","username":"me","password":"pw"}"#);
        assert_eq!(
            parse_report(&msg),
            Some(("tok", Some(Submitted { origin: "https://a.test".into(), username: "me".into(), password: "pw".into() })))
        );
        assert_eq!(parse_report(&format!("{PREFIX}tok\t{}", r#"{"origin":"https://a.test","password":""}"#)), Some(("tok", None)));
        assert_eq!(parse_report(&format!("{PREFIX}tok\tgarbage")), Some(("tok", None)));
        assert_eq!(parse_report(&format!("{PREFIX}no-token")), None);
        assert_eq!(parse_report(r#"{"origin":"https://a.test","password":"pw"}"#), None);
    }

    #[test]
    fn scripts_escape_values() {
        let s = fill_script("https://a.test", "me\"</script>", "p'\\w\n");
        assert!(s.ends_with(r#"})("https://a.test","me\"\u003c/script>","p'\\w\n");"#), "{s}");
        let capture = capture_script("abc123");
        assert!(capture.contains("location.origin") && capture.contains("bare-login\u{2063}abc123\\u0009\""));
    }

    #[test]
    fn vault_round_trip() {
        let dir = std::env::temp_dir().join(format!("bare-vault-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        init(&dir);
        assert_eq!(offer("https://a.test", "me", "pw1"), Offer::Save);
        save("https://a.test", "me", "pw1");
        save("https://a.test", "you", "pw2");
        assert_eq!(offer("https://a.test", "me", "pw1"), Offer::None);
        assert_eq!(offer("https://a.test", "me", "new"), Offer::Update);
        assert_eq!(offer("https://b.test", "me", "pw1"), Offer::Save);
        set_never("https://b.test", true);
        assert_eq!(offer("https://b.test", "me", "pw1"), Offer::None);
        save("https://a.test", "me", "new");

        // On disk it's encrypted: no username, password or site in the clear.
        let raw = fs::read(dir.join("passwords.dat")).unwrap();
        for secret in ["a.test", "you", "pw2", "new"] {
            assert!(!raw.windows(secret.len()).any(|w| w == secret.as_bytes()), "{secret} stored in the clear");
        }

        // Read back.
        init(&dir);
        assert_eq!(password("https://a.test", "me").as_deref(), Some("new"));
        assert_eq!(usernames("https://a.test"), vec!["me".to_string(), "you".to_string()]);
        assert!(!has_logins("https://c.test"));
        assert_eq!(never_list(), vec!["https://b.test".to_string()]);
        remove("https://a.test", "you");
        init(&dir);
        assert_eq!(all().len(), 1);

        // A tampered or foreign file is ignored, not trusted.
        fs::write(dir.join("passwords.dat"), b"not a vault").unwrap();
        init(&dir);
        assert!(all().is_empty());
        assert!(dir.join("passwords.unreadable.dat").exists());
        fs::remove_dir_all(&dir).unwrap();
    }
}
