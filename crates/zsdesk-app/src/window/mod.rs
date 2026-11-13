//! Windows 11 style Explorer window.
//!
//! The title bar is replaced by the tab strip: `WM_NCCALCSIZE` removes the
//! caption while the DWM frame retains system caption-button metrics, Snap
//! Layouts and Mica. Measured button bounds are routed as native nonclient hits
//! before custom frame hit testing. The remaining chrome is painted with
//! Direct2D using ZSUI Fluent tokens; the folder view and controls stay native.

mod bookmarks;
mod geometry;
mod window_actions;
mod input;
mod keyboard;
mod routing_settings;
mod navigation;
mod address_input;
mod terminal;
mod quick_preview;
mod quicklook;
pub(crate) mod preview_process;
mod wps_preview;
mod preview_handlers;
mod hover_preview;
mod paint;
mod global_search;
mod search_panel;
mod integrated_search;
mod startup_settings;
mod context_menu;
#[cfg(test)]mod context_menu_regression;
mod listary;
mod mouse_navigation;
mod dual_pane;
mod pane_input;
mod mouse_gestures;
mod wheel_input;
mod folder_sizes;
mod view_memory;
mod shell_registration;
mod deferred_selection;
pub(crate) mod diagnostics;
#[cfg(test)]mod integration_navigation_regression;
#[cfg(test)]
mod selection_regression;
#[cfg(test)]
mod thumbnail_regression;
#[cfg(test)]
mod geometry_regression;
#[cfg(test)]
mod interaction_regression;
#[cfg(test)]
mod search_regression;
#[cfg(test)]
mod everything_regression;

use std::{cell::{Cell, RefCell}, collections::HashMap, mem::size_of, path::PathBuf, rc::Rc};
use anyhow::{Context, Result};
use windows::{core::{PCWSTR, PWSTR}, Win32::{
    Foundation::{COLORREF, HWND, HINSTANCE, LPARAM, LRESULT, WPARAM, POINT, RECT},
    Graphics::{Dwm::DwmDefWindowProc, Gdi::{BeginPaint, BitBlt, CreateFontIndirectW, CreateSolidBrush, DeleteObject, EndPaint,
        InvalidateRect, ScreenToClient, ClientToScreen, SetBkColor, SetTextColor, HBRUSH, HDC, HFONT, PAINTSTRUCT, SRCCOPY,
        MonitorFromRect, MONITOR_DEFAULTTONULL}},
    System::{DataExchange::COPYDATASTRUCT, LibraryLoader::GetModuleHandleW, Ole::{OleInitialize, OleUninitialize, DROPEFFECT, DROPEFFECT_LINK, DROPEFFECT_NONE}},
    UI::{Controls::*, HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi, SystemParametersInfoForDpi},
        Input::KeyboardAndMouse::*, Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::*},
}};
use zsdesk_core::model::explorer::{ExplorerSession, ExplorerTab, ShellLocation, WindowPlacement};
use zsdesk_ui::{chrome::{scale, ChromeInput, ChromeLayout, Hit, Rect as LayoutRect, STRIP_H}, theme::{Mode, Palette}};
use zsdesk_windows::{gfx::{Gfx, Surface}, ole::{has_hdrop, hdrop_paths, DropHandler, DropRegistration},
    shell::{acknowledge_selection_changed, commands::ViewStatus, Crumb, ExplorerBrowserHost, NamespaceTreeHost, Panes,
        WM_BOOKMARK_PATH_DROPPED, WM_SHELL_FOLDER_INVOKED, WM_SHELL_NAVIGATED, WM_SHELL_NAVIGATION_FAILED,
        WM_SHELL_SELECTION_CHANGED, WM_TREE_NAVIGATE}, theme};
use crate::{commands::Command, storage};
use navigation::NavigationRequest;

pub(super) const ID_ADDRESS: usize = 1001;
pub(super) const ID_BOOKMARK_RENAME: usize = 1002;
pub(super) const ID_SEARCH: usize = 1007;
pub(super) const ACTION_SEARCH: usize = 2109;
pub(super) const ACTION_COMMIT_ADDRESS: usize = 2101;
pub(super) const ACTION_CANCEL_ADDRESS: usize = 2102;
pub(super) const ACTION_COMMIT_RENAME: usize = 2103;
pub(super) const ACTION_CANCEL_RENAME: usize = 2104;
pub(super) const ACTION_END_ADDRESS_EDIT: usize = 2110;
pub(super) const ACTION_FOCUS_VIEW: usize = 2111;
pub(super) const ACTION_SEARCH_SETTINGS:usize=2112;
pub(super) const ACTION_SEARCH_PAGE:usize=2113;
pub(super) const ACTION_CLOSE_SEARCH:usize=2114;
const WM_LAYOUT: u32 = WM_APP + 2;
pub(super) const WM_ACTION: u32 = WM_APP + 3;
const WM_OPEN_REQUEST: u32 = WM_APP + 4;
const TIMER_STATUS: usize = 1;
const TIMER_MESSAGE: usize = 2;
const TIMER_SEARCH: usize = 3;
const TIMER_FAST_SEARCH:usize=4;
const TIMER_PLACEMENT:usize=9;
const WM_FAST_SEARCH:u32=WM_APP+43;
const WM_DIAGNOSTIC:u32=WM_APP+44;
type Slot = RefCell<Option<WindowState>>;
#[derive(Clone,Copy,PartialEq,Eq)]
enum BrowserRole{Fresh,Ordinary,Results}

thread_local! {
    static DEFERRED_MESSAGES: RefCell<std::collections::VecDeque<(HWND,u32,WPARAM,LPARAM)>> = const { RefCell::new(std::collections::VecDeque::new()) };
}

const EDIT_COLORS_PROPERTY:PCWSTR=windows::core::w!("ZSDesk.EditColors");
const DEFERRED_PAINT_PROPERTY:PCWSTR=windows::core::w!("ZSDesk.DeferredPaint");
#[derive(Clone,Copy)]
struct EditColors{normal:isize,focused:isize,background:u32,focus_background:u32,text:u32}
struct EditColorBrushes(EditColors);
impl Drop for EditColorBrushes{fn drop(&mut self){
    for brush in [self.0.normal,self.0.focused]{if brush!=0{let _=unsafe{DeleteObject(HBRUSH(brush as*mut _).into())};}}
}}
fn edit_colors(hwnd:HWND)->Option<EditColors>{
    let pointer=unsafe{GetPropW(hwnd,EDIT_COLORS_PROPERTY)}.0 as*const EditColorBrushes;
    (!pointer.is_null()).then(||unsafe{(*pointer).0})
}
fn replace_edit_colors(hwnd:HWND,background:u32,focus_background:u32,text:u32)->bool{
    let normal=unsafe{CreateSolidBrush(COLORREF(background))};let focused=unsafe{CreateSolidBrush(COLORREF(focus_background))};
    let brushes=Box::new(EditColorBrushes(EditColors{normal:normal.0 as isize,focused:focused.0 as isize,background,focus_background,text}));
    if normal.0.is_null()||focused.0.is_null(){return false;}
    let old=unsafe{GetPropW(hwnd,EDIT_COLORS_PROPERTY)}.0 as*mut EditColorBrushes;
    let pointer=Box::into_raw(brushes);
    if unsafe{SetPropW(hwnd,EDIT_COLORS_PROPERTY,Some(windows::Win32::Foundation::HANDLE(pointer.cast())))}.is_err(){
        unsafe{drop(Box::from_raw(pointer));}return false;
    }
    if !old.is_null(){unsafe{drop(Box::from_raw(old));}}
    true
}
fn take_deferred_paint(hwnd:HWND)->bool{
    unsafe{RemovePropW(hwnd,DEFERRED_PAINT_PROPERTY)}.is_ok_and(|value|!value.0.is_null())
}
fn release_window_properties(hwnd:HWND){
    if let Ok(value)=unsafe{RemovePropW(hwnd,EDIT_COLORS_PROPERTY)}{
        let pointer=value.0 as*mut EditColorBrushes;if !pointer.is_null(){unsafe{drop(Box::from_raw(pointer));}}
    }
    let _=take_deferred_paint(hwnd);
    for property in [windows::core::w!("ZSDesk.NativeCaption"),windows::core::w!("ZSDesk.CaptionVisual"),windows::core::w!("ZSDesk.CaptionPress")]{let _=unsafe{RemovePropW(hwnd,property)};}
}

pub fn run(session: ExplorerSession, config_path: PathBuf, selection: Option<std::ffi::OsString>, modern: bool, secondary_window: bool,focus_search:Option<bool>) -> Result<()> {
    crate::runtime::trace("window-ole-init");
    unsafe { OleInitialize(None).context("OleInitialize failed")?; }
    let result = unsafe { run_inner(session, config_path, selection, modern, secondary_window,focus_search) };
    crate::runtime::trace("window-loop-return");
    unsafe { OleUninitialize(); }
    crate::runtime::trace("window-ole-uninit-return");
    result
}

unsafe fn run_inner(session: ExplorerSession, config_path: PathBuf, selection: Option<std::ffi::OsString>, modern: bool, secondary_window: bool,focus_search:Option<bool>) -> Result<()> {
    unsafe { InitCommonControlsEx(&INITCOMMONCONTROLSEX { dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32, dwICC: ICC_BAR_CLASSES | ICC_STANDARD_CLASSES }) }.ok()?;
    let instance = HINSTANCE(unsafe { GetModuleHandleW(None)? }.0);
    let class = wide("ZSDesk.Native.Explorer");
    let wc = WNDCLASSW {
        style: CS_DBLCLKS,
        lpfnWndProc: Some(window_proc), hInstance: instance,
        hIcon: unsafe { LoadIconW(Some(instance), PCWSTR(101usize as *const u16))? },
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW)? }, hbrBackground: HBRUSH::default(),
        lpszClassName: pcwstr(&class), ..Default::default()
    };
    unsafe { RegisterClassW(&wc) };
    let placement = session.ui.window;
    let relaunch = relaunch_command(&config_path);
    // The slot outlives the window and every callback that can reach it.
    let mut state = WindowState::new(session, config_path, selection, modern);
    state.secondary_window = secondary_window;
    let slot = Box::new(RefCell::new(Some(state)));
    let (x, y, w, h) = initial_rect(placement);
    let hwnd = unsafe { CreateWindowExW(WINDOW_EX_STYLE::default(), pcwstr(&class), PCWSTR(wide("ZSDesk").as_ptr()),
        WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN, x, y, w, h, None, None, Some(instance), Some((&*slot as *const Slot).cast()))? };
    // Apply the custom frame before anything is painted.
    let _ = unsafe { SetWindowPos(hwnd, None, 0, 0, 0, 0, SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE) };
    let created = slot.borrow_mut().as_mut().map(|s| unsafe { s.create(hwnd) });
    if let Some(Err(error)) = created {
        let text = wide(&format!("无法创建文件管理窗口：{error:#}"));
        unsafe { MessageBoxW(Some(hwnd), pcwstr(&text), PCWSTR(wide("ZSDesk").as_ptr()), MB_OK | MB_ICONERROR); }
        let _ = unsafe { DestroyWindow(hwnd) };
        return Err(error);
    }
    flush_deferred(hwnd,true);
    // Before the first show, so the taskbar button is created in the pin's group.
    if let Some((command,icon))=&relaunch{
        let _=zsdesk_windows::taskbar::set_identity(hwnd,&zsdesk_windows::taskbar::Identity{app_id:crate::edition::application_id(),
            relaunch_command:command,display_name:"ZSDesk",icon});
    }
    let accelerators = unsafe { CreateAcceleratorTableW(&accelerator_table())? };
    let _ = unsafe { ShowWindow(hwnd, if placement.is_some_and(|p| p.maximized) { SW_SHOWMAXIMIZED } else { SW_SHOW }) };
    if let Some(settings)=focus_search{post(hwnd,WM_ACTION,if settings{ACTION_SEARCH_SETTINGS}else{Command::FocusSearch.id()as usize},0);}
    let mut msg = MSG::default();
    let mut quick_preview:Option<preview_process::PreviewProcess>=None;
    let mut pane_preview:Option<preview_process::PreviewProcess>=None;
    preview_process::sync_pane(hwnd,&slot,&mut pane_preview);
    let mut hover_preview=hover_preview::HoverPreview::new(hwnd);
    let mut middle_click=hover_preview::MiddleClick::new(hwnd);
    let mut gestures=mouse_gestures::MouseGestures::default();
    loop {
        let result = unsafe { GetMessageW(&mut msg, None, 0, 0) }.0;
        if result <= 0 { break; }
        if diagnostics::dispatch(&msg){continue;}
        if address_input::pre_translate(&msg){continue;}
        dual_pane::activate_for_message(&slot,&msg);
        if gestures.process(hwnd,&msg,&slot) { hover_preview.dismiss(&mut quick_preview);continue; }
        if mouse_navigation::pre_translate(hwnd,&msg){continue;}
        if msg.hwnd==hwnd && msg.message==WM_TIMER && msg.wParam.0==preview_process::TIMER {
            if let Some(dark)=slot.try_borrow().ok().and_then(|slot|slot.as_ref().map(|state|state.dark)){
                for preview in [quick_preview.as_mut(),pane_preview.as_mut()].into_iter().flatten(){let _=preview.set_dark(dark);}
            }
            for error in preview_process::poll_previews(&mut quick_preview,&mut pane_preview){
                if let Ok(mut slot)=slot.try_borrow_mut(){if let Some(state)=slot.as_mut(){state.show_message(error);}}
            }
            preview_process::sync_pane(hwnd,&slot,&mut pane_preview);
            if !preview_process::needs_timer(&quick_preview,&pane_preview){let _=unsafe{KillTimer(Some(hwnd),preview_process::TIMER)};}
            continue;
        }
        if middle_click.process(&msg,&slot){hover_preview.dismiss(&mut quick_preview);continue;}
        if hover_preview.process(&msg,&slot,&mut quick_preview) {continue;}
        if let Some(preview)=quick_preview.as_mut().filter(|p|p.owns_window(msg.hwnd)){
            if !preview.pre_translate(&msg){unsafe{let _=TranslateMessage(&msg);DispatchMessageW(&msg);}}
            continue;
        }
        if msg.message == WM_MOUSEWHEEL {
            let handled = slot.try_borrow_mut().ok().and_then(|mut slot| slot.as_mut().map(|state| state.wheel_zoom(&msg))).unwrap_or(false);
            if handled { continue; }
        }
        let plain_key=msg.message==WM_KEYDOWN && ![VK_CONTROL,VK_SHIFT,VK_MENU].iter().any(|k|unsafe{GetKeyState(k.0 as i32)}<0);
        let enabled=slot.try_borrow().ok().and_then(|s|s.as_ref().map(|s|s.session.ui.space_preview)).unwrap_or(false);
        let preview_open=quick_preview.as_ref().is_some_and(|p|p.is_open());
        if !enabled&&!preview_open&&quicklook::key(&msg,&slot){continue;}
        if (enabled||preview_open)&&plain_key && !text_input_window(msg.hwnd) && (msg.wParam.0==VK_SPACE.0 as usize || (msg.wParam.0==VK_ESCAPE.0 as usize && preview_open)){
            if msg.lParam.0 as usize & (1usize<<30)!=0 { continue; }
            if quick_preview.as_ref().is_some_and(|p|p.is_open()){hover_preview.dismiss(&mut quick_preview);quick_preview=None;continue;}
            if enabled && msg.wParam.0==VK_SPACE.0 as usize {
                let request=preview_process::selected_preview_request(&slot);
                if let Some((path,provider,dark))=request {
                    match preview_process::PreviewProcess::open_with_options(hwnd,&path,provider,dark,false){Ok(preview)=>quick_preview=Some(preview),Err(error)=>{if let Ok(mut s)=slot.try_borrow_mut(){if let Some(s)=s.as_mut(){s.show_message(format!("无法预览：{error}"));}}}}
                    continue;
                }
            }
        }
        let editing = text_input_window(msg.hwnd) || text_input_window(unsafe { GetFocus() });
        if plain_key && msg.wParam.0 == VK_ESCAPE.0 as usize && !editing && window_actions::fullscreen(hwnd) {
            post(hwnd, WM_ACTION, Command::ToggleFullscreen.id() as usize, 0); continue;
        }
        let modifiers = keyboard::Modifiers { control: unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0,
            shift: unsafe { GetKeyState(VK_SHIFT.0 as i32) } < 0, alt: unsafe { GetKeyState(VK_MENU.0 as i32) } < 0 };
        if msg.message == WM_KEYDOWN || msg.message == WM_SYSKEYDOWN {
            if let Some(action) = keyboard::resolve(msg.wParam.0 as u16, modifiers, true, editing) {
                let view = slot.try_borrow().ok().and_then(|s| s.as_ref().and_then(|s| s.keyboard_view(&msg, true)));
                if let Some(view) = view {
                    // Shell verbs can enter COM/modal loops: release the state borrow first.
                    let outcome = keyboard::execute_message(&view, hwnd, action, &msg);
                    if let Ok(mut state) = slot.try_borrow_mut() { if let Some(state) = state.as_mut() { state.shell_result(Some(outcome)); } }
                    continue;
                }
            }
        }
        let file_only_accelerator = modifiers.control && !modifiers.alt &&
            ((modifiers.shift && ([VK_C.0 as usize, VK_N.0 as usize].contains(&msg.wParam.0)
                || (VK_1.0 as usize..=VK_8.0 as usize).contains(&msg.wParam.0)))
            || [VK_OEM_PLUS.0 as usize, VK_OEM_MINUS.0 as usize, VK_0.0 as usize].contains(&msg.wParam.0));
        if !(editing && file_only_accelerator) && unsafe { TranslateAcceleratorW(hwnd, accelerators, &msg) } != 0 { continue; }
        if (WM_KEYFIRST..=WM_KEYLAST).contains(&msg.message) && !editing {
            let view = slot.try_borrow().ok().and_then(|s| s.as_ref().and_then(|s| s.keyboard_view(&msg, false)));
            if let Some(view) = view {
                if unsafe { view.TranslateAccelerator(&msg) } == windows::core::HRESULT(0) { continue; }
                if msg.message == WM_KEYDOWN && msg.wParam.0 == VK_BACK.0 as usize && !modifiers.control && !modifiers.shift && !modifiers.alt {
                    post(hwnd, WM_ACTION, Command::Back.id() as usize, 0); continue;
                }
            }
        }
        unsafe { let _ = TranslateMessage(&msg); DispatchMessageW(&msg); }
        if [WM_ACTION,WM_LAYOUT,WM_SIZE,WM_SHELL_SELECTION_CHANGED,WM_SHELL_NAVIGATED].contains(&msg.message){
            preview_process::sync_pane(hwnd,&slot,&mut pane_preview);
        }
        if msg.message==WM_SHELL_SELECTION_CHANGED {
            if let Some(preview)=quick_preview.as_mut().filter(|p|p.is_open() && !p.is_hover()){
                let path=preview_process::selected_preview_request(&slot).map(|(path,_,_)|path);
                if let Some(path)=path{let _=preview.update(&path);}
            }
        }
    }
    drop(hover_preview);
    drop(quick_preview);
    drop(pane_preview);
    let _ = unsafe { DestroyAcceleratorTable(accelerators) };
    // Posted payloads still own their allocation even after the receiver is destroyed.
    for message in [WM_SHELL_NAVIGATED, WM_SHELL_FOLDER_INVOKED, WM_BOOKMARK_PATH_DROPPED, WM_OPEN_REQUEST, WM_TREE_NAVIGATE, WM_SHELL_SELECTION_CHANGED,crate::routing_control::WM_RESULT,WM_DIAGNOSTIC] {
        while unsafe { PeekMessageW(&mut msg, Some(hwnd), message, message, PM_REMOVE) }.as_bool() {
            if msg.lParam.0 == 0 { continue; }
            unsafe {
                match message {
                    WM_BOOKMARK_PATH_DROPPED => drop(Box::from_raw(msg.lParam.0 as *mut PathBuf)),
                    WM_OPEN_REQUEST => drop(Box::from_raw(msg.lParam.0 as *mut crate::instance::OpenRequest)),
                    WM_SHELL_SELECTION_CHANGED => acknowledge_selection_changed(msg.lParam.0),
                    crate::routing_control::WM_RESULT => drop(Box::from_raw(msg.lParam.0 as *mut String)),
                    WM_DIAGNOSTIC=>drop(Box::from_raw(msg.lParam.0 as *mut crate::instance::DiagnosticRequest)),
                    _ => drop(Box::from_raw(msg.lParam.0 as *mut ShellLocation)),
                }
            }
        }
    }
    Ok(())
}

fn initial_rect(placement: Option<WindowPlacement>) -> (i32, i32, i32, i32) {
    if let Some(p) = placement {
        let rect = RECT { left: p.x, top: p.y, right: p.x + p.width, bottom: p.y + p.height };
        if p.width >= 480 && p.height >= 320 && !unsafe { MonitorFromRect(&rect, MONITOR_DEFAULTTONULL) }.is_invalid() {
            return (p.x, p.y, p.width, p.height);
        }
    }
    let dpi = unsafe { windows::Win32::UI::HiDpi::GetDpiForSystem() };
    (CW_USEDEFAULT, CW_USEDEFAULT, scale(1180, dpi), scale(760, dpi))
}

fn is_text_box(hwnd: HWND) -> bool {
    [ID_ADDRESS as i32, ID_BOOKMARK_RENAME as i32, ID_SEARCH as i32].contains(&unsafe { GetDlgCtrlID(hwnd) })
}
fn text_input_window(hwnd:HWND)->bool{
    if is_text_box(hwnd){return true;}
    let mut name=[0u16;128];let length=unsafe{GetClassNameW(hwnd,&mut name)}.max(0)as usize;
    String::from_utf16_lossy(&name[..length]).to_ascii_lowercase().contains("edit")
}

/// A breadcrumb hit target inside the address box.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CrumbPart { Crumb(usize), Chevron(usize), Overflow }

#[derive(Clone, Copy, Debug)]
pub(super) struct CrumbSlot { pub part: CrumbPart, pub rect: LayoutRect }

#[derive(Clone, Copy, Debug)]
pub(super) struct TabDrag { pub index: usize, pub start_axis: i32, pub moved: bool }
#[derive(Clone,Copy)]
pub(super) struct BookmarkDrag { pub id:u64,pub start:POINT,pub point:POINT,pub moved:bool }

/// Drop zones of the chrome shared with the OLE drop target.
#[derive(Clone, Copy, Default)]
pub(super) struct DropZones { pub bookmarks: LayoutRect, pub strip: LayoutRect, pub tabs:LayoutRect, pub identity:Option<pane_input::Identity> }

struct WindowState {
    search_panel:search_panel::SearchPanel,
    hwnd: HWND,
    dpi: u32,
    session: ExplorerSession,
    config_path: PathBuf,
    search_saved_preferences:zsdesk_core::model::explorer::SearchPreferences,
    bookmarks_saved:Vec<zsdesk_core::model::explorer::Bookmark>,
    session_ending:bool,
    shell_registry: Option<crate::shell_window_registry::Registry>,
    shell_registration_warning: bool,
    modern: bool,
    browser: Option<ExplorerBrowserHost>,
    browser_source: usize,
    folder_sizes: folder_sizes::FolderSizes,
    view_hwnd: HWND,
    navigation_epoch: u64,
    other_pane: dual_pane::InactivePane,
    active_pane_right: bool,
    background_pane: bool,
    routing_pane_message: bool,
    pane_splitter_drag: Option<(i32,u16)>,
    tree: Option<NamespaceTreeHost>,
    address_edit: HWND,
    search_edit: HWND,
    rename_hwnd: HWND,
    rename_id: Option<u64>,
    tooltip: HWND,
    tooltip_text: Vec<u16>,
    font: HFONT,
    gfx: Option<Gfx>,
    surface: Option<Surface>,
    palette: Palette,
    dark: bool,
    mica: bool,
    active: bool,
    layout: ChromeLayout,
    crumbs: Vec<Crumb>,
    crumb_slots: Vec<CrumbSlot>,
    crumbs_for: Option<ShellLocation>,
    address_editing: bool,
    open_chevron: Option<CrumbPart>,
    hover: Hit,
    hover_crumb: Option<CrumbPart>,
    pressed: Option<Hit>,
    tracking_mouse: bool,
    tab_drag: Option<TabDrag>,
    bookmark_drag:Option<BookmarkDrag>,
    splitter_drag: Option<(i32, i32)>,
    icons: HashMap<String, paint::IconEntry>,
    view_status: ViewStatus,
    view_mode: (i32, i32),
    bookmark_widths: Vec<i32>,
    bookmark_order:Vec<usize>,
    drop_registration: Option<DropRegistration>,
    drop_zones: Rc<Cell<DropZones>>,
    last_focus: HWND,
    pending: Option<NavigationRequest>,
    queued: Option<NavigationRequest>,
    shown_tab: Option<u64>,
    closed_tabs: Vec<ExplorerTab>,
    status: String,
    launch_selection: Option<std::ffi::OsString>,
    launch_selections: Vec<std::ffi::OsString>,
    deferred_selection: Option<deferred_selection::Pending>,
    open_requests: std::collections::VecDeque<crate::instance::OpenRequest>,
    /// Folder the current search started from, and the results location it produced.
    search_scope: Option<ShellLocation>,
    search_result: Option<ShellLocation>,
    browser_role:BrowserRole,
    fast_search:Option<crate::fast_search::FastSearch>,
    fast_generation:u64,
    fast_submitted:Option<integrated_search::SubmittedSearch>,
    fast_found:usize,
    search_progress:Option<String>,
    fast_events:std::collections::VecDeque<crate::fast_search::SearchEvent>,
    fast_reset_pending:bool,
    fast_clearing:Option<std::time::Instant>,
    fast_restore:Option<global_search::SelectionRestore>,
    search_settings_job:Option<integrated_search::SettingsJob>,
    fullscreen_restore: Option<window_actions::FullscreenRestore>,
    wheel_remainder: i32,
    secondary_window: bool,
    preview_rect: Option<LayoutRect>,
}

impl WindowState {
    fn new(mut session: ExplorerSession, config_path: PathBuf, launch_selection: Option<std::ffi::OsString>, modern: bool) -> Self {
        if !modern{session.ui.context_menu.mode=zsdesk_core::model::explorer::ContextMenuMode::Original;}
        let modern=crate::context_preferences::modern(&session.ui.context_menu);
        Self {
            search_panel:search_panel::SearchPanel::default(),
            hwnd: HWND::default(), dpi: 96, search_saved_preferences:session.ui.search_preferences(),bookmarks_saved:session.bookmarks.clone(),session_ending:false,session, config_path, modern, browser: None, tree: None,
            browser_source:0,folder_sizes:Default::default(),view_hwnd:HWND::default(),navigation_epoch:0,
            other_pane:Default::default(),active_pane_right:false,background_pane:false,routing_pane_message:false,pane_splitter_drag:None,
            address_edit: HWND::default(), search_edit: HWND::default(), rename_hwnd: HWND::default(), rename_id: None,
            tooltip: HWND::default(), tooltip_text: vec![0], font: HFONT::default(), gfx: None, surface: None,
            palette: Palette::light(), dark: false, mica: false, active: true, layout: ChromeLayout::default(),
            crumbs: Vec::new(), crumb_slots: Vec::new(), crumbs_for: None, address_editing: false, open_chevron: None,
            hover: Hit::None, hover_crumb: None, pressed: None, tracking_mouse: false, tab_drag: None,bookmark_drag:None, splitter_drag: None,
            icons: HashMap::new(), view_status: ViewStatus::default(), view_mode: (0, 0), bookmark_widths: Vec::new(),bookmark_order:Vec::new(),
            drop_registration: None, drop_zones: Rc::new(Cell::new(DropZones::default())), last_focus: HWND::default(),
            pending: None, queued: None, shown_tab: None, closed_tabs: Vec::new(), status: String::new(),
            launch_selection, launch_selections:Vec::new(), deferred_selection:None, open_requests: Default::default(), search_scope: None, search_result: None,
            browser_role:BrowserRole::Fresh,fast_search:None,fast_generation:0,fast_submitted:None,fast_found:0,search_progress:None,fast_events:Default::default(),
            fast_reset_pending:false,fast_clearing:None,fast_restore:None,
            search_settings_job:None,
            fullscreen_restore:None,wheel_remainder:0,
            secondary_window:false,
            preview_rect:None,
            shell_registry:None,
            shell_registration_warning:false,
        }
    }

    unsafe fn create(&mut self, hwnd: HWND) -> Result<()> {
        diagnostics::initialize();diagnostics::record("window","create",None);
        zsdesk_windows::shell::set_open_archives_externally(self.session.ui.archives_open_externally);
        self.hwnd = hwnd;
        self.dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
        listary::initialize(hwnd)?;
        context_menu::install();
        zsdesk_windows::shell::set_folder_open_capture(Some(pane_input::capture_folder_open));
        let key = crate::instance::property(&self.config_path);
        unsafe { SetPropW(hwnd, PCWSTR(key.as_ptr()), Some(windows::Win32::Foundation::HANDLE(1usize as *mut _)))?; }
        let route_key = crate::routing_control::profile_property(&self.config_path);
        unsafe { SetPropW(hwnd, PCWSTR(route_key.as_ptr()), Some(windows::Win32::Foundation::HANDLE(1usize as *mut _)))?; }
        unsafe { SetPropW(hwnd, windows::core::w!("ZSDesk.Activate.v1"), Some(windows::Win32::Foundation::HANDLE(1usize as *mut _)))?; }
        let preferred_show=if self.session.ui.window.is_some_and(|p|p.maximized){SW_SHOWMAXIMIZED.0}else{SW_SHOWNORMAL.0};
        unsafe{SetPropW(hwnd,windows::core::w!("ZSDesk.PreferredShow"),Some(windows::Win32::Foundation::HANDLE(preferred_show as usize as *mut _)))?;}
        let (family, _) = zsdesk_windows::gfx::system_ui_font(Some(hwnd));
        self.gfx = Some(Gfx::new(&family).context("Direct2D 初始化失败")?);
        // Menus are drawn, not created by Win32: prepare them before first use.
        zsdesk_windows::flyout::prepare();
        self.address_edit = unsafe { child(hwnd, "EDIT", "", ID_ADDRESS, ES_AUTOHSCROLL as u32, false)? };
        self.search_edit = unsafe { child(hwnd, "EDIT", "", ID_SEARCH, ES_AUTOHSCROLL as u32, true)? };
        for (edit, id) in [(self.address_edit, ID_ADDRESS), (self.search_edit, ID_SEARCH)] {
            unsafe { SetWindowSubclass(edit, Some(edit_proc), id, hwnd.0 as usize) }.ok()?;
        }
        self.tooltip = unsafe { create_tooltip(hwnd) };
        self.apply_theme();
        unsafe { self.update_font(); }
        let handler = Rc::new(ChromeDrop { hwnd: hwnd.0 as isize, zones: self.drop_zones.clone(), accepted: Cell::new(false) });
        self.drop_registration = unsafe { DropRegistration::register(hwnd, handler) }.ok();
        self.measure_bookmarks();
        self.relayout();
        let browser = self.create_browser()?;
        self.browser = Some(browser);
        if let Some(browser)=&self.browser{browser.set_context_menu_options(crate::context_preferences::native(&self.session.ui.context_menu));}
        self.tree = unsafe { NamespaceTreeHost::create_with_options(hwnd, win_rect(self.layout.tree), self.current_location(),self.navigation_tree_options()) }.ok();
        self.theme_children();
        self.position_children();
        unsafe { SetTimer(Some(hwnd), TIMER_STATUS, 1200, None); }
        unsafe { self.navigate_current(); }
        if self.session.ui.dual_pane { self.ensure_dual_pane(); }
        #[cfg(not(test))]
        {
            self.check_integration();
            if let Err(error) = crate::routing_control::refresh_owned_target(&self.config_path, hwnd) {
                self.show_message(format!("文件位置绑定检查失败：{error}"));
            }
            if let Err(error) = crate::quick_search::start_if_enabled(&self.config_path) {
                self.show_message(format!("后台搜索启动失败：{error:#}"));
            }
        }
        self.update_shell_registry();
        Ok(())
    }

    /// Integration health check: if the registered executable no longer exists
    /// (moved or upgraded to another folder), point the verbs at this copy.
    fn check_integration(&mut self) {
        use zsdesk_windows::integration::{self, Integration};
        let level = integration::current();
        if level == Integration::Off { return; }
        if level==Integration::Default {
            if let Ok(exe)=std::env::current_exe(){
                if let Err(error)=integration::upgrade_default(&exe,&self.config_path.with_file_name("takeover-backup.json")) {
                    self.show_message(format!("系统打开接管尚未更新：{error}"));
                }
            }
        }
        let Some(command) = integration::registered_command() else { return };
        let registered = command.trim_start_matches('"').split('"').next().unwrap_or_default().to_string();
        if std::path::Path::new(&registered).exists() { return; }
        if let Ok(exe) = std::env::current_exe() {
            let backup = self.config_path.with_file_name("takeover-backup.json");
            if integration::apply(level, &exe, &backup).is_err() { self.show_message("系统集成需要修复：请在 ≡ → 系统集成 中重新选择".into()); }
        }
    }

    fn current_location(&self) -> &ShellLocation { self.session.current_location() }
    fn s(&self, v: i32) -> i32 { scale(v, self.dpi) }
    fn client_size(&self) -> (i32, i32) {
        let mut r = RECT::default();
        let _ = unsafe { GetClientRect(self.hwnd, &mut r) };
        (r.right, r.bottom)
    }

    /// Caption buttons (minimize, maximize, close) in client coordinates.
    pub(super) fn caption_buttons(&self) -> Option<[LayoutRect; 3]> {
        caption_rects(self.hwnd)
    }

    /// Width covered by the DWM caption buttons, in client pixels.
    fn caption_width(&self) -> i32 {
        if window_actions::fullscreen(self.hwnd) { return self.s(46*3); }
        let (width, _) = self.client_size();
        if let Some(bounds) = theme::caption_button_bounds(self.hwnd) {
            let mut window = RECT::default();
            let _ = unsafe { GetWindowRect(self.hwnd, &mut window) };
            let mut origin = POINT::default();
            let _ = unsafe { ClientToScreen(self.hwnd, &mut origin) };
            let left_in_client = bounds.left - (origin.x - window.left);
            if left_in_client > 0 && left_in_client < width { return width - left_in_client; }
        }
        self.s(46 * 3)
    }

    fn relayout(&mut self) {
        let (width, height) = self.client_size();
        let input = ChromeInput {
            search_panel_open:self.search_panel.open,
            tab_placement:self.session.ui.tab_placement,
            width, height, dpi: self.dpi, caption_width: self.caption_width(),
            caption_button: self.caption_buttons().map(|r|r[0]), command_bar: &self.session.ui.command_bar,
            tab_count: self.session.tabs.len(), active_tab: self.session.active_tab,
            show_tree: self.session.ui.show_tree && self.tree.is_some() && (!self.session.ui.show_preview || width>=self.s(if self.session.ui.tab_placement==zsdesk_core::model::explorer::TabPlacement::Top{900}else{1110})), tree_width: self.s(self.session.ui.tree_width),
            show_bookmarks: self.session.ui.show_bookmarks, status_bar: true, bookmark_widths: &self.bookmark_widths,
        };
        let mut layout=ChromeLayout::new(&input);
        layout.retain_tab_viewport(&self.layout,self.session.tabs.len(),self.session.active_tab);
        self.layout=layout;
        self.preview_rect=None;
        if self.session.ui.show_preview && self.layout.browser.width>self.s(240){
            let area=self.layout.browser;
            let width=(area.width/3).clamp(self.s(280),self.s(520)).min(area.width-self.s(160));
            self.layout.browser.width-=width+self.s(1);
            self.preview_rect=Some(LayoutRect::new(area.x+area.width-width,area.y,width,area.height));
        }
        self.layout_dual_pane();
        self.layout_crumbs();
        self.drop_zones.set(DropZones { bookmarks: self.layout.bookmarks_row, strip: self.layout.strip,tabs:self.layout.tab_panel,
            identity:if self.background_pane{self.drop_zones.get().identity}else{self.identity_for_source(self.browser_source)} });
        let frame_height=if crate::edition::THEMED && self.mica {self.layout.browser.y}else{self.layout.strip.height};
        theme::extend_frame(self.hwnd, if self.fullscreen_restore.is_some() { 0 } else { frame_height });
    }

    fn position_children(&mut self) {
        if let Some(browser) = &self.browser { unsafe { browser.resize(win_rect(self.layout.browser)); } }
        self.position_other_pane();
        if let Some(tree) = &self.tree {
            let visible = self.session.ui.show_tree && !self.layout.tree.is_empty();
            if visible { unsafe { tree.resize(win_rect(self.layout.tree)); } }
            tree.set_visible(visible);
        }
        let line = self.edit_line_height();
        let field = self.layout.search;
        let pad = self.s(10);
        unsafe { move_control(self.search_edit, LayoutRect::new(field.x + pad, field.y + (field.height - line) / 2, (field.width - pad - self.s(34)).max(0), line)); }
        let address = self.layout.address;
        unsafe { move_control(self.address_edit, LayoutRect::new(address.x + pad, address.y + (address.height - line) / 2, (address.width - pad * 2).max(0), line)); }
        let _ = unsafe { ShowWindow(self.address_edit, if self.address_editing { SW_SHOWNOACTIVATE } else { SW_HIDE }) };
        if let (Some(index), false) = (self.renamed_bookmark_index(), self.rename_hwnd.0.is_null()) {
            {
                let r=self.bookmark_rect(index);
                let pad = self.s(8);
                unsafe { move_control(self.rename_hwnd, LayoutRect::new(r.x + pad, r.y + (r.height - line) / 2, r.width.max(self.s(140)) - pad * 2, line)); }
            }
        }
        self.invalidate();
    }

    fn edit_line_height(&self) -> i32 {
        let mut metrics = windows::Win32::Graphics::Gdi::TEXTMETRICW::default();
        let dc = unsafe { windows::Win32::Graphics::Gdi::GetDC(Some(self.address_edit)) };
        let old = unsafe { windows::Win32::Graphics::Gdi::SelectObject(dc, self.font.into()) };
        let _ = unsafe { windows::Win32::Graphics::Gdi::GetTextMetricsW(dc, &mut metrics) };
        unsafe { windows::Win32::Graphics::Gdi::SelectObject(dc, old); windows::Win32::Graphics::Gdi::ReleaseDC(Some(self.address_edit), dc); }
        (metrics.tmHeight + self.s(2)).max(self.s(16))
    }

    unsafe fn resize(&mut self) {
        let dpi = unsafe { GetDpiForWindow(self.hwnd) }.max(96);
        if dpi != self.dpi {
            self.dpi = dpi;
            unsafe { self.update_font(); }
            self.icons.clear();
            self.measure_bookmarks();
        }
        // Keep native panes at their normal dimensions while the main window
        // is iconic; a zero-sized Shell view can retain stale layout on restore.
        if unsafe { IsIconic(self.hwnd) }.as_bool() { return; }
        self.relayout();
        self.position_children();
    }

    unsafe fn update_font(&mut self) {
        let mut metrics = NONCLIENTMETRICSW { cbSize: size_of::<NONCLIENTMETRICSW>() as u32, ..Default::default() };
        let _ = unsafe { SystemParametersInfoForDpi(SPI_GETNONCLIENTMETRICS.0, metrics.cbSize, Some((&mut metrics as *mut NONCLIENTMETRICSW).cast()), 0, self.dpi) };
        let old = self.font;
        self.font = unsafe { CreateFontIndirectW(&metrics.lfMessageFont) };
        for hwnd in [self.address_edit, self.search_edit, self.rename_hwnd, self.tooltip] {
            if !hwnd.0.is_null() { unsafe { SendMessageW(hwnd, WM_SETFONT, Some(WPARAM(self.font.0 as usize)), Some(LPARAM(1))); } }
        }
        if !old.0.is_null() { let _ = unsafe { DeleteObject(old.into()) }; }
    }

    /// Follows the system app mode for the chrome, DWM frame and Shell controls.
    fn apply_theme(&mut self) {
        let prefs=if crate::edition::THEMED {self.session.ui.appearance.clone()}
            else {zsdesk_core::model::explorer::AppearancePreferences::default()};
        self.dark = theme::configure_color_scheme(prefs.color_scheme);
        zsdesk_windows::flyout::refresh_system();
        self.palette = Palette::for_mode(if self.dark { Mode::Dark } else { Mode::Light });
        if let Some(accent) = theme::system_accent(self.dark) { self.palette = self.palette.with_accent(accent); }
        theme::set_dark_frame(self.hwnd, self.dark);
        theme::round_corners(self.hwnd, false);
        theme::round_corners(self.tooltip, true);
        self.mica = theme::set_backdrop(self.hwnd, prefs.backdrop);
        unsafe {
            if self.mica { let _=SetPropW(self.hwnd,windows::core::w!("ZSDesk.NativeCaption"),Some(windows::Win32::Foundation::HANDLE(1usize as *mut _))); }
            else { let _=RemovePropW(self.hwnd,windows::core::w!("ZSDesk.NativeCaption")); }
        }
        let (normal, focused, text) = (colorref(self.palette.input), colorref(self.palette.input_focused), colorref(self.palette.text));
        let _=replace_edit_colors(self.hwnd,normal,focused,text);
        let cue = wide(&format!("搜索 {}", self.current_location().title()));
        unsafe { SendMessageW(self.search_edit, EM_SETCUEBANNER, Some(WPARAM(1)), Some(LPARAM(cue.as_ptr() as isize))); }
        for edit in [self.address_edit, self.search_edit] { theme::theme_control(edit, self.dark); }
    }

    fn refresh_system_theme(&mut self){
        // A changed caption/frame mode invalidates an in-progress press.
        unsafe{SendMessageW(self.hwnd,WM_CANCELMODE,None,None);}
        self.apply_theme();self.theme_children();
        self.relayout();self.position_children();self.invalidate();
    }

    fn theme_children(&self) {
        // DefView owns its ItemsView theme lifecycle. Retheming it during
        // asynchronous navigation suppresses selection backgrounds in new views.
        if let Some(tree) = &self.tree { tree.apply_theme(self.dark, colorref(self.palette.content), colorref(self.palette.text)); }
        if let Some(browser)=&self.browser {browser.set_content_background(self.dark,colorref(self.palette.content));}
        self.theme_other_pane();
    }

    fn invalidate(&self) { let _ = unsafe { InvalidateRect(Some(self.hwnd), None, false) }; }
    fn invalidate_rect(&self, r: LayoutRect) {
        let rect = win_rect(r);
        let _ = unsafe { InvalidateRect(Some(self.hwnd), Some(&rect), false) };
    }

    fn keyboard_view(&self, msg: &MSG, allow_chrome: bool) -> Option<windows::Win32::UI::Shell::IShellView> {
        if self.pending.is_some() { return None; }
        let browser = self.browser.as_ref()?;
        let view = browser.view_window()?;
        let focus = unsafe { GetFocus() };
        let in_view = focus == view || unsafe { IsChild(view, focus) }.as_bool();
        let chrome = allow_chrome && (focus == self.hwnd || (focus.0.is_null() && msg.hwnd == self.hwnd));
        if !in_view && !chrome { return None; }
        unsafe { browser.current_view().ok() }
    }

    fn owns_live_focus(&self, target: HWND) -> bool {
        !target.0.is_null() && unsafe { IsWindow(Some(target)) }.as_bool()
            && unsafe { IsChild(self.hwnd, target) }.as_bool()
            && unsafe { IsWindowVisible(target) }.as_bool()
            && unsafe { IsWindowEnabled(target) }.as_bool()
    }

    fn refresh_status(&mut self) {
        let status = self.browser.as_ref().map(|b| b.status()).unwrap_or_default();
        let mode = self.browser.as_ref().and_then(|b| b.view_mode()).unwrap_or((0, 0));
        if status != self.view_status || mode != self.view_mode {
            let selection_changed = (status.selected > 0) != (self.view_status.selected > 0);
            self.view_status = status;
            self.view_mode = mode;
            self.invalidate_rect(self.layout.status);
            if selection_changed { self.invalidate_rect(self.layout.command_row); }
        }
    }

    fn show_message(&mut self, text: String) {
        self.status = text;
        self.relayout();self.position_children();
        unsafe { SetTimer(Some(self.hwnd), TIMER_MESSAGE, 6000, None); }
        self.invalidate_rect(self.layout.command_row);
        self.invalidate_rect(self.layout.status);
    }

    fn save(&mut self) {
        match storage::save_window_with_shared_preferences(&self.config_path,&self.canonical_session(),self.secondary_window,&self.search_saved_preferences,&self.bookmarks_saved){
            Ok((preferences,bookmarks))=>{
                self.session.ui.apply_search_preferences(&preferences);self.search_saved_preferences=preferences;
                self.session.next_id=self.session.next_id.max(bookmarks.iter().map(|b|b.id).max().unwrap_or(0).saturating_add(1));
                if self.session.bookmarks!=bookmarks{
                    self.session.bookmarks=bookmarks.clone();
                    if self.rename_id.is_some()&&self.renamed_bookmark_index().is_none(){unsafe{self.cancel_rename();}}
                    self.measure_bookmarks();self.relayout();self.position_children();
                }
                self.bookmarks_saved=bookmarks;
            },
            Err(e)=>self.show_message(format!("保存会话失败：{e}")),
        }
    }

    fn remember_placement(&mut self) {
        if self.fullscreen_restore.is_some() { return; }
        let mut placement = WINDOWPLACEMENT { length: size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
        if unsafe { GetWindowPlacement(self.hwnd, &mut placement) }.is_ok() {
            let mut r = placement.rcNormalPosition;
            // rcNormalPosition is in workspace coordinates; convert by the work-area offset.
            let (dx, dy) = if !unsafe{IsZoomed(self.hwnd)}.as_bool()&&!unsafe{IsIconic(self.hwnd)}.as_bool()
                &&unsafe{GetWindowRect(self.hwnd,&mut r)}.is_ok(){(0,0)}else{work_area_offset(self.hwnd)};
            self.session.ui.window = Some(WindowPlacement { x: r.left + dx, y: r.top + dy, width: r.right - r.left, height: r.bottom - r.top,
                maximized: geometry::restore_maximized(placement.showCmd, placement.flags.0) });
        }
    }

    unsafe fn destroy(&mut self) {
        if let Some(mut registry)=self.shell_registry.take(){registry.stop();}
        pane_input::discard(self.hwnd);
        diagnostics::record("window","close",None);diagnostics::close(self.hwnd);
        context_menu::destroy(self.hwnd);
        self.folder_sizes.cancel();
        self.cancel_fast_search();self.fast_search=None;
        unsafe { self.capture_active(); self.commit_bookmark_rename(); }
        self.destroy_other_pane();
        self.remember_placement();
        self.drop_registration = None;
        self.tree = None;
        self.browser = None;
        self.save();
        if !self.font.0.is_null() { let _ = unsafe { DeleteObject(self.font.into()) }; self.font = HFONT::default(); }
    }
}

/// Relaunch command and icon recorded on the window for taskbar pins. A
/// non-default profile is passed back so a pin reopens the same profile.
fn relaunch_command(config_path:&std::path::Path)->Option<(String,String)>{
    let exe=std::env::current_exe().ok()?;let exe=exe.to_string_lossy().into_owned();
    let default=std::path::absolute(crate::storage::config_path()).ok();
    let mut command=format!("\"{exe}\"");
    if default.as_deref()!=Some(config_path){
        if let Some(profile)=config_path.parent(){command.push_str(&format!(" --profile-dir \"{}\"",profile.display()));}
    }
    Some((command,format!("{exe},0")))
}

fn work_area_offset(hwnd: HWND) -> (i32, i32) {
    use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST};
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
    if unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        (info.rcWork.left - info.rcMonitor.left, info.rcWork.top - info.rcMonitor.top)
    } else { (0, 0) }
}

/// Frame thickness used for the resize border (matches the system caption metrics).
fn frame_size(dpi: u32) -> (i32, i32) {
    let padded = unsafe { GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi) };
    (unsafe { GetSystemMetricsForDpi(SM_CXFRAME, dpi) } + padded, unsafe { GetSystemMetricsForDpi(SM_CYFRAME, dpi) } + padded)
}

fn resize_cursor(hit:u32)->Option<PCWSTR>{
    Some(match hit{HTLEFT|HTRIGHT=>IDC_SIZEWE,HTTOP|HTBOTTOM=>IDC_SIZENS,
        HTTOPLEFT|HTBOTTOMRIGHT=>IDC_SIZENWSE,HTTOPRIGHT|HTBOTTOMLEFT=>IDC_SIZENESW,_=>return None})
}
fn caption_rects(hwnd:HWND)->Option<[LayoutRect;3]>{
    let mut client=RECT::default();unsafe{GetClientRect(hwnd,&mut client)}.ok()?;
    if client.right<=0{return None;}
    let dpi=unsafe{GetDpiForWindow(hwnd)}.max(96);
    if !window_actions::fullscreen(hwnd){
        if let Some(bounds)=theme::caption_button_bounds(hwnd){
            let mut window=RECT::default();unsafe{GetWindowRect(hwnd,&mut window)}.ok()?;
            let mut origin=POINT::default();unsafe{ClientToScreen(hwnd,&mut origin)}.ok().ok()?;
            let x=bounds.left-(origin.x-window.left);let y=(bounds.top-(origin.y-window.top)).max(0);
            let width=(bounds.right-bounds.left)/3;let height=bounds.bottom-(origin.y-window.top)-y;
            if width>0&&height>0&&x>=0&&x<client.right{return Some([0,1,2].map(|i|LayoutRect::new(x+width*i,y,width,height)));}
        }
    }
    let width=scale(46,dpi);Some([0,1,2].map(|i|LayoutRect::new(client.right-width*(3-i),0,width,scale(STRIP_H,dpi))))
}
fn caption_hit(hwnd:HWND,mut point:POINT)->Option<u32>{
    unsafe{ScreenToClient(hwnd,&mut point)}.ok().ok()?;
    caption_rects(hwnd)?.iter().zip([HTMINBUTTON,HTMAXBUTTON,HTCLOSE]).find_map(|(rect,hit)|rect.contains(point.x,point.y).then_some(hit))
}
fn native_caption(hwnd:HWND)->bool{
    !window_actions::fullscreen(hwnd)&&!unsafe{GetPropW(hwnd,windows::core::w!("ZSDesk.NativeCaption"))}.0.is_null()
}

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg==WM_NCDESTROY{release_window_properties(hwnd);}
    let native_caption = native_caption(hwnd);
    let caption_message = matches!(msg, WM_NCHITTEST | WM_NCMOUSEMOVE | WM_NCMOUSELEAVE | WM_NCLBUTTONDOWN | WM_NCLBUTTONUP | WM_NCLBUTTONDBLCLK);
    if native_caption && caption_message {
        let mut result = LRESULT(0);
        if unsafe { DwmDefWindowProc(hwnd, msg, wp, lp, &mut result) }.as_bool() {
            if msg==WM_NCHITTEST&&unsafe{IsZoomed(hwnd)}.as_bool()&&geometry::is_resize_hit(result.0 as u32){return unsafe{hit_test(hwnd,lp)};}
            return result;
        }
    }
    if msg==WM_APPCOMMAND&&mouse_navigation::app_command(hwnd,wp,lp){return LRESULT(1);}
    if msg==WM_TIMER&&wp.0==context_menu::TIMER{context_menu::poll(hwnd);return LRESULT(0);}
    if msg!=0&&msg==crate::context_menu_process::progress_message(){context_menu::poll(hwnd);return LRESULT(0);}
    if msg == WM_NCHITTEST {
        if let Some(hit)=caption_hit(hwnd,POINT{x:x_lparam(lp.0),y:y_lparam(lp.0)}){return LRESULT(hit as isize);}
        if window_actions::fullscreen(hwnd) { return LRESULT(HTCLIENT as isize); }
        let mut bounds=RECT::default();
        if unsafe {GetWindowRect(hwnd,&mut bounds)}.is_ok() {
            let point=POINT{x:x_lparam(lp.0),y:y_lparam(lp.0)};
            let resizable=unsafe {GetWindowLongPtrW(hwnd,GWL_STYLE)} as u32 & WS_THICKFRAME.0 != 0;
            if let Some(hit)=geometry::resize_hit(bounds,point,frame_size(unsafe {GetDpiForWindow(hwnd)}.max(96)),unsafe {IsZoomed(hwnd)}.as_bool(),resizable) {
                return LRESULT(hit as isize);
            }
        }
    }
    if msg == WM_NCCALCSIZE && window_actions::fullscreen(hwnd) { return LRESULT(0); }
    if let Some(result) = zsdesk_windows::shell::menu::forward_menu_message(msg, wp, lp) { return result; }
    // Child controls own their cursor, including edit/IME and native Shell
    // cursors. Returning FALSE lets the child finish WM_SETCURSOR processing.
    if msg == WM_SETCURSOR && wp.0 != hwnd.0 as usize { return LRESULT(0); }
    if msg == WM_SETCURSOR {
        if (lp.0&0xffff)as u32==HTCAPTION{
            if let Ok(cursor)=unsafe{LoadCursorW(None,IDC_ARROW)}{unsafe{SetCursor(Some(cursor));}return LRESULT(1);}
        }
        if let Some(cursor)=resize_cursor((lp.0&0xffff)as u32){
            if let Ok(cursor)=unsafe{LoadCursorW(None,cursor)}{unsafe{SetCursor(Some(cursor));}return LRESULT(1);}
        }
    }
    // The painted buttons must use the same rectangles for input. DWM may
    // return HTCAPTION for an extended frame after maximizing.
    // DwmDefWindowProc has already had first refusal for a native caption.
    // When it declines button input, keep the same explicit fallback used by
    // the painted caption instead of entering DefWindowProc's tracking loop.
    if msg==WM_NCLBUTTONDOWN&&[HTMINBUTTON,HTMAXBUTTON,HTCLOSE].contains(&(wp.0 as u32)){
        unsafe{SetPropW(hwnd,windows::core::w!("ZSDesk.CaptionPress"),Some(windows::Win32::Foundation::HANDLE(wp.0 as*mut _))).ok();SetCapture(hwnd);}
        track_caption_buttons(hwnd,msg,wp);return LRESULT(0);
    }
    let caption_press=unsafe{GetPropW(hwnd,windows::core::w!("ZSDesk.CaptionPress"))}.0 as usize;
    if caption_press!=0&&matches!(msg,WM_LBUTTONUP|WM_NCLBUTTONUP|WM_CAPTURECHANGED|WM_CANCELMODE){
        let _=unsafe{RemovePropW(hwnd,windows::core::w!("ZSDesk.CaptionPress"))};
        // Cancel/capture loss must disarm before ReleaseCapture re-enters this
        // procedure; never release a different window's mouse capture.
        if unsafe{GetCapture()}==hwnd{let _=unsafe{ReleaseCapture()};}
        if matches!(msg,WM_LBUTTONUP|WM_NCLBUTTONUP){
            let mut p=POINT{x:x_lparam(lp.0),y:y_lparam(lp.0)};
            if msg==WM_LBUTTONUP{let _=unsafe{ClientToScreen(hwnd,&mut p)};}
            if caption_hit(hwnd,p)==Some(caption_press as u32){
                let command=match caption_press as u32{HTCLOSE=>SC_CLOSE,HTMINBUTTON=>SC_MINIMIZE,_=>if window_actions::fullscreen(hwnd)||unsafe{IsZoomed(hwnd)}.as_bool(){SC_RESTORE}else{SC_MAXIMIZE}};
                post(hwnd,WM_SYSCOMMAND,command as usize,0);
            }
        }
        track_caption_buttons(hwnd,WM_NCMOUSELEAVE,WPARAM(0));return LRESULT(0);
    }
    if msg==WM_CANCELMODE&&unsafe{GetCapture()}==hwnd{let _=unsafe{ReleaseCapture()};}
    if caption_press != 0 && msg == WM_MOUSEMOVE {
        let mut point = POINT { x: x_lparam(lp.0), y: y_lparam(lp.0) };
        let _ = unsafe { ClientToScreen(hwnd, &mut point) };
        let hit = caption_hit(hwnd, point).unwrap_or(0);
        track_caption_buttons(hwnd, WM_NCLBUTTONDOWN, WPARAM(if hit == caption_press as u32 { hit as usize } else { 0 }));
        return LRESULT(0);
    }
    track_caption_buttons(hwnd, msg, wp);
    if msg == WM_NCHITTEST {
        let point = POINT { x: x_lparam(lp.0), y: y_lparam(lp.0) };
        if let Some(code) = caption_button_hit(hwnd, point) { return LRESULT(code as isize); }
    }
    let mut dwm_result = LRESULT(0);
    if !caption_message && unsafe { DwmDefWindowProc(hwnd, msg, wp, lp, &mut dwm_result) }.as_bool() {
        return dwm_result;
    }
    match msg {
        WM_NCCREATE => {
            let cs = unsafe { &*(lp.0 as *const CREATESTRUCTW) };
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize); }
            return unsafe { DefWindowProcW(hwnd, msg, wp, lp) };
        }
        WM_NCCALCSIZE if lp.0 != 0 => {
            let client = if wp.0 != 0 { unsafe { std::ptr::addr_of_mut!((*(lp.0 as *mut NCCALCSIZE_PARAMS)).rgrc[0]) } }
                else { lp.0 as *mut RECT };
            let proposed = unsafe { *client };
            let _ = unsafe { DefWindowProcW(hwnd, msg, wp, lp) };
            let (_, frame_y) = frame_size(unsafe { GetDpiForWindow(hwnd) }.max(96));
            unsafe { *client = geometry::captionless_client(proposed, *client, IsZoomed(hwnd).as_bool(), frame_y); }
            // No WVR_VALIDRECTS: the native preserved rectangles describe a
            // caption that this window removed. Repaint using the new bounds.
            return LRESULT(0);
        }
        WM_NCHITTEST => return unsafe { hit_test(hwnd, lp) },
        WM_ERASEBKGND => return LRESULT(1),
        WM_CTLCOLOREDIT => {
            if let Some(colors)=edit_colors(hwnd){
                let dc = HDC(wp.0 as *mut _);
                let is_focused = unsafe { GetFocus() }.0 == lp.0 as *mut _;
                unsafe { SetTextColor(dc,COLORREF(colors.text));SetBkColor(dc,COLORREF(if is_focused{colors.focus_background}else{colors.background})); }
                let brush=if is_focused{colors.focused}else{colors.normal};if brush!=0{return LRESULT(brush);}
            }
        }
        WM_COPYDATA if lp.0 != 0 => {
            let data = unsafe { &*(lp.0 as *const COPYDATASTRUCT) };
            if data.dwData==crate::instance::FOCUS_SEARCH_MAGIC{
                if data.cbData!=1||data.lpData.is_null(){return LRESULT(0);}
                let settings=unsafe{*(data.lpData as*const u8)};if settings>1{return LRESULT(0);}
                post(hwnd,WM_ACTION,if settings==1{ACTION_SEARCH_SETTINGS}else{Command::FocusSearch.id()as usize},0);return LRESULT(1);
            }
            if data.dwData==crate::reveal::VERIFY_THIS_PC&&data.cbData==0{
                let pointer=unsafe{GetWindowLongPtrW(hwnd,GWLP_USERDATA)}as*const Slot;
                let accepted=!pointer.is_null()&&unsafe{&*pointer}.try_borrow().ok().is_some_and(|slot|slot.as_ref().is_some_and(|state|state.pending.is_none()&&state.queued.is_none()&&matches!(state.current_location(),ShellLocation::ThisPc)));
                return LRESULT(accepted as isize);
            }
            if data.dwData==crate::reveal::VERIFY_TAG {
                if data.lpData.is_null()||data.cbData>1024*1024{return LRESULT(0);}
                let bytes=unsafe{std::slice::from_raw_parts(data.lpData as*const u8,data.cbData as usize)};
                let Some((folder,names))=crate::reveal::decode(bytes)else{return LRESULT(0);};
                let pointer=unsafe{GetWindowLongPtrW(hwnd,GWLP_USERDATA)}as*const Slot;
                if pointer.is_null(){return LRESULT(0);}
                let accepted=unsafe{&*pointer}.try_borrow().ok().is_some_and(|slot|slot.as_ref().is_some_and(|state|{
                    if state.pending.is_some()||state.queued.is_some()||!state.current_location().as_path().is_some_and(|path|crate::explorer_forwarder::same_path(path,&folder)){return false;}
                    if names.is_empty(){return true;}
                    let selected=state.browser.as_ref().map(|browser|browser.selection_paths_limit(128)).unwrap_or_default();
                    selected.len()==names.len()&&names.iter().all(|name|selected.iter().any(|path|crate::explorer_forwarder::same_path(std::path::Path::new(path),&folder.join(name))))
                }));
                return LRESULT(accepted as isize);
            }
            if data.dwData == crate::instance::ACTIVATE_MAGIC && data.cbData == 0 {
                return LRESULT(unsafe { crate::instance::activate(hwnd) } as isize);
            }
            if data.dwData==crate::instance::DIAGNOSTICS_MAGIC{
                if data.lpData.is_null() || data.cbData>65536{return LRESULT(0);}
                let bytes=unsafe{std::slice::from_raw_parts(data.lpData as *const u8,data.cbData as usize)};
                let Ok(request)=serde_json::from_slice::<crate::instance::DiagnosticRequest>(bytes)else{return LRESULT(0);};
                if !request.output.is_absolute(){return LRESULT(0);}
                let p=Box::into_raw(Box::new(request));
                if unsafe{PostMessageW(Some(hwnd),WM_DIAGNOSTIC,WPARAM(0),LPARAM(p as isize))}.is_err(){unsafe{drop(Box::from_raw(p));}return LRESULT(0);}
                return LRESULT(1);
            }
            if data.dwData==crate::reveal::COPYDATA_TAG {
                if data.lpData.is_null() || data.cbData>1024*1024{return LRESULT(0);}
                let bytes=unsafe{std::slice::from_raw_parts(data.lpData as *const u8,data.cbData as usize)};
                let Some((folder,selections))=crate::reveal::decode(bytes) else{return LRESULT(0);};
                let payload=Box::into_raw(Box::new(crate::instance::OpenRequest{location:ShellLocation::Path(folder),selection:None,selections}));
                if unsafe{PostMessageW(Some(hwnd),WM_OPEN_REQUEST,WPARAM(0),LPARAM(payload as isize))}.is_err(){unsafe{drop(Box::from_raw(payload));}return LRESULT(0);}
                return LRESULT(1);
            }
            if data.dwData != crate::instance::OPEN_MAGIC || data.cbData > 65536 || data.lpData.is_null() { return LRESULT(0); }
            let bytes = unsafe { std::slice::from_raw_parts(data.lpData.cast::<u8>(), data.cbData as usize) };
            let Ok((target, selection)) = serde_json::from_slice::<(Option<ShellLocation>, Option<String>)>(bytes) else { return LRESULT(0) };
            if let Some(location) = target {
                let payload = Box::into_raw(Box::new(crate::instance::OpenRequest { location, selection: selection.map(Into::into), selections:Vec::new() }));
                if unsafe { PostMessageW(Some(hwnd), WM_OPEN_REQUEST, WPARAM(0), LPARAM(payload as isize)) }.is_err() {
                    unsafe { drop(Box::from_raw(payload)); }
                    return LRESULT(0);
                }
            }
            return LRESULT(1);
        }
        WM_GETMINMAXINFO if lp.0 != 0 => {
            let _ = unsafe { DefWindowProcW(hwnd, msg, wp, lp) };
            let info = unsafe { &mut *(lp.0 as *mut MINMAXINFO) };
            let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
            info.ptMinTrackSize = POINT { x: scale(640, dpi), y: scale(420, dpi) };
            return LRESULT(0);
        }
        WM_DPICHANGED if lp.0 != 0 => {
            let r = unsafe { &*(lp.0 as *const RECT) };
            let _ = unsafe { SetWindowPos(hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE) };
            // A DPI change can keep the same physical size and omit WM_SIZE.
            // Rebuild fonts, size-specific icons and all child metrics anyway.
            post(hwnd, WM_LAYOUT, 0, 0);
            return LRESULT(0);
        }
        _ => {}
    }
    let pointer = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const Slot;
    if pointer.is_null() { return unsafe { DefWindowProcW(hwnd, msg, wp, lp) }; }
    if msg == WM_NCDESTROY {
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0); }
        return unsafe { DefWindowProcW(hwnd, msg, wp, lp) };
    }
    // DefWindowProc must run outside the state borrow for system commands.
    // It enters modal move/size loops and dispatches WM_DESTROY synchronously.
    if msg==WM_SYSCOMMAND&&window_actions::fullscreen(hwnd)&&matches!((wp.0 as u32)&0xfff0,SC_RESTORE|SC_MAXIMIZE){
        post(hwnd,WM_ACTION,Command::ToggleFullscreen.id()as usize,0);return LRESULT(0);
    }
    // Calls into Shell and native controls may synchronously re-enter this procedure.
    let Ok(mut slot) = (unsafe { &*pointer }).try_borrow_mut() else {
        return unsafe { reentrant(hwnd, msg, wp, lp) };
    };
    let Some(state) = slot.as_mut() else { return unsafe { DefWindowProcW(hwnd, msg, wp, lp) } };
    if state.hwnd.0.is_null() {
        if msg==WM_CLOSE{defer_message(hwnd,msg,wp,lp);return LRESULT(0);}
        return unsafe { DefWindowProcW(hwnd, msg, wp, lp) };
    }
    let result = unsafe { state.handle(msg, wp, lp) };
    if msg != WM_DESTROY { state.publish_shell_windows(); }
    if msg == WM_DESTROY { *slot = None; }
    let live=slot.is_some();
    drop(slot);
    flush_deferred(hwnd,live);
    if take_deferred_paint(hwnd)&&live { let _ = unsafe { InvalidateRect(Some(hwnd), None, false) }; }
    result.unwrap_or_else(|| unsafe { DefWindowProcW(hwnd, msg, wp, lp) })
}

/// Hover/press state of the caption buttons. Without Mica the strip is opaque and
/// ZSDesk paints the buttons itself, while DWM keeps hit-testing them.
fn caption_visual(hwnd: HWND) -> (u32, bool) {
    let value = unsafe { GetPropW(hwnd, windows::core::w!("ZSDesk.CaptionVisual")) }.0 as usize;
    ((value & 0xffff) as u32, value & 0x10000 != 0)
}
fn track_caption_buttons(hwnd: HWND, msg: u32, wp: WPARAM) {
    let code = wp.0 as u32;
    let is_button = [HTMINBUTTON, HTMAXBUTTON, HTCLOSE].contains(&code);
    let (old, pressed) = caption_visual(hwnd);
    let new = match msg {
        WM_NCMOUSEMOVE => {
            let mut track = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE | TME_NONCLIENT, hwndTrack: hwnd, dwHoverTime: 0 };
            let _ = unsafe { TrackMouseEvent(&mut track) };
            (if is_button { code } else { 0 }, pressed && is_button && code == old)
        }
        WM_NCLBUTTONDOWN => (if is_button { code } else { 0 }, is_button),
        WM_NCLBUTTONUP => (if is_button { code } else { 0 }, false),
        WM_NCMOUSELEAVE | WM_MOUSEMOVE | WM_CANCELMODE | WM_CAPTURECHANGED | WM_KILLFOCUS => (0, false),
        _ => return,
    };
    if new != (old, pressed) {
        unsafe { let _ = SetPropW(hwnd, windows::core::w!("ZSDesk.CaptionVisual"), Some(windows::Win32::Foundation::HANDLE((new.0 as usize | ((new.1 as usize) << 16)) as *mut _))); }
        let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
        let mut client = RECT::default();
        let _ = unsafe { GetClientRect(hwnd, &mut client) };
        let strip = RECT { bottom: scale(STRIP_H, dpi), ..client };
        let _ = unsafe { InvalidateRect(Some(hwnd), Some(&strip), false) };
    }
}

/// Messages that arrive while a modal loop holds the state.
unsafe fn reentrant(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_SETCURSOR if (lp.0 & 0xFFFF) as u32 == HTCLIENT && !text_input_window(HWND(wp.0 as *mut _)) => {
            if let Ok(cursor)=unsafe{LoadCursorW(None,IDC_ARROW)} {unsafe{SetCursor(Some(cursor));}}
            LRESULT(1)
        }
        WM_SETTINGCHANGE if theme_setting_changed(wp,lp)=>{
            // WM_SETTINGCHANGE may carry a borrowed system string. Convert it
            // to a pointer-free refresh before deferring through a modal loop.
            defer_message(hwnd,WM_THEMECHANGED,WPARAM(0),LPARAM(0));LRESULT(0)
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            unsafe { BeginPaint(hwnd, &mut ps); let _ = EndPaint(hwnd, &ps); }
            let _=unsafe{SetPropW(hwnd,DEFERRED_PAINT_PROPERTY,Some(windows::Win32::Foundation::HANDLE(1usize as*mut _)))};
            LRESULT(0)
        }
        WM_CLOSE | WM_LAYOUT | WM_ACTION | WM_SHELL_NAVIGATED | WM_SHELL_FOLDER_INVOKED | WM_SHELL_NAVIGATION_FAILED | WM_BOOKMARK_PATH_DROPPED
        | WM_OPEN_REQUEST | WM_TREE_NAVIGATE | WM_SHELL_SELECTION_CHANGED | crate::routing_control::WM_RESULT | WM_FAST_SEARCH | WM_DIAGNOSTIC
        | WM_CANCELMODE | WM_CAPTURECHANGED | WM_THEMECHANGED | WM_DWMCOMPOSITIONCHANGED | WM_SYSCOLORCHANGE | pane_input::MESSAGE | crate::shell_window_registry::WM_SELECT => {
            defer_message(hwnd,msg,wp,lp);
            LRESULT(0)
        }
        WM_SIZE => { defer_message(hwnd,WM_LAYOUT,WPARAM(0),LPARAM(0)); LRESULT(0) }
        message if message >= 0xC000 && message == zsdesk_windows::shell::menu::failure_message() => {
            defer_message(hwnd,msg,wp,lp); LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

fn defer_message(hwnd:HWND,message:u32,wp:WPARAM,lp:LPARAM){
    DEFERRED_MESSAGES.with(|pending|{
        let mut pending=pending.borrow_mut();
        if [WM_LAYOUT,WM_FAST_SEARCH,WM_THEMECHANGED,WM_DWMCOMPOSITIONCHANGED,WM_SYSCOLORCHANGE].contains(&message) && pending.iter().any(|(h,m,_,_)|*h==hwnd && *m==message){return;}
        pending.push_back((hwnd,message,wp,lp));
    });
}

fn theme_setting_changed(wp:WPARAM,lp:LPARAM)->bool{
    wp.0==SPI_SETHIGHCONTRAST.0 as usize || (lp.0!=0&&unsafe{PCWSTR(lp.0 as*const u16).to_string()}.is_ok_and(|value|value=="ImmersiveColorSet"))
}
fn flush_deferred(hwnd:HWND,live:bool){
    let messages=DEFERRED_MESSAGES.with(|pending|std::mem::take(&mut *pending.borrow_mut()));
    for(h,message,wp,lp)in messages{
        if h!=hwnd {defer_message(h,message,wp,lp);continue;}
        if live && unsafe{PostMessageW(Some(h),message,wp,lp)}.is_ok(){continue;}
        if lp.0==0{continue;}
        unsafe{match message{
            WM_BOOKMARK_PATH_DROPPED=>drop(Box::from_raw(lp.0 as *mut PathBuf)),
            WM_OPEN_REQUEST=>drop(Box::from_raw(lp.0 as *mut crate::instance::OpenRequest)),
            WM_SHELL_SELECTION_CHANGED=>acknowledge_selection_changed(lp.0),
            crate::routing_control::WM_RESULT=>drop(Box::from_raw(lp.0 as *mut String)),
            WM_DIAGNOSTIC=>drop(Box::from_raw(lp.0 as *mut crate::instance::DiagnosticRequest)),
            WM_SHELL_NAVIGATED|WM_SHELL_FOLDER_INVOKED|WM_TREE_NAVIGATE=>drop(Box::from_raw(lp.0 as *mut ShellLocation)),
            _=>{},
        }}
    }
}

fn caption_button_rects(hwnd: HWND) -> Option<[LayoutRect; 3]> {
    let bounds = theme::caption_button_bounds(hwnd)?;
    let mut window = RECT::default();
    let _ = unsafe { GetWindowRect(hwnd, &mut window) };
    let mut origin = POINT::default();
    let _ = unsafe { ClientToScreen(hwnd, &mut origin) };
    let (dx, dy) = (origin.x - window.left, origin.y - window.top);
    let (x, y) = (bounds.left - dx, (bounds.top - dy).max(0));
    let width = (bounds.right - bounds.left) / 3;
    let height = bounds.bottom - dy - y;
    Some([0, 1, 2].map(|i| LayoutRect::new(x + width * i, y, width, height)))
}

fn caption_button_hit(hwnd: HWND, mut point: POINT) -> Option<u32> {
    if !unsafe { ScreenToClient(hwnd, &mut point) }.as_bool() { return None; }
    caption_button_hit_code(&caption_button_rects(hwnd)?, point.x, point.y)
}

fn caption_button_hit_code(buttons: &[LayoutRect; 3], x: i32, y: i32) -> Option<u32> {
    buttons.iter().position(|button| button.contains(x, y)).map(|index| [HTMINBUTTON, HTMAXBUTTON, HTCLOSE][index])
}

unsafe fn hit_test(hwnd: HWND, lp: LPARAM) -> LRESULT {
    let mut point = POINT { x: x_lparam(lp.0), y: y_lparam(lp.0) };
    let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
    let maximized=unsafe {IsZoomed(hwnd)}.as_bool();
    let mut window = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut window) }.is_ok() {
        let resizable = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } as u32 & WS_THICKFRAME.0 != 0;
        if let Some(edge) = geometry::resize_hit(window, point, frame_size(dpi), maximized, resizable) {
            return LRESULT(edge as isize);
        }
    }
    let mut default = unsafe { DefWindowProcW(hwnd, WM_NCHITTEST, WPARAM(0), lp) };
    if maximized && geometry::is_resize_hit(default.0 as u32) {default=LRESULT(HTBORDER as isize);}
    if [HTMINBUTTON, HTMAXBUTTON, HTCLOSE, HTSYSMENU].contains(&(default.0 as u32)) { return default; }
    let _ = unsafe { ScreenToClient(hwnd, &mut point) };
    let mut client = RECT::default();let _ = unsafe { GetClientRect(hwnd, &mut client) };
    if point.x < 0 || point.y < 0 || point.x >= client.right || point.y >= client.bottom { return default; }
    let pointer = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const Slot;
    let hit = (!pointer.is_null()).then(|| unsafe { &*pointer }.try_borrow().ok()
        .and_then(|s| s.as_ref().map(|s| s.layout.hit(point.x, point.y)))).flatten();
    match hit {
        Some(Hit::Caption) => LRESULT(HTCAPTION as isize),
        None if point.y < scale(STRIP_H, dpi) => LRESULT(HTCAPTION as isize),
        _ => LRESULT(HTCLIENT as isize),
    }
}

impl WindowState {
    unsafe fn handle(&mut self, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        if msg==crate::shell_window_registry::WM_SELECT {
            self.apply_shell_selections();return Some(LRESULT(0));
        }
        if msg>=0xc000 && msg==shell_registration::taskbar_created_message() {
            if let Some(registry)=&mut self.shell_registry{registry.restart();}
            return Some(LRESULT(0));
        }
        if dual_pane::browser_message(msg) && !self.routing_pane_message { return self.route_browser_message(msg,wp,lp); }
        let point = || POINT { x: x_lparam(lp.0), y: y_lparam(lp.0) };
        match msg {
            WM_QUERYENDSESSION=>return Some(LRESULT(1)),
            WM_ENDSESSION=>{self.session_ending=wp.0!=0;if self.session_ending{self.save();}return None;},
            WM_CLOSE if !self.session_ending&&self.session.has_locked_tabs()=>{
                self.show_message(if !self.session.ui.dual_pane&&self.session.secondary_pane.tabs.iter().any(|t|t.locked){"隐藏窗格中有已锁定的标签页，请开启双栏并解锁后再关闭窗口"}else{"有已锁定的标签页，请先在标签右键菜单中解锁后再关闭窗口"}.into());return Some(LRESULT(0));
            },
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let dc = unsafe { BeginPaint(self.hwnd, &mut ps) };
                self.paint();
                if let Some(surface) = &self.surface {
                    let r = ps.rcPaint;
                    let _ = unsafe { BitBlt(dc, r.left, r.top, r.right - r.left, r.bottom - r.top, Some(surface.dc), r.left, r.top, SRCCOPY) };
                }
                let _ = unsafe { EndPaint(self.hwnd, &ps) };
            }
            WM_SIZE => {unsafe { self.resize();SetTimer(Some(self.hwnd),TIMER_PLACEMENT,300,None); }},
            WM_EXITSIZEMOVE => { self.remember_placement(); self.save(); unsafe { self.resize(); } }
            WM_LAYOUT => unsafe { self.resize() },
            WM_ACTIVATE => {
                let activation = (wp.0 & 0xFFFF) as u32;
                let active = activation != WA_INACTIVE;
                if !active {
                    let focus = unsafe { GetFocus() };
                    self.last_focus = if self.owns_live_focus(focus) { focus } else { HWND::default() };
                }
                self.active = active;
                self.relayout();
                self.invalidate_rect(self.layout.strip);
                if activation == WA_ACTIVE {
                    let target = self.last_focus;
                    if self.owns_live_focus(target) { let _ = unsafe { SetFocus(Some(target)) }; }
                    else if let Some(b) = &self.browser { unsafe { b.focus(); } }
                }
                return Some(LRESULT(0));
            }
            WM_SETTINGCHANGE => {
                let area = if lp.0 != 0 { unsafe { PCWSTR(lp.0 as *const u16).to_string() }.unwrap_or_default() } else { String::new() };
                if theme_setting_changed(wp,lp) {self.refresh_system_theme();self.icons.clear();}
                if let Some(tree) = &self.tree { tree.refresh_metrics(); }
                if area == "ShellState" || area.is_empty() { self.invalidate(); }
            }
            WM_THEMECHANGED | WM_DWMCOMPOSITIONCHANGED | WM_SYSCOLORCHANGE => self.refresh_system_theme(),
            WM_TIMER => {
                if wp.0==deferred_selection::TIMER{self.poll_pane_selections();}
                if wp.0==TIMER_PLACEMENT{let _=unsafe{KillTimer(Some(self.hwnd),TIMER_PLACEMENT)};self.remember_placement();self.save();}
                if wp.0 == TIMER_STATUS { self.update_shell_registry();self.refresh_status();self.poll_search_settings();self.poll_pane_searches();self.poll_folder_sizes(); }
                if wp.0 == TIMER_SEARCH { let _ = unsafe { KillTimer(Some(self.hwnd), TIMER_SEARCH) }; unsafe { self.live_search(); } }
                if wp.0 == TIMER_FAST_SEARCH { let _=unsafe{KillTimer(Some(self.hwnd),TIMER_FAST_SEARCH)};self.poll_pane_searches(); }
                if wp.0 == TIMER_MESSAGE {
                    let _ = unsafe { KillTimer(Some(self.hwnd), TIMER_MESSAGE) };
                    self.status.clear();
                    self.relayout();self.position_children();
                    self.invalidate_rect(self.layout.command_row);
                    self.invalidate_rect(self.layout.status);
                }
            }
            WM_MOUSEMOVE => self.mouse_move(point(), wp.0),
            WM_MOUSELEAVE => {
                self.tracking_mouse = false;
                if self.tab_drag.is_none() && self.splitter_drag.is_none() { self.set_hover(Hit::None, None); }
            }
            WM_LBUTTONDOWN => self.mouse_down(point()),
            WM_LBUTTONUP => self.mouse_up(point()),
            WM_LBUTTONDBLCLK => self.double_click(point()),
            WM_MBUTTONUP => self.middle_click(point()),
            WM_RBUTTONUP => self.right_click(point()),
            WM_CANCELMODE => {
                self.tab_drag = None;self.bookmark_drag=None; self.splitter_drag = None; self.pane_splitter_drag=None; self.pressed = None;
                if unsafe { GetCapture() } == self.hwnd { let _ = unsafe { ReleaseCapture() }; }
                self.set_hover(Hit::None, None); self.invalidate();
            }
            WM_CAPTURECHANGED => { self.tab_drag = None;self.bookmark_drag=None; self.splitter_drag = None; self.pane_splitter_drag=None; self.pressed = None; self.invalidate(); }
            WM_SETCURSOR => {
                if (lp.0 & 0xFFFF) as u32 == HTCLIENT {
                    let mut p = POINT::default();
                    let _ = unsafe { GetCursorPos(&mut p) };
                    let _ = unsafe { ScreenToClient(self.hwnd, &mut p) };
                    let cursor = match self.layout.hit(p.x, p.y) {
                        Hit::Splitter | Hit::PaneSplitter => Some(IDC_SIZEWE),
                        Hit::Address if !self.address_editing && self.crumb_at(p.x, p.y).is_none() => Some(IDC_IBEAM),
                        _ => None,
                    };
                    let cursor = cursor.or(self.splitter_drag.or(self.pane_splitter_drag.map(|(x,w)|(x,i32::from(w)))).map(|_| IDC_SIZEWE)).unwrap_or(IDC_ARROW);
                    if let Ok(c) = unsafe { LoadCursorW(None, cursor) } { unsafe { SetCursor(Some(c)); } }
                    return Some(LRESULT(1));
                }
            }
            WM_SETFOCUS => {
                if let Some(b) = &self.browser { unsafe { b.focus(); } }
                return Some(LRESULT(0));
            }
            WM_COMMAND => {
                let id = wp.0 & 0xFFFF;
                let code = (wp.0 >> 16) as u32;
                if lp.0 != 0 {
                    // Notifications from native EDIT children.
                    if (id == ID_ADDRESS || id == ID_SEARCH) && (code == EN_SETFOCUS || code == EN_KILLFOCUS) {
                        self.invalidate_rect(self.layout.nav_row);
                        if id==ID_SEARCH && code==EN_SETFOCUS && !self.search_panel.open { post(self.hwnd,WM_ACTION,Command::FocusSearch.id() as usize,0); }
                        if id == ID_ADDRESS && code == EN_KILLFOCUS { post(self.hwnd, WM_ACTION, ACTION_END_ADDRESS_EDIT, 0); }
                    }
                    if id == ID_BOOKMARK_RENAME && code == EN_KILLFOCUS { post(self.hwnd, WM_ACTION, ACTION_COMMIT_RENAME, 0); }
                    // Search as you type, after a short pause (like Windows 11).
                    if id == ID_SEARCH && code == EN_CHANGE && unsafe { GetFocus() } == self.search_edit {
                        unsafe { SetTimer(Some(self.hwnd), TIMER_SEARCH, 250, None); }
                    }
                } else {
                    if Command::from_id(id as u16).is_some_and(Command::pane_specific) {
                        self.queue_pane_action(self.browser_source,id);
                    }else{post(self.hwnd, WM_ACTION, id, 0);}
                }
                return Some(LRESULT(0));
            }
            WM_ACTION => unsafe { self.action(wp.0, lp.0) },
            pane_input::MESSAGE => self.receive_pane_input(wp.0),
            WM_SHELL_NAVIGATED => if lp.0 != 0 { unsafe { self.navigation_completed(*Box::from_raw(lp.0 as *mut ShellLocation)); } },
            WM_SHELL_FOLDER_INVOKED => if lp.0 != 0 { unsafe { self.navigate_to(*Box::from_raw(lp.0 as *mut ShellLocation)); } },
            WM_SHELL_NAVIGATION_FAILED => unsafe { self.navigation_failed() },
            WM_SHELL_SELECTION_CHANGED => { unsafe { acknowledge_selection_changed(lp.0); } self.refresh_status(); }
            WM_TREE_NAVIGATE => if lp.0 != 0 {
                let location = unsafe { *Box::from_raw(lp.0 as *mut ShellLocation) };
                if wp.0 != 0 { unsafe { self.new_tab(location); } } else { unsafe { self.navigate_from_tree(location); } }
            },
            WM_BOOKMARK_PATH_DROPPED => if lp.0 != 0 { self.add_bookmark_path(unsafe { *Box::from_raw(lp.0 as *mut PathBuf) }); },
            WM_OPEN_REQUEST => if lp.0 != 0 { unsafe { self.open_requested(*Box::from_raw(lp.0 as *mut crate::instance::OpenRequest)); } },
            WM_FAST_SEARCH => self.poll_pane_searches(),
            WM_DIAGNOSTIC=>if lp.0!=0{let request=unsafe{Box::from_raw(lp.0 as *mut crate::instance::DiagnosticRequest)};let _=self.write_diagnostics(&request);},
            crate::routing_control::WM_RESULT => if lp.0!=0{self.show_message(unsafe{*Box::from_raw(lp.0 as *mut String)});},
            message if message>=0xC000&&message==zsdesk_windows::shell::file_transfer::result_message()=>{
                let code=wp.0 as u32;
                if code==0{
                    while let Some(path)=zsdesk_windows::shell::file_transfer::take_created_folder(self.hwnd){self.select_created_folder(path);}
                }else if code!=0x800704c7{self.show_message(format!("文件操作未完成：{}",windows::core::Error::from_hresult(windows::core::HRESULT(code as i32))));}
            },
            message if message >= 0xC000 && message == zsdesk_windows::shell::menu::failure_message() => {
                self.show_message(format!("右键菜单加载失败（0x{:08X}），隐藏设置仍保留。", wp.0 as u32));
            }
            WM_DESTROY => { crate::runtime::trace("window-destroy");zsdesk_windows::taskbar::clear_identity(self.hwnd);unsafe { self.destroy(); PostQuitMessage(0); }crate::runtime::trace("window-quit-posted"); }
            _ => return None,
        }
        Some(LRESULT(0))
    }
}

/// Bookmarks bar and tab strip accept files/folders dragged from anywhere.
struct ChromeDrop { hwnd: isize, zones: Rc<Cell<DropZones>>, accepted: Cell<bool> }

impl ChromeDrop {
    fn zone(&self, point: POINT) -> Option<bool> {
        let hwnd = HWND(self.hwnd as *mut _);
        let mut p = point;
        let _ = unsafe { ScreenToClient(hwnd, &mut p) };
        let zones = self.zones.get();
        if zones.bookmarks.contains(p.x, p.y) { Some(true) } else if zones.strip.contains(p.x, p.y)||zones.tabs.contains(p.x,p.y) { Some(false) } else { None }
    }
}

impl DropHandler for ChromeDrop {
    fn enter(&self, data: &windows::Win32::System::Com::IDataObject, _keys: u32, point: POINT, _allowed: DROPEFFECT) -> DROPEFFECT {
        self.accepted.set(has_hdrop(data));
        self.over(0, point, DROPEFFECT_LINK)
    }
    fn over(&self, _keys: u32, point: POINT, _allowed: DROPEFFECT) -> DROPEFFECT {
        if self.accepted.get() && self.zone(point).is_some() { DROPEFFECT_LINK } else { DROPEFFECT_NONE }
    }
    fn leave(&self) { self.accepted.set(false); }
    fn dropped(&self, data: &windows::Win32::System::Com::IDataObject, _keys: u32, point: POINT, _allowed: DROPEFFECT) -> DROPEFFECT {
        let Some(bookmarks) = self.zone(point).filter(|_| self.accepted.get()) else { return DROPEFFECT_NONE };
        let paths=hdrop_paths(data).into_iter().take(32).collect::<Vec<_>>();
        if !bookmarks{
            let sent=self.zones.get().identity.is_some_and(|identity|
                pane_input::post_tab_paths(HWND(self.hwnd as*mut _),identity,paths));
            return if sent{DROPEFFECT_LINK}else{DROPEFFECT_NONE};
        }
        for path in paths {
            let payload = Box::into_raw(Box::new(path));
            if unsafe { PostMessageW(Some(HWND(self.hwnd as *mut _)), WM_BOOKMARK_PATH_DROPPED, WPARAM(0), LPARAM(payload as isize)) }.is_err() {
                unsafe { drop(Box::from_raw(payload)); }
            }
        }
        DROPEFFECT_LINK
    }
}

unsafe fn child(parent: HWND, class: &str, text: &str, id: usize, style: u32, visible: bool) -> windows::core::Result<HWND> {
    let visible = if visible { WS_VISIBLE } else { WINDOW_STYLE(0) };
    unsafe { CreateWindowExW(WINDOW_EX_STYLE::default(), PCWSTR(wide(class).as_ptr()), PCWSTR(wide(text).as_ptr()),
        WS_CHILD | visible | WS_TABSTOP | WINDOW_STYLE(style), 0, 0, 1, 1, Some(parent), Some(HMENU(id as *mut _)), None, None) }
}

unsafe fn create_tooltip(owner: HWND) -> HWND {
    let Ok(tip) = (unsafe { CreateWindowExW(WS_EX_TOPMOST, TOOLTIPS_CLASSW, PCWSTR::null(), WS_POPUP | WINDOW_STYLE(TTS_NOPREFIX | TTS_ALWAYSTIP),
        CW_USEDEFAULT, CW_USEDEFAULT, CW_USEDEFAULT, CW_USEDEFAULT, Some(owner), None, None, None) }) else { return HWND::default() };
    let mut empty = [0u16];
    let info = TTTOOLINFOW { cbSize: size_of::<TTTOOLINFOW>() as u32, uFlags: TTF_SUBCLASS, hwnd: owner, uId: 1,
        lpszText: PWSTR(empty.as_mut_ptr()), ..Default::default() };
    unsafe { SendMessageW(tip, TTM_ADDTOOLW, None, Some(LPARAM(&info as *const TTTOOLINFOW as isize))); }
    unsafe { SendMessageW(tip, TTM_SETMAXTIPWIDTH, None, Some(LPARAM(600))); }
    tip
}

pub(super) unsafe fn move_control(hwnd: HWND, r: LayoutRect) {
    if !hwnd.0.is_null() { let _ = unsafe { MoveWindow(hwnd, r.x, r.y, r.width, r.height, true) }; }
}
pub(super) fn win_rect(r: LayoutRect) -> RECT { RECT { left: r.x, top: r.y, right: r.x + r.width, bottom: r.y + r.height } }
pub(super) fn wide(s: &str) -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() }
pub(super) fn pcwstr(s: &[u16]) -> PCWSTR { PCWSTR(s.as_ptr()) }
pub(super) fn post(hwnd: HWND, msg: u32, wp: usize, lp: isize) { let _ = unsafe { PostMessageW(Some(hwnd), msg, WPARAM(wp), LPARAM(lp)) }; }
pub(super) fn x_lparam(v: isize) -> i32 { (v as i16) as i32 }
pub(super) fn y_lparam(v: isize) -> i32 { ((v >> 16) as i16) as i32 }
pub(super) fn colorref(c: zsdesk_ui::theme::Color) -> u32 {
    // Flatten translucent tokens onto white/black is unnecessary: EDIT colors are opaque.
    (c.r as u32) | ((c.g as u32) << 8) | ((c.b as u32) << 16)
}

fn accelerator_table() -> Vec<ACCEL> {
    let mut table: Vec<(ACCEL_VIRT_FLAGS, VIRTUAL_KEY, Command)> = vec![
        (FCONTROL | FVIRTKEY, VK_T, Command::NewTab), (FCONTROL | FVIRTKEY, VK_W, Command::CloseTab),
        (FCONTROL | FVIRTKEY, VK_F4, Command::CloseTab), (FCONTROL | FALT | FVIRTKEY, VK_W, Command::CloseOtherTabs),
        (FCONTROL | FSHIFT | FVIRTKEY, VK_T, Command::ReopenTab), (FCONTROL | FVIRTKEY, VK_TAB, Command::NextTab),
        (FCONTROL | FSHIFT | FVIRTKEY, VK_TAB, Command::PreviousTab), (FALT | FVIRTKEY, VK_LEFT, Command::Back),
        (FALT | FVIRTKEY, VK_RIGHT, Command::Forward), (FALT | FVIRTKEY, VK_UP, Command::Up),
        (FCONTROL | FVIRTKEY, VK_L, Command::FocusAddress), (FALT | FVIRTKEY, VK_D, Command::FocusAddress),
        (FVIRTKEY, VK_F4, Command::FocusAddress), (FVIRTKEY, VK_F5, Command::Refresh),
        (FCONTROL | FVIRTKEY, VK_D, Command::AddBookmark), (FCONTROL | FVIRTKEY, VK_F, Command::FocusSearch),
        (FCONTROL | FVIRTKEY, VK_E, Command::FocusSearch), (FVIRTKEY, VK_F3, Command::FocusSearch),
        (FCONTROL | FSHIFT | FVIRTKEY, VK_N, Command::NewFolder), (FCONTROL | FSHIFT | FVIRTKEY, VK_C, Command::CopyPath),
        (FALT | FVIRTKEY, VK_P, Command::TogglePreview), (FALT | FSHIFT | FVIRTKEY, VK_P, Command::ToggleDetails),
        (FCONTROL | FVIRTKEY, VK_N, Command::NewWindow),
        (FVIRTKEY, VK_F11, Command::ToggleFullscreen),
        (FVIRTKEY, VK_F6, Command::FocusNext), (FSHIFT | FVIRTKEY, VK_F6, Command::FocusPrevious),
        (FCONTROL | FVIRTKEY, VK_R, Command::Refresh),
        (FCONTROL | FVIRTKEY, VK_OEM_PLUS, Command::ZoomIn),
        (FCONTROL | FVIRTKEY, VK_OEM_MINUS, Command::ZoomOut),
        (FCONTROL | FVIRTKEY, VK_0, Command::ResetZoom),
    ];
    for n in 1..=9u8 {
        table.push((FCONTROL | FVIRTKEY, VIRTUAL_KEY(VK_0.0 + n as u16), Command::SelectTab(n)));
    }
    for n in 1..=8u8 {
        table.push((FCONTROL | FSHIFT | FVIRTKEY, VIRTUAL_KEY(VK_0.0 + n as u16), Command::ViewPreset(n)));
    }
    table.into_iter().map(|(flags, key, c)| ACCEL { fVirt: flags, key: key.0, cmd: c.id() }).collect()
}

unsafe extern "system" fn edit_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, id: usize, parent: usize) -> LRESULT {
    if id==ID_ADDRESS {
        if msg==WM_GETDLGCODE && wp.0==VK_RETURN.0 as usize {
            return LRESULT(unsafe{DefSubclassProc(hwnd,msg,wp,lp)}.0 | DLGC_WANTMESSAGE as isize);
        }
        if address_input::handle(hwnd,msg,wp) {return LRESULT(0);}
        if msg==WM_NCDESTROY {address_input::end(hwnd);}
    }
    // EDIT focus notifications can arrive while the parent state is borrowed
    // by Shell. Queue the transition directly so reentrancy cannot lose it.
    if msg==WM_KILLFOCUS && id==ID_ADDRESS {post(HWND(parent as *mut _),WM_ACTION,ACTION_END_ADDRESS_EDIT,0);}
    if msg==WM_SETFOCUS && id==ID_SEARCH {post(HWND(parent as *mut _),WM_ACTION,Command::FocusSearch.id() as usize,0);}
    if msg == WM_KEYDOWN && wp.0 == VK_A.0 as usize && unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0
        && unsafe { GetKeyState(VK_MENU.0 as i32) } >= 0 {
        unsafe { SendMessageW(hwnd, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1))); }
        return LRESULT(0);
    }
    if msg == WM_KEYDOWN && [VK_RETURN.0 as usize, VK_ESCAPE.0 as usize, VK_TAB.0 as usize].contains(&wp.0) {
        // Let the IME commit or cancel a composition before handling navigation.
        if edit_composing(hwnd) {return unsafe{DefSubclassProc(hwnd,msg,wp,lp)};}
        let enter = wp.0 == VK_RETURN.0 as usize;
        let action = match (id, wp.0) {
            (_, k) if k == VK_TAB.0 as usize => ACTION_FOCUS_VIEW,
            (ID_SEARCH, _) if enter => ACTION_SEARCH,
            (ID_SEARCH, _) => ACTION_CLOSE_SEARCH,
            (ID_ADDRESS, _) if enter => return LRESULT(0),
            (ID_ADDRESS, _) => ACTION_CANCEL_ADDRESS,
            (_, _) if enter => ACTION_COMMIT_RENAME,
            _ => ACTION_CANCEL_RENAME,
        };
        post(HWND(parent as *mut _), WM_ACTION, action, 0);
        return LRESULT(0);
    }
    // Swallow the beep for Enter/Escape/Tab characters.
    if msg == WM_CHAR && [0x01, 0x0D, 0x1B, 0x09].contains(&wp.0) { return LRESULT(0); }
    if msg == WM_NCDESTROY { unsafe { let _ = RemoveWindowSubclass(hwnd, Some(edit_proc), id); } }
    unsafe { DefSubclassProc(hwnd, msg, wp, lp) }
}

fn edit_composing(hwnd:HWND)->bool{
    use windows::Win32::UI::Input::Ime::{ImmGetContext,ImmReleaseContext,ImmGetCompositionStringW,GCS_COMPSTR};
    let context=unsafe{ImmGetContext(hwnd)};
    if context.0.is_null(){return false;}
    let composing=unsafe{ImmGetCompositionStringW(context,GCS_COMPSTR,None,0)}>0;
    let _=unsafe{ImmReleaseContext(hwnd,context)};composing
}

#[cfg(test)]mod deferred_tests{
    use super::*;
    #[test]
    fn ctrl_a_selects_owned_edit_text_without_changing_it() {
        use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyboardState, SetKeyboardState};
        struct OwnedEdit { owner: HWND, keys: Option<[u8; 256]> }
        impl Drop for OwnedEdit {
            fn drop(&mut self) { unsafe {
                if let Some(keys) = &self.keys { let _ = SetKeyboardState(keys); }
                let _ = DestroyWindow(self.owner);
            } }
        }
        unsafe {
            let owner = CreateWindowExW(WS_EX_NOACTIVATE, windows::core::w!("STATIC"), windows::core::w!("text shortcut test"),
                WS_POPUP, 0, 0, 160, 40, None, None, None, None).unwrap();
            let mut owned = OwnedEdit { owner, keys: None };
            let edit = child(owner, "EDIT", "alpha beta", ID_ADDRESS, ES_AUTOHSCROLL as u32, false).unwrap();
            SetWindowSubclass(edit, Some(edit_proc), ID_ADDRESS, owner.0 as usize).ok().unwrap();
            let mut keys = [0u8; 256];
            GetKeyboardState(&mut keys).unwrap();
            owned.keys = Some(keys);
            keys[VK_CONTROL.0 as usize] = 0x80;
            keys[VK_MENU.0 as usize] = 0;
            SetKeyboardState(&keys).unwrap();
            SendMessageW(edit, WM_KEYDOWN, Some(WPARAM(VK_A.0 as usize)), None);
            let selection = SendMessageW(edit, EM_GETSEL, None, None).0 as u32;
            assert_eq!((selection & 0xffff, selection >> 16), (0, 10));
            SendMessageW(edit, WM_CHAR, Some(WPARAM(1)), None);
            let mut text = [0u16; 32];
            let length = GetWindowTextW(edit, &mut text).max(0) as usize;
            assert_eq!(String::from_utf16_lossy(&text[..length]), "alpha beta");
            assert!(!IsWindowVisible(edit).as_bool());
        }
    }
    #[test]fn modal_messages_wait_without_reposting_and_layout_is_coalesced(){
        DEFERRED_MESSAGES.with(|q|q.borrow_mut().clear());
        let h=HWND::default();
        defer_message(h,WM_LAYOUT,WPARAM(0),LPARAM(0));defer_message(h,WM_LAYOUT,WPARAM(0),LPARAM(0));
        defer_message(h,WM_ACTION,WPARAM(10),LPARAM(1));defer_message(h,WM_ACTION,WPARAM(11),LPARAM(2));
        let q=DEFERRED_MESSAGES.with(|q|std::mem::take(&mut *q.borrow_mut()));
        assert_eq!(q.len(),3);assert_eq!(q[1].2.0,10);assert_eq!(q[2].3.0,2);
    }
    #[test]
    fn caption_button_rectangles_return_native_nonclient_codes() {
        let buttons = [LayoutRect::new(10, 0, 10, 20), LayoutRect::new(20, 0, 10, 20), LayoutRect::new(30, 0, 10, 20)];
        assert_eq!(caption_button_hit_code(&buttons, 15, 10), Some(HTMINBUTTON));
        assert_eq!(caption_button_hit_code(&buttons, 25, 10), Some(HTMAXBUTTON));
        assert_eq!(caption_button_hit_code(&buttons, 35, 10), Some(HTCLOSE));
        assert_eq!(caption_button_hit_code(&buttons, 40, 10), None);
    }
}
