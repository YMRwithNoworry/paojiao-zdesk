use super::*;
use super::isolation_tests::Environment;
use std::{ffi::c_void,os::windows::io::AsRawHandle,thread};
use windows::core::BOOL;
#[link(name="user32")]unsafe extern "system"{
    fn CreateDesktopW(name:*const u16,device:*const u16,mode:*const c_void,flags:u32,access:u32,security:*const c_void)->*mut c_void;
    fn SetThreadDesktop(desktop:*mut c_void)->i32;
    fn CloseDesktop(desktop:*mut c_void)->i32;
}
fn wait_until(label:&str,mut check:impl FnMut()->Result<bool>)->Result<()>{
    let deadline=Instant::now()+Duration::from_secs(25);
    while !check()?{ensure!(Instant::now()<deadline,"{label}");thread::sleep(Duration::from_millis(20));}Ok(())
}
fn windows(pid:u32,class:&str)->Vec<HWND>{
    struct Query<'a>{pid:u32,class:&'a str,found:Vec<HWND>}
    unsafe extern "system" fn collect(hwnd:HWND,data:LPARAM)->BOOL{
        let query=unsafe{&mut*(data.0 as*mut Query<'_>)};let mut pid=0;unsafe{GetWindowThreadProcessId(hwnd,Some(&mut pid));}
        let mut class=[0u16;128];let size=unsafe{GetClassNameW(hwnd,&mut class)}.max(0)as usize;
        if pid==query.pid&&String::from_utf16_lossy(&class[..size])==query.class{query.found.push(hwnd);}BOOL(1)
    }
    let mut query=Query{pid,class,found:Vec::new()};let _=unsafe{EnumWindows(Some(collect),LPARAM(&mut query as*mut _ as isize))};query.found
}
fn child_view(owner:HWND)->Option<HWND>{
    unsafe extern "system" fn collect(hwnd:HWND,data:LPARAM)->BOOL{
        let mut class=[0u16;128];let size=unsafe{GetClassNameW(hwnd,&mut class)}.max(0)as usize;
        if String::from_utf16_lossy(&class[..size])=="SHELLDLL_DefView"{unsafe{*(data.0 as*mut Option<HWND>)=Some(hwnd);}return BOOL(0);}BOOL(1)
    }
    let mut result=None;let _=unsafe{EnumChildWindows(Some(owner),Some(collect),LPARAM(&mut result as*mut _ as isize))};result
}
fn status(profile:&Path)->serde_json::Value{
    fs::read(profile.join("runtime-status.json")).ok().and_then(|bytes|serde_json::from_slice(&bytes).ok()).unwrap_or(serde_json::Value::Null)
}
fn exercise(root:&Path)->Result<serde_json::Value>{
    let profile=root.join("profile");let files=root.join("files");fs::create_dir_all(&profile)?;fs::create_dir_all(&files)?;
    fs::write(files.join("fixture.txt"),b"runtime retirement fixture")?;
    let mut session=crate::storage::default_session();session.ui.show_preview=false;session.ui.hover_preview=false;
    crate::storage::save(&profile.join("explorer-session.json"),&session)?;
    let _workspace=Environment::set("ZSDESK_RUNTIME_TEST_ROOT",root);
    let _probe=Environment::set("ZSDESK_MENU_TEST_PROBE","retirement-dialog");
    let _menu_root=Environment::set("ZSDESK_MENU_ROOT",root.join("workers"));
    let _trace=Environment::set("ZSDESK_RUNTIME_TRACE",root.join("runtime.current.log"));
    let stderr=fs::File::create(root.join("host.current.stderr.log"))?;
    let child=child::spawn(&std::env::current_exe()?,&["context_menu_process::runtime_tests::native_runtime_host_entry".into(),"--exact".into(),"--ignored".into(),"--nocapture".into()],root,"a0123456789abcdef0123456789abcdef",&stderr)?;
    struct Host{child:Child,job:HANDLE}
    impl Drop for Host{fn drop(&mut self){if !self.job.0.is_null(){let _=unsafe{CloseHandle(self.job)};}if self.child.try_wait().ok().flatten().is_none(){let _=self.child.kill();let _=self.child.wait();}}}
    let mut host=Host{child,job:HANDLE::default()};
    host.job=unsafe{CreateJobObjectW(None,None)}?;let mut limits=JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();limits.BasicLimitInformation.LimitFlags=JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    unsafe{SetInformationJobObject(host.job,JobObjectExtendedLimitInformation,(&limits as*const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),std::mem::size_of_val(&limits)as u32)?;AssignProcessToJobObject(host.job,HANDLE(host.child.as_raw_handle()))?;}
    let pid=host.child.id();
    wait_until("initial file window did not open",||Ok(windows(pid,"ZSDesk.Native.Explorer").len()==1))?;
    let first=windows(pid,"ZSDesk.Native.Explorer")[0];
    wait_until("initial shell view did not load",||Ok(child_view(first).is_some()))?;thread::sleep(Duration::from_millis(150));
    let view=child_view(first).context("file view")?;
    unsafe{PostMessageW(Some(view),WM_CONTEXTMENU,WPARAM(view.0 as usize),LPARAM(-1))?;}
    let mut extension_pid=0;
    wait_until("retirement fixture did not become visible",||{
        let diagnostic=fs::read(root.join("workers/menu-diagnostics.current.json")).ok().and_then(|b|serde_json::from_slice::<serde_json::Value>(&b).ok()).unwrap_or_default();
        extension_pid=diagnostic["worker_pid"].as_u64().unwrap_or(0)as u32;
        Ok(extension_pid!=0&&!windows(extension_pid,"ZSDesk.Test.PendingExtension").is_empty())
    })?;
    unsafe{PostMessageW(Some(first),WM_CLOSE,WPARAM(0),LPARAM(0))?;}
    wait_until("host never reached retirement with no file windows",||{let value=status(&profile);Ok(windows(pid,"ZSDesk.Native.Explorer").is_empty()&&value["pending_extension_workers"].as_u64().unwrap_or(0)>0&&value["components"].as_array().is_some_and(|items|items.is_empty()))})?;
    let before=status(&profile);
    crate::runtime::run(profile.join("explorer-session.json"),vec!["--new-window".into(),"--profile-dir".into(),profile.as_os_str().to_owned(),files.as_os_str().to_owned()])?;
    wait_until("new request was rejected during retirement",||Ok(windows(pid,"ZSDesk.Native.Explorer").len()==1))?;
    let second=windows(pid,"ZSDesk.Native.Explorer")[0];let reopened=status(&profile);
    ensure!(host.child.try_wait()?.is_none()&&!windows(extension_pid,"ZSDesk.Test.PendingExtension").is_empty(),"old extension was killed to permit reopening");
    for popup in windows(extension_pid,"ZSDesk.Test.PendingExtension"){unsafe{PostMessageW(Some(popup),WM_CLOSE,WPARAM(0),LPARAM(0))?;}}
    wait_until("extension retirement did not finish",||Ok(status(&profile)["pending_extension_workers"].as_u64()==Some(0)))?;
    ensure!(unsafe{IsWindow(Some(second))}.as_bool(),"retiring extension closed new window");
    unsafe{PostMessageW(Some(second),WM_CLOSE,WPARAM(0),LPARAM(0))?;}
    wait_until("host did not exit after final window and extension closed",||Ok(host.child.try_wait()?.is_some()))?;
    ensure!(host.child.wait()?.success(),"host failed after retirement");
    Ok(serde_json::json!({"host_pid":pid,"extension_pid":extension_pid,"before_reopen":before,"after_reopen":reopened,"same_host_reaccepted":true,"old_extension_survived_reopen":true,"new_window_survived_extension_exit":true,"host_exited_cleanly":true,"fixture":"controlled modeless extension dialog; no third-party command executed"}))
}
#[test]#[ignore="Internal isolated runtime entry"]
fn native_runtime_host_entry()->Result<()>{
    let root=PathBuf::from(std::env::var_os("ZSDESK_RUNTIME_TEST_ROOT").context("test root")?);ensure!(root.is_absolute(),"absolute test root required");
    let profile=root.join("profile");let files=root.join("files");
    crate::runtime::run(profile.join("explorer-session.json"),vec!["--new-window".into(),"--profile-dir".into(),profile.into_os_string(),files.into_os_string()])
}
#[test]#[ignore="Actual runtime close/reopen while an owned extension retires, on an unswitched private desktop"]
fn native_runtime_reopens_while_extension_retires()->Result<()>{
    let root=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("output")?).join("runtime-retirement");ensure!(root.is_absolute(),"absolute output required");fs::create_dir_all(&root)?;
    let station_name=super::isolation_tests::current_station_name()?;let name=format!("ZSDesk.RuntimeRetirement.{}",std::process::id());
    let _station=Environment::set("ZSDESK_MENU_TEST_STATION",station_name);
    let _desktop=Environment::set("ZSDESK_MENU_TEST_DESKTOP",&name);
    let foreground=unsafe{GetForegroundWindow()};
    let desktop=unsafe{CreateDesktopW(wide(&name).as_ptr(),std::ptr::null(),std::ptr::null(),0,0x000f00ff,std::ptr::null())};
    let raw=desktop as usize;let destination=root.clone();
    let result=if desktop.is_null(){Err(anyhow::anyhow!("CreateDesktop failed"))}else{thread::spawn(move||{ensure!(unsafe{SetThreadDesktop(raw as*mut c_void)}!=0,"SetThreadDesktop failed");exercise(&destination)}).join().map_err(|_|anyhow::anyhow!("runtime test panicked")).and_then(|v|v)};
    let closed=desktop.is_null()||unsafe{CloseDesktop(desktop)}!=0;
    let report=serde_json::json!({"state":if result.is_ok()&&closed{"passed"}else{"failed"},"checks":result.as_ref().map_err(|e|format!("{e:#}")),"desktop_closed":closed,"foreground_unchanged":foreground==unsafe{GetForegroundWindow()},"user_profile_accessed":false,"clipboard_commands_executed":false});
    fs::write(root.join("verification.current.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");result?;ensure!(closed,"private desktop cleanup failed");Ok(())
}
