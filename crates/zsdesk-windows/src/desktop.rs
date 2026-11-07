//! Desktop Shell cooperation layer for fences.
//!
//! Only documented Shell interfaces are used: the desktop's own folder view is
//! reached through `IShellWindows::FindWindowSW(SWC_DESKTOP)` and
//! `IFolderView2`. Icon visibility uses `FWF_NOICONS` (the same switch as
//! "查看 → 显示桌面图标"). A visible-state check on the desktop list view
//! confirms the result; the documented `WM_COMMAND 0x7402` toggle is the
//! fallback when the view interface is unavailable.

use std::path::{Path, PathBuf};

use windows::{
    core::{w, Interface, Result, PCWSTR},
    Win32::{
        Foundation::{HWND, LPARAM, POINT, WPARAM},
        System::{
            Variant::VARIANT,
            Com::{CoCreateInstance, CoTaskMemFree, IServiceProvider, CLSCTX_ALL, CLSCTX_INPROC_SERVER},
            Ole::IDropTarget,
            Registry::{RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RRF_RT_REG_DWORD},
            SystemServices::{SFGAO_DROPTARGET, SFGAO_FILESYSTEM, SFGAO_FOLDER, SFGAO_GHOSTED, SFGAO_LINK, SFGAO_STREAM, SFGAO_FLAGS},
        },
        UI::{
            Accessibility::{AccessibleObjectFromPoint, IAccessible},
            Shell::{
                Common::{ITEMIDLIST, STRRET}, FileOpenDialog, IEnumIDList, IFileOpenDialog, IFolderView2, IShellBrowser,
                IShellFolder, IShellWindows, ILCombine, SHBindToParent, SHGetDesktopFolder, SHGetKnownFolderPath,
                SHGetNameFromIDList, ShellExecuteExW, ShellWindows, StrRetToStrW, FOLDERID_Desktop, FOLDERID_PublicDesktop,
                FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, FWF_NOICONS, KF_FLAG_DEFAULT, SEE_MASK_INVOKEIDLIST, SHCONTF_FOLDERS,
                SHCONTF_INCLUDEHIDDEN, SHCONTF_NONFOLDERS, SHELLEXECUTEINFOW, SHGDN_FOREDITING, SHGDN_INFOLDER, SHGDN_NORMAL,
                SIGDN_DESKTOPABSOLUTEPARSING, SIGDN_FILESYSPATH, SVGIO_ALLVIEW, SWC_DESKTOP, SWFO_NEEDDISPATCH, SID_STopLevelBrowser,
                IShellItem,
            },
            WindowsAndMessaging::{
                FindWindowExW, FindWindowW, GetAncestor, GetClassNameW, GetParent, IsWindowVisible, SendMessageW,
                WindowFromPoint, GA_ROOT, SW_SHOWNORMAL, WM_COMMAND, EnumWindows,
            },
        },
    },
};

use crate::shell::{menu::items_ui_object, pidl};

/// A Shell item shown in a fence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellEntry {
    /// Desktop-absolute parsing name ("C:\\…\\a.txt" or "::{CLSID}").
    pub key: String,
    pub name: String,
    pub pidl: Vec<u8>,
    pub is_folder: bool,
    pub is_file_system: bool,
    pub is_link: bool,
    pub is_stream: bool,
    pub can_drop: bool,
    pub ghosted: bool,
    pub path: Option<PathBuf>,
    pub size: Option<u64>,
    pub modified: Option<u64>,
}

impl ShellEntry {
    /// True for folders that should open as folders (not .zip files).
    pub fn is_container(&self) -> bool { self.is_folder && !self.is_stream }
}

/// `Progman`, the owner for desktop-level windows (survives "显示桌面").
pub fn progman() -> Option<HWND> { unsafe { FindWindowW(w!("Progman"), PCWSTR::null()) }.ok() }

fn class_name(hwnd: HWND) -> String {
    let mut buffer = [0u16; 64];
    let len = unsafe { GetClassNameW(hwnd, &mut buffer) }.max(0) as usize;
    String::from_utf16_lossy(&buffer[..len])
}

/// The desktop `SHELLDLL_DefView` (under Progman, or a WorkerW when a wallpaper host moved it).
pub fn desktop_defview() -> Option<HWND> {
    if let Some(progman) = progman() {
        if let Ok(view) = unsafe { FindWindowExW(Some(progman), None, w!("SHELLDLL_DefView"), PCWSTR::null()) } { return Some(view); }
    }
    struct Search(Option<HWND>);
    unsafe extern "system" fn each(hwnd: HWND, lp: LPARAM) -> windows::core::BOOL {
        let search = unsafe { &mut *(lp.0 as *mut Search) };
        if let Ok(view) = unsafe { FindWindowExW(Some(hwnd), None, w!("SHELLDLL_DefView"), PCWSTR::null()) } {
            search.0 = Some(view);
            return false.into();
        }
        true.into()
    }
    let mut search = Search(None);
    let _ = unsafe { EnumWindows(Some(each), LPARAM(&mut search as *mut Search as isize)) };
    search.0
}

fn desktop_list() -> Option<HWND> {
    let view = desktop_defview()?;
    unsafe { FindWindowExW(Some(view), None, w!("SysListView32"), PCWSTR::null()) }.ok()
}

/// The desktop's `IFolderView2` (lives in Explorer; calls are marshaled).
pub fn desktop_view() -> Result<IFolderView2> {
    let windows: IShellWindows = unsafe { CoCreateInstance(&ShellWindows, None, CLSCTX_ALL)? };
    let location = VARIANT::from(0i32); // CSIDL_DESKTOP
    let empty = VARIANT::default();
    let mut hwnd = 0i32;
    let dispatch = unsafe { windows.FindWindowSW(&location, &empty, SWC_DESKTOP, &mut hwnd, SWFO_NEEDDISPATCH)? };
    let provider: IServiceProvider = dispatch.cast()?;
    let browser: IShellBrowser = unsafe { provider.QueryService(&SID_STopLevelBrowser)? };
    let view = unsafe { browser.QueryActiveShellView()? };
    view.cast()
}

/// Whether Explorer currently shows desktop icons.
pub fn icons_visible() -> bool {
    if let Some(list) = desktop_list() { return unsafe { IsWindowVisible(list) }.as_bool(); }
    desktop_view().ok().and_then(|v| unsafe { v.GetCurrentFolderFlags() }.ok()).is_none_or(|f| f & FWF_NOICONS.0 as u32 == 0)
}

/// Shows or hides Explorer's desktop icons. Returns the resulting visibility.
pub fn set_icons_visible(visible: bool) -> bool {
    if icons_visible() == visible { return visible; }
    if let Ok(view) = desktop_view() {
        let _ = unsafe { view.SetCurrentFolderFlags(FWF_NOICONS.0 as u32, if visible { 0 } else { FWF_NOICONS.0 as u32 }) };
    }
    if icons_visible() != visible {
        if let Some(defview) = desktop_defview() {
            unsafe { SendMessageW(defview, WM_COMMAND, Some(WPARAM(0x7402)), Some(LPARAM(0))); }
        }
    }
    icons_visible()
}

/// Result of classifying a screen point for the desktop double-click gesture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DesktopHit { NotDesktop, Empty, Item }

pub fn classify_point(point: POINT) -> DesktopHit {
    let hwnd = unsafe { WindowFromPoint(point) };
    if hwnd.0.is_null() { return DesktopHit::NotDesktop; }
    let class = class_name(hwnd);
    match class.as_str() {
        "SHELLDLL_DefView" | "WorkerW" | "Progman" => return DesktopHit::Empty,
        "SysListView32" => {}
        _ => return DesktopHit::NotDesktop,
    }
    let parent = unsafe { GetParent(hwnd) }.unwrap_or_default();
    if class_name(parent) != "SHELLDLL_DefView" { return DesktopHit::NotDesktop; }
    let root = unsafe { GetAncestor(parent, GA_ROOT) };
    if !matches!(class_name(root).as_str(), "Progman" | "WorkerW") { return DesktopHit::NotDesktop; }
    let mut accessible: Option<IAccessible> = None;
    let mut child = VARIANT::default();
    if unsafe { AccessibleObjectFromPoint(point, &mut accessible, &mut child) }.is_err() { return DesktopHit::Empty; }
    match i32::try_from(&child) { Ok(0) | Err(_) => DesktopHit::Empty, Ok(_) => DesktopHit::Item }
}

fn show_hidden() -> bool { crate::shell::commands::folder_settings().0 }

/// Items on the desktop, in the order Explorer's desktop view reports them.
pub fn enumerate_desktop() -> Vec<ShellEntry> {
    let Ok(desktop) = (unsafe { SHGetDesktopFolder() }) else { return Vec::new() };
    let empty: Vec<u8> = vec![0, 0];
    // Prefer the live desktop view: it already applies "桌面图标设置" and hidden-file rules.
    if let Ok(view) = desktop_view() {
        if let Ok(items) = unsafe { view.Items::<IEnumIDList>(SVGIO_ALLVIEW) } {
            let entries = collect(&desktop, &empty, &items, 5000);
            if !entries.is_empty() { return entries; }
        }
    }
    let flags = (SHCONTF_FOLDERS.0 | SHCONTF_NONFOLDERS.0 | if show_hidden() { SHCONTF_INCLUDEHIDDEN.0 } else { 0 }) as u32;
    let mut items = None;
    let _ = unsafe { desktop.EnumObjects(HWND::default(), flags, &mut items) };
    let Some(items) = items else { return Vec::new() };
    let mut entries = collect(&desktop, &empty, &items, 5000);
    entries.retain(|e| e.is_file_system || namespace_icon_visible(&e.key));
    entries
}

/// Default visibility of desktop namespace icons ("桌面图标设置").
fn namespace_icon_visible(key: &str) -> bool {
    let Some(clsid) = key.strip_prefix("::") else { return true };
    let name: Vec<u16> = clsid.encode_utf16().chain(Some(0)).collect();
    let mut value = 0u32;
    let mut size = 4u32;
    let ok = unsafe { RegGetValueW(HKEY_CURRENT_USER, w!("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\HideDesktopIcons\\NewStartPanel"),
        PCWSTR(name.as_ptr()), RRF_RT_REG_DWORD, None, Some((&mut value as *mut u32).cast()), Some(&mut size)) }.is_ok();
    if ok { return value == 0; }
    // Without a stored value only the Recycle Bin is shown.
    clsid.eq_ignore_ascii_case("{645FF040-5081-101B-9F08-00AA002F954E}")
}

/// Contents of a folder (absolute path).
pub fn enumerate_folder(path: &Path) -> Result<Vec<ShellEntry>> {
    let item: IShellItem = unsafe { crate::shell::shell_item(&zsdesk_core::model::explorer::ShellLocation::Path(path.to_path_buf()))? };
    let parent = pidl::item_pidl(&item).unwrap_or_else(|| vec![0, 0]);
    let folder: IShellFolder = unsafe { item.BindToHandler(None, &windows::Win32::UI::Shell::BHID_SFObject)? };
    let flags = (SHCONTF_FOLDERS.0 | SHCONTF_NONFOLDERS.0 | if show_hidden() { SHCONTF_INCLUDEHIDDEN.0 } else { 0 }) as u32;
    let mut items = None;
    unsafe { folder.EnumObjects(HWND::default(), flags, &mut items) }.ok()?;
    let Some(items) = items else { return Ok(Vec::new()) };
    Ok(collect(&folder, &parent, &items, 3000))
}

/// Resolves a saved member even when the live desktop view is hidden or omits it.
pub fn entry_from_parsing_name(key: &str) -> Option<ShellEntry> {
    let location = if key.starts_with("::{") || key.starts_with("shell:") {
        zsdesk_core::model::explorer::ShellLocation::Shell { parsing_name: key.into(), title: String::new() }
    } else { zsdesk_core::model::explorer::ShellLocation::Path(PathBuf::from(key)) };
    let absolute = pidl::location_pidl(&location)?;
    let mut child: *mut ITEMIDLIST = std::ptr::null_mut();
    let parent: IShellFolder = unsafe { SHBindToParent(absolute.as_ptr().cast(), Some(&mut child)).ok()? };
    if child.is_null() { return None; }
    let parent_pidl = crate::shell::menu::parent_pidl(&absolute);
    describe(&parent, &parent_pidl, child).map(|mut entry| {
        entry.key = key.to_string();
        entry
    })
}

fn collect(folder: &IShellFolder, parent: &[u8], items: &IEnumIDList, limit: usize) -> Vec<ShellEntry> {
    let mut entries = Vec::new();
    loop {
        let mut child = [std::ptr::null_mut::<ITEMIDLIST>()];
        let mut fetched = 0u32;
        if unsafe { items.Next(&mut child, Some(&mut fetched)) }.is_err() || fetched == 0 || child[0].is_null() { break; }
        let entry = describe(folder, parent, child[0]);
        unsafe { CoTaskMemFree(Some(child[0].cast())) };
        if let Some(entry) = entry { entries.push(entry); }
        if entries.len() >= limit { break; }
    }
    entries
}

fn describe(folder: &IShellFolder, parent: &[u8], child: *mut ITEMIDLIST) -> Option<ShellEntry> {
    let absolute = unsafe { ILCombine(Some(parent.as_ptr().cast()), Some(child)) };
    if absolute.is_null() { return None; }
    let pidl = pidl::pidl_bytes(absolute);
    let key = unsafe { SHGetNameFromIDList(absolute, SIGDN_DESKTOPABSOLUTEPARSING) }.ok().map(|v| unsafe { crate::shell::take(v) });
    let path = unsafe { SHGetNameFromIDList(absolute, SIGDN_FILESYSPATH) }.ok().map(|v| unsafe { crate::shell::take(v) }).filter(|p| !p.is_empty()).map(PathBuf::from);
    unsafe { CoTaskMemFree(Some(absolute.cast())) };
    let name = display_name(folder, child, SHGDN_NORMAL.0 as u32)?;
    let mut attrs = SFGAO_FLAGS(SFGAO_FOLDER.0 | SFGAO_FILESYSTEM.0 | SFGAO_LINK.0 | SFGAO_DROPTARGET.0 | SFGAO_GHOSTED.0 | SFGAO_STREAM.0);
    let children = [child as *const ITEMIDLIST];
    let _ = unsafe { folder.GetAttributesOf(&children, &mut attrs.0 as *mut u32 as *mut _) };
    let a = attrs.0;
    let (size, modified) = path.as_ref().and_then(|p| std::fs::metadata(p).ok()).map(|m| {
        let modified = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_millis() as u64);
        (if m.is_file() { Some(m.len()) } else { None }, modified)
    }).unwrap_or((None, None));
    Some(ShellEntry {
        key: key.unwrap_or_else(|| name.clone()), name, pidl,
        is_folder: a & SFGAO_FOLDER.0 != 0, is_file_system: a & SFGAO_FILESYSTEM.0 != 0, is_link: a & SFGAO_LINK.0 != 0,
        is_stream: a & SFGAO_STREAM.0 != 0, can_drop: a & SFGAO_DROPTARGET.0 != 0, ghosted: a & SFGAO_GHOSTED.0 != 0,
        path, size, modified,
    })
}

fn display_name(folder: &IShellFolder, child: *const ITEMIDLIST, flags: u32) -> Option<String> {
    let mut value = STRRET::default();
    unsafe { folder.GetDisplayNameOf(child, windows::Win32::UI::Shell::SHGDNF(flags), &mut value) }.ok()?;
    let mut text = windows::core::PWSTR::null();
    unsafe { StrRetToStrW(&mut value, Some(child), &mut text) }.ok()?;
    Some(unsafe { crate::shell::take(text) })
}

/// Name shown in the rename box (respects "显示文件扩展名").
pub fn editing_name(pidl: &[u8]) -> Option<String> {
    let mut last: *mut ITEMIDLIST = std::ptr::null_mut();
    let parent: IShellFolder = unsafe { SHBindToParent(pidl.as_ptr().cast(), Some(&mut last)).ok()? };
    display_name(&parent, last, (SHGDN_INFOLDER.0 | SHGDN_FOREDITING.0) as u32)
}

/// Renames through the parent folder (`IShellFolder::SetNameOf`).
pub fn rename(owner: HWND, pidl: &[u8], name: &str) -> Result<()> {
    let mut last: *mut ITEMIDLIST = std::ptr::null_mut();
    let parent: IShellFolder = unsafe { SHBindToParent(pidl.as_ptr().cast(), Some(&mut last))? };
    let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    let mut out: *mut ITEMIDLIST = std::ptr::null_mut();
    unsafe { parent.SetNameOf(Some(owner), last, PCWSTR(wide.as_ptr()), windows::Win32::UI::Shell::SHGDNF((SHGDN_INFOLDER.0 | SHGDN_FOREDITING.0) as u32), Some(&mut out))? };
    if !out.is_null() { unsafe { CoTaskMemFree(Some(out.cast())) }; }
    Ok(())
}

/// Opens an item with its default verb (works for virtual items too).
pub fn open(owner: HWND, pidl: &[u8], verb: Option<&str>) -> Result<()> {
    let verb: Option<Vec<u16>> = verb.map(|v| v.encode_utf16().chain(Some(0)).collect());
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32, fMask: SEE_MASK_INVOKEIDLIST, hwnd: owner,
        lpVerb: verb.as_ref().map_or(PCWSTR::null(), |v| PCWSTR(v.as_ptr())), nShow: SW_SHOWNORMAL.0,
        lpIDList: pidl.as_ptr() as *mut _, ..Default::default()
    };
    unsafe { ShellExecuteExW(&mut info) }
}

/// Drop target of a folder item (Shell copy/move semantics).
pub fn item_drop_target(owner: HWND, pidl: &[u8]) -> Result<IDropTarget> { items_ui_object(owner, &[pidl.to_vec()]) }

/// Drop target of a file-system folder.
pub fn folder_drop_target(owner: HWND, path: &Path) -> Result<IDropTarget> {
    let pidl = pidl::location_pidl(&zsdesk_core::model::explorer::ShellLocation::Path(path.to_path_buf()))
        .ok_or_else(|| windows::core::Error::from_hresult(windows::core::HRESULT(0x80070003u32 as i32)))?;
    item_drop_target(owner, &pidl)
}

/// The data object for dragging or copying items.
pub fn data_object(owner: HWND, pidls: &[Vec<u8>]) -> Result<windows::Win32::System::Com::IDataObject> { items_ui_object(owner, pidls) }

/// Background folder for "新建" / "粘贴" in a fence.
pub fn folder_for_path(path: &Path) -> Result<IShellFolder> {
    let item: IShellItem = unsafe { crate::shell::shell_item(&zsdesk_core::model::explorer::ShellLocation::Path(path.to_path_buf()))? };
    unsafe { item.BindToHandler(None, &windows::Win32::UI::Shell::BHID_SFObject) }
}

pub fn user_desktop() -> Option<PathBuf> { known_folder(&FOLDERID_Desktop) }
pub fn public_desktop() -> Option<PathBuf> { known_folder(&FOLDERID_PublicDesktop) }

fn known_folder(id: &windows::core::GUID) -> Option<PathBuf> {
    let path = unsafe { SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()? };
    Some(PathBuf::from(unsafe { crate::shell::take(path) }))
}

/// Folder picker ("选择要映射的文件夹").
pub fn pick_folder(owner: HWND, title: &str) -> Option<PathBuf> {
    let dialog: IFileOpenDialog = unsafe { CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()? };
    let title: Vec<u16> = title.encode_utf16().chain(Some(0)).collect();
    unsafe {
        let options = dialog.GetOptions().ok()?;
        dialog.SetOptions(options | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM).ok()?;
        let _ = dialog.SetTitle(PCWSTR(title.as_ptr()));
        dialog.Show(Some(owner)).ok()?;
        let item = dialog.GetResult().ok()?;
        let path = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        Some(PathBuf::from(crate::shell::take(path)))
    }
}

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: PCWSTR = w!("ZSDesk.Desktop");

pub fn autostart_command() -> Option<String> {
    let mut buffer = vec![0u16; 2048];
    let mut size = (buffer.len() * 2) as u32;
    unsafe { RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE, RRF_RT_REG_SZ, None, Some(buffer.as_mut_ptr().cast()), Some(&mut size)) }.ok().ok()?;
    let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    Some(String::from_utf16_lossy(&buffer[..end]))
}

/// Adds or removes the per-user autostart entry for the desktop fences host.
pub fn set_autostart(command: Option<&str>) -> bool {
    match command {
        Some(command) => {
            let data: Vec<u16> = command.encode_utf16().chain(Some(0)).collect();
            unsafe { RegSetKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE, REG_SZ.0, Some(data.as_ptr().cast()), (data.len() * 2) as u32) }.is_ok()
        }
        None => unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE) }.is_ok(),
    }
}

const WIN_E_RUN_VALUE: PCWSTR = w!("ZSDesk.WinE");

pub fn win_e_agent_command() -> Option<String> {
    let mut buffer = vec![0u16; 2048];
    let mut size = (buffer.len() * 2) as u32;
    unsafe { RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, WIN_E_RUN_VALUE, RRF_RT_REG_SZ, None, Some(buffer.as_mut_ptr().cast()), Some(&mut size)) }.ok().ok()?;
    let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    Some(String::from_utf16_lossy(&buffer[..end]))
}

/// Registers only the opt-in keyboard agent; it is independent of the desktop-fence autostart value.
pub fn set_win_e_agent_autostart(command: Option<&str>) -> bool {
    match command {
        Some(command) => {
            let data: Vec<u16> = command.encode_utf16().chain(Some(0)).collect();
            unsafe { RegSetKeyValueW(HKEY_CURRENT_USER, RUN_KEY, WIN_E_RUN_VALUE, REG_SZ.0, Some(data.as_ptr().cast()), (data.len() * 2) as u32) }.is_ok()
        }
        None => unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, WIN_E_RUN_VALUE) }.is_ok(),
    }
}
