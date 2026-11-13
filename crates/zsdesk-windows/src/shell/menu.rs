//! Native Shell context menus (`IContextMenu`) hosted by ZSDesk windows.
//!
//! While a menu is tracked, owner-drawn and lazily populated submenus
//! ("新建", "发送到", "打开方式") need `WM_INITMENUPOPUP`, `WM_DRAWITEM`,
//! `WM_MEASUREITEM` and `WM_MENUCHAR` forwarded to `IContextMenu2/3`. Every
//! window procedure that can own a menu calls [`forward_menu_message`] first.

use std::{cell::{Cell, RefCell}, rc::Rc};

use windows::{
    core::{Interface, PCSTR, PCWSTR, PSTR, Result},
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
        System::Com::{CoTaskMemFree, IDataObject},
        UI::{
            Shell::{
                Common::ITEMIDLIST, IContextMenu, IContextMenu2, IContextMenu3, IShellFolder, IShellView, IShellItem,IExplorerBrowser,
                ILClone, ILRemoveLastID, SHBindToParent, SVGIO_BACKGROUND, SVGIO_SELECTION,
                SHCreateShellItemArrayFromIDLists, BHID_DataObject,
                CMINVOKECOMMANDINFO, CMINVOKECOMMANDINFOEX, CMIC_MASK_PTINVOKE,
                CMIC_MASK_SHIFT_DOWN, CMIC_MASK_CONTROL_DOWN, CMF_NORMAL, CMF_CANRENAME, CMF_EXTENDEDVERBS,
                CMF_EXPLORE, CMF_ASYNCVERBSTATE, CMF_OPTIMIZEFORINVOKE, GCS_VERBW, IFolderView2, DefSubclassProc, SetWindowSubclass, RemoveWindowSubclass, GetWindowSubclass,
            },
            WindowsAndMessaging::{
                CreatePopupMenu, DestroyMenu, GetMenuItemCount, GetMenuItemID, GetSubMenu, TrackPopupMenuEx,
                HMENU, SW_SHOWNORMAL, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_DRAWITEM, WM_INITMENUPOPUP,
                WM_MEASUREITEM, WM_MENUCHAR, TPM_LEFTALIGN, TPM_TOPALIGN, TRACK_POPUP_MENU_FLAGS,
                GetMenuStringW, GetMenuState, DeleteMenu, MF_BYPOSITION, MF_BYCOMMAND, MF_DISABLED, MF_GRAYED, MF_SEPARATOR,
                WM_CONTEXTMENU, WM_NCDESTROY, GetCursorPos, GetWindowRect, SetForegroundWindow,
            },
            Input::KeyboardAndMouse::{GetKeyState, VK_SHIFT, VK_CONTROL},
        },
    },
};

/// First and last command identifiers reserved for Shell items.
pub const SHELL_FIRST: u32 = 1;
pub const SHELL_LAST: u32 = 0x6FFF;
/// Application items appended to a Shell menu should use ids from here.
pub const APP_FIRST: u32 = 0x7000;

#[path="quick_menu.rs"]
mod quick_menu;
pub use quick_menu::{QuickAction,QuickMenu};

/// The file-manager front menu can be supplied without constructing any
/// extension IContextMenu on its UI thread. Unset inside extension workers.
pub type ViewMenuDelegate=fn(HWND,&IShellView,LPARAM)->Result<()>;
thread_local!{static VIEW_MENU_DELEGATE:Cell<Option<ViewMenuDelegate>>=const{Cell::new(None)};}
pub fn set_view_menu_delegate(delegate:Option<ViewMenuDelegate>){VIEW_MENU_DELEGATE.with(|current|current.set(delegate));}
pub(crate) fn view_menu_is_delegated()->bool{VIEW_MENU_DELEGATE.with(|current|current.get().is_some())}
pub type MenuDiagnosticDelegate=fn(&str,windows::core::HRESULT);
thread_local!{static DIAGNOSTIC:Cell<Option<MenuDiagnosticDelegate>>=const{Cell::new(None)};}
pub fn set_diagnostic_delegate(delegate:Option<MenuDiagnosticDelegate>){DIAGNOSTIC.with(|current|current.set(delegate));}
fn diagnostic(phase:&str,status:windows::core::HRESULT){
    if let Some(record)=DIAGNOSTIC.with(Cell::get){let _=std::panic::catch_unwind(||record(phase,status));}
}

/// Keeps process-wide menu compatibility state alive while multiple UI STAs
/// share it. Drop only after all those UI threads have stopped.
pub struct SharedMenuRuntime(std::marker::PhantomData<Rc<()>>);
impl SharedMenuRuntime {
    pub fn new()->Self{
        #[cfg(all(target_arch="x86_64",target_env="msvc"))]
        unsafe{zs_menu_shared_host(1)};
        Self(std::marker::PhantomData)
    }
}
impl Drop for SharedMenuRuntime{fn drop(&mut self){
    #[cfg(all(target_arch="x86_64",target_env="msvc"))]
    unsafe{zs_menu_shared_host(0)};
}}
#[cfg(all(target_arch="x86_64",target_env="msvc"))]
extern "C" { fn zs_menu_shared_host(enabled:i32); }

/// `SEE_MASK_UNICODE`, shared by CMINVOKECOMMANDINFOEX.
const CMIC_MASK_UNICODE: u32 = 0x4000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MenuOutcome { Dismissed, Application(u32), Rename, Open, Explore, ShellInvoked, NativeHandled }

#[derive(Clone, Copy, Debug, Default)]
pub struct MenuInterceptions { pub rename: bool, pub open: bool }
impl MenuInterceptions { fn bits(self) -> u32 { u32::from(self.rename) | (u32::from(self.open) << 1) } }

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct NativeMenuResult { kind: u32, command: u32, status: i32, phase: u32 }

fn command_offset(command: u32, first: u32) -> Result<u16> {
    command.checked_sub(first).and_then(|offset| u16::try_from(offset).ok())
        .ok_or_else(|| windows::core::Error::from_hresult(windows::core::HRESULT(0x80070057u32 as i32)))
}
fn application_ids(count: usize) -> Result<Vec<u32>> {
    let count = u32::try_from(count).ok().and_then(|count| APP_FIRST.checked_add(count))
        .ok_or_else(|| windows::core::Error::from_hresult(windows::core::HRESULT(0x80070057u32 as i32)))?;
    Ok((APP_FIRST..count).collect())
}
fn intercepted_verb(verb: Option<&str>, intercept: MenuInterceptions) -> Option<MenuOutcome> {
    match verb {
        Some(verb) if intercept.rename && verb.eq_ignore_ascii_case("rename") => Some(MenuOutcome::Rename),
        Some(verb) if intercept.open && verb.eq_ignore_ascii_case("open") => Some(MenuOutcome::Open),
        Some(verb) if intercept.open && verb.eq_ignore_ascii_case("explore") => Some(MenuOutcome::Explore),
        _ => None,
    }
}
fn empty_recycle_verb(verb:&str)->bool{verb.eq_ignore_ascii_case("empty")||verb.eq_ignore_ascii_case("emptyrecyclebin")}
fn decode_native_result(result: NativeMenuResult, applications: &[u32]) -> Result<Option<MenuOutcome>> {
    let status = windows::core::HRESULT(result.status);
    if matches!(status.0 as u32, 0x800704c7 | 0x80004004) { return Ok(Some(MenuOutcome::Dismissed)); }
    status.ok()?;
    Ok(match result.kind {
        0 => Some(MenuOutcome::Dismissed),
        1 if applications.contains(&result.command) => Some(MenuOutcome::Application(result.command)),
        2 => Some(MenuOutcome::Rename), 3 => Some(MenuOutcome::Open), 4 => Some(MenuOutcome::Explore),
        5 => Some(MenuOutcome::ShellInvoked), 6 => None, 7 => Some(MenuOutcome::NativeHandled),
        _ => return Err(windows::core::Error::from_hresult(windows::core::HRESULT(0x8000ffffu32 as i32))),
    })
}
/// Mixed fence menus carry application IDs that only the public
/// TrackPopupMenu contract preserves; Shift-extended menus are classic, as in
/// Explorer.
fn uses_modern_presenter(modern: bool, extended: bool, application_count: usize, background: bool, has_folder: bool) -> bool {
    modern && !extended && application_count == 0 && (!background || has_folder)
}
fn can_fall_back_before_presentation(status: windows::core::HRESULT, result: NativeMenuResult) -> bool {
    status.is_err() && result.phase == 0
}
struct BeforeShell<'a> { callback: &'a mut dyn FnMut() -> Result<()> }
unsafe extern "system" fn before_shell(context: *mut core::ffi::c_void) -> windows::core::HRESULT {
    if context.is_null() { return windows::core::HRESULT(0x80004003u32 as i32); }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let state = unsafe { &mut *context.cast::<BeforeShell<'_>>() };
        (state.callback)()
    }));
    match result { Ok(Ok(())) => windows::core::HRESULT(0), Ok(Err(error)) => error.code(), Err(_) => windows::core::HRESULT(0x8000ffffu32 as i32) }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryAction { Undo, Redo }

impl HistoryAction {
    fn verb(self) -> &'static str { match self { Self::Undo => "undo", Self::Redo => "redo" } }
    fn unavailable(self) -> &'static str {
        match self { Self::Undo => "当前没有可撤销的文件操作", Self::Redo => "当前没有可重做的文件操作" }
    }
}

#[derive(Clone, Copy)]
struct HistoryCommand { id: u32, enabled: bool }

fn listed_history_command(menu: &ShellMenu, handle: HMENU, verb: &str, depth: usize) -> Option<HistoryCommand> {
    if depth > 8 { return None; }
    for index in 0..unsafe { GetMenuItemCount(Some(handle)) }.max(0) {
        let id = unsafe { GetMenuItemID(handle, index) };
        if ShellMenu::is_shell_id(id) && menu.verb(id).is_some_and(|name| name.eq_ignore_ascii_case(verb)) {
            let state = unsafe { GetMenuState(handle, id, MF_BYCOMMAND) };
            return Some(HistoryCommand { id, enabled: state != u32::MAX && state & (MF_DISABLED.0 | MF_GRAYED.0) == 0 });
        }
        let child = unsafe { GetSubMenu(handle, index) };
        if !child.is_invalid() {
            if let Some(command) = listed_history_command(menu, child, verb, depth + 1) { return Some(command); }
        }
    }
    None
}

fn invoke_history_command(action: HistoryAction, command: Option<HistoryCommand>, invoke: impl FnOnce(u32) -> Result<()>) -> Result<()> {
    let command = command.filter(|command| command.enabled).ok_or_else(|| windows::core::Error::new(
        windows::core::HRESULT(0x80040101u32 as i32), action.unavailable()))?;
    invoke(command.id)
}

#[derive(Clone)]
struct ActiveContext { menu: IContextMenu, filter: bool, root: isize }
thread_local! {
    static ACTIVE: RefCell<Option<ActiveContext>> = const { RefCell::new(None) };
    static OPTIONS: RefCell<ContextMenuOptions> = RefCell::new(ContextMenuOptions::default());
    static PUBLIC_BIND_MICROS: Cell<u64> = const { Cell::new(0) };
    static PUBLIC_POPULATE_MICROS: Cell<u64> = const { Cell::new(0) };
}

struct MenuPhaseTimer{started:std::time::Instant,populate:bool}
impl MenuPhaseTimer{fn bind()->Self{Self{started:std::time::Instant::now(),populate:false}}fn populate()->Self{Self{started:std::time::Instant::now(),populate:true}}}
impl Drop for MenuPhaseTimer{fn drop(&mut self){let elapsed=self.started.elapsed().as_micros().min(u64::MAX as u128)as u64;if self.populate{PUBLIC_POPULATE_MICROS.with(|value|value.set(elapsed));}else{PUBLIC_BIND_MICROS.with(|value|value.set(elapsed));}}}
pub(crate) fn public_menu_timings()->(u64,u64){(PUBLIC_BIND_MICROS.with(Cell::get),PUBLIC_POPULATE_MICROS.with(Cell::get))}

/// Preferences affect only this application's menus. Both native presentations
/// retain Shell command identities while hiding the selected functions.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContextMenuOptions {
    pub modern: bool,
    pub hidden_verbs: Vec<String>,
    pub show_seven_zip: bool,
    pub show_wps: bool,
}
impl Default for ContextMenuOptions {
    fn default() -> Self { Self { modern: true, hidden_verbs: Vec::new(), show_seven_zip: true, show_wps: false } }
}
/// SHA-256 of the only `Windows.UI.FileExplorer.dll` build whose private
/// presenter ABI `native/shell_menu.cpp` supports (`kExpectedMenuSha256`).
const SUPPORTED_PRESENTER_SHA256: &str = "4ce847cbbe84bedab2799bc7de9aa1d85684e66b95e2c59daf4ecee759aef0b1";
/// `kPresenterClass` registration; its in-process server is the module above.
const PRESENTER_SERVER_KEY: &str = "CLSID\\{86CA1AA0-34AA-4E8B-A509-50C905BAE2A2}\\InprocServer32";

/// Whether the Windows 11 presenter can host this application's menus.
///
/// `Some(false)` means it certainly cannot: Windows 10 has no such presenter,
/// and any other build of the module fails the native digest check after the
/// worker has already created a hidden Shell view, loaded the XAML runtime and
/// hashed the module, on every cold menu process. Callers use the classic
/// presentation directly in that case. `None` (registration unreadable) keeps
/// the native verification as the authority.
pub fn modern_presenter_supported() -> Option<bool> {
    static SUPPORTED: std::sync::OnceLock<Option<bool>> = std::sync::OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        if !cfg!(all(target_arch = "x86_64", target_env = "msvc")) { return Some(false); }
        let build = crate::theme::os_build();
        if build != 0 && build < 22000 { return Some(false); }
        let path = presenter_server_path()?;
        let digest = file_sha256(&path)?;
        Some(digest.eq_ignore_ascii_case(SUPPORTED_PRESENTER_SHA256))
    })
}

fn presenter_server_path() -> Option<std::path::PathBuf> {
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CLASSES_ROOT, RRF_RT_REG_SZ};
    let key: Vec<u16> = PRESENTER_SERVER_KEY.encode_utf16().chain(Some(0)).collect();
    let mut buffer = vec![0u16; 1024];
    let mut size = (buffer.len() * 2) as u32;
    // Without RRF_NOEXPAND a REG_EXPAND_SZ (%SystemRoot%\...) is expanded and
    // then satisfies the REG_SZ restriction.
    unsafe { RegGetValueW(HKEY_CLASSES_ROOT, PCWSTR(key.as_ptr()), PCWSTR::null(), RRF_RT_REG_SZ,
        None, Some(buffer.as_mut_ptr().cast()), Some(&mut size)) }.ok().ok()?;
    let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    let path = std::path::PathBuf::from(String::from_utf16_lossy(&buffer[..end]).trim().trim_matches('"'));
    path.is_absolute().then_some(path)
}

fn file_sha256(path: &std::path::Path) -> Option<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).ok()?;
        if read == 0 { break; }
        hasher.update(&buffer[..read]);
    }
    Some(hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect())
}

pub fn set_context_menu_options(mut options: ContextMenuOptions) {
    // An unsupported presenter only adds failed setup work before the same
    // classic menu would be shown; request the classic menu directly.
    options.modern = options.modern && modern_presenter_supported() != Some(false);
    options.hidden_verbs.retain(|v| !v.is_empty() && v.len() <= 128 && !v.contains('\0'));
    for verb in &mut options.hidden_verbs { verb.make_ascii_lowercase(); }
    options.hidden_verbs.sort(); options.hidden_verbs.dedup();
    #[cfg(all(target_arch = "x86_64", target_env = "msvc"))]
    unsafe {
        zs_menu_set_hidden_features(modern_hidden_features(&options));
        zs_menu_set_original_callback(Some(show_expanded_view_menu));
    }
    OPTIONS.with(|current| *current.borrow_mut() = options);
}
pub fn context_menu_options() -> ContextMenuOptions { OPTIONS.with(|options| options.borrow().clone()) }

/// Posted to the containing application window when a filtered menu cannot be
/// opened. WPARAM contains the HRESULT; no unfiltered fallback is displayed.
pub fn failure_message() -> u32 {
    static MESSAGE: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *MESSAGE.get_or_init(|| unsafe {
        windows::Win32::UI::WindowsAndMessaging::RegisterWindowMessageW(windows::core::w!("ZSDesk.ContextMenuFailure"))
    })
}

#[cfg(all(target_arch = "x86_64", target_env = "msvc"))]
extern "C" {
    fn zs_menu_install_fault_report(path:PCWSTR)->windows::core::HRESULT;
    fn zs_menu_dismiss_active(owner:HWND)->windows::core::HRESULT;
    fn zs_menu_for_mixed_items(owner: HWND, count: u32, pidls: *const *const ITEMIDLIST, context: *mut *mut core::ffi::c_void) -> windows::core::HRESULT;
    fn zs_menu_set_hidden_features(hidden: u32);
    fn zs_menu_set_original_callback(callback: Option<unsafe extern "system" fn(*mut core::ffi::c_void, POINT) -> windows::core::HRESULT>);
    fn zs_menu_track_standalone(owner: HWND, context: *mut core::ffi::c_void, menu: HMENU,
        count: u32, pidls: *const *const ITEMIDLIST, folder: *mut core::ffi::c_void, background: i32, source_browser:*mut core::ffi::c_void,
        point: *mut POINT, applications: *const u32, application_count: u32, intercept: u32,
        before: Option<unsafe extern "system" fn(*mut core::ffi::c_void) -> windows::core::HRESULT>, before_context: *mut core::ffi::c_void,
        outcome: *mut NativeMenuResult) -> windows::core::HRESULT;
}

/// Last-chance native fault evidence for an isolated extension worker only.
/// The handler writes code/module/offset and keeps normal crash propagation.
pub fn install_fault_report(path:&std::path::Path)->Result<()>{
    if !path.is_absolute(){return Err(windows::core::Error::from_hresult(windows::core::HRESULT(0x80070057u32 as i32)));}
    #[cfg(all(target_arch="x86_64",target_env="msvc"))]{
        use std::os::windows::ffi::OsStrExt;
        let text:Vec<u16>=path.as_os_str().encode_wide().chain(Some(0)).collect();
        return unsafe{zs_menu_install_fault_report(PCWSTR(text.as_ptr()))}.ok();
    }
    #[cfg(not(all(target_arch="x86_64",target_env="msvc")))]
    Err(windows::core::Error::from_hresult(windows::core::HRESULT(0x80004001u32 as i32)))
}

/// Cancels only the current thread's native presenter for this exact owner.
/// Callers must not cancel a command after granting its execution barrier.
pub fn dismiss_active_menu(owner:HWND)->Result<()>{
    #[cfg(all(target_arch="x86_64",target_env="msvc"))]
    {return unsafe{zs_menu_dismiss_active(owner)}.ok();}
    #[cfg(not(all(target_arch="x86_64",target_env="msvc")))]
    Ok(())
}

pub fn report_failure(owner: HWND, error: windows::core::HRESULT) {
    diagnostic("native-menu-failure",error);
    let message = failure_message();
    if message != 0 {
        let root = unsafe { windows::Win32::UI::WindowsAndMessaging::GetAncestor(owner, windows::Win32::UI::WindowsAndMessaging::GA_ROOT) };
        let _ = unsafe { windows::Win32::UI::WindowsAndMessaging::PostMessageW(Some(root), message, WPARAM(error.0 as u32 as usize), LPARAM(0)) };
    }
}

// Shared bit contract with native/MenuModelFilter.h. Unknown canonical verbs
// remain usable by the original menu without guessing a private XAML identity.
fn modern_hidden_features(options: &ContextMenuOptions) -> u32 {
    let mut hidden = 0;
    for verb in &options.hidden_verbs { hidden |= command_feature(verb); }
    if !options.show_seven_zip { hidden |= 1 << 10; }
    if !options.show_wps { hidden |= 1 << 11; }
    hidden
}
fn command_feature(verb: &str) -> u32 {
    match verb {
        "cut" | "windows.cut" => 1 << 0, "copy" | "windows.copy" => 1 << 1,
        "paste" | "windows.paste" => 1 << 2, "pastelink" | "windows.pastelink" => 1 << 3,
        "copyaspath" | "copypath" | "windows.copyaspath" => 1 << 4,
        "rename" | "windows.rename" => 1 << 5, "delete" | "windows.delete" => 1 << 6,
        "properties" | "windows.properties" => 1 << 7,
        "share" | "windows.share" | "windows.modernshare" | "windows.modernshareflyout" => 1 << 8,
        "new" | "newfolder" | "windows.new" | "windows.newfolder" => 1 << 9,
        _ => 0,
    }
}

struct ActiveMenuScope(Option<ActiveContext>);
impl ActiveMenuScope {
    fn enter(menu: &IContextMenu, filter: bool, root: HMENU) -> Self {
        Self(ACTIVE.with(|active| active.replace(Some(ActiveContext { menu: menu.clone(), filter, root: root.0 as isize }))))
    }
}
impl Drop for ActiveMenuScope {
    fn drop(&mut self) { ACTIVE.with(|active| *active.borrow_mut() = self.0.take()); }
}

/// Forwards menu messages to the context menu being tracked on this thread.
pub fn forward_menu_message(msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
    if ![WM_INITMENUPOPUP, WM_DRAWITEM, WM_MEASUREITEM, WM_MENUCHAR].contains(&msg) { return None; }
    let active = ACTIVE.with(|active| active.borrow().clone())?;
    let menu = active.menu;
    let mut handled = None;
    if let Ok(menu3) = menu.cast::<IContextMenu3>() {
        let mut result = LRESULT(0);
        if unsafe { menu3.HandleMenuMsg2(msg, wp, lp, Some(&mut result)) }.is_ok() { handled = Some(result); }
    } else if let Ok(menu2) = menu.cast::<IContextMenu2>() {
        if unsafe { menu2.HandleMenuMsg(msg, wp, lp) }.is_ok() { handled = Some(LRESULT(0)); }
    }
    // Only the top level carries the commands these preferences name. Lazily
    // filled submenus ("打开方式", "发送到", "新建", extension cascades) are
    // left exactly as their handler built them: filtering them removed "WPS
    // Office" from "打开方式" and called every handler's GetCommandString on
    // each submenu opening.
    if msg == WM_INITMENUPOPUP && active.filter && wp.0 as isize == active.root {
        filter_native_menu(HMENU(wp.0 as *mut _), &menu, &context_menu_options());
    }
    handled
}

pub struct ShellMenu {
    menu: IContextMenu,
    hmenu: HMENU,
    recycle_bin:bool,
    /// Populated for Shift+right-click. Explorer answers that gesture with the
    /// classic menu directly; the Windows 11 presenter is never involved.
    extended: Cell<bool>,
}

impl ShellMenu {
    pub fn new(menu: IContextMenu) -> Result<Self> {
        let hmenu = unsafe { CreatePopupMenu()? };
        Ok(Self { menu, hmenu, recycle_bin:false, extended: Cell::new(false) })
    }

    /// A snapshot of absolute identities, including mixed-folder selections.
    pub fn for_items(owner: HWND, pidls: &[Vec<u8>]) -> Result<Self> {
        let _timer=MenuPhaseTimer::bind();
        let offsets = item_offsets(pidls)?;
        let mut menu=if same_item_parent(pidls, &offsets) { Self::new(items_ui_object(owner, pidls)?)? }
        else { Self::new(mixed_item_context(owner, pidls)?)? };
        menu.recycle_bin=pidls.len()==1&&super::pidl::item_from_pidl(&pidls[0]).is_ok_and(|item|super::shell_item::is_recycle_bin_item(&item));
        Ok(menu)
    }

    /// Background menu of a folder ("新建", "粘贴", "属性" …).
    pub fn for_folder_background(owner: HWND, folder: &IShellFolder) -> Result<Self> {
        let _timer=MenuPhaseTimer::bind();
        let menu: IContextMenu = unsafe { folder.CreateViewObject(owner)? };
        Self::new(menu)
    }

    pub fn for_view(view: &IShellView, selection: bool) -> Result<Self> {
        let _timer=MenuPhaseTimer::bind();
        let menu: IContextMenu = unsafe { view.GetItemObject(if selection { SVGIO_SELECTION } else { SVGIO_BACKGROUND })? };
        let mut menu=Self::new(menu)?;
        if !selection{
            use windows::Win32::UI::Shell::{IFolderView2,IPersistFolder2};
            if let Ok(folder)=view.cast::<IFolderView2>().and_then(|view|unsafe{view.GetFolder::<IPersistFolder2>()}){
                if let Ok(pidl)=unsafe{folder.GetCurFolder()}{
                    let bytes=super::pidl::pidl_bytes(pidl);unsafe{windows::Win32::System::Com::CoTaskMemFree(Some(pidl.cast()))};
                    menu.recycle_bin=super::pidl::item_from_pidl(&bytes).is_ok_and(|item|super::shell_item::is_recycle_bin_item(&item));
                }
            }
        }
        Ok(menu)
    }

    pub fn hmenu(&self) -> HMENU { self.hmenu }
    pub(super) fn context(&self)->&IContextMenu{&self.menu}

    /// ShellNew owns its dynamic document templates and canonical command IDs.
    pub fn track_new(&self,owner:HWND,point:POINT,before:&mut dyn FnMut()->Result<()>)->Result<MenuOutcome>{
        let sub=self.submenu_with_verb("NewFolder").or_else(||{
            (0..unsafe{GetMenuItemCount(Some(self.hmenu))}.max(0)).find_map(|index|{
                let sub=unsafe{GetSubMenu(self.hmenu,index)};
                if sub.is_invalid(){None}else{self.init_popup(sub,index);Some(sub)}
            })
        }).ok_or_else(||windows::core::Error::new(windows::core::HRESULT(0x80004005u32 as i32),"当前文件夹没有可用的新建命令"))?;
        let Some(command)=self.track_unfiltered(owner,sub,point)else{return Ok(MenuOutcome::Dismissed);};
        before()?;self.invoke(owner,command,Some(point),false,false)?;Ok(MenuOutcome::ShellInvoked)
    }

    /// Fills the popup menu. `extended` adds Shift-only verbs.
    pub fn populate(&self, rename: bool, extended: bool) -> Result<()> {
        let _timer=MenuPhaseTimer::populate();
        // Permit handlers that support it to obtain expensive state off the
        // menu's UI path. Windows retains normal state evaluation otherwise.
        let mut flags = CMF_NORMAL | CMF_EXPLORE | CMF_ASYNCVERBSTATE;
        if rename { flags |= CMF_CANRENAME; }
        if extended { flags |= CMF_EXTENDEDVERBS; }
        self.extended.set(extended);
        unsafe { self.menu.QueryContextMenu(self.hmenu, 0, SHELL_FIRST, SHELL_LAST, flags) }.ok()
    }

    /// Direct canonical commands need no third-party visual menu construction.
    /// Shell handlers receive the documented optimize-for-invoke hint.
    pub fn populate_for_invoke(&self, extended: bool) -> Result<()> {
        let flags = CMF_NORMAL | CMF_EXPLORE | CMF_OPTIMIZEFORINVOKE
            | if extended { CMF_EXTENDEDVERBS } else { CMF_NORMAL };
        unsafe { self.menu.QueryContextMenu(self.hmenu, 0, SHELL_FIRST, SHELL_LAST, flags) }.ok()
    }

    /// Resolves history against a complete, synchronously populated menu.
    /// The caller must provide a new menu and hold no application-state borrow.
    pub fn invoke_history(&self, owner: HWND, action: HistoryAction) -> Result<()> {
        unsafe { self.menu.QueryContextMenu(self.hmenu, 0, SHELL_FIRST, SHELL_LAST, CMF_NORMAL | CMF_EXPLORE) }.ok()?;
        let command = listed_history_command(self, self.hmenu, action.verb(), 0);
        invoke_history_command(action, command, |id| self.invoke(owner, id, None, false, false))
    }

    /// Shows `menu` (this menu or one of its submenus) and returns the chosen id.
    pub fn track(&self, owner: HWND, menu: HMENU, point: POINT) -> Option<u32> {
        self.track_with_policy(owner, menu, point, true)
    }

    /// Shell commands execute against the interface supplied by the presenter.
    /// Only explicitly intercepted application actions return to the fence.
    pub fn track_selected(&self, owner: HWND, point: POINT, pidls: &[Vec<u8>], application_count: usize,
            intercept: MenuInterceptions) -> Result<MenuOutcome> {
        self.track_standalone(owner, point, pidls, None, false, None, application_count, intercept, &mut || Ok(()))
    }

    /// Revalidate an asynchronous selection before a Shell operation executes.
    pub fn track_selected_before(&self, owner: HWND, point: POINT, pidls: &[Vec<u8>], application_count: usize,
            intercept: MenuInterceptions, before:&mut dyn FnMut()->Result<()>) -> Result<MenuOutcome> {
        self.track_standalone(owner,point,pidls,None,false,None,application_count,intercept,before)
    }

    pub fn track_folder_background(&self, owner: HWND, point: POINT, path: &std::path::Path, application_count: usize,
            before: &mut dyn FnMut() -> Result<()>) -> Result<MenuOutcome> {
        let folder = if application_count==0 && context_menu_options().modern {
            unsafe { super::shell_item(&zsdesk_core::model::explorer::ShellLocation::Path(path.to_path_buf())) }.ok()
        } else { None };
        self.track_standalone(owner, point, &[], folder.as_ref(), true, None, application_count, MenuInterceptions::default(), before)
    }

    pub fn track_background_item(&self,owner:HWND,point:POINT,folder:&IShellItem,before:&mut dyn FnMut()->Result<()>)->Result<MenuOutcome>{
        self.track_standalone(owner,point,&[],Some(folder),true,None,0,MenuInterceptions::default(),before)
    }

    pub fn track_background_view(&self,owner:HWND,point:POINT,folder:&IShellItem,browser:&IExplorerBrowser,before:&mut dyn FnMut()->Result<()>)->Result<MenuOutcome>{
        self.track_standalone(owner,point,&[],Some(folder),true,Some(browser),0,MenuInterceptions::default(),before)
    }

    fn track_standalone(&self, owner: HWND, mut point: POINT, pidls: &[Vec<u8>], folder: Option<&IShellItem>, background: bool, browser:Option<&IExplorerBrowser>,
            application_count: usize, intercept: MenuInterceptions, before: &mut dyn FnMut() -> Result<()>) -> Result<MenuOutcome> {
        let applications = application_ids(application_count)?;
        #[cfg(all(target_arch = "x86_64", target_env = "msvc"))]
        // A mixed fence menu owns application IDs as well as one populated
        // IContextMenu. Keep those IDs and that exact interface together through
        // TrackPopupMenu/InvokeCommand; the private Win11 presenter can rebuild
        // the command model and cannot preserve this public dispatch contract.
        if uses_modern_presenter(context_menu_options().modern, self.extended.get(), application_count, background, folder.is_some()) {
            if let Ok(count) = u32::try_from(pidls.len()) {
                self.filter(self.hmenu, &context_menu_options());
                let identifiers: Vec<*const ITEMIDLIST> = pidls.iter().map(|pidl| pidl.as_ptr().cast()).collect();
                let mut outcome = NativeMenuResult::default();
                let mut observer = BeforeShell { callback: &mut *before };
                let active = ActiveMenuScope::enter(&self.menu, true, self.hmenu);
                let result = unsafe { zs_menu_track_standalone(owner, self.menu.as_raw(), self.hmenu,
                    count, identifiers.as_ptr(), folder.map_or(std::ptr::null_mut(), Interface::as_raw),
                    i32::from(background), browser.map_or(std::ptr::null_mut(),Interface::as_raw), &mut point, applications.as_ptr(), applications.len() as u32, intercept.bits(),
                    Some(before_shell), (&mut observer as *mut BeforeShell<'_>).cast(), &mut outcome) };
                drop(active);
                if result.is_ok() {
                    if let Some(action) = decode_native_result(outcome, &applications)? { return Ok(action); }
                    // "Show more options": the closing XAML popup took the
                    // foreground, and TrackPopupMenuEx needs its owner in front
                    // for the classic menu to open and to dismiss normally.
                    unsafe { let _ = SetForegroundWindow(owner); }
                } else if !can_fall_back_before_presentation(result, outcome) {
                    // After presentation or invocation has started, retrying a
                    // second menu could execute a user's file operation twice.
                    return Err(windows::core::Error::from_hresult(result));
                }
            }
        }
        let Some(command) = self.track(owner, self.hmenu, point) else { return Ok(MenuOutcome::Dismissed) };
        if applications.contains(&command) { return Ok(MenuOutcome::Application(command)); }
        if !Self::is_shell_id(command) { return Err(windows::core::Error::from_hresult(windows::core::HRESULT(0x80070057u32 as i32))); }
        if let Some(action) = intercepted_verb(self.verb(command).as_deref(), intercept) { return Ok(action); }
        before()?;
        self.invoke(owner, command, Some(point), unsafe { GetKeyState(VK_SHIFT.0 as i32) } < 0,
            unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0)?;
        Ok(MenuOutcome::ShellInvoked)
    }

    /// Explicit command-bar menus retain their available commands independently
    /// of which commands are shown by a file's context menu.
    pub fn track_unfiltered(&self, owner: HWND, menu: HMENU, point: POINT) -> Option<u32> {
        self.track_with_policy(owner, menu, point, false)
    }

    fn track_with_policy(&self, owner: HWND, menu: HMENU, point: POINT, context: bool) -> Option<u32> {
        let options = context_menu_options();
        // This path hosts the original presentation, including an explicit
        // expansion from a Windows 11 menu or its compatible fallback.
        let filter = context;
        if filter { self.filter(menu, &options); }
        let _active = ActiveMenuScope::enter(&self.menu, filter, menu);
        let flags = TRACK_POPUP_MENU_FLAGS(TPM_RETURNCMD.0 | TPM_RIGHTBUTTON.0 | TPM_LEFTALIGN.0 | TPM_TOPALIGN.0);
        let chosen = unsafe { TrackPopupMenuEx(menu, flags.0, point.x, point.y, owner, None) }.0 as u32;
        // Complete the foreground popup handoff before its owner is reused or
        // destroyed. Without this documented benign message, a subsequent
        // popup can immediately dismiss after switching between menu owners.
        let _=unsafe{windows::Win32::UI::WindowsAndMessaging::PostMessageW(Some(owner),
            windows::Win32::UI::WindowsAndMessaging::WM_NULL,WPARAM(0),LPARAM(0))};
        (chosen != 0).then_some(chosen)
    }

    /// Removes entries by canonical verb, retaining native IDs and handlers.
    /// Text matching is used only for named extension groups such as 7-Zip.
    pub fn filter(&self, menu: HMENU, options: &ContextMenuOptions) {
        filter_native_menu(menu, &self.menu, options);
    }

    pub fn is_shell_id(id: u32) -> bool { (SHELL_FIRST..=SHELL_LAST).contains(&id) }

    /// Canonical verb ("open", "rename", "delete", "NewFolder" …) for a Shell id.
    pub fn verb(&self, id: u32) -> Option<String> {
        canonical_verb(&self.menu, id)
    }

    /// Finds the submenu that contains `verb` (used to extract "新建").
    pub fn submenu_with_verb(&self, verb: &str) -> Option<HMENU> {
        let count = unsafe { GetMenuItemCount(Some(self.hmenu)) };
        for index in 0..count.max(0) {
            let sub = unsafe { GetSubMenu(self.hmenu, index) };
            if sub.is_invalid() { continue; }
            // "新建" and similar handlers fill their submenu on WM_INITMENUPOPUP.
            self.init_popup(sub, index);
            let inner = unsafe { GetMenuItemCount(Some(sub)) };
            for i in 0..inner.max(0) {
                let id = unsafe { GetMenuItemID(sub, i) };
                if self.verb(id).is_some_and(|v| v.eq_ignore_ascii_case(verb)) { return Some(sub); }
            }
        }
        None
    }

    fn init_popup(&self, sub: HMENU, index: i32) {
        let (wp, lp) = (WPARAM(sub.0 as usize), LPARAM(index as isize));
        if let Ok(menu3) = self.menu.cast::<IContextMenu3>() {
            let mut result = LRESULT(0);
            let _ = unsafe { menu3.HandleMenuMsg2(WM_INITMENUPOPUP, wp, lp, Some(&mut result)) };
        } else if let Ok(menu2) = self.menu.cast::<IContextMenu2>() {
            let _ = unsafe { menu2.HandleMenuMsg(WM_INITMENUPOPUP, wp, lp) };
        }
    }

    /// Runs a Shell command chosen from the menu.
    pub fn invoke(&self, owner: HWND, id: u32, point: Option<POINT>, shift: bool, control: bool) -> Result<()> {
        if !Self::is_shell_id(id) { return Err(windows::core::Error::from_hresult(windows::core::HRESULT(0x80070057u32 as i32))); }
        if self.recycle_bin&&self.verb(id).is_some_and(|verb|empty_recycle_verb(&verb)){
            // The documented API keeps the native confirmation and progress UI
            // without depending on an Explorer-only context-menu service site.
            return unsafe{windows::Win32::UI::Shell::SHEmptyRecycleBinW(Some(owner),PCWSTR::null(),0)};
        }
        let offset = usize::from(command_offset(id, SHELL_FIRST)?);
        self.invoke_raw(owner, PCSTR(offset as *const u8), PCWSTR(offset as *const u16), point, shift, control)
    }

    /// Runs a canonical verb without showing the menu.
    pub fn invoke_verb(&self, owner: HWND, verb: &str, shift: bool) -> Result<()> {
        if self.recycle_bin&&empty_recycle_verb(verb){return unsafe{windows::Win32::UI::Shell::SHEmptyRecycleBinW(Some(owner),PCWSTR::null(),0)};}
        let ansi: Vec<u8> = verb.bytes().chain(Some(0)).collect();
        let wide: Vec<u16> = verb.encode_utf16().chain(Some(0)).collect();
        self.invoke_raw(owner, PCSTR(ansi.as_ptr()), PCWSTR(wide.as_ptr()), None, shift, false)
    }

    fn invoke_raw(&self, owner: HWND, verb: PCSTR, verb_w: PCWSTR, point: Option<POINT>, shift: bool, control: bool) -> Result<()> {
        let mut mask = CMIC_MASK_UNICODE;
        if point.is_some() { mask |= CMIC_MASK_PTINVOKE; }
        if shift { mask |= CMIC_MASK_SHIFT_DOWN; }
        if control { mask |= CMIC_MASK_CONTROL_DOWN; }
        let info = CMINVOKECOMMANDINFOEX {
            cbSize: std::mem::size_of::<CMINVOKECOMMANDINFOEX>() as u32,
            fMask: mask, hwnd: owner, lpVerb: verb, nShow: SW_SHOWNORMAL.0, lpVerbW: verb_w,
            ptInvoke: point.unwrap_or_default(), ..Default::default()
        };
        unsafe { self.menu.InvokeCommand((&info as *const CMINVOKECOMMANDINFOEX).cast::<CMINVOKECOMMANDINFO>()) }
    }
}

fn should_hide(verb: Option<&str>, label: &str, options: &ContextMenuOptions) -> bool {
    let canonical = verb.unwrap_or_default().to_ascii_lowercase();
    if options.hidden_verbs.iter().any(|v| v == &canonical) { return true; }
    if command_feature(&canonical) & modern_hidden_features(options) != 0 { return true; }
    let label = label.replace('&', "").to_lowercase();
    let group = label.split('(').next().unwrap_or(&label).trim();
    if options.hidden_verbs.iter().any(|v| v == "new") && matches!(group, "新建" | "新增" | "new") { return true; }
    (!options.show_seven_zip && (label.contains("7-zip") || label.contains("7zip") || canonical.starts_with("7-zip")))
        || (!options.show_wps && (label.contains("wps") || canonical.starts_with("wps")))
}

fn canonical_verb(context: &IContextMenu, id: u32) -> Option<String> {
    if !ShellMenu::is_shell_id(id) { return None; }
    let mut buffer = [0u16; 128];
    unsafe { context.GetCommandString((id - SHELL_FIRST) as usize, GCS_VERBW, None, PSTR(buffer.as_mut_ptr().cast()), buffer.len() as u32) }.ok()?;
    let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    Some(String::from_utf16_lossy(&buffer[..end]))
}

/// Hides the preference-controlled entries of a context menu's top level.
/// A hidden cascade ("7-Zip", "新建") disappears with all of its children;
/// submenus that stay visible are never edited.
fn filter_native_menu(menu: HMENU, context: &IContextMenu, options: &ContextMenuOptions) {
    let count = unsafe { GetMenuItemCount(Some(menu)) }.max(0);
    for index in (0..count).rev() {
        let id = unsafe { GetMenuItemID(menu, index) };
        if id >= APP_FIRST && id != u32::MAX { continue; }
        let mut text = [0u16; 1024];
        let length = unsafe { GetMenuStringW(menu, index as u32, Some(&mut text), MF_BYPOSITION) }.max(0) as usize;
        let label = String::from_utf16_lossy(&text[..length]);
        // With only named extension filtering enabled there is no need to call
        // every third-party handler's GetCommandString on each popup.
        let verb = if options.hidden_verbs.is_empty() { None } else { canonical_verb(context, id) };
        if should_hide(verb.as_deref(), &label, options) {
            let _ = unsafe { DeleteMenu(menu, index as u32, MF_BYPOSITION) };
        }
    }
    trim_separators(menu);
}

fn trim_separators(menu: HMENU) {
    let mut previous_separator = true;
    let mut index = 0;
    loop {
        let count = unsafe { GetMenuItemCount(Some(menu)) }.max(0);
        if index >= count { break; }
        let flags = unsafe { GetMenuState(menu, index as u32, MF_BYPOSITION) };
        let separator = flags & MF_SEPARATOR.0 != 0;
        if separator && (previous_separator || index + 1 == count) {
            let _ = unsafe { DeleteMenu(menu, index as u32, MF_BYPOSITION) };
        } else { previous_separator = separator; index += 1; }
    }
}

const VIEW_MENU_SUBCLASS: usize = 0x5A53_4D50;
struct ViewMenuState { view: IShellView, modern: Rc<Cell<bool>> }

/// Installed delegates own both menu styles. Extension activation belongs to
/// the isolated menu worker, never to the main file view's UI process.
pub(crate) unsafe fn attach_view_menu(view: &IShellView, modern: Rc<Cell<bool>>) {
    let Ok(window) = (unsafe { view.GetWindow() }) else { return };
    let mut existing = 0usize;
    if unsafe { GetWindowSubclass(window, Some(view_menu_proc), VIEW_MENU_SUBCLASS, Some(&mut existing)) }.as_bool() {
        if existing != 0 { unsafe { (*(existing as *mut ViewMenuState)).modern = modern; } }
        return;
    }
    let state = Box::into_raw(Box::new(ViewMenuState { view: view.clone(), modern }));
    if !unsafe { SetWindowSubclass(window, Some(view_menu_proc), VIEW_MENU_SUBCLASS, state as usize) }.as_bool() {
        unsafe { drop(Box::from_raw(state)); }
    }
}

unsafe extern "system" fn view_menu_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, _: usize, data: usize) -> LRESULT {
    if msg == WM_NCDESTROY {
        unsafe { let _ = RemoveWindowSubclass(hwnd, Some(view_menu_proc), VIEW_MENU_SUBCLASS); }
        let state = unsafe { Box::from_raw(data as *mut ViewMenuState) };
        let result = unsafe { DefSubclassProc(hwnd, msg, wp, lp) }; drop(state); return result;
    }
    if let Some(result) = forward_menu_message(msg, wp, lp) { return result; }
    let state = unsafe { &*(data as *const ViewMenuState) };
    if msg==WM_CONTEXTMENU{
        if let Some(delegate)=VIEW_MENU_DELEGATE.with(Cell::get){
            let view=state.view.clone();
            diagnostic("main-context-delegate",windows::core::HRESULT(0));
            let result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||delegate(hwnd,&view,lp)))
                .unwrap_or_else(|_|Err(windows::core::Error::from_hresult(windows::core::HRESULT(0x8000ffffu32 as i32))));
            if let Err(error)=result{report_failure(hwnd,error.code());}
            return LRESULT(0);
        }
    }
    if msg == WM_CONTEXTMENU && !state.modern.get() {
        let view = state.view.clone();
        // Both mouse and keyboard requests are owned by this filtered menu.
        // Retrying through the Shell after a handler fails would build another
        // menu synchronously and could reveal commands disabled by the user.
        if let Err(error) = show_original_view_menu(hwnd, &view, lp) { report_failure(hwnd, error.code()); }
        return LRESULT(0);
    }
    unsafe { DefSubclassProc(hwnd, msg, wp, lp) }
}

fn show_original_view_menu(owner: HWND, view: &IShellView, position: LPARAM) -> Result<()> {
    let point = POINT { x: (position.0 as u32 & 0xffff) as i16 as i32, y: ((position.0 as u32 >> 16) & 0xffff) as i16 as i32 };
    show_original_view_menu_at(owner, view, point)
}

#[cfg(all(target_arch = "x86_64", target_env = "msvc"))]
unsafe extern "system" fn show_expanded_view_menu(raw_view: *mut core::ffi::c_void, point: POINT) -> windows::core::HRESULT {
    let Some(view) = (unsafe { IShellView::from_raw_borrowed(&raw_view) }) else {
        return windows::core::HRESULT(0x8000_4003u32 as i32);
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(||unsafe { view.GetWindow() }
        .and_then(|owner| {
            if let Some(delegate)=VIEW_MENU_DELEGATE.with(Cell::get){
                let packed=LPARAM(((point.x as u16 as u32)|((point.y as u16 as u32)<<16))as isize);
                delegate(owner,view,packed)
            }else{show_original_view_menu_at(owner, view, point)}
        }))).unwrap_or_else(|_|Err(windows::core::Error::from_hresult(windows::core::HRESULT(0x8000ffffu32 as i32))));
    match result { Ok(()) => windows::core::HRESULT(0), Err(error) => error.code() }
}

fn show_original_view_menu_at(owner: HWND, view: &IShellView, mut point: POINT) -> Result<()> {
    let selected = view.cast::<IFolderView2>().ok()
        .and_then(|folder| unsafe { folder.ItemCount(SVGIO_SELECTION) }.ok()).unwrap_or(0) > 0;
    let menu = ShellMenu::for_view(view, selected)?;
    let shift = unsafe { GetKeyState(VK_SHIFT.0 as i32) } < 0;
    let control = unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0;
    menu.populate(true, shift)?;
    if point.x == -1 && point.y == -1 {
        let mut rect = windows::Win32::Foundation::RECT::default();
        if unsafe { GetWindowRect(owner, &mut rect) }.is_ok() { point = POINT { x: rect.left + 24, y: rect.top + 24 }; }
        else { let _ = unsafe { GetCursorPos(&mut point) }; }
    }
    if let Some(id) = menu.track(owner, menu.hmenu(), point) {
        // The menu interaction was handled even if the selected Shell command
        // fails; falling through would open a second context menu.
        if let Err(error) = menu.invoke(owner, id, Some(point), shift, control) { report_failure(owner, error.code()); }
    }
    Ok(())
}

#[cfg(test)]
mod presenter_tests {
    #[test]
    fn supported_presenter_matches_the_native_verification() {
        let native = include_str!("../../native/shell_menu.cpp");
        assert!(native.contains(&format!("kExpectedMenuSha256[]=\"{}\"", super::SUPPORTED_PRESENTER_SHA256)),
            "Rust and native presenter digests must name the same module build");
        assert!(native.contains("kPresenterClass = {0x86ca1aa0,0x34aa,0x4e8b,{0xa5,0x09,0x50,0xc9,0x05,0xba,0xe2,0xa2}}"));
        assert!(super::PRESENTER_SERVER_KEY.contains("{86CA1AA0-34AA-4E8B-A509-50C905BAE2A2}"));
    }
}

#[cfg(test)]
mod policy_tests {
    use super::*;
    #[test]
    fn recycle_menu_routes_only_the_root_empty_command()->Result<()>{
        use windows::Win32::{System::Ole::{OleInitialize,OleUninitialize},UI::Shell::{SHGetKnownFolderItem,FOLDERID_RecycleBinFolder,KF_FLAG_DEFAULT}};
        unsafe{OleInitialize(None)}?;
        struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}let _apartment=Apartment;
        let item:windows::Win32::UI::Shell::IShellItem=unsafe{SHGetKnownFolderItem(&FOLDERID_RecycleBinFolder,KF_FLAG_DEFAULT,None)}?;
        let identity=super::super::pidl::item_pidl(&item).unwrap();
        let menu=ShellMenu::for_items(HWND::default(),&[identity])?;assert!(menu.recycle_bin);
        menu.populate(false,false)?;
        let count=unsafe{GetMenuItemCount(Some(menu.hmenu()))};
        assert!((0..count).any(|index|menu.verb(unsafe{GetMenuItemID(menu.hmenu(),index)}).is_some_and(|verb|empty_recycle_verb(&verb))),"recycle root has no empty command");
        assert!(!empty_recycle_verb("delete"));assert!(!empty_recycle_verb("empty-other-folder"));
        // Read-only: no command is invoked and the user's recycle bin is untouched.
        Ok(())
    }
    #[test]
    fn shift_extended_menus_never_use_the_windows11_presenter() {
        assert!(uses_modern_presenter(true, false, 0, false, false));
        assert!(!uses_modern_presenter(true, true, 0, false, false), "Shift+right-click must open the classic menu directly");
        assert!(!uses_modern_presenter(true, true, 0, true, true));
        assert!(!uses_modern_presenter(false, false, 0, false, false));
        assert!(!uses_modern_presenter(true, false, 2, false, false), "application IDs need the public menu contract");
        assert!(!uses_modern_presenter(true, false, 0, true, false), "a background without a folder item has no presenter target");
    }
    #[test]
    fn missing_or_disabled_history_commands_are_not_invoked() {
        for command in [None, Some(HistoryCommand { id: 17, enabled: false })] {
            for action in [HistoryAction::Undo, HistoryAction::Redo] {
                let error = invoke_history_command(action, command, |_| panic!("unavailable command invoked")).unwrap_err();
                assert_eq!(error.code(), windows::core::HRESULT(0x80040101u32 as i32));
            }
        }
    }
    #[test]
    fn enabled_history_uses_the_views_exact_id_once() {
        for (action, expected) in [(HistoryAction::Undo, 43), (HistoryAction::Redo, 91)] {
            let calls = Cell::new(0);
            invoke_history_command(action, Some(HistoryCommand { id: expected, enabled: true }), |id| {
                assert_eq!(id, expected); calls.set(calls.get() + 1); Ok(())
            }).unwrap();
            assert_eq!(calls.get(), 1);
        }
    }
    #[test]
    fn numeric_offsets_preserve_the_query_base_and_cannot_become_strings() {
        assert_eq!(std::mem::size_of::<NativeMenuResult>(), 16);
        assert_eq!(command_offset(1, 1).unwrap(), 0);
        assert_eq!(command_offset(43, 17).unwrap(), 26);
        assert_eq!(command_offset(0x10000, 1).unwrap(), 0xffff);
        assert!(command_offset(0, 1).is_err());
        assert!(command_offset(0x10001, 1).is_err());
    }
    #[test]
    fn item_context_cannot_reinterpret_a_child_under_another_parent() {
        let first = vec![3, 0, 10, 4, 0, 20, 21, 0, 0];
        let sibling = vec![3, 0, 10, 3, 0, 30, 0, 0];
        assert_eq!(sibling_item_offsets(&[first.clone(), sibling]).unwrap(), vec![3, 3]);
        let other_parent = vec![3, 0, 11, 4, 0, 20, 21, 0, 0];
        let mixed = [first.clone(), other_parent];
        let offsets = item_offsets(&mixed).unwrap();
        assert!(!same_item_parent(&mixed, &offsets));
        assert_eq!(offsets, vec![3, 3]);
        assert!(sibling_item_offsets(&[vec![0, 0]]).is_err());
        assert!(sibling_item_offsets(&[vec![4, 0, 10, 0]]).is_err());
        assert!(sibling_item_offsets(&[vec![1, 0, 0, 0]]).is_err());
        let mut trailing = first; trailing.push(0);
        assert!(sibling_item_offsets(&[trailing]).is_err());
        assert_eq!(sibling_item_offsets(&[vec![3, 0, 10, 0, 0], vec![3, 0, 11, 0, 0]]).unwrap(), vec![0, 0]);
    }
    #[test]
    fn native_commands_are_not_misclassified_as_application_ids_or_replayed() {
        let applications = application_ids(2).unwrap();
        assert_eq!(decode_native_result(NativeMenuResult { kind: 1, command: APP_FIRST + 1, status: 0, phase: 1 }, &applications).unwrap(), Some(MenuOutcome::Application(APP_FIRST + 1)));
        assert!(decode_native_result(NativeMenuResult { kind: 1, command: 0x7c01, status: 0, phase: 1 }, &applications).is_err());
        assert_eq!(decode_native_result(NativeMenuResult { kind: 7, command: 0x7c01, status: 0, phase: 1 }, &applications).unwrap(), Some(MenuOutcome::NativeHandled));
        let failed = windows::core::HRESULT(0x80070005u32 as i32);
        assert!(can_fall_back_before_presentation(failed, NativeMenuResult::default()));
        assert!(!can_fall_back_before_presentation(failed, NativeMenuResult { phase: 1, ..Default::default() }));
        assert!(!can_fall_back_before_presentation(failed, NativeMenuResult { phase: 2, ..Default::default() }));
        assert!(decode_native_result(NativeMenuResult { kind: 5, command: 19, status: failed.0, phase: 2 }, &applications).is_err());
    }
    #[test]
    fn only_requested_open_and_rename_interceptions_defer_shell_execution() {
        let intercept = MenuInterceptions { open: true, rename: true };
        assert_eq!(intercepted_verb(Some("ReName"), intercept), Some(MenuOutcome::Rename));
        assert_eq!(intercepted_verb(Some("open"), intercept), Some(MenuOutcome::Open));
        assert_eq!(intercepted_verb(Some("explore"), intercept), Some(MenuOutcome::Explore));
        for verb in [Some("copy"), Some("delete"), Some("openas"), None] { assert_eq!(intercepted_verb(verb, intercept), None); }
        assert_eq!(intercepted_verb(Some("open"), MenuInterceptions::default()), None);
    }
    #[test]
    fn modern_policy_covers_visible_functions_and_keeps_extensions_independent() {
        let mut options = ContextMenuOptions::default();
        assert_eq!(modern_hidden_features(&options), 1 << 11);
        options.hidden_verbs = ["cut", "copy", "paste", "pastelink", "copyaspath",
            "rename", "delete", "properties", "windows.modernshare", "new", "custom-third-party"]
            .map(str::to_string).to_vec();
        options.show_seven_zip = false;
        assert_eq!(modern_hidden_features(&options), 0x0fff);
        options.hidden_verbs.clear();
        options.show_seven_zip = true;
        options.show_wps = true;
        assert_eq!(modern_hidden_features(&options), 0);
    }
    #[test]
    fn filters_canonical_commands_without_using_menu_positions() {
        let options = ContextMenuOptions { modern:false, hidden_verbs:vec!["rename".into(),"delete".into()], ..Default::default() };
        assert!(should_hide(Some("ReName"), "任意本地化名称", &options));
        assert!(!should_hide(Some("open"), "重命名相关文档", &options));
        assert!(!should_hide(None, "打开", &options));
    }
    #[test]
    fn expanded_original_menu_uses_the_same_semantic_aliases() {
        let options = ContextMenuOptions { hidden_verbs: vec!["share".into(), "new".into(), "copyaspath".into()], ..Default::default() };
        for alias in ["Windows.Share", "Windows.ModernShareFlyout", "NewFolder", "Windows.CopyAsPath"] {
            assert!(should_hide(Some(alias), "任意显示名称", &options));
        }
        assert!(!should_hide(Some("open"), "共享文件", &options));
    }
    #[test]
    fn extension_groups_are_independent_and_restorable() {
        let mut options = ContextMenuOptions { show_seven_zip:false, ..Default::default() };
        assert!(should_hide(None,"&7-Zip",&options));
        assert!(should_hide(None,"WPS 文件压缩",&options));
        assert!(!should_hide(None,"压缩为 ZIP 文件",&options));
        options.show_seven_zip=true;options.show_wps=true;
        assert!(!should_hide(None,"7-Zip",&options));
        assert!(!should_hide(None,"WPS 文件压缩",&options));
    }
}

impl Drop for ShellMenu {
    fn drop(&mut self) { let _ = unsafe { DestroyMenu(self.hmenu) }; }
}

unsafe fn ui_object<T: Interface>(parent: &IShellFolder, owner: HWND, children: &[*const ITEMIDLIST]) -> Result<T> {
    unsafe { parent.GetUIObjectOf::<T>(owner, children, None) }
}

/// Sibling UI objects, or a complete cross-folder IDataObject for drag and drop.
pub fn items_ui_object<T: Interface>(owner: HWND, pidls: &[Vec<u8>]) -> Result<T> {
    let offsets = item_offsets(pidls)?;
    if !same_item_parent(pidls, &offsets) {
        if T::IID == IDataObject::IID {
            let pointers: Vec<_> = pidls.iter().map(|pidl| pidl.as_ptr().cast()).collect();
            let array = unsafe { SHCreateShellItemArrayFromIDLists(&pointers)? };
            return unsafe { array.BindToHandler(None, &BHID_DataObject) };
        }
        return Err(windows::core::Error::from_hresult(windows::core::HRESULT(0x80004002u32 as i32)));
    }
    let first = &pidls[0];
    let parent: IShellFolder = unsafe { SHBindToParent(first.as_ptr().cast(), None)? };
    let children: Vec<*const ITEMIDLIST> = pidls.iter().zip(offsets).map(|(pidl, offset)| unsafe { pidl.as_ptr().add(offset).cast() }).collect();
    unsafe { ui_object(&parent, owner, &children) }
}

fn last_item_offset(pidl: &[u8]) -> Option<usize> {
    let mut at = 0usize;
    let mut last = None;
    loop {
        let size = u16::from_le_bytes(pidl.get(at..at.checked_add(2)?)?.try_into().ok()?) as usize;
        if size == 0 { return (at + 2 == pidl.len()).then_some(last).flatten(); }
        if size < 2 { return None; }
        let next = at.checked_add(size)?;
        if next > pidl.len() { return None; }
        last = Some(at); at = next;
    }
}

fn item_offsets(pidls: &[Vec<u8>]) -> Result<Vec<usize>> {
    let invalid = || windows::core::Error::from_hresult(windows::core::HRESULT(0x80070057u32 as i32));
    let offsets: Vec<_> = pidls.iter().map(|pidl| last_item_offset(pidl).ok_or_else(invalid)).collect::<Result<_>>()?;
    if offsets.is_empty() { return Err(invalid()); }
    Ok(offsets)
}

fn same_item_parent(pidls: &[Vec<u8>], offsets: &[usize]) -> bool {
    pidls.iter().zip(offsets).all(|(pidl, &offset)| pidl[..offset] == pidls[0][..offsets[0]])
}

#[cfg(test)]
fn sibling_item_offsets(pidls: &[Vec<u8>]) -> Result<Vec<usize>> {
    let offsets = item_offsets(pidls)?;
    if !same_item_parent(pidls, &offsets) { return Err(windows::core::Error::from_hresult(windows::core::HRESULT(0x80070057u32 as i32))); }
    Ok(offsets)
}

fn mixed_item_context(owner: HWND, pidls: &[Vec<u8>]) -> Result<IContextMenu> {
    let pointers: Vec<*const ITEMIDLIST> = pidls.iter().map(|pidl| pidl.as_ptr().cast()).collect();
    #[cfg(all(target_arch = "x86_64", target_env = "msvc"))]
    {
        let count = u32::try_from(pidls.len()).map_err(|_| windows::core::Error::from_hresult(windows::core::HRESULT(0x80070057u32 as i32)))?;
        let mut raw = std::ptr::null_mut();
        unsafe { zs_menu_for_mixed_items(owner, count, pointers.as_ptr(), &mut raw) }.ok()?;
        if raw.is_null() { return Err(windows::core::Error::from_hresult(windows::core::HRESULT(0x80004003u32 as i32))); }
        Ok(unsafe { IContextMenu::from_raw(raw) })
    }
    #[cfg(not(all(target_arch = "x86_64", target_env = "msvc")))]
    {
        use std::mem::ManuallyDrop;
        use windows::Win32::UI::Shell::{SHGetDesktopFolder, SHCreateDefaultContextMenu, DEFCONTEXTMENU};
        let desktop = unsafe { SHGetDesktopFolder()? };
        let mut context = DEFCONTEXTMENU { hwnd: owner, psf: ManuallyDrop::new(Some(desktop)),
            cidl: pointers.len() as u32, apidl: pointers.as_ptr() as *mut *mut ITEMIDLIST, ..Default::default() };
        let result = unsafe { SHCreateDefaultContextMenu(&context) };
        unsafe { ManuallyDrop::drop(&mut context.psf); }
        result
    }
}

/// Parent PIDL bytes of an absolute PIDL (empty list for desktop children).
pub fn parent_pidl(pidl: &[u8]) -> Vec<u8> {
    let copy = unsafe { ILClone(pidl.as_ptr().cast()) };
    if copy.is_null() { return vec![0, 0]; }
    unsafe { let _ = ILRemoveLastID(Some(copy)); }
    let bytes = super::pidl::pidl_bytes(copy);
    unsafe { CoTaskMemFree(Some(copy.cast())) };
    bytes
}

#[cfg(test)]
#[path="menu_runtime_tests.rs"]
mod runtime_tests;
