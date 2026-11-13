//! Per-window taskbar identity.
//!
//! A process-wide AppUserModelID is not enough for pinning: a pin created
//! from a running window without relaunch properties (portable copies, or a
//! shortcut made before the ID existed) records only the executable, so the
//! next launch shows a second button beside the pin. Windows that carry their
//! own ID and relaunch command group with every pin made from them.
use windows::{core::{PCWSTR, PWSTR}, Win32::{Foundation::{HWND, PROPERTYKEY},
    System::{Com::{CoTaskMemAlloc, StructuredStorage::{PropVariantClear, PROPVARIANT}}, Variant::{VT_EMPTY, VT_LPWSTR}},
    UI::Shell::PropertiesSystem::{IPropertyStore, SHGetPropertyStoreForWindow}}};

const APP_USER_MODEL: windows::core::GUID = windows::core::GUID::from_u128(0x9f4c2855_9f79_4b39_a8d0_e1d42de1d5f3);
const fn key(pid: u32) -> PROPERTYKEY { PROPERTYKEY { fmtid: APP_USER_MODEL, pid } }
const RELAUNCH_COMMAND: PROPERTYKEY = key(2);
const RELAUNCH_ICON: PROPERTYKEY = key(3);
const RELAUNCH_NAME: PROPERTYKEY = key(4);
const ID: PROPERTYKEY = key(5);

pub struct Identity<'a> { pub app_id: &'a str, pub relaunch_command: &'a str, pub display_name: &'a str, pub icon: &'a str }

struct Value(PROPVARIANT);
impl Value {
    fn text(text: &str) -> windows::core::Result<Self> {
        let units: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
        let memory = unsafe { CoTaskMemAlloc(units.len() * 2) }.cast::<u16>();
        if memory.is_null() { return Err(windows::core::Error::from_hresult(windows::core::HRESULT(0x8007000Eu32 as i32))); }
        unsafe { std::ptr::copy_nonoverlapping(units.as_ptr(), memory, units.len()); }
        let mut value = PROPVARIANT::default();
        unsafe {
            (*value.Anonymous.Anonymous).vt = VT_LPWSTR;
            (*value.Anonymous.Anonymous).Anonymous.pwszVal = PWSTR(memory);
        }
        Ok(Self(value))
    }
    fn empty() -> Self { let mut value = PROPVARIANT::default(); unsafe { (*value.Anonymous.Anonymous).vt = VT_EMPTY; } Self(value) }
}
impl Drop for Value { fn drop(&mut self) { let _ = unsafe { PropVariantClear(&mut self.0) }; } }

/// Relaunch properties are written before the ID that refers to them.
pub fn set_identity(hwnd: HWND, identity: &Identity<'_>) -> windows::core::Result<()> {
    let store: IPropertyStore = unsafe { SHGetPropertyStoreForWindow(hwnd)? };
    for (key, text) in [(RELAUNCH_COMMAND, identity.relaunch_command), (RELAUNCH_NAME, identity.display_name),
        (RELAUNCH_ICON, identity.icon), (ID, identity.app_id)] {
        let value = Value::text(text)?;
        unsafe { store.SetValue(&key, &value.0)?; }
    }
    unsafe { store.Commit() }
}

/// Windows requires these properties to be removed before the window goes away.
pub fn clear_identity(hwnd: HWND) {
    let Ok(store) = (unsafe { SHGetPropertyStoreForWindow::<IPropertyStore>(hwnd) }) else { return };
    let empty = Value::empty();
    for key in [ID, RELAUNCH_COMMAND, RELAUNCH_NAME, RELAUNCH_ICON] { let _ = unsafe { store.SetValue(&key, &empty.0) }; }
    let _ = unsafe { store.Commit() };
}

/// Reads the window's own ID; `None` when it inherits the process ID.
pub fn window_app_id(hwnd: HWND) -> Option<String> {
    let store: IPropertyStore = unsafe { SHGetPropertyStoreForWindow(hwnd) }.ok()?;
    let value = Value(unsafe { store.GetValue(&ID) }.ok()?);
    unsafe {
        let inner = &*value.0.Anonymous.Anonymous;
        (inner.vt == VT_LPWSTR && !inner.Anonymous.pwszVal.is_null()).then(|| PCWSTR(inner.Anonymous.pwszVal.0).to_string().ok()).flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, WS_OVERLAPPEDWINDOW, WINDOW_EX_STYLE};
    #[test]
    fn window_identity_round_trips_and_is_removed() {
        let _ = unsafe { windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED) };
        let hwnd = unsafe { CreateWindowExW(WINDOW_EX_STYLE::default(), windows::core::w!("STATIC"), windows::core::w!("taskbar identity"),
            WS_OVERLAPPEDWINDOW, 0, 0, 10, 10, None, None, None, None) }.unwrap();
        assert_eq!(window_app_id(hwnd), None);
        set_identity(hwnd, &Identity { app_id: "ZSDesk.Test.Identity", relaunch_command: "\"C:\\ZSDesk\\ZSDesk.exe\"",
            display_name: "ZSDesk", icon: "C:\\ZSDesk\\ZSDesk.exe,0" }).unwrap();
        assert_eq!(window_app_id(hwnd).as_deref(), Some("ZSDesk.Test.Identity"));
        clear_identity(hwnd);
        assert_eq!(window_app_id(hwnd), None);
        let _ = unsafe { DestroyWindow(hwnd) };
    }
}
