//! Native invisible command/hotkey receiver; never creates a search window.
use super::{Action,control};
use std::path::{Path,PathBuf};
use anyhow::{bail,Context,Result};
use windows::{core::w,Win32::{Foundation::{HWND,LPARAM,LRESULT,WPARAM,HINSTANCE},System::LibraryLoader::GetModuleHandleW,
    UI::{Input::KeyboardAndMouse::*,WindowsAndMessaging::*}}};
const EVENT:u32=WM_APP+75;
const HOTKEY:i32=0x5a53;
struct State{profile:PathBuf,registered:bool,hotkey:String,error:Option<String>}

fn chord(text:&str)->Result<(HOT_KEY_MODIFIERS,u32)>{
    let mut modifiers=MOD_NOREPEAT;let mut key=None;
    for token in text.split('+').map(str::trim){
        match token.to_ascii_lowercase().as_str(){
            "ctrl"|"control"|"primary"=>modifiers|=MOD_CONTROL,"alt"=>modifiers|=MOD_ALT,
            "shift"=>modifiers|=MOD_SHIFT,"win"|"windows"|"super"=>modifiers|=MOD_WIN,
            "space"=>key=Some(VK_SPACE.0 as u32),"enter"|"return"=>key=Some(VK_RETURN.0 as u32),
            value if value.len()>1&&value.starts_with('f')=>{let n:u32=value[1..].parse()?;if !(1..=24).contains(&n)||n==12{bail!("功能键无效");}key=Some(VK_F1.0 as u32+n-1);}
            value if value.len()==1&&value.as_bytes()[0].is_ascii_alphanumeric()=>key=Some(value.as_bytes()[0].to_ascii_uppercase()as u32),
            _=>bail!("不支持此快捷键，请选择 Ctrl+Alt+Space 或功能键组合"),
        }
    }
    Ok((modifiers,key.context("缺少快捷键按键")?))
}
fn focus_main(profile:&Path,settings:bool)->Result<()>{
    if crate::instance::focus_search(profile,settings)?{return Ok(());}
    let args=vec![if settings{"--search-settings"}else{"--focus-search"}.into(),"--profile-dir".into(),profile.parent().context("配置目录缺失")?.as_os_str().to_owned()];
    if !crate::runtime::enqueue_local(profile,args.clone())?{
        use std::os::windows::process::CommandExt;
        std::process::Command::new(std::env::current_exe()?).args(args).creation_flags(0x08000000).spawn()?;
    }
    Ok(())
}
fn configure(hwnd:HWND,state:&mut State)->Result<bool>{
    if state.registered{let _=unsafe{UnregisterHotKey(Some(hwnd),HOTKEY)};state.registered=false;}
    let prefs=control::load(&state.profile)?.preferences;state.hotkey=prefs.hotkey.clone();state.error=None;
    if prefs.enabled{
        match chord(&prefs.hotkey).and_then(|(mods,key)|unsafe{RegisterHotKey(Some(hwnd),HOTKEY,mods,key)}.map_err(Into::into)){
            Ok(())=>state.registered=true,Err(error)=>state.error=Some(format!("快捷键未注册：{error:#}")),
        }
    }
    control::write_status(&state.profile,state.error.as_deref(),false,&state.hotkey,state.registered);
    Ok(prefs.enabled||crate::explorer_forwarder::owns_integration(&state.profile))
}
unsafe extern "system" fn proc(hwnd:HWND,message:u32,wp:WPARAM,lp:LPARAM)->LRESULT{
    if message==WM_NCCREATE{
        let create=unsafe{&*(lp.0 as*const CREATESTRUCTW)};unsafe{SetWindowLongPtrW(hwnd,GWLP_USERDATA,create.lpCreateParams as isize)};return LRESULT(1);
    }
    let pointer=unsafe{GetWindowLongPtrW(hwnd,GWLP_USERDATA)}as*mut State;
    if pointer.is_null(){return unsafe{DefWindowProcW(hwnd,message,wp,lp)};}
    if message==WM_HOTKEY&&wp.0==HOTKEY as usize{
        let profile=unsafe{&*pointer}.profile.clone();if let Err(error)=focus_main(&profile,false){
            let state=unsafe{&*pointer};control::write_status(&profile,Some(&error.to_string()),false,&state.hotkey,state.registered);
        }return LRESULT(0);
    }
    if message==EVENT{
        let profile=unsafe{&*pointer}.profile.clone();let action=control::action_for_index(wp.0);
        match action{
            Some("stop")=>{let _=unsafe{DestroyWindow(hwnd)};},
            Some("reload")=>{
                let result=configure(hwnd,unsafe{&mut*pointer});
                match result{Ok(false)=>{let _=unsafe{DestroyWindow(hwnd)};},Err(error)=>{
                    let state=unsafe{&*pointer};control::write_status(&profile,Some(&error.to_string()),false,&state.hotkey,false);
                },_=>{}}
            }
            Some("open"|"settings"|"rebuild")=>{let _=focus_main(&profile,action!=Some("open"));},_=>{}
        }return LRESULT(0);
    }
    if message==WM_DESTROY{
        let state=unsafe{&mut*pointer};if state.registered{let _=unsafe{UnregisterHotKey(Some(hwnd),HOTKEY)};state.registered=false;}
        control::write_status(&state.profile,state.error.as_deref(),false,&state.hotkey,false);unsafe{PostQuitMessage(0)};return LRESULT(0);
    }
    unsafe{DefWindowProcW(hwnd,message,wp,lp)}
}
pub(super) fn run(profile:PathBuf,action:Action,instance:&control::Instance)->Result<()>{
    let configuration=control::load(&profile)?;
    if action!=Action::Resident{focus_main(&profile,action==Action::Settings)?;}
    if !configuration.preferences.enabled&&!crate::explorer_forwarder::owns_integration(&profile){return Ok(());}
    let module=HINSTANCE(unsafe{GetModuleHandleW(None)}?.0);let class=w!("ZSDesk.SearchHotkey");
    unsafe{RegisterClassW(&WNDCLASSW{hInstance:module,lpfnWndProc:Some(proc),lpszClassName:class,..Default::default()});}
    let mut state=Box::new(State{profile,registered:false,hotkey:String::new(),error:None});
    let hwnd=unsafe{CreateWindowExW(WS_EX_TOOLWINDOW,class,w!("ZSdesk"),WS_POPUP,0,0,0,0,None,None,Some(module),Some((&mut*state as*mut State).cast()))}?;
    struct Window(HWND);impl Drop for Window{fn drop(&mut self){if unsafe{IsWindow(Some(self.0))}.as_bool(){let _=unsafe{DestroyWindow(self.0)};}}}let _window=Window(hwnd);
    configure(hwnd,&mut state)?;let listener=instance.listen_native(hwnd,EVENT)?;
    let mut msg=MSG::default();while unsafe{GetMessageW(&mut msg,None,0,0)}.0>0{unsafe{let _=TranslateMessage(&msg);DispatchMessageW(&msg);}}
    drop(listener);Ok(())
}

#[cfg(test)]mod tests{
    use super::*;
    #[test]fn hotkey_translation_keeps_modifiers_and_reserved_keys(){
        let (mods,key)=chord("Ctrl+Alt+Space").unwrap();assert!(mods.contains(MOD_CONTROL)&&mods.contains(MOD_ALT));assert_eq!(key,VK_SPACE.0 as u32);
        assert!(chord("Ctrl+F12").is_err());assert_eq!(chord("Alt+F10").unwrap().1,VK_F10.0 as u32);
    }
}
