//! Platform adapter for framework-owned utility windows.
use std::sync::{Arc, atomic::{AtomicBool, AtomicIsize, AtomicU8, Ordering}};
use zsdesk_core::model::explorer::BackdropStyle;
use zsui::{Command, UiInvalidationHandle, windows_win32_host::interop::{NativeMessageFilter, WindowsWindowHooks}};
use windows::{Win32::{Foundation::HWND, Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_SYSTEMBACKDROP_TYPE},
    UI::{Input::{Ime::{ImmGetContext, ImmGetCompositionStringW, ImmReleaseContext, GCS_COMPSTR},
        KeyboardAndMouse::{GetFocus, GetKeyState, VK_CONTROL, VK_MENU, VK_SHIFT, VK_UP, VK_DOWN, VK_RETURN, VK_ESCAPE}},
        WindowsAndMessaging::{GetAncestor, GetClassNameW, GetForegroundWindow, GA_ROOT, WM_KEYDOWN, WM_PAINT}}}};

#[derive(Clone)]
pub struct AppearancePolicy(Arc<AtomicU8>, Arc<AtomicBool>, Arc<AtomicIsize>);
impl AppearancePolicy {
    pub fn new(style: BackdropStyle) -> Self { let result = Self(Arc::new(AtomicU8::new(0)), Arc::new(AtomicBool::new(false)), Arc::new(AtomicIsize::new(0))); result.set(style); result }
    pub fn search_keys(&self, enabled: bool) { self.1.store(enabled, Ordering::Release); }
    pub fn foreground(&self) -> bool { let root = self.2.load(Ordering::Acquire); root != 0 && unsafe { GetForegroundWindow() }.0 as isize == root }
    pub fn set(&self, style: BackdropStyle) { self.0.store(match style { BackdropStyle::System => 0, BackdropStyle::Solid => 1, BackdropStyle::Mica => 2, BackdropStyle::Acrylic => 3 }, Ordering::Release); }
    fn style(&self) -> BackdropStyle { match self.0.load(Ordering::Acquire) { 1 => BackdropStyle::Solid, 2 => BackdropStyle::Mica, 3 => BackdropStyle::Acrylic, _ => BackdropStyle::System } }
}

pub fn hooks(wake: UiInvalidationHandle, appearance: AppearancePolicy, search_keys: bool) -> WindowsWindowHooks {
    appearance.search_keys(search_keys);
    WindowsWindowHooks::new().message_filter(move || Box::new(UtilityFilter { root: HWND::default(), wake: wake.clone(), appearance: appearance.clone() }))
}

struct UtilityFilter { root: HWND, wake: UiInvalidationHandle, appearance: AppearancePolicy }
impl NativeMessageFilter for UtilityFilter {
    fn pre_translate(&mut self, message: &windows_sys::Win32::UI::WindowsAndMessaging::MSG) -> bool {
        let window = HWND(message.hwnd);
        let root = unsafe { GetAncestor(window, GA_ROOT) };
        if self.root.0.is_null() {
            let mut name = [0u16; 64];
            let length = unsafe { GetClassNameW(root, &mut name) }.max(0) as usize;
            if String::from_utf16_lossy(&name[..length]) != "ZsuiMainWindow" { return false; }
            self.root = root;
            self.appearance.2.store(root.0 as isize, Ordering::Release);
            // This filter runs only after native initialization (including
            // RegisterHotKey) has succeeded and the message loop has started.
            let _ = self.wake.request_app_command(Command::custom("quick.ready"));
        }
        if root != self.root { return false; }
        if message.message == WM_PAINT {
            // Framework policy owns Mica and caption geometry. The product adds
            // the documented transient backdrop for its optional acrylic preset.
            let style = self.appearance.style();
            let mut current = 0i32;
            let _ = unsafe { DwmGetWindowAttribute(root, DWMWA_SYSTEMBACKDROP_TYPE, (&mut current as *mut i32).cast(), 4) };
            let wanted = match style { BackdropStyle::Solid => 1, BackdropStyle::Acrylic => 3, _ => 2 };
            if current != wanted { zsdesk_windows::theme::set_backdrop(root, style); }
        }
        if !self.appearance.1.load(Ordering::Acquire) || message.message != WM_KEYDOWN || composing(window) { return false; }
        let control = unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0;
        let alt = unsafe { GetKeyState(VK_MENU.0 as i32) } < 0;
        let shift = unsafe { GetKeyState(VK_SHIFT.0 as i32) } < 0;
        if alt || shift { return false; }
        let command = match message.wParam as u16 {
            value if value == VK_ESCAPE.0 => Some("quick.escape"),
            value if value == VK_UP.0 && !control => Some("quick.previous"),
            value if value == VK_DOWN.0 && !control => Some("quick.next"),
            value if value == VK_RETURN.0 && control => Some("quick.reveal"),
            _ => None,
        };
        command.is_some_and(|command| self.wake.request_app_command(Command::custom(command)).is_ok())
    }
}

impl Drop for UtilityFilter { fn drop(&mut self) { self.appearance.2.store(0, Ordering::Release); } }

fn composing(fallback: HWND) -> bool {
    let focused = unsafe { GetFocus() };
    let window = if focused.0.is_null() { fallback } else { focused };
    let context = unsafe { ImmGetContext(window) };
    if context.0.is_null() { return false; }
    let active = unsafe { ImmGetCompositionStringW(context, GCS_COMPSTR, None, 0) } > 0;
    let _ = unsafe { ImmReleaseContext(window, context) };
    active
}

pub fn theme_mode(scheme: zsdesk_core::model::explorer::ColorScheme) -> zsui::ZsuiThemeMode {
    use zsdesk_core::model::explorer::ColorScheme;
    match scheme { ColorScheme::System => zsui::ZsuiThemeMode::System, ColorScheme::Light => zsui::ZsuiThemeMode::Light, ColorScheme::Dark => zsui::ZsuiThemeMode::Dark }
}
