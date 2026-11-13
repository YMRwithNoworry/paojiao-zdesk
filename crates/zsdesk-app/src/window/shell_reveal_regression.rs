//! Real Shell reveal API through the application registry into native views.
use super::*;
use anyhow::{Context,ensure};
use std::{ffi::c_void,fs,path::{Path,PathBuf},thread,time::{Duration,Instant}};
use crate::shell_window_registry::{Registry,RevealFixtureScope,fixture_window_snapshot};
use windows::{core::Interface,Win32::{System::{Com::{CoCreateInstance,CLSCTX_LOCAL_SERVER},Variant::InitVariantFromBuffer},
    UI::Shell::{IShellWindows,IWebBrowserApp,ShellWindows,SHOpenFolderAndSelectItems,SWC_BROWSER,SWFO_NEEDDISPATCH,Common::ITEMIDLIST}}};

#[link(name="user32")]unsafe extern "system"{
    fn CreateDesktopW(name:*const u16,device:*const u16,mode:*const c_void,flags:u32,access:u32,attributes:*const c_void)->*mut c_void;
    fn GetThreadDesktop(thread:u32)->*mut c_void;
    fn SetThreadDesktop(desktop:*mut c_void)->i32;
    fn CloseDesktop(desktop:*mut c_void)->i32;
}
struct Apartment;
impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}
struct Runner{hwnd:HWND,slot:Box<Slot>}
impl Drop for Runner{fn drop(&mut self){let _=unsafe{DestroyWindow(self.hwnd)};}}

fn pump(){
    for _ in 0..512{
        let mut message=MSG::default();if !unsafe{PeekMessageW(&mut message,None,0,0,PM_REMOVE)}.as_bool(){break;}
        if message.message!=WM_QUIT{unsafe{let _=TranslateMessage(&message);DispatchMessageW(&message);}}
    }
}
fn wait_for(label:&str,mut predicate:impl FnMut()->bool)->Result<()>{
    let deadline=Instant::now()+Duration::from_secs(12);
    loop{pump();if predicate(){return Ok(());}
        ensure!(Instant::now()<deadline,"{label}");thread::sleep(Duration::from_millis(15));}
}
fn folder_pidl(path:&Path)->Result<Vec<u8>>{
    zsdesk_windows::shell::pidl::location_pidl(&ShellLocation::Path(path.into())).context("resolve reveal fixture PIDL")
}
fn child_pidl(absolute:&[u8])->Result<Vec<u8>>{
    ensure!(zsdesk_windows::shell::pidl::valid(absolute),"absolute fixture PIDL invalid");
    let mut at=0;let mut last=0;
    while at+2<=absolute.len(){let count=u16::from_le_bytes([absolute[at],absolute[at+1]])as usize;if count==0{break;}last=at;at+=count;}
    Ok(absolute[last..].to_vec())
}
fn selected(slot:&Slot,right:bool,expected:&[PathBuf])->bool{
    let Ok(slot)=slot.try_borrow()else{return false;};let Some(state)=slot.as_ref()else{return false;};
    let actual=state.pane_selection_paths(right);
    actual.len()==expected.len()&&expected.iter().all(|path|actual.iter().any(|value|crate::explorer_forwarder::same_path(Path::new(value),path)))
}

fn run(default_shell:(isize,u32),mut protected:Vec<(isize,u32)>,output:&Path)->Result<serde_json::Value>{
    use std::os::windows::fs::MetadataExt;
    unsafe{OleInitialize(None)}.context("initialize full-window reveal STA")?;let _apartment=Apartment;
    protected.extend(fixture_window_snapshot()?);protected.sort_unstable();protected.dedup();
    let root=output.join(format!("full-view-reveal-{}",std::process::id()));
    let left=root.join("left");let right=root.join("right 中文");
    for path in [&root,&left,&right]{fs::create_dir_all(path)?;ensure!(fs::symlink_metadata(path)?.file_attributes()&0x400==0,"fixture must use real directories");}
    let left_keep=left.join("keep-left.txt");let left_target=left.join("left-target.txt");
    let targets=[right.join("2.exe"),right.join("下载 中文.txt")];
    for path in [&left_keep,&left_target,&targets[0],&targets[1]]{fs::write(path,b"owned Shell reveal fixture; not an executable")?;}
    for index in 0..80{fs::write(right.join(format!("other-{index:03}.txt")),b"fixture")?;}
    let _fallbacks=RevealFixtureScope::new(protected,vec![left.clone(),right.clone()]);
    unsafe{InitCommonControlsEx(&INITCOMMONCONTROLSEX{dwSize:size_of::<INITCOMMONCONTROLSEX>()as u32,dwICC:ICC_STANDARD_CLASSES|ICC_BAR_CLASSES})}.ok()?;
    let instance=HINSTANCE(unsafe{GetModuleHandleW(None)}?.0);let class=wide("ZSDesk.Native.FullRevealRegression");
    unsafe{RegisterClassW(&WNDCLASSW{hInstance:instance,lpfnWndProc:Some(window_proc),lpszClassName:PCWSTR(class.as_ptr()),..Default::default()});}
    let mut session=ExplorerSession::default();session.tabs=vec![ExplorerTab::new(1,ShellLocation::Path(left.clone()))];session.active_tab=0;session.next_id=2;
    let slot=Box::new(RefCell::new(Some(WindowState::new(session,root.join("profile/explorer-session.json"),None,false))));
    let hwnd=unsafe{CreateWindowExW(WS_EX_NOACTIVATE,PCWSTR(class.as_ptr()),windows::core::w!("ZSDesk real reveal fixture"),
        WS_OVERLAPPEDWINDOW|WS_CLIPCHILDREN,-30000,-30000,1040,700,None,None,Some(instance),Some((&*slot as*const Slot).cast()))}?;
    let runner=Runner{hwnd,slot};
    unsafe{runner.slot.borrow_mut().as_mut().unwrap().create(hwnd)?;let _=ShowWindow(hwnd,SW_SHOWNOACTIVATE);}
    wait_for("left fixture navigation did not complete",||runner.slot.borrow().as_ref().unwrap().shell_entries().len()==1)?;
    {
        let mut slot=runner.slot.borrow_mut();let state=slot.as_mut().unwrap();
        state.toggle_dual_pane();
    }
    wait_for("second fixture pane did not initialize",||runner.slot.borrow().as_ref().unwrap().shell_entries().len()==2)?;
    {
        let mut slot=runner.slot.borrow_mut();let state=slot.as_mut().unwrap();
        state.activate_pane(true);unsafe{state.navigate_to(ShellLocation::Path(right.clone()));}state.activate_pane(false);
    }
    wait_for("right fixture navigation did not complete",||{
        let slot=runner.slot.borrow();let state=slot.as_ref().unwrap();
        state.shell_entries().len()==2&&state.pane_title(true)==right.to_string_lossy()
    })?;
    {
        let mut slot=runner.slot.borrow_mut();let state=slot.as_mut().unwrap();
        state.browser.as_ref().unwrap().select_child(left_keep.file_name().unwrap())?;
        state.shell_registry=Some(Registry::new_for_fixture(hwnd,default_shell)?);state.publish_shell_windows();
    }
    wait_for("left sentinel was not selected",||selected(&runner.slot,false,std::slice::from_ref(&left_keep)))?;
    let shell:IShellWindows=unsafe{CoCreateInstance(&ShellWindows,None,CLSCTX_LOCAL_SERVER)}.context("connect real ShellWindows server")?;
    let left_pidl=folder_pidl(&left)?;let right_pidl=folder_pidl(&right)?;
    let registered=|bytes:&[u8]|->bool{
        let Ok(location)=(unsafe{InitVariantFromBuffer(bytes.as_ptr().cast(),bytes.len()as u32)})else{return false;};
        let mut found=0;
        let Ok(dispatch)=(unsafe{shell.FindWindowSW(&location,&windows::Win32::System::Variant::VARIANT::default(),SWC_BROWSER,&mut found,SWFO_NEEDDISPATCH)})else{return false;};
        let Ok(app)=dispatch.cast::<IWebBrowserApp>()else{return false;};
        unsafe{app.HWND()}.is_ok_and(|value|value.0==hwnd.0 as isize)
    };
    wait_for("real views were not registered",||registered(&left_pidl)&&registered(&right_pidl))?;
    let absolute=targets.iter().map(|path|folder_pidl(path)).collect::<Result<Vec<_>>>()?;
    let children=absolute.iter().map(|pidl|child_pidl(pidl)).collect::<Result<Vec<_>>>()?;
    let relative=children.iter().map(|pidl|pidl.as_ptr().cast::<ITEMIDLIST>()).collect::<Vec<_>>();
    // No Slot borrow is held across Shell COM: normal WM_SELECT dispatch must
    // run WindowState::apply_shell_selections and mutate the real native view.
    unsafe{SHOpenFolderAndSelectItems(right_pidl.as_ptr().cast(),Some(&relative),0)}.context("real reveal into inactive pane, relative children")?;
    wait_for("real right view did not select both relative children",||selected(&runner.slot,true,&targets))?;
    ensure!(runner.slot.borrow().as_ref().unwrap().active_pane_right,"revealed inactive pane did not become active");
    ensure!(selected(&runner.slot,false,std::slice::from_ref(&left_keep)),"revealing the right folder changed left selection");
    let absolute_child=[absolute[1].as_ptr().cast::<ITEMIDLIST>()];
    unsafe{SHOpenFolderAndSelectItems(right_pidl.as_ptr().cast(),Some(&absolute_child),0)}.context("real view Chromium absolute child")?;
    wait_for("real view did not select Chromium absolute child",||selected(&runner.slot,true,&targets[1..]))?;
    unsafe{SHOpenFolderAndSelectItems(absolute[0].as_ptr().cast(),None,0)}.context("real view absolute item without child array")?;
    wait_for("real view did not select absolute 2.exe",||selected(&runner.slot,true,&targets[..1]))?;
    let left_absolute=folder_pidl(&left_target)?;let left_child=child_pidl(&left_absolute)?;let left_children=[left_child.as_ptr().cast::<ITEMIDLIST>()];
    unsafe{SHOpenFolderAndSelectItems(left_pidl.as_ptr().cast(),Some(&left_children),0)}.context("real reveal back into inactive left pane")?;
    wait_for("real left view did not select requested file",||selected(&runner.slot,false,std::slice::from_ref(&left_target)))?;
    ensure!(!runner.slot.borrow().as_ref().unwrap().active_pane_right,"left reveal did not activate the left pane");
    ensure!(selected(&runner.slot,true,&targets[..1]),"left reveal changed right file selection");
    let mut registry=runner.slot.borrow_mut().as_mut().unwrap().shell_registry.take().context("fixture registry missing")?;
    registry.publish(Vec::new());
    wait_for("real view registrations were not revoked",||!registered(&left_pidl)&&!registered(&right_pidl))?;
    registry.stop();wait_for("real view registry worker did not stop",||registry.fixture_worker_finished())?;registry.stop();
    drop(registry);
    Ok(serde_json::json!({"state":"passed","fixture_root":root,"real_window_state":true,"real_shell_views":2,
        "actual_shell_api":true,"relative_children_selected":2,"chromium_absolute_child_selected":true,
        "absolute_item_selected":true,"inactive_pane_activated":true,"other_pane_selection_preserved":true,
        "registry_revoked":true,"registry_worker_stopped":true,"global_takeover_changed":false}))
}

#[test]
#[ignore="Requires ZSDESK_SHELL_API_FIXTURE_PROBE=1; real Shell API to real application views on an unswitched private desktop"]
fn native_full_window_shell_reveal_selects_real_items()->Result<()>{
    ensure!(std::env::var("ZSDESK_SHELL_API_FIXTURE_PROBE").as_deref()==Ok("1"),"explicit Shell API fixture opt-in required");
    let output=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("isolated output required")?);
    ensure!(output.is_absolute(),"output must be absolute");fs::create_dir_all(&output)?;
    let shell=unsafe{GetShellWindow()};let mut pid=0;unsafe{GetWindowThreadProcessId(shell,Some(&mut pid));}
    ensure!(!shell.0.is_null()&&pid!=0,"default desktop Shell is not available");
    let default_shell=(shell.0 as isize,pid);let protected=fixture_window_snapshot()?;
    let foreground=unsafe{GetForegroundWindow()};
    let name=wide(&format!("ZSDesk.FullRevealProbe.{}",std::process::id()));
    let desktop=unsafe{CreateDesktopW(name.as_ptr(),std::ptr::null(),std::ptr::null(),0,0x000f00ff,std::ptr::null())};
    ensure!(!desktop.is_null(),"private full-reveal desktop creation failed");let handle=desktop as usize;let destination=output.clone();
    let result=thread::spawn(move||->Result<serde_json::Value>{
        let original=unsafe{GetThreadDesktop(windows::Win32::System::Threading::GetCurrentThreadId())};
        ensure!(unsafe{SetThreadDesktop(handle as*mut c_void)}!=0,"attach full-reveal desktop failed");
        let result=run(default_shell,protected,&destination);
        let restored=unsafe{SetThreadDesktop(original)}!=0;
        let result=result?;ensure!(restored,"release full-reveal thread desktop failed");Ok(result)
    }).join().map_err(|_|anyhow::anyhow!("full-reveal thread panicked")).and_then(|result|result);
    let closed=unsafe{CloseDesktop(desktop)}!=0;let mut foreground_pid=0;
    let after=unsafe{GetForegroundWindow()};unsafe{GetWindowThreadProcessId(after,Some(&mut foreground_pid));}
    let isolated=foreground_pid!=std::process::id();
    let report=serde_json::json!({"state":if result.is_ok()&&closed&&isolated{"passed"}else{"failed"},
        "checks":result.as_ref().map_err(|error|format!("{error:#}")),"private_desktop_closed":closed,
        "foreground_unchanged":foreground==after,"test_did_not_activate":isolated});
    fs::write(output.join("full-view-shell-reveal.current.json"),serde_json::to_vec_pretty(&report)?)?;
    println!("{report}");result?;ensure!(closed&&isolated,"full-reveal desktop cleanup/isolation failed");Ok(())
}
