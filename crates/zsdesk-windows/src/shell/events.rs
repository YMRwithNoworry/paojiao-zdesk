use windows::Win32::UI::WindowsAndMessaging::WM_APP;

pub const WM_SHELL_NAVIGATED: u32 = WM_APP + 0x210;
pub const WM_SHELL_FOLDER_INVOKED: u32 = WM_APP + 0x211;
pub const WM_TREE_NAVIGATE: u32 = WM_APP + 0x212;
pub const WM_BOOKMARK_PATH_DROPPED: u32 = WM_APP + 0x213;

pub const WM_SHELL_NAVIGATION_FAILED: u32 = WM_APP + 0x214;

/// Selection changed in the Shell view. `LPARAM` is an `Rc<Cell<bool>>` pending
/// flag that the receiver must reclaim with [`acknowledge_selection_changed`].
pub const WM_SHELL_SELECTION_CHANGED: u32 = WM_APP + 0x215;
pub const WM_SHELL_PICKER_ACCEPT: u32 = WM_APP + 0x216;

/// Clears the coalescing flag carried by `WM_SHELL_SELECTION_CHANGED`.
///
/// # Safety
///
/// `lparam` must come from a `WM_SHELL_SELECTION_CHANGED` message and be consumed once.
pub unsafe fn acknowledge_selection_changed(lparam: isize) {
    if lparam != 0 {
        let flag = unsafe { std::rc::Rc::from_raw(lparam as *const std::cell::Cell<bool>) };
        flag.set(false);
    }
}
