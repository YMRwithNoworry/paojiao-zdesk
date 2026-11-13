//! Opt-in verification against the actual portable QuickLook executable.
use super::*;
use anyhow::{Context,ensure};
use std::{ffi::c_void,fs,path::PathBuf,ptr,thread,time::{Duration,Instant}};
use windows::{core::{BOOL,PWSTR},Win32::{Foundation::WAIT_TIMEOUT,Security::{TOKEN_ASSIGN_PRIMARY,TOKEN_DUPLICATE,TokenLinkedToken,TOKEN_LINKED_TOKEN,
    CreateRestrictedToken,CreateWellKnownSid,WinBuiltinAdministratorsSid,SID_AND_ATTRIBUTES,PSID,SECURITY_MAX_SID_SIZE,DISABLE_MAX_PRIVILEGE},
    System::{Pipes::{WaitNamedPipeW,GetNamedPipeServerProcessId},
    Threading::{CreateProcessAsUserW,CREATE_NO_WINDOW,STARTUPINFOW,PROCESS_INFORMATION,WaitForSingleObject,TerminateProcess}}}};

#[link(name="user32")]unsafe extern "system"{
    fn CreateDesktopW(name:*const u16,device:*const u16,mode:*const c_void,flags:u32,access:u32,attributes:*const c_void)->*mut c_void;
    fn CloseDesktop(desktop:*mut c_void)->i32;
    fn EnumDesktopWindows(desktop:*mut c_void,callback:Option<unsafe extern "system" fn(HWND,LPARAM)->BOOL>,value:LPARAM)->BOOL;
}
struct Desktop(*mut c_void);
impl Drop for Desktop{fn drop(&mut self){unsafe{CloseDesktop(self.0);}}}
struct Process{handle:HANDLE,pid:u32,thread:u32}
impl Drop for Process{fn drop(&mut self){
    if unsafe{WaitForSingleObject(self.handle,0)}==WAIT_TIMEOUT{
        let _=unsafe{PostThreadMessageW(self.thread,WM_QUIT,WPARAM(0),LPARAM(0))};
        if unsafe{WaitForSingleObject(self.handle,5000)}==WAIT_TIMEOUT{let _=unsafe{TerminateProcess(self.handle,1)};unsafe{WaitForSingleObject(self.handle,5000);}}
    }
    let _=unsafe{CloseHandle(self.handle)};
}}
fn send_owned(pid:u32,bytes:&[u8])->bool{
    let Some(name)=pipe_name()else{return false;};
    let Ok(file)=OpenOptions::new().write(true).custom_flags(FILE_FLAG_OVERLAPPED.0|SECURITY_SQOS_PRESENT.0|SECURITY_IDENTIFICATION.0).open(name)else{return false;};
    let mut actual=0;
    if unsafe{GetNamedPipeServerProcessId(HANDLE(file.as_raw_handle()),&mut actual)}.is_err()||actual!=pid{return false;}
    send_file(&file,bytes)
}
struct Titles{pid:u32,values:Vec<String>}
unsafe extern "system" fn enumerate(window:HWND,lp:LPARAM)->BOOL{
    let titles=unsafe{&mut*(lp.0 as*mut Titles)};let mut pid=0;
    unsafe{GetWindowThreadProcessId(window,Some(&mut pid));}
    if pid==titles.pid&&unsafe{IsWindowVisible(window)}.as_bool(){
        let mut text=[0u16;2048];let length=unsafe{GetWindowTextW(window,&mut text)}.max(0)as usize;
        if length>0{titles.values.push(String::from_utf16_lossy(&text[..length]));}
    }
    true.into()
}
fn titles(desktop:&Desktop,pid:u32)->Vec<String>{
    let mut titles=Titles{pid,values:Vec::new()};
    unsafe{EnumDesktopWindows(desktop.0,Some(enumerate),LPARAM((&mut titles as*mut Titles)as isize));}
    titles.values
}
fn limited_token()->anyhow::Result<Handle>{
    let mut process=HANDLE::default();unsafe{OpenProcessToken(GetCurrentProcess(),TOKEN_QUERY|TOKEN_DUPLICATE|TOKEN_ASSIGN_PRIMARY,&mut process)}?;let process=Handle(process);
    if !unsafe{windows::Win32::UI::Shell::IsUserAnAdmin()}.as_bool(){return Ok(process);}
    let mut linked=TOKEN_LINKED_TOKEN::default();let mut size=0;
    if unsafe{GetTokenInformation(process.0,TokenLinkedToken,Some((&mut linked as*mut TOKEN_LINKED_TOKEN).cast()),std::mem::size_of_val(&linked)as u32,&mut size)}.is_ok(){return Ok(Handle(linked.LinkedToken));}
    let mut bytes=[0u32;(SECURITY_MAX_SID_SIZE as usize).div_ceil(4)];let mut size=std::mem::size_of_val(&bytes)as u32;
    let sid=PSID(bytes.as_mut_ptr().cast());unsafe{CreateWellKnownSid(WinBuiltinAdministratorsSid,None,Some(sid),&mut size)}?;
    let disabled=[SID_AND_ATTRIBUTES{Sid:sid,Attributes:0}];let mut token=HANDLE::default();
    unsafe{CreateRestrictedToken(process.0,DISABLE_MAX_PRIVILEGE,Some(&disabled),None,None,&mut token)}?;
    Ok(Handle(token))
}

#[test]
#[ignore="Actual portable QuickLook on an unswitched desktop; requires ZSDESK_QUICKLOOK_EXE and ZSDESK_NATIVE_TEST_OUTPUT; refuses an existing QuickLook pipe"]
fn native_quicklook_opens_and_closes_actual_preview()->anyhow::Result<()>{
    let executable=PathBuf::from(std::env::var_os("ZSDESK_QUICKLOOK_EXE").context("portable QuickLook required")?);
    ensure!(executable.is_absolute()&&executable.is_file(),"QuickLook executable is invalid");
    let directory=executable.parent().context("portable directory")?;
    ensure!(directory.join("portable.lock").is_file(),"test requires portable mode");
    let output=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("isolated output required")?).join("quicklook-process");
    ensure!(output.is_absolute(),"absolute fixture root required");fs::create_dir_all(&output)?;
    let file=output.join("中文 文件预览.txt");fs::write(&file,"ZSDesk QuickLook end-to-end preview fixture\n")?;
    let name=wide(pipe_name().context("user SID")?);
    ensure!(!unsafe{WaitNamedPipeW(PCWSTR(name.as_ptr()),1)}.as_bool(),"an existing QuickLook instance must be preserved");
    let foreground=unsafe{GetForegroundWindow()};let mut desktop_name=wide(&format!("ZSDesk.QuickLook.{}",std::process::id()));
    let desktop=Desktop(unsafe{CreateDesktopW(desktop_name.as_ptr(),ptr::null(),ptr::null(),0,0x000f00ff,ptr::null())});
    ensure!(!desktop.0.is_null(),"CreateDesktop failed");
    let application=wide(&executable.to_string_lossy());let mut command=wide(&format!("{} /autorun",crate::quick_search::control::quote_argument(&executable.to_string_lossy())));
    let current=wide(&directory.to_string_lossy());
    let startup=STARTUPINFOW{cb:std::mem::size_of::<STARTUPINFOW>()as u32,lpDesktop:PWSTR(desktop_name.as_mut_ptr()),..Default::default()};
    let mut information=PROCESS_INFORMATION::default();
    // Only this owned child receives the linked/restricted token. QuickLook's
    // elevated-only plugin association registration must never run in a test.
    let token=limited_token()?;
    unsafe{CreateProcessAsUserW(Some(token.0),PCWSTR(application.as_ptr()),Some(PWSTR(command.as_mut_ptr())),None,None,false,CREATE_NO_WINDOW,None,PCWSTR(current.as_ptr()),&startup,&mut information)}?;
    let _=unsafe{CloseHandle(information.hThread)};let process=Process{handle:information.hProcess,pid:information.dwProcessId,thread:information.dwThreadId};
    fs::write(output.join("process.current.json"),serde_json::to_vec_pretty(&serde_json::json!({"pid":process.pid,"executable":executable,"purpose":"actual QuickLook private-desktop protocol test"}))?)?;
    let deadline=Instant::now()+Duration::from_secs(20);
    loop{
        ensure!(unsafe{WaitForSingleObject(process.handle,0)}==WAIT_TIMEOUT,"QuickLook exited before its pipe was ready");
        if unsafe{WaitNamedPipeW(PCWSTR(name.as_ptr()),25)}.as_bool(){break;}
        ensure!(Instant::now()<deadline,"QuickLook pipe timed out");thread::sleep(Duration::from_millis(50));
    }
    ensure!(send_owned(process.pid,&toggle_message(&file).unwrap()),"real preview toggle was not delivered");
    let deadline=Instant::now()+Duration::from_secs(15);let filename=file.file_name().unwrap().to_string_lossy();
    let opened=loop{
        let current=titles(&desktop,process.pid);
        if current.iter().any(|title|title.contains(filename.as_ref())){break current;}
        ensure!(Instant::now()<deadline,"QuickLook did not show the requested file; titles={current:?}");thread::sleep(Duration::from_millis(50));
    };
    ensure!(send_owned(process.pid,b"QuickLook.App.PipeMessages.Close|\n"),"preview close was not delivered");
    let deadline=Instant::now()+Duration::from_secs(5);
    while titles(&desktop,process.pid).iter().any(|title|title.contains(filename.as_ref())){
        ensure!(Instant::now()<deadline,"QuickLook preview did not close");thread::sleep(Duration::from_millis(50));
    }
    // QuickLook's pipe Quit stops only the pipe server; it is not its app-exit
    // command. Exit the owned private desktop message loop while that server
    // remains alive for QuickLook's own OnExit disposal handshake.
    unsafe{PostThreadMessageW(process.thread,WM_QUIT,WPARAM(0),LPARAM(0))}?;
    ensure!(unsafe{WaitForSingleObject(process.handle,5000)}!=WAIT_TIMEOUT,"owned QuickLook did not exit");
    let after=unsafe{GetForegroundWindow()};let mut foreground_pid=0;unsafe{GetWindowThreadProcessId(after,Some(&mut foreground_pid));}
    ensure!(foreground_pid!=process.pid,"private preview changed the user's desktop");
    fs::write(output.join("verification.current.json"),serde_json::to_vec_pretty(&serde_json::json!({"state":"passed","pid":process.pid,
        "actual_preview_titles":opened,"unicode_filename":true,"preview_closed":true,"process_exited":true,"foreground_unchanged":foreground==after,
        "desktop_switched":false,"user_input_sent":false}))?)?;
    Ok(())
}
