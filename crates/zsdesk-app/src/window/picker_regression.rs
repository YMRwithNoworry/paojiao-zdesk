//! Actual shared explorer chrome on offscreen windows; no input is injected.
use super::*;
use anyhow::{ensure,Context};
use std::{fs,thread,time::{Duration,Instant}};
use windows::{core::Interface,Win32::UI::Shell::{IFolderView2,FWF_SINGLESEL}};
unsafe extern "system" fn prevent_activation(code:i32,wp:WPARAM,lp:LPARAM)->LRESULT{
    if code==HCBT_ACTIVATE as i32{return LRESULT(1);}unsafe{CallNextHookEx(None,code,wp,lp)}
}
fn pump(ms:u64){let end=Instant::now()+Duration::from_millis(ms);while Instant::now()<end{
    let mut message=MSG::default();for _ in 0..128{if !unsafe{PeekMessageW(&mut message,None,0,0,PM_REMOVE)}.as_bool(){break;}
        if message.message!=WM_QUIT{unsafe{let _=TranslateMessage(&message);DispatchMessageW(&message);}}}thread::sleep(Duration::from_millis(2));}}
struct Fixture{window:HWND,slot:Box<Slot>}
impl Drop for Fixture{fn drop(&mut self){let _=unsafe{DestroyWindow(self.window)};pump(20);}}
fn ready(fixture:&Fixture)->Result<()>{let end=Instant::now()+Duration::from_secs(12);loop{pump(25);
    if fixture.slot.borrow().as_ref().is_some_and(|state|state.pending.is_none()&&state.queued.is_none()&&state.shown_tab.is_some()){return Ok(());}
    ensure!(Instant::now()<end,"picker navigation timed out");}}
fn footer_dpi_checks(fixture:&Fixture)->Result<()>{
    for dpi in [96,144,192]{
        unsafe{SetWindowPos(fixture.window,None,-30000,-30000,scale(900,dpi),scale(650,dpi),SWP_NOZORDER|SWP_NOACTIVATE)}?;
        {
            let mut slot=fixture.slot.borrow_mut();let state=slot.as_mut().context("picker state")?;
            state.dpi=dpi;state.relayout();state.position_children();
        }
        pump(30);
        let slot=fixture.slot.borrow();let state=slot.as_ref().context("picker state")?;let picker=state.picker.as_ref().context("picker footer")?;
        let (width,height)=state.client_size();let top=height-state.picker_height();
        ensure!(control_text(picker.accept)==picker.options.kind.title(),"picker action label does not match file/folder mode");
        for button in [picker.accept,picker.cancel]{
            ensure!(unsafe{IsWindowVisible(button)}.as_bool()&&unsafe{IsWindowEnabled(button)}.as_bool(),"picker action is hidden/disabled at {dpi} DPI");
            let mut rect=RECT::default();unsafe{GetWindowRect(button,&mut rect)}?;
            let mut start=POINT{x:rect.left,y:rect.top};let mut end=POINT{x:rect.right,y:rect.bottom};
            unsafe{ScreenToClient(fixture.window,&mut start)}.ok()?;unsafe{ScreenToClient(fixture.window,&mut end)}.ok()?;
            ensure!(start.x>=0&&start.y>=top&&end.x<=width&&end.y<=height&&end.x>start.x&&end.y>start.y,"picker action lies outside footer at {dpi} DPI");
            let hit=unsafe{ChildWindowFromPointEx(fixture.window,POINT{x:(start.x+end.x)/2,y:(start.y+end.y)/2},CWP_SKIPINVISIBLE|CWP_SKIPDISABLED)};
            ensure!(hit==button,"picker button is obscured at {dpi} DPI");
        }
        ensure!(unsafe{IsWindowVisible(picker.filter)}.as_bool()!=picker.options.kind.folders(),"file type visibility does not match selection mode");
        ensure!(state.layout.browser.bottom()<=top,"browser overlaps picker actions at {dpi} DPI");
    }Ok(())
}
fn exercise(root:&Path)->Result<serde_json::Value>{
    let hook=unsafe{SetWindowsHookExW(WH_CBT,Some(prevent_activation),None,windows::Win32::System::Threading::GetCurrentThreadId())}?;
    struct Hook(HHOOK);impl Drop for Hook{fn drop(&mut self){let _=unsafe{UnhookWindowsHookEx(self.0)};}}let _hook=Hook(hook);
    unsafe{OleInitialize(None)}?;struct Apartment;impl Drop for Apartment{fn drop(&mut self){zsdesk_windows::flyout::release_thread_resources();unsafe{OleUninitialize();}}}let _apartment=Apartment;
    let first=root.join("目录 A");let second=root.join("目录 B");fs::create_dir_all(&first)?;fs::create_dir_all(&second)?;
    for folder in [&first,&second]{fs::write(folder.join("selected.txt"),b"picker fixture")?;fs::write(folder.join("second.txt"),b"picker fixture")?;fs::write(folder.join("excluded.bin"),b"filter fixture")?;fs::create_dir_all(folder.join("child"))?;}
    let profile=root.join("explorer-session.json");let mut saved=ExplorerSession::default();saved.tabs=vec![ExplorerTab::new(1,ShellLocation::Path(first.clone()))];saved.active_tab=0;saved.next_id=2;
    let before=serde_json::to_vec_pretty(&saved)?;fs::write(&profile,&before)?;
    let instance=HINSTANCE(unsafe{GetModuleHandleW(None)}?.0);let name=windows::core::w!("ZSDesk.Native.PickerRegression");
    unsafe{RegisterClassW(&WNDCLASSW{lpfnWndProc:Some(window_proc),hInstance:instance,lpszClassName:name,..Default::default()});}
    for (cancelled,multiple) in [(false,false),(false,true),(true,false)]{
        let(tx,rx)=std::sync::mpsc::channel();let result=Rc::new(RefCell::new(Outcome::Cancelled));
        let options=Options{token:100+u64::from(cancelled)+u64::from(multiple)*2,title:"选择文件".into(),initial:ShellLocation::Path(first.clone()),multiple,kind:SelectionKind::Files,initial_name:String::new(),
            types:vec![FileType{label:"文本文件 (*.txt)".into(),pattern:"*.txt".into()}],filter:0,events:tx};
        let mut state=WindowState::new(saved.clone(),profile.clone(),None,false);state.picker=Some(State::new(options,result.clone()));state.secondary_window=true;
        let slot=Box::new(RefCell::new(Some(state)));
        let window=unsafe{CreateWindowExW(WS_EX_TOOLWINDOW|WS_EX_NOACTIVATE,name,windows::core::w!("Picker fixture"),WS_OVERLAPPEDWINDOW|WS_CLIPCHILDREN,
            -30000,-30000,1100,780,None,None,Some(instance),Some((&*slot as*const Slot).cast()))}?;
        let fixture=Fixture{window,slot};unsafe{fixture.slot.borrow_mut().as_mut().unwrap().create(window)?;let _=ShowWindow(window,SW_SHOWNOACTIVATE);}
        ready(&fixture)?;
        footer_dpi_checks(&fixture)?;
        {
            let slot=fixture.slot.borrow();let state=slot.as_ref().context("picker state")?;let browser=state.browser.as_ref().context("picker browser")?;
            let paths=browser.item_paths_limit(64);ensure!(paths.iter().any(|path|path.ends_with("selected.txt")),"file filter removed matching file: {paths:?}");
            ensure!(!paths.iter().any(|path|path.ends_with("excluded.bin")),"file filter showed excluded file: {paths:?}");
            ensure!(paths.iter().any(|path|path.ends_with("child")),"file filter hid navigation folders");
            let view=unsafe{browser.current_view()}?.cast::<IFolderView2>()?;ensure!((unsafe{view.GetCurrentFolderFlags()}?&FWF_SINGLESEL.0 as u32!=0)==!multiple,"selection mode was lost");
            let key=crate::instance::property(&profile);ensure!(unsafe{GetPropW(window,PCWSTR(key.as_ptr()))}.0.is_null(),"picker registered as ordinary Explorer target");
            ensure!(state.layout.browser.bottom()<=state.client_size().1-state.picker_height(),"picker footer overlaps file view");
        }
        unsafe{fixture.slot.borrow_mut().as_mut().unwrap().new_tab(ShellLocation::Path(second.clone()));}ready(&fixture)?;
        {
            let slot=fixture.slot.borrow();let state=slot.as_ref().unwrap();ensure!(state.session.tabs.len()==2&&state.layout.tabs.len()==2,"existing explorer tabs were not reused");
            if multiple{state.browser.as_ref().unwrap().select_children(&["selected.txt".into(),"second.txt".into()])?;}
            else{state.browser.as_ref().unwrap().select_child(std::ffi::OsStr::new("selected.txt"))?;}
        }pump(100);
        if !cancelled&&!multiple{
            let mut slot=fixture.slot.borrow_mut();let picker=slot.as_mut().unwrap().picker.as_mut().unwrap();picker.edited=true;
            unsafe{SetWindowTextW(picker.name,windows::core::w!("excluded.bin"))}?;
        }
        let message=if !cancelled&&!multiple{zsdesk_windows::shell::WM_SHELL_PICKER_ACCEPT}else{WM_COMMAND};
        if message==WM_COMMAND{
            let button={let slot=fixture.slot.borrow();let picker=slot.as_ref().unwrap().picker.as_ref().unwrap();if cancelled{picker.cancel}else{picker.accept}};
            unsafe{SendMessageW(button,BM_CLICK,None,None);}
        }else{unsafe{SendMessageW(window,message,Some(WPARAM(ACCEPT)),Some(LPARAM(0)));}}pump(150);
        ensure!(unsafe{IsWindow(Some(window))}.as_bool(),"picker closed before caller confirmation");
        let submitted=rx.try_iter().find_map(|event|match event{Event::Submit{outcome,..}=>Some(outcome),_=>None}).context("selection was not submitted")?;
        ensure!(matches!(&*result.borrow(),Outcome::Cancelled),"unconfirmed selection was committed");
        match &submitted{Outcome::Cancelled=>ensure!(cancelled,"selection was cancelled"),Outcome::Selected{paths,filter}=>{
            let mut expected=vec![second.join("selected.txt")];if multiple{expected.push(second.join("second.txt"));}expected.sort();let mut actual=paths.clone();actual.sort();
            ensure!(!cancelled&&actual==expected&&*filter==0,"selection returned wrong folder or filter");}}
        ensure!(fs::read(&profile)?==before,"picker overwrote the explorer tab session");drop(fixture);
    }
    for selected_child in [false,true]{
        let(tx,rx)=std::sync::mpsc::channel();let result=Rc::new(RefCell::new(Outcome::Cancelled));
        let options=Options{token:200+u64::from(selected_child),title:"选择文件夹".into(),initial:ShellLocation::Path(first.clone()),multiple:false,kind:SelectionKind::Folders,
            initial_name:String::new(),types:Vec::new(),filter:0,events:tx};
        let mut state=WindowState::new(saved.clone(),profile.clone(),None,false);state.picker=Some(State::new(options,result.clone()));state.secondary_window=true;
        let slot=Box::new(RefCell::new(Some(state)));
        let window=unsafe{CreateWindowExW(WS_EX_TOOLWINDOW|WS_EX_NOACTIVATE,name,windows::core::w!("Folder picker fixture"),WS_OVERLAPPEDWINDOW|WS_CLIPCHILDREN,
            -30000,-30000,1100,780,None,None,Some(instance),Some((&*slot as*const Slot).cast()))}?;
        let fixture=Fixture{window,slot};unsafe{fixture.slot.borrow_mut().as_mut().unwrap().create(window)?;let _=ShowWindow(window,SW_SHOWNOACTIVATE);}ready(&fixture)?;
        footer_dpi_checks(&fixture)?;
        {
            let slot=fixture.slot.borrow();let state=slot.as_ref().unwrap();let browser=state.browser.as_ref().unwrap();let paths=browser.item_paths_limit(64);
            ensure!(paths.iter().any(|path|path.ends_with("child"))&&!paths.iter().any(|path|path.ends_with(".txt")||path.ends_with(".bin")),"folder picker exposed file results: {paths:?}");
            ensure!(validate_paths(&[first.join("selected.txt")],false,SelectionKind::Folders).is_err(),"folder picker accepts files");
            ensure!(validate_paths(&[first.clone()],false,SelectionKind::Files).is_err(),"file picker accepts directories");
            if selected_child{browser.select_child(std::ffi::OsStr::new("child"))?;}
        }pump(80);
        let button=fixture.slot.borrow().as_ref().unwrap().picker.as_ref().unwrap().accept;
        unsafe{SendMessageW(button,BM_CLICK,None,None);}pump(150);
        ensure!(unsafe{IsWindow(Some(window))}.as_bool(),"folder picker closed without caller confirmation");
        let submitted=rx.try_iter().find_map(|event|match event{Event::Submit{outcome,..}=>Some(outcome),_=>None}).context("folder selection was not submitted")?;
        let expected=if selected_child{first.join("child")}else{first.clone()};
        ensure!(matches!(submitted,Outcome::Selected{paths,..} if paths==vec![expected]),"wrong selected/current folder");
        ensure!(fs::read(&profile)?==before,"folder picker overwrote session");drop(fixture);
    }
    storage::save_file_dialog_takeover(&profile,false)?;
    let mut stale=WindowState::new(saved.clone(),profile.clone(),None,false);stale.save();
    let stored=storage::load(&profile)?;ensure!(!stored.ui.file_dialog_takeover&&stored.tabs==saved.tabs,"takeover switch was overwritten by an existing window");
    let mut foreground_pid=0;unsafe{GetWindowThreadProcessId(GetForegroundWindow(),Some(&mut foreground_pid));}ensure!(foreground_pid!=std::process::id(),"fixture became the foreground application");
    Ok(serde_json::json!({"shared_explorer_tabs":true,"native_filter":true,"single_and_multiple_selection":true,"choose_and_cancel":true,"visible_hit_tested_actions_dpi":[96,144,192],"select_current_and_child_folders":true,"file_folder_contract_separation":true,"profile_unchanged":true,"switch_survives_existing_window_save":true,"offscreen_no_input":true,"owned_windows_destroyed":true}))
}
#[test]
#[ignore="Full shared explorer picker on owned offscreen windows without input"]
fn native_picker_reuses_explorer_tabs_filters_and_preserves_session()->Result<()>{
    use std::os::windows::fs::MetadataExt;
    let output=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("output")?);
    ensure!(output.is_absolute()&&output.parent().is_some_and(|parent|parent.parent().is_some()),"dedicated absolute output directory required");
    let root=output.join("shared-picker");
    for path in root.ancestors(){if path.exists(){ensure!(fs::symlink_metadata(path)?.file_attributes()&0x400==0,"fixture output must use real directories");}}
    fs::create_dir_all(&root)?;
    let output=root.clone();let result=thread::spawn(move||exercise(&output)).join().map_err(|_|anyhow::anyhow!("picker test panicked")).and_then(|result|result);
    let report=serde_json::json!({"checks":result.as_ref().map_err(|error|format!("{error:#}")),"created_desktop":false});fs::write(root.join("verification.current.json"),serde_json::to_vec_pretty(&report)?)?;result?;Ok(())
}
