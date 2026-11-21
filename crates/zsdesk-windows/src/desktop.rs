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
        Foundation::{HWND, LPARAM, POINT, RECT, WPARAM},
        System::{
            Variant::VARIANT,
            Com::{CoCreateInstance, CoTaskMemFree, IServiceProvider, CLSCTX_ALL, CLSCTX_INPROC_SERVER},
            Ole::IDropTarget,
            Registry::{RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, REG_SZ, RRF_RT_REG_SZ, RRF_RT_REG_DWORD},
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
                IShellItem, IShellItem2,
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

/// Read the position of a new item from Explorer's desktop view.
pub struct DesktopPositions {
    view: IFolderView2,
    origin: POINT,
}
impl DesktopPositions {
    pub fn read() -> Option<Self> {
        let view = desktop_view().ok()?;
        let list = desktop_list().or_else(desktop_defview)?;
        let mut origin = POINT::default();
        unsafe { windows::Win32::Graphics::Gdi::ClientToScreen(list, &mut origin) }.ok().ok()?;
        Some(Self { view, origin })
    }
    pub fn position(&self, item: &ShellEntry) -> Option<POINT> {
        if item.pidl.len() <= 2 || !pidl::valid(&item.pidl) { return None; }
        let point = unsafe { self.view.GetItemPosition(item.pidl.as_ptr().cast()) }.ok()?;
        Some(POINT { x: self.origin.x.checked_add(point.x)?, y: self.origin.y.checked_add(point.y)? })
    }
}

/// Read Explorer's alignment policy and lattice without changing its layout.
/// Coordinates and spacing are physical pixels in the caller's PMv2 context.
#[derive(Clone, Copy, Debug)]
pub struct DesktopIconGrid { pub flags:u32, pub spacing:POINT, pub origin:POINT, pub dpi:u32 }
pub fn icon_grid(work:RECT)->Result<DesktopIconGrid>{
    let view=desktop_view()?;
    let flags=unsafe{view.GetCurrentFolderFlags()?};
    let mut spacing=POINT::default();unsafe{view.GetSpacing(&mut spacing)?;}
    if spacing.x<=0||spacing.y<=0{return Err(windows::core::Error::from_hresult(windows::core::HRESULT(0x80004005u32 as i32)));}
    let list=desktop_list().or_else(desktop_defview).ok_or_else(windows::core::Error::from_thread)?;
    let dpi=unsafe{windows::Win32::UI::HiDpi::GetDpiForWindow(list)}.max(96);
    let mut screen=POINT::default();let _=unsafe{windows::Win32::Graphics::Gdi::ClientToScreen(list,&mut screen)};
    let mut origin=POINT{x:work.left,y:work.top};let mut first=None;
    // Prefer an item on the destination monitor: work-area insets and mixed
    // monitor origins need not share the primary monitor's lattice phase.
    let count=unsafe{view.ItemCount(SVGIO_ALLVIEW)}.unwrap_or(0).min(4096);
    for index in 0..count{
        let Ok(item)=(unsafe{view.Item(index)})else{continue;};
        let point=unsafe{view.GetItemPosition(item)};unsafe{CoTaskMemFree(Some(item.cast()))};
        let Ok(point)=point else{continue;};
        first.get_or_insert(point);
        let point=POINT{x:screen.x+point.x,y:screen.y+point.y};
        if point.x>=work.left&&point.x<work.right&&point.y>=work.top&&point.y<work.bottom{
            origin=point;first=None;break;
        }
    }
    if let Some(point)=first{origin=POINT{x:work.left+point.x.rem_euclid(spacing.x),y:work.top+point.y.rem_euclid(spacing.y)};}
    Ok(DesktopIconGrid{flags,spacing,origin,dpi})
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
    let live=desktop_view().and_then(|view|{
        let flags_before=unsafe{view.GetCurrentFolderFlags()}?;
        if flags_before&FWF_NOICONS.0 as u32!=0{return Ok(DesktopSnapshot{entries:Vec::new(),flags_before,flags_after:flags_before});}
        let items=unsafe{view.Items::<IEnumIDList>(SVGIO_ALLVIEW)}?;
        let entries=collect_desktop_snapshot(&desktop,&empty,&items,5000)?;
        let flags_after=unsafe{view.GetCurrentFolderFlags()}?;
        Ok(DesktopSnapshot{entries,flags_before,flags_after})
    });
    select_desktop_snapshot(live,||{
        let flags = (SHCONTF_FOLDERS.0 | SHCONTF_NONFOLDERS.0 | if show_hidden() { SHCONTF_INCLUDEHIDDEN.0 } else { 0 }) as u32;
        let mut items = None;
        let _ = unsafe { desktop.EnumObjects(HWND::default(), flags, &mut items) };
        let Some(items) = items else { return Vec::new() };
        let roots:Vec<_>=[user_desktop(),public_desktop()].into_iter().flatten().collect();
        let mut entries = collect(&desktop, &empty, &items, 5000);
        entries.retain(|entry|fallback_desktop_entry(entry,&roots,||{
            // Desktop namespace aliases can have filesystem parsing names,
            // e.g. Documents or a cloud drive. Keep their original PIDLs and
            // apply the namespace visibility policy instead of treating every
            // filesystem-backed alias as a physical desktop file.
            let namespace=desktop_namespace_key(entry);
            namespace.as_deref().is_some_and(namespace_icon_visible)
        }));entries
    })
}

struct DesktopSnapshot{entries:Vec<ShellEntry>,flags_before:u32,flags_after:u32}
fn select_desktop_snapshot(live:Result<DesktopSnapshot>,fallback:impl FnOnce()->Vec<ShellEntry>)->Vec<ShellEntry>{
    // FWF_NOICONS is used by our organize mode itself and can suppress the
    // view inventory. Empty is authoritative only when neither boundary read
    // is suppressed; a flag-query/read failure also uses the filtered source.
    match live{
        Ok(snapshot) if (snapshot.flags_before|snapshot.flags_after)&FWF_NOICONS.0 as u32==0=>snapshot.entries,
        _=>fallback(),
    }
}
fn direct_desktop_child(entry:&ShellEntry,roots:&[PathBuf])->bool{
    use std::os::windows::ffi::OsStrExt;
    let units=|path:&Path|path.as_os_str().encode_wide().map(|unit|if unit==47{92}else{unit}).collect::<Vec<_>>();
    entry.is_file_system&&entry.path.as_deref().and_then(Path::parent).is_some_and(|parent|roots.iter().any(|root|{
        let mut parent=units(parent);let mut root=units(root);while parent.len()>3&&parent.last()==Some(&92){parent.pop();}while root.len()>3&&root.last()==Some(&92){root.pop();}
        (unsafe{windows::Win32::Globalization::CompareStringOrdinal(&parent,&root,true)})==windows::Win32::Globalization::CSTR_EQUAL
    }))
}
fn fallback_desktop_entry(entry:&ShellEntry,roots:&[PathBuf],namespace_visible:impl FnOnce()->bool)->bool{
    direct_desktop_child(entry,roots)||namespace_visible()
}
fn desktop_namespace_key(entry:&ShellEntry)->Option<String>{
    // System.NamespaceCLSID is public Shell metadata (propkey.h); parsing-name
    // strings alone lose this identity for filesystem-backed namespace icons.
    const KEY:windows::Win32::Foundation::PROPERTYKEY=windows::Win32::Foundation::PROPERTYKEY{
        fmtid:windows::core::GUID::from_u128(0x28636aa6_953d_11d2_b5d6_00c04fd918d0),pid:6};
    let namespace=pidl::item_from_pidl(&entry.pidl).and_then(|item|item.cast::<IShellItem2>())
        .and_then(|item|unsafe{item.GetCLSID(&KEY)}).ok().filter(|id|*id!=windows::core::GUID::zeroed());
    namespace.map(|id|format!("::{{{id:?}}}")).or_else(||entry.key.starts_with("::{").then(||entry.key.clone()))
}
fn namespace_visibility(user:Option<u32>,machine:Option<u32>,default_visible:bool)->bool{
    user.or(machine).map_or(default_visible,|value|value==0)
}

/// Default visibility of desktop namespace icons ("桌面图标设置").
fn namespace_icon_visible(key: &str) -> bool {
    let Some(clsid) = key.strip_prefix("::") else { return false };
    let name: Vec<u16> = clsid.encode_utf16().chain(Some(0)).collect();
    let read=|hive|{let mut value=0u32;let mut size=4u32;
        unsafe{RegGetValueW(hive,w!("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\HideDesktopIcons\\NewStartPanel"),
            PCWSTR(name.as_ptr()),RRF_RT_REG_DWORD,None,Some((&mut value as*mut u32).cast()),Some(&mut size))}.is_ok().then_some(value)};
    // Without a stored value only the Recycle Bin is shown.
    namespace_visibility(read(HKEY_CURRENT_USER),read(HKEY_LOCAL_MACHINE),clsid.eq_ignore_ascii_case("{645FF040-5081-101B-9F08-00AA002F954E}"))
}

fn collect_desktop_snapshot(folder:&IShellFolder,parent:&[u8],items:&IEnumIDList,limit:usize)->Result<Vec<ShellEntry>>{
    let mut entries=Vec::new();loop{
        let mut child=[std::ptr::null_mut::<ITEMIDLIST>()];let mut fetched=0;
        let status=unsafe{items.Next(&mut child,Some(&mut fetched))};
        if status.is_err()||fetched==0||entries.len()>=limit{
            if !child[0].is_null(){unsafe{CoTaskMemFree(Some(child[0].cast()))};}
            status.ok()?;if fetched==0{return Ok(entries);}
            return Err(windows::core::Error::from_hresult(windows::core::HRESULT(0x800700eau32 as i32)));
        }
        let entry=if !child[0].is_null(){let entry=describe(folder,parent,child[0]);unsafe{CoTaskMemFree(Some(child[0].cast()))};entry}else{None};
        entries.push(entry.ok_or_else(||windows::core::Error::from_hresult(windows::core::HRESULT(0x80004005u32 as i32)))?);
    }
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
        zsdesk_core::model::explorer::ShellLocation::Shell { pidl:None, parsing_name: key.into(), title: String::new() }
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

/// The original edition retains its established registry value name.
pub const fn autostart_value(themed:bool)->&'static str {
    if themed {"ZSDesk.Theme.Desktop"} else {"ZSDesk.Desktop"}
}

pub fn autostart_command(value_name:&str) -> Result<Option<String>> {
    let name:Vec<u16>=value_name.encode_utf16().chain(Some(0)).collect();
    let mut size=0;
    let status=unsafe {RegGetValueW(HKEY_CURRENT_USER,RUN_KEY,PCWSTR(name.as_ptr()),RRF_RT_REG_SZ,None,None,Some(&mut size))};
    if status.0==2 {return Ok(None);}
    status.ok()?;
    if !(2..=65_536).contains(&size) || size%2!=0 {
        return Err(windows::core::Error::new(windows::core::HRESULT(0x8007000Du32 as i32),"登录启动项格式无效"));
    }
    let mut buffer=vec![0u16;size as usize/2+1];
    unsafe {RegGetValueW(HKEY_CURRENT_USER,RUN_KEY,PCWSTR(name.as_ptr()),RRF_RT_REG_SZ,None,
        Some(buffer.as_mut_ptr().cast()),Some(&mut size))}.ok()?;
    let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    Ok(Some(String::from_utf16_lossy(&buffer[..end])))
}

/// Commands include both the absolute executable and explicit profile directory.
/// Extra arguments or another executable/profile never count as owned state.
pub fn autostart_owned(current:&str,expected:&str)->bool {
    !expected.trim().is_empty() && current.trim().eq_ignore_ascii_case(expected.trim())
}

#[derive(Clone,Copy,Debug,PartialEq,Eq)]
enum AutostartChange { Unchanged, Write, Delete }
fn autostart_change(current:Option<&str>,expected:&str,enabled:bool)->Result<AutostartChange> {
    if expected.trim().is_empty() || current.is_some_and(|value|!autostart_owned(value,expected)) {
        return Err(windows::core::Error::new(windows::core::HRESULT(0x800700B7u32 as i32),"登录启动项属于另一安装或配置，现有设置已保留"));
    }
    Ok(match (current.is_some(),enabled) {
        (true,false)=>AutostartChange::Delete,(false,true)=>AutostartChange::Write,_=>AutostartChange::Unchanged,
    })
}

/// Changes only an absent entry or the exact command owned by this installation
/// and profile. Conflicting entries are neither replaced nor removed.
pub fn set_autostart(value_name:&str,expected:&str,enabled:bool)->Result<()> {
    let current=autostart_command(value_name)?;
    let change=autostart_change(current.as_deref(),expected,enabled)?;
    if change==AutostartChange::Unchanged {return Ok(());}
    // A second read prevents acting on an entry changed while the menu was open.
    if autostart_command(value_name)?!=current {
        return Err(windows::core::Error::new(windows::core::HRESULT(0x800700AAu32 as i32),"登录启动项已变化，请重新打开设置"));
    }
    let name:Vec<u16>=value_name.encode_utf16().chain(Some(0)).collect();
    match change {
        AutostartChange::Write => {
            let data: Vec<u16> = expected.encode_utf16().chain(Some(0)).collect();
            unsafe { RegSetKeyValueW(HKEY_CURRENT_USER, RUN_KEY, PCWSTR(name.as_ptr()), REG_SZ.0, Some(data.as_ptr().cast()), (data.len() * 2) as u32) }.ok()
        }
        AutostartChange::Delete => unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, PCWSTR(name.as_ptr())) }.ok(),
        AutostartChange::Unchanged=>Ok(()),
    }
}

#[cfg(test)]mod desktop_inventory_tests{
    use super::*;
    fn entry(path:&str)->ShellEntry{ShellEntry{key:path.into(),name:path.into(),pidl:vec![2,0,0,0],is_folder:true,is_file_system:true,is_link:false,is_stream:false,can_drop:true,ghosted:false,path:Some(path.into()),size:None,modified:None}}
    #[test]fn empty_live_desktop_is_not_replaced_with_namespace_enumeration(){
        assert!(select_desktop_snapshot(Ok(DesktopSnapshot{entries:Vec::new(),flags_before:0,flags_after:0}),||panic!("successful unsuppressed empty view must remain empty")).is_empty());
        let fallback=entry(r"C:\Users\Test\Desktop\file.txt");
        let value=select_desktop_snapshot(Err(windows::core::Error::from_hresult(windows::core::HRESULT(0x80004005u32 as i32))),||vec![fallback.clone()]);
        assert_eq!(value,vec![fallback]);
    }
    #[test]fn suppressed_live_inventory_uses_filtered_desktop_items_instead_of_deleting_them(){
        let physical=entry(r"C:\Users\Test\Desktop\file.txt");
        let roots=vec![PathBuf::from(r"C:\Users\Test\Desktop")];
        let hidden_alias=entry(r"C:\Users\Test\Documents");
        let suppressed=FWF_NOICONS.0 as u32;
        for (flags_before,flags_after) in [(suppressed,suppressed),(suppressed,0),(0,suppressed)]{
            let actual=select_desktop_snapshot(Ok(DesktopSnapshot{entries:Vec::new(),flags_before,flags_after}),||{
                [physical.clone(),hidden_alias.clone()].into_iter().filter(|item|fallback_desktop_entry(item,&roots,||false)).collect()
            });
            assert_eq!(actual,vec![physical.clone()],"hiding desktop icons must preserve real automatic loose items without admitting hidden namespace aliases");
        }
        let actual=select_desktop_snapshot(Ok(DesktopSnapshot{entries:vec![physical.clone()],flags_before:0,flags_after:0}),||panic!("complete live view needs no fallback"));
        assert_eq!(actual,vec![physical]);
    }
    #[test]fn filesystem_aliases_are_not_physical_desktop_children(){
        let roots=vec![PathBuf::from(r"C:\Users\Test\Desktop"),PathBuf::from(r"C:\Users\Public\Desktop")];
        for path in [r"C:\Users\Test\Desktop",r"C:\Users\Test",r"C:\Users\Test\Documents",r"C:\Users\Test\Music",r"C:\Users\Test\Downloads",r"C:\Users\Test\Pictures",r"C:\Users\Test\Videos",r"E:\Cloud\Provider"]{
            assert!(!fallback_desktop_entry(&entry(path),&roots,||false),"hidden namespace alias admitted: {path}");
        }
        for path in [r"C:\Users\Test\Desktop\folder",r"c:\users\test\desktop\link.lnk",r"C:\Users\Public\Desktop\shared.lnk"]{
            assert!(fallback_desktop_entry(&entry(path),&roots,||panic!("physical desktop child needs no namespace exception")));
        }
        assert!(!fallback_desktop_entry(&entry(r"C:\Users\Test\Desktop\folder\nested.txt"),&roots,||false));
        let visible_namespace=entry(r"C:\Users\Test");let saved=visible_namespace.clone();
        assert!(fallback_desktop_entry(&visible_namespace,&roots,||true));assert_eq!(visible_namespace,saved,"visibility checking must preserve the original namespace PIDL");
    }
    #[test]fn desktop_namespace_visibility_honors_user_override_and_machine_defaults(){
        assert!(namespace_visibility(Some(0),Some(1),false));assert!(!namespace_visibility(Some(1),Some(0),true));
        assert!(namespace_visibility(None,Some(0),false));assert!(!namespace_visibility(None,Some(1),true));
        assert!(namespace_visibility(None,None,true));assert!(!namespace_visibility(None,None,false));
    }
}

#[cfg(test)]
mod autostart_tests {
    use super::*;
    const OWN:&str=r#""C:\Apps\ZSDesk\ZSDesk.exe" --desktop --profile-dir "C:\Profiles\Main""#;
    #[test]
    fn themed_and_original_entries_are_distinct_and_original_name_is_stable() {
        assert_eq!(autostart_value(false),"ZSDesk.Desktop");
        assert_eq!(autostart_value(true),"ZSDesk.Theme.Desktop");
        assert_ne!(autostart_value(false),autostart_value(true));
    }
    #[test]
    fn only_the_exact_executable_and_profile_can_be_changed() {
        assert_eq!(autostart_change(None,OWN,true).unwrap(),AutostartChange::Write);
        assert_eq!(autostart_change(Some(OWN),OWN,true).unwrap(),AutostartChange::Unchanged);
        assert_eq!(autostart_change(Some(OWN),OWN,false).unwrap(),AutostartChange::Delete);
        assert_eq!(autostart_change(None,OWN,false).unwrap(),AutostartChange::Unchanged);
        assert!(autostart_owned(&OWN.to_ascii_lowercase(),OWN));
        for foreign in [OWN.replace("Main","Other"),OWN.replace("ZSDesk.exe","Other.exe"),
            format!("{OWN} --other"),r#""C:\Apps\ZSDesk\ZSDesk.exe" --desktop"#.into()] {
            assert!(!autostart_owned(&foreign,OWN));
            assert!(autostart_change(Some(&foreign),OWN,true).is_err());
            assert!(autostart_change(Some(&foreign),OWN,false).is_err());
        }
        assert!(!autostart_owned("",""));
    }
}
