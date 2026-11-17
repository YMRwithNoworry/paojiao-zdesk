//! Restricted launcher for the hidden Shell menu worker.
//! Dropping a child closes its handle; operation lifetime remains with the caller.
use std::{cmp::Ordering,ffi::{OsStr,OsString},fs::File,io,os::windows::{ffi::OsStrExt,io::{AsRawHandle,FromRawHandle,OwnedHandle,RawHandle},process::ExitStatusExt},path::Path,process::ExitStatus};
use windows::{core::{PCWSTR,PWSTR},Win32::{Foundation::{DuplicateHandle,DUPLICATE_SAME_ACCESS,HANDLE,WAIT_OBJECT_0,WAIT_TIMEOUT},
    Globalization::{CompareStringOrdinal,CSTR_LESS_THAN,CSTR_GREATER_THAN},
    System::{Environment::{GetEnvironmentStringsW,FreeEnvironmentStringsW},Threading::*}}};

pub(super) struct OwnedChild {process:OwnedHandle,pid:u32,exit:Option<ExitStatus>}
impl AsRawHandle for OwnedChild {fn as_raw_handle(&self)->RawHandle{self.process.as_raw_handle()}}
impl OwnedChild {
    pub(super) fn id(&self)->u32{self.pid}
    fn poll(&mut self,timeout:u32)->io::Result<Option<ExitStatus>>{
        if self.exit.is_some(){return Ok(self.exit);}
        match unsafe{WaitForSingleObject(HANDLE(self.as_raw_handle()),timeout)}{
            WAIT_OBJECT_0=>{
                let mut code=0;unsafe{GetExitCodeProcess(HANDLE(self.as_raw_handle()),&mut code)}.map_err(io::Error::other)?;
                self.exit=Some(ExitStatus::from_raw(code));Ok(self.exit)
            }
            WAIT_TIMEOUT=>Ok(None),
            _=>Err(io::Error::last_os_error()),
        }
    }
    pub(super) fn try_wait(&mut self)->io::Result<Option<ExitStatus>>{self.poll(0)}
    pub(super) fn wait(&mut self)->io::Result<ExitStatus>{self.poll(INFINITE)?.ok_or_else(||io::Error::other("worker wait did not signal"))}
    pub(super) fn kill(&mut self)->io::Result<()>{
        if self.exit.is_some(){return Ok(());}
        match unsafe{TerminateProcess(HANDLE(self.as_raw_handle()),1)}{
            Ok(())=>Ok(()),
            Err(error)=>if self.try_wait()?.is_some(){Ok(())}else{Err(io::Error::other(error))},
        }
    }
}

fn encoded(value:&OsStr)->io::Result<Vec<u16>>{
    let units:Vec<_>=value.encode_wide().collect();
    if units.contains(&0){return Err(io::Error::new(io::ErrorKind::InvalidInput,"worker argument contains NUL"));}
    Ok(units)
}
fn append_argument(command:&mut Vec<u16>,units:&[u16]){
    // Always quote. Before a quote, double backslashes and escape the quote;
    // before the closing quote, double trailing backslashes.
    command.push(b'"' as u16);let mut slashes=0;
    for &unit in units{
        if unit==b'\\' as u16{slashes+=1;continue;}
        command.extend(std::iter::repeat_n(b'\\' as u16,if unit==b'"' as u16{slashes*2+1}else{slashes}));
        command.push(unit);slashes=0;
    }
    command.extend(std::iter::repeat_n(b'\\' as u16,slashes*2));command.push(b'"' as u16);
}
fn command_line(executable:&Path,args:&[OsString])->io::Result<(Vec<u16>,Vec<u16>)>{
    if !executable.is_absolute(){return Err(io::Error::new(io::ErrorKind::InvalidInput,"worker executable must be absolute"));}
    let mut name=encoded(executable.as_os_str())?;
    if name.contains(&(b'"' as u16)){return Err(io::Error::new(io::ErrorKind::InvalidInput,"worker executable contains quote"));}
    let mut command=Vec::new();append_argument(&mut command,&name);
    for arg in args{command.push(b' ' as u16);append_argument(&mut command,&encoded(arg)?);}
    command.push(0);name.push(0);
    if command.len()>32767{return Err(io::Error::new(io::ErrorKind::InvalidInput,"worker command line exceeds Windows limit"));}
    Ok((name,command))
}
fn environment_name(entry:&[u16])->&[u16]{
    // Preserve Windows' hidden per-drive entries such as =C:=C:\work.
    let separator=entry.iter().enumerate().skip(1).find(|(_,unit)|**unit==b'=' as u16).map(|(index,_)|index).unwrap_or(entry.len());
    &entry[..separator]
}
fn ordinal(left:&[u16],right:&[u16])->Ordering{
    match unsafe{CompareStringOrdinal(left,right,true)}{
        CSTR_LESS_THAN=>Ordering::Less,CSTR_GREATER_THAN=>Ordering::Greater,_=>Ordering::Equal,
    }
}
fn merge_environment(mut entries:Vec<Vec<u16>>,workspace:&Path,session:&str)->io::Result<Vec<u16>>{
    for (name,value) in [("ZSDESK_MENU_WORKSPACE",workspace.as_os_str()),("ZSDESK_MENU_SESSION",OsStr::new(session))]{
        let key:Vec<_>=name.encode_utf16().collect();
        entries.retain(|entry|ordinal(environment_name(entry),&key)!=Ordering::Equal);
        let mut entry=key;entry.push(b'=' as u16);entry.extend(encoded(value)?);entries.push(entry);
    }
    entries.sort_by(|left,right|ordinal(environment_name(left),environment_name(right)));
    let mut block=Vec::new();for entry in entries{block.extend(entry);block.push(0);}block.push(0);Ok(block)
}
fn environment(workspace:&Path,session:&str)->io::Result<Vec<u16>>{
    struct Snapshot(PWSTR);impl Drop for Snapshot{fn drop(&mut self){let _=unsafe{FreeEnvironmentStringsW(PCWSTR(self.0.0))};}}
    let snapshot=Snapshot(unsafe{GetEnvironmentStringsW()});
    if snapshot.0.is_null(){return Err(io::Error::last_os_error());}
    let mut entries=Vec::new();let mut offset=0;
    // The OS owns the block and guarantees double-NUL termination.
    // Bound allocations if a process has an exceptionally large environment.
    const MAX_UNITS:usize=16*1024*1024;
    loop{
        if offset>=MAX_UNITS{return Err(io::Error::new(io::ErrorKind::InvalidInput,"worker environment exceeds limit"));}
        if unsafe{*snapshot.0.0.add(offset)}==0{break;}
        let begin=offset;
        while offset<MAX_UNITS&&unsafe{*snapshot.0.0.add(offset)}!=0{offset+=1;}
        if offset>=MAX_UNITS{return Err(io::Error::new(io::ErrorKind::InvalidInput,"worker environment exceeds limit"));}
        entries.push(unsafe{std::slice::from_raw_parts(snapshot.0.0.add(begin),offset-begin)}.to_vec());offset+=1;
    }
    merge_environment(entries,workspace,session)
}
fn inheritable(handle:RawHandle)->io::Result<OwnedHandle>{
    let mut duplicate=HANDLE::default();let process=unsafe{GetCurrentProcess()};
    unsafe{DuplicateHandle(process,HANDLE(handle),process,&mut duplicate,0,true,DUPLICATE_SAME_ACCESS)}.map_err(io::Error::other)?;
    Ok(unsafe{OwnedHandle::from_raw_handle(duplicate.0)})
}
struct AttributeList{_storage:Vec<usize>,raw:LPPROC_THREAD_ATTRIBUTE_LIST}
impl AttributeList{
    fn handles(handles:&[HANDLE])->io::Result<Self>{
        let mut bytes=0;let _=unsafe{InitializeProcThreadAttributeList(None,1,None,&mut bytes)};
        if bytes==0||bytes>65536{return Err(io::Error::other("invalid process attribute list size"));}
        let mut storage=vec![0usize;bytes.div_ceil(std::mem::size_of::<usize>())];
        let raw=LPPROC_THREAD_ATTRIBUTE_LIST(storage.as_mut_ptr().cast());
        unsafe{InitializeProcThreadAttributeList(Some(raw),1,None,&mut bytes)}.map_err(io::Error::other)?;
        let list=Self{_storage:storage,raw};
        unsafe{UpdateProcThreadAttribute(raw,0,PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,Some(handles.as_ptr().cast()),std::mem::size_of_val(handles),None,None)}.map_err(io::Error::other)?;
        Ok(list)
    }
}
impl Drop for AttributeList{fn drop(&mut self){unsafe{DeleteProcThreadAttributeList(self.raw);}}}

pub(super) fn spawn(executable:&Path,args:&[OsString],workspace:&Path,session:&str,stderr:&File)->io::Result<OwnedChild>{
    let (name,mut command)=command_line(executable,args)?;let environment=environment(workspace,session)?;
    let null_in=File::open("NUL")?;let null_out=File::options().write(true).open("NUL")?;
    let input=inheritable(null_in.as_raw_handle())?;let output=inheritable(null_out.as_raw_handle())?;let error=inheritable(stderr.as_raw_handle())?;
    let handles=[HANDLE(input.as_raw_handle()),HANDLE(output.as_raw_handle()),HANDLE(error.as_raw_handle())];
    let attributes=AttributeList::handles(&handles)?;
    #[cfg(test)]let mut test_desktop=if let Some(station)=std::env::var_os("ZSDESK_MENU_TEST_STATION"){
        let desktop=std::env::var_os("ZSDESK_MENU_TEST_DESKTOP").ok_or_else(||io::Error::new(io::ErrorKind::InvalidInput,"test station requires a desktop"))?;
        let mut name=station;name.push("\\");name.push(desktop);let mut name=encoded(&name)?;name.push(0);Some(name)
    }else{None};
    let startup=STARTUPINFOEXW{StartupInfo:STARTUPINFOW{
        cb:std::mem::size_of::<STARTUPINFOEXW>() as u32,
        dwFlags:STARTF_FORCEOFFFEEDBACK|STARTF_USESTDHANDLES,
        #[cfg(test)]lpDesktop:test_desktop.as_mut().map_or(PWSTR::null(),|name|PWSTR(name.as_mut_ptr())),
        hStdInput:handles[0],hStdOutput:handles[1],hStdError:handles[2],..Default::default()
    },lpAttributeList:attributes.raw};
    let mut info=PROCESS_INFORMATION::default();
    unsafe{CreateProcessW(PCWSTR(name.as_ptr()),Some(PWSTR(command.as_mut_ptr())),None,None,true,
        CREATE_NO_WINDOW|CREATE_UNICODE_ENVIRONMENT|EXTENDED_STARTUPINFO_PRESENT,
        Some(environment.as_ptr().cast()),PCWSTR::null(),&startup.StartupInfo,&mut info)}.map_err(io::Error::other)?;
    // No fallible operation follows successful creation. Every returned handle
    // gains one RAII owner before the thread handle is closed.
    let child=OwnedChild{process:unsafe{OwnedHandle::from_raw_handle(info.hProcess.0)},pid:info.dwProcessId,exit:None};
    let _thread=unsafe{OwnedHandle::from_raw_handle(info.hThread.0)};Ok(child)
}

#[cfg(test)]
mod tests{
    use super::*;
    use anyhow::{Context,Result,ensure};
    use std::{fs,io::{Read,Write},os::windows::ffi::OsStringExt,time::{Duration,Instant}};

    #[test]fn worker_arguments_roundtrip_through_windows_parser()->Result<()>{
        use windows::Win32::{UI::Shell::CommandLineToArgvW,Foundation::{HLOCAL,LocalFree}};
        let args:Vec<_>=["", "中文 路径", "C:\\", "\\\\server\\share name\\", "quote\"middle", "one\\\\\"two", "line\nnext", "--flag"].map(OsString::from).into();
        let mut args=args;args.push(OsString::from_wide(&[0xd800,0x005c,0x0022,0xdc00]));
        let executable=Path::new(r"C:\Program Files\ZSDesk\ZSDesk.exe");let(_,command)=command_line(executable,&args)?;
        let mut count=0;let argv=unsafe{CommandLineToArgvW(PCWSTR(command.as_ptr()),&mut count)};
        ensure!(!argv.is_null(),"argument parser failed");
        struct Parsed(*mut PWSTR);impl Drop for Parsed{fn drop(&mut self){unsafe{LocalFree(Some(HLOCAL(self.0.cast())));}}}let _parsed=Parsed(argv);
        let actual:Vec<_>=unsafe{std::slice::from_raw_parts(argv,count as usize)}.iter().map(|value|OsString::from_wide(unsafe{value.as_wide()})).collect();
        let expected:Vec<_>=std::iter::once(executable.as_os_str().to_owned()).chain(args).collect();assert_eq!(actual,expected);
        assert!(command_line(executable,&[OsString::from_wide(&[0])]).is_err());
        assert!(command_line(executable,&[OsString::from("x".repeat(32767))]).is_err());Ok(())
    }
    #[test]fn worker_environment_overrides_only_its_two_variables()->Result<()>{
        let entries=["=C:=C:\\工作", "SystemRoot=C:\\Windows", "zsdesk_menu_workspace=old", "ZSDESK_MENU_SESSION=old", "PATH=C:\\工具", "Unrelated=value=with=equals"]
            .map(|entry|entry.encode_utf16().collect()).into();
        let value=merge_environment(entries,Path::new(r"E:\中文 空格\"),"new\"session")?;
        assert_eq!(&value[value.len()-2..],&[0,0]);
        let rows:Vec<_>=value.split(|unit|*unit==0).filter(|row|!row.is_empty()).map(String::from_utf16_lossy).collect();
        assert_eq!(rows.len(),6);assert!(rows.contains(&"=C:=C:\\工作".into()));assert!(rows.contains(&"Unrelated=value=with=equals".into()));
        assert!(rows.contains(&"ZSDESK_MENU_WORKSPACE=E:\\中文 空格\\".into()));assert!(rows.contains(&"ZSDESK_MENU_SESSION=new\"session".into()));
        assert!(merge_environment(Vec::new(),Path::new(r"E:\work"),"bad\0session").is_err());Ok(())
    }

    const ENTRY:&str="context_menu_process::child::tests::native_hidden_menu_child_entry";
    const SESSION:&str="ZSDesk child protocol 中文 \"quoted\" trailing\\";
    fn extra_args()->Vec<OsString>{["", "中文 空格", "C:\\", "\\\\server\\share name\\", "quote\"middle", "one\\\\\"two"].map(OsString::from).into()}
    #[test]#[ignore="Native hidden worker launch contract; creates an owned child and fixture"]
    fn native_hidden_menu_child_startup_contract()->Result<()>{
        let root=std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").map(std::path::PathBuf::from).context("ZSDESK_NATIVE_TEST_OUTPUT required")?;
        ensure!(root.is_absolute()&&root.is_dir(),"test root must be an existing absolute directory");
        let path=root.join(format!("hidden-worker-{}-{} 中文",std::process::id(),super::super::SERIAL.fetch_add(1,std::sync::atomic::Ordering::Relaxed)));fs::create_dir(&path)?;
        struct Fixture(std::path::PathBuf);impl Drop for Fixture{fn drop(&mut self){let _=fs::remove_file(self.0.join("result.json"));let _=fs::remove_file(self.0.join("stderr.log"));let _=fs::remove_dir(&self.0);}}
        let _fixture=Fixture(path.clone());
        let stderr=File::create(path.join("stderr.log"))?;
        let workspace_before=std::env::var_os("ZSDESK_MENU_WORKSPACE");let session_before=std::env::var_os("ZSDESK_MENU_SESSION");
        let unrelated=inheritable(unsafe{GetCurrentProcess()}.0)?;
        let mut args:Vec<OsString>=[ENTRY,"--exact","--ignored","--nocapture"].map(OsString::from).into();args.extend(extra_args());
        args.push(format!("unlisted-process-{}-{}",unrelated.as_raw_handle()as usize,std::process::id()).into());
        let child=spawn(&std::env::current_exe()?,&args,&path,SESSION,&stderr)?;
        struct Running(OwnedChild);impl Drop for Running{fn drop(&mut self){if self.0.try_wait().ok().flatten().is_none(){let _=self.0.kill();let _=self.0.wait();}}}let mut running=Running(child);
        let pid=running.0.id();ensure!(pid!=0&&pid!=std::process::id(),"invalid child PID");
        let deadline=Instant::now()+Duration::from_secs(15);
        while running.0.try_wait()?.is_none(){ensure!(Instant::now()<deadline,"hidden child timed out");std::thread::sleep(Duration::from_millis(10));}
        let status=running.0.wait()?;ensure!(status.success(),"hidden child exited {status}: {}",fs::read_to_string(path.join("stderr.log")).unwrap_or_default());
        assert_eq!(running.0.try_wait()?,Some(status));
        let result:serde_json::Value=serde_json::from_slice(&fs::read(path.join("result.json"))?)?;
        assert_eq!(result["pid"].as_u64(),Some(u64::from(pid)));assert_eq!(result["session"],SESSION);
        assert_eq!(result["system_root"].as_str(),std::env::var("SystemRoot").ok().as_deref());
        assert!(result["force_off_feedback"].as_bool()==Some(true)&&result["force_on_feedback"].as_bool()==Some(false));
        assert_eq!(result["unlisted_parent_inherited"],false);assert_eq!(result["stdin_eof"],true);assert_eq!(result["uses_standard_handles"],true);
        let expected:Vec<Vec<u16>>=args.iter().map(|arg|arg.encode_wide().collect()).collect();assert_eq!(result["arguments"],serde_json::to_value(expected)?);
        assert!(fs::read_to_string(path.join("stderr.log"))?.contains("ZSDesk hidden worker stderr reached parent file"));
        assert_eq!(std::env::var_os("ZSDESK_MENU_WORKSPACE"),workspace_before);assert_eq!(std::env::var_os("ZSDESK_MENU_SESSION"),session_before);
        let invalid=path.join("not-present.exe");assert!(spawn(&invalid,&[],&path,SESSION,&stderr).is_err());
        // Exit code 259 is a valid final code, even though GetExitCodeProcess
        // also uses that value for STILL_ACTIVE before a process is signaled.
        let mut exit_args=args.clone();exit_args.insert(4,"requested-exit-259".into());
        let mut exiting=Running(spawn(&std::env::current_exe()?,&exit_args,&path,SESSION,&stderr)?);
        let deadline=Instant::now()+Duration::from_secs(15);
        while exiting.0.try_wait()?.is_none(){ensure!(Instant::now()<deadline,"exit-code child timed out");std::thread::sleep(Duration::from_millis(10));}
        assert_eq!(exiting.0.wait()?.code(),Some(259));
        // Closing OwnedChild must not terminate a committed operation. Keep a
        // second exact process handle only so this test can retire its fixture.
        fs::remove_file(path.join("result.json"))?;
        let mut wait_args=args;wait_args.insert(4,"wait-for-termination".into());
        let mut waiting=spawn(&std::env::current_exe()?,&wait_args,&path,SESSION,&stderr)?;
        let duplicate=match waiting.process.try_clone(){Ok(handle)=>handle,Err(error)=>{let _=waiting.kill();let _=waiting.wait();return Err(error.into());}};
        let mut keeper=Running(OwnedChild{process:duplicate,pid:waiting.pid,exit:None});drop(waiting);
        let deadline=Instant::now()+Duration::from_secs(15);
        while !path.join("result.json").exists(){ensure!(Instant::now()<deadline&&keeper.0.try_wait()?.is_none(),"child did not survive handle Drop");std::thread::sleep(Duration::from_millis(10));}
        ensure!(keeper.0.try_wait()?.is_none(),"dropping child terminated process");keeper.0.kill()?;assert_eq!(keeper.0.wait()?.code(),Some(1));
        println!("{}",serde_json::json!({"force_off_feedback":true,"handle_whitelist_verified":true,"utf16_arguments_verified":true,"environment_preserved":true,"stderr_redirected":true,"exit_code_259_verified":true,"drop_does_not_kill":true}));
        Ok(())
    }
    #[test]#[ignore="Internal child entry for hidden worker startup contract"]
    fn native_hidden_menu_child_entry()->Result<()>{
        let root=std::env::var_os("ZSDESK_MENU_WORKSPACE").map(std::path::PathBuf::from).context("workspace")?;
        ensure!(std::env::var("ZSDESK_MENU_SESSION")?==SESSION,"unexpected child session");
        let mut startup=STARTUPINFOW::default();unsafe{GetStartupInfoW(&mut startup);}
        let arguments:Vec<_>=std::env::args_os().skip(1).collect();
        let marker=arguments.last().context("unlisted handle argument")?.to_string_lossy();
        let (raw,parent)=marker.strip_prefix("unlisted-process-").context("unlisted handle marker")?.split_once('-').context("unlisted process identity")?;
        let raw=raw.parse::<usize>()?;let parent=parent.parse::<u32>()?;
        let inherited_parent=unsafe{GetProcessId(HANDLE(raw as *mut _))};
        let mut input=[0u8;1];let stdin_eof=std::io::stdin().read(&mut input)?==0;
        std::io::stdout().write_all(b"discarded worker stdout\n")?;
        eprintln!("ZSDesk hidden worker stderr reached parent file");
        let value=serde_json::json!({"pid":std::process::id(),"arguments":arguments.iter().map(|arg|arg.encode_wide().collect::<Vec<_>>()).collect::<Vec<_>>(),
            "session":std::env::var("ZSDESK_MENU_SESSION")?,"system_root":std::env::var("SystemRoot").ok(),
            "force_off_feedback":startup.dwFlags.contains(STARTF_FORCEOFFFEEDBACK),"force_on_feedback":startup.dwFlags.contains(STARTF_FORCEONFEEDBACK),"uses_standard_handles":startup.dwFlags.contains(STARTF_USESTDHANDLES),
            "stdin_eof":stdin_eof,"unlisted_parent_inherited":parent==inherited_parent});
        fs::write(root.join("result.json"),serde_json::to_vec(&value)?)?;
        if arguments.iter().any(|arg|arg=="requested-exit-259"){std::process::exit(259);}
        if arguments.iter().any(|arg|arg=="wait-for-termination"){std::thread::sleep(Duration::from_secs(30));}
        Ok(())
    }
}
