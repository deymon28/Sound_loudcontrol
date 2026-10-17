//! "Start with Windows" via the per-user `Run` registry key.

use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
};
use windows::core::{HSTRING, w};

const RUN_KEY: windows::core::PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const VALUE_NAME: windows::core::PCWSTR = w!("LoudControl");

fn command_line() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    Some(format!("\"{}\" --minimized", exe.display()))
}

/// True when the Run entry exists and points at this executable.
pub fn is_enabled() -> bool {
    let Some(expected) = command_line() else { return false };
    let mut buf = [0u16; 1024];
    let mut size = (buf.len() * 2) as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            VALUE_NAME,
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if status != ERROR_SUCCESS {
        return false;
    }
    let len = (size as usize / 2).saturating_sub(1);
    String::from_utf16_lossy(&buf[..len]).eq_ignore_ascii_case(&expected)
}

pub fn set_enabled(enabled: bool) -> Result<(), String> {
    let status = if enabled {
        let cmd = command_line().ok_or("cannot resolve executable path")?;
        let wide: Vec<u16> = cmd.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                RUN_KEY,
                VALUE_NAME,
                REG_SZ.0,
                Some(wide.as_ptr().cast()),
                (wide.len() * 2) as u32,
            )
        }
    } else {
        let s = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, VALUE_NAME) };
        if s == windows::Win32::Foundation::ERROR_FILE_NOT_FOUND { ERROR_SUCCESS } else { s }
    };
    if status == ERROR_SUCCESS {
        log::info!("start with Windows {}", if enabled { "enabled" } else { "disabled" });
        Ok(())
    } else {
        Err(format!("registry error {}", HSTRING::from(windows::core::Error::from(status.to_hresult()).message())))
    }
}
