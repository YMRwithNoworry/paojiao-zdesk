//! Desktop fences (桌面格子).
//!
//! Runs as a single-instance desktop component for each profile with a
//! tray icon. Fences are top-level layered windows owned by `Progman`, so they
//! stay on the desktop layer and survive "显示桌面". Organize mode hides
//! Explorer's desktop icons through the Shell view only after a watchdog
//! process (`--desktop-guard`) confirmed it will restore them if the host
//! process or its desktop thread exits unexpectedly.

mod fence;
mod host;
mod menu_worker;
mod return_to_desktop;
#[cfg(test)]mod window_message_regression;
#[cfg(test)]mod context_menu_regression;

/// Reads the existing desktop entry without starting a desktop host.
pub fn startup_enabled(config: &std::path::Path) -> anyhow::Result<bool> {
    let executable = std::env::current_exe()?;
    let expected = host::fence_startup_command(&executable, config)
        .ok_or_else(|| anyhow::anyhow!("无法确定桌面格子的程序和配置目录"))?;
    Ok(zsdesk_windows::desktop::autostart_command(crate::edition::desktop_startup_value())?
        .as_deref().is_some_and(|current| zsdesk_windows::desktop::autostart_owned(current, &expected)))
}

/// Applies the same ownership checks as the desktop tray's login switch.
pub fn set_startup_enabled(config: &std::path::Path, enabled: bool) -> anyhow::Result<()> {
    anyhow::ensure!(crate::edition::DESKTOP, "当前版本不包含桌面格子");
    let executable = std::env::current_exe()?;
    let expected = host::fence_startup_command(&executable, config)
        .ok_or_else(|| anyhow::anyhow!("无法确定桌面格子的程序和配置目录"))?;
    zsdesk_windows::desktop::set_autostart(crate::edition::desktop_startup_value(), &expected, enabled)?;
    Ok(())
}

/// Removes only this installation's exact desktop logon command.
pub fn uninstall_owned_startup(config: &std::path::Path, executable: &std::path::Path) -> anyhow::Result<()> {
    let Some(expected) = host::fence_startup_command(executable, config) else { return Ok(()); };
    let value = crate::edition::desktop_startup_value();
    if zsdesk_windows::desktop::autostart_command(value)?.as_deref()
        .is_some_and(|current| zsdesk_windows::desktop::autostart_owned(current, &expected)) {
        zsdesk_windows::desktop::set_autostart(value, &expected, false)?;
    }
    Ok(())
}
pub(crate) mod migration;
mod tencent_import;

/// Shell extension calls still retiring after the bounded desktop shutdown.
pub(crate) fn pending_menu_threads()->usize{menu_worker::retiring_count()+return_to_desktop::pending()}

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
            Threading::{CreateMutexW, OpenEventW, OpenProcess, SetEvent, EVENT_MODIFY_STATE, INFINITE, PROCESS_SYNCHRONIZE},
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
use zsdesk_windows::{desktop::{self, DesktopHit}, gfx::Gfx, ole::DropHandler, shell::{menu::{forward_menu_message, HistoryAction, MenuInterceptions, MenuOutcome, ShellMenu, APP_FIRST}, notify::read_change}};

use host::{After, DesktopHost, TIMER_AUTOSCROLL, TIMER_DESKTOP, TIMER_FOLDER, TIMER_PEEK, TIMER_SAVE, TIMER_ICONS, WM_DESKTOP_CHANGED, WM_DESKTOP_DBLCLICK, WM_FOLDER_CHANGED,
    WM_ICON_READY, WM_RENAME_DONE, WM_TRAY};

const HOST_CLASS: &str = "ZSDesk.Native.DesktopHost";
const DESKTOP_MAGIC: usize = 0x5A53_4443;
const WM_COMMAND_REQUEST: u32 = WM_APP + 20;
const WM_RENAME_REQUEST: u32 = WM_APP + 21;
const WM_FOREGROUND: u32 = WM_APP + 22;
const WM_REPAIR_LAYER: u32 = WM_APP + 23;
const TIMER_RAISE: usize = 9;
const TIMER_REPAIR_LAYER: usize = 10;
const TIMER_FOREGROUND: usize = 11;
const TIMER_CANCEL_INTERACTION:usize=6;

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
    static LAYER_EVENTS: RefCell<std::collections::VecDeque<(u64, HWND, u32, u32)>> = const { RefCell::new(std::collections::VecDeque::new()) };
    static NEXT_LAYER_EVENT: Cell<u64> = const { Cell::new(1) };
    static MENU_WORKER: RefCell<Option<menu_worker::Worker>> = const { RefCell::new(None) };
    static DEFERRED_QUIT: Cell<bool> = const { Cell::new(false) };
}

fn queue_foreground(host: HWND, window: HWND) {
    if !unsafe { IsWindow(Some(window)) }.as_bool() {request_layer_repair();return;}
    let mut process = 0;
    let thread = unsafe { GetWindowThreadProcessId(window, Some(&mut process)) };
    let sequence = NEXT_LAYER_EVENT.with(|next| { let value=next.get(); next.set(value.wrapping_add(1)); value });
    LAYER_EVENTS.with(|events| {
        let mut events=events.borrow_mut();
        if events.back().is_some_and(|event| event.1==window && event.2==process && event.3==thread) { return; }
        if events.len()==32 { events.pop_front(); }
        events.push_back((sequence,window,process,thread));
    });
    flush_foreground(host);
}

fn flush_foreground(host: HWND) {
    let events:Vec<_>=LAYER_EVENTS.with(|events| events.borrow().iter().copied().collect());
    if events.is_empty() { let _=unsafe { KillTimer(Some(host),TIMER_FOREGROUND) }; return; }
    if with_host(|desktop| {
        // Delayed WinEvents describe history. Reconcile the current foreground
        // once instead of replaying stale screenshot/menu HWNDs in order.
        desktop.repair_layer();
    }).is_some() {
        let last=events.last().unwrap().0;
        LAYER_EVENTS.with(|queue| queue.borrow_mut().retain(|event|event.0>last));
    }
    if LAYER_EVENTS.with(|events|!events.borrow().is_empty()) { unsafe { SetTimer(Some(host),TIMER_FOREGROUND,30,None); } }
    else { let _=unsafe { KillTimer(Some(host),TIMER_FOREGROUND) }; }
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
pub(super) fn schedule_desktop_raise(host:HWND){unsafe{SetTimer(Some(host),TIMER_RAISE,180,None);}}

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
    zsdesk_windows::flyout::prepare();
    let config = host::load_config(&config_path);
    let hook = config.settings.double_click_toggle;
    HOST.with(|h| *h.borrow_mut() = Some(DesktopHost::new(hwnd, config_path, explorer_config, config, gfx)));
    with_host(|h| h.start());
    update_hook(hook);
    let foreground = unsafe { windows::Win32::UI::Accessibility::SetWinEventHook(EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND, None,
        Some(foreground_hook), 0, 0, WINEVENT_OUTOFCONTEXT) };
    execute(command);
    let mut msg = MSG::default();
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
        unsafe { let _ = TranslateMessage(&msg); DispatchMessageW(&msg); }
    }
    update_hook(false);
    if !foreground.is_invalid() { let _ = unsafe { windows::Win32::UI::Accessibility::UnhookWinEvent(foreground) }; }
    if let Some(mut host) = HOST.with(|h| h.borrow_mut().take()) { host.shutdown(); }
    let menu = MENU_WORKER.with(|worker| worker.borrow_mut().take());
    drop(menu);
    Ok(())
}

fn execute(command: DesktopCommand) {
    // DestroyWindow dispatches WM_DESTROY synchronously. End the HOST borrow
    // before calling it so shutdown can actually borrow and clean up the host.
    if command == DesktopCommand::Quit {
        if return_to_desktop::pending()!=0 { DEFERRED_QUIT.with(|pending|pending.set(true)); return; }
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

fn finish_deferred_quit() {
    if return_to_desktop::pending()==0 && DEFERRED_QUIT.with(|pending|pending.replace(false)) { execute(DesktopCommand::Quit); }
}

/// Watchdog: restores desktop icons if the host process exits while they are hidden.
pub fn guard(config_path: PathBuf, pid: u32, desktop_thread: Option<u32>) -> Result<()> {
    use windows::Win32::System::Threading::{GetProcessIdOfThread,OpenThread,WaitForMultipleObjects,
        THREAD_QUERY_LIMITED_INFORMATION,THREAD_SYNCHRONIZE};
    struct Owned(HANDLE);
    impl Drop for Owned { fn drop(&mut self) { let _=unsafe{CloseHandle(self.0)}; } }
    let process = Owned(unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid)? });
    let thread = if let Some(id)=desktop_thread {
        let handle=Owned(unsafe{OpenThread(THREAD_SYNCHRONIZE|THREAD_QUERY_LIMITED_INFORMATION,false,id)}?);
        anyhow::ensure!(unsafe{GetProcessIdOfThread(handle.0)}==pid,"桌面监护线程不属于目标进程");
        Some(handle)
    } else { None };
    let mut handles=vec![process.0];
    if let Some(thread)=&thread { handles.push(thread.0); }
    let name = wide(&format!("Local\\ZSDesk.DesktopGuard.{pid}"));
    let ready=Owned(unsafe { OpenEventW(EVENT_MODIFY_STATE, false, PCWSTR(name.as_ptr())) }?);
    // The parent may hide system icons only after both lifetime handles are
    // open and the desktop thread's ownership has been checked.
    unsafe { SetEvent(ready.0)?; }
    drop(ready);
    let waited=unsafe { WaitForMultipleObjects(&handles, false, INFINITE) }.0;
    anyhow::ensure!((waited as usize)<handles.len(),"桌面监护等待失败：{waited:#x}");
    drop(thread);drop(process);
    let mut config = host::load_config(&config_path);
    if config.settings.icons_hidden_by_zsdesk {
        unsafe { OleInitialize(None)?; }
        let restored=desktop::set_icons_visible(true);
        unsafe { OleUninitialize(); }
        if restored {
            config.settings.icons_hidden_by_zsdesk = false;
            host::save_config(&config_path, &config)?;
        } else {
            anyhow::bail!("系统桌面图标恢复失败，恢复标记已保留");
        }
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
        WM_CLOSE => { execute(DesktopCommand::Quit); return LRESULT(0); }
        menu_worker::READY => { finish_menu_work(); return LRESULT(0); }
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
            let version4=with_host(|host|host.tray_version4).unwrap_or(false);
            if let Some(action)=tray_action(version4,wp,lp){dispatch_tray(action);}
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
        WM_TIMER if wp.0 == TIMER_FOREGROUND => { flush_foreground(hwnd); return LRESULT(0); }
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
            if with_host(|h| if h.accept_icon_notification(wp.0) { h.icons_ready(); }).is_none() { unsafe { SetTimer(Some(hwnd), TIMER_ICONS, 50, None); } }
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
            queue_foreground(hwnd,window);
            if class == "WorkerW" || class == "Progman" {
                // "显示桌面" may finish raising its WorkerW after the event: raise again shortly.
                unsafe { SetTimer(Some(hwnd), TIMER_RAISE, 180, None); }
            }
            return LRESULT(0);
        }
        WM_DISPLAYCHANGE => { with_host(|h| h.place_all()); return LRESULT(0); }
        WM_THEMECHANGED|WM_DWMCOMPOSITIONCHANGED|WM_SYSCOLORCHANGE => {
            with_host(|host|{host.update_palette();for window in &host.fences{fence::apply_glass(window.hwnd);}host.render_all();});return LRESULT(0);
        }
        WM_SETTINGCHANGE => {
            let area = if lp.0 != 0 { unsafe { PCWSTR(lp.0 as *const u16).to_string() }.unwrap_or_default() } else { String::new() };
            if wp.0 as u32 == SPI_SETWORKAREA.0 { with_host(|h| h.place_all()); }
            if area == "ImmersiveColorSet"||wp.0 as u32==SPI_SETHIGHCONTRAST.0 { with_host(|h| { h.update_palette();for window in &h.fences{fence::apply_glass(window.hwnd);}h.render_all(); }); }
            if area == "ShellState" { unsafe { SetTimer(Some(hwnd), TIMER_DESKTOP, 250, None); } }
            return LRESULT(0);
        }
        WM_QUERYENDSESSION => return LRESULT(1),
        WM_ENDSESSION => { if wp.0 != 0 { with_host(|h| h.shutdown()); } return LRESULT(0); }
        WM_DESTROY => {
            MENU_WORKER.with(|worker| { if let Some(worker)=worker.borrow().as_ref(){worker.stop();} });
            LAYER_EVENTS.with(|events| events.borrow_mut().clear());
            let _=unsafe { KillTimer(Some(hwnd),TIMER_FOREGROUND) };
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
    if msg==zsdesk_windows::shell::file_transfer::result_message(){file_operation_finished(id,hwnd,wp.0 as u32);return LRESULT(0);}
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
        WM_MOUSEWHEEL => { with_host(|h| h.wheel(id, ((wp.0 >> 16) as i16) as i32, point_of(lp))); return LRESULT(0); }
        WM_MOUSELEAVE => { with_host(|h| h.mouse_leave(id)); return LRESULT(0); }
        WM_CANCELMODE|WM_CAPTURECHANGED => {
            cancel_fence_interaction(hwnd,id);
            if msg==WM_CANCELMODE{return unsafe{DefWindowProcW(hwnd,msg,wp,lp)};}
            return LRESULT(0);
        }
        WM_TIMER if wp.0==TIMER_CANCEL_INTERACTION=>{cancel_fence_interaction(hwnd,id);return LRESULT(0);}
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
        WM_TIMER if wp.0 == TIMER_AUTOSCROLL => { with_host(|h| h.rubber_autoscroll(id)); return LRESULT(0); }
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
        // Resizes happen inside host calls, so the shape is kept here, outside the host borrow.
        WM_SIZE => fence::shape_window(hwnd),
        _ => {}
    }
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

// ---- modal work -----------------------------------------------------------------

fn key_down(key: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY) -> bool { (unsafe { GetKeyState(key.0 as i32) }) < 0 }

fn cancel_fence_interaction(hwnd:HWND,id:u64){
    if with_host(|host|host.capture_lost(id)).is_some(){let _=unsafe{KillTimer(Some(hwnd),TIMER_CANCEL_INTERACTION)};}
    else{unsafe{SetTimer(Some(hwnd),TIMER_CANCEL_INTERACTION,30,None);}}
    // WM_CAPTURECHANGED can name another window. Release only capture still
    // owned by this fence, after the mutable host borrow has ended.
    if unsafe{windows::Win32::UI::Input::KeyboardAndMouse::GetCapture()}==hwnd{let _=unsafe{windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture()};}
}

fn fence_hwnd(fence: u64) -> Option<HWND> { with_host(|h| h.win_index(fence).map(|i| h.fences[i].hwnd)).flatten() }

fn refresh_fence(fence: u64) {
    with_host(|h| if h.config.fence(fence).is_some_and(|f| f.is_desktop()) { h.refresh_desktop(); } else { h.refresh_folder(fence); });
}

fn file_operation_finished(fence:u64,owner:HWND,code:u32){
    use zsdesk_windows::shell::file_transfer;
    if code!=0{
        file_action_error(owner,&windows::core::Error::from_hresult(windows::core::HRESULT(code as i32)));
        refresh_fence(fence);return;
    }
    let created=file_transfer::take_created_folder(owner);
    if let Some(path)=&created{
        let name=path.file_name().map(|name|name.to_string_lossy().into_owned()).into_iter().collect();
        with_host(|host|host.expect_new_items(fence,name));
    }
    refresh_fence(fence);
    let Some(path)=created else{return;};
    with_host(|host|{
        if host.target_folder(fence).as_deref()!=path.parent(){return;}
        let Some(index)=host.win_index(fence)else{return;};
        let Some(item)=host.fences[index].entries.iter().position(|entry|entry.path.as_ref()==Some(&path))else{return;};
        let key=host.fences[index].entries[item].key.clone();
        if let Some(config)=host.config.fence(fence).cloned(){
            let window=&mut host.fences[index];window.selection.clear();window.selection.insert(key.clone());window.focus=Some(item);window.anchor=Some(item);
            let geometry=window.geometry(&config);
            if let Some(cell)=geometry.cells.get(item){
                if cell.cell.y<geometry.content.y{window.scroll-=(geometry.content.y-cell.cell.y).max(0);}
                else if cell.cell.bottom()>geometry.content.bottom(){window.scroll+=cell.cell.bottom()-geometry.content.bottom();}
                window.scroll=window.scroll.clamp(0,geometry.max_scroll);
            }
        }
        host.render(fence);
        if unsafe{IsWindowVisible(owner)}.as_bool()&&unsafe{GetForegroundWindow()}==owner{host.begin_rename(fence,fence::RenameTarget::Item(key));}
    });
}

fn file_action_error(owner: HWND, error: &windows::core::Error) {
    if matches!(error.code().0 as u32, 0x800704c7 | 0x80004004) || !unsafe { IsWindow(Some(owner)) }.as_bool() { return; }
    let text = wide(&format!("无法完成文件操作：{}（0x{:08X}）", error.message(), error.code().0 as u32));
    unsafe { MessageBoxW(Some(owner), PCWSTR(text.as_ptr()), w!("ZSDesk 桌面格子"), MB_OK | MB_ICONWARNING); }
}

fn dispatch_application_menu(fence: u64, owner: HWND, command: u32, ids: &[String], point: POINT) {
    if !unsafe { IsWindow(Some(owner)) }.as_bool() { return; }
    let Some(id) = command.checked_sub(APP_FIRST).and_then(|offset| ids.get(offset as usize)) else {
        file_action_error(owner, &windows::core::Error::new(windows::core::HRESULT(0x80070057u32 as i32), "菜单命令已失效"));
        return;
    };
    if let Some(after) = with_host(|host| host.menu_command(Some(fence), id, point)) { run_after(after); }
}

fn run_after(after: After) {
    match after {
        After::Nothing => {}
        After::ItemMenu { fence, point } => item_menu(fence, point),
        After::FenceMenu { fence, point } => fence_menu(fence, point),
        After::FolderSystemMenu { fence, point } => folder_system_menu(fence, point),
        After::TrayMenu(point) => tray_menu(point),
        After::Drag { fence } => drag(fence),
        After::ReturnToDesktop { fence, point } => { return_to_desktop::return_selection(fence, point); finish_deferred_quit(); },
        After::Open { fence, index, explorer } => open(fence, index, explorer),
        After::Verb { fence, verb, background } => run_verb(fence, verb, background),
        After::History { fence, action } => run_history(fence, action),
        After::Quit => execute(DesktopCommand::Quit),
        After::Notice(message) => {
            let owner = with_host(|host| host.hwnd).unwrap_or_default();
            let text = wide(message);
            unsafe { MessageBoxW(Some(owner), PCWSTR(text.as_ptr()), w!("ZSDesk 桌面格子"), MB_OK | MB_ICONINFORMATION); }
        }
        After::Rename { fence, key, name } => {
            let target = with_host(|h| h.win_index(fence).and_then(|i| h.fences[i].entries.iter().find(|e| e.key == key).map(|e| (h.fences[i].hwnd, e.pidl.clone())))).flatten();
            if let Some((hwnd, pidl)) = target {
                if let Err(error) = desktop::rename(hwnd, &pidl, &name) { file_action_error(hwnd, &error); }
            }
            refresh_fence(fence);
        }
        After::PickFolder { replace } => {
            let owner = with_host(|h| h.hwnd).unwrap_or_default();
            if let Some(path) = desktop::pick_folder(owner, "选择要映射到桌面格子的文件夹") { with_host(|h| h.add_folder_fence(path, replace)); }
        }
        After::PickFolderTab { to } => {
            let owner = with_host(|h| h.hwnd).unwrap_or_default();
            if let Some(path) = desktop::pick_folder(owner, "选择要映射为标签的文件夹") { with_host(|h| h.add_folder_tab(to, path)); }
        }
        After::AddTab { fence, point } => tab_popup(fence, point, |h| h.add_tab_menu()),
        After::TabList { fence, point } => tab_popup(fence, point, |h| h.tab_list_menu(fence)),
        After::DeleteFence(id) => {
            let Some((hwnd, title, tab)) = with_host(|h| h.config.fence(id).map(|f| (h.hwnd, f.title.clone(), f.group != 0))).flatten() else { return };
            let text = wide(&if tab { format!("关闭标签“{title}”？\n\n标签中的文件和文件夹不会被删除。") }
                else { format!("删除格子“{title}”？\n\n格子中的文件和文件夹不会被删除。") });
            let answer = unsafe { MessageBoxW(Some(hwnd), PCWSTR(text.as_ptr()), w!("ZSDesk 桌面格子"), MB_OKCANCEL | MB_ICONQUESTION) };
            if answer == IDOK { with_host(|h| h.delete_fence(id)); }
        }
    }
}

fn fast_context_menu_enabled() -> bool {
    with_host(|host| host.config.settings.zsui_fast_context_menu).unwrap_or(false)
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
    let Some((hwnd, items)) = with_host(|host| host.win_index(fence).map(|index| (host.fences[index].hwnd, host.item_menu_items(fence)))).flatten() else { return };
    let mut spec = MenuSpec { id: None, title: None, items };
    add_shell_menu_entry(&mut spec);
    if let Some(id) = popup(hwnd, &spec, point) {
        if id == "windows-shell-menu" {
            item_menu_native(fence, point);
        } else if let Some(after) = with_host(|host| host.menu_command(Some(fence), &id, point)) {
            run_after(after);
        }
    }
}

fn item_menu_native(fence: u64, point: POINT) {
    let input_started = with_host(|host| host.menu_input_started.take().filter(|(id, _)| *id == fence).map(|(_, time)| time)).flatten().unwrap_or_else(std::time::Instant::now);
    let Some((hwnd, pidls, target_key, profile)) = with_host(|host| host.win_index(fence).map(|index| {
        let window = &host.fences[index];
        let target = host.focused_entry(fence).filter(|entry| window.selection.contains(&entry.key))
            .or_else(|| window.entries.iter().find(|entry| window.selection.contains(&entry.key)));
        (window.hwnd, host.selected_pidls(fence), target.map(|entry| entry.key.clone()), host.explorer_config.clone())
    })).flatten() else { return };
    if pidls.is_empty() { return fence_menu(fence, point); }
    let mut request = menu_worker::Request::new(fence, hwnd, point, profile, Vec::new(), menu_worker::Target::Items { pidls, key: target_key }, key_down(VK_SHIFT));
    request.input_started = input_started;
    quick_fence_menu(request);
}


fn quick_fence_menu(mut request:menu_worker::Request){
    request.mode=crate::context_menu_process::Mode::More;
    queue_menu(request);
}

fn queue_menu(mut request:menu_worker::Request){
    let owner=HWND(request.owner as*mut _);
    let Some(host)=with_host(|host|host.hwnd)else{return;};
    let _=unsafe{SetForegroundWindow(owner)};
    request.foreground=unsafe{GetForegroundWindow()}.0 as isize;
    let result=MENU_WORKER.with(|worker|->anyhow::Result<()>{
        let mut worker=worker.borrow_mut();if worker.is_none(){*worker=Some(menu_worker::Worker::new(host)?);}
        worker.as_ref().unwrap().submit(request);Ok(())
    });
    if let Err(error)=result{file_action_error(owner,&windows::core::Error::new(windows::core::HRESULT(0x80004005u32 as i32),error.to_string()));}
    request_layer_repair();
}

fn menu_target_current(request:&menu_worker::Request)->bool{
    with_host(|host|host.win_index(request.fence).is_some_and(|index|{
        if host.fences[index].hwnd.0 as isize!=request.owner||!unsafe{IsWindowVisible(host.fences[index].hwnd)}.as_bool(){return false;}
        match &request.target{
            menu_worker::Target::Items{pidls,..}=>host.selected_pidls(request.fence)==*pidls,
            menu_worker::Target::Background{folder}=>host.target_folder(request.fence)==*folder,
        }
    })).unwrap_or(false)
}

fn menu_diagnostics()->serde_json::Value{MENU_WORKER.with(|worker|worker.borrow().as_ref().map_or(serde_json::Value::Null,menu_worker::Worker::diagnostics))}
fn menu_request_current(generation:u64)->bool{MENU_WORKER.with(|worker|worker.borrow().as_ref().is_some_and(|worker|worker.is_current(generation)))}

fn finish_menu_work(){
    let events=MENU_WORKER.with(|worker|worker.borrow_mut().as_mut().map(menu_worker::Worker::drain).unwrap_or_default());
    for event in events{match event{
        menu_worker::Event::Validate{request,before_invoke,answer}=>{
            let valid=menu_request_current(request.generation)&&menu_target_current(&request);
            if valid&&before_invoke&&matches!(request.target,menu_worker::Target::Background{..}){with_host(|host|host.expect_new_items(request.fence,Vec::new()));}
            let _=answer.try_send(valid);
        }
        menu_worker::Event::Finished{request,ids,outcome,..}=>{
            let _layer=MenuLayerRepair;
            let owner=HWND(request.owner as*mut _);
            // Shell commands already ran against their captured PIDLs. Only
            // application actions require the current selection to still match.
            let actionable=matches!(&outcome,Ok(MenuOutcome::Application(_)|MenuOutcome::Rename|MenuOutcome::Open|MenuOutcome::Explore));
            // A superseded construction can fail after another menu was
            // requested. Its error must not interrupt the newer interaction.
            if outcome.is_err()&&!menu_request_current(request.generation){continue;}
            if actionable&&(!menu_request_current(request.generation)||!menu_target_current(&request)){continue;}
            let target_key=match request.target{menu_worker::Target::Items{key,..}=>key,_=>None};
            finish_menu(request.fence,owner,request.point,target_key,&ids,outcome.map_err(|(code,message)|windows::core::Error::new(windows::core::HRESULT(code),message)));
        }
    }}
}

fn finish_menu(fence:u64,hwnd:HWND,point:POINT,target_key:Option<String>,ids:&[String],outcome:windows::core::Result<MenuOutcome>){
    match outcome {
        Ok(MenuOutcome::Application(command)) => dispatch_application_menu(fence, hwnd, command, &ids, point),
        Ok(action @ (MenuOutcome::Rename | MenuOutcome::Open | MenuOutcome::Explore)) => {
            let index = target_key.as_ref().and_then(|key| with_host(|h| h.win_index(fence)
                .and_then(|i| h.fences[i].entries.iter().position(|entry| &entry.key == key))).flatten());
            let Some(index) = index else {
                file_action_error(hwnd, &windows::core::Error::new(windows::core::HRESULT(0x80070490u32 as i32), "所选项目已变化，请重新选择"));
                return;
            };
            if action == MenuOutcome::Rename {
                if let Some(key) = target_key { with_host(|h| h.begin_rename(fence, fence::RenameTarget::Item(key))); }
            } else {
                open_with_verb(fence, index, action == MenuOutcome::Explore, Some(if action == MenuOutcome::Explore { "explore" } else { "open" }));
            }
        }
        Ok(MenuOutcome::Dismissed) => {},
        Ok(_) => refresh_fence(fence),
        Err(error) if matches!(error.code().0 as u32,0x80004004|0x800704c7)=>{},
        Err(error) => { file_action_error(hwnd, &error); refresh_fence(fence); }
    }
}

fn fence_menu(fence: u64, point: POINT) {
    with_host(|host| host.menu_input_started = None);
    if fast_context_menu_enabled() {
        fast_fence_menu(fence, point);
    } else {
        native_fence_menu(fence, point);
    }
}

fn fast_fence_menu(fence: u64, point: POINT) {
    let _layer = MenuLayerRepair;
    let Some((hwnd, mut spec, has_folder)) = with_host(|host| host.win_index(fence).map(|index|
        (host.fences[index].hwnd, host.fence_menu(fence), host.target_folder(fence).is_some()))).flatten() else { return };
    if has_folder { add_shell_menu_entry(&mut spec); }
    if let Some(id) = popup(hwnd, &spec, point) {
        if id == "windows-shell-menu" {
            folder_system_menu(fence, point);
        } else if let Some(after) = with_host(|host| host.menu_command(Some(fence), &id, point)) {
            run_after(after);
        }
    }
}

fn native_fence_menu(fence: u64, point: POINT) {
    MENU_WORKER.with(|worker| if let Some(worker) = worker.borrow().as_ref() { worker.cancel_current(); });
    tab_popup(fence, point, |host| host.fence_menu(fence));
}

fn folder_system_menu(fence: u64, point: POINT) {
    let input_started = std::time::Instant::now();
    let Some((hwnd, folder, profile)) = with_host(|host| host.win_index(fence).map(|index|
        (host.fences[index].hwnd, host.target_folder(fence), host.explorer_config.clone()))).flatten() else { return };
    if folder.is_none() { return; }
    let mut request = menu_worker::Request::new(fence, hwnd, point, profile, Vec::new(), menu_worker::Target::Background { folder }, key_down(VK_SHIFT));
    request.input_started = input_started;
    quick_fence_menu(request);
}

/// Small ZSUI menus anchored under a title-bar button ("+", "⌄ N").
fn tab_popup(fence: u64, point: POINT, spec: impl FnOnce(&DesktopHost) -> MenuSpec) {
    let _layer = MenuLayerRepair;
    let Some((hwnd, spec)) = with_host(|h| h.win_index(fence).map(|i| (h.fences[i].hwnd, spec(h)))).flatten() else { return };
    let _ = unsafe { SetForegroundWindow(hwnd) };
    request_layer_repair();
    if let Some(id) = popup(hwnd, &spec, point) {
        if let Some(a) = with_host(|h| h.menu_command(Some(fence), &id, point)) { run_after(a); }
    }
    let _ = unsafe { PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0)) };
}

/// Appearance and behaviour switches of the tray menu: applied in place while
/// the menu stays open. Everything else (windows, dialogs, quit) closes it.
fn tray_toggle(id: &str) -> bool {
    id.starts_with("theme-") || id.starts_with("opacity-") || matches!(id, "blur" | "browse-inside" | "hover-expand" | "autostart")
}

fn tray_menu(point: POINT) {
    apply_context_preferences();
    let Some((hwnd, spec)) = with_host(|h| (h.hwnd, h.tray_menu())) else { return };
    let _ = unsafe { SetForegroundWindow(hwnd) };
    let dark = with_host(|host| host.menu_dark()).unwrap_or_else(zsdesk_windows::theme::system_dark_mode);
    let chosen = crate::flyout_menu::show_persistent(hwnd, &spec, crate::flyout_menu::MenuAnchor::Point { x: point.x, y: point.y }, dark, &mut |id| {
        if !tray_toggle(id) { return None; }
        // Toggles never need modal follow-up work; a notice (autostart failure) does.
        match with_host(|h| h.menu_command(None, id, point))? {
            After::Nothing => with_host(|h| h.tray_menu()),
            other => { run_after(other); None }
        }
    });
    if let Some(id) = chosen.filter(|id| !tray_toggle(id)) {
        if let Some(a) = with_host(|h| h.menu_command(None, &id, point)) { run_after(a); }
    }else{with_host(|host|host.restore_tray_focus());}
    let _ = unsafe { PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0)) };
}

#[derive(Clone,Copy,Debug,PartialEq,Eq)]
enum TrayAction{Open,Menu{anchor:Option<(i32,i32)>}}
fn tray_action(version4:bool,wp:WPARAM,lp:LPARAM)->Option<TrayAction>{
    use windows::Win32::UI::Shell::{NIN_SELECT,NINF_KEY};
    let(icon,event)=if version4{(((lp.0 as usize>>16)&0xffff)as u32,(lp.0 as u32)&0xffff)}else{(wp.0 as u32,lp.0 as u32)};
    if icon!=host::TRAY_ID{return None;}
    if version4{
        if event==NIN_SELECT||event==(NIN_SELECT|NINF_KEY){return Some(TrayAction::Open);}
        if event==WM_CONTEXTMENU{let point=point_of(LPARAM(wp.0 as isize));return Some(TrayAction::Menu{anchor:Some((point.x,point.y))});}
        // Version 4 can report the raw mouse up as well as semantic activation;
        // dispatch only the semantic event so one click opens one window.
        None
    }else{match event{WM_LBUTTONUP=>Some(TrayAction::Open),WM_RBUTTONUP|WM_CONTEXTMENU=>Some(TrayAction::Menu{anchor:None}),_=>None}}
}
fn dispatch_tray(action:TrayAction){
    #[cfg(test)]if window_message_regression::capture_tray_action(action){return;}
    match action{
        TrayAction::Open=>{with_host(|host|host.menu_command(None,"open-zsdesk",POINT::default()));}
        TrayAction::Menu{anchor}=>{
            let mut point=anchor.map(|(x,y)|POINT{x,y}).unwrap_or_default();
            if anchor.is_none()||anchor==Some((-1,-1)){let _=unsafe{GetCursorPos(&mut point)};}
            run_after(After::TrayMenu(point));
        }
    }
}

fn apply_context_preferences() {
    if let Some(profile) = with_host(|h| h.explorer_config.clone()) {
        let _ = crate::context_preferences::apply_from_profile(&profile);
    }
}

fn drag(fence: u64) {
    let Some((hwnd, owner, pidls, keys, entries)) = with_host(|h| h.win_index(fence).map(|i| (h.fences[i].hwnd, h.hwnd, h.selected_pidls(fence), h.selected_keys(fence),
        h.selected_entries_owned(fence)))).flatten() else { return };
    if pidls.is_empty() { return; }
    let Ok(data) = desktop::data_object(hwnd, &pidls) else { return };
    with_host(|h| h.internal_drag = Some((fence, keys)));
    let surfaces = return_to_desktop::DesktopDropSurfaces::new(owner, fence, &data, entries);
    let result = unsafe { SHDoDragDrop(Some(hwnd), &data, None, DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK) };
    let returned = surfaces.completed();
    drop(surfaces);
    with_host(|h| h.internal_drag = None);
    if let Err(error) = result { file_action_error(owner, &error); }
    with_host(|host| { host.sync_windows(); if returned { host.repair_layer(); } });
    refresh_fence(fence);
    finish_deferred_quit();
}

fn open(fence: u64, index: usize, explorer: bool) {
    open_with_verb(fence, index, explorer, None);
}

fn open_with_verb(fence: u64, index: usize, explorer: bool, verb: Option<&str>) {
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
    if let Err(error) = desktop::open(hwnd, &entry.pidl, verb) { file_action_error(hwnd, &error); }
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
    let result = (|| -> windows::core::Result<()> {
        use zsdesk_windows::shell::file_transfer;
        if background&&verb=="paste"{
            let folder=with_host(|host|host.target_folder(fence)).flatten().ok_or_else(||windows::core::Error::from_hresult(windows::core::HRESULT(0x80070490u32 as i32)))?;
            with_host(|host|host.expect_new_items(fence,Vec::new()));
            return file_transfer::paste(hwnd,&folder);
        }
        if !background&&matches!(verb,"copy"|"cut"|"delete"){
            let pidls=with_host(|host|host.selected_pidls(fence)).unwrap_or_default();if pidls.is_empty(){return Ok(());}
            return if verb=="delete"{file_transfer::delete_paths(hwnd,file_transfer::paths_from_pidls(&pidls)?,shift)}else{file_transfer::copy_items(hwnd,&pidls,verb=="cut")};
        }
        if background {
            let folder = with_host(|h| h.target_folder(fence)).flatten()
                .ok_or_else(|| windows::core::Error::from_hresult(windows::core::HRESULT(0x80070490u32 as i32)))?;
            let folder = desktop::folder_for_path(&folder)?;
            let menu = ShellMenu::for_folder_background(hwnd, &folder)?;
            menu.populate_for_invoke(false)?;
            with_host(|h| h.expect_new_items(fence, Vec::new()));
            menu.invoke_verb(hwnd, verb, false)
        } else {
            let pidls = with_host(|h| h.selected_pidls(fence)).unwrap_or_default();
            if pidls.is_empty() { return Ok(()); }
            let menu = ShellMenu::for_items(hwnd, &pidls)?;
            menu.populate_for_invoke(shift)?;
            menu.invoke_verb(hwnd, verb, shift)
        }
    })();
    if let Err(error) = result { file_action_error(hwnd, &error); }
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
            if unsafe { InsertMenuW(hmenu, *position, flags, sub.0 as usize, PCWSTR(wide(label).as_ptr())) }.is_err() {
                let _ = unsafe { DestroyMenu(sub) };
            }
            *position += 1;
        }
        MenuItemSpec::Separator => {
            unsafe { let _ = InsertMenuW(hmenu, *position, MF_BYPOSITION | MF_SEPARATOR, 0, None); }
            *position += 1;
        }
        _ => {}
    }
}

/// Fence, tab and tray menus as Windows 11 style flyouts at `point`.
fn popup(owner: HWND, spec: &MenuSpec, point: POINT) -> Option<String> {
    let dark = with_host(|host| host.menu_dark()).unwrap_or_else(zsdesk_windows::theme::system_dark_mode);
    crate::flyout_menu::show(owner, spec, crate::flyout_menu::MenuAnchor::Point { x: point.x, y: point.y }, dark)
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
