//! Opt-in frame regression on an unswitched private desktop.
use super::*;
use super::navigation::window_text;
use std::{ffi::c_void, ptr, thread, time::Duration};
use anyhow::ensure;

#[link(name="user32")]
unsafe extern "system" {
    fn CreateDesktopW(name:*const u16,device:*const u16,mode:*const c_void,flags:u32,access:u32,attributes:*const c_void)->*mut c_void;
    fn SetThreadDesktop(desktop:*mut c_void)->i32;
    fn CloseDesktop(desktop:*mut c_void)->i32;
    fn OpenDesktopW(name:*const u16,flags:u32,inherit:i32,access:u32)->*mut c_void;
}

fn pump() {
    for _ in 0..8 {
        let mut message=MSG::default();
        while unsafe {PeekMessageW(&mut message,None,0,0,PM_REMOVE)}.as_bool() {
            if message.message!=WM_QUIT {unsafe {let _=TranslateMessage(&message);DispatchMessageW(&message);}}
        }
        thread::sleep(Duration::from_millis(4));
    }
}
fn rectangle(window:HWND)->Result<RECT> {
    let mut rect=RECT::default();unsafe {GetWindowRect(window,&mut rect)?;}Ok(rect)
}
fn packed(point:POINT)->LPARAM {LPARAM(((point.x as u32&0xffff)|((point.y as u32&0xffff)<<16))as isize)}

fn run_frame()->Result<serde_json::Value> {
    let instance=HINSTANCE(unsafe {GetModuleHandleW(None)?}.0);
    let class=windows::core::w!("ZSDesk.Native.FrameRegression");
    let wc=WNDCLASSW {lpfnWndProc:Some(window_proc),hInstance:instance,lpszClassName:class,..Default::default()};
    unsafe {RegisterClassW(&wc);}
    // A real window_proc is sufficient for frame handling; no Shell host,
    // profile, file operation or integration startup is created here.
    let slot=Box::new(RefCell::new(None::<WindowState>));
    let window=unsafe {CreateWindowExW(WS_EX_NOACTIVATE,class,windows::core::w!("Frame regression"),
        WS_OVERLAPPEDWINDOW|WS_CLIPCHILDREN,100,100,900,650,None,None,Some(instance),Some((&*slot as*const Slot).cast()))?};
    struct Owned(HWND);
    impl Drop for Owned {fn drop(&mut self){let _=unsafe {DestroyWindow(self.0)};}}
    let owner=Owned(window);
    unsafe {SetWindowPos(window,None,0,0,0,0,SWP_FRAMECHANGED|SWP_NOMOVE|SWP_NOSIZE|SWP_NOZORDER|SWP_NOACTIVATE)?;let _=ShowWindow(window,SW_SHOWNOACTIVATE);}
    pump();let normal=rectangle(window)?;
    let mid=POINT{x:(normal.left+normal.right)/2,y:(normal.top+normal.bottom)/2};
    for (point,expected) in [
        (POINT{x:normal.left+1,y:normal.top+1},HTTOPLEFT),(POINT{x:normal.right-2,y:normal.top+1},HTTOPRIGHT),
        (POINT{x:normal.left+1,y:normal.bottom-2},HTBOTTOMLEFT),(POINT{x:normal.right-2,y:normal.bottom-2},HTBOTTOMRIGHT),
        (POINT{x:normal.left+1,y:mid.y},HTLEFT),(POINT{x:normal.right-2,y:mid.y},HTRIGHT),
        (POINT{x:mid.x,y:normal.top+1},HTTOP),(POINT{x:mid.x,y:normal.bottom-2},HTBOTTOM),
    ] {
        let hit=unsafe {SendMessageW(window,WM_NCHITTEST,None,Some(packed(point)))}.0 as u32;
        ensure!(hit==expected,"resize hit {point:?}: {hit} != {expected}");
    }
    let mut client=normal;
    unsafe {SendMessageW(window,WM_NCCALCSIZE,Some(WPARAM(0)),Some(LPARAM((&mut client as*mut RECT)as isize)));}
    ensure!(client.top==normal.top,"FALSE NCCALCSIZE retained a caption inset");
    for _ in 0..2 {
        unsafe {let _=ShowWindow(window,SW_SHOWMAXIMIZED);}pump();
        ensure!(unsafe {IsZoomed(window)}.as_bool(),"window did not maximize");
        let maximized=rectangle(window)?;
        let hit=unsafe {SendMessageW(window,WM_NCHITTEST,None,Some(packed(POINT{x:maximized.left+1,y:maximized.top+1})))}.0 as u32;
        ensure!(!geometry::is_resize_hit(hit),"maximized window exposes a resize border");
        unsafe {let _=ShowWindow(window,SW_RESTORE);}pump();
        ensure!(rectangle(window)?==normal,"normal window bounds drifted after restore");
    }
    unsafe {let _=ShowWindow(window,SW_SHOWMAXIMIZED);let _=ShowWindow(window,SW_MINIMIZE);}pump();
    let mut placement=WINDOWPLACEMENT{length:size_of::<WINDOWPLACEMENT>()as u32,..Default::default()};
    unsafe {GetWindowPlacement(window,&mut placement)?;}
    ensure!(geometry::restore_maximized(placement.showCmd,placement.flags.0),"minimized maximize state was lost");
    unsafe {let _=ShowWindow(window,SW_RESTORE);}pump();
    ensure!(unsafe {IsZoomed(window)}.as_bool(),"minimized maximized window restored to normal");
    let report=serde_json::json!({"resize_edges":8,"normal_maximize_restore_cycles":2,
        "minimized_maximize_state_preserved":true,"normal_rect":[normal.left,normal.top,normal.right,normal.bottom],
        "dpi":unsafe {GetDpiForWindow(window)},"shell_operations":0,"undo_executed":false});
    drop(owner);pump();drop(slot);Ok(report)
}

#[test]
#[ignore="Local Windows geometry validation; creates an unswitched private desktop"]
fn native_resize_edges_and_restore_preserve_normal_bounds()->Result<()> {
    let before=unsafe {GetForegroundWindow()};
    let name=wide(&format!("ZSDesk.FrameRegression.{}",std::process::id()));
    let desktop=unsafe {CreateDesktopW(name.as_ptr(),ptr::null(),ptr::null(),0,0x000f00ff,ptr::null())};
    ensure!(!desktop.is_null(),"CreateDesktop failed");
    let handle=desktop as usize;
    let result=thread::spawn(move||->Result<serde_json::Value>{
        ensure!(unsafe {SetThreadDesktop(handle as*mut c_void)}!=0,"SetThreadDesktop failed");run_frame()
    }).join().map_err(|_|anyhow::anyhow!("frame regression thread panicked"));
    let closed=unsafe {CloseDesktop(desktop)}!=0;
    let foreground_unchanged=before==unsafe {GetForegroundWindow()};
    let report=result??;
    ensure!(closed,"private desktop was not released");
    ensure!(foreground_unchanged,"foreground changed during private frame test");
    println!("{}",serde_json::json!({"state":"passed","checks":report,"desktop_closed":closed,"desktop_switched":false,"foreground_unchanged":foreground_unchanged}));
    Ok(())
}

fn screen_edge_cases(output:PathBuf,handle:usize)->Result<serde_json::Value>{
        ensure!(unsafe{SetThreadDesktop(handle as*mut c_void)}!=0,"SetThreadDesktop failed");
        let instance=HINSTANCE(unsafe{GetModuleHandleW(None)?}.0);
        let class=windows::core::w!("ZSDesk.Native.EdgeRegression");
        unsafe{RegisterClassW(&WNDCLASSW{lpfnWndProc:Some(window_proc),hInstance:instance,lpszClassName:class,..Default::default()});}
        let mut cases=Vec::new();
        for native in [false,true]{for full in [false,true]{
            let profile=output.join(format!("edge-fixture-{native}-{full}.json"));ensure!(!profile.exists(),"existing fixture retained");
            struct Profile(PathBuf);impl Drop for Profile{fn drop(&mut self){let _=std::fs::remove_file(&self.0);}}let _profile=Profile(profile.clone());
            let slot=Box::new(RefCell::new(None::<WindowState>));
            let window=unsafe{CreateWindowExW(WS_EX_NOACTIVATE,class,windows::core::w!("Edge regression"),WS_OVERLAPPEDWINDOW|WS_CLIPCHILDREN,
                100,100,900,650,None,None,Some(instance),Some((&*slot as*const Slot).cast()))?};
            let owner=FocusWindow(window);
            if native{unsafe{SetPropW(window,windows::core::w!("ZSDesk.NativeCaption"),Some(windows::Win32::Foundation::HANDLE(1usize as*mut _)))?;}}
            if full{
                use windows::Win32::Graphics::Gdi::{GetMonitorInfoW,MonitorFromWindow,MONITORINFO,MONITOR_DEFAULTTONEAREST};
                let mut monitor=MONITORINFO{cbSize:size_of::<MONITORINFO>()as u32,..Default::default()};
                unsafe{GetMonitorInfoW(MonitorFromWindow(window,MONITOR_DEFAULTTONEAREST),&mut monitor)}.ok()?;
                unsafe{SetPropW(window,window_actions::FULLSCREEN,Some(windows::Win32::Foundation::HANDLE(1usize as*mut _)))?;
                    SetWindowLongPtrW(window,GWL_STYLE,GetWindowLongPtrW(window,GWL_STYLE)&!((WS_CAPTION|WS_THICKFRAME).0 as isize));
                    let r=monitor.rcMonitor;SetWindowPos(window,None,r.left,r.top,r.right-r.left,r.bottom-r.top,SWP_FRAMECHANGED|SWP_NOZORDER|SWP_NOACTIVATE)?;
                    let _=ShowWindow(window,SW_SHOWNOACTIVATE);}
            }else{unsafe{let _=ShowWindow(window,SW_SHOWMAXIMIZED);}}
            pump();ensure!(screen_edge_window(window),"edge mode not active");
            let mut client=RECT::default();unsafe{GetClientRect(window,&mut client)}?;let dpi=unsafe{GetDpiForWindow(window)}.max(96);
            let mut session=storage::default_session();session.tabs=vec![ExplorerTab::new(1,ShellLocation::ThisPc),ExplorerTab::new(2,ShellLocation::ThisPc)];session.active_tab=0;
            let mut state=WindowState::new(session,profile,None,false);
            state.hwnd=window;state.dpi=dpi;
            let top=scale(zsdesk_ui::chrome::TAB_TOP,state.dpi);let width=scale(200,state.dpi);
            state.layout.strip=LayoutRect::new(0,0,client.right,scale(STRIP_H,state.dpi));
            state.layout.tabs=(0..2).map(|index|zsdesk_ui::chrome::TabSlot{index,rect:LayoutRect::new(scale(12,state.dpi)+index as i32*width,top,width-2,scale(STRIP_H,state.dpi)-top),..Default::default()}).collect();
            let tab_x=state.layout.tabs[1].rect.x+width/2;
            ensure!(state.chrome_hit(tab_x,0)==Hit::Tab(1),"top edge did not select its tab");
            *slot.borrow_mut()=Some(state);
            let mut point=POINT{x:tab_x,y:0};unsafe{ClientToScreen(window,&mut point)}.ok()?;
            ensure!(unsafe{SendMessageW(window,WM_NCHITTEST,None,Some(packed(point)))}.0==HTCLIENT as isize,"top tab edge became caption/resize hit");
            let edge=POINT{x:client.right-1,y:0};let mut screen=edge;unsafe{ClientToScreen(window,&mut screen)}.ok()?;
            ensure!(unsafe{SendMessageW(window,WM_NCHITTEST,None,Some(packed(screen)))}.0==HTCLOSE as isize,"upper-right edge missed close");
            super::interaction_regression::caption_click_at(window,2,Some(edge))?;
            ensure!(!unsafe{IsWindow(Some(window))}.as_bool(),"upper-right edge click did not close its own window");
            cases.push(serde_json::json!({"native_caption":native,"fullscreen":full,"dpi":dpi,"top_tab_client_hit":true,"corner_close_executed":true}));
            drop(owner);pump();drop(slot);
        }}
        Ok(serde_json::json!({"cases":cases,"no_user_input":true}))
}

#[test]
#[ignore="Owned process endpoint for the private-desktop edge regression"]
fn native_screen_edge_worker_entry()->Result<()>{
    let name=std::env::var("ZSDESK_EDGE_TEST_DESKTOP")?;
    ensure!(name.starts_with("ZSDesk.EdgeRegression.")&&name.bytes().all(|byte|byte.is_ascii_alphanumeric()||byte==b'.'),"dedicated fixture desktop required");
    let output=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").ok_or_else(||anyhow::anyhow!("fixture output required"))?);
    ensure!(output.is_absolute(),"absolute fixture output required");
    let desktop=unsafe{OpenDesktopW(wide(&name).as_ptr(),0,0,0x000f00ff)};
    ensure!(!desktop.is_null(),"OpenDesktop failed");
    let report=screen_edge_cases(output,desktop as usize)?;
    // Shell/UI services may keep threads alive until process exit. The parent
    // owns the desktop and verifies its closure after this worker exits.
    println!("{}",serde_json::json!({"edge_worker_checks":report}));Ok(())
}

#[test]
#[ignore="Real maximized/fullscreen edge input in an owned process on an unswitched private desktop"]
fn native_screen_edges_reach_close_and_top_tabs()->Result<()>{
    use std::{fs,os::windows::process::CommandExt,process::{Command,Stdio},time::Instant};
    let output=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").ok_or_else(||anyhow::anyhow!("explicit fixture output required"))?);
    ensure!(output.is_absolute(),"absolute fixture output required");
    let name=format!("ZSDesk.EdgeRegression.{}",std::process::id());let encoded=wide(&name);
    let desktop=unsafe{CreateDesktopW(encoded.as_ptr(),ptr::null(),ptr::null(),0,0x000f00ff,ptr::null())};
    ensure!(!desktop.is_null(),"CreateDesktop failed");
    let result=(||->Result<serde_json::Value>{
        let log=output.join("edge-worker.current.log");
        if log.exists(){use std::os::windows::fs::MetadataExt;ensure!(fs::symlink_metadata(&log)?.file_attributes()&0x400==0,"worker log is a reparse point");}
        let stdout=fs::File::create(&log)?;let stderr=stdout.try_clone()?;
        struct Worker(std::process::Child);impl Drop for Worker{fn drop(&mut self){if self.0.try_wait().ok().flatten().is_none(){let _=self.0.kill();let _=self.0.wait();}}}
        let mut child=Worker(Command::new(std::env::current_exe()?).args(["window::geometry_regression::native_screen_edge_worker_entry","--exact","--ignored","--nocapture","--test-threads=1"])
            .env("ZSDESK_EDGE_TEST_DESKTOP",&name).stdin(Stdio::null()).stdout(stdout).stderr(stderr).creation_flags(0x08000000).spawn()?);
        let pid=child.0.id();let deadline=Instant::now()+Duration::from_secs(30);
        let status=loop{if let Some(status)=child.0.try_wait()?{break status;}ensure!(Instant::now()<deadline,"owned edge worker timed out");thread::sleep(Duration::from_millis(10));};
        let text=fs::read_to_string(&log)?;ensure!(status.success(),"edge worker failed: {text}");
        let report=text.lines().filter_map(|line|serde_json::from_str::<serde_json::Value>(line).ok()).find_map(|value|value.get("edge_worker_checks").cloned()).ok_or_else(||anyhow::anyhow!("edge worker report missing"))?;
        ensure!(report["cases"].as_array().is_some_and(|cases|cases.len()==4),"edge cases incomplete");
        Ok(serde_json::json!({"worker_pid":pid,"worker_exited":true,"checks":report}))
    })();
    let closed=unsafe{CloseDesktop(desktop)}!=0;let close_error=unsafe{windows::Win32::Foundation::GetLastError()}.0;
    let reopened=unsafe{OpenDesktopW(encoded.as_ptr(),0,0,0x000f00ff)};let absent=reopened.is_null();if !absent{let _=unsafe{CloseDesktop(reopened)};}
    let report=result?;ensure!(closed&&absent,"private desktop cleanup failed: {close_error}");
    println!("{}",serde_json::json!({"state":"passed","result":report,"desktop_closed":true,"desktop_absent":true,"desktop_switched":false}));Ok(())
}

struct FocusWindow(HWND);
impl Drop for FocusWindow {fn drop(&mut self){let _=unsafe {DestroyWindow(self.0)};}}

fn activate_message(window:HWND,activation:u32) {
    unsafe {SendMessageW(window,WM_ACTIVATE,Some(WPARAM(activation as usize)),Some(LPARAM(0)));}
}
fn inside(window:HWND,child:HWND)->bool {window==child || unsafe {IsChild(window,child)}.as_bool()}

fn run_focus(output:&std::path::Path)->Result<serde_json::Value> {
    use std::time::Instant;
    unsafe {OleInitialize(None)?;}
    struct Apartment;
    impl Drop for Apartment {fn drop(&mut self){unsafe {OleUninitialize();}}}
    let _apartment=Apartment;
    unsafe {InitCommonControlsEx(&INITCOMMONCONTROLSEX{dwSize:size_of::<INITCOMMONCONTROLSEX>() as u32,
        dwICC:ICC_BAR_CLASSES|ICC_STANDARD_CLASSES|ICC_LISTVIEW_CLASSES})}.ok()?;
    let fixture=output.join("focus-fixture");std::fs::create_dir_all(&fixture)?;
    let file=fixture.join("focus sample.txt");
    if file.exists() {ensure!(std::fs::read(&file)?==b"ZSDesk focus regression fixture","focus fixture changed");}
    else {std::fs::write(&file,b"ZSDesk focus regression fixture")?;}
    let mut session=ExplorerSession::default();
    session.tabs=vec![ExplorerTab::new(1,ShellLocation::Path(fixture))];session.next_id=2;
    let slot=Box::new(RefCell::new(Some(WindowState::new(session,output.join("focus-profile/explorer-session.json"),None,false))));
    let instance=HINSTANCE(unsafe {GetModuleHandleW(None)?}.0);
    let class=windows::core::w!("ZSDesk.Native.FocusRegression");
    unsafe {RegisterClassW(&WNDCLASSW{lpfnWndProc:Some(window_proc),hInstance:instance,lpszClassName:class,..Default::default()});}
    let owner=FocusWindow(unsafe {CreateWindowExW(WS_EX_NOACTIVATE,class,windows::core::w!("Focus regression"),
        WS_OVERLAPPEDWINDOW|WS_CLIPCHILDREN,100,100,1000,700,None,None,Some(instance),Some((&*slot as*const Slot).cast()))?});
    unsafe {SetWindowPos(owner.0,None,0,0,0,0,SWP_FRAMECHANGED|SWP_NOMOVE|SWP_NOSIZE|SWP_NOZORDER|SWP_NOACTIVATE)?;
        slot.borrow_mut().as_mut().unwrap().create(owner.0)?;let _=ShowWindow(owner.0,SW_SHOWNOACTIVATE);}
    let started=Instant::now();
    loop {pump();if slot.borrow().as_ref().is_some_and(|state|state.pending.is_none() && state.shown_tab.is_some()){break;}
        ensure!(started.elapsed()<Duration::from_secs(12),"focus fixture navigation timeout");}
    let view=slot.borrow().as_ref().unwrap().browser.as_ref().and_then(|browser|browser.view_window()).context("native file view")?;
    let edit=slot.borrow().as_ref().unwrap().search_edit;
    let foreign=FocusWindow(unsafe {CreateWindowExW(WS_EX_NOACTIVATE,windows::core::w!("STATIC"),windows::core::w!("Other owned window"),
        WS_OVERLAPPEDWINDOW,1150,100,300,200,None,None,Some(instance),None)?});
    let foreign_edit=unsafe {child(foreign.0,"EDIT","",2001,ES_AUTOHSCROLL as u32,true)?};
    unsafe {let _=ShowWindow(foreign.0,SW_SHOWNOACTIVATE);}

    // Exercise the real WindowState/window_proc activation route. Synthetic
    // messages stay on this private desktop; no physical input is generated.
    let remember_edit=||->Result<()> {
        unsafe {let _=SetFocus(Some(edit));}
        ensure!(unsafe {GetFocus()}==edit,"search EDIT did not receive local focus");
        activate_message(owner.0,WA_INACTIVE);
        ensure!(slot.borrow().as_ref().unwrap().last_focus==edit,"deactivation did not remember owned EDIT");
        unsafe {let _=SetFocus(None);}Ok(())
    };
    remember_edit()?;
    activate_message(owner.0,WA_CLICKACTIVE);
    ensure!(unsafe {GetFocus()}!=edit,"click activation restored the previous EDIT before the clicked control");
    let native_target={let state=slot.borrow();unsafe {state.as_ref().unwrap().browser.as_ref().unwrap().focus();}unsafe {GetFocus()}};
    ensure!(inside(view,native_target),"native file-list focus did not stay inside the view");
    unsafe {
        SendMessageW(native_target,WM_LBUTTONDOWN,Some(WPARAM(1)),Some(packed(POINT{x:80,y:64})));
        SendMessageW(native_target,WM_LBUTTONUP,None,Some(packed(POINT{x:80,y:64})));
    }
    pump();ensure!(inside(view,unsafe {GetFocus()}),"file-list click transferred focus to chrome input");

    remember_edit()?;activate_message(owner.0,WA_CLICKACTIVE);
    ensure!(unsafe {GetFocus()}!=edit,"blank click activation restored the previous EDIT");
    // The main-window focus path forwards to the actual native view.
    unsafe {let _=SetFocus(Some(owner.0));}
    pump();ensure!(inside(view,unsafe {GetFocus()}),"main-window focus did not return to the native view");
    let before_hover=unsafe {GetFocus()};
    let search=slot.borrow().as_ref().unwrap().layout.search;
    unsafe {SendMessageW(owner.0,WM_MOUSEMOVE,None,Some(packed(POINT{x:search.x+2,y:search.y+2})));}
    ensure!(unsafe {GetFocus()}==before_hover,"hovering the search field moved keyboard focus");
    let cursor=unsafe {SendMessageW(owner.0,WM_SETCURSOR,Some(WPARAM(edit.0 as usize)),Some(LPARAM((HTCLIENT|(WM_MOUSEMOVE<<16))as isize)))};
    ensure!(cursor.0==0,"main window consumed the EDIT child's cursor message");

    remember_edit()?;activate_message(owner.0,WA_ACTIVE);
    ensure!(unsafe {GetFocus()}==edit,"keyboard activation did not restore valid owned EDIT focus");
    let mut invalid_targets=Vec::new();
    for kind in ["hidden","disabled","foreign","destroyed"] {
        unsafe {let _=SetFocus(None);}
        let target=match kind {
            "hidden"=>{unsafe {let _=ShowWindow(edit,SW_HIDE);}edit},
            "disabled"=>{unsafe {let _=ShowWindow(edit,SW_SHOW);let _=EnableWindow(edit,false);}edit},
            "foreign"=>{unsafe {let _=EnableWindow(edit,true);}foreign_edit},
            _=>{let dead=unsafe {child(owner.0,"EDIT","",2002,ES_AUTOHSCROLL as u32,true)?};
                unsafe {DestroyWindow(dead)?;}dead},
        };
        slot.borrow_mut().as_mut().unwrap().last_focus=target;
        ensure!(!slot.borrow().as_ref().unwrap().owns_live_focus(target),"{kind} focus target was accepted");
        activate_message(owner.0,WA_ACTIVE);
        ensure!(inside(view,unsafe {GetFocus()}),"{kind} remembered focus did not fall back to the native view");
        invalid_targets.push(kind);
    }
    unsafe {let _=SetFocus(Some(foreign_edit));}
    activate_message(owner.0,WA_INACTIVE);
    ensure!(slot.borrow().as_ref().unwrap().last_focus.0.is_null(),"foreign focus was saved on deactivation");
    drop(foreign);drop(owner);pump();
    ensure!(slot.borrow().is_none(),"WindowState was not released");
    Ok(serde_json::json!({"state":"passed","native_shell_view":true,"click_activation_restores_edit":false,
        "file_view_click_keeps_view_focus":true,"main_window_focus_forwards_to_view":true,"hover_moves_focus":false,
        "keyboard_activation_restores_valid_focus":true,"invalid_focus_targets_rejected":invalid_targets,
        "foreign_focus_saved":false,"child_cursor_message_consumed":false,"shell_operations":0,"undo_executed":false,
        "delivery":"SendMessage activation/click messages and focus calls on self-owned private-desktop windows; no physical input"}))
}

#[test]
#[ignore="Private desktop input-focus regression; requires ZSDESK_NATIVE_TEST_OUTPUT"]
fn native_activation_keeps_mouse_and_keyboard_focus_semantics()->Result<()> {
    let output=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("set ZSDESK_NATIVE_TEST_OUTPUT")?);
    ensure!(output.is_absolute(),"output must be absolute");std::fs::create_dir_all(&output)?;
    let before=unsafe {GetForegroundWindow()};let name=wide(&format!("ZSDesk.FocusRegression.{}",std::process::id()));
    let desktop=unsafe {CreateDesktopW(name.as_ptr(),ptr::null(),ptr::null(),0,0x000f00ff,ptr::null())};
    ensure!(!desktop.is_null(),"CreateDesktop failed");let handle=desktop as usize;let worker_output=output.clone();
    let result=thread::spawn(move||->Result<serde_json::Value>{
        ensure!(unsafe {SetThreadDesktop(handle as*mut c_void)}!=0,"SetThreadDesktop failed");run_focus(&worker_output)
    }).join().map_err(|_|anyhow::anyhow!("focus regression thread panicked"));
    let closed=unsafe {CloseDesktop(desktop)}!=0;let after=unsafe {GetForegroundWindow()};
    let checks=result.and_then(|result|result);
    let report=serde_json::json!({"state":if checks.is_ok() && closed && before==after{"passed"}else{"failed"},
        "checks":checks.as_ref().map_err(|error|format!("{error:#}")),"desktop_closed":closed,"desktop_switched":false,
        "foreground_before":before.0 as usize,"foreground_after":after.0 as usize,"foreground_unchanged":before==after});
    std::fs::write(output.join("focus.current.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");
    checks?;ensure!(closed,"private focus desktop was not released");ensure!(before==after,"foreground changed during focus test");Ok(())
}

fn fixture_file(path:&std::path::Path,bytes:&[u8])->Result<()> {
    if path.exists(){ensure!(std::fs::read(path)?==bytes,"preview fixture changed: {}",path.display());}
    else {std::fs::write(path,bytes)?;}Ok(())
}
fn preview_pdf()->Vec<u8> {
    let stream="BT /F1 18 Tf 35 110 Td (ZSDesk preview fixture) Tj ET";
    let objects=["<< /Type /Catalog /Pages 2 0 R >>".to_string(),"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 200] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>".into(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),format!("<< /Length {} >>\nstream\n{stream}\nendstream",stream.len())];
    let mut value=b"%PDF-1.4\n".to_vec();let mut offsets=Vec::new();
    for (index,object) in objects.iter().enumerate(){offsets.push(value.len());value.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n",index+1).as_bytes());}
    let xref=value.len();value.extend_from_slice(b"xref\n0 6\n0000000000 65535 f \n");
    for offset in offsets {value.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());}
    value.extend_from_slice(format!("trailer\n<< /Size 6 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes());value
}
fn preview_bitmap()->Vec<u8> {
    let mut value=vec![0;70];value[..2].copy_from_slice(b"BM");
    value[2..6].copy_from_slice(&70u32.to_le_bytes());value[10..14].copy_from_slice(&54u32.to_le_bytes());
    value[14..18].copy_from_slice(&40u32.to_le_bytes());value[18..22].copy_from_slice(&2i32.to_le_bytes());
    value[22..26].copy_from_slice(&2i32.to_le_bytes());value[26..28].copy_from_slice(&1u16.to_le_bytes());
    value[28..30].copy_from_slice(&24u16.to_le_bytes());value[34..38].copy_from_slice(&16u32.to_le_bytes());
    value[54..].copy_from_slice(&[210,140,25,210,140,25,0,0,210,140,25,210,140,25,0,0]);value
}
fn exclusive_read(path:&std::path::Path)->Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    let start=std::time::Instant::now();
    loop {
        match std::fs::OpenOptions::new().read(true).share_mode(0).open(path) {
            Ok(file)=>{drop(file);return Ok(());}
            Err(error) if start.elapsed()>=Duration::from_secs(2)=>return Err(error.into()),
            Err(_)=>pump(),
        }
    }
}
fn run_preview(output:&std::path::Path)->Result<serde_json::Value> {
    use windows::Win32::UI::HiDpi::{AreDpiAwarenessContextsEqual,GetThreadDpiAwarenessContext,GetWindowDpiAwarenessContext,
        SetThreadDpiAwarenessContext,DPI_AWARENESS_CONTEXT,DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,DPI_AWARENESS_CONTEXT_UNAWARE};
    use zsdesk_core::model::explorer::PreviewProviderPreference;
    unsafe {OleInitialize(None)?;}
    struct Apartment;
    impl Drop for Apartment {fn drop(&mut self){unsafe {OleUninitialize();}}}
    let _apartment=Apartment;
    struct Dpi(DPI_AWARENESS_CONTEXT);
    impl Drop for Dpi {fn drop(&mut self){if !self.0.0.is_null(){unsafe {SetThreadDpiAwarenessContext(self.0);}}}}
    let previous=unsafe {SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)};
    ensure!(!previous.0.is_null(),"cannot set owner DPI awareness");let _dpi=Dpi(previous);
    let owner=FocusWindow(unsafe {CreateWindowExW(WS_EX_NOACTIVATE,windows::core::w!("STATIC"),windows::core::w!("Hidden preview regression"),
        WS_OVERLAPPEDWINDOW,100,100,900,650,None,None,None,None)?});
    let owner_dpi=unsafe {GetWindowDpiAwarenessContext(owner.0)};
    ensure!(!unsafe {SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_UNAWARE)}.0.is_null(),"cannot set distinct caller DPI awareness");
    let caller=unsafe {GetThreadDpiAwarenessContext()};
    ensure!(!unsafe {AreDpiAwarenessContextsEqual(caller,owner_dpi)}.as_bool(),"DPI test requires distinct owner and caller contexts");
    let same_dpi=||unsafe {AreDpiAwarenessContextsEqual(GetThreadDpiAwarenessContext(),caller)}.as_bool();
    let fixture=output.join("preview-fixture");std::fs::create_dir_all(&fixture)?;
    let text=fixture.join("preview text.txt");let image=fixture.join("preview image.bmp");let pdf=fixture.join("preview document.pdf");
    fixture_file(&text,"中文预览文本。\r\nSecond line.".as_bytes())?;
    fixture_file(&image,&preview_bitmap())?;fixture_file(&pdf,&preview_pdf())?;
    let focus_before=unsafe {GetFocus()};
    let mut preview=quick_preview::QuickPreview::open_hidden_fallback(owner.0,&text)?;
    let text_snapshot=preview.snapshot();
    ensure!(text_snapshot.kind=="text","text fallback unavailable: {}",text_snapshot.notice);
    ensure!(!unsafe {IsWindowVisible(text_snapshot.hwnd)}.as_bool(),"hidden preview became visible");
    ensure!(unsafe {AreDpiAwarenessContextsEqual(GetWindowDpiAwarenessContext(text_snapshot.hwnd),owner_dpi)}.as_bool(),"preview inherited the caller's temporary DPI context");
    ensure!(same_dpi(),"opening preview did not restore caller DPI");
    let text_child=text_snapshot.text_child.context("text child")?;
    ensure!(window_text(text_child).contains("中文预览文本"),"Unicode text preview changed");
    preview.update(&image)?;
    let image_snapshot=preview.snapshot();
    ensure!(image_snapshot.kind=="image" && image_snapshot.image_bytes>0 && image_snapshot.image_bytes<=1024,"image fallback not bounded");
    ensure!(!unsafe {IsWindow(Some(text_child))}.as_bool(),"updating preview retained its text child");
    ensure!(same_dpi(),"updating preview did not restore caller DPI");
    drop(preview);pump();
    ensure!(!unsafe {IsWindow(Some(image_snapshot.hwnd))}.as_bool(),"preview HWND survived close");
    ensure!(same_dpi(),"closing fallback preview changed caller DPI");
    exclusive_read(&text)?;exclusive_read(&image)?;

    let pdf_preview=quick_preview::QuickPreview::open_hidden(owner.0,&pdf)?;
    let pdf_snapshot=pdf_preview.snapshot();
    ensure!(!unsafe {IsWindowVisible(pdf_snapshot.hwnd)}.as_bool(),"PDF preview became visible");
    ensure!(same_dpi(),"system preview activation changed caller DPI");
    drop(pdf_preview);pump();
    ensure!(!unsafe {IsWindow(Some(pdf_snapshot.hwnd))}.as_bool(),"PDF preview HWND survived close");
    ensure!(same_dpi(),"system preview Unload changed caller DPI");exclusive_read(&pdf)?;
    ensure!(unsafe {GetFocus()}==focus_before,"hidden preview changed thread focus");
    // Registration discovery is read-only. No Office/WPS document or associated
    // application is opened by this regression.
    let registrations:Vec<_>=[("Office",PreviewProviderPreference::Office),("WPS",PreviewProviderPreference::Wps)].into_iter().map(|(label,preference)|{
        let found=super::preview_handlers::discover(std::path::Path::new("fixture.docx"),preference);
        serde_json::json!({"preference":label,"candidates":found.candidates.iter().map(|candidate|
            serde_json::json!({"clsid":format!("{:?}",candidate.clsid),"context":candidate.context.0,"label":candidate.label})).collect::<Vec<_>>(),
            "unavailable":found.unavailable,"activation_tested":false})
    }).collect();
    drop(owner);
    Ok(serde_json::json!({"state":"passed","hidden":true,"unicode_text":true,"image_bytes":image_snapshot.image_bytes,
        "text_child_released_on_update":true,"windows_destroyed":true,"exclusive_read_after_close":true,
        "different_owner_caller_dpi":true,"caller_dpi_restored_on_open_update_close":true,"focus_unchanged":true,
        "pdf":{"mode":pdf_snapshot.kind,"handler_available":pdf_snapshot.kind=="system","notice":pdf_snapshot.notice},
        "office_wps_registration_only":registrations,"user_documents_opened":false}))
}

#[test]
#[ignore="Hidden preview DPI and release regression; requires ZSDESK_NATIVE_TEST_OUTPUT"]
fn native_hidden_preview_restores_dpi_and_releases_resources()->Result<()> {
    let output=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("set ZSDESK_NATIVE_TEST_OUTPUT")?);
    ensure!(output.is_absolute(),"output must be absolute");std::fs::create_dir_all(&output)?;
    let before=unsafe {GetForegroundWindow()};let name=wide(&format!("ZSDesk.PreviewRegression.{}",std::process::id()));
    let desktop=unsafe {CreateDesktopW(name.as_ptr(),ptr::null(),ptr::null(),0,0x000f00ff,ptr::null())};
    ensure!(!desktop.is_null(),"CreateDesktop failed");let handle=desktop as usize;let worker_output=output.clone();
    let result=thread::spawn(move||->Result<serde_json::Value>{
        ensure!(unsafe {SetThreadDesktop(handle as*mut c_void)}!=0,"SetThreadDesktop failed");run_preview(&worker_output)
    }).join().map_err(|_|anyhow::anyhow!("preview regression thread panicked"));
    let closed=unsafe {CloseDesktop(desktop)}!=0;let after=unsafe {GetForegroundWindow()};
    let checks=result.and_then(|result|result);
    let report=serde_json::json!({"state":if checks.is_ok() && closed && before==after{"passed"}else{"failed"},
        "checks":checks.as_ref().map_err(|error|format!("{error:#}")),"desktop_closed":closed,"desktop_switched":false,
        "foreground_before":before.0 as usize,"foreground_after":after.0 as usize,"foreground_unchanged":before==after});
    std::fs::write(output.join("preview-dpi.current.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");
    checks?;ensure!(closed,"private preview desktop was not released");ensure!(before==after,"foreground changed during preview test");Ok(())
}
