//! Shell change notifications (`SHChangeNotifyRegister`) delivered as window messages.

use windows::{
    core::BOOL,
    Win32::{
        Foundation::{HANDLE, HWND, LPARAM, WPARAM},
        UI::Shell::{
            Common::ITEMIDLIST, SHChangeNotification_Lock, SHChangeNotification_Unlock, SHChangeNotifyDeregister,
            SHChangeNotifyEntry, SHChangeNotifyRegister, SHGetNameFromIDList, SHCNE_ATTRIBUTES, SHCNE_CREATE,
            SHCNE_DELETE, SHCNE_MKDIR, SHCNE_RENAMEFOLDER, SHCNE_RENAMEITEM, SHCNE_RMDIR, SHCNE_UPDATEDIR,
            SHCNE_UPDATEITEM, SHCNE_UPDATEIMAGE, SHCNE_DRIVEADD, SHCNE_DRIVEREMOVED, SHCNE_MEDIAINSERTED,
            SHCNE_MEDIAREMOVED, SHCNE_NETSHARE, SHCNE_NETUNSHARE, SHCNE_SERVERDISCONNECT, SHCNRF_InterruptLevel,
            SHCNRF_NewDelivery, SHCNRF_ShellLevel, SHCNRF_RecursiveInterrupt, SIGDN_DESKTOPABSOLUTEPARSING, SHCNE_ID,
        },
    },
};

pub struct ChangeNotify(Vec<u32>);

impl ChangeNotify {
    /// Registers `hwnd` for changes inside the given folders (absolute PIDL bytes).
    pub fn register(hwnd: HWND, message: u32, folders: &[Vec<u8>], recursive: bool) -> Option<Self> {
        if folders.is_empty() { return None; }
        let events = SHCNE_CREATE.0 | SHCNE_DELETE.0 | SHCNE_MKDIR.0 | SHCNE_RMDIR.0 | SHCNE_RENAMEITEM.0 | SHCNE_RENAMEFOLDER.0
            | SHCNE_UPDATEDIR.0 | SHCNE_UPDATEITEM.0 | SHCNE_ATTRIBUTES.0 | SHCNE_UPDATEIMAGE.0 | SHCNE_DRIVEADD.0
            | SHCNE_DRIVEREMOVED.0 | SHCNE_MEDIAINSERTED.0 | SHCNE_MEDIAREMOVED.0 | SHCNE_NETSHARE.0 | SHCNE_NETUNSHARE.0
            | SHCNE_SERVERDISCONNECT.0;
        let mut sources=SHCNRF_ShellLevel | SHCNRF_InterruptLevel | SHCNRF_NewDelivery;
        if recursive { sources |= SHCNRF_RecursiveInterrupt; }
        // The Shell requires one entry per registration for reliable deregistration.
        // fRecursive alone covers Shell events, not raw descendant filesystem writes.
        let mut registrations=Self(Vec::with_capacity(folders.len()));
        for folder in folders {
            let entry=SHChangeNotifyEntry { pidl:folder.as_ptr().cast_mut().cast(),fRecursive:BOOL::from(recursive) };
            let id=unsafe { SHChangeNotifyRegister(hwnd,sources,events as i32,message,1,&entry) };
            if id==0 { return None; }
            registrations.0.push(id);
        }
        Some(registrations)
    }
}

impl Drop for ChangeNotify {
    fn drop(&mut self) { for id in self.0.drain(..) { let _ = unsafe { SHChangeNotifyDeregister(id) }; } }
}

/// One decoded notification.
#[derive(Debug, Default, Clone)]
pub struct Change {
    pub event: u32,
    pub first: Option<String>,
    pub second: Option<String>,
}

impl Change {
    pub fn is_rename(&self) -> bool { self.event & (SHCNE_RENAMEITEM.0 | SHCNE_RENAMEFOLDER.0) as u32 != 0 }
}

/// Decodes and releases a notification message (`wParam`/`lParam` from the posted message).
pub fn read_change(wp: WPARAM, lp: LPARAM) -> Change {
    let mut pidls: *mut *mut ITEMIDLIST = std::ptr::null_mut();
    let mut event = 0i32;
    let lock = unsafe { SHChangeNotification_Lock(HANDLE(wp.0 as *mut _), lp.0 as u32, Some(&mut pidls), Some(&mut event)) };
    let mut change = Change { event: event as u32, ..Default::default() };
    if !lock.is_invalid() && !lock.0.is_null() {
        if !pidls.is_null() {
            let name = |p: *mut ITEMIDLIST| -> Option<String> {
                if p.is_null() { return None; }
                let text = unsafe { SHGetNameFromIDList(p, SIGDN_DESKTOPABSOLUTEPARSING).ok()? };
                Some(unsafe { super::shell_item::take_pwstr(text) })
            };
            change.first = name(unsafe { *pidls });
            change.second = name(unsafe { *pidls.add(1) });
        }
        let _ = unsafe { SHChangeNotification_Unlock(lock) };
    }
    let _ = SHCNE_ID(0);
    change
}
