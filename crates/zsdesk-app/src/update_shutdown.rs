//! Cooperative shutdown of one installation before its files are replaced.
mod agent;
use std::{path::Path,time::{Duration,Instant}};
use windows::{core::{w,BOOL},Win32::{Foundation::{CloseHandle,HANDLE,HWND,LPARAM,WPARAM},
    System::Threading::{OpenProcess,QueryFullProcessImageNameW,PROCESS_NAME_WIN32,PROCESS_QUERY_LIMITED_INFORMATION},
    UI::WindowsAndMessaging::*}};

pub fn message()->u32{unsafe{RegisterWindowMessageW(w!("ZSDesk.ShutdownForUpdate.v1"))}}
fn closable_class(class:&str)->bool{
    matches!(class,"ZSDesk.Native.Explorer"|"ZSDesk.Native.DesktopHost"|"ZSDesk.SearchHotkey"|"ZSDesk.UtilityPanel"
        |"ZSDesk.FolderOccupancy"|"ZSDesk.Diagnostics.Window"|"ZSDesk.QuickPreview"|"ZsuiMainWindow")
}
pub fn executable(pid:u32)->Option<std::path::PathBuf>{
    let handle=unsafe{OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION,false,pid)}.ok()?;
    struct Owned(HANDLE);impl Drop for Owned{fn drop(&mut self){let _=unsafe{CloseHandle(self.0)};}}let _owned=Owned(handle);
    let mut text=vec![0u16;32768];let mut length=text.len()as u32;
    unsafe{QueryFullProcessImageNameW(handle,PROCESS_NAME_WIN32,windows::core::PWSTR(text.as_mut_ptr()),&mut length)}.ok()?;
    Some(String::from_utf16_lossy(&text[..length as usize]).into())
}
pub fn windows_for_executable(exe:&Path)->Vec<HWND>{
    struct State<'a>{exe:&'a Path,windows:Vec<HWND>}
    unsafe extern "system" fn collect(hwnd:HWND,lp:LPARAM)->BOOL{
        let state=unsafe{&mut*(lp.0 as*mut State<'_>)};let mut pid=0;unsafe{GetWindowThreadProcessId(hwnd,Some(&mut pid));}
        if executable(pid).is_some_and(|path|crate::explorer_forwarder::same_path(&path,state.exe)){state.windows.push(hwnd);}BOOL(1)
    }
    let mut state=State{exe,windows:Vec::new()};let _=unsafe{EnumWindows(Some(collect),LPARAM(&mut state as*mut _ as isize))};state.windows
}
pub fn request_windows(exe:&Path){
    for window in windows_for_executable(exe){
        let mut class=[0u16;128];let count=unsafe{GetClassNameW(window,&mut class)}.max(0)as usize;
        let class=String::from_utf16_lossy(&class[..count]);
        let event=if class=="ZSDesk.Native.Runtime"{message()}else if closable_class(&class){WM_CLOSE}else{continue;};
        let _=unsafe{PostMessageW(Some(window),event,WPARAM(0),LPARAM(0))};
    }
}
#[cfg(test)]mod tests{
    use super::*;
    #[test]fn update_closes_product_windows_without_closing_extension_operation_hosts(){
        assert!(closable_class("ZSDesk.Native.Explorer"));assert!(closable_class("ZSDesk.SearchHotkey"));
        for class in ["ZSDesk.ExtensionMenu.Worker","ZSDesk.DesktopReturnDrop","#32770","ZSDesk.UnknownProvider","CabinetWClass"]{assert!(!closable_class(class));}
    }
}
pub fn run_if_requested()->Option<anyhow::Result<()>>{
    let args=std::env::args_os().skip(1).collect::<Vec<_>>();
    if args.first().is_none_or(|arg|arg!="--shutdown-for-update"){return None;}
    Some((||{
        anyhow::ensure!(args.len()==2,"需要指定待更新程序的完整路径");let exe=Path::new(&args[1]);
        anyhow::ensure!(exe.is_absolute()&&exe.file_name().is_some_and(|name|name.eq_ignore_ascii_case("ZSDesk.exe")),"更新程序路径无效");
        request_windows(exe);
        agent::request_agent_shutdown(exe);
        let deadline=Instant::now()+Duration::from_secs(25);
        loop{
            // Opening for exclusive write proves that main and helper images
            // have released the actual installed executable. No process is killed.
            if std::fs::OpenOptions::new().write(true).open(exe).is_ok(){return Ok(());}
            anyhow::ensure!(Instant::now()<deadline,"程序仍有文件操作或窗口未结束，请完成后重试更新");
            std::thread::sleep(Duration::from_millis(100));
        }
    })())
}
