//! Keep the edited Shell item visible when the view sorts its new name.
use super::*;
use std::{ffi::{OsStr, OsString}, fs::{File, OpenOptions}, os::windows::{ffi::OsStringExt, fs::OpenOptionsExt, io::AsRawHandle}, path::Path, time::{Duration, Instant}};
use windows::Win32::{Foundation::HANDLE, Storage::FileSystem::{GetFinalPathNameByHandleW, GETFINALPATHNAMEBYHANDLE_FLAGS,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_SHARE_DELETE}};
use zsdesk_windows::shell::{notify::{Change, ChangeNotify}, pidl::item_pidl, shell_item};

pub(super) const TIMER: usize = 0x5a78;
pub(super) const WM_CHANGED: u32 = WM_APP + 0x4d1;

fn final_path(file: &File) -> Option<PathBuf> {
    let mut path = vec![0u16; 512];
    loop {
        let n = unsafe { GetFinalPathNameByHandleW(HANDLE(file.as_raw_handle()), &mut path, GETFINALPATHNAMEBYHANDLE_FLAGS(0)) } as usize;
        if n == 0 || n > 32768 { return None; }
        if n < path.len() { return Some(OsString::from_wide(&path[..n]).into()); }
        path.resize(n + 1, 0);
    }
}

fn shared_handle(path: &Path) -> Option<(File, PathBuf)> {
    let file = OpenOptions::new().access_mode(FILE_READ_ATTRIBUTES.0)
        .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0)
        .custom_flags((FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT).0).open(path).ok()?;
    let original = final_path(&file)?;
    Some((file, original))
}

fn new_name<'a>(old: &Path, new: &'a Path) -> Option<&'a OsStr> {
    (crate::explorer_forwarder::same_path(old.parent()?, new.parent()?) &&
        old.file_name()? != new.file_name()?).then(|| new.file_name()).flatten()
}

pub(super) struct Watch {
    editor: HWND,
    source: usize,
    tab: u64,
    epoch: u64,
    original: PathBuf,
    handle: Option<(File, PathBuf)>,
    _registration: Option<ChangeNotify>,
    target: Option<OsString>,
    ended: Option<Instant>,
    first_reveal: Option<Instant>,
    sorted: bool,
}

impl Watch {
    fn notify(&mut self, change: Change) {
        if !change.is_rename() { return; }
        if let (Some(old), Some(new)) = (change.first, change.second) {
            if crate::explorer_forwarder::same_path(&self.original, Path::new(&old)) {
                self.target = new_name(&self.original, Path::new(&new)).map(OsStr::to_os_string);
            }
        }
    }
}

impl WindowState {
    pub(super) fn observe_rename_input(&mut self, focus: HWND, target: HWND, message: u32) {
        if let Some(watch) = &self.rename_follow {
            if focus == watch.editor && target == watch.editor && !matches!(message, WM_MOUSEWHEEL | WM_MOUSEHWHEEL) { return; }
            // A deliberate scroll or new selection takes precedence over a late sort.
            if watch.ended.is_some() || matches!(message, WM_MOUSEWHEEL | WM_MOUSEHWHEEL) ||
                (target != watch.editor && matches!(message, WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_POINTERDOWN)) {
                self.rename_follow = None;
                let _ = unsafe { KillTimer(Some(self.hwnd), TIMER) };
                return;
            }
        }
        if target == focus && !self.view_hwnd.is_invalid() && unsafe { IsChild(self.view_hwnd, focus) }.as_bool() && text_input_window(focus) {
            self.watch_inline_rename(focus);
        }
    }

    pub(super) fn watch_inline_rename(&mut self, editor: HWND) {
        if self.background_pane || self.rename_follow.as_ref().is_some_and(|w| w.editor == editor && w.ended.is_none()) { return; }
        let Some(browser) = &self.browser else { return };
        let selected = browser.selection_paths_limit(2);
        let [name] = selected.as_slice() else { return };
        let original = PathBuf::from(name);
        let Some(directory) = self.current_location().as_path() else { return };
        if !original.parent().is_some_and(|p| crate::explorer_forwarder::same_path(p, directory)) { return; }
        let handle = shared_handle(&original);
        let registration = unsafe { shell_item(self.current_location()) }.ok().and_then(|item| item_pidl(&item))
            .and_then(|pidl| ChangeNotify::renames(self.hwnd, WM_CHANGED, pidl));
        if handle.is_none() && registration.is_none() { return; }
        diagnostics::record("rename-follow", "edit-attached", None);
        self.rename_follow = Some(Watch { editor, source:self.browser_source, tab:self.session.active_tab().id,
            epoch:self.navigation_epoch, original, handle, _registration:registration, target:None, ended:None, first_reveal:None, sorted:false });
        unsafe { SetTimer(Some(self.hwnd), TIMER, 100, None); }
    }

    pub(super) fn renamed_item_notification(&mut self, wp: WPARAM, lp: LPARAM) {
        let change = zsdesk_windows::shell::notify::read_change(wp, lp);
        if let Some(watch) = &mut self.rename_follow { watch.notify(change); }
    }

    pub(super) fn poll_rename_follow(&mut self) {
        if !self.advance_rename_follow() {
            self.rename_follow = None;
            let _ = unsafe { KillTimer(Some(self.hwnd), TIMER) };
        }
    }

    fn advance_rename_follow(&mut self) -> bool {
        let Some(watch) = &mut self.rename_follow else { return false };
        if self.browser_source != watch.source || self.session.active_tab().id != watch.tab || self.navigation_epoch != watch.epoch { return false; }
        if watch.ended.is_none() && unsafe { GetFocus() } == watch.editor && unsafe { IsWindow(Some(watch.editor)) }.as_bool() { return true; }
        if watch.ended.get_or_insert_with(Instant::now).elapsed() > Duration::from_secs(5) { return false; }
        if watch.target.is_none() {
            watch.target = watch.handle.as_ref().and_then(|(file, original)| {
                let path = final_path(file)?;
                new_name(original, &path).map(OsStr::to_os_string)
            });
        }
        let Some(name) = &watch.target else { return true };
        let Some(browser) = &self.browser else { return false };
        let destination = watch.original.with_file_name(name);
        // Shell can briefly expose the old name or an empty selection during relayout.
        if !watch.sorted && browser.selection_paths_limit(2).iter().any(|p| ![&destination, &watch.original].iter()
            .any(|old| crate::explorer_forwarder::same_path(Path::new(p), old))) { return false; }
        if !watch.sorted {
            // DefView leaves a newly edited row appended even after SetSortColumns
            // is given its existing keys. Refresh once, preserving the user's sort.
            let refreshed = unsafe { browser.current_view().and_then(|v| v.Refresh()) };
            diagnostics::record("rename-follow", "refresh-after-commit", refreshed.as_ref().err().map(|e|e.code().0));
            if refreshed.is_err() { return false; }
            watch.sorted = true;
            return true;
        }
        let focus = unsafe { GetFocus() };
        if self.view_hwnd != focus && !unsafe { IsChild(self.view_hwnd, focus) }.as_bool() { return false; }
        if browser.select_child(name).is_ok() {
            // Re-resolve by name after Shell has processed the asynchronous sort.
            return watch.first_reveal.get_or_insert_with(Instant::now).elapsed() < Duration::from_millis(650);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn renamed_file_and_folder_keep_their_identity_in_a_large_directory() -> anyhow::Result<()> {
        let temp = std::env::temp_dir().join(format!("zsdesk-rename-follow-{}", std::process::id()));
        std::fs::create_dir(&temp)?;
        struct Cleanup(PathBuf);
        impl Drop for Cleanup { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); } }
        let _cleanup = Cleanup(temp.clone());
        for i in 0..1200 { File::create(temp.join(format!("middle-{i:04}.txt")))?; }
        for directory in [false, true] {
            let old = temp.join(if directory { "000-folder" } else { "000-file.txt" });
            let new = temp.join(if directory { "zzz-folder" } else { "zzz-file.txt" });
            if directory { std::fs::create_dir(&old)?; } else { File::create(&old)?; }
            let (handle, before) = shared_handle(&old).unwrap();
            assert!(new_name(&before, &before).is_none());
            std::fs::rename(&old, &new)?;
            let after = final_path(&handle).unwrap();
            assert_eq!(new_name(&before, &after), new.file_name());
            assert!(new_name(&before, &after.join("unrelated")).is_none());
        }
        Ok(())
    }
}
