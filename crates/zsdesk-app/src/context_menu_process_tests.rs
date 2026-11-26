use super::*;
use std::{ffi::c_void,os::windows::io::AsRawHandle};
use windows::Win32::{System::{IO::DeviceIoControl,Ioctl::FSCTL_SET_SPARSE},Storage::FileSystem::GetCompressedFileSizeW};
#[link(name="user32")]unsafe extern "system"{
    fn CreateDesktopW(name:*const u16,device:*const u16,mode:*const c_void,flags:u32,access:u32,attributes:*const c_void)->*mut c_void;
    fn SetThreadDesktop(desktop:*mut c_void)->i32;
    fn CloseDesktop(desktop:*mut c_void)->i32;
    fn CreateWindowStationW(name:*const u16,flags:u32,access:u32,security:*const c_void)->*mut c_void;
    fn GetProcessWindowStation()->*mut c_void;
    fn SetProcessWindowStation(station:*mut c_void)->i32;
    fn CloseWindowStation(station:*mut c_void)->i32;
    fn GetUserObjectInformationW(object:*mut c_void,index:i32,information:*mut c_void,length:u32,needed:*mut u32)->i32;
}
pub(super) fn current_station_name()->Result<String>{
    let station=unsafe{GetProcessWindowStation()};ensure!(!station.is_null(),"current window station missing");
    let mut name=[0u16;256];let mut needed=0;
    ensure!(unsafe{GetUserObjectInformationW(station,2,name.as_mut_ptr().cast(),std::mem::size_of_val(&name)as u32,&mut needed)}!=0,"window station name unavailable");
    Ok(String::from_utf16_lossy(&name[..name.iter().position(|unit|*unit==0).unwrap_or(name.len())]))
}
pub(super) fn clipboard_metadata()->serde_json::Value{
    use windows::Win32::System::DataExchange::{GetClipboardSequenceNumber,GetClipboardOwner,GetOpenClipboardWindow,IsClipboardFormatAvailable,CountClipboardFormats};
    fn identity(window:Option<HWND>)->serde_json::Value{let mut pid=0;let thread=window.map_or(0,|window|unsafe{GetWindowThreadProcessId(window,Some(&mut pid))});
        serde_json::json!({"window":window.map(|window|window.0 as usize),"pid":pid,"thread":thread})}
    serde_json::json!({"sequence":unsafe{GetClipboardSequenceNumber()},"owner":identity(unsafe{GetClipboardOwner()}.ok()),"open_owner":identity(unsafe{GetOpenClipboardWindow()}.ok()),
        "format_count":unsafe{CountClipboardFormats()},"has_hdrop":unsafe{IsClipboardFormatAvailable(15)}.is_ok(),"has_unicode_text":unsafe{IsClipboardFormatAvailable(13)}.is_ok()})
}
fn clipboard_paths_with_evidence()->Result<(Vec<PathBuf>,serde_json::Value)>{
    use windows::Win32::{System::{Com::{FORMATETC,DVASPECT_CONTENT,TYMED_HGLOBAL,STGMEDIUM},Ole::ReleaseStgMedium,
        DataExchange::{OpenClipboard,CloseClipboard,EnumClipboardFormats,GetClipboardSequenceNumber}},UI::Shell::{HDROP,DragQueryFileW}};
    use std::os::windows::ffi::OsStringExt;
    let before=clipboard_metadata();let(data,sequence)=read_clipboard_retry("read native copy result")?;
    let format=FORMATETC{cfFormat:15,ptd:std::ptr::null_mut(),dwAspect:DVASPECT_CONTENT.0,lindex:-1,tymed:TYMED_HGLOBAL.0 as u32};
    let query=unsafe{data.QueryGetData(&format)};let before_get=clipboard_metadata();let received=unsafe{data.GetData(&format)};
    let mut paths=Vec::new();let mut medium_type=None;let mut reported_count=None;
    let get_hresult=match received{
        Ok(medium)=>{
            struct Medium(STGMEDIUM);impl Drop for Medium{fn drop(&mut self){unsafe{ReleaseStgMedium(&mut self.0);}}}let medium=Medium(medium);medium_type=Some(medium.0.tymed);
            if medium.0.tymed==TYMED_HGLOBAL.0 as u32{
                let hdrop=HDROP(unsafe{medium.0.u.hGlobal.0});let count=unsafe{DragQueryFileW(hdrop,u32::MAX,None)};reported_count=Some(count);
                if count<=10000{for index in 0..count{let length=unsafe{DragQueryFileW(hdrop,index,None)};
                    if length==0||length>=32768{continue;}let mut name=vec![0u16;length as usize+1];
                    let copied=unsafe{DragQueryFileW(hdrop,index,Some(&mut name))};if copied==length{paths.push(PathBuf::from(std::ffi::OsString::from_wide(&name[..length as usize])));}
                }}
            }
            "0x00000000".to_owned()
        }
        Err(error)=>format!("0x{:08X}",error.code().0 as u32),
    };
    let after_get=clipboard_metadata();drop(data);
    let mut formats=Vec::new();let mut format_error=None;
    match unsafe{OpenClipboard(None)}{
        Ok(())=>{let mut current=0;for _ in 0..128{current=unsafe{EnumClipboardFormats(current)};if current==0{break;}formats.push(current);}let _=unsafe{CloseClipboard()};}
        Err(error)=>format_error=Some(format!("0x{:08X}",error.code().0 as u32)),
    }
    let evidence=serde_json::json!({"before_read":before,"acquired_sequence":sequence,"before_get_data":before_get,"after_get_data":after_get,
        "query_get_data_hresult":format!("0x{:08X}",query.0 as u32),
        "get_data_hresult":get_hresult,"medium_type":medium_type,"hdrop_reported_count":reported_count,"formats":formats,"format_enumeration_error":format_error,
        "final_sequence":unsafe{GetClipboardSequenceNumber()}});
    Ok((paths,evidence))
}
unsafe extern "system" fn owner_proc(hwnd:HWND,msg:u32,wp:WPARAM,lp:LPARAM)->LRESULT{
    unsafe{DefWindowProcW(hwnd,msg,wp,lp)}
}
struct Owner(HWND);impl Drop for Owner{fn drop(&mut self){let _=unsafe{DestroyWindow(self.0)};}}
struct PoolCleanup;
impl Drop for PoolCleanup{fn drop(&mut self){
    shutdown_idle_worker();let deadline=Instant::now()+Duration::from_secs(5);
    while pending_workers()>0&&Instant::now()<deadline{pump(10);}
}}
pub(crate) struct Environment{key:&'static str,previous:Option<std::ffi::OsString>}
impl Environment{pub(crate) fn set(key:&'static str,value:impl AsRef<std::ffi::OsStr>)->Self{let previous=std::env::var_os(key);std::env::set_var(key,value);Self{key,previous}}}
impl Drop for Environment{fn drop(&mut self){if let Some(previous)=&self.previous{std::env::set_var(self.key,previous);}else{std::env::remove_var(self.key);}}}
pub(crate) struct Fixtures(pub(crate) Vec<PathBuf>);impl Drop for Fixtures{fn drop(&mut self){for path in &self.0{let _=fs::remove_file(path);}}}
pub(super) fn new_item_command(menu:&ShellMenu,text:bool)->Result<u32>{
    let popup=menu.submenu_with_verb("NewFolder").context("ShellNew folder command missing")?;
    let count=unsafe{GetMenuItemCount(Some(popup))};let mut rows=Vec::new();
    for index in 0..count {
        let id=unsafe{GetMenuItemID(popup,index)};let verb=menu.verb(id).unwrap_or_default();
        let mut label=[0u16;512];let length=unsafe{GetMenuStringW(popup,index as u32,Some(&mut label),MF_BYPOSITION)};
        let label=String::from_utf16_lossy(&label[..length.max(0)as usize]);
        if (!text&&verb.eq_ignore_ascii_case("NewFolder"))||(text&&verb.to_ascii_lowercase().contains(".txt")){return Ok(id);}
        rows.push(serde_json::json!({"id":id,"verb":verb,"label":label}));
    }
    anyhow::bail!("ShellNew command missing (text={text}): {rows:?}")
}
pub(super) fn static_background_command(menu:&ShellMenu)->Result<u32>{
    let verb=std::env::var("ZSDESK_MENU_STATIC_VERB").context("static verb identity")?;
    ensure!(verb.starts_with("ZSDeskCompatProbe")&&verb.bytes().all(|b|b.is_ascii_alphanumeric()),"invalid fixture verb");
    for index in 0..unsafe{GetMenuItemCount(Some(menu.hmenu()))}.max(0){
        let command=unsafe{GetMenuItemID(menu.hmenu(),index)};
        if menu.verb(command).is_some_and(|value|value.eq_ignore_ascii_case(&verb)){return Ok(command);}
    }
    anyhow::bail!("registered static background verb is missing")
}
pub(crate) fn sparse(root:&Path)->Result<(Fixtures,Vec<Vec<u8>>,Vec<serde_json::Value>)>{
    let mut fixtures=Fixtures(Vec::new());let mut pidls=Vec::new();let mut rows=Vec::new();
    for (name,gib) in [("ZSDesk-menu-大文件.bin",12u64),("ZSDesk-menu-归档.zip",6),("ZSDesk-menu-视频.mp4",6)]{
        let path=root.join(name);let file=fs::OpenOptions::new().write(true).create_new(true).open(&path)?;fixtures.0.push(path.clone());
        let mut returned=0;unsafe{DeviceIoControl(HANDLE(file.as_raw_handle()),FSCTL_SET_SPARSE,None,0,None,0,Some(&mut returned),None)}?;
        let logical=gib*1024*1024*1024;file.set_len(logical)?;drop(file);
        let mut high=0;let low=unsafe{GetCompressedFileSizeW(PCWSTR(wide(&path.to_string_lossy()).as_ptr()),Some(&mut high))};let physical=(u64::from(high)<<32)|u64::from(low);
        ensure!(physical<1024*1024,"fixture unexpectedly allocated data blocks");
        pidls.push(zsdesk_windows::shell::pidl::location_pidl(&zsdesk_core::model::explorer::ShellLocation::Path(path.clone())).context("fixture PIDL")?);
        rows.push(serde_json::json!({"name":name,"logical_bytes":logical,"allocated_bytes":physical}));
    }Ok((fixtures,pidls,rows))
}
fn complete(client:&mut Client,owner:HWND)->Result<(std::result::Result<MenuOutcome,String>,(u64,u64))>{
    complete_with_guard(client,owner,&mut |_|true)
}
fn complete_with_guard(client:&mut Client,owner:HWND,before:&mut dyn FnMut(bool)->bool)->Result<(std::result::Result<MenuOutcome,String>,(u64,u64))>{
    let deadline=Instant::now()+Duration::from_secs(35);
    loop{
        ensure!(unsafe{IsWindow(Some(owner))}.as_bool(),"parent window was lost");
        if let Some(result)=client.poll(before){return Ok((result.map_err(|error|format!("{error:#}")),client.timings().unwrap_or_default()));}
        ensure!(Instant::now()<deadline,"worker test exceeded protocol timeout");pump(0);client.wait_for_progress(50);
    }
}

fn read_clipboard_retry(stage:&str)->Result<(windows::Win32::System::Com::IDataObject,u32)>{
    use windows::Win32::System::{DataExchange::GetClipboardSequenceNumber,Ole::OleGetClipboard};
    let deadline=Instant::now()+Duration::from_secs(1);
    loop{
        let before=unsafe{GetClipboardSequenceNumber()};
        match unsafe{OleGetClipboard()}{
            Ok(data) if before==unsafe{GetClipboardSequenceNumber()}=>return Ok((data,before)),
            Ok(_)=>ensure!(Instant::now()<deadline,"{stage}: clipboard changed while capturing a stable snapshot"),
            Err(error)=>{
                if error.code().0 as u32!=0x800401d0||Instant::now()>=deadline{return Err(error).with_context(||format!("{stage}: clipboard acquisition failed after bounded retry"));}
            }
        }
        pump(20);
    }
}

// Open no popup and authorize no command. Exercise the actual compact provider
// and its original Shell sharing cascade after handing off the prepared worker.
#[test]
#[ignore="Read-only shell menu query on a Windows installation"]
fn prepared_worker_queries_file_commands_without_display() -> Result<()> {
    let root=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("test output")?);
    let folder=root.join("files");
    fs::create_dir_all(&folder)?;
    let file=folder.join("menu.txt");
    fs::write(&file,b"menu test")?;
    let _fixture=Fixtures(vec![file.clone()]);
    let _root=Environment::set("ZSDESK_MENU_ROOT",root.join("workers"));
    let _pool=PoolCleanup;
    unsafe { OleInitialize(None)?; }
    struct Apartment;
    impl Drop for Apartment { fn drop(&mut self){unsafe{OleUninitialize()}} }
    let _apartment=Apartment;
    let owner=Owner(unsafe{CreateWindowExW(WINDOW_EX_STYLE::default(),w!("STATIC"),w!(""),WINDOW_STYLE::default(),0,0,0,0,Some(HWND_MESSAGE),None,None,None)}?);
    let pidl=|path:PathBuf| zsdesk_windows::shell::pidl::location_pidl(&zsdesk_core::model::explorer::ShellLocation::Path(path)).context("item identity");
    let parent=pidl(folder.clone())?;
    let view=MenuViewContext{folder:Some(parent.clone()),..Default::default()};
    let mut options=ContextMenuOptions{drawn:true,..Default::default()};
    if let Ok(app)=std::env::var("ZSDESK_MENU_TEST_FIRST_PAGE_APP"){ options.native_first_page_apps=Some(vec![app]); }
    let query=||->Result<(u32,bool,u64)> {
        let _probe=Environment::set("ZSDESK_MENU_TEST_PROBE","query-compact-catalog");
        let mut client=Client::spawn_with_context(owner.0,POINT::default(),vec![pidl(file.clone())?],Some(folder.clone()),options.clone(),MenuInterceptions::default(),Mode::More,false,None,view.clone())?;
        let (outcome,(bind,_))=complete_with_guard(&mut client,owner.0,&mut |invoke| !invoke)?;
        ensure!(matches!(outcome,Ok(MenuOutcome::Dismissed)),"catalog query failed: {outcome:?}");
        let status=client.last_status.as_ref().context("worker status")?;
        ensure!(!status.catalog.is_empty()&&!client.committed&&client.timing.menu_open_observation.is_none(),"expected a read-only query");
        if let Some(sharing)=status.catalog.iter().find(|row|row.label==zsdesk_core::i18n::text("共享对象")){
            ensure!(!sharing.children.is_empty(),"sharing submenu lost its Shell commands");
        }
        fs::write(root.join("compact-catalog.json"),serde_json::to_vec_pretty(&status.catalog)?)?;
        Ok((client.pid().context("worker PID")?,client.timing.reused_process,bind))
    };
    let (_,cold_reused,cold_bind)=query()?;
    ensure!(!cold_reused,"unexpected worker before prewarm");
    prewarm::start(owner.0,parent,Some(folder.clone()),true,options.clone(),Some(view.clone()));
    let deadline=Instant::now()+Duration::from_secs(6);
    let prepared=loop {
        if let Some(pid)=idle_worker_diagnostics()[0]["pid"].as_u64(){break pid as u32;}
        ensure!(Instant::now()<deadline,"worker failed to prepare");pump(10);
    };
    let (pid,reused,warm_bind)=query()?;
    ensure!(reused&&pid==prepared,"prepared PID was discarded or replaced");
    ensure!(idle_worker_diagnostics().as_array().is_some_and(Vec::is_empty),"drawn menu remained parked after use");
    eprintln!("prepared_pid={pid} cold_bind_us={cold_bind} prepared_bind_us={warm_bind}");
    Ok(())
}
fn exercise(root:&Path,desktop_name:&str)->Result<serde_json::Value>{
    unsafe{OleInitialize(None)?;}
    struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}let _apartment=Apartment;
    let _pool=PoolCleanup;
    let _desktop=Environment::set("ZSDESK_MENU_TEST_DESKTOP",desktop_name);let _root=Environment::set("ZSDESK_MENU_ROOT",root.join("workers"));
    let (files,pidls,fixtures)=sparse(root)?;
    let module=HINSTANCE(unsafe{GetModuleHandleW(None)?}.0);let class=w!("ZSDesk.MenuIsolation.Parent");
    unsafe{RegisterClassW(&WNDCLASSW{hInstance:module,lpfnWndProc:Some(owner_proc),lpszClassName:class,..Default::default()});}
    let owner=Owner(unsafe{CreateWindowExW(WS_EX_NOACTIVATE,class,w!("Menu isolation test"),WS_OVERLAPPEDWINDOW,20,20,640,480,None,None,Some(module),None)}?);
    unsafe{let _=ShowWindow(owner.0,SW_SHOWNOACTIVATE);}
    let options=ContextMenuOptions{modern:false,..Default::default()};let point=POINT{x:70,y:100};
    let mut workers=Vec::new();
    let mut reusable_pid=None;
    for probe in ["crash","memory-limit","dismiss","menu-dismiss"]{
        let _probe=Environment::set("ZSDESK_MENU_TEST_PROBE",probe);let mut client=Client::spawn(owner.0,point,pidls.clone(),None,options.clone(),MenuInterceptions::default(),Mode::More)?;
        let child=client.pid().context("worker pid")?;ensure!(child!=std::process::id(),"extension ran in parent");
        let mut limits=JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        unsafe{QueryInformationJobObject(Some(HANDLE(client.job.context("worker job")? as*mut _)),JobObjectExtendedLimitInformation,(&mut limits as*mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),std::mem::size_of_val(&limits)as u32,None)}?;
        ensure!(limits.ProcessMemoryLimit==WORKER_MEMORY_LIMIT&&limits.BasicLimitInformation.LimitFlags.contains(JOB_OBJECT_LIMIT_PROCESS_MEMORY|JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK|JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION)
            &&!limits.BasicLimitInformation.LimitFlags.contains(JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE),"worker resource/child lifetime policy changed");
        let(result,timing)=complete(&mut client,owner.0)?;
        let diagnostic=read_recent_diagnostics()["current"].clone();
        if probe=="crash"{
            ensure!(result.as_ref().is_err_and(|error|error.to_ascii_lowercase().contains("c0000005")),"unhandled worker access violation was not detected: {result:?}");
            ensure!(diagnostic["worker_exit_hex"]=="0xC0000005"&&diagnostic["fault"]["exception_code"]=="0xC0000005","crash code/module evidence was not retained");
        }else{ensure!(matches!(result,Ok(MenuOutcome::Dismissed)),"{probe} failed: {result:?}");}
        let stages=client.detailed_timings();
        if probe=="dismiss"{reusable_pid=Some(child);}
        if probe=="menu-dismiss"{
            ensure!(reusable_pid==Some(child)&&stages.reused_process&&stages.process_spawn_microseconds==0&&stages.com_microseconds==0,"warm request rebuilt its worker/COM apartment");
            ensure!(stages.parent_to_menu_microseconds.is_some()&&stages.menu_open_observation.as_deref()==Some("visible-classic-menu"),"production menu-open timing was not observed");
            ensure!(diagnostic["native_menu"].is_object(),"native menu timing was omitted from retained diagnostics");
        }
        workers.push(serde_json::json!({"probe":probe,"pid":child,"outcome":result,"bind_microseconds":timing.0,"populate_microseconds":timing.1,"stages":stages,"diagnostic":diagnostic,"parent_alive":true,"memory_limit_bytes":limits.ProcessMemoryLimit,"child_process_breakaway":true,"kill_on_job_close":false}));drop(client);
    }
    // A cached status/decision from an earlier request cannot grant a later
    // selection. Exercise session rotation without a third-party popup dwell.
    let mut rotations=Vec::new();let mut previous=None;let mut rotated=false;
    {
        let _probe=Environment::set("ZSDESK_MENU_TEST_PROBE","dismiss");
        for index in 0..=MAX_SESSION_REQUESTS{
            let mut client=Client::spawn(owner.0,point,vec![pidls[(index as usize)%pidls.len()].clone()],None,options.clone(),MenuInterceptions::default(),Mode::More)?;
            let identity=(client.pid().unwrap(),client.ordinal);
            let mut validations=0;
            let(result,_)=complete_with_guard(&mut client,owner.0,&mut|before|{ensure_not_invoking(before);validations+=1;index!=0})?;
            ensure!(matches!(result,Ok(MenuOutcome::Dismissed))&&validations==1,"stale request authorization was reused");
            if let Some((pid,ordinal))=previous{if ordinal==MAX_SESSION_REQUESTS{ensure!(pid!=identity.0&&identity.1==1,"request limit did not rotate process");rotated=true;}
                else{ensure!(pid==identity.0&&identity.1==ordinal+1,"idle worker unexpectedly lost its session");}}
            previous=Some(identity);rotations.push(serde_json::json!({"pid":identity.0,"ordinal":identity.1}));drop(client);
        }
    }
    ensure!(rotated,"request-count bound was not exercised");
    ensure!(IDLE_TIMEOUT==Duration::from_secs(30)&&MAX_IDLE_WORKERS==1&&MAX_SESSION_REQUESTS==32,"classic worker bounds changed unexpectedly");
    let expired_pid=previous.context("final warm worker")?.0;
    {
        let mut pool=IDLE.lock().unwrap_or_else(|error|error.into_inner());
        let idle=pool.iter_mut().find(|idle|idle.child.as_ref().is_some_and(|child|child.id()==expired_pid)).context("warm worker missing before expiry test")?;
        idle.last_used=Instant::now()-IDLE_TIMEOUT-Duration::from_secs(1);
    }
    retire_expired_idle();
    ensure!(!IDLE.lock().unwrap_or_else(|error|error.into_inner()).iter().any(|idle|idle.child.as_ref().is_some_and(|child|child.id()==expired_pid)),"expired worker stayed cached");
    {
        let _probe=Environment::set("ZSDESK_MENU_TEST_PROBE","dismiss");
        let mut client=Client::spawn(owner.0,point,vec![pidls[0].clone()],None,options.clone(),MenuInterceptions::default(),Mode::More)?;
        ensure!(client.pid()!=Some(expired_pid)&&!client.detailed_timings().reused_process,"expired process was reused");
        let(result,_)=complete(&mut client,owner.0)?;ensure!(matches!(result,Ok(MenuOutcome::Dismissed)),"replacement worker failed: {result:?}");
    }
    shutdown_idle_worker();
    let end=Instant::now()+Duration::from_secs(5);while pending_workers()>0&&Instant::now()<end{pump(10);}ensure!(pending_workers()==0,"menu worker did not exit");
    for (path,record) in files.0.iter().zip(&fixtures){ensure!(fs::metadata(path)?.len()==record["logical_bytes"].as_u64().unwrap(),"fixture content length changed");}
    drop(owner);drop(files);
    Ok(serde_json::json!({"fixtures":fixtures,"logical_total_bytes":24u64*1024*1024*1024,"workers":workers,"reuse_rotation":rotations,"idle_expiry_verified":true,"idle_limit_seconds":IDLE_TIMEOUT.as_secs(),"all_workers_exited":true,"fixtures_removed":true,"fault_injection":"unhandled noncontinuable SEH access violation 0xc0000005 before extension invocation"}))
}
fn ensure_not_invoking(before:bool){assert!(!before,"dismiss-only probe attempted an invocation");}

pub(super) fn background_details_command(menu:&ShellMenu)->Result<u32>{
    // Populate native view submenus, then invoke their actual numeric command.
    // DefView does not expose a canonical string verb for every view mode.
    let _=menu.submenu_with_verb("viewdetails");
    let count=unsafe{GetMenuItemCount(Some(menu.hmenu()))};
    for index in 0..count.max(0){
        let sub=unsafe{GetSubMenu(menu.hmenu(),index)};if sub.is_invalid(){continue;}
        for item in 0..unsafe{GetMenuItemCount(Some(sub))}.max(0){
            let mut text=[0u16;128];let length=unsafe{GetMenuStringW(sub,item as u32,Some(&mut text),MF_BYPOSITION)}.max(0)as usize;
            let label=String::from_utf16_lossy(&text[..length]).replace('&',"").to_lowercase();
            if label.starts_with("详细信息")||label=="details"{
                let id=unsafe{GetMenuItemID(sub,item)};ensure!(ShellMenu::is_shell_id(id),"invalid Details command");return Ok(id);
            }
        }
    }
    anyhow::bail!("native Details menu command unavailable")
}
#[test]
#[ignore="Private desktop, sparse 24 GiB selection and extension worker process isolation; requires ZSDESK_NATIVE_TEST_OUTPUT"]
fn native_large_selection_immediate_menu_and_extension_isolation()->Result<()>{
    let root=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("output")?).join("menu-isolation");ensure!(root.is_absolute(),"absolute output required");fs::create_dir_all(&root)?;
    let before=unsafe{GetForegroundWindow()};let name=format!("ZSDesk.MenuIsolation.{}",std::process::id());
    let desktop=unsafe{CreateDesktopW(wide(&name).as_ptr(),std::ptr::null(),std::ptr::null(),0,0x000f00ff,std::ptr::null())};ensure!(!desktop.is_null(),"private desktop failed");let raw=desktop as usize;let target=root.clone();
    let result=thread::spawn(move||->Result<serde_json::Value>{ensure!(unsafe{SetThreadDesktop(raw as*mut c_void)}!=0,"SetThreadDesktop failed");exercise(&target,&name)}).join().map_err(|_|anyhow::anyhow!("isolation test panicked")).and_then(|value|value);
    let closed=unsafe{CloseDesktop(desktop)}!=0;let foreground=unsafe{GetForegroundWindow()};let mut pid=0;unsafe{GetWindowThreadProcessId(foreground,Some(&mut pid));}
    let isolated=pid!=std::process::id();let report=serde_json::json!({"state":if result.is_ok()&&closed&&isolated{"passed"}else{"failed"},"checks":result.as_ref().map_err(|error|format!("{error:#}")),"desktop_closed":closed,"foreground_unchanged":before==foreground,"test_did_not_activate":isolated});
    fs::write(root.join("verification.current.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");result?;ensure!(closed&&isolated,"desktop cleanup/focus changed");Ok(())
}

fn exercise_virtual_archive(root:&Path,owner:HWND)->Result<serde_json::Value>{
    use zsdesk_core::model::explorer::ShellLocation;
    use zsdesk_windows::shell::{pidl,file_transfer};
    let station=current_station_name()?;ensure!(!station.eq_ignore_ascii_case("WinSta0"),"archive transfer requires private clipboard");
    let directory=root.join(format!("virtual-archive-{}",std::process::id()));ensure!(!directory.exists(),"archive fixture already exists");fs::create_dir(&directory)?;
    fs::write(directory.join("owner"),b"zsdesk-virtual-archive-test")?;
    struct Fixture(PathBuf);impl Drop for Fixture{fn drop(&mut self){
        if self.0.is_absolute()&&fs::symlink_metadata(&self.0).is_ok_and(|m|!m.file_type().is_symlink())
            &&fs::read(self.0.join("owner")).is_ok_and(|bytes|bytes==b"zsdesk-virtual-archive-test"){let _=fs::remove_dir_all(&self.0);}
    }}let _fixture=Fixture(directory.clone());
    let archive=directory.join("中文 空格.zip");let bytes=include_bytes!("../tests/data/virtual-files.zip");fs::write(&archive,bytes)?;
    let destination=directory.join("extracted");fs::create_dir(&destination)?;
    let mut pidls=Vec::new();
    for name in ["inside.txt","目录"]{
        let item=pidl::location_pidl(&ShellLocation::Path(archive.join(name))).with_context(||format!("archive child {name}"))?;
        ensure!(file_transfer::paths_from_pidls(&[item.clone()]).is_err(),"virtual archive item was converted to a disk path");pidls.push(item);
    }
    let _probe=Environment::set("ZSDESK_MENU_TEST_PROBE","modern-owner-copy");
    let mut copy=Client::spawn(owner,POINT{x:70,y:100},pidls,None,ContextMenuOptions{modern:true,..Default::default()},MenuInterceptions::default(),Mode::More)?;
    let copy_pid=copy.pid().context("archive copy worker")?;let (outcome,_)=complete(&mut copy,owner)?;
    ensure!(matches!(outcome,Ok(MenuOutcome::ShellInvoked|MenuOutcome::NativeHandled)),"archive copy command failed: {outcome:?}");
    ensure!(copy.last_status.as_ref().is_some_and(|status|status.probe_menu_open&&status.probe_command_messages==1),"archive copy did not use its supported Shell menu exactly once");
    let copy_presentation=copy.detailed_timings().menu_open_observation;
    ensure!(matches!(copy_presentation.as_deref(),Some("native-presenter-open"|"visible-classic-menu"|"classic-menu-entered")),"archive menu presentation was not observed");
    ensure!(file_transfer::has_paste_content(),"archive copy did not publish paste content");
    let filesystem_clipboard=file_transfer::has_filesystem_clipboard();let copy_reused=copy.last_status.as_ref().is_some_and(|status|status.reusable);drop(copy);
    let destination_id=pidl::location_pidl(&ShellLocation::Path(destination.clone())).context("archive paste destination")?;
    let mut paste=Client::spawn(owner,POINT::default(),vec![destination_id],Some(destination.clone()),ContextMenuOptions{modern:true,..Default::default()},MenuInterceptions::default(),Mode::Paste)?;
    let (outcome,_)=complete(&mut paste,owner)?;
    ensure!(matches!(outcome,Ok(MenuOutcome::ShellInvoked|MenuOutcome::NativeHandled)),"archive paste command failed: {outcome:?}");drop(paste);
    let deadline=Instant::now()+Duration::from_secs(15);
    loop{
        if fs::read(destination.join("inside.txt")).is_ok_and(|data|data==b"ZSDesk virtual file payload\r\n")
            &&fs::read(destination.join("目录/子文件.txt")).is_ok_and(|data|data=="ZSDesk 中文虚拟文件\r\n".as_bytes()){break;}
        ensure!(Instant::now()<deadline,"archive paste did not extract the exact selected contents");pump(20);
    }
    ensure!(fs::read(&archive)?==bytes,"copy modified the source archive");
    file_transfer::set_text(owner,"ZSDesk archive private clipboard released")?;shutdown_idle_worker();
    let deadline=Instant::now()+Duration::from_secs(5);while pending_workers()>0&&Instant::now()<deadline{pump(10);}
    ensure!(pending_workers()==0,"archive provider worker did not retire after clipboard release");
    Ok(serde_json::json!({"native_archive_selection_copy":true,"native_paste_extracted_exact_contents":true,"unicode_nested_folder":true,
        "virtual_pidls_not_disk_paths":true,"copy_presentation":copy_presentation,"filesystem_clipboard":filesystem_clipboard,"copy_worker_reusable":copy_reused,"copy_worker_pid":copy_pid,
        "source_archive_unchanged":true,"provider_workers_retired":true,"user_clipboard_accessed":false}))
}

fn exercise_modern_owner(root:&Path,desktop_name:&str)->Result<serde_json::Value>{
    use windows::Win32::System::DataExchange::GetClipboardSequenceNumber;
    let expected_station=std::env::var("ZSDESK_MENU_TEST_STATION").context("isolated station required before touching clipboard")?;
    ensure!(current_station_name()?==expected_station&&!expected_station.eq_ignore_ascii_case("WinSta0"),"modern clipboard regression requires a separate window station");
    unsafe{OleInitialize(None)?;}
    struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}let _apartment=Apartment;
    let _pool=PoolCleanup;
    let _desktop=Environment::set("ZSDESK_MENU_TEST_DESKTOP",desktop_name);
    let _root=Environment::set("ZSDESK_MENU_ROOT",root.join("workers"));
    let _probe=Environment::set("ZSDESK_MENU_TEST_PROBE","modern-owner-copy");
    let mut fixtures=Fixtures(Vec::new());let mut pidls=Vec::new();
    for name in ["原生菜单-甲.txt","原生菜单 乙.txt"]{
        let path=root.join(name);fs::write(&path,b"ZSDesk native owner command fixture")?;fixtures.0.push(path.clone());
        pidls.push(zsdesk_windows::shell::pidl::location_pidl(&zsdesk_core::model::explorer::ShellLocation::Path(path)).context("fixture PIDL")?);
    }
    let module=HINSTANCE(unsafe{GetModuleHandleW(None)?}.0);let class=w!("ZSDesk.ModernMenu.Parent");
    unsafe{RegisterClassW(&WNDCLASSW{hInstance:module,lpfnWndProc:Some(owner_proc),lpszClassName:class,..Default::default()});}
    let owner=Owner(unsafe{CreateWindowExW(WS_EX_NOACTIVATE,class,w!("Native menu owner regression"),WS_OVERLAPPEDWINDOW,20,20,640,480,None,None,Some(module),None)}?);
    unsafe{let _=ShowWindow(owner.0,SW_SHOWNOACTIVATE);}
    struct PrivateClipboardReset{owner:HWND,station:String}
    impl Drop for PrivateClipboardReset{fn drop(&mut self){
        if current_station_name().is_ok_and(|name|name==self.station){
            if let Err(error)=zsdesk_windows::shell::file_transfer::set_text(self.owner,"ZSDesk private clipboard released"){eprintln!("private clipboard cleanup: {error}");}
        }
    }}
    let _private_clipboard=PrivateClipboardReset{owner:owner.0,station:expected_station.clone()};
    // This station owns a separate clipboard. No user IDataObject is captured
    // or restored, even when the native command fails or the test times out.
    zsdesk_windows::shell::file_transfer::set_text(owner.0,"ZSDesk isolated native-menu clipboard")?;
    let initial_sequence=unsafe{GetClipboardSequenceNumber()};let initial_clipboard=clipboard_metadata();
    let warm_pid={
        let _warm=Environment::set("ZSDESK_MENU_TEST_PROBE","menu-dismiss");
        let mut warm=Client::spawn(owner.0,POINT{x:70,y:100},vec![pidls[0].clone()],None,ContextMenuOptions{modern:false,..Default::default()},MenuInterceptions::default(),Mode::More)?;
        let pid=warm.pid().context("warmup worker")?;let(result,_)=complete(&mut warm,owner.0)?;
        ensure!(matches!(result,Ok(MenuOutcome::Dismissed)),"warmup menu did not dismiss");drop(warm);pid
    };
    let mut client=Client::spawn(owner.0,POINT{x:70,y:100},pidls,None,ContextMenuOptions{modern:true,..Default::default()},MenuInterceptions::default(),Mode::More)?;
    let child=client.pid().context("worker PID")?;
    ensure!(child==warm_pid&&client.detailed_timings().reused_process,"new selection did not reuse idle process");
    let mut clipboard_changed_before_invoke=false;
    let mut before_invoke_clipboard=serde_json::Value::Null;
    let(result,timing)=match complete_with_guard(&mut client,owner.0,&mut |before_invoke|{
        if before_invoke{before_invoke_clipboard=clipboard_metadata();if unsafe{GetClipboardSequenceNumber()}!=initial_sequence{clipboard_changed_before_invoke=true;return false;}}true
    }){
        Ok(value)=>value,
        Err(error)=>{
            let stderr=worker_stderr(&client.workspace);
            // This test worker can only copy these fixtures. End that exact
            // child on a failed watchdog; production committed operations keep
            // their original lifetime policy.
            if let Some(child)=client.child.as_mut(){let _=child.kill();let _=child.wait();}
            client.complete=true;
            return Err(error.context(stderr));
        }
    };
    let status=read_small::<Status>(&client.workspace.join("status.json"),65536)?;
    ensure!(!clipboard_changed_before_invoke,"native copy validation: private clipboard changed before invocation");
    let(paths,clipboard_read)=clipboard_paths_with_evidence()?;
    let exact=paths.len()==fixtures.0.len()&&fixtures.0.iter().all(|expected|paths.contains(expected));
    let mut evidence=serde_json::json!({"worker_pid":child,"outcome":result,"native_menu_observed_open":status.probe_menu_open,
        "native_owner_command_messages":status.probe_command_messages,"native_menu":status.native_menu,
        "shell_copy_paths_verified":exact,"copied_count":paths.len(),"expected_count":fixtures.0.len(),
        "private_window_station":expected_station,"user_clipboard_accessed":false,"initial_clipboard":initial_clipboard,"before_invoke_clipboard":before_invoke_clipboard,
        "worker_clipboard_after_invoke":status.probe_clipboard_after_invoke,"clipboard_read":clipboard_read,"worker_phase":status.phase,"worker_reusable":status.reusable,
        "bind_microseconds":timing.0,"populate_microseconds":timing.1,"worker_stderr":worker_stderr(&client.workspace),"physical_input_generated":false});
    fs::write(root.join("command.current.json"),serde_json::to_vec_pretty(&evidence)?)?;
    ensure!(status.probe_menu_open,"the Windows 11 presenter never reported an open menu: {evidence}");
    ensure!(status.probe_command_messages==1,"native command was not dispatched exactly once on the worker owner: {evidence}");
    ensure!(matches!(result,Ok(MenuOutcome::ShellInvoked|MenuOutcome::NativeHandled)),"native copy did not finish: {evidence}");
    ensure!(exact,"native copy clipboard result mismatch: {evidence}");
    for path in &fixtures.0{ensure!(fs::read(path)?==b"ZSDesk native owner command fixture","fixture changed");}
    zsdesk_windows::shell::file_transfer::set_text(owner.0,"ZSDesk private clipboard after copy")?;drop(client);shutdown_idle_worker();
    let deadline=Instant::now()+Duration::from_secs(5);while pending_workers()>0&&Instant::now()<deadline{pump(10);}
    ensure!(pending_workers()==0,"native menu worker did not retire");
    {
        // The exact native copy command now returns to the application's file
        // action queue. It must not publish clipboard data before that handoff.
        let _probe=Environment::set("ZSDESK_MENU_TEST_PROBE","modern-owner-copy");
        let initial_sequence=unsafe{GetClipboardSequenceNumber()};
        let pidl=zsdesk_windows::shell::pidl::location_pidl(&zsdesk_core::model::explorer::ShellLocation::Path(fixtures.0[0].clone())).context("intercept fixture PIDL")?;
        let mut client=Client::spawn(owner.0,POINT{x:70,y:100},vec![pidl],None,ContextMenuOptions{modern:true,..Default::default()},
            MenuInterceptions{file_actions:true,..Default::default()},Mode::More)?;
        let pid=client.pid().context("intercept worker PID")?;let mut invocation_barriers=0;
        let (outcome,_)=complete_with_guard(&mut client,owner.0,&mut|before|{if before{invocation_barriers+=1;false}else{true}})?;
        ensure!(matches!(outcome,Ok(MenuOutcome::Copy))&&invocation_barriers==0&&!client.committed,"native file action was not intercepted before invoke: {outcome:?}; status={}",serde_json::to_string(&client.last_status)?);
        let status=client.last_status.as_ref().context("intercept status")?;
        ensure!(status.probe_menu_open&&status.probe_command_messages==1&&!status.before_invoke,"interception bypassed native menu dispatch");
        let owns=unsafe{GetClipboardOwner()}.ok().is_some_and(|window|{let mut owner_pid=0;unsafe{GetWindowThreadProcessId(window,Some(&mut owner_pid));}owner_pid==pid});
        ensure!(!owns&&unsafe{GetClipboardSequenceNumber()}==initial_sequence,"intercepted copy already changed the clipboard");
        evidence["filesystem_copy_intercepted_before_invoke"]=serde_json::json!(true);
        drop(client);shutdown_idle_worker();
        let deadline=Instant::now()+Duration::from_secs(5);while pending_workers()>0&&Instant::now()<deadline{pump(10);}
        ensure!(pending_workers()==0,"intercepted menu worker did not retire");
    }
    for background in [false,true]{
        let _probe=Environment::set("ZSDESK_MENU_TEST_PROBE","modern-expand");
        let path=if background{root.to_path_buf()}else{fixtures.0[0].clone()};
        let target=zsdesk_windows::shell::pidl::location_pidl(&zsdesk_core::model::explorer::ShellLocation::Path(path.clone())).context("expanded target")?;
        let mut client=Client::spawn(owner.0,POINT{x:70,y:100},vec![target],background.then_some(path),ContextMenuOptions::default(),MenuInterceptions::default(),if background{Mode::Background}else{Mode::More})?;
        let(outcome,_)=complete(&mut client,owner.0)?;
        let status=client.last_status.as_ref().context("expanded status")?;
        ensure!(matches!(outcome,Ok(MenuOutcome::Dismissed))&&status.probe_menu_open&&status.probe_classic_loop,
            "Show more options did not reach a classic menu: background={background}, result={outcome:?}, status={}",serde_json::to_string(status)?);
        drop(client);shutdown_idle_worker();let until=Instant::now()+Duration::from_secs(5);while pending_workers()>0&&Instant::now()<until{pump(10);}
        ensure!(pending_workers()==0,"expanded menu worker did not retire");
    }
    evidence["modern_more_options_selection_and_background"]=serde_json::json!(true);
    evidence["virtual_archive_transfer"]=exercise_virtual_archive(root,owner.0)?;
    // A visible modern presenter is not a TrackPopupMenu. Cancellation must
    // close that exact presenter before the idle/retirement policy can finish.
    {
        let _cancel_probe=Environment::set("ZSDESK_MENU_TEST_PROBE","modern-cancel");
        let path=root.join("cancel.txt");fs::write(&path,b"cancel fixture")?;fixtures.0.push(path.clone());
        let pidl=zsdesk_windows::shell::pidl::location_pidl(&zsdesk_core::model::explorer::ShellLocation::Path(path)).context("cancel PIDL")?;
        let mut client=Client::spawn(owner.0,POINT{x:70,y:100},vec![pidl],None,ContextMenuOptions{modern:true,..Default::default()},MenuInterceptions::default(),Mode::More)?;
        let deadline=Instant::now()+Duration::from_secs(15);
        loop {
            ensure!(client.poll(&mut|_|true).is_none(),"cancel fixture finished before menu opened");
            if client.detailed_timings().menu_open_observation.as_deref()==Some("native-presenter-open"){break;}
            ensure!(Instant::now()<deadline,"modern cancel fixture never became visible");client.wait_for_progress(20);pump(5);
        }
        client.cancel();let deadline=Instant::now()+Duration::from_secs(5);
        loop{if client.poll(&mut|_|false).is_some(){break;}ensure!(Instant::now()<deadline,"modern presenter did not cancel");client.wait_for_progress(20);pump(5);}
        drop(client);shutdown_idle_worker();
        let deadline=Instant::now()+Duration::from_secs(5);while pending_workers()>0&&Instant::now()<deadline{pump(10);}
        ensure!(pending_workers()==0,"cancelled modern presenter retained worker");
    }
    Ok(evidence)
}

#[test]
#[ignore="Win11 presenter with a separate window station and clipboard; run alone with ZSDESK_NATIVE_TEST_OUTPUT"]
fn native_modern_menu_worker_routes_owner_command_to_exact_selection()->Result<()>{
    let root=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("output")?).join("modern-menu-owner");ensure!(root.is_absolute(),"absolute output required");fs::create_dir_all(&root)?;
    let before=unsafe{GetForegroundWindow()};let original=unsafe{GetProcessWindowStation()};ensure!(!original.is_null(),"current window station missing");
    let station_name=format!("ZSDesk.ModernMenuOwner.{}.{}",std::process::id(),SERIAL.fetch_add(1,Ordering::Relaxed));
    let station=unsafe{CreateWindowStationW(wide(&station_name).as_ptr(),0,0x000f037f,std::ptr::null())};ensure!(!station.is_null(),"private window station creation failed");
    if unsafe{SetProcessWindowStation(station)}==0{let _=unsafe{CloseWindowStation(station)};anyhow::bail!("private window station selection failed");}
    let _station=Environment::set("ZSDESK_MENU_TEST_STATION",&station_name);let name="Default".to_owned();
    let desktop=unsafe{CreateDesktopW(wide(&name).as_ptr(),std::ptr::null(),std::ptr::null(),0,0x000f00ff,std::ptr::null())};
    if desktop.is_null(){let _=unsafe{SetProcessWindowStation(original)};let _=unsafe{CloseWindowStation(station)};anyhow::bail!("private desktop creation failed");}
    let raw=desktop as usize;let target=root.clone();
    let result=thread::spawn(move||->Result<serde_json::Value>{ensure!(unsafe{SetThreadDesktop(raw as*mut c_void)}!=0,"SetThreadDesktop failed");exercise_modern_owner(&target,&name)}).join().map_err(|_|anyhow::anyhow!("modern menu owner test panicked")).and_then(|value|value);
    let workers_retired=pending_workers()==0;
    let restored=unsafe{SetProcessWindowStation(original)}!=0;let closed=unsafe{CloseDesktop(desktop)}!=0;let station_closed=unsafe{CloseWindowStation(station)}!=0;
    let foreground=unsafe{GetForegroundWindow()};let mut pid=0;unsafe{GetWindowThreadProcessId(foreground,Some(&mut pid));}
    let isolated=pid!=std::process::id();let report=serde_json::json!({"state":if result.is_ok()&&restored&&closed&&station_closed&&isolated&&workers_retired{"passed"}else{"failed"},"checks":result.as_ref().map_err(|error|format!("{error:#}")),
        "private_window_station":station_name,"user_clipboard_accessed":false,"workers_retired":workers_retired,"station_restored":restored,"station_closed":station_closed,"desktop_closed":closed,"foreground_unchanged":before==foreground,"test_did_not_activate":isolated});
    fs::write(root.join("verification.current.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");result?;ensure!(restored&&closed&&station_closed&&isolated&&workers_retired,"window station/worker cleanup or focus changed");Ok(())
}

#[test]
#[ignore="Requires ZSDESK_MENU_STATIC_PROBE and output; creates/removes one unique HKCU background verb on a private desktop"]
fn native_background_static_verb_preserves_folder_and_working_directory()->Result<()>{
    use windows::Win32::System::Registry::*;
    let root=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("output")?).join("background-static-verb");
    ensure!(root.is_absolute(),"absolute output required");fs::create_dir_all(&root)?;
    let executable=PathBuf::from(std::env::var_os("ZSDESK_MENU_STATIC_PROBE").context("static fixture executable")?);
    ensure!(executable.is_absolute()&&executable.is_file(),"static fixture executable missing");
    let verb=format!("ZSDeskCompatProbe{}",std::process::id());let key_name=format!(r"Software\Classes\Directory\Background\shell\{verb}");
    let mut key=HKEY::default();let mut disposition=REG_CREATE_KEY_DISPOSITION::default();
    unsafe{RegCreateKeyExW(HKEY_CURRENT_USER,PCWSTR(wide(&key_name).as_ptr()),None,PCWSTR::null(),REG_OPTION_NON_VOLATILE,KEY_READ|KEY_WRITE,None,&mut key,Some(&mut disposition))}.ok()?;
    if disposition!=REG_CREATED_NEW_KEY{let _=unsafe{RegCloseKey(key)};anyhow::bail!("fixture registry key already exists");}
    struct Registration{key:HKEY,name:String}
    impl Drop for Registration{fn drop(&mut self){let _=unsafe{RegCloseKey(self.key)};let _=unsafe{RegDeleteTreeW(HKEY_CURRENT_USER,PCWSTR(wide(&self.name).as_ptr()))};}}
    let registration=Registration{key,name:key_name.clone()};
    let marker=root.join("result.txt");
    let set=|key:HKEY,value:&str|->Result<()>{let text=wide(value);let bytes=unsafe{std::slice::from_raw_parts(text.as_ptr().cast::<u8>(),text.len()*2)};unsafe{RegSetValueExW(key,PCWSTR::null(),None,REG_SZ,Some(bytes))}.ok()?;Ok(())};
    set(key,"ZSDesk isolated background command")?;
    let mut command=HKEY::default();unsafe{RegCreateKeyExW(key,w!("command"),None,PCWSTR::null(),REG_OPTION_NON_VOLATILE,KEY_WRITE,None,&mut command,None)}.ok()?;
    let result=set(command,&format!("\"{}\" --invoked \"{}\" \"%V\"",executable.display(),marker.display()));let _=unsafe{RegCloseKey(command)};result?;
    let _verb=Environment::set("ZSDESK_MENU_STATIC_VERB",&verb);
    let before=unsafe{GetForegroundWindow()};let name=format!("ZSDesk.BackgroundStatic.{}",std::process::id());
    let desktop=unsafe{CreateDesktopW(wide(&name).as_ptr(),std::ptr::null(),std::ptr::null(),0,0x000f00ff,std::ptr::null())};ensure!(!desktop.is_null(),"private desktop");
    let raw=desktop as usize;let target=root.clone();
    let result=thread::spawn(move||->Result<serde_json::Value>{
        ensure!(unsafe{SetThreadDesktop(raw as*mut c_void)}!=0,"SetThreadDesktop failed");unsafe{OleInitialize(None)}?;
        struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}let _apartment=Apartment;let _pool=PoolCleanup;
        let _desktop=Environment::set("ZSDESK_MENU_TEST_DESKTOP",&name);let _root=Environment::set("ZSDESK_MENU_ROOT",target.join("workers"));
        let folder=target.join("目录 中文 空格");fs::create_dir_all(&folder)?;
        let pidl=zsdesk_windows::shell::pidl::location_pidl(&zsdesk_core::model::explorer::ShellLocation::Path(folder.clone())).context("folder identity")?;
        let module=HINSTANCE(unsafe{GetModuleHandleW(None)?}.0);let class=w!("ZSDesk.StaticBackground.Regression");
        unsafe{RegisterClassW(&WNDCLASSW{hInstance:module,lpfnWndProc:Some(owner_proc),lpszClassName:class,..Default::default()});}
        let owner=Owner(unsafe{CreateWindowExW(WS_EX_NOACTIVATE,class,w!("Static background test"),WS_OVERLAPPEDWINDOW,20,20,640,480,None,None,Some(module),None)}?);
        unsafe{let _=ShowWindow(owner.0,SW_SHOWNOACTIVATE);}
        let mut checks=Vec::new();
        for modern in [false,true]{
            if marker.exists(){fs::remove_file(&marker)?;}
            let catalog_count={
                let _probe=Environment::set("ZSDESK_MENU_TEST_PROBE","catalog-only");
                let expected=format!("verb:{}",std::env::var("ZSDESK_MENU_STATIC_VERB")?.to_ascii_lowercase());
                let options=ContextMenuOptions{modern,hidden_verbs:vec![expected.clone()],..Default::default()};
                let mut client=Client::spawn(owner.0,POINT{x:70,y:100},vec![pidl.clone()],Some(folder.clone()),options.clone(),MenuInterceptions::default(),Mode::CatalogBackground)?;
                let (outcome,_)=complete(&mut client,owner.0)?;
                ensure!(matches!(outcome,Ok(MenuOutcome::Dismissed)),"catalog request did not return without invocation: {outcome:?}");
                let status=client.last_status.as_ref().context("catalog IPC status missing")?;
                ensure!(!status.before_invoke&&status.timing.menu_open_observation.is_none()&&!marker.exists(),"catalog executed or displayed a command");
                fn flatten<'a>(items:&'a[zsdesk_windows::shell::menu::MenuCatalogItem],depth:usize,rows:&mut Vec<&'a zsdesk_windows::shell::menu::MenuCatalogItem>)->Result<()>{
                    ensure!(depth<=2,"catalog exceeded submenu depth");
                    for item in items{rows.push(item);if !item.children.is_empty(){flatten(&item.children,depth+1,rows)?;}}
                    ensure!(rows.len()<=512,"catalog exceeded item bound");Ok(())
                }
                let mut rows=Vec::new();flatten(&status.catalog,0,&mut rows)?;
                let entry=rows.iter().find(|item|item.key==expected).context("hidden static extension absent from unfiltered catalog")?;
                ensure!(entry.manageable&&zsdesk_windows::shell::menu::catalog_item_hidden(entry,&options),"catalog did not preserve hidden extension identity");
                ensure!(serde_json::to_vec(status)?.len()<=2*1024*1024,"catalog status exceeds IPC bound");
                rows.len()
            };
            let _probe=Environment::set("ZSDESK_MENU_TEST_PROBE",if modern{"modern-owner-static-probe"}else{"background-static-probe"});
            let mut client=Client::spawn(owner.0,POINT{x:70,y:100},vec![pidl.clone()],Some(folder.clone()),ContextMenuOptions{modern,..Default::default()},MenuInterceptions::default(),Mode::Background)?;
            let (outcome,timing)=complete(&mut client,owner.0)?;ensure!(matches!(outcome,Ok(MenuOutcome::ShellInvoked|MenuOutcome::NativeHandled)),"static background command failed: {outcome:?}");
            let deadline=Instant::now()+Duration::from_secs(5);while !marker.exists(){pump(10);ensure!(Instant::now()<deadline,"static command produced no result");}
            let actual=fs::read_to_string(&marker)?;let lines:Vec<_>=actual.lines().collect();
            ensure!(lines.len()==2&&lines.iter().all(|line|std::path::Path::new(line)==folder),"static command received wrong folder/working directory: {actual}");
            checks.push(serde_json::json!({"modern":modern,"folder_argument":true,"working_directory":true,"bind_microseconds":timing.0,"populate_microseconds":timing.1,
                "catalog_hidden_entry_found":true,"catalog_no_execution":true,"catalog_count":catalog_count}));
            drop(client);
        }
        Ok(serde_json::json!({"commands":checks,"fixture_executed":true}))
    }).join().map_err(|_|anyhow::anyhow!("background static probe panicked")).and_then(|r|r);
    let closed=unsafe{CloseDesktop(desktop)}!=0;drop(registration);
    let mut check=HKEY::default();let removed=unsafe{RegOpenKeyExW(HKEY_CURRENT_USER,PCWSTR(wide(&key_name).as_ptr()),None,KEY_READ,&mut check)}.is_err();if !removed{let _=unsafe{RegCloseKey(check)};}
    let report=serde_json::json!({"state":if result.is_ok()&&closed&&removed{"passed"}else{"failed"},"checks":result.as_ref().map_err(|e|format!("{e:#}")),"desktop_closed":closed,"registry_removed":removed,"foreground_unchanged":before==unsafe{GetForegroundWindow()}});
    fs::write(root.join("verification.current.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");result?;ensure!(closed&&removed,"fixture cleanup failed");Ok(())
}

#[test]
#[ignore="Native file/folder property sheets on a private desktop; requires ZSDESK_NATIVE_TEST_OUTPUT"]
fn native_modern_fence_properties_execute_once()->Result<()>{
    exercise_fence_properties(true,false)
}
#[test]
#[ignore="Classic property sheet control on a private desktop; requires ZSDESK_NATIVE_TEST_OUTPUT"]
fn native_classic_fence_properties_control()->Result<()>{exercise_fence_properties(false,false)}
#[test]
#[ignore="Registered test IExplorerCommand DLL on a private desktop; requires ZSDESK_MENU_EXTENSION_PROBE and output"]
fn native_modern_fence_extension_executes_exact_selection()->Result<()>{
    let dll=PathBuf::from(std::env::var_os("ZSDESK_MENU_EXTENSION_PROBE").context("extension fixture DLL")?);
    with_extension_registration(&dll,||{
        let _nested=Environment::set("ZSDESK_TEST_COMMAND_NESTED","");
        exercise_fence_properties(true,true)
    })
}

fn with_extension_registration(dll:&Path,action:impl FnOnce()->Result<()>)->Result<()>{
    use windows::Win32::{Foundation::{ERROR_FILE_NOT_FOUND,ERROR_PATH_NOT_FOUND,WAIT_OBJECT_0,WAIT_ABANDONED},
        System::{Registry::*,Threading::{CreateMutexW,WaitForSingleObject,ReleaseMutex}}};
    ensure!(dll.is_absolute()&&dll.is_file()&&fs::symlink_metadata(dll)?.file_attributes()&0x400==0,"extension fixture DLL must be an absolute regular file");
    struct Lock{handle:HANDLE,owned:bool}impl Drop for Lock{fn drop(&mut self){unsafe{if self.owned{let _=ReleaseMutex(self.handle);}let _=CloseHandle(self.handle);}}}
    let mut lock=Lock{handle:unsafe{CreateMutexW(None,false,w!("Local\\ZSDesk.MenuExtensionFixture.B3489BD3"))}?,owned:false};
    let waited=unsafe{WaitForSingleObject(lock.handle,15000)};ensure!(waited==WAIT_OBJECT_0||waited==WAIT_ABANDONED,"extension fixture registration is in use");lock.owned=true;
    struct Key(HKEY);impl Drop for Key{fn drop(&mut self){let _=unsafe{RegCloseKey(self.0)};}}
    fn open(path:&str)->Result<Option<Key>>{
        let mut key=HKEY::default();let status=unsafe{RegOpenKeyExW(HKEY_CURRENT_USER,PCWSTR(wide(path).as_ptr()),None,KEY_READ,&mut key)};
        if status==ERROR_FILE_NOT_FOUND||status==ERROR_PATH_NOT_FOUND{return Ok(None);}status.ok()?;Ok(Some(Key(key)))
    }
    fn counts(key:HKEY)->Result<(u32,u32)>{let(mut children,mut values)=(0,0);unsafe{RegQueryInfoKeyW(key,None,None,None,Some(&mut children),None,None,Some(&mut values),None,None,None,None)}.ok()?;Ok((children,values))}
    fn string(key:HKEY,name:&str)->Result<String>{
        let mut kind=REG_VALUE_TYPE::default();let mut length=0;unsafe{RegQueryValueExW(key,PCWSTR(wide(name).as_ptr()),None,Some(&mut kind),None,Some(&mut length))}.ok()?;
        ensure!(kind==REG_SZ&&length<=65536&&length%2==0,"fixture registry value changed type or size");let mut bytes=vec![0u8;length as usize];
        unsafe{RegQueryValueExW(key,PCWSTR(wide(name).as_ptr()),None,Some(&mut kind),Some(bytes.as_mut_ptr()),Some(&mut length))}.ok()?;
        let text=String::from_utf16(&bytes[..length as usize].chunks_exact(2).map(|pair|u16::from_le_bytes([pair[0],pair[1]])).collect::<Vec<_>>())?;
        Ok(text.trim_end_matches('\0').to_owned())
    }
    struct Registration{root:String,child:String,created_root:bool,child_created:bool,values:Vec<(String,String)>,cleaned:bool}
    impl Registration{
        fn cleanup(&mut self)->Result<()>{
            if self.cleaned{return Ok(());}
            if self.child_created{
                if let Some(key)=open(&self.child)?{
                    ensure!(counts(key.0)?==(0,self.values.len()as u32),"fixture registry contents changed; registration retained");
                    for(name,value)in &self.values{ensure!(string(key.0,name)?==*value,"fixture registry value changed; registration retained");}
                    drop(key);unsafe{RegDeleteTreeW(HKEY_CURRENT_USER,PCWSTR(wide(&self.child).as_ptr()))}.ok()?;
                }
            }
            ensure!(open(&self.child)?.is_none(),"fixture registration remains");
            if let Some(key)=open(&self.root)?{
                ensure!(counts(key.0)?==(0,0),"fixture CLSID root changed; retained");drop(key);
                if self.created_root{unsafe{RegDeleteTreeW(HKEY_CURRENT_USER,PCWSTR(wide(&self.root).as_ptr()))}.ok()?;}
            }
            let after=open(&self.root)?;
            ensure!(if self.created_root{after.is_none()}else{after.as_ref().is_some_and(|key|counts(key.0).ok()==Some((0,0)))},"fixture root was not restored");
            self.cleaned=true;Ok(())
        }
    }
    impl Drop for Registration{fn drop(&mut self){let _=self.cleanup();}}
    let root=r"Software\Classes\CLSID\{B3489BD3-6C40-4AAA-9610-D72754252627}".to_owned();
    if let Some(key)=open(&root)?{ensure!(counts(key.0)?==(0,0),"extension fixture CLSID contains existing data; not overwritten");}
    let mut key=HKEY::default();let mut disposition=REG_CREATE_KEY_DISPOSITION::default();
    unsafe{RegCreateKeyExW(HKEY_CURRENT_USER,PCWSTR(wide(&root).as_ptr()),None,PCWSTR::null(),REG_OPTION_NON_VOLATILE,KEY_ALL_ACCESS,None,&mut key,Some(&mut disposition))}.ok()?;drop(Key(key));
    let mut registration=Registration{child:format!(r"{root}\InprocServer32"),root,created_root:disposition==REG_CREATED_NEW_KEY,child_created:false,values:Vec::new(),cleaned:false};
    let setup=(||->Result<()>{
        let mut key=HKEY::default();unsafe{RegCreateKeyExW(HKEY_CURRENT_USER,PCWSTR(wide(&registration.child).as_ptr()),None,PCWSTR::null(),REG_OPTION_NON_VOLATILE,KEY_ALL_ACCESS,None,&mut key,Some(&mut disposition))}.ok()?;let key=Key(key);
        ensure!(disposition==REG_CREATED_NEW_KEY,"fixture registration appeared concurrently");registration.child_created=true;
        for(name,value)in [("",dll.to_string_lossy().into_owned()),("ThreadingModel","Apartment".to_owned())]{
            let text=wide(&value);let bytes=unsafe{std::slice::from_raw_parts(text.as_ptr().cast::<u8>(),text.len()*2)};
            unsafe{RegSetValueExW(key.0,PCWSTR(wide(name).as_ptr()),None,REG_SZ,Some(bytes))}.ok()?;registration.values.push((name.to_owned(),value));
        }Ok(())
    })();
    let result=setup.and_then(|_|action());let cleaned=registration.cleanup();
    if let Some(output)=std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT"){
        let output=PathBuf::from(output);if output.is_absolute(){fs::create_dir_all(&output)?;fs::write(output.join("extension-registration.current.json"),serde_json::to_vec_pretty(&serde_json::json!({
            "clsid":"{B3489BD3-6C40-4AAA-9610-D72754252627}","created_root":registration.created_root,"registration_removed":registration.cleaned,
            "preexisting_empty_root_preserved":!registration.created_root&&registration.cleaned,"cleanup_error":cleaned.as_ref().err().map(|error|format!("{error:#}")),"test_error":result.as_ref().err().map(|error|format!("{error:#}"))}))?)?;}
    }
    cleaned?;result
}

fn read_extension_text(path:&Path)->Result<String>{
    let bytes=fs::read(path)?;ensure!(bytes.len()<=1024*1024&&bytes.len()%2==0,"invalid extension fixture output");
    Ok(String::from_utf16(&bytes.chunks_exact(2).map(|pair|u16::from_le_bytes([pair[0],pair[1]])).collect::<Vec<_>>())?)
}

fn drive_nested_extension_ui(pid:u32,desktop:usize,deep:bool,stop:std::sync::Arc<std::sync::atomic::AtomicBool>)->Result<serde_json::Value>{
    use windows::{core::{BSTR,Interface},Win32::{System::{Com::{CoCreateInstance,CoInitializeEx,CoUninitialize,CLSCTX_INPROC_SERVER,COINIT_MULTITHREADED},Variant::VARIANT},UI::Accessibility::*}};
    ensure!(unsafe{SetThreadDesktop(desktop as*mut c_void)}!=0,"UIA thread private desktop");
    unsafe{CoInitializeEx(None,COINIT_MULTITHREADED)}.ok()?;
    struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{CoUninitialize();}}}let _apartment=Apartment;
    let automation:IUIAutomation=unsafe{CoCreateInstance(&CUIAutomation8,None,CLSCTX_INPROC_SERVER)}?;
    let bounded=automation.cast::<IUIAutomation2>()?;unsafe{bounded.SetConnectionTimeout(1000)?;bounded.SetTransactionTimeout(1000)?;}
    let names=if deep{vec!["ZSDesk issue27 extension","ZSDesk nested tools","ZSDesk deep execute"]}else{vec!["ZSDesk issue27 extension","ZSDesk child execute"]};
    let conditions=names.iter().map(|name|unsafe{automation.CreatePropertyCondition(UIA_NamePropertyId,&VARIANT::from(BSTR::from(*name)))}).collect::<windows::core::Result<Vec<_>>>()?;
    struct Visible{pid:u32,windows:Vec<HWND>}
    unsafe extern "system" fn collect(window:HWND,data:LPARAM)->windows::core::BOOL{
        let target=unsafe{&mut*(data.0 as*mut Visible)};let mut pid=0;unsafe{GetWindowThreadProcessId(window,Some(&mut pid));}
        if pid==target.pid&&unsafe{IsWindowVisible(window)}.as_bool(){target.windows.push(window);}true.into()
    }
    fn snapshot(automation:&IUIAutomation,pid:u32)->serde_json::Value{
        let result=(||->Result<serde_json::Value>{
            let request=unsafe{automation.CreateCacheRequest()}?;unsafe{request.SetTreeScope(TreeScope_Element)}?;
            for property in [UIA_NamePropertyId,UIA_ControlTypePropertyId,UIA_ClassNamePropertyId,UIA_BoundingRectanglePropertyId,
                UIA_IsOffscreenPropertyId,UIA_IsEnabledPropertyId,UIA_ProcessIdPropertyId,UIA_NativeWindowHandlePropertyId,
                UIA_IsExpandCollapsePatternAvailablePropertyId,UIA_IsInvokePatternAvailablePropertyId,UIA_ExpandCollapseExpandCollapseStatePropertyId]{unsafe{request.AddProperty(property)}?;}
            let condition=unsafe{automation.CreateTrueCondition()}?;let mut visible=Visible{pid,windows:Vec::new()};
            let _=unsafe{EnumWindows(Some(collect),LPARAM((&mut visible as*mut Visible)as isize))};
            let until=Instant::now()+Duration::from_secs(2);let mut windows=Vec::new();let mut total=0;
            for window in visible.windows.into_iter().take(8){
                if Instant::now()>=until||total>=256{break;}
                let Ok(root)=(unsafe{automation.ElementFromHandle(window)})else{continue;};
                let values=unsafe{root.FindAllBuildCache(TreeScope_Subtree,&condition,&request)};
                let elements=match values{Ok(value)=>value,Err(error)=>{windows.push(serde_json::json!({"window":window.0 as usize,"error":format!("{error}")}));continue;}};
                let count=unsafe{elements.Length()}?;let mut rows=Vec::new();
                for index in 0..count.min(128){
                    if total>=256||Instant::now()>=until{break;}total+=1;
                    let element=unsafe{elements.GetElement(index)}?;
                    let boolean=|property|unsafe{element.GetCachedPropertyValue(property)}.ok().and_then(|value|bool::try_from(&value).ok());
                    let integer=|property|unsafe{element.GetCachedPropertyValue(property)}.ok().and_then(|value|i32::try_from(&value).ok());
                    rows.push(serde_json::json!({"index":index,"name":unsafe{element.CachedName()}.ok().map(|value|value.to_string()),
                        "control_type":unsafe{element.CachedControlType()}.ok().map(|value|value.0),"class":unsafe{element.CachedClassName()}.ok().map(|value|value.to_string()),
                        "bounds":unsafe{element.CachedBoundingRectangle()}.ok().map(|rect|[rect.left,rect.top,rect.right,rect.bottom]),
                        "offscreen":unsafe{element.CachedIsOffscreen()}.ok().map(|value|value.as_bool()),"enabled":unsafe{element.CachedIsEnabled()}.ok().map(|value|value.as_bool()),
                        "pid":unsafe{element.CachedProcessId()}.ok(),"native_window":integer(UIA_NativeWindowHandlePropertyId),
                        "expand_available":boolean(UIA_IsExpandCollapsePatternAvailablePropertyId),"invoke_available":boolean(UIA_IsInvokePatternAvailablePropertyId),
                        "expand_state":integer(UIA_ExpandCollapseExpandCollapseStatePropertyId)}));
                }
                windows.push(serde_json::json!({"window":window.0 as usize,"available_elements":count,"elements":rows}));
            }
            Ok(serde_json::json!({"windows":windows,"sampled_elements":total,"bounded":true}))
        })();
        result.unwrap_or_else(|error|serde_json::json!({"error":format!("{error:#}")}))
    }
    let started=Instant::now();let deadline=started+Duration::from_secs(25);let mut stage=0usize;let mut evidence=Vec::new();let mut observed=std::collections::BTreeSet::new();
    let mut rejected=std::collections::BTreeMap::new();let mut snapshots=Vec::new();let mut stage_started=Instant::now();let mut waited_snapshot=false;
    while Instant::now()<deadline&&!stop.load(Ordering::Acquire){
        let mut visible=Visible{pid,windows:Vec::new()};let _=unsafe{EnumWindows(Some(collect),LPARAM((&mut visible as*mut Visible)as isize))};
        for window in visible.windows{
            let Ok(root)=(unsafe{automation.ElementFromHandle(window)})else{continue;};
            let Ok(element)=(unsafe{root.FindFirst(TreeScope_Subtree,&conditions[stage])})else{continue;};
            let offscreen=unsafe{element.CurrentIsOffscreen()}.ok().map(|value|value.as_bool());
            let enabled=unsafe{element.CurrentIsEnabled()}.ok().map(|value|value.as_bool());let process=unsafe{element.CurrentProcessId()}.ok();
            let bounds=unsafe{element.CurrentBoundingRectangle()}.ok();let control_type=unsafe{element.CurrentControlType()}.ok().map(|value|value.0);
            let mut reasons=Vec::new();if offscreen!=Some(false){reasons.push("offscreen-or-unreadable");}if enabled!=Some(true){reasons.push("disabled-or-unreadable");}
            if process!=Some(pid as i32){reasons.push("different-or-unreadable-process");}
            if !bounds.as_ref().is_some_and(|rect|rect.right>rect.left&&rect.bottom>rect.top){reasons.push("empty-or-unreadable-bounds");}
            let key=format!("{stage}:{}",window.0 as usize);
            if !reasons.is_empty(){rejected.insert(key,serde_json::json!({"name":names[stage],"reasons":reasons,"control_type":control_type,"offscreen":offscreen,"enabled":enabled,"pid":process,
                "bounds":bounds.map(|rect|[rect.left,rect.top,rect.right,rect.bottom])}));continue;}
            let bounds=bounds.unwrap();
            observed.insert(names[stage].to_owned());
            if stage+1<names.len(){
                let expand=match unsafe{element.GetCurrentPatternAs::<IUIAutomationExpandCollapsePattern>(UIA_ExpandCollapsePatternId)}{
                    Ok(value)=>value,Err(error)=>{rejected.insert(key,serde_json::json!({"name":names[stage],"reason":"expand-pattern-unavailable","control_type":control_type,"error":format!("{error}")}));continue;}};
                let before=match unsafe{expand.CurrentExpandCollapseState()}{Ok(value)=>value,Err(error)=>{
                    rejected.insert(key,serde_json::json!({"name":names[stage],"reason":"expand-state-unreadable","error":format!("{error}")}));continue;}};
                let focus=unsafe{element.SetFocus()};let after_focus=unsafe{expand.CurrentExpandCollapseState()}.unwrap_or(before);
                let expansion=if after_focus!=ExpandCollapseState_Expanded&&after_focus!=ExpandCollapseState_PartiallyExpanded{unsafe{expand.Expand()}}else{Ok(())};
                evidence.push(serde_json::json!({"name":names[stage],"action":"native-uia-focus-expand","window":window.0 as usize,"bounds":[bounds.left,bounds.top,bounds.right,bounds.bottom],"before":before.0,
                    "focus_error":focus.as_ref().err().map(|error|format!("{error}")),"after_focus":after_focus.0,"expand_error":expansion.as_ref().err().map(|error|format!("{error}")),"after_expand":unsafe{expand.CurrentExpandCollapseState()}.ok().map(|value|value.0)}));
                snapshots.push(serde_json::json!({"reason":"after-expand","stage":stage,"at_ms":started.elapsed().as_millis(),"tree":snapshot(&automation,pid)}));
                if expansion.is_err(){anyhow::bail!("native UIA expansion failed: {}",serde_json::json!({"stage":stage,"actions":evidence,"snapshots":snapshots,"rejected_matches":rejected}));}
                stage+=1;stage_started=Instant::now();waited_snapshot=false;break;
            }else{
                let invoke=match unsafe{element.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId)}{
                    Ok(value)=>value,Err(error)=>{rejected.insert(key,serde_json::json!({"name":names[stage],"reason":"invoke-pattern-unavailable","control_type":control_type,"error":format!("{error}")}));continue;}};
                // Attempt exactly once. Even an automation error can accompany
                // native completion; the parent verifies its real outcome/trace.
                ensure!(!stop.load(Ordering::Acquire),"native UIA invocation cancelled before dispatch");let result=unsafe{invoke.Invoke()};
                evidence.push(serde_json::json!({"name":names[stage],"action":"native-uia-invoke","window":window.0 as usize,"bounds":[bounds.left,bounds.top,bounds.right,bounds.bottom],"hresult":result.as_ref().err().map(|error|format!("0x{:08X}",error.code().0 as u32))}));
                return Ok(serde_json::json!({"state":"invocation-attempted","deep":deep,"observed":observed,"actions":evidence,"private_desktop":true,"rejected_matches":rejected,"snapshots":snapshots}));
            }
        }
        if !waited_snapshot&&stage_started.elapsed()>=Duration::from_millis(700){snapshots.push(serde_json::json!({"reason":"waiting-for-next-item","stage":stage,"at_ms":started.elapsed().as_millis(),"tree":snapshot(&automation,pid)}));waited_snapshot=true;}
        thread::sleep(Duration::from_millis(20));
    }
    snapshots.push(serde_json::json!({"reason":"stop","stage":stage,"at_ms":started.elapsed().as_millis(),"tree":snapshot(&automation,pid)}));
    anyhow::bail!("native submenu element did not become actionable: {}",serde_json::json!({"stage":stage,"wanted":names[stage],"observed":observed,"actions":evidence,"rejected_matches":rejected,"snapshots":snapshots,"cancelled":stop.load(Ordering::Acquire)}))
}

#[test]
#[ignore="Registered nested IExplorerCommand through real native UIA menu expansion on a private desktop; requires ZSDESK_MENU_EXTENSION_PROBE"]
fn native_modern_nested_extension_menu_executes_exact_selection_once()->Result<()>{
    use sha2::{Digest,Sha256};
    ensure!(std::env::var_os("ZSDESK_MENU_TEST_INPUT_DESKTOP").is_none(),"nested extension fixture never runs on the input desktop");
    let dll=PathBuf::from(std::env::var_os("ZSDESK_MENU_EXTENSION_PROBE").context("nested extension fixture DLL")?);
    let dll_hash=format!("{:x}",Sha256::digest(fs::read(&dll)?));
    with_extension_registration(&dll,||{
        let output=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("output")?).join("nested-extension");
        ensure!(output.is_absolute(),"absolute output required");fs::create_dir_all(&output)?;
        let name=format!("ZSDesk.NestedExtension.{}",std::process::id());
        let desktop=unsafe{CreateDesktopW(wide(&name).as_ptr(),std::ptr::null(),std::ptr::null(),0,0x000f00ff,std::ptr::null())};ensure!(!desktop.is_null(),"private desktop creation");
        let raw=desktop as usize;let root=output.clone();let desktop_name=name.clone();
        let result=thread::spawn(move||->Result<serde_json::Value>{
            ensure!(unsafe{SetThreadDesktop(raw as*mut c_void)}!=0,"test thread private desktop");unsafe{OleInitialize(None)}?;
            struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}let _apartment=Apartment;let _pool=PoolCleanup;
            let _desktop=Environment::set("ZSDESK_MENU_TEST_DESKTOP",&desktop_name);
            let _root=Environment::set("ZSDESK_MENU_ROOT",root.join("workers"));
            let _probe=Environment::set("ZSDESK_MENU_TEST_PROBE","modern-ui-nested-extension");
            let _nested=Environment::set("ZSDESK_TEST_COMMAND_NESTED","1");
            let _delay=Environment::set("ZSDESK_TEST_COMMAND_DELAY_MS","350");
            let module=HINSTANCE(unsafe{GetModuleHandleW(None)?}.0);let class=w!("ZSDesk.NestedExtension.Parent");
            unsafe{RegisterClassW(&WNDCLASSW{hInstance:module,lpfnWndProc:Some(owner_proc),lpszClassName:class,..Default::default()});}
            let owner=Owner(unsafe{CreateWindowExW(WS_EX_NOACTIVATE,class,w!("Nested extension regression"),WS_OVERLAPPEDWINDOW,20,20,640,480,None,None,Some(module),None)}?);
            unsafe{let _=ShowWindow(owner.0,SW_SHOWNOACTIVATE);}let mut reports=Vec::new();
            for deep in [false,true]{
                let paths=if deep{let path=root.join("nested 中文 文件夹");fs::create_dir_all(&path)?;vec![path]}else{
                    let paths=vec![root.join("first selected.txt"),root.join("第二个 选中文件.txt")];for path in &paths{fs::write(path,b"owned nested-command fixture")?;}paths};
                let marker=root.join(if deep{"deep.marker"}else{"children.marker"});let trace=PathBuf::from(format!("{}.trace",marker.display()));
                ensure!(!marker.exists()&&!trace.exists(),"previous nested command evidence retained; inspect before retrying");
                let _marker=Environment::set("ZSDESK_TEST_COMMAND_MARKER",&marker);
                let pidls=paths.iter().map(|path|zsdesk_windows::shell::pidl::location_pidl(&zsdesk_core::model::explorer::ShellLocation::Path(path.clone())).context("selected fixture PIDL")).collect::<Result<Vec<_>>>()?;
                let mut client=Client::spawn(owner.0,POINT{x:70,y:100},pidls,None,ContextMenuOptions{modern:true,..Default::default()},
                    MenuInterceptions{rename:true,open:true,file_actions:true},Mode::More)?;
                let pid=client.pid().context("nested worker PID")?;let stop=std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));let signal=stop.clone();
                let(tx,rx)=std::sync::mpsc::channel();
                let ui=thread::spawn(move||{let result=drive_nested_extension_ui(pid,raw,deep,signal).map_err(|error|format!("{error:#}"));let _=tx.send(result);});
                let started=Instant::now();let deadline=started+Duration::from_secs(35);let(mut outcome,mut ui_result,mut terminal_at)=(None,None,None);let mut barriers=0;
                while Instant::now()<deadline{
                    if ui_result.is_none(){if let Ok(result)=rx.try_recv(){ui_result=Some(result);}}
                    if outcome.is_none(){outcome=client.poll(&mut|before|{if before{barriers+=1;}true});if outcome.is_some(){terminal_at=Some(Instant::now());}}
                    if let Some(terminal)=terminal_at{
                        if ui_result.is_some()&&marker.exists()&&terminal.elapsed()>=Duration::from_millis(450){break;}
                        if outcome.as_ref().is_some_and(|value|value.is_err())&&terminal.elapsed()>=Duration::from_secs(2){break;}
                    }
                    if ui_result.as_ref().is_some_and(|value|value.is_err()){break;}
                    pump(10);client.wait_for_progress(20);
                }
                let observed_status=serde_json::to_value(&client.last_status)?;stop.store(true,Ordering::Release);client.cancel();
                let joined_until=Instant::now()+Duration::from_secs(5);let mut after_cancel=None;
                while (!ui.is_finished()||outcome.is_none()&&after_cancel.is_none())&&Instant::now()<joined_until{
                    if outcome.is_none()&&after_cancel.is_none(){after_cancel=client.poll(&mut|_|false);}pump(10);
                }
                let ui_joined=ui.is_finished();if ui_joined{let _=ui.join();}if ui_result.is_none(){ui_result=rx.try_recv().ok();}
                let trace_text=if trace.exists(){Some(read_extension_text(&trace)?)}else{None};
                let marker_text=if marker.exists(){Some(read_extension_text(&marker)?)}else{None};
                let invoked=trace_text.as_ref().map_or(0,|text|text.lines().filter(|line|line.starts_with("invoke-start:")).count());
                let returned=trace_text.as_ref().map_or(0,|text|text.lines().filter(|line|line.starts_with("invoke-done:")).count());
                let level=if deep{3}else{1};
                let report=serde_json::json!({"deep":deep,"worker_pid":pid,"ui":ui_result,"ui_thread_joined":ui_joined,"outcome":outcome.as_ref().map(|value|value.as_ref().map_err(|error|format!("{error:#}"))),
                    "status":observed_status,"after_cancel_status":client.last_status,"after_cancel_outcome":after_cancel.as_ref().map(|value|value.as_ref().map_err(|error|format!("{error:#}"))),
                    "diagnostic":read_recent_diagnostics(),"stderr":worker_stderr(&client.workspace),"invocation_barriers":barriers,"invoke_attempts":invoked,"invoke_completed":returned,"trace":trace_text,
                    "selected_paths":paths,"actual_selection":marker_text,"elapsed_ms":started.elapsed().as_millis(),"configured_invoke_delay_ms":350,"no_input_injection":true});
                reports.push(report);fs::write(root.join("verification.current.json"),serde_json::to_vec_pretty(&reports)?)?;
                let native_open=client.last_status.as_ref().is_some_and(|status|status.probe_menu_open&&!status.probe_classic_loop&&status.error.is_none());
                drop(client);shutdown_idle_worker();let until=Instant::now()+Duration::from_secs(5);while pending_workers()>0&&Instant::now()<until{pump(10);}
                let mut actual=marker_text.as_ref().map(|text|text.lines().map(str::to_owned).collect::<Vec<_>>()).unwrap_or_default();actual.sort();
                let mut expected=paths.iter().map(|path|path.to_string_lossy().into_owned()).collect::<Vec<_>>();expected.sort();
                ensure!(ui_joined&&ui_result.as_ref().is_some_and(|value|value.is_ok())&&outcome.is_some_and(|value|matches!(value,Ok(MenuOutcome::ShellInvoked|MenuOutcome::NativeHandled)))
                    &&native_open&&barriers==1&&invoked==1&&returned==1&&actual==expected&&trace_text.as_ref().is_some_and(|text|
                        text.lines().any(|line|line==format!("invoke-start:{level}"))&&text.lines().any(|line|line=="enum:0")&&(!deep||text.lines().any(|line|line=="enum:2"))
                            &&text.lines().filter_map(|line|line.strip_prefix("invoke-delay-observed:")).filter_map(|value|value.parse::<u64>().ok()).any(|elapsed|elapsed>=300))&&pending_workers()==0,
                    "native nested extension contract failed: {}",reports.last().unwrap());
                fs::remove_file(&marker)?;fs::remove_file(&trace)?;for path in paths{if path.is_dir(){fs::remove_dir(path)?;}else{fs::remove_file(path)?;}}
            }
            Ok(serde_json::json!({"cases":reports,"private_desktop":desktop_name,"dll_sha256":dll_hash,"no_input_injection":true}))
        }).join().map_err(|_|anyhow::anyhow!("nested extension controller panicked")).and_then(|value|value);
        let closed=unsafe{CloseDesktop(desktop)}!=0;
        fs::write(output.join("result.current.json"),serde_json::to_vec_pretty(&serde_json::json!({"checks":result.as_ref().map_err(|error|format!("{error:#}")),"desktop_closed":closed,"desktop_name":name,"state":if result.is_ok()&&closed{"passed"}else{"failed"}}))?)?;
        result?;ensure!(closed,"nested extension private desktop retained");Ok(())
    })
}
fn exercise_fence_properties(modern:bool,extension:bool)->Result<()>{
    let root=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("output")?).join(if extension{"issue27-extension"}else{"issue27-properties"});
    ensure!(root.is_absolute(),"absolute output required");fs::create_dir_all(&root)?;
    let name=format!("ZSDesk.Issue27.{}",std::process::id());
    let interactive=std::env::var_os("ZSDESK_MENU_TEST_INPUT_DESKTOP").is_some();
    let desktop=if interactive{std::ptr::null_mut()}else{unsafe{CreateDesktopW(wide(&name).as_ptr(),std::ptr::null(),std::ptr::null(),0,0x000f00ff,std::ptr::null())}};
    ensure!(interactive||!desktop.is_null(),"private desktop creation failed");
    let raw=desktop as usize;let target=root.clone();
    let result=thread::spawn(move||->Result<()>{
        if !interactive{ensure!(unsafe{SetThreadDesktop(raw as*mut c_void)}!=0,"SetThreadDesktop failed");}unsafe{OleInitialize(None)}?;
        struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}let _apartment=Apartment;let _pool=PoolCleanup;
        let _desktop=(!interactive).then(||Environment::set("ZSDESK_MENU_TEST_DESKTOP",&name));
        let _root=Environment::set("ZSDESK_MENU_ROOT",target.join("workers"));
        let _probe=Environment::set("ZSDESK_MENU_TEST_PROBE",if extension{"modern-owner-extension"}else if modern{"modern-ui-properties"}else{"properties-direct"});
        let module=HINSTANCE(unsafe{GetModuleHandleW(None)?}.0);let class=w!("ZSDesk.Issue27.Parent");
        unsafe{RegisterClassW(&WNDCLASSW{hInstance:module,lpfnWndProc:Some(owner_proc),lpszClassName:class,..Default::default()});}
        let owner=Owner(unsafe{CreateWindowExW(WS_EX_NOACTIVATE,class,w!("Issue 27 regression"),WS_OVERLAPPEDWINDOW,20,20,640,480,None,None,Some(module),None)}?);
        unsafe{let _=ShowWindow(owner.0,SW_SHOWNOACTIVATE);}
        let mut reports=Vec::new();
        for folder in [false,true]{
            let path=target.join(if folder{"issue27 中文 文件夹"}else{"issue27 中文 文件.txt"});
            if folder{fs::create_dir_all(&path)?;}else{fs::write(&path,b"issue27 property fixture")?;}
            let marker=target.join(if folder{"folder.marker"}else{"file.marker"});
            if marker.exists(){fs::remove_file(&marker)?;}
            let _marker=Environment::set("ZSDESK_TEST_COMMAND_MARKER",&marker);
            let pidl=zsdesk_windows::shell::pidl::location_pidl(&zsdesk_core::model::explorer::ShellLocation::Path(path.clone())).context("fixture PIDL")?;
            let mut client=Client::spawn(owner.0,POINT{x:70,y:100},vec![pidl],None,ContextMenuOptions{modern,..Default::default()},
                MenuInterceptions{rename:true,open:true,file_actions:true},Mode::More)?;
            struct Dialogs{pid:u32,seen:std::collections::HashSet<usize>,popups:Vec<HWND>,windows:Vec<String>}
            unsafe extern "system" fn close_properties(window:HWND,data:LPARAM)->windows::core::BOOL{
                let state=unsafe{&mut*(data.0 as*mut Dialogs)};let mut pid=0;unsafe{GetWindowThreadProcessId(window,Some(&mut pid));}
                if pid==state.pid&&unsafe{IsWindowVisible(window)}.as_bool(){
                    let mut title=[0u16;512];let size=unsafe{GetWindowTextW(window,&mut title)}.max(0)as usize;
                    let mut class=[0u16;64];let length=unsafe{GetClassNameW(window,&mut class)}.max(0)as usize;
                    let description=format!("{}: {}",String::from_utf16_lossy(&class[..length]),String::from_utf16_lossy(&title[..size]));
                    if !state.windows.contains(&description){state.windows.push(description);}
                    if String::from_utf16_lossy(&class[..length]).contains("Popup")&&!state.popups.contains(&window){state.popups.push(window);}
                    if String::from_utf16_lossy(&class[..length])=="#32770"&&String::from_utf16_lossy(&title[..size]).contains("issue27"){
                        if state.seen.insert(window.0 as usize){let _=unsafe{PostMessageW(Some(window),WM_CLOSE,WPARAM(0),LPARAM(0))};}
                    }
                }true.into()
            }
            let mut dialogs=Dialogs{pid:client.pid().context("worker pid")?,seen:Default::default(),popups:Vec::new(),windows:Vec::new()};let mut barriers=0;
            use windows::Win32::{System::Com::{CoCreateInstance,CLSCTX_INPROC_SERVER},UI::Accessibility::*};
            let automation:IUIAutomation=unsafe{CoCreateInstance(&CUIAutomation,None,CLSCTX_INPROC_SERVER)}?;
            let condition=unsafe{automation.CreateTrueCondition()}?;let mut clicked=!modern||extension;
            let deadline=Instant::now()+Duration::from_secs(25);let mut outcome=None;
            while Instant::now()<deadline{
                let _=unsafe{EnumWindows(Some(close_properties),LPARAM((&mut dialogs as*mut Dialogs)as isize))};
                if !clicked{for &popup in &dialogs.popups{
                    let Ok(element)=(unsafe{automation.ElementFromHandle(popup)})else{continue;};
                    let Ok(elements)=(unsafe{element.FindAll(TreeScope_Subtree,&condition)})else{continue;};
                    for index in 0..unsafe{elements.Length()}?{
                        let element=unsafe{elements.GetElement(index)}?;let name=unsafe{element.CurrentName()}.unwrap_or_default().to_string();
                        if if extension{name=="ZSDesk issue27 extension"}else{name=="属性"||name=="Properties"}{
                            if let Ok(invoke)=unsafe{element.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId)}{
                                unsafe{invoke.Invoke()}?;clicked=true;break;
                            }
                        }
                    }if clicked{break;}
                }}
                if outcome.is_none(){outcome=client.poll(&mut|before|{if before{barriers+=1;}true});}
                if outcome.as_ref().is_some_and(|value|value.is_err())||outcome.is_some()&&(if extension{marker.exists()}else{!dialogs.seen.is_empty()}){break;}
                pump(10);client.wait_for_progress(20);
            }
            reports.push(serde_json::json!({"folder":folder,"outcome":outcome.as_ref().map(|value|value.as_ref().map_err(|error|format!("{error:#}"))),
                "property_sheets":dialogs.seen.len(),"extension_marker":marker.exists(),"command_trigger":if extension{"native-owner-message"}else if modern{"native-uia-item"}else{"classic-verb"},"interactive":interactive,"windows":dialogs.windows,"invocation_barriers":barriers,"status":client.last_status,"diagnostic":read_recent_diagnostics(),"stderr":worker_stderr(&client.workspace)}));
            fs::write(target.join("verification.current.json"),serde_json::to_vec_pretty(&reports)?)?;
            client.cancel();drop(client);shutdown_idle_worker();let until=Instant::now()+Duration::from_secs(5);while pending_workers()>0&&Instant::now()<until{pump(10);}
            let executed=if extension{marker.exists()}else{dialogs.seen.len()==1};
            ensure!(outcome.is_some_and(|value|matches!(value,Ok(MenuOutcome::ShellInvoked|MenuOutcome::NativeHandled)))&&executed&&barriers==1&&clicked,
                "properties did not execute once: {}",reports.last().unwrap());
            if extension{let bytes=fs::read(&marker)?;ensure!(bytes.len()%2==0,"invalid extension result");let text=String::from_utf16(&bytes.chunks_exact(2).map(|pair|u16::from_le_bytes([pair[0],pair[1]])).collect::<Vec<_>>())?;
                ensure!(text.trim_end()==path.to_string_lossy(),"extension invoked a different selection: {text}");fs::remove_file(&marker)?;}
            ensure!(pending_workers()==0,"properties worker retained");
            if folder{fs::remove_dir(&path)?;}else{fs::remove_file(&path)?;}
        }Ok(())
    }).join().map_err(|_|anyhow::anyhow!("property regression panicked")).and_then(|value|value);
    let closed=interactive||unsafe{CloseDesktop(desktop)}!=0;result?;ensure!(closed,"private desktop retained");Ok(())
}

#[test]
#[ignore="Native Win11 to classic handoff on an unswitched WinSta0 desktop; no clipboard command"]
fn native_show_more_options_opens_classic_menu()->Result<()>{
    let root=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("output")?).join("more-options");fs::create_dir_all(&root)?;
    ensure!(current_station_name()?.eq_ignore_ascii_case("WinSta0"),"popup tracking requires the interactive window station");
    let name=format!("ZSDesk.MoreOptions.{}",std::process::id());let desktop=unsafe{CreateDesktopW(wide(&name).as_ptr(),std::ptr::null(),std::ptr::null(),0,0x000f00ff,std::ptr::null())};ensure!(!desktop.is_null(),"private desktop");
    let raw=desktop as usize;let target=root.clone();let before=unsafe{GetForegroundWindow()};
    let result=thread::spawn(move||->Result<()>{
        ensure!(unsafe{SetThreadDesktop(raw as*mut c_void)}!=0,"private desktop attach");unsafe{OleInitialize(None)?;}
        struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}let _apartment=Apartment;let _pool=PoolCleanup;
        let _station=Environment::set("ZSDESK_MENU_TEST_STATION","WinSta0");let _desktop=Environment::set("ZSDESK_MENU_TEST_DESKTOP",&name);
        let _root=Environment::set("ZSDESK_MENU_ROOT",target.join("workers"));let _probe=Environment::set("ZSDESK_MENU_TEST_PROBE","modern-expand");
        let file=target.join("selected.txt");fs::write(&file,b"more options fixture")?;
        let module=HINSTANCE(unsafe{GetModuleHandleW(None)?}.0);let class=w!("ZSDesk.MoreOptions.Parent");
        unsafe{RegisterClassW(&WNDCLASSW{hInstance:module,lpfnWndProc:Some(owner_proc),lpszClassName:class,..Default::default()});}
        let owner=Owner(unsafe{CreateWindowExW(WS_EX_NOACTIVATE,class,w!("More options regression"),WS_OVERLAPPEDWINDOW,20,20,640,480,None,None,Some(module),None)}?);let _=unsafe{ShowWindow(owner.0,SW_SHOWNOACTIVATE)};
        for background in [false,true]{
            let _probe=Environment::set("ZSDESK_MENU_TEST_PROBE",if background{"modern-expand"}else{"modern-expand-copy"});
            let path=if background{target.clone()}else{file.clone()};let item=zsdesk_windows::shell::pidl::location_pidl(&zsdesk_core::model::explorer::ShellLocation::Path(path.clone())).context("target PIDL")?;
            let mut client=Client::spawn(owner.0,POINT{x:70,y:100},vec![item],background.then_some(path),ContextMenuOptions::default(),MenuInterceptions{file_actions:!background,..Default::default()},if background{Mode::Background}else{Mode::More})?;
            let(outcome,_)=complete(&mut client,owner.0)?;let status=client.last_status.as_ref().context("status")?;
            ensure!(matches!(&outcome,Ok(value)if *value==if background{MenuOutcome::Dismissed}else{MenuOutcome::Copy})&&status.probe_menu_open&&status.probe_classic_loop,"native/classic handoff failed: background={background}, result={outcome:?}, status={}",serde_json::to_string(status)?);
            drop(client);shutdown_idle_worker();let until=Instant::now()+Duration::from_secs(5);while pending_workers()>0&&Instant::now()<until{pump(10);}
            ensure!(pending_workers()==0,"expanded worker retained");
        }Ok(())
    }).join().map_err(|_|anyhow::anyhow!("more options regression panicked")).and_then(|result|result);
    let closed=unsafe{CloseDesktop(desktop)}!=0;let report=serde_json::json!({"state":if result.is_ok()&&closed{"passed"}else{"failed"},"checks":result.as_ref().map_err(|error|format!("{error:#}")),"desktop_closed":closed,"foreground_unchanged":before==unsafe{GetForegroundWindow()},"clipboard_commands_executed":false});
    fs::write(root.join("verification.current.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");result?;ensure!(closed,"desktop retained");Ok(())
}

#[test]
#[ignore="Two live parent STAs and private-desktop/root boundaries; requires ZSDESK_NATIVE_TEST_OUTPUT"]
fn native_menu_workers_reuse_across_parent_stas_without_crossing_boundaries()->Result<()>{
    use std::sync::mpsc;
    let root=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("output")?).join("cross-sta-workers");
    ensure!(root.is_absolute(),"absolute output required");fs::create_dir_all(&root)?;
    let first_folder=root.join("first 中文");let second_folder=root.join("second 中文");
    struct Directories(Vec<PathBuf>);impl Drop for Directories{fn drop(&mut self){for path in self.0.iter().rev(){let _=fs::remove_dir(path);}}}
    let mut _directories=Directories(Vec::new());
    for path in [&first_folder,&second_folder]{
        if path.exists(){let metadata=fs::symlink_metadata(path)?;ensure!(metadata.is_dir()&&metadata.file_attributes()&0x400==0,"existing fixture is not a regular directory");}
        else{fs::create_dir(path)?;_directories.0.push(path.clone());}
    }
    fn entries(path:&Path)->Result<std::collections::HashSet<PathBuf>>{
        Ok(fs::read_dir(path)?.map(|entry|entry.map(|entry|entry.path())).collect::<std::io::Result<_>>()?)
    }
    let first_before=entries(&first_folder)?;let second_before=entries(&second_folder)?;
    let _root=Environment::set("ZSDESK_MENU_ROOT",root.join("workers"));
    let _probe=Environment::set("ZSDESK_MENU_TEST_PROBE","catalog-only");
    let name=format!("ZSDesk.CrossSTA.{}",std::process::id());let other_name=format!("{name}.Other");
    let _desktop=Environment::set("ZSDESK_MENU_TEST_DESKTOP",&name);
    let desktop=unsafe{CreateDesktopW(wide(&name).as_ptr(),std::ptr::null(),std::ptr::null(),0,0x000f00ff,std::ptr::null())};ensure!(!desktop.is_null(),"private desktop");
    let other=unsafe{CreateDesktopW(wide(&other_name).as_ptr(),std::ptr::null(),std::ptr::null(),0,0x000f00ff,std::ptr::null())};
    if other.is_null(){unsafe{CloseDesktop(desktop)};anyhow::bail!("second private desktop");}
    let foreground=unsafe{GetForegroundWindow()};let raw=desktop as usize;let other_raw=other as usize;
    fn owner()->Result<Owner>{Ok(Owner(unsafe{CreateWindowExW(WS_EX_NOACTIVATE,w!("STATIC"),w!("Cross STA regression"),WS_POPUP,0,0,64,64,None,None,None,None)}?))}
    fn catalog(owner:HWND,folder:&Path)->Result<Client>{
        let pidl=zsdesk_windows::shell::pidl::location_pidl(&zsdesk_core::model::explorer::ShellLocation::Path(folder.into())).context("folder PIDL")?;
        Client::spawn(owner,POINT{x:10,y:10},vec![pidl],Some(folder.into()),ContextMenuOptions{modern:false,..Default::default()},MenuInterceptions::default(),Mode::CatalogBackground)
    }
    struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}
    let (first_tx,first_rx)=mpsc::sync_channel::<std::result::Result<(u32,u32,u32),String>>(1);
    let (control,first_control)=mpsc::channel::<u8>();let (cleaned,cleaned_rx)=mpsc::channel();
    let a=first_folder.clone();
    let first=thread::spawn(move||->Result<()>{
        let result=(||->Result<()>{
            ensure!(unsafe{SetThreadDesktop(raw as*mut c_void)}!=0,"first desktop");unsafe{OleInitialize(None)}?;let _apartment=Apartment;
            let owner=owner()?;let mut client=catalog(owner.0,&a)?;let (result,_)=complete(&mut client,owner.0)?;
            ensure!(matches!(result,Ok(MenuOutcome::Dismissed)),"first catalog failed: {result:?}");
            let ready=(client.pid().context("first worker")?,unsafe{windows::Win32::System::Threading::GetCurrentThreadId()},client.detailed_timings().worker_thread);
            drop(client);first_tx.send(Ok(ready))?;
            // The first STA keeps pumping while the other parent STAs run.
            loop{match first_control.try_recv(){
                Ok(1)=>{shutdown_idle_worker_for_thread();let _=cleaned.send(());},
                Ok(0)|Err(mpsc::TryRecvError::Disconnected)=>break,
                _=>pump(10),
            }}Ok(())
        })();if let Err(error)=&result{let _=first_tx.try_send(Err(format!("{error:#}")));}result
    });
    let mut second=None;
    let result=(||->Result<serde_json::Value>{
        let (first_pid,first_thread,worker_thread)=first_rx.recv_timeout(Duration::from_secs(30))?.map_err(anyhow::Error::msg)?;
        let (taken,taken_rx)=mpsc::sync_channel(1);let (continue_tx,continue_rx)=mpsc::sync_channel(1);
        let a=first_folder.clone();let b=second_folder.clone();
        second=Some(thread::spawn(move||->Result<serde_json::Value>{
            ensure!(unsafe{SetThreadDesktop(raw as*mut c_void)}!=0,"second desktop");unsafe{OleInitialize(None)}?;let _apartment=Apartment;
            let _probe=Environment::set("ZSDESK_MENU_TEST_PROBE","new-folder");let owner=owner()?;
            let pidl=zsdesk_windows::shell::pidl::location_pidl(&zsdesk_core::model::explorer::ShellLocation::Path(b.clone())).context("second folder PIDL")?;
            let mut client=Client::spawn(owner.0,POINT{x:10,y:10},vec![pidl],Some(b.clone()),ContextMenuOptions{modern:false,..Default::default()},MenuInterceptions::default(),Mode::New)?;
            let second_thread=unsafe{windows::Win32::System::Threading::GetCurrentThreadId()};
            taken.send((client.pid().context("second worker")?,second_thread,client.detailed_timings().reused_process))?;
            continue_rx.recv_timeout(Duration::from_secs(10))?;
            let (result,_)=complete(&mut client,owner.0)?;ensure!(matches!(result,Ok(MenuOutcome::ShellInvoked)),"second operation failed: {result:?}");
            ensure!(client.detailed_timings().worker_thread==worker_thread,"child STA changed during reuse");
            let created=&client.last_status.as_ref().context("second status")?.created;
            let second_entries=entries(&b)?;let added:Vec<_>=second_entries.difference(&second_before).cloned().collect();
            eprintln!("cross-STA creation: expected_parent={b:?}, callbacks={created:?}, initial_entries={second_before:?}, added_entries={added:?}");
            ensure!(added.len()==1&&added[0].is_dir(),"second request did not create exactly one new directory: {added:?}");
            ensure!(!created.is_empty()&&created.iter().all(|item|item.path.parent().is_some_and(|parent|crate::explorer_forwarder::same_path(parent,&b))
                &&crate::explorer_forwarder::same_path(&item.path,&added[0])&&item.path.is_dir()),
                "creation notification does not match the actual new result: expected_parent={b:?}, callbacks={created:?}, added_entries={added:?}");
            ensure!(entries(&a)?==first_before,"first folder was modified by second request");
            // Earlier failed fixtures belong to the baseline and are never
            // removed by this run. Only the one verified newly created folder
            // qualifies for this test's ordinary teardown.
            let cleanup=fs::remove_dir(&added[0]);
            Ok(serde_json::json!({"parent_thread":second_thread,"worker_sta":worker_thread,"fresh_folder_target":true,"reused_process":true,
                "creation_callbacks":created.len(),"unique_created_directories":1,"baseline_preserved":true,
                "new_fixture_removed":cleanup.is_ok(),"new_fixture_cleanup_error":cleanup.err().map(|error|error.to_string())}))
        }));
        let (second_pid,second_thread,reused)=taken_rx.recv_timeout(Duration::from_secs(30))?;
        ensure!(first_thread!=second_thread&&first_pid==second_pid&&reused,"second parent STA did not reuse a warm worker");
        control.send(1)?;cleaned_rx.recv_timeout(Duration::from_secs(2))?;continue_tx.send(())?;
        let second_result=second.take().unwrap().join().map_err(|_|anyhow::anyhow!("second STA panicked"))??;
        control.send(1)?;cleaned_rx.recv_timeout(Duration::from_secs(2))?;
        ensure!(IDLE.lock().unwrap().iter().any(|idle|idle.thread==second_thread&&idle.child.as_ref().is_some_and(|child|child.id()==first_pid)),"old owner cleanup retired a transferred idle worker");
        let mut isolated=Vec::new();
        for (on_other_desktop,workspace) in [(true,root.join("workers")),(false,root.join("other-workers"))]{
            let _desktop=Environment::set("ZSDESK_MENU_TEST_DESKTOP",if on_other_desktop{&other_name}else{&name});
            let _workspace=Environment::set("ZSDESK_MENU_ROOT",&workspace);let a=first_folder.clone();
            let handle=if on_other_desktop{other_raw}else{raw};
            let pid=thread::spawn(move||->Result<u32>{
                ensure!(unsafe{SetThreadDesktop(handle as*mut c_void)}!=0,"boundary desktop");unsafe{OleInitialize(None)}?;let _apartment=Apartment;
                let owner=owner()?;let mut client=catalog(owner.0,&a)?;
                ensure!(!client.detailed_timings().reused_process,"worker crossed a desktop/workspace boundary");
                let (result,_)=complete(&mut client,owner.0)?;ensure!(matches!(result,Ok(MenuOutcome::Dismissed)),"boundary catalog failed");
                Ok(client.pid().context("boundary worker")?)
            }).join().map_err(|_|anyhow::anyhow!("boundary STA panicked"))??;
            ensure!(pid!=first_pid,"worker crossed a boundary");isolated.push(pid);
        }
        Ok(serde_json::json!({"first_parent_thread":first_thread,"first_worker":first_pid,"second":second_result,"boundary_worker_pids":isolated,
            "old_owner_cleanup_preserved_new_owner":true,"desktop_and_workspace_boundaries":true,"all_commands_populated":true}))
    })();
    if let Some(second)=second{let _=second.join();}
    let _=control.send(0);let first_finished=first.join().map_err(|_|anyhow::anyhow!("first STA panicked")).and_then(|value|value);
    shutdown_idle_worker();let deadline=Instant::now()+Duration::from_secs(8);while pending_workers()>0&&Instant::now()<deadline{pump(10);}
    let retired=pending_workers()==0;
    let closed=unsafe{CloseDesktop(desktop)}!=0;let other_closed=unsafe{CloseDesktop(other)}!=0;
    let report=serde_json::json!({"state":if result.is_ok()&&first_finished.is_ok()&&retired&&closed&&other_closed{"passed"}else{"failed"},
        "checks":result.as_ref().map_err(|error|format!("{error:#}")),"first_sta_finished":first_finished.is_ok(),"workers_retired":retired,
        "desktops_closed":closed&&other_closed,"foreground_unchanged":foreground==unsafe{GetForegroundWindow()}});
    fs::write(root.join("verification.current.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");
    result?;first_finished?;ensure!(retired&&closed&&other_closed,"cross-STA cleanup failed");Ok(())
}
