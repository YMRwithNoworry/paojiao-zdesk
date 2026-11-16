//! Independent recovery for a native dialog hidden behind a picker window.
use super::*;
use std::{io::{BufRead,BufReader,Write},process::{Child,ChildStdin,Command,Stdio},sync::mpsc};
use windows::Win32::{Foundation::{CloseHandle,FILETIME,HANDLE,WAIT_TIMEOUT},System::Threading::{GetCurrentProcess,GetProcessTimes,OpenProcess,WaitForSingleObject,PROCESS_QUERY_LIMITED_INFORMATION,PROCESS_SYNCHRONIZE}};
const PROPERTY:PCWSTR=windows::core::w!("ZSDesk.FileDialogLease.v1");
#[derive(Clone,Debug,serde::Serialize,serde::Deserialize)]pub(super) struct Identity{pub window:isize,pub pid:u32,pub thread:u32,pub created:u64}
struct Handle(HANDLE);impl Drop for Handle{fn drop(&mut self){let _=unsafe{CloseHandle(self.0)};}}
pub(super) struct OwnedHandle(pub HANDLE);impl Drop for OwnedHandle{fn drop(&mut self){let _=unsafe{CloseHandle(self.0)};}}
pub(super) fn process_created(process:HANDLE)->Option<u64>{created(process)}
fn created(process:HANDLE)->Option<u64>{let(mut start,mut end,mut kernel,mut user)=(FILETIME::default(),FILETIME::default(),FILETIME::default(),FILETIME::default());
    unsafe{GetProcessTimes(process,&mut start,&mut end,&mut kernel,&mut user)}.ok()?;Some((u64::from(start.dwHighDateTime)<<32)|u64::from(start.dwLowDateTime))}
impl Identity{
    pub fn capture(window:HWND)->Option<Self>{
        if !unsafe{IsWindow(Some(window))}.as_bool(){return None;}
        let mut pid=0;let thread=unsafe{GetWindowThreadProcessId(window,Some(&mut pid))};if pid==0||thread==0{return None;}
        let process=Handle(unsafe{OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION,false,pid)}.ok()?);
        Some(Self{window:window.0 as isize,pid,thread,created:created(process.0)?})
    }
    pub fn hwnd(&self)->HWND{HWND(self.window as*mut _)}
    pub fn matches(&self)->bool{Self::capture(self.hwnd()).is_some_and(|actual|actual.pid==self.pid&&actual.thread==self.thread&&actual.created==self.created)}
}
#[derive(Clone,serde::Serialize,serde::Deserialize)]struct Lease{source:Identity,picker:isize,token:u64,parent:u32}
fn picker_alive(lease:&Lease)->bool{
    let window=HWND(lease.picker as*mut _);let mut pid=0;unsafe{GetWindowThreadProcessId(window,Some(&mut pid));}
    pid==lease.parent&&unsafe{GetPropW(window,crate::window::picker::MARKER)}.0 as usize==lease.token as usize
}
fn restore(lease:&Lease){
    let window=lease.source.hwnd();
    if lease.source.matches()&&unsafe{GetPropW(window,PROPERTY)}.0 as usize==lease.token as usize{
        let _=unsafe{RemovePropW(window,PROPERTY)};unsafe{let _=ShowWindowAsync(window,SW_SHOWNOACTIVATE);}
    }
}
pub(super) struct Guard{lease:Lease,child:Child,input:Option<ChildStdin>,reader:Option<JoinHandle<()>>}
impl Guard{
    pub fn hide(source:Identity,picker:isize,token:u64)->Result<Self>{
        Self::hide_using(source,picker,token,&std::env::current_exe()?)
    }
    pub(super) fn hide_using(source:Identity,picker:isize,token:u64,executable:&Path)->Result<Self>{
        use std::os::windows::process::CommandExt;
        let parent=std::process::id();let stamp=created(unsafe{GetCurrentProcess()}).context("进程身份不可用")?;
        let lease=Lease{source,picker,token,parent};
        let mut child=Command::new(executable).args(["--file-picker-guard",&parent.to_string(),&stamp.to_string()])
            .creation_flags(0x08000000).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?;
        let input=child.stdin.take();let stdout=child.stdout.take().context("恢复进程输出不可用")?;
        let recovery=lease.clone();let(tx,rx)=mpsc::sync_channel(1);let reader=thread::spawn(move||{
            let mut stream=BufReader::new(stdout);let mut line=String::new();let ok=stream.read_line(&mut line).is_ok()&&line.trim()=="ready";let _=tx.send(ok);
            if ok{for line in stream.lines(){if line.is_err(){break;}}}restore(&recovery);
        });
        let mut result=Self{lease,child,input,reader:Some(reader)};
        let stream=result.input.as_mut().context("恢复进程输入不可用")?;serde_json::to_writer(&mut*stream,&result.lease)?;stream.write_all(b"\n")?;stream.flush()?;
        ensure!(rx.recv_timeout(Duration::from_secs(3)).unwrap_or(false),"文件选择窗口恢复保护未就绪");Ok(result)
    }
    pub fn exited(&mut self)->bool{self.child.try_wait().ok().flatten().is_some()}
}
impl Drop for Guard{fn drop(&mut self){
    if let Some(mut stream)=self.input.take(){let _=stream.write_all(b"restore\n");let _=stream.flush();}
    restore(&self.lease);let until=Instant::now()+Duration::from_secs(1);
    while self.child.try_wait().ok().flatten().is_none()&&Instant::now()<until{thread::sleep(Duration::from_millis(10));}
    if self.child.try_wait().ok().flatten().is_none(){let _=self.child.kill();let _=self.child.wait();}
    if let Some(reader)=self.reader.take(){let _=reader.join();}
}}
pub(crate) fn run_if_requested()->Option<Result<()>>{
    if let Some(result)=super::watcher::run_if_requested(){return Some(result);}
    if std::env::args_os().nth(1).as_deref()!=Some(std::ffi::OsStr::new("--file-picker-guard")){return None;}Some(run())
}
fn run()->Result<()>{
    let pid:u32=std::env::args().nth(2).context("parent")?.parse()?;let stamp:u64=std::env::args().nth(3).context("creation")?.parse()?;
    let parent=Handle(unsafe{OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION|PROCESS_SYNCHRONIZE,false,pid)}?);
    ensure!(created(parent.0)==Some(stamp),"parent changed");
    let mut line=String::new();std::io::stdin().read_line(&mut line)?;ensure!(line.len()<8192,"invalid lease");let lease:Lease=serde_json::from_str(&line)?;
    ensure!(lease.parent==pid&&lease.source.matches()&&class(lease.source.hwnd())=="#32770"&&picker_alive(&lease),"invalid picker identity");
    ensure!(unsafe{GetPropW(lease.source.hwnd(),PROPERTY)}.0.is_null(),"dialog already claimed");
    ensure!(unsafe{IsWindowVisible(lease.source.hwnd())}.as_bool(),"source not visible");
    unsafe{SetPropW(lease.source.hwnd(),PROPERTY,Some(HANDLE(lease.token as usize as*mut _)))}?;
    struct Recovery(Lease);impl Drop for Recovery{fn drop(&mut self){restore(&self.0);}}let _recovery=Recovery(lease.clone());
    unsafe{let _=ShowWindowAsync(lease.source.hwnd(),SW_HIDE);}
    let until=Instant::now()+Duration::from_millis(800);
    while unsafe{IsWindowVisible(lease.source.hwnd())}.as_bool(){ensure!(Instant::now()<until,"source did not hide");thread::sleep(Duration::from_millis(10));}
    println!("ready");std::io::stdout().flush()?;
    let stop=Arc::new(AtomicBool::new(false));let signal=stop.clone();
    thread::spawn(move||{let mut value=String::new();let _=std::io::stdin().read_line(&mut value);signal.store(true,Ordering::Release);});
    while !stop.load(Ordering::Acquire)&&unsafe{WaitForSingleObject(parent.0,0)}==WAIT_TIMEOUT&&picker_alive(&lease)&&lease.source.matches(){thread::sleep(Duration::from_millis(40));}
    Ok(())
}

#[cfg(test)]mod tests{
    use super::*;
    #[test]
    #[ignore="Owned offscreen HWNDs and the verified release guard executable"]
    fn native_picker_guard_restores_on_release_close_and_helper_failure()->Result<()>{
        let executable=PathBuf::from(std::env::var_os("ZSDESK_GUARD_TEST_EXE").context("verified release executable")?);
        ensure!(executable.is_absolute()&&executable.is_file(),"release executable required");
        let stop=Arc::new(AtomicBool::new(false));let signal=stop.clone();let(tx,rx)=mpsc::channel();
        let worker=thread::spawn(move||->Result<()>{
            struct Window(HWND);impl Drop for Window{fn drop(&mut self){let _=unsafe{DestroyWindow(self.0)};}}
            let source=Window(unsafe{CreateWindowExW(WS_EX_TOOLWINDOW|WS_EX_NOACTIVATE,w!("#32770"),w!("Owned picker guard fixture"),WS_POPUP,-30000,-30000,200,100,None,None,None,None)}?);
            let picker=Window(unsafe{CreateWindowExW(WS_EX_TOOLWINDOW|WS_EX_NOACTIVATE,w!("STATIC"),None,WS_POPUP,-30000,-30000,100,100,None,None,None,None)}?);
            unsafe{let _=ShowWindow(source.0,SW_SHOWNOACTIVATE);}let _=tx.send((source.0.0 as isize,picker.0.0 as isize));
            while !signal.load(Ordering::Acquire){let mut message=MSG::default();while unsafe{PeekMessageW(&mut message,None,0,0,PM_REMOVE)}.as_bool(){unsafe{let _=TranslateMessage(&message);DispatchMessageW(&message);}}thread::sleep(Duration::from_millis(5));}Ok(())
        });
        struct Ui{stop:Arc<AtomicBool>,worker:Option<JoinHandle<Result<()>>>}impl Drop for Ui{fn drop(&mut self){self.stop.store(true,Ordering::Release);if let Some(worker)=self.worker.take(){let _=worker.join();}}}
        let _ui=Ui{stop,worker:Some(worker)};let(source,picker)=rx.recv_timeout(Duration::from_secs(3))?;let(source,picker)=(HWND(source as*mut _),HWND(picker as*mut _));
        for mode in 0..3{
            let token=100+mode;unsafe{SetPropW(picker,crate::window::picker::MARKER,Some(HANDLE(token as usize as*mut _)))}?;
            let mut guard=Guard::hide_using(Identity::capture(source).context("source")?,picker.0 as isize,token,&executable)?;
            ensure!(!unsafe{IsWindowVisible(source)}.as_bool(),"guard did not hide source");
            if mode==0{drop(guard);}else{
                if mode==1{guard.child.kill()?;guard.child.wait()?;}else{let _=unsafe{RemovePropW(picker,crate::window::picker::MARKER)};}
                let end=Instant::now()+Duration::from_secs(3);while !unsafe{IsWindowVisible(source)}.as_bool(){ensure!(Instant::now()<end,"independent recovery did not restore source");thread::sleep(Duration::from_millis(10));}
                drop(guard);
            }
            let end=Instant::now()+Duration::from_secs(2);while !unsafe{IsWindowVisible(source)}.as_bool(){ensure!(Instant::now()<end,"source not restored");thread::sleep(Duration::from_millis(10));}
            ensure!(unsafe{GetPropW(source,PROPERTY)}.0.is_null(),"source lease was not removed");
        }Ok(())
    }
}
