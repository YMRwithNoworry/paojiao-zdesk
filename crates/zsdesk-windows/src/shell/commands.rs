//! Command bar operations on the hosted Shell view. Common file transfers use
//! native batch operations; extension-specific verbs retain Shell dispatch.

use windows::{
    core::{Interface, Result, BOOL},
    Win32::{
        Foundation::{HWND, POINT, PROPERTYKEY},
        UI::Shell::{
            IFolderView2, IShellItem, IShellItem2, IShellItemArray, SORTCOLUMN, SORT_ASCENDING, SORT_DESCENDING, SVGIO_ALLVIEW,
            SVGIO_SELECTION, SVSI_SELECT, SVSI_DESELECT, FWF_CHECKSELECT, FOLDERFLAGS, SHGetSetSettings, SHELLSTATEA,
            SSF_SHOWALLOBJECTS, SSF_SHOWEXTENSIONS, SIGDN_DESKTOPABSOLUTEPARSING, SIGDN_FILESYSPATH,
        },
    },
};

use super::{menu::ShellMenu, ExplorerBrowserHost};

pub const PKEY_NAME: PROPERTYKEY = PROPERTYKEY { fmtid: windows::core::GUID::from_u128(0xB725F130_47EF_101A_A5F1_02608C9EEBAC), pid: 10 };
pub const PKEY_TYPE: PROPERTYKEY = PROPERTYKEY { fmtid: windows::core::GUID::from_u128(0xB725F130_47EF_101A_A5F1_02608C9EEBAC), pid: 4 };
pub const PKEY_SIZE: PROPERTYKEY = PROPERTYKEY { fmtid: windows::core::GUID::from_u128(0xB725F130_47EF_101A_A5F1_02608C9EEBAC), pid: 12 };
pub const PKEY_MODIFIED: PROPERTYKEY = PROPERTYKEY { fmtid: windows::core::GUID::from_u128(0xB725F130_47EF_101A_A5F1_02608C9EEBAC), pid: 14 };
pub const PKEY_CREATED: PROPERTYKEY = PROPERTYKEY { fmtid: windows::core::GUID::from_u128(0xB725F130_47EF_101A_A5F1_02608C9EEBAC), pid: 15 };
const PKEY_NULL: PROPERTYKEY = PROPERTYKEY { fmtid: windows::core::GUID::zeroed(), pid: 0 };

/// Sort/group column identifiers shared with the UI layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Column { Name, Modified, Type, Size, Created }

impl Column {
    pub const fn key(self) -> PROPERTYKEY {
        match self { Self::Name => PKEY_NAME, Self::Modified => PKEY_MODIFIED, Self::Type => PKEY_TYPE, Self::Size => PKEY_SIZE, Self::Created => PKEY_CREATED }
    }
    fn from_key(key: &PROPERTYKEY) -> Option<Self> {
        [Self::Name, Self::Modified, Self::Type, Self::Size, Self::Created].into_iter().find(|c| {
            let k = c.key(); k.fmtid == key.fmtid && k.pid == key.pid || *c==Column::Size&&*key==super::folder_totals::PKEY_TOTAL_SIZE })
    }
}

/// Item counts and the total size of selected files.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ViewStatus { pub items: usize, pub selected: usize, pub selected_bytes: Option<u64> }

/// `None` is also cached: unsupported size metadata must not be retried by
/// every status timer. Revision changes distinguish equal-sized selections.
#[derive(Default)]
pub(crate) struct SelectionSizeCache { entry: Option<(u64, usize, Option<u64>)> }
impl SelectionSizeCache {
    pub(crate) fn get(&self, revision: u64, selected: usize) -> Option<Option<u64>> {
        self.entry.filter(|&(r, count, _)| r == revision && count == selected).map(|(_, _, bytes)| bytes)
    }
    pub(crate) fn store(&mut self, revision: u64, selected: usize, bytes: Option<u64>) {
        self.entry = Some((revision, selected, bytes));
    }
}

impl ExplorerBrowserHost {
    fn folder_view(&self) -> Result<IFolderView2> { unsafe { self.current_view()?.cast() } }

    /// Top-level window of the Shell view (for focus tests).
    pub fn view_window(&self) -> Option<HWND> {
        let view = unsafe { self.current_view().ok()? };
        // Use the IOleWindow method inherited by IShellView directly.
        unsafe { view.GetWindow().ok() }
    }

    /// Runs a verb on the selection ("cut", "copy", "delete", "properties" …).
    pub fn invoke_selection(&self, owner: HWND, verb: &str, shift: bool) -> Result<()> {
        let view = unsafe { self.current_view()? };
        if matches!(verb,"copy"|"cut"|"delete")&&super::file_transfer::filesystem_selection(&view)?{
            return match verb{
                "copy"=>super::file_transfer::copy_view(&view,owner,false),
                "cut"=>super::file_transfer::copy_view(&view,owner,true),
                _=>super::file_transfer::delete_view(&view,unsafe{view.GetWindow()?},shift),
            };
        }
        // MTP and archive entries have Shell identities, not filesystem paths.
        // An accepted asynchronous filesystem operation never runs twice.
        let menu = ShellMenu::for_view(&view, true)?;
        menu.populate_for_invoke(shift)?;
        menu.invoke_verb(owner, verb, shift)
    }

    /// Runs a verb on the folder background ("paste", "NewFolder", "properties").
    pub fn invoke_background(&self, owner: HWND, verb: &str) -> Result<()> {
        let view = unsafe { self.current_view()? };
        if verb=="paste"||verb.eq_ignore_ascii_case("NewFolder"){
            if let Some(path)=super::file_transfer::filesystem_folder_path(&view)?{
                let source=unsafe{view.GetWindow()?};
                if verb!="paste"{return super::file_transfer::new_folder(source,&path);}
                // MTP, archives and virtual-file providers can offer a Shell
                // data object without CF_HDROP. Keep their native paste path.
                if super::file_transfer::has_filesystem_clipboard(){return super::file_transfer::paste(source,&path);}
            }
        }
        let menu = ShellMenu::for_view(&view, false)?;
        menu.populate_for_invoke(false)?;
        menu.invoke_verb(owner, verb, false)
    }

    /// Shows the native "新建" submenu at `point` (screen) and runs the choice.
    pub fn show_new_menu(&self, owner: HWND, point: POINT) -> Result<()> {
        let view = unsafe { self.current_view()? };
        let menu = ShellMenu::for_view(&view, false)?;
        menu.populate(false, false)?;
        // Without a recognizable "新建" submenu show the whole background menu instead.
        let submenu = menu.submenu_with_verb("NewFolder");
        let sub=submenu.unwrap_or(menu.hmenu());
        if let Some(id) = menu.track_unfiltered_initialized(owner,sub,point,submenu.is_some())? {
            if ShellMenu::is_shell_id(id) { menu.invoke(owner, id, Some(point), false, false)?; }
        }
        Ok(())
    }

    /// Starts in-place rename of the selected item.
    pub fn rename_selection(&self) -> Result<()> { unsafe { self.folder_view()?.DoRename() } }

    pub fn select_all(&self, select: bool) {
        let Ok(view) = self.folder_view() else { return };
        let count = unsafe { view.ItemCount(SVGIO_ALLVIEW) }.unwrap_or(0);
        let flag = if select { SVSI_SELECT.0 } else { SVSI_DESELECT.0 } as u32;
        for i in 0..count.min(100_000) { let _ = unsafe { view.SelectItem(i, flag) }; }
    }

    pub fn invert_selection(&self) -> Result<()> {
        invert_view_selection(&unsafe { self.current_view()? })
    }

    pub fn sort(&self) -> Option<(Column, bool)> {
        let view = self.folder_view().ok()?;
        let mut columns = [SORTCOLUMN::default()];
        unsafe { view.GetSortColumns(&mut columns).ok()? };
        Some((Column::from_key(&columns[0].propkey)?, columns[0].direction == SORT_ASCENDING))
    }

    pub fn set_sort(&self, column: Column, ascending: bool) -> Result<()> {
        let shell=unsafe{self.current_view()}?;let view:IFolderView2=shell.cast()?;
        let columns = [SORTCOLUMN { propkey: super::folder_totals::sort_key(&shell,column.key()), direction: if ascending { SORT_ASCENDING } else { SORT_DESCENDING } }];
        unsafe { view.SetSortColumns(&columns) }
    }

    pub fn group(&self) -> Option<Column> {
        let view = self.folder_view().ok()?;
        let mut key = PROPERTYKEY::default();
        let mut ascending = BOOL(1);
        unsafe { view.GetGroupBy(&mut key, Some(&mut ascending)).ok()? };
        Column::from_key(&key)
    }

    pub fn set_group(&self, column: Option<Column>) -> Result<()> {
        let shell=unsafe{self.current_view()}?;let view:IFolderView2=shell.cast()?;
        let key = super::folder_totals::sort_key(&shell,column.map_or(PKEY_NULL, Column::key));
        unsafe { view.SetGroupBy(&key, true) }
    }

    pub fn checkboxes(&self) -> bool {
        self.folder_view().ok().and_then(|v| unsafe { v.GetCurrentFolderFlags() }.ok()).is_some_and(|f| f & FWF_CHECKSELECT.0 as u32 != 0)
    }

    pub fn set_checkboxes(&self, on: bool) -> Result<()> {
        let view = self.folder_view()?;
        unsafe { view.SetCurrentFolderFlags(FWF_CHECKSELECT.0 as u32, if on { FWF_CHECKSELECT.0 as u32 } else { 0 }) }
    }

    pub fn status(&self) -> ViewStatus {
        let Ok(view) = self.folder_view() else { return ViewStatus::default() };
        let items = unsafe { view.ItemCount(SVGIO_ALLVIEW) }.unwrap_or(0).max(0) as usize;
        let selected = unsafe { view.ItemCount(SVGIO_SELECTION) }.unwrap_or(0).max(0) as usize;
        let revision = self.selection_revision.get();
        let cached = self.selection_size.borrow().get(revision, selected);
        let bytes = if let Some(bytes) = cached { bytes } else {
            // Never hold a RefCell borrow while calling third-party Shell
            // properties: COM can reenter and notify a new selection.
            let bytes = if selected > 0 {
                unsafe { view.GetSelection(false) }.ok().and_then(|array|
                    selection_size(&array, || self.selection_revision.get() == revision))
            } else { None };
            if self.selection_revision.get() == revision {
                self.selection_size.borrow_mut().store(revision, selected, bytes);
                bytes
            } else { None }
        };
        ViewStatus { items, selected, selected_bytes: bytes }
    }

    /// File-system paths (or parsing names) of the selection, or the folder itself.
    pub fn selection_paths(&self) -> Vec<String> {
        self.selection_paths_limit(5000)
    }
    pub fn selection_paths_limit(&self,limit:u32)->Vec<String>{
        let Ok(view) = self.folder_view() else { return Vec::new() };
        let Ok(array) = (unsafe { view.GetSelection(false) }) else { return Vec::new() };
        let count = unsafe { array.GetCount() }.unwrap_or(0);
        (0..count.min(limit)).filter_map(|i| {
            let item = unsafe { array.GetItemAt(i) }.ok()?;
            let name = unsafe { item.GetDisplayName(SIGDN_FILESYSPATH) }.or_else(|_| unsafe { item.GetDisplayName(SIGDN_DESKTOPABSOLUTEPARSING) }).ok()?;
            Some(unsafe { super::shell_item::take_pwstr(name) })
        }).collect()
    }
    pub fn item_paths_limit(&self,limit:u32)->Vec<String>{
        let Ok(view)=self.folder_view()else{return Vec::new();};
        let count=unsafe{view.ItemCount(SVGIO_ALLVIEW)}.unwrap_or(0).max(0)as u32;
        (0..count.min(limit)).filter_map(|i|{
            let item:IShellItem=unsafe{view.GetItem(i as i32).ok()?};
            let value=unsafe{item.GetDisplayName(SIGDN_FILESYSPATH).ok()?};
            Some(unsafe{super::shell_item::take_pwstr(value)})
        }).collect()
    }
    pub fn first_selected_path(&self)->Option<std::path::PathBuf>{
        let view=self.folder_view().ok()?;
        let array=unsafe{view.GetSelection(false).ok()?};
        let item=unsafe{array.GetItemAt(0).ok()?};
        let value=unsafe{item.GetDisplayName(SIGDN_FILESYSPATH).ok()?};
        Some(std::path::PathBuf::from(unsafe{super::shell_item::take_pwstr(value)}))
    }
}

/// Inverts the original selection, independently of the menu presentation.
pub fn invert_view_selection(shell: &windows::Win32::UI::Shell::IShellView) -> Result<()> {
    let view: IFolderView2 = shell.cast()?;
    let count = unsafe { view.ItemCount(SVGIO_ALLVIEW)? }.max(0);
    let mut original = Vec::new();
    original.try_reserve_exact(count as usize).map_err(|_| windows::core::Error::new(
        windows::core::HRESULT(0x8007000Eu32 as i32), "无法为反向选择分配状态缓冲区"))?;
    // Do not interpret a failed query as an unselected item, or query the next
    // item after changing the current selection. No UI state changes until the
    // complete original state has been read successfully.
    for index in 0..count {
        let pidl = unsafe { view.Item(index)? };
        if pidl.is_null() { return Err(windows::core::Error::new(windows::core::HRESULT(0x80004005u32 as i32), "无法读取项目的选择状态")); }
        let mut state = 0;
        let result = unsafe { (view.vtable().GetSelectionState)(view.as_raw(), pidl, &mut state) };
        unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(pidl.cast())); }
        if result != windows::core::HRESULT(0) {
            return Err(windows::core::Error::new(if result.is_err() {result} else {windows::core::HRESULT(0x80004005u32 as i32)}, "无法读取完整的选择状态"));
        }
        original.push(state & SVSI_SELECT.0 as u32 != 0);
    }
    if unsafe { view.ItemCount(SVGIO_ALLVIEW)? } != count {
        return Err(windows::core::Error::new(windows::core::HRESULT(0x8000000Au32 as i32), "文件列表已更新，请重试反向选择"));
    }
    struct Redraw<'a>(&'a IFolderView2);
    impl Drop for Redraw<'_> { fn drop(&mut self) { let _ = unsafe { self.0.SetRedraw(true) }; } }
    let _redraw = Redraw(&view);
    let _ = unsafe { view.SetRedraw(false) };
    for (index, selected) in original.into_iter().enumerate() {
        let flags = (if selected {SVSI_DESELECT.0} else {SVSI_SELECT.0}) as u32
            | windows::Win32::UI::Shell::SVSI_NOTAKEFOCUS.0 as u32;
        unsafe { view.SelectItem(index as i32, flags)?; }
    }
    Ok(())
}

fn selection_size(array: &IShellItemArray, still_current: impl Fn() -> bool) -> Option<u64> {
    let count = unsafe { array.GetCount() }.ok()?;
    let mut total = 0u64;
    let mut any = false;
    for i in 0..count {
        if !still_current() { return None; }
        let Ok(item) = (unsafe { array.GetItemAt(i) }) else { continue };
        let Ok(item2) = item.cast::<IShellItem2>() else { continue };
        if let Ok(size) = unsafe { item2.GetUInt64(&PKEY_SIZE) } { total = total.checked_add(size)?; any = true; }
    }
    any.then_some(total)
}

/// Global "show hidden items" / "show file extensions" (same switches as Explorer's View menu).
pub fn folder_settings() -> (bool, bool) {
    let mut state = SHELLSTATEA::default();
    unsafe { SHGetSetSettings(Some(&mut state), SSF_SHOWALLOBJECTS | SSF_SHOWEXTENSIONS, false) };
    let bits = state._bitfield1 as u32;
    (bits & 1 != 0, bits & 2 != 0)
}

pub fn set_folder_settings(show_hidden: Option<bool>, show_extensions: Option<bool>) {
    let mut state = SHELLSTATEA::default();
    unsafe { SHGetSetSettings(Some(&mut state), SSF_SHOWALLOBJECTS | SSF_SHOWEXTENSIONS, false) };
    let mut bits = state._bitfield1 as u32;
    let mut mask = Default::default();
    if let Some(on) = show_hidden { bits = if on { bits | 1 } else { bits & !1 }; mask = SSF_SHOWALLOBJECTS; }
    if let Some(on) = show_extensions { bits = if on { bits | 2 } else { bits & !2 }; mask |= SSF_SHOWEXTENSIONS; }
    state._bitfield1 = bits as i32;
    unsafe { SHGetSetSettings(Some(&mut state), mask, true) };
    let _: FOLDERFLAGS = FOLDERFLAGS(0);
}

#[cfg(test)]
mod selection_cache_tests {
    use super::SelectionSizeCache;
    #[test]
    fn same_count_with_a_new_selection_revision_recomputes_size() {
        let mut cache = SelectionSizeCache::default();
        cache.store(10, 2, Some(300));
        assert_eq!(cache.get(10, 2), Some(Some(300)));
        assert_eq!(cache.get(11, 2), None, "two different selected files cannot reuse the old sum");
        assert_eq!(cache.get(10, 3), None);
        cache.store(11, 2, Some(900));
        assert_eq!(cache.get(11, 2), Some(Some(900)));
    }
    #[test]
    fn unknown_property_result_is_cached_until_state_changes() {
        let mut cache = SelectionSizeCache::default();
        cache.store(4, 1, None);
        assert_eq!(cache.get(4, 1), Some(None));
        assert_eq!(cache.get(5, 1), None);
    }
}
