//! Resident Win+E routing. The hook only recognizes this chord; registry and
//! process work run on a separate worker so the hook thread never waits on I/O.
use anyhow::{Context, Result};
use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use windows::Win32::{
    Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM},
    System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
    UI::{
        Input::KeyboardAndMouse::{
            GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT,
            KEYEVENTF_KEYUP, VIRTUAL_KEY,
        },
        WindowsAndMessaging::{
            CallNextHookEx, DispatchMessageW, GetMessageW, PeekMessageW, PostThreadMessageW,
            SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, KBDLLHOOKSTRUCT, MSG,
            PM_NOREMOVE, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP,
        },
    },
};

#[derive(Default)]
struct Chord {
    captured: bool,
}
#[derive(Debug, PartialEq, Eq)]
enum Decision {
    Pass,
    Suppress,
    Launch,
}
impl Chord {
    fn input(
        &mut self,
        key: u32,
        down: bool,
        win: bool,
        other_modifier: bool,
        enabled: bool,
    ) -> Decision {
        if key != 0x45 {
            return Decision::Pass;
        }
        if !down {
            return if std::mem::take(&mut self.captured) {
                Decision::Suppress
            } else {
                Decision::Pass
            };
        }
        if self.captured {
            return Decision::Suppress;
        }
        if enabled && win && !other_modifier {
            self.captured = true;
            Decision::Launch
        } else {
            Decision::Pass
        }
    }
}

enum Request {
    Launch,
    Stop,
}
struct HookContext {
    chord: Chord,
    enabled: Arc<AtomicBool>,
    requests: mpsc::SyncSender<Request>,
}
thread_local! { static HOOK_CONTEXT: RefCell<Option<HookContext>> = const { RefCell::new(None) }; }

unsafe extern "system" fn keyboard_hook(code: i32, message: WPARAM, data: LPARAM) -> LRESULT {
    if code < 0 || data.0 == 0 {
        return unsafe { CallNextHookEx(None, code, message, data) };
    }
    let message_id = message.0 as u32;
    let down = matches!(message_id, WM_KEYDOWN | WM_SYSKEYDOWN);
    if !down && !matches!(message_id, WM_KEYUP | WM_SYSKEYUP) {
        return unsafe { CallNextHookEx(None, code, message, data) };
    }
    let key = unsafe { (*(data.0 as *const KBDLLHOOKSTRUCT)).vkCode };
    if key != 0x45 {
        return unsafe { CallNextHookEx(None, code, message, data) };
    }
    let pressed = |key| unsafe { GetAsyncKeyState(key) < 0 };
    let win = pressed(0x5b) || pressed(0x5c);
    let other = pressed(0x10) || pressed(0x11) || pressed(0x12);
    let decision = HOOK_CONTEXT.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(context) = slot.as_mut() else {
            return Decision::Pass;
        };
        let decision = context.chord.input(
            key,
            down,
            win,
            other,
            context.enabled.load(Ordering::Acquire),
        );
        if decision == Decision::Launch && context.requests.try_send(Request::Launch).is_err() {
            // Never consume a chord when its handler cannot accept the launch.
            context.chord.captured = false;
            return Decision::Pass;
        }
        decision
    });
    if decision == Decision::Launch {
        // Mark the held Windows key as used, so releasing it does not also open
        // Start. 0xE8 is unassigned; no ordinary character or shortcut is sent.
        let key_down = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0xe8),
                    ..Default::default()
                },
            },
        };
        let key_up = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0xe8),
                    dwFlags: KEYEVENTF_KEYUP,
                    ..Default::default()
                },
            },
        };
        let _ = unsafe { SendInput(&[key_down, key_up], std::mem::size_of::<INPUT>() as i32) };
    }
    if decision != Decision::Pass {
        LRESULT(1)
    } else {
        unsafe { CallNextHookEx(None, code, message, data) }
    }
}

fn owned(executable: &Path) -> bool {
    use zsdesk_windows::integration::{self, Integration};
    integration::current() == Integration::Default && integration::win_e_enabled(executable)
}

fn write_status(
    profile: &Path,
    installed: bool,
    active: bool,
    launches: u64,
    launch_ok: Option<bool>,
    error: Option<&str>,
) {
    let value = serde_json::json!({"schema":1,"pid":std::process::id(),"installed":installed,
        "active":active,"mode":"resident-keyboard-hook","launch_count":launches,
        "last_launch_succeeded":launch_ok,"error":error});
    if let Ok(bytes) = serde_json::to_vec_pretty(&value) {
        let _ = crate::quick_search::control::atomic_write(
            &profile.with_file_name("win-e-status.json"),
            &bytes,
        );
    }
}

pub fn report_error(profile: &Path, error: &str) {
    write_status(profile, false, false, 0, None, Some(error));
}

pub struct Bridge {
    thread_id: u32,
    hook: Option<JoinHandle<()>>,
    worker: Option<JoinHandle<()>>,
    requests: mpsc::SyncSender<Request>,
    enabled: Arc<AtomicBool>,
}
impl Bridge {
    pub fn start(profile: &Path) -> Result<Option<Self>> {
        // A temporary/test profile must never take over the user's keyboard.
        let expected = std::path::absolute(crate::storage::config_path())?;
        if !std::path::absolute(profile)?
            .to_string_lossy()
            .eq_ignore_ascii_case(&expected.to_string_lossy())
        {
            return Ok(None);
        }
        let executable = std::env::current_exe()?;
        let enabled = Arc::new(AtomicBool::new(false));
        let (requests, incoming) = mpsc::sync_channel(8);
        let (ready, started) = mpsc::sync_channel(1);
        let hook_enabled = enabled.clone();
        let hook_requests = requests.clone();
        let hook = thread::Builder::new()
            .name("zsdesk-win-e-hook".into())
            .stack_size(512 * 1024)
            .spawn(move || {
                let mut message = MSG::default();
                let _ = unsafe { PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE) };
                let result = (|| -> windows::core::Result<_> {
                    let module = unsafe { GetModuleHandleW(None)? };
                    unsafe {
                        SetWindowsHookExW(
                            WH_KEYBOARD_LL,
                            Some(keyboard_hook),
                            Some(HINSTANCE(module.0)),
                            0,
                        )
                    }
                })();
                match result {
                    Ok(hook) => {
                        HOOK_CONTEXT.with(|slot| {
                            *slot.borrow_mut() = Some(HookContext {
                                chord: Chord::default(),
                                enabled: hook_enabled,
                                requests: hook_requests,
                            })
                        });
                        let _ = ready.send(Ok(unsafe { GetCurrentThreadId() }));
                        while unsafe { GetMessageW(&mut message, None, 0, 0) }.0 > 0 {
                            let _ = unsafe { TranslateMessage(&message) };
                            unsafe {
                                DispatchMessageW(&message);
                            }
                        }
                        let _ = unsafe { UnhookWindowsHookEx(hook) };
                        HOOK_CONTEXT.with(|slot| *slot.borrow_mut() = None);
                    }
                    Err(error) => {
                        let _ = ready.send(Err(error.to_string()));
                    }
                }
            })?;
        let thread_id = match started.recv() {
            Ok(Ok(id)) => id,
            Ok(Err(error)) => {
                let _ = hook.join();
                anyhow::bail!("Win+E 接管启动失败：{error}");
            }
            Err(error) => {
                let _ = hook.join();
                anyhow::bail!("Win+E 接管线程未就绪：{error}");
            }
        };
        let mut bridge = Self {
            thread_id,
            hook: Some(hook),
            worker: None,
            requests,
            enabled,
        };
        let profile: PathBuf = profile.into();
        let worker_enabled = bridge.enabled.clone();
        bridge.worker = Some(
            thread::Builder::new()
                .name("zsdesk-win-e-launch".into())
                .stack_size(512 * 1024)
                .spawn(move || {
                    let mut active = owned(&executable);
                    worker_enabled.store(active, Ordering::Release);
                    let mut launches = 0;
                    let mut launch_ok = None;
                    let mut last_error = None;
                    write_status(&profile, true, active, launches, launch_ok, None);
                    loop {
                        let request = incoming.recv_timeout(Duration::from_millis(500));
                        if matches!(
                            request,
                            Ok(Request::Stop) | Err(mpsc::RecvTimeoutError::Disconnected)
                        ) {
                            break;
                        }
                        let current = owned(&executable);
                        let changed = current != active;
                        active = current;
                        worker_enabled.store(active, Ordering::Release);
                        let launch = matches!(request, Ok(Request::Launch));
                        if launch {
                            let result = if active {
                                crate::quick_search::spawn_utility(&profile, "--win-e")
                            } else {
                                crate::app::open_original_explorer("shell:MyComputerFolder".into())
                                    .map_err(Into::into)
                            };
                            launches += 1;
                            launch_ok = Some(result.is_ok());
                            last_error = result.err().map(|error| format!("{error:#}"));
                            if last_error.is_some() && active {
                                let _ = crate::app::open_original_explorer(
                                    "shell:MyComputerFolder".into(),
                                );
                            }
                        }
                        if changed || launch {
                            write_status(
                                &profile,
                                true,
                                active,
                                launches,
                                launch_ok,
                                last_error.as_deref(),
                            );
                        }
                    }
                    worker_enabled.store(false, Ordering::Release);
                    write_status(
                        &profile,
                        false,
                        false,
                        launches,
                        launch_ok,
                        last_error.as_deref(),
                    );
                })
                .context("无法启动 Win+E 命令处理线程")?,
        );
        Ok(Some(bridge))
    }
}
impl Drop for Bridge {
    fn drop(&mut self) {
        self.enabled.store(false, Ordering::Release);
        let _ = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
        if let Some(hook) = self.hook.take() {
            let _ = hook.join();
        }
        let _ = self.requests.send(Request::Stop);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn win_e_launches_once_and_consumes_its_repeat_and_release() {
        let mut chord = Chord::default();
        assert_eq!(chord.input(0x45, true, true, false, true), Decision::Launch);
        assert_eq!(
            chord.input(0x45, true, true, false, true),
            Decision::Suppress
        );
        assert_eq!(
            chord.input(0x45, false, false, false, true),
            Decision::Suppress
        );
        assert_eq!(chord.input(0x45, true, true, false, true), Decision::Launch);
    }
    #[test]
    fn ordinary_keys_and_modified_chords_are_preserved() {
        let mut chord = Chord::default();
        assert_eq!(chord.input(0x45, true, false, false, true), Decision::Pass);
        assert_eq!(chord.input(0x45, false, false, false, true), Decision::Pass);
        assert_eq!(chord.input(0x45, true, true, true, true), Decision::Pass);
        assert_eq!(chord.input(0x44, true, true, false, true), Decision::Pass);
        assert_eq!(chord.input(0x45, true, true, false, false), Decision::Pass);
    }
    #[test]
    fn restoring_integration_mid_chord_still_pairs_the_consumed_key() {
        let mut chord = Chord::default();
        assert_eq!(chord.input(0x45, true, true, false, true), Decision::Launch);
        assert_eq!(
            chord.input(0x45, true, false, false, false),
            Decision::Suppress
        );
        assert_eq!(
            chord.input(0x45, false, false, false, false),
            Decision::Suppress
        );
        assert_eq!(chord.input(0x45, true, true, false, false), Decision::Pass);
    }
}
