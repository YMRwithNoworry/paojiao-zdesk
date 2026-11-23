//! Edit the search activation chord in the foreground dialog. Recording uses only the focused edit's key messages.
use super::control;
use anyhow::{Context, Result};
use std::{
    cell::{Cell, RefCell},
    path::Path,
};
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Controls::*,
            Input::KeyboardAndMouse::*,
            Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
            WindowsAndMessaging::*,
        },
    },
};
use zsdesk_core::i18n::text as tr;

const RESOURCE: u16 = 220;
const FIELD: i32 = 222;
const MODE: i32 = 223;
const HINT: i32 = 224;
const ENABLED: i32 = 225;
const PROBE_ID: i32 = 0x5a61;
const RECORDING: &str = "请按下新的组合键，或切换到手动输入。";
const TYPING: &str = "例如 Ctrl+Shift+F。修改后点“保存”生效。";
static EDITORS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
pub(super) fn dialog_active() -> bool {
    EDITORS.load(std::sync::atomic::Ordering::Acquire) != 0
}

struct Dialog<'a> {
    profile: &'a Path,
    initial: String,
    enabled: bool,
    recording: Cell<bool>,
    result: RefCell<Option<String>>,
    pending: RefCell<Option<(String, std::time::Instant)>>,
    dark: bool,
}
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
fn label(hwnd: HWND, id: i32, t: &str) {
    let _ = unsafe { SetDlgItemTextW(hwnd, id, PCWSTR(wide(t).as_ptr())) };
}
fn mode(hwnd: HWND, dialog: &Dialog, recording: bool) {
    dialog.recording.set(recording);
    label(
        hwnd,
        MODE,
        tr(if recording {
            "手动输入"
        } else {
            "录制组合键"
        }),
    );
    label(hwnd, HINT, tr(if recording { RECORDING } else { TYPING }));
    if let Ok(edit) = unsafe { GetDlgItem(Some(hwnd), FIELD) } {
        unsafe {
            SendMessageW(edit, EM_SETREADONLY, Some(WPARAM(recording as usize)), None);
            let _ = SetFocus(Some(edit));
            SendMessageW(edit, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
        }
    }
}

pub(crate) fn edit(owner: HWND, profile: &Path) -> Result<Option<String>> {
    let current = control::load(profile)?.preferences;
    let dark = zsdesk_windows::theme::menus_dark() && !zsdesk_windows::theme::high_contrast();
    let state = Dialog {
        profile,
        initial: current.hotkey,
        enabled: current.enabled,
        recording: Cell::new(true),
        result: RefCell::new(None),
        pending: RefCell::new(None),
        dark,
    };
    let instance = HINSTANCE(unsafe { GetModuleHandleW(None) }?.0);
    struct Editing;
    impl Drop for Editing {
        fn drop(&mut self) {
            EDITORS.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        }
    }
    EDITORS.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    let _editing = Editing;
    let answer = unsafe {
        DialogBoxParamW(
            Some(instance),
            PCWSTR(RESOURCE as usize as *const u16),
            Some(owner),
            Some(procedure),
            LPARAM(&state as *const _ as isize),
        )
    };
    anyhow::ensure!(answer != -1, "{}", windows::core::Error::from_thread());
    Ok(state.result.into_inner())
}

fn recorded_key(vk: u32, ctrl: bool, alt: bool, shift: bool, win: bool) -> Option<Result<String>> {
    let name = match vk {
        0x30..=0x39 | 0x41..=0x5a => char::from_u32(vk)?.to_string(),
        0x70..=0x87 => format!("F{}", vk - 0x70 + 1),
        0x0d => "Enter".into(),
        0x1b => "Escape".into(),
        0x09 => "Tab".into(),
        0x20 => "Space".into(),
        0x08 => "Backspace".into(),
        0x2e => "Delete".into(),
        0x25 => "Left".into(),
        0x26 => "Up".into(),
        0x27 => "Right".into(),
        0x28 => "Down".into(),
        0x24 => "Home".into(),
        0x23 => "End".into(),
        0x21 => "PageUp".into(),
        0x22 => "PageDown".into(),
        0x10..=0x12 | 0x5b | 0x5c | 0xa0..=0xa5 => return None,
        _ => match control::extra_key(vk) {
            Some(name) => name.into(),
            None => return Some(Err(anyhow::anyhow!("{}", tr("此按键不支持作为搜索快捷键")))),
        },
    };
    let mut parts = Vec::new();
    for (on, part) in [(ctrl, "Ctrl"), (alt, "Alt"), (shift, "Shift"), (win, "Win")] {
        if on {
            parts.push(part);
        }
    }
    parts.push(&name);
    Some(control::validate_hotkey(&parts.join("+")))
}

unsafe extern "system" fn field(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    id: usize,
    data: usize,
) -> LRESULT {
    let owner = HWND(data as *mut _);
    let state = unsafe { GetWindowLongPtrW(owner, GWLP_USERDATA) } as *const Dialog;
    if msg == WM_NCDESTROY {
        let _ = unsafe { RemoveWindowSubclass(hwnd, Some(field), id) };
    }
    if !state.is_null() && unsafe { (*state).recording.get() } {
        let down = |key: VIRTUAL_KEY| unsafe { GetKeyState(key.0 as i32) } < 0;
        let (ctrl, alt, shift, win) = (
            down(VK_CONTROL),
            down(VK_MENU),
            down(VK_SHIFT),
            down(VK_LWIN) || down(VK_RWIN),
        );
        let navigation = |v: usize| !ctrl && !alt && !win && matches!(v, 0x09 | 0x0d | 0x1b);
        if msg == WM_GETDLGCODE && lp.0 != 0 {
            let m = unsafe { &*(lp.0 as *const MSG) };
            if matches!(m.message, WM_KEYDOWN | WM_SYSKEYDOWN) && !navigation(m.wParam.0) {
                return LRESULT(DLGC_WANTMESSAGE as isize);
            }
        }
        if matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN) && !navigation(wp.0) {
            if let Some(result) = recorded_key(wp.0 as u32, ctrl, alt, shift, win) {
                match result {
                    Ok(chord) => {
                        label(owner, FIELD, &chord);
                        label(owner, HINT, tr("点“保存”应用这个快捷键。"));
                    }
                    Err(err) => {
                        label(owner, FIELD, "");
                        label(owner, HINT, &err.to_string());
                    }
                }
            }
            return LRESULT(0);
        }
        if matches!(msg, WM_CHAR | WM_SYSCHAR) {
            return LRESULT(0);
        }
    }
    unsafe { DefSubclassProc(hwnd, msg, wp, lp) }
}

fn save(hwnd: HWND, state: &Dialog) -> Result<(String, bool)> {
    let mut key = [0u16; 129];
    let count = unsafe { GetDlgItemTextW(hwnd, FIELD, &mut key) } as usize;
    let chord = control::validate_hotkey(&String::from_utf16_lossy(&key[..count]))?;
    let enabled = unsafe { IsDlgButtonChecked(hwnd, ENABLED) } == BST_CHECKED.0;
    let prefs = control::load(state.profile)?.preferences;
    let status = control::status(state.profile);
    let own = status["running"] == true
        && status["last_status"]["hotkey_registered"] == true
        && status["last_status"]["hotkey"]
            .as_str()
            .is_some_and(|key| key.eq_ignore_ascii_case(&chord));
    if enabled && !own {
        let (mods, key) = control::native_hotkey(&chord)?;
        unsafe { RegisterHotKey(Some(hwnd), PROBE_ID, mods, key) }.with_context(|| {
            zsdesk_core::tr_format!("无法使用 {}：组合键已被占用或由系统保留", chord)
        })?;
        unsafe { UnregisterHotKey(Some(hwnd), PROBE_ID) }?;
    }
    control::save_shortcut(state.profile, chord.clone(), enabled)?;
    if prefs.enabled || enabled || super::needs_background(state.profile) {
        control::signal(state.profile, "reload")?;
        super::start_if_enabled(state.profile)?;
    }
    Ok((chord, enabled))
}

unsafe extern "system" fn procedure(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> isize {
    if msg == WM_INITDIALOG {
        let state = unsafe { &*(lp.0 as *const Dialog) };
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, lp.0);
        }
        zsdesk_windows::theme::set_dark_frame(hwnd, state.dark);
        zsdesk_windows::theme::round_corners(hwnd, false);
        let _ = unsafe { SetWindowTextW(hwnd, PCWSTR(wide(tr("搜索快捷键")).as_ptr())) };
        for (id, s) in [
            (221, "唤起搜索窗口"),
            (ENABLED, "启用全局搜索快捷键"),
            (1, "保存"),
            (2, "取消"),
        ] {
            label(hwnd, id, tr(s));
        }
        for id in [FIELD, MODE, ENABLED, 1, 2] {
            if let Ok(c) = unsafe { GetDlgItem(Some(hwnd), id) } {
                zsdesk_windows::theme::theme_control(c, state.dark);
            }
        }
        label(hwnd, FIELD, &state.initial);
        if let Ok(edit) = unsafe { GetDlgItem(Some(hwnd), FIELD) } {
            unsafe {
                SendMessageW(edit, EM_SETLIMITTEXT, Some(WPARAM(128)), None);
            }
            let hooked =
                unsafe { SetWindowSubclass(edit, Some(field), 1, hwnd.0 as usize) }.as_bool();
            mode(hwnd, state, hooked);
        }
        let _ = unsafe {
            CheckDlgButton(
                hwnd,
                ENABLED,
                if state.enabled {
                    BST_CHECKED
                } else {
                    BST_UNCHECKED
                },
            )
        };
        return 0;
    }
    let p = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const Dialog;
    if p.is_null() {
        return 0;
    }
    let state = unsafe { &*p };
    match msg {
        WM_COMMAND => match (wp.0 & 0xffff) as i32 {
            1 => {
                if state.pending.borrow().is_some() {
                    return 1;
                }
                match save(hwnd, state) {
                    Ok((key, true)) => {
                        state
                            .pending
                            .replace(Some((key, std::time::Instant::now())));
                        for id in [1, 2, MODE, FIELD, ENABLED] {
                            if let Ok(c) = unsafe { GetDlgItem(Some(hwnd), id) } {
                                let _ = unsafe { EnableWindow(c, false) };
                            }
                        }
                        label(hwnd, HINT, tr("正在确认快捷键注册…"));
                        unsafe {
                            SetTimer(Some(hwnd), 1, 120, None);
                        }
                    }
                    Ok((key, false)) => {
                        state.result.replace(Some(zsdesk_core::tr_format!(
                            "已保存 {}，全局快捷键已关闭",
                            key
                        )));
                        let _ = unsafe { EndDialog(hwnd, 1) };
                    }
                    Err(e) => label(hwnd, HINT, &format!("{e:#}")),
                }
                1
            }
            2 => {
                if state.pending.borrow().is_none() {
                    let _ = unsafe { EndDialog(hwnd, 0) };
                }
                1
            }
            MODE => {
                mode(hwnd, state, !state.recording.get());
                1
            }
            _ => 0,
        },
        WM_CTLCOLORDLG | WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT if state.dark => {
            let dc = HDC(wp.0 as *mut _);
            let color = if msg == WM_CTLCOLOREDIT
                || unsafe { GetDlgCtrlID(HWND(lp.0 as *mut _)) } == FIELD
            {
                0x383838
            } else {
                0x202020
            };
            unsafe {
                SetTextColor(dc, COLORREF(0xf3f3f3));
                SetBkColor(dc, COLORREF(color));
                SetDCBrushColor(dc, COLORREF(color));
                GetStockObject(DC_BRUSH).0 as isize
            }
        }
        WM_TIMER if wp.0 == 1 => {
            let Some((key, since)) = state.pending.borrow().clone() else {
                return 1;
            };
            let status = control::status(state.profile);
            let last = &status["last_status"];
            let live = status["running"] == true;
            let active = last["hotkey_registered"] == true;
            if live
                && active
                && last["hotkey"].as_str() == Some(key.as_str())
                && last["error"].is_null()
            {
                state
                    .result
                    .replace(Some(zsdesk_core::tr_format!("唤起快捷键已设为 {}", key)));
                let _ = unsafe { EndDialog(hwnd, 1) };
            } else if let Some(err) = last["error"].as_str().filter(|e| {
                e.contains(&key) && since.elapsed() >= std::time::Duration::from_millis(350)
            }) {
                let _ = unsafe { KillTimer(Some(hwnd), 1) };
                state.pending.replace(None);
                for id in [1, 2, MODE, FIELD, ENABLED] {
                    if let Ok(c) = unsafe { GetDlgItem(Some(hwnd), id) } {
                        let _ = unsafe { EnableWindow(c, true) };
                    }
                }
                label(hwnd, HINT, err);
            } else if since.elapsed() >= std::time::Duration::from_secs(4) {
                state.result.replace(Some(zsdesk_core::tr_format!(
                    "已保存 {}，后台尚未确认注册",
                    key
                )));
                let _ = unsafe { EndDialog(hwnd, 1) };
            }
            1
        }
        WM_CLOSE => {
            if state.pending.borrow().is_none() {
                let _ = unsafe { EndDialog(hwnd, 0) };
            }
            1
        }
        WM_NCDESTROY => {
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            }
            0
        }
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recorded_chords_use_the_registration_parser() {
        for (vk, c, a, s, w) in [
            (0x4b, true, true, false, false),
            (0xbf, true, false, false, false),
            (0x65, true, false, false, true),
            (0x7a, false, false, false, false),
            (0x20, true, false, true, false),
        ] {
            let key = recorded_key(vk, c, a, s, w).unwrap().unwrap();
            let (mods, code) = control::native_hotkey(&key).unwrap();
            assert_eq!(code, vk);
            assert_eq!(mods.contains(MOD_CONTROL), c);
            assert_eq!(mods.contains(MOD_WIN), w);
            assert_eq!(mods.contains(MOD_SHIFT), s);
            assert_eq!(mods.contains(MOD_ALT), a);
        }
        assert!(recorded_key(0x10, false, false, true, false).is_none());
        assert!(recorded_key(0x4b, false, false, false, false)
            .unwrap()
            .is_err());
        assert!(recorded_key(0x7b, true, true, false, false)
            .unwrap()
            .is_err());
        assert_eq!(
            control::validate_hotkey("Ctrl+/").unwrap(),
            control::validate_hotkey("Ctrl+Slash").unwrap()
        );
    }
}
