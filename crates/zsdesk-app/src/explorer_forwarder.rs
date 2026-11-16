//! Event-driven forwarding of newly created, single-tab Explorer folder windows.
//! Visibility is leased by a separate, bounded recovery process; Shell COM can
//! block without leaving the source invisible. Existing windows are untouched.
use std::{collections::{HashMap,HashSet},path::{Path,PathBuf},sync::{Arc,atomic::{AtomicBool,Ordering}},thread::{self,JoinHandle},time::{Duration,Instant}};
use anyhow::{ensure,Context,Result};
use windows::{core::{Interface,PCWSTR},Win32::{Foundation::{HWND,LPARAM,WPARAM},System::{Com::{CoCreateInstance,CoInitializeEx,CoUninitialize,CLSCTX_LOCAL_SERVER,COINIT_APARTMENTTHREADED},DataExchange::COPYDATASTRUCT,Variant::VARIANT},UI::{Shell::{IShellWindows,IWebBrowser2,IShellFolderViewDual,Folder2,ShellWindows},WindowsAndMessaging::*}}};
use windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled;
mod visibility_guard;
use visibility_guard::{Connection,Event,Lease};

/// Invoke before application startup so the recovery helper does not enter the
/// normal single-instance/runtime lifecycle.
pub(crate) fn run_guard_if_requested()->Option<Result<()>>{visibility_guard::run_if_requested()}

pub(crate) fn same_path(a:&Path,b:&Path)->bool{
    use std::os::windows::ffi::OsStrExt;
    let units=|p:&Path|p.as_os_str().encode_wide().map(|v|if v==47{92}else{v}).collect::<Vec<_>>();
    let mut a=units(a);let mut b=units(b);while a.len()>3&&a.last()==Some(&92){a.pop();}while b.len()>3&&b.last()==Some(&92){b.pop();}
    (unsafe{windows::Win32::Globalization::CompareStringOrdinal(&a,&b,true)})==windows::Win32::Globalization::CSTR_EQUAL
}
pub fn owns_integration(profile:&Path)->bool{
    use zsdesk_windows::integration;
    std::path::absolute(profile).ok().zip(std::path::absolute(crate::storage::config_path()).ok()).is_some_and(|(a,b)|same_path(&a,&b))
        && std::env::current_exe().is_ok_and(|exe|integration::owns_folder_integration(&exe))
}
fn forwarding_policy(registered:bool,window_open:bool,permanent:bool,temporary:bool)->bool{
    if registered{permanent}else{window_open&&temporary}
}
pub(crate) fn enabled(profile:&Path)->bool{
    !crate::edition::DESKTOP_ONLY&&crate::storage::load(profile).is_ok_and(|session|
        forwarding_policy(owns_integration(profile),find_target(profile).is_some(),session.ui.forward_unhandled_windows,session.ui.forward_while_running))
}
struct Apartment;
impl Drop for Apartment{fn drop(&mut self){unsafe{CoUninitialize();}}}
#[derive(Clone)]
struct Snapshot{window:HWND,pid:u32,folder:PathBuf,selected:Vec<PathBuf>,browser:IWebBrowser2,this_pc:bool}
impl Snapshot{
    fn same_location(&self,other:&Self)->bool{self.window==other.window&&self.pid==other.pid&&self.this_pc==other.this_pc&&same_path(&self.folder,&other.folder)}
    fn equivalent(&self,other:&Self)->bool{self.window==other.window&&self.pid==other.pid&&self.this_pc==other.this_pc&&same_path(&self.folder,&other.folder)
        && self.selected.len()==other.selected.len()&&self.selected.iter().all(|a|other.selected.iter().any(|b|same_path(a,b)))}
}
struct Snapshots{views:Vec<Snapshot>,multiple:HashSet<isize>}
fn read_snapshot(browser:&IWebBrowser2,window:HWND,pid:u32)->windows::core::Result<Snapshot>{
    let view=unsafe{browser.Document()?}.cast::<IShellFolderViewDual>()?;
    let folder=unsafe{view.Folder()?}.cast::<Folder2>()?;
    let folder=PathBuf::from(unsafe{folder.Self_()?.Path()?}.to_string());
    let items=unsafe{view.SelectedItems()?};let count=unsafe{items.Count()?};
    if !(0..=128).contains(&count){return Err(windows::core::Error::from_hresult(windows::core::HRESULT(0x80070057u32 as i32)));}
    let mut selected=Vec::with_capacity(count as usize);
    for i in 0..count{selected.push(PathBuf::from(unsafe{items.Item(&VARIANT::from(i))?.Path()?}.to_string()));}
    let this_pc=folder.to_string_lossy().eq_ignore_ascii_case("::{20D04FE0-3AEA-1069-A2D8-08002B30309D}");
    Ok(Snapshot{window,pid,folder,selected,browser:browser.clone(),this_pc})
}
fn snapshots(shell:&IShellWindows)->Result<Snapshots>{
    snapshots_for(shell,None)
}
fn snapshots_for(shell:&IShellWindows,requested:Option<&HashSet<isize>>)->Result<Snapshots>{
    let mut result=Vec::new();let mut counts=HashMap::new();
    for index in 0..unsafe{shell.Count()?}.clamp(0,128){
        let Ok(dispatch)=(unsafe{shell.Item(&VARIANT::from(index))})else{continue;};
        let Ok(browser)=dispatch.cast::<IWebBrowser2>()else{continue;};
        let Ok(handle)=(unsafe{browser.HWND()})else{continue;};let window=HWND(handle.0 as*mut _);
        *counts.entry(window.0 as isize).or_insert(0usize)+=1;
        if requested.is_some_and(|windows|!windows.contains(&(window.0 as isize))){continue;}
        let mut class=[0u16;80];let n=unsafe{GetClassNameW(window,&mut class)}.max(0)as usize;
        if !matches!(String::from_utf16_lossy(&class[..n]).as_str(),"CabinetWClass"|"ExploreWClass"){continue;}
        let expected=PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(||r"C:\Windows".into())).join("explorer.exe");
        if !unsafe{browser.FullName()}.is_ok_and(|name|same_path(Path::new(&name.to_string()),&expected)){continue;}
        let mut pid=0;unsafe{GetWindowThreadProcessId(window,Some(&mut pid));}if pid==0{continue;}
        let snapshot=read_snapshot(&browser,window,pid);
        if let Ok(snapshot)=snapshot{if (snapshot.this_pc&&snapshot.selected.is_empty())||(snapshot.folder.is_absolute()&&snapshot.selected.iter().all(|p|p.parent().is_some_and(|parent|same_path(parent,&snapshot.folder)))){result.push(snapshot);}}
    }
    let multiple=counts.iter().filter_map(|(window,count)|(*count>1).then_some(*window)).collect();
    result.retain(|snapshot|counts.get(&(snapshot.window.0 as isize))==Some(&1));Ok(Snapshots{views:result,multiple})
}
#[cfg(test)]fn windows(shell:&IShellWindows)->Result<Vec<Snapshot>>{Ok(snapshots(shell)?.views)}
fn find_target(profile:&Path)->Option<HWND>{
    let property=crate::instance::property(profile);let mut previous=None;
    while let Ok(hwnd)=unsafe{FindWindowExW(None,previous,windows::core::w!("ZSDesk.Native.Explorer"),None)}{
        previous=Some(hwnd);if unsafe{GetPropW(hwnd,PCWSTR(property.as_ptr()))}.0 as usize==1{return Some(hwnd);}
    }None
}
fn send_with_budget(window:HWND,tag:usize,packet:&[u8],milliseconds:u32)->bool{
    let data=COPYDATASTRUCT{dwData:tag,cbData:packet.len()as u32,lpData:packet.as_ptr()as*mut _};let mut response=0;
    unsafe{SendMessageTimeoutW(window,WM_COPYDATA,WPARAM(0),LPARAM(&data as*const _ as isize),SMTO_ABORTIFHUNG,milliseconds,Some(&mut response))}.0!=0&&response==1
}
#[cfg(test)]fn send(window:HWND,tag:usize,packet:&[u8])->bool{send_with_budget(window,tag,packet,500)}
#[cfg(test)]
fn forward(profile:&Path,executable:&Path,snapshot:&Snapshot)->Result<()>{
    let packet=if snapshot.this_pc{serde_json::to_vec(&(Some(zsdesk_core::model::explorer::ShellLocation::ThisPc),None::<String>))?}
        else{crate::reveal::encode(&snapshot.folder,&snapshot.selected).context("文件位置请求超出支持范围")?};
    if find_target(profile).is_none(){
        use std::os::windows::process::CommandExt;
        let mut command=std::process::Command::new(executable);
        command.arg("--profile-dir").arg(profile.parent().context("配置目录缺失")?);
        if snapshot.this_pc{command.arg("--this-pc");}else{command.arg(&snapshot.folder);}
        command
            .creation_flags(0x08000000).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn()?;
    }
    let deadline=Instant::now()+Duration::from_secs(8);
    let target=loop{if let Some(target)=find_target(profile){break target;}ensure!(Instant::now()<deadline,"文件管理窗口未就绪");thread::sleep(Duration::from_millis(50));};
    ensure!(send(target,if snapshot.this_pc{crate::instance::OPEN_MAGIC}else{crate::reveal::COPYDATA_TAG},&packet),"目标窗口未接受文件位置请求");
    let deadline=Instant::now()+Duration::from_secs(4);
    while !send(target,if snapshot.this_pc{crate::reveal::VERIFY_THIS_PC}else{crate::reveal::VERIFY_TAG},if snapshot.this_pc{&[]}else{&packet}){ensure!(Instant::now()<deadline,"导航或选中状态未确认，保留系统窗口");thread::sleep(Duration::from_millis(75));}
    // A fresh snapshot prevents closing a window the user has already navigated
    // elsewhere, turned into multiple tabs, or placed behind a modal dialog.
    let shell:IShellWindows=unsafe{CoCreateInstance(&ShellWindows,None,CLSCTX_LOCAL_SERVER)}?;
    let latest=windows(&shell)?.into_iter().find(|item|item.window==snapshot.window).context("来源窗口状态已改变")?;
    ensure!(snapshot.equivalent(&latest)&&unsafe{IsWindowEnabled(snapshot.window)}.as_bool(),"来源窗口已改变，保留系统窗口");
    let foreground=unsafe{GetForegroundWindow()}==snapshot.window;
    if foreground{crate::instance::request_activation(target);}
    unsafe{snapshot.browser.Quit()}?;Ok(())
}

const SELECTION_SETTLE_MS:u64=150;
const EMPTY_OBSERVATION_MS:u64=600;
const SELECTED_OBSERVATION_MS:u64=450;
const SELECTION_STABLE_MS:u64=150;
const CLOSE_RESERVE_MS:u64=120;
struct Candidate{lease:Lease,hidden:bool,activation_launched:bool,ready_at:Option<u64>,first:Option<Snapshot>,observed:Option<(Snapshot,u64)>,sent:Option<Snapshot>,launched:bool,committed:bool}
impl Candidate{
    fn new(lease:Lease)->Self{Self{lease,hidden:false,activation_launched:false,ready_at:None,first:None,observed:None,sent:None,launched:false,committed:false}}
}
fn packet(snapshot:&Snapshot)->Result<Vec<u8>>{
    if snapshot.this_pc{Ok(serde_json::to_vec(&(Some(zsdesk_core::model::explorer::ShellLocation::ThisPc),None::<String>))?)}
    else{crate::reveal::encode(&snapshot.folder,&snapshot.selected).context("文件位置请求超出支持范围")}
}
fn start_target(profile:&Path,snapshot:&Snapshot)->Result<()>{
    use std::os::windows::process::CommandExt;
    let mut command=std::process::Command::new(std::env::current_exe()?);
    command.arg("--profile-dir").arg(profile.parent().context("配置目录缺失")?);
    if snapshot.this_pc{command.arg("--this-pc");}else{command.arg(&snapshot.folder);}
    command.creation_flags(0x08000000).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn()?;
    Ok(())
}
fn can_deliver(ready_at:u64,now:u64,has_selection:bool)->bool{has_selection||now.saturating_sub(ready_at)>=SELECTION_SETTLE_MS}
fn close_due(deadline:u64,now:u64,ready:u64,stable:u64,has_selection:bool)->bool{
    let observation=if has_selection{SELECTED_OBSERVATION_MS}else{EMPTY_OBSERVATION_MS};
    now>=ready&&now>=stable&&now.saturating_sub(ready)>=observation
        &&now.saturating_sub(stable)>=SELECTION_STABLE_MS&&now.saturating_add(CLOSE_RESERVE_MS)<=deadline
}
fn launch_target_once(launched:&mut bool,launch:impl FnOnce()->Result<()>)->Result<()>{
    if !*launched{launch()?;*launched=true;}
    Ok(())
}
fn activate_from_source(profile:&Path,snapshot:&Snapshot,target:HWND,launched:&mut bool)->Result<bool>{
    if unsafe{GetForegroundWindow()}==target{return Ok(true);}
    if unsafe{GetForegroundWindow()}!=snapshot.window||!unsafe{IsWindowEnabled(target)}.as_bool(){return Ok(false);}
    crate::instance::request_activation(target);
    if unsafe{GetForegroundWindow()}==target{return Ok(true);}
    if !*launched{
        // Ask the foreground Shell to launch the normal activation-only entry.
        // Keep input queues independent and retain the source until activation.
        use windows::{core::BSTR,Win32::UI::Shell::IShellDispatch2};
        let view=unsafe{snapshot.browser.Document()}?.cast::<IShellFolderViewDual>()?;
        let shell=unsafe{view.Application()}?.cast::<IShellDispatch2>()?;
        let executable=std::env::current_exe()?;
        let arguments=format!("--profile-dir {}",crate::quick_search::control::quote_argument(&profile.parent().context("配置目录缺失")?.to_string_lossy()));
        unsafe{shell.ShellExecute(&BSTR::from(executable.to_string_lossy().as_ref()),&VARIANT::from(BSTR::from(arguments.as_str())),
            &VARIANT::default(),&VARIANT::from(BSTR::from("open")),&VARIANT::from(1i32))}?;*launched=true;
    }Ok(false)
}
fn update_candidate(profile:&Path,guard:&Connection,candidate:&mut Candidate,snapshot:&Snapshot)->Result<()>{
    if !candidate.hidden{
        // Visibility is acquired by the independent reader immediately after
        // creation, before this thread enters any Shell COM call.
        return Ok(());
    }
    ensure!(guard.active(&candidate.lease),"来源窗口隐藏租约已结束");
    ensure!(snapshot.pid==candidate.lease.identity.pid&&snapshot.window.0 as isize==candidate.lease.identity.window
        &&visibility_guard::matches_identity(&candidate.lease.identity),"来源窗口身份已改变");
    ensure!(unsafe{IsWindowEnabled(snapshot.window)}.as_bool()&&!visibility_guard::shift_down(),"来源窗口已中断接管");
    let stamp=visibility_guard::now();
    // Explorer can initialize its default location before applying the
    // requested path. While masked, treat that navigation as preparation.
    if candidate.first.as_ref().is_none_or(|first|!first.same_location(snapshot)){
        candidate.first=Some(snapshot.clone());candidate.ready_at=Some(stamp);candidate.observed=None;
    }
    let ready=*candidate.ready_at.get_or_insert(stamp);
    if candidate.observed.as_ref().is_none_or(|(last,_)|!last.equivalent(snapshot)){
        candidate.observed=Some((snapshot.clone(),stamp));
    }
    if !can_deliver(ready,stamp,!snapshot.selected.is_empty())||candidate.committed{return Ok(());}
    let Some(target)=find_target(profile)else{
        ensure!(owns_integration(profile),"临时接管已结束，系统窗口保持打开");
        launch_target_once(&mut candidate.launched,||start_target(profile,snapshot))?;
        return Ok(());
    };
    let bytes=packet(snapshot)?;
    let remaining=candidate.lease.deadline.saturating_sub(visibility_guard::now());
    ensure!(remaining>0&&guard.active(&candidate.lease),"来源窗口已恢复");
    let budget=remaining.min(25).max(1)as u32;
    if candidate.sent.as_ref().is_none_or(|sent|!sent.equivalent(snapshot)){
        if !send_with_budget(target,if snapshot.this_pc{crate::instance::OPEN_MAGIC}else{crate::reveal::COPYDATA_TAG},&bytes,budget){return Ok(());}
        candidate.sent=Some(snapshot.clone());
        if candidate.lease.foreground||unsafe{GetForegroundWindow()}==snapshot.window{
            // Foreground permission is best effort, never a reason to discard
            // a successfully delivered request and leave two folder windows.
            let _=activate_from_source(profile,snapshot,target,&mut candidate.activation_launched);
        }
    }
    // Silence cannot prove that no future /select will arrive. Retain a longer
    // observation for an empty selection and restart the stability period on
    // every change. A slow-ready view or late change simply exhausts the fixed
    // lease and restores Explorer; it never earns a lease extension.
    let stable=candidate.observed.as_ref().map_or(stamp,|(_,at)|*at);
    if close_due(candidate.lease.deadline,visibility_guard::now(),ready,stable,!snapshot.selected.is_empty()){
        let verified=send_with_budget(target,if snapshot.this_pc{crate::reveal::VERIFY_THIS_PC}else{crate::reveal::VERIFY_TAG},
            if snapshot.this_pc{&[]}else{&bytes},budget);
        if verified&&guard.active(&candidate.lease){
            let latest=read_snapshot(&snapshot.browser,snapshot.window,snapshot.pid)?;
            if !snapshot.equivalent(&latest){candidate.observed=Some((latest,visibility_guard::now()));return Ok(());}
            if guard.active(&candidate.lease){guard.commit(&candidate.lease)?;candidate.committed=true;}
        }
    }
    Ok(())
}

pub struct Bridge{stop:Arc<AtomicBool>,worker:Option<JoinHandle<()>>}
impl Bridge{
    pub fn start(profile:&Path)->Result<Option<Self>>{
        if !same_path(&std::path::absolute(profile)?,&std::path::absolute(crate::storage::config_path())?){return Ok(None);}
        let profile=profile.to_path_buf();let stop=Arc::new(AtomicBool::new(false));let signal=stop.clone();
        let worker=thread::Builder::new().name("zsdesk-explorer-forwarding".into()).stack_size(512*1024).spawn(move||{
            if unsafe{CoInitializeEx(None,COINIT_APARTMENTTHREADED)}.is_err(){return;}let _apartment=Apartment;
            let mut shell:Option<IShellWindows>=None;let mut guard:Option<Connection>=None;
            let mut candidates=HashMap::<u64,Candidate>::new();let mut enabled=false;
            let mut refresh=Instant::now()-Duration::from_secs(3);let mut heartbeat=Instant::now();let mut forwarded=0u64;
            while !signal.load(Ordering::Acquire){
                if refresh.elapsed()>=Duration::from_secs(2){
                    enabled=self::enabled(&profile);
                    refresh=Instant::now();
                    if !enabled{guard.take();shell=None;candidates.clear();write_status(&profile,false,forwarded,None,None);}
                    else if guard.is_none(){
                        match Connection::spawn().and_then(|connection|{connection.enable(true)?;Ok(connection)}){
                            Ok(connection)=>{write_status(&profile,true,forwarded,None,Some(connection.pid()));guard=Some(connection);},
                            Err(error)=>write_status(&profile,false,forwarded,Some(&format!("{error:#}")),None),
                        }
                    }
                }
                // A temporary session must not reopen ZSDesk after its last window closes.
                if enabled&&find_target(&profile).is_none()&&!owns_integration(&profile){
                    enabled=false;guard.take();shell=None;candidates.clear();write_status(&profile,false,forwarded,None,None);
                }
                if enabled{
                    if guard.as_mut().is_some_and(Connection::exited){guard.take();shell=None;candidates.clear();
                        write_status(&profile,false,forwarded,Some("窗口可见性守护已退出，系统窗口保持可见"),None);}
                    if let Some(guard)=&guard{
                        if heartbeat.elapsed()>=Duration::from_millis(250){let _=guard.heartbeat();heartbeat=Instant::now();}
                        for event in guard.events(){match event{
                            Event::Prepared(lease)=>{candidates.insert(lease.id,Candidate::new(lease));},
                            Event::Hidden(lease)=>{if let Some(candidate)=candidates.get_mut(&lease.id){candidate.lease=lease;candidate.hidden=true;}},
                            Event::Closed{lease}=>{candidates.remove(&lease);forwarded+=1;write_status(&profile,true,forwarded,None,Some(guard.pid()));},
                            Event::Restored{lease,reason}=>{candidates.remove(&lease);write_status(&profile,true,forwarded,Some(&format!("系统窗口已恢复：{reason}")),Some(guard.pid()));},
                        }}
                        candidates.retain(|_,candidate|if candidate.hidden{guard.active(&candidate.lease)}else{visibility_guard::now().saturating_sub(candidate.lease.started)<visibility_guard::PREPARE_MS});
                        // There is no idle Shell-window enumeration. WinEvent
                        // notifications create bounded work only for new HWNDs.
                        if !candidates.is_empty(){
                            if shell.is_none(){shell=unsafe{CoCreateInstance::<_,IShellWindows>(&ShellWindows,None,CLSCTX_LOCAL_SERVER)}.ok();}
                            let requested=candidates.values().map(|candidate|candidate.lease.identity.window).collect();
                            if let Some(value)=&shell{match snapshots_for(value,Some(&requested)){
                                Ok(batch)=>{
                                    let mut cancel=Vec::new();
                                    for(id,candidate)in &mut candidates{
                                        if candidate.hidden&&!guard.active(&candidate.lease){continue;}
                                        if batch.multiple.contains(&candidate.lease.identity.window){cancel.push((*id,"多标签系统窗口保持独立".to_owned()));continue;}
                                        if let Some(snapshot)=batch.views.iter().find(|view|view.window.0 as isize==candidate.lease.identity.window){
                                            if let Err(error)=update_candidate(&profile,guard,candidate,snapshot){cancel.push((*id,format!("{error:#}")));}
                                        }
                                    }
                                    for(id,error)in cancel{guard.restore(id);candidates.remove(&id);write_status(&profile,true,forwarded,Some(&error),Some(guard.pid()));crate::window::diagnostics::record("explorer-forwarding",&error,None);}
                                },
                                Err(error)=>{for id in candidates.keys(){guard.restore(*id);}candidates.clear();shell=None;
                                    write_status(&profile,true,forwarded,Some(&format!("{error:#}")),Some(guard.pid()));},
                            }}
                        }
                    }
                }
                thread::sleep(Duration::from_millis(if candidates.is_empty(){25}else{5}));
            }
            // Dropping the connection requests restoration before the helper
            // exits; a host crash is independently detected by that helper.
            drop(guard);
        })?;
        Ok(Some(Self{stop,worker:Some(worker)}))
    }
}
impl Drop for Bridge{fn drop(&mut self){self.stop.store(true,Ordering::Release);if self.worker.as_ref().is_some_and(JoinHandle::is_finished){if let Some(worker)=self.worker.take(){let _=worker.join();}}}}
fn write_status(profile:&Path,active:bool,forwarded:u64,error:Option<&str>,guard_pid:Option<u32>){
    if let Ok(bytes)=serde_json::to_vec(&serde_json::json!({"schema":2,"pid":std::process::id(),"active":active,"forwarded":forwarded,
        "last_error":error,"visibility_guard_pid":guard_pid,"maximum_hidden_ms":visibility_guard::LEASE_MS,"selection_settle_ms":SELECTION_SETTLE_MS,
        "empty_observation_ms":EMPTY_OBSERVATION_MS,"selected_observation_ms":SELECTED_OBSERVATION_MS,
        "selection_stable_ms":SELECTION_STABLE_MS,"minimum_close_budget_ms":CLOSE_RESERVE_MS})){
        let _=crate::quick_search::control::atomic_write(&profile.with_file_name("explorer-forwarding-status.json"),&bytes);
    }
}

#[cfg(test)] mod tests{
    use super::*;
    #[test]fn temporary_forwarding_is_opt_in_and_ends_with_the_last_window(){
        assert!(!forwarding_policy(false,true,true,false));
        assert!(forwarding_policy(false,true,false,true));
        assert!(!forwarding_policy(false,false,true,true));
        assert!(forwarding_policy(true,false,true,false));
        assert!(!forwarding_policy(true,true,false,true));
    }
    #[test]fn late_selection_wait_is_bounded_and_does_not_renew_the_visibility_lease(){
        assert!(!can_deliver(100,249,false));assert!(can_deliver(100,250,false));
        assert!(can_deliver(100,100,true));
        assert!(!close_due(900,549,100,100,true));assert!(close_due(900,550,100,100,true));
        assert!(!close_due(900,699,100,100,false));assert!(close_due(900,700,100,100,false));
        assert!(close_due(900,730,100,500,true),"a slow enumeration must not miss a 40 ms closing window");
        assert!(!close_due(900,700,100,600,true),"late selection has not settled");
        assert!(close_due(900,750,100,600,true));
        assert!(!close_due(900,800,100,650,true),"too little time remains for confirmation and close");
        for stamp in [500,650,780,800,900,1000]{
            assert!(!close_due(900,stamp,500,500,false),"slow empty view must restore rather than close");
            assert!(!close_due(900,stamp,500,500,true),"slow selected view must restore rather than close");
        }
        assert!(!close_due(900,900,100,550,true));assert!(!close_due(900,1000,100,550,true));
    }
    #[test]fn cold_target_for_plain_folder_or_selection_is_started_at_most_once()->Result<()>{
        let mut launched=false;let mut starts=0;
        for selected in [false,false,true,true,false]{
            let _=selected;launch_target_once(&mut launched,||{starts+=1;Ok(())})?;
        }
        assert!(launched);assert_eq!(starts,1);
        let mut failed=false;
        assert!(launch_target_once(&mut failed,||anyhow::bail!("fixture launch failure")).is_err());
        assert!(!failed,"failed launch must remain retryable");
        launch_target_once(&mut failed,||Ok(()))?;assert!(failed);Ok(())
    }
    #[test]fn path_comparison_preserves_unicode_and_normalizes_windows_spelling(){assert!(same_path(Path::new(r"E:\资料\Äbc"),Path::new("e:/资料/äBC/")));assert!(!same_path(Path::new(r"E:\a"),Path::new(r"E:\ab")));}
    #[test]
    #[ignore="Explicitly selected owned Explorer window and test profile required"]
    fn native_forwarding_confirms_navigation_before_closing_source()->Result<()>{
        let hwnd:usize=std::env::var("ZSDESK_FORWARD_TEST_HWND")?.parse()?;
        let profile=PathBuf::from(std::env::var_os("ZSDESK_FORWARD_TEST_PROFILE").context("profile")?);
        let executable=PathBuf::from(std::env::var_os("ZSDESK_FORWARD_TEST_EXE").context("exe")?);
        let output=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("output")?);
        unsafe{CoInitializeEx(None,COINIT_APARTMENTTHREADED)}.ok()?;let _apartment=Apartment;
        let shell:IShellWindows=unsafe{CoCreateInstance(&ShellWindows,None,CLSCTX_LOCAL_SERVER)}?;
        let snapshot=windows(&shell)?.into_iter().find(|item|item.window.0 as usize==hwnd).context("test Explorer window")?;
        ensure!(snapshot.folder.starts_with(&output),"test must be inside isolated fixtures");
        let selection=snapshot.selected.clone();forward(&profile,&executable,&snapshot)?;
        let deadline=Instant::now()+Duration::from_secs(3);while unsafe{IsWindow(Some(snapshot.window))}.as_bool(){ensure!(Instant::now()<deadline,"source window remained open");thread::sleep(Duration::from_millis(30));}
        let value=serde_json::json!({"state":"passed","folder":snapshot.folder,"selected":selection,"navigation_confirmed":true,"source_closed":true});
        std::fs::write(output.join("forwarding.current.json"),serde_json::to_vec_pretty(&value)?)?;println!("{value}");Ok(())
    }
}
