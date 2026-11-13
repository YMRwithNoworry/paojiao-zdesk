//! Discovery and lifetime of the optional external or bundled Everything host.
use std::{fs,io::Read,os::windows::process::CommandExt,path::{Path,PathBuf},process::{Child,Command,Stdio},thread,time::{Duration,Instant}};
use anyhow::{bail,ensure,Context,Result};
use serde::{Deserialize,Serialize};
use sha2::{Digest,Sha256};
use windows::{core::{w,PCWSTR,PWSTR},Win32::{Foundation::{CloseHandle,GetLastError,FILETIME,HANDLE,HWND,LPARAM,WPARAM,WAIT_ABANDONED,WAIT_OBJECT_0,WAIT_TIMEOUT},System::{Registry::*,Threading::{CreateMutexW,GetProcessTimes,OpenProcess,QueryFullProcessImageNameW,ReleaseMutex,WaitForSingleObject,PROCESS_NAME_WIN32,PROCESS_QUERY_LIMITED_INFORMATION,PROCESS_SYNCHRONIZE}},UI::{Shell::{IsUserAnAdmin,ShellExecuteW},WindowsAndMessaging::*}}};

const MANIFEST:&str=include_str!("../../../../packaging/engines/everything.json");
const ROOT_CLASS:PCWSTR=w!("ZSDesk.Native.Runtime");
const DEFAULT_SERVICE_PIPE:&str=r"\\.\PIPE\Everything Service";
const OWN_SERVICE_PIPE:&str=r"\\.\PIPE\ZSdesk Everything Service";
const SERVICE_INSTANCE:&str="ZSdesk";
#[link(name="kernel32")]
unsafe extern "system"{
    fn WaitNamedPipeW(name:*const u16,timeout:u32)->i32;
    fn ProcessIdToSessionId(process:u32,session:*mut u32)->i32;
}
fn wide(value:&str)->Vec<u16>{value.encode_utf16().chain(Some(0)).collect()}
struct Handle(HANDLE);
impl Drop for Handle{fn drop(&mut self){let _=unsafe{CloseHandle(self.0)};}}
struct StartLock(Handle);
impl Drop for StartLock{fn drop(&mut self){let _=unsafe{ReleaseMutex(self.0.0)};}}
struct StartingHost{child:Child,recorded:bool}
impl Drop for StartingHost{
    fn drop(&mut self){if !self.recorded{let _=self.child.kill();let _=self.child.wait();}}
}
#[derive(Clone,Debug,Serialize,Deserialize,PartialEq,Eq)]
struct Identity{pid:u32,created:u64,executable:PathBuf}
#[derive(Serialize,Deserialize)]
struct OwnedHost{schema:u32,instance:String,identity:Identity,executable_sha256:String}
#[derive(Clone)]
pub(super) struct Connection{pub hwnd:HWND,pub executable:PathBuf,pub instance:Option<String>,pub managed:bool}

fn identity_pid(pid:u32)->Option<Identity>{
    if pid==0{return None;}
    let process=Handle(unsafe{OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION,false,pid)}.ok()?);
    let mut buffer=vec![0u16;32768];let mut length=buffer.len()as u32;
    unsafe{QueryFullProcessImageNameW(process.0,PROCESS_NAME_WIN32,PWSTR(buffer.as_mut_ptr()),&mut length)}.ok()?;
    let mut created=FILETIME::default();let mut exit=FILETIME::default();let mut kernel=FILETIME::default();let mut user=FILETIME::default();
    unsafe{GetProcessTimes(process.0,&mut created,&mut exit,&mut kernel,&mut user)}.ok()?;
    Some(Identity{pid,created:(u64::from(created.dwHighDateTime)<<32)|u64::from(created.dwLowDateTime),executable:PathBuf::from(String::from_utf16_lossy(&buffer[..length as usize]))})
}
fn identity(hwnd:HWND)->Option<Identity>{let mut pid=0;unsafe{GetWindowThreadProcessId(hwnd,Some(&mut pid));}identity_pid(pid)}
fn ordinary_everything(path:&Path)->bool{
    let name=path.file_name().unwrap_or_default().to_string_lossy();
    ["everything.exe","everything64.exe","everything32.exe"].iter().any(|known|name.eq_ignore_ascii_case(known))
}
fn same_path(a:&Path,b:&Path)->bool{crate::explorer_forwarder::same_path(a,b)}
fn class_name(instance:Option<&str>)->String{instance.map_or_else(||"EVERYTHING_TASKBAR_NOTIFICATION".into(),|name|format!("EVERYTHING_TASKBAR_NOTIFICATION_({name})"))}
fn find(instance:Option<&str>,expected:Option<&Path>)->Option<Connection>{
    let class=wide(&class_name(instance));let mut previous=None;
    for _ in 0..32{
        let window=unsafe{FindWindowExW(None,previous,PCWSTR(class.as_ptr()),None)}.ok()?;previous=Some(window);
        let Some(process)=identity(window)else{continue;};
        if !ordinary_everything(&process.executable)||expected.is_some_and(|path|!same_path(path,&process.executable)){continue;}
        return Some(Connection{hwnd:window,executable:process.executable,instance:instance.map(str::to_owned),managed:false});
    }None
}
fn session()->u32{let mut session=0;let _=unsafe{ProcessIdToSessionId(std::process::id(),&mut session)};session}
fn instance_name()->String{std::env::var("ZSDESK_EVERYTHING_INSTANCE").unwrap_or_else(|_|format!("ZSdesk-{}",session()))}
fn root()->Result<PathBuf>{
    let path=std::env::var_os("ZSDESK_EVERYTHING_DATA_ROOT").or_else(||std::env::var_os("ZSDESK_EVERYTHING_DATA")).map(PathBuf::from)
        .unwrap_or_else(||dirs::data_local_dir().unwrap_or_else(std::env::temp_dir).join("ZSdesk/Everything").join(format!("session-{}",session())));
    ensure!(path.is_absolute(),"Everything 索引目录必须是绝对路径");Ok(path)
}
fn external(configured:Option<&Path>)->Option<Connection>{
    if let Ok(instance)=std::env::var("ZSDESK_EVERYTHING_INSTANCE"){
        return find(Some(&instance),configured);
    }
    for instance in [None,Some("1.5a"),Some("1.5")]{if let Some(connection)=find(instance,configured){return Some(connection);}}
    // Automatically borrowing an arbitrary named instance can select a test
    // fixture or a different database. Only an explicitly chosen executable
    // permits discovering a user's custom named instance.
    configured?;
    let managed=class_name(Some(&instance_name()));let mut previous=None;
    while let Ok(window)=unsafe{FindWindowExW(None,previous,None,None)}{
        previous=Some(window);let mut buffer=[0u16;256];let length=unsafe{GetClassNameW(window,&mut buffer)};
        if length<=0{continue;}let class=String::from_utf16_lossy(&buffer[..length as usize]);
        if class==managed{continue;}
        let Some(instance)=class.strip_prefix("EVERYTHING_TASKBAR_NOTIFICATION_(").and_then(|name|name.strip_suffix(')'))else{continue;};
        if let Some(connection)=find(Some(instance),configured){return Some(connection);}
    }
    None
}
pub(super) fn running()->Option<HWND>{
    external(None).or_else(||find(Some(&instance_name()),None)).map(|connection|connection.hwnd)
}
fn registry_text(root:HKEY,key:&str,value:Option<&str>)->Option<String>{
    let key=wide(key);let value=value.map(wide);let name=value.as_ref().map_or(PCWSTR::null(),|v|PCWSTR(v.as_ptr()));
    for view in [RRF_SUBKEY_WOW6464KEY,RRF_SUBKEY_WOW6432KEY]{
        let mut buffer=vec![0u16;32768];let mut size=(buffer.len()*2)as u32;
        if unsafe{RegGetValueW(root,PCWSTR(key.as_ptr()),name,RRF_RT_REG_SZ|RRF_RT_REG_EXPAND_SZ|view,None,Some(buffer.as_mut_ptr().cast()),Some(&mut size))}.is_ok(){
            let length=buffer.iter().position(|v|*v==0).unwrap_or(buffer.len());return Some(String::from_utf16_lossy(&buffer[..length]));
        }
    }None
}
fn registry_path(root:HKEY,key:&str,value:Option<&str>)->Option<PathBuf>{
    registry_text(root,key,value).map(|value|PathBuf::from(value.trim_matches('"')))
}
pub(super) fn executable(configured:Option<&Path>)->Option<PathBuf>{
    if let Some(path)=configured.filter(|p|p.is_absolute()&&p.is_file()){return Some(path.into());}
    for hive in [HKEY_CURRENT_USER,HKEY_LOCAL_MACHINE]{
        if let Some(path)=registry_path(hive,r"Software\Microsoft\Windows\CurrentVersion\App Paths\Everything.exe",None).filter(|p|p.is_file()){return Some(path);}
        for key in [r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Everything",r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Everything 1.5a"]{
            if let Some(dir)=registry_path(hive,key,Some("InstallLocation")){if dir.join("Everything.exe").is_file(){return Some(dir.join("Everything.exe"));}}
        }
    }
    for variable in ["ProgramFiles","ProgramFiles(x86)"]{if let Some(dir)=std::env::var_os(variable){let path=PathBuf::from(dir).join("Everything/Everything.exe");if path.is_file(){return Some(path);}}}
    bundle().ok()
}
pub(super) fn ensure_executable(path:&Path)->Result<()>{ensure!(path.is_absolute()&&path.is_file()&&ordinary_everything(path),"请选择 Everything.exe 程序文件");Ok(())}
fn expected_hash()->Result<String>{let value:serde_json::Value=serde_json::from_str(MANIFEST)?;value["executable_sha256"].as_str().map(str::to_owned).context("Everything 发布清单缺少内容标识")}
fn digest(path:&Path)->Result<String>{
    let mut file=fs::File::open(path)?;let mut hash=Sha256::new();let mut buffer=[0u8;65536];
    loop{let count=file.read(&mut buffer)?;if count==0{break;}hash.update(&buffer[..count]);}Ok(format!("{:x}",hash.finalize()))
}
fn bundle()->Result<PathBuf>{
    let path=match std::env::var_os("ZSDESK_EVERYTHING_BUNDLE"){Some(path)=>PathBuf::from(path),None=>std::env::current_exe()?.parent().context("程序目录缺失")?.join("engines/everything/Everything.exe")};
    ensure_executable(&path).context("未找到随程序提供的 Everything 引擎")?;
    ensure!(digest(&path)?==expected_hash()?,"Everything 引擎与发布清单不一致，请修复安装");Ok(path)
}
fn lock(root:&Path,cancel:&impl Fn()->bool)->Result<StartLock>{
    let token=format!("{:x}",Sha256::digest(root.to_string_lossy().to_lowercase().as_bytes()));
    let name=wide(&format!("Local\\ZSdesk.Everything.Start.{token}"));let handle=Handle(unsafe{CreateMutexW(None,false,PCWSTR(name.as_ptr()))}?);
    let deadline=Instant::now()+Duration::from_secs(10);
    loop{if cancel(){bail!("搜索已取消");}
        let waited=unsafe{WaitForSingleObject(handle.0,50)};
        if waited==WAIT_OBJECT_0||waited==WAIT_ABANDONED{return Ok(StartLock(handle));}
        ensure!(waited==WAIT_TIMEOUT,"Everything 启动锁异常");ensure!(Instant::now()<deadline,"Everything 正在启动，请稍后重试");
    }
}
fn pipe_available(name:&str)->bool{
    // Zero uses the server's default wait; keep discovery bounded instead.
    if unsafe{WaitNamedPipeW(wide(name).as_ptr(),1)}!=0{return true;}
    // WaitNamedPipe reports ERROR_SEM_TIMEOUT when existing instances are busy.
    matches!(unsafe{GetLastError()}.0,121|231)
}

fn service_executable(command:&str)->Option<PathBuf>{
    let command=command.trim();
    let executable=if let Some(quoted)=command.strip_prefix('"'){
        let end=quoted.find('"')?;
        if quoted[end+1..].chars().next().is_some_and(|ch|!ch.is_whitespace()){return None;}
        &quoted[..end]
    }else{command.split_whitespace().next()?};
    let path=PathBuf::from(executable);
    (path.is_absolute()&&ordinary_everything(&path)).then_some(path)
}

fn version_value(bytes:&[u8],query:&str)->Option<String>{
    use windows::Win32::Storage::FileSystem::VerQueryValueW;
    let mut pointer=std::ptr::null_mut();let mut length=0;
    if !unsafe{VerQueryValueW(bytes.as_ptr().cast(),PCWSTR(wide(query).as_ptr()),&mut pointer,&mut length)}.as_bool()
        ||pointer.is_null()||length==0||length>65536{return None;}
    let start=pointer as usize;let end=start.checked_add(length as usize*2)?;
    if start<bytes.as_ptr()as usize||end>(bytes.as_ptr()as usize).checked_add(bytes.len())?{return None;}
    let units:Vec<u16>=(0..length as usize).map(|index|unsafe{pointer.cast::<u16>().add(index).read_unaligned()}).collect();
    let end=units.iter().position(|unit|*unit==0).unwrap_or(units.len());
    Some(String::from_utf16_lossy(&units[..end]))
}

fn compatible_service_metadata(major:u32,minor:u32,product:&str,company:&str)->bool{
    major==1&&minor==4&&product.trim().eq_ignore_ascii_case("Everything")
        &&["voidtools","voidtools Pty Ltd"].iter().any(|name|company.trim().eq_ignore_ascii_case(name))
}

fn compatible_service_executable(path:&Path)->bool{
    use windows::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW,GetFileVersionInfoW,VerQueryValueW,VS_FIXEDFILEINFO};
    let name=wide(&path.to_string_lossy());let size=unsafe{GetFileVersionInfoSizeW(PCWSTR(name.as_ptr()),None)};
    if size==0||size>2*1024*1024{return false;}
    let mut bytes=vec![0u8;size as usize];
    if unsafe{GetFileVersionInfoW(PCWSTR(name.as_ptr()),None,size,bytes.as_mut_ptr().cast())}.is_err(){return false;}
    let mut pointer=std::ptr::null_mut();let mut length=0;
    if !unsafe{VerQueryValueW(bytes.as_ptr().cast(),w!("\\"),&mut pointer,&mut length)}.as_bool()
        ||pointer.is_null()||length<std::mem::size_of::<VS_FIXEDFILEINFO>()as u32{return false;}
    let fixed_start=pointer as usize;
    if fixed_start<bytes.as_ptr()as usize||fixed_start.checked_add(std::mem::size_of::<VS_FIXEDFILEINFO>()).is_none_or(|end|end>bytes.as_ptr()as usize+bytes.len()){return false;}
    let fixed=unsafe{pointer.cast::<VS_FIXEDFILEINFO>().read_unaligned()};
    if fixed.dwSignature!=0xFEEF04BD||fixed.dwFileVersionMS>>16!=1||fixed.dwFileVersionMS&0xffff!=4{return false;}
    if !unsafe{VerQueryValueW(bytes.as_ptr().cast(),w!("\\VarFileInfo\\Translation"),&mut pointer,&mut length)}.as_bool()
        ||pointer.is_null()||length<4||length>4096{return false;}
    let start=pointer as usize;
    if start<bytes.as_ptr()as usize||start.checked_add(length as usize).is_none_or(|end|end>bytes.as_ptr()as usize+bytes.len()){return false;}
    for index in 0..length as usize/4{
        let language=unsafe{pointer.cast::<u16>().add(index*2).read_unaligned()};
        let codepage=unsafe{pointer.cast::<u16>().add(index*2+1).read_unaligned()};
        let prefix=format!("\\StringFileInfo\\{language:04x}{codepage:04x}");
        let product=version_value(&bytes,&format!("{prefix}\\ProductName"));
        let company=version_value(&bytes,&format!("{prefix}\\CompanyName"));
        if product.zip(company).is_some_and(|(product,company)|compatible_service_metadata(fixed.dwFileVersionMS>>16,fixed.dwFileVersionMS&0xffff,&product,&company)){return true;}
    }false
}

fn default_service_compatible()->bool{
    let Some(path)=registry_text(HKEY_LOCAL_MACHINE,r"SYSTEM\CurrentControlSet\Services\Everything",Some("ImagePath"))
        .as_deref().and_then(service_executable)else{return false;};
    compatible_service_executable(&path)
}
fn select_service_pipe(own:bool,default:bool,compatible:bool)->Option<&'static str>{
    if own{Some(OWN_SERVICE_PIPE)}else if default&&compatible{Some(DEFAULT_SERVICE_PIPE)}else{None}
}
fn service_pipe()->Option<&'static str>{
    if pipe_available(OWN_SERVICE_PIPE){return Some(OWN_SERVICE_PIPE);}
    let default=pipe_available(DEFAULT_SERVICE_PIPE);
    select_service_pipe(false,default,default&&default_service_compatible())
}
fn can_index()->bool{(unsafe{IsUserAnAdmin()}).as_bool()||service_pipe().is_some()}
fn explicit_folders(config:&Path)->bool{
    fs::metadata(config).is_ok_and(|meta|meta.len()<1024*1024)&&fs::read_to_string(config).is_ok_and(|text|text.lines().any(|line|line.trim().strip_prefix("folders=").is_some_and(|value|!value.trim_matches([' ','"']).is_empty())))
}
fn managed_config(text:&str,pipe:&str)->String{
    let mut lines=text.trim_start_matches('\u{feff}').lines().map(str::to_owned).collect::<Vec<_>>();
    // The supplied file belongs to the managed instance and is only written
    // before launch. Query scopes are IPC filters, never index mutations.
    for (key,value) in [("service_pipe_name",pipe),("run_in_background","1"),("show_tray_icon","0")]{
        let prefix=format!("{key}=");
        if let Some(line)=lines.iter_mut().find(|line|line.starts_with(&prefix)){*line=format!("{key}={value}");}
        else{lines.push(format!("{key}={value}"));}
    }
    // Correct the old misspelled defaults without replacing explicit choices.
    for (key,value) in [("fast_size_sort","1"),("fast_date_modified_sort","1")]{
        if !lines.iter().any(|line|line.starts_with(&format!("{key}="))){lines.push(format!("{key}={value}"));}
    }
    lines.join("\r\n")+"\r\n"
}
fn write_config(root:&Path)->Result<()>{
    let path=root.join("Everything.ini");
    fs::create_dir_all(root)?;
    let pipe=service_pipe().unwrap_or(OWN_SERVICE_PIPE);
    let original=match fs::read_to_string(&path){Ok(text)=>text,Err(error)if error.kind()==std::io::ErrorKind::NotFound=>String::new(),Err(error)=>return Err(error.into())};
    let initial="[Everything]\r\napp_data=0\r\nrun_as_admin=0\r\ncheck_for_updates_on_startup=0\r\nauto_include_fixed_volumes=1\r\nauto_include_removable_volumes=0\r\nauto_include_fixed_refs_volumes=0\r\nauto_include_removable_refs_volumes=0\r\nfolders=\r\nhttp_server_enabled=0\r\netp_server_enabled=0\r\nindex_size=1\r\nindex_date_modified=1\r\n";
    let text=managed_config(if original.is_empty(){initial}else{&original},pipe);
    if text!=original{crate::quick_search::control::atomic_write(&path,text.as_bytes())?;}
    Ok(())
}
fn command(path:&Path)->Command{let mut command=Command::new(path);command.creation_flags(0x08000000).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());command}
fn marker(root:&Path)->PathBuf{root.join("owned-host.json")}
fn owned(root:&Path,connection:&Connection)->bool{
    let Ok(bytes)=fs::read(marker(root))else{return false;};if bytes.len()>16384{return false;}
    let Ok(record)=serde_json::from_slice::<OwnedHost>(&bytes)else{return false;};
    record.schema==1&&record.instance==instance_name()&&identity(connection.hwnd).is_some_and(|actual|actual==record.identity)
        &&expected_hash().is_ok_and(|hash|hash==record.executable_sha256)
}
fn managed_connection(root:&Path)->Option<Connection>{let mut connection=find(Some(&instance_name()),None)?;connection.managed=owned(root,&connection);Some(connection)}
pub(super) fn ensure_running(configured:Option<&Path>,cancel:&impl Fn()->bool,allow_setup:bool)->Result<Connection>{
    if let Some(connection)=external(configured){return Ok(connection);}
    let root=root()?;let _lock=lock(&root,cancel)?;
    if let Some(connection)=external(configured){return Ok(connection);}
    if let Some(connection)=managed_connection(&root).filter(|connection|configured.is_none_or(|path|same_path(path,&connection.executable))){return Ok(connection);}
    if let Some(path)=configured{
        ensure_executable(path)?;
        let mut launch=command(path);if let Ok(instance)=std::env::var("ZSDESK_EVERYTHING_INSTANCE"){launch.arg("-instance").arg(instance);}
        launch.arg("-startup").spawn()?;
        let deadline=Instant::now()+Duration::from_secs(8);
        loop{if cancel(){bail!("搜索已取消");}if let Some(connection)=external(Some(path)){return Ok(connection);}ensure!(Instant::now()<deadline,"指定的 Everything 未就绪，请检查是否为支持 IPC 的版本");thread::sleep(Duration::from_millis(25));}
    }
    if fs::symlink_metadata(marker(&root)).is_ok(){let _=fs::remove_file(marker(&root));}
    let executable=bundle()?;
    if !allow_setup&&!can_index()&&!explicit_folders(&root.join("Everything.ini")){
        bail!("未发现兼容的 Everything 1.4 索引服务；请在“≡ → 搜索 → 启用快速索引服务…”中启用 ZSdesk 专用服务。版本不明或 1.5 的服务不会复用，也不会改用全盘文件夹扫描");
    }
    write_config(&root)?;
    let instance=instance_name();let mut launch=command(&executable);
    launch.arg("-instance").arg(&instance).arg("-config").arg(root.join("Everything.ini")).arg("-db").arg(root.join("Everything.db")).arg("-startup");
    // The pinned client reads the service pipe from its private INI. Keep
    // normal queries on the background IPC path; no search-window arguments.
    let mut starting=StartingHost{child:launch.spawn()?,recorded:false};let child_pid=starting.child.id();
    if let Some(identity)=identity_pid(child_pid).filter(|identity|same_path(&identity.executable,&executable)){
        let record=OwnedHost{schema:1,instance:instance.clone(),identity,executable_sha256:expected_hash()?};
        crate::quick_search::control::atomic_write(&marker(&root),&serde_json::to_vec_pretty(&record)?)?;
        starting.recorded=true;
    }
    let deadline=Instant::now()+Duration::from_secs(10);
    loop{
        if let Some(mut connection)=find(Some(&instance),Some(&executable)){
            let identity=identity(connection.hwnd).context("无法核对 Everything 进程身份")?;
            if identity.pid==child_pid{
                let record=OwnedHost{schema:1,instance:instance.clone(),identity,executable_sha256:expected_hash()?};
                crate::quick_search::control::atomic_write(&marker(&root),&serde_json::to_vec_pretty(&record)?)?;connection.managed=true;
                starting.recorded=true;
            }
            if cancel(){bail!("搜索已取消");}return Ok(connection);
        }
        if let Some(status)=starting.child.try_wait()?{bail!("Everything 引擎启动失败（{status}）");}
        // Finish ownership recording even if the initial query was cancelled.
        if Instant::now()>=deadline{
            // Only the process created by this startup attempt is stopped;
            // this cannot terminate an already-running user instance.
            let _=starting.child.kill();let _=starting.child.wait();
            bail!("Everything 引擎启动超时，请打开索引设置检查状态");
        }
        thread::sleep(Duration::from_millis(25));
    }
}
pub(super) fn ipc(window:HWND,code:usize,value:isize)->Result<usize>{
    let mut result=0;let sent=unsafe{SendMessageTimeoutW(window,WM_USER,WPARAM(code),LPARAM(value),SMTO_ABORTIFHUNG,1000,Some(&mut result))};
    ensure!(sent.0!=0,"Everything 暂时没有响应");Ok(result)
}
pub(super) fn wait_ready(connection:&Connection,cancel:&impl Fn()->bool,pending:&mut dyn FnMut())->Result<()>{
    if cancel(){bail!("搜索已取消");}
    // Check once per query before accepting an already-loaded empty database.
    // Resource/version checks must not repeat in the 50 ms readiness loop.
    if connection.managed&&ipc(connection.hwnd,403,0).unwrap_or(0)==0&&service_pipe().is_none()
        &&!root().is_ok_and(|root|explicit_folders(&root.join("Everything.ini"))){bail!("未发现兼容的 Everything 1.4 索引服务，请在“≡ → 搜索 → 启用快速索引服务…”中启用 ZSdesk 专用服务；版本不明或 1.5 的服务不会复用");}
    wait_for_ready(||Ok(ipc(connection.hwnd,401,0)?!=0),cancel,pending,Duration::from_secs(120))
}
fn wait_for_ready(mut loaded:impl FnMut()->Result<bool>,cancel:&impl Fn()->bool,pending:&mut dyn FnMut(),budget:Duration)->Result<()>{
    let deadline=Instant::now()+budget;let mut notified=false;
    loop{
        if cancel(){bail!("搜索已取消");}
        if loaded()?{return Ok(());}
        if !notified{pending();notified=true;}
        ensure!(Instant::now()<deadline,"Everything 文件名索引仍在准备中，请稍后重试");
        thread::sleep(Duration::from_millis(25));
    }
}
pub(super) fn configured(profile:&Path)->Option<PathBuf>{crate::storage::load(profile).ok().and_then(|session|session.ui.everything_executable)}
pub(super) fn open_settings(profile:&Path)->Result<()>{
    let selected=configured(profile);let connection=ensure_running(selected.as_deref(),&||false,true)?;
    // Official SDK: EVERYTHING_IPC_ID_TRAY_OPTIONS, sent to the notification
    // window. This avoids creating another search window or changing config.
    unsafe{PostMessageW(Some(connection.hwnd),WM_COMMAND,WPARAM(40005),LPARAM(0))}?;Ok(())
}
/// Called only from an explicit user action; Windows owns the UAC prompt.
pub(super) fn enable_service(_profile:&Path)->Result<()>{
    let executable=bundle()?;
    let arguments=format!("-instance {} -install-service -install-service-pipe-name {}",crate::quick_search::control::quote_argument(SERVICE_INSTANCE),crate::quick_search::control::quote_argument(OWN_SERVICE_PIPE));
    let result=unsafe{ShellExecuteW(None,w!("runas"),PCWSTR(wide(&executable.to_string_lossy()).as_ptr()),PCWSTR(wide(&arguments).as_ptr()),None,SW_SHOWNORMAL)};
    ensure!(result.0 as isize>32,"未能启用 Everything 服务；可在索引设置中重试");Ok(())
}
/// The host calls this after its own runtime window is gone. Other profiles and
/// editions share the same named indexer, so the last client releases it.
pub(super) fn shutdown_managed_if_unused()->Result<()>{
    if other_hosts(){return Ok(());}
    let root=root()?;let _lock=lock(&root,&||false)?;
    if other_hosts(){return Ok(());}
    let Some(connection)=managed_connection(&root).filter(|connection|connection.managed)else{return Ok(());};
    let expected=identity(connection.hwnd).context("Everything 进程已结束")?;
    if visible_client(expected.pid){return Ok(());}
    ensure!(digest(&expected.executable)?==expected_hash()?,"Everything 程序内容已变化，保留该进程");
    let process=Handle(unsafe{OpenProcess(PROCESS_SYNCHRONIZE|PROCESS_QUERY_LIMITED_INFORMATION,false,expected.pid)}?);
    ensure!(identity(connection.hwnd).as_ref()==Some(&expected),"Everything 进程身份发生变化");
    unsafe{PostMessageW(Some(connection.hwnd),WM_USER,WPARAM(4),LPARAM(0))}?;
    if unsafe{WaitForSingleObject(process.0,5000)}==WAIT_OBJECT_0{
        if let Ok(bytes)=fs::read(marker(&root)){if serde_json::from_slice::<OwnedHost>(&bytes).is_ok_and(|record|record.identity==expected){let _=fs::remove_file(marker(&root));}}
    }
    Ok(())
}
fn visible_client(process:u32)->bool{
    struct Find{process:u32,visible:bool}let mut find=Find{process,visible:false};
    unsafe extern "system" fn each(hwnd:HWND,param:LPARAM)->windows::core::BOOL{
        let find=unsafe{&mut*(param.0 as*mut Find)};let mut process=0;unsafe{GetWindowThreadProcessId(hwnd,Some(&mut process));}
        if process==find.process&&unsafe{IsWindowVisible(hwnd)}.as_bool(){
            let mut name=[0u16;96];let n=unsafe{GetClassNameW(hwnd,&mut name)}.max(0)as usize;
            let name=String::from_utf16_lossy(&name[..n]);if name=="EVERYTHING"||name=="#32770"{find.visible=true;return false.into();}
        }true.into()
    }
    let _=unsafe{EnumWindows(Some(each),LPARAM((&mut find as*mut Find)as isize))};find.visible
}
fn other_hosts()->bool{
    let mut previous=None;
    while let Ok(window)=unsafe{FindWindowExW(None,previous,ROOT_CLASS,None)}{
        previous=Some(window);let mut pid=0;unsafe{GetWindowThreadProcessId(window,Some(&mut pid));}
        if pid!=0&&pid!=std::process::id(){return true;}
    }false
}

#[cfg(test)]mod service_tests{
    use super::*;
    #[test]fn unread_database_never_becomes_a_successful_empty_snapshot(){
        use std::cell::Cell;
        let notifications=Cell::new(0);
        assert!(wait_for_ready(||Ok(false),&||false,&mut||notifications.set(notifications.get()+1),Duration::ZERO).is_err());
        assert_eq!(notifications.get(),1);
        assert!(wait_for_ready(||Err(anyhow::anyhow!("IPC unavailable")),&||false,&mut||panic!("IPC error is not indexing"),Duration::ZERO).is_err());
    }
    #[test]fn warm_database_has_no_wait_and_indexing_can_be_cancelled(){
        use std::cell::Cell;
        wait_for_ready(||Ok(true),&||false,&mut||panic!("ready database must not emit loading"),Duration::ZERO).unwrap();
        let cancelled=Cell::new(false);let probes=Cell::new(0);
        let error=wait_for_ready(||{probes.set(probes.get()+1);Ok(false)},&||cancelled.get(),&mut||cancelled.set(true),Duration::from_secs(1)).unwrap_err();
        assert_eq!(probes.get(),1);assert!(error.to_string().contains("取消"));
    }
    #[test]fn managed_startup_preserves_index_scope_and_user_sort_choices(){
        let original="\u{feff}[Everything]\r\nfolders=E:\\中文 范围\r\nauto_include_fixed_volumes=0\r\nservice_pipe_name=wrong\r\nfast_size_sort=0\r\nrun_in_background=0\r\n";
        let config=managed_config(original,OWN_SERVICE_PIPE);
        for line in ["folders=E:\\中文 范围","auto_include_fixed_volumes=0","fast_size_sort=0","run_in_background=1","show_tray_icon=0","fast_date_modified_sort=1"]{assert!(config.lines().any(|candidate|candidate==line),"{line}");}
        assert!(config.lines().any(|line|line==format!("service_pipe_name={OWN_SERVICE_PIPE}")));
        assert_eq!(config,managed_config(&config,OWN_SERVICE_PIPE));
    }
    #[test]fn dedicated_service_wins_and_unknown_default_is_never_selected(){
        assert_eq!(select_service_pipe(true,true,false),Some(OWN_SERVICE_PIPE));
        assert_eq!(select_service_pipe(true,true,true),Some(OWN_SERVICE_PIPE));
        assert_eq!(select_service_pipe(false,true,true),Some(DEFAULT_SERVICE_PIPE));
        assert_eq!(select_service_pipe(false,true,false),None);
        assert_eq!(select_service_pipe(false,false,true),None);
        assert!(compatible_service_metadata(1,4,"Everything","voidtools"));
        assert!(!compatible_service_metadata(1,5,"Everything","voidtools"));
        assert!(!compatible_service_metadata(1,4,"Another product","voidtools"));
        assert!(!compatible_service_metadata(1,4,"Everything","Unknown"));
    }
    #[test]fn service_image_path_requires_an_unambiguous_absolute_executable(){
        assert_eq!(service_executable(r#""C:\Program Files\Everything\Everything.exe" -svc"#),Some(PathBuf::from(r"C:\Program Files\Everything\Everything.exe")));
        assert_eq!(service_executable(r#""E:\应用\Everything64.exe" -svc"#),Some(PathBuf::from(r"E:\应用\Everything64.exe")));
        assert_eq!(service_executable(r"C:\Everything\Everything.exe -svc"),Some(PathBuf::from(r"C:\Everything\Everything.exe")));
        for invalid in [r"Everything.exe -svc",r"C:\Program Files\Everything\Everything.exe -svc",r#""C:\Everything\Everything.exe"suffix -svc"#,r"C:\Tools\other.exe -svc"]{
            assert!(service_executable(invalid).is_none(),"{invalid}");
        }
    }
    #[test]
    #[ignore="Requires ZSDESK_EVERYTHING_BUNDLE pointing at the official pinned 1.4 executable; read-only"]
    fn native_official_service_resource_is_compatible()->Result<()>{
        let executable=bundle()?;ensure!(compatible_service_executable(&executable),"pinned Everything resource was rejected");Ok(())
    }
}
