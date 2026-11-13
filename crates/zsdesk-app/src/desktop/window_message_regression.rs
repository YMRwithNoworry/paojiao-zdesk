//! Tray and pointer protocol tests confined to owned windows on a private desktop.
use super::*;
use anyhow::ensure;
use std::{ffi::c_void,fs,path::Path,thread,time::{Duration,Instant}};
use zsdesk_core::model::desktop::{DesktopConfig,Fence,FenceSource,FenceView,Placement};
use windows::Win32::UI::{Input::KeyboardAndMouse::{GetCapture,SetCapture,ReleaseCapture},Shell::{NIN_SELECT,NINF_KEY}};
#[link(name="user32")]unsafe extern "system"{
    fn CreateDesktopW(name:*const u16,device:*const u16,mode:*const c_void,flags:u32,access:u32,attributes:*const c_void)->*mut c_void;
    fn SetThreadDesktop(desktop:*mut c_void)->i32;
    fn CloseDesktop(desktop:*mut c_void)->i32;
}
thread_local!{static TRAY_ACTIONS:RefCell<Option<Vec<TrayAction>>>=const{RefCell::new(None)};}
pub(super) fn capture_tray_action(action:TrayAction)->bool{
    TRAY_ACTIONS.with(|slot|{let mut slot=slot.borrow_mut();if let Some(actions)=slot.as_mut(){actions.push(action);true}else{false}})
}
struct CaptureActions;impl Drop for CaptureActions{fn drop(&mut self){TRAY_ACTIONS.with(|slot|*slot.borrow_mut()=None);}}
struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}
struct OwnedHost{hwnd:HWND,windows:Vec<HWND>}
impl Drop for OwnedHost{fn drop(&mut self){
    if let Some(mut host)=HOST.with(|slot|slot.borrow_mut().take()){host.shutdown();}
    HOST_HWND.with(|slot|slot.set(0));for window in &self.windows{if unsafe{IsWindow(Some(*window))}.as_bool(){let _=unsafe{DestroyWindow(*window)};}}
    let _=unsafe{DestroyWindow(self.hwnd)};
}}
fn pump(ms:u64){let end=Instant::now()+Duration::from_millis(ms);while Instant::now()<end{
    let mut msg=MSG::default();for _ in 0..128{if !unsafe{PeekMessageW(&mut msg,None,0,0,PM_REMOVE)}.as_bool(){break;}if msg.message!=WM_QUIT{unsafe{let _=TranslateMessage(&msg);DispatchMessageW(&msg);}}}thread::sleep(Duration::from_millis(3));}}
fn owned_host(root:&Path)->Result<OwnedHost>{
    let instance=HINSTANCE(unsafe{GetModuleHandleW(None)?}.0);let host_class=w!("ZSDesk.Native.WindowMessageHost");let fence_class=w!("ZSDesk.Native.WindowMessageFence");
    unsafe{RegisterClassW(&WNDCLASSW{hInstance:instance,lpfnWndProc:Some(host_proc),lpszClassName:host_class,..Default::default()});
        RegisterClassW(&WNDCLASSW{hInstance:instance,lpfnWndProc:Some(fence_proc),lpszClassName:fence_class,..Default::default()});}
    let hwnd=unsafe{CreateWindowExW(WS_EX_TOOLWINDOW|WS_EX_NOACTIVATE,host_class,w!("Message protocol test"),WS_POPUP,0,0,0,0,None,None,Some(instance),None)}?;
    let mut owned=OwnedHost{hwnd,windows:Vec::new()};
    let mut config=DesktopConfig::default();config.settings.hide_tray=true;config.settings.double_click_toggle=false;config.settings.organize=false;config.settings.icons_hidden_by_zsdesk=false;
    for id in 1..=2{let mut fence=Fence::new(id,format!("Fence {id}"),FenceSource::Folder{root:root.into()},Placement{monitor:String::new(),x:50+id as i32*50,y:60,width:320,height:240});fence.view=FenceView::List;config.fences.push(fence);}
    config.next_id=3;
    let gfx=Gfx::new("Segoe UI")?;let mut host=DesktopHost::new(hwnd,root.join("profile/desktop-fences.json"),root.join("profile/explorer-session.json"),config,gfx);
    for id in 1..=2{
        let window=unsafe{CreateWindowExW(WS_EX_TOOLWINDOW|WS_EX_NOACTIVATE|WS_EX_LAYERED,fence_class,w!("Owned capture test"),WS_POPUP,50+id as i32*50,60,320,240,None,None,Some(instance),None)}?;
        owned.windows.push(window);unsafe{SetWindowLongPtrW(window,GWLP_USERDATA,id as isize);let _=ShowWindow(window,SW_SHOWNOACTIVATE);}
        let mut fence=fence::FenceWin::new(id);fence.hwnd=window;fence.selection.insert("retained-selection".into());host.fences.push(fence);
    }
    HOST.with(|slot|*slot.borrow_mut()=Some(host));HOST_HWND.with(|slot|slot.set(hwnd.0 as isize));Ok(owned)
}
fn set_interaction(){with_host(|host|host.fences[0].interaction=Some(fence::Interaction::Rubber{start:POINT{x:1,y:1},current:POINT{x:40,y:40},base:host.fences[0].selection.clone()}));}
fn verify_selection()->Result<()>{
    ensure!(with_host(|host|host.fences[0].selection.len()==1&&host.fences[0].selection.contains("retained-selection"))==Some(true),"cancel changed selection");Ok(())
}
fn captures(host:&OwnedHost)->Result<serde_json::Value>{
    let first=host.windows[0];let second=host.windows[1];
    set_interaction();unsafe{SetCapture(first);SendMessageW(first,WM_CANCELMODE,None,None);}
    ensure!(unsafe{GetCapture()}!=first,"cancel mode retained mouse capture");
    ensure!(with_host(|host|host.fences[0].interaction.is_none())==Some(true),"cancel mode retained interaction");verify_selection()?;
    // This covers synchronous cancellation while another host operation holds
    // its RefCell. The retry timer must eventually clear state without looping.
    set_interaction();unsafe{SetCapture(first);}
    HOST.with(|slot|{let _borrow=slot.borrow_mut();unsafe{SendMessageW(first,WM_CANCELMODE,None,None);}});
    ensure!(unsafe{GetCapture()}!=first,"reentrant cancel retained capture");pump(100);
    ensure!(with_host(|host|host.fences[0].interaction.is_none())==Some(true),"deferred cancel did not retire interaction");verify_selection()?;
    // A release notification for the old fence must not steal the new owner's capture.
    set_interaction();unsafe{SetCapture(first);SetCapture(second);}
    ensure!(unsafe{GetCapture()}==second,"old fence released another window's capture");
    ensure!(with_host(|host|host.fences[0].interaction.is_none())==Some(true),"capture loss retained interaction");verify_selection()?;
    let _=unsafe{ReleaseCapture()};unsafe{SendMessageW(first,WM_LBUTTONUP,None,Some(LPARAM(0)));}verify_selection()?;
    Ok(serde_json::json!({"cancel_mode_releases_owned_capture":true,"reentrant_cancel_deferred":true,"capture_change_preserves_new_owner":true,"selection_preserved":true,"mouse_up_after_cancel_preserves_selection":true}))
}
fn tray(host:&OwnedHost)->Result<serde_json::Value>{
    TRAY_ACTIONS.with(|slot|*slot.borrow_mut()=Some(Vec::new()));let _capture=CaptureActions;
    with_host(|state|state.tray_version4=true);
    let coords=((-120i16 as u16 as u32)|((350u32&0xffff)<<16))as usize;
    let send=|event:u32,icon:u32|unsafe{SendMessageW(host.hwnd,WM_TRAY,Some(WPARAM(coords)),Some(LPARAM(((icon<<16)|event)as isize)))};
    send(WM_LBUTTONUP,host::TRAY_ID);send(NIN_SELECT,host::TRAY_ID);send(WM_LBUTTONDBLCLK,host::TRAY_ID);
    send(NIN_SELECT|NINF_KEY,host::TRAY_ID);send(NIN_SELECT|NINF_KEY,host::TRAY_ID);
    send(WM_RBUTTONUP,host::TRAY_ID);send(WM_CONTEXTMENU,host::TRAY_ID);send(NIN_SELECT,host::TRAY_ID+1);
    let modern=TRAY_ACTIONS.with(|slot|slot.borrow().as_ref().unwrap().clone());
    ensure!(modern==vec![TrayAction::Open,TrayAction::Open,TrayAction::Open,TrayAction::Menu{anchor:Some((-120,350))}],"v4 event duplication/coordinates/keyboard dispatch mismatch: {modern:?}");
    TRAY_ACTIONS.with(|slot|slot.borrow_mut().as_mut().unwrap().clear());with_host(|state|state.tray_version4=false);
    for event in [WM_LBUTTONUP,WM_LBUTTONDBLCLK,WM_RBUTTONUP]{unsafe{SendMessageW(host.hwnd,WM_TRAY,Some(WPARAM(host::TRAY_ID as usize)),Some(LPARAM(event as isize)));}}
    unsafe{SendMessageW(host.hwnd,WM_TRAY,Some(WPARAM(host::TRAY_ID as usize+1)),Some(LPARAM(WM_LBUTTONUP as isize)));}
    let legacy=TRAY_ACTIONS.with(|slot|slot.borrow().as_ref().unwrap().clone());
    ensure!(legacy==vec![TrayAction::Open,TrayAction::Menu{anchor:None}],"legacy tray fallback mismatch: {legacy:?}");
    Ok(serde_json::json!({"v4_actions":modern.iter().map(|action|format!("{action:?}")).collect::<Vec<_>>(),"legacy_actions":legacy.iter().map(|action|format!("{action:?}")).collect::<Vec<_>>(),"single_activation_per_click":true,"enter_space_semantic_notifications":true,"negative_anchor_coordinates":true,"other_icon_ignored":true,"actual_tray_icon_registered":false}))
}
const MANAGEMENT_SUBCLASS:usize=0x5a534d47;
const MANAGEMENT_TIMER:usize=0x5a534d48;
const MANAGEMENT_WATCHDOG:usize=0x5a534d49;
thread_local!{static MANAGEMENT_INDEX:Cell<Option<u32>>=const{Cell::new(None)};static MANAGEMENT_ENTERED:Cell<bool>=const{Cell::new(false)};}
unsafe extern "system" fn management_probe(hwnd:HWND,msg:u32,wp:WPARAM,lp:LPARAM,_id:usize,_data:usize)->LRESULT{
    if msg==WM_ENTERMENULOOP{
        MANAGEMENT_ENTERED.with(|value|value.set(true));
        unsafe{SetTimer(Some(hwnd),MANAGEMENT_TIMER,80,None);SetTimer(Some(hwnd),MANAGEMENT_WATCHDOG,3000,None);}
    }
    if msg==WM_INITMENUPOPUP{
        let menu=HMENU(wp.0 as*mut _);let count=unsafe{GetMenuItemCount(Some(menu))};
        for index in 0..count{
            let mut label=[0u16;256];let count=unsafe{GetMenuStringW(menu,index as u32,Some(&mut label),MF_BYPOSITION)};
            if String::from_utf16_lossy(&label[..count.max(0)as usize])=="锁定格子"{MANAGEMENT_INDEX.with(|value|value.set(Some(index as u32)));}
        }
    }
    if msg==WM_TIMER&&wp.0==MANAGEMENT_TIMER{
        let _=unsafe{KillTimer(Some(hwnd),MANAGEMENT_TIMER)};
        let _=unsafe{PostMessageW(Some(hwnd),WM_CHAR,WPARAM(b'z' as usize),LPARAM(1))};return LRESULT(0);
    }
    if msg==WM_MENUCHAR{
        if let Some(index)=MANAGEMENT_INDEX.with(Cell::get){return LRESULT((index|(MNC_EXECUTE<<16))as isize);}
    }
    if msg==WM_TIMER&&wp.0==MANAGEMENT_WATCHDOG{zsdesk_windows::flyout::cancel();let _=unsafe{EndMenu()};return LRESULT(0);}
    unsafe{windows::Win32::UI::Shell::DefSubclassProc(hwnd,msg,wp,lp)}
}
fn management(host:&OwnedHost)->Result<serde_json::Value>{
    let owner=host.windows[0];let point=POINT{x:130,y:100};
    let spec=with_host(|state|state.fence_menu(1)).context("management spec")?;
    fn ids(spec:&MenuSpec,out:&mut Vec<String>){for item in &spec.items{match item{
        MenuItemSpec::Command{command:UiCommand::Custom{id,..},..}=>out.push(id.clone()),
        MenuItemSpec::Submenu{menu,..}=>ids(menu,out),_=>{}
    }}}
    let mut commands=Vec::new();ids(&spec,&mut commands);
    for id in ["paste","folder-system-menu","rename","delete","lock","collapse","new-folder-fence","new-tab-folder","icons-large","theme-dark","autostart","select-none"]{
        ensure!(commands.iter().any(|command|command==id),"management command missing: {id}");
    }
    ensure!(spec.items.iter().any(|item|matches!(item,MenuItemSpec::Submenu{label,..}if label=="所选项目")),"selected-item management submenu missing");
    let deferred=with_host(|state|state.menu_command(Some(1),"folder-system-menu",point)).context("background command routing")?;
    ensure!(matches!(deferred,After::FolderSystemMenu{fence:1,..}),"folder system menu did not use its explicit route");
    let pasted=with_host(|state|state.menu_command(Some(1),"paste",point)).context("paste command routing")?;
    ensure!(matches!(pasted,After::Verb{fence:1,verb:"paste",background:true}),"fence paste did not use the background paste route");
    MANAGEMENT_INDEX.with(|value|value.set(None));MANAGEMENT_ENTERED.with(|value|value.set(false));
    let before=zsdesk_windows::shell::menu_diagnostics();
    unsafe{windows::Win32::UI::Shell::SetWindowSubclass(owner,Some(management_probe),MANAGEMENT_SUBCLASS,0)}.ok()?;
    fence_menu(1,point);
    let _=unsafe{KillTimer(Some(owner),MANAGEMENT_TIMER)};let _=unsafe{KillTimer(Some(owner),MANAGEMENT_WATCHDOG)};
    let _=unsafe{windows::Win32::UI::Shell::RemoveWindowSubclass(owner,Some(management_probe),MANAGEMENT_SUBCLASS)};
    ensure!(MANAGEMENT_ENTERED.with(Cell::get),"title management popup did not open");
    ensure!(with_host(|state|state.config.fence(1).is_some_and(|fence|fence.locked))==Some(true),"chosen management command was lost or invoked another command");
    ensure!(MENU_WORKER.with(|worker|worker.borrow().is_none()),"management popup started a Shell worker");
    let after=zsdesk_windows::shell::menu_diagnostics();
    ensure!(before.presenter_creations==after.presenter_creations,"management popup activated the Shell presenter");
    Ok(serde_json::json!({"real_management_popup":true,"string_command_dispatched":"lock","selected_item_submenu":true,
        "folder_system_menu_explicit":true,"shell_worker_started":false,"production_profile_modified":false}))
}
fn exercise(root:&Path)->Result<serde_json::Value>{
    unsafe{OleInitialize(None)?;}let _apartment=Apartment;let host=owned_host(root)?;
    let pointer=captures(&host)?;let tray=tray(&host)?;let management=management(&host)?;drop(host);pump(30);
    Ok(serde_json::json!({"pointer":pointer,"tray":tray,"management":management,"desktop_start_called":false,"notification_area_modified":false}))
}
#[test]
#[ignore="Owned-window tray/capture message regression on a private desktop; requires ZSDESK_NATIVE_TEST_OUTPUT"]
fn native_fence_capture_and_tray_message_protocol()->Result<()>{
    let root=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("output")?).join("desktop-window-rules");ensure!(root.is_absolute(),"absolute output required");fs::create_dir_all(&root)?;
    let before=unsafe{GetForegroundWindow()};let name=wide(&format!("ZSDesk.WindowRules.{}",std::process::id()));let desktop=unsafe{CreateDesktopW(name.as_ptr(),std::ptr::null(),std::ptr::null(),0,0x000f00ff,std::ptr::null())};ensure!(!desktop.is_null(),"CreateDesktop failed");let raw=desktop as usize;let output=root.clone();
    let result=thread::spawn(move||->Result<serde_json::Value>{ensure!(unsafe{SetThreadDesktop(raw as*mut c_void)}!=0,"SetThreadDesktop failed");exercise(&output)}).join().map_err(|_|anyhow::anyhow!("window message test panicked")).and_then(|value|value);
    let closed=unsafe{CloseDesktop(desktop)}!=0;let current=unsafe{GetForegroundWindow()};let mut process=0;unsafe{GetWindowThreadProcessId(current,Some(&mut process));}let isolated=process!=std::process::id();
    let report=serde_json::json!({"state":if result.is_ok()&&closed&&isolated{"passed"}else{"failed"},"checks":result.as_ref().map_err(|error|format!("{error:#}")),"desktop_closed":closed,"foreground_unchanged":before==current,"test_did_not_activate":isolated});
    fs::write(root.join("verification.current.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");result?;ensure!(closed&&isolated,"desktop cleanup/focus failed");Ok(())
}
