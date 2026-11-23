//! Native command/hotkey receiver and modeless search-panel lifetime.
use super::{Action,control};
use std::path::{Path,PathBuf};
use anyhow::{Context,Result};
use windows::{core::w,Win32::{Foundation::{HWND,LPARAM,LRESULT,WPARAM,HINSTANCE},System::LibraryLoader::GetModuleHandleW,
    UI::{Input::KeyboardAndMouse::*,WindowsAndMessaging::*}}};
const EVENT:u32=WM_APP+75;
const HOTKEY:i32=0x5a53;
const LIFETIME_TIMER:usize=1;
struct State{profile:PathBuf,registered:bool,registration_id:i32,hotkey:String,error:Option<String>,persistent:bool,start_with_windows:bool,visible:bool,lifetime_check:std::time::Instant}

fn chord(text:&str)->Result<(HOT_KEY_MODIFIERS,u32)>{control::native_hotkey(text)}
fn focus_main(profile:&Path,settings:bool)->Result<()>{
    if !settings{return super::popup::show(profile);}
    if crate::instance::focus_search(profile,settings)?{return Ok(());}
    let args=vec![if settings{"--search-settings"}else{"--focus-search"}.into(),"--profile-dir".into(),profile.parent().context(zsdesk_core::i18n::text("配置目录缺失"))?.as_os_str().to_owned()];
    if !crate::runtime::enqueue_local(profile,args.clone())?{
        use std::os::windows::process::CommandExt;
        std::process::Command::new(std::env::current_exe()?).args(args).creation_flags(0x08000000).spawn()?;
    }
    Ok(())
}
fn configure(hwnd:HWND,state:&mut State)->Result<bool>{
    let prefs=control::load(&state.profile)?.preferences;
    state.start_with_windows=prefs.enabled&&prefs.start_with_windows;
    let requested=if prefs.enabled{Some(chord(&prefs.hotkey)?)}else{None};
    state.error=None;
    if state.registered&&prefs.enabled&&state.hotkey.eq_ignore_ascii_case(&prefs.hotkey){
        control::write_status(&state.profile,None,false,&state.hotkey,true);return Ok(true);
    }
    if let Some((mods,key))=requested{
        let next_id=if state.registered && state.registration_id==HOTKEY{HOTKEY+1}else{HOTKEY};
        match unsafe{RegisterHotKey(Some(hwnd),next_id,mods,key)}{
            Ok(())=>{
                if state.registered {
                    if let Err(error)=unsafe{UnregisterHotKey(Some(hwnd),state.registration_id)}{
                        let _=unsafe{UnregisterHotKey(Some(hwnd),next_id)};return Err(error.into());
                    }
                }
                state.registration_id=next_id;state.registered=true;state.hotkey=prefs.hotkey.clone();
                crate::fast_search::prime_shortcut(&state.profile);
            },
            Err(error)=>{
                let reason=match error.code().0 as u32{
                    0x80070581=>zsdesk_core::i18n::text("已被其他程序占用"),
                    0x800705b3=>zsdesk_core::i18n::text("需要在已登录用户的交互式会话中运行"),
                    _=>zsdesk_core::i18n::text("系统未接受注册"),
                };
                state.error=Some(zsdesk_core::tr_format!("快捷键 {} 未注册（{reason}）：{error}" , prefs.hotkey, reason=reason, error=error));
                if state.registered{
                    state.error.as_mut().unwrap().push_str(zsdesk_core::i18n::text("；原快捷键仍可使用"));
                    if let Err(e)=control::restore_rejected_chord(&state.profile,&prefs.hotkey,&state.hotkey){
                        state.error=Some(format!("{} ({e:#})",state.error.take().unwrap_or_default()));
                    }
                }
                if !state.registered{state.hotkey=prefs.hotkey.clone();}
            },
        }
    }else{if state.registered{unsafe{UnregisterHotKey(Some(hwnd),state.registration_id)}?;state.registered=false;crate::fast_search::release_shortcut_layout(&state.profile);}state.hotkey=prefs.hotkey.clone();}
    control::write_status(&state.profile,state.error.as_deref(),false,&state.hotkey,state.registered);
    Ok(prefs.enabled||super::needs_background(&state.profile))
}
unsafe extern "system" fn proc(hwnd:HWND,message:u32,wp:WPARAM,lp:LPARAM)->LRESULT{
    if message==WM_NCCREATE{
        let create=unsafe{&*(lp.0 as*const CREATESTRUCTW)};unsafe{SetWindowLongPtrW(hwnd,GWLP_USERDATA,create.lpCreateParams as isize)};return LRESULT(1);
    }
    let pointer=unsafe{GetWindowLongPtrW(hwnd,GWLP_USERDATA)}as*mut State;
    if pointer.is_null(){return unsafe{DefWindowProcW(hwnd,message,wp,lp)};}
    if message==WM_HOTKEY&&unsafe{(*pointer).registered&&wp.0==(*pointer).registration_id as usize}{
        if super::shortcut::dialog_active() || crate::utility_panel::hide_search_if_foreground(){return LRESULT(0);}
        let profile=unsafe{&*pointer}.profile.clone();let result=focus_main(&profile,false);
        let state=unsafe{&mut*pointer};state.error=result.err().map(|error|zsdesk_core::tr_format!("无法打开快速搜索：{error:#}", error=error));state.visible=super::popup::is_open();
        control::write_status(&profile,state.error.as_deref(),state.visible,&state.hotkey,state.registered);return LRESULT(0);
    }
    if message==EVENT{
        let profile=unsafe{&*pointer}.profile.clone();let action=control::action_for_index(wp.0);
        match action{
            Some("stop")=>{let _=unsafe{DestroyWindow(hwnd)};},
            Some("reload")=>{
                let result=configure(hwnd,unsafe{&mut*pointer});
                match result{Ok(persistent)=>{
                    unsafe{&mut*pointer}.persistent=persistent;
                    if !persistent&&!super::popup::is_open(){let _=unsafe{DestroyWindow(hwnd)};}
                },Err(error)=>{
                    let state=unsafe{&*pointer};control::write_status(&profile,Some(&error.to_string()),false,&state.hotkey,state.registered);
                }}
            }
            Some("open"|"settings"|"rebuild")=>{
                let result=focus_main(&profile,action!=Some("open"));let state=unsafe{&mut*pointer};
                state.error=result.err().map(|error|zsdesk_core::tr_format!("无法打开搜索窗口：{error:#}" , error=error));state.visible=super::popup::is_open();
                control::write_status(&profile,state.error.as_deref(),state.visible,&state.hotkey,state.registered);
            },_=>{}
        }return LRESULT(0);
    }
    if message==WM_TIMER&&wp.0==LIFETIME_TIMER{
        let state=unsafe{&mut*pointer};let visible=super::popup::is_open();
        if !visible&&!state.start_with_windows&&crate::runtime::stop_unowned_background(){let _=unsafe{DestroyWindow(hwnd)};return LRESULT(0);}
        if state.lifetime_check.elapsed()>=std::time::Duration::from_secs(1){
            state.lifetime_check=std::time::Instant::now();
            state.persistent=control::load(&state.profile).is_ok_and(|configuration|configuration.preferences.enabled)||super::needs_background(&state.profile);
        }
        if state.visible!=visible{state.visible=visible;control::write_status(&state.profile,state.error.as_deref(),visible,&state.hotkey,state.registered);}
        if !unsafe{&*pointer}.persistent&&!super::popup::is_open(){let _=unsafe{DestroyWindow(hwnd)};}
        return LRESULT(0);
    }
    if message==WM_DESTROY{
        let state=unsafe{&*pointer};crate::fast_search::release_shortcut_layout(&state.profile);
        super::popup::close();let _=unsafe{KillTimer(Some(hwnd),LIFETIME_TIMER)};
        let state=unsafe{&mut*pointer};if state.registered{let _=unsafe{UnregisterHotKey(Some(hwnd),state.registration_id)};state.registered=false;}
        control::write_status(&state.profile,state.error.as_deref(),false,&state.hotkey,false);unsafe{PostQuitMessage(0)};return LRESULT(0);
    }
    unsafe{DefWindowProcW(hwnd,message,wp,lp)}
}
pub(super) fn run(profile:PathBuf,action:Action,instance:&control::Instance)->Result<()>{
    unsafe{windows::Win32::System::Ole::OleInitialize(None)}?;
    struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{windows::Win32::System::Ole::OleUninitialize();}}}let _apartment=Apartment;
    let configuration=control::load(&profile)?;
    if action!=Action::Resident{focus_main(&profile,action==Action::Settings)?;}
    // An explicitly opened modeless popup still needs its STA message pump
    // when the user deliberately keeps the global shortcut disabled.
    if action!=Action::Open&&!configuration.preferences.enabled&&!super::needs_background(&profile){return Ok(());}
    let module=HINSTANCE(unsafe{GetModuleHandleW(None)}?.0);let class=w!("ZSDesk.SearchHotkey");
    unsafe{RegisterClassW(&WNDCLASSW{hInstance:module,lpfnWndProc:Some(proc),lpszClassName:class,..Default::default()});}
    let mut state=Box::new(State{profile,registered:false,registration_id:HOTKEY,hotkey:String::new(),error:None,persistent:false,start_with_windows:false,visible:false,lifetime_check:std::time::Instant::now()});
    let hwnd=unsafe{CreateWindowExW(WS_EX_TOOLWINDOW,class,w!("ZSdesk"),WS_POPUP,0,0,0,0,None,None,Some(module),Some((&mut*state as*mut State).cast()))}?;
    struct Window(HWND);impl Drop for Window{fn drop(&mut self){if unsafe{IsWindow(Some(self.0))}.as_bool(){let _=unsafe{DestroyWindow(self.0)};}}}let _window=Window(hwnd);
    state.persistent=configure(hwnd,&mut state)?;let listener=instance.listen_native(hwnd,EVENT)?;
    unsafe{SetTimer(Some(hwnd),LIFETIME_TIMER,250,None);}
    let mut msg=MSG::default();while unsafe{GetMessageW(&mut msg,None,0,0)}.0>0{unsafe{let _=TranslateMessage(&msg);DispatchMessageW(&msg);}}
    drop(listener);zsdesk_windows::shell::file_transfer::persist_thread_clipboard()?;Ok(())
}

#[cfg(test)]mod tests{
    use super::*;
    #[test]fn hotkey_translation_keeps_modifiers_and_reserved_keys(){
        let (mods,key)=chord("Ctrl+Alt+Space").unwrap();assert!(mods.contains(MOD_CONTROL)&&mods.contains(MOD_ALT));assert_eq!(key,VK_SPACE.0 as u32);
        assert!(chord("Ctrl+F12").is_err());assert_eq!(chord("Alt+F10").unwrap().1,VK_F10.0 as u32);
    }
    #[test]
    #[ignore="Native hotkey rollback on an unswitched private desktop; requires ZSDESK_NATIVE_TEST_OUTPUT"]
    fn native_search_hotkey_reload_preserves_previous_registration()->Result<()>{
        use std::{ffi::c_void,fs,thread};
        #[link(name="user32")]unsafe extern "system"{
            fn GetThreadDesktop(thread:u32)->*mut c_void;
            fn CreateDesktopW(name:*const u16,device:*const u16,mode:*const c_void,flags:u32,access:u32,security:*const c_void)->*mut c_void;
            fn SetThreadDesktop(desktop:*mut c_void)->i32;
            fn CloseDesktop(desktop:*mut c_void)->i32;
        }
        let root=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("native output")?).join("hotkey-registration");
        anyhow::ensure!(root.is_absolute(),"absolute fixture required");fs::create_dir_all(&root)?;
        let name:Vec<_>=format!("ZSDesk.Hotkey.{}",std::process::id()).encode_utf16().chain(Some(0)).collect();
        // RegisterHotKey requires WinSta0 (0x800705B3 on a noninteractive
        // window station). Keep this test on an unswitched private desktop
        // with only owned HWNDs and rare chords; never send physical input.
        let original=unsafe{GetThreadDesktop(windows::Win32::System::Threading::GetCurrentThreadId())};
        anyhow::ensure!(!original.is_null()&&unsafe{SetThreadDesktop(original)}!=0,"original desktop unavailable");
        let desktop=unsafe{CreateDesktopW(name.as_ptr(),std::ptr::null(),std::ptr::null(),0,0x000f00ff,std::ptr::null())};
        anyhow::ensure!(!desktop.is_null(),"private desktop unavailable");
        let handle=desktop as usize;let output=root.clone();
        let result=thread::spawn(move||->Result<()>{
            anyhow::ensure!(unsafe{SetThreadDesktop(handle as*mut c_void)}!=0,"private desktop selection failed");
            struct Window(HWND);impl Drop for Window{fn drop(&mut self){for id in [HOTKEY,HOTKEY+1]{let _=unsafe{UnregisterHotKey(Some(self.0),id)};}let _=unsafe{DestroyWindow(self.0)};}}
            let make=||->Result<Window>{Ok(Window(unsafe{CreateWindowExW(WINDOW_EX_STYLE::default(),w!("STATIC"),None,WS_POPUP,0,0,1,1,None,None,None,None)}?))};
            let owner=make()?;let blocker=make()?;let probe=make()?;
            let profile=output.join("explorer-session.json");let config=profile.with_file_name("quick-search.json");
            let write=|enabled:bool,hotkey:&str|->Result<()>{fs::write(&config,serde_json::to_vec(&control::Configuration{preferences:zsdesk_core::model::explorer::QuickSearchPreferences{enabled,hotkey:hotkey.into(),start_with_windows:false},startup_command:None})?)?;Ok(())};
            let mut state=State{profile,registered:false,registration_id:HOTKEY,hotkey:String::new(),error:None,persistent:false,start_with_windows:false,visible:false,lifetime_check:std::time::Instant::now()};
            write(true,"Ctrl+Alt+Shift+F23")?;let initial=configure(owner.0,&mut state)?;
            let (old_mods,old_key)=chord("Ctrl+Alt+Shift+F23")?;
            let initial_status=serde_json::json!({"stage":"initial-registration","hwnd":owner.0.0 as usize,"modifiers":old_mods.0,"vk":old_key,
                "owner_thread":unsafe{GetWindowThreadProcessId(owner.0,None)},"calling_thread":unsafe{windows::Win32::System::Threading::GetCurrentThreadId()},
                "registered":state.registered,"error":state.error,"physical_input_generated":false});
            fs::write(output.join("registration.current.json"),serde_json::to_vec_pretty(&initial_status)?)?;
            anyhow::ensure!(initial&&state.registered,"initial hotkey registration failed: {initial_status}");
            let (blocked_mods,blocked_key)=chord("Ctrl+Alt+Shift+F24")?;unsafe{RegisterHotKey(Some(blocker.0),HOTKEY+1,blocked_mods,blocked_key)}?;
            write(true,"Ctrl+Alt+Shift+F24")?;configure(owner.0,&mut state)?;
            assert!(state.registered&&state.hotkey=="Ctrl+Alt+Shift+F23"&&state.error.is_some());
            assert!(unsafe{RegisterHotKey(Some(probe.0),HOTKEY,old_mods,old_key)}.is_err(),"old key was lost during rejected replacement");
            write(true,"Ctrl+UnknownKey")?;assert!(configure(owner.0,&mut state).is_err());assert!(state.registered);
            write(false,"Ctrl+Alt+Shift+F24")?;configure(owner.0,&mut state)?;assert!(!state.registered);
            unsafe{RegisterHotKey(Some(probe.0),HOTKEY,old_mods,old_key)}?;
            fs::write(output.join("verification.current.json"),serde_json::to_vec_pretty(&serde_json::json!({"state":"passed","private_desktop":true,"desktop_switched":false,"failed_replacement_preserves_old_key":true,"invalid_preferences_preserve_registration":true,"disable_releases_key":true,"physical_input_generated":false}))?)?;
            Ok(())
        }).join().map_err(|_|anyhow::anyhow!("hotkey test panicked")).and_then(|value|value);
        let restored=unsafe{SetThreadDesktop(original)}!=0;
        let deadline=std::time::Instant::now()+std::time::Duration::from_secs(2);
        let closed=loop{if unsafe{CloseDesktop(desktop)}!=0{break true;}if std::time::Instant::now()>=deadline{break false;}std::thread::sleep(std::time::Duration::from_millis(20));};
        result?;anyhow::ensure!(restored&&closed,"private desktop cleanup failed");Ok(())
    }
}
