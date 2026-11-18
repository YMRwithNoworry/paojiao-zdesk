//! Both native menu styles isolate extensions for selection and background requests.
use super::*;
use anyhow::ensure;
use std::{ffi::c_void,fs,path::Path,thread,time::{Duration,Instant}};
use crate::context_menu_process::isolation_tests::{Environment,sparse};

#[link(name="user32")]unsafe extern "system"{
    fn CreateDesktopW(name:*const u16,device:*const u16,mode:*const c_void,flags:u32,access:u32,attributes:*const c_void)->*mut c_void;
    fn SetThreadDesktop(desktop:*mut c_void)->i32;
    fn CloseDesktop(desktop:*mut c_void)->i32;
}
struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}
struct Runner{hwnd:HWND,slot:Box<Slot>}
impl Drop for Runner{fn drop(&mut self){let _=unsafe{DestroyWindow(self.hwnd)};}}
fn pump(ms:u64){let end=Instant::now()+Duration::from_millis(ms);while Instant::now()<end{
    let mut message=MSG::default();for _ in 0..256{if !unsafe{PeekMessageW(&mut message,None,0,0,PM_REMOVE)}.as_bool(){break;}if message.message!=WM_QUIT{unsafe{let _=TranslateMessage(&message);DispatchMessageW(&message);}}}thread::sleep(Duration::from_millis(3));}}
fn create(root:&Path,folder:&Path,modern:bool)->Result<Runner>{
    unsafe{InitCommonControlsEx(&INITCOMMONCONTROLSEX{dwSize:size_of::<INITCOMMONCONTROLSEX>()as u32,dwICC:ICC_STANDARD_CLASSES|ICC_BAR_CLASSES|ICC_LISTVIEW_CLASSES})}.ok()?;
    let mut session=storage::default_session();session.tabs=vec![ExplorerTab::new(1,ShellLocation::Path(folder.into()))];session.active_tab=0;session.next_id=2;
    session.ui.context_menu.mode=if modern{zsdesk_core::model::explorer::ContextMenuMode::Windows11}else{zsdesk_core::model::explorer::ContextMenuMode::Original};
    session.ui.show_preview=false;session.ui.hover_preview=false;session.ui.show_details=false;session.ui.show_tree=false;
    let slot=Box::new(RefCell::new(Some(WindowState::new(session,root.join("profile/explorer-session.json"),None,modern))));
    let instance=HINSTANCE(unsafe{GetModuleHandleW(None)?}.0);let class=windows::core::w!("ZSDesk.Native.QuickMenuRegression");
    unsafe{RegisterClassW(&WNDCLASSW{hInstance:instance,lpfnWndProc:Some(window_proc),lpszClassName:class,..Default::default()});}
    let hwnd=unsafe{CreateWindowExW(WS_EX_NOACTIVATE,class,windows::core::w!("Quick context menu regression"),WS_OVERLAPPEDWINDOW|WS_CLIPCHILDREN,100,100,1040,720,None,None,Some(instance),Some((&*slot as*const Slot).cast()))}?;
    let runner=Runner{hwnd,slot};unsafe{runner.slot.borrow_mut().as_mut().unwrap().create(hwnd)?;let _=ShowWindow(hwnd,SW_SHOWNOACTIVATE);}
    let deadline=Instant::now()+Duration::from_secs(12);
    loop{pump(25);if runner.slot.borrow().as_ref().is_some_and(|state|state.pending.is_none()&&state.browser.as_ref().is_some_and(|browser|browser.status().items==3)){break;}
        ensure!(Instant::now()<deadline,"main Shell view did not materialize sparse files");}
    Ok(runner)
}
fn exercise(root:&Path,desktop_name:&str)->Result<serde_json::Value>{
    struct Workers;
    impl Drop for Workers{fn drop(&mut self){
        crate::context_menu_process::shutdown_idle_worker();let deadline=Instant::now()+Duration::from_secs(5);
        while crate::context_menu_process::pending_workers()>0&&Instant::now()<deadline{pump(10);}
    }}
    let _workers=Workers;
    let _desktop=Environment::set("ZSDESK_MENU_TEST_DESKTOP",desktop_name);
    let _root=Environment::set("ZSDESK_MENU_ROOT",root.join("workers"));
    unsafe{OleInitialize(None)?;}let _apartment=Apartment;
    let folder=root.join("files");fs::create_dir_all(&folder)?;let(files,_,records)=sparse(&folder)?;
    let mut rows=Vec::new();
    for modern in [false,true]{
        let runner=create(&root.join(if modern{"modern"}else{"classic"}),&folder,modern)?;
        let exercised=(||->Result<()>{
        for selected in [false,true]{
            let view={let state=runner.slot.borrow();let browser=state.as_ref().unwrap().browser.as_ref().unwrap();
                browser.select_all(selected);browser.view_window().context("view HWND")?};
            pump(60);
            {let state=runner.slot.borrow();let status=state.as_ref().unwrap().browser.as_ref().unwrap().status();
                ensure!(status.selected==if selected{3}else{0},"selection/background setup failed");
                if selected{ensure!(status.selected_bytes==Some(24u64*1024*1024*1024),"large-selection size truncated");}}
            for crash in [true,false]{
                println!("context regression: modern={modern}, selected={selected}, injected_crash={crash}");
                let _probe=Environment::set("ZSDESK_MENU_TEST_PROBE",if crash{"crash"}else{"dismiss"});
                let before=zsdesk_windows::shell::menu_diagnostics();
                let mut bounds=RECT::default();unsafe{GetWindowRect(view,&mut bounds)?;}
                let position=if selected{LPARAM(-1)}else{LPARAM((((bounds.left+60)as u16 as u32)|(((bounds.bottom-40)as u16 as u32)<<16))as isize)};
                let started=Instant::now();
                ensure!(unsafe{SendMessageW(view,WM_CONTEXTMENU,Some(WPARAM(view.0 as usize)),Some(position))}.0==0,"view did not consume menu request");
                let dispatch_microseconds=started.elapsed().as_micros()as u64;
                ensure!(super::context_menu::request_active(runner.hwnd),"main view bypassed async menu worker");
                let mut pid=None;
                let deadline=Instant::now()+Duration::from_secs(15);
                while super::context_menu::request_active(runner.hwnd){
                    if let Some(value)=super::context_menu::active_pid(runner.hwnd){pid=Some(value);ensure!(value!=std::process::id(),"extension ran in main process");}
                    pump(10);ensure!(Instant::now()<deadline,"isolated menu result did not settle");
                }
                let recorded=crate::context_menu_process::read_recent_diagnostics();
                let pid=pid.or_else(||recorded["current"]["worker_pid"].as_u64().map(|value|value as u32)).context("async worker PID not recorded")?;
                ensure!(pid!=std::process::id(),"extension ran in main process");
                let after=zsdesk_windows::shell::menu_diagnostics();
                ensure!(before.presenter_creations==after.presenter_creations&&before.last_public_populate_microseconds==after.last_public_populate_microseconds,
                    "modern/classic menu constructed extension state in the main process");
                ensure!(unsafe{IsWindow(Some(runner.hwnd))}.as_bool()&&unsafe{IsWindow(Some(view))}.as_bool(),"extension crash closed main view");
                if crash{let guard=runner.slot.borrow();let status=&guard.as_ref().unwrap().status;
                    ensure!(status.contains("系统扩展菜单异常退出")&&status.contains("0xC0000005")&&status.contains("文件管理窗口可继续使用"),
                        "crash did not preserve the process failure code and recovery message: {status}");}
                rows.push(serde_json::json!({"modern_preference":modern,"target":if selected{"selection"}else{"background"},
                    "injected_crash":crash,"child_pid":pid,"parent_survived":true,"dispatch_microseconds":dispatch_microseconds,
                    "main_presenter_or_extension_created":false,"worker_rendering_exercised":false}));
            }
        }
        // Real filesystem background menus must keep view commands. ShellNew
        // completion must return to this visible view, including inline edit.
        let mut cases=vec![("background-menu","menu-dismiss",false),("view-details","background-details",false),
            ("new-menu","menu-dismiss",true),("new-folder","new-folder",true),("new-text","new-text",true),
            ("background-new-folder","new-folder",false),("background-new-text","new-text",false)];
        if modern{cases.push(("modern-background-new-folder","modern-owner-new-folder",false));}
        for (case,probe,command_bar) in cases {
            println!("context regression: modern={modern}, case={case}");
            let _probe=Environment::set("ZSDESK_MENU_TEST_PROBE",probe);
            let before:std::collections::HashSet<_>=fs::read_dir(&folder)?.map(|entry|entry.unwrap().path()).collect();
            {
                let mut slot=runner.slot.borrow_mut();let state=slot.as_mut().unwrap();
                state.browser.as_ref().unwrap().select_all(false);
                if probe=="background-details"{state.browser.as_ref().unwrap().set_view_mode(1,48)?;}
            }
            pump(30);
            if command_bar {
                unsafe{runner.slot.borrow_mut().as_mut().unwrap().command(zsdesk_ui::chrome::CommandId::New);}
            }else{
                let view=runner.slot.borrow().as_ref().unwrap().browser.as_ref().unwrap().view_window().unwrap();
                unsafe{SendMessageW(view,WM_CONTEXTMENU,Some(WPARAM(view.0 as usize)),Some(LPARAM(-1)));}
            }
            ensure!(super::context_menu::request_active(runner.hwnd),"filesystem/new menu did not reach the worker");
            let deadline=Instant::now()+Duration::from_secs(15);
            while super::context_menu::request_active(runner.hwnd){pump(10);ensure!(Instant::now()<deadline,"filesystem/new menu did not settle");}
            let diagnostic=crate::context_menu_process::read_recent_diagnostics();let current=&diagnostic["current"];
            ensure!(current["error"].is_null(),"filesystem/new menu failed ({probe}, modern={modern}): {current}");
            if probe=="menu-dismiss"{ensure!(current["timing"]["menu_open_observation"].is_string(),"filesystem popup not presented");}
            if probe=="background-details"{ensure!(runner.slot.borrow().as_ref().unwrap().browser.as_ref().unwrap().view_mode().is_some_and(|(mode,_)|mode==4),"filesystem view command lost");}
            if probe.starts_with("new-")||probe=="modern-owner-new-folder"{
                let added:Vec<_>=fs::read_dir(&folder)?.map(|entry|entry.unwrap().path()).filter(|path|!before.contains(path)).collect();
                ensure!(added.len()==1,"expected one newly created item: {added:?}");
                let deadline=Instant::now()+Duration::from_secs(10);
                loop {pump(25);let slot=runner.slot.borrow();let state=slot.as_ref().unwrap();
                    let selected=state.browser.as_ref().unwrap().selection_paths();
                    if selected.iter().any(|path|PathBuf::from(path)==added[0])&&text_input_window(unsafe{GetFocus()}){break;}
                    ensure!(Instant::now()<deadline,"created item was not selected for inline rename ({probe}): selected={selected:?}");
                }
                unsafe{SendMessageW(GetFocus(),WM_KEYDOWN,Some(WPARAM(VK_ESCAPE.0 as usize)),Some(LPARAM(0)));}pump(30);
                if added[0].is_dir(){fs::remove_dir(&added[0])?;}else{fs::remove_file(&added[0])?;}
                unsafe{runner.slot.borrow().as_ref().unwrap().browser.as_ref().unwrap().current_view()?.Refresh()?;}pump(120);
            }
            rows.push(serde_json::json!({"modern_preference":modern,"target":"filesystem/new","case":case,"probe":probe,"diagnostic":current}));
        }
        // Every new tab creates another Shell view. Repeat real selection
        // menus across tab switches and both presentation modes.
        for cycle in 0..4{
            unsafe{runner.slot.borrow_mut().as_mut().unwrap().new_tab(ShellLocation::Path(folder.clone()));}
            let deadline=Instant::now()+Duration::from_secs(12);
            loop{pump(25);if runner.slot.borrow().as_ref().is_some_and(|s|s.pending.is_none()&&s.browser.as_ref().is_some_and(|b|b.status().items==3)){break;}
                ensure!(Instant::now()<deadline,"new tab view did not settle");}
            let _probe=Environment::set("ZSDESK_MENU_TEST_PROBE","menu-dismiss");
            let view={let slot=runner.slot.borrow();let b=slot.as_ref().unwrap().browser.as_ref().unwrap();b.select_all(true);b.view_window().unwrap()};
            pump(30);unsafe{SendMessageW(view,WM_CONTEXTMENU,Some(WPARAM(view.0 as usize)),Some(LPARAM(-1)));}
            ensure!(super::context_menu::request_active(runner.hwnd),"new tab context request lost");
            let deadline=Instant::now()+Duration::from_secs(15);
            while super::context_menu::request_active(runner.hwnd){pump(10);ensure!(Instant::now()<deadline,"new tab menu stalled");}
            let diagnostic=crate::context_menu_process::read_recent_diagnostics();let current=&diagnostic["current"];
            ensure!(current["error"].is_null()&&current["timing"]["menu_open_observation"].is_string(),"new tab menu missing: {current}");
            rows.push(serde_json::json!({"modern_preference":modern,"target":"new-tab-selection","cycle":cycle,"diagnostic":current}));
        }
        // A virtual root has no filesystem path; render its actual background
        // menu repeatedly, including after a previous worker has been reused.
        unsafe{runner.slot.borrow_mut().as_mut().unwrap().navigate_to(ShellLocation::ThisPc);}
        let deadline=Instant::now()+Duration::from_secs(12);
        loop {pump(20);let slot=runner.slot.borrow();let state=slot.as_ref().unwrap();
            if state.pending.is_none()&&state.current_location()==&ShellLocation::ThisPc{break;}
            ensure!(Instant::now()<deadline,"This PC navigation did not settle");
        }
        for cycle in 0..5 {
            let command=cycle==4;
            let _probe=Environment::set("ZSDESK_MENU_TEST_PROBE",if command{"background-details"}else{"menu-dismiss"});
            if command{runner.slot.borrow().as_ref().unwrap().browser.as_ref().unwrap().set_view_mode(1,48)?;pump(20);}
            let view={let slot=runner.slot.borrow();let browser=slot.as_ref().unwrap().browser.as_ref().unwrap();browser.select_all(false);browser.view_window().context("This PC view")?};
            let mut bounds=RECT::default();unsafe{GetWindowRect(view,&mut bounds)?;}
            let position=LPARAM((((bounds.left+80)as u16 as u32)|(((bounds.bottom-60)as u16 as u32)<<16))as isize);
            unsafe{SendMessageW(view,WM_CONTEXTMENU,Some(WPARAM(view.0 as usize)),Some(position));}
            ensure!(super::context_menu::request_active(runner.hwnd),"This PC background did not reach its worker");
            let deadline=Instant::now()+Duration::from_secs(15);
            while super::context_menu::request_active(runner.hwnd){pump(10);ensure!(Instant::now()<deadline,"This PC menu did not dismiss");}
            let diagnostic=crate::context_menu_process::read_recent_diagnostics();let current=&diagnostic["current"];
            ensure!(current["error"].is_null(),"This PC background menu failed (modern={modern}, cycle={cycle}): {current}");
            if command{ensure!(runner.slot.borrow().as_ref().unwrap().browser.as_ref().unwrap().view_mode().is_some_and(|(mode,_)|mode==4),"background view command did not reach the main view: {current}");}
            else{ensure!(current["timing"]["menu_open_observation"].is_string(),"This PC background never displayed a menu (modern={modern}, cycle={cycle}): {current}");}
            rows.push(serde_json::json!({"modern_preference":modern,"target":"this-pc-background","cycle":cycle,"diagnostic":current}));
        }
        Ok(())})();
        if let Err(error)=&exercised{eprintln!("context regression failed before teardown (modern={modern}): {error:#}");}
        drop(runner);pump(30);
        crate::context_menu_process::shutdown_idle_worker();
        let deadline=Instant::now()+Duration::from_secs(5);
        while crate::context_menu_process::pending_workers()>0&&Instant::now()<deadline{pump(10);}
        ensure!(crate::context_menu_process::pending_workers()==0,"context worker remains running");
        exercised?;
    }
    drop(files);
    Ok(serde_json::json!({"selection_bytes":24u64*1024*1024*1024,"files":records,"routes":rows,
        "main_delegate_installed":true,"fixtures_removed":true,"rendering_scope":"classic/modern filesystem and This PC backgrounds; ShellNew popup and document/folder creation with visible inline rename; selection fault containment"}))
}

#[test]
#[ignore="Native quick-menu first-display measurement and selection preservation on a private desktop; requires ZSDESK_NATIVE_TEST_OUTPUT"]
fn native_quick_menu_is_displayed_without_a_shell_worker()->Result<()>{
    let output=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("output")?).join("quick-menu-latency");
    ensure!(output.is_absolute(),"absolute output required");fs::create_dir_all(&output)?;
    let name=format!("ZSDesk.QuickMenuLatency.{}",std::process::id());let before=unsafe{GetForegroundWindow()};
    let desktop=unsafe{CreateDesktopW(wide(&name).as_ptr(),std::ptr::null(),std::ptr::null(),0,0x000f00ff,std::ptr::null())};
    ensure!(!desktop.is_null(),"CreateDesktop failed");let raw=desktop as usize;let destination=output.clone();
    let result=thread::spawn(move||->Result<serde_json::Value>{
        ensure!(unsafe{SetThreadDesktop(raw as*mut c_void)}!=0,"SetThreadDesktop failed");unsafe{OleInitialize(None)}?;let _apartment=Apartment;
        let _desktop=Environment::set("ZSDESK_MENU_TEST_DESKTOP",&name);let _probe=Environment::set("ZSDESK_MENU_TEST_PROBE","quick-dismiss");
        let root=destination.join(format!("run-{}-{}",std::process::id(),unsafe{windows::Win32::System::Threading::GetCurrentThreadId()}));
        let folder=root.join("files");fs::create_dir_all(&folder)?;
        struct Fixture(PathBuf);impl Drop for Fixture{fn drop(&mut self){let _=fs::remove_dir_all(&self.0);}}let _fixture=Fixture(root.clone());
        for item in ["one.txt","two.txt","three.txt"]{fs::write(folder.join(item),b"quick menu latency fixture")?;}
        let runner=create(&root,&folder,true)?;let mut rows=Vec::new();
        for selected in [false,true]{
            let (view,window,expected)={let slot=runner.slot.borrow();let browser=slot.as_ref().unwrap().browser.as_ref().unwrap();
                browser.select_all(false);if selected{browser.select_children(&[std::ffi::OsString::from("one.txt")])?;}
                let view=unsafe{browser.current_view()}?;let window=browser.view_window().context("quick-menu source view")?;
                let expected=super::context_menu::selected_identities(&view)?;(view,window,expected)};
            pump(25);let mut bounds=RECT::default();unsafe{GetWindowRect(window,&mut bounds)?;}
            let point=POINT{x:bounds.left+65,y:bounds.top+45};super::QUICK_MENU_ENTERED.with(|entered|entered.set(None));
            let started=Instant::now();super::context_menu::show(window,&view,window,
                LPARAM(((point.x as u32&0xffff)|((point.y as u32&0xffff)<<16))as isize),
                if selected{zsdesk_windows::shell::menu::ViewMenuTarget::Selection}else{zsdesk_windows::shell::menu::ViewMenuTarget::Background})?;
            let elapsed=started.elapsed();let displayed=super::QUICK_MENU_ENTERED.with(|value|value.get());
            ensure!(elapsed>=Duration::from_millis(50)&&elapsed<Duration::from_secs(2),"quick popup did not complete its bounded tracking loop: {elapsed:?}");
            let displayed=displayed.context("quick popup never entered the native menu loop")?;
            ensure!(!super::context_menu::request_active(runner.hwnd),"quick menu dismissal started a Shell worker");
            let actual=super::context_menu::selected_identities(&view)?;
            ensure!(zsdesk_windows::shell::menu::same_item_identities(&expected,&actual),"quick popup changed the visible selection");
            rows.push(serde_json::json!({"target":if selected{"filesystem-selection"}else{"filesystem-background"},
                "time_to_native_menu_loop_microseconds":displayed.duration_since(started).as_micros()as u64,
                "popup_dismissed_microseconds":elapsed.as_micros()as u64,"worker_started":false,"selection_unchanged":true}));
        }
        drop(runner);pump(30);Ok(serde_json::json!({"state":"passed","checks":rows}))
    }).join().map_err(|_|anyhow::anyhow!("native quick menu test panicked")).and_then(|value|value);
    let deadline=Instant::now()+Duration::from_secs(3);let closed=loop{if unsafe{CloseDesktop(desktop)}!=0{break true;}if Instant::now()>=deadline{break false;}thread::sleep(Duration::from_millis(20));};
    let foreground_unchanged=before==unsafe{GetForegroundWindow()};
    let report=serde_json::json!({"state":if result.is_ok()&&closed&&foreground_unchanged{"passed"}else{"failed"},
        "checks":result.as_ref().map_err(|error|format!("{error:#}")),"desktop_closed":closed,"foreground_unchanged":foreground_unchanged});
    fs::write(output.join("verification.current.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");
    result?;ensure!(closed&&foreground_unchanged,"quick menu desktop/focus cleanup failed");Ok(())
}

#[test]
#[ignore="Main view classic/modern selection/background isolation; requires ZSDESK_NATIVE_TEST_OUTPUT"]
fn native_main_window_background_and_selection_menu_isolation()->Result<()>{
    let root=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("output")?).join("main-context-menu");ensure!(root.is_absolute(),"absolute output required");fs::create_dir_all(&root)?;
    let before=unsafe{GetForegroundWindow()};let name=format!("ZSDesk.MainContextMenu.{}",std::process::id());let desktop=unsafe{CreateDesktopW(wide(&name).as_ptr(),std::ptr::null(),std::ptr::null(),0,0x000f00ff,std::ptr::null())};ensure!(!desktop.is_null(),"CreateDesktop failed");let raw=desktop as usize;let destination=root.clone();
    let result=thread::spawn(move||->Result<serde_json::Value>{ensure!(unsafe{SetThreadDesktop(raw as*mut c_void)}!=0,"SetThreadDesktop failed");exercise(&destination,&name)}).join().map_err(|_|anyhow::anyhow!("main quick menu test panicked")).and_then(|value|value);
    // Shell releases auxiliary STA windows after the owning UI thread exits.
    let deadline=Instant::now()+Duration::from_secs(3);
    let closed=loop{if unsafe{CloseDesktop(desktop)}!=0{break true;}if Instant::now()>=deadline{break false;}thread::sleep(Duration::from_millis(20));};
    let current=unsafe{GetForegroundWindow()};let mut process=0;unsafe{GetWindowThreadProcessId(current,Some(&mut process));}
    let isolated=process!=std::process::id();let report=serde_json::json!({"state":if result.is_ok()&&closed&&isolated{"passed"}else{"failed"},"checks":result.as_ref().map_err(|error|format!("{error:#}")),"desktop_closed":closed,"foreground_unchanged":before==current,"test_did_not_activate":isolated});
    fs::write(root.join("verification.current.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");result?;ensure!(closed&&isolated,"desktop cleanup/focus failed");Ok(())
}

#[test]
#[ignore="Native/classic handoff with source selection and screen-anchor verification on a private desktop"]
fn native_more_options_preserves_source_selection_and_anchor()->Result<()>{
    use windows::core::Interface;
    use windows::Win32::UI::Shell::IFolderView2;
    use zsdesk_windows::shell::{file_transfer,menu::{self,ViewMenuTarget}};
    fn keys(pidls:&[Vec<u8>])->Result<Vec<Vec<u16>>>{
        if pidls.is_empty(){return Ok(Vec::new());}
        let mut values=file_transfer::paths_from_pidls(pidls)?.iter().map(|path|ExplorerBrowserHost::search_path_key(path)).collect::<Vec<_>>();
        values.sort();Ok(values)
    }
    fn request(root:&Path,owner:HWND,point:POINT,targets:&[Vec<u8>])->Result<Option<(PathBuf,serde_json::Value)>>{
        let wanted=keys(targets)?;
        for entry in fs::read_dir(root)?.flatten(){
            let path=entry.path().join("request.json");let Ok(bytes)=fs::read(path)else{continue;};
            let Ok(value)=serde_json::from_slice::<serde_json::Value>(&bytes)else{continue;};
            if value["owner"].as_i64()!=Some(owner.0 as isize as i64)||value["point"]!=serde_json::json!([point.x,point.y]){continue;}
            let Ok(pidls)=serde_json::from_value::<Vec<Vec<u8>>>(value["pidls"].clone())else{continue;};
            if keys(&pidls)?==wanted{return Ok(Some((entry.path(),value)));}
        }Ok(None)
    }
    fn status(workspace:&Path,token:&str)->Option<serde_json::Value>{
        let bytes=fs::read(workspace.join("status.json")).ok()?;
        let value:serde_json::Value=serde_json::from_slice(&bytes).ok()?;
        (value["token"].as_str()==Some(token)).then_some(value)
    }
    #[derive(Default)]struct Popup{pid:u32,rectangle:Option<RECT>}
    unsafe extern "system" fn find_classic(window:HWND,data:LPARAM)->windows::core::BOOL{
        let found=unsafe{&mut*(data.0 as*mut Popup)};let mut pid=0;unsafe{GetWindowThreadProcessId(window,Some(&mut pid));}
        if pid==found.pid&&unsafe{IsWindowVisible(window)}.as_bool(){
            let mut class=[0u16;64];let length=unsafe{GetClassNameW(window,&mut class)}.max(0)as usize;
            if String::from_utf16_lossy(&class[..length])=="#32768"{let mut rect=RECT::default();if unsafe{GetWindowRect(window,&mut rect)}.is_ok(){found.rectangle=Some(rect);}}
        }true.into()
    }
    let root=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("output")?).join("more-options-source");fs::create_dir_all(&root)?;
    let name=format!("ZSDesk.MoreOptionsSource.{}",std::process::id());
    let desktop=unsafe{CreateDesktopW(wide(&name).as_ptr(),std::ptr::null(),std::ptr::null(),0,0x000f00ff,std::ptr::null())};ensure!(!desktop.is_null(),"private desktop");
    let raw=desktop as usize;let target=root.clone();let foreground=unsafe{GetForegroundWindow()};
    let result=thread::spawn(move||->Result<serde_json::Value>{
        ensure!(unsafe{SetThreadDesktop(raw as*mut c_void)}!=0,"private desktop attach");unsafe{OleInitialize(None)}?;let _apartment=Apartment;
        struct Workers;impl Drop for Workers{fn drop(&mut self){crate::context_menu_process::shutdown_idle_worker();let until=Instant::now()+Duration::from_secs(5);while crate::context_menu_process::pending_workers()>0&&Instant::now()<until{pump(10);}}}let _workers=Workers;
        let _desktop=Environment::set("ZSDESK_MENU_TEST_DESKTOP",&name);
        let workers=target.join("workers");fs::create_dir_all(&workers)?;let _root=Environment::set("ZSDESK_MENU_ROOT",&workers);
        let _probe=Environment::set("ZSDESK_MENU_TEST_PROBE","modern-expand");
        let files=target.join("files");fs::create_dir_all(&files)?;
        for name in ["one.txt","two.txt","three.txt"]{fs::write(files.join(name),b"menu handoff fixture")?;}
        let runner=create(&target,&files,true)?;let mut checks=Vec::new();
        for count in [0usize,1,2]{for keyboard in [false,true]{
            let (view,window)={let slot=runner.slot.borrow();let browser=slot.as_ref().unwrap().browser.as_ref().unwrap();
                browser.select_all(false);if count>0{browser.select_children(&["one.txt","two.txt"][..count].iter().map(|name|std::ffi::OsString::from(*name)).collect::<Vec<_>>())?;}
                (unsafe{browser.current_view()}?,browser.view_window().context("source view")?)};
            pump(35);
            let expected=super::context_menu::selected_identities(&view)?;ensure!(expected.len()==count,"fixture selection count");
            let folder:IFolderView2=view.cast()?;let focused=unsafe{folder.GetFocusedItem()}.ok();
            let mut bounds=RECT::default();unsafe{GetWindowRect(window,&mut bounds)}?;
            let mouse=POINT{x:bounds.left+150,y:bounds.top+70};
            let position=if keyboard{LPARAM(-1)}else{LPARAM(((mouse.x as u32&0xffff)|((mouse.y as u32&0xffff)<<16))as isize)};
            let anchor=menu::context_menu_point(&view,position)?;
            let target_kind=if count==0{ViewMenuTarget::Background}else{ViewMenuTarget::Selection};
            let targets=if count==0{vec![zsdesk_windows::shell::pidl::location_pidl(&ShellLocation::Path(files.clone())).context("background PIDL")?]}else{expected.clone()};
            super::context_menu::show(window,&view,window,position,target_kind)?;
            ensure!(super::context_menu::request_active(runner.hwnd),"menu request not active");
            let mut frozen=None;let mut popup=None;let mut restored=false;let mut cleared=false;let mut worker_owner=None;let mut worker_pid=None;
            let until=Instant::now()+Duration::from_secs(20);
            while super::context_menu::request_active(runner.hwnd){
                if frozen.is_none(){frozen=request(&workers,runner.hwnd,anchor,&targets)?;}
                if let Some((workspace,request))=&frozen{
                    if let Some(value)=request["token"].as_str().and_then(|token|status(workspace,token)){
                        if let Some(owner)=value["window"].as_u64().filter(|owner|*owner!=0){worker_owner=Some(owner);}
                    }
                }
                if let Some(pid)=super::context_menu::active_pid(runner.hwnd){
                    worker_pid=Some(pid);
                    let mut found=Popup{pid,..Default::default()};let _=unsafe{EnumWindows(Some(find_classic),LPARAM((&mut found as*mut Popup)as isize))};
                    if let Some(rect)=found.rectangle{popup=Some(rect);
                        if count>0&&!cleared{
                            // Model DefView's asynchronous highlight clear at
                            // the native/classic transition, without user input.
                            unsafe{view.SelectItem(std::ptr::null(),windows::Win32::UI::Shell::SVSI_DESELECTOTHERS.0 as u32)}?;
                            ensure!(super::context_menu::selected_identities(&view)?.is_empty(),"clear fixture did not clear");
                            cleared=true;super::context_menu::poll(runner.hwnd);
                            restored=keys(&super::context_menu::selected_identities(&view)?)?==keys(&expected)?;
                        }
                    }
                }
                pump(5);ensure!(Instant::now()<until,"menu handoff timed out");
            }
            let (workspace,frozen)=frozen.context("immutable worker request not observed")?;
            ensure!(frozen["view_context"]["keyboard"]==keyboard,"keyboard origin changed");
            let token=frozen["token"].as_str().context("request token")?;
            let until=Instant::now()+Duration::from_secs(5);
            let finished=loop{
                if let Some(value)=status(&workspace,token){if matches!(value["phase"].as_str(),Some("idle"|"done")){break value;}}
                ensure!(Instant::now()<until,"request {token} has no matching terminal status");pump(5);
            };
            ensure!(finished["pid"].as_u64()==worker_pid.map(u64::from)&&finished["error"].is_null()&&finished["before_invoke"]==false&&finished["outcome"]=="Dismissed","expansion executed or failed: {finished}");
            // The retained diagnostic intentionally contains only the first
            // 24 stderr lines. A reused worker's later menus require its own
            // token-matched terminal status and the full bounded fixture log.
            let log=workspace.join("worker.stderr.log");ensure!(fs::metadata(&log)?.len()<=1024*1024,"fixture log exceeds bound");
            let all_stderr=fs::read_to_string(log)?;
            let stderr=&all_stderr[all_stderr.rfind("expand anchor=").context("current expansion trace missing")?..];
            let classic=stderr.lines().filter(|line|line.starts_with("classic-popup tracking ")).last().unwrap_or_default();
            let expanded=stderr.lines().filter(|line|line.starts_with("expand anchor=")).last().unwrap_or_default();
            ensure!(classic==format!("classic-popup tracking owner={} anchor={},{}",worker_owner.context("worker owner window not observed")?,anchor.x,anchor.y),"classic menu lost original owner/anchor: {stderr}");
            ensure!(expanded.contains(&format!("anchor={},{}",anchor.x,anchor.y))&&expanded.ends_with(&format!("callback={},{}",anchor.x.saturating_add(43),anchor.y.saturating_add(89))),"offset expansion callback not exercised: {stderr}");
            ensure!(popup.is_some(),"classic popup was not observed");
            ensure!(count==0||cleared&&restored,"source highlight was not restored");
            ensure!(keys(&super::context_menu::selected_identities(&view)?)?==keys(&expected)?,"source selection changed across expansion");
            if count>0{ensure!(unsafe{folder.GetFocusedItem()}.ok()==focused,"focused source row changed");}
            let rect=popup.unwrap();checks.push(serde_json::json!({"selected":count,"keyboard":keyboard,"request_token":token,"worker_pid":worker_pid,"terminal_phase":finished["phase"],"anchor":[anchor.x,anchor.y],"classic_rect":[rect.left,rect.top,rect.right,rect.bottom],"visual_selection_restored":restored,"target_pidls_verified":true,"command_executed":false,"rebuilt_menu":stderr.contains("rebuilt=true")}));
        }}
        // A real new selection must not be overwritten by the saved selection.
        let _probe=Environment::set("ZSDESK_MENU_TEST_PROBE","dismiss");
        let (view,window)={let slot=runner.slot.borrow();let browser=slot.as_ref().unwrap().browser.as_ref().unwrap();browser.select_children(&["one.txt".into()])?;(unsafe{browser.current_view()}?,browser.view_window().unwrap())};
        super::context_menu::show(window,&view,window,LPARAM(-1),ViewMenuTarget::Selection)?;
        {let slot=runner.slot.borrow();slot.as_ref().unwrap().browser.as_ref().unwrap().select_children(&["three.txt".into()])?;}
        let replacement=super::context_menu::selected_identities(&view)?;super::context_menu::poll(runner.hwnd);
        ensure!(keys(&super::context_menu::selected_identities(&view)?)?==keys(&replacement)?,"replacement selection was overwritten");
        super::context_menu::close(runner.hwnd);
        // An observed source input cancels restoration of a now-empty selection.
        {let slot=runner.slot.borrow();slot.as_ref().unwrap().browser.as_ref().unwrap().select_children(&["one.txt".into()])?;}
        super::context_menu::show(window,&view,window,LPARAM(-1),ViewMenuTarget::Selection)?;
        unsafe{PostMessageW(Some(runner.hwnd),WM_LBUTTONDOWN,WPARAM(0),LPARAM(0))}?;let mut message=MSG::default();
        ensure!(unsafe{PeekMessageW(&mut message,Some(runner.hwnd),WM_LBUTTONDOWN,WM_LBUTTONDOWN,PM_REMOVE)}.as_bool(),"source input not observed");
        unsafe{view.SelectItem(std::ptr::null(),windows::Win32::UI::Shell::SVSI_DESELECTOTHERS.0 as u32)}?;super::context_menu::poll(runner.hwnd);
        ensure!(super::context_menu::selected_identities(&view)?.is_empty()&&!super::context_menu::request_active(runner.hwnd),"user-cleared selection was restored");
        drop(runner);pump(20);Ok(serde_json::json!({"cases":checks,"replacement_selection_preserved":true,"user_clear_not_restored":true,"clipboard_untouched":true}))
    }).join().map_err(|_|anyhow::anyhow!("source menu regression panicked")).and_then(|value|value);
    let closed=unsafe{CloseDesktop(desktop)}!=0;
    let report=serde_json::json!({"state":if result.is_ok()&&closed{"passed"}else{"failed"},"checks":result.as_ref().map_err(|error|format!("{error:#}")),"desktop_closed":closed,"foreground_unchanged":foreground==unsafe{GetForegroundWindow()}});
    fs::write(root.join("verification.current.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");result?;ensure!(closed,"private desktop retained");Ok(())
}

#[test]
#[ignore="Blank double-click navigation and folder-search scope in a private desktop"]
fn native_blank_double_click_and_folder_search_scope()->Result<()>{
    let root=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("output")?).join("blank-doubleclick");fs::create_dir_all(&root)?;
    let name=format!("ZSDesk.BlankClick.{}",std::process::id());let desktop=unsafe{CreateDesktopW(wide(&name).as_ptr(),std::ptr::null(),std::ptr::null(),0,0x000f00ff,std::ptr::null())};ensure!(!desktop.is_null(),"private desktop");
    let raw=desktop as usize;let target=root.clone();
    let result=thread::spawn(move||->Result<()>{
        ensure!(unsafe{SetThreadDesktop(raw as*mut c_void)}!=0,"private desktop attach");unsafe{OleInitialize(None)?;}let _apartment=Apartment;
        let folder=target.join("files");fs::create_dir_all(&folder)?;for name in ["one.txt","two.txt","three.txt"]{fs::write(folder.join(name),b"blank click fixture")?;}
        let runner=create(&target,&folder,false)?;super::pane_input::native_blank_double_click(&runner.slot,&target,pump)?;
        use zsdesk_core::model::explorer::NewTabLocation;
        let fixed=target.join("fixed-tab");fs::create_dir_all(&fixed)?;
        for (policy,expected) in [(NewTabLocation::CurrentFolder,ShellLocation::Path(folder.clone())),
            (NewTabLocation::ThisPc,ShellLocation::ThisPc),(NewTabLocation::Folder(fixed.clone()),ShellLocation::Path(fixed))]{
            {let slot=runner.slot.borrow();let state=slot.as_ref().unwrap();storage::save_new_tab_location(&state.config_path,&policy)?;}
            let before=runner.slot.borrow().as_ref().unwrap().session.tabs.len();
            unsafe{SendMessageW(runner.hwnd,WM_COMMAND,Some(WPARAM(Command::NewTab.id()as usize)),Some(LPARAM(0)));}
            let deadline=Instant::now()+Duration::from_secs(12);
            loop{pump(20);let slot=runner.slot.borrow();let state=slot.as_ref().unwrap();
                if state.pending.is_none()&&state.queued.is_none()&&state.current_location()==&expected{
                    ensure!(state.session.tabs.len()==before+1,"new-tab command did not add exactly one tab");break;}
                ensure!(Instant::now()<deadline,"new-tab policy did not navigate to {}",expected.address());}
        }
        drop(runner);pump(30);Ok(())
    }).join().map_err(|_|anyhow::anyhow!("blank navigation panicked")).and_then(|result|result);
    let closed=unsafe{CloseDesktop(desktop)}!=0;
    let report=serde_json::json!({"state":if result.is_ok()&&closed{"passed"}else{"failed"},"checks":result.as_ref().map_err(|error|format!("{error:#}")),"desktop_closed":closed,"blank_double_click_enabled_and_disabled":result.is_ok(),"search_scope_follows_folder":result.is_ok(),"new_tab_this_pc_current_and_fixed_folder":result.is_ok()});
    fs::write(root.join("verification.current.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");result?;ensure!(closed,"desktop retained");Ok(())
}
