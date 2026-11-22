//! Early common-dialog masking, independent of UI Automation and picker startup.
use super::*;
use std::{cell::RefCell,io::{BufRead,BufReader,Write},process::{Child,ChildStdin,Command,Stdio},sync::Mutex};
use windows::Win32::{Foundation::{COLORREF,HANDLE,WAIT_TIMEOUT},System::{SystemInformation::GetTickCount64,Threading::{GetCurrentProcess,OpenProcess,WaitForSingleObject,PROCESS_QUERY_LIMITED_INFORMATION,PROCESS_SYNCHRONIZE}}};
const MARKER:PCWSTR=w!("ZSDesk.EarlyFileDialog.v1");
const MASK:u32=WS_EX_LAYERED.0|WS_EX_TOOLWINDOW.0|WS_EX_APPWINDOW.0|WS_EX_TRANSPARENT.0;
const PREPARE_MS:u64=8000;
// Only a visible picker may keep the caller's dialog masked beyond this budget.
const MASK_MS:u64=2000;
const BOUND:PCWSTR=w!("ZSDesk.BoundFilePicker.v1");
const COMPLETE_MS:u64=2000;
fn now()->u64{unsafe{GetTickCount64()}}
#[derive(Clone,serde::Serialize,serde::Deserialize)]
struct Lease{source:guard::Identity,token:u64,style:u32,started:u64,masked_at:Option<u64>}
impl Lease{
    fn remaining(&self)->Option<Duration>{
        let remaining=MASK_MS.saturating_sub(now().saturating_sub(self.masked_at?));
        (remaining>0&&marked(self)).then_some(Duration::from_millis(remaining))
    }
}
#[derive(serde::Serialize,serde::Deserialize)]
enum Event{Ready,Prepared(Lease),Hidden(Lease),Released(u64)}
#[derive(serde::Serialize,serde::Deserialize)]
enum Control{Enable(bool),Heartbeat,Hide(u64),Bind(u64,isize),Complete(u64),Restore(u64),Shutdown}
type Pipe=Arc<Mutex<Option<ChildStdin>>>;
fn command(pipe:&Pipe,value:&Control)->Result<()>{
    let mut locked=pipe.lock().map_err(|_|anyhow::anyhow!("dialog guard lock"))?;
    let stream=locked.as_mut().context("dialog guard stopped")?;
    serde_json::to_writer(&mut *stream,value)?;stream.write_all(b"\n")?;stream.flush()?;Ok(())
}
fn marked(lease:&Lease)->bool{lease.source.matches()&&unsafe{GetPropW(lease.source.hwnd(),MARKER)}.0 as usize==lease.token as usize}
fn frame(window:HWND){let _=unsafe{SetWindowPos(window,None,0,0,0,0,SWP_NOMOVE|SWP_NOSIZE|SWP_NOZORDER|SWP_NOACTIVATE|SWP_FRAMECHANGED|SWP_ASYNCWINDOWPOS)};}
fn restore(lease:&Lease){
    if !marked(lease){return;}
    let window=lease.source.hwnd();let current=unsafe{GetWindowLongPtrW(window,GWL_EXSTYLE)}as u32;
    if current&WS_EX_LAYERED.0!=0{let _=unsafe{SetLayeredWindowAttributes(window,COLORREF(0),255,LWA_ALPHA)};}
    unsafe{SetWindowLongPtrW(window,GWL_EXSTYLE,((current&!MASK)|(lease.style&MASK))as isize);let _=RemovePropW(window,MARKER);}
    frame(window);
}
fn mask(lease:&Lease)->bool{
    let window=lease.source.hwnd();if !lease.source.matches()||!unsafe{GetPropW(window,MARKER)}.0.is_null(){return false;}
    let current=unsafe{GetWindowLongPtrW(window,GWL_EXSTYLE)}as u32;
    if current&WS_EX_LAYERED.0!=0||current&MASK!=lease.style&MASK{return false;}
    if unsafe{SetPropW(window,MARKER,Some(HANDLE(lease.token as usize as*mut _)))}.is_err(){return false;}
    unsafe{SetWindowLongPtrW(window,GWL_EXSTYLE,((current|WS_EX_LAYERED.0|WS_EX_TOOLWINDOW.0|WS_EX_TRANSPARENT.0)&!WS_EX_APPWINDOW.0)as isize);}
    if unsafe{SetLayeredWindowAttributes(window,COLORREF(0),0,LWA_ALPHA)}.is_err(){restore(lease);return false;}
    frame(window);true
}
pub(super) struct Claim{lease:Lease,pipe:Pipe,transferred:bool}
impl Claim{
    pub fn source(&self)->guard::Identity{self.lease.source.clone()}
    pub fn token(&self)->u64{self.lease.token}
    pub fn age_ms(&self)->u64{now().saturating_sub(self.lease.started)}
    pub fn active(&self)->bool{marked(&self.lease)}
    pub fn preparing(&self)->bool{self.lease.remaining().is_some()}
    pub fn show_check(&self)->impl Fn()->Option<Duration>+use<>{let lease=self.lease.clone();move||lease.remaining()}
    pub fn bind(&self,window:isize)->Result<()>{
        ensure!(self.preparing(),"原文件选择窗口已恢复");
        command(&self.pipe,&Control::Bind(self.token(),window))?;
        let picker=HWND(window as*mut _);
        // Wait for the watchdog to acknowledge before permitting first show.
        let until=Instant::now()+Duration::from_millis(100);
        while self.preparing()&&Instant::now()<until{
            if unsafe{GetPropW(picker,BOUND)}.0 as usize==self.token()as usize{return Ok(());}
            thread::sleep(Duration::from_millis(2));
        }
        anyhow::bail!("文件选择窗口接管已超时")
    }
    pub fn complete(mut self)->Result<()>{ensure!(self.active(),"原文件选择窗口已恢复");command(&self.pipe,&Control::Complete(self.token()))?;self.transferred=true;Ok(())}
}
impl Drop for Claim{fn drop(&mut self){if !self.transferred{let _=command(&self.pipe,&Control::Restore(self.token()));restore(&self.lease);}}}
pub(super) struct Connection{pipe:Pipe,events:mpsc::Receiver<Lease>,child:Child,reader:Option<JoinHandle<()>>}
impl Connection{
    pub fn spawn()->Result<Self>{Self::spawn_using(&std::env::current_exe()?)}
    pub(super) fn spawn_using(executable:&Path)->Result<Self>{Self::spawn_for(executable,None)}
    fn spawn_for(executable:&Path,target:Option<u32>)->Result<Self>{
        use std::os::windows::process::CommandExt;
        let identity=guard::process_created(unsafe{GetCurrentProcess()}).context("parent identity")?;
        let mut process=Command::new(executable);process.args(["--file-dialog-watcher",&std::process::id().to_string(),&identity.to_string()]);
        if let Some(target)=target{process.arg(target.to_string());}
        process.creation_flags(0x08000000).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        #[cfg(test)]if let Some(output)=std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT"){
            let output=PathBuf::from(output);std::fs::create_dir_all(&output)?;process.stderr(std::fs::File::create(output.join("watcher.current.log"))?);
        }
        let mut child=process.spawn()?;
        let pipe=Arc::new(Mutex::new(child.stdin.take()));let input=pipe.clone();let stdout=child.stdout.take().context("dialog watcher output")?;
        let(tx,events)=mpsc::channel();let(ready_tx,ready_rx)=mpsc::sync_channel(1);
        let reader=thread::spawn(move||{
            let mut known=HashMap::<u64,Lease>::new();
            for line in BufReader::new(stdout).lines(){
                let Ok(line)=line else{break;};if line=="ready"{let _=ready_tx.try_send(true);continue;}
                if line.len()>8192{break;}let Ok(event)=serde_json::from_str::<Event>(&line)else{break;};
                match event{
                    Event::Ready=>{let _=ready_tx.try_send(true);},
                    Event::Prepared(lease)=>{let token=lease.token;known.insert(token,lease);let _=command(&input,&Control::Hide(token));},
                    Event::Hidden(lease)=>{if tx.send(lease).is_err(){break;}},
                    Event::Released(token)=>{known.remove(&token);},
                }
            }
            for lease in known.values(){restore(lease);}
        });
        let result=Self{pipe,events,child,reader:Some(reader)};
        ensure!(ready_rx.recv_timeout(Duration::from_secs(3)).unwrap_or(false),"文件选择接管监听未就绪");Ok(result)
    }
    pub fn enable(&self,value:bool)->Result<()>{command(&self.pipe,&Control::Enable(value))}
    pub fn heartbeat(&self)->Result<()>{command(&self.pipe,&Control::Heartbeat)}
    pub fn exited(&mut self)->bool{self.child.try_wait().ok().flatten().is_some()}
    pub fn claims(&self)->Vec<Claim>{self.events.try_iter().map(|lease|Claim{lease,pipe:self.pipe.clone(),transferred:false}).collect()}
}
impl Drop for Connection{fn drop(&mut self){
    let _=command(&self.pipe,&Control::Shutdown);if let Ok(mut pipe)=self.pipe.lock(){pipe.take();}
    let end=Instant::now()+Duration::from_secs(2);while self.child.try_wait().ok().flatten().is_none()&&Instant::now()<end{thread::sleep(Duration::from_millis(10));}
    if self.child.try_wait().ok().flatten().is_none(){let _=self.child.kill();let _=self.child.wait();}
    if let Some(reader)=self.reader.take(){let _=reader.join();}
}}
struct BoundPicker{window:isize,shown:bool}
struct Entry{lease:Lease,hidden:bool,picker:Option<BoundPicker>,finishing:Option<u64>}
struct Watcher{parent:u32,target:Option<u32>,enabled:bool,heartbeat:u64,serial:u64,known:HashSet<isize>,pending:HashMap<isize,u64>,entries:HashMap<u64,Entry>,output:mpsc::SyncSender<Event>}
thread_local!{
    static WATCHER:RefCell<Option<Watcher>>=const{RefCell::new(None)};
    static COMMANDS:RefCell<Option<mpsc::Receiver<Control>>>=const{RefCell::new(None)};
}
fn process_commands()->bool{
    let values=COMMANDS.with(|slot|slot.borrow().as_ref().map(|rx|rx.try_iter().collect::<Vec<_>>()).unwrap_or_default());
    for value in values{let running=WATCHER.with(|slot|slot.borrow_mut().as_mut().is_some_and(|watcher|watcher.control(value)));if !running{return false;}}true
}
unsafe extern "system" fn early_message(window:HWND,message:u32,wp:WPARAM,lp:LPARAM)->windows::Win32::Foundation::LRESULT{
    if message!=crate::early_window_hook::MESSAGE{return unsafe{DefWindowProcW(window,message,wp,lp)};}
    let source=HWND(wp.0 as*mut _);
    // Cross-process style changes can synchronously reenter this endpoint.
    let accepted=WATCHER.with(|slot|{let Ok(mut slot)=slot.try_borrow_mut()else{return false;};
        if let Some(watcher)=slot.as_mut(){watcher.observe(source);watcher.prepare(source.0 as isize);true}else{false}});
    if !accepted{return windows::Win32::Foundation::LRESULT(0);}
    let until=Instant::now()+Duration::from_millis(50);
    loop{
        if !process_commands(){break;}
        let state=WATCHER.with(|slot|slot.borrow().as_ref().and_then(|watcher|watcher.entries.values().find(|entry|entry.lease.source.window==source.0 as isize).map(|entry|entry.hidden)));
        if state!=Some(false)||Instant::now()>=until{break;}thread::sleep(Duration::from_millis(1));
    }
    windows::Win32::Foundation::LRESULT(0)
}
impl Watcher{
    fn release(&mut self,token:u64){if let Some(entry)=self.entries.remove(&token){restore(&entry.lease);let _=self.output.try_send(Event::Released(token));}}
    fn clear(&mut self){let tokens:Vec<_>=self.entries.keys().copied().collect();for token in tokens{self.release(token);}self.pending.clear();}
    fn observe(&mut self,window:HWND){
        if !self.enabled{return;}let root=unsafe{GetAncestor(window,GA_ROOT)};
        if root.0.is_null()||class(root)!="#32770"{return;}let key=root.0 as isize;
        if self.known.contains(&key)||self.entries.len()>=4{return;}
        let mut pid=0;unsafe{GetWindowThreadProcessId(root,Some(&mut pid));}
        if pid==0||pid==self.parent||pid==std::process::id()||self.target.is_some_and(|target|target!=pid){return;}
        self.pending.entry(key).or_insert_with(now);
    }
    fn prepare(&mut self,key:isize){
        let Some(stamp)=self.pending.get(&key).copied()else{return;};
        let window=HWND(key as*mut _);
        if !unsafe{IsWindow(Some(window))}.as_bool()||now().saturating_sub(stamp)>PREPARE_MS{self.pending.remove(&key);return;}
        if self.entries.len()>=4{return;}
        let part=parts(window);
        // The filename and acceptance controls exist before the Shell
        // view finishes loading; masking must not wait for that view.
        let accept=accept_label(window).unwrap_or_default();
        let label=control_label(&accept);
        let provisional=part.native&&(open_label(&label)||matches!(label.as_str(),"上传"|"upload")
            ||label.starts_with("选择文件夹")||label.starts_with("select folder"));
        if !provisional&&(part.name.0.is_null()||selection_kind_for(window,&part,&accept,None).is_none()){return;}
        self.pending.remove(&key);self.known.insert(key);
        let Some(source)=guard::Identity::capture(window)else{return;};let style=unsafe{GetWindowLongPtrW(window,GWL_EXSTYLE)}as u32;
        if style&WS_EX_LAYERED.0!=0||!unsafe{IsWindowEnabled(window)}.as_bool(){return;}
        self.serial=self.serial.wrapping_add(1).max(1);let token=self.serial;
        let lease=Lease{source,token,style,started:now(),masked_at:None};
        if self.output.try_send(Event::Prepared(lease.clone())).is_ok(){self.entries.insert(token,Entry{lease,hidden:false,picker:None,finishing:None});}
    }
    fn tick(&mut self){
        let mut expired=Vec::new();
        for (token,entry) in &mut self.entries{
            if !entry.lease.source.matches(){expired.push(*token);continue;}
            if let Some(picker)=entry.picker.as_mut(){
                let window=HWND(picker.window as*mut _);
                if unsafe{GetPropW(window,crate::window::picker::MARKER)}.0 as usize!=*token as usize{
                    if picker.shown{entry.finishing.get_or_insert(now());}
                }else if !picker.shown&&unsafe{IsWindowVisible(window)}.as_bool(){picker.shown=true;}
            }
            // Validation can temporarily disable the caller. A live custom
            // picker remains the visible owner until acceptance or cancellation.
            if entry.finishing.is_some_and(|at|now().saturating_sub(at)>=COMPLETE_MS)
                ||(entry.picker.as_ref().is_none_or(|picker|!picker.shown)&&entry.finishing.is_none()&&entry.lease.masked_at.map_or_else(
                    ||now().saturating_sub(entry.lease.started)>=PREPARE_MS,
                    |at|now().saturating_sub(at)>=MASK_MS)){expired.push(*token);}
        }
        for token in expired{self.release(token);}
        self.known.retain(|key|unsafe{IsWindow(Some(HWND(*key as*mut _)))}.as_bool());
        let pending:Vec<_>=self.pending.keys().copied().collect();
        for key in pending{self.prepare(key);}
    }
    fn control(&mut self,value:Control)->bool{match value{
        Control::Enable(enabled)=>{self.heartbeat=now();if self.enabled!=enabled{self.clear();self.enabled=enabled;self.known.clear();
            // Baseline existing dialogs; switching a setting never steals one.
            unsafe extern "system" fn visit(window:HWND,value:LPARAM)->BOOL{if class(window)=="#32770"{unsafe{(*(value.0 as*mut HashSet<isize>)).insert(window.0 as isize);}}true.into()}
            let _=unsafe{EnumWindows(Some(visit),LPARAM(&mut self.known as*mut _ as isize))};}},
        Control::Heartbeat=>self.heartbeat=now(),
        Control::Hide(token)=>{if let Some(entry)=self.entries.get_mut(&token){if !entry.hidden&&now().saturating_sub(entry.lease.started)<PREPARE_MS{
            let start=now();
            if mask(&entry.lease){entry.hidden=true;entry.lease.masked_at=Some(start);
                if self.output.try_send(Event::Hidden(entry.lease.clone())).is_ok(){return true;}}
        }}self.release(token);},
        Control::Bind(token,picker)=>{if let Some(entry)=self.entries.get_mut(&token){let window=HWND(picker as*mut _);let mut pid=0;unsafe{GetWindowThreadProcessId(window,Some(&mut pid));}
            if pid==self.parent&&entry.lease.remaining().is_some()
                &&unsafe{GetPropW(window,crate::window::picker::MARKER)}.0 as usize==token as usize
                &&unsafe{SetPropW(window,BOUND,Some(HANDLE(token as usize as*mut _)))}.is_ok(){entry.picker=Some(BoundPicker{window:picker,shown:false});return true;}}self.release(token);},
        Control::Complete(token)=>{if let Some(entry)=self.entries.get_mut(&token){entry.finishing=Some(now());}},
        Control::Restore(token)=>self.release(token),
        Control::Shutdown=>return false,
    }true}
}
unsafe extern "system" fn event(_:HWINEVENTHOOK,kind:u32,window:HWND,object:i32,child:i32,_:u32,_:u32){
    if object!=OBJID_WINDOW.0||child!=0{return;}
    WATCHER.with(|slot|{if let Ok(mut slot)=slot.try_borrow_mut(){if let Some(watcher)=slot.as_mut(){
        if kind==EVENT_OBJECT_DESTROY{watcher.known.remove(&(window.0 as isize));watcher.pending.remove(&(window.0 as isize));}else{watcher.observe(window);}
    }}});
}
pub(super) fn run_if_requested()->Option<Result<()>>{
    (std::env::args_os().nth(1).as_deref()==Some(std::ffi::OsStr::new("--file-dialog-watcher"))).then(run)
}
fn run()->Result<()>{
    let parent_pid=std::env::args().nth(2).context("parent")?.parse()?;let stamp:u64=std::env::args().nth(3).context("parent creation")?.parse()?;
    let parent=guard::OwnedHandle(unsafe{OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION|PROCESS_SYNCHRONIZE,false,parent_pid)}?);
    ensure!(guard::process_created(parent.0)==Some(stamp),"dialog watcher parent changed");
    let(tx,commands)=mpsc::channel();let dead=Arc::new(AtomicBool::new(false));let signal=dead.clone();
    thread::spawn(move||{for line in std::io::stdin().lock().lines(){let Ok(line)=line else{break;};if line.len()>8192{break;}
        let Ok(value)=serde_json::from_str::<Control>(&line)else{break;};if tx.send(value).is_err(){break;}}signal.store(true,Ordering::Release);});
    let(output,events)=mpsc::sync_channel(32);let signal=dead.clone();
    thread::spawn(move||{let mut stream=std::io::stdout().lock();
        for event in events{if serde_json::to_writer(&mut stream,&event).is_err()||stream.write_all(b"\n").is_err()||stream.flush().is_err(){break;}}signal.store(true,Ordering::Release);});
    let target=std::env::args().nth(4).map(|value|value.parse()).transpose()?;
    WATCHER.with(|slot|*slot.borrow_mut()=Some(Watcher{parent:parent_pid,target,enabled:false,heartbeat:now(),serial:now().saturating_mul(65536),known:HashSet::new(),pending:HashMap::new(),entries:HashMap::new(),output}));
    COMMANDS.with(|slot|*slot.borrow_mut()=Some(commands));
    struct Recovery;impl Drop for Recovery{fn drop(&mut self){WATCHER.with(|slot|{if let Some(mut watcher)=slot.borrow_mut().take(){watcher.clear();}});}}let _recovery=Recovery;
    let _early=crate::early_window_hook::Hook::install(w!("ZSDesk.EarlyDialog.Guard"),Some(early_message),false)?;
    struct Hooks(Vec<HWINEVENTHOOK>);impl Drop for Hooks{fn drop(&mut self){for hook in &self.0{unsafe{UnhookWinEvent(*hook);}}}}let mut hooks=Hooks(Vec::new());
    for kind in [EVENT_OBJECT_CREATE,EVENT_OBJECT_SHOW,EVENT_OBJECT_NAMECHANGE,EVENT_SYSTEM_FOREGROUND,EVENT_OBJECT_DESTROY]{let hook=unsafe{SetWinEventHook(kind,kind,None,Some(event),0,0,WINEVENT_OUTOFCONTEXT|WINEVENT_SKIPOWNPROCESS)};ensure!(!hook.0.is_null(),"file dialog event hook unavailable");hooks.0.push(hook);}
    WATCHER.with(|slot|{if let Some(watcher)=slot.borrow_mut().as_mut(){watcher.control(Control::Enable(true));let _=watcher.output.try_send(Event::Ready);}});
    while !dead.load(Ordering::Acquire)&&unsafe{WaitForSingleObject(parent.0,0)}==WAIT_TIMEOUT{
        if !process_commands(){break;}
        let mut message=MSG::default();for _ in 0..512{if !unsafe{PeekMessageW(&mut message,None,0,0,PM_REMOVE)}.as_bool(){break;}unsafe{let _=TranslateMessage(&message);DispatchMessageW(&message);}}
        let healthy=WATCHER.with(|slot|{let mut slot=slot.borrow_mut();let watcher=slot.as_mut().unwrap();watcher.tick();now().saturating_sub(watcher.heartbeat)<PREPARE_MS+2000});if !healthy{break;}
        thread::sleep(Duration::from_millis(5));
    }Ok(())
}

#[cfg(test)]mod tests{
    use super::*;
    use std::{fs,os::windows::process::CommandExt};
    #[test]
    #[ignore="Owned source process for native early-dialog tests"]
    fn native_early_dialog_source()->Result<()>{
        let root=PathBuf::from(std::env::var_os("ZSDESK_DIALOG_FIXTURE").context("fixture")?);
        let until=Instant::now()+Duration::from_secs(10);while !root.join("go").exists(){ensure!(Instant::now()<until,"fixture start timeout");thread::sleep(Duration::from_millis(5));}
        let(tx,rx)=mpsc::channel::<isize>();let output=root.clone();let reporter=thread::spawn(move||{if let Ok(window)=rx.recv_timeout(Duration::from_secs(10)){let _=fs::write(output.join("source.json"),window.to_string());}});
        let mode=std::env::var("ZSDESK_DIALOG_KIND").unwrap_or_default();
        let kind=if mode=="folder"{SelectionKind::Folders}else{SelectionKind::Files};
        let result=super::super::native_tests::original_with_mode(root.clone(),tx,kind,(mode!="single-image").then_some("上传(&U)"),mode=="single-image")?;
        let _=reporter.join();fs::write(root.join("result.json"),serde_json::to_vec(&result)?)?;
        fs::write(root.join("activation-alpha.json"),super::super::native_tests::FIRST_ACTIVATION_ALPHA.load(Ordering::Relaxed).to_string())?;Ok(())
    }
    fn wait_until(mut predicate:impl FnMut()->bool,message:&str)->Result<()>{let end=Instant::now()+Duration::from_secs(10);while !predicate(){ensure!(Instant::now()<end,"{message}");thread::sleep(Duration::from_millis(5));}Ok(())}
    struct Source(Child,PathBuf);
    impl Source{
        fn spawn(root:&Path,mode:&str)->Result<Self>{
            fs::create_dir_all(root)?;ensure!(!root.join("go").exists(),"fresh native fixture required");
            fs::write(root.join("选择 中文.txt"),b"fixture")?;fs::create_dir_all(root.join("selected folder"))?;
            let log=fs::File::create(root.join("source.log"))?;
            let child=Command::new(std::env::current_exe()?).args(["native_early_dialog_source","--ignored","--nocapture","--test-threads=1"])
                .env("ZSDESK_DIALOG_FIXTURE",root).env("ZSDESK_DIALOG_KIND",mode).stdout(log.try_clone()?).stderr(log).creation_flags(0x08000000).spawn()?;
            Ok(Self(child,root.to_path_buf()))
        }
    }
    impl Drop for Source{fn drop(&mut self){
        if let Ok(value)=fs::read_to_string(self.1.join("source.json")){if let Ok(window)=value.parse::<isize>(){let hwnd=HWND(window as*mut _);let mut pid=0;unsafe{GetWindowThreadProcessId(hwnd,Some(&mut pid));}if pid==self.0.id(){let _=unsafe{PostMessageW(Some(hwnd),WM_COMMAND,WPARAM(2),LPARAM(0))};}}}
        let end=Instant::now()+Duration::from_secs(3);while self.0.try_wait().ok().flatten().is_none()&&Instant::now()<end{thread::sleep(Duration::from_millis(10));}
        if self.0.try_wait().ok().flatten().is_none(){let _=self.0.kill();let _=self.0.wait();}
    }}
    #[test]
    #[ignore="Requires isolated Windows desktop, Edge and a local DevTools fixture"]
    fn browser_upload_picker_first_show()->Result<()>{
        let root=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("output")?);
        let executable=PathBuf::from(std::env::var_os("ZSDESK_GUARD_TEST_EXE").context("watcher executable")?);
        let edge=std::env::var_os("ZSDESK_TEST_EDGE").context("Edge executable")?;
        let script=Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/test-edge-file-picker.mjs");
        let edge_profile=root.join("edge-profile");
        struct Browser(Child);impl Drop for Browser{fn drop(&mut self){let _=self.0.kill();let _=self.0.wait();}}
        let _browser=Browser(Command::new(edge).arg(format!("--user-data-dir={}",edge_profile.display()))
            .args(["--remote-debugging-port=0","--no-first-run","--no-default-browser-check","--disable-background-networking",
                "data:text/html,<input%20type=file%20accept=image/*%20multiple>"]).creation_flags(0x08000000).spawn()?);
        let profile=root.join("explorer-session.json");
        let mut session=crate::storage::default_session();session.ui.file_dialog_takeover=true;
        crate::storage::save(&profile,&session)?;
        let status=root.join("file-dialog-takeover-status.json");
        let read_status=||fs::read(&status).ok().and_then(|bytes|serde_json::from_slice::<serde_json::Value>(&bytes).ok()).unwrap_or_default();
        let stop=AtomicBool::new(false);
        thread::scope(|scope|->Result<()>{
            struct StopOnDrop<'a>(&'a AtomicBool);
            impl Drop for StopOnDrop<'_>{fn drop(&mut self){self.0.store(true,Ordering::Release);}}
            // Edge can run IFileOpenDialog in a separate utility process.
            let bridge=scope.spawn(||run_with_watcher(&profile,&stop,||Connection::spawn_for(&executable,None)));
            let _stop=StopOnDrop(&stop);
            wait_until(||read_status()["early_masking"]==true,"bridge watcher was not ready")?;
            let port_file=edge_profile.join("DevToolsActivePort");
            let devtools=|args:&[&str]|->Result<String>{
                let result=Command::new("node").arg(&script).arg(&port_file).args(args).creation_flags(0x08000000).output()?;
                ensure!(result.status.success(),"browser fixture failed: {}",String::from_utf8_lossy(&result.stderr));
                Ok(String::from_utf8(result.stdout)?.trim().to_owned())
            };
            let picker_visible=||{
            let mut picker=HWND::default();
                unsafe extern "system" fn find(window:HWND,slot:LPARAM)->BOOL{
                    let mut owner=0;unsafe{GetWindowThreadProcessId(window,Some(&mut owner));}
                    if owner==std::process::id()&&unsafe{IsWindowVisible(window)}.as_bool()&&!unsafe{GetPropW(window,crate::window::picker::MARKER)}.0.is_null(){
                        unsafe{*(slot.0 as*mut HWND)=window;}return false.into();
                    }true.into()
                }
                let _=unsafe{EnumWindows(Some(find),LPARAM(&mut picker as*mut _ as isize))};picker
            };
            let native_window=||{
                unsafe extern "system" fn find(window:HWND,value:LPARAM)->BOOL{
                    if class(window)=="#32770"&&unsafe{IsWindowVisible(window)}.as_bool()&&control_visible(parts(window).types){
                        unsafe{*(value.0 as *mut HWND)=window};return false.into();
                    }true.into()
                }
                let mut window=HWND::default();
                let _=unsafe{EnumWindows(Some(find),LPARAM(&mut window as*mut _ as isize))};window
            };
            for mode in ["cancel","select","native"]{
                let clicked=devtools(&["open"])?.parse::<u128>()?;
                let mut picker=HWND::default();
                wait_until(||{picker=picker_visible();!picker.0.is_null()},"ZSDesk browser picker did not become visible")
                    .with_context(||format!("bridge status: {}",read_status()))?;
                let visible=std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis();
                eprintln!("Edge {mode}: visible {} ms after click; {}",visible.saturating_sub(clicked),read_status());
                match mode{
                    "select"=>{
                        let first=root.join("first.png");let second=root.join("second.png");
                        for file in [&first,&second]{fs::write(file,b"local selection fixture")?;}
                        // Exercise the real picker footer and original Edge
                        // caller. The data URL has no upload endpoint.
                        let edit=unsafe{GetDlgItem(Some(picker),crate::window::picker::NAME as i32)}?;
                        let names=wide(&format!("\"{}\" \"{}\"",first.display(),second.display()));
                        ensure!(send(edit,WM_SETTEXT,0,names.as_ptr()as isize).is_some_and(|result|result!=0),"file names not set");
                        // The isolated desktop cannot assign keyboard focus.
                        unsafe{PostMessageW(Some(picker),WM_COMMAND,WPARAM(crate::window::picker::TEST_ACCEPT_EDIT),LPARAM(0))}?;
                        wait_until(||read_status()["last_confirmation"]=="accepted","Edge did not accept selection")?;
                        assert_eq!(devtools(&["files"])?,r#"["first.png","second.png"]"#);
                    }
                    "native"=>{
                        let source=native_window();ensure!(!source.0.is_null(),"masked Edge dialog missing");
                        let button=unsafe{GetDlgItem(Some(picker),crate::window::picker::USE_NATIVE as i32)}?;
                        unsafe{PostMessageW(Some(picker),WM_COMMAND,WPARAM(crate::window::picker::USE_NATIVE),LPARAM(button.0 as isize))}?;
                        wait_until(||picker_visible().0.is_null()&&read_status()["last_handoff"]["stage"]=="native-requested","Edge native switch did not finish")?;
                        ensure!(native_window()==source&&unsafe{GetPropW(source,MARKER)}.0.is_null(),"original Edge dialog was not restored");
                        native_choice(source,2)?;
                    }
                    _=>{
                        unsafe{PostMessageW(Some(picker),WM_CLOSE,WPARAM(0),LPARAM(0))}?;
                        wait_until(||read_status()["last_confirmation"]=="cancelled","bridge did not confirm browser cancellation")?;
                    }
                }
                wait_until(||picker_visible().0.is_null()&&native_window().0.is_null(),"previous browser picker did not close")?;
            }
            stop.store(true,Ordering::Release);
            bridge.join().map_err(|_|anyhow::anyhow!("bridge panicked"))?
        })
    }
    #[test]
    #[ignore="Live picker and isolated native upload fixture; use a separate Windows desktop"]
    fn native_upload_picker_first_show()->Result<()>{
        live_native_picker("files")
    }
    #[test]
    #[ignore="Single-image picker with a profile display name; use a separate Windows desktop"]
    fn native_qq_style_image_picker()->Result<()>{
        live_native_picker("single-image")
    }
    fn live_native_picker(mode:&str)->Result<()>{
        let root=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("output")?).join("live-upload");
        let executable=PathBuf::from(std::env::var_os("ZSDESK_GUARD_TEST_EXE").context("guard executable")?);
        let mut source=Source::spawn(&root,mode)?;
        let pid=source.0.id();let profile=root.join("explorer-session.json");
        let mut session=crate::storage::default_session();session.ui.file_dialog_takeover=true;
        crate::storage::save(&profile,&session)?;
        let status=root.join("file-dialog-takeover-status.json");
        let read_status=||fs::read(&status).ok().and_then(|bytes|serde_json::from_slice::<serde_json::Value>(&bytes).ok()).unwrap_or_default();
        let stop=AtomicBool::new(false);
        thread::scope(|scope|->Result<()>{
            struct StopOnDrop<'a>(&'a AtomicBool);
            impl Drop for StopOnDrop<'_>{fn drop(&mut self){self.0.store(true,Ordering::Release);}}
            let bridge=scope.spawn(||run_with_watcher(&profile,&stop,||Connection::spawn_for(&executable,Some(pid))));
            let _stop=StopOnDrop(&stop);
            wait_until(||read_status()["early_masking"]==true,"bridge watcher was not ready")?;
            let opened=Instant::now();fs::write(root.join("go"),b"go")?;
            let mut picker=HWND::default();
            wait_until(||{
                unsafe extern "system" fn find(window:HWND,slot:LPARAM)->BOOL{
                    let mut pid=0;unsafe{GetWindowThreadProcessId(window,Some(&mut pid));}
                    if pid==std::process::id()&&unsafe{IsWindowVisible(window)}.as_bool()
                        &&!unsafe{GetPropW(window,crate::window::picker::MARKER)}.0.is_null(){
                        unsafe{*(slot.0 as*mut HWND)=window;}return false.into();
                    }true.into()
                }
                let _=unsafe{EnumWindows(Some(find),LPARAM(&mut picker as*mut _ as isize))};
                !picker.0.is_null()
            },"ZSDesk picker did not become visible").with_context(||format!("bridge status: {}",read_status()))?;
            let visible_ms=opened.elapsed().as_millis();
            eprintln!("native upload picker visible after {visible_ms} ms");
            let single=mode=="single-image";
            let filename=root.join("avatar.png");
            if single{
                fs::write(&filename,b"local image selection fixture")?;
                let edit=unsafe{GetDlgItem(Some(picker),crate::window::picker::NAME as i32)}?;
                let name=wide(&format!("\"{}\"",filename.display()));
                ensure!(send(edit,WM_SETTEXT,0,name.as_ptr()as isize).is_some_and(|result|result!=0),"file name not set");
                unsafe{PostMessageW(Some(picker),WM_COMMAND,WPARAM(crate::window::picker::TEST_ACCEPT_EDIT),LPARAM(0))}?;
            }else{unsafe{PostMessageW(Some(picker),WM_CLOSE,WPARAM(0),LPARAM(0))}?;}
            wait_until(||source.0.try_wait().ok().flatten().is_some(),"native caller did not close").with_context(||format!("bridge status: {}",read_status()))?;
            ensure!(source.0.wait()?.success(),"source failed");
            let result:serde_json::Value=serde_json::from_slice(&fs::read(root.join("result.json"))?)?;
            if single{
                ensure!(result[2]==false&&result[0]==serde_json::json!([filename]),"native caller received wrong image");
            }else{ensure!(result[2]==true,"native caller did not receive cancellation");}
            let confirmation=if single{"accepted"}else{"cancelled"};
            wait_until(||read_status()["last_confirmation"]==confirmation,"bridge did not confirm result")?;
            stop.store(true,Ordering::Release);
            bridge.join().map_err(|_|anyhow::anyhow!("bridge panicked"))?
        })
    }
    #[test]
    #[ignore="Owned offscreen native upload dialogs and separate watcher process"]
    fn native_early_dialog_handoff_and_recovery()->Result<()>{
        let output=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("output")?).join("early-dialog");
        let executable=PathBuf::from(std::env::var_os("ZSDESK_GUARD_TEST_EXE").context("guard executable")?);
        unsafe{CoInitializeEx(None,COINIT_MULTITHREADED)}.ok()?;
        struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{CoUninitialize();}}}let _apartment=Apartment;
        let automation:IUIAutomation=unsafe{CoCreateInstance(&CUIAutomation,None,CLSCTX_INPROC_SERVER)}?;
        let mut reports=Vec::new();
        for mode in ["files","folder","cancel","disable","guard-failure","unclaimed","bound-hidden"]{
            let root=output.join(mode);let mut source=Source::spawn(&root,mode)?;
            let mut watcher=Connection::spawn_for(&executable,Some(source.0.id()))?;watcher.enable(true)?;thread::sleep(Duration::from_millis(30));fs::write(root.join("go"),b"go")?;
            let mut claim=None;wait_until(||{watcher.heartbeat().ok();claim=watcher.claims().pop();claim.is_some()},"dialog was not masked")?;
            let claim=claim.unwrap();let identity=claim.source();let window=identity.hwnd();let token=claim.token();
            ensure!(identity.pid==source.0.id()&&claim.active(),"wrong native source");
            let mut alpha=255;unsafe{GetLayeredWindowAttributes(window,None,Some(&mut alpha),None)}?;ensure!(alpha==0,"source painted through early mask");
            let mask_elapsed=now().saturating_sub(claim.lease.started);
            let mut first_show=0usize;wait_until(||{first_show=unsafe{GetPropW(window,w!("ZSDesk.Test.FirstShowAlpha"))}.0 as usize;first_show!=0},"initial show notification missing")?;
            ensure!(first_show==1,"system dialog was opaque on its initial show: {mode}, alpha={}",first_show-1);
            let mut recovered_ms=None;
            if matches!(mode,"disable"|"guard-failure"|"unclaimed"|"bound-hidden"){
                struct HiddenPicker(HWND);
                impl Drop for HiddenPicker{fn drop(&mut self){let _=unsafe{RemovePropW(self.0,BOUND)};let _=unsafe{RemovePropW(self.0,crate::window::picker::MARKER)};let _=unsafe{DestroyWindow(self.0)};}}
                let _picker=if mode=="bound-hidden"{
                    let picker=HiddenPicker(unsafe{CreateWindowExW(WS_EX_TOOLWINDOW,w!("Static"),None,WS_POPUP,-30000,-30000,1,1,None,None,None,None)}?);
                    unsafe{SetPropW(picker.0,crate::window::picker::MARKER,Some(HANDLE(token as usize as*mut _)))}?;
                    claim.bind(picker.0.0 as isize)?;
                    Some(picker)
                }else{None};
                if mode=="disable"{watcher.enable(false)?;}else if mode=="guard-failure"{watcher.child.kill()?;watcher.child.wait()?;}
                wait_until(||{if matches!(mode,"unclaimed"|"bound-hidden"){watcher.heartbeat().ok();}!claim.active()},"original dialog was not restored")?;
                let elapsed=now().saturating_sub(claim.lease.masked_at.unwrap());
                recovered_ms=Some(elapsed);
                ensure!(elapsed<=MASK_MS+500,"native dialog stayed masked for {elapsed} ms: {mode}");
                ensure!((claim.show_check())().is_none(),"a released lease still permits a picker to show: {mode}");
                ensure!(unsafe{IsWindow(Some(window))}.as_bool()&&unsafe{GetWindowLongPtrW(window,GWL_EXSTYLE)}as u32&MASK==claim.lease.style&MASK,"recovery changed native styles");
                unsafe{PostMessageW(Some(window),WM_COMMAND,WPARAM(2),LPARAM(0))}?;drop(claim);
            }else{
                let part=parts(window);let kind=dialog_kind(window,&part,Some(&automation)).context("masked dialog contract")?;
                let mut selection_mode=None;wait_until(||{selection_mode=multiple(&automation,parts(window).shell);selection_mode.is_some()},"masked multi-selection")?;
                let multiple=selection_mode.unwrap();
                let accept=unsafe{GetDlgItem(Some(window),1)}.ok().and_then(text).context("accept")?;
                let mut session=Session{source:identity,name:part.name,types:if kind.folders(){HWND::default()}else{part.types},kind,multiple,accept,token,picker:None,guard:None,early:Some(claim),worker:None,fallback:false};
                let outcome=if mode=="cancel"{Outcome::Cancelled}else{Outcome::Selected{paths:vec![root.join(if kind.folders(){"selected folder"}else{"选择 中文.txt"})],filter:0}};
                session.finish(outcome)?;
                // Completion must not unmask the original dialog even for a
                // single frame while the caller validates and returns.
                wait_until(||{
                    if !unsafe{IsWindow(Some(window))}.as_bool(){return true;}
                    let mut alpha=255;let masked=unsafe{GetLayeredWindowAttributes(window,None,Some(&mut alpha),None)}.is_ok()&&alpha==0;
                    if !masked{panic!("original dialog flashed during completion: {mode}");}false
                },"caller did not complete")?;
            }
            wait_until(||source.0.try_wait().ok().flatten().is_some(),"source did not exit")?;
            ensure!(source.0.wait()?.success(),"source failed");
            let result:serde_json::Value=serde_json::from_slice(&fs::read(root.join("result.json"))?)?;
            ensure!(result[2]==serde_json::json!(!matches!(mode,"files"|"folder")),"caller cancellation contract changed");
            let activation_alpha=fs::read_to_string(root.join("activation-alpha.json"))?;
            reports.push(serde_json::json!({"mode":mode,"masked_ms_after_claim":mask_elapsed,"recovered_ms":recovered_ms,"first_show_alpha":first_show-1,"first_activation_alpha":activation_alpha,"result":result,"passed":true}));
        }
        fs::write(output.join("verification.current.json"),serde_json::to_vec_pretty(&reports)?)?;Ok(())
    }
}
