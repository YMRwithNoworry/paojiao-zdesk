//! System theme detection and DWM frame attributes.
//!
//! Dark mode for Shell-hosted controls uses the uxtheme ordinals
//! `SetPreferredAppMode` (#135), `AllowDarkModeForWindow` (#133) and
//! `FlushMenuThemes` (#136). They are only resolved on Windows 10 1903+
//! (build 18362) where their signatures are stable; otherwise light controls
//! remain and only the ZSDesk chrome follows the dark palette.

use windows::{
    core::{w, PCSTR, PCWSTR},
    Win32::{
        Foundation::{HMODULE, HWND, RECT},
        Graphics::Dwm::{DwmExtendFrameIntoClientArea, DwmGetWindowAttribute, DwmSetWindowAttribute,
            DWMWA_CAPTION_BUTTON_BOUNDS, DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMSBT_MAINWINDOW},
        System::{LibraryLoader::{GetProcAddress, LoadLibraryW}, Registry::{RegGetValueW, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_BINARY, RRF_RT_REG_DWORD, RRF_RT_REG_SZ}},
        UI::{Controls::{SetWindowTheme, MARGINS}},
    },
};

/// Windows build number (e.g. 22631), read from the registry because
/// `GetVersionEx` is manifest-dependent.
pub fn os_build() -> u32 {
    static BUILD: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *BUILD.get_or_init(|| {
        let mut buffer = [0u16; 32];
        let mut size = (buffer.len() * 2) as u32;
        let ok = unsafe { RegGetValueW(HKEY_LOCAL_MACHINE, w!("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion"),
            w!("CurrentBuildNumber"), RRF_RT_REG_SZ, None, Some(buffer.as_mut_ptr().cast()), Some(&mut size)) }.is_ok();
        if !ok { return 0; }
        let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
        String::from_utf16_lossy(&buffer[..end]).trim().parse().unwrap_or(0)
    })
}

/// True when "Choose your app mode" is Dark.
pub fn system_dark_mode() -> bool {
    let mut value = 1u32;
    let mut size = 4u32;
    let ok = unsafe { RegGetValueW(HKEY_CURRENT_USER, w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
        w!("AppsUseLightTheme"), RRF_RT_REG_DWORD, None, Some((&mut value as *mut u32).cast()), Some(&mut size)) }.is_ok();
    ok && value == 0
}

type SetPreferredAppMode = unsafe extern "system" fn(i32) -> i32;
type AllowDarkModeForWindow = unsafe extern "system" fn(HWND, i32) -> i32;
type FlushMenuThemes = unsafe extern "system" fn();

fn uxtheme_ordinal(ordinal: usize) -> Option<unsafe extern "system" fn() -> isize> {
    if os_build() < 18362 { return None; }
    // Keep one process-lifetime reference; navigation reapplies child themes.
    static MODULE: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
    let address = (*MODULE.get_or_init(|| unsafe { LoadLibraryW(w!("uxtheme.dll")).ok().map(|m| m.0 as usize) }))?;
    let module = HMODULE(address as *mut _);
    unsafe { GetProcAddress(module, PCSTR(ordinal as *const u8)) }
}

static MENUS_DARK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether this process's application menus use the dark theme (the file
/// window's color scheme, as last applied).
pub fn menus_dark() -> bool { MENUS_DARK.load(std::sync::atomic::Ordering::Relaxed) && !high_contrast() }

/// Lets Shell views, trees and menus in this process use dark visuals.
pub fn allow_dark_mode(dark: bool) {
    let dark=dark&&!high_contrast();
    MENUS_DARK.store(dark, std::sync::atomic::Ordering::Relaxed);
    if let Some(proc) = uxtheme_ordinal(135) {
        let set: SetPreferredAppMode = unsafe { std::mem::transmute(proc) };
        unsafe { set(if dark { 1 } else { 0 }); }
    }
    if let Some(proc) = uxtheme_ordinal(136) {
        let flush: FlushMenuThemes = unsafe { std::mem::transmute(proc) };
        unsafe { flush(); }
    }
}

/// Configures process-local Shell colors without changing Windows preferences.
pub fn configure_color_scheme(scheme: zsdesk_core::model::explorer::ColorScheme) -> bool {
    if high_contrast(){allow_dark_mode(false);return false;}
    use zsdesk_core::model::explorer::ColorScheme;
    let dark = match scheme { ColorScheme::System => system_dark_mode(), ColorScheme::Light => false, ColorScheme::Dark => true };
    MENUS_DARK.store(dark, std::sync::atomic::Ordering::Relaxed);
    if let Some(proc) = uxtheme_ordinal(135) {
        let set: SetPreferredAppMode = unsafe { std::mem::transmute(proc) };
        unsafe { set(match scheme { ColorScheme::System => 1, ColorScheme::Dark => 2, ColorScheme::Light => 3 }); }
    }
    if let Some(proc) = uxtheme_ordinal(136) {
        let flush: FlushMenuThemes = unsafe { std::mem::transmute(proc) }; unsafe { flush(); }
    }
    dark
}

/// The user's accent color ("Personalization → Colors") in the shade WinUI
/// uses for accent fills: `SystemAccentColorDark1` on light surfaces and
/// `SystemAccentColorLight2` on dark ones. `None` keeps the Fluent default.
pub fn system_accent(dark: bool) -> Option<[u8; 3]> {
    if high_contrast() { return None; }
    let mut palette = [0u8; 32];
    let mut size = palette.len() as u32;
    unsafe { RegGetValueW(HKEY_CURRENT_USER, w!("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\Accent"),
        w!("AccentPalette"), RRF_RT_REG_BINARY, None, Some(palette.as_mut_ptr().cast()), Some(&mut size)) }.ok().ok()?;
    accent_shade(&palette[..(size as usize).min(palette.len())], dark)
}

/// `AccentPalette` holds eight RGBA entries: Light3, Light2, Light1, Accent, Dark1, Dark2, Dark3, …
fn accent_shade(palette: &[u8], dark: bool) -> Option<[u8; 3]> {
    if palette.len() < 32 { return None; }
    let index = if dark { 1 } else { 4 };
    Some([palette[index * 4], palette[index * 4 + 1], palette[index * 4 + 2]])
}

pub fn high_contrast()->bool{
    use windows::Win32::UI::{Accessibility::HIGHCONTRASTW, WindowsAndMessaging::{SystemParametersInfoW, SPI_GETHIGHCONTRAST, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS}};
    let mut contrast = HIGHCONTRASTW { cbSize: std::mem::size_of::<HIGHCONTRASTW>() as u32, ..Default::default() };
    unsafe { SystemParametersInfoW(SPI_GETHIGHCONTRAST, contrast.cbSize, Some((&mut contrast as *mut HIGHCONTRASTW).cast()), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0)) }.is_ok()
        && contrast.dwFlags.0 & 1 != 0
}
pub fn transparency_available() -> bool {
    if high_contrast(){return false;}
    let mut enabled = 1u32; let mut size = 4u32;
    let _ = unsafe { RegGetValueW(HKEY_CURRENT_USER, w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
        w!("EnableTransparency"), RRF_RT_REG_DWORD, None, Some((&mut enabled as *mut u32).cast()), Some(&mut size)) };
    enabled != 0
}

/// Uses supported DWM materials; unsupported or accessibility modes remain opaque.
pub fn set_backdrop(hwnd: HWND, style: zsdesk_core::model::explorer::BackdropStyle) -> bool {
    use zsdesk_core::model::explorer::BackdropStyle;
    if os_build() < 22621 || hwnd.0.is_null() { return false; }
    let value = if !transparency_available() { 1i32 } else { match style {
        BackdropStyle::Solid => 1, BackdropStyle::System | BackdropStyle::Mica => 2, BackdropStyle::Acrylic => 3,
    }};
    let mut current = 0i32;
    if unsafe { DwmGetWindowAttribute(hwnd, DWMWA_SYSTEMBACKDROP_TYPE, (&mut current as *mut i32).cast(), 4) }.is_ok() && current == value {
        return value != 1;
    }
    unsafe { DwmSetWindowAttribute(hwnd, DWMWA_SYSTEMBACKDROP_TYPE, (&value as *const i32).cast(), 4) }.is_ok() && value != 1
}

/// Applies dark/light control theme to a hosted window (tree, list).
pub fn theme_control(hwnd: HWND, dark: bool) {
    if hwnd.0.is_null() { return; }
    let contrast=high_contrast();let dark=dark&&!contrast;
    if let Some(proc) = uxtheme_ordinal(133) {
        let allow: AllowDarkModeForWindow = unsafe { std::mem::transmute(proc) };
        unsafe { allow(hwnd, dark as i32); }
    }
    let name = if contrast{PCWSTR::null()}else if dark { w!("DarkMode_Explorer") } else { w!("Explorer") };
    let _ = unsafe { SetWindowTheme(hwnd, name, PCWSTR::null()) };
}

/// Dark title bar / caption buttons (attribute 20, or 19 before build 18985).
pub fn set_dark_frame(hwnd: HWND, dark: bool) {
    let value: i32 = (dark&&!high_contrast()) as i32;
    let attribute = if os_build() >= 18985 { DWMWA_USE_IMMERSIVE_DARK_MODE } else { windows::Win32::Graphics::Dwm::DWMWINDOWATTRIBUTE(19) };
    let _ = unsafe { DwmSetWindowAttribute(hwnd, attribute, (&value as *const i32).cast(), 4) };
}

/// Windows 11 rounded window corners (`small` for popups such as tooltips and editors).
/// Earlier systems keep square corners; ZSDesk draws its own rounded surfaces there.
pub fn round_corners(hwnd: HWND, small: bool) {
    if os_build() < 22000 || hwnd.0.is_null() { return; }
    let value: i32 = if small { 3 } else { 2 }; // DWMWCP_ROUNDSMALL / DWMWCP_ROUND
    let _ = unsafe { DwmSetWindowAttribute(hwnd, windows::Win32::Graphics::Dwm::DWMWA_WINDOW_CORNER_PREFERENCE, (&value as *const i32).cast(), 4) };
}

/// Blurs whatever is behind `hwnd` (the Windows 10+ accent policy used for
/// frosted glass); the window's own alpha still decides how much shows through.
/// Returns whether the system accepted the change.
pub fn set_blur_behind(hwnd: HWND, enabled: bool) -> bool {
    let enabled=enabled&&transparency_available();
    #[repr(C)]
    struct AccentPolicy { state: u32, flags: u32, gradient: u32, animation: u32 }
    #[repr(C)]
    struct CompositionData { attribute: u32, data: *mut core::ffi::c_void, size: usize }
    type SetAttribute = unsafe extern "system" fn(HWND, *mut CompositionData) -> i32;
    const ACCENT_DISABLED: u32 = 0;
    const ACCENT_ENABLE_BLURBEHIND: u32 = 3;
    const WCA_ACCENT_POLICY: u32 = 19;
    if hwnd.0.is_null() { return false; }
    let Ok(user32) = (unsafe { windows::Win32::System::LibraryLoader::GetModuleHandleW(w!("user32.dll")) }) else { return false };
    let Some(function) = (unsafe { GetProcAddress(user32, PCSTR(c"SetWindowCompositionAttribute".as_ptr().cast())) }) else { return false };
    // SAFETY: the exported function has had this signature since Windows 7.
    let set: SetAttribute = unsafe { std::mem::transmute(function) };
    let mut accent = AccentPolicy { state: if enabled { ACCENT_ENABLE_BLURBEHIND } else { ACCENT_DISABLED }, flags: 0, gradient: 0, animation: 0 };
    let mut data = CompositionData { attribute: WCA_ACCENT_POLICY, data: (&mut accent as *mut AccentPolicy).cast(), size: std::mem::size_of::<AccentPolicy>() };
    unsafe { set(hwnd, &mut data) != 0 }
}

/// Enables the Mica backdrop on Windows 11 22H2+. Returns whether it applies.
pub fn enable_mica(hwnd: HWND) -> bool {
    if os_build() < 22621 { return false; }
    let value = DWMSBT_MAINWINDOW.0;
    unsafe { DwmSetWindowAttribute(hwnd, DWMWA_SYSTEMBACKDROP_TYPE, (&value as *const i32).cast(), 4) }.is_ok()
}

/// Extends the DWM frame `top` pixels into the client area (caption buttons and backdrop).
pub fn extend_frame(hwnd: HWND, top: i32) {
    let margins = MARGINS { cxLeftWidth: 0, cxRightWidth: 0, cyTopHeight: top, cyBottomHeight: 0 };
    let _ = unsafe { DwmExtendFrameIntoClientArea(hwnd, &margins) };
}

/// Caption button bounds in window coordinates.
pub fn caption_button_bounds(hwnd: HWND) -> Option<RECT> {
    let mut rect = RECT::default();
    unsafe { DwmGetWindowAttribute(hwnd, DWMWA_CAPTION_BUTTON_BOUNDS, (&mut rect as *mut RECT).cast(), std::mem::size_of::<RECT>() as u32) }.ok()?;
    (rect.right > rect.left).then_some(rect)
}

#[cfg(test)]
mod tests {
    #[test]
    fn accent_palette_uses_winui_shades() {
        // Windows 11 default blue palette.
        let palette: Vec<u8> = [0x99EBFFu32, 0x4CC2FF, 0x0091F8, 0x0078D4, 0x0067C0, 0x003E92, 0x001A68, 0xF7630C]
            .iter().flat_map(|rgb| [(rgb >> 16) as u8, (rgb >> 8) as u8, *rgb as u8, 0]).collect();
        assert_eq!(super::accent_shade(&palette, false), Some([0x00, 0x67, 0xC0]));
        assert_eq!(super::accent_shade(&palette, true), Some([0x4C, 0xC2, 0xFF]));
        assert_eq!(super::accent_shade(&palette[..16], true), None);
    }
}
