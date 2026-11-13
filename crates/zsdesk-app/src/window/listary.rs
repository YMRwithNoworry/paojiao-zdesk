//! Read-only current-folder endpoint for the official Listary FileAppPlugin API.
//! The endpoint belongs to its file window and contains no on-disk path cache.
use super::*;
use std::{cell::Cell, os::windows::ffi::OsStrExt, path::Path};

pub(super) const CLASS: PCWSTR = windows::core::w!("ZSDesk.Listary.Folder.v1");
thread_local! { static WRITING: Cell<bool> = const { Cell::new(false) }; }

unsafe extern "system" fn endpoint_proc(hwnd: HWND, message: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if message == WM_SETTEXT && !WRITING.with(Cell::get) { return LRESULT(0); }
    unsafe { DefWindowProcW(hwnd, message, wp, lp) }
}

pub(super) fn initialize(owner: HWND) -> windows::core::Result<()> {
    let instance = HINSTANCE(unsafe { GetModuleHandleW(None)? }.0);
    unsafe { RegisterClassW(&WNDCLASSW { hInstance: instance, lpfnWndProc: Some(endpoint_proc), lpszClassName: CLASS, ..Default::default() }); }
    let _endpoint = unsafe { CreateWindowExW(WS_EX_NOACTIVATE, CLASS, None, WS_CHILD, 0, 0, 0, 0,
        Some(owner), None, Some(instance), None)? };
    Ok(())
}

fn folder_units(path: Option<&Path>) -> Vec<u16> {
    let Some(path) = path.filter(|path| path.is_absolute()) else { return vec![0] };
    let mut units: Vec<u16> = path.as_os_str().encode_wide().collect();
    if units.len() >= 32768 || units.contains(&0) { return vec![0]; }
    units.push(0);
    units
}

pub(super) fn publish(owner: HWND, path: Option<&Path>) {
    let Ok(endpoint) = (unsafe { FindWindowExW(Some(owner), None, CLASS, None) }) else { return };
    let value = folder_units(path);
    let mut old = vec![0u16; value.len().max(1)];
    let copied = unsafe { GetWindowTextW(endpoint, &mut old) }.max(0) as usize;
    if unsafe { GetWindowTextLengthW(endpoint) }.max(0) as usize + 1 == value.len()
        && copied + 1 == value.len() && old == value { return; }
    struct WriteScope(bool);
    impl Drop for WriteScope { fn drop(&mut self) { WRITING.with(|writing| writing.set(self.0)); } }
    let _scope = WriteScope(WRITING.with(|writing| writing.replace(true)));
    let _ = unsafe { SetWindowTextW(endpoint, PCWSTR(value.as_ptr())) };
}

impl WindowState {
    pub(super) fn publish_listary_folder(&self) {
        if self.background_pane {return;}
        let ready = self.pending.is_none() && self.queued.is_none() && self.shown_tab == Some(self.session.active_tab().id);
        publish(self.hwnd, ready.then(|| self.current_location().as_path()).flatten());
    }
}

#[cfg(test)] mod tests {
    use super::*;
    #[test] fn endpoint_rejects_nonfilesystem_and_invalid_paths() {
        assert_eq!(folder_units(None), vec![0]);
        assert_eq!(folder_units(Some(Path::new("relative"))), vec![0]);
        assert_eq!(folder_units(Some(Path::new("E:\\bad\0path"))), vec![0]);
        let value = folder_units(Some(Path::new(r"\\server\共享\中文 空格")));
        assert_eq!(String::from_utf16_lossy(&value[..value.len()-1]), r"\\server\共享\中文 空格");
    }
}
