//! A single UI process per profile; opening another path forwards to its window.
use std::{path::Path, hash::{Hash,Hasher},collections::hash_map::DefaultHasher,time::{Duration,Instant}};
use anyhow::{Result,bail};
use windows::{core::{PCWSTR,w},Win32::{Foundation::{HANDLE,HWND,CloseHandle,GetLastError,ERROR_ALREADY_EXISTS},System::Threading::CreateMutexW,UI::WindowsAndMessaging::*}};
use zsdesk_core::model::explorer::ShellLocation;
use windows::Win32::System::DataExchange::COPYDATASTRUCT;

pub struct Instance(HANDLE);
impl Drop for Instance {fn drop(&mut self){let _=unsafe {CloseHandle(self.0)};}}
pub const OPEN_MAGIC:usize=0x5a534432;
pub const DIAGNOSTICS_MAGIC:usize=0x5a534449;
pub const ACTIVATE_MAGIC:usize=0x5a534441;
pub const FOCUS_SEARCH_MAGIC:usize=0x5a534453;

pub fn focus_search(profile:&Path,settings:bool)->Result<bool>{
    let key=property(profile);let mut previous=None;
    while let Ok(hwnd)=unsafe{FindWindowExW(None,previous,w!("ZSDesk.Native.Explorer"),None)}{
        previous=Some(hwnd);if unsafe{GetPropW(hwnd,PCWSTR(key.as_ptr()))}.0 as usize!=1{continue;}
        let flags=u8::from(settings);let data=COPYDATASTRUCT{dwData:FOCUS_SEARCH_MAGIC,cbData:1,lpData:(&flags as*const u8).cast_mut().cast()};let mut accepted=0;
        let sent=unsafe{SendMessageTimeoutW(hwnd,WM_COPYDATA,windows::Win32::Foundation::WPARAM(0),windows::Win32::Foundation::LPARAM(&data as*const _ as isize),SMTO_ABORTIFHUNG,2500,Some(&mut accepted))};
        if sent.0==0||accepted!=1{bail!("文件窗口暂时无法接收搜索请求");}
        let _=request_activation(hwnd);return Ok(true);
    }Ok(false)
}

fn show_command(visible:bool,iconic:bool,preferred:isize)->SHOW_WINDOW_CMD{
    if iconic {SW_RESTORE}
    else if !visible && preferred==SW_SHOWMAXIMIZED.0 as isize {SW_SHOWMAXIMIZED}
    else if !visible {SW_SHOWNORMAL}
    else {SW_SHOW}
}

/// Called by the receiving window's UI thread after accepting a real open request.
/// Windows remains responsible for deciding whether foreground activation is allowed.
pub unsafe fn activate(hwnd:HWND)->bool{
    let preferred=unsafe {GetPropW(hwnd,w!("ZSDesk.PreferredShow"))}.0 as isize;
    let command=show_command(unsafe {IsWindowVisible(hwnd)}.as_bool(),unsafe {IsIconic(hwnd)}.as_bool(),preferred);
    unsafe {
        let _=ShowWindow(hwnd,command);
        let _=SetForegroundWindow(hwnd);
        IsWindowVisible(hwnd).as_bool() && !IsIconic(hwnd).as_bool()
    }
}

/// Delegate the permission to the actual receiver and let that process activate.
/// Forwarding permission and then activating from the sender would revoke the
/// sender's inherited permission before it could use it.
pub fn request_activation(hwnd:HWND)->bool{
    unsafe {
        if GetPropW(hwnd,w!("ZSDesk.Activate.v1")).0 as usize==1{
            let mut pid=0;GetWindowThreadProcessId(hwnd,Some(&mut pid));
            if pid==0{return false;}
            let _=AllowSetForegroundWindow(pid);
            let data=COPYDATASTRUCT{dwData:ACTIVATE_MAGIC,cbData:0,lpData:std::ptr::null_mut()};
            let mut accepted=0;
            let sent=SendMessageTimeoutW(hwnd,WM_COPYDATA,windows::Win32::Foundation::WPARAM(0),
                windows::Win32::Foundation::LPARAM(&data as *const _ as isize),
                SMTO_ABORTIFHUNG,1500,Some(&mut accepted));
            sent.0!=0 && accepted==1
        }else{
            // Older receivers do not implement the activation message. Keep
            // the sender's foreground permission for that compatibility path.
            let preferred=GetPropW(hwnd,w!("ZSDesk.PreferredShow")).0 as isize;
            let _=ShowWindowAsync(hwnd,show_command(IsWindowVisible(hwnd).as_bool(),IsIconic(hwnd).as_bool(),preferred));
            SetForegroundWindow(hwnd).as_bool()
        }
    }
}
#[derive(serde::Serialize,serde::Deserialize)]
pub struct DiagnosticRequest{pub output:std::path::PathBuf,pub request_id:u64}
pub fn diagnose(profile:&Path,output:&Path)->Result<()>{
    let key=property(profile);let mut previous=None;let mut target=None;
    while let Ok(hwnd)=unsafe{FindWindowExW(None,previous,w!("ZSDesk.Native.Explorer"),None)}{
        previous=Some(hwnd);if unsafe{GetPropW(hwnd,PCWSTR(key.as_ptr()))}.0 as usize==1{target=Some(hwnd);break;}
    }
    let Some(hwnd)=target else{bail!("此配置的文件管理器未运行");};
    let request_id=std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos() as u64 ^ (u64::from(std::process::id())<<32);
    let request=DiagnosticRequest{output:output.to_path_buf(),request_id};let bytes=serde_json::to_vec(&request)?;
    let data=COPYDATASTRUCT{dwData:DIAGNOSTICS_MAGIC,cbData:bytes.len()as u32,lpData:bytes.as_ptr()as *mut _};let mut accepted=0;
    let sent=unsafe{SendMessageTimeoutW(hwnd,WM_COPYDATA,windows::Win32::Foundation::WPARAM(0),windows::Win32::Foundation::LPARAM(&data as *const _ as isize),SMTO_ABORTIFHUNG,2500,Some(&mut accepted))};
    if sent.0==0 || accepted!=1{bail!("文件管理器未接受诊断请求");}
    let deadline=Instant::now()+Duration::from_secs(5);
    while Instant::now()<deadline{
        if let Ok(bytes)=std::fs::read(output){if let Ok(value)=serde_json::from_slice::<serde_json::Value>(&bytes){if value["request_id"].as_u64()==Some(request_id){return Ok(());}}}
        std::thread::sleep(Duration::from_millis(20));
    }
    bail!("诊断报告尚未生成")
}
pub fn property(path:&Path)->Vec<u16>{
    let mut h=DefaultHasher::new();path.to_string_lossy().to_lowercase().hash(&mut h);
    format!("Local\\ZSDesk.Profile.{:016x}",h.finish()).encode_utf16().chain(Some(0)).collect()
}
pub fn enter(path:&Path,target:Option<&ShellLocation>,selection:Option<&std::ffi::OsStr>)->Result<Option<Instance>>{
    let key=property(path);
    let handle=unsafe {CreateMutexW(None,false,PCWSTR(key.as_ptr()))?};
    let existed=unsafe {GetLastError()}==ERROR_ALREADY_EXISTS;
    let guard=Instance(handle);
    if !existed{return Ok(Some(guard));}
    let deadline=Instant::now()+Duration::from_secs(3);
    loop {
        let mut previous=None;
        while let Ok(hwnd)=unsafe {FindWindowExW(None,previous,w!("ZSDesk.Native.Explorer"),None)} {
            previous=Some(hwnd);
            if unsafe {GetPropW(hwnd,PCWSTR(key.as_ptr()))}.0 as usize!=1 {continue;}
            let bytes=serde_json::to_vec(&(target,selection.map(|s|s.to_string_lossy().into_owned())))?;
            let data=COPYDATASTRUCT{dwData:OPEN_MAGIC,cbData:bytes.len() as u32,lpData:bytes.as_ptr() as *mut _};
            let mut accepted=0;
            let sent=unsafe {SendMessageTimeoutW(hwnd,WM_COPYDATA,windows::Win32::Foundation::WPARAM(0),
                windows::Win32::Foundation::LPARAM((&data as *const COPYDATASTRUCT) as isize),
                SMTO_ABORTIFHUNG,2500,Some(&mut accepted))};
            if sent.0==0||accepted!=1{bail!("已打开的 ZSDesk 暂时无法接收请求");}
            // Acceptance is final: an activation failure must not repeat the
            // already accepted folder open through Windows Explorer.
            let _=request_activation(hwnd);
            return Ok(None);
        }
        if Instant::now()>=deadline {bail!("ZSDesk 正在启动，请稍后重试");}
        std::thread::sleep(Duration::from_millis(60));
    }
}
pub struct OpenRequest{pub location:ShellLocation,pub selection:Option<std::ffi::OsString>,pub selections:Vec<std::ffi::OsString>}

#[cfg(test)]mod tests{
    use super::*;
    #[test]fn real_request_restores_hidden_and_saved_maximized_windows(){
        assert_eq!(show_command(false,false,1),SW_SHOWNORMAL);
        assert_eq!(show_command(false,false,3),SW_SHOWMAXIMIZED);
        assert_eq!(show_command(false,true,3),SW_RESTORE);
        assert_eq!(show_command(true,true,1),SW_RESTORE);
        assert_eq!(show_command(true,false,3),SW_SHOW);
    }
}
