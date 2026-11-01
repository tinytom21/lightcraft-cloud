//! The few Win32 calls the app needs, in one place (Windows only): the tray's message pump, the console of a command-line run, and the
//! per-user `Run` key for "Start when I log in". Everything else is std.

#![allow(unsafe_code)]

use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::IntoRawHandle;

use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::System::Console::{
    ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ, RegCloseKey, RegCreateKeyExW, RegDeleteKeyW, RegDeleteValueW, RegOpenKeyExW,
    RegQueryValueExW, RegSetValueExW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage};

fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s).encode_wide().chain(Some(0)).collect()
}

/// Handle every window message waiting for this thread, and return. The tray icon's window lives on the main thread and gets its
/// clicks (and the menu) only while this runs; the main loop calls it every few tens of milliseconds.
pub fn pump_messages() {
    // SAFETY: `msg` is a plain struct that the call fills in; a null window means "this thread's messages".
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// A program of the windows subsystem has no console, so a command-line run would print into nothing. This joins the console of the
/// program that started us (a terminal), when there is one, and points the standard handles at it unless they were redirected to a
/// file or a pipe. Call it first thing for the command-line commands, not for the tray app or the windows.
pub fn attach_parent_console() {
    // SAFETY: plain handle calls; the handle given to SetStdHandle is leaked on purpose (it lives as long as the process).
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
        for (which, name, reading) in [(STD_INPUT_HANDLE, "CONIN$", true), (STD_OUTPUT_HANDLE, "CONOUT$", false), (STD_ERROR_HANDLE, "CONOUT$", false)] {
            let h = GetStdHandle(which);
            if (h.is_null() || h == INVALID_HANDLE_VALUE)
                && let Ok(f) = std::fs::OpenOptions::new().read(reading).write(!reading).open(name)
            {
                SetStdHandle(which, f.into_raw_handle());
            }
        }
    }
}

/// A key of the current user's registry hive, open for the length of the call.
struct Key(HKEY);

impl Key {
    fn open(subkey: &str, create: bool) -> Result<Option<Key>, String> {
        let sub = wide(subkey);
        let mut h: HKEY = std::ptr::null_mut();
        // SAFETY: `sub` is NUL-terminated; `h` receives the key.
        let rc = unsafe {
            if create {
                RegCreateKeyExW(HKEY_CURRENT_USER, sub.as_ptr(), 0, std::ptr::null(), 0, KEY_SET_VALUE | KEY_QUERY_VALUE, std::ptr::null(), &mut h, std::ptr::null_mut())
            } else {
                RegOpenKeyExW(HKEY_CURRENT_USER, sub.as_ptr(), 0, KEY_SET_VALUE | KEY_QUERY_VALUE, &mut h)
            }
        };
        match rc {
            0 => Ok(Some(Key(h))),
            2 if !create => Ok(None), // ERROR_FILE_NOT_FOUND
            e => Err(format!("registry key HKCU\\{subkey}: error {e}")),
        }
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: the key was opened by `open` and is closed once.
        unsafe { RegCloseKey(self.0) };
    }
}

/// Set the string value `name` under `HKCU\<subkey>`, creating the key if needed.
pub fn registry_set(subkey: &str, name: &str, value: &str) -> Result<(), String> {
    let key = Key::open(subkey, true)?.ok_or("registry key not created")?;
    let (n, v) = (wide(name), wide(value));
    // SAFETY: both are NUL-terminated UTF-16; the byte length counts the terminator, as REG_SZ wants.
    let rc = unsafe { RegSetValueExW(key.0, n.as_ptr(), 0, REG_SZ, v.as_ptr().cast(), (v.len() * 2) as u32) };
    if rc == 0 { Ok(()) } else { Err(format!("registry value {name}: error {rc}")) }
}

/// The string value `name` under `HKCU\<subkey>`, if it is there.
pub fn registry_get(subkey: &str, name: &str) -> Option<String> {
    let key = Key::open(subkey, false).ok()??;
    let n = wide(name);
    let mut size = 0u32;
    // SAFETY: first call asks for the size only; the second fills a buffer of that size.
    unsafe {
        if RegQueryValueExW(key.0, n.as_ptr(), std::ptr::null(), std::ptr::null_mut(), std::ptr::null_mut(), &mut size) != 0 {
            return None;
        }
        let mut buf = vec![0u16; (size as usize).div_ceil(2)];
        let mut kind = 0u32;
        if RegQueryValueExW(key.0, n.as_ptr(), std::ptr::null(), &mut kind, buf.as_mut_ptr().cast(), &mut size) != 0 || kind != REG_SZ {
            return None;
        }
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..len]))
    }
}

/// Delete the value `name` under `HKCU\<subkey>`; fine if it (or the key) is not there. Creates nothing.
pub fn registry_delete(subkey: &str, name: &str) -> Result<(), String> {
    let Some(key) = Key::open(subkey, false)? else { return Ok(()) };
    let n = wide(name);
    // SAFETY: `n` is NUL-terminated.
    let rc = unsafe { RegDeleteValueW(key.0, n.as_ptr()) };
    match rc {
        0 | 2 => Ok(()),
        e => Err(format!("registry value {name}: error {e}")),
    }
}

/// Whether `HKCU\<subkey>` exists.
pub fn registry_key_exists(subkey: &str) -> bool {
    matches!(Key::open(subkey, false), Ok(Some(_)))
}

/// Delete `HKCU\<subkey>` (an empty key; for tests that made their own).
pub fn registry_delete_key(subkey: &str) {
    let sub = wide(subkey);
    // SAFETY: `sub` is NUL-terminated.
    unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, sub.as_ptr()) };
}

/// Windows' own Pictures folder (it can be moved, to OneDrive for example, so it is not always `%USERPROFILE%\Pictures`). None if Windows
/// cannot say.
pub fn known_pictures_folder() -> Option<std::path::PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{FOLDERID_Pictures, SHGetKnownFolderPath};
    let mut out: *mut u16 = std::ptr::null_mut();
    // SAFETY: the call fills `out` with a NUL-terminated string that it allocates; it is read once and freed with CoTaskMemFree, as the
    // documentation says (also when the call fails, where it may leave a null).
    unsafe {
        let hr = SHGetKnownFolderPath(&FOLDERID_Pictures, 0, std::ptr::null_mut(), &mut out);
        if out.is_null() {
            return None;
        }
        let path = (hr >= 0).then(|| {
            let mut len = 0;
            while *out.add(len) != 0 {
                len += 1;
            }
            std::path::PathBuf::from(std::ffi::OsString::from_wide(std::slice::from_raw_parts(out, len)))
        });
        CoTaskMemFree(out.cast());
        path
    }
}
