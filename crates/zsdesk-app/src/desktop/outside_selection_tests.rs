use super::*;
use anyhow::{Context,ensure};
use std::{ffi::c_void,fs,path::Path,thread,time::{Duration,Instant}};
use zsdesk_core::model::desktop::{DesktopConfig,Fence,FenceSource,Placement};
use windows::Win32::System::Ole::{OleInitialize,OleUninitialize};
#[link(name="user32")]unsafe extern "system"{
    fn CreateDesktopW(name:*const u16,device:*const u16,mode:*const c_void,flags:u32,access:u32,attributes:*const c_void)->*mut c_void;
    fn SetThreadDesktop(desktop:*mut c_void)->i32;
    fn CloseDesktop(desktop:*mut c_void)->i32;
}
struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}
struct OwnedWindow(HWND);impl Drop for OwnedWindow{fn drop(&mut self){let _=unsafe{DestroyWindow(self.0)};}}
struct Fixture{desktop:HWND,host:HWND}
impl Drop for Fixture{fn drop(&mut self){
    super::super::update_hook(false,false);
    if let Some(mut host)=HOST.with(|slot|slot.borrow_mut().take()){host.config.settings.icons_hidden_by_zsdesk=false;host.config.settings.organize=false;host.shutdown();}
    HOST_HWND.with(|slot|slot.set(0));let _=unsafe{DestroyWindow(self.host)};let _=unsafe{DestroyWindow(self.desktop)};
}}
unsafe extern "system" fn background_proc(hwnd:HWND,message:u32,wp:WPARAM,lp:LPARAM)->LRESULT{unsafe{DefWindowProcW(hwnd,message,wp,lp)}}
fn setup(root:&Path)->Result<(Fixture,Vec<PathBuf>)>{
    let files=root.join("files");fs::create_dir_all(&files)?;
    let paths:Vec<_>=["loose A.txt","loose B.txt","inside fence.txt"].iter().map(|name|files.join(name)).collect();
    for path in &paths{fs::write(path,b"owned desktop selection fixture")?;}
    let entries=desktop::enumerate_folder(&files)?;
    let instance=HINSTANCE(unsafe{GetModuleHandleW(None)}?.0);
    unsafe{RegisterClassW(&WNDCLASSW{lpfnWndProc:Some(background_proc),hInstance:instance,lpszClassName:w!("WorkerW"),..Default::default()});
        RegisterClassW(&WNDCLASSW{lpfnWndProc:Some(background_proc),hInstance:instance,lpszClassName:w!("ZSDesk.OutsideNormalApplication"),..Default::default()});
        RegisterClassW(&WNDCLASSW{lpfnWndProc:Some(super::super::host_proc),hInstance:instance,lpszClassName:w!("ZSDesk.OutsideSelectionHost"),..Default::default()});
        RegisterClassW(&WNDCLASSW{lpfnWndProc:Some(super::super::fence_proc),hInstance:instance,lpszClassName:w!("ZSDesk.OutsideSelectionItem"),..Default::default()});}
    let desktop=unsafe{CreateWindowExW(WS_EX_TOOLWINDOW|WS_EX_NOACTIVATE,w!("WorkerW"),w!("Owned desktop"),WS_POPUP,-400,0,1600,1000,None,None,Some(instance),None)}?;
    let hwnd=unsafe{CreateWindowExW(WS_EX_TOOLWINDOW|WS_EX_NOACTIVATE,w!("ZSDesk.OutsideSelectionHost"),PCWSTR::null(),WS_POPUP,0,0,0,0,None,None,Some(instance),None)}?;
    let fixture=Fixture{desktop,host:hwnd};let _=unsafe{ShowWindow(desktop,SW_SHOWNOACTIVATE)};
    let mut config=DesktopConfig::default();config.settings.organize=true;config.settings.icons_hidden_by_zsdesk=true;config.settings.hide_tray=true;config.settings.double_click_toggle=false;config.next_id=4;
    for id in 1..=3{
        let source=if id<3{FenceSource::Desktop}else{FenceSource::Folder{root:files.clone()}};
        let mut item=Fence::new(id,"Fixture",source,Placement{x:100+(id as i32-1)*160,y:100,width:96,height:104,..Default::default()});
        let entry=entries.iter().find(|entry|entry.path.as_ref()==Some(&paths[id as usize-1])).context("fixture Shell identity")?;
        item.members=vec![entry.key.clone()];if id<3{config.released_items.push(item);}else{config.fences.push(item);}
    }
    let mut host=DesktopHost::new(hwnd,root.join("profile/desktop-fences.json"),root.join("profile/explorer-session.json"),config,zsdesk_windows::gfx::Gfx::new("Segoe UI")?);
    for id in 1..=3{
        let window=unsafe{CreateWindowExW(WS_EX_TOOLWINDOW|WS_EX_LAYERED,w!("ZSDesk.OutsideSelectionItem"),PCWSTR::null(),WS_POPUP,100+(id as i32-1)*160,100,96,104,Some(desktop),None,Some(instance),None)}?;
        unsafe{SetWindowLongPtrW(window,GWLP_USERDATA,id as isize);}
        let mut state=super::super::fence::FenceWin::new(id);state.hwnd=window;state.standalone=id<3;
        state.entries=vec![entries.iter().find(|entry|entry.path.as_ref()==Some(&paths[id as usize-1])).unwrap().clone()];
        if id==3{state.selection.insert(state.entries[0].key.clone());state.focus=Some(0);state.anchor=Some(0);}
        host.fences.push(state);host.render(id);let _=unsafe{ShowWindow(window,SW_SHOWNOACTIVATE)};
    }
    HOST.with(|slot|*slot.borrow_mut()=Some(host));HOST_HWND.with(|slot|slot.set(hwnd.0 as isize));super::super::update_hook(false,true);
    ensure!(HOOK.with(Cell::get)!=0&&!DOUBLE_CLICK_ENABLED.with(Cell::get),"disabling double click disabled the organize-mode pointer observer");Ok((fixture,paths))
}
fn input(fixture:&Fixture,message:u32,point:POINT){
    observe(message,point,false);unsafe{SendMessageW(fixture.host,MESSAGE,None,None);}
}
fn selected()->Vec<u64>{with_host(|host|host.fences.iter().filter(|window|window.standalone&&!window.selection.is_empty()).map(|window|window.id).collect()).unwrap_or_default()}
fn overlays()->Vec<HWND>{GESTURE.with(|slot|slot.borrow().as_ref().and_then(|state|state.overlay.as_ref()).map(|overlay|overlay.segments.iter().map(|(window,_)|*window).collect()).unwrap_or_default())}
fn snapshot(stage:&str)->serde_json::Value{
    let windows=with_host(|host|host.fences.iter().map(|window|{
        let rect=window.window_rect();let centre=POINT{x:(rect.left+rect.right)/2,y:(rect.top+rect.bottom)/2};let hit=unsafe{WindowFromPoint(centre)};
        serde_json::json!({"id":window.id,"hwnd":window.hwnd.0 as usize,"rect":[rect.left,rect.top,rect.right,rect.bottom],"dpi":window.dpi,
            "visible":unsafe{IsWindowVisible(window.hwnd)}.as_bool(),"selected":window.selection.len(),"entries":window.entries.len(),"hit":hit.0 as usize,"hit_class":class_name(hit),
            "can_start":host.can_start_desktop_selection(),"interaction":window.interaction.is_some()})
    }).collect::<Vec<_>>()).unwrap_or_default();
    let foreground=unsafe{GetForegroundWindow()};let blank=unsafe{WindowFromPoint(POINT{x:40,y:40})};
    let observation=OBSERVATION.with(Cell::get).map(|event|serde_json::json!({"generation":event.generation,"point":[event.current.x,event.current.y],"finished":event.finished,"cancelled":event.cancelled}));
    let gesture=GESTURE.with(|slot|slot.borrow().as_ref().map(|state|serde_json::json!({"generation":state.generation,"dragged":state.dragged,"control":state.control,"shift":state.shift})));
    let mut order=Vec::new();let mut current=unsafe{GetTopWindow(None)}.unwrap_or_default();while !current.0.is_null()&&order.len()<32{
        order.push(serde_json::json!({"hwnd":current.0 as usize,"class":class_name(current),"visible":unsafe{IsWindowVisible(current)}.as_bool()}));current=unsafe{GetWindow(current,GW_HWNDNEXT)}.unwrap_or_default();
    }
    serde_json::json!({"stage":stage,"windows":windows,"observation":observation,"gesture":gesture,"z_order":order,"foreground":foreground.0 as usize,"foreground_class":class_name(foreground),"blank":blank.0 as usize,"blank_class":class_name(blank)})
}
fn exercise(root:&Path)->Result<serde_json::Value>{
    unsafe{OleInitialize(None)?;}let _apartment=Apartment;let (fixture,paths)=setup(root)?;
    ensure!(!foreground_is_desktop(fixture.desktop,HWND::default()),"unknown foreground granted icon activation");
    ensure!(!foreground_cancels_marquee(fixture.desktop,HWND::default()),"unknown foreground cancelled a valid desktop gesture");
    ensure!(foreground_is_desktop(fixture.desktop,fixture.desktop)&&!foreground_cancels_marquee(fixture.desktop,fixture.desktop),"verified desktop foreground policy failed");
    let start=POINT{x:40,y:40};let finish=POINT{x:560,y:230};ensure!(background_root(start)==Some(fixture.desktop),"fixture blank point did not hit the owned desktop");
    input(&fixture,WM_LBUTTONDOWN,start);input(&fixture,WM_MOUSEMOVE,finish);
    ensure!(selected()==vec![1,2],"desktop-background band did not select both loose icons");
    ensure!(with_host(|host|host.fences.iter().find(|window|window.id==3).is_some_and(|window|window.selection.len()==1&&window.interaction.is_none()))==Some(true),"band crossed into fence item interaction");
    let windows=overlays();ensure!(windows.len()==4,"marquee border not rendered");
    for window in &windows{
        let style=unsafe{GetWindowLongPtrW(*window,GWL_EXSTYLE)}as u32;
        ensure!(style&(WS_EX_TRANSPARENT.0|WS_EX_NOACTIVATE.0)==WS_EX_TRANSPARENT.0|WS_EX_NOACTIVATE.0&&style&WS_EX_TOPMOST.0==0,"marquee blocks input or covers applications");
        ensure!(unsafe{SendMessageW(*window,WM_NCHITTEST,None,None)}.0==HTTRANSPARENT as isize,"marquee did not pass hit testing through");
    }
    input(&fixture,WM_LBUTTONUP,finish);ensure!(selected()==vec![1,2],"mouse release discarded selection");
    ensure!(with_host(|host|host.fences.iter().filter(|window|window.standalone).all(|window|{
        let rect=window.window_rect();(unsafe{WindowFromPoint(POINT{x:(rect.left+rect.right)/2,y:(rect.top+rect.bottom)/2})})==window.hwnd
    }))==Some(true),"completed selection hid a loose icon behind the desktop: {}",snapshot("first-release"));
    ensure!(windows.iter().all(|window|!unsafe{IsWindow(Some(*window))}.as_bool()),"marquee windows survived release");
    ensure!(with_host(|host|host.selected_pidls(1).len())==Some(2),"loose-icon group actions lost the selected identities");
    // A regular blank click clears loose selection, preserving fence contents.
    input(&fixture,WM_LBUTTONDOWN,start);input(&fixture,WM_LBUTTONUP,start);ensure!(selected().is_empty(),"blank click did not clear loose selection");
    input(&fixture,WM_LBUTTONDOWN,start);input(&fixture,WM_MOUSEMOVE,POINT{x:220,y:230});
    input(&fixture,WM_LBUTTONUP,POINT{x:220,y:230});ensure!(selected()==vec![1],"single loose selection failed: {}",snapshot("single-release"));
    // Modifiers are frozen at the pointer-down event, including while the
    // user's keyboard focus is still on Explorer's desktop window.
    input(&fixture,WM_LBUTTONDOWN,POINT{x:240,y:40});OBSERVATION.with(|slot|if let Some(mut event)=slot.get(){event.shift=true;slot.set(Some(event));});
    GESTURE.with(|slot|if let Some(state)=slot.borrow_mut().as_mut(){state.shift=true;});
    input(&fixture,WM_MOUSEMOVE,POINT{x:380,y:230});input(&fixture,WM_LBUTTONUP,POINT{x:380,y:230});ensure!(selected()==vec![1,2],"shift marquee did not add to loose selection");
    input(&fixture,WM_LBUTTONDOWN,start);OBSERVATION.with(|slot|if let Some(mut event)=slot.get(){event.control=true;slot.set(Some(event));});
    GESTURE.with(|slot|if let Some(state)=slot.borrow_mut().as_mut(){state.control=true;});
    input(&fixture,WM_MOUSEMOVE,POINT{x:220,y:230});input(&fixture,WM_LBUTTONUP,POINT{x:220,y:230});ensure!(selected()==vec![2],"control marquee did not toggle only the hit identity");
    input(&fixture,WM_LBUTTONDOWN,start);input(&fixture,WM_MOUSEMOVE,finish);input(&fixture,WM_RBUTTONDOWN,finish);
    ensure!(selected()==vec![2]&&!active()&&overlays().is_empty(),"right-click cancellation lost the preceding selection or border");
    HOST.with(|slot|{let _busy=slot.borrow_mut();
        input(&fixture,WM_LBUTTONDOWN,start);input(&fixture,WM_MOUSEMOVE,POINT{x:220,y:230});input(&fixture,WM_LBUTTONUP,POINT{x:220,y:230});
        assert!(OBSERVATION.with(|slot|slot.get().is_some_and(|event|event.finished)),"busy host discarded MouseUp");
    });
    unsafe{SendMessageW(fixture.host,WM_TIMER,Some(WPARAM(TIMER)),None);}
    ensure!(selected()==vec![1]&&!active(),"deferred MouseUp did not commit through host timer");
    input(&fixture,WM_LBUTTONDOWN,start);input(&fixture,WM_MOUSEMOVE,finish);let deferred_borders=overlays();
    HOST.with(|slot|{let _busy=slot.borrow_mut();cancel();assert!(GESTURE.with(|slot|slot.borrow().is_some()),"busy cancellation discarded restore snapshot");});
    unsafe{SendMessageW(fixture.host,WM_TIMER,Some(WPARAM(TIMER)),None);}
    ensure!(selected()==vec![1]&&deferred_borders.iter().all(|window|!unsafe{IsWindow(Some(*window))}.as_bool()),"deferred cancellation lost selection or retained borders");
    DISPATCHING.with(|slot|slot.set(true));
    input(&fixture,WM_LBUTTONDOWN,POINT{x:240,y:40});input(&fixture,WM_MOUSEMOVE,POINT{x:380,y:230});input(&fixture,WM_LBUTTONUP,POINT{x:380,y:230});
    DISPATCHING.with(|slot|slot.set(false));unsafe{SendMessageW(fixture.host,WM_TIMER,Some(WPARAM(TIMER)),None);}
    ensure!(selected()==vec![2]&&!active(),"nested dispatch discarded the pending gesture");
    // A click on a fence, icon, or ordinary application never starts a
    // desktop-background gesture. No real user pointer input is injected.
    input(&fixture,WM_LBUTTONDOWN,POINT{x:450,y:125});ensure!(!active(),"fence interior started an outside band");
    input(&fixture,WM_LBUTTONDOWN,POINT{x:120,y:125});ensure!(!active(),"loose icon press started a background band");
    // STATIC text is deliberately skipped by WindowFromPoint; use a real
    // top-level application class and verify its hit before testing exclusion.
    let instance=HINSTANCE(unsafe{GetModuleHandleW(None)}?.0);
    let blocker=OwnedWindow(unsafe{CreateWindowExW(WS_EX_NOACTIVATE,w!("ZSDesk.OutsideNormalApplication"),w!("Owned normal application"),WS_POPUP,20,20,80,60,None,None,Some(instance),None)}?);
    let _=unsafe{ShowWindow(blocker.0,SW_SHOWNOACTIVATE)};
    ensure!(unsafe{WindowFromPoint(start)}==blocker.0,"normal-application fixture did not cover the test point: {}",snapshot("normal-application-hit"));
    ensure!(!foreground_is_desktop(fixture.desktop,blocker.0)&&foreground_cancels_marquee(fixture.desktop,blocker.0),"known application foreground policy failed");
    ensure!(background_root(start).is_none(),"ordinary application treated as desktop");
    input(&fixture,WM_LBUTTONDOWN,start);ensure!(!active(),"ordinary application click started a background band");drop(blocker);
    observe(WM_LBUTTONDOWN,start,true);unsafe{SendMessageW(fixture.host,MESSAGE,None,None);}
    ensure!(!active()&&selected()==vec![2],"double-click event started a competing selection gesture");
    configure(false);input(&fixture,WM_LBUTTONDOWN,start);input(&fixture,WM_MOUSEMOVE,finish);input(&fixture,WM_LBUTTONUP,finish);
    ensure!(selected()==vec![2]&&!active(),"disabled organize observer changed native desktop selection");
    for path in &paths{ensure!(fs::read(path)?==b"owned desktop selection fixture","selection modified a fixture file");}
    drop(fixture);
    Ok(serde_json::json!({"blank_desktop_hit_confirmed":true,"two_loose_icons_selected":true,"fence_contents_untouched":true,"keyboard_group_identities_preserved":true,
        "blank_click_clears_selection":true,"shift_adds_control_toggles":true,"right_click_cancel_restores_selection":true,"disabled_observer_preserves_native_input":true,
        "busy_host_preserves_release_and_cancel":true,"nested_dispatch_preserves_gesture":true,
        "double_click_setting_independent":true,"double_click_gesture_preserved":true,"ordinary_application_and_icon_clicks_ignored":true,"blocked_application_hit_confirmed":true,
        "unknown_foreground_never_activates_icon":true,"unknown_foreground_keeps_valid_gesture":true,"known_application_foreground_cancels":true,"selection_visible_after_commit":true,
        "transparent_nonactivating_non_topmost_border":true,"border_destroyed_on_release":true,"files_unchanged":true,"user_desktop_modified":false}))
}
#[test]
#[ignore="Owned-window desktop-background marquee on an unswitched private desktop; requires ZSDESK_NATIVE_TEST_OUTPUT"]
fn native_desktop_background_marquee_selects_only_loose_icons()->Result<()>{
    let root=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("output")?).join("desktop-outside-selection");ensure!(root.is_absolute(),"absolute output path required");fs::create_dir_all(&root)?;
    let before=unsafe{GetForegroundWindow()};let name=wide(&format!("ZSDesk.OutsideSelection.{}",std::process::id()));
    let desktop=unsafe{CreateDesktopW(name.as_ptr(),std::ptr::null(),std::ptr::null(),0,0x000f00ff,std::ptr::null())};ensure!(!desktop.is_null(),"private desktop unavailable");
    let raw=desktop as usize;let target=root.clone();let result=thread::spawn(move||->Result<serde_json::Value>{ensure!(unsafe{SetThreadDesktop(raw as*mut c_void)}!=0,"SetThreadDesktop failed");exercise(&target)}).join().map_err(|_|anyhow::anyhow!("outside-selection test panicked")).and_then(|result|result);
    let deadline=Instant::now()+Duration::from_secs(3);let closed=loop{if unsafe{CloseDesktop(desktop)}!=0{break true;}if Instant::now()>=deadline{break false;}thread::sleep(Duration::from_millis(20));};
    let current=unsafe{GetForegroundWindow()};let mut process=0;unsafe{GetWindowThreadProcessId(current,Some(&mut process));}let isolated=process!=std::process::id();
    let report=serde_json::json!({"state":if result.is_ok()&&closed&&isolated{"passed"}else{"failed"},"checks":result.as_ref().map_err(|error|format!("{error:#}")),"desktop_closed":closed,"foreground_unchanged":before==current,"test_did_not_activate":isolated});
    fs::write(root.join("verification.current.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");result?;ensure!(closed&&isolated,"private desktop cleanup failed");Ok(())
}
#[test]
fn unknown_foreground_neither_activates_nor_cancels_marquee(){
    for foreground in [HWND::default(),HWND(usize::MAX as*mut _)]{
        assert_eq!(foreground_kind(HWND::default(),foreground),ForegroundKind::Unknown);
        assert!(!foreground_is_desktop(HWND::default(),foreground));
        assert!(!foreground_cancels_marquee(HWND::default(),foreground));
    }
}
#[test]
fn physical_rectangles_keep_negative_coordinates_and_direction(){
    let a=POINT{x:-1800,y:-400};let b=POINT{x:900,y:500};let first=union_rect(a,b);let reverse=union_rect(b,a);
    assert_eq!((first.left,first.top,first.right,first.bottom),(reverse.left,reverse.top,reverse.right,reverse.bottom));
    assert!(intersects(first,RECT{left:-1200,top:-200,right:-1100,bottom:-100}));
    assert!(!intersects(first,RECT{left:900,top:100,right:1000,bottom:200}));
}
