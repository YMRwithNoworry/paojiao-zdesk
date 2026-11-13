//! Desktop fences host: configuration, fence windows, desktop cooperation,
//! icons, menus and drag-and-drop routing.

use std::{collections::{HashMap, HashSet}, path::{Path, PathBuf}, time::{Duration, Instant}};

use windows::{
    core::{BOOL, PCWSTR},
    Win32::{
        Foundation::{HWND, LPARAM, POINT, POINTL, RECT, WPARAM},
        Graphics::{Direct2D::ID2D1Bitmap, Gdi::{EnumDisplayMonitors, GetMonitorInfoW, MonitorFromRect, HDC, HMONITOR, MONITORINFO,
            MONITORINFOEXW, MONITOR_DEFAULTTONEAREST}, Imaging::IWICBitmapSource},
        System::{Com::IDataObject, Ole::{IDropTarget, DROPEFFECT, DROPEFFECT_MOVE, DROPEFFECT_NONE}, SystemServices::MODIFIERKEYS_FLAGS},
        UI::{
            HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI},
            Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL, VK_SHIFT},
            Shell::{Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIF_SHOWTIP, NIM_ADD, NIM_DELETE, NIM_SETVERSION, NIM_SETFOCUS, NOTIFYICONDATAW, NOTIFYICON_VERSION_4, NOTIFY_ICON_MESSAGE},
            WindowsAndMessaging::*,
        },
    },
};
use zsdesk_core::model::desktop::{auto_layout, categorize, distribute, sort_items, snap, drag_rect, Bounds, Category, DesktopConfig,
    Edges as CoreEdges, Fence, FenceSort, FenceSource, FenceTheme, FenceView, IconSize, ItemInfo, Placement, MIN_FENCE_H, MIN_FENCE_W, TITLE_H};
use zsdesk_ui::{chrome::scale, menus::{entry, menu, submenu, MenuItemSpec, MenuSpec}, theme::{FencePalette, Mode}};
use zsdesk_windows::{desktop::{self, ShellEntry}, gfx::{Gfx, TextStyle}, shell::{icons::{IconLoader, IconRequest}, menu::HistoryAction, notify::ChangeNotify, pidl}};

use super::fence::{FenceHit, FenceWin, Interaction, RenameTarget, SelectionChange, TabMeta};
#[cfg(test)]#[path="layer_regression.rs"]mod layer_regression;

/// Hovering a dragged file over another tab switches to it after this delay.
const TAB_HOVER_SWITCH: Duration = Duration::from_millis(600);

fn zoom_steps(remainder: &mut i32, delta: i32) -> i32 {
    *remainder = remainder.saturating_add(delta);
    let steps = (*remainder / 120).clamp(-16, 16);
    *remainder %= 120;
    steps
}
fn zoom_icon_size(current: IconSize, steps: i32) -> IconSize {
    let index = match current { IconSize::Small => 0i32, IconSize::Medium => 1, IconSize::Large => 2 };
    match (index + steps).clamp(0, 2) { 0 => IconSize::Small, 2 => IconSize::Large, _ => IconSize::Medium }
}
fn desktop_item_position(origin:Bounds,width:i32,height:i32,occupied:&[Bounds])->Bounds {
    let candidate=Bounds::new(origin.x.clamp(0,(width-origin.w).max(0)),origin.y.clamp(0,(height-origin.h).max(0)),origin.w,origin.h);
    let free=|cell:Bounds|occupied.iter().all(|other|cell.right()<=other.x||cell.x>=other.right()||cell.bottom()<=other.y||cell.y>=other.bottom());
    if free(candidate) { return candidate; }
    for x in (0..=(width-origin.w).max(0)).step_by(origin.w.max(1) as usize) {
        for y in (0..=(height-origin.h).max(0)).step_by(origin.h.max(1) as usize) {
            let cell=Bounds::new(x,y,origin.w,origin.h); if free(cell) { return cell; }
        }
    }
    candidate
}

pub const WM_TRAY: u32 = WM_APP + 10;
pub const WM_DESKTOP_CHANGED: u32 = WM_APP + 11;
pub const WM_FOLDER_CHANGED: u32 = WM_APP + 12;
pub const WM_ICON_READY: u32 = WM_APP + 13;
pub const WM_DESKTOP_DBLCLICK: u32 = WM_APP + 14;
pub const WM_RENAME_DONE: u32 = WM_APP + 15;
pub const TIMER_SAVE: usize = 1;
pub const TIMER_DESKTOP: usize = 2;
pub const TIMER_FOLDER: usize = 3;
pub const TIMER_PEEK: usize = 4;
pub const TIMER_ICONS: usize = 5;
/// Fence timer (6 is the interaction-cancel retry in `desktop::fence_proc`).
pub const TIMER_AUTOSCROLL: usize = 7;
pub const FENCE_CLASS: &str = "ZSDesk.Native.Fence";
pub(super) const TRAY_ID:u32=1;

/// Work that must run outside the host borrow (modal menus, Shell UI, drag loops).
pub enum After {
    Nothing,
    ItemMenu { fence: u64, point: POINT },
    FenceMenu { fence: u64, point: POINT },
    FolderSystemMenu { fence: u64, point: POINT },
    TrayMenu(POINT),
    Drag { fence: u64 },
    ReturnToDesktop { fence: u64, point: POINT },
    Open { fence: u64, index: usize, explorer: bool },
    Verb { fence: u64, verb: &'static str, background: bool },
    History { fence: u64, action: HistoryAction },
    Rename { fence: u64, key: String, name: String },
    PickFolder { replace: Option<u64> },
    /// Adds a folder-mapped tab to this fence's group.
    PickFolderTab { to: u64 },
    /// "+" in the title bar: choose the kind of new tab.
    AddTab { fence: u64, point: POINT },
    /// "⌄ N": tabs that do not fit.
    TabList { fence: u64, point: POINT },
    DeleteFence(u64),
    Quit,
    Notice(&'static str),
}

/// Where a drag over a fence would drop.
struct DropSpot { hover: usize, tab: Option<usize>, into: u64, target: Option<IDropTarget>, own: bool }

fn history_shortcut(key: u16, control: bool, shift: bool, alt: bool) -> Option<HistoryAction> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{VK_Y, VK_Z};
    if !control || shift || alt { return None; }
    if key == VK_Z.0 { Some(HistoryAction::Undo) }
    else if key == VK_Y.0 { Some(HistoryAction::Redo) }
    else { None }
}

pub(super) fn fence_startup_command(executable:&Path,config:&Path)->Option<String> {
    let profile=config.parent()?;
    if !executable.is_absolute() || !profile.is_absolute() {return None;}
    let quote=crate::quick_search::control::quote_argument;
    Some(format!("{} --desktop --profile-dir {}",quote(&executable.to_string_lossy()),quote(&profile.to_string_lossy())))
}

fn pending_collection_allowed(config: &DesktopConfig, known: &HashSet<String>, key: &str) -> bool {
    !known.contains(key) && !config.is_released_key(key) && !config.fences.iter().any(|fence| fence.is_desktop()
        && fence.members.iter().any(|member| member.eq_ignore_ascii_case(key)))
}

const MAX_COLD_ICONS: usize = 128;
const MAX_COLD_ICON_BYTES: u64 = 8 * 1024 * 1024;
type IconKey = (String, i32);
struct IconSlot {
    wic: Option<IWICBitmapSource>, bitmap: Option<(u64, ID2D1Bitmap)>, request: Option<u64>,
    signature: (Option<u64>, Option<u64>), last_used: u64, source_bytes: u64, retry_after: Option<Instant>,
}
impl IconSlot {
    fn new(signature: (Option<u64>, Option<u64>), tick: u64) -> Self {
        Self { wic: None, bitmap: None, request: None, signature, last_used: tick, source_bytes: 0, retry_after: None }
    }
    fn pixel_bytes(&self) -> u64 { self.source_bytes.saturating_mul(if self.bitmap.is_some() { 2 } else { 1 }) }
}

/// Visible images form the required working set. Only the reusable cold cache
/// is budgeted; evicting a visible bitmap would cause decode/repaint thrashing.
fn trim_cold_icons(icons: &mut HashMap<IconKey, IconSlot>, visible: &HashSet<IconKey>) {
    let mut cold: Vec<_> = icons.iter().filter(|(key, _)| !visible.contains(*key))
        .map(|(key, slot)| (key.clone(), slot.last_used, slot.pixel_bytes())).collect();
    cold.sort_unstable_by_key(|(_, used, _)| *used);
    let mut bytes: u64 = cold.iter().map(|(_, _, bytes)| *bytes).sum();
    let mut count = cold.len();
    for (key, _, size) in cold {
        if count <= MAX_COLD_ICONS && bytes <= MAX_COLD_ICON_BYTES { break; }
        icons.remove(&key); count -= 1; bytes = bytes.saturating_sub(size);
    }
}

#[derive(Clone)]
pub struct Monitor { pub name: String, pub work: RECT, pub dpi: u32, pub primary: bool }

#[derive(Clone, Copy)]
struct DesktopAnchor { hwnd: HWND, process: u32, thread: u32 }
impl DesktopAnchor {
    fn capture(hwnd: HWND) -> Option<Self> {
        if !unsafe { IsWindow(Some(hwnd)) }.as_bool()
            || !unsafe { IsWindowVisible(hwnd) }.as_bool()
            || !matches!(super::class_name(hwnd).as_str(), "WorkerW" | "Progman") { return None; }
        let mut process = 0;
        let thread = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process)) };
        (process != 0 && thread != 0).then_some(Self { hwnd, process, thread })
    }
    fn valid(self) -> bool {
        Self::capture(self.hwnd).is_some_and(|current| current.process == self.process && current.thread == self.thread)
    }
}

fn window_precedes(upper: HWND, lower: HWND) -> bool {
    let mut window = unsafe { GetWindow(lower, GW_HWNDPREV) }.unwrap_or_default();
    let mut visited = HashSet::new();
    while !window.0.is_null() && visited.insert(window.0 as usize) {
        if window == upper { return true; }
        window = unsafe { GetWindow(window, GW_HWNDPREV) }.unwrap_or_default();
    }
    false
}

fn virtual_screen()->RECT {
    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics,SM_XVIRTUALSCREEN,SM_YVIRTUALSCREEN,SM_CXVIRTUALSCREEN,SM_CYVIRTUALSCREEN};
    let (x,y)=unsafe{(GetSystemMetrics(SM_XVIRTUALSCREEN),GetSystemMetrics(SM_YVIRTUALSCREEN))};
    RECT{left:x,top:y,right:x+unsafe{GetSystemMetrics(SM_CXVIRTUALSCREEN)},bottom:y+unsafe{GetSystemMetrics(SM_CYVIRTUALSCREEN)}}
}
fn intersect(a:RECT,b:RECT)->Option<RECT> {
    let rect=RECT{left:a.left.max(b.left),top:a.top.max(b.top),right:a.right.min(b.right),bottom:a.bottom.min(b.bottom)};
    (rect.right>rect.left&&rect.bottom>rect.top).then_some(rect)
}
fn contains(outer:RECT,inner:RECT)->bool {
    outer.left<=inner.left&&outer.top<=inner.top&&outer.right>=inner.right&&outer.bottom>=inner.bottom
}

fn is_cloaked(hwnd:HWND)->bool {
    use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute,DWMWA_CLOAKED};
    let mut cloaked=0u32;
    unsafe{DwmGetWindowAttribute(hwnd,DWMWA_CLOAKED,(&mut cloaked as *mut u32).cast(),std::mem::size_of::<u32>()as u32)}.is_ok()&&cloaked!=0
}

pub struct DesktopHost {
    pub hwnd: HWND,
    pub config_path: PathBuf,
    pub explorer_config: PathBuf,
    pub config: DesktopConfig,
    pub fences: Vec<FenceWin>,
    desktop_entries: Vec<ShellEntry>,
    pub gfx: Gfx,
    icons: HashMap<IconKey, IconSlot>,
    icon_clock: u64,
    next_icon_request: u64,
    icon_content_active: bool,
    shutting_down: bool,
    desktop_anchor: Option<DesktopAnchor>,
    desktop_exposed: bool,
    loader: IconLoader,
    desktop_notify: Option<ChangeNotify>,
    guard: Option<std::process::Child>,
    /// File names dropped/created for a desktop fence, assigned once they appear.
    pending: Vec<(String, u64, Instant)>,
    known: HashSet<String>,
    pub taskbar_created: u32,
    palette: FencePalette,
    /// Active Shell drop target while dragging over a fence: (fence, item or MAX, target).
    drop_delegate: Option<(u64, usize, IDropTarget)>,
    drop_data: Option<IDataObject>,
    pub internal_drag: Option<(u64, Vec<String>)>,
    pub(super) menu_input_started: Option<(u64,Instant)>,
    tray: bool,
    plugins: zsdesk_plugin::PluginManager,
    pub(super) tray_version4:bool,
    /// Fence whose title bar receives the fence being moved (merge as a tab).
    merge_target: Option<u64>,
    /// Tab under a file drag and when the pointer arrived there.
    tab_hover: Option<(u64, Instant)>,
}

impl DesktopHost {
    pub fn new(hwnd: HWND, config_path: PathBuf, explorer_config: PathBuf, config: DesktopConfig, gfx: Gfx) -> Self {
        let loader = IconLoader::spawn(hwnd, WM_ICON_READY);
        let taskbar_created = unsafe { RegisterWindowMessageW(windows::core::w!("TaskbarCreated")) };
        let plugin_dir = explorer_config.parent().unwrap_or_else(|| Path::new(".")).join("plugins");
        let plugins = zsdesk_plugin::PluginManager::load(&plugin_dir);
        let mut host = Self { hwnd, config_path, explorer_config, config, fences: Vec::new(), desktop_entries: Vec::new(), gfx,
            icons: HashMap::new(), icon_clock: 0, next_icon_request: 1, icon_content_active: false, shutting_down: false, desktop_anchor: None, desktop_exposed:false,
            loader, desktop_notify: None, guard: None, pending: Vec::new(), known: HashSet::new(), menu_input_started:None, plugins,
            taskbar_created, palette: FencePalette::new(Mode::Dark, 55), drop_delegate: None, drop_data: None, internal_drag: None, tray: false,tray_version4:false,
            merge_target: None, tab_hover: None };
        host.update_palette();
        super::fence::set_blur(host.config.settings.blur);
        host
    }

    // ---- lifecycle -------------------------------------------------------

    pub fn start(&mut self) {
        self.add_tray();
        // A previous crash may have left icons hidden while organize mode is off.
        if self.config.settings.icons_hidden_by_zsdesk && !self.config.settings.organize {
            desktop::set_icons_visible(true);
            self.config.settings.icons_hidden_by_zsdesk = false;
            self.save_now();
        }
        if self.config.settings.organize && !self.hide_icons() {
            self.config.settings.organize = false;
        }
        self.register_desktop_notify();
        self.refresh_desktop();
        self.sync_windows();
        for i in 0..self.fences.len() { let id = self.fences[i].id; if self.config.fence(id).is_some_and(|f| !f.is_desktop()) { self.refresh_folder(id); } }
        self.render_all();
    }

    pub fn shutdown(&mut self) {
        if self.shutting_down { return; }
        self.shutting_down = true;
        self.loader.shutdown();
        self.icons.clear();
        self.gfx.release_device_resources();
        self.desktop_notify = None;
        self.save_now();
        if self.config.settings.organize && self.config.settings.icons_hidden_by_zsdesk {
            self.config.settings.icons_hidden_by_zsdesk = !desktop::set_icons_visible(true);
            self.save_now();
        }
        for fence in &mut self.fences { fence.drop = None; fence.notify = None; if !fence.hwnd.0.is_null() { let _ = unsafe { DestroyWindow(fence.hwnd) }; } }
        self.fences.clear();
        self.remove_tray();
        if !self.config.settings.icons_hidden_by_zsdesk {
            if let Some(mut guard) = self.guard.take() { let _ = guard.kill(); let _ = guard.wait(); }
        }
    }

    pub fn accept_icon_notification(&self, token: usize) -> bool {
        !self.shutting_down && self.loader.accepts_notification(token)
    }

    /// The fence skin: only the fence surfaces themselves.
    pub fn fence_dark(&self) -> bool {
        match self.config.settings.theme {
            FenceTheme::Dark => true, FenceTheme::Light => false, FenceTheme::System => zsdesk_windows::theme::system_dark_mode(),
        }
    }

    /// Menus and the tray belong to the application, not to one fence skin:
    /// they follow the application's color scheme (the system's unless the
    /// themed edition chose one), like the file manager's own menus.
    pub fn menu_dark(&self) -> bool {
        use zsdesk_core::model::explorer::ColorScheme;
        let scheme = if crate::edition::THEMED {
            crate::storage::load(&self.explorer_config).map(|session| session.ui.appearance.color_scheme).unwrap_or(ColorScheme::System)
        } else { ColorScheme::System };
        match scheme { ColorScheme::Dark => true, ColorScheme::Light => false, ColorScheme::System => zsdesk_windows::theme::system_dark_mode() }
    }

    pub fn update_palette(&mut self) {
        let dark = self.fence_dark();
        zsdesk_windows::flyout::refresh_system();
        self.palette = FencePalette::new(if dark { Mode::Dark } else { Mode::Light }, self.config.settings.opacity);
        if let Some(accent) = zsdesk_windows::theme::system_accent(dark) { self.palette = self.palette.with_accent(accent); }
    }

    pub fn schedule_save(&self) { unsafe { SetTimer(Some(self.hwnd), TIMER_SAVE, 400, None); } }

    pub fn save_now(&mut self) {
        let _ = unsafe { KillTimer(Some(self.hwnd), TIMER_SAVE) };
        self.config.normalize();
        if let Err(error) = save_config(&self.config_path, &self.config) {
            self.notify_error(&format!("无法保存桌面格子布局：{error}"));
        }
    }

    fn notify_error(&self, text: &str) {
        let text: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
        unsafe { MessageBoxW(Some(self.hwnd), PCWSTR(text.as_ptr()), windows::core::w!("ZSDesk 桌面格子"), MB_OK | MB_ICONWARNING); }
    }

    // ---- tray --------------------------------------------------------------

    pub fn add_tray(&mut self) {
        if self.config.settings.hide_tray || self.tray { return; }
        let mut data = tray_data(self.hwnd);
        data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_SHOWTIP;
        data.uCallbackMessage = WM_TRAY;
        let module = unsafe { windows::Win32::System::LibraryLoader::GetModuleHandleW(None) }.unwrap_or_default();
        data.hIcon = unsafe { LoadIconW(Some(windows::Win32::Foundation::HINSTANCE(module.0)), PCWSTR(101usize as *const u16)) }.unwrap_or_default();
        let tip: Vec<u16> = "ZSDesk 桌面格子".encode_utf16().collect();
        data.szTip[..tip.len()].copy_from_slice(&tip);
        (self.tray,self.tray_version4)=register_tray(&mut data,|message,data|unsafe{Shell_NotifyIconW(message,data)}.as_bool());
    }

    /// Shows the tray icon again (`--desktop --tray`, or when nothing else is visible).
    pub fn show_tray(&mut self) {
        self.config.settings.hide_tray = false;
        self.schedule_save();
        self.add_tray();
    }

    fn remove_tray(&mut self) {
        if self.tray { let data = tray_data(self.hwnd); unsafe { let _ = Shell_NotifyIconW(NIM_DELETE, &data); } self.tray = false; }
        self.tray_version4=false;
    }

    pub(super) fn restore_tray_focus(&self){if self.tray{let data=tray_data(self.hwnd);let _=unsafe{Shell_NotifyIconW(NIM_SETFOCUS,&data)};}}

    /// Explorer restarted: owned fence windows were destroyed with Progman.
    pub fn explorer_restarted(&mut self) {
        self.desktop_anchor = None;
        self.tray = false;
        self.tray_version4=false;
        self.add_tray();
        for fence in &mut self.fences { fence.drop = None; fence.hwnd = HWND::default(); }
        self.fences.clear();
        if self.config.settings.organize { desktop::set_icons_visible(false); }
        self.register_desktop_notify();
        self.refresh_desktop();
        self.sync_windows();
        for id in self.config.fences.iter().filter(|f| !f.is_desktop()).map(|f| f.id).collect::<Vec<_>>() { self.refresh_folder(id); }
        self.render_all();
    }

    // ---- organize mode -----------------------------------------------------

    fn guard_running(&mut self) -> bool { self.guard.as_mut().is_some_and(|g| matches!(g.try_wait(), Ok(None))) }

    /// Starts the recovery watchdog; icons are only hidden once it confirmed.
    fn start_guard(&mut self) -> bool {
        if self.guard_running() { return true; }
        let Ok(exe) = std::env::current_exe() else { return false };
        let pid = std::process::id();
        let name: Vec<u16> = format!("Local\\ZSDesk.DesktopGuard.{pid}").encode_utf16().chain(Some(0)).collect();
        let Ok(event) = (unsafe { windows::Win32::System::Threading::CreateEventW(None, true, false, PCWSTR(name.as_ptr())) }) else { return false };
        let mut command = std::process::Command::new(exe);
        command.arg("--desktop-guard").arg(pid.to_string());
        command.arg("--guard-thread").arg(unsafe{windows::Win32::System::Threading::GetCurrentThreadId()}.to_string());
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
        if let Some(dir) = self.config_path.parent() { command.arg("--profile-dir").arg(dir); }
        let ready = match command.spawn() {
            Ok(child) => { self.guard = Some(child); unsafe { windows::Win32::System::Threading::WaitForSingleObject(event, 4000) }.0 == 0 }
            Err(_) => false,
        };
        let _ = unsafe { windows::Win32::Foundation::CloseHandle(event) };
        if !ready {
            if let Some(mut guard)=self.guard.take(){let _=guard.kill();let _=guard.wait();}
        }
        ready
    }

    fn hide_icons(&mut self) -> bool {
        if !self.start_guard() { return false; }
        self.config.settings.icons_hidden_by_zsdesk = true;
        self.save_now();
        if desktop::set_icons_visible(false) {
            self.config.settings.icons_hidden_by_zsdesk=false;self.save_now();return false;
        }
        true
    }

    pub fn organize(&mut self) {
        if self.config.settings.organize { self.show_all(); return; }
        if !self.hide_icons() {
            self.notify_error("无法启动桌面图标恢复进程，已保留系统桌面图标。");
            return;
        }
        self.config.settings.organize = true;
        let has_desktop = self.config.fences.iter().any(|f| f.is_desktop());
        if !has_desktop {
            if !self.config.stash.is_empty() {
                let stash = std::mem::take(&mut self.config.stash);
                self.config.fences.extend(stash);
            } else {
                self.refresh_desktop_entries();
                self.create_default_fences();
            }
        }
        if !self.config.fences.iter().any(|f| f.is_desktop() && f.catch_all) {
            if let Some(f) = self.config.fences.iter_mut().find(|f| f.is_desktop()) { f.catch_all = true; }
        }
        self.config.settings.hidden = false;
        self.save_now();
        self.refresh_desktop();
        self.sync_windows();
        self.render_all();
    }

    fn create_default_fences(&mut self) {
        let present: HashSet<Category> = self.desktop_entries.iter().map(entry_category).collect();
        let mut categories: Vec<Category> = Category::ALL.iter().copied().filter(|c| *c != Category::Others && present.contains(c)).collect();
        categories.push(Category::Others);
        let monitor = primary_monitor();
        let (w, h) = (unscale(monitor.work.right - monitor.work.left, monitor.dpi), unscale(monitor.work.bottom - monitor.work.top, monitor.dpi));
        let rects = auto_layout(categories.len(), w, h);
        for (category, rect) in categories.into_iter().zip(rects) {
            let id = self.config.take_id();
            let mut fence = Fence::new(id, category.title(), FenceSource::Desktop,
                Placement { monitor: monitor.name.clone(), x: rect.x, y: rect.y, width: rect.w, height: rect.h });
            fence.rule = Some(category);
            fence.catch_all = category == Category::Others;
            self.config.fences.push(fence);
        }
    }

    /// Leaves organize mode: icons return to Explorer, desktop fences are stashed.
    pub fn restore_desktop(&mut self) {
        if !self.config.settings.organize { return; }
        if !desktop::set_icons_visible(true) {
            self.notify_error("系统桌面图标尚未恢复，已保留桌面格子和恢复进程，请稍后重试。");return;
        }
        self.config.settings.organize = false;
        self.config.settings.icons_hidden_by_zsdesk = false;
        let (desktop_fences, rest): (Vec<Fence>, Vec<Fence>) = std::mem::take(&mut self.config.fences).into_iter().partition(|f| f.is_desktop());
        self.config.fences = rest;
        self.config.stash = desktop_fences;
        self.save_now();
        self.sync_windows();
        if let Some(mut guard) = self.guard.take() { let _ = guard.kill(); let _=guard.wait(); }
    }

    pub fn new_desktop_fence(&mut self, at: Option<POINT>) {
        if !self.config.settings.organize { self.organize(); if !self.config.settings.organize { return; } }
        let point = at.unwrap_or_else(|| { let mut p = POINT::default(); let _ = unsafe { GetCursorPos(&mut p) }; p });
        let monitor = monitor_from_point(point);
        let id = self.config.take_id();
        let x = unscale(point.x - monitor.work.left, monitor.dpi) - 40;
        let y = unscale(point.y - monitor.work.top, monitor.dpi) - 10;
        let fence = Fence::new(id, format!("新建格子 {}", self.config.fences.len() + 1), FenceSource::Desktop,
            Placement { monitor: monitor.name, x: x.max(0), y: y.max(0), width: 320, height: 220 });
        self.config.fences.push(fence);
        self.save_now();
        self.sync_windows();
        self.refresh_desktop();
        self.begin_rename(id, RenameTarget::Title);
    }

    pub fn add_folder_fence(&mut self, path: PathBuf, replace: Option<u64>) {
        if let Some(id) = replace {
            if let Some(f) = self.config.fence_mut(id) {
                f.title = zsdesk_core::model::explorer::path_title(&path);
                f.source = FenceSource::Folder { root: path };
                f.browse = None;
            }
            self.save_now();
            self.refresh_folder(id);
            self.render(id);
            return;
        }
        if let Some(existing) = self.config.fences.iter().find(|f| matches!(&f.source, FenceSource::Folder { root } if root == &path)).map(|f| f.id) {
            self.flash(existing);
            return;
        }
        self.create_folder_fence(path);
    }

    fn create_folder_fence(&mut self, path: PathBuf) {
        let monitor = primary_monitor();
        let others: Vec<Bounds> = self.config.fences.iter().filter(|f| f.placement.monitor == monitor.name)
            .map(|f| Bounds::new(f.placement.x, f.placement.y, f.placement.width, f.placement.height)).collect();
        let area_w = unscale(monitor.work.right - monitor.work.left, monitor.dpi);
        let mut x = area_w - 360;
        let mut y = 24;
        while others.iter().any(|o| o.x < x + 340 && o.right() > x && o.y < y + 260 && o.bottom() > y) && y < 2000 { y += 40; x -= 24; }
        let id = self.config.take_id();
        let fence = Fence::new(id, zsdesk_core::model::explorer::path_title(&path), FenceSource::Folder { root: path },
            Placement { monitor: monitor.name, x: x.max(0), y, width: 340, height: 260 });
        self.config.fences.push(fence);
        self.save_now();
        self.sync_windows();
        self.refresh_folder(id);
        self.render(id);
    }

    pub fn delete_fence(&mut self, id: u64) {
        self.config.release_tab(id);
        self.config.fences.retain(|f| f.id != id);
        if self.config.settings.organize && !self.config.fences.iter().any(|f| f.is_desktop() && f.catch_all) {
            if let Some(f) = self.config.fences.iter_mut().find(|f| f.is_desktop()) { f.catch_all = true; }
            else { self.create_catch_all(); }
        }
        self.save_now();
        self.sync_windows();
        self.refresh_desktop();
    }

    fn create_catch_all(&mut self) {
        let monitor = primary_monitor();
        let id = self.config.take_id();
        let mut fence = Fence::new(id, Category::Others.title(), FenceSource::Desktop, Placement { monitor: monitor.name, x: 24, y: 24, width: 340, height: 260 });
        fence.catch_all = true;
        fence.rule = Some(Category::Others);
        self.config.fences.push(fence);
    }

    pub fn show_all(&mut self) {
        // With no fences and no tray icon nothing would be reachable.
        if self.config.fences.is_empty() && self.config.settings.hide_tray { self.show_tray(); }
        self.config.settings.hidden = false;
        for f in &self.fences {
            if self.config.fence(f.id).is_some_and(Fence::is_hidden_tab) { continue; }
            let _ = unsafe { ShowWindow(f.hwnd, SW_SHOWNOACTIVATE) };
            if f.peek { unsafe { SetTimer(Some(f.hwnd), TIMER_PEEK, 250, None); } }
        }
        self.schedule_save();
        self.render_all();
    }

    pub fn toggle_hidden(&mut self) {
        self.config.settings.hidden = !self.config.settings.hidden;
        let show = if self.config.settings.hidden { SW_HIDE } else { SW_SHOWNOACTIVATE };
        for f in &self.fences {
            if f.standalone { continue; }
            if !self.config.settings.hidden && self.config.fence(f.id).is_some_and(Fence::is_hidden_tab) { continue; }
            let _ = unsafe { ShowWindow(f.hwnd, show) };
            if !self.config.settings.hidden && f.peek { unsafe { SetTimer(Some(f.hwnd), TIMER_PEEK, 250, None); } }
        }
        if self.config.settings.hidden { self.release_hidden_graphics(); }
        self.render_all();
        self.schedule_save();
    }

    fn flash(&mut self, id: u64) {
        if self.config.fence(id).is_some_and(Fence::is_hidden_tab) { self.switch_tab(id); }
        if let Some(f) = self.fences.iter().find(|f| f.id == id) {
            let _ = unsafe { ShowWindow(f.hwnd, SW_SHOWNOACTIVATE) };
            let _ = unsafe { SetForegroundWindow(f.hwnd) };
        }
    }

    // ---- windows -----------------------------------------------------------

    /// Creates missing fence windows and destroys windows of removed fences.
    pub fn sync_windows(&mut self) {
        let ids: HashSet<u64> = self.config.fences.iter().chain(self.config.released_items.iter().filter(|_| self.config.settings.organize)).map(|f| f.id).collect();
        self.fences.retain_mut(|w| {
            let keep = ids.contains(&w.id) && unsafe { IsWindow(Some(w.hwnd)) }.as_bool();
            if !keep { w.drop = None; w.notify = None; if !w.hwnd.0.is_null() { let _ = unsafe { DestroyWindow(w.hwnd) }; } }
            keep
        });
        let missing: Vec<u64> = ids.into_iter().filter(|id| !self.fences.iter().any(|w| w.id == *id)).collect();
        let mut failures=Vec::new();
        for id in missing { if let Err(error)=self.create_window(id,false) {failures.push((id,error));break;} }
        if !failures.is_empty() {
            // A failed custom window must never leave its desktop items
            // suppressed behind FWF_NOICONS. Native icons are the recovery
            // surface, and the saved grouping is retained in the stash.
            let hidden_desktop=self.config.settings.organize&&failures.iter().any(|(id,_)|
                self.config.fence(*id).is_some_and(Fence::is_desktop));
            if hidden_desktop {self.restore_desktop();}
            self.notify_error(&format!("无法显示部分桌面项目：{}{}",failures[0].1,
                if hidden_desktop&&!self.config.settings.organize{"；已恢复系统桌面图标，原分组布局仍保留。"}else{""}));
        }
        for i in 0..self.config.fences.len() {
            let id = self.config.fences[i].id;
            if self.config.fences[i].is_desktop() { continue; }
            if self.fences.iter().find(|w| w.id == id).is_some_and(|w| w.entries.is_empty() && w.notify.is_none()) { self.refresh_folder(id); }
        }
        self.apply_visibility();
    }

    /// Shows each group's active tab at the group position and hides the others.
    fn apply_visibility(&mut self) {
        for i in 0..self.fences.len() {
            if self.config.settings.hidden && !self.fences[i].standalone { continue; }
            let id = self.fences[i].id;
            let Some(fence) = self.config.fence(id).cloned() else { continue };
            let hwnd = self.fences[i].hwnd;
            let shown = unsafe { IsWindowVisible(hwnd) }.as_bool();
            if !fence.is_hidden_tab() && !shown {
                self.fences[i].peek = false;
                let r = if self.fences[i].standalone { item_screen_rect(&fence.placement) } else { to_screen(&fence.placement, fence.collapsed) };
                let _ = unsafe { SetWindowPos(hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE) };
                self.fences[i].dpi = unsafe { windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd) }.max(96);
                self.render(id);
                let _ = unsafe { ShowWindow(hwnd, SW_SHOWNOACTIVATE) };
            } else if fence.is_hidden_tab() && shown {
                let win = &mut self.fences[i];
                let _ = unsafe { ShowWindow(hwnd, SW_HIDE) };
                let _ = unsafe { KillTimer(Some(hwnd), TIMER_PEEK) };
                win.peek = false;
                win.surface = None;
                win.hover = FenceHit::None;
            }
        }
    }

    // ---- tabs ----------------------------------------------------------------

    /// Tab chips for `id`'s group, measured for the pill strip (empty for a plain fence).
    fn tab_metas(&self, id: u64, dpi: u32) -> Vec<TabMeta> {
        let ids = self.config.tab_ids(id);
        if ids.len() < 2 { return Vec::new(); }
        let sf = |v: f32| v * dpi as f32 / 96.0;
        let style = TextStyle::ui(sf(13.0)).weight(600);
        ids.iter().filter_map(|t| self.config.fence(*t)).map(|f| {
            let locked = f.locked && f.tab_active;
            let (text, _) = self.gfx.measure(&f.title, &style, sf(400.0));
            let width = (text.ceil() + sf(20.0) + if locked { sf(16.0) } else { 0.0 }) as i32;
            TabMeta { id: f.id, label: f.title.clone(), width: width.clamp(scale(44, dpi), scale(140, dpi)), locked }
        }).collect()
    }

    /// Shows tab `to` in place of its group's current tab.
    pub fn switch_tab(&mut self, to: u64) {
        let Some(from) = self.config.active_tab(to) else { return };
        if from == to || self.config.fence(to).is_none_or(|f| f.group == 0) { return; }
        self.commit_rename_if_any(from);
        self.config.activate_tab(to);
        self.schedule_save();
        let (Some(fi), Some(ti)) = (self.win_index(from), self.win_index(to)) else { self.sync_windows(); return };
        let rect = self.fences[fi].window_rect();
        let (old, new) = (self.fences[fi].hwnd, self.fences[ti].hwnd);
        let peek = std::mem::replace(&mut self.fences[fi].peek, false);
        let blocked = self.fences[fi].peek_blocked;
        let focused = unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetFocus() } == old;
        self.fences[fi].hover = FenceHit::None;
        self.fences[fi].drop_tab = None;
        let _ = unsafe { SetWindowPos(new, None, rect.left, rect.top, rect.right - rect.left, rect.bottom - rect.top, SWP_NOZORDER | SWP_NOACTIVATE) };
        super::set_z(new, old);
        let dpi = unsafe { windows::Win32::UI::HiDpi::GetDpiForWindow(new) }.max(96);
        let tabs = self.tab_metas(to, dpi);
        {
            let win = &mut self.fences[ti];
            win.dpi = dpi;
            win.peek = peek;
            win.peek_blocked = blocked;
            win.tracking = false;
            win.interaction = None;
            win.drop_tab = None;
            win.tabs = tabs;
        }
        if let Some(fence) = self.config.fence(to).cloned() {
            let win = &mut self.fences[ti];
            win.clamp_scroll(&fence);
            let mut cursor = POINT::default();
            let _ = unsafe { GetCursorPos(&mut cursor) };
            let _ = unsafe { windows::Win32::Graphics::Gdi::ScreenToClient(new, &mut cursor) };
            win.hover = win.geometry(&fence).hit(cursor.x, cursor.y, win.dpi, fence.locked, win.collapsed(&fence));
        }
        if !self.config.settings.hidden {
            // Paint before showing so the swap does not flash.
            self.render(to);
            let _ = unsafe { ShowWindow(new, SW_SHOWNOACTIVATE) };
        }
        let _ = unsafe { ShowWindow(old, SW_HIDE) };
        let _ = unsafe { KillTimer(Some(old), TIMER_PEEK) };
        if peek { unsafe { SetTimer(Some(new), TIMER_PEEK, 250, None); } }
        self.fences[fi].surface = None;
        if focused { let _ = unsafe { windows::Win32::UI::Input::KeyboardAndMouse::SetFocus(Some(new)) }; }
        self.prune_icon_cache();
    }

    /// Next (`step` 1) or previous (-1) tab of `id`'s group.
    fn cycle_tab(&mut self, id: u64, step: isize) {
        let ids = self.config.tab_ids(id);
        let Some(at) = ids.iter().position(|t| *t == id) else { return };
        if ids.len() < 2 { return; }
        let next = (at as isize + step).rem_euclid(ids.len() as isize) as usize;
        self.switch_tab(ids[next]);
    }

    pub fn add_desktop_tab(&mut self, to: u64) {
        if !self.config.settings.organize || self.config.fence(to).is_none() { return; }
        let previous = self.config.active_tab(to).unwrap_or(to);
        let id = self.config.take_id();
        self.config.add_tab(to, Fence::new(id, "新建标签", FenceSource::Desktop, Placement::default()));
        self.after_new_tab(previous, id);
        self.refresh_desktop();
        self.begin_rename(id, RenameTarget::Title);
    }

    pub fn add_folder_tab(&mut self, to: u64, path: PathBuf) {
        if self.config.fence(to).is_none() { return; }
        let previous = self.config.active_tab(to).unwrap_or(to);
        let id = self.config.take_id();
        self.config.add_tab(to, Fence::new(id, zsdesk_core::model::explorer::path_title(&path), FenceSource::Folder { root: path }, Placement::default()));
        self.after_new_tab(previous, id);
        self.refresh_folder(id);
        self.render(id);
    }

    fn after_new_tab(&mut self, previous: u64, id: u64) {
        self.save_now();
        self.sync_windows();
        if let (Some(p), Some(n)) = (self.win_index(previous), self.win_index(id)) { super::set_z(self.fences[n].hwnd, self.fences[p].hwnd); }
        self.render_all();
    }

    /// Takes `tab` out of its group as its own fence at `placement`.
    fn detach_tab(&mut self, tab: u64, placement: Placement) {
        if self.config.tab_ids(tab).len() < 2 { return; }
        self.commit_rename_if_any(tab);
        self.config.split_tab(tab, placement);
        self.save_now();
        if let (Some(i), Some(fence)) = (self.win_index(tab), self.config.fence(tab).cloned()) {
            let r = to_screen(&fence.placement, fence.collapsed);
            let win = &mut self.fences[i];
            win.peek = false;
            win.interaction = None;
            let _ = unsafe { SetWindowPos(win.hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE) };
        }
        self.apply_visibility();
        self.render_all();
    }

    fn detach_tab_at(&mut self, tab: u64, screen: POINT) {
        let Some(fence) = self.config.fence(tab).cloned() else { return };
        let monitor = monitor_from_point(screen);
        let x = unscale(screen.x - monitor.work.left, monitor.dpi) - 40;
        let y = unscale(screen.y - monitor.work.top, monitor.dpi) - TITLE_H / 2;
        self.detach_tab(tab, Placement { monitor: monitor.name, x: x.max(0), y: y.max(0), ..fence.placement });
    }

    /// Drops the moved fence `source` (with its tabs) into `target`'s tab group.
    fn merge_fences(&mut self, source: u64, target: u64) {
        let target_hwnd = self.win_index(target).map(|i| self.fences[i].hwnd);
        if !self.config.merge_into(source, target) { return; }
        self.save_now();
        if let (Some(i), Some(fence)) = (self.win_index(source), self.config.fence(source).cloned()) {
            let r = to_screen(&fence.placement, fence.collapsed);
            let hwnd = self.fences[i].hwnd;
            self.fences[i].peek = false;
            let _ = unsafe { SetWindowPos(hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE) };
            if let Some(target) = target_hwnd { super::set_z(hwnd, target); }
        }
        self.apply_visibility();
        self.render_all();
    }

    /// Visible fence (outside `id`'s group) whose title bar is under `screen`.
    fn merge_target_at(&self, id: u64, screen: POINT) -> Option<u64> {
        let own = self.config.tab_ids(id);
        self.fences.iter().filter(|w| !own.contains(&w.id) && unsafe { IsWindowVisible(w.hwnd) }.as_bool()).find(|w| {
            let r = w.window_rect();
            screen.x >= r.left && screen.x < r.right && screen.y >= r.top && screen.y < r.top + scale(TITLE_H, w.dpi)
        }).map(|w| w.id)
    }

    fn set_merge_target(&mut self, id: u64, target: Option<u64>) {
        if self.merge_target == target { return; }
        if let Some(old) = self.merge_target.take() {
            if let Some(i) = self.win_index(old) { self.fences[i].merge_hover = None; self.render(old); }
        }
        let title = self.config.fence(id).map(|f| f.title.clone()).unwrap_or_default();
        if let Some(new) = target {
            if let Some(i) = self.win_index(new) { self.fences[i].merge_hover = Some(title); self.render(new); }
        }
        self.merge_target = target;
        if let Some(i) = self.win_index(id) { self.fences[i].ghost = target.is_some(); }
        self.render(id);
    }

    /// Menu for the "+" button.
    pub fn add_tab_menu(&self) -> MenuSpec {
        let organize = self.config.settings.organize;
        menu(vec![
            entry(if organize { "桌面分组标签" } else { "桌面分组标签（需先一键整理桌面）" }, "new-tab-desktop", organize, false),
            entry("映射文件夹标签…", "new-tab-folder", true, false),
        ])
    }

    /// Every tab of `id`'s group, for the "⌄ N" button.
    pub fn tab_list_menu(&self, id: u64) -> MenuSpec {
        let active = self.config.active_tab(id);
        menu(self.config.tab_ids(id).into_iter().filter_map(|t| self.config.fence(t))
            .map(|f| entry(&f.title.replace('&', "&&"), &format!("switch-tab-{}", f.id), true, Some(f.id) == active)).collect())
    }

    fn create_window(&mut self, id: u64, require_item:bool) -> Result<(),String> {
        let Some(fence) = self.config.fence(id) else { return Err("桌面项目配置不存在".into()) };
        let rect = if self.config.is_released(id) { item_screen_rect(&fence.placement) } else { to_screen(&fence.placement, fence.collapsed) };
        let class: Vec<u16> = FENCE_CLASS.encode_utf16().chain(Some(0)).collect();
        let title: Vec<u16> = fence.title.encode_utf16().chain(Some(0)).collect();
        let owner = desktop::progman();
        let instance = unsafe { windows::Win32::System::LibraryLoader::GetModuleHandleW(None) }.ok().map(|m| windows::Win32::Foundation::HINSTANCE(m.0));
        let hwnd = unsafe { CreateWindowExW(WS_EX_LAYERED | WS_EX_TOOLWINDOW, PCWSTR(class.as_ptr()), PCWSTR(title.as_ptr()), WS_POPUP,
            rect.left, rect.top, rect.right - rect.left, rect.bottom - rect.top, owner, None, instance, None) }.map_err(|error|error.to_string())?;
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, id as isize); }
        let mut win = FenceWin::new(id);
        win.hwnd = hwnd;
        win.standalone = self.config.is_released(id);
        if win.standalone { win.entries = fence.members.iter().filter_map(|key| desktop::entry_from_parsing_name(key)).collect(); }
        if win.standalone {
            if win.entries.len()!=1 {
                let _=unsafe{DestroyWindow(hwnd)};
                return if require_item {Err("桌面项目已移动或不可用".into())}else{Ok(())};
            }
            if let Err(error)=unsafe { SetPropW(hwnd, windows::core::w!("ZSDesk.DesktopItem"), Some(windows::Win32::Foundation::HANDLE(1usize as *mut _))) } {
                let _=unsafe{DestroyWindow(hwnd)};return Err(error.to_string());
            }
        }
        super::fence::apply_glass(hwnd);
        win.dpi = unsafe { windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd) }.max(96);
        // The suggested rectangle used the monitor DPI; recompute if the window got another one.
        let handler = std::rc::Rc::new(super::FenceDrop { fence: id });
        win.drop = unsafe { zsdesk_windows::ole::DropRegistration::register(hwnd, handler) }.ok();
        self.fences.push(win);
        super::set_z(hwnd, HWND_BOTTOM);
        if !self.render(id) {
            if let Some(index)=self.win_index(id) {let mut failed=self.fences.remove(index);failed.drop=None;failed.notify=None;}
            let _=unsafe{DestroyWindow(hwnd)};return Err("桌面图标绘制资源不足".into());
        }
        if (!self.config.settings.hidden || self.config.is_released(id)) && !fence_hidden_tab(&self.config, id) { let _ = unsafe { ShowWindow(hwnd, SW_SHOWNOACTIVATE) }; }
        Ok(())
    }

    /// Places every fence directly above `desktop` (the raised "显示桌面" WorkerW
    /// or Progman), skipping topmost windows so fences never cover applications.
    pub fn raise_above(&mut self, desktop: HWND) {
        self.desktop_exposed=true;
        self.place_above_desktop(desktop);
    }

    fn place_above_desktop(&mut self, desktop: HWND) {
        let Some(mut anchor) = DesktopAnchor::capture(desktop) else { return };
        // Progman may receive activation while a previously raised WorkerW
        // still covers it. Keep the higher verified desktop window as anchor,
        // but only while it still hides the applications beneath it.
        if let Some(previous) = self.desktop_anchor.filter(|value| value.valid()) {
            if window_precedes(previous.hwnd, anchor.hwnd) && self.covers_applications_below(previous.hwnd) { anchor = previous; }
        }
        self.desktop_anchor = Some(anchor);
        // A desktop-class window in the middle of the application band that
        // leaves applications beneath it visible (a leftover Show Desktop
        // surface, a small Shell helper) is not the desktop on screen. Fences
        // above it would cover those applications.
        if !self.covers_applications_below(anchor.hwnd) {
            self.desktop_exposed = false;
            self.place_below_applications(Some(anchor));
            return;
        }
        let desktop = anchor.hwnd;
        let mut after = unsafe { GetWindow(desktop, GW_HWNDPREV) }.unwrap_or_default();
        while !after.0.is_null() && (unsafe { GetWindowLongPtrW(after, GWL_EXSTYLE) } as u32 & WS_EX_TOPMOST.0 != 0 || self.fences.iter().any(|f| f.hwnd == after)) {
            after = unsafe { GetWindow(after, GW_HWNDPREV) }.unwrap_or_default();
        }
        let after = if after.0.is_null() { HWND_TOP } else { after };
        for fence in &self.fences { super::set_z(fence.hwnd, after); }
    }

    pub fn lower_all(&mut self, foreground: HWND) {
        let anchor=self.desktop_anchor.filter(|anchor|anchor.valid())
            .or_else(||desktop::progman().and_then(DesktopAnchor::capture));
        self.desktop_anchor=anchor;
        let class=super::class_name(foreground);
        let transient=foreground.0.is_null() || matches!(class.as_str(),"WorkerW"|"Progman"|"#32768")
            || self.fences.iter().any(|fence|fence.hwnd==foreground)
            || unsafe{GetWindowLongPtrW(foreground,GWL_EXSTYLE)}as u32 & WS_EX_TOPMOST.0!=0;
        // Only a stable, explicit desktop activation grants the exposed layer.
        // A screenshot/menu overlay may temporarily cover that desktop, but it
        // must never turn a background desktop into the exposed desktop.
        if self.desktop_exposed&&transient {
            if let Some(anchor)=anchor {self.place_above_desktop(anchor.hwnd);return;}
        }
        self.desktop_exposed=false;
        self.place_below_applications(anchor);
    }

    /// A visible, on-screen, non-topmost window of another application.
    fn application_window(&self, window: HWND) -> Option<RECT> {
        let mut rect=RECT::default();
        (window!=self.hwnd && !self.fences.iter().any(|fence|fence.hwnd==window)
            && !matches!(super::class_name(window).as_str(),"WorkerW"|"Progman")
            && unsafe{IsWindowVisible(window)}.as_bool() && !unsafe{IsIconic(window)}.as_bool() && !is_cloaked(window)
            && unsafe{GetWindowLongPtrW(window,GWL_EXSTYLE)}as u32 & WS_EX_TOPMOST.0==0
            && unsafe{GetWindowRect(window,&mut rect)}.is_ok() && rect.right>rect.left && rect.bottom>rect.top).then_some(rect)
    }

    /// Whether `desktop` hides every application window below it, so fences
    /// placed directly above it cannot appear over an application.
    fn covers_applications_below(&self, desktop: HWND) -> bool {
        let mut cover=RECT::default();
        if unsafe{GetWindowRect(desktop,&mut cover)}.is_err() { return false; }
        let screen=virtual_screen();
        let mut current=unsafe{GetWindow(desktop,GW_HWNDNEXT)}.unwrap_or_default();let mut seen=HashSet::new();
        while !current.0.is_null()&&seen.len()<4096&&seen.insert(current.0 as usize) {
            // Maximized frames extend past the monitor; only the visible part counts.
            if let Some(rect)=self.application_window(current).and_then(|rect|intersect(rect,screen)) {
                if !contains(cover,rect) { return false; }
            }
            current=unsafe{GetWindow(current,GW_HWNDNEXT)}.unwrap_or_default();
        }
        true
    }

    /// Follow the bottom of the whole normal application band, not the
    /// current foreground HWND. HWND_NOTOPMOST inserts at the *front* of
    /// that band and made fences cover applications after overlay dismissal.
    fn place_below_applications(&mut self, anchor: Option<DesktopAnchor>) {
        let mut after=None;let mut current=unsafe{GetTopWindow(None)}.unwrap_or_default();let mut seen=HashSet::new();
        while !current.0.is_null()&&seen.len()<4096&&seen.insert(current.0 as usize) {
            if self.application_window(current).is_some() {after=Some(current);}
            current=unsafe{GetWindow(current,GW_HWNDNEXT)}.unwrap_or_default();
        }
        if after.is_none(){if let Some(anchor)=anchor{self.place_above_desktop(anchor.hwnd);return;}}
        let after=after.unwrap_or(HWND_BOTTOM);
        for fence in &self.fences { super::set_z(fence.hwnd, after); }
    }

    /// Owner activation and popup menus can reorder Progman's owned windows
    /// without a foreground event from another application.
    pub fn repair_layer(&mut self) {
        let anchor = self.desktop_anchor.filter(|value| value.valid())
            .or_else(|| desktop::progman().and_then(DesktopAnchor::capture));
        let foreground = unsafe { GetForegroundWindow() };
        let class = super::class_name(foreground);
        self.desktop_anchor=anchor;
        if matches!(class.as_str(),"WorkerW"|"Progman") {
            // Screenshot and task-switching windows can briefly activate the
            // shell while closing. Wait for the foreground to settle first.
            super::schedule_desktop_raise(self.hwnd);
        }
        self.lower_all(foreground);
    }

    pub fn place_all(&mut self) {
        for i in 0..self.fences.len() {
            let id = self.fences[i].id;
            let Some(fence) = self.config.fence(id) else { continue };
            let r = if self.fences[i].standalone { item_screen_rect(&fence.placement) } else { to_screen(&fence.placement, self.fences[i].collapsed(fence)) };
            let _ = unsafe { SetWindowPos(self.fences[i].hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE) };
            self.fences[i].dpi = unsafe { windows::Win32::UI::HiDpi::GetDpiForWindow(self.fences[i].hwnd) }.max(96);
        }
        self.render_all();
    }

    pub fn win_index(&self, id: u64) -> Option<usize> { self.fences.iter().position(|w| w.id == id) }

    pub fn render_all(&mut self) {
        let ids: Vec<u64> = self.fences.iter().map(|w| w.id).collect();
        for id in ids { self.render(id); }
    }

    pub fn render(&mut self, id: u64) -> bool {
        if self.shutting_down { return false; }
        if self.config.settings.hidden && !self.config.is_released(id) { return true; }
        let Some(index) = self.win_index(id) else { return false };
        let Some(fence) = self.config.fence(id).cloned() else { return false };
        if fence.is_hidden_tab() { self.fences[index].surface = None; return true; }
        let tabs = self.tab_metas(id, self.fences[index].dpi);
        let mut win = std::mem::replace(&mut self.fences[index], FenceWin::new(id));
        win.tabs = tabs;
        let palette = self.palette;
        let show_counts = self.config.settings.show_counts;
        let generation = self.gfx.generation();
        self.icon_clock = self.icon_clock.wrapping_add(1);
        let tick = self.icon_clock;
        let (gfx, icons, loader, next_request) = (&self.gfx, &mut self.icons, &self.loader, &mut self.next_icon_request);
        let presented=win.render(gfx, &fence, &palette, show_counts, |entry, px| {
            let key = (entry.key.clone(), px);
            let signature = (entry.size, entry.modified);
            let slot = icons.entry(key).or_insert_with(|| IconSlot::new(signature, tick));
            if slot.signature != signature { *slot = IconSlot::new(signature, tick); }
            slot.last_used = tick;
            if slot.wic.is_none() && slot.request.is_none() && slot.retry_after.is_none_or(|t| Instant::now() >= t) {
                let thumbnail = !entry.is_folder && !entry.is_link && matches!(entry_category(entry), Category::Images | Category::Media | Category::Documents);
                let token = *next_request;
                *next_request = next_request.wrapping_add(1).max(1);
                if loader.request(IconRequest { key: entry.key.clone(), pidl: entry.pidl.clone(), size: px as u32, thumbnail, token }) { slot.request = Some(token); }
            }
            if slot.bitmap.as_ref().is_none_or(|(g, _)| *g != generation) {
                slot.bitmap = slot.wic.as_ref().and_then(|w| gfx.upload(w)).map(|b| (generation, b));
            }
            slot.bitmap.as_ref().map(|(_, b)| b.clone())
        });
        self.fences[index] = win;
        self.prune_icon_cache();
        presented
    }

    pub fn icons_ready(&mut self) {
        let ready = self.loader.take_ready();
        let mut accepted = false;
        for result in ready {
            let Some(slot) = self.icons.get_mut(&(result.key.clone(), result.size as i32)) else { continue };
            if slot.request != Some(result.token) { continue; }
            slot.request = None;
            slot.wic = (!result.bitmap.is_invalid()).then(|| self.gfx.wic_from_hbitmap(result.bitmap, result.alpha)).flatten();
            slot.bitmap = None;
            slot.source_bytes = slot.wic.as_ref().map(|wic| { let (w,h) = Gfx::wic_size(wic); (u64::from(w) * u64::from(h) * 4).max(result.pixel_bytes) }).unwrap_or(0);
            slot.retry_after = slot.wic.is_none().then(|| Instant::now() + Duration::from_secs(30));
            accepted = true;
        }
        // One repaint per batch also retries visible requests rejected by a full queue.
        if accepted { self.render_all(); }
        self.prune_icon_cache();
    }

    pub fn invalidate_changed_icons(&mut self, first: Option<&str>, second: Option<&str>) {
        let paths: Vec<_> = [first, second].into_iter().flatten().map(|p| p.trim_end_matches('\\').to_lowercase()).collect();
        self.icons.retain(|(key, _), _| {
            if paths.is_empty() { return false; }
            let key = key.to_lowercase();
            !paths.iter().any(|p| key == *p || key.strip_prefix(p).is_some_and(|tail| tail.starts_with('\\')))
        });
        self.prune_icon_cache();
    }

    fn visible_icon_keys(&self) -> HashSet<IconKey> {
        let mut keys = HashSet::new();
        for win in &self.fences {
            if self.config.settings.hidden && !win.standalone { continue; }
            let Some(fence) = self.config.fence(win.id) else { continue };
            if win.collapsed(fence) || fence.is_hidden_tab() { continue; }
            let geometry = win.geometry(fence);
            let px = if fence.view == FenceView::List { scale(20, win.dpi) } else { geometry.icon_px };
            for (index, cell) in geometry.cells.iter().enumerate() {
                if cell.cell.bottom() >= geometry.content.y && cell.cell.y <= geometry.content.bottom() {
                    if let Some(entry) = win.entries.get(index) { keys.insert((entry.key.clone(), px)); }
                }
            }
        }
        keys
    }

    fn prune_icon_cache(&mut self) {
        let active = self.fences.iter().any(|w| (!self.config.settings.hidden || w.standalone) && self.config.fence(w.id).is_some_and(|f| !w.collapsed(f) && !f.is_hidden_tab()));
        if !active {
            self.icons.clear();
            self.loader.retain_requests(&HashSet::new());
            if self.icon_content_active { self.gfx.release_device_resources(); }
        } else {
            let visible = self.visible_icon_keys();
            trim_cold_icons(&mut self.icons, &visible);
            let tokens = self.icons.values().filter_map(|s| s.request).collect();
            self.loader.retain_requests(&tokens);
        }
        self.icon_content_active = active;
    }

    fn release_hidden_graphics(&mut self) {
        if self.fences.iter().any(|window| window.standalone) {
            for window in self.fences.iter_mut().filter(|window|!window.standalone) {
                window.surface=None; let _=unsafe { KillTimer(Some(window.hwnd),TIMER_PEEK) };
            }
            self.prune_icon_cache(); return;
        }
        self.icons.clear();
        self.loader.retain_requests(&HashSet::new());
        self.gfx.release_device_resources();
        for win in &mut self.fences {
            win.surface = None;
            let _ = unsafe { KillTimer(Some(win.hwnd), TIMER_PEEK) };
        }
        self.icon_content_active = false;
    }

    /// Resource accounting; pixel bytes are allocations, not process working set.
    pub fn resource_snapshot(&self) -> serde_json::Value {
        let visible = self.visible_icon_keys();
        let cold: Vec<_> = self.icons.iter().filter(|(key, _)| !visible.contains(*key)).map(|(_,slot)| slot).collect();
        let queue = self.loader.stats();
        let (formats, target) = self.gfx.cache_stats();
        let windows: Vec<_> = self.fences.iter().map(|win| {
            let rect = win.window_rect();
            let fence = self.config.fence(win.id);
            serde_json::json!({"id":win.id,"title":fence.map(|f|f.title.as_str()),"entry_count":win.entries.len(),
                "group":fence.map(|f|f.group),"hidden_tab":fence.map(Fence::is_hidden_tab),
                "collapsed":fence.map(|f|win.collapsed(f)),"peek":win.peek,"dpi":win.dpi,
                "rect":{"left":rect.left,"top":rect.top,"right":rect.right,"bottom":rect.bottom},
                "surface":win.surface.as_ref().map(|s|serde_json::json!({"width":s.width,"height":s.height,"pixel_bytes":s.pixel_bytes()}))})
        }).collect();
        serde_json::json!({"process_id":std::process::id(),"hidden":self.config.settings.hidden,"organize":self.config.settings.organize,
            "fences":self.fences.len(),"windows":windows,"surface_pixel_bytes":self.fences.iter().filter_map(|w|w.surface.as_ref()).map(|s|s.pixel_bytes()).sum::<u64>(),
            "icon_entries":self.icons.len(),"icon_pixel_bytes":self.icons.values().map(IconSlot::pixel_bytes).sum::<u64>(),
            "visible_icon_keys":visible.len(),"cold_icon_entries":cold.len(),"cold_icon_pixel_bytes":cold.iter().map(|s|s.pixel_bytes()).sum::<u64>(),
            "cold_entry_limit":MAX_COLD_ICONS,"cold_pixel_limit":MAX_COLD_ICON_BYTES,"pending_icon_requests":queue.pending,"queued_icon_requests":queue.queued,
            "ready_icon_results":queue.ready,"ready_icon_pixel_bytes":queue.ready_pixel_bytes,"text_formats":formats,"render_target_live":target,"imaging_factory_live":self.gfx.imaging_factory_live(),"context_menu":super::menu_diagnostics()})
    }

    pub fn write_diagnostics(&self, path: &Path) -> anyhow::Result<()> {
        let path = std::path::absolute(path)?;
        let normalize = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()).to_string_lossy().to_lowercase();
        anyhow::ensure!(normalize(&path) != normalize(&self.config_path) && normalize(&path) != normalize(&self.explorer_config),
            "诊断输出不能覆盖程序配置");
        if let Some(parent) = path.parent() { std::fs::create_dir_all(parent)?; }
        let temporary = path.with_extension("diagnostics.tmp");
        std::fs::write(&temporary, serde_json::to_vec_pretty(&self.resource_snapshot())?)?;
        std::fs::rename(temporary, path)?;
        Ok(())
    }

    // ---- content -----------------------------------------------------------

    pub fn register_desktop_notify(&mut self) {
        self.desktop_notify = None;
        let folders: Vec<Vec<u8>> = [desktop::user_desktop(), desktop::public_desktop()].into_iter().flatten()
            .filter_map(|p| pidl::location_pidl(&zsdesk_core::model::explorer::ShellLocation::Path(p))).chain(Some(vec![0, 0])).collect();
        self.desktop_notify = ChangeNotify::register(self.hwnd, WM_DESKTOP_CHANGED, &folders, false);
    }

    fn refresh_desktop_entries(&mut self) {
        self.desktop_entries = desktop::enumerate_desktop();
        let mut known: HashSet<String> = self.desktop_entries.iter().map(|e| e.key.to_lowercase()).collect();
        for key in self.config.fences.iter().chain(&self.config.released_items).filter(|f| f.is_desktop()).flat_map(|f| &f.members) {
            if known.contains(&key.to_lowercase()) { continue; }
            if let Some(entry) = desktop::entry_from_parsing_name(key) {
                if let Some(existing) = self.desktop_entries.iter_mut().find(|e| e.pidl == entry.pidl) {
                    known.remove(&existing.key.to_lowercase());
                    existing.key = key.clone();
                    known.insert(key.to_lowercase());
                } else if known.insert(entry.key.to_lowercase()) { self.desktop_entries.push(entry); }
            }
        }
    }

    /// Re-reads desktop items and distributes them into desktop fences.
    pub fn refresh_desktop(&mut self) {
        self.refresh_desktop_entries();
        // Items created/dropped for a specific fence join it once they appear.
        self.pending.retain(|(name, _, t)| t.elapsed() < Duration::from_secs(if name == "*" { 10 } else { 30 }));
        let mut assignments: Vec<(u64, String)> = Vec::new();
        for entry in &self.desktop_entries {
            // A restored or temporarily unavailable member keeps its saved
            // group even while another fence is collecting newly created files.
            if !pending_collection_allowed(&self.config, &self.known, &entry.key) { continue; }
            let file = entry.path.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().to_lowercase());
            if let Some(index) = self.pending.iter().position(|(name, _, _)| file.as_deref() == Some(name.as_str()) || name == "*") {
                let (name, fence, _) = self.pending[index].clone();
                if name != "*" { self.pending.remove(index); }
                assignments.push((fence, entry.key.clone()));
            }
        }
        let first_run = self.known.is_empty();
        self.known = self.desktop_entries.iter().map(|e| e.key.clone()).collect();
        if !first_run {
            for (fence, key) in assignments { self.config.assign(fence, &[key], None); self.schedule_save(); }
        }
        let infos: Vec<ItemInfo> = self.desktop_entries.iter().map(item_info).collect();
        let groups = self.config.distributed_items(&infos);
        for (fence, indices) in self.config.fences.iter().zip(groups) {
            if !fence.is_desktop() { continue; }
            if let Some(win) = self.fences.iter_mut().find(|w| w.id == fence.id) {
                win.entries = indices.into_iter().map(|i| self.desktop_entries[i].clone()).collect();
                let keys: HashSet<String> = win.entries.iter().map(|e| e.key.clone()).collect();
                win.selection.retain(|k| keys.contains(k));
                win.focus = win.focus.filter(|&f| f < win.entries.len());
                win.clamp_scroll(fence);
            }
        }
        for item in &self.config.released_items {
            if let Some(win) = self.fences.iter_mut().find(|win| win.id == item.id) {
                let entries: Vec<_> = self.desktop_entries.iter().filter(|entry| item.members.iter().any(|key| key.eq_ignore_ascii_case(&entry.key))).cloned().collect();
                win.entries = entries;
                win.selection.retain(|key| win.entries.iter().any(|entry| entry.key == *key));
                win.focus = win.focus.filter(|index| *index < win.entries.len());
            }
        }
        let ids: Vec<u64> = self.config.fences.iter().chain(&self.config.released_items).filter(|f| f.is_desktop()).map(|f| f.id).collect();
        for id in ids { self.render(id); }
    }

    /// Publish only items that already exist. This changes visual membership,
    /// never deletes, copies or moves filesystem objects.
    pub(super) fn release_desktop_keys(&mut self, keys: &[String], point: POINT) -> Result<usize,String> {
        if !self.config.settings.organize {
            return if desktop::icons_visible(){Ok(keys.len())}else{Err("系统桌面图标当前不可见，未完成移回桌面。".into())};
        }
        let original=self.config.clone();
        let original_ids:HashSet<u64>=self.fences.iter().map(|window|window.id).collect();
        let monitor = monitor_from_point(point);
        let width = 96;
        let height = 104;
        let work_width=unscale(monitor.work.right-monitor.work.left,monitor.dpi);
        let work_height=unscale(monitor.work.bottom-monitor.work.top,monitor.dpi);
        let origin=Bounds::new(unscale(point.x-monitor.work.left,monitor.dpi),unscale(point.y-monitor.work.top,monitor.dpi),width,height);
        let mut occupied:Vec<Bounds>=self.config.fences.iter().chain(&self.config.released_items)
            .filter(|item|item.placement.monitor==monitor.name && !item.is_hidden_tab()
                && !item.members.iter().any(|member|keys.iter().any(|key|member.eq_ignore_ascii_case(key))))
            .map(|item|Bounds::new(item.placement.x,item.placement.y,item.placement.width,if item.collapsed{TITLE_H}else{item.placement.height})).collect();
        let mut planned=Vec::new();let mut unique=HashSet::new();
        for key in keys {
            if !unique.insert(key.to_lowercase()){continue;}
            if desktop::entry_from_parsing_name(key).is_none() {
                self.config=original;self.refresh_desktop();
                let unseen=keys.iter().any(|key|!self.fences.iter().any(|window|
                    unsafe{IsWindowVisible(window.hwnd)}.as_bool()&&window.entries.iter().any(|entry|entry.key.eq_ignore_ascii_case(key))));
                if unseen&&self.config.settings.icons_hidden_by_zsdesk {self.restore_desktop();}
                return Err("所选项目已移动或不可用，未更改分组。".into());
            }
            let position=desktop_item_position(origin,work_width,work_height,&occupied);
            let placement = Placement { monitor: monitor.name.clone(), x:position.x, y:position.y,width,height };
            planned.push(self.config.release_item(key.clone(), placement));
            occupied.push(position);
        }
        let publish=(||->Result<(),String>{
            // All new windows must allocate and present successfully before
            // membership is saved or the old groups are repainted.
            for &id in &planned {
                if self.win_index(id).is_none(){self.create_window(id,true)?;}
            }
            self.place_all();self.refresh_desktop();
            for &id in &planned {
                let Some(index)=self.win_index(id)else{return Err("桌面图标窗口未能创建".into());};
                if self.fences[index].entries.len()!=1 || !self.render(id)
                    || !unsafe{IsWindowVisible(self.fences[index].hwnd)}.as_bool() {
                    return Err("桌面图标未能显示，分组调整已撤回。".into());
                }
            }
            self.config.normalize();
            save_config(&self.config_path,&self.config).map_err(|error|format!("无法保存桌面布局：{error}"))?;
            Ok(())
        })();
        if let Err(error)=publish {
            self.config=original;
            self.fences.retain_mut(|window|{
                if original_ids.contains(&window.id){return true;}
                window.drop=None;window.notify=None;
                if !window.hwnd.0.is_null(){let _=unsafe{DestroyWindow(window.hwnd)};}
                false
            });
            self.place_all();self.refresh_desktop();
            // A moved file may not have existed in the old desktop snapshot.
            // If neither a surviving group nor a loose icon represents it,
            // restore the native recovery surface rather than hide the file.
            let unseen=keys.iter().any(|key|!self.fences.iter().any(|window|
                unsafe{IsWindowVisible(window.hwnd)}.as_bool()&&window.entries.iter().any(|entry|entry.key.eq_ignore_ascii_case(key))));
            if unseen&&self.config.settings.icons_hidden_by_zsdesk {self.restore_desktop();}
            return Err(error);
        }
        let _=unsafe{KillTimer(Some(self.hwnd),TIMER_SAVE)};
        self.repair_layer();Ok(planned.len())
    }

    pub fn refresh_folder(&mut self, id: u64) {
        let Some(fence) = self.config.fence(id).cloned() else { return };
        let Some(path) = fence.folder().cloned() else { return };
        let Some(index) = self.win_index(id) else { return };
        let result = desktop::enumerate_folder(&path);
        let win = &mut self.fences[index];
        let path_changed = win.notify_path.as_ref() != Some(&path);
        let focused_key = win.focus.and_then(|i| win.entries.get(i)).map(|e| e.key.clone());
        let anchor_key = win.anchor.and_then(|i| win.entries.get(i)).map(|e| e.key.clone());
        let (entries, message) = match result {
            Ok(entries) => {
                let infos: Vec<ItemInfo> = entries.iter().map(item_info).collect();
                let mut order: Vec<usize> = (0..entries.len()).collect();
                sort_items(&fence, &infos, &mut order);
                let mut slots: Vec<_> = entries.into_iter().map(Some).collect();
                (order.into_iter().map(|i| slots[i].take().unwrap()).collect(), None)
            }
            Err(_) => (Vec::new(), Some(format!("文件夹不可用：{}", path.display()))),
        };
        let changed = path_changed || entries != win.entries || message != win.message;
        if changed {
            win.entries = entries; win.message = message;
            let keys: HashSet<String> = win.entries.iter().map(|e| e.key.clone()).collect();
            win.selection.retain(|k| keys.contains(k));
            win.focus = focused_key.and_then(|key| win.entries.iter().position(|e| e.key == key));
            win.anchor = anchor_key.and_then(|key| win.entries.iter().position(|e| e.key == key));
            win.clamp_scroll(&fence);
        }
        if path_changed || win.notify.is_none() {
            win.notify = pidl::location_pidl(&zsdesk_core::model::explorer::ShellLocation::Path(path.clone()))
                .and_then(|p| ChangeNotify::register(win.hwnd, WM_FOLDER_CHANGED, &[p], false));
            win.notify_path = Some(path);
        }
        let missing_visible_icon = if !self.config.settings.hidden && !win.collapsed(&fence) {
            let geometry = win.geometry(&fence);
            let px = if fence.view == FenceView::List { scale(20, win.dpi) } else { geometry.icon_px };
            geometry.cells.iter().enumerate().any(|(i, cell)| cell.cell.bottom() >= geometry.content.y && cell.cell.y <= geometry.content.bottom()
                && win.entries.get(i).is_some_and(|e| !self.icons.contains_key(&(e.key.clone(), px))))
        } else { false };
        if changed || missing_visible_icon { self.render(id); }
    }

    pub fn rename_notified(&mut self, old: &str, new: &str) {
        if self.config.rename_member(old, new) { self.schedule_save(); }
    }

    // ---- placement ---------------------------------------------------------

    fn store_rect(&mut self, id: u64, rect: RECT) {
        let Some(fence) = self.config.fence_mut(id) else { return };
        let peeking = self.fences.iter().any(|w| w.id == id && w.peek);
        let keep_height = (fence.collapsed && !peeking).then_some(fence.placement.height);
        fence.placement = from_screen(rect, keep_height);
        self.config.sync_group(id);
        self.schedule_save();
    }

    fn other_bounds(&self, id: u64) -> Vec<Bounds> {
        self.fences.iter().filter(|w| w.id != id && unsafe { IsWindowVisible(w.hwnd) }.as_bool()).map(|w| {
            let r = w.window_rect();
            Bounds::new(r.left, r.top, r.right - r.left, r.bottom - r.top)
        }).collect()
    }

    // ---- input -------------------------------------------------------------

    pub fn mouse_move(&mut self, id: u64, point: POINT) -> After {
        let Some(index) = self.win_index(id) else { return After::Nothing };
        let Some(fence) = self.config.fence(id).cloned() else { return After::Nothing };
        let win = &mut self.fences[index];
        if !win.tracking {
            let mut track = windows::Win32::UI::Input::KeyboardAndMouse::TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<windows::Win32::UI::Input::KeyboardAndMouse::TRACKMOUSEEVENT>() as u32,
                dwFlags: windows::Win32::UI::Input::KeyboardAndMouse::TME_LEAVE, hwndTrack: win.hwnd, dwHoverTime: 0 };
            win.tracking = unsafe { windows::Win32::UI::Input::KeyboardAndMouse::TrackMouseEvent(&mut track) }.is_ok();
        }
        let mut screen = point;
        let _ = unsafe { windows::Win32::Graphics::Gdi::ClientToScreen(win.hwnd, &mut screen) };
        match win.interaction.clone() {
            Some(Interaction::Move { cursor, rect }) | Some(Interaction::Resize { cursor, rect, .. }) => {
                let edges = match &win.interaction { Some(Interaction::Resize { edges, .. }) => CoreEdges { left: edges.left, top: edges.top, right: edges.right, bottom: edges.bottom }, _ => CoreEdges::MOVE };
                let start = Bounds::new(rect.left, rect.top, rect.right - rect.left, rect.bottom - rect.top);
                let dpi = win.dpi;
                let s = |v: i32| scale(v, dpi);
                let rolled_up = fence.collapsed && !win.peek;
                let moved = drag_rect(start, edges, screen.x - cursor.x, screen.y - cursor.y, s(MIN_FENCE_W), if rolled_up { start.h } else { s(MIN_FENCE_H) });
                let monitor = monitor_from_point(screen);
                let area = Bounds::new(monitor.work.left, monitor.work.top, monitor.work.right - monitor.work.left, monitor.work.bottom - monitor.work.top);
                let hwnd = win.hwnd;
                let others = self.other_bounds(id);
                let target = if edges.is_move() { self.merge_target_at(id, screen) } else { None };
                // Over another fence's title bar the fence follows the pointer freely.
                let placed = if target.is_some() { moved } else { snap(moved, edges, &others, area, s(10), s(12)) };
                let _ = unsafe { SetWindowPos(hwnd, None, placed.x, placed.y, placed.w, placed.h, SWP_NOZORDER | SWP_NOACTIVATE) };
                if !edges.is_move() { if let Some(f) = self.config.fence(id).cloned() { self.fences[index].clamp_scroll(&f); } }
                if edges.is_move() { self.set_merge_target(id, target); }
                self.render(id);
                return After::Nothing;
            }
            Some(Interaction::TabPress { tab, cursor }) => {
                let threshold = unsafe { GetSystemMetrics(SM_CXDRAG) };
                if !fence.locked && ((point.x - cursor.x).abs() > threshold || (point.y - cursor.y).abs() > threshold) {
                    win.interaction = Some(Interaction::TabDrag { tab, torn: false });
                    self.render(id);
                }
                return After::Nothing;
            }
            Some(Interaction::TabDrag { tab, .. }) => {
                let g = win.geometry(&fence);
                let margin = scale(24, win.dpi);
                let torn = point.x < -margin || point.x > g.width + margin || point.y < -margin || point.y > g.title.height + margin;
                if !torn {
                    let before = g.tabs.iter().find(|(_, r)| point.x < r.x + r.width / 2).map(|(i, _)| win.tabs[*i].id);
                    let order = self.config.tab_ids(id);
                    let current = order.iter().position(|t| *t == tab).and_then(|at| order.get(at + 1).copied());
                    if before != Some(tab) && before != current { self.config.move_tab(tab, before); self.schedule_save(); }
                }
                if let Some(i) = self.win_index(id) { self.fences[i].interaction = Some(Interaction::TabDrag { tab, torn }); }
                self.render(id);
                return After::Nothing;
            }
            Some(Interaction::Press { index: item, cursor }) => {
                let threshold = unsafe { GetSystemMetrics(SM_CXDRAG) };
                if (point.x - cursor.x).abs() > threshold || (point.y - cursor.y).abs() > threshold {
                    win.interaction = None;
                    if !win.selection.contains(&win.entries[item].key) {
                        win.selection = HashSet::from([win.entries[item].key.clone()]);
                    }
                    return After::Drag { fence: id };
                }
                return After::Nothing;
            }
            Some(Interaction::Rubber { .. }) => {
                self.update_rubber(id, point);
                return After::Nothing;
            }
            Some(Interaction::Thumb { cursor_y, scroll }) => {
                let g = win.geometry(&fence);
                // Same track as Geometry::new: the thumb follows the pointer 1:1.
                let track = (g.content.height - scale(8, win.dpi) - g.thumb.height).max(1);
                win.scroll = (scroll + ((point.y - cursor_y) as i64 * g.max_scroll as i64 / track as i64) as i32).clamp(0, g.max_scroll);
                self.render(id);
                return After::Nothing;
            }
            None => {}
        }
        if fence.collapsed && !win.peek && !win.peek_blocked && self.config.settings.hover_expand {
            self.peek(id, true);
            return After::Nothing;
        }
        let win = &mut self.fences[index];
        let g = win.geometry(&fence);
        let hit = g.hit(point.x, point.y, win.dpi, fence.locked, win.collapsed(&fence));
        if hit != win.hover { win.hover = hit; self.render(id); }
        After::Nothing
    }

    /// Extends the rubber band to `point` (client coordinates). The band's
    /// anchor is kept in content coordinates, so scrolling while selecting
    /// keeps the items it already covers. Beyond the top or bottom edge the
    /// content scrolls, and keeps scrolling on a timer while the pointer rests.
    fn update_rubber(&mut self, id: u64, point: POINT) {
        let Some(index) = self.win_index(id) else { return };
        let Some(fence) = self.config.fence(id).cloned() else { return };
        let win = &mut self.fences[index];
        let Some(Interaction::Rubber { start, base, .. }) = win.interaction.clone() else { return };
        let g = win.geometry(&fence);
        let step = scale(12, win.dpi);
        let scroll = if point.y > g.content.bottom() { (win.scroll + step).min(g.max_scroll) }
            else if point.y < g.content.y { (win.scroll - step).max(0) } else { win.scroll };
        let scrolling = scroll != win.scroll;
        win.scroll = scroll;
        let g = if scrolling { win.geometry(&fence) } else { g };
        let anchor = POINT { x: start.x, y: start.y - win.scroll };
        let mut selection = base.clone();
        for i in g.in_rect(anchor, point) { selection.insert(win.entries[i].key.clone()); }
        win.selection = selection;
        win.interaction = Some(Interaction::Rubber { start, current: point, base });
        if scrolling { unsafe { SetTimer(Some(win.hwnd), TIMER_AUTOSCROLL, 40, None); } }
        else { let _ = unsafe { KillTimer(Some(win.hwnd), TIMER_AUTOSCROLL) }; }
        self.render(id);
    }

    /// Timer tick: keeps a rubber-band selection scrolling at the content edge.
    pub fn rubber_autoscroll(&mut self, id: u64) {
        let Some(index) = self.win_index(id) else { return };
        let hwnd = self.fences[index].hwnd;
        if !matches!(self.fences[index].interaction, Some(Interaction::Rubber { .. })) {
            let _ = unsafe { KillTimer(Some(hwnd), TIMER_AUTOSCROLL) };
            return;
        }
        let mut cursor = POINT::default();
        let _ = unsafe { GetCursorPos(&mut cursor) };
        let _ = unsafe { windows::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut cursor) };
        self.update_rubber(id, cursor);
    }

    /// Rolls a collapsed fence down (`open`) or back up to its title bar.
    pub fn peek(&mut self, id: u64, open: bool) {
        let Some(index) = self.win_index(id) else { return };
        let Some(fence) = self.config.fence(id).cloned() else { return };
        let win = &mut self.fences[index];
        if !fence.collapsed { win.peek = false; return; }
        if win.peek == open { return; }
        let rect = win.window_rect();
        let height = if open {
            let monitor = monitor_from_point(POINT { x: rect.left, y: rect.top });
            scale(fence.placement.height, win.dpi).min(monitor.work.bottom - rect.top).max(scale(TITLE_H, win.dpi))
        } else { scale(TITLE_H, win.dpi) };
        win.peek = open;
        let hwnd = win.hwnd;
        let _ = unsafe { SetWindowPos(hwnd, None, rect.left, rect.top, rect.right - rect.left, height, SWP_NOZORDER | SWP_NOACTIVATE) };
        // Poll instead of WM_MOUSELEAVE so menus and drags keep the fence open.
        if open { unsafe { SetTimer(Some(hwnd), TIMER_PEEK, 250, None); } } else { let _ = unsafe { KillTimer(Some(hwnd), TIMER_PEEK) }; }
        self.render(id);
    }

    /// Collapses a peeking fence once the pointer has left it.
    pub fn peek_check(&mut self, id: u64) {
        let Some(index) = self.win_index(id) else { return };
        let win = &self.fences[index];
        if !win.peek { let _ = unsafe { KillTimer(Some(win.hwnd), TIMER_PEEK) }; return; }
        if win.interaction.is_some() || win.rename.is_some() || win.drop_hover.is_some() { return; }
        let mut cursor = POINT::default();
        let _ = unsafe { GetCursorPos(&mut cursor) };
        let r = win.window_rect();
        let inside = cursor.x >= r.left && cursor.x < r.right && cursor.y >= r.top && cursor.y < r.bottom;
        // Keep it open while one of its menus is showing.
        let mut gui = GUITHREADINFO { cbSize: std::mem::size_of::<GUITHREADINFO>() as u32, ..Default::default() };
        let thread = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
        let menu_open = zsdesk_windows::flyout::is_open()
            || unsafe { GetGUIThreadInfo(thread, &mut gui) }.is_ok() && (gui.flags.0 & (GUI_INMENUMODE.0 | GUI_POPUPMENUMODE.0)) != 0;
        if !inside && !menu_open { self.peek(id, false); }
    }

    pub fn mouse_leave(&mut self, id: u64) {
        if let Some(index) = self.win_index(id) {
            self.fences[index].tracking = false;
            self.fences[index].peek_blocked = false;
            if self.fences[index].interaction.is_none() { self.fences[index].hover = FenceHit::None; self.render(id); }
        }
    }

    pub fn mouse_down(&mut self, id: u64, point: POINT, double: bool) -> After {
        let Some(index) = self.win_index(id) else { return After::Nothing };
        let Some(fence) = self.config.fence(id).cloned() else { return After::Nothing };
        self.commit_rename_if_any(id);
        if self.config.is_released(id) && self.fences[index].selection.is_empty() && unsafe { GetKeyState(VK_CONTROL.0 as i32) } >= 0 {
            let others:Vec<u64>=self.fences.iter_mut().filter(|window|window.standalone&&window.id!=id).map(|window|{window.selection.clear();window.focus=None;window.id}).collect();
            for other in others { self.render(other); }
        }
        let win = &mut self.fences[index];
        let g = win.geometry(&fence);
        let hit = g.hit(point.x, point.y, win.dpi, fence.locked, win.collapsed(&fence));
        let ctrl = unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0;
        let shift = unsafe { GetKeyState(VK_SHIFT.0 as i32) } < 0;
        let mut screen = point;
        let _ = unsafe { windows::Win32::Graphics::Gdi::ClientToScreen(win.hwnd, &mut screen) };
        let _ = unsafe { windows::Win32::UI::Input::KeyboardAndMouse::SetFocus(Some(win.hwnd)) };
        match hit {
            FenceHit::Title if double => { self.toggle_collapse(id); return After::Nothing; }
            FenceHit::Title => {
                if !fence.locked { win.interaction = Some(Interaction::Move { cursor: screen, rect: win.window_rect() }); unsafe { windows::Win32::UI::Input::KeyboardAndMouse::SetCapture(win.hwnd); } }
            }
            FenceHit::Edge(edges) => {
                win.interaction = Some(Interaction::Resize { edges, cursor: screen, rect: win.window_rect() });
                unsafe { windows::Win32::UI::Input::KeyboardAndMouse::SetCapture(win.hwnd); }
            }
            FenceHit::More => return After::FenceMenu { fence: id, point: screen_below(win.hwnd, g.more) },
            FenceHit::Tab(i) => {
                let tab = win.tabs[i].id;
                if double && tab == id { self.toggle_collapse(id); return After::Nothing; }
                win.interaction = Some(Interaction::TabPress { tab, cursor: point });
                unsafe { windows::Win32::UI::Input::KeyboardAndMouse::SetCapture(win.hwnd); }
            }
            FenceHit::AddTab => return After::AddTab { fence: id, point: screen_below(win.hwnd, g.add) },
            FenceHit::TabList => return After::TabList { fence: id, point: screen_below(win.hwnd, g.tab_list) },
            FenceHit::Back => { self.browse_up(id); return After::Nothing; }
            FenceHit::Scrollbar => {
                win.interaction = Some(Interaction::Thumb { cursor_y: point.y, scroll: win.scroll });
                unsafe { windows::Win32::UI::Input::KeyboardAndMouse::SetCapture(win.hwnd); }
            }
            FenceHit::Item(i) => {
                if double { return After::Open { fence: id, index: i, explorer: ctrl }; }
                let key = win.entries[i].key.clone();
                if shift {
                    let anchor = win.anchor.unwrap_or(i);
                    let (a, b) = (anchor.min(i), anchor.max(i));
                    if !ctrl { win.selection.clear(); }
                    for e in &win.entries[a..=b] { win.selection.insert(e.key.clone()); }
                } else if ctrl {
                    if !win.selection.remove(&key) { win.selection.insert(key); }
                    win.anchor = Some(i);
                } else {
                    if !win.selection.contains(&key) { win.selection = HashSet::from([key]); }
                    win.anchor = Some(i);
                }
                win.focus = Some(i);
                win.interaction = Some(Interaction::Press { index: i, cursor: point });
                unsafe { windows::Win32::UI::Input::KeyboardAndMouse::SetCapture(win.hwnd); }
            }
            FenceHit::Empty => {
                let base = if ctrl { win.selection.clone() } else { HashSet::new() };
                win.selection = base.clone();
                let start = POINT { x: point.x, y: point.y + win.scroll };
                win.interaction = Some(Interaction::Rubber { start, current: point, base });
                unsafe { windows::Win32::UI::Input::KeyboardAndMouse::SetCapture(win.hwnd); }
            }
            FenceHit::None => {}
        }
        self.render(id);
        After::Nothing
    }

    pub fn mouse_up(&mut self, id: u64, point: POINT) -> After {
        let Some(index) = self.win_index(id) else { return After::Nothing };
        let win = &mut self.fences[index];
        let interaction = win.interaction.take();
        let _ = unsafe { KillTimer(Some(win.hwnd), TIMER_AUTOSCROLL) };
        let collapse_desktop_selection=win.standalone&&matches!(&interaction,Some(Interaction::Press{..}))
            &&unsafe{GetKeyState(VK_CONTROL.0 as i32)}>=0&&unsafe{GetKeyState(VK_SHIFT.0 as i32)}>=0;
        if unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetCapture() } == win.hwnd {
            let _ = unsafe { windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture() };
        }
        match interaction {
            Some(Interaction::Move { .. }) if self.merge_target.is_some() => {
                let target = self.merge_target.take();
                for w in &mut self.fences { w.ghost = false; w.merge_hover = None; }
                if let Some(target) = target { self.merge_fences(id, target); }
                return After::Nothing;
            }
            Some(Interaction::Move { .. }) | Some(Interaction::Resize { .. }) => {
                let rect = win.window_rect();
                self.store_rect(id, rect);
            }
            Some(Interaction::TabPress { tab, .. }) => {
                if tab != id { self.switch_tab(tab); }
                return After::Nothing;
            }
            Some(Interaction::TabDrag { tab, torn }) => {
                if torn {
                    let mut screen = point;
                    let _ = unsafe { windows::Win32::Graphics::Gdi::ClientToScreen(win.hwnd, &mut screen) };
                    self.detach_tab_at(tab, screen);
                    return After::Nothing;
                }
                self.schedule_save();
            }
            Some(Interaction::Press { index: item, .. }) => {
                let ctrl = unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0;
                let shift = unsafe { GetKeyState(VK_SHIFT.0 as i32) } < 0;
                if !ctrl && !shift { win.selection = HashSet::from([win.entries[item].key.clone()]); }
            }
            _ => {}
        }
        let _ = point;
        if collapse_desktop_selection {
            let others:Vec<_>=self.fences.iter_mut().filter(|window|window.standalone&&window.id!=id)
                .map(|window|{window.selection.clear();window.focus=None;window.id}).collect();
            for other in others { self.render(other); }
        }
        self.render(id);
        After::Nothing
    }

    /// Capture taken away mid-gesture (menus, Alt+Tab): drop the gesture and
    /// any merge preview it was showing.
    pub fn capture_lost(&mut self, id: u64) {
        let Some(index) = self.win_index(id) else { return };
        let _ = unsafe { KillTimer(Some(self.fences[index].hwnd), TIMER_AUTOSCROLL) };
        if self.fences[index].interaction.take().is_none() { return; }
        if self.merge_target.is_some() { self.set_merge_target(id, None); }
        self.render(id);
    }

    pub fn right_click(&mut self, id: u64, point: POINT) -> After {
        self.menu_input_started=Some((id,Instant::now()));
        let Some(index) = self.win_index(id) else { return After::Nothing };
        let Some(fence) = self.config.fence(id).cloned() else { return After::Nothing };
        let clear_others={
            let window=&self.fences[index];
            match window.geometry(&fence).hit(point.x,point.y,window.dpi,true,window.collapsed(&fence)) {
                FenceHit::Item(item)=>window.standalone&&!window.selection.contains(&window.entries[item].key),
                _=>false,
            }
        };
        if clear_others {
            let changed:Vec<_>=self.fences.iter_mut().filter(|window|window.standalone&&window.id!=id&&!window.selection.is_empty())
                .map(|window|{window.selection.clear();window.focus=None;window.id}).collect();
            for other in changed {self.render(other);}
        }
        let win = &mut self.fences[index];
        let g = win.geometry(&fence);
        let mut screen = point;
        let _ = unsafe { windows::Win32::Graphics::Gdi::ClientToScreen(win.hwnd, &mut screen) };
        match g.hit(point.x, point.y, win.dpi, true, win.collapsed(&fence)) {
            FenceHit::Item(i) => {
                let key = win.entries[i].key.clone();
                let changed=!win.selection.contains(&key)||win.focus!=Some(i);
                if !win.selection.contains(&key) { win.selection = HashSet::from([key]); }
                win.focus = Some(i);
                if changed { self.render(id); }
                After::ItemMenu { fence: id, point: screen }
            }
            FenceHit::Tab(i) => {
                let tab = win.tabs[i].id;
                if tab != id { self.switch_tab(tab); }
                After::FenceMenu { fence: tab, point: screen }
            }
            _ if win.standalone => After::Nothing,
            _ => After::FenceMenu { fence: id, point: screen },
        }
    }

    pub fn wheel(&mut self, id: u64, delta: i32, screen: POINT) {
        let Some(index) = self.win_index(id) else { return };
        let Some(fence) = self.config.fence(id).cloned() else { return };
        let win = &mut self.fences[index];
        let g = win.geometry(&fence);
        let mut client = screen;
        let _ = unsafe { windows::Win32::Graphics::Gdi::ScreenToClient(win.hwnd, &mut client) };
        if unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0 && g.content.contains(client.x, client.y) && win.rename.is_none() {
            let steps = zoom_steps(&mut win.zoom_remainder, delta);
            if steps == 0 { return; }
            let anchor = g.cells.iter().position(|cell| cell.cell.contains(client.x, client.y)).or(win.focus);
            let next = zoom_icon_size(fence.icon_size, steps);
            if let Some(model) = self.config.fence_mut(id) { model.icon_size = next; model.view = FenceView::Icons; }
            if let Some(model) = self.config.fence(id) {
                let changed = win.geometry(model);
                if let Some(cell) = anchor.and_then(|index| changed.cells.get(index)) {
                    win.scroll = (win.scroll + cell.cell.y - client.y).max(0);
                }
                win.clamp_scroll(model);
            }
            self.schedule_save(); self.render(id); return;
        }
        win.zoom_remainder = 0;
        if win.tabbed() && client.y >= 0 && client.y < g.title.height {
            if delta != 0 { self.cycle_tab(id, if delta < 0 { 1 } else { -1 }); }
            return;
        }
        let step = if fence.view == FenceView::List { scale(26 * 3, win.dpi) } else { g.cells.first().map_or(80, |c| c.cell.height + scale(2, win.dpi)) };
        // Precision touchpads send small deltas; keep the sign so they still move.
        let moved = delta * step / 120;
        let moved = if moved == 0 && delta != 0 { delta.signum() } else { moved };
        win.scroll = (win.scroll - moved).clamp(0, g.max_scroll);
        if matches!(win.interaction, Some(Interaction::Rubber { .. })) { self.update_rubber(id, client); return; }
        self.render(id);
    }

    pub fn key(&mut self, id: u64, key: u16) -> After {
        use windows::Win32::UI::Input::KeyboardAndMouse::*;
        let input_started=Instant::now();
        let Some(index) = self.win_index(id) else { return After::Nothing };
        let Some(fence) = self.config.fence(id).cloned() else { return After::Nothing };
        let ctrl = unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0;
        let shift = unsafe { GetKeyState(VK_SHIFT.0 as i32) } < 0;
        let alt = unsafe { GetKeyState(VK_MENU.0 as i32) } < 0;
        if !ctrl&&!alt&&(key==VK_APPS.0 || (key==VK_F10.0&&shift)) {
            self.menu_input_started=Some((id,input_started));
            let window=&self.fences[index];let geometry=window.geometry(&fence);
            let cell=window.focus.and_then(|item|geometry.cells.get(item)).map(|cell|cell.cell).unwrap_or(geometry.content);
            let mut point=POINT{x:cell.x+cell.width.min(32)/2,y:cell.y+cell.height.min(32)/2};
            let _=unsafe{windows::Win32::Graphics::Gdi::ClientToScreen(window.hwnd,&mut point)};
            return if window.selection.is_empty(){After::FenceMenu{fence:id,point}}else{After::ItemMenu{fence:id,point}};
        }
        if self.config.is_released(id)&&key==VK_ESCAPE.0 {
            let changed:Vec<_>=self.fences.iter_mut().filter(|window|window.standalone&&!window.selection.is_empty())
                .map(|window|{window.selection.clear();window.focus=None;window.id}).collect();
            for other in changed {self.render(other);}return After::Nothing;
        }
        if let Some(action) = history_shortcut(key, ctrl, shift, alt) {
            return After::History { fence: id, action };
        }
        if ctrl && key == VK_TAB.0 { self.cycle_tab(id, if shift { -1 } else { 1 }); return After::Nothing; }
        if self.config.is_released(id) && ctrl && !alt && key==VK_A.0 {
            let ids:Vec<u64>=self.fences.iter_mut().filter(|window|window.standalone).map(|window|{
                window.change_selection(if shift{SelectionChange::Invert}else{SelectionChange::All});window.id
            }).collect();
            for id in ids { self.render(id); } return After::Nothing;
        }
        let win = &mut self.fences[index];
        let count = win.entries.len();
        let k = VIRTUAL_KEY(key);
        match k {
            VK_RETURN => if let Some(f) = win.focus.filter(|_| !win.selection.is_empty()) { return After::Open { fence: id, index: f, explorer: ctrl }; },
            VK_DELETE => return After::Verb { fence: id, verb: "delete", background: false },
            VK_F2 => if let Some(f) = win.focus { let key = win.entries[f].key.clone(); self.begin_rename(id, RenameTarget::Item(key)); },
            VK_F5 => if fence.is_desktop() { self.refresh_desktop(); } else { self.refresh_folder(id); },
            VK_ESCAPE => { win.selection.clear(); self.render(id); }
            VK_BACK => if fence.browse.is_some() { self.browse_up(id); },
            _ if ctrl && !alt && k == VK_A => { win.change_selection(if shift { SelectionChange::Invert } else { SelectionChange::All }); self.render(id); }
            _ if ctrl && k == VK_C => return After::Verb { fence: id, verb: "copy", background: false },
            _ if ctrl && k == VK_X => return After::Verb { fence: id, verb: "cut", background: false },
            _ if ctrl && k == VK_V => return After::Verb { fence: id, verb: "paste", background: true },
            VK_LEFT | VK_RIGHT | VK_UP | VK_DOWN | VK_HOME | VK_END if count > 0 => {
                let g = win.geometry(&fence);
                let columns = if fence.view == FenceView::List { 1 } else { g.columns() } as isize;
                let current = win.focus.unwrap_or(0) as isize;
                let next = match k {
                    VK_LEFT => current - 1, VK_RIGHT => current + 1, VK_UP => current - columns, VK_DOWN => current + columns,
                    VK_HOME => 0, _ => count as isize - 1,
                }.clamp(0, count as isize - 1) as usize;
                if shift {
                    let anchor = win.anchor.unwrap_or(current as usize);
                    win.selection = win.entries[anchor.min(next)..=anchor.max(next)].iter().map(|e| e.key.clone()).collect();
                } else {
                    win.selection = HashSet::from([win.entries[next].key.clone()]);
                    win.anchor = Some(next);
                }
                win.focus = Some(next);
                // Keep the focused item visible.
                let cell = g.cells[next].cell;
                if cell.y < g.content.y { win.scroll -= g.content.y - cell.y; }
                if cell.bottom() > g.content.bottom() { win.scroll += cell.bottom() - g.content.bottom(); }
                win.clamp_scroll(&fence);
                self.render(id);
            }
            _ => {}
        }
        After::Nothing
    }

    pub fn toggle_collapse(&mut self, id: u64) {
        let peeking = self.win_index(id).is_some_and(|i| self.fences[i].peek);
        if let Some(i) = self.win_index(id) {
            let w = &mut self.fences[i];
            w.peek = false;
            let _ = unsafe { KillTimer(Some(w.hwnd), TIMER_PEEK) };
        }
        let Some(fence) = self.config.fence_mut(id) else { return };
        // Double-clicking a peeking fence pins it open.
        fence.collapsed = if peeking { false } else { !fence.collapsed };
        if fence.collapsed { if let Some(i) = self.fences.iter().position(|w| w.id == id) { self.fences[i].peek_blocked = true; } }
        self.config.sync_group(id);
        let fence = self.config.fence_mut(id).unwrap();
        let placement = fence.placement.clone();
        let collapsed = fence.collapsed;
        if let Some(index) = self.win_index(id) {
            let current = self.fences[index].window_rect();
            let dpi = self.fences[index].dpi;
            let height = if collapsed { scale(TITLE_H, dpi) } else { scale(placement.height, dpi) };
            let _ = unsafe { SetWindowPos(self.fences[index].hwnd, None, current.left, current.top, current.right - current.left, height, SWP_NOZORDER | SWP_NOACTIVATE) };
        }
        self.schedule_save();
        self.render(id);
    }

    fn browse_up(&mut self, id: u64) {
        let Some(fence) = self.config.fence_mut(id) else { return };
        let FenceSource::Folder { root } = fence.source.clone() else { return };
        if let Some(current) = fence.browse.clone() {
            let parent = current.parent().map(Path::to_path_buf);
            fence.browse = parent.filter(|p| p.starts_with(&root) && p != &root);
            if let Some(index) = self.win_index(id) { self.fences[index].scroll = 0; self.fences[index].selection.clear(); }
            self.schedule_save();
            self.refresh_folder(id);
        }
    }

    pub fn browse_into(&mut self, id: u64, path: PathBuf) {
        let Some(fence) = self.config.fence_mut(id) else { return };
        fence.browse = Some(path);
        if let Some(index) = self.win_index(id) { self.fences[index].scroll = 0; self.fences[index].selection.clear(); self.fences[index].focus = None; }
        self.schedule_save();
        self.refresh_folder(id);
    }

    // ---- rename --------------------------------------------------------------

    pub fn begin_rename(&mut self, id: u64, target: RenameTarget) {
        self.commit_rename_if_any(id);
        let Some(index) = self.win_index(id) else { return };
        let Some(fence) = self.config.fence(id).cloned() else { return };
        let win = &self.fences[index];
        let g = win.geometry(&fence);
        let (rect, text, multiline) = match &target {
            RenameTarget::Title if win.tabbed() => {
                let active = win.active_index();
                let chip = g.tabs.iter().find(|(i, _)| *i == active).map_or(g.title, |(_, r)| *r);
                let width = chip.width.max(scale(120, win.dpi)).min((g.width - chip.x - scale(4, win.dpi)).max(scale(60, win.dpi)));
                (zsdesk_ui::chrome::Rect::new(chip.x, chip.y, width, chip.height), fence.title.clone(), false)
            }
            RenameTarget::Title => (zsdesk_ui::chrome::Rect::new(scale(36, win.dpi), scale(4, win.dpi), (g.width - scale(72, win.dpi)).max(scale(60, win.dpi)), g.title.height - scale(8, win.dpi)), fence.title.clone(), false),
            RenameTarget::Item(key) => {
                let Some(i) = win.entries.iter().position(|e| &e.key == key) else { return };
                let name = desktop::editing_name(&win.entries[i].pidl).unwrap_or_else(|| win.entries[i].name.clone());
                let c = g.cells[i];
                let r = if fence.view == FenceView::List { c.label } else { zsdesk_ui::chrome::Rect::new(c.cell.x, c.label.y, c.cell.width, c.label.height.max(scale(40, win.dpi))) };
                (r, name, fence.view == FenceView::Icons)
            }
        };
        let mut origin = POINT { x: rect.x, y: rect.y };
        let _ = unsafe { windows::Win32::Graphics::Gdi::ClientToScreen(win.hwnd, &mut origin) };
        let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
        let mut style = WS_POPUP | WS_BORDER | WINDOW_STYLE(ES_AUTOHSCROLL as u32);
        if multiline { style = WS_POPUP | WS_BORDER | WINDOW_STYLE((ES_MULTILINE | ES_CENTER | ES_AUTOVSCROLL) as u32); }
        let Ok(edit) = (unsafe { CreateWindowExW(WS_EX_TOOLWINDOW, windows::core::w!("EDIT"), PCWSTR(wide.as_ptr()), style,
            origin.x, origin.y, rect.width, rect.height, Some(win.hwnd), None, None, None) }) else { return };
        super::prepare_rename_edit(edit, win.hwnd, win.dpi);
        // Select the name without extension, like Explorer.
        let stem = match &target { RenameTarget::Item(_) if !text.starts_with('.') => text.rfind('.').unwrap_or(text.len()), _ => text.len() };
        let stem = text[..stem].encode_utf16().count();
        unsafe {
            SendMessageW(edit, windows::Win32::UI::Controls::EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(stem as isize)));
            let _ = ShowWindow(edit, SW_SHOW);
            let _ = windows::Win32::UI::Input::KeyboardAndMouse::SetFocus(Some(edit));
        }
        self.fences[index].rename = Some((edit, target));
        self.render(id);
    }

    /// Finishes rename: returns the Shell rename to run outside the borrow.
    pub fn finish_rename(&mut self, id: u64, commit: bool) -> After {
        let Some(index) = self.win_index(id) else { return After::Nothing };
        let Some((edit, target)) = self.fences[index].rename.take() else { return After::Nothing };
        let text = super::window_text(edit);
        let _ = unsafe { DestroyWindow(edit) };
        let text = text.trim().to_string();
        let mut after = After::Nothing;
        if commit && !text.is_empty() {
            match target {
                RenameTarget::Title => {
                    if let Some(f) = self.config.fence_mut(id) { f.title = text; }
                    self.schedule_save();
                }
                RenameTarget::Item(key) => after = After::Rename { fence: id, key, name: text },
            }
        }
        self.render(id);
        after
    }

    fn commit_rename_if_any(&mut self, id: u64) {
        if self.win_index(id).is_some_and(|i| self.fences[i].rename.is_some()) {
            if let After::Rename { fence, key, name } = self.finish_rename(id, true) {
                super::post_rename(self.hwnd, fence, key, name);
            }
        }
    }

    // ---- menus ---------------------------------------------------------------

    /// Management commands for a fence, independent of Shell command IDs.
    pub fn fence_menu(&self, id: u64) -> MenuSpec {
        let Some(fence) = self.config.fence(id) else { return menu(Vec::new()) };
        let s = &self.config.settings;
        let tabbed = self.config.tab_ids(id).len() > 1;
        let mut items = Vec::new();
        // The folder's Shell background menu has no DefView, so it lacks Paste;
        // keep the fence's own Ctrl+V reachable with the mouse.
        items.push(entry("粘贴\tCtrl+V", "paste", self.target_folder(id).is_some(), false));
        items.push(entry("文件夹系统菜单…", "folder-system-menu", self.target_folder(id).is_some(), false));
        let selected=self.win_index(id).is_some_and(|index|!self.fences[index].selection.is_empty());
        if selected{items.push(submenu("所选项目",self.item_menu_items(id)));}
        items.push(MenuItemSpec::Separator);
        if fence.is_desktop() { items.push(entry("新建格子", "new-fence", true, false)); }
        items.push(entry("新建文件夹映射格子…", "new-folder-fence", true, false));
        items.push(submenu("新建标签", vec![
            entry("桌面分组", "new-tab-desktop", s.organize, false),
            entry("映射文件夹…", "new-tab-folder", true, false),
        ]));
        items.push(entry(if tabbed { "重命名标签" } else { "重命名格子" }, "rename", true, false));
        if tabbed { items.push(entry("移到新格子", "split-tab", true, false)); }
        items.push(MenuItemSpec::Separator);
        items.push(submenu("选择", self.selection_menu_items(id)));
        items.push(entry("撤销文件操作\tCtrl+Z", "history-undo", true, false));
        items.push(entry("重做文件操作\tCtrl+Y", "history-redo", true, false));
        items.push(MenuItemSpec::Separator);
        items.push(submenu("查看", vec![
            entry("大图标", "icons-large", true, fence.view == FenceView::Icons && fence.icon_size == IconSize::Large),
            entry("中等图标", "icons-medium", true, fence.view == FenceView::Icons && fence.icon_size == IconSize::Medium),
            entry("小图标", "icons-small", true, fence.view == FenceView::Icons && fence.icon_size == IconSize::Small),
            entry("列表", "list", true, fence.view == FenceView::List),
        ]));
        let mut sorts = Vec::new();
        if fence.is_desktop() { sorts.push(entry("手动排列", "sort-manual", true, fence.sort == FenceSort::Manual)); }
        sorts.extend([
            entry("名称", "sort-name", true, fence.sort == FenceSort::Name), entry("类型", "sort-type", true, fence.sort == FenceSort::Type),
            entry("大小", "sort-size", true, fence.sort == FenceSort::Size), entry("修改日期", "sort-modified", true, fence.sort == FenceSort::Modified),
            MenuItemSpec::Separator,
            entry("递增", "sort-asc", fence.sort != FenceSort::Manual, !fence.descending),
            entry("递减", "sort-desc", fence.sort != FenceSort::Manual, fence.descending),
        ]);
        items.push(submenu("排序方式", sorts));
        if fence.is_desktop() {
            let mut rules: Vec<MenuItemSpec> = Category::ALL.iter().filter(|c| **c != Category::Others)
                .map(|c| entry(c.title(), &format!("rule-{c:?}"), true, fence.rule == Some(*c))).collect();
            rules.push(MenuItemSpec::Separator);
            rules.push(entry("不自动收取", "rule-none", true, fence.rule.is_none()));
            rules.push(entry("收取未分类项目", "catch-all", true, fence.catch_all));
            items.push(submenu("自动收取", rules));
        } else {
            items.push(entry(if crate::edition::DESKTOP_ONLY{"在系统资源管理器中打开"}else{"在 ZSDesk 中打开"}, "open-explorer", true, false));
            items.push(entry("返回上一级", "browse-up", fence.browse.is_some(), false));
            items.push(entry("更改映射的文件夹…", "change-folder", true, false));
            items.push(entry("刷新", "refresh", true, false));
        }
        items.push(MenuItemSpec::Separator);
        items.push(entry("锁定格子", "lock", true, fence.locked));
        items.push(entry(if fence.collapsed { "展开格子" } else { "折叠格子" }, "collapse", true, false));
        items.push(entry(if tabbed { "关闭标签" } else { "删除格子" }, "delete", true, false));
        items.push(MenuItemSpec::Separator);
        items.push(submenu("桌面格子", self.global_items(s)));
        menu(items)
    }

    fn global_items(&self, s: &zsdesk_core::model::desktop::DesktopSettings) -> Vec<MenuItemSpec> {
        let expected=std::env::current_exe().ok().and_then(|exe|fence_startup_command(&exe,&self.config_path));
        let autostart=expected.as_ref().is_some_and(|expected|
            desktop::autostart_command(crate::edition::desktop_startup_value()).ok().flatten()
                .is_some_and(|current|desktop::autostart_owned(&current,expected)));
        let transparency = |opacity: u8| {
            let label = if opacity >= 100 { "不透明".to_string() } else { format!("透明度 {}%", 100 - u32::from(opacity)) };
            entry(&label, &format!("opacity-{opacity}"), true, s.opacity == opacity)
        };
        vec![
            entry(if s.organize { "重新整理桌面" } else { "一键整理桌面" }, "organize", true, false),
            entry("还原系统桌面图标", "restore", s.organize, false),
            MenuItemSpec::Separator,
            entry("锁定全部格子", "lock-all", !self.config.fences.is_empty(), !self.config.fences.is_empty() && self.config.fences.iter().all(|f| f.locked)),
            entry(if s.hidden { "显示全部格子" } else { "隐藏全部格子" }, "toggle-hidden", true, false),
            entry("双击桌面空白处隐藏/显示格子", "double-click", true, s.double_click_toggle),
            submenu("外观", vec![
                entry("显示文件数量", "show-counts", true, s.show_counts),
                MenuItemSpec::Separator,
                entry("深色", "theme-dark", true, s.theme == FenceTheme::Dark),
                entry("浅色", "theme-light", true, s.theme == FenceTheme::Light),
                entry("跟随系统", "theme-system", true, s.theme == FenceTheme::System),
                MenuItemSpec::Separator,
                entry("毛玻璃效果", "blur", true, s.blur),
                MenuItemSpec::Separator,
                // The setting stores the background opacity; the menu shows transparency.
                transparency(100), transparency(85), transparency(70), transparency(55),
                transparency(45), transparency(30), transparency(15),
            ]),
            entry("文件夹格子内打开子文件夹", "browse-inside", true, s.browse_inside),
            entry("鼠标悬停展开折叠的格子", "hover-expand", true, s.hover_expand),
            entry("隐藏托盘图标", "hide-tray", true, s.hide_tray),
            entry("ZSUI 即时右键菜单", "zsui-fast-context-menu", true, s.zsui_fast_context_menu),
            // Only the fences: the file manager window has its own login switch
            // in its settings menu, so the two can be chosen independently.
            entry("登录 Windows 时启动桌面格子", "autostart", true, autostart),
            MenuItemSpec::Separator,
            entry(if crate::edition::DESKTOP_ONLY{"打开系统资源管理器"}else{"打开 ZSDesk 文件管理器"}, "open-zsdesk", true, false),
            entry("退出桌面格子", "quit", true, false),
        ]
    }

    pub fn tray_menu(&self) -> MenuSpec {
        let mut items = vec![
            entry("新建格子", "new-fence", true, false),
            entry("新建文件夹映射格子…", "new-folder-fence", true, false),
            MenuItemSpec::Separator,
        ];
        items.extend(self.global_items(&self.config.settings));
        menu(items)
    }

    /// Runs a fence/tray menu command. Returns work that needs a modal context.
    pub fn menu_command(&mut self, fence_id: Option<u64>, id: &str, point: POINT) -> After {
        let fid = fence_id.unwrap_or(0);
        if let Some(rest) = id.strip_prefix("plugin:") {
            if let Some((plugin, action)) = rest.split_once(':') {
                let context = serde_json::json!({"selected": self.selected_keys(fid)}).to_string();
                match self.plugins.invoke(plugin, action, &context) {
                    Ok(Some(message)) => self.notify_error(&message),
                    Ok(None) => {},
                    Err(error) => self.notify_error(&format!("WASM 插件执行失败：{error}")),
                }
            }
            return After::Nothing;
        }
        match id {
            "folder-system-menu" if self.target_folder(fid).is_some()=>return After::FolderSystemMenu{fence:fid,point},
            "paste" if self.target_folder(fid).is_some()=>return After::Verb{fence:fid,verb:"paste",background:true},
            "new-fence" => self.new_desktop_fence(Some(point)),
            "new-folder-fence" => return After::PickFolder { replace: None },
            "change-folder" => return After::PickFolder { replace: Some(fid) },
            "rename" => self.begin_rename(fid, RenameTarget::Title),
            "delete" => return After::DeleteFence(fid),
            "history-undo" => return After::History { fence: fid, action: HistoryAction::Undo },
            "history-redo" => return After::History { fence: fid, action: HistoryAction::Redo },
            "select-all" | "select-none" | "select-invert" => {
                if let Some(index) = self.win_index(fid) {
                    let change = match id { "select-all" => SelectionChange::All, "select-none" => SelectionChange::Clear, _ => SelectionChange::Invert };
                    self.fences[index].change_selection(change);
                    self.render(fid);
                }
            }
            "lock" => { if let Some(f) = self.config.fence_mut(fid) { f.locked = !f.locked; } self.config.sync_group(fid); self.schedule_save(); self.render(fid); }
            "new-tab-desktop" => self.add_desktop_tab(fid),
            "new-tab-folder" => return After::PickFolderTab { to: fid },
            "split-tab" => if let Some(fence) = self.config.fence(fid).cloned() {
                let placement = Placement { x: fence.placement.x + 24, y: fence.placement.y + 24, ..fence.placement };
                self.detach_tab(fid, placement);
            },
            id if id.starts_with("switch-tab-") => if let Ok(tab) = id["switch-tab-".len()..].parse::<u64>() { self.switch_tab(tab); },
            "collapse" => self.toggle_collapse(fid),
            "open-explorer" => if let Some(path) = self.config.fence(fid).and_then(|f| f.folder().cloned()) { self.open_in_explorer(&path); },
            "browse-up" => self.browse_up(fid),
            "refresh" => self.refresh_folder(fid),
            "organize" => if self.config.settings.organize { self.refresh_desktop(); self.show_all(); } else { self.organize(); },
            "restore" => self.restore_desktop(),
            "lock-all" => {
                let all = self.config.fences.iter().all(|f| f.locked);
                for f in &mut self.config.fences { f.locked = !all; }
                self.schedule_save();
                self.render_all();
            }
            "toggle-hidden" => self.toggle_hidden(),
            "double-click" => { self.config.settings.double_click_toggle = !self.config.settings.double_click_toggle; self.schedule_save(); super::update_hook(self.config.settings.double_click_toggle); }
            "browse-inside" => { self.config.settings.browse_inside = !self.config.settings.browse_inside; self.schedule_save(); }
            "hover-expand" => { self.config.settings.hover_expand = !self.config.settings.hover_expand; self.schedule_save(); }
            "show-counts" => { self.config.settings.show_counts = !self.config.settings.show_counts; self.schedule_save(); self.render_all(); }
            "zsui-fast-context-menu" => { self.config.settings.zsui_fast_context_menu = !self.config.settings.zsui_fast_context_menu; self.schedule_save(); }
            "blur" => {
                self.config.settings.blur = !self.config.settings.blur;
                super::fence::set_blur(self.config.settings.blur);
                for win in &self.fences { super::fence::apply_glass(win.hwnd); }
                self.schedule_save();
                self.render_all();
            }
            "hide-tray" => {
                if self.config.settings.hide_tray { self.show_tray(); }
                else {
                    self.config.settings.hide_tray = true;
                    self.remove_tray();
                    self.schedule_save();
                    return After::Notice("托盘图标已隐藏。\n\n右键任意格子 → 桌面格子 可取消隐藏；也可在文件管理器 ≡ → 桌面格子 → 显示托盘图标。");
                }
            }
            "autostart" => {
                let Some(command)=std::env::current_exe().ok().and_then(|exe|fence_startup_command(&exe,&self.config_path)) else {
                    return After::Notice("无法确定当前安装和配置目录，登录启动设置未更改。");
                };
                let value=crate::edition::desktop_startup_value();
                let current=match desktop::autostart_command(value) {
                    Ok(current)=>current,Err(_)=>return After::Notice("无法读取登录启动设置。"),
                };
                if current.as_ref().is_some_and(|current|!desktop::autostart_owned(current,&command)) {
                    return After::Notice("已有登录启动项属于另一安装或配置，现有设置已保留。");
                }
                if desktop::set_autostart(value,&command,current.is_none()).is_err() {
                    return After::Notice("无法更改登录启动设置，请重新打开设置后重试。");
                }
            }
            "open-zsdesk" => self.open_explorer_args(&[]),
            "quit" => return After::Quit,
            id if id.starts_with("theme-") => {
                self.config.settings.theme = match id { "theme-light" => FenceTheme::Light, "theme-system" => FenceTheme::System, _ => FenceTheme::Dark };
                self.update_palette(); self.schedule_save(); self.render_all();
            }
            id if id.starts_with("opacity-") => {
                self.config.settings.opacity = id[8..].parse().unwrap_or(55);
                self.update_palette(); self.schedule_save(); self.render_all();
            }
            id if id.starts_with("icons-") || id == "list" => {
                if let Some(f) = self.config.fence_mut(fid) {
                    match id { "list" => f.view = FenceView::List, _ => { f.view = FenceView::Icons; f.icon_size = match id { "icons-large" => IconSize::Large, "icons-small" => IconSize::Small, _ => IconSize::Medium }; } }
                }
                if let (Some(i), Some(f)) = (self.win_index(fid), self.config.fence(fid).cloned()) { self.fences[i].clamp_scroll(&f); }
                self.schedule_save(); self.render(fid);
            }
            id if id.starts_with("sort-") => {
                if let Some(f) = self.config.fence_mut(fid) {
                    match id {
                        "sort-asc" => f.descending = false, "sort-desc" => f.descending = true,
                        "sort-manual" => f.sort = FenceSort::Manual, "sort-name" => f.sort = FenceSort::Name,
                        "sort-type" => f.sort = FenceSort::Type, "sort-size" => f.sort = FenceSort::Size, _ => f.sort = FenceSort::Modified,
                    }
                }
                self.schedule_save();
                if self.config.fence(fid).is_some_and(|f| f.is_desktop()) { self.refresh_desktop(); } else { self.refresh_folder(fid); }
            }
            id if id.starts_with("rule-") || id == "catch-all" => {
                if let Some(f) = self.config.fence_mut(fid) {
                    if id == "catch-all" { f.catch_all = !f.catch_all; }
                    else { f.rule = Category::ALL.iter().copied().find(|c| format!("rule-{c:?}") == id); }
                }
                self.schedule_save();
                self.refresh_desktop();
            }
            id if id.starts_with("move-to-") => {
                if let Some(target) = id[8..].parse::<u64>().ok() {
                    let keys = self.selected_keys(fid);
                    self.config.assign(target, &keys, None);
                    self.sync_windows();
                    self.schedule_save();
                    self.refresh_desktop();
                }
            }
            "unassign" => {
                let keys = self.selected_keys(fid);
                for f in &mut self.config.fences { f.members.retain(|m| !keys.contains(m)); }
                self.config.released_items.retain(|item| !item.members.iter().any(|key|keys.contains(key)));
                self.sync_windows();
                self.schedule_save();
                self.refresh_desktop();
            }
            "return-desktop" => return After::ReturnToDesktop { fence: fid, point },
            "open-in-fence" => if let Some(path) = self.focused_entry(fid).and_then(|e| e.path.clone()) { self.browse_into(fid, path); },
            "open-in-zsdesk" => if let Some(e) = self.focused_entry(fid).cloned() { self.open_entry_in_explorer(&e); },
            _ => {}
        }
        After::Nothing
    }

    pub fn item_menu_items(&self, id: u64) -> Vec<MenuItemSpec> {
        let Some(fence) = self.config.fence(id) else { return Vec::new() };
        let mut items = Vec::new();
        let focused = self.focused_entry(id);
        if !self.config.is_released(id) {
            items.push(entry("移回桌面", "return-desktop", !self.selected_keys(id).is_empty(), false));
        }
        if focused.is_some_and(|e| e.is_container() && e.path.is_some()) {
            if !fence.is_desktop() { items.push(entry("在格子中打开", "open-in-fence", true, false)); }
            items.push(entry(if crate::edition::DESKTOP_ONLY{"在系统资源管理器中打开"}else{"在 ZSDesk 中打开"}, "open-in-zsdesk", true, false));
        }
        if fence.is_desktop() {
            let mut targets: Vec<MenuItemSpec> = self.config.fences.iter().filter(|f| f.is_desktop() && f.id != id)
                .map(|f| entry(&f.title.replace('&', "&&"), &format!("move-to-{}", f.id), true, false)).collect();
            if targets.is_empty() { targets.push(entry("（没有其他格子）", "none", false, false)); }
            items.push(submenu("移动到格子", targets));
            let keys = self.selected_keys(id);
            let manual = fence.members.iter().any(|m| keys.contains(m));
            items.push(entry("交给自动收取", "unassign", manual, false));
        }
        if !items.is_empty() { items.push(MenuItemSpec::Separator); }
        items.push(submenu("选择", self.selection_menu_items(id)));
        for action in self.plugins.actions() {
            items.push(entry(&action.label.replace('&', "&&"), &format!("plugin:{}:{}", action.plugin_id, action.id), true, false));
        }
        items
    }

    fn selection_menu_items(&self, id: u64) -> Vec<MenuItemSpec> {
        let window = self.win_index(id).map(|index| &self.fences[index]);
        let has_items = window.is_some_and(|window| !window.entries.is_empty());
        let has_selection = window.is_some_and(|window| !window.selection.is_empty());
        vec![entry("全选\tCtrl+A", "select-all", has_items, false),
            entry("全部取消", "select-none", has_selection, false),
            entry("反向选择\tCtrl+Shift+A", "select-invert", has_items, false)]
    }

    pub fn selected_keys(&self, id: u64) -> Vec<String> {
        if self.config.is_released(id) { return self.fences.iter().filter(|window|window.standalone).flat_map(|window|window.selected_entries().into_iter().map(|entry|entry.key.clone())).collect(); }
        self.win_index(id).map(|i| self.fences[i].entries.iter().filter(|e| self.fences[i].selection.contains(&e.key)).map(|e| e.key.clone()).collect()).unwrap_or_default()
    }

    pub fn selected_pidls(&self, id: u64) -> Vec<Vec<u8>> {
        if self.config.is_released(id) { return self.fences.iter().filter(|window|window.standalone).flat_map(|window|window.selected_entries().into_iter().map(|entry|entry.pidl.clone())).collect(); }
        self.win_index(id).map(|i| self.fences[i].selected_entries().into_iter().map(|e| e.pidl.clone()).collect()).unwrap_or_default()
    }

    pub(super) fn selected_entries_owned(&self,id:u64)->Vec<ShellEntry> {
        if self.config.is_released(id) { self.fences.iter().filter(|window|window.standalone).flat_map(|window|window.selected_entries().into_iter().cloned()).collect() }
        else { self.win_index(id).map(|index|self.fences[index].selected_entries().into_iter().cloned().collect()).unwrap_or_default() }
    }

    pub fn focused_entry(&self, id: u64) -> Option<&ShellEntry> {
        let win = &self.fences[self.win_index(id)?];
        win.focus.and_then(|f| win.entries.get(f)).filter(|e| win.selection.contains(&e.key))
            .or_else(|| win.selected_entries().into_iter().next())
    }

    /// Folder for "粘贴" / "新建" in this fence (the user desktop for desktop fences).
    pub fn target_folder(&self, id: u64) -> Option<PathBuf> {
        let fence = self.config.fence(id)?;
        if fence.is_desktop() { desktop::user_desktop() } else { fence.folder().cloned() }
    }

    /// New items appearing on the desktop within the next seconds join `fence`.
    pub fn expect_new_items(&mut self, fence: u64, names: Vec<String>) {
        if !self.config.fence(fence).is_some_and(|f| f.is_desktop()) { return; }
        let now = Instant::now();
        if names.is_empty() { self.pending.push(("*".into(), fence, now)); }
        for name in names { self.pending.push((name.to_lowercase(), fence, now)); }
    }

    pub fn open_in_explorer(&self, path: &Path) { self.open_explorer_args(&[path.as_os_str().to_owned()]); }

    pub fn open_entry_in_explorer(&self, entry: &ShellEntry) {
        if let Some(path) = &entry.path { self.open_in_explorer(path); } else { self.open_explorer_args(&[entry.key.clone().into()]); }
    }

    fn open_explorer_args(&self, args: &[std::ffi::OsString]) {
        if crate::edition::DESKTOP_ONLY{
            let target=args.first().cloned().unwrap_or_else(||"shell:MyComputerFolder".into());
            if let Err(error)=crate::app::open_original_explorer(target){self.notify_error(&format!("无法打开文件夹：{error:#}"));}
            return;
        }
        match crate::runtime::enqueue_local(&self.explorer_config,args.to_vec()) {
            Ok(true)=>return,
            Ok(false)=>{},
            Err(error)=>{self.notify_error(&format!("无法打开文件管理器：{error:#}"));return;}
        }
        let Ok(exe) = std::env::current_exe() else { return };
        let mut command = std::process::Command::new(exe);
        if let Some(dir) = self.explorer_config.parent() { command.arg("--profile-dir").arg(dir); }
        command.args(args);
        let _ = command.spawn();
    }

    // ---- drag and drop -------------------------------------------------------

    /// Chooses the Shell drop target under `point` (screen) for fence `id`.
    /// Over another tab's chip the drop goes into that tab.
    fn drop_target_at(&self, id: u64, point: POINT) -> Option<DropSpot> {
        let index = self.win_index(id)?;
        let fence = self.config.fence(id)?;
        let win = &self.fences[index];
        let mut client = point;
        let _ = unsafe { windows::Win32::Graphics::Gdi::ScreenToClient(win.hwnd, &mut client) };
        let g = win.geometry(fence);
        let internal = self.internal_drag.as_ref();
        let hit = g.hit(client.x, client.y, win.dpi, true, win.collapsed(fence));
        if let FenceHit::Item(i) = hit {
            let entry = &win.entries[i];
            let dragging_self = internal.is_some_and(|(_, keys)| keys.contains(&entry.key));
            if entry.can_drop && !dragging_self && (entry.is_container() || entry.path.as_ref().is_some_and(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("exe")))) {
                return Some(DropSpot { hover: i, tab: None, into: id, target: desktop::item_drop_target(win.hwnd, &entry.pidl).ok(), own: false });
            }
        }
        // A loose file is not a fence-sized destination. Only actual folder
        // or application drop targets above may accept a drop onto its icon.
        if win.standalone { return None; }
        let tab = match hit { FenceHit::Tab(i) if win.tabs[i].id != id => Some(i), _ => None };
        let into = tab.map_or(id, |i| win.tabs[i].id);
        let into_fence = self.config.fence(into)?;
        let spot = |target, own| Some(DropSpot { hover: usize::MAX, tab, into, target, own });
        let from_desktop_fence = internal.is_some_and(|(src, _)| self.config.fence(*src).is_some_and(|f| f.is_desktop()));
        if into_fence.is_desktop() && from_desktop_fence { return spot(None, true); }
        if !into_fence.is_desktop() && internal.is_some_and(|(src, _)| *src == into) { return spot(None, false); }
        let folder = self.target_folder(into)?;
        spot(desktop::folder_drop_target(win.hwnd, &folder).ok(), false)
    }

    pub fn drag_enter(&mut self, id: u64, data: &IDataObject, keys: u32, point: POINT, allowed: DROPEFFECT) -> DROPEFFECT {
        self.drop_data = Some(data.clone());
        self.drop_delegate = None;
        self.drag_over(id, keys, point, allowed)
    }

    pub fn drag_over(&mut self, id: u64, keys: u32, point: POINT, allowed: DROPEFFECT) -> DROPEFFECT {
        let Some(DropSpot { hover, tab, into, target, own }) = self.drop_target_at(id, point) else { return DROPEFFECT_NONE };
        let pt = POINTL { x: point.x, y: point.y };
        let mut effect = allowed;
        let same = self.drop_delegate.as_ref().is_some_and(|(f, h, _)| *f == into && *h == hover);
        if !same {
            if let Some((_, _, old)) = self.drop_delegate.take() { let _ = unsafe { old.DragLeave() }; }
            if let (Some(target), Some(data)) = (target, self.drop_data.clone()) {
                if unsafe { target.DragEnter(&data, MODIFIERKEYS_FLAGS(keys), pt, &mut effect) }.is_ok() {
                    self.drop_delegate = Some((into, hover, target));
                } else { effect = DROPEFFECT_NONE; }
            } else if !own { effect = DROPEFFECT_NONE; }
        } else if let Some((_, _, target)) = &self.drop_delegate {
            if unsafe { target.DragOver(MODIFIERKEYS_FLAGS(keys), pt, &mut effect) }.is_err() { effect = DROPEFFECT_NONE; }
        }
        if own { effect = DROPEFFECT_MOVE; }
        if let Some(index) = self.win_index(id) {
            let show = if effect == DROPEFFECT_NONE || tab.is_some() { None } else { Some(hover) };
            let tab_show = if effect == DROPEFFECT_NONE { None } else { tab };
            let win = &mut self.fences[index];
            if win.drop_hover != show || win.drop_tab != tab_show { win.drop_hover = show; win.drop_tab = tab_show; self.render(id); }
        }
        // Resting on another tab shows it, so the drop can target its items.
        match tab {
            Some(_) => match self.tab_hover {
                Some((t, since)) if t == into => if since.elapsed() >= TAB_HOVER_SWITCH { self.tab_hover = None; self.switch_tab(into); },
                _ => self.tab_hover = Some((into, Instant::now())),
            },
            None => self.tab_hover = None,
        }
        effect
    }

    pub fn drag_leave(&mut self, id: u64) {
        if let Some((_, _, old)) = self.drop_delegate.take() { let _ = unsafe { old.DragLeave() }; }
        self.drop_data = None;
        self.tab_hover = None;
        if let Some(index) = self.win_index(id) { self.fences[index].drop_hover = None; self.fences[index].drop_tab = None; self.render(id); }
    }

    /// Prepares a drop. Returns the Shell target to call outside the borrow, if any.
    pub fn take_drop(&mut self, id: u64, data: &IDataObject, point: POINT) -> Option<IDropTarget> {
        if let Some(index) = self.win_index(id) { self.fences[index].drop_hover = None; self.fences[index].drop_tab = None; }
        self.tab_hover = None;
        let spot = self.drop_target_at(id, point);
        let into = spot.as_ref().map_or(id, |s| s.into);
        let on_tab = spot.as_ref().is_some_and(|s| s.tab.is_some());
        let own = spot.is_some_and(|s| s.own && s.hover == usize::MAX);
        let delegate = self.drop_delegate.take().map(|(_, _, t)| t);
        self.drop_data = None;
        if own {
            if let Some(t) = delegate { let _ = unsafe { t.DragLeave() }; }
            if let Some((_, keys)) = self.internal_drag.clone() {
                let before = self.win_index(id).filter(|_| !on_tab).and_then(|i| {
                    let fence = self.config.fence(id)?;
                    let win = &self.fences[i];
                    let mut client = point;
                    let _ = unsafe { windows::Win32::Graphics::Gdi::ScreenToClient(win.hwnd, &mut client) };
                    let at = win.geometry(fence).insertion(client.x, client.y);
                    win.entries.get(at).map(|e| e.key.clone()).filter(|k| !keys.contains(k))
                });
                if let Some(fence) = self.config.fence_mut(into) {
                    if fence.sort != FenceSort::Manual && before.is_some() { fence.sort = FenceSort::Manual; }
                }
                self.config.assign(into, &keys, before.as_deref());
                self.schedule_save();
                self.refresh_desktop();
            }
            self.render(id);
            return None;
        }
        // Files dropped into a desktop fence land on the desktop and join this fence.
        if self.config.fence(into).is_some_and(|f| f.is_desktop()) {
            let names: Vec<String> = zsdesk_windows::ole::hdrop_paths(data).iter()
                .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())).collect();
            self.expect_new_items(into, names);
        }
        self.render(id);
        delegate
    }
}

fn fence_hidden_tab(config: &DesktopConfig, id: u64) -> bool { config.fence(id).is_some_and(Fence::is_hidden_tab) }

fn entry_category(entry: &ShellEntry) -> Category {
    if entry.is_link { return Category::Apps; }
    categorize(&entry.name_for_category(), entry.is_container(), entry.is_file_system)
}

trait CategoryName { fn name_for_category(&self) -> String; }
impl CategoryName for ShellEntry {
    /// Classify by the real file name (display names may hide extensions).
    fn name_for_category(&self) -> String {
        self.path.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| self.name.clone())
    }
}

fn item_info(entry: &ShellEntry) -> ItemInfo {
    ItemInfo { key: entry.key.clone(), name: entry.name.clone(), is_folder: entry.is_container(), category: entry_category(entry), size: entry.size, modified: entry.modified }
}

fn tray_data(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW { cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32, hWnd: hwnd, uID: TRAY_ID, ..Default::default() }
}

fn register_tray(data:&mut NOTIFYICONDATAW,mut notify:impl FnMut(NOTIFY_ICON_MESSAGE,&NOTIFYICONDATAW)->bool)->(bool,bool){
    if !notify(NIM_ADD,data){return(false,false);}
    // Explorer forgets the negotiated version whenever the icon is re-added.
    data.Anonymous.uVersion=NOTIFYICON_VERSION_4;
    (true,notify(NIM_SETVERSION,data))
}

#[cfg(test)]mod tray_protocol_tests{
    use super::*;
    #[test]fn add_and_explorer_readd_negotiate_version_four_each_time(){
        let mut messages=Vec::new();
        for _ in 0..2{
            let mut data=tray_data(HWND(123usize as*mut _));
            let state=register_tray(&mut data,|message,data|{
                assert_eq!(data.uID,TRAY_ID);messages.push(message);
                if message==NIM_SETVERSION{assert_eq!(unsafe{data.Anonymous.uVersion},NOTIFYICON_VERSION_4);}
                true
            });assert_eq!(state,(true,true));
        }
        assert_eq!(messages,vec![NIM_ADD,NIM_SETVERSION,NIM_ADD,NIM_SETVERSION]);
    }
    #[test]fn failed_add_never_negotiates_and_failed_version_retains_legacy_icon(){
        let mut data=tray_data(HWND::default());let mut messages=Vec::new();
        assert_eq!(register_tray(&mut data,|message,_|{messages.push(message);false}),(false,false));assert_eq!(messages,vec![NIM_ADD]);
        messages.clear();assert_eq!(register_tray(&mut data,|message,_|{messages.push(message);message==NIM_ADD}),(true,false));
        assert_eq!(messages,vec![NIM_ADD,NIM_SETVERSION]);
    }
}

fn screen_below(hwnd: HWND, r: zsdesk_ui::chrome::Rect) -> POINT {
    let mut p = POINT { x: r.x, y: r.bottom() };
    let _ = unsafe { windows::Win32::Graphics::Gdi::ClientToScreen(hwnd, &mut p) };
    p
}

// ---- monitors ----------------------------------------------------------------

fn monitor_info(monitor: HMONITOR) -> Option<Monitor> {
    let mut info = MONITORINFOEXW { monitorInfo: MONITORINFO { cbSize: std::mem::size_of::<MONITORINFOEXW>() as u32, ..Default::default() }, ..Default::default() };
    if !unsafe { GetMonitorInfoW(monitor, &mut info.monitorInfo) }.as_bool() { return None; }
    let (mut x, mut y) = (96u32, 96u32);
    let _ = unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut x, &mut y) };
    let end = info.szDevice.iter().position(|&c| c == 0).unwrap_or(info.szDevice.len());
    Some(Monitor { name: String::from_utf16_lossy(&info.szDevice[..end]), work: info.monitorInfo.rcWork, dpi: x.max(96), primary: info.monitorInfo.dwFlags & 1 != 0 })
}

pub fn monitors() -> Vec<Monitor> {
    unsafe extern "system" fn each(monitor: HMONITOR, _: HDC, _: *mut RECT, lp: LPARAM) -> BOOL {
        let list = unsafe { &mut *(lp.0 as *mut Vec<Monitor>) };
        if let Some(m) = monitor_info(monitor) { list.push(m); }
        true.into()
    }
    let mut list: Vec<Monitor> = Vec::new();
    let _ = unsafe { EnumDisplayMonitors(None, None, Some(each), LPARAM(&mut list as *mut Vec<Monitor> as isize)) };
    list
}

fn primary_monitor() -> Monitor {
    let list = monitors();
    list.iter().find(|m| m.primary).cloned().or_else(|| list.first().cloned())
        .unwrap_or(Monitor { name: String::new(), work: RECT { left: 0, top: 0, right: 1920, bottom: 1040 }, dpi: 96, primary: true })
}

fn monitor_from_point(point: POINT) -> Monitor {
    let monitor = unsafe { windows::Win32::Graphics::Gdi::MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST) };
    monitor_info(monitor).unwrap_or_else(primary_monitor)
}

fn unscale(v: i32, dpi: u32) -> i32 { ((v as i64 * 96 + dpi as i64 / 2) / dpi as i64) as i32 }

/// Placement (DIP relative to a monitor work area) → screen rectangle.
pub fn to_screen(p: &Placement, collapsed: bool) -> RECT {
    screen_rect(p, collapsed, false)
}
pub(super) fn item_screen_rect(p: &Placement) -> RECT { screen_rect(p, false, true) }
fn screen_rect(p: &Placement, collapsed: bool, item: bool) -> RECT {
    let list = monitors();
    let m = list.iter().find(|m| m.name == p.monitor).cloned().unwrap_or_else(primary_monitor);
    let s = |v: i32| scale(v, m.dpi);
    let w = s(p.width.max(if item { 72 } else { MIN_FENCE_W })).min(m.work.right - m.work.left);
    let h = if collapsed { s(TITLE_H) } else { s(p.height.max(if item { 84 } else { MIN_FENCE_H })).min(m.work.bottom - m.work.top) };
    let x = (m.work.left + s(p.x)).clamp(m.work.left, (m.work.right - w).max(m.work.left));
    let y = (m.work.top + s(p.y)).clamp(m.work.top, (m.work.bottom - h).max(m.work.top));
    RECT { left: x, top: y, right: x + w, bottom: y + h }
}

pub fn from_screen(rect: RECT, keep_height: Option<i32>) -> Placement {
    let monitor = unsafe { MonitorFromRect(&rect, MONITOR_DEFAULTTONEAREST) };
    let m = monitor_info(monitor).unwrap_or_else(primary_monitor);
    Placement {
        monitor: m.name, x: unscale(rect.left - m.work.left, m.dpi), y: unscale(rect.top - m.work.top, m.dpi),
        width: unscale(rect.right - rect.left, m.dpi), height: keep_height.unwrap_or_else(|| unscale(rect.bottom - rect.top, m.dpi)),
    }
}

pub fn load_config(path: &Path) -> DesktopConfig {
    let mut config: DesktopConfig = std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    config.normalize();
    config
}

pub fn save_config(path: &Path, config: &DesktopConfig) -> std::io::Result<()> {
    if let Some(parent) = path.parent() { std::fs::create_dir_all(parent)?; }
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, serde_json::to_vec_pretty(config).map_err(std::io::Error::other)?)?;
    std::fs::rename(&temporary, path)
}

#[cfg(test)]
mod shortcut_tests {
    use super::*;
    use windows::Win32::UI::Input::KeyboardAndMouse::{VK_C, VK_Y, VK_Z};
    #[test]
    fn fence_control_wheel_accumulates_and_clamps_icon_zoom() {
        let mut remainder=0;
        assert_eq!(zoom_steps(&mut remainder,60),0);
        assert_eq!(zoom_steps(&mut remainder,60),1);
        assert_eq!(zoom_icon_size(IconSize::Medium,1),IconSize::Large);
        assert_eq!(zoom_icon_size(IconSize::Large,16),IconSize::Large);
        assert_eq!(zoom_steps(&mut remainder,-240),-2);
        assert_eq!(zoom_icon_size(IconSize::Large,-2),IconSize::Small);
        assert_eq!(zoom_icon_size(IconSize::Small,-16),IconSize::Small);
    }
    #[test]
    fn loose_desktop_icon_placement_avoids_fences_and_stays_on_monitor() {
        let origin=Bounds::new(100,100,96,104);
        let occupied=vec![Bounds::new(0,0,192,208)];
        let cell=desktop_item_position(origin,800,600,&occupied);
        assert!(cell.x>=192||cell.y>=208);
        assert!(cell.x>=0&&cell.y>=0&&cell.right()<=800&&cell.bottom()<=600);
        let outside=desktop_item_position(Bounds::new(-500,900,96,104),800,600,&[]);
        assert_eq!((outside.x,outside.y),(0,496));
    }
    #[test]
    fn fence_startup_command_binds_the_exact_profile() {
        let exe=Path::new(r"C:\Apps\ZSDesk Theme\ZSDesk.exe");
        let main=fence_startup_command(exe,Path::new(r"C:\Profiles\中文 Main\desktop-fences.json")).unwrap();
        assert_eq!(main,r#""C:\Apps\ZSDesk Theme\ZSDesk.exe" --desktop --profile-dir "C:\Profiles\中文 Main""#);
        let other=fence_startup_command(exe,Path::new(r"C:\Profiles\Other\desktop-fences.json")).unwrap();
        assert!(!desktop::autostart_owned(&main,&other));
        assert!(fence_startup_command(Path::new("ZSDesk.exe"),Path::new("desktop-fences.json")).is_none());
    }
    #[test]
    fn fence_history_shortcuts_require_exact_modifiers() {
        assert_eq!(history_shortcut(VK_Z.0, true, false, false), Some(HistoryAction::Undo));
        assert_eq!(history_shortcut(VK_Y.0, true, false, false), Some(HistoryAction::Redo));
        for key in [VK_Z, VK_Y] {
            assert_eq!(history_shortcut(key.0, false, false, false), None);
            assert_eq!(history_shortcut(key.0, true, true, false), None);
            assert_eq!(history_shortcut(key.0, true, false, true), None);
        }
        assert_eq!(history_shortcut(VK_C.0, true, false, false), None);
    }
    #[test]
    fn restored_member_keeps_its_saved_group_during_pending_collection() {
        let original = r"E:\fixture\恢复 文件.txt";
        let restored = r"e:\fixture\恢复 文件.txt";
        let mut saved = Fence::new(1, "Saved", FenceSource::Desktop, Placement::default());
        saved.members.push(original.into());
        let mut collecting = Fence::new(2, "Collecting", FenceSource::Desktop, Placement::default());
        collecting.catch_all = true;
        let config = DesktopConfig { fences: vec![saved, collecting], ..Default::default() };
        assert!(distribute(&config.fences, &[]).iter().all(Vec::is_empty));
        let mut config: DesktopConfig = serde_json::from_str(&serde_json::to_string(&config).unwrap()).unwrap();
        config.normalize();
        let known = HashSet::new();
        if pending_collection_allowed(&config, &known, restored) { config.assign(2, &[restored.into()], None); }
        assert_eq!(config.fence(1).unwrap().members, vec![original]);
        let item = ItemInfo { key: restored.into(), name: "恢复 文件.txt".into(), is_folder: false,
            category: Category::Documents, size: None, modified: None };
        assert_eq!(distribute(&config.fences, &[item]), vec![vec![0], vec![]]);
        assert!(pending_collection_allowed(&config, &known, r"E:\fixture\新建 文件.txt"));
    }
}

#[cfg(test)]
mod memory_tests {
    use super::*;
    #[test]
    fn cold_icon_cache_is_lru_bounded_without_evicting_visible_icons() {
        let mut icons = HashMap::new();
        for i in 0..500u64 {
            let mut slot = IconSlot::new((None, None), i);
            slot.source_bytes = 128 * 1024;
            icons.insert((format!("file-{i}"), 48), slot);
        }
        let visible = HashSet::from([("file-0".to_string(), 48)]);
        trim_cold_icons(&mut icons, &visible);
        assert!(icons.contains_key(&("file-0".into(), 48)));
        assert!(!icons.contains_key(&("file-1".into(), 48)));
        assert!(icons.contains_key(&("file-499".into(), 48)));
        let cold: Vec<_> = icons.iter().filter(|(key,_)| !visible.contains(*key)).collect();
        assert!(cold.len() <= MAX_COLD_ICONS);
        assert!(cold.iter().map(|(_,slot)|slot.pixel_bytes()).sum::<u64>() <= MAX_COLD_ICON_BYTES);
    }
    #[test]
    fn cold_pending_entries_are_count_bounded_even_without_bitmap_bytes() {
        let mut icons: HashMap<_,_> = (0..500u64).map(|i| ((i.to_string(),48),IconSlot::new((None,None),i))).collect();
        trim_cold_icons(&mut icons,&HashSet::new());
        assert_eq!(icons.len(),MAX_COLD_ICONS);
    }
}

#[cfg(test)]
mod desktop_publish_tests {
    use super::*;
    use anyhow::{Context,ensure};
    use std::{ffi::c_void,fs,thread};
    #[link(name="user32")]unsafe extern "system" {
        fn CreateDesktopW(name:*const u16,device:*const u16,mode:*const c_void,flags:u32,access:u32,attributes:*const c_void)->*mut c_void;
        fn SetThreadDesktop(desktop:*mut c_void)->i32;
        fn CloseDesktop(desktop:*mut c_void)->i32;
    }
    struct Apartment;
    impl Drop for Apartment {fn drop(&mut self){unsafe{windows::Win32::System::Ole::OleUninitialize();}}}
    struct FixtureHost(DesktopHost);
    impl Drop for FixtureHost {fn drop(&mut self){
        self.0.loader.shutdown();
        for window in &mut self.0.fences {window.drop=None;window.notify=None;let _=unsafe{DestroyWindow(window.hwnd)};}
        self.0.fences.clear();let _=unsafe{DestroyWindow(self.0.hwnd)};
    }}
    fn run(root:&Path)->anyhow::Result<serde_json::Value>{
        unsafe{windows::Win32::System::Ole::OleInitialize(None)?;}let _apartment=Apartment;
        let instance=unsafe{windows::Win32::System::LibraryLoader::GetModuleHandleW(None)?};
        let class:Vec<u16>=FENCE_CLASS.encode_utf16().chain(Some(0)).collect();
        unsafe{RegisterClassW(&WNDCLASSW{lpfnWndProc:Some(super::super::fence_proc),hInstance:windows::Win32::Foundation::HINSTANCE(instance.0),lpszClassName:PCWSTR(class.as_ptr()),..Default::default()});}
        let hwnd=unsafe{CreateWindowExW(WS_EX_NOACTIVATE,windows::core::w!("STATIC"),PCWSTR::null(),WS_POPUP,0,0,1,1,None,None,None,None)}?;
        let files=root.join("files");fs::create_dir_all(&files)?;
        let keys:Vec<String>=(0..3).map(|index|files.join(format!("fixture-{index}.txt")).to_string_lossy().into_owned()).collect();
        for key in &keys {fs::write(key,b"desktop-publish-fixture")?;}
        let mut config=DesktopConfig::default();config.settings.organize=true;config.settings.icons_hidden_by_zsdesk=false;config.settings.hide_tray=true;
        let mut group=Fence::new(1,"Owned fixture",FenceSource::Desktop,Placement{x:20,y:20,width:320,height:240,..Default::default()});
        group.members=keys.clone();group.catch_all=true;config.fences.push(group);config.next_id=2;
        let mut fixture=FixtureHost(DesktopHost::new(hwnd,root.join("profile/desktop-fences.json"),root.join("profile/explorer-session.json"),config,Gfx::new("Segoe UI")?));
        let host=&mut fixture.0;
        host.create_window(1,false).map_err(anyhow::Error::msg)?;host.refresh_desktop();
        ensure!(host.release_desktop_keys(&keys[..2],POINT{x:500,y:300}).map_err(anyhow::Error::msg)?==2,"two loose icons were not published");
        let ids:Vec<_>=host.config.released_items.iter().map(|item|item.id).collect();
        ensure!(ids.len()==2,"released membership missing");
        for &id in &ids {let index=host.win_index(id).context("published window")?;let key=host.fences[index].entries[0].key.clone();host.fences[index].selection.insert(key);}
        let first=host.win_index(ids[0]).unwrap();let window=&host.fences[first];let geometry=window.geometry(host.config.fence(ids[0]).unwrap());
        let point=POINT{x:geometry.cells[0].icon.x+4,y:geometry.cells[0].icon.y+4};
        host.mouse_down(ids[0],point,false);
        let action=host.mouse_move(ids[0],POINT{x:point.x+40,y:point.y});
        ensure!(matches!(action,After::Drag{..})&&host.selected_entries_owned(ids[0]).len()==2,"drag collapsed a multi-selection");
        let _=unsafe{windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture()};
        let window=&host.fences[host.win_index(ids[0]).unwrap()];let mut screen=point;
        unsafe{windows::Win32::Graphics::Gdi::ClientToScreen(window.hwnd,&mut screen)}.ok()?;
        ensure!(host.drop_target_at(ids[0],screen).is_none(),"ordinary loose file accepted a fence-membership drop");
        let before=host.config.clone();let windows=host.fences.len();let blocker=root.join("not-a-directory");fs::write(&blocker,b"blocker")?;
        host.config_path=blocker.join("desktop-fences.json");
        ensure!(host.release_desktop_keys(&keys[2..],POINT{x:700,y:300}).is_err(),"failed save was reported as successful release");
        ensure!(host.config==before&&host.fences.len()==windows,"failed publish did not restore configuration and windows");
        let group=host.fences.iter().find(|window|window.id==1).context("original group")?;
        ensure!(group.entries.iter().any(|entry|entry.key.eq_ignore_ascii_case(&keys[2])),"rollback hid original grouped item");
        for key in &keys {ensure!(fs::read(key)?==b"desktop-publish-fixture","visual transaction changed a file");}
        Ok(serde_json::json!({"published_icons_visible":true,"multi_selection_drag_preserved":true,"ordinary_file_rejects_membership_drop":true,
            "failed_save_rolls_back_windows_and_membership":true,"files_unchanged":true,"real_desktop_modified":false}))
    }
    #[test]
    #[ignore="Native loose-icon transaction and selection test on private desktop; requires ZSDESK_NATIVE_TEST_OUTPUT"]
    fn native_released_icons_publish_transaction_preserves_files_and_selection()->anyhow::Result<()> {
        let root=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("isolated output")?).join("desktop-publish");
        ensure!(root.is_absolute(),"absolute output required");fs::create_dir_all(&root)?;
        let name:Vec<u16>=format!("ZSDesk.Publish.{}",std::process::id()).encode_utf16().chain(Some(0)).collect();
        let desktop=unsafe{CreateDesktopW(name.as_ptr(),std::ptr::null(),std::ptr::null(),0,0x000f00ff,std::ptr::null())};
        ensure!(!desktop.is_null(),"private desktop creation failed");let raw=desktop as usize;let output=root.clone();
        let result=thread::spawn(move||->anyhow::Result<serde_json::Value>{ensure!(unsafe{SetThreadDesktop(raw as*mut c_void)}!=0,"private desktop attach failed");run(&output)})
            .join().map_err(|_|anyhow::anyhow!("desktop transaction test panicked")).and_then(|result|result);
        let closed=unsafe{CloseDesktop(desktop)}!=0;
        let report=serde_json::json!({"state":if result.is_ok()&&closed{"passed"}else{"failed"},"checks":result.as_ref().map_err(|error|format!("{error:#}")),"desktop_closed":closed});
        fs::write(root.join("verification.current.json"),serde_json::to_vec_pretty(&report)?)?;
        result?;ensure!(closed,"private desktop did not close");Ok(())
    }
}
