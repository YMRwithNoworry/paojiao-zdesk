//! Local diagnostics: bounded files, background I/O and pointer-free UI replies.
use super::*;
use std::{fs,io::Write,path::Path,sync::{Arc,Weak,Mutex,MutexGuard,OnceLock,TryLockError,
    atomic::{AtomicBool,AtomicU64,Ordering},mpsc},thread::JoinHandle,time::{Duration,Instant,SystemTime,UNIX_EPOCH}};
use sha2::{Digest,Sha256};
use windows::Win32::{Foundation::{CloseHandle,HANDLE,WAIT_OBJECT_0,WAIT_ABANDONED},
    System::Threading::{CreateMutexW,ReleaseMutex,WaitForSingleObject}};

const MAX_LOG_BYTES:u64=256*1024;
const CLASS:PCWSTR=windows::core::w!("ZSDesk.Diagnostics.Window");
const TEXT_ID:usize=7100;
const REFRESH_ID:usize=7101;
const COPY_ID:usize=7102;
const CLOSE_ID:usize=7103;
const REPORT_READY:u32=WM_APP+73;
static LOGGER:OnceLock<Mutex<Logger>>=OnceLock::new();
static DROPPED:AtomicU64=AtomicU64::new(0);
static WINDOW_TOKEN:AtomicU64=AtomicU64::new(1);

#[derive(Default)]
struct Logger {
    sender:Option<mpsc::SyncSender<LogMessage>>,thread:Option<JoinHandle<()>>,
    initialization_error:Option<String>,last_io_error:Option<String>,io_failures:u64,
    written:u64,retry_after:Option<Instant>,
}
enum LogMessage {
    Entry(String),Flush(mpsc::SyncSender<()>),
    Snapshot{window:isize,token:usize,sequence:u64,result:Weak<ReportMailbox>},
}
#[derive(Default)]
struct ReportMailbox {alive:AtomicBool,latest:AtomicU64,reply:Mutex<Option<(u64,String)>>}

fn logger()->&'static Mutex<Logger>{LOGGER.get_or_init(||Mutex::new(Logger::default()))}
fn try_logger()->Option<MutexGuard<'static,Logger>> {
    match logger().try_lock(){Ok(state)=>Some(state),Err(TryLockError::Poisoned(error))=>Some(error.into_inner()),Err(TryLockError::WouldBlock)=>None}
}
pub(crate) fn directory()->PathBuf {
    std::env::var_os("ZSDESK_DIAGNOSTICS_ROOT").map(PathBuf::from).filter(|path|path.is_absolute())
        .unwrap_or_else(||dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join(crate::edition::profile_name()).join("diagnostics"))
}

/// Covers check/rotate/append across app instances; acquired only by background I/O.
struct LogFileGuard(HANDLE);
impl Drop for LogFileGuard {fn drop(&mut self){let _=unsafe{ReleaseMutex(self.0)};let _=unsafe{CloseHandle(self.0)};}}
fn lock_files(root:&Path)->std::io::Result<LogFileGuard> {
    fs::create_dir_all(root)?;
    let canonical=fs::canonicalize(root)?;
    let identity=canonical.to_string_lossy().replace('/',"\\").to_lowercase();
    let digest=Sha256::digest(identity.as_bytes());
    let name=format!("Local\\ZSDesk.Diagnostics.{digest:x}");
    let handle=unsafe{CreateMutexW(None,false,PCWSTR(wide(&name).as_ptr()))}.map_err(std::io::Error::other)?;
    let result=unsafe{WaitForSingleObject(handle,1000)};
    if result==WAIT_OBJECT_0 || result==WAIT_ABANDONED {Ok(LogFileGuard(handle))}
    else {let _=unsafe{CloseHandle(handle)};Err(std::io::Error::new(std::io::ErrorKind::TimedOut,"诊断文件正在使用，请稍后刷新"))}
}
fn append(root:&Path,line:&str)->std::io::Result<()> {
    if line.len()as u64+1>MAX_LOG_BYTES{return Err(std::io::Error::new(std::io::ErrorKind::InvalidData,"单条诊断记录过长"));}
    let _files=lock_files(root)?;
    let current=root.join("events.current.jsonl");let previous=root.join("events.previous.jsonl");
    if fs::metadata(&current).is_ok_and(|metadata|metadata.len()+line.len()as u64+1>MAX_LOG_BYTES) {
        if previous.is_file(){fs::remove_file(&previous)?;}
        fs::rename(&current,&previous)?;
    }
    let mut file=fs::OpenOptions::new().create(true).append(true).open(current)?;
    writeln!(file,"{line}")
}
fn write_result(result:std::io::Result<()>) {
    let mut state=logger().lock().unwrap_or_else(|error|error.into_inner());
    match result {
        Ok(())=>{state.written=state.written.saturating_add(1);state.last_io_error=None;},
        Err(error)=>{state.io_failures=state.io_failures.saturating_add(1);state.last_io_error=Some(error.to_string());},
    }
}
pub(crate) fn initialize() {
    let Some(mut state)=try_logger()else{return;};
    if state.thread.as_ref().is_some_and(|thread|!thread.is_finished()){return;}
    if state.retry_after.is_some_and(|deadline|Instant::now()<deadline){return;}
    state.sender=None;
    if let Some(thread)=state.thread.take(){let _=thread.join();}
    let (sender,receiver)=mpsc::sync_channel::<LogMessage>(128);let root=directory();
    match std::thread::Builder::new().name("zsdesk-diagnostics".into()).spawn(move||{
        while let Ok(message)=receiver.recv(){match message{
            LogMessage::Entry(line)=>write_result(append(&root,&line)),
            LogMessage::Flush(done)=>{let _=done.try_send(());},
            LogMessage::Snapshot{window,token,sequence,result}=>{
                if !result.upgrade().is_some_and(|mailbox|mailbox.alive.load(Ordering::Acquire)&&mailbox.latest.load(Ordering::Acquire)==sequence){continue;}
                let text=report(&root);
                let Some(mailbox)=result.upgrade().filter(|mailbox|mailbox.alive.load(Ordering::Acquire)&&mailbox.latest.load(Ordering::Acquire)==sequence)else{continue;};
                *mailbox.reply.lock().unwrap_or_else(|error|error.into_inner())=Some((sequence,text));
                if mailbox.alive.load(Ordering::Acquire){let _=unsafe{PostMessageW(Some(HWND(window as *mut _)),REPORT_READY,WPARAM(token),LPARAM(sequence as isize))};}
            },
        }}
    }) {
        Ok(thread)=>{state.sender=Some(sender);state.thread=Some(thread);state.initialization_error=None;state.retry_after=None;},
        Err(error)=>{state.initialization_error=Some(error.to_string());state.retry_after=Some(Instant::now()+Duration::from_secs(2));},
    }
}
fn sender()->Option<mpsc::SyncSender<LogMessage>> {
    initialize();try_logger().and_then(|state|state.sender.clone())
}
/// Never enumerates files, acquires the file mutex or waits for another thread.
pub(crate) fn record(component:&str,phase:&str,code:Option<i32>) {
    let Some(sender)=sender()else{DROPPED.fetch_add(1,Ordering::Relaxed);return;};
    let millis=SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis();
    let dropped=DROPPED.swap(0,Ordering::Relaxed);
    let value=serde_json::json!({"time_unix_ms":millis,"pid":std::process::id(),
        "version":env!("CARGO_PKG_VERSION"),"component":component.chars().take(64).collect::<String>(),
        "phase":phase.chars().take(512).collect::<String>(),"hresult":code.map(|v|format!("0x{:08X}",v as u32)),
        "dropped_events":dropped});
    if sender.try_send(LogMessage::Entry(value.to_string())).is_err(){DROPPED.fetch_add(dropped.saturating_add(1),Ordering::Relaxed);}
}
/// Call only after all visible components have finished, not from WM_CLOSE.
pub(crate) fn flush_at_shutdown() {
    let Some(sender)=try_logger().and_then(|state|state.sender.clone())else{return;};
    let (done,ready)=mpsc::sync_channel(1);
    if sender.try_send(LogMessage::Flush(done)).is_ok(){let _=ready.recv_timeout(Duration::from_millis(300));}
}
fn status_text()->String {
    let Some(state)=try_logger()else{return "诊断状态正在更新".into();};
    format!("已写入 {} 条；写入失败 {} 次；待汇总丢弃事件 {} 条\r\n后台线程：{}\r\n初始化错误：{}\r\n最近写入错误：{}",
        state.written,state.io_failures,DROPPED.load(Ordering::Relaxed),
        if state.thread.as_ref().is_some_and(|thread|!thread.is_finished()){"运行中"}else{"未运行，可刷新重试"},
        state.initialization_error.as_deref().unwrap_or("无"),state.last_io_error.as_deref().unwrap_or("无"))
}
fn log_tail(path:&Path)->String {
    use std::io::{Read,Seek,SeekFrom};
    let Ok(mut file)=fs::File::open(path)else{return String::new();};
    let Ok(size)=file.metadata().map(|metadata|metadata.len())else{return String::new();};
    if size>32768{let _=file.seek(SeekFrom::Start(size-32768));}
    let mut bytes=Vec::new();let _=file.take(32768).read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).into_owned()
}
fn report(root:&Path)->String {
    let (current,previous)=match lock_files(root) {
        Ok(_guard)=>(log_tail(&root.join("events.current.jsonl")),log_tail(&root.join("events.previous.jsonl"))),
        Err(error)=>(format!("无法读取事件日志：{error}"),String::new()),
    };
    let menu=crate::context_menu_process::read_recent_diagnostics();
    let body=serde_json::to_string_pretty(&menu).unwrap_or_default();
    let text=format!("ZSDesk {} {}\r\n本地诊断日志\r\n\r\n日志目录：{}\r\n\r\n记录状态\r\n{}\r\n\r\n右键菜单诊断\r\n{}\r\n\r\n最近事件\r\n{}\r\n\r\n上一代事件\r\n{}",
        env!("CARGO_PKG_VERSION"),crate::edition::name(),root.display(),status_text(),body,current,previous);
    let text=if text.chars().count()>200_000 {text.chars().take(200_000).chain("\r\n日志显示已截断，可在日志目录查看完整记录。".chars()).collect()}else{text};
    text.replace("\r\n","\n").replace('\n',"\r\n")
}

struct DiagnosticWindow {edit:HWND,buttons:[HWND;3],text:String,adopted:Rc<Cell<bool>>,token:usize,mailbox:Arc<ReportMailbox>}
impl Drop for DiagnosticWindow {fn drop(&mut self){self.mailbox.alive.store(false,Ordering::Release);}}
thread_local!{static WINDOWS:RefCell<HashMap<isize,HWND>>=RefCell::new(HashMap::new());}
fn live_window(hwnd:HWND,pointer:*mut DiagnosticWindow)->bool {
    unsafe{IsWindow(Some(hwnd))}.as_bool()&&unsafe{GetWindowLongPtrW(hwnd,GWLP_USERDATA)}==pointer as isize
}
fn request_report(hwnd:HWND,pointer:*mut DiagnosticWindow) {
    if !live_window(hwnd,pointer){return;}
    let(token,mailbox)=unsafe{((*pointer).token,(*pointer).mailbox.clone())};
    let sequence=mailbox.latest.fetch_add(1,Ordering::AcqRel).wrapping_add(1);
    let queued=sender().is_some_and(|sender|sender.try_send(LogMessage::Snapshot{window:hwnd.0 as isize,token,sequence,result:Arc::downgrade(&mailbox)}).is_ok());
    if !queued {
        *mailbox.reply.lock().unwrap_or_else(|error|error.into_inner())=Some((sequence,format!("诊断报告暂不可用，请稍后刷新。\r\n\r\n{}",status_text())));
        let _=unsafe{PostMessageW(Some(hwnd),REPORT_READY,WPARAM(token),LPARAM(sequence as isize))};
    }
}
pub(super) fn show(owner:HWND)->Result<()> {
    initialize();
    if let Some(hwnd)=WINDOWS.with(|windows|windows.borrow().get(&(owner.0 as isize)).copied()) {
        if unsafe{IsWindow(Some(hwnd))}.as_bool(){let _=unsafe{ShowWindow(hwnd,SW_RESTORE)};let _=unsafe{SetForegroundWindow(hwnd)};return Ok(());}
    }
    let instance=HINSTANCE(unsafe{GetModuleHandleW(None)?}.0);
    unsafe{RegisterClassW(&WNDCLASSW{hInstance:instance,lpfnWndProc:Some(procedure),lpszClassName:CLASS,
        hCursor:LoadCursorW(None,IDC_ARROW)?,hbrBackground:HBRUSH((windows::Win32::Graphics::Gdi::COLOR_WINDOW.0+1)as usize as *mut _),..Default::default()});}
    let adopted=Rc::new(Cell::new(false));
    let mailbox=Arc::new(ReportMailbox::default());mailbox.alive.store(true,Ordering::Release);
    let state=Box::new(DiagnosticWindow{edit:HWND::default(),buttons:[HWND::default();3],text:"正在读取本地诊断日志…".into(),adopted:adopted.clone(),
        token:WINDOW_TOKEN.fetch_add(1,Ordering::Relaxed)as usize,mailbox});
    let pointer=Box::into_raw(state);
    let result=unsafe{CreateWindowExW(WS_EX_APPWINDOW,CLASS,windows::core::w!("ZSDesk 诊断日志"),WS_OVERLAPPEDWINDOW|WS_CLIPCHILDREN,
        CW_USEDEFAULT,CW_USEDEFAULT,880,620,Some(owner),None,Some(instance),Some(pointer.cast()))};
    let hwnd=match result{Ok(hwnd)=>hwnd,Err(error)=>{if !adopted.get(){unsafe{drop(Box::from_raw(pointer));}}return Err(error.into());}};
    if !live_window(hwnd,pointer){return Ok(());}
    let _=unsafe{SetWindowTextW(hwnd,windows::core::w!("ZSDesk 诊断日志"))};
    WINDOWS.with(|windows|{windows.borrow_mut().insert(owner.0 as isize,hwnd);});
    unsafe{let _=ShowWindow(hwnd,SW_SHOW);}
    if live_window(hwnd,pointer){request_report(hwnd,pointer);}
    Ok(())
}
pub(super) fn dispatch(message:&MSG)->bool {
    let root=unsafe{GetAncestor(message.hwnd,GA_ROOT)};
    let owns=WINDOWS.with(|windows|windows.borrow().values().any(|window|*window==root));
    if !owns{return false;}
    if message.message==WM_KEYDOWN&&message.wParam.0==VK_ESCAPE.0 as usize{post(root,WM_CLOSE,0,0);}
    else{unsafe{let _=TranslateMessage(message);DispatchMessageW(message);}}
    true
}
pub(super) fn close(owner:HWND) {
    let hwnd=WINDOWS.with(|windows|windows.borrow_mut().remove(&(owner.0 as isize)));
    if let Some(hwnd)=hwnd{let _=unsafe{DestroyWindow(hwnd)};}
}
unsafe extern "system" fn procedure(hwnd:HWND,message:u32,wp:WPARAM,lp:LPARAM)->LRESULT {
    if message==WM_NCCREATE {
        let create=unsafe{&*(lp.0 as *const CREATESTRUCTW)};
        unsafe{(*(create.lpCreateParams as *mut DiagnosticWindow)).adopted.set(true);SetWindowLongPtrW(hwnd,GWLP_USERDATA,create.lpCreateParams as isize);}
        return LRESULT(1);
    }
    let pointer=unsafe{GetWindowLongPtrW(hwnd,GWLP_USERDATA)}as *mut DiagnosticWindow;
    if pointer.is_null(){return unsafe{DefWindowProcW(hwnd,message,wp,lp)};}
    if message==WM_NCDESTROY {
        unsafe{SetWindowLongPtrW(hwnd,GWLP_USERDATA,0);drop(Box::from_raw(pointer));}
        WINDOWS.with(|windows|windows.borrow_mut().retain(|_,window|*window!=hwnd));
        return unsafe{DefWindowProcW(hwnd,message,wp,lp)};
    }
    match message {
        WM_CREATE=>{
            let edit=unsafe{child(hwnd,"EDIT","",TEXT_ID,(ES_MULTILINE|ES_AUTOVSCROLL|ES_AUTOHSCROLL|ES_READONLY)as u32|WS_VSCROLL.0|WS_HSCROLL.0,true)};
            if !live_window(hwnd,pointer){return LRESULT(-1);}
            let Ok(edit)=edit else{return LRESULT(-1);};unsafe{(*pointer).edit=edit;}
            let text=unsafe{(*pointer).text.clone()};
            unsafe{SendMessageW(edit,EM_SETLIMITTEXT,Some(WPARAM(256*1024)),None);}
            if !live_window(hwnd,pointer){return LRESULT(-1);}
            let _=unsafe{SetWindowTextW(edit,PCWSTR(wide(&text).as_ptr()))};
            if !live_window(hwnd,pointer){return LRESULT(-1);}
            for(index,(label,id))in[("刷新",REFRESH_ID),("复制日志",COPY_ID),("关闭",CLOSE_ID)].into_iter().enumerate(){
                let button=unsafe{child(hwnd,"BUTTON",label,id,BS_PUSHBUTTON as u32,true)};
                if !live_window(hwnd,pointer){return LRESULT(-1);}
                if let Ok(button)=button{unsafe{(*pointer).buttons[index]=button;}}
            }
            let font=unsafe{windows::Win32::Graphics::Gdi::GetStockObject(windows::Win32::Graphics::Gdi::DEFAULT_GUI_FONT)};
            if !live_window(hwnd,pointer){return LRESULT(-1);}
            let buttons=unsafe{(*pointer).buttons};
            for control in std::iter::once(edit).chain(buttons){
                if !live_window(hwnd,pointer){return LRESULT(-1);}
                unsafe{SendMessageW(control,WM_SETFONT,Some(WPARAM(font.0 as usize)),Some(LPARAM(1)));}
            }
            post(hwnd,WM_SIZE,0,0);LRESULT(0)
        }
        WM_SIZE=>{
            let(edit,buttons)=unsafe{((*pointer).edit,(*pointer).buttons)};
            let mut rect=RECT::default();let _=unsafe{GetClientRect(hwnd,&mut rect)};
            let dpi=unsafe{GetDpiForWindow(hwnd)}.max(96);let pad=scale(12,dpi);let row=scale(32,dpi);let width=(rect.right-2*pad).max(0);
            if !live_window(hwnd,pointer){return LRESULT(0);}
            unsafe{move_control(edit,LayoutRect::new(pad,pad,width,(rect.bottom-3*pad-row).max(0)));}
            for(index,button)in buttons.iter().enumerate(){
                if !live_window(hwnd,pointer){return LRESULT(0);}
                unsafe{move_control(*button,LayoutRect::new(pad+index as i32*scale(108,dpi),(rect.bottom-pad-row).max(0),scale(96,dpi),row));}
            }
            LRESULT(0)
        }
        REPORT_READY=>{
            let(token,edit,mailbox)=unsafe{((*pointer).token,(*pointer).edit,(*pointer).mailbox.clone())};
            if wp.0!=token||lp.0 as u64!=mailbox.latest.load(Ordering::Acquire){return LRESULT(0);}
            let reply=mailbox.reply.try_lock().ok().and_then(|mut reply|reply.take());
            if let Some((_,text))=reply.filter(|(sequence,_)|*sequence==lp.0 as u64){
                unsafe{(*pointer).text=text.clone();}
                let _=unsafe{SetWindowTextW(edit,PCWSTR(wide(&text).as_ptr()))};
            }
            LRESULT(0)
        }
        WM_COMMAND=>{
            match wp.0&0xffff {
                REFRESH_ID=>request_report(hwnd,pointer),
                COPY_ID=>{let text=unsafe{&*pointer}.text.clone();input::set_clipboard_text(hwnd,&text);},
                CLOSE_ID=>post(hwnd,WM_CLOSE,0,0),_=>{},
            }LRESULT(0)
        }
        WM_CLOSE=>{let _=unsafe{DestroyWindow(hwnd)};LRESULT(0)}
        _=>unsafe{DefWindowProcW(hwnd,message,wp,lp)},
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn diagnostics_rotation_keeps_two_bounded_generations()->std::io::Result<()> {
        let root=std::env::temp_dir().join(format!("zsdesk-logs-{}",SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        fs::create_dir(&root)?;
        struct Fixture(PathBuf);impl Drop for Fixture{fn drop(&mut self){let _=fs::remove_file(self.0.join("events.current.jsonl"));let _=fs::remove_file(self.0.join("events.previous.jsonl"));let _=fs::remove_dir(&self.0);}}
        let _fixture=Fixture(root.clone());
        for _ in 0..3 {append(&root,&"x".repeat(MAX_LOG_BYTES as usize-1))?;}
        assert_eq!(fs::read_dir(&root)?.count(),2);
        assert!(fs::metadata(root.join("events.current.jsonl"))?.len()<=MAX_LOG_BYTES);
        assert!(fs::metadata(root.join("events.previous.jsonl"))?.len()<=MAX_LOG_BYTES);Ok(())
    }
    #[test]
    fn stale_diagnostic_report_cannot_replace_newer_request() {
        let mailbox=Arc::new(ReportMailbox::default());mailbox.alive.store(true,Ordering::Release);
        mailbox.latest.store(2,Ordering::Release);
        assert!(!Arc::downgrade(&mailbox).upgrade().is_some_and(|value|value.latest.load(Ordering::Acquire)==1));
        let weak=Arc::downgrade(&mailbox);drop(mailbox);assert!(weak.upgrade().is_none());
    }
    #[test]
    #[ignore="Owned helper for the concurrent-process diagnostic rotation test"]
    fn native_diagnostics_writer_entry()->anyhow::Result<()> {
        use anyhow::{Context,ensure};
        let root=PathBuf::from(std::env::var_os("ZSDESK_DIAGNOSTICS_TEST_ROOT").context("fixture root")?);
        ensure!(root.is_absolute(),"absolute fixture required");
        let writer=std::env::var("ZSDESK_DIAGNOSTICS_TEST_WRITER")?;
        for sequence in 0..900 {
            append(&root,&serde_json::json!({"writer":writer,"sequence":sequence,"padding":"x".repeat(1024)}).to_string())?;
        }
        Ok(())
    }
    #[test]
    #[ignore="Concurrent process diagnostic rotation; requires ZSDESK_NATIVE_TEST_OUTPUT"]
    fn native_diagnostics_concurrent_process_rotation_is_bounded()->anyhow::Result<()> {
        use anyhow::{Context,ensure};use std::process::{Child,Command,Stdio};use std::os::windows::process::CommandExt;
        let root=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("isolated output")?).join("diagnostics-concurrent");
        ensure!(root.is_absolute(),"absolute output required");fs::create_dir_all(&root)?;
        let logs=root.join("logs");
        struct OwnedChild(Child);impl Drop for OwnedChild{fn drop(&mut self){if self.0.try_wait().ok().flatten().is_none(){let _=self.0.kill();let _=self.0.wait();}}}
        let mut children=Vec::new();let mut pids=Vec::new();
        for writer in 0..2 {
            let child=Command::new(std::env::current_exe()?).arg("window::diagnostics::tests::native_diagnostics_writer_entry")
                .args(["--exact","--ignored","--nocapture"]).env("ZSDESK_DIAGNOSTICS_TEST_ROOT",&logs)
                .env("ZSDESK_DIAGNOSTICS_TEST_WRITER",writer.to_string()).creation_flags(0x08000000)
                .stdin(Stdio::null()).stdout(Stdio::null()).stderr(fs::File::create(root.join(format!("writer-{writer}.stderr.current.log")))?).spawn()?;
            pids.push(child.id());children.push(OwnedChild(child));
        }
        let deadline=Instant::now()+Duration::from_secs(30);
        for child in &mut children {
            loop {
                if let Some(status)=child.0.try_wait()?{ensure!(status.success(),"diagnostic writer {} exited {status}",child.0.id());break;}
                ensure!(Instant::now()<deadline,"diagnostic writer exceeded deadline");std::thread::sleep(Duration::from_millis(10));
            }
        }
        let mut records=0usize;let mut seen=std::collections::HashSet::new();let mut final_record=false;
        for name in ["events.current.jsonl","events.previous.jsonl"] {
            let path=logs.join(name);ensure!(fs::metadata(&path)?.len()<=MAX_LOG_BYTES,"rotation exceeded its byte limit");
            for line in fs::read_to_string(path)?.lines() {
                let value:serde_json::Value=serde_json::from_str(line)?;
                let writer=value["writer"].as_str().context("writer identity")?;let sequence=value["sequence"].as_u64().context("sequence")?;
                ensure!(seen.insert((writer.to_owned(),sequence)),"duplicated log record");
                final_record|=sequence==899;records+=1;
            }
        }
        ensure!(fs::read_dir(&logs)?.count()==2&&records>200&&final_record,"missing complete retained log generations");
        fs::write(root.join("verification.current.json"),serde_json::to_vec_pretty(&serde_json::json!({"state":"passed","worker_pids":pids,
            "retained_records":records,"generations":2,"max_generation_bytes":MAX_LOG_BYTES,"complete_json_lines":true}))?)?;
        Ok(())
    }
    #[test]
    #[ignore="Private-desktop synchronous destruction regression; requires ZSDESK_NATIVE_TEST_OUTPUT"]
    fn native_diagnostics_resize_survives_synchronous_child_close()->anyhow::Result<()> {
        use anyhow::{Context,ensure};use std::ffi::c_void;
        #[link(name="user32")]unsafe extern "system"{
            fn CreateDesktopW(name:*const u16,device:*const u16,mode:*const c_void,flags:u32,access:u32,attributes:*const c_void)->*mut c_void;
            fn SetThreadDesktop(desktop:*mut c_void)->i32;fn CloseDesktop(desktop:*mut c_void)->i32;
        }
        unsafe extern "system" fn destroy_parent(child:HWND,msg:u32,wp:WPARAM,lp:LPARAM,id:usize,_:usize)->LRESULT {
            if msg==WM_WINDOWPOSCHANGING {
                let _=unsafe{RemoveWindowSubclass(child,Some(destroy_parent),id)};
                if let Ok(parent)=unsafe{GetParent(child)}{let _=unsafe{DestroyWindow(parent)};}
                return LRESULT(0);
            }
            unsafe{DefSubclassProc(child,msg,wp,lp)}
        }
        let output=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("isolated output")?).join("diagnostics");
        ensure!(output.is_absolute(),"absolute output required");fs::create_dir_all(&output)?;
        let name=wide(&format!("ZSDesk.DiagnosticsTest.{}",std::process::id()));
        let desktop=unsafe{CreateDesktopW(name.as_ptr(),std::ptr::null(),std::ptr::null(),0,0x000f00ff,std::ptr::null())};
        ensure!(!desktop.is_null(),"private desktop creation failed");let raw=desktop as usize;
        let result=std::thread::spawn(move||->anyhow::Result<()>{
            ensure!(unsafe{SetThreadDesktop(raw as*mut c_void)}!=0,"private desktop attach failed");
            struct Owner(HWND);impl Drop for Owner{fn drop(&mut self){close(self.0);let _=unsafe{DestroyWindow(self.0)};}}
            let owner=Owner(unsafe{CreateWindowExW(WS_EX_NOACTIVATE,windows::core::w!("STATIC"),PCWSTR::null(),WS_POPUP,0,0,1,1,None,None,None,None)}?);
            show(owner.0)?;
            let window=WINDOWS.with(|windows|windows.borrow().get(&(owner.0.0 as isize)).copied()).context("diagnostic window")?;
            let edit=unsafe{GetDlgItem(Some(window),TEXT_ID as i32)}?;
            unsafe{SetWindowSubclass(edit,Some(destroy_parent),0x5a44,0)}.ok()?;
            unsafe{SendMessageW(window,WM_SIZE,None,None);}
            ensure!(!unsafe{IsWindow(Some(window))}.as_bool(),"resize did not exercise synchronous parent destruction");
            ensure!(WINDOWS.with(|windows|!windows.borrow().contains_key(&(owner.0.0 as isize))),"destroyed diagnostic window remained registered");
            Ok(())
        }).join().map_err(|_|anyhow::anyhow!("diagnostic reentrancy test panicked")).and_then(|result|result);
        let closed=unsafe{CloseDesktop(desktop)}!=0;
        fs::write(output.join("verification.current.json"),serde_json::to_vec_pretty(&serde_json::json!({"state":if result.is_ok()&&closed{"passed"}else{"failed"},
            "result":result.as_ref().map_err(|error|format!("{error:#}")),"synchronous_child_destroy":true,"desktop_closed":closed}))?)?;
        result?;ensure!(closed,"private desktop did not close");Ok(())
    }
}
