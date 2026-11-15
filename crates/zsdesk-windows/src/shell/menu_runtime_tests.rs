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

#[test]
#[ignore="Rebuilds a consumed HMENU and executes a counted fixture command on a private desktop; no file or clipboard operation"]
fn native_recreated_classic_menu_preserves_target_anchor_and_single_dispatch()->Result<()>{
    use windows::{core::{implement,HRESULT,PSTR},Win32::UI::Shell::IContextMenu_Impl};
    type Calls=Rc<RefCell<Vec<(Vec<Vec<u8>>,[i32;2])>>>;
    #[implement(IContextMenu)]
    struct BoundContext{targets:Vec<Vec<u8>>,calls:Calls,queries:Rc<Cell<usize>>}
    #[allow(non_snake_case)]impl IContextMenu_Impl for BoundContext_Impl{
        fn QueryContextMenu(&self,menu:HMENU,_:u32,first:u32,_:u32,_:u32)->HRESULT{
            self.queries.set(self.queries.get()+1);
            match unsafe{AppendMenuW(menu,MF_STRING,first as usize,w!("Execute fixture (&Z)"))}{Ok(())=>HRESULT(1),Err(error)=>error.code()}
        }
        fn GetCommandString(&self,id:usize,_:u32,_:*const u32,output:PSTR,count:u32)->windows::core::Result<()>{
            let verb:Vec<u16>="zsdesk.fixture".encode_utf16().chain(Some(0)).collect();
            if id!=0||count<verb.len()as u32{return Err(windows::core::Error::from_hresult(HRESULT(0x80070057u32 as i32)));}
            unsafe{std::ptr::copy_nonoverlapping(verb.as_ptr(),output.0.cast(),verb.len());}Ok(())
        }
        fn InvokeCommand(&self,info:*const CMINVOKECOMMANDINFO)->windows::core::Result<()>{
            if info.is_null()||unsafe{(*info).cbSize}<std::mem::size_of::<CMINVOKECOMMANDINFOEX>()as u32{return Err(windows::core::Error::from_hresult(HRESULT(0x80070057u32 as i32)));}
            let info=unsafe{&*info.cast::<CMINVOKECOMMANDINFOEX>()};
            if !info.lpVerb.0.is_null()||!info.lpVerbW.0.is_null(){return Err(windows::core::Error::from_hresult(HRESULT(0x80070057u32 as i32)));}
            self.calls.borrow_mut().push((self.targets.clone(),[info.ptInvoke.x,info.ptInvoke.y]));Ok(())
        }
    }
    let output=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("output")?).join("recreated-classic");std::fs::create_dir_all(&output)?;
    let name:Vec<u16>=format!("ZSDesk.RecreatedMenu.{}",std::process::id()).encode_utf16().chain(Some(0)).collect();
    let desktop=unsafe{CreateDesktopW(name.as_ptr(),ptr::null(),ptr::null(),0,0x000f00ff,ptr::null())};ensure!(!desktop.is_null(),"private desktop");let raw=desktop as usize;
    let result=thread::spawn(move||->Result<serde_json::Value>{
        ensure!(unsafe{SetThreadDesktop(raw as*mut c_void)}!=0,"private desktop attach");unsafe{OleInitialize(None)}?;let _apartment=Apartment;
        let module=HINSTANCE(unsafe{GetModuleHandleW(None)?}.0);let class=w!("ZSDesk.RecreatedMenu.Owner");
        unsafe{RegisterClassW(&WNDCLASSW{hInstance:module,lpfnWndProc:Some(procedure),lpszClassName:class,..Default::default()});}
        let owner=Owner(unsafe{CreateWindowExW(WINDOW_EX_STYLE::default(),class,w!("Recreated menu"),WS_OVERLAPPEDWINDOW,20,20,600,400,None,None,Some(module),None)}?);
        unsafe{let _=ShowWindow(owner.0,SW_SHOW);let _=SetForegroundWindow(owner.0);}
        CHOOSE_LAST.with(|last|last.set(false));let mut checks=Vec::new();
        for (background,targets) in [(false,vec![vec![3,0,11,0,0]]),(false,vec![vec![3,0,11,0,0],vec![3,0,12,0,0]]),(true,Vec::new())]{
            for input_flags in [0u32,1,3]{
                let calls:Calls=Default::default();let queries=Rc::new(Cell::new(0));
                let context:IContextMenu=BoundContext{targets:targets.clone(),calls:calls.clone(),queries:queries.clone()}.into();
                let mut menu=ShellMenu::new(context)?;menu.background=background;menu.input_flags.set(input_flags);menu.populate(false,false)?;
                unsafe{DestroyMenu(menu.hmenu)}?;menu.owns_menu.set(false);
                let anchor=POINT{x:70+(input_flags as i32)*13,y:100+(input_flags as i32)*17};let mut authorizations=0;
                let outcome=menu.track_recreated_classic(owner.0,anchor,&[],MenuInterceptions::default(),&mut||{authorizations+=1;Ok(())})?;
                let _=unsafe{KillTimer(Some(owner.0),1)};let _=unsafe{KillTimer(Some(owner.0),2)};
                ensure!(outcome==MenuOutcome::ShellInvoked&&authorizations==1,"rebuilt command was not authorized and executed exactly once");
                ensure!(*calls.borrow()==vec![(targets.clone(),[anchor.x,anchor.y])]&&queries.get()==2,"rebuilt popup changed target/anchor or replayed invocation");
                checks.push(serde_json::json!({"background":background,"count":targets.len(),"input_flags":input_flags,"anchor":[anchor.x,anchor.y],"queries":queries.get(),"invocations":calls.borrow().len(),"authorizations":authorizations}));
            }
        }Ok(serde_json::json!({"cases":checks,"clipboard_untouched":true}))
    }).join().map_err(|_|anyhow::anyhow!("recreated menu regression panicked")).and_then(|value|value);
    let closed=unsafe{CloseDesktop(desktop)}!=0;
    let report=serde_json::json!({"state":if result.is_ok()&&closed{"passed"}else{"failed"},"checks":result.as_ref().map_err(|error|format!("{error:#}")),"desktop_closed":closed});
    std::fs::write(output.join("verification.current.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");result?;ensure!(closed,"private desktop retained");Ok(())
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
        ensure!(calls.get()==1,"unresolved mouse hit queried a provider or keyboard failure was retried");
        ensure!(FAILURES.with(Cell::get)==2&&FAILED_CODE.with(Cell::get)==0x8000000a,"unresolved mouse target was not reported to the main owner");
        ensure!(FALLBACK_REQUESTS.with(Cell::get)==0,"failed filtered menu fell through to a second menu");
        fn delegate(_window:HWND,_view:&IShellView,_source:HWND,_position:LPARAM,_target:ViewMenuTarget)->windows::core::Result<()>{
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
        ensure!(DELEGATE_CALLS.with(Cell::get)==2,"keyboard menu bypassed isolation or unresolved mouse target reused the previous selection");
        ensure!(calls.get()==1&&FALLBACK_REQUESTS.with(Cell::get)==0,"delegated failure activated a main-process extension or retried native fallback");
        ensure!(FAILURES.with(Cell::get)==4,"delegated failures were not reported exactly once");
        drop(view);drop(owner);
        Ok(serde_json::json!({"requests":6,"provider_calls":calls.get(),"original_provider_errors":1,"delegate_errors":2,"unresolved_mouse_requests_rejected":3,"delegated_styles":["classic","modern"],"fallback_requests":0,"failure_injected_at":"IShellView::GetItemObject and isolation delegate"}))
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
        let blank=point_parameter(POINT{x:rect.right-30,y:rect.bottom-30});
        ensure!(menu_target::classify(view,view,blank)==ViewMenuTarget::Background,"blank space with retained selection was not classified as background");
        ensure!(browser.status().selected==1,"read-only context classification changed selection");
        ensure!(menu_target::classify(view,view,LPARAM(-1))==ViewMenuTarget::CurrentSelection,"keyboard classification lost selection semantics");
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
    fn accessible_item_point(view:HWND,name:&str)->Result<LPARAM>{
        use windows::{core::IUnknown,Win32::{System::{Com::IDispatch,Variant::VARIANT},UI::Accessibility::*}};
        let mut windows=vec![view];
        unsafe extern "system" fn collect(hwnd:HWND,data:LPARAM)->BOOL{unsafe{(&mut*(data.0 as*mut Vec<HWND>)).push(hwnd);}BOOL(1)}
        let _=unsafe{EnumChildWindows(Some(view),Some(collect),LPARAM(&mut windows as*mut _ as isize))};
        let mut seen=std::collections::HashSet::new();let mut budget=2048;
        for hwnd in windows{
            let mut raw=std::ptr::null_mut();if unsafe{AccessibleObjectFromWindow(hwnd,OBJID_CLIENT.0 as u32,&IAccessible::IID,&mut raw)}.is_err()||raw.is_null(){continue;}
            let mut pending=vec![(unsafe{IAccessible::from_raw(raw)},VARIANT::from(0i32))];
            while let Some((object,child))=pending.pop(){
                if budget==0{anyhow::bail!("accessible fixture exceeds bound");}budget-=1;
                let role=unsafe{object.get_accRole(&child)}.ok().and_then(|value|u32::try_from(&value).ok());
                if matches!(role,Some(ROLE_SYSTEM_LISTITEM|ROLE_SYSTEM_OUTLINEITEM|ROLE_SYSTEM_ROW))&&unsafe{object.get_accName(&child)}.is_ok_and(|value|value.to_string()==name){
                    let(mut x,mut y,mut width,mut height)=(0,0,0,0);unsafe{object.accLocation(&mut x,&mut y,&mut width,&mut height,&child)}?;
                    if width>0&&height>0{return Ok(point_parameter(POINT{x:x+30.min(width/2),y:y+height/2}));}
                }
                if i32::try_from(&child).ok()!=Some(0){continue;}
                let identity=object.cast::<IUnknown>()?.as_raw()as usize;if !seen.insert(identity){continue;}
                let count=unsafe{object.accChildCount()}.unwrap_or(0).clamp(0,512)as usize;if count==0{continue;}
                let mut children=(0..count).map(|_|VARIANT::default()).collect::<Vec<_>>();let mut obtained=0;
                if unsafe{AccessibleChildren(&object,0,&mut children,&mut obtained)}.is_err(){continue;}
                for child in children.into_iter().take(obtained.max(0)as usize){
                    if let Ok(next)=IDispatch::try_from(&child).and_then(|value|value.cast::<IAccessible>()){pending.push((next,VARIANT::from(0i32)));}
                    else{pending.push((object.clone(),child));}
                }
            }
        }
        anyhow::bail!("accessible file row not found: {name}")
    }
    fn exercise_hit_target(output:&std::path::Path)->Result<serde_json::Value>{
        use windows::Win32::{System::Com::CoTaskMemFree,UI::Shell::*};
        unsafe{OleInitialize(None)?;}let _apartment=Apartment;
        let folder=output.join("files");std::fs::create_dir_all(&folder)?;
        let first=folder.join("alpha.txt");let second=folder.join("beta.txt");
        std::fs::write(&first,b"first target")?;std::fs::write(&second,b"second target")?;
        let instance=HINSTANCE(unsafe{GetModuleHandleW(None)?}.0);
        unsafe{RegisterClassW(&WNDCLASSW{hInstance:instance,lpfnWndProc:Some(owner_proc),lpszClassName:w!("ZSDesk.ViewMenuHitRegression"),..Default::default()});}
        let owner=Owner(unsafe{CreateWindowExW(WINDOW_EX_STYLE::default(),w!("ZSDesk.ViewMenuHitRegression"),w!("Context target regression"),WS_OVERLAPPEDWINDOW,20,20,800,600,None,None,Some(instance),None)?});
        let browser=unsafe{ExplorerBrowserHost::create_transient(owner.0,RECT{left:0,top:0,right:780,bottom:560},false,Panes::default())?};
        NAVIGATED.with(|value|value.set(false));NAV_FAILED.with(|value|value.set(false));
        unsafe{let _=ShowWindow(owner.0,SW_SHOWNOACTIVATE);browser.navigate(&ShellLocation::Path(folder.clone()))?;}
        let until=Instant::now()+Duration::from_secs(10);
        while !NAVIGATED.with(Cell::get)||browser.status().items!=2{pump(20);ensure!(!NAV_FAILED.with(Cell::get)&&Instant::now()<until,"view did not load");}
        let shell=unsafe{browser.current_view()?};let view=unsafe{shell.GetWindow()?};let fv:IFolderView2=shell.cast()?;
        let mut checks=Vec::new();
        for mode in [FVM_DETAILS,FVM_ICON]{
            unsafe{fv.SetViewModeAndIconSize(mode,if mode==FVM_DETAILS{16}else{48})}.context("set hit-test view mode")?;pump(100);
            let mut indices=Vec::new();
            for index in 0..unsafe{fv.ItemCount(SVGIO_ALLVIEW)?}{
                let parent:IShellFolder=unsafe{fv.GetFolder()?};let relative=unsafe{fv.Item(index)?};
                let item:windows::core::Result<IShellItem>=unsafe{SHCreateItemWithParent(None,&parent,relative)};unsafe{CoTaskMemFree(Some(relative.cast()))};
                let path=crate::shell::file_transfer::paths_from_pidls(&[crate::shell::pidl::item_pidl(&item?).context("PIDL")?])?.remove(0);
                indices.push((index,path));
            }
            let a=indices.iter().find(|(_,path)|path==&first).context("first index")?.0;
            let b=indices.iter().find(|(_,path)|path==&second).context("second index")?.0;
            let point_for=|index|->Result<LPARAM>{let path=&indices.iter().find(|(item,_)|*item==index).context("point item index")?.1;
                accessible_item_point(view,&path.file_name().unwrap().to_string_lossy())};
            unsafe{fv.SelectItem(a,(SVSI_SELECT.0|SVSI_DESELECTOTHERS.0|SVSI_FOCUSED.0)as u32)?;}pump(20);
            let target=point_for(b)?;
            let classified=menu_target::classify(view,view,target);
            ensure!(classified==ViewMenuTarget::Selection,"file point was classified {classified:?} in mode {}",mode.0);
            ensure!(menu_target::prepare(view,view,target).context("prepare clicked item")?==ViewMenuTarget::Selection,"file click did not prepare selection");
            ensure!(crate::shell::file_transfer::selected_paths(&shell)?==vec![second.clone()],"menu kept previous file instead of clicked file");
            unsafe{fv.SelectItem(a,SVSI_SELECT.0 as u32)?;}pump(20);
            menu_target::prepare(view,view,point_for(a)?)?;
            ensure!(crate::shell::file_transfer::selected_paths(&shell)?.len()==2,"right click collapsed an existing multi-selection");
            ensure!(menu_target::prepare(view,view,LPARAM(-1))?==ViewMenuTarget::CurrentSelection,"keyboard context changed semantics");
            let mut rect=RECT::default();unsafe{GetWindowRect(view,&mut rect)?;}
            let blank=point_parameter(POINT{x:rect.right-30,y:rect.bottom-30});
            ensure!(menu_target::prepare(view,view,blank)?==ViewMenuTarget::Background,"blank space inherited previous file menu");
            checks.push(serde_json::json!({"mode":mode.0,"clicked_item_selected":true,"multi_selection_preserved":true,"blank_and_keyboard_distinct":true}));
        }
        drop(fv);drop(shell);drop(browser);pump(20);drop(owner);Ok(serde_json::json!({"checks":checks,"file_commands_invoked":0}))
    }
    #[test]
    #[ignore="Actual DefView mouse target and selection on a private desktop"]
    fn native_view_menu_hit_binds_clicked_file()->Result<()>{
        let output=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("output")?).join("view-menu-hit");std::fs::create_dir_all(&output)?;
        let name:Vec<u16>=format!("ZSDesk.MenuHit.{}",std::process::id()).encode_utf16().chain(Some(0)).collect();
        let desktop=unsafe{CreateDesktopW(name.as_ptr(),ptr::null(),ptr::null(),0,0x000f00ff,ptr::null())};ensure!(!desktop.is_null(),"private desktop creation failed");
        let handle=desktop as usize;let target=output.clone();
        let result=thread::spawn(move||{ensure!(unsafe{SetThreadDesktop(handle as*mut c_void)}!=0,"private desktop selection failed");exercise_hit_target(&target)}).join().map_err(|_|anyhow::anyhow!("hit target regression panicked")).and_then(|value|value);
        let closed=unsafe{CloseDesktop(desktop)}!=0;
        let report=serde_json::json!({"state":if result.is_ok()&&closed{"passed"}else{"failed"},"result":result.as_ref().map_err(|error|format!("{error:#}")),"desktop_closed":closed});
        std::fs::write(output.join("verification.current.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");result?;ensure!(closed,"desktop retained");Ok(())
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
