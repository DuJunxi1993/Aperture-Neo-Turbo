//! OS theme detection: read the Windows "app mode" (light/dark) preference
//! so the viewer can follow the system theme. Returns `true` for light mode.

/// Returns true if the Windows system is in light app mode, false for dark.
/// Reads `HKCU\...\Themes\Personalize\AppsUseLightTheme`. On failure (a
/// non-Windows build, or the value is absent) falls back to dark (false).
pub fn system_theme_is_light() -> bool {
    #[cfg(windows)]
    {
        use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
        use windows::core::PCWSTR;

        const PATH: &str =
            "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize";
        const VALUE: &str = "AppsUseLightTheme";

        let path_wide: Vec<u16> = PATH.encode_utf16().chain(std::iter::once(0)).collect();
        let value_wide: Vec<u16> = VALUE.encode_utf16().chain(std::iter::once(0)).collect();
        let mut data: u32 = 0;
        let mut size: u32 = std::mem::size_of::<u32>() as u32;

        let result = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                PCWSTR(path_wide.as_ptr()),
                PCWSTR(value_wide.as_ptr()),
                RRF_RT_REG_DWORD,
                None,
                Some((&mut data as *mut u32).cast()),
                Some(&mut size),
            )
        };
        if result.is_ok() {
            data == 1
        } else {
            false
        }
    }
    #[cfg(not(windows))]
    {
        false
    }
}
