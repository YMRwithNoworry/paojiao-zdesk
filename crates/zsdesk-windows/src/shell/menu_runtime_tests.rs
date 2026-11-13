//! Real mixed popup dispatch and Shell copy, isolated from the input desktop.
use super::*;
use anyhow::{Context,Result,ensure};
use std::{cell::Cell,ffi::c_void,path::PathBuf,ptr,thread};
use windows::{core::w,Win32::{Foundation::HINSTANCE,System::{LibraryLoader::GetModuleHandleW,Ole::{OleInitialize,OleUninitialize,OleGetClipboard,OleSetClipboard},DataExchange::GetClipboardSequenceNumber},UI::{Input::KeyboardAndMouse::{SetActiveWindow,SetFocus},WindowsAndMessaging::*}}};
#[link(name="user32")]unsafe extern "system"{
    fn CreateDesktopW(name:*const u16,device:*const u16,mode:*const c_void,flags:u32,access:u32,attributes:*const c_void)->*mut c_void;
    fn SetThreadDesktop(desktop:*mut c_void)->i32;
    fn CloseDesktop(desktop:*mut c_void)->i32;
}
thread_local!{static CHOOSE_LAST:Cell<bool>=const{Cell::new(false)};static MENU_EVENTS:Cell<u32>=const{Cell::new(0)};}
unsafe extern "system" fn procedure(hwnd:HWND,message:u32,wp:WPARAM,lp:LPARAM)->LRESULT{
    if let Some(result)=forward_menu_message(message,wp,lp){return result;}
    match message{
        WM_ENTERMENULOOP=>{MENU_EVENTS.with(|v|v.set(v.get()+1));unsafe{SetTimer(Some(hwnd),1,200,None);SetTimer(Some(hwnd),2,3000,None);}LRESULT(0)}
        WM_TIMER if wp.0==1=>{
            MENU_EVENTS.with(|v|v.set(v.get()+10));
            let _=unsafe{KillTimer(Some(hwnd),1)};
            let key=if CHOOSE_LAST.with(Cell::get){b'c'}else{b'z'};
            let _=unsafe{PostMessageW(Some(hwnd),WM_CHAR,WPARAM(key as usize),LPARAM(1))};LRESULT(0)
        }
        WM_TIMER if wp.0==2=>{let _=unsafe{KillTimer(Some(hwnd),2)};let _=unsafe{EndMenu()};LRESULT(0)}
        _=>unsafe{DefWindowProcW(hwnd,message,wp,lp)},
    }
}
struct Apartment;
impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}
struct Owner(HWND);
impl Drop for Owner{fn drop(&mut self){let _=unsafe{DestroyWindow(self.0)};}}
struct Clipboard{previous:windows::Win32::System::Com::IDataObject,sequence:Option<u32>}
impl Drop for Clipboard{fn drop(&mut self){if self.sequence==Some(unsafe{GetClipboardSequenceNumber()}){let _=unsafe{OleSetClipboard(&self.previous)};}}}
fn exercise(output:&std::path::Path)->Result<serde_json::Value>{
    unsafe{OleInitialize(None)?;}let _apartment=Apartment;
    let file=output.join("menu-copy-中文.txt");std::fs::write(&file,b"Native menu fixture")?;
    let identifier=super::super::pidl::location_pidl(&zsdesk_core::model::explorer::ShellLocation::Path(file.clone())).context("fixture PIDL")?;
    let instance=HINSTANCE(unsafe{GetModuleHandleW(None)?}.0);
    unsafe{RegisterClassW(&WNDCLASSW{hInstance:instance,lpfnWndProc:Some(procedure),lpszClassName:w!("ZSDesk.MixedMenuRegression"),..Default::default()});}
    let owner=Owner(unsafe{CreateWindowExW(WINDOW_EX_STYLE::default(),w!("ZSDesk.MixedMenuRegression"),w!("Mixed menu regression"),WS_OVERLAPPEDWINDOW,20,20,500,400,None,None,Some(instance),None)?});
    unsafe{let _=ShowWindow(owner.0,SW_SHOW);let _=SetActiveWindow(owner.0);let _=SetFocus(Some(owner.0));let _=SetForegroundWindow(owner.0);}
    let prior=context_menu_options();let mut options=ContextMenuOptions::default();options.modern=true;options.show_wps=false;set_context_menu_options(options);
    let result=(||->Result<serde_json::Value>{
        let previous=unsafe{OleGetClipboard()}.context("cannot preserve clipboard")?;
        let mut clipboard=Clipboard{previous,sequence:None};
        let mut outcomes=Vec::new();
        for choose_shell in [false,true]{
            let menu=ShellMenu::for_items(owner.0,&[identifier.clone()])?;menu.populate(false,false)?;
            let count=unsafe{GetMenuItemCount(Some(menu.hmenu()))};
            let copy=(0..count).map(|index|unsafe{GetMenuItemID(menu.hmenu(),index)}).find(|id|menu.verb(*id).is_some_and(|verb|verb.eq_ignore_ascii_case("copy"))).context("copy verb")?;
            for index in (0..count).rev(){if unsafe{GetMenuItemID(menu.hmenu(),index)}!=copy{unsafe{DeleteMenu(menu.hmenu(),index as u32,MF_BYPOSITION)?;}}}
            unsafe{InsertMenuW(menu.hmenu(),0,MF_BYPOSITION|MF_STRING,APP_FIRST as usize,w!("应用命令(&Z)"))?;}
            CHOOSE_LAST.with(|last|last.set(choose_shell));
            let outcome=menu.track_selected(owner.0,POINT{x:70,y:100},&[identifier.clone()],1,MenuInterceptions::default())?;
            let _=unsafe{KillTimer(Some(owner.0),1)};let _=unsafe{KillTimer(Some(owner.0),2)};
            if choose_shell{
                ensure!(outcome==MenuOutcome::ShellInvoked,"wrong Shell outcome: {outcome:?}");
                let copied=unsafe{OleGetClipboard()}?;let paths=crate::ole::hdrop_paths(&copied);
                if paths==vec![file.clone()]{clipboard.sequence=Some(unsafe{GetClipboardSequenceNumber()});}
                ensure!(paths==vec![file.clone()],"copy did not use the selected fixture");
            }else{ensure!(outcome==MenuOutcome::Application(APP_FIRST),"wrong application outcome: {outcome:?}, events={}",MENU_EVENTS.with(Cell::get));}
            outcomes.push(format!("{outcome:?}"));
        }
        ensure!(std::fs::read(&file)?==b"Native menu fixture","fixture content changed");
        let restored=clipboard.sequence==Some(unsafe{GetClipboardSequenceNumber()});drop(clipboard);
        Ok(serde_json::json!({"outcomes":outcomes,"native_popup_keyboard_selection":true,"shell_copy_paths_verified":true,"clipboard_restore_attempted":restored,"fixture_unchanged":true,"modern_preference":true}))
    })();
    set_context_menu_options(prior);drop(owner);result
}
#[test]
#[ignore="Real mixed native menu test; requires ZSDESK_NATIVE_TEST_OUTPUT and temporarily preserves/restores clipboard"]
fn native_mixed_fence_menu_routes_application_and_shell_commands()->Result<()>{
    let output=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("output")?).join("mixed-menu");ensure!(output.is_absolute(),"absolute output required");std::fs::create_dir_all(&output)?;
    let before=unsafe{GetForegroundWindow()};let name:Vec<u16>=format!("ZSDesk.MixedMenu.{}",std::process::id()).encode_utf16().chain(Some(0)).collect();
    let desktop=unsafe{CreateDesktopW(name.as_ptr(),ptr::null(),ptr::null(),0,0x000f00ff,ptr::null())};ensure!(!desktop.is_null(),"CreateDesktop failed");
    let raw=desktop as usize;let target=output.clone();
    let result=thread::spawn(move||->Result<serde_json::Value>{ensure!(unsafe{SetThreadDesktop(raw as*mut c_void)}!=0,"SetThreadDesktop failed");exercise(&target)}).join().map_err(|_|anyhow::anyhow!("menu regression panicked")).and_then(|r|r);
    let closed=unsafe{CloseDesktop(desktop)}!=0;let unchanged=before==unsafe{GetForegroundWindow()};
    let report=serde_json::json!({"state":if result.is_ok()&&closed&&unchanged{"passed"}else{"failed"},"checks":result.as_ref().map_err(|e|format!("{e:#}")),"desktop_closed":closed,"foreground_unchanged":unchanged});
    std::fs::write(output.join("verification.current.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");result?;ensure!(closed&&unchanged,"desktop cleanup/focus failed");Ok(())
}

// Exercise the file manager's actual IShellView subclass, rather than the
// fence's independent PIDL context menu. Timing excludes synthetic user dwell.
mod view_menu {
    use super::*;
    use std::time::{Duration,Instant};
    use windows::{core::{implement,BOOL,GUID,HRESULT,Ref},Win32::{Foundation::RECT,
        System::Ole::IOleWindow_Impl,UI::{Controls::LPFNSVADDPROPSHEETPAGE,
        Shell::{IShellView_Impl,IShellBrowser,FOLDERSETTINGS,_SVGIO}}}};
    use crate::shell::{ExplorerBrowserHost,Panes,WM_SHELL_NAVIGATED,WM_SHELL_NAVIGATION_FAILED,
        WM_SHELL_FOLDER_INVOKED,WM_SHELL_SELECTION_CHANGED,acknowledge_selection_changed};
    use zsdesk_core::model::explorer::ShellLocation;

    const PROBE_SUBCLASS:usize=0x5a534d54;
    const CLOSE_TIMER:usize=0x5a53;
    const WATCHDOG_TIMER:usize=0x5a54;
    thread_local! {
        static NAVIGATED:Cell<bool>=const{Cell::new(false)};
        static NAV_FAILED:Cell<bool>=const{Cell::new(false)};
        static ENTERED:Cell<Option<Instant>>=const{Cell::new(None)};
        static CONTEXT_REQUESTS:Cell<u32>=const{Cell::new(0)};
        static FALLBACK_REQUESTS:Cell<u32>=const{Cell::new(0)};
        static FAILURES:Cell<u32>=const{Cell::new(0)};
        static FAILED_CODE:Cell<u32>=const{Cell::new(0)};
        static DELEGATE_CALLS:Cell<u32>=const{Cell::new(0)};
    }
    unsafe extern "system" fn owner_proc(hwnd:HWND,msg:u32,wp:WPARAM,lp:LPARAM)->LRESULT{
        if msg==WM_SHELL_NAVIGATED||msg==WM_SHELL_FOLDER_INVOKED{
            if lp.0!=0{unsafe{drop(Box::from_raw(lp.0 as*mut ShellLocation));}}
            if msg==WM_SHELL_NAVIGATED{NAVIGATED.with(|value|value.set(true));}
            return LRESULT(0);
        }
        if msg==WM_SHELL_NAVIGATION_FAILED{NAV_FAILED.with(|value|value.set(true));return LRESULT(0);}
        if msg==WM_SHELL_SELECTION_CHANGED{unsafe{acknowledge_selection_changed(lp.0)};return LRESULT(0);}
        if msg==failure_message(){FAILURES.with(|value|value.set(value.get()+1));FAILED_CODE.with(|value|value.set(wp.0 as u32));return LRESULT(0);}
        if msg==WM_CONTEXTMENU{FALLBACK_REQUESTS.with(|value|value.set(value.get()+1));return LRESULT(1);}
        unsafe{DefWindowProcW(hwnd,msg,wp,lp)}
    }
    unsafe extern "system" fn probe_proc(hwnd:HWND,msg:u32,wp:WPARAM,lp:LPARAM,_id:usize,_data:usize)->LRESULT{
        if msg==WM_CONTEXTMENU{CONTEXT_REQUESTS.with(|value|value.set(value.get()+1));}
        if msg==WM_ENTERMENULOOP{ENTERED.with(|value|value.set(Some(Instant::now())));unsafe{SetTimer(Some(hwnd),CLOSE_TIMER,40,None);}}
        if msg==WM_TIMER&&(wp.0==CLOSE_TIMER||wp.0==WATCHDOG_TIMER){let _=unsafe{KillTimer(Some(hwnd),wp.0)};let _=unsafe{EndMenu()};return LRESULT(0);}
        if msg==WM_NCDESTROY{let _=unsafe{RemoveWindowSubclass(hwnd,Some(probe_proc),PROBE_SUBCLASS)};}
        unsafe{DefSubclassProc(hwnd,msg,wp,lp)}
    }
    fn pump(ms:u64){
        let end=Instant::now()+Duration::from_millis(ms);
        while Instant::now()<end{
            let mut message=MSG::default();
            for _ in 0..128{if !unsafe{PeekMessageW(&mut message,None,0,0,PM_REMOVE)}.as_bool(){break;}
                if message.message!=WM_QUIT{unsafe{let _=TranslateMessage(&message);DispatchMessageW(&message);}}}
            thread::sleep(Duration::from_millis(3));
        }
    }
    struct Preferences(ContextMenuOptions);
    impl Drop for Preferences{fn drop(&mut self){set_context_menu_options(self.0.clone());}}
    struct Probe(HWND);
    impl Drop for Probe{fn drop(&mut self){let _=unsafe{KillTimer(Some(self.0),CLOSE_TIMER)};let _=unsafe{KillTimer(Some(self.0),WATCHDOG_TIMER)};let _=unsafe{RemoveWindowSubclass(self.0,Some(probe_proc),PROBE_SUBCLASS)};}}
    fn point_parameter(point:POINT)->LPARAM{LPARAM(((point.x as u32&0xffff)|((point.y as u32&0xffff)<<16))as isize)}

    // A failing Shell provider exercises the real subclass and error message
    // route deterministically; it does not depend on an installed bad extension.
    #[implement(IShellView)]
    struct FailedView{owner:isize,calls:Rc<Cell<u32>>}
    fn rejected<T>()->windows::core::Result<T>{Err(windows::core::Error::from_hresult(HRESULT(0x80004005u32 as i32)))}
    impl IOleWindow_Impl for FailedView_Impl{
        fn GetWindow(&self)->windows::core::Result<HWND>{Ok(HWND(self.owner as*mut _))}
        fn ContextSensitiveHelp(&self,_:BOOL)->windows::core::Result<()>{rejected()}
    }
    impl IShellView_Impl for FailedView_Impl{
        fn TranslateAccelerator(&self,_:*const MSG)->HRESULT{HRESULT(1)}
        fn EnableModeless(&self,_:BOOL)->windows::core::Result<()>{rejected()}
        fn UIActivate(&self,_:u32)->windows::core::Result<()>{rejected()}
        fn Refresh(&self)->windows::core::Result<()>{rejected()}
        fn CreateViewWindow(&self,_:Ref<IShellView>,_:*const FOLDERSETTINGS,_:Ref<IShellBrowser>,_:*const RECT)->windows::core::Result<HWND>{rejected()}
        fn DestroyViewWindow(&self)->windows::core::Result<()>{Ok(())}
        fn GetCurrentInfo(&self)->windows::core::Result<FOLDERSETTINGS>{rejected()}
        fn AddPropertySheetPages(&self,_:u32,_:LPFNSVADDPROPSHEETPAGE,_:LPARAM)->windows::core::Result<()>{rejected()}
        fn SaveViewState(&self)->windows::core::Result<()>{rejected()}
        fn SelectItem(&self,_:*const ITEMIDLIST,_:u32)->windows::core::Result<()>{rejected()}
        fn GetItemObject(&self,_:&_SVGIO,_:*const GUID,result:*mut *mut c_void)->windows::core::Result<()>{
            self.calls.set(self.calls.get()+1);if !result.is_null(){unsafe{*result=ptr::null_mut();}}rejected()
        }
    }
    fn failure_route(instance:HINSTANCE)->Result<serde_json::Value>{
        let owner=Owner(unsafe{CreateWindowExW(WINDOW_EX_STYLE::default(),w!("ZSDesk.ViewMenuRegression"),w!("Failed provider"),WS_OVERLAPPEDWINDOW,20,20,200,100,None,None,Some(instance),None)?});
        let calls=Rc::new(Cell::new(0));let view:IShellView=FailedView{owner:owner.0.0 as isize,calls:calls.clone()}.into();
        unsafe{attach_view_menu(&view,Rc::new(Cell::new(false)));}
        FAILURES.with(|value|value.set(0));FALLBACK_REQUESTS.with(|value|value.set(0));
        for position in [LPARAM(-1),point_parameter(POINT{x:45,y:65})]{
            let handled=unsafe{SendMessageW(owner.0,WM_CONTEXTMENU,Some(WPARAM(owner.0.0 as usize)),Some(position))};
            ensure!(handled.0==0,"failed context request was not handled");pump(20);
        }
        ensure!(calls.get()==2,"failed provider queried more than once per request");
        ensure!(FAILURES.with(Cell::get)==2&&FAILED_CODE.with(Cell::get)==0x80004005,"error was not reported to the main owner");
        ensure!(FALLBACK_REQUESTS.with(Cell::get)==0,"failed filtered menu fell through to a second menu");
        fn delegate(_window:HWND,_view:&IShellView,_position:LPARAM)->windows::core::Result<()>{
            DELEGATE_CALLS.with(|calls|calls.set(calls.get()+1));rejected()
        }
        struct Delegated;impl Drop for Delegated{fn drop(&mut self){set_view_menu_delegate(None);}}
        let _delegated=Delegated;set_view_menu_delegate(Some(delegate));
        DELEGATE_CALLS.with(|calls|calls.set(0));FAILURES.with(|calls|calls.set(0));
        for modern in [false,true]{
            unsafe{attach_view_menu(&view,Rc::new(Cell::new(modern)));}
            for position in [LPARAM(-1),point_parameter(POINT{x:45,y:65})]{
                ensure!(unsafe{SendMessageW(owner.0,WM_CONTEXTMENU,Some(WPARAM(owner.0.0 as usize)),Some(position))}.0==0,
                    "delegated menu style {modern} was not consumed");pump(20);
            }
        }
        ensure!(DELEGATE_CALLS.with(Cell::get)==4,"modern or classic style bypassed the installed isolation delegate");
        ensure!(calls.get()==2&&FALLBACK_REQUESTS.with(Cell::get)==0,"delegated failure activated a main-process extension or retried native fallback");
        ensure!(FAILURES.with(Cell::get)==4,"delegated failures were not reported exactly once");
        drop(view);drop(owner);
        Ok(serde_json::json!({"requests":6,"provider_calls":calls.get(),"original_provider_errors":2,"delegate_errors":4,"delegated_styles":["classic","modern"],"fallback_requests":0,"failure_injected_at":"IShellView::GetItemObject and isolation delegate"}))
    }
    fn exercise_view(output:&std::path::Path)->Result<serde_json::Value>{
        unsafe{OleInitialize(None)?;}let _apartment=Apartment;
        let _preferences=Preferences(context_menu_options());
        let mut options=ContextMenuOptions::default();options.modern=false;set_context_menu_options(options.clone());
        let folder=output.join("fixture");std::fs::create_dir_all(&folder)?;
        let file=folder.join("主窗 右键.txt");std::fs::write(&file,b"Native file-manager context menu fixture")?;
        let instance=HINSTANCE(unsafe{GetModuleHandleW(None)?}.0);
        unsafe{RegisterClassW(&WNDCLASSW{hInstance:instance,lpfnWndProc:Some(owner_proc),lpszClassName:w!("ZSDesk.ViewMenuRegression"),..Default::default()});}
        let failure=failure_route(instance)?;
        FAILURES.with(|value|value.set(0));NAVIGATED.with(|value|value.set(false));NAV_FAILED.with(|value|value.set(false));
        let owner=Owner(unsafe{CreateWindowExW(WINDOW_EX_STYLE::default(),w!("ZSDesk.ViewMenuRegression"),w!("File-manager context menu regression"),WS_OVERLAPPEDWINDOW,20,20,700,500,None,None,Some(instance),None)?});
        let browser=unsafe{ExplorerBrowserHost::create_transient(owner.0,RECT{left:0,top:0,right:680,bottom:460},false,Panes::default())?};
        browser.set_context_menu_options(options);
        unsafe{let _=ShowWindow(owner.0,SW_SHOW);let _=SetActiveWindow(owner.0);let _=SetFocus(Some(owner.0));let _=SetForegroundWindow(owner.0);browser.navigate(&ShellLocation::Path(folder))?;}
        let deadline=Instant::now()+Duration::from_secs(12);
        loop{pump(25);ensure!(!NAV_FAILED.with(Cell::get),"Shell navigation failed");
            if NAVIGATED.with(Cell::get)&&browser.status().items==1{break;}
            ensure!(Instant::now()<deadline,"Shell view did not materialize");}
        browser.select_child(file.file_name().context("fixture filename")?)?;pump(80);
        ensure!(browser.status().selected==1,"fixture selection was not applied");
        let view=browser.view_window().context("actual Shell view HWND")?;
        unsafe{SetWindowSubclass(view,Some(probe_proc),PROBE_SUBCLASS,0)}.ok()?;let probe=Probe(view);
        let mut rect=RECT::default();unsafe{GetWindowRect(view,&mut rect)?;}
        let mut rows=Vec::new();
        for (name,position) in [("keyboard",LPARAM(-1)),("mouse",point_parameter(POINT{x:rect.left+90,y:rect.top+90}))]{
            ENTERED.with(|value|value.set(None));CONTEXT_REQUESTS.with(|value|value.set(0));
            PUBLIC_BIND_MICROS.with(|value|value.set(0));PUBLIC_POPULATE_MICROS.with(|value|value.set(0));
            unsafe{SetTimer(Some(view),WATCHDOG_TIMER,5000,None);}
            let started=Instant::now();
            let result=unsafe{SendMessageW(view,WM_CONTEXTMENU,Some(WPARAM(view.0 as usize)),Some(position))};
            let elapsed=started.elapsed();
            let _=unsafe{KillTimer(Some(view),WATCHDOG_TIMER)};let _=unsafe{KillTimer(Some(view),CLOSE_TIMER)};
            let entered=ENTERED.with(Cell::get).context("actual Shell view did not enter a popup menu loop")?;
            ensure!(result.0==0&&CONTEXT_REQUESTS.with(Cell::get)==1,"context request was not dispatched exactly once");
            pump(20);ensure!(FAILURES.with(Cell::get)==0,"native view menu failed");
            let timing=crate::shell::menu_diagnostics();
            rows.push(serde_json::json!({"entry":name,"public_bind_microseconds":timing.last_public_bind_microseconds,
                "query_context_menu_microseconds":timing.last_public_populate_microseconds,
                "input_to_menu_loop_microseconds":entered.duration_since(started).as_micros()as u64,
                "total_with_40ms_auto_dismiss_microseconds":elapsed.as_micros()as u64,"native_popup_entered":true}));
        }
        ensure!(std::fs::read(&file)?==b"Native file-manager context menu fixture","fixture content changed");
        drop(probe);drop(browser);pump(30);drop(owner);pump(20);
        Ok(serde_json::json!({"original_view_menu":rows,"failure_route":failure,"no_command_invoked":true,
            "modern_view_menu":"not exercised","fixture_unchanged":true}))
    }
    #[test]
    #[ignore="Real file-manager original menus on an unswitched desktop; requires ZSDESK_NATIVE_TEST_OUTPUT"]
    fn native_main_view_context_menu_entries_and_failure_route()->Result<()>{
        let output=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("output")?).join("main-view-menu");ensure!(output.is_absolute(),"absolute output required");std::fs::create_dir_all(&output)?;
        let before=unsafe{GetForegroundWindow()};let name:Vec<u16>=format!("ZSDesk.MainMenu.{}",std::process::id()).encode_utf16().chain(Some(0)).collect();
        let desktop=unsafe{CreateDesktopW(name.as_ptr(),ptr::null(),ptr::null(),0,0x000f00ff,ptr::null())};ensure!(!desktop.is_null(),"CreateDesktop failed");
        let raw=desktop as usize;let target=output.clone();
        let result=thread::spawn(move||->Result<serde_json::Value>{ensure!(unsafe{SetThreadDesktop(raw as*mut c_void)}!=0,"SetThreadDesktop failed");exercise_view(&target)}).join().map_err(|_|anyhow::anyhow!("main menu regression panicked")).and_then(|result|result);
        let closed=unsafe{CloseDesktop(desktop)}!=0;let current=unsafe{GetForegroundWindow()};let mut foreground_process=0;unsafe{GetWindowThreadProcessId(current,Some(&mut foreground_process));}
        let isolated=foreground_process!=std::process::id();
        let report=serde_json::json!({"state":if result.is_ok()&&closed&&isolated{"passed"}else{"failed"},"checks":result.as_ref().map_err(|error|format!("{error:#}")),"desktop_closed":closed,"foreground_unchanged":before==current,"test_did_not_activate":isolated,"desktop_switched":false});
        std::fs::write(output.join("verification.current.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");result?;ensure!(closed&&isolated,"desktop cleanup/focus failed");Ok(())
    }
}
