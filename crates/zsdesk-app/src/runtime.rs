//! One application host per profile. Each native UI keeps its own STA/thread.
use std::{cell::Cell,collections::VecDeque,ffi::OsString,path::{Path,PathBuf},sync::{mpsc,OnceLock},thread,time::{Duration,Instant}};
use anyhow::{bail,Context,Result};
use serde::{Deserialize,Serialize};
use windows::{core::{w,PCWSTR},Win32::{Foundation::{CloseHandle,GetLastError,HANDLE,HWND,LPARAM,LRESULT,WPARAM,ERROR_ALREADY_EXISTS},System::{DataExchange::COPYDATASTRUCT,LibraryLoader::GetModuleHandleW,Threading::CreateMutexW},UI::WindowsAndMessaging::*}};

const CLASS:PCWSTR=w!("ZSDesk.Native.Runtime");
const MAGIC:usize=0x5a535254;
const WAKE:u32=WM_APP+110;
const DONE:u32=WM_APP+111;
const POOL_WAKE:u32=WM_APP+112;
const MAX_PACKET:usize=128*1024;
const MAX_WINDOW_THREADS:usize=16;
const MAX_QUEUED_WINDOWS:usize=64;
static LOCAL:OnceLock<(PathBuf,isize)>=OnceLock::new();
#[derive(Serialize,Deserialize)]
struct Request{args:Vec<OsString>}
struct Ingress{profile:PathBuf,tx:mpsc::SyncSender<Request>,accepting:Cell<bool>}
struct Kernel(HANDLE);
impl Drop for Kernel{fn drop(&mut self){let _=unsafe{CloseHandle(self.0)};}}
struct HostWindow(HWND);
impl Drop for HostWindow{fn drop(&mut self){let _=unsafe{DestroyWindow(self.0)};}}
struct Apartment;
impl Drop for Apartment{fn drop(&mut self){unsafe{windows::Win32::System::Ole::OleUninitialize();}}}
fn wide(value:&str)->Vec<u16>{value.encode_utf16().chain(Some(0)).collect()}
fn property(profile:&Path)->Vec<u16>{wide(&format!("Local\\ZSDesk.Runtime.{}",crate::quick_search::control::key(profile)))}
fn find(profile:&Path)->Option<HWND>{
    let key=property(profile);let mut previous=None;
    while let Ok(hwnd)=unsafe{FindWindowExW(None,previous,CLASS,None)}{
        if unsafe{GetPropW(hwnd,PCWSTR(key.as_ptr()))}.0 as usize==1{return Some(hwnd);}previous=Some(hwnd);
    }None
}
fn send(hwnd:HWND,args:Vec<OsString>)->Result<()>{
    let bytes=serde_json::to_vec(&Request{args})?;if bytes.len()>MAX_PACKET{bail!("启动请求过长");}
    let mut pid=0;unsafe{GetWindowThreadProcessId(hwnd,Some(&mut pid));let _=AllowSetForegroundWindow(pid);}
    let data=COPYDATASTRUCT{dwData:MAGIC,cbData:bytes.len()as u32,lpData:bytes.as_ptr()as*mut _};let mut accepted=0;
    let sent=unsafe{SendMessageTimeoutW(hwnd,WM_COPYDATA,WPARAM(0),LPARAM(&data as*const _ as isize),SMTO_ABORTIFHUNG,2500,Some(&mut accepted))};
    if sent.0==0||accepted!=1{bail!("ZSdesk 暂时无法接收启动请求");}Ok(())
}
/// Product actions can reuse the host directly instead of starting a launcher.
pub fn enqueue_local(profile:&Path,mut args:Vec<OsString>)->Result<bool>{
    let Some((owned,window))=LOCAL.get()else{return Ok(false);};
    if !crate::explorer_forwarder::same_path(profile,owned){return Ok(false);}
    if !args.iter().any(|arg|arg=="--profile-dir"){
        args.extend(["--profile-dir".into(),profile.parent().context("缺少配置目录")?.as_os_str().to_owned()]);
    }
    send(HWND(*window as*mut _),args)?;Ok(true)
}
unsafe extern "system" fn procedure(hwnd:HWND,message:u32,wp:WPARAM,lp:LPARAM)->LRESULT{
    if message==WM_NCCREATE{
        let create=unsafe{&*(lp.0 as*const CREATESTRUCTW)};unsafe{SetWindowLongPtrW(hwnd,GWLP_USERDATA,create.lpCreateParams as isize);}return LRESULT(1);
    }
    if message==WM_COPYDATA&&lp.0!=0{
        let data=unsafe{&*(lp.0 as*const COPYDATASTRUCT)};
        let state=unsafe{GetWindowLongPtrW(hwnd,GWLP_USERDATA)}as*const Ingress;
        if state.is_null()||data.dwData!=MAGIC||data.cbData==0||data.cbData as usize>MAX_PACKET||data.lpData.is_null(){return LRESULT(0);}
        let state=unsafe{&*state};let bytes=unsafe{std::slice::from_raw_parts(data.lpData as*const u8,data.cbData as usize)};
        if !state.accepting.get(){return LRESULT(0);}
        if let Ok(request)=serde_json::from_slice::<Request>(bytes){
            if crate::app::runtime_profile(&request.args).ok().flatten().is_some_and(|p|crate::explorer_forwarder::same_path(&p,&state.profile))
                &&state.tx.try_send(request).is_ok(){let _=unsafe{PostMessageW(Some(hwnd),WAKE,WPARAM(0),LPARAM(0))};return LRESULT(1);}
        }return LRESULT(0);
    }
    if message==WM_CLOSE{return LRESULT(0);}
    unsafe{DefWindowProcW(hwnd,message,wp,lp)}
}
struct Worker{id:u64,kind:&'static str,ending:bool,thread:thread::JoinHandle<()>}
struct WindowJob{id:u64,args:Vec<OsString>}
enum PoolCommand{Run(WindowJob),Stop}
struct WindowWorker{
    id:u64,thread_id:Option<u32>,job:Option<u64>,requests:u64,ending:bool,
    commands:mpsc::SyncSender<PoolCommand>,thread:thread::JoinHandle<()>,
}
enum Notice{
    ComponentReturned(u64,Option<String>),
    PoolReady(u64,u32),
    WindowReturned(u64,u64,Option<String>),
    PoolReturned(u64,Option<String>),
}
fn notify(destination:isize,sender:&mpsc::Sender<Notice>,notice:Notice){
    let _=sender.send(notice);
    let _=unsafe{PostMessageW(Some(HWND(destination as*mut _)),DONE,WPARAM(0),LPARAM(0))};
}
fn wake_pool(worker:&WindowWorker)->Result<()>{
    if let Some(id)=worker.thread_id{
        unsafe{PostThreadMessageW(id,POOL_WAKE,WPARAM(0),LPARAM(0))}?;
    }
    // Before PoolReady the thread checks its already-populated queue itself.
    Ok(())
}
fn run_window_pool(id:u64,destination:isize,commands:mpsc::Receiver<PoolCommand>,sender:&mpsc::Sender<Notice>)->Result<()>{
    // Keep this STA alive across serial windows. Shell's per-thread menu
    // runtime then has a bounded lifetime instead of being recreated per open.
    unsafe{windows::Win32::System::Ole::OleInitialize(None)}?;let _apartment=Apartment;
    let mut message=MSG::default();
    let _=unsafe{PeekMessageW(&mut message,None,0,0,PM_NOREMOVE)};
    notify(destination,sender,Notice::PoolReady(id,unsafe{windows::Win32::System::Threading::GetCurrentThreadId()}));
    loop{
        loop{match commands.try_recv(){
            Ok(PoolCommand::Stop)=>return Ok(()),
            Ok(PoolCommand::Run(job))=>{
                trace(&format!("window-job-start {} pool={id}",job.id));
                let result=std::panic::catch_unwind(||crate::app::run_arguments(job.args,true));
                zsdesk_windows::flyout::release_thread_resources();
                trace(&format!("window-job-return {} pool={id}",job.id));
                let panicked=result.is_err();
                let error=match result{Ok(Ok(()))=>None,Ok(Err(error))=>Some(format!("{error:#}")),Err(_)=>Some("文件窗口线程异常退出".into())};
                notify(destination,sender,Notice::WindowReturned(id,job.id,error));
                if panicked{bail!("文件窗口线程状态不可复用");}
                // A destroyed window posts WM_QUIT for its nested message loop.
                // It must not end the next window started on this same STA.
                while unsafe{PeekMessageW(&mut message,None,WM_QUIT,WM_QUIT,PM_REMOVE)}.as_bool(){}
            }
            Err(mpsc::TryRecvError::Empty)=>break,
            Err(mpsc::TryRecvError::Disconnected)=>return Ok(()),
        }}
        // An idle thread still pumps COM/window messages, but consumes no CPU
        // and performs no disk polling while it waits for a new window job.
        let result=unsafe{GetMessageW(&mut message,None,0,0)}.0;
        if result<0{return Err(windows::core::Error::from_thread().into());}
        if result==0{continue;}
        if message.hwnd.0.is_null()&&message.message==POOL_WAKE{continue;}
        unsafe{let _=TranslateMessage(&message);DispatchMessageW(&message);}
    }
}
fn spawn_window_pool(id:u64,job:WindowJob,destination:isize,sender:mpsc::Sender<Notice>)->Result<WindowWorker>{
    let (commands,receiver)=mpsc::sync_channel(1);let job_id=job.id;
    commands.try_send(PoolCommand::Run(job)).map_err(|_|anyhow::anyhow!("文件窗口请求未入队"))?;
    let thread=thread::Builder::new().name(format!("zsdesk-window-sta-{id}")).spawn(move||{
        let result=std::panic::catch_unwind(||run_window_pool(id,destination,receiver,&sender));
        let error=match result{Ok(Ok(()))=>None,Ok(Err(error))=>Some(format!("{error:#}")),Err(_)=>Some("文件窗口线程池异常退出".into())};
        notify(destination,&sender,Notice::PoolReturned(id,error));
    })?;
    Ok(WindowWorker{id,thread_id:None,job:Some(job_id),requests:1,ending:false,commands,thread})
}
pub(crate) fn trace(stage:&str){
    if let Some(path)=std::env::var_os("ZSDESK_RUNTIME_TRACE"){
        let path=PathBuf::from(path);if !path.is_absolute(){return;}
        if let Ok(mut file)=std::fs::OpenOptions::new().create(true).append(true).open(path){
            use std::io::Write;let _=writeln!(file,"{} {} {stage}",std::process::id(),unsafe{windows::Win32::System::Threading::GetCurrentThreadId()});
        }
    }
}
fn status(profile:&Path,workers:&[Worker],pool:&[WindowWorker],queued:usize,error:Option<&str>,previous:&mut Option<Vec<u8>>){
    let mut components=workers.iter().filter(|worker|!worker.ending).map(|worker|worker.kind).collect::<Vec<_>>();
    components.extend(pool.iter().filter(|worker|worker.job.is_some()&&!worker.ending).map(|_|"window"));
    let value=serde_json::json!({"schema":1,"pid":std::process::id(),"version":env!("CARGO_PKG_VERSION"),"components":components,"last_error":error,
        "window_pool":pool.iter().map(|worker|serde_json::json!({"id":worker.id,"thread_id":worker.thread_id,"active":worker.job.is_some(),"requests":worker.requests,"stopping":worker.ending})).collect::<Vec<_>>(),
        "queued_window_requests":queued,"max_window_threads":MAX_WINDOW_THREADS,"retiring_menu_threads":crate::desktop::pending_menu_threads(),
        "pending_file_operations":zsdesk_windows::shell::file_transfer::pending_operations(),"pending_extension_workers":crate::context_menu_process::pending_workers()});
    if let Ok(bytes)=serde_json::to_vec(&value){
        if previous.as_ref()==Some(&bytes){return;}
        if crate::quick_search::control::atomic_write(&profile.with_file_name("runtime-status.json"),&bytes).is_ok(){*previous=Some(bytes);}
    }
}
pub fn run(profile:PathBuf,args:Vec<OsString>)->Result<()>{
    let key=property(&profile);let handle=unsafe{CreateMutexW(None,false,PCWSTR(key.as_ptr()))}?;
    let existed=unsafe{GetLastError()}==ERROR_ALREADY_EXISTS;let _instance=Kernel(handle);
    if existed{
        let deadline=Instant::now()+Duration::from_secs(5);
        loop{if let Some(hwnd)=find(&profile){return send(hwnd,args);}if Instant::now()>deadline{bail!("ZSdesk 正在启动，请稍后重试");}thread::sleep(Duration::from_millis(25));}
    }
    // COM's first STA must outlive the other STAs. The host pumps messages
    // until every component has released its Shell/Office interfaces.
    unsafe{windows::Win32::System::Ole::OleInitialize(None)}?;let _apartment=Apartment;
    let _menus=zsdesk_windows::shell::menu::SharedMenuRuntime::new();
    let (tx,rx)=mpsc::sync_channel(64);let mut ingress=Box::new(Ingress{profile:profile.clone(),tx,accepting:Cell::new(true)});
    let module=unsafe{GetModuleHandleW(None)}?;let instance=windows::Win32::Foundation::HINSTANCE(module.0);
    unsafe{RegisterClassW(&WNDCLASSW{lpfnWndProc:Some(procedure),hInstance:instance,lpszClassName:CLASS,..Default::default()});}
    let window=HostWindow(unsafe{CreateWindowExW(WS_EX_TOOLWINDOW|WS_EX_NOACTIVATE,CLASS,w!("ZSdesk"),WS_POPUP,0,0,0,0,None,None,Some(instance),Some((&mut*ingress as*mut Ingress).cast()))}?);
    let hwnd=window.0;unsafe{SetPropW(hwnd,PCWSTR(key.as_ptr()),Some(HANDLE(1usize as*mut _)))}?;
    LOCAL.set((profile.clone(),hwnd.0 as isize)).map_err(|_|anyhow::anyhow!("应用宿主已初始化"))?;
    ingress.tx.try_send(Request{args}).context("初始启动请求失败")?;
    let (finished_tx,finished_rx)=mpsc::channel::<Notice>();let mut workers:Vec<Worker>=Vec::new();let mut pool:Vec<WindowWorker>=Vec::new();
    let mut queued:VecDeque<WindowJob>=VecDeque::new();let mut last_error=None;let mut serial=0u64;let mut shutting_down=false;let mut previous_status=None;
    loop{
        while !shutting_down&&queued.len()<MAX_QUEUED_WINDOWS{let Ok(request)=rx.try_recv()else{break;};
            let kind=if request.args.iter().any(|a|a=="--desktop"){"desktop"}else if request.args.iter().any(|a|a=="--background"||a=="--quick-search"||a=="--quick-search-settings"){"background"}else{"window"};
            let destination=hwnd.0 as isize;let finished=finished_tx.clone();serial+=1;let id=serial;
            if kind=="window"{queued.push_back(WindowJob{id,args:request.args});continue;}
            let worker=thread::Builder::new().name(format!("zsdesk-{kind}")).spawn(move||{
                trace(&format!("component-start {id} {kind}"));
                let result=std::panic::catch_unwind(||crate::app::run_arguments(request.args,true));
                trace(&format!("component-return {id} {kind}"));
                let error=match result{Ok(Ok(()))=>None,Ok(Err(e))=>Some(format!("{e:#}")),Err(_)=>Some(format!("{kind} 组件异常退出"))};
                notify(destination,&finished,Notice::ComponentReturned(id,error));
            });
            match worker{Ok(thread)=>workers.push(Worker{id,kind,ending:false,thread}),Err(error)=>last_error=Some(error.to_string())}
        }
        while let Ok(notice)=finished_rx.try_recv(){match notice{
            Notice::ComponentReturned(id,error)=>{
                if error.is_some(){last_error=error;}
                if let Some(worker)=workers.iter_mut().find(|worker|worker.id==id){worker.ending=true;}
            }
            Notice::PoolReady(id,thread_id)=>if let Some(worker)=pool.iter_mut().find(|worker|worker.id==id){worker.thread_id=Some(thread_id);},
            Notice::WindowReturned(id,job,error)=>{
                if error.is_some(){last_error=error;}
                if let Some(worker)=pool.iter_mut().find(|worker|worker.id==id&&worker.job==Some(job)){worker.job=None;}
            }
            Notice::PoolReturned(id,error)=>{
                if error.is_some(){last_error=error;}
                if let Some(worker)=pool.iter_mut().find(|worker|worker.id==id){worker.job=None;worker.ending=true;}
            }
        }}
        let mut index=0;while index<workers.len(){if workers[index].thread.is_finished(){let worker=workers.swap_remove(index);let _=worker.thread.join();}else{index+=1;}}
        let mut index=0;while index<pool.len(){if pool[index].thread.is_finished(){let worker=pool.swap_remove(index);let _=worker.thread.join();}else{index+=1;}}
        while !shutting_down&&!queued.is_empty(){
            if let Some(index)=pool.iter().position(|worker|worker.job.is_none()&&!worker.ending){
                let job=queued.pop_front().unwrap();let id=job.id;let worker=&mut pool[index];
                match worker.commands.try_send(PoolCommand::Run(job)){
                    Ok(())=>{worker.job=Some(id);worker.requests+=1;if let Err(error)=wake_pool(worker){last_error=Some(format!("文件窗口唤醒失败：{error:#}"));}}
                    Err(error)=>{
                        let command=match error{mpsc::TrySendError::Full(command)|mpsc::TrySendError::Disconnected(command)=>command};
                        if let PoolCommand::Run(job)=command{queued.push_front(job);}
                        worker.ending=true;
                    }
                }
            }else if pool.len()<MAX_WINDOW_THREADS{
                let job=queued.pop_front().unwrap();serial+=1;
                match spawn_window_pool(serial,job,hwnd.0 as isize,finished_tx.clone()){
                    Ok(worker)=>pool.push(worker),Err(error)=>last_error=Some(format!("无法创建文件窗口线程：{error:#}")),
                }
            }else{break;}
        }
        // Idle STAs stay reusable while a desktop/background component or
        // another window remains alive. Once the last component ends, retire
        // the pool while the host's first STA continues pumping COM messages.
        if !shutting_down&&workers.is_empty()&&queued.is_empty()&&pool.iter().all(|worker|worker.job.is_none()){
            shutting_down=true;ingress.accepting.set(false);
            crate::context_menu_process::shutdown_idle_worker();
            for worker in &mut pool{
                if !worker.ending{
                    worker.ending=true;let _=worker.commands.try_send(PoolCommand::Stop);
                    if let Err(error)=wake_pool(worker){last_error=Some(format!("文件窗口线程退出通知失败：{error:#}"));}
                }
            }
        }
        // COM/TLS finalizers can still need the first STA after component code
        // returns. Keep pumping while the OS thread finishes; never block it.
        if shutting_down{crate::context_menu_process::shutdown_idle_worker();}
        let retiring_menus=crate::desktop::pending_menu_threads()+zsdesk_windows::shell::file_transfer::pending_operations()+crate::context_menu_process::pending_workers();
        unsafe{if workers.iter().any(|worker|worker.ending)||pool.iter().any(|worker|worker.ending)||retiring_menus>0{SetTimer(Some(hwnd),1,25,None);}else{let _=KillTimer(Some(hwnd),1);}}
        status(&profile,&workers,&pool,queued.len(),last_error.as_deref(),&mut previous_status);
        if shutting_down&&workers.is_empty()&&pool.is_empty()&&retiring_menus==0{break;}
        let mut message=MSG::default();let received=unsafe{GetMessageW(&mut message,None,0,0)}.0;
        // Component WM_QUIT belongs to its UI thread. Only component lifetime
        // accounting may end this host, including after COM pumps a quit.
        if received==0{continue;}
        if received<0{last_error=Some(format!("应用消息循环失败：{}",windows::core::Error::from_thread()));continue;}
        unsafe{let _=TranslateMessage(&message);DispatchMessageW(&message);}
    }
    crate::context_menu_process::shutdown_idle_worker();
    crate::window::diagnostics::flush_at_shutdown();
    drop(window);drop(ingress);
    if let Err(error)=crate::fast_search::everything::shutdown_managed_if_unused(){trace(&format!("Everything shutdown: {error:#}"));}
    if let Some(error)=last_error{bail!("{error}");}Ok(())
}
