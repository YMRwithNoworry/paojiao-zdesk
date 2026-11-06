//! Windows 11 style Explorer window.
//!
//! The title bar is replaced by the tab strip: `WM_NCCALCSIZE` removes the
//! caption while the DWM frame retains system caption-button metrics, Snap
//! Layouts and Mica. Measured button bounds are routed as native nonclient hits
//! before custom frame hit testing. The remaining chrome is painted with
//! Direct2D using ZSUI Fluent tokens; the folder view and controls stay native.

mod bookmarks;
mod input;
mod keyboard;
mod routing_settings;
mod navigation;
mod quick_preview;
mod paint;
mod global_search;
#[cfg(test)]
mod selection_regression;

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
const WM_LAYOUT: u32 = WM_APP + 2;
pub(super) const WM_ACTION: u32 = WM_APP + 3;
const WM_OPEN_REQUEST: u32 = WM_APP + 4;
const WM_OPEN_TAB_PATH: u32 = WM_APP + 5;
const TIMER_STATUS: usize = 1;
const TIMER_MESSAGE: usize = 2;
const TIMER_SEARCH: usize = 3;
const TIMER_FAST_SEARCH:usize=4;
const WM_FAST_SEARCH:u32=WM_APP+43;
const WM_DIAGNOSTIC:u32=WM_APP+44;
type Slot = RefCell<Option<WindowState>>;
#[derive(Clone,Copy,PartialEq,Eq)]
enum BrowserRole{Fresh,Ordinary,Results}

thread_local! {
    /// A paint request arrived while the state was borrowed by a modal loop.
    static DEFERRED_PAINT: Cell<bool> = const { Cell::new(false) };
    static DEFERRED_MESSAGES: RefCell<std::collections::VecDeque<(HWND,u32,WPARAM,LPARAM)>> = const { RefCell::new(std::collections::VecDeque::new()) };
    /// Caption button under the pointer / pressed (HTMINBUTTON, HTMAXBUTTON, HTCLOSE) for self-drawn buttons.
    pub(super) static CAPTION_HOVER: Cell<(u32, bool)> = const { Cell::new((0, false)) };
    /// (normal brush, focused brush, normal bg, focused bg, text) for native EDIT controls.
    static EDIT_COLORS: Cell<(isize, isize, u32, u32, u32)> = const { Cell::new((0, 0, 0xFFFFFF, 0xFFFFFF, 0)) };
}

pub fn run(session: ExplorerSession, config_path: PathBuf, selection: Option<std::ffi::OsString>, modern: bool) -> Result<()> {
    unsafe { OleInitialize(None).context("OleInitialize failed")?; }
    let result = unsafe { run_inner(session, config_path, selection, modern) };
    unsafe { OleUninitialize(); }
    result
}

unsafe fn run_inner(session: ExplorerSession, config_path: PathBuf, selection: Option<std::ffi::OsString>, modern: bool) -> Result<()> {
    unsafe { InitCommonControlsEx(&INITCOMMONCONTROLSEX { dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32, dwICC: ICC_BAR_CLASSES | ICC_STANDARD_CLASSES }) }.ok()?;
    let instance = HINSTANCE(unsafe { GetModuleHandleW(None)? }.0);
    let class = wide("ZSDesk.Native.Explorer");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(window_proc), hInstance: instance,
        hIcon: unsafe { LoadIconW(Some(instance), PCWSTR(101usize as *const u16))? },
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW)? }, hbrBackground: HBRUSH::default(),
        lpszClassName: pcwstr(&class), ..Default::default()
    };
    unsafe { RegisterClassW(&wc) };
    let placement = session.ui.window;
    // The slot outlives the window and every callback that can reach it.
    let slot = Box::new(RefCell::new(Some(WindowState::new(session, config_path, selection, modern))));
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
    let accelerators = unsafe { CreateAcceleratorTableW(&accelerator_table())? };
    let _ = unsafe { ShowWindow(hwnd, if placement.is_some_and(|p| p.maximized) { SW_SHOWMAXIMIZED } else { SW_SHOW }) };
    let mut msg = MSG::default();
    let mut quick_preview:Option<quick_preview::QuickPreview>=None;
    loop {
        let result = unsafe { GetMessageW(&mut msg, None, 0, 0) }.0;
        if result <= 0 { break; }
        if let Some(preview)=quick_preview.as_mut().filter(|p|p.owns_window(msg.hwnd)){
            if !preview.pre_translate(&msg){unsafe{let _=TranslateMessage(&msg);DispatchMessageW(&msg);}}
            continue;
        }
        let plain_key=msg.message==WM_KEYDOWN && ![VK_CONTROL,VK_SHIFT,VK_MENU].iter().any(|k|unsafe{GetKeyState(k.0 as i32)}<0);
        let enabled=slot.try_borrow().ok().and_then(|s|s.as_ref().map(|s|s.session.ui.space_preview)).unwrap_or(false);
        if plain_key && !text_input_window(msg.hwnd) && (msg.wParam.0==VK_SPACE.0 as usize || (msg.wParam.0==VK_ESCAPE.0 as usize && quick_preview.as_ref().is_some_and(|p|p.is_open()))){
            if quick_preview.as_ref().is_some_and(|p|p.is_open()){quick_preview=None;continue;}
            if enabled && msg.wParam.0==VK_SPACE.0 as usize {
                let path=slot.try_borrow().ok().and_then(|s|s.as_ref().and_then(|s|s.browser.as_ref().and_then(|b|b.first_selected_path())));
                if let Some(path)=path {
                    match quick_preview::QuickPreview::open(hwnd,&path){Ok(preview)=>quick_preview=Some(preview),Err(error)=>{if let Ok(mut s)=slot.try_borrow_mut(){if let Some(s)=s.as_mut(){s.show_message(format!("无法预览：{error}"));}}}}
                    continue;
                }
            }
        }
        let editing = text_input_window(msg.hwnd) || text_input_window(unsafe { GetFocus() });
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
        let file_only_accelerator = modifiers.control && modifiers.shift && !modifiers.alt
            && [VK_C.0 as usize, VK_N.0 as usize].contains(&msg.wParam.0);
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
        if msg.message==WM_SHELL_SELECTION_CHANGED {
            if let Some(preview)=quick_preview.as_mut().filter(|p|p.is_open()){
                let path=slot.try_borrow().ok().and_then(|s|s.as_ref().and_then(|s|s.browser.as_ref().and_then(|b|b.first_selected_path())));
                if let Some(path)=path{let _=preview.update(&path);}
            }
        }
    }
    drop(quick_preview);
    let _ = unsafe { DestroyAcceleratorTable(accelerators) };
    // Posted payloads still own their allocation even after the receiver is destroyed.
    for message in [WM_SHELL_NAVIGATED, WM_SHELL_FOLDER_INVOKED, WM_BOOKMARK_PATH_DROPPED, WM_OPEN_REQUEST, WM_TREE_NAVIGATE, WM_OPEN_TAB_PATH, WM_SHELL_SELECTION_CHANGED,crate::routing_control::WM_RESULT,WM_DIAGNOSTIC] {
        while unsafe { PeekMessageW(&mut msg, Some(hwnd), message, message, PM_REMOVE) }.as_bool() {
            if msg.lParam.0 == 0 { continue; }
            unsafe {
                match message {
                    WM_BOOKMARK_PATH_DROPPED | WM_OPEN_TAB_PATH => drop(Box::from_raw(msg.lParam.0 as *mut PathBuf)),
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
pub(super) struct TabDrag { pub index: usize, pub start_x: i32, pub moved: bool }

/// Drop zones of the chrome shared with the OLE drop target.
#[derive(Clone, Copy, Default)]
pub(super) struct DropZones { pub bookmarks: LayoutRect, pub strip: LayoutRect }

struct WindowState {
    hwnd: HWND,
    dpi: u32,
    session: ExplorerSession,
    config_path: PathBuf,
    modern: bool,
    browser: Option<ExplorerBrowserHost>,
    tree: Option<NamespaceTreeHost>,
    address_edit: HWND,
    search_edit: HWND,
    rename_hwnd: HWND,
    rename_index: Option<usize>,
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
    splitter_drag: Option<(i32, i32)>,
    icons: HashMap<String, paint::IconEntry>,
    view_status: ViewStatus,
    view_mode: (i32, i32),
    bookmark_widths: Vec<i32>,
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
    open_requests: std::collections::VecDeque<crate::instance::OpenRequest>,
    /// Folder the current search started from, and the results location it produced.
    search_scope: Option<ShellLocation>,
    search_result: Option<ShellLocation>,
    browser_role:BrowserRole,
    fast_search:Option<crate::fast_search::FastSearch>,
    fast_generation:u64,
    fast_found:usize,
    search_progress:Option<String>,
    fast_events:std::collections::VecDeque<crate::fast_search::SearchEvent>,
}

impl WindowState {
    fn new(mut session: ExplorerSession, config_path: PathBuf, launch_selection: Option<std::ffi::OsString>, modern: bool) -> Self {
        if !modern{session.ui.context_menu.mode=zsdesk_core::model::explorer::ContextMenuMode::Original;}
        let modern=session.ui.context_menu.mode==zsdesk_core::model::explorer::ContextMenuMode::Windows11;
        Self {
            hwnd: HWND::default(), dpi: 96, session, config_path, modern, browser: None, tree: None,
            address_edit: HWND::default(), search_edit: HWND::default(), rename_hwnd: HWND::default(), rename_index: None,
            tooltip: HWND::default(), tooltip_text: vec![0], font: HFONT::default(), gfx: None, surface: None,
            palette: Palette::light(), dark: false, mica: false, active: true, layout: ChromeLayout::default(),
            crumbs: Vec::new(), crumb_slots: Vec::new(), crumbs_for: None, address_editing: false, open_chevron: None,
            hover: Hit::None, hover_crumb: None, pressed: None, tracking_mouse: false, tab_drag: None, splitter_drag: None,
            icons: HashMap::new(), view_status: ViewStatus::default(), view_mode: (0, 0), bookmark_widths: Vec::new(),
            drop_registration: None, drop_zones: Rc::new(Cell::new(DropZones::default())), last_focus: HWND::default(),
            pending: None, queued: None, shown_tab: None, closed_tabs: Vec::new(), status: String::new(),
            launch_selection, launch_selections:Vec::new(), open_requests: Default::default(), search_scope: None, search_result: None,
            browser_role:BrowserRole::Fresh,fast_search:None,fast_generation:0,fast_found:0,search_progress:None,fast_events:Default::default(),
        }
    }

    unsafe fn create(&mut self, hwnd: HWND) -> Result<()> {
        self.hwnd = hwnd;
        self.dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
        let key = crate::instance::property(&self.config_path);
        unsafe { SetPropW(hwnd, PCWSTR(key.as_ptr()), Some(windows::Win32::Foundation::HANDLE(1usize as *mut _)))?; }
        let route_key = crate::routing_control::profile_property(&self.config_path);
        unsafe { SetPropW(hwnd, PCWSTR(route_key.as_ptr()), Some(windows::Win32::Foundation::HANDLE(1usize as *mut _)))?; }
        unsafe { SetPropW(hwnd, windows::core::w!("ZSDesk.Activate.v1"), Some(windows::Win32::Foundation::HANDLE(1usize as *mut _)))?; }
        let preferred_show=if self.session.ui.window.is_some_and(|p|p.maximized){SW_SHOWMAXIMIZED.0}else{SW_SHOWNORMAL.0};
        unsafe{SetPropW(hwnd,windows::core::w!("ZSDesk.PreferredShow"),Some(windows::Win32::Foundation::HANDLE(preferred_show as usize as *mut _)))?;}
        let (family, _) = zsdesk_windows::gfx::system_ui_font(Some(hwnd));
        self.gfx = Some(Gfx::new(&family).context("Direct2D 初始化失败")?);
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
        let panes = Panes { preview: self.session.ui.show_preview, details: self.session.ui.show_details };
        #[cfg(not(test))]
        let browser = unsafe { ExplorerBrowserHost::create(hwnd, win_rect(self.layout.browser), self.modern, panes)? };
        #[cfg(test)]
        let browser = unsafe { ExplorerBrowserHost::create_transient(hwnd, win_rect(self.layout.browser), self.modern, panes)? };
        self.browser = Some(browser);
        if let Some(browser)=&self.browser{browser.set_context_menu_options(crate::context_preferences::native(&self.session.ui.context_menu));}
        self.tree = unsafe { NamespaceTreeHost::create(hwnd, win_rect(self.layout.tree)) }.ok();
        self.theme_children();
        self.position_children();
        unsafe { SetTimer(Some(hwnd), TIMER_STATUS, 1200, None); }
        unsafe { self.navigate_current(); }
        #[cfg(not(test))]
        {
            self.check_integration();
            if let Err(error) = crate::routing_control::refresh_owned_target(&self.config_path, hwnd) {
                self.show_message(format!("文件位置绑定检查失败：{error}"));
            }
        }
        Ok(())
    }

    /// Integration health check: if the registered executable no longer exists
    /// (moved or upgraded to another folder), point the verbs at this copy.
    fn check_integration(&mut self) {
        use zsdesk_windows::integration::{self, Integration};
        let level = integration::current();
        if level == Integration::Off { return; }
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
    pub(super) fn caption_buttons(&self) -> Option<[LayoutRect; 3]> { caption_button_rects(self.hwnd) }

    /// Width covered by the DWM caption buttons, in client pixels.
    fn caption_width(&self) -> i32 {
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
            width, height, dpi: self.dpi, caption_width: self.caption_width(),
            caption_button: self.caption_buttons().map(|r|r[0]), command_bar: &self.session.ui.command_bar,
            tab_count: self.session.tabs.len(), active_tab: self.session.active_tab,
            show_tree: self.session.ui.show_tree && self.tree.is_some(), tree_width: self.s(self.session.ui.tree_width),
            show_bookmarks: self.session.ui.show_bookmarks, status_bar: !self.status.is_empty() || self.search_progress.is_some(), bookmark_widths: &self.bookmark_widths,
        };
        self.layout = ChromeLayout::new(&input);
        self.layout_crumbs();
        self.drop_zones.set(DropZones { bookmarks: self.layout.bookmarks_row, strip: self.layout.strip });
        theme::extend_frame(self.hwnd, self.layout.strip.height);
    }

    fn position_children(&mut self) {
        if let Some(browser) = &self.browser { unsafe { browser.resize(win_rect(self.layout.browser)); } }
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
        let _ = unsafe { ShowWindow(self.address_edit, if self.address_editing { SW_SHOW } else { SW_HIDE }) };
        if let (Some(index), false) = (self.rename_index, self.rename_hwnd.0.is_null()) {
            if let Some(r) = self.layout.bookmarks.get(index) {
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
        self.dark = theme::system_dark_mode();
        self.palette = Palette::for_mode(if self.dark { Mode::Dark } else { Mode::Light });
        theme::allow_dark_mode(self.dark);
        theme::set_dark_frame(self.hwnd, self.dark);
        theme::round_corners(self.hwnd, false);
        theme::round_corners(self.tooltip, true);
        self.mica = theme::enable_mica(self.hwnd);
        let (normal, focused, text) = (colorref(self.palette.input), colorref(self.palette.input_focused), colorref(self.palette.text));
        EDIT_COLORS.with(|c| {
            let (old_a, old_b, ..) = c.get();
            for old in [old_a, old_b] { if old != 0 { let _ = unsafe { DeleteObject(HBRUSH(old as *mut _).into()) }; } }
            let a = unsafe { CreateSolidBrush(COLORREF(normal)) };
            let b = unsafe { CreateSolidBrush(COLORREF(focused)) };
            c.set((a.0 as isize, b.0 as isize, normal, focused, text));
        });
        let cue = wide(&format!("搜索 {}", self.current_location().title()));
        unsafe { SendMessageW(self.search_edit, EM_SETCUEBANNER, Some(WPARAM(1)), Some(LPARAM(cue.as_ptr() as isize))); }
        for edit in [self.address_edit, self.search_edit] { theme::theme_control(edit, self.dark); }
    }

    fn theme_children(&self) {
        // DefView owns its ItemsView theme lifecycle. Retheming it during
        // asynchronous navigation suppresses selection backgrounds in new views.
        if let Some(tree) = &self.tree { tree.apply_theme(self.dark, colorref(self.palette.content), colorref(self.palette.text)); }
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
        if let Err(e) = storage::save(&self.config_path, &self.session) { self.show_message(format!("保存会话失败：{e}")); }
    }

    fn remember_placement(&mut self) {
        let mut placement = WINDOWPLACEMENT { length: size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
        if unsafe { GetWindowPlacement(self.hwnd, &mut placement) }.is_ok() {
            let r = placement.rcNormalPosition;
            // rcNormalPosition is in workspace coordinates; convert by the work-area offset.
            let (dx, dy) = work_area_offset(self.hwnd);
            self.session.ui.window = Some(WindowPlacement { x: r.left + dx, y: r.top + dy, width: r.right - r.left, height: r.bottom - r.top,
                maximized: placement.showCmd == SW_SHOWMAXIMIZED.0 as u32 });
        }
    }

    unsafe fn destroy(&mut self) {
        self.cancel_fast_search();self.fast_search=None;
        unsafe { self.capture_active(); self.commit_bookmark_rename(); }
        self.remember_placement();
        self.drop_registration = None;
        self.tree = None;
        self.browser = None;
        self.save();
        if !self.font.0.is_null() { let _ = unsafe { DeleteObject(self.font.into()) }; self.font = HFONT::default(); }
    }
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

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if let Some(result) = zsdesk_windows::shell::menu::forward_menu_message(msg, wp, lp) { return result; }
    track_caption_buttons(hwnd, msg, wp);
    if msg == WM_NCHITTEST {
        let point = POINT { x: x_lparam(lp.0), y: y_lparam(lp.0) };
        if let Some(code) = caption_button_hit(hwnd, point) { return LRESULT(code as isize); }
    }
    let mut dwm_result = LRESULT(0);
    if unsafe { DwmDefWindowProc(hwnd, msg, wp, lp, &mut dwm_result) }.as_bool() { return dwm_result; }
    match msg {
        WM_NCCREATE => {
            let cs = unsafe { &*(lp.0 as *const CREATESTRUCTW) };
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize); }
            return unsafe { DefWindowProcW(hwnd, msg, wp, lp) };
        }
        WM_NCCALCSIZE if wp.0 != 0 => {
            let params = unsafe { &mut *(lp.0 as *mut NCCALCSIZE_PARAMS) };
            let original_top = params.rgrc[0].top;
            let result = unsafe { DefWindowProcW(hwnd, msg, wp, lp) };
            // Remove the caption; keep the side and bottom resize borders.
            params.rgrc[0].top = original_top;
            if unsafe { IsZoomed(hwnd) }.as_bool() {
                let (_, frame_y) = frame_size(unsafe { GetDpiForWindow(hwnd) }.max(96));
                params.rgrc[0].top += frame_y;
            }
            return result;
        }
        WM_NCHITTEST => return unsafe { hit_test(hwnd, lp) },
        WM_ERASEBKGND => return LRESULT(1),
        WM_CTLCOLOREDIT => {
            let (normal, focused, bg, focus_bg, text) = EDIT_COLORS.with(Cell::get);
            let dc = HDC(wp.0 as *mut _);
            let is_focused = unsafe { GetFocus() }.0 == lp.0 as *mut _;
            unsafe { SetTextColor(dc, COLORREF(text)); SetBkColor(dc, COLORREF(if is_focused { focus_bg } else { bg })); }
            let brush = if is_focused { focused } else { normal };
            if brush != 0 { return LRESULT(brush); }
        }
        WM_COPYDATA if lp.0 != 0 => {
            let data = unsafe { &*(lp.0 as *const COPYDATASTRUCT) };
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
        WM_GETMINMAXINFO => {
            let info = unsafe { &mut *(lp.0 as *mut MINMAXINFO) };
            let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
            info.ptMinTrackSize = POINT { x: scale(640, dpi), y: scale(420, dpi) };
            return LRESULT(0);
        }
        WM_DPICHANGED => {
            let r = unsafe { &*(lp.0 as *const RECT) };
            let _ = unsafe { SetWindowPos(hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE) };
        }
        _ => {}
    }
    let pointer = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const Slot;
    if pointer.is_null() { return unsafe { DefWindowProcW(hwnd, msg, wp, lp) }; }
    if msg == WM_NCDESTROY {
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0); }
        return unsafe { DefWindowProcW(hwnd, msg, wp, lp) };
    }
    // Calls into Shell and native controls may synchronously re-enter this procedure.
    let Ok(mut slot) = (unsafe { &*pointer }).try_borrow_mut() else {
        return unsafe { reentrant(hwnd, msg, wp, lp) };
    };
    let Some(state) = slot.as_mut() else { return unsafe { DefWindowProcW(hwnd, msg, wp, lp) } };
    if state.hwnd.0.is_null() { return unsafe { DefWindowProcW(hwnd, msg, wp, lp) }; }
    let result = unsafe { state.handle(msg, wp, lp) };
    if msg == WM_DESTROY { *slot = None; }
    let live=slot.is_some();
    drop(slot);
    flush_deferred(hwnd,live);
    if DEFERRED_PAINT.with(|d| d.replace(false)) { let _ = unsafe { InvalidateRect(Some(hwnd), None, false) }; }
    result.unwrap_or_else(|| unsafe { DefWindowProcW(hwnd, msg, wp, lp) })
}

/// Hover/press state of the caption buttons. The app paints them without Mica;
/// their measured bounds still produce native nonclient button hit codes.
fn track_caption_buttons(hwnd: HWND, msg: u32, wp: WPARAM) {
    let code = wp.0 as u32;
    let is_button = [HTMINBUTTON, HTMAXBUTTON, HTCLOSE].contains(&code);
    let (old, pressed) = CAPTION_HOVER.with(Cell::get);
    let new = match msg {
        WM_NCMOUSEMOVE => {
            let mut track = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE | TME_NONCLIENT, hwndTrack: hwnd, dwHoverTime: 0 };
            let _ = unsafe { TrackMouseEvent(&mut track) };
            (if is_button { code } else { 0 }, pressed && is_button && code == old)
        }
        WM_NCLBUTTONDOWN => (if is_button { code } else { 0 }, is_button),
        WM_NCLBUTTONUP => (if is_button { code } else { 0 }, false),
        WM_NCMOUSELEAVE | WM_MOUSEMOVE => (0, false),
        _ => return,
    };
    if new != (old, pressed) {
        CAPTION_HOVER.with(|c| c.set(new));
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
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            unsafe { BeginPaint(hwnd, &mut ps); let _ = EndPaint(hwnd, &ps); }
            DEFERRED_PAINT.with(|d| d.set(true));
            LRESULT(0)
        }
        WM_LAYOUT | WM_ACTION | WM_SHELL_NAVIGATED | WM_SHELL_FOLDER_INVOKED | WM_SHELL_NAVIGATION_FAILED | WM_BOOKMARK_PATH_DROPPED
        | WM_OPEN_REQUEST | WM_TREE_NAVIGATE | WM_OPEN_TAB_PATH | WM_SHELL_SELECTION_CHANGED | crate::routing_control::WM_RESULT | WM_FAST_SEARCH | WM_DIAGNOSTIC => {
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
        if [WM_LAYOUT,WM_FAST_SEARCH].contains(&message) && pending.iter().any(|(h,m,_,_)|*h==hwnd && *m==message){return;}
        pending.push_back((hwnd,message,wp,lp));
    });
}
fn flush_deferred(hwnd:HWND,live:bool){
    let messages=DEFERRED_MESSAGES.with(|pending|std::mem::take(&mut *pending.borrow_mut()));
    for(h,message,wp,lp)in messages{
        if h!=hwnd {defer_message(h,message,wp,lp);continue;}
        if live && unsafe{PostMessageW(Some(h),message,wp,lp)}.is_ok(){continue;}
        if lp.0==0{continue;}
        unsafe{match message{
            WM_BOOKMARK_PATH_DROPPED|WM_OPEN_TAB_PATH=>drop(Box::from_raw(lp.0 as *mut PathBuf)),
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
    unsafe { ScreenToClient(hwnd, &mut point).ok()?; }
    caption_button_hit_code(&caption_button_rects(hwnd)?, point.x, point.y)
}

fn caption_button_hit_code(buttons: &[LayoutRect; 3], x: i32, y: i32) -> Option<u32> {
    buttons.iter().position(|button| button.contains(x, y)).map(|index| [HTMINBUTTON, HTMAXBUTTON, HTCLOSE][index])
}

unsafe fn hit_test(hwnd: HWND, lp: LPARAM) -> LRESULT {
    let default = unsafe { DefWindowProcW(hwnd, WM_NCHITTEST, WPARAM(0), lp) };
    if default.0 != HTCLIENT as isize { return default; }
    let mut point = POINT { x: x_lparam(lp.0), y: y_lparam(lp.0) };
    let _ = unsafe { ScreenToClient(hwnd, &mut point) };
    let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
    let (frame_x, frame_y) = frame_size(dpi);
    if !unsafe { IsZoomed(hwnd) }.as_bool() && point.y < frame_y {
        let mut client = RECT::default();
        let _ = unsafe { GetClientRect(hwnd, &mut client) };
        if point.x < frame_x * 2 { return LRESULT(HTTOPLEFT as isize); }
        if point.x >= client.right - frame_x * 2 { return LRESULT(HTTOPRIGHT as isize); }
        return LRESULT(HTTOP as isize);
    }
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
        let point = || POINT { x: x_lparam(lp.0), y: y_lparam(lp.0) };
        match msg {
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
            WM_SIZE => unsafe { self.resize() },
            WM_LAYOUT => unsafe { self.resize() },
            WM_ACTIVATE => {
                let active = (wp.0 & 0xFFFF) as u32 != WA_INACTIVE;
                if !active { self.last_focus = unsafe { GetFocus() }; }
                self.active = active;
                self.relayout();
                self.invalidate_rect(self.layout.strip);
                if active {
                    let target = self.last_focus;
                    if !target.0.is_null() && unsafe { IsWindow(Some(target)) }.as_bool() { let _ = unsafe { SetFocus(Some(target)) }; }
                    else if let Some(b) = &self.browser { unsafe { b.focus(); } }
                }
                return Some(LRESULT(0));
            }
            WM_SETTINGCHANGE => {
                let area = if lp.0 != 0 { unsafe { PCWSTR(lp.0 as *const u16).to_string() }.unwrap_or_default() } else { String::new() };
                if area == "ImmersiveColorSet" {
                    self.apply_theme();
                    self.theme_children();
                    self.icons.clear();
                    self.invalidate();
                }
                if area == "ShellState" || area.is_empty() { self.invalidate(); }
            }
            WM_TIMER => {
                if wp.0 == TIMER_STATUS { self.refresh_status(); }
                if wp.0 == TIMER_SEARCH { let _ = unsafe { KillTimer(Some(self.hwnd), TIMER_SEARCH) }; unsafe { self.live_search(); } }
                if wp.0 == TIMER_FAST_SEARCH { let _=unsafe{KillTimer(Some(self.hwnd),TIMER_FAST_SEARCH)};self.poll_fast_search(); }
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
            WM_LBUTTONDBLCLK => self.mouse_down(point()),
            WM_MBUTTONUP => self.middle_click(point()),
            WM_RBUTTONUP => self.right_click(point()),
            WM_CAPTURECHANGED => { self.tab_drag = None; self.splitter_drag = None; self.pressed = None; self.invalidate(); }
            WM_SETCURSOR => {
                if (lp.0 & 0xFFFF) as u32 == HTCLIENT {
                    let mut p = POINT::default();
                    let _ = unsafe { GetCursorPos(&mut p) };
                    let _ = unsafe { ScreenToClient(self.hwnd, &mut p) };
                    let cursor = match self.layout.hit(p.x, p.y) {
                        Hit::Splitter => Some(IDC_SIZEWE),
                        Hit::Address if !self.address_editing && self.crumb_at(p.x, p.y).is_none() => Some(IDC_IBEAM),
                        _ => None,
                    };
                    if let Some(cursor) = cursor.or(self.splitter_drag.map(|_| IDC_SIZEWE)) {
                        if let Ok(c) = unsafe { LoadCursorW(None, cursor) } { unsafe { SetCursor(Some(c)); } }
                        return Some(LRESULT(1));
                    }
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
                        if id == ID_ADDRESS && code == EN_KILLFOCUS { post(self.hwnd, WM_ACTION, ACTION_END_ADDRESS_EDIT, 0); }
                    }
                    if id == ID_BOOKMARK_RENAME && code == EN_KILLFOCUS { post(self.hwnd, WM_ACTION, ACTION_COMMIT_RENAME, 0); }
                    // Search as you type, after a short pause (like Windows 11).
                    if id == ID_SEARCH && code == EN_CHANGE && unsafe { GetFocus() } == self.search_edit {
                        unsafe { SetTimer(Some(self.hwnd), TIMER_SEARCH, 250, None); }
                    }
                } else {
                    post(self.hwnd, WM_ACTION, id, 0);
                }
                return Some(LRESULT(0));
            }
            WM_ACTION => unsafe { self.action(wp.0, lp.0) },
            WM_SHELL_NAVIGATED => if lp.0 != 0 { unsafe { self.navigation_completed(*Box::from_raw(lp.0 as *mut ShellLocation)); } },
            WM_SHELL_FOLDER_INVOKED => if lp.0 != 0 { unsafe { self.navigate_to(*Box::from_raw(lp.0 as *mut ShellLocation)); } },
            WM_SHELL_NAVIGATION_FAILED => unsafe { self.navigation_failed() },
            WM_SHELL_SELECTION_CHANGED => { unsafe { acknowledge_selection_changed(lp.0); } self.refresh_status(); }
            WM_TREE_NAVIGATE => if lp.0 != 0 {
                let location = unsafe { *Box::from_raw(lp.0 as *mut ShellLocation) };
                if wp.0 != 0 { unsafe { self.new_tab(location); } } else { unsafe { self.navigate_to(location); } }
            },
            WM_BOOKMARK_PATH_DROPPED => if lp.0 != 0 { self.add_bookmark_path(unsafe { *Box::from_raw(lp.0 as *mut PathBuf) }); },
            WM_OPEN_TAB_PATH => if lp.0 != 0 {
                let path = unsafe { *Box::from_raw(lp.0 as *mut PathBuf) };
                let target = if path.is_dir() { path } else { path.parent().map(PathBuf::from).unwrap_or(path) };
                unsafe { self.new_tab(ShellLocation::Path(target)); }
            },
            WM_OPEN_REQUEST => if lp.0 != 0 { unsafe { self.open_requested(*Box::from_raw(lp.0 as *mut crate::instance::OpenRequest)); } },
            WM_FAST_SEARCH => self.poll_fast_search(),
            WM_DIAGNOSTIC=>if lp.0!=0{let request=unsafe{Box::from_raw(lp.0 as *mut crate::instance::DiagnosticRequest)};let _=self.write_diagnostics(&request);},
            crate::routing_control::WM_RESULT => if lp.0!=0{self.show_message(unsafe{*Box::from_raw(lp.0 as *mut String)});},
            message if message >= 0xC000 && message == zsdesk_windows::shell::menu::failure_message() => {
                self.show_message(format!("右键菜单加载失败（0x{:08X}），隐藏设置仍保留。", wp.0 as u32));
            }
            WM_DESTROY => { unsafe { self.destroy(); PostQuitMessage(0); } }
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
        if zones.bookmarks.contains(p.x, p.y) { Some(true) } else if zones.strip.contains(p.x, p.y) { Some(false) } else { None }
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
        let message = if bookmarks { WM_BOOKMARK_PATH_DROPPED } else { WM_OPEN_TAB_PATH };
        for path in hdrop_paths(data).into_iter().take(32) {
            let payload = Box::into_raw(Box::new(path));
            if unsafe { PostMessageW(Some(HWND(self.hwnd as *mut _)), message, WPARAM(0), LPARAM(payload as isize)) }.is_err() {
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
        (FCONTROL | FVIRTKEY, VK_F4, Command::CloseTab),
        (FCONTROL | FSHIFT | FVIRTKEY, VK_T, Command::ReopenTab), (FCONTROL | FVIRTKEY, VK_TAB, Command::NextTab),
        (FCONTROL | FSHIFT | FVIRTKEY, VK_TAB, Command::PreviousTab), (FALT | FVIRTKEY, VK_LEFT, Command::Back),
        (FALT | FVIRTKEY, VK_RIGHT, Command::Forward), (FALT | FVIRTKEY, VK_UP, Command::Up),
        (FCONTROL | FVIRTKEY, VK_L, Command::FocusAddress), (FALT | FVIRTKEY, VK_D, Command::FocusAddress),
        (FVIRTKEY, VK_F4, Command::FocusAddress), (FVIRTKEY, VK_F5, Command::Refresh),
        (FCONTROL | FVIRTKEY, VK_D, Command::AddBookmark), (FCONTROL | FVIRTKEY, VK_F, Command::FocusSearch),
        (FCONTROL | FVIRTKEY, VK_E, Command::FocusSearch), (FVIRTKEY, VK_F3, Command::FocusSearch),
        (FCONTROL | FSHIFT | FVIRTKEY, VK_N, Command::NewFolder), (FCONTROL | FSHIFT | FVIRTKEY, VK_C, Command::CopyPath),
        (FALT | FVIRTKEY, VK_P, Command::TogglePreview), (FALT | FSHIFT | FVIRTKEY, VK_P, Command::ToggleDetails),
    ];
    for n in 1..=9u8 {
        table.push((FCONTROL | FVIRTKEY, VIRTUAL_KEY(VK_0.0 + n as u16), Command::SelectTab(n)));
    }
    table.into_iter().map(|(flags, key, c)| ACCEL { fVirt: flags, key: key.0, cmd: c.id() }).collect()
}

unsafe extern "system" fn edit_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, id: usize, parent: usize) -> LRESULT {
    if msg == WM_KEYDOWN && wp.0 == VK_A.0 as usize && unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0
        && unsafe { GetKeyState(VK_MENU.0 as i32) } >= 0 {
        unsafe { SendMessageW(hwnd, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1))); }
        return LRESULT(0);
    }
    if msg == WM_KEYDOWN && [VK_RETURN.0 as usize, VK_ESCAPE.0 as usize, VK_TAB.0 as usize].contains(&wp.0) {
        let enter = wp.0 == VK_RETURN.0 as usize;
        let action = match (id, wp.0) {
            (_, k) if k == VK_TAB.0 as usize => ACTION_FOCUS_VIEW,
            (ID_SEARCH, _) if enter => ACTION_SEARCH,
            (ID_SEARCH, _) => ACTION_FOCUS_VIEW,
            (ID_ADDRESS, _) if enter => ACTION_COMMIT_ADDRESS,
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
