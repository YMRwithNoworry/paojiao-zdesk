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
                CMF_EXPLORE, CMF_ITEMMENU, CMF_NODEFAULT, CMF_ASYNCVERBSTATE, CMF_OPTIMIZEFORINVOKE, GCS_VERBW, IFolderView2, DefSubclassProc, SetWindowSubclass, RemoveWindowSubclass, GetWindowSubclass,
            },
            WindowsAndMessaging::{
                CreatePopupMenu, DestroyMenu, GetMenuItemCount, GetMenuItemID, GetSubMenu, TrackPopupMenuEx,
                HMENU, SW_SHOWNORMAL, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_DRAWITEM, WM_INITMENUPOPUP, WM_UNINITMENUPOPUP, WM_MENUSELECT,
                WM_MEASUREITEM, WM_MENUCHAR, TPM_LEFTALIGN, TPM_TOPALIGN, TRACK_POPUP_MENU_FLAGS,
                GetMenuStringW, GetMenuState, DeleteMenu, MF_BYPOSITION, MF_BYCOMMAND, MF_DISABLED, MF_GRAYED, MF_SEPARATOR,
                WM_CONTEXTMENU, WM_NCDESTROY, SetForegroundWindow,
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

/// Compares immutable Shell selections as sets, including PIDLs whose cached
/// filesystem metadata changed. A missing/replaced item never matches a saved
/// selection. Call outside host/model borrows because Shell can pump messages.
pub fn same_item_identities(expected:&[Vec<u8>],current:&[Vec<u8>])->bool{
    use windows::Win32::UI::Shell::{SIGDN_DESKTOPABSOLUTEPARSING,SICHINT_CANONICAL};
    if expected.len()!=current.len()||expected.len()>8192
        ||!expected.iter().chain(current).all(|pidl|super::pidl::valid(pidl)){return false;}
    let mut a=expected.iter().collect::<Vec<_>>();let mut b=current.iter().collect::<Vec<_>>();a.sort();b.sort();
    if a==b{return true;}
    fn canonical(pidls:&[Vec<u8>])->Result<Vec<(String,IShellItem)>>{
        let mut result=Vec::with_capacity(pidls.len());let mut bytes=0usize;
        for pidl in pidls{
            let item=super::pidl::item_from_pidl(pidl)?;
            let raw=unsafe{item.GetDisplayName(SIGDN_DESKTOPABSOLUTEPARSING)}?;
            let name=unsafe{raw.to_string()};unsafe{CoTaskMemFree(Some(raw.0.cast()));}
            let name=name?;bytes=bytes.saturating_add(name.len());
            if bytes>4*1024*1024{return Err(windows::core::Error::from_hresult(windows::core::HRESULT(0x8007000eu32 as i32)));}
            result.push((name,item));
        }
        result.sort_by(|a,b|a.0.cmp(&b.0));Ok(result)
    }
    let (Ok(a),Ok(b))=(canonical(expected),canonical(current))else{return false;};
    if a.windows(2).any(|p|p[0].0==p[1].0)||b.windows(2).any(|p|p[0].0==p[1].0){return false;}
    a.iter().zip(&b).all(|((name,a),(other,b))|name==other&&unsafe{a.Compare(b,SICHINT_CANONICAL.0 as u32)}.is_ok_and(|order|order==0))
}

#[path="quick_menu.rs"]
mod quick_menu;
pub use quick_menu::{QuickAction,QuickMenu};
#[path="menu_catalog.rs"]
mod menu_catalog;
pub use menu_catalog::{MenuCatalogItem,catalog_item_hidden};
#[path="drawn_menu.rs"]
mod drawn_menu;
#[path="menu_target.rs"]
mod menu_target;
pub use menu_target::ViewMenuTarget;
pub fn classify_view_menu_point(view:HWND,source:HWND,position:LPARAM)->ViewMenuTarget{menu_target::classify(view,source,position)}
pub fn drawn_menu_open()->bool{drawn_menu::is_open()}
pub fn drawn_menu_window()->Option<HWND>{drawn_menu::popup_window()}
pub fn drawn_menu_snapshot()->serde_json::Value{drawn_menu::diagnostic_snapshot()}
pub fn release_menu_thread_resources(){drawn_menu::release_thread_resources();}
/// Resolves an actual public-popup command on its owning STA.
#[doc(hidden)]
pub fn active_command_by_verb(verb:&str)->Option<u32>{
    let active=ACTIVE.with(|active|active.borrow().clone())?;let root=HMENU(active.root as*mut _);
    (0..unsafe{GetMenuItemCount(Some(root))}.max(0)).map(|index|unsafe{GetMenuItemID(root,index)})
        .find(|id|canonical_verb(&active.menu,*id).is_some_and(|name|name.eq_ignore_ascii_case(verb)))
}

/// The file-manager front menu can be supplied without constructing any
/// extension IContextMenu on its UI thread. Unset inside extension workers.
pub type ViewMenuDelegate=fn(HWND,&IShellView,HWND,LPARAM,ViewMenuTarget)->Result<()>;
thread_local!{static VIEW_MENU_DELEGATE:Cell<Option<ViewMenuDelegate>>=const{Cell::new(None)};}
pub fn set_view_menu_delegate(delegate:Option<ViewMenuDelegate>){VIEW_MENU_DELEGATE.with(|current|current.set(delegate));}
pub(crate) fn view_menu_is_delegated()->bool{VIEW_MENU_DELEGATE.with(|current|current.get().is_some())}
pub type MenuDiagnosticDelegate=fn(&str,windows::core::HRESULT);
thread_local!{static DIAGNOSTIC:Cell<Option<MenuDiagnosticDelegate>>=const{Cell::new(None)};}
pub fn set_diagnostic_delegate(delegate:Option<MenuDiagnosticDelegate>){DIAGNOSTIC.with(|current|current.set(delegate));}
fn diagnostic(phase:&str,status:windows::core::HRESULT){
    if let Some(record)=DIAGNOSTIC.with(Cell::get){let _=std::panic::catch_unwind(||record(phase,status));}
}

/// Resolve keyboard requests at the selected item instead of at a fixed view corner.
pub fn context_menu_point(view:&IShellView,position:LPARAM)->Result<POINT>{
    if position.0!=-1{return Ok(POINT{x:position.0 as i16 as i32,y:(position.0>>16)as i16 as i32});}
    use windows::Win32::{Foundation::RECT,UI::{Shell::{FOLDERVIEWMODE,SVSI_SELECT},WindowsAndMessaging::GetClientRect},Graphics::Gdi::ClientToScreen};
    let window=unsafe{view.GetWindow()}?;let mut bounds=RECT::default();unsafe{GetClientRect(window,&mut bounds)}?;
    let mut point=POINT{x:16,y:16};
    if let Ok(folder)=view.cast::<IFolderView2>(){
        let has_selection=unsafe{folder.ItemCount(SVGIO_SELECTION)}.is_ok_and(|count|count>0);
        let focused=has_selection.then(||unsafe{folder.GetFocusedItem()}.ok()).flatten().filter(|index|*index>=0).filter(|index|{
            let Ok(item)=(unsafe{folder.Item(*index)})else{return false;};
            let selected=unsafe{folder.GetSelectionState(item)}.is_ok_and(|flags|flags&SVSI_SELECT.0 as u32!=0);
            unsafe{CoTaskMemFree(Some(item.cast()))};selected
        });
        if let Some(index)=focused.or_else(||has_selection.then(||unsafe{folder.GetSelectedItem(-1)}.ok()).flatten().filter(|index|*index>=0)){
            if let Ok(item)=unsafe{folder.Item(index)}{
                let location=unsafe{folder.GetItemPosition(item)};unsafe{CoTaskMemFree(Some(item.cast()))};
                if let Ok(location)=location{let mut mode=FOLDERVIEWMODE(0);let mut icon=16;let _=unsafe{folder.GetViewModeAndIconSize(&mut mode,&mut icon)};
                    point=POINT{x:location.x+4,y:location.y+icon.clamp(8,48)};}
            }
        }
    }
    point.x=point.x.clamp(0,bounds.right.saturating_sub(1).max(0));point.y=point.y.clamp(0,bounds.bottom.saturating_sub(1).max(0));
    unsafe{ClientToScreen(window,&mut point)}.ok()?;Ok(point)
}

pub fn control_owns_context_menu(view:HWND,mut source:HWND)->bool{
    use windows::Win32::UI::WindowsAndMessaging::{GetClassNameW,GetParent,IsChild};
    if source.0.is_null()||source==view||!unsafe{IsChild(view,source)}.as_bool(){return false;}
    for _ in 0..8{
        let mut class=[0u16;96];let size=unsafe{GetClassNameW(source,&mut class)}.max(0)as usize;
        if matches!(String::from_utf16_lossy(&class[..size]).as_str(),"SysHeader32"|"Edit"|"RICHEDIT50W"|"RichEdit20W"|"ComboBox"){return true;}
        let Ok(parent)=(unsafe{GetParent(source)})else{break;};if parent==view||parent.0.is_null(){break;}source=parent;
    }false
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
extern "C" { fn zs_menu_shared_host(enabled:i32); fn zs_menu_set_working_directory(path:PCWSTR); }

/// `SEE_MASK_UNICODE`, shared by CMINVOKECOMMANDINFOEX.
const CMIC_MASK_UNICODE: u32 = 0x4000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MenuOutcome { Dismissed, Application(u32), Rename, Open, Explore, ShellInvoked, NativeHandled, CompressZip, Copy, Cut, Paste, PinInExplorer, ChooseApplication, Share, Delete{permanent:bool} }

#[derive(Clone, Copy, Debug, Default)]
pub struct MenuInterceptions { pub rename: bool, pub open: bool, pub file_actions:bool }
impl MenuInterceptions { fn bits(self) -> u32 { u32::from(self.rename) | (u32::from(self.open) << 1) | (u32::from(self.file_actions)<<2) } }

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
fn intercepted_verb(verb: Option<&str>, intercept: MenuInterceptions,shift:bool) -> Option<MenuOutcome> {
    match verb {
        Some(verb) if intercept.file_actions && verb == "zsdesk.compresszip" => Some(MenuOutcome::CompressZip),
        Some(verb) if intercept.rename && verb.eq_ignore_ascii_case("rename") => Some(MenuOutcome::Rename),
        Some(verb) if intercept.open && verb.eq_ignore_ascii_case("open") => Some(MenuOutcome::Open),
        Some(verb) if intercept.open && verb.eq_ignore_ascii_case("explore") => Some(MenuOutcome::Explore),
        Some(verb) if intercept.file_actions && verb.eq_ignore_ascii_case("windows.modernshare") => Some(MenuOutcome::Share),
        Some(verb) if intercept.file_actions && verb.eq_ignore_ascii_case("openas") => Some(MenuOutcome::ChooseApplication),
        Some(verb) if intercept.file_actions&&(verb.eq_ignore_ascii_case("copy")||verb.eq_ignore_ascii_case("windows.copy"))=>Some(MenuOutcome::Copy),
        Some(verb) if intercept.file_actions&&(verb.eq_ignore_ascii_case("cut")||verb.eq_ignore_ascii_case("windows.cut"))=>Some(MenuOutcome::Cut),
        Some(verb) if intercept.file_actions&&(verb.eq_ignore_ascii_case("paste")||verb.eq_ignore_ascii_case("windows.paste"))=>Some(MenuOutcome::Paste),
        Some(verb) if intercept.file_actions&&(verb.eq_ignore_ascii_case("delete")||verb.eq_ignore_ascii_case("windows.delete"))=>Some(MenuOutcome::Delete{permanent:shift}),
        _ => None,
    }
}
fn empty_recycle_verb(verb:&str)->bool{verb.eq_ignore_ascii_case("empty")||verb.eq_ignore_ascii_case("emptyrecyclebin")}
fn decode_native_result(result: NativeMenuResult, applications: &[u32]) -> Result<Option<MenuOutcome>> {
    let status = windows::core::HRESULT(result.status);
    if super::file_transfer::is_cancelled(status) { return Ok(Some(MenuOutcome::Dismissed)); }
    status.ok()?;
    Ok(match result.kind {
        0 => Some(MenuOutcome::Dismissed),
        1 if applications.contains(&result.command) => Some(MenuOutcome::Application(result.command)),
        2 => Some(MenuOutcome::Rename), 3 => Some(MenuOutcome::Open), 4 => Some(MenuOutcome::Explore),
        5 => Some(MenuOutcome::ShellInvoked),
        6 if result.phase == 1 && result.command == 0 => None,
        7 => Some(MenuOutcome::NativeHandled),
        8 => Some(MenuOutcome::Copy),9=>Some(MenuOutcome::Cut),10=>Some(MenuOutcome::Paste),
        11=>Some(MenuOutcome::Delete{permanent:false}),12=>Some(MenuOutcome::Delete{permanent:true}),
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
struct ActiveContext { menu: IContextMenu, filter: bool, root: isize,skip_initialization:Rc<Cell<bool>> }
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
    #[serde(default)]
    pub quick_first:bool,
    #[serde(default)]
    pub native_first_page_apps:Option<Vec<String>>,
    pub modern: bool,
    #[serde(default)]
    pub drawn:bool,
    pub hidden_verbs: Vec<String>,
    #[serde(default)]
    pub shown_verbs: Vec<String>,
    pub show_seven_zip: bool,
    pub show_wps: bool,
}
impl Default for ContextMenuOptions {
    fn default() -> Self { Self { quick_first:false,native_first_page_apps:None,modern:true,drawn:true,hidden_verbs:Vec::new(),shown_verbs:Vec::new(),show_seven_zip:true,show_wps:true } }
}
/// SHA-256 of the only `Windows.UI.FileExplorer.dll` build whose private
/// presenter ABI `native/shell_menu.cpp` supports (`kExpectedMenuSha256`).
const SUPPORTED_PRESENTER_SHA256: &str = "4ce847cbbe84bedab2799bc7de9aa1d85684e66b95e2c59daf4ecee759aef0b1";
/// `kPresenterClass` registration; its in-process server is the module above.
const PRESENTER_SERVER_KEY: &str = "CLSID\\{86CA1AA0-34AA-4E8B-A509-50C905BAE2A2}\\InprocServer32";
#[path="feature_compatibility.rs"]
mod feature_compatibility;

/// Whether the Windows 11 presenter can host this application's menus.
///
/// The verified image must also use the verified feature layout. Runtime
/// feature state is read for each request; missing or unknown evidence selects
/// the classic presentation before private presenter activation. The native
/// image/vtable verification remains the final guard.
pub fn modern_presenter_supported() -> Option<bool> {
    static IMAGE_SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let image_matches = *IMAGE_SUPPORTED.get_or_init(|| {
        if !cfg!(all(target_arch = "x86_64", target_env = "msvc")) { return false; }
        let build = crate::theme::os_build();
        if build != 0 && build < 22000 { return false; }
        presenter_server_path().and_then(|path| file_sha256(&path))
            .is_some_and(|digest| digest.eq_ignore_ascii_case(SUPPORTED_PRESENTER_SHA256))
    });
    Some(feature_compatibility::supports_old_layout(image_matches))
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

fn presentation(modern:bool,_drawn:bool,build:u32,_verified_native:bool)->(bool,bool){
    if (10240..22000).contains(&build) { return (false,false); }
    (modern,modern)
}
pub fn presentation_name()->&'static str{
    let options=context_menu_options();if options.drawn{"public-windows11"}else if options.modern{"verified-native-windows11"}else{"native-classic"}
}
pub fn resolve_presentation(mut options: ContextMenuOptions) -> ContextMenuOptions {
    (options.modern,options.drawn)=presentation(options.modern,options.drawn,crate::theme::os_build(),false);
    options
}
pub fn set_context_menu_options(options: ContextMenuOptions) {
    let mut options=resolve_presentation(options);
    for rules in [&mut options.hidden_verbs,&mut options.shown_verbs]{
        rules.retain(|v| !v.is_empty() && v.len() <= 512 && !v.contains('\0'));
        for verb in rules.iter_mut(){verb.make_ascii_lowercase();}
        rules.sort();rules.dedup();rules.truncate(1024);
    }
    #[cfg(all(target_arch = "x86_64", target_env = "msvc"))]
    unsafe{zs_menu_set_hidden_features(modern_hidden_features(&options));zs_menu_set_original_callback(Some(show_expanded_view_menu));}
    OPTIONS.with(|current| *current.borrow_mut() = options);
}
pub fn context_menu_options() -> ContextMenuOptions { OPTIONS.with(|options| options.borrow().clone()) }
pub fn drawn_renderer_ready()->bool{drawn_menu::renderer_ready()}
pub fn prepare_drawn_runtime()->Result<()>{drawn_menu::prepare()}

pub fn native_app_registrations()->Result<Vec<(String,String)>>{
    let mut list=super::legacy_commands::inventory().into_iter().collect::<std::collections::BTreeMap<_,_>>();
    let registrations=match super::packaged_commands::discover(){
        Ok(found)=>found.registrations,Err(error) if list.is_empty()=>return Err(error),Err(_)=>Vec::new(),
    };
    for r in registrations{
        let package=r.package_family.split('_').next().unwrap_or(&r.package_family);
        list.entry(r.settings_id()).or_insert_with(||r.display_name.clone().unwrap_or_else(||format!("{package} · {}", r.application_id)));
    }
    let mut list=list.into_iter().collect::<Vec<_>>();
    list.sort_by(|a,b|a.1.cmp(&b.1).then(a.0.cmp(&b.0)));Ok(list)
}
/// Pre-0.3.19 profiles may identify a packaged provider by class alone.
pub fn native_app_selected(id:&str,selected:&[String])->bool{
    let previous=id.strip_prefix("pkg:").and_then(|key|key.split_once(':')).map(|(class,_)|class);
    selected.iter().any(|key|key.eq_ignore_ascii_case(id)||previous.is_some_and(|class|class.eq_ignore_ascii_case(key)))
}

/// Initialize the verified native presenter on its worker STA, without opening a popup.
pub fn prepare_native_runtime(owner: HWND, browser: &IExplorerBrowser) -> Result<()> {
    #[cfg(all(target_arch = "x86_64", target_env = "msvc"))]
    if modern_presenter_supported() == Some(true) {
        unsafe extern "C" {
            fn zs_menu_prepare_runtime(owner: HWND, browser: *mut core::ffi::c_void) -> windows::core::HRESULT;
        }
        return unsafe { zs_menu_prepare_runtime(owner, browser.as_raw()) }.ok();
    }
    Err(windows::core::Error::from_hresult(windows::Win32::Foundation::E_NOTIMPL))
}

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
        count: u32, pidls: *const *const ITEMIDLIST, folder: *mut core::ffi::c_void, background: i32, source_browser:*mut core::ffi::c_void,input_flags:u32,
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
    if drawn_menu::is_open(){drawn_menu::cancel();return Ok(());}
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
        Self::enter_initialized(menu,filter,root,false)
    }
    fn enter_initialized(menu:&IContextMenu,filter:bool,root:HMENU,initialized:bool)->Self{
        Self(ACTIVE.with(|active| active.replace(Some(ActiveContext { menu: menu.clone(), filter, root: root.0 as isize,skip_initialization:Rc::new(Cell::new(initialized)) }))))
    }
}
impl Drop for ActiveMenuScope {
    fn drop(&mut self) { ACTIVE.with(|active| *active.borrow_mut() = self.0.take()); }
}

/// Forwards menu messages to the context menu being tracked on this thread.
pub fn forward_menu_message(msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
    if ![WM_INITMENUPOPUP,WM_UNINITMENUPOPUP,WM_MENUSELECT, WM_DRAWITEM, WM_MEASUREITEM, WM_MENUCHAR].contains(&msg) { return None; }
    let active = ACTIVE.with(|active| active.borrow().clone())?;
    if msg==WM_INITMENUPOPUP&&wp.0 as isize==active.root&&active.skip_initialization.replace(false){return Some(LRESULT(0));}
    let menu = active.menu;
    let mut handled = None;
    if let Ok(menu3) = menu.cast::<IContextMenu3>() {
        let mut result = LRESULT(0);
        if unsafe { menu3.HandleMenuMsg2(msg, wp, lp, Some(&mut result)) }.is_ok() { handled = Some(result); }
    } else if let Ok(menu2) = menu.cast::<IContextMenu2>() {
        if unsafe { menu2.HandleMenuMsg(msg, wp, lp) }.is_ok() { handled = Some(LRESULT(0)); }
    }
    // Legacy function/vendor preferences apply only at the top level. Opened
    // cascades honor explicit path-scoped catalog rules, never label substrings
    // or generic names such as "open" belonging to another command group.
    if msg == WM_INITMENUPOPUP && active.filter {
        let popup=HMENU(wp.0 as *mut _);let options=context_menu_options();
        if wp.0 as isize==active.root{filter_native_menu(popup,&menu,&options);}
        else if options.hidden_verbs.iter().any(|key|key.starts_with("verb-path-")||key.starts_with("label-path-")){
            if let Some(path)=menu_catalog::popup_path(HMENU(active.root as*mut _),popup,&menu){filter_native_menu_at(popup,&menu,&options,&path,false);}
        }
    }
    handled
}

pub struct ShellMenu {
    menu: IContextMenu,
    hmenu: HMENU,
    owns_menu:Cell<bool>,
    recycle_bin:bool,
    recycle_contents:bool,
    background:bool,
    input_flags:Cell<u32>,
    /// Populated for Shift+right-click. Explorer answers that gesture with the
    /// classic menu directly; the Windows 11 presenter is never involved.
    extended: Cell<bool>,
    optimized:Cell<Option<bool>>,
    directory:RefCell<Option<Vec<u16>>>,
}

impl ShellMenu {
    pub fn new(menu: IContextMenu) -> Result<Self> {
        let hmenu = unsafe { CreatePopupMenu()? };
        Ok(Self { menu, hmenu,owns_menu:Cell::new(true), recycle_bin:false,recycle_contents:false,background:false,input_flags:Cell::new(0), extended: Cell::new(false),optimized:Cell::new(None),directory:RefCell::new(None) })
    }
    pub fn new_background(menu:IContextMenu)->Result<Self>{let mut value=Self::new(menu)?;value.background=true;Ok(value)}

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
        Self::new_background(menu)
    }

    pub fn for_view(view: &IShellView, selection: bool) -> Result<Self> {
        let _timer=MenuPhaseTimer::bind();
        let menu: IContextMenu = unsafe { view.GetItemObject(if selection { SVGIO_SELECTION } else { SVGIO_BACKGROUND })? };
        let mut menu=Self::new(menu)?;
        menu.background=!selection;
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

    /// Retains the real view/site while adding registered packaged commands to
    /// presentations that do not perform native Windows 11 discovery.
    pub fn for_view_with_packages(view:&IShellView,browser:&IExplorerBrowser,owner:HWND,selection:bool)->Result<Self>{
        let mut menu=Self::for_view(view,selection)?;
        menu.menu=super::packaged_overlay::create(menu.menu.clone(),browser.clone(),view.clone(),owner,!selection)?;
        Ok(menu)
    }
    pub fn for_compact_selection(owner:HWND,pidls:&[Vec<u8>],resolve:Box<dyn Fn()->Result<IShellView>>)->Result<Option<Self>>{
        use windows::Win32::System::SystemServices::SFGAO_FILESYSTEM;
        let mut ptrs=Vec::with_capacity(pidls.len());
        let mut parents=std::collections::HashSet::new();
        for pidl in pidls{
            let item=super::pidl::item_from_pidl(pidl)?;
            if !unsafe{item.GetAttributes(SFGAO_FILESYSTEM)}.is_ok_and(|attrs|attrs.contains(SFGAO_FILESYSTEM)){
                return Ok(None);
            }
            // Deleted objects can have a filesystem backing path but a virtual parent.
            if parents.insert(parent_pidl(pidl))&&!unsafe{item.GetParent().and_then(|parent|parent.GetAttributes(SFGAO_FILESYSTEM))}.is_ok_and(|attrs|attrs.contains(SFGAO_FILESYSTEM)){
                return Ok(None);
            }
            ptrs.push(pidl.as_ptr().cast());
        }
        let items=unsafe{SHCreateShellItemArrayFromIDLists(&ptrs)}?;
        Self::new(super::compact_commands::for_selection(items,owner,resolve)).map(Some)
    }
    pub fn for_compact_view(view:&IShellView,browser:&IExplorerBrowser,owner:HWND,selection:bool,options:&ContextMenuOptions)->Result<Self>{
        let in_bin=super::file_transfer::paste_folder(view).ok().flatten().is_some_and(|item|super::shell_item::is_recycle_bin_item(&item));
        let bin_icon=selection&&!in_bin&&view.cast::<windows::Win32::UI::Shell::IFolderView2>()
            .and_then(|folder|unsafe{folder.GetSelection(false)}).is_ok_and(|items|
                unsafe{items.GetCount()}==Ok(1)&&unsafe{items.GetItemAt(0)}.is_ok_and(|item|super::shell_item::is_recycle_bin_item(&item)));
        if in_bin||bin_icon{
            // A deleted item's context is owned by the Recycle Bin namespace.
            // Opening its backing filesystem path would target another object.
            let mut menu=Self::for_view(view,selection)?;
            menu.recycle_bin=bin_icon||!selection;
            menu.recycle_contents=in_bin&&selection;
            return Ok(menu);
        }
        let mut context=super::compact_commands::create(view,!selection);
        if options.native_first_page_apps.as_ref().is_some_and(|ids|ids.iter().any(|id|id.starts_with("pkg:")||windows::core::GUID::try_from(id.as_str()).is_ok())){
            context=super::packaged_overlay::create(context,browser.clone(),view.clone(),owner,!selection)?;
        }
        if selection{Self::new(context)}else{Self::new_background(context)}
    }

    pub fn hmenu(&self) -> HMENU { self.hmenu }
    pub fn set_input_context(&self,keyboard:bool,f10:bool){self.input_flags.set(u32::from(keyboard)|(u32::from(keyboard&&f10)<<1));}
    pub fn set_directory(&self,path:Option<&std::path::Path>){
        use std::os::windows::ffi::OsStrExt;
        *self.directory.borrow_mut()=path.map(|path|path.as_os_str().encode_wide().chain(Some(0)).collect());
    }
    pub(super) fn context(&self)->&IContextMenu{&self.menu}

    /// ShellNew owns its dynamic document templates and canonical command IDs.
    pub fn track_new(&self,owner:HWND,point:POINT,before:&mut dyn FnMut()->Result<()>)->Result<MenuOutcome>{
        let sub=self.submenu_with_verb("NewFolder").or_else(||{
            (0..unsafe{GetMenuItemCount(Some(self.hmenu))}.max(0)).find_map(|index|{
                let sub=unsafe{GetSubMenu(self.hmenu,index)};
                if sub.is_invalid(){None}else{self.init_popup(sub,index);Some(sub)}
            })
        }).ok_or_else(||windows::core::Error::new(windows::core::HRESULT(0x80004005u32 as i32),"当前文件夹没有可用的新建命令"))?;
        let Some(command)=self.track_unfiltered_initialized(owner,sub,point,true)?else{return Ok(MenuOutcome::Dismissed);};
        before()?;self.invoke(owner,command,Some(point),false,false)?;Ok(MenuOutcome::ShellInvoked)
    }

    /// Fills the popup menu. `extended` adds Shift-only verbs.
    pub fn populate(&self, rename: bool, extended: bool) -> Result<()> {
        let _timer=MenuPhaseTimer::populate();
        // Permit handlers that support it to obtain expensive state off the
        // menu's UI path. Windows retains normal state evaluation otherwise.
        let mut flags = (if self.background{CMF_NODEFAULT}else{CMF_ITEMMENU}) | CMF_EXPLORE | CMF_ASYNCVERBSTATE;
        if rename&&!self.background { flags |= CMF_CANRENAME; }
        if extended { flags |= CMF_EXTENDEDVERBS; }
        self.extended.set(extended);
        unsafe { self.menu.QueryContextMenu(self.hmenu, 0, SHELL_FIRST, SHELL_LAST, flags) }.ok()
    }

    /// The compact presenter builds its own commands. Expensive classic-only
    /// handlers are queried only if the user opens Show more options.
    pub fn populate_compact(&self,rename:bool,extended:bool,folder_selection:bool)->Result<()> {
        if extended||!context_menu_options().modern{return self.populate(rename,extended);}
        self.optimized.set(Some(rename));self.extended.set(false);
        // The optimized lookup omits PinToStartScreen and can return raw verb
        // names instead of localized folder command labels.
        let flags=(if self.background{CMF_NODEFAULT}else{CMF_ITEMMENU})|CMF_EXPLORE|CMF_ASYNCVERBSTATE|
            if folder_selection{CMF_NORMAL}else{CMF_OPTIMIZEFORINVOKE}|if rename{CMF_CANRENAME}else{CMF_NORMAL};
        unsafe{self.menu.QueryContextMenu(self.hmenu,0,SHELL_FIRST,SHELL_LAST,flags)}.ok()
    }
    fn ensure_classic(&self)->Result<()> {
        if let Some(rename)=self.optimized.take(){
            while unsafe{GetMenuItemCount(Some(self.hmenu))}>0{unsafe{DeleteMenu(self.hmenu,0,MF_BYPOSITION)?;}}
            self.populate(rename,self.extended.get())?;
        }Ok(())
    }

    /// Direct canonical commands need no third-party visual menu construction.
    /// Shell handlers receive the documented optimize-for-invoke hint.
    pub fn populate_for_invoke(&self, extended: bool) -> Result<()> {
        let flags = (if self.background{CMF_NODEFAULT}else{CMF_ITEMMENU}) | CMF_EXPLORE | CMF_OPTIMIZEFORINVOKE
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
    pub fn track_selected_in_view_before(&self,owner:HWND,point:POINT,pidls:&[Vec<u8>],browser:&IExplorerBrowser,
            intercept:MenuInterceptions,before:&mut dyn FnMut()->Result<()>)->Result<MenuOutcome>{
        self.track_standalone(owner,point,pidls,None,false,Some(browser),0,intercept,before)
    }

    pub fn track_folder_background(&self, owner: HWND, point: POINT, path: &std::path::Path, application_count: usize,
            before: &mut dyn FnMut() -> Result<()>) -> Result<MenuOutcome> {
        self.track_folder_background_with_interceptions(owner,point,path,application_count,MenuInterceptions::default(),before)
    }
    pub fn track_folder_background_with_interceptions(&self,owner:HWND,point:POINT,path:&std::path::Path,application_count:usize,
            intercept:MenuInterceptions,before:&mut dyn FnMut()->Result<()>)->Result<MenuOutcome>{
        let folder = if application_count==0 && context_menu_options().modern {
            unsafe { super::shell_item(&zsdesk_core::model::explorer::ShellLocation::Path(path.to_path_buf())) }.ok()
        } else { None };
        self.track_standalone(owner, point, &[], folder.as_ref(), true, None, application_count, intercept, before)
    }

    pub fn track_background_item(&self,owner:HWND,point:POINT,folder:&IShellItem,before:&mut dyn FnMut()->Result<()>)->Result<MenuOutcome>{
        self.track_standalone(owner,point,&[],Some(folder),true,None,0,MenuInterceptions::default(),before)
    }

    pub fn track_background_view(&self,owner:HWND,point:POINT,folder:&IShellItem,browser:&IExplorerBrowser,before:&mut dyn FnMut()->Result<()>)->Result<MenuOutcome>{
        self.track_background_view_with_interceptions(owner,point,folder,browser,MenuInterceptions::default(),before)
    }
    pub fn track_background_view_with_interceptions(&self,owner:HWND,point:POINT,folder:&IShellItem,browser:&IExplorerBrowser,
            intercept:MenuInterceptions,before:&mut dyn FnMut()->Result<()>)->Result<MenuOutcome>{
        self.track_standalone(owner,point,&[],Some(folder),true,Some(browser),0,intercept,before)
    }

    fn track_standalone(&self, owner: HWND, point: POINT, pidls: &[Vec<u8>], folder: Option<&IShellItem>, background: bool, browser:Option<&IExplorerBrowser>,
            application_count: usize, intercept: MenuInterceptions, before: &mut dyn FnMut() -> Result<()>) -> Result<MenuOutcome> {
        let intercept=if self.recycle_contents{MenuInterceptions::default()}else{intercept};
        let applications = application_ids(application_count)?;
        let options=context_menu_options();
        #[cfg(all(target_arch="x86_64",target_env="msvc"))]
        if !options.drawn&&uses_modern_presenter(options.modern,self.extended.get()&&self.input_flags.get()&2==0,application_count,background,folder.is_some()){
            let count=u32::try_from(pidls.len()).map_err(|_|windows::core::Error::from_hresult(windows::core::HRESULT(0x80070057u32 as i32)))?;
            self.filter(self.hmenu,&options);
            let identifiers:Vec<*const ITEMIDLIST>=pidls.iter().map(|pidl|pidl.as_ptr().cast()).collect();
            let mut outcome=NativeMenuResult::default();let mut observer=BeforeShell{callback:&mut *before};
            // A native expansion callback must not move the next popup to its
            // own row/cursor position. Keep the caller's screen-space anchor.
            let mut native_point=point;
            let directory=self.directory.borrow();unsafe{zs_menu_set_working_directory(directory.as_ref().map_or(PCWSTR::null(),|value|PCWSTR(value.as_ptr())));}
            struct DirectoryScope;impl Drop for DirectoryScope{fn drop(&mut self){unsafe{zs_menu_set_working_directory(PCWSTR::null());}}}let _directory=DirectoryScope;
            let active=ActiveMenuScope::enter(&self.menu,true,self.hmenu);
            let result=unsafe{zs_menu_track_standalone(owner,self.menu.as_raw(),self.hmenu,count,identifiers.as_ptr(),
                folder.map_or(std::ptr::null_mut(),Interface::as_raw),i32::from(background),browser.map_or(std::ptr::null_mut(),Interface::as_raw),self.input_flags.get(),
                &mut native_point,applications.as_ptr(),applications.len()as u32,intercept.bits(),Some(before_shell),(&mut observer as*mut BeforeShell<'_>).cast(),&mut outcome)};
            drop(active);
            // The native presenter can consume and destroy the supplied HMENU
            // when it closes. Release our ownership before another menu can
            // reuse that numeric handle.
            let root_valid=unsafe{windows::Win32::UI::WindowsAndMessaging::IsMenu(self.hmenu)}.as_bool();
            if !root_valid{self.owns_menu.set(false);}
            if std::env::var_os("ZSDESK_MENU_TEST_PROBE").is_some(){eprintln!("native-popup returned hr={:?} kind={} command={} status={} phase={}",result,outcome.kind,outcome.command,outcome.status,outcome.phase);}
            if result.is_ok(){if let Some(action)=decode_native_result(outcome,&applications)?{return Ok(action);}
                unsafe{let _=SetForegroundWindow(owner);}
                if !root_valid{return self.track_recreated_classic(owner,point,&applications,intercept,before);}
            }
            else if !can_fall_back_before_presentation(result,outcome){return Err(windows::core::Error::from_hresult(result));}
            if !root_valid{return self.track_recreated_classic(owner,point,&applications,intercept,before);}
        }
        // Drawn and classic styles return IDs through one guarded dispatch path.
        self.ensure_classic()?;
        let Some(command) = self.track_result(owner,self.hmenu,point,true,false)? else { return Ok(MenuOutcome::Dismissed) };
        if applications.contains(&command) { return Ok(MenuOutcome::Application(command)); }
        let shift=unsafe{GetKeyState(VK_SHIFT.0 as i32)}<0;let control=unsafe{GetKeyState(VK_CONTROL.0 as i32)}<0;
        self.dispatch_shell_command(owner,command,Some(point),intercept,shift,control,before)
    }
    fn track_recreated_classic(&self,owner:HWND,point:POINT,applications:&[u32],intercept:MenuInterceptions,before:&mut dyn FnMut()->Result<()>)->Result<MenuOutcome>{
        if std::env::var_os("ZSDESK_MENU_TEST_PROBE").is_some(){eprintln!("classic-popup rebuilt=true anchor={},{}",point.x,point.y);}
        let mut menu=Self::new(self.menu.clone())?;menu.background=self.background;menu.recycle_bin=self.recycle_bin;
        menu.input_flags.set(self.input_flags.get());*menu.directory.borrow_mut()=self.directory.borrow().clone();
        // Keep the same context/site and immutable selection, but rebuild its
        // complete public menu into a fresh handle after native dismissal.
        menu.populate(self.optimized.take().unwrap_or(intercept.rename),self.extended.get())?;
        struct Style(ContextMenuOptions);impl Drop for Style{fn drop(&mut self){set_context_menu_options(self.0.clone());}}
        let previous=context_menu_options();let mut classic=previous.clone();classic.modern=false;classic.drawn=false;
        let _style=Style(previous);set_context_menu_options(classic);
        let Some(command)=menu.track_result(owner,menu.hmenu,point,true,false)?else{return Ok(MenuOutcome::Dismissed);};
        if applications.contains(&command){return Ok(MenuOutcome::Application(command));}
        let shift=unsafe{GetKeyState(VK_SHIFT.0 as i32)}<0;let control=unsafe{GetKeyState(VK_CONTROL.0 as i32)}<0;
        menu.dispatch_shell_command(owner,command,Some(point),intercept,shift,control,before)
    }
    fn dispatch_shell_command(&self,owner:HWND,command:u32,point:Option<POINT>,intercept:MenuInterceptions,
            shift:bool,control:bool,before:&mut dyn FnMut()->Result<()>)->Result<MenuOutcome>{
        if !Self::is_shell_id(command) { return Err(windows::core::Error::from_hresult(windows::core::HRESULT(0x80070057u32 as i32))); }
        let verb=self.verb(command);
        if let Some(action) = intercepted_verb(verb.as_deref(), intercept,shift) { return Ok(action); }
        before()?;
        if let Err(error)=self.invoke(owner,command,point,shift,control){
            if error.code()==windows::Win32::Foundation::E_ACCESSDENIED&&verb.as_deref().is_some_and(|v|matches!(v.to_ascii_lowercase().as_str(),"pintostartscreen"|"unpinfromstart"|"unpinfromstartscreen")){
                return Ok(MenuOutcome::PinInExplorer);
            }
            return Err(error);
        }
        Ok(MenuOutcome::ShellInvoked)
    }

    /// Explicit command-bar menus retain their available commands independently
    /// of which commands are shown by a file's context menu.
    pub fn track_unfiltered(&self, owner: HWND, menu: HMENU, point: POINT) -> Option<u32> {
        self.track_with_policy(owner, menu, point, false)
    }
    pub fn track_unfiltered_initialized(&self,owner:HWND,menu:HMENU,point:POINT,initialized:bool)->Result<Option<u32>>{
        self.track_result(owner,menu,point,false,initialized)
    }

    fn track_with_policy(&self, owner: HWND, menu: HMENU, point: POINT, context: bool) -> Option<u32> {
        match self.track_result(owner,menu,point,context,false){Ok(value)=>value,Err(error)=>{report_failure(owner,error.code());None}}
    }
    fn track_result(&self,owner:HWND,menu:HMENU,point:POINT,context:bool,initialized:bool)->Result<Option<u32>>{
        let options = context_menu_options();
        let filter = context;
        if filter { self.filter(menu, &options); }
        let _active = ActiveMenuScope::enter_initialized(&self.menu,filter,menu,initialized);
        if options.drawn{
            return drawn_menu::track_initialized(owner,menu,point,&self.menu,&options,filter,initialized);
        }
        let flags = TRACK_POPUP_MENU_FLAGS(TPM_RETURNCMD.0 | TPM_RIGHTBUTTON.0 | TPM_LEFTALIGN.0 | TPM_TOPALIGN.0);
        let trace=std::env::var_os("ZSDESK_MENU_TEST_PROBE").is_some();
        if trace{
            eprintln!("classic-popup tracking owner={} anchor={},{}",owner.0 as usize,point.x,point.y);
            unsafe{windows::Win32::Foundation::SetLastError(windows::Win32::Foundation::ERROR_SUCCESS);}
        }
        let chosen = unsafe { TrackPopupMenuEx(menu, flags.0, point.x, point.y, owner, None) }.0 as u32;
        if trace{let error=unsafe{windows::Win32::Foundation::GetLastError()}.0;
            eprintln!("classic-popup result={chosen} error={error} items={} owner={} anchor={},{}",unsafe{GetMenuItemCount(Some(menu))},owner.0 as usize,point.x,point.y);}
        // Complete the foreground popup handoff before its owner is reused or
        // destroyed. Without this documented benign message, a subsequent
        // popup can immediately dismiss after switching between menu owners.
        let _=unsafe{windows::Win32::UI::WindowsAndMessaging::PostMessageW(Some(owner),
            windows::Win32::UI::WindowsAndMessaging::WM_NULL,WPARAM(0),LPARAM(0))};
        Ok((chosen != 0).then_some(chosen))
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
        let directory=self.directory.borrow();
        let info = CMINVOKECOMMANDINFOEX {
            cbSize: std::mem::size_of::<CMINVOKECOMMANDINFOEX>() as u32,
            fMask: mask, hwnd: owner, lpVerb: verb, nShow: SW_SHOWNORMAL.0, lpVerbW: verb_w,
            ptInvoke: point.unwrap_or_default(),lpDirectoryW:directory.as_ref().map_or(PCWSTR::null(),|value|PCWSTR(value.as_ptr())), ..Default::default()
        };
        unsafe { self.menu.InvokeCommand((&info as *const CMINVOKECOMMANDINFOEX).cast::<CMINVOKECOMMANDINFO>()) }
    }
}

fn should_hide(verb: Option<&str>, label: &str, options: &ContextMenuOptions) -> bool {
    if let Some(visible)=menu_catalog::explicit_visibility(verb,label,&[],options){return !visible;}
    legacy_should_hide(verb,label,options)
}
fn legacy_should_hide(verb: Option<&str>, label: &str, options: &ContextMenuOptions) -> bool {
    let canonical = verb.unwrap_or_default().to_ascii_lowercase();
    if options.hidden_verbs.iter().any(|v| v == &canonical) { return true; }
    if command_feature(&canonical) & modern_hidden_features(options) != 0 { return true; }
    let label = label.replace('&', "").to_lowercase();
    let group = label.split('(').next().unwrap_or(&label).trim();
    if options.hidden_verbs.iter().any(|v| v == "new") && matches!(group, "新建" | "新增" | "new") { return true; }
    (!options.show_seven_zip && (label.contains("7-zip") || label.contains("7zip") || canonical.starts_with("7-zip")))
        || (!options.show_wps && (label.contains("wps") || canonical.starts_with("wps")))
}

pub(super) fn canonical_verb(context: &IContextMenu, id: u32) -> Option<String> {
    if !ShellMenu::is_shell_id(id) { return None; }
    canonical_verb_at_offset(context, id - SHELL_FIRST)
}
pub(super) fn canonical_verb_at_offset(context: &IContextMenu, offset: u32) -> Option<String> {
    let mut buffer = [0u16; 128];
    unsafe { context.GetCommandString(offset as usize, GCS_VERBW, None, PSTR(buffer.as_mut_ptr().cast()), buffer.len() as u32) }.ok()?;
    let end = buffer.iter().position(|&c| c == 0)?;
    if end==0{return None;}String::from_utf16(&buffer[..end]).ok()
}

pub fn common_command_visible(options:&ContextMenuOptions,verb:&str,label:&str)->bool{
    !should_hide(Some(verb),label,options)
}

/// Hides the preference-controlled entries of a context menu's top level.
/// A hidden cascade disappears with its children. Deeper commands are handled
/// only by explicit path-scoped catalog rules as each popup is initialized.
fn filter_native_menu(menu: HMENU, context: &IContextMenu, options: &ContextMenuOptions) {
    filter_native_menu_at(menu,context,options,&[],true);
}
fn filter_native_menu_at(menu: HMENU, context: &IContextMenu, options: &ContextMenuOptions,path:&[String],legacy:bool) {
    if options.hidden_verbs.is_empty()&&options.shown_verbs.is_empty()&&options.show_seven_zip&&options.show_wps{return;}
    let count = unsafe { GetMenuItemCount(Some(menu)) }.max(0);
    let has_dynamic=options.hidden_verbs.iter().chain(&options.shown_verbs).any(|key|key.starts_with("verb:")||key.starts_with("verb-")||key.starts_with("label:")||key.starts_with("label-"));
    let mut keys=std::collections::HashMap::<String,usize>::new();
    if has_dynamic{for index in 0..count{
        if let Some(raw)=menu_catalog::read_item(menu,index,context){if raw.complete||raw.verb.is_some(){
            if let Some(key)=menu_catalog::identity_key(raw.verb.as_deref(),&raw.label,path){*keys.entry(key).or_default()+=1;}
        }}
    }}
    for index in (0..count).rev() {
        let id = unsafe { GetMenuItemID(menu, index) };
        if id >= APP_FIRST && id != u32::MAX { continue; }
        let mut text = [0u16; 1024];
        let length = unsafe { GetMenuStringW(menu, index as u32, Some(&mut text), MF_BYPOSITION) }.max(0) as usize;
        let label = String::from_utf16_lossy(&text[..length]);
        // With only named extension filtering enabled there is no need to call
        // every third-party handler's GetCommandString on each popup.
        let verb = if options.hidden_verbs.is_empty()&&options.shown_verbs.is_empty() { None } else { canonical_verb(context, id) };
        let unique=!has_dynamic||menu_catalog::identity_key(verb.as_deref(),&label,path).is_none_or(|key|keys.get(&key).is_none_or(|count|*count==1));
        let hide=if !unique{false}else{match menu_catalog::explicit_visibility(verb.as_deref(),&label,path,options){Some(visible)=>!visible,None=>legacy&&legacy_should_hide(verb.as_deref(),&label,options)}};
        if hide {
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

/// Resolve an owned pointer request outside the application's model borrow.
/// Keep only the cloned COM view across accessibility calls, which may pump.
pub fn prepare_pointer_menu(window:HWND,position:LPARAM)->Result<(IShellView,ViewMenuTarget)>{
    let view=attached_menu_view(window)?;
    let target=menu_target::prepare(window,window,position)?;
    Ok((view,target))
}
pub fn attached_menu_view(window:HWND)->Result<IShellView>{
    let mut data=0usize;
    if !unsafe{GetWindowSubclass(window,Some(view_menu_proc),VIEW_MENU_SUBCLASS,Some(&mut data))}.as_bool()||data==0{
        return Err(windows::core::Error::from_hresult(windows::core::HRESULT(0x800704c7u32 as i32)));
    }
    Ok(unsafe{(*(data as*const ViewMenuState)).view.clone()})
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
        let source=HWND(wp.0 as*mut _);
        if control_owns_context_menu(hwnd,source){return unsafe{DefSubclassProc(hwnd,msg,wp,lp)};}
        let target=match menu_target::prepare(hwnd,source,lp){Ok(target)=>target,Err(error)=>{report_failure(hwnd,error.code());return LRESULT(0);}};
        if target==ViewMenuTarget::NativeControl{return unsafe{DefSubclassProc(hwnd,msg,wp,lp)};}
        let target=if target==ViewMenuTarget::Unresolved{
            if state.view.cast::<IFolderView2>().and_then(|view|unsafe{view.ItemCount(SVGIO_SELECTION)}).is_ok_and(|count|count==0){ViewMenuTarget::Background}
            else{report_failure(hwnd,windows::core::HRESULT(0x8000000au32 as i32));return LRESULT(0);}
        }else{target};
        if let Some(delegate)=VIEW_MENU_DELEGATE.with(Cell::get){
            let view=state.view.clone();
            diagnostic("main-context-delegate",windows::core::HRESULT(0));
            let result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||delegate(hwnd,&view,source,lp,target)))
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
                delegate(owner,view,owner,packed,ViewMenuTarget::CurrentSelection)
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
        point=context_menu_point(view,LPARAM(-1))?;
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
    #[cfg(all(target_arch="x86_64",target_env="msvc"))]
    #[test]fn native_dismissal_waits_for_the_actual_command_result(){
        unsafe extern "C"{fn zs_menu_test_completion()->windows::core::HRESULT;}
        unsafe{zs_menu_test_completion()}.ok().unwrap();
    }
    #[cfg(all(target_arch="x86_64",target_env="msvc"))]
    #[test]fn native_lazy_submenus_initialize_nested_commands_without_invoking(){
        unsafe extern "C"{fn zs_menu_test_lazy_submenus()->windows::core::HRESULT;}
        unsafe{zs_menu_test_lazy_submenus()}.ok().unwrap();
    }
    #[cfg(all(target_arch="x86_64",target_env="msvc"))]
    #[test]fn native_nested_submenus_remain_owned_by_the_exact_parent_position(){
        unsafe extern "C"{fn zs_menu_test_submenu_ownership()->windows::core::HRESULT;}
        unsafe{zs_menu_test_submenu_ownership()}.ok().unwrap();
    }
    #[cfg(all(target_arch="x86_64",target_env="msvc"))]
    #[test]fn native_declared_submenu_failures_preserve_enumeration_and_generation_contracts()->Result<()>{
        unsafe extern "C"{fn zs_menu_test_enumeration_contract(passed:*mut u32)->windows::core::HRESULT;}
        let mut passed=0;unsafe{zs_menu_test_enumeration_contract(&mut passed)}.ok()?;assert_eq!(passed,0xff);Ok(())
    }
    use windows::{core::{implement,Error},Win32::UI::Shell::IContextMenu_Impl};
    #[cfg(all(target_arch="x86_64",target_env="msvc"))]
    #[test]fn native_commands_keep_authorization_bound_to_command_and_generation()->Result<()>{
        unsafe extern "C"{fn zs_menu_test_command_guard(passed:*mut u32)->windows::core::HRESULT;}
        let mut passed=0;unsafe{zs_menu_test_command_guard(&mut passed)}.ok()?;
        assert_eq!(passed,0x0f,"native command identity, one-shot dispatch, stale completion or reentrant cancellation failed");Ok(())
    }
    #[implement(IContextMenu)]
    struct FileActionProbe{verb:&'static str,invoked:Rc<Cell<usize>>}
    #[allow(non_snake_case)]impl IContextMenu_Impl for FileActionProbe_Impl{
        fn QueryContextMenu(&self,_:HMENU,_:u32,_:u32,_:u32,_:u32)->windows::core::HRESULT{windows::core::HRESULT(1)}
        fn InvokeCommand(&self,_:*const CMINVOKECOMMANDINFO)->Result<()>{self.invoked.set(self.invoked.get()+1);Ok(())}
        fn GetCommandString(&self,id:usize,_:u32,_:*const u32,text:PSTR,size:u32)->Result<()>{
            if id!=0{return Err(Error::from_hresult(windows::core::HRESULT(0x80070057u32 as i32)));}
            let verb:Vec<_>=self.verb.encode_utf16().chain(Some(0)).collect();
            if size<verb.len()as u32{return Err(Error::from_hresult(windows::core::HRESULT(0x8007007au32 as i32)));}
            unsafe{std::ptr::copy_nonoverlapping(verb.as_ptr(),text.0.cast(),verb.len());}Ok(())
        }
    }
    #[cfg(all(target_arch="x86_64",target_env="msvc"))]
    extern "C"{fn zs_menu_test_dispatch(context:*mut core::ffi::c_void,intercept:u32,shift:i32,
        before:Option<unsafe extern "system" fn(*mut core::ffi::c_void)->windows::core::HRESULT>,state:*mut core::ffi::c_void,out:*mut NativeMenuResult)->windows::core::HRESULT;}

    #[cfg(all(target_arch="x86_64",target_env="msvc"))]
    #[test]fn deferred_native_callback_dispatches_once_after_dismissal()->Result<()>{
        unsafe extern "C"{fn zs_menu_test_pending_callback(context:*mut core::ffi::c_void,
            before:Option<unsafe extern "system" fn(*mut core::ffi::c_void)->windows::core::HRESULT>,state:*mut core::ffi::c_void,late:i32,out:*mut NativeMenuResult)->windows::core::HRESULT;}
        unsafe extern "system" fn allow(raw:*mut core::ffi::c_void)->windows::core::HRESULT{
            let calls=unsafe{&*raw.cast::<Cell<usize>>()};calls.set(calls.get()+1);windows::core::HRESULT(0)
        }
        let invoked=Rc::new(Cell::new(0));let context:IContextMenu=FileActionProbe{verb:"third-party-tool",invoked:invoked.clone()}.into();
        let permitted=Cell::new(0usize);let mut outcome=NativeMenuResult::default();
        for late in [0,1]{
            unsafe{zs_menu_test_pending_callback(context.as_raw(),Some(allow),(&permitted as*const Cell<usize>).cast_mut().cast(),late,&mut outcome)}.ok()?;
            assert_eq!(permitted.get(),late as usize+1);assert_eq!(invoked.get(),late as usize+1);assert_eq!(outcome.kind,5);assert_eq!(outcome.command,1);assert_eq!(outcome.status,0);
        }Ok(())
    }

    #[test]fn file_action_interceptions_return_before_any_shell_invoke()->Result<()>{
        for (verb,shift,expected) in [
            ("copy",false,MenuOutcome::Copy),("Windows.Copy",false,MenuOutcome::Copy),
            ("cut",false,MenuOutcome::Cut),("Windows.Cut",false,MenuOutcome::Cut),
            ("paste",false,MenuOutcome::Paste),("Windows.Paste",false,MenuOutcome::Paste),
            ("delete",false,MenuOutcome::Delete{permanent:false}),("Windows.Delete",true,MenuOutcome::Delete{permanent:true})]{
            for enabled in [false,true]{
                let invoked=Rc::new(Cell::new(0));let context:IContextMenu=FileActionProbe{verb,invoked:invoked.clone()}.into();
                let menu=ShellMenu::new(context.clone())?;let before=Cell::new(0);
                let intercept=MenuInterceptions{file_actions:enabled,..Default::default()};
                let result=menu.dispatch_shell_command(HWND::default(),1,None,intercept,shift,false,&mut||{before.set(before.get()+1);Ok(())})?;
                assert_eq!(result,if enabled{expected}else{MenuOutcome::ShellInvoked});
                assert_eq!(invoked.get(),usize::from(!enabled));assert_eq!(before.get(),usize::from(!enabled));
                #[cfg(all(target_arch="x86_64",target_env="msvc"))]{
                    invoked.set(0);before.set(0);let mut callback=||{before.set(before.get()+1);Ok(())};
                    let mut observer=BeforeShell{callback:&mut callback};let mut native=NativeMenuResult::default();
                    unsafe{zs_menu_test_dispatch(context.as_raw(),intercept.bits(),i32::from(shift),Some(before_shell),(&mut observer as*mut BeforeShell<'_>).cast(),&mut native)}.ok()?;
                    assert_eq!(decode_native_result(native,&[])?,Some(if enabled{expected}else{MenuOutcome::ShellInvoked}));
                    assert_eq!(native.phase,if enabled{1}else{2});
                    assert_eq!(invoked.get(),usize::from(!enabled));assert_eq!(before.get(),usize::from(!enabled));
                }
            }
        }
        for verb in ["copyaspath","pastelink","copyto","move","custom.delete",""]{
            assert_eq!(intercepted_verb(Some(verb),MenuInterceptions{file_actions:true,..Default::default()},true),None);
        }Ok(())
    }
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
        assert_eq!(decode_native_result(NativeMenuResult { kind: 6, command: 0, status: 0, phase: 1 }, &applications).unwrap(), None);
        assert!(decode_native_result(NativeMenuResult { kind: 6, command: 19, status: 0, phase: 1 }, &applications).is_err());
        assert!(decode_native_result(NativeMenuResult { kind: 6, command: 0, status: 0, phase: 2 }, &applications).is_err());
    }
    #[test]
    fn only_requested_open_and_rename_interceptions_defer_shell_execution() {
        let intercept = MenuInterceptions { open: true, rename: true, ..Default::default() };
        assert_eq!(intercepted_verb(Some("ReName"), intercept,false), Some(MenuOutcome::Rename));
        assert_eq!(intercepted_verb(Some("open"), intercept,false), Some(MenuOutcome::Open));
        assert_eq!(intercepted_verb(Some("explore"), intercept,false), Some(MenuOutcome::Explore));
        for verb in [Some("copy"), Some("delete"), Some("openas"), None] { assert_eq!(intercepted_verb(verb, intercept,false), None); }
        assert_eq!(intercepted_verb(Some("open"), MenuInterceptions::default(),false), None);
    }
    #[test]
    fn modern_policy_covers_visible_functions_and_keeps_extensions_independent() {
        let mut options = ContextMenuOptions::default();
        assert_eq!(modern_hidden_features(&options), 0);
        options.hidden_verbs = ["cut", "copy", "paste", "pastelink", "copyaspath",
            "rename", "delete", "properties", "windows.modernshare", "new", "custom-third-party"]
            .map(str::to_string).to_vec();
        options.show_seven_zip = false;
        options.show_wps = false;
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
        let mut options = ContextMenuOptions { show_seven_zip:false, show_wps:false, ..Default::default() };
        assert!(should_hide(None,"&7-Zip",&options));
        assert!(should_hide(None,"WPS 文件压缩",&options));
        assert!(!should_hide(None,"压缩为 ZIP 文件",&options));
        options.show_seven_zip=true;options.show_wps=true;
        assert!(!should_hide(None,"7-Zip",&options));
        assert!(!should_hide(None,"WPS 文件压缩",&options));
    }
}

impl Drop for ShellMenu {
    fn drop(&mut self) { if self.owns_menu.get(){let _ = unsafe { DestroyMenu(self.hmenu) };} }
}

unsafe fn ui_object<T: Interface>(parent: &IShellFolder, owner: HWND, children: &[*const ITEMIDLIST]) -> Result<T> {
    unsafe { parent.GetUIObjectOf::<T>(owner, children, None) }
}

/// Sibling UI objects, or a complete cross-folder IDataObject for drag and drop.
pub fn items_ui_object<T: Interface>(owner: HWND, pidls: &[Vec<u8>]) -> Result<T> {
    let offsets = item_offsets(pidls)?;
    let bind_data_object=||{
        let pointers:Vec<_>=pidls.iter().map(|pidl|pidl.as_ptr().cast()).collect();
        let array=unsafe{SHCreateShellItemArrayFromIDLists(&pointers)?};
        unsafe{array.BindToHandler(None,&BHID_DataObject)}
    };
    if !same_item_parent(pidls, &offsets) {
        if T::IID == IDataObject::IID {
            return bind_data_object();
        }
        return Err(windows::core::Error::from_hresult(windows::core::HRESULT(0x80004002u32 as i32)));
    }
    let first = &pidls[0];
    let parent: IShellFolder = unsafe { SHBindToParent(first.as_ptr().cast(), None)? };
    let children: Vec<*const ITEMIDLIST> = pidls.iter().zip(offsets).map(|(pidl, offset)| unsafe { pidl.as_ptr().add(offset).cast() }).collect();
    match unsafe{ui_object(&parent,owner,&children)}{
        Err(err) if T::IID==IDataObject::IID && matches!(err.code().0 as u32,0x80004002|0x80004001)=>bind_data_object(),
        result=>result,
    }
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

#[cfg(test)]mod presentation_tests{
    use super::*;
    #[test]fn file_dialog_commands_require_host_file_actions(){
        let enabled=MenuInterceptions{file_actions:true,..Default::default()};
        for (verb,outcome) in [("Windows.ModernShare",MenuOutcome::Share),("openas",MenuOutcome::ChooseApplication)] {
            assert_eq!(intercepted_verb(Some(verb),enabled,false),Some(outcome));
            assert_eq!(intercepted_verb(Some(verb),MenuInterceptions::default(),false),None);
        }
        assert_eq!(intercepted_verb(Some("sharewith"),enabled,false),None);
    }
    #[test]fn menu_style_routes_by_platform_capability_not_one_machine(){
        for build in [17763,19041,19045]{assert_eq!(presentation(true,false,build,false),(false,false));}
        for build in [22000,22621,26100,26300,30000]{
            assert_eq!(presentation(true,false,build,false),(true,true));
            assert_eq!(presentation(true,false,build,true),(true,true));
            assert_eq!(presentation(false,false,build,true),(false,false));
        }
        assert_eq!(presentation(true,true,19045,false),(false,false));
        assert_eq!(presentation(true,false,0,false),(true,true));
    }
}
