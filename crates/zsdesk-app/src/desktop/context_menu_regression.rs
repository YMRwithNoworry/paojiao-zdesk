//! A mapping fence on an unswitched private desktop; no desktop start/organize.
use super::*;
use anyhow::ensure;
use std::{ffi::c_void,fs,path::Path,time::{Duration,Instant},thread};
use crate::context_menu_process::isolation_tests::{Environment,sparse};
use zsdesk_core::model::desktop::{DesktopConfig,Fence,FenceSource,FenceView,Placement};
use zsdesk_windows::shell::menu::QuickAction;
use windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled;

#[link(name="user32")]unsafe extern "system"{
    fn CreateDesktopW(name:*const u16,device:*const u16,mode:*const c_void,flags:u32,access:u32,attributes:*const c_void)->*mut c_void;
    fn SetThreadDesktop(desktop:*mut c_void)->i32;
    fn CloseDesktop(desktop:*mut c_void)->i32;
}
const MENU_TIMER:usize=0x5a530921;
const PROBE:usize=0x5a530922;
thread_local!{
    static MORE:Cell<bool>=const{Cell::new(false)};
    static ENTERED:Cell<Option<Instant>>=const{Cell::new(None)};
    static QUICK:Cell<bool>=const{Cell::new(false)};
    static DIALOGS:Cell<u32>=const{Cell::new(0)};
    static HEARTBEATS:Cell<u32>=const{Cell::new(0)};
}
unsafe extern "system" fn dismiss_owned_dialog(window:HWND,_data:LPARAM)->windows::core::BOOL{
    if class_name(window)=="#32770"{
        let owner=unsafe{GetWindow(window,GW_OWNER)}.unwrap_or_default();
        if with_host(|host|host.fences.iter().any(|fence|fence.hwnd==owner)).unwrap_or(false){
            // MessageBox's sole “OK” button may use IDCANCEL internally on
            // Windows. Read the actual owned control instead of guessing IDOK.
            let mut buttons=Vec::<HWND>::new();
            unsafe extern "system" fn child(hwnd:HWND,data:LPARAM)->windows::core::BOOL{
                if class_name(hwnd)=="Button"&&unsafe{IsWindowVisible(hwnd)}.as_bool()&&unsafe{IsWindowEnabled(hwnd)}.as_bool(){unsafe{&mut*(data.0 as*mut Vec<HWND>)}.push(hwnd);}true.into()
            }
            let _=unsafe{EnumChildWindows(Some(window),Some(child),LPARAM(&mut buttons as*mut Vec<HWND> as isize))};
            if let [button]=buttons.as_slice(){let id=unsafe{GetDlgCtrlID(*button)};
                if id>0{DIALOGS.with(|count|count.set(count.get()+1));let _=unsafe{PostMessageW(Some(window),WM_COMMAND,WPARAM(id as usize),LPARAM(button.0 as isize))};}
            }
        }
    }true.into()
}
// The test does not run the real host's global desktop repair/notify timers.
// Only the production menu completion callback is forwarded into the host.
unsafe extern "system" fn test_host_proc(hwnd:HWND,msg:u32,wp:WPARAM,lp:LPARAM)->LRESULT{
    if msg==menu_worker::READY{finish_menu_work();return LRESULT(0);}
    if msg==WM_TIMER&&wp.0==MENU_TIMER{
        HEARTBEATS.with(|beats|beats.set(beats.get()+1));
        let _=unsafe{EnumThreadWindows(windows::Win32::System::Threading::GetCurrentThreadId(),Some(dismiss_owned_dialog),LPARAM(0))};return LRESULT(0);
    }
    unsafe{DefWindowProcW(hwnd,msg,wp,lp)}
}
unsafe extern "system" fn probe(window:HWND,msg:u32,wp:WPARAM,lp:LPARAM,_id:usize,_data:usize)->LRESULT{
    if msg==WM_ENTERMENULOOP{ENTERED.with(|value|value.set(Some(Instant::now())));unsafe{SetTimer(Some(window),PROBE,40,None);}}
    if msg==WM_INITMENUPOPUP{
        let menu=HMENU(wp.0 as*mut _);let count=unsafe{GetMenuItemCount(Some(menu))};
        QUICK.with(|value|value.set((0..count).any(|index|unsafe{GetMenuItemID(menu,index)}==QuickAction::More.id())));
    }
    if msg==WM_TIMER&&wp.0==PROBE{
        let _=unsafe{KillTimer(Some(window),PROBE)};
        if MORE.with(Cell::get){let _=unsafe{PostMessageW(Some(window),WM_CHAR,WPARAM(b'm' as usize),LPARAM(1))};}
        else{zsdesk_windows::flyout::cancel();let _=unsafe{EndMenu()};}return LRESULT(0);
    }
    if msg==WM_NCDESTROY{let _=unsafe{windows::Win32::UI::Shell::RemoveWindowSubclass(window,Some(probe),PROBE)};}
    unsafe{windows::Win32::UI::Shell::DefSubclassProc(window,msg,wp,lp)}
}
struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}
struct OwnedHost{hwnd:HWND,fence:HWND}
impl Drop for OwnedHost{fn drop(&mut self){
    let worker=MENU_WORKER.with(|worker|worker.borrow_mut().take());drop(worker);
    if let Some(mut host)=HOST.with(|slot|slot.borrow_mut().take()){host.shutdown();}
    if unsafe{IsWindow(Some(self.fence))}.as_bool(){let _=unsafe{DestroyWindow(self.fence)};}
    HOST_HWND.with(|slot|slot.set(0));let _=unsafe{DestroyWindow(self.hwnd)};
}}
fn pump(ms:u64){let end=Instant::now()+Duration::from_millis(ms);while Instant::now()<end{
    let mut msg=MSG::default();for _ in 0..256{if !unsafe{PeekMessageW(&mut msg,None,0,0,PM_REMOVE)}.as_bool(){break;}if msg.message!=WM_QUIT{unsafe{let _=TranslateMessage(&msg);DispatchMessageW(&msg);}}}thread::sleep(Duration::from_millis(3));}}
fn setup(root:&Path,folder:&Path)->Result<(OwnedHost,HWND,POINT)>{
    let instance=HINSTANCE(unsafe{GetModuleHandleW(None)?}.0);let host_class=w!("ZSDesk.Native.FenceMenuHost");let fence_class=w!("ZSDesk.Native.FenceMenuWindow");
    unsafe{RegisterClassW(&WNDCLASSW{hInstance:instance,lpfnWndProc:Some(test_host_proc),lpszClassName:host_class,..Default::default()});
        RegisterClassW(&WNDCLASSW{hInstance:instance,lpfnWndProc:Some(fence_proc),lpszClassName:fence_class,..Default::default()});}
    let hwnd=unsafe{CreateWindowExW(WS_EX_TOOLWINDOW|WS_EX_NOACTIVATE,host_class,w!("Fence regression host"),WS_POPUP,0,0,0,0,None,None,Some(instance),None)}?;
    let mut owned=OwnedHost{hwnd,fence:HWND::default()};
    let mut config=DesktopConfig::default();config.settings.double_click_toggle=false;config.settings.hide_tray=true;config.settings.organize=false;config.settings.icons_hidden_by_zsdesk=false;
    let mut fence=Fence::new(1,"大文件测试",FenceSource::Folder{root:folder.into()},Placement{monitor:String::new(),x:120,y:100,width:620,height:420});fence.view=FenceView::List;config.fences.push(fence);config.next_id=2;
    let gfx=Gfx::new("Segoe UI")?;let mut host=DesktopHost::new(hwnd,root.join("profile/desktop-fences.json"),root.join("profile/explorer-session.json"),config,gfx);
    let window=unsafe{CreateWindowExW(WS_EX_TOOLWINDOW|WS_EX_NOACTIVATE|WS_EX_LAYERED,fence_class,w!("Mapping fence regression"),WS_POPUP,120,100,620,420,None,None,Some(instance),None)}?;
    owned.fence=window;
    unsafe{SetWindowLongPtrW(window,GWLP_USERDATA,1);}
    let mut fence=fence::FenceWin::new(1);fence.hwnd=window;fence.entries=desktop::enumerate_folder(folder)?;
    ensure!(fence.entries.len()==3&&fence.entries.iter().filter_map(|entry|entry.size).sum::<u64>()==24u64*1024*1024*1024,"fence selection metadata lost 64-bit sizes");
    fence.selection.extend(fence.entries.iter().map(|entry|entry.key.clone()));fence.focus=Some(0);fence.anchor=Some(0);fence.notify_path=Some(folder.into());
    let geometry=fence.geometry(host.config.fence(1).unwrap());let cell=geometry.cells.first().context("first fence cell")?.cell;
    let point=POINT{x:cell.x+20,y:cell.y+cell.height/2};host.fences.push(fence);
    HOST.with(|slot|*slot.borrow_mut()=Some(host));HOST_HWND.with(|slot|slot.set(hwnd.0 as isize));
    unsafe{let _=ShowWindow(window,SW_SHOWNOACTIVATE);SetTimer(Some(hwnd),MENU_TIMER,20,None);windows::Win32::UI::Shell::SetWindowSubclass(window,Some(probe),PROBE,0)}.ok()?;
    Ok((owned,window,point))
}
fn right_click(window:HWND,point:POINT){let packed=((point.x as u32&0xffff)|((point.y as u32&0xffff)<<16))as isize;unsafe{SendMessageW(window,WM_RBUTTONUP,None,Some(LPARAM(packed)));}}
fn exercise(root:&Path,name:&str)->Result<serde_json::Value>{
    let _desktop=Environment::set("ZSDESK_MENU_TEST_DESKTOP",name);let _root=Environment::set("ZSDESK_MENU_ROOT",root.join("workers"));
    unsafe{OleInitialize(None)?;}let _apartment=Apartment;
    let folder=root.join("files");fs::create_dir_all(&folder)?;let(files,_,records)=sparse(&folder)?;let(host,window,point)=setup(root,&folder)?;
    let before=zsdesk_windows::shell::menu_diagnostics();let mut rows=Vec::new();
    for failed in [true,false,false]{
        let _probe=Environment::set("ZSDESK_MENU_TEST_PROBE",if failed{"crash"}else{"menu-dismiss"});
        let previous=menu_diagnostics()["recent"].as_array().map_or(0,Vec::len);right_click(window,point);
        let end=Instant::now()+Duration::from_secs(12);
        loop{pump(20);let report=menu_diagnostics();if report["recent"].as_array().is_some_and(|rows|rows.len()>previous){break;}ensure!(Instant::now()<end,"fence extension result timed out: {report}");}
        ensure!(unsafe{IsWindow(Some(window))}.as_bool()&&with_host(|host|host.fences.len())==Some(1),"extension failure lost the mapping fence");
        let report=menu_diagnostics();let timing=report["recent"].as_array().and_then(|rows|rows.last()).context("menu timing")?;
        let child=timing["extension_process"].as_u64().with_context(||format!("fence did not start an extension process: {timing}"))?;
        ensure!(child!=u64::from(std::process::id()),"fence executed extension in parent: {timing}");
        if failed{ensure!(DIALOGS.with(Cell::get)>0,"fence extension failure had no error feedback");}
        else{ensure!(timing["input_to_menu_microseconds"].as_u64().is_some(),"real menu visibility was not observed: {timing}");}
        rows.push(serde_json::json!({"injected_access_violation":failed,"timing":timing,"host_alive":true}));
    }
    let after=zsdesk_windows::shell::menu_diagnostics();
    ensure!(before.presenter_creations==after.presenter_creations&&before.last_public_populate_microseconds==after.last_public_populate_microseconds,"fence UI constructed Shell extensions in parent");
    ensure!(rows[2]["timing"]["isolation"]["reused_process"]==true,"warm fence menu did not reuse eligible worker: {rows:?}");
    let beats=HEARTBEATS.with(Cell::get);drop(host);pump(40);drop(files);
    crate::context_menu_process::shutdown_idle_worker();
    let end=Instant::now()+Duration::from_secs(5);while crate::context_menu_process::pending_workers()>0&&Instant::now()<end{pump(10);}ensure!(crate::context_menu_process::pending_workers()==0&&pending_menu_threads()==0,"fence menu worker did not retire");
    Ok(serde_json::json!({"actual_mapping_fence":true,"host_start_called":false,"organize_called":false,"fixtures":records,"selected_bytes":24u64*1024*1024*1024,
        "query_context_menu_in_main":false,"workers":rows,"host_heartbeats":beats,"fixtures_removed":true,"all_workers_exited":true}))
}
#[test]
#[ignore="Actual mapping fence right-click on a private desktop with sparse 24 GiB files; requires ZSDESK_NATIVE_TEST_OUTPUT"]
fn native_mapping_fence_large_selection_immediate_context_menu()->Result<()>{
    let root=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("output")?).join("fence-quick-menu");ensure!(root.is_absolute(),"absolute output required");fs::create_dir_all(&root)?;
    let before=unsafe{GetForegroundWindow()};let name=format!("ZSDesk.FenceQuickMenu.{}",std::process::id());let desktop=unsafe{CreateDesktopW(wide(&name).as_ptr(),std::ptr::null(),std::ptr::null(),0,0x000f00ff,std::ptr::null())};ensure!(!desktop.is_null(),"CreateDesktop failed");let raw=desktop as usize;let destination=root.clone();
    let result=thread::spawn(move||->Result<serde_json::Value>{ensure!(unsafe{SetThreadDesktop(raw as*mut c_void)}!=0,"SetThreadDesktop failed");exercise(&destination,&name)}).join().map_err(|_|anyhow::anyhow!("fence menu test panicked")).and_then(|value|value);
    let closed=unsafe{CloseDesktop(desktop)}!=0;let current=unsafe{GetForegroundWindow()};let mut process=0;unsafe{GetWindowThreadProcessId(current,Some(&mut process));}
    let isolated=process!=std::process::id();let report=serde_json::json!({"state":if result.is_ok()&&closed&&isolated{"passed"}else{"failed"},"checks":result.as_ref().map_err(|error|format!("{error:#}")),"desktop_closed":closed,"foreground_unchanged":before==current,"test_did_not_activate":isolated});
    fs::write(root.join("verification.current.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");result?;ensure!(closed&&isolated,"desktop cleanup/focus failed");Ok(())
}
