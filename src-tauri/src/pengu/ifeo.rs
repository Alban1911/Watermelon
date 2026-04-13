use anyhow::{anyhow, Result};
use std::path::Path;
use std::ptr;

use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WRITE, REG_SZ, RegCloseKey,
    RegCreateKeyExW, RegDeleteTreeW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
};

const SUBKEY: &str = "SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\\Image File Execution Options\\LeagueClientUx.exe";
const VALUE_NAME: &str = "Debugger";

/// Writes the IFEO `Debugger` value for `LeagueClientUx.exe` so Windows
/// redirects launches through `rundll32 "<core_dll>",_BootstrapEntry`.
/// Fails (typically ACCESS_DENIED) if the process is not elevated.
pub fn write_key(core_dll_path: &Path) -> Result<()> {
    let dll_str = core_dll_path
        .to_str()
        .ok_or_else(|| anyhow!("core.dll path is not valid UTF-8"))?;
    let value = format!("rundll32 \"{}\",_BootstrapEntry ", dll_str);

    let subkey_w = to_wide(SUBKEY);
    let value_name_w = to_wide(VALUE_NAME);
    let value_w = to_wide(&value);

    unsafe {
        let mut key: HKEY = ptr::null_mut();
        let mut disposition: u32 = 0;
        let create = RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            subkey_w.as_ptr(),
            0,
            ptr::null(),
            0,
            KEY_WRITE,
            ptr::null(),
            &mut key,
            &mut disposition,
        );
        if create != ERROR_SUCCESS {
            return Err(anyhow!(
                "RegCreateKeyExW failed with code {} — admin required?",
                create
            ));
        }

        let byte_len = (value_w.len() * std::mem::size_of::<u16>()) as u32;
        let set = RegSetValueExW(
            key,
            value_name_w.as_ptr(),
            0,
            REG_SZ,
            value_w.as_ptr() as *const u8,
            byte_len,
        );
        let _ = RegCloseKey(key);

        if set != ERROR_SUCCESS {
            return Err(anyhow!("RegSetValueExW failed with code {}", set));
        }
    }
    Ok(())
}

/// Reads the current IFEO `Debugger` value for `LeagueClientUx.exe`.
/// Returns `Ok(None)` if the key or value doesn't exist, `Ok(Some(value))`
/// if a string value is present, and `Err` on unexpected registry errors.
/// Used by `pengu::mod` to decide whether the current key belongs to us
/// before deleting or overwriting it.
pub fn read_debugger_value() -> Result<Option<String>> {
    let subkey_w = to_wide(SUBKEY);
    let value_name_w = to_wide(VALUE_NAME);

    unsafe {
        let mut key: HKEY = ptr::null_mut();
        let open = RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            subkey_w.as_ptr(),
            0,
            KEY_READ,
            &mut key,
        );
        if open == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if open != ERROR_SUCCESS {
            return Err(anyhow!("RegOpenKeyExW failed with code {}", open));
        }

        // Probe size first.
        let mut data_size: u32 = 0;
        let mut value_type: u32 = 0;
        let probe = RegQueryValueExW(
            key,
            value_name_w.as_ptr(),
            ptr::null(),
            &mut value_type,
            ptr::null_mut(),
            &mut data_size,
        );
        if probe == ERROR_FILE_NOT_FOUND {
            let _ = RegCloseKey(key);
            return Ok(None);
        }
        if probe != ERROR_SUCCESS {
            let _ = RegCloseKey(key);
            return Err(anyhow!("RegQueryValueExW (probe) failed with code {}", probe));
        }
        if value_type != REG_SZ {
            let _ = RegCloseKey(key);
            return Ok(None);
        }

        // Read the actual data.
        let wide_len = (data_size as usize).div_ceil(std::mem::size_of::<u16>());
        let mut buffer: Vec<u16> = vec![0u16; wide_len];
        let mut buffer_size = data_size;
        let read = RegQueryValueExW(
            key,
            value_name_w.as_ptr(),
            ptr::null(),
            ptr::null_mut(),
            buffer.as_mut_ptr() as *mut u8,
            &mut buffer_size,
        );
        let _ = RegCloseKey(key);

        if read != ERROR_SUCCESS {
            return Err(anyhow!("RegQueryValueExW (read) failed with code {}", read));
        }

        // Strip the trailing null(s) the registry stores as part of REG_SZ.
        let end = buffer
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(buffer.len());
        Ok(Some(String::from_utf16_lossy(&buffer[..end])))
    }
}

/// Deletes the IFEO key for `LeagueClientUx.exe`. Treats "not found" as
/// success so deactivation and crash recovery are idempotent.
pub fn delete_key() -> Result<()> {
    let subkey_w = to_wide(SUBKEY);
    let result = unsafe { RegDeleteTreeW(HKEY_LOCAL_MACHINE, subkey_w.as_ptr()) };
    if result == ERROR_SUCCESS || result == ERROR_FILE_NOT_FOUND {
        Ok(())
    } else {
        Err(anyhow!("RegDeleteTreeW failed with code {}", result))
    }
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
