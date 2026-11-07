//! Desktop fences (桌面格子).
//!
//! Runs as `ZSDesk.exe --desktop` in its own single-instance process with a
//! tray icon. Fences are top-level layered windows owned by `Progman`, so they
//! stay on the desktop layer and survive "显示桌面". Organize mode hides
//! Explorer's desktop icons through the Shell view only after a watchdog
//! process (`--desktop-guard`) confirmed it will restore them if this process
//! dies.

mod fence;
mod host;
pub(crate) mod migration;
mod tencent_import;

use std::{cell::{Cell, RefCell}, collections::hash_map::DefaultHasher, hash::{Hash, Hasher}, path::PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use windows::{
    core::{w, PCWSTR},
    Win32::{
        Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, POINTL, WPARAM},
        Graphics::Gdi::{CreateFontIndirectW, DeleteObject, HFONT},
        System::{
            Com::IDataObject,
            DataExchange::COPYDATASTRUCT,
            LibraryLoader::GetModuleHandleW,
            Ole::{OleInitialize, OleUninitialize, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE, DROPEFFECT_NONE},
            SystemServices::MODIFIERKEYS_FLAGS,
            Threading::{CreateMutexW, OpenEventW, OpenProcess, SetEvent, WaitForSingleObject, EVENT_MODIFY_STATE, INFINITE, PROCESS_SYNCHRONIZE},
        },
        UI::{
            Controls::WM_MOUSELEAVE,
            HiDpi::SystemParametersInfoForDpi,
            Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL, VK_ESCAPE, VK_RETURN, VK_SHIFT},
            Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass, SHDoDragDrop, SHGetDesktopFolder, IShellView},
            WindowsAndMessaging::*,
        },
    },
};
use zsdesk_ui::menus::{entry, MenuItemSpec, MenuSpec, UiCommand};
use zsdesk_windows::{desktop::{self, DesktopHit}, gfx::Gfx, ole::DropHandler, shell::{menu::{forward_menu_message, HistoryAction, ShellMenu, APP_FIRST}, notify::read_change}};

use host::{After, DesktopHost, TIMER_DESKTOP, TIMER_FOLDER, TIMER_PEEK, TIMER_SAVE, TIMER_ICONS, WM_DESKTOP_CHANGED, WM_DESKTOP_DBLCLICK, WM_FOLDER_CHANGED,
    WM_ICON_READY, WM_RENAME_DONE, WM_TRAY};

const HOST_CLASS: &str = "ZSDesk.Native.DesktopHost";
const DESKTOP_MAGIC: usize = 0x5A53_4443;
const WM_COMMAND_REQUEST: u32 = WM_APP + 20;
const WM_RENAME_REQUEST: u32 = WM_APP + 21;
const WM_FOREGROUND: u32 = WM_APP + 22;
const WM_REPAIR_LAYER: u32 = WM_APP + 23;
const TIMER_RAISE: usize = 9;
const TIMER_REPAIR_LAYER: usize = 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DesktopClick { root: isize, time: u32, x: i32, y: i32 }

/// Request handed to the desktop host (also forwarded between processes).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DesktopCommand { Show, Organize, Restore, NewFence, Map(PathBuf), Tray, Quit, Diagnostics(PathBuf) }

thread_local! {
    static HOST: RefCell<Option<DesktopHost>> = const { RefCell::new(None) };
    static HOOK: Cell<isize> = const { Cell::new(0) };
    static HOST_HWND: Cell<isize> = const { Cell::new(0) };
    static LAST_CLICK: Cell<Option<DesktopClick>> = const { Cell::new(None) };
    /// Only explicit desktop-layer updates may change a fence's z-order.
    static ALLOW_Z: Cell<bool> = const { Cell::new(false) };
}

/// Moves a fence in the z-order without the layer guard interfering.
pub(super) fn set_z(hwnd: HWND, after: HWND) {
    let previous = ALLOW_Z.with(|z| z.replace(true));
    let _ = unsafe { SetWindowPos(hwnd, Some(after), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER) };
    ALLOW_Z.with(|z| z.set(previous));
}

fn request_layer_repair() {
    let host = HOST_HWND.with(Cell::get);
    if host != 0 { let _ = unsafe { PostMessageW(Some(HWND(host as *mut _)), WM_REPAIR_LAYER, WPARAM(0), LPARAM(0)) }; }
}

struct MenuLayerRepair;
impl Drop for MenuLayerRepair { fn drop(&mut self) { request_layer_repair(); } }

/// Foreground changes: "显示桌面" raises a WorkerW above every window, so fences
/// follow the desktop window instead of staying pinned beneath it.
unsafe extern "system" fn foreground_hook(_: windows::Win32::UI::Accessibility::HWINEVENTHOOK, _: u32, hwnd: HWND, _: i32, _: i32, _: u32, _: u32) {
    let host = HOST_HWND.with(Cell::get);
    let _ = unsafe { PostMessageW(Some(HWND(host as *mut _)), WM_FOREGROUND, WPARAM(0), LPARAM(hwnd.0 as isize)) };
}

/// Runs `f` on the host unless it is already borrowed by an outer call.
fn with_host<R>(f: impl FnOnce(&mut DesktopHost) -> R) -> Option<R> {
    HOST.with(|h| h.try_borrow_mut().ok().and_then(|mut h| h.as_mut().map(f)))
}

fn instance_key(config: &std::path::Path) -> Vec<u16> {
    let mut h = DefaultHasher::new();
    config.to_string_lossy().to_lowercase().hash(&mut h);
    format!("Local\\ZSDesk.Desktop.{:016x}", h.finish()).encode_utf16().chain(Some(0)).collect()
}

pub fn run(config_path: PathBuf, explorer_config: PathBuf, command: DesktopCommand) -> Result<()> {
    let key = instance_key(&config_path);
    let mutex = unsafe { CreateMutexW(None, false, PCWSTR(key.as_ptr()))? };
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        let _ = unsafe { CloseHandle(mutex) };
        return forward(&key, &command);
    }
    if command == DesktopCommand::Quit { let _ = unsafe { CloseHandle(mutex) }; return Ok(()); }
    if matches!(command, DesktopCommand::Diagnostics(_)) {
        let _ = unsafe { CloseHandle(mutex) };
        anyhow::bail!("当前配置的桌面格子未运行，未为诊断启动任何窗口");
    }
    if let Err(error) = unsafe { OleInitialize(None) }.context("OleInitialize failed") {
        let _ = unsafe { CloseHandle(mutex) };
        return Err(error);
    }
    let result = run_host(config_path, explorer_config, command, &key);
    unsafe { OleUninitialize(); }
    let _ = unsafe { CloseHandle(mutex) };
    result
}

fn forward(key: &[u16], command: &DesktopCommand) -> Result<()> {
    let bytes = serde_json::to_vec(command)?;
    for _ in 0..50 {
        let mut previous = None;
        while let Ok(hwnd) = unsafe { FindWindowExW(None, previous, PCWSTR(wide(HOST_CLASS).as_ptr()), None) } {
            previous = Some(hwnd);
            if unsafe { GetPropW(hwnd, PCWSTR(key.as_ptr())) }.0 as usize != 1 { continue; }
            let data = COPYDATASTRUCT { dwData: DESKTOP_MAGIC, cbData: bytes.len() as u32, lpData: bytes.as_ptr() as *mut _ };
            let mut result = 0;
            unsafe { SendMessageTimeoutW(hwnd, WM_COPYDATA, WPARAM(0), LPARAM(&data as *const COPYDATASTRUCT as isize), SMTO_ABORTIFHUNG, 3000, Some(&mut result)) };
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(60));
    }
    anyhow::bail!("桌面格子正在启动，请稍后重试")
}

fn run_host(config_path: PathBuf, explorer_config: PathBuf, command: DesktopCommand, key: &[u16]) -> Result<()> {
    let instance = HINSTANCE(unsafe { GetModuleHandleW(None)? }.0);
    let host_class = wide(HOST_CLASS);
    let fence_class = wide(host::FENCE_CLASS);
    unsafe {
        RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(host_proc), hInstance: instance, lpszClassName: PCWSTR(host_class.as_ptr()), ..Default::default() });
        RegisterClassW(&WNDCLASSW { style: CS_DBLCLKS, lpfnWndProc: Some(fence_proc), hInstance: instance,
            hCursor: LoadCursorW(None, IDC_ARROW)?, lpszClassName: PCWSTR(fence_class.as_ptr()), ..Default::default() });
    }
    let hwnd = unsafe { CreateWindowExW(WS_EX_TOOLWINDOW, PCWSTR(host_class.as_ptr()), w!("ZSDesk 桌面格子"), WS_POPUP, 0, 0, 0, 0, None, None, Some(instance), None)? };
    unsafe { SetPropW(hwnd, PCWSTR(key.as_ptr()), Some(HANDLE(1usize as *mut _)))?; }
    HOST_HWND.with(|h| h.set(hwnd.0 as isize));
    let (family, _) = zsdesk_windows::gfx::system_ui_font(None);
    let gfx = Gfx::new(&family).context("Direct2D 初始化失败")?;
    let config = host::load_config(&config_path);
    let hook = config.settings.double_click_toggle;
    HOST.with(|h| *h.borrow_mut() = Some(DesktopHost::new(hwnd, config_path, explorer_config, config, gfx)));
    with_host(|h| h.start());
    update_hook(hook);
    let foreground = unsafe { windows::Win32::UI::Accessibility::SetWinEventHook(EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND, None,
        Some(foreground_hook), 0, 0, WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS) };
    execute(command);
    let mut msg = MSG::default();
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
        unsafe { let _ = TranslateMessage(&msg); DispatchMessageW(&msg); }
    }
    update_hook(false);
    if !foreground.is_invalid() { let _ = unsafe { windows::Win32::UI::Accessibility::UnhookWinEvent(foreground) }; }
    if let Some(mut host) = HOST.with(|h| h.borrow_mut().take()) { host.shutdown(); }
    Ok(())
}

fn execute(command: DesktopCommand) {
    // DestroyWindow dispatches WM_DESTROY synchronously. End the HOST borrow
    // before calling it so shutdown can actually borrow and clean up the host.
    if command == DesktopCommand::Quit {
        if let Some(hwnd) = with_host(|h| h.hwnd) { let _ = unsafe { DestroyWindow(hwnd) }; }
        return;
    }
    let after = with_host(|h| match command {
        DesktopCommand::Show => { h.show_all(); After::Nothing }
        DesktopCommand::Organize => { h.organize(); After::Nothing }
        DesktopCommand::Restore => { h.restore_desktop(); After::Nothing }
        DesktopCommand::NewFence => { h.new_desktop_fence(None); After::Nothing }
        DesktopCommand::Map(path) => { h.add_folder_fence(path, None); After::Nothing }
        DesktopCommand::Tray => { h.show_tray(); After::Nothing }
        DesktopCommand::Quit => After::Nothing,
        DesktopCommand::Diagnostics(path) => {
            if let Err(error) = h.write_diagnostics(&path) { eprintln!("桌面资源诊断失败：{error:#}"); }
            After::Nothing
        }
    });
    if let Some(after) = after { run_after(after); }
}

/// Watchdog: restores desktop icons if the host process exits while they are hidden.
pub fn guard(config_path: PathBuf, pid: u32) -> Result<()> {
    let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid)? };
    let name = wide(&format!("Local\\ZSDesk.DesktopGuard.{pid}"));
    if let Ok(event) = unsafe { OpenEventW(EVENT_MODIFY_STATE, false, PCWSTR(name.as_ptr())) } {
        let _ = unsafe { SetEvent(event) };
        let _ = unsafe { CloseHandle(event) };
    }
    unsafe { WaitForSingleObject(process, INFINITE) };
    let _ = unsafe { CloseHandle(process) };
    let mut config = host::load_config(&config_path);
    if config.settings.icons_hidden_by_zsdesk {
        unsafe { OleInitialize(None)?; }
        desktop::set_icons_visible(true);
        unsafe { OleUninitialize(); }
        config.settings.icons_hidden_by_zsdesk = false;
        host::save_config(&config_path, &config)?;
    }
    Ok(())
}

// ---- window procedures --------------------------------------------------------

unsafe extern "system" fn host_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if let Some(result) = forward_menu_message(msg, wp, lp) { return result; }
    let taskbar_created = with_host(|h| h.taskbar_created).unwrap_or(0);
    if msg == taskbar_created && msg != 0 {
        with_host(|h| h.explorer_restarted());
        return LRESULT(0);
    }
    match msg {
        WM_COPYDATA if lp.0 != 0 => {
            let data = unsafe { &*(lp.0 as *const COPYDATASTRUCT) };
            if data.dwData != DESKTOP_MAGIC || data.cbData > 65536 || data.lpData.is_null() { return LRESULT(0); }
            let bytes = unsafe { std::slice::from_raw_parts(data.lpData.cast::<u8>(), data.cbData as usize) };
            if let Ok(command) = serde_json::from_slice::<DesktopCommand>(bytes) {
                let payload = Box::into_raw(Box::new(command));
                if unsafe { PostMessageW(Some(hwnd), WM_COMMAND_REQUEST, WPARAM(0), LPARAM(payload as isize)) }.is_err() { unsafe { drop(Box::from_raw(payload)); } }
            }
            return LRESULT(1);
        }
        WM_COMMAND_REQUEST if lp.0 != 0 => { execute(*unsafe { Box::from_raw(lp.0 as *mut DesktopCommand) }); return LRESULT(0); }
        WM_RENAME_REQUEST if lp.0 != 0 => {
            let (fence, key, name) = *unsafe { Box::from_raw(lp.0 as *mut (u64, String, String)) };
            run_after(After::Rename { fence, key, name });
            return LRESULT(0);
        }
        WM_TRAY => {
            let event = (lp.0 & 0xFFFF) as u32;
            if event == WM_RBUTTONUP || event == WM_CONTEXTMENU {
                let mut p = POINT::default();
                let _ = unsafe { GetCursorPos(&mut p) };
                run_after(After::TrayMenu(p));
            } else if event == WM_LBUTTONDBLCLK {
                with_host(|h| h.menu_command(None, "open-zsdesk", POINT::default()));
            } else if event == WM_LBUTTONUP {
                with_host(|h| h.show_all());
            }
            return LRESULT(0);
        }
        WM_DESKTOP_CHANGED => {
            let change = read_change(wp, lp);
            with_host(|h| h.invalidate_changed_icons(change.first.as_deref(), change.second.as_deref()));
            if change.is_rename() {
                if let (Some(old), Some(new)) = (change.first.as_deref(), change.second.as_deref()) {
                    with_host(|h| h.rename_notified(old, new));
                }
            }
            unsafe { SetTimer(Some(hwnd), TIMER_DESKTOP, 250, None); }
            return LRESULT(0);
        }
        WM_TIMER if wp.0 == TIMER_RAISE => {
            let _ = unsafe { KillTimer(Some(hwnd), TIMER_RAISE) };
            let window = unsafe { GetForegroundWindow() };
            if matches!(class_name(window).as_str(), "WorkerW" | "Progman") { with_host(|h| h.raise_above(window)); }
            return LRESULT(0);
        }
        WM_REPAIR_LAYER | WM_TIMER if msg == WM_REPAIR_LAYER || wp.0 == TIMER_REPAIR_LAYER => {
            let _ = unsafe { KillTimer(Some(hwnd), TIMER_REPAIR_LAYER) };
            if with_host(|h| h.repair_layer()).is_none() { unsafe { SetTimer(Some(hwnd), TIMER_REPAIR_LAYER, 30, None); } }
            return LRESULT(0);
        }
        WM_TIMER => {
            let done = with_host(|h| {
                if wp.0 == TIMER_SAVE { h.save_now(); }
                if wp.0 == TIMER_DESKTOP { let _ = unsafe { KillTimer(Some(h.hwnd), TIMER_DESKTOP) }; h.refresh_desktop(); }
                if wp.0 == TIMER_ICONS { let _ = unsafe { KillTimer(Some(h.hwnd), TIMER_ICONS) }; h.icons_ready(); }
            });
            // Busy with a modal operation: try again shortly.
            if done.is_none() { unsafe { SetTimer(Some(hwnd), wp.0, 500, None); } }
            return LRESULT(0);
        }
        WM_ICON_READY => {
            if with_host(|h| h.icons_ready()).is_none() { unsafe { SetTimer(Some(hwnd), TIMER_ICONS, 50, None); } }
            return LRESULT(0);
        }
        WM_DESKTOP_DBLCLICK => {
            let point = unpack_screen_point(lp.0);
            if desktop_click_root(point) != Some(HWND(wp.0 as *mut _)) { return LRESULT(0); }
            let organize = with_host(|h| h.config.settings.organize).unwrap_or(false);
            let hit = desktop::classify_point(point);
            if hit == DesktopHit::Empty || (organize && hit == DesktopHit::Item) {
                with_host(|h| h.toggle_hidden());
            }
            return LRESULT(0);
        }
        WM_FOREGROUND => {
            let window = HWND(lp.0 as *mut _);
            let class = class_name(window);
            if class == "WorkerW" || class == "Progman" {
                with_host(|h| h.raise_above(window));
                // "显示桌面" may finish raising its WorkerW after the event: raise again shortly.
                unsafe { SetTimer(Some(hwnd), TIMER_RAISE, 180, None); }
            }
            else if !class.starts_with("ZSDesk.") && class != "#32768" { with_host(|h| h.lower_all(window)); }
            return LRESULT(0);
        }
        WM_DISPLAYCHANGE => { with_host(|h| h.place_all()); return LRESULT(0); }
        WM_SETTINGCHANGE => {
            let area = if lp.0 != 0 { unsafe { PCWSTR(lp.0 as *const u16).to_string() }.unwrap_or_default() } else { String::new() };
            if wp.0 as u32 == SPI_SETWORKAREA.0 { with_host(|h| h.place_all()); }
            if area == "ImmersiveColorSet" { with_host(|h| { h.update_palette(); h.render_all(); }); }
            if area == "ShellState" { unsafe { SetTimer(Some(hwnd), TIMER_DESKTOP, 250, None); } }
            return LRESULT(0);
        }
        WM_QUERYENDSESSION => return LRESULT(1),
        WM_ENDSESSION => { if wp.0 != 0 { with_host(|h| h.shutdown()); } return LRESULT(0); }
        WM_DESTROY => {
            with_host(|h| h.shutdown());
            unsafe { PostQuitMessage(0); }
            return LRESULT(0);
        }
        _ => {}
    }
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

fn point_of(lp: LPARAM) -> POINT { POINT { x: (lp.0 as i16) as i32, y: ((lp.0 >> 16) as i16) as i32 } }

unsafe extern "system" fn fence_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg != 0 && msg == zsdesk_windows::shell::menu::failure_message() {
        let text = wide(&format!("右键菜单加载失败（0x{:08X}），隐藏设置仍保留。", wp.0 as u32));
        unsafe { MessageBoxW(Some(hwnd), PCWSTR(text.as_ptr()), w!("ZSDesk 桌面格子"), MB_OK | MB_ICONWARNING); }
        request_layer_repair();
        return LRESULT(0);
    }
    if let Some(result) = forward_menu_message(msg, wp, lp) { return result; }
    let id = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as u64;
    if id == 0 { return unsafe { DefWindowProcW(hwnd, msg, wp, lp) }; }
    match msg {
        WM_MOUSEMOVE => { if let Some(a) = with_host(|h| h.mouse_move(id, point_of(lp))) { run_after(a); } return LRESULT(0); }
        WM_ACTIVATE | WM_SETFOCUS | WM_ENTERMENULOOP | WM_EXITMENULOOP => { request_layer_repair(); }
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
            if let Some(a) = with_host(|h| h.mouse_down(id, point_of(lp), msg == WM_LBUTTONDBLCLK)) { run_after(a); }
            return LRESULT(0);
        }
        WM_LBUTTONUP => { if let Some(a) = with_host(|h| h.mouse_up(id, point_of(lp))) { run_after(a); } return LRESULT(0); }
        WM_RBUTTONUP => { if let Some(a) = with_host(|h| h.right_click(id, point_of(lp))) { run_after(a); } return LRESULT(0); }
        WM_CONTEXTMENU => return LRESULT(0),
        WM_MOUSEWHEEL => { with_host(|h| h.wheel(id, ((wp.0 >> 16) as i16) as i32)); return LRESULT(0); }
        WM_MOUSELEAVE => { with_host(|h| h.mouse_leave(id)); return LRESULT(0); }
        WM_CAPTURECHANGED => {
            with_host(|h| if let Some(i) = h.win_index(id) {
                if h.fences[i].interaction.take().is_some() { let r = h.fences[i].window_rect(); let _ = r; h.render(id); }
            });
            return LRESULT(0);
        }
        WM_KEYDOWN | WM_SYSKEYDOWN if msg == WM_KEYDOWN => {
            if let Some(a) = with_host(|h| h.key(id, wp.0 as u16)) { run_after(a); }
            return LRESULT(0);
        }
        WM_SETCURSOR if (lp.0 & 0xFFFF) as u32 == HTCLIENT => {
            let cursor = with_host(|h| {
                let i = h.win_index(id)?;
                let fence = h.config.fence(id)?;
                let mut p = POINT::default();
                let _ = unsafe { GetCursorPos(&mut p) };
                let _ = unsafe { windows::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut p) };
                let g = h.fences[i].geometry(fence);
                match g.hit(p.x, p.y, h.fences[i].dpi, fence.locked, h.fences[i].collapsed(fence)) {
                    fence::FenceHit::Edge(e) => Some(match (e.left || e.right, e.top || e.bottom) {
                        (true, true) if (e.left && e.top) || (e.right && e.bottom) => IDC_SIZENWSE,
                        (true, true) => IDC_SIZENESW,
                        (true, false) => IDC_SIZEWE,
                        _ => IDC_SIZENS,
                    }),
                    _ => None,
                }
            }).flatten();
            if let Some(cursor) = cursor {
                if let Ok(c) = unsafe { LoadCursorW(None, cursor) } { unsafe { SetCursor(Some(c)); } }
                return LRESULT(1);
            }
        }
        WM_WINDOWPOSCHANGING if lp.0 != 0 => {
            // Preserve the current desktop layer during hover resizing, drawing,
            // and activation. HWND_BOTTOM would put a fence behind the WorkerW
            // raised by Show Desktop, making it disappear as soon as it expands.
            let pos = unsafe { &mut *(lp.0 as *mut WINDOWPOS) };
            if !ALLOW_Z.with(Cell::get) {
                pos.flags |= SWP_NOZORDER;
            }
            return LRESULT(0);
        }
        WM_DPICHANGED => {
            with_host(|h| if let Some(i) = h.win_index(id) {
                h.fences[i].dpi = (wp.0 & 0xFFFF) as u32;
                if h.fences[i].interaction.is_none() { h.place_all(); } else { h.render(id); }
            });
            return LRESULT(0);
        }
        WM_FOLDER_CHANGED => {
            let change = read_change(wp, lp);
            with_host(|h| h.invalidate_changed_icons(change.first.as_deref(), change.second.as_deref()));
            unsafe { SetTimer(Some(hwnd), TIMER_FOLDER, 250, None); }
            return LRESULT(0);
        }
        WM_TIMER if wp.0 == TIMER_PEEK => { with_host(|h| h.peek_check(id)); return LRESULT(0); }
        WM_TIMER if wp.0 == TIMER_FOLDER => {
            let _ = unsafe { KillTimer(Some(hwnd), TIMER_FOLDER) };
            if with_host(|h| h.refresh_folder(id)).is_none() { unsafe { SetTimer(Some(hwnd), TIMER_FOLDER, 500, None); } }
            return LRESULT(0);
        }
        WM_RENAME_DONE => {
            if let Some(a) = with_host(|h| h.finish_rename(id, wp.0 != 0)) { run_after(a); }
            return LRESULT(0);
        }
        WM_MOUSEACTIVATE => return LRESULT(MA_ACTIVATE as isize),
        _ => {}
    }
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

// ---- modal work -----------------------------------------------------------------

fn key_down(key: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY) -> bool { (unsafe { GetKeyState(key.0 as i32) }) < 0 }

fn fence_hwnd(fence: u64) -> Option<HWND> { with_host(|h| h.win_index(fence).map(|i| h.fences[i].hwnd)).flatten() }

fn refresh_fence(fence: u64) {
    with_host(|h| if h.config.fence(fence).is_some_and(|f| f.is_desktop()) { h.refresh_desktop(); } else { h.refresh_folder(fence); });
}

fn run_after(after: After) {
    match after {
        After::Nothing => {}
        After::ItemMenu { fence, point } => item_menu(fence, point),
        After::FenceMenu { fence, point } => fence_menu(fence, point),
        After::TrayMenu(point) => tray_menu(point),
        After::Drag { fence } => drag(fence),
        After::Open { fence, index, explorer } => open(fence, index, explorer),
        After::Verb { fence, verb, background } => run_verb(fence, verb, background),
        After::History { fence, action } => run_history(fence, action),
        After::Rename { fence, key, name } => {
            let target = with_host(|h| h.win_index(fence).and_then(|i| h.fences[i].entries.iter().find(|e| e.key == key).map(|e| (h.fences[i].hwnd, e.pidl.clone())))).flatten();
            if let Some((hwnd, pidl)) = target { let _ = desktop::rename(hwnd, &pidl, &name); }
            refresh_fence(fence);
        }
        After::PickFolder { replace } => {
            let owner = with_host(|h| h.hwnd).unwrap_or_default();
            if let Some(path) = desktop::pick_folder(owner, "选择要映射到桌面格子的文件夹") { with_host(|h| h.add_folder_fence(path, replace)); }
        }
        After::DeleteFence(id) => {
            let Some((hwnd, title)) = with_host(|h| h.config.fence(id).map(|f| (h.hwnd, f.title.clone()))).flatten() else { return };
            let text = wide(&format!("删除格子“{title}”？\n\n格子中的文件和文件夹不会被删除。"));
            let answer = unsafe { MessageBoxW(Some(hwnd), PCWSTR(text.as_ptr()), w!("ZSDesk 桌面格子"), MB_OKCANCEL | MB_ICONQUESTION) };
            if answer == IDOK { with_host(|h| h.delete_fence(id)); }
        }
    }
}

fn fast_context_menu_enabled() -> bool {
    with_host(|h| h.config.settings.zsui_fast_context_menu).unwrap_or(false)
}
fn add_shell_menu_entry(spec: &mut MenuSpec) {
    spec.items.insert(0, entry("完整 Windows Shell 菜单…", "windows-shell-menu", true, false));
    spec.items.insert(1, MenuItemSpec::Separator);
}

fn item_menu(fence: u64, point: POINT) {
    if fast_context_menu_enabled() {
        fast_item_menu(fence, point);
    } else {
        item_menu_native(fence, point);
    }
}

fn fast_item_menu(fence: u64, point: POINT) {
    let _layer = MenuLayerRepair;
    let Some((hwnd, items)) = with_host(|h| {
        h.win_index(fence).map(|i| (h.fences[i].hwnd, h.item_menu_items(fence)))
    }).flatten() else { return };
    let mut spec = MenuSpec { id: None, title: None, items };
    add_shell_menu_entry(&mut spec);
    if let Some(id) = popup(hwnd, &spec, point) {
        if id == "windows-shell-menu" {
            item_menu_shell_native(fence, point);
        } else if let Some(after) = with_host(|h| h.menu_command(Some(fence), &id, point)) {
            run_after(after);
        }
    }
}

fn item_menu_native(fence: u64, point: POINT) {
    item_menu_with_shell(fence, point, true);
}

fn item_menu_shell_native(fence: u64, point: POINT) {
    item_menu_with_shell(fence, point, false);
}

fn item_menu_with_shell(fence: u64, point: POINT, include_app_items: bool) {
    let _layer = MenuLayerRepair;
    let Some((hwnd, pidls, extra)) = with_host(|h| h.win_index(fence).map(|i| {
        (h.fences[i].hwnd, h.selected_pidls(fence), if include_app_items { h.item_menu_items(fence) } else { Vec::new() })
    })).flatten() else { return };
    if pidls.is_empty() { return fence_menu(fence, point); }
    apply_context_preferences();
    let menu = match ShellMenu::for_items(hwnd, &pidls) {
        Ok(menu) => menu,
        Err(error) => { zsdesk_windows::shell::menu::report_failure(hwnd, error.code()); return; }
    };
    let shift = key_down(VK_SHIFT);
    if let Err(error) = menu.populate(true, shift) { zsdesk_windows::shell::menu::report_failure(hwnd, error.code()); return; }
    let ids = insert_top(menu.hmenu(), &extra);
    let _ = unsafe { SetForegroundWindow(hwnd) };
    request_layer_repair();
    let Some(chosen) = menu.track_selected(hwnd, point, &pidls) else { refresh_fence(fence); return };
    if chosen >= APP_FIRST {
        if let Some(id) = ids.get((chosen - APP_FIRST) as usize) {
            if let Some(a) = with_host(|h| h.menu_command(Some(fence), id, point)) { run_after(a); }
        }
        return;
    }
    let verb = menu.verb(chosen).unwrap_or_default().to_ascii_lowercase();
    match verb.as_str() {
        "rename" => {
            let key = with_host(|h| h.focused_entry(fence).map(|e| e.key.clone())).flatten();
            if let Some(key) = key { with_host(|h| h.begin_rename(fence, fence::RenameTarget::Item(key))); }
        }
        "open" | "explore" if pidls.len() == 1 && with_host(|h| h.focused_entry(fence).is_some_and(|e| e.is_container() && !e.is_link)).unwrap_or(false) => {
            let index = with_host(|h| h.win_index(fence).and_then(|i| h.fences[i].focus)).flatten().unwrap_or(0);
            open(fence, index, verb == "explore");
        }
        _ => {
            let _ = menu.invoke(hwnd, chosen, Some(point), shift, key_down(VK_CONTROL));
            refresh_fence(fence);
        }
    }
}

fn fence_menu(fence: u64, point: POINT) {
    if fast_context_menu_enabled() {
        fast_fence_menu(fence, point);
    } else {
        fence_menu_native(fence, point);
    }
}

fn fast_fence_menu(fence: u64, point: POINT) {
    let _layer = MenuLayerRepair;
    let Some((hwnd, mut spec, has_folder)) = with_host(|h| {
        h.win_index(fence).map(|i| {
            (h.fences[i].hwnd, h.fence_menu(fence), h.target_folder(fence).is_some())
        })
    }).flatten() else { return };
    if has_folder {
        add_shell_menu_entry(&mut spec);
    }
    if let Some(id) = popup(hwnd, &spec, point) {
        if id == "windows-shell-menu" {
            fence_menu_native(fence, point);
        } else if let Some(after) = with_host(|h| h.menu_command(Some(fence), &id, point)) {
            run_after(after);
        }
    }
}

fn fence_menu_native(fence: u64, point: POINT) {
    let _layer = MenuLayerRepair;
    apply_context_preferences();
    let Some((hwnd, spec, folder)) = with_host(|h| h.win_index(fence).map(|i| (h.fences[i].hwnd, h.fence_menu(fence), h.target_folder(fence)))).flatten() else { return };
    let shell = folder.as_ref().and_then(|f| desktop::folder_for_path(f).ok()).and_then(|f| ShellMenu::for_folder_background(hwnd, &f).ok())
        .filter(|m| m.populate(false, key_down(VK_SHIFT)).is_ok());
    let _ = unsafe { SetForegroundWindow(hwnd) };
    request_layer_repair();
    match shell {
        Some(menu) => {
            let ids = insert_top(menu.hmenu(), &spec.items);
            let chosen = folder.as_ref().and_then(|folder| menu.track_folder_background(hwnd, point, folder));
            let Some(chosen) = chosen else { refresh_fence(fence); return };
            if chosen >= APP_FIRST {
                if let Some(id) = ids.get((chosen - APP_FIRST) as usize) {
                    if let Some(a) = with_host(|h| h.menu_command(Some(fence), id, point)) { run_after(a); }
                }
                return;
            }
            // "新建" / "粘贴" create items in the backing folder; desktop fences claim them.
            with_host(|h| h.expect_new_items(fence, Vec::new()));
            let _ = menu.invoke(hwnd, chosen, Some(point), key_down(VK_SHIFT), false);
            refresh_fence(fence);
        }
        None => {
            if let Some(id) = popup(hwnd, &spec, point) {
                if let Some(a) = with_host(|h| h.menu_command(Some(fence), &id, point)) { run_after(a); }
            }
        }
    }
}

fn tray_menu(point: POINT) {
    apply_context_preferences();
    let Some((hwnd, spec)) = with_host(|h| (h.hwnd, h.tray_menu())) else { return };
    let _ = unsafe { SetForegroundWindow(hwnd) };
    if let Some(id) = popup(hwnd, &spec, point) {
        if let Some(a) = with_host(|h| h.menu_command(None, &id, point)) { run_after(a); }
    }
    let _ = unsafe { PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0)) };
}

fn apply_context_preferences() {
    if let Some(profile) = with_host(|h| h.explorer_config.clone()) {
        let _ = crate::context_preferences::apply_from_profile(&profile);
    }
}

fn drag(fence: u64) {
    let Some((hwnd, pidls, keys)) = with_host(|h| h.win_index(fence).map(|i| (h.fences[i].hwnd, h.selected_pidls(fence), h.selected_keys(fence)))).flatten() else { return };
    if pidls.is_empty() { return; }
    let Ok(data) = desktop::data_object(hwnd, &pidls) else { return };
    with_host(|h| h.internal_drag = Some((fence, keys)));
    let _ = unsafe { SHDoDragDrop(Some(hwnd), &data, None, DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK) };
    with_host(|h| h.internal_drag = None);
    refresh_fence(fence);
}

fn open(fence: u64, index: usize, explorer: bool) {
    let Some((hwnd, entry, is_desktop, inside)) = with_host(|h| {
        let i = h.win_index(fence)?;
        let entry = h.fences[i].entries.get(index)?.clone();
        Some((h.fences[i].hwnd, entry, h.config.fence(fence)?.is_desktop(), h.config.settings.browse_inside))
    }).flatten() else { return };
    if entry.is_container() && !entry.is_link {
        if !is_desktop && inside && !explorer {
            if let Some(path) = entry.path.clone() { with_host(|h| h.browse_into(fence, path)); return; }
        }
        // ZSDesk hosts file-system folders and the main Shell folders itself.
        let hostable = entry.path.is_some() || ["{20D04FE0-3AEA-1069-A2D8-08002B30309D}", "{645FF040-5081-101B-9F08-00AA002F954E}", "{F02C1A0D-BE21-4350-88B0-7367FC96EF3C}"]
            .iter().any(|c| entry.key.to_ascii_uppercase().contains(c));
        if hostable { with_host(|h| h.open_entry_in_explorer(&entry)); return; }
    }
    let _ = desktop::open(hwnd, &entry.pidl, None);
}

fn run_history(fence: u64, action: HistoryAction) {
    let Some(hwnd) = fence_hwnd(fence) else { return };
    // A private, unwindowed Shell view supplies background history commands.
    // Do not borrow the user's desktop selection or create another GUI host.
    let result = (|| -> windows::core::Result<()> {
        let folder = unsafe { SHGetDesktopFolder()? };
        let view: IShellView = unsafe { folder.CreateViewObject(hwnd)? };
        let menu = ShellMenu::for_view(&view, false)?;
        menu.invoke_history(hwnd, action)
    })();
    if let Err(error) = result {
        let verb = if action == HistoryAction::Undo { "撤销" } else { "重做" };
        let text = wide(&format!("无法{verb}文件操作：{error}"));
        unsafe { MessageBoxW(Some(hwnd), PCWSTR(text.as_ptr()), w!("ZSDesk 桌面格子"), MB_OK | MB_ICONINFORMATION); }
    }
    refresh_fence(fence);
}

fn run_verb(fence: u64, verb: &'static str, background: bool) {
    let Some(hwnd) = fence_hwnd(fence) else { return };
    let shift = key_down(VK_SHIFT);
    if background {
        let Some(folder) = with_host(|h| h.target_folder(fence)).flatten() else { return };
        let Ok(folder) = desktop::folder_for_path(&folder) else { return };
        let Ok(menu) = ShellMenu::for_folder_background(hwnd, &folder) else { return };
        if menu.populate_for_invoke(false).is_ok() {
            with_host(|h| h.expect_new_items(fence, Vec::new()));
            let _ = menu.invoke_verb(hwnd, verb, false);
        }
    } else {
        let pidls = with_host(|h| h.selected_pidls(fence)).unwrap_or_default();
        if pidls.is_empty() { return; }
        let Ok(menu) = ShellMenu::for_items(hwnd, &pidls) else { return };
        if menu.populate_for_invoke(shift).is_ok() { let _ = menu.invoke_verb(hwnd, verb, shift); }
    }
    refresh_fence(fence);
}

/// Inserts application items (ZSUI spec) at the top of a Shell menu.
fn insert_top(hmenu: HMENU, items: &[MenuItemSpec]) -> Vec<String> {
    let mut ids = Vec::new();
    let mut position = 0u32;
    for item in items { insert_item(hmenu, &mut position, item, &mut ids); }
    if !items.is_empty() { unsafe { let _ = InsertMenuW(hmenu, position, MF_BYPOSITION | MF_SEPARATOR, 0, None); } }
    ids
}

fn insert_item(hmenu: HMENU, position: &mut u32, item: &MenuItemSpec, ids: &mut Vec<String>) {
    match item {
        MenuItemSpec::Command { label, command: UiCommand::Custom { id, .. }, checked, enabled, .. } => {
            let command = APP_FIRST as usize + ids.len();
            ids.push(id.clone());
            let flags = MF_BYPOSITION | MF_STRING | if *checked { MF_CHECKED } else { MF_UNCHECKED } | if *enabled { MF_ENABLED } else { MF_GRAYED };
            unsafe { let _ = InsertMenuW(hmenu, *position, flags, command, PCWSTR(wide(label).as_ptr())); }
            *position += 1;
        }
        MenuItemSpec::Submenu { label, menu, enabled, .. } => {
            let Ok(sub) = (unsafe { CreatePopupMenu() }) else { return };
            let mut inner = 0u32;
            for child in &menu.items { insert_item(sub, &mut inner, child, ids); }
            let flags = MF_BYPOSITION | MF_POPUP | if *enabled { MF_ENABLED } else { MF_GRAYED };
            unsafe { let _ = InsertMenuW(hmenu, *position, flags, sub.0 as usize, PCWSTR(wide(label).as_ptr())); }
            *position += 1;
        }
        MenuItemSpec::Separator => {
            unsafe { let _ = InsertMenuW(hmenu, *position, MF_BYPOSITION | MF_SEPARATOR, 0, None); }
            *position += 1;
        }
        _ => {}
    }
}

/// Plain popup for a ZSUI menu spec.
fn popup(owner: HWND, spec: &MenuSpec, point: POINT) -> Option<String> {
    let menu = unsafe { CreatePopupMenu() }.ok()?;
    let ids = { let mut ids = Vec::new(); let mut p = 0; for item in &spec.items { insert_item(menu, &mut p, item, &mut ids); } ids };
    let _ = unsafe { SetForegroundWindow(owner) };
    let chosen = unsafe { TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, point.x, point.y, None, owner, None) }.0 as u32;
    let _ = unsafe { PostMessageW(Some(owner), WM_NULL, WPARAM(0), LPARAM(0)) };
    let _ = unsafe { DestroyMenu(menu) };
    chosen.checked_sub(APP_FIRST).and_then(|i| ids.get(i as usize)).cloned()
}

// ---- drag and drop target ---------------------------------------------------------

pub struct FenceDrop { pub fence: u64 }

impl DropHandler for FenceDrop {
    fn enter(&self, data: &IDataObject, keys: u32, point: POINT, allowed: DROPEFFECT) -> DROPEFFECT {
        with_host(|h| h.drag_enter(self.fence, data, keys, point, allowed)).unwrap_or(DROPEFFECT_NONE)
    }
    fn over(&self, keys: u32, point: POINT, allowed: DROPEFFECT) -> DROPEFFECT {
        with_host(|h| h.drag_over(self.fence, keys, point, allowed)).unwrap_or(DROPEFFECT_NONE)
    }
    fn leave(&self) { with_host(|h| h.drag_leave(self.fence)); }
    fn dropped(&self, data: &IDataObject, keys: u32, point: POINT, allowed: DROPEFFECT) -> DROPEFFECT {
        let target = with_host(|h| h.take_drop(self.fence, data, point)).flatten();
        let mut effect = allowed;
        match target {
            // The Shell target runs the copy/move (with its progress UI) outside the host borrow.
            Some(target) => if unsafe { target.Drop(data, MODIFIERKEYS_FLAGS(keys), POINTL { x: point.x, y: point.y }, &mut effect) }.is_err() { effect = DROPEFFECT_NONE; },
            None => effect = DROPEFFECT_NONE,
        }
        refresh_fence(self.fence);
        effect
    }
}

// ---- desktop double-click hook ------------------------------------------------------

pub fn update_hook(enabled: bool) {
    LAST_CLICK.with(|click| click.set(None));
    let current = HOOK.with(Cell::get);
    if enabled && current == 0 {
        let instance = unsafe { GetModuleHandleW(None) }.ok().map(|m| HINSTANCE(m.0));
        if let Ok(hook) = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), instance, 0) } { HOOK.with(|h| h.set(hook.0 as isize)); }
    } else if !enabled && current != 0 {
        let _ = unsafe { UnhookWindowsHookEx(HHOOK(current as *mut _)) };
        HOOK.with(|h| h.set(0));
    }
}

// This fast path uses only window-manager metadata. Shell accessibility work
// remains in the asynchronously posted host message.
fn desktop_click_root(point: POINT) -> Option<HWND> {
    let window = unsafe { WindowFromPoint(point) };
    if window.0.is_null() { return None; }
    let root = unsafe { GetAncestor(window, GA_ROOT) };
    matches!(class_name(root).as_str(), "WorkerW" | "Progman").then_some(root)
}

fn next_desktop_click(previous: Option<DesktopClick>, current: Option<DesktopClick>, limit: u32, dx: i32, dy: i32) -> (Option<DesktopClick>, bool) {
    let Some(current) = current.filter(|click| click.root != 0) else { return (None, false) };
    let double = previous.is_some_and(|previous| previous.root == current.root
        && current.time.wrapping_sub(previous.time) <= limit
        && (i64::from(current.x) - i64::from(previous.x)).abs() <= i64::from(dx.max(0))
        && (i64::from(current.y) - i64::from(previous.y)).abs() <= i64::from(dy.max(0)));
    (if double { None } else { Some(current) }, double)
}

fn pack_screen_point(point: POINT) -> isize { ((u64::from(point.y as u32) << 32) | u64::from(point.x as u32)) as isize }
fn unpack_screen_point(value: isize) -> POINT { POINT { x: value as u32 as i32, y: ((value as u64) >> 32) as u32 as i32 } }

unsafe extern "system" fn mouse_hook(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code >= 0 && wp.0 as u32 == WM_LBUTTONDOWN && lp.0 != 0 {
        let info = unsafe { &*(lp.0 as *const MSLLHOOKSTRUCT) };
        let limit = unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime() };
        let (dx, dy) = (unsafe { GetSystemMetrics(SM_CXDOUBLECLK) }, unsafe { GetSystemMetrics(SM_CYDOUBLECLK) });
        let current = desktop_click_root(info.pt).map(|root| DesktopClick { root: root.0 as isize, time: info.time, x: info.pt.x, y: info.pt.y });
        let (next, double) = next_desktop_click(LAST_CLICK.with(Cell::get), current, limit, dx, dy);
        LAST_CLICK.with(|click| click.set(next));
        if double {
            let host = HOST_HWND.with(Cell::get);
            if let Some(current) = current {
                let _ = unsafe { PostMessageW(Some(HWND(host as *mut _)), WM_DESKTOP_DBLCLICK, WPARAM(current.root as usize), LPARAM(pack_screen_point(info.pt))) };
            }
        }
    } else if code >= 0 && matches!(wp.0 as u32, WM_RBUTTONDOWN | WM_RBUTTONUP | WM_MBUTTONDOWN | WM_XBUTTONDOWN | WM_MOUSEWHEEL | WM_MOUSEHWHEEL) {
        LAST_CLICK.with(|click| click.set(None));
    }
    unsafe { CallNextHookEx(None, code, wp, lp) }
}

// ---- rename edit -------------------------------------------------------------------

pub(super) fn prepare_rename_edit(edit: HWND, owner: HWND, dpi: u32) {
    let mut metrics = NONCLIENTMETRICSW { cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32, ..Default::default() };
    let _ = unsafe { SystemParametersInfoForDpi(SPI_GETNONCLIENTMETRICS.0, metrics.cbSize, Some((&mut metrics as *mut NONCLIENTMETRICSW).cast()), 0, dpi) };
    let font = unsafe { CreateFontIndirectW(&metrics.lfMessageFont) };
    unsafe {
        SendMessageW(edit, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
        let _ = SetWindowSubclass(edit, Some(rename_proc), owner.0 as usize, font.0 as usize);
    }
}

unsafe extern "system" fn rename_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, owner: usize, font: usize) -> LRESULT {
    let owner = HWND(owner as *mut _);
    match msg {
        WM_KEYDOWN if wp.0 == windows::Win32::UI::Input::KeyboardAndMouse::VK_A.0 as usize
            && unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0
            && unsafe { GetKeyState(windows::Win32::UI::Input::KeyboardAndMouse::VK_MENU.0 as i32) } >= 0 => {
            unsafe { SendMessageW(hwnd, windows::Win32::UI::Controls::EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1))); }
            return LRESULT(0);
        }
        WM_KEYDOWN if wp.0 == VK_RETURN.0 as usize => { let _ = unsafe { PostMessageW(Some(owner), WM_RENAME_DONE, WPARAM(1), LPARAM(0)) }; return LRESULT(0); }
        WM_KEYDOWN if wp.0 == VK_ESCAPE.0 as usize => { let _ = unsafe { PostMessageW(Some(owner), WM_RENAME_DONE, WPARAM(0), LPARAM(0)) }; return LRESULT(0); }
        WM_CHAR if wp.0 == 0x01 || wp.0 == 0x0D || wp.0 == 0x1B => return LRESULT(0),
        WM_KILLFOCUS => { let _ = unsafe { PostMessageW(Some(owner), WM_RENAME_DONE, WPARAM(1), LPARAM(0)) }; }
        WM_NCDESTROY => unsafe {
            let _ = RemoveWindowSubclass(hwnd, Some(rename_proc), owner.0 as usize);
            let _ = DeleteObject(HFONT(font as *mut _).into());
        },
        _ => {}
    }
    unsafe { DefSubclassProc(hwnd, msg, wp, lp) }
}

pub(super) fn post_rename(host: HWND, fence: u64, key: String, name: String) {
    let payload = Box::into_raw(Box::new((fence, key, name)));
    if unsafe { PostMessageW(Some(host), WM_RENAME_REQUEST, WPARAM(0), LPARAM(payload as isize)) }.is_err() { unsafe { drop(Box::from_raw(payload)); } }
}

pub(super) fn window_text(hwnd: HWND) -> String {
    let length = unsafe { GetWindowTextLengthW(hwnd) }.max(0) as usize;
    let mut buffer = vec![0u16; length + 1];
    let copied = unsafe { GetWindowTextW(hwnd, &mut buffer) }.max(0) as usize;
    String::from_utf16_lossy(&buffer[..copied])
}

fn wide(text: &str) -> Vec<u16> { text.encode_utf16().chain(Some(0)).collect() }

fn class_name(hwnd: HWND) -> String {
    let mut buffer = [0u16; 64];
    let len = unsafe { GetClassNameW(hwnd, &mut buffer) }.max(0) as usize;
    String::from_utf16_lossy(&buffer[..len])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn desktop_double_click_requires_two_clicks_on_the_same_desktop_root() {
        let first = DesktopClick { root: 1, time: 100, x: 500, y: 300 };
        let second = DesktopClick { time: 200, ..first };
        let (pending, double) = next_desktop_click(None, Some(first), 500, 4, 4);
        assert!(!double);
        assert_eq!(next_desktop_click(pending, Some(second), 500, 4, 4), (None, true));
        let other = DesktopClick { root: 2, ..second };
        assert!(!next_desktop_click(pending, Some(other), 500, 4, 4).1);
        // A fence/application click or an intervening right button clears the
        // candidate, even when the following desktop click is at the same point.
        let cleared = next_desktop_click(pending, None, 500, 4, 4).0;
        assert!(!next_desktop_click(cleared, Some(second), 500, 4, 4).1);
    }
    #[test]
    fn desktop_double_click_time_wrap_and_screen_coordinates_are_bounded() {
        let first = DesktopClick { root: 1, time: u32::MAX - 50, x: -40000, y: 50000 };
        let next = DesktopClick { time: 25, ..first };
        assert!(next_desktop_click(Some(first), Some(next), 500, 4, 4).1);
        assert!(!next_desktop_click(Some(first), Some(DesktopClick { x: 40000, ..next }), 500, 4, 4).1);
        let point = unpack_screen_point(pack_screen_point(POINT { x: -40000, y: 50000 }));
        assert_eq!((point.x, point.y), (-40000, 50000));
    }
    #[test]
    fn commands_round_trip_between_processes() {
        for command in [DesktopCommand::Show, DesktopCommand::Organize, DesktopCommand::Map(r"D:\资料 2026".into()), DesktopCommand::Quit] {
            let bytes = serde_json::to_vec(&command).unwrap();
            assert_eq!(serde_json::from_slice::<DesktopCommand>(&bytes).unwrap(), command);
        }
    }
    #[test]
    fn instance_keys_ignore_case() {
        assert_eq!(instance_key(std::path::Path::new(r"C:\A\b.json")), instance_key(std::path::Path::new(r"c:\a\B.json")));
    }
    #[test]
    fn fast_menu_keeps_plugin_actions_and_adds_full_shell_entry_first() {
        let mut spec = MenuSpec::new();
        spec.items.push(entry("Run plugin", "plugin.demo.run", true, false));
        add_shell_menu_entry(&mut spec);
        assert!(matches!(
            &spec.items[0],
            MenuItemSpec::Command { command: UiCommand::Custom { id, .. }, .. } if id == "windows-shell-menu"
        ));
        assert!(matches!(spec.items[1], MenuItemSpec::Separator));
        assert!(matches!(
            &spec.items[2],
            MenuItemSpec::Command { command: UiCommand::Custom { id, .. }, .. } if id == "plugin.demo.run"
        ));
    }
}
