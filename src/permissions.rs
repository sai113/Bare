//! Site permission requests (camera, microphone, location, ...): what to ask the user and what
//! to refuse outright. CEF reports them as bit masks; the values mirror CEF's
//! `cef_media_access_permission_types_t` and `cef_permission_request_types_t`.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::OnceLock,
};

// Media access (getUserMedia / getDisplayMedia).
pub const MEDIA_AUDIO: u32 = 1;
pub const MEDIA_VIDEO: u32 = 2;
#[cfg(test)]
const MEDIA_DESKTOP_AUDIO: u32 = 4;
#[cfg(test)]
const MEDIA_DESKTOP_VIDEO: u32 = 8;

// Permission prompts.
pub const CAMERA_PAN_TILT_ZOOM: u32 = 2;
pub const CAMERA: u32 = 4;
pub const CLIPBOARD: u32 = 16;
pub const LOCAL_FONTS: u32 = 128;
pub const GEOLOCATION: u32 = 256;
pub const IDLE_DETECTION: u32 = 2048;
pub const MIC: u32 = 4096;
pub const MIDI_SYSEX: u32 = 8192;
pub const MULTIPLE_DOWNLOADS: u32 = 16384;
const NOTIFICATIONS: u32 = 32768;
pub const WINDOW_MANAGEMENT: u32 = 8388608;

/// Media types Bare can grant. Screen sharing needs a screen picker, which Bare doesn't have yet.
pub fn grantable_media(requested: u32) -> u32 {
    requested & (MEDIA_AUDIO | MEDIA_VIDEO)
}

/// Prompt types that are refused without asking.
pub fn always_denied(requested: u32) -> bool {
    // Notifications are blocked browser-wide; nothing else to ask about means deny.
    requested & !NOTIFICATIONS == 0
}

/// Human description of a media request, e.g. "use your camera and microphone".
pub fn describe_media(mask: u32) -> String {
    let mut things = Vec::new();
    if mask & MEDIA_VIDEO != 0 {
        things.push("camera");
    }
    if mask & MEDIA_AUDIO != 0 {
        things.push("microphone");
    }
    format!("use your {}", join(&things))
}

/// Human description of a permission prompt, e.g. "know your location".
pub fn describe_prompt(mask: u32) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut devices = Vec::new();
    if mask & (CAMERA | CAMERA_PAN_TILT_ZOOM) != 0 {
        devices.push(if mask & CAMERA_PAN_TILT_ZOOM != 0 { "camera (including moving it)" } else { "camera" });
    }
    if mask & MIC != 0 {
        devices.push("microphone");
    }
    if !devices.is_empty() {
        parts.push(format!("use your {}", join(&devices)));
    }
    let named = [
        (GEOLOCATION, "know your location"),
        (CLIPBOARD, "see what you copy"),
        (MIDI_SYSEX, "control your MIDI devices"),
        (LOCAL_FONTS, "see your installed fonts"),
        (IDLE_DETECTION, "know when you're away"),
        (WINDOW_MANAGEMENT, "manage windows on your screens"),
        (MULTIPLE_DOWNLOADS, "download multiple files"),
    ];
    let known = CAMERA | CAMERA_PAN_TILT_ZOOM | MIC | NOTIFICATIONS | named.iter().fold(0, |m, (bit, _)| m | bit);
    parts.extend(named.iter().filter(|(bit, _)| mask & bit != 0).map(|(_, text)| text.to_string()));
    if mask & !known != 0 {
        parts.push("use other browser features".into());
    }
    let parts: Vec<&str> = parts.iter().map(String::as_str).collect();
    join(&parts)
}

// ---- session-only grants ----
//
// Chromium permanently saves answers to permission prompts (location, ...). Bare promises
// "until Bare closes", so every such answer is written down here and reset when the
// window closes, and again at the next start in case Bare crashed.

static LEDGER: OnceLock<PathBuf> = OnceLock::new();

pub fn init(profile: &Path) {
    let _ = LEDGER.set(profile.join("session-permissions.txt"));
}

/// Remembers that `origin` was granted the prompt permissions in `mask`.
pub fn record_grant(origin: &str, mask: u32) {
    let Some(path) = LEDGER.get() else { return };
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{origin}\t{mask}");
    }
}

/// All recorded grants, clearing the record.
pub fn take_grants() -> Vec<(String, u32)> {
    let Some(path) = LEDGER.get() else { return Vec::new() };
    let text = fs::read_to_string(path).unwrap_or_default();
    let _ = fs::remove_file(path);
    parse_grants(&text)
}

fn parse_grants(text: &str) -> Vec<(String, u32)> {
    text.lines()
        .filter_map(|l| {
            let (origin, mask) = l.split_once('\t')?;
            Some((origin.to_string(), mask.trim().parse().ok()?))
        })
        .filter(|(o, _)| o.starts_with("https://") || o.starts_with("http://"))
        .collect()
}

fn join(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [one] => one.to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media() {
        assert_eq!(describe_media(MEDIA_AUDIO | MEDIA_VIDEO), "use your camera and microphone");
        assert_eq!(describe_media(MEDIA_AUDIO), "use your microphone");
        assert_eq!(grantable_media(MEDIA_VIDEO | MEDIA_DESKTOP_VIDEO), MEDIA_VIDEO);
        assert_eq!(grantable_media(MEDIA_DESKTOP_VIDEO | MEDIA_DESKTOP_AUDIO), 0);
    }

    #[test]
    fn prompts() {
        assert_eq!(describe_prompt(GEOLOCATION), "know your location");
        assert_eq!(describe_prompt(CAMERA | MIC), "use your camera and microphone");
        assert_eq!(describe_prompt(CAMERA | GEOLOCATION | CLIPBOARD), "use your camera, know your location and see what you copy");
        assert_eq!(describe_prompt(1 << 30), "use other browser features");
        assert!(always_denied(NOTIFICATIONS));
        assert!(always_denied(0));
        assert!(!always_denied(GEOLOCATION | NOTIFICATIONS));
    }

    #[test]
    fn grant_records() {
        let text = "https://a.test\t256\nhttps://b.test:8443\t4100\ngarbage\nfile:///x\t256\nhttps://c.test\tnope\n";
        assert_eq!(parse_grants(text), vec![("https://a.test".into(), 256), ("https://b.test:8443".into(), 4100)]);
    }
}
