//! Memory usage of each tab's renderer process, for the tab tooltip ("Memory usage: 120 MB").
//!
//! Each page's process reports its process id when a page starts (see `app::RenderHandler`).
//! The browser then reads that process's private memory, the figure Chrome shows too, after
//! checking the id really belongs to a Bare process.

/// Private (non-shared) memory of process `pid` in bytes, if it's one of Bare's processes.
pub fn private_bytes(pid: u32) -> Option<u64> {
    imp::private_bytes(pid)
}

/// "120 MB", "1.4 GB".
pub fn format_bytes(bytes: u64) -> String {
    let mb = bytes as f64 / (1024.0 * 1024.0);
    if mb >= 1024.0 { format!("{:.1} GB", mb / 1024.0) } else { format!("{} MB", mb.round() as u64) }
}

#[cfg(windows)]
mod imp {
    use std::{ffi::OsString, os::windows::ffi::OsStringExt, path::PathBuf};

    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::{
            ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX},
            Threading::{OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW},
        },
    };

    pub fn private_bytes(pid: u32) -> Option<u64> {
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return None;
            }
            let result = (|| {
                // Only report on processes running Bare's own executable.
                let mut buf = [0u16; 1024];
                let mut len = buf.len() as u32;
                if QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len) == 0 {
                    return None;
                }
                let image = PathBuf::from(OsString::from_wide(&buf[..len as usize]));
                let ours = std::env::current_exe().ok()?;
                if !image.file_name()?.eq_ignore_ascii_case(ours.file_name()?) {
                    return None;
                }
                let mut counters: PROCESS_MEMORY_COUNTERS_EX = std::mem::zeroed();
                counters.cb = size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
                if GetProcessMemoryInfo(h, (&raw mut counters).cast::<PROCESS_MEMORY_COUNTERS>(), counters.cb) == 0 {
                    return None;
                }
                Some(counters.PrivateUsage as u64)
            })();
            CloseHandle(h);
            result
        }
    }
}

#[cfg(not(windows))]
mod imp {
    /// Linux: resident memory from /proc (macOS: not yet).
    pub fn private_bytes(pid: u32) -> Option<u64> {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        let kb: u64 = status.lines().find_map(|l| l.strip_prefix("VmRSS:"))?.trim().trim_end_matches("kB").trim().parse().ok()?;
        Some(kb * 1024)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting() {
        assert_eq!(format_bytes(120 * 1024 * 1024), "120 MB");
        assert_eq!(format_bytes(1536 * 1024 * 1024), "1.5 GB");
    }

    #[test]
    fn own_process() {
        // The test binary is "one of ours", so it can read its own memory.
        let bytes = private_bytes(std::process::id()).expect("own memory");
        assert!(bytes > 1024 * 1024);
        // Another program's process is refused.
        #[cfg(windows)]
        {
            let mut other = std::process::Command::new("ping").args(["-n", "5", "127.0.0.1"]).stdout(std::process::Stdio::null()).spawn().unwrap();
            assert_eq!(private_bytes(other.id()), None);
            let _ = other.kill();
        }
    }
}
