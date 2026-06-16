//! Per-user autostart registration.
//!
//! Writes / reads / removes a `REG_SZ` value at
//! `HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Run` so the
//! daemon launches on next sign-in.
//!
//! Per-user (HKCU, not HKLM) means no elevation is required, no installer
//! is needed, and uninstall is a single subcommand or one click in
//! Settings → Apps → Startup.
//!
//! The recorded command line is the absolute path of the currently-running
//! `dwmend.exe`, quoted to survive spaces. We deliberately do NOT add CLI
//! flags — running plain `dwmend` invokes the daemon, which is what we
//! want.

use color_eyre::Result;
use color_eyre::eyre::eyre;
use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, WIN32_ERROR};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE, REG_SAM_FLAGS, REG_SZ, RegCloseKey,
    RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
};
use windows::core::PCWSTR;

/// Registry subkey under HKCU that Windows reads on login.
const RUN_KEY_PATH: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
/// Value name we own under that subkey. Capitalised so it shows in Task
/// Manager's Startup tab with a recognisable label.
const RUN_VALUE_NAME: &str = "Dwmend";

/// RAII handle that closes the open HKEY on drop. Keeping the close path
/// in `Drop` lets us use `?` freely in the body of each public function
/// without leaking the key on the error path.
struct OpenKey(HKEY);

impl Drop for OpenKey {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from RegOpenKeyExW.
        let _ = unsafe { RegCloseKey(self.0) };
    }
}

/// Enable autostart. The path of the currently-running executable is
/// quoted and written to the Run key. Returns the path written so the
/// caller can echo it.
pub fn enable() -> Result<PathBuf> {
    let exe = std::env::current_exe().map_err(|e| eyre!("current_exe(): {e}"))?;
    let quoted = quote_path(&exe);

    let key = open_run_key(KEY_SET_VALUE)?;
    let name = wide(RUN_VALUE_NAME);
    let data = wide_no_terminator(&quoted);
    // SAFETY: `data` is a UTF-16 buffer; we pass its byte length. The
    // Windows registry accepts REG_SZ values either with or without a
    // terminating NUL; we use the no-terminator form because we don't
    // want a stray 0x0000 0x0000 to be visible in regedit.
    let status = unsafe {
        RegSetValueExW(
            key.0,
            PCWSTR(name.as_ptr()),
            None,
            REG_SZ,
            Some(slice_as_bytes(&data)),
        )
    };
    check(status, "RegSetValueExW")?;
    Ok(exe)
}

/// Remove the autostart entry. Already-disabled is success.
pub fn disable() -> Result<()> {
    let key = open_run_key(KEY_SET_VALUE)?;
    let name = wide(RUN_VALUE_NAME);
    // SAFETY: `name` is a null-terminated UTF-16 string.
    let status = unsafe { RegDeleteValueW(key.0, PCWSTR(name.as_ptr())) };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(()); // already absent
    }
    check(status, "RegDeleteValueW")
}

/// Return the registered path if autostart is enabled, otherwise `None`.
///
/// We strip the surrounding quotes the registered value carries, so the
/// caller gets back a plain `PathBuf` it can `display()` directly.
pub fn status() -> Result<Option<PathBuf>> {
    let key = match open_run_key(KEY_READ) {
        Ok(k) => k,
        Err(_) => return Ok(None), // key doesn't even exist
    };
    let name = wide(RUN_VALUE_NAME);
    let mut size: u32 = 0;
    // First call: query the size of the value (in bytes).
    // SAFETY: `name` is null-terminated; passing None for the data pointer
    // with a valid size out-param is the documented size-only query form.
    let status = unsafe {
        RegQueryValueExW(
            key.0,
            PCWSTR(name.as_ptr()),
            None,
            None,
            None,
            Some(&mut size),
        )
    };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    check(status, "RegQueryValueExW(size)")?;

    // REG_SZ values are stored as UTF-16; size is in bytes.
    let mut buf: Vec<u8> = vec![0; size as usize];
    let mut got = size;
    // SAFETY: `buf` is initialised to `size` bytes; we pass its pointer
    // and matching size.
    let status = unsafe {
        RegQueryValueExW(
            key.0,
            PCWSTR(name.as_ptr()),
            None,
            None,
            Some(buf.as_mut_ptr()),
            Some(&mut got),
        )
    };
    check(status, "RegQueryValueExW")?;

    let raw = bytes_to_wide(&buf[..got as usize]);
    let mut s = OsString::from_wide(&raw)
        .to_string_lossy()
        .trim_end_matches('\0')
        .to_string();
    // Strip surrounding quotes if present (we wrote them quoted).
    if s.starts_with('"') && s.ends_with('"') && s.len() >= 2 {
        s = s[1..s.len() - 1].to_string();
    }
    Ok(Some(PathBuf::from(s)))
}

// ---- helpers ---------------------------------------------------------------

fn open_run_key(access: REG_SAM_FLAGS) -> Result<OpenKey> {
    let path = wide(RUN_KEY_PATH);
    let mut key: HKEY = HKEY::default();
    // SAFETY: `path` is a null-terminated UTF-16 string; `key` is a valid
    // out-param.
    let status = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(path.as_ptr()),
            None,
            access,
            &mut key,
        )
    };
    check(status, "RegOpenKeyExW")?;
    Ok(OpenKey(key))
}

fn check(status: WIN32_ERROR, op: &str) -> Result<()> {
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(eyre!("{op} failed: WIN32_ERROR({})", status.0))
    }
}

fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn wide_no_terminator(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s).encode_wide().collect()
}

fn slice_as_bytes(s: &[u16]) -> &[u8] {
    // SAFETY: u16 is plain bytes; reading it as &[u8] of double the length
    // is valid for the lifetime of `s`. We never alias mutably.
    unsafe { std::slice::from_raw_parts(s.as_ptr() as *const u8, std::mem::size_of_val(s)) }
}

fn bytes_to_wide(b: &[u8]) -> Vec<u16> {
    let n = b.len() / 2;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        out.push(u16::from_le_bytes([b[2 * i], b[2 * i + 1]]));
    }
    out
}

fn quote_path(p: &std::path::Path) -> String {
    let s = p.display().to_string();
    if s.starts_with('"') && s.ends_with('"') {
        s
    } else {
        format!("\"{s}\"")
    }
}
