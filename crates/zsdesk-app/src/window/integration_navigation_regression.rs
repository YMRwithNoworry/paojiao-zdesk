//! Side-button navigation and Listary's read-only folder contract on an unswitched desktop.
use super::*;
use anyhow::ensure;
use std::{ffi::c_void, fs, path::Path, ptr, thread, time::{Duration, Instant}};
use windows::Win32::System::SystemServices::{APPCOMMAND_BROWSER_BACKWARD, APPCOMMAND_BROWSER_FORWARD};

#[link(name="user32")] unsafe extern "system" {
    fn CreateDesktopW(name:*const u16,device:*const u16,mode:*const c_void,flags:u32,access:u32,attributes:*const c_void)->*mut c_void;
    fn SetThreadDesktop(desktop:*mut c_void)->i32;
    fn CloseDesktop(desktop:*mut c_void)->i32;
}
struct Apartment;
impl Drop for Apartment { fn drop(&mut self) { unsafe { OleUninitialize(); } } }
struct Runner { hwnd: HWND, slot: Box<Slot> }
impl Drop for Runner { fn drop(&mut self) { let _ = unsafe { DestroyWindow(self.hwnd) }; } }

fn pump(owner: HWND, milliseconds:u64) {
    let end=Instant::now()+Duration::from_millis(milliseconds);
    while Instant::now()<end {
        for _ in 0..256 {
            let mut message=MSG::default();
            if !unsafe { PeekMessageW(&mut message,None,0,0,PM_REMOVE) }.as_bool() { break; }
            if message.message==WM_QUIT || mouse_navigation::pre_translate(owner,&message) { continue; }
            unsafe { let _=TranslateMessage(&message);DispatchMessageW(&message); }
        }
        thread::sleep(Duration::from_millis(2));
    }
}
fn settled(runner:&Runner)->Result<()> {
    let end=Instant::now()+Duration::from_secs(12);
    loop {
        pump(runner.hwnd,25);
        let state=runner.slot.borrow();let state=state.as_ref().context("window closed")?;
        if state.pending.is_none() && state.queued.is_none() && state.shown_tab==Some(state.session.active_tab().id) { return Ok(()); }
        ensure!(Instant::now()<end,"navigation timed out");
    }
}
fn create(output:&Path)->Result<Runner> {
    unsafe { InitCommonControlsEx(&INITCOMMONCONTROLSEX { dwSize:size_of::<INITCOMMONCONTROLSEX>()as u32,dwICC:ICC_STANDARD_CLASSES|ICC_BAR_CLASSES }) }.ok()?;
    let folder=output.join("第一层 中文");fs::create_dir_all(&folder)?;
    let mut session=storage::default_session();session.tabs=vec![ExplorerTab::new(1,ShellLocation::Path(folder))];session.active_tab=0;session.next_id=2;
    session.ui.show_preview=false;
    let slot=Box::new(RefCell::new(Some(WindowState::new(session,output.join("profile/explorer-session.json"),None,false))));
    let instance=HINSTANCE(unsafe { GetModuleHandleW(None)? }.0);
    let class=windows::core::w!("ZSDesk.Native.Explorer");
    unsafe { RegisterClassW(&WNDCLASSW { hInstance:instance,lpfnWndProc:Some(window_proc),lpszClassName:class,..Default::default() }); }
    let hwnd=unsafe { CreateWindowExW(WS_EX_NOACTIVATE,class,windows::core::w!("Navigation integration regression"),
        WS_OVERLAPPEDWINDOW|WS_CLIPCHILDREN,100,100,1040,720,None,None,Some(instance),Some((&*slot as *const Slot).cast()))? };
    let runner=Runner { hwnd,slot };
    unsafe { runner.slot.borrow_mut().as_mut().unwrap().create(hwnd)?;let _=ShowWindow(hwnd,SW_SHOWNOACTIVATE); }
    settled(&runner)?;Ok(runner)
}
fn path(runner:&Runner)->ShellLocation { runner.slot.borrow().as_ref().unwrap().current_location().clone() }
fn view(runner:&Runner)->Result<HWND> { runner.slot.borrow().as_ref().unwrap().browser.as_ref().and_then(|browser|browser.view_window()).context("native view") }
fn folder_text(owner:HWND)->Result<String> {
    let endpoint=unsafe { FindWindowExW(Some(owner),None,listary::CLASS,None)? };
    let mut buffer=vec![0u16;32768];let mut copied=0;
    ensure!(unsafe { SendMessageTimeoutW(endpoint,WM_GETTEXT,WPARAM(buffer.len()),LPARAM(buffer.as_mut_ptr()as isize),SMTO_ABORTIFHUNG,500,Some(&mut copied)) }.0!=0,"folder endpoint timeout");
    ensure!(copied<buffer.len(),"folder endpoint length invalid");Ok(String::from_utf16_lossy(&buffer[..copied]))
}
fn go(runner:&Runner,destination:&Path)->Result<()> {
    unsafe { runner.slot.borrow_mut().as_mut().unwrap().navigate_to(ShellLocation::Path(destination.into())); }
    ensure!(folder_text(runner.hwnd)?.is_empty(),"pending navigation exposed an old directory");
    settled(runner)?;Ok(())
}
fn pointer(owner:HWND,target:HWND,button:u16) {
    for message in [WM_XBUTTONDOWN,WM_XBUTTONUP] {
        let value=MSG { hwnd:target,message,wParam:WPARAM((button as usize)<<16),lParam:LPARAM(0),..Default::default() };
        assert!(mouse_navigation::pre_translate(owner,&value),"side button was not consumed");
    }
}
fn app(owner:HWND,source:HWND,back:bool) {
    let code=if back { APPCOMMAND_BROWSER_BACKWARD.0 } else { APPCOMMAND_BROWSER_FORWARD.0 };
    let handled=unsafe { SendMessageW(owner,WM_APPCOMMAND,Some(WPARAM(source.0 as usize)),Some(LPARAM(((code|0x8000)<<16)as isize))) };
    assert_eq!(handled.0,1,"synchronous app command was not consumed");
}

fn run(output:&Path)->Result<serde_json::Value> {
    unsafe { OleInitialize(None)?; }let _apartment=Apartment;
    let runner=create(output)?;
    let first=output.join("第一层 中文");let second=output.join("second folder");let third=output.join("third");
    for directory in [&second,&third] { fs::create_dir_all(directory)?; }
    ensure!(folder_text(runner.hwnd)?==first.to_string_lossy(),"initial folder endpoint mismatch");
    go(&runner,&second)?;go(&runner,&third)?;
    let native_view=view(&runner)?;
    pointer(runner.hwnd,native_view,XBUTTON1);
    // Simulate a driver also emitting WM_APPCOMMAND for the same physical input.
    app(runner.hwnd,native_view,true);settled(&runner)?;
    ensure!(path(&runner)==ShellLocation::Path(second.clone()),"duplicate forms navigated twice");
    ensure!(folder_text(runner.hwnd)?==second.to_string_lossy(),"back navigation left a stale folder endpoint");
    pointer(runner.hwnd,view(&runner)?,XBUTTON2);settled(&runner)?;
    ensure!(path(&runner)==ShellLocation::Path(third.clone()),"forward side button failed");
    thread::sleep(Duration::from_millis(160));
    app(runner.hwnd,view(&runner)?,true);settled(&runner)?;
    ensure!(path(&runner)==ShellLocation::Path(second.clone()),"driver-only back command failed");
    thread::sleep(Duration::from_millis(160));
    app(runner.hwnd,view(&runner)?,false);settled(&runner)?;
    ensure!(path(&runner)==ShellLocation::Path(third.clone()),"driver-only forward command failed");

    let edit=runner.slot.borrow().as_ref().unwrap().address_edit;
    unsafe { runner.slot.borrow_mut().as_mut().unwrap().focus_address(); }
    pump(runner.hwnd,20);
    let before=navigation::window_text(edit);
    pointer(runner.hwnd,view(&runner)?,XBUTTON1);app(runner.hwnd,edit,true);pump(runner.hwnd,40);
    ensure!(path(&runner)==ShellLocation::Path(third.clone()) && navigation::window_text(edit)==before,"side button changed the folder or address text while editing");
    let rename=unsafe { child(runner.hwnd,"EDIT","unchanged filename",0x52fe,ES_AUTOHSCROLL as u32,true)? };
    unsafe { let _=SetFocus(Some(rename)); }
    pointer(runner.hwnd,view(&runner)?,XBUTTON1);app(runner.hwnd,rename,true);pump(runner.hwnd,25);
    ensure!(path(&runner)==ShellLocation::Path(third.clone()) && navigation::window_text(rename)=="unchanged filename","rename text input was navigated away");
    let _=unsafe { DestroyWindow(rename) };
    runner.slot.borrow().as_ref().unwrap().focus_view();pump(runner.hwnd,20);
    // Root-relative Chinese paths used to panic before Shell saw the address.
    for name in ["中文目录","施工记录😀","é中混合"]{
        let target=output.join(name);std::fs::create_dir_all(&target)?;
        unsafe{runner.slot.borrow_mut().as_mut().unwrap().focus_address();SetWindowTextW(edit,PCWSTR(wide(&target.to_string_lossy()).as_ptr()))?;}
        address_input::handle(edit,WM_KEYDOWN,WPARAM(VK_RETURN.0 as usize));
        address_input::handle(edit,WM_KEYUP,WPARAM(VK_RETURN.0 as usize));settled(&runner)?;
        ensure!(path(&runner)==ShellLocation::Path(target),"Unicode Return did not navigate");
        ensure!(unsafe{IsWindow(Some(runner.hwnd))}.as_bool(),"Unicode address destroyed the file window");
    }
    go(&runner,&third)?;

    let endpoint=unsafe { FindWindowExW(Some(runner.hwnd),None,listary::CLASS,None)? };
    let forged=wide(r"E:\not-the-current-folder");
    let changed=unsafe { SendMessageW(endpoint,WM_SETTEXT,None,Some(LPARAM(forged.as_ptr()as isize))) };
    ensure!(changed.0==0 && folder_text(runner.hwnd)?==third.to_string_lossy(),"read-only folder endpoint accepted external text");
    let mut probe_state=serde_json::Value::String("not supplied; native endpoint only".into());
    if let Some(executable)=std::env::var_os("ZSDESK_LISTARY_TEST_PROBE") {
        use std::os::windows::process::CommandExt;
        use windows::Win32::{Foundation::{CloseHandle,HANDLE,WAIT_OBJECT_0},System::Threading::{CreateEventW,SetEvent,WaitForSingleObject}};
        struct Event(HANDLE);impl Drop for Event{fn drop(&mut self){let _=unsafe{CloseHandle(self.0)};}}
        struct ProbeProcess(std::process::Child);
        impl std::ops::Deref for ProbeProcess{type Target=std::process::Child;fn deref(&self)->&Self::Target{&self.0}}
        impl std::ops::DerefMut for ProbeProcess{fn deref_mut(&mut self)->&mut Self::Target{&mut self.0}}
        impl Drop for ProbeProcess{fn drop(&mut self){if !matches!(self.0.try_wait(),Ok(Some(_))){let _=self.0.kill();let _=self.0.wait();}}}
        let ready_name=format!("Local\\ZSDesk.ListaryProbe.{}.{}.Ready",std::process::id(),runner.hwnd.0 as usize);
        let run_name=format!("Local\\ZSDesk.ListaryProbe.{}.{}.Run",std::process::id(),runner.hwnd.0 as usize);
        let ready=Event(unsafe{CreateEventW(None,true,false,PCWSTR(wide(&ready_name).as_ptr()))}?);
        let run=Event(unsafe{CreateEventW(None,true,false,PCWSTR(wide(&run_name).as_ptr()))}?);
        let executable=PathBuf::from(executable);ensure!(executable.is_absolute(),"plugin probe must be absolute");
        let mut child=ProbeProcess(std::process::Command::new(executable).arg((runner.hwnd.0 as usize).to_string()).arg(&third)
            .env("ZSDESK_LISTARY_TEST_DESKTOP",format!("ZSDesk.IntegrationNavigation.{}",std::process::id()))
            .env("ZSDESK_LISTARY_TEST_READY",&ready_name).env("ZSDESK_LISTARY_TEST_RUN",&run_name)
            .creation_flags(0x08000000).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn()?);
        let started=Instant::now();let pid=child.id();
        loop {
            if unsafe{WaitForSingleObject(ready.0,0)}==WAIT_OBJECT_0{break;}
            if let Some(status)=child.try_wait()?{anyhow::bail!("plugin probe exited before ready: {status}");}
            if started.elapsed()>Duration::from_secs(10){let _=child.kill();let _=child.wait();anyhow::bail!("plugin probe readiness timed out");}
            pump(runner.hwnd,20);
        }
        let paused=Instant::now();unsafe{SetEvent(run.0)}?;
        // Deliberately do not pump or dispatch sent messages on the target STA.
        // A synchronous WM_GETTEXT implementation cannot complete this check.
        let status=loop {
            if let Some(status)=child.try_wait()?{break status;}
            if paused.elapsed()>Duration::from_millis(1200){let _=child.kill();let _=child.wait();anyhow::bail!("folder read blocked on a paused target UI thread");}
            thread::sleep(Duration::from_millis(2));
        };
        ensure!(status.success(),"official API plugin probe returned {status}");
        probe_state=serde_json::json!({"passed":true,"pid":pid,"exit":status.code(),"method":"IFileAppPlugin/IFileWindow/IGetFolder with cross-process cached caption",
            "target_pump_paused":true,"paused_read_milliseconds":paused.elapsed().as_millis(),"deadline_milliseconds":1200});
    }
    {
        let mut state=runner.slot.borrow_mut();let state=state.as_mut().unwrap();
        let mut other=state.session.active_tab().clone();other.id=2;state.session.tabs.push(other);state.session.active_tab=1;
        state.publish_listary_folder();ensure!(folder_text(runner.hwnd)?.is_empty(),"newly selected tab exposed the old tab directory");
        state.session.active_tab=0;state.publish_listary_folder();
        state.session.active_tab_mut().replace_current(ShellLocation::ThisPc);state.publish_listary_folder();
        ensure!(folder_text(runner.hwnd)?.is_empty(),"virtual folder exposed an unrelated filesystem path");
        state.session.active_tab_mut().replace_current(ShellLocation::Search{query:"test".into(),scope:Some(third.clone())});state.publish_listary_folder();
        ensure!(folder_text(runner.hwnd)?.is_empty(),"search results were advertised as a single folder");
    }
    drop(runner);pump(HWND::default(),25);
    ensure!(!unsafe { IsWindow(Some(endpoint)) }.as_bool(),"folder endpoint survived its owner");
    Ok(serde_json::json!({"native_shell_history":true,"child_side_back_forward":true,"driver_app_commands":true,"duplicate_driver_pair_once":true,
        "address_and_rename_protected":true,"pending_and_tab_paths_cleared":true,"virtual_and_search_paths_empty":true,"read_only_endpoint":true,"endpoint_destroyed":true,"listary_plugin_contract":probe_state,"actual_listary_application":"not exercised"}))
}

#[test]
#[ignore="Private-desktop side-button and Listary protocol test; requires ZSDESK_NATIVE_TEST_OUTPUT"]
fn native_side_buttons_and_listary_folder_contract()->Result<()> {
    let output=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("output")?).join("integration-navigation");
    ensure!(output.is_absolute(),"absolute output required");fs::create_dir_all(&output)?;
    let foreground=unsafe { GetForegroundWindow() };let name=wide(&format!("ZSDesk.IntegrationNavigation.{}",std::process::id()));
    let desktop=unsafe { CreateDesktopW(name.as_ptr(),ptr::null(),ptr::null(),0,0x000f00ff,ptr::null()) };
    ensure!(!desktop.is_null(),"private desktop creation failed");let raw=desktop as usize;let target=output.clone();
    let checks=thread::spawn(move||->Result<serde_json::Value>{ensure!(unsafe { SetThreadDesktop(raw as*mut c_void) }!=0,"SetThreadDesktop failed");run(&target)}).join().map_err(|_|anyhow::anyhow!("native integration test panicked")).and_then(|result|result);
    let closed=unsafe { CloseDesktop(desktop) }!=0;let current=unsafe { GetForegroundWindow() };let mut pid=0;unsafe { GetWindowThreadProcessId(current,Some(&mut pid)); }
    let isolated=pid!=std::process::id();let report=serde_json::json!({"state":if checks.is_ok()&&closed&&isolated{"passed"}else{"failed"},"checks":checks.as_ref().map_err(|error|format!("{error:#}")),"desktop_closed":closed,"test_did_not_activate":isolated,"foreground_unchanged":foreground==current,"desktop_switched":false});
    fs::write(output.join("verification.current.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");checks?;ensure!(closed&&isolated,"desktop cleanup/isolation failed");Ok(())
}
