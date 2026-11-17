//! Read-only cut highlighting, driven by the system clipboard notification.
use std::path::Path;
use windows::Win32::{Foundation::HWND,System::DataExchange::{AddClipboardFormatListener,RemoveClipboardFormatListener},UI::WindowsAndMessaging::{KillTimer,SetTimer}};
use super::{with_host,protocol};
pub(super) const TIMER:usize=protocol::HostTimer::Clipboard as usize;
pub(super) fn path_key(path:&Path)->String{path.to_string_lossy().replace('/',"\\").to_lowercase()}
pub(super) struct Listener(HWND);
impl Listener{
    pub(super) fn new(hwnd:HWND)->windows::core::Result<Self>{
        unsafe{AddClipboardFormatListener(hwnd)?;}refresh(hwnd);Ok(Self(hwnd))
    }
}
impl Drop for Listener{fn drop(&mut self){
    let _=unsafe{RemoveClipboardFormatListener(self.0)};let _=unsafe{KillTimer(Some(self.0),TIMER)};
}}
pub(super) fn refresh(hwnd:HWND){
    // Never hold HOST across delayed clipboard rendering or a COM call.
    let applied=zsdesk_windows::shell::file_transfer::cut_clipboard_paths().ok()
        .and_then(|paths|with_host(|host|host.set_cut_paths(&paths))).is_some();
    if applied{let _=unsafe{KillTimer(Some(hwnd),TIMER)};}
    else{unsafe{SetTimer(Some(hwnd),TIMER,100,None);}}
}
