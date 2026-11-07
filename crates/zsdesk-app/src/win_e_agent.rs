//! Optional per-user Win+E interceptor. Runs as a login-started background process;
//! the hook callback only queues a private thread message and never launches UI.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};

use anyhow::{Context, Result, bail};
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::{CloseHandle, GetLastError, HANDLE, HINSTANCE, LPARAM, LRESULT, WPARAM, ERROR_ALREADY_EXISTS},
        System::{LibraryLoader::GetModuleHandleW, Threading::{CreateEventW, GetCurrentThreadId, OpenEventW, SetEvent, WaitForSingleObject, EVENT_MODIFY_STATE, INFINITE}},
        UI::{
            Input::KeyboardAndMouse::{GetAsyncKeyState, VK_E, VK_LWIN, VK_RWIN},
            WindowsAndMessaging::{
                CallNextHookEx, DispatchMessageW, GetMessageW, KBDLLHOOKSTRUCT, KBDLLHOOKSTRUCT_FLAGS, LLKHF_UP,
                MSG, PeekMessageW, PostThreadMessageW, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx,
                WH_KEYBOARD_LL, WM_APP, WM_QUIT, PM_NOREMOVE,
            },
        },
    },
};

const OPEN_THIS_PC: u32 = WM_APP + 0x61;
const STOP_EVENT: &str = "Local\\ZSDesk.WinE.Agent.Stop";
static HOOK_THREAD: AtomicU32 = AtomicU32::new(0);
const LEFT_WIN_DOWN: u8 = 1;
const RIGHT_WIN_DOWN: u8 = 2;
static WIN_KEY_STATE: AtomicU8 = AtomicU8::new(0);
fn update_win_key_state(state: u8, vk: u32, key_up: bool) -> u8 {
    let mask = match vk { x if x == VK_LWIN.0 as u32 => LEFT_WIN_DOWN, x if x == VK_RWIN.0 as u32 => RIGHT_WIN_DOWN, _ => return state };
    if key_up { state & !mask } else { state | mask }
}
static E_DOWN: AtomicBool = AtomicBool::new(false);
fn is_win_e_press(vk: u32, key_up: bool, left_win: bool, right_win: bool) -> bool {
    vk == VK_E.0 as u32 && !key_up && (left_win || right_win)
}


struct KeyboardHook(windows::Win32::UI::WindowsAndMessaging::HHOOK);
impl Drop for KeyboardHook {
    fn drop(&mut self) { let _ = unsafe { UnhookWindowsHookEx(self.0) }; }
}

fn stop_event_name() -> Vec<u16> { STOP_EVENT.encode_utf16().chain(Some(0)).collect() }

pub fn stop() -> Result<bool> {
    let name = stop_event_name();
    let event = match unsafe { OpenEventW(EVENT_MODIFY_STATE, false, PCWSTR(name.as_ptr())) } {
        Ok(event) => event,
        Err(_) => return Ok(false),
    };
    let signaled = unsafe { SetEvent(event) }.is_ok();
    let _ = unsafe { CloseHandle(event) };
    Ok(signaled)
}

/// Runs until the named stop signal is raised or its message pump is terminated.
pub fn run() -> Result<()> {
    let name = stop_event_name();
    let event = unsafe { CreateEventW(None, true, false, PCWSTR(name.as_ptr())) }.context("创建 Win+E Agent 停止信号失败")?;
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        let _ = unsafe { CloseHandle(event) };
        bail!("Win+E Agent 已经运行");
    }
    let thread = unsafe { GetCurrentThreadId() };
    HOOK_THREAD.store(thread, Ordering::Release);
    let mut win_state = 0;
    if unsafe { GetAsyncKeyState(VK_LWIN.0 as i32) } < 0 { win_state |= LEFT_WIN_DOWN; }
    if unsafe { GetAsyncKeyState(VK_RWIN.0 as i32) } < 0 { win_state |= RIGHT_WIN_DOWN; }
    WIN_KEY_STATE.store(win_state, Ordering::Release);
    let module = unsafe { GetModuleHandleW(None) }.context("获取 Win+E 拦截器模块失败")?;
    let hook = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), Some(HINSTANCE(module.0)), 0) }
        .context("安装 Win+E 键盘钩子失败")?;
    let _hook = KeyboardHook(hook);
    let event_handle = event.0 as usize;
    let mut message = MSG::default();
    let _ = unsafe { PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE) };
    std::thread::Builder::new().name("zsdesk-wine-stop".into()).spawn(move || {
        let event = HANDLE(event_handle as *mut _);
        unsafe { WaitForSingleObject(event, INFINITE); }
        let thread = HOOK_THREAD.load(Ordering::Acquire);
        if thread != 0 { let _ = unsafe { PostThreadMessageW(thread, WM_QUIT, WPARAM(0), LPARAM(0)) }; }
        let _ = unsafe { CloseHandle(event) };
    }).context("启动 Win+E Agent 停止监听失败")?;
    loop {
        let result = unsafe { GetMessageW(&mut message, None, 0, 0) }.0;
        if result == -1 { bail!("Win+E 后台消息循环失败"); }
        if result == 0 || message.message == WM_QUIT { break; }
        if message.message == OPEN_THIS_PC {
            let launched = std::env::current_exe().ok()
                .and_then(|exe| std::process::Command::new(exe).arg("shell:MyComputerFolder").spawn().ok())
                .is_some();
            if !launched {
                let _ = std::process::Command::new("explorer.exe").arg("shell:MyComputerFolder").spawn();
            }
        } else {
            let _ = unsafe { TranslateMessage(&message) };
            unsafe { DispatchMessageW(&message); }
        }
    }
    HOOK_THREAD.store(0, Ordering::Release);
    Ok(())
}

unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 && lparam.0 != 0 {
        let key = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        let key_up = key.flags & LLKHF_UP != KBDLLHOOKSTRUCT_FLAGS(0);
        let win_state = update_win_key_state(WIN_KEY_STATE.load(Ordering::Relaxed), key.vkCode, key_up);
        WIN_KEY_STATE.store(win_state, Ordering::Release);
        let win_down = win_state != 0
            || unsafe { GetAsyncKeyState(VK_LWIN.0 as i32) } < 0
            || unsafe { GetAsyncKeyState(VK_RWIN.0 as i32) } < 0;
        if key.vkCode == VK_E.0 as u32 && key_up { E_DOWN.store(false, Ordering::Release); }
        if is_win_e_press(key.vkCode, key_up, win_down, false) && !E_DOWN.swap(true, Ordering::AcqRel) {
            let thread = HOOK_THREAD.load(Ordering::Acquire);
            if thread != 0 && unsafe { PostThreadMessageW(thread, OPEN_THIS_PC, WPARAM(0), LPARAM(0)) }.is_ok() {
                return LRESULT(1);
            }
            E_DOWN.store(false, Ordering::Release);
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::is_win_e_press;
    use windows::Win32::UI::Input::KeyboardAndMouse::VK_E;

    #[test]
    fn only_win_modified_e_key_down_is_intercepted() {
        assert!(is_win_e_press(VK_E.0 as u32, false, true, false));
        assert!(is_win_e_press(VK_E.0 as u32, false, false, true));
        assert!(!is_win_e_press(VK_E.0 as u32, false, false, false));
        assert!(!is_win_e_press(VK_E.0 as u32, true, true, false));
        assert!(!is_win_e_press(0x41, false, true, false));
    }

    #[test]
    fn modifier_state_tracks_both_win_keys_and_releases_independently() {
        use super::{update_win_key_state, LEFT_WIN_DOWN, RIGHT_WIN_DOWN};
        use windows::Win32::UI::Input::KeyboardAndMouse::{VK_LWIN, VK_RWIN};
        let state = update_win_key_state(0, VK_LWIN.0 as u32, false);
        let state = update_win_key_state(state, VK_RWIN.0 as u32, false);
        assert_eq!(state, LEFT_WIN_DOWN | RIGHT_WIN_DOWN);
        let state = update_win_key_state(state, VK_LWIN.0 as u32, true);
        assert_eq!(state, RIGHT_WIN_DOWN);
        assert_eq!(update_win_key_state(state, VK_RWIN.0 as u32, true), 0);
        assert_eq!(update_win_key_state(0, 0x41, false), 0);
    }
}

