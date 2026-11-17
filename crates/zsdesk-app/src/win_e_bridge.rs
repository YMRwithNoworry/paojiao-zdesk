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
            KEYEVENTF_KEYUP, KEYEVENTF_EXTENDEDKEY, VIRTUAL_KEY,
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
    masked_windows:u8,
}
#[derive(Debug, PartialEq, Eq)]
enum Decision {
    Pass,
    Suppress,
    Launch,
}
impl Chord {
    fn capture_windows(&mut self,left:bool,right:bool){self.masked_windows|=u8::from(left)|(u8::from(right)<<1);}
    fn mask_release(&mut self,key:u32,down:bool)->bool{
        let bit=match key{0x5b=>1,0x5c=>2,_=>0};
        if down||bit==0||self.masked_windows&bit==0{return false;}
        self.masked_windows&=!bit;true
    }
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
    Launch(Arc<AtomicBool>),
    Stop,
}
struct HookContext {
    chord: Chord,
    enabled: Arc<AtomicBool>,
    requests: mpsc::SyncSender<Request>,
    mask_ready: Option<Arc<AtomicBool>>,
}
thread_local! { static HOOK_CONTEXT: RefCell<Option<HookContext>> = const { RefCell::new(None) }; }
const INPUT_TAG:usize=0x5a53454d;
fn masked_release_inputs(key:u32)->[INPUT;3]{
    let event=|key:u16,flags|INPUT{r#type:INPUT_KEYBOARD,Anonymous:INPUT_0{ki:KEYBDINPUT{wVk:VIRTUAL_KEY(key),dwFlags:flags,dwExtraInfo:INPUT_TAG,..Default::default()}}};
    // Preserve the Windows key-up in the same input batch as the harmless
    // menu-mask key. A new foreground window must not see a bare Win release.
    [event(0xe8,Default::default()),event(key as u16,KEYEVENTF_KEYUP|KEYEVENTF_EXTENDEDKEY),event(0xe8,KEYEVENTF_KEYUP)]
}

unsafe extern "system" fn keyboard_hook(code: i32, message: WPARAM, data: LPARAM) -> LRESULT {
    if code < 0 || data.0 == 0 {
        return unsafe { CallNextHookEx(None, code, message, data) };
    }
    let message_id = message.0 as u32;
    let down = matches!(message_id, WM_KEYDOWN | WM_SYSKEYDOWN);
    if !down && !matches!(message_id, WM_KEYUP | WM_SYSKEYUP) {
        return unsafe { CallNextHookEx(None, code, message, data) };
    }
    let input=unsafe{&*(data.0 as*const KBDLLHOOKSTRUCT)};
    if input.dwExtraInfo==INPUT_TAG{
        if input.vkCode==0xe8&&!down{
            HOOK_CONTEXT.with(|slot|{if let Ok(mut slot)=slot.try_borrow_mut(){if let Some(context)=slot.as_mut(){
                if let Some(ready)=context.mask_ready.take(){ready.store(true,Ordering::Release);}
            }}});
        }
        return unsafe{CallNextHookEx(None,code,message,data)};
    }
    let key = input.vkCode;
    let mask_release=HOOK_CONTEXT.with(|slot|slot.try_borrow_mut().ok().and_then(|mut slot|slot.as_mut().map(|context|context.chord.mask_release(key,down))).unwrap_or(false));
    if mask_release{
        let events=masked_release_inputs(key);
        if unsafe{SendInput(&events,std::mem::size_of::<INPUT>()as i32)}==events.len()as u32{return LRESULT(1);}
        // An injection failure must not strand a logically pressed modifier.
        return unsafe{CallNextHookEx(None,code,message,data)};
    }
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
        if decision==Decision::Launch{context.chord.capture_windows(pressed(0x5b),pressed(0x5c));}
        decision
    });
    if decision == Decision::Launch {
        let ready=Arc::new(AtomicBool::new(false));
        HOOK_CONTEXT.with(|slot|{if let Some(context)=slot.borrow_mut().as_mut(){context.mask_ready=Some(ready.clone());}});
        // Mark the held Windows key as used, so releasing it does not also open
        // Start. 0xE8 is unassigned; no ordinary character or shortcut is sent.
        let key_down = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0xe8),
                    dwExtraInfo:INPUT_TAG,
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
                    dwExtraInfo:INPUT_TAG,
                    ..Default::default()
                },
            },
        };
        let inserted=unsafe{SendInput(&[key_down,key_up],std::mem::size_of::<INPUT>()as i32)};
        // Do not activate another foreground window until the input stream has
        // acknowledged the mask. Enqueuing Launch first races Shell's Win-key
        // state, especially on Windows 11 23H2.
        let accepted=inserted==2&&HOOK_CONTEXT.with(|slot|slot.borrow().as_ref()
            .is_some_and(|context|context.requests.try_send(Request::Launch(ready)).is_ok()));
        if !accepted{
            HOOK_CONTEXT.with(|slot|{if let Some(context)=slot.borrow_mut().as_mut(){
                context.chord.captured=false;context.chord.masked_windows=0;context.mask_ready=None;
            }});
            return unsafe{CallNextHookEx(None,code,message,data)};
        }
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
fn wait_for_mask(ready:&AtomicBool,timeout:Duration)->bool{
    let deadline=std::time::Instant::now()+timeout;
    while !ready.load(Ordering::Acquire)&&std::time::Instant::now()<deadline{thread::sleep(Duration::from_millis(2));}
    ready.load(Ordering::Acquire)
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
                                mask_ready: None,
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
                        let launch = matches!(request, Ok(Request::Launch(_)));
                        if launch {
                            let mask_ready=match request{Ok(Request::Launch(ready))=>ready,_=>unreachable!()};
                            let result = if !wait_for_mask(&mask_ready,Duration::from_millis(250)){
                                Err(anyhow::anyhow!("Win+E 按键状态未就绪，已保留系统打开方式"))
                            } else if active {
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
    fn launch_waits_for_its_own_mask_acknowledgement(){
        let ready=Arc::new(AtomicBool::new(false));assert!(!wait_for_mask(&ready,Duration::ZERO));
        let pending=ready.clone();let worker=thread::spawn(move||wait_for_mask(&pending,Duration::from_secs(1)));
        thread::sleep(Duration::from_millis(10));ready.store(true,Ordering::Release);assert!(worker.join().unwrap());
        assert!(!wait_for_mask(&AtomicBool::new(false),Duration::ZERO));
    }
    #[test]
    fn captured_windows_releases_are_masked_once_without_eating_other_chords(){
        let mut chord=Chord::default();
        assert!(!chord.mask_release(0x5b,false));
        assert_eq!(chord.input(0x45,true,true,false,true),Decision::Launch);
        chord.capture_windows(true,true);
        assert_eq!(chord.input(0x44,true,true,false,true),Decision::Pass);
        assert!(!chord.mask_release(0x5b,true));
        assert!(chord.mask_release(0x5b,false));assert!(!chord.mask_release(0x5b,false));
        assert!(chord.mask_release(0x5c,false));assert!(!chord.mask_release(0x5c,false));
        assert_eq!(chord.input(0x45,false,false,false,true),Decision::Suppress);
        for key in [0x5b,0x5c]{let inputs=masked_release_inputs(key);let a=unsafe{inputs[0].Anonymous.ki};let b=unsafe{inputs[1].Anonymous.ki};let c=unsafe{inputs[2].Anonymous.ki};
            assert_eq!(a.wVk.0,0xe8);assert_eq!(b.wVk.0,key as u16);assert_eq!(c.wVk.0,0xe8);
            assert!(!a.dwFlags.contains(KEYEVENTF_KEYUP)&&b.dwFlags.contains(KEYEVENTF_KEYUP)&&c.dwFlags.contains(KEYEVENTF_KEYUP));
            assert_eq!([a.dwExtraInfo,b.dwExtraInfo,c.dwExtraInfo],[INPUT_TAG;3]);
        }
    }
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
