//! Integration with the operating system: default-browser status and command-line targets.

use std::path::Path;

use crate::{settings, url};

/// What to open for a command-line argument: a link from another app, a file opened from
/// Explorer, a bare:// page, or something typed (`Bare.exe example.com`).
pub fn launch_target(arg: &str) -> String {
    let arg = arg.trim();
    if arg.to_ascii_lowercase().starts_with("bare://") {
        return arg.to_string();
    }
    let path = Path::new(arg);
    if path.is_absolute() && path.exists() {
        return url::file_url(path);
    }
    url::resolve(arg, settings::get().search.prefix())
}

/// Installed with BareSetup.exe (registered in Windows' list of browsers)?
pub fn registered_as_browser() -> bool {
    reg_query(r"HKCU\Software\RegisteredApplications", "Bare").is_some()
}

/// Is Bare the browser Windows uses for https links?
pub fn is_default_browser() -> bool {
    reg_query(
        r"HKCU\Software\Microsoft\Windows\Shell\Associations\UrlAssociations\https\UserChoice",
        "ProgId",
    )
    .is_some_and(|v| v.eq_ignore_ascii_case("BareURL"))
}

/// Is Windows set to dark mode for apps?
pub fn dark_mode() -> bool {
    reg_query(r"HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize", "AppsUseLightTheme")
        .is_some_and(|v| v.trim() == "0x0")
}

/// Opens Windows' Default apps page for Bare. Windows doesn't let apps make themselves the
/// default; the user confirms there.
pub fn open_default_apps_settings() {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("explorer.exe").arg("ms-settings:defaultapps?registeredAppUser=Bare").spawn();
    }
}

/// Reads a string registry value with `reg query` (Windows only).
fn reg_query(key: &str, value: &str) -> Option<String> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let out = std::process::Command::new("reg")
            .args(["query", key, "/v", value])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        // "    ProgId    REG_SZ    BareURL" / "    AppsUseLightTheme    REG_DWORD    0x0"
        text.lines().find_map(|l| {
            let rest = l.trim().strip_prefix(value)?.trim_start();
            let (kind, data) = rest.split_once(char::is_whitespace)?;
            kind.starts_with("REG_").then(|| data.trim().to_string())
        })
    }
    #[cfg(not(windows))]
    {
        let _ = (key, value);
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_targets() {
        assert_eq!(launch_target("bare://settings"), "bare://settings");
        assert_eq!(launch_target("https://example.com/x"), "https://example.com/x");
        assert_eq!(launch_target("example.com"), "https://example.com");
        let file = std::env::temp_dir().join(format!("bare launch {}.html", std::process::id()));
        std::fs::write(&file, "<p>hi</p>").unwrap();
        let target = launch_target(&file.to_string_lossy());
        assert!(target.starts_with("file:///") && target.ends_with(".html") && target.contains("%20"), "{target}");
        std::fs::remove_file(file).unwrap();
        // A path that doesn't exist is not opened as a file.
        assert!(!launch_target(r"C:\nope\missing.html").starts_with("file:"));
    }
}
