//! Replaces the icon and version info of CEF's bootstrap.exe so it shows up as "Bare".

use std::{ffi::c_void, path::Path, ptr};

use windows_sys::{
    Win32::{
        Foundation::{FreeLibrary, HMODULE},
        System::LibraryLoader::{
            BeginUpdateResourceW, EndUpdateResourceW, EnumResourceLanguagesW, EnumResourceNamesW,
            LOAD_LIBRARY_AS_DATAFILE, LoadLibraryExW, UpdateResourceW,
        },
        UI::WindowsAndMessaging::{RT_GROUP_ICON, RT_ICON, RT_VERSION},
    },
    core::{BOOL, PCWSTR},
};

use crate::Result;

const LANG_EN_US: u16 = 0x0409;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

pub fn brand(exe: &Path, ico: &Path, version: &str) -> Result<()> {
    let ico = std::fs::read(ico)?;
    let existing = existing_resources(exe)?;
    let path = wide(&exe.to_string_lossy());
    unsafe {
        let h = BeginUpdateResourceW(path.as_ptr(), 0);
        if h.is_null() {
            return Err("BeginUpdateResource failed".into());
        }
        // Remove the bootstrap's own icons and version info.
        for (kind, name, lang) in existing {
            UpdateResourceW(h, kind, name, lang, ptr::null(), 0);
        }
        // Icon images (RT_ICON 1..n) plus the group directory (RT_GROUP_ICON 1).
        let count = u16::from_le_bytes([ico[4], ico[5]]) as usize;
        let mut group = vec![0, 0, 1, 0];
        group.extend_from_slice(&(count as u16).to_le_bytes());
        for i in 0..count {
            let e = &ico[6 + 16 * i..6 + 16 * (i + 1)];
            let size = u32::from_le_bytes(e[8..12].try_into()?) as usize;
            let offset = u32::from_le_bytes(e[12..16].try_into()?) as usize;
            let image = &ico[offset..offset + size];
            let id = (i + 1) as u16;
            UpdateResourceW(h, RT_ICON, id as usize as PCWSTR, LANG_EN_US, image.as_ptr() as *const c_void, size as u32);
            group.extend_from_slice(&e[..12]);
            group.extend_from_slice(&id.to_le_bytes());
        }
        UpdateResourceW(h, RT_GROUP_ICON, 1 as PCWSTR, LANG_EN_US, group.as_ptr() as *const c_void, group.len() as u32);

        let info = version_info(version);
        UpdateResourceW(h, RT_VERSION, 1 as PCWSTR, LANG_EN_US, info.as_ptr() as *const c_void, info.len() as u32);

        if EndUpdateResourceW(h, 0) == 0 {
            return Err("EndUpdateResource failed".into());
        }
    }
    Ok(())
}

type Res = (PCWSTR, PCWSTR, u16);

/// Every (type, numeric id, language) of the icon/version resources currently in `exe`.
fn existing_resources(exe: &Path) -> Result<Vec<Res>> {
    unsafe extern "system" fn on_lang(_: HMODULE, kind: PCWSTR, name: PCWSTR, lang: u16, out: isize) -> BOOL {
        unsafe { (*(out as *mut Vec<Res>)).push((kind, name, lang)) };
        1
    }
    unsafe extern "system" fn on_name(m: HMODULE, kind: PCWSTR, name: PCWSTR, out: isize) -> BOOL {
        // Only numeric ids (named resources would need their string copied).
        if (name as usize) >> 16 == 0 {
            unsafe { EnumResourceLanguagesW(m, kind, name, Some(on_lang), out) };
        }
        1
    }
    let path = wide(&exe.to_string_lossy());
    let mut found: Vec<Res> = Vec::new();
    unsafe {
        let m = LoadLibraryExW(path.as_ptr(), ptr::null_mut(), LOAD_LIBRARY_AS_DATAFILE);
        if m.is_null() {
            return Err("could not open bootstrap.exe".into());
        }
        for kind in [RT_ICON, RT_GROUP_ICON, RT_VERSION] {
            EnumResourceNamesW(m, kind, Some(on_name), &mut found as *mut _ as isize);
        }
        FreeLibrary(m);
    }
    Ok(found)
}

/// Builds a VS_VERSIONINFO resource.
fn version_info(version: &str) -> Vec<u8> {
    let nums: Vec<u16> = version.split('.').map(|n| n.parse().unwrap_or(0)).chain([0; 4]).take(4).collect();
    let ms = (nums[0] as u32) << 16 | nums[1] as u32;
    let ls = (nums[2] as u32) << 16 | nums[3] as u32;
    let mut fixed = Vec::new();
    for v in [0xFEEF04BDu32, 0x0001_0000, ms, ls, ms, ls, 0x3F, 0, 0x0004_0004, 1, 0, 0, 0] {
        fixed.extend_from_slice(&v.to_le_bytes());
    }

    let strings: Vec<Vec<u8>> = [
        ("CompanyName", "Bare contributors"),
        ("FileDescription", "Bare Browser"),
        ("FileVersion", version),
        ("InternalName", "Bare"),
        ("LegalCopyright", "Copyright (c) 2026 Bare contributors. MIT License."),
        ("OriginalFilename", "Bare.exe"),
        ("ProductName", "Bare"),
        ("ProductVersion", version),
    ]
    .iter()
    .map(|(k, v)| node(k, Value::Text(v), vec![]))
    .collect();
    let table = node("040904B0", Value::None, strings);
    let string_info = node("StringFileInfo", Value::None, vec![table]);
    let translation = [0x09u8, 0x04, 0xB0, 0x04];
    let var = node("Translation", Value::Binary(&translation), vec![]);
    let var_info = node("VarFileInfo", Value::None, vec![var]);
    node("VS_VERSION_INFO", Value::Binary(&fixed), vec![string_info, var_info])
}

enum Value<'a> {
    None,
    Text(&'a str),
    Binary(&'a [u8]),
}

fn pad(buf: &mut Vec<u8>) {
    while buf.len() % 4 != 0 {
        buf.push(0);
    }
}

/// One version-info block: header, key, value, then children, each 32-bit aligned.
fn node(key: &str, value: Value, children: Vec<Vec<u8>>) -> Vec<u8> {
    let (value_len, kind, bytes): (u16, u16, Vec<u8>) = match value {
        Value::None => (0, 1, vec![]),
        Value::Text(s) => {
            let w = wide(s);
            (w.len() as u16, 1, w.iter().flat_map(|c| c.to_le_bytes()).collect())
        }
        Value::Binary(b) => (b.len() as u16, 0, b.to_vec()),
    };
    let mut buf = vec![0, 0];
    buf.extend_from_slice(&value_len.to_le_bytes());
    buf.extend_from_slice(&kind.to_le_bytes());
    buf.extend(wide(key).iter().flat_map(|c| c.to_le_bytes()));
    pad(&mut buf);
    buf.extend_from_slice(&bytes);
    for child in children {
        pad(&mut buf);
        buf.extend_from_slice(&child);
    }
    let len = buf.len() as u16;
    buf[..2].copy_from_slice(&len.to_le_bytes());
    buf
}
