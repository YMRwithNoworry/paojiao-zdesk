//! Standard folder activation into the existing default-profile host.
use std::{cell::{Cell,RefCell},collections::HashSet,ffi::OsString,path::{Path,PathBuf},rc::Rc};
use anyhow::{Result,ensure};
use windows::{core::PCWSTR,Win32::System::DataExchange::*};
use zsdesk_windows::integration;

const MAX_BYTES:usize=65536;
const MAX_CONVERSATIONS:usize=64;
thread_local!{static ACTIVE:RefCell<Option<Rc<State>>>=const{RefCell::new(None)};}

fn same_absolute(first:&Path,second:&Path)->bool{
    std::path::absolute(first).ok().zip(std::path::absolute(second).ok())
        .is_some_and(|(a,b)|crate::explorer_forwarder::same_path(&a,&b))
}
fn authorized(profile:&Path,default:&Path,expected_exe:&Path,current_exe:&Path,owned:bool)->bool{
    owned&&same_absolute(profile,default)&&same_absolute(expected_exe,current_exe)
}

/// Execute packets contain one NUL-terminated UTF-16LE Open command. DDE data
/// padding may contain only zero units; extra commands cannot hide after NUL.
fn parse_packet(bytes:&[u8])->Option<PathBuf>{
    if bytes.len()<2||bytes.len()>MAX_BYTES||bytes.len()%2!=0{return None;}
    let units=bytes.chunks_exact(2).map(|value|u16::from_le_bytes([value[0],value[1]])).collect::<Vec<_>>();
    let end=units.iter().position(|unit|*unit==0)?;
    if units[end..].iter().any(|unit|*unit!=0){return None;}
    let command=String::from_utf16(&units[..end]).ok()?;
    let directory=command.strip_prefix("[Open(\"")?.strip_suffix("\")]")?;
    if directory.is_empty()||directory.chars().any(|c|c=='"'||c.is_control()){return None;}
    let directory=PathBuf::from(directory);directory.is_absolute().then_some(directory)
}

struct State{
    service:HSZ,topic:HSZ,live:Cell<bool>,executing:Cell<bool>,conversations:RefCell<HashSet<usize>>,
    owns:Box<dyn Fn()->bool>,enqueue:Box<dyn Fn(PathBuf)->bool>,
}
impl State{
    fn allowed(&self)->bool{self.live.get()&&(self.owns)()&&self.live.get()}
    fn topic_matches(&self,topic:HSZ)->bool{!topic.0.is_null()&&unsafe{DdeCmpStringHandles(topic,self.topic)}==0}
    fn names_match(&self,topic:HSZ,service:HSZ)->bool{
        self.topic_matches(topic)&&!service.0.is_null()&&unsafe{DdeCmpStringHandles(service,self.service)}==0
    }
    fn execute(&self,bytes:&[u8])->bool{
        if self.executing.replace(true){return false;}
        struct Busy<'a>(&'a Cell<bool>);impl Drop for Busy<'_>{fn drop(&mut self){self.0.set(false);}}let _busy=Busy(&self.executing);
        if !self.allowed(){return false;}
        let Some(directory)=parse_packet(bytes)else{return false;};
        if !directory.is_dir()||!self.allowed(){return false;}
        // No retry or secondary launch: a positive answer means this exact
        // request has already entered the host's bounded ingress queue.
        (self.enqueue)(directory)
    }
}

fn read_execute(data:HDDEDATA)->Option<Vec<u8>>{
    struct Data(HDDEDATA);impl Drop for Data{fn drop(&mut self){if !self.0.0.is_null(){let _=unsafe{DdeFreeDataHandle(self.0)};}}}
    let _data=Data(data);
    if data.0.is_null(){return None;}
    let size=unsafe{DdeGetData(data,None,0)}as usize;
    if !(2..=MAX_BYTES).contains(&size)||size%2!=0{return None;}
    let mut bytes=vec![0u8;size];
    (unsafe{DdeGetData(data,Some(&mut bytes),0)}as usize==size).then_some(bytes)
}
fn reply(value:usize)->HDDEDATA{HDDEDATA(value as*mut _)}
fn dispatch(kind:u32,conversation:HCONV,topic:HSZ,service:HSZ,data:HDDEDATA)->HDDEDATA{
    // Execute data is copied/freed even when ownership was revoked or the
    // service is closing. No DDE data pointer crosses into application work.
    let packet=if kind==XTYP_EXECUTE.0{read_execute(data)}else{None};
    let state=ACTIVE.try_with(|slot|slot.try_borrow().ok().and_then(|slot|slot.clone())).ok().flatten();
    let Some(state)=state else{return reply(0);};
    if kind==XTYP_DISCONNECT.0{
        if let Ok(mut conversations)=state.conversations.try_borrow_mut(){conversations.remove(&(conversation.0 as usize));}
        return reply(0);
    }
    if kind==XTYP_CONNECT.0{
        let room=state.conversations.try_borrow().is_ok_and(|values|values.len()<MAX_CONVERSATIONS);
        return reply(usize::from(room&&state.names_match(topic,service)&&state.allowed()));
    }
    if kind==XTYP_CONNECT_CONFIRM.0{
        if !conversation.0.is_null()&&state.names_match(topic,service)&&state.allowed(){
            if let Ok(mut conversations)=state.conversations.try_borrow_mut(){if conversations.len()<MAX_CONVERSATIONS{conversations.insert(conversation.0 as usize);}}
        }return reply(0);
    }
    if kind==XTYP_EXECUTE.0{
        let known=state.conversations.try_borrow().is_ok_and(|values|values.contains(&(conversation.0 as usize)));
        if known&&state.topic_matches(topic)&&packet.as_ref().is_some_and(|bytes|state.execute(bytes)){return reply(DDE_FACK as usize);}
    }
    reply(DDE_FNOTPROCESSED as usize)
}
unsafe extern "system" fn callback(kind:u32,_format:u32,conversation:HCONV,topic:HSZ,service:HSZ,data:HDDEDATA,_first:usize,_second:usize)->HDDEDATA{
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(||dispatch(kind,conversation,topic,service,data)))
        .unwrap_or_else(|_|{
            #[cfg(test)]eprintln!("folder-dde callback: panic caught, returning DDE_FNOTPROCESSED");
            reply(DDE_FNOTPROCESSED as usize)
        })
}

#[derive(Default)]
struct Resources{instance:u32,service:HSZ,topic:HSZ,registered:bool}
impl Resources{
    fn close(&mut self)->bool{
        if self.instance==0{return true;}
        let instance=std::mem::take(&mut self.instance);let mut okay=true;
        if std::mem::take(&mut self.registered){
            #[cfg(test)]eprintln!("folder-dde resources {instance}: unregister begin");
            okay&=!unsafe{DdeNameService(instance,Some(self.service),None,DNS_UNREGISTER)}.0.is_null();
        }
        if !self.topic.0.is_null(){okay&=unsafe{DdeFreeStringHandle(instance,std::mem::take(&mut self.topic))}.as_bool();}
        if !self.service.0.is_null(){okay&=unsafe{DdeFreeStringHandle(instance,std::mem::take(&mut self.service))}.as_bool();}
        #[cfg(test)]eprintln!("folder-dde resources {instance}: uninitialize begin");
        let uninitialized=unsafe{DdeUninitialize(instance)}.as_bool();
        #[cfg(test)]eprintln!("folder-dde resources {instance}: uninitialize returned {uninitialized}");
        if !uninitialized{crate::window::diagnostics::record("folder-dde","uninitialize-failed",None);}
        okay&&uninitialized
    }
}
impl Drop for Resources{fn drop(&mut self){let _=self.close();}}
fn string_handle(instance:u32,value:&str)->Result<HSZ>{
    ensure!(!value.is_empty()&&value.encode_utf16().count()<=255&&!value.contains('\0'),"DDE service name is invalid");
    let text=value.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let result=unsafe{DdeCreateStringHandleW(instance,PCWSTR(text.as_ptr()),CP_WINUNICODE)};
    ensure!(!result.0.is_null(),"DDE string allocation failed: 0x{:08X}",unsafe{DdeGetLastError(instance)});Ok(result)
}

/// Owned by the runtime's first STA. Rc makes the guard non-Send; the existing
/// host message loop dispatches DDE without a new thread or child process.
pub(crate) struct Server{state:Rc<State>,_resources:Resources}
impl Server{
    pub(crate) fn start(profile:&Path)->Result<Option<Self>>{
        if crate::edition::DESKTOP_ONLY{return Ok(None);}
        if !same_absolute(profile,&crate::storage::config_path()){return Ok(None);}
        let profile=std::path::absolute(profile)?;let executable=std::env::current_exe()?;
        if let Err(error)=integration::upgrade_default(&executable,&profile.with_file_name("takeover-backup.json")){
            crate::window::diagnostics::record("folder-dde","integration-upgrade-failed",error.raw_os_error().map(|value|windows::core::HRESULT::from_win32(value as u32).0));
            crate::runtime::trace(&format!("folder DDE integration upgrade: {error}"));
        }
        let owner_profile=profile.clone();let ingress_profile=profile.clone();
        let owns=Box::new(move||std::env::current_exe().is_ok_and(|current|authorized(&owner_profile,&crate::storage::config_path(),&executable,&current,integration::folder_dde_owned(&current))));
        let enqueue=Box::new(move|directory:PathBuf|{
            let args=vec![OsString::from("--shell-open"),directory.into_os_string()];
            match crate::runtime::enqueue_local(&ingress_profile,args){
                Ok(accepted)=>accepted,
                Err(error)=>{crate::window::diagnostics::record("folder-dde","enqueue-rejected",None);crate::runtime::trace(&format!("folder DDE ingress: {error:#}"));false}
            }
        });
        Self::register(integration::FOLDER_DDE_SERVICE,integration::FOLDER_DDE_TOPIC,owns,enqueue).map(Some)
    }
    fn register(service:&str,topic:&str,owns:Box<dyn Fn()->bool>,enqueue:Box<dyn Fn(PathBuf)->bool>)->Result<Self>{
        ensure!(ACTIVE.with(|slot|slot.try_borrow().is_ok_and(|value|value.is_none())),"DDE folder service already exists on this STA");
        let mut resources=Resources::default();
        let flags=APPCMD_FILTERINITS|CBF_FAIL_ADVISES|CBF_FAIL_POKES|CBF_FAIL_REQUESTS|CBF_SKIP_REGISTRATIONS|CBF_SKIP_UNREGISTRATIONS;
        let status=unsafe{DdeInitializeW(&mut resources.instance,Some(callback),flags,None)};
        ensure!(status==0,"DDE initialization failed: 0x{status:08X}");
        resources.service=string_handle(resources.instance,service)?;resources.topic=string_handle(resources.instance,topic)?;
        let state=Rc::new(State{service:resources.service,topic:resources.topic,live:Cell::new(false),executing:Cell::new(false),conversations:RefCell::new(HashSet::new()),owns,enqueue});
        let mut server=Self{state,_resources:resources};
        ACTIVE.with(|slot|*slot.borrow_mut()=Some(server.state.clone()));
        let registered=unsafe{DdeNameService(server._resources.instance,Some(server._resources.service),None,DNS_REGISTER)};
        ensure!(!registered.0.is_null(),"DDE registration failed: 0x{:08X}",unsafe{DdeGetLastError(server._resources.instance)});
        server._resources.registered=true;server.state.live.set(true);Ok(server)
    }
    fn close(&mut self)->bool{
        // Withdraw access before any DDEML teardown call can reenter the callback.
        self.state.live.set(false);
        let _=ACTIVE.try_with(|slot|{if let Ok(mut value)=slot.try_borrow_mut(){if value.as_ref().is_some_and(|state|Rc::ptr_eq(state,&self.state)){value.take();}}});
        self._resources.close()
    }
}
impl Drop for Server{fn drop(&mut self){let _=self.close();}}

#[cfg(test)]
mod tests{
    use super::*;
    use anyhow::Context;
    use std::{ffi::c_void,fs,sync::{Arc,Condvar,Mutex,mpsc,atomic::{AtomicBool,AtomicU64,Ordering}},thread,time::{Duration,Instant}};
    use windows::Win32::UI::WindowsAndMessaging::{DispatchMessageW,GetForegroundWindow,GetWindowThreadProcessId,PeekMessageW,TranslateMessage,MSG,PM_REMOVE,WM_QUIT};
    #[link(name="user32")]unsafe extern "system"{
        fn CreateDesktopW(name:*const u16,device:*const u16,mode:*const c_void,flags:u32,access:u32,security:*const c_void)->*mut c_void;
        fn SetThreadDesktop(desktop:*mut c_void)->i32;
        fn CloseDesktop(desktop:*mut c_void)->i32;
    }
    static NEXT:AtomicU64=AtomicU64::new(1);
    fn packet(command:&str)->Vec<u8>{command.encode_utf16().chain(Some(0)).flat_map(u16::to_le_bytes).collect()}
    fn open(path:&Path)->Vec<u8>{packet(&format!("[Open(\"{}\")]",path.display()))}
    #[test]fn folder_dde_parser_accepts_one_absolute_unicode_command(){
        for path in [r"C:\",r"C:\目录 空格\data",r"\\server\share\folder",r"\\?\C:\long folder"]{
            assert_eq!(parse_packet(&open(Path::new(path))),Some(PathBuf::from(path)));
        }
        let mut padded=open(Path::new(r"C:\"));padded.extend([0,0,0,0]);assert_eq!(parse_packet(&padded),Some(PathBuf::from(r"C:\")));
    }
    #[test]fn folder_dde_parser_rejects_malformed_or_multiple_commands(){
        for command in [r#"[Open("relative")]"#,r#"[Open("C:relative")]"#,r#"[Open("\root-relative")]"#,r#"[Open("")]"#,r#"[open("C:\") ]"#,r#"[Open("C:\")][Open("C:\")]"#,r#"[Open("C:\")]extra"#,r#"[Open("C:\"quote")]"#]{assert!(parse_packet(&packet(command)).is_none(),"{command}");}
        let mut bytes=open(Path::new(r"C:\"));bytes.truncate(bytes.len()-2);assert!(parse_packet(&bytes).is_none());
        let mut bytes=open(Path::new(r"C:\"));bytes.push(0);assert!(parse_packet(&bytes).is_none());
        let mut bytes=open(Path::new(r"C:\"));bytes.extend(packet("[Open(\"C:\\\")]"));assert!(parse_packet(&bytes).is_none());
        assert!(parse_packet(&[0x00,0xd8,0,0]).is_none());assert!(parse_packet(&vec![0;MAX_BYTES+2]).is_none());
    }
    #[test]fn folder_dde_authority_requires_default_profile_and_current_installation(){
        let profile=Path::new(r"C:\Users\test\AppData\Roaming\ZSDesk\explorer-session.json");let exe=Path::new(r"C:\Apps\ZSDesk\ZSDesk.exe");
        assert!(authorized(profile,profile,exe,exe,true));assert!(!authorized(profile,profile,exe,exe,false));
        assert!(!authorized(Path::new(r"C:\Other\explorer-session.json"),profile,exe,exe,true));
        assert!(!authorized(profile,profile,exe,Path::new(r"C:\Old\ZSDesk.exe"),true));
        assert!(authorized(profile,profile,exe,Path::new(r"c:/apps/zsdesk/ZSDesk.exe"),true));
    }
    #[test]fn folder_dde_non_default_profile_never_registers()->Result<()>{
        let other=crate::storage::config_path().with_file_name("dde-private-profile.json");
        assert!(Server::start(&other)?.is_none());assert!(ACTIVE.with(|slot|slot.borrow().is_none()));Ok(())
    }
    #[cfg(feature="desktop-box")]
    #[test]fn folder_dde_desktop_box_never_registers_default_profile()->Result<()>{
        assert!(Server::start(&crate::storage::config_path())?.is_none());assert!(ACTIVE.with(|slot|slot.borrow().is_none()));Ok(())
    }
    struct Fixture(PathBuf);impl Drop for Fixture{fn drop(&mut self){let _=fs::remove_file(self.0.join("file.txt"));let _=fs::remove_dir(self.0.join("目录 空格"));let _=fs::remove_dir(&self.0);}}
    fn fixture(root:&Path)->Result<Fixture>{
        fs::create_dir_all(root)?;let directory=root.join(format!("dde-{}-{}",std::process::id(),NEXT.fetch_add(1,Ordering::Relaxed)));
        fs::create_dir(&directory)?;let fixture=Fixture(directory);fs::create_dir(fixture.0.join("目录 空格"))?;fs::write(fixture.0.join("file.txt"),b"DDE fixture")?;Ok(fixture)
    }
    #[test]fn folder_dde_dispatch_rechecks_authority_and_never_replays_or_reenters()->Result<()>{
        let fixture=fixture(&std::env::temp_dir())?;let directory=fixture.0.join("目录 空格");let bytes=open(&directory);
        let owned=Rc::new(Cell::new(true));let admit=Rc::new(Cell::new(false));let calls=Rc::new(Cell::new(0));let nested=Rc::new(Cell::new(false));let holder:Rc<RefCell<Option<std::rc::Weak<State>>>>=Default::default();
        let state=Rc::new(State{service:HSZ::default(),topic:HSZ::default(),live:Cell::new(true),executing:Cell::new(false),conversations:Default::default(),
            owns:{let owned=owned.clone();Box::new(move||owned.get())},
            enqueue:{let(admit,calls,nested,holder,bytes)=(admit.clone(),calls.clone(),nested.clone(),holder.clone(),bytes.clone());Box::new(move|_|{calls.set(calls.get()+1);if !nested.replace(true){let state=holder.borrow().as_ref().unwrap().upgrade().unwrap();assert!(!state.execute(&bytes));}admit.get()})}});
        *holder.borrow_mut()=Some(Rc::downgrade(&state));
        assert!(!state.execute(&open(&fixture.0.join("file.txt"))));assert!(!state.execute(&open(&fixture.0.join("absent"))));assert_eq!(calls.get(),0);
        assert!(!state.execute(&bytes));assert_eq!(calls.get(),1,"rejected enqueue must not be retried");
        admit.set(true);assert!(state.execute(&bytes));assert_eq!(calls.get(),2);
        owned.set(false);assert!(!state.execute(&bytes));assert_eq!(calls.get(),2);state.live.set(false);owned.set(true);assert!(!state.execute(&bytes));
        let checks=Rc::new(Cell::new(0));let queued=Rc::new(Cell::new(false));
        let revoked=State{service:HSZ::default(),topic:HSZ::default(),live:Cell::new(true),executing:Cell::new(false),conversations:Default::default(),
            owns:{let checks=checks.clone();Box::new(move||{checks.set(checks.get()+1);checks.get()==1})},
            enqueue:{let queued=queued.clone();Box::new(move|_|{queued.set(true);true})}};
        assert!(!revoked.execute(&bytes));assert_eq!(checks.get(),2);assert!(!queued.get(),"revoked ownership must not enter ingress");Ok(())
    }
    unsafe extern "system" fn client_callback(_:u32,_:u32,_:HCONV,_:HSZ,_:HSZ,_:HDDEDATA,_:usize,_:usize)->HDDEDATA{reply(0)}
    struct Client{resources:Resources,conversation:HCONV}
    impl Client{
        fn new(service:&str,topic:&str)->Result<Self>{let mut resources=Resources::default();let status=unsafe{DdeInitializeW(&mut resources.instance,Some(client_callback),APPCMD_CLIENTONLY|CBF_SKIP_ALLNOTIFICATIONS,None)};ensure!(status==0,"client DDE initialization: {status}");resources.service=string_handle(resources.instance,service)?;resources.topic=string_handle(resources.instance,topic)?;Ok(Self{resources,conversation:HCONV::default()})}
        fn connect(&mut self)->bool{self.conversation=unsafe{DdeConnect(self.resources.instance,self.resources.service,self.resources.topic,None)};!self.conversation.0.is_null()}
        fn execute(&self,bytes:&[u8])->bool{!unsafe{DdeClientTransaction(Some(bytes.as_ptr()),bytes.len()as u32,self.conversation,None,0,XTYP_EXECUTE,1500,None)}.0.is_null()}
        fn close(&mut self)->bool{
            let mut okay=true;
            if !self.conversation.0.is_null(){
                eprintln!("folder-dde client {}: disconnect begin",self.resources.instance);
                okay=unsafe{DdeDisconnect(std::mem::take(&mut self.conversation))}.as_bool();
                eprintln!("folder-dde client {}: disconnect returned {okay}",self.resources.instance);
            }
            self.resources.close()&&okay
        }
    }
    impl Drop for Client{fn drop(&mut self){let _=self.close();}}

    type Stages=Arc<Mutex<String>>;
    fn stage(stages:&Stages,value:&str){
        *stages.lock().unwrap_or_else(|error|error.into_inner())=value.to_owned();
        eprintln!("folder-dde stage: {value}");
    }
    struct Desktop(*mut c_void);
    impl Desktop{
        fn close(&mut self)->bool{
            if self.0.is_null(){return true;}
            let until=Instant::now()+Duration::from_secs(3);
            loop{if unsafe{CloseDesktop(self.0)}!=0{self.0=std::ptr::null_mut();return true;}
                if Instant::now()>=until{return false;}thread::sleep(Duration::from_millis(10));}
        }
    }
    impl Drop for Desktop{fn drop(&mut self){let _=self.close();}}
    // This ignored test runs only in its own explicitly filtered test process.
    // Cooperative deadlines normally close both DDE instances and join both
    // threads. A stuck OS DDE call must fail that owned process, not hang the
    // runner forever or leave detached fixture threads/desktops behind.
    struct Watchdog{done:Arc<(Mutex<bool>,Condvar)>,thread:Option<thread::JoinHandle<()>>}
    impl Watchdog{
        fn start(root:&Path,desktop:&str,stages:Stages)->Result<Self>{
            const TEST:&str="native_folder_dde_executes_only_owned_directory_requests_and_unregisters";
            let args=std::env::args().skip(1).collect::<Vec<_>>();
            ensure!(args.iter().filter(|arg|!arg.starts_with('-')).count()==1&&args.iter().any(|arg|arg.ends_with(TEST))&&args.iter().any(|arg|arg=="--ignored")&&args.iter().any(|arg|arg=="--test-threads=1"),"run the DDE fixture alone with its exact filter, --ignored and --test-threads=1");
            let done=Arc::new((Mutex::new(false),Condvar::new()));let signal=done.clone();let root=root.to_path_buf();let desktop=desktop.to_owned();
            let thread=thread::Builder::new().name("dde-fixture-deadline".into()).spawn(move||{
                let guard=signal.0.lock().unwrap_or_else(|error|error.into_inner());
                let (guard,wait)=signal.1.wait_timeout_while(guard,Duration::from_secs(40),|done|!*done).unwrap_or_else(|error|error.into_inner());
                if wait.timed_out()&&!*guard{
                    let last=stages.lock().unwrap_or_else(|error|error.into_inner()).clone();
                    let record=serde_json::json!({"state":"failed","reason":"DDE fixture total deadline","last_stage":last,"pid":std::process::id(),"desktop_name":desktop,"normal_cleanup_verified":false,"process_exit_code":124});
                    let _=fs::write(root.join("verification.current.json"),serde_json::to_vec_pretty(&record).unwrap_or_default());
                    // This executable is the dedicated native test process,
                    // never the product or the caller's WPS application.
                    std::process::exit(124);
                }
            })?;
            Ok(Self{done,thread:Some(thread)})
        }
    }
    impl Drop for Watchdog{fn drop(&mut self){
        *self.done.0.lock().unwrap_or_else(|error|error.into_inner())=true;self.done.1.notify_one();
        if let Some(thread)=self.thread.take(){let _=thread.join();}
    }}
    enum Control{Unregister(mpsc::SyncSender<(bool,bool)>),Stop}
    fn pump(){
        let mut message=MSG::default();for _ in 0..64{
            if !unsafe{PeekMessageW(&mut message,None,0,0,PM_REMOVE)}.as_bool(){break;}
            if message.message!=WM_QUIT{unsafe{let _=TranslateMessage(&message);DispatchMessageW(&message);}}
        }
    }
    #[test]
    #[ignore="Dedicated cross-thread DDE fixture on an unswitched private desktop; requires exact test filter, --ignored, --test-threads=1 and ZSDESK_NATIVE_TEST_OUTPUT"]
    fn native_folder_dde_executes_only_owned_directory_requests_and_unregisters()->Result<()>{
        let root=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("output")?).join("folder-dde");ensure!(root.is_absolute(),"absolute fixture output required");let fixture=fixture(&root)?;
        let serial=NEXT.fetch_add(1,Ordering::Relaxed);let service=format!("ZSDesk.Folders.Test.{}.{serial}",std::process::id());let desktop_name=format!("ZSDesk.FolderDde.{}.{serial}",std::process::id());
        let name=desktop_name.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
        let mut desktop=Desktop(unsafe{CreateDesktopW(name.as_ptr(),std::ptr::null(),std::ptr::null(),0,0x000f00ff,std::ptr::null())});ensure!(!desktop.0.is_null(),"private DDE desktop creation failed");
        let stages:Stages=Arc::new(Mutex::new("created private desktop".into()));let _watchdog=Watchdog::start(&root,&desktop_name,stages.clone())?;
        let until=Instant::now()+Duration::from_secs(30);let raw=desktop.0 as usize;
        let owned=Arc::new(AtomicBool::new(false));let admit=Arc::new(AtomicBool::new(true));let fail=Arc::new(AtomicBool::new(false));let abort=Arc::new(AtomicBool::new(false));let client_done=Arc::new(AtomicBool::new(false));
        let accepted:Arc<Mutex<Vec<PathBuf>>>=Default::default();let panic_recovered=Arc::new(AtomicBool::new(false));let (controls,commands)=mpsc::channel::<Control>();let (ready,started)=mpsc::channel::<std::result::Result<(),String>>();
        let server=thread::Builder::new().name("dde-fixture-server".into()).spawn({
            let(service,owned,admit,fail,abort,client_done,accepted,stages)=(service.clone(),owned.clone(),admit.clone(),fail.clone(),abort.clone(),client_done.clone(),accepted.clone(),stages.clone());
            move||->Result<()>{
                let result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||->Result<()>{
                    stage(&stages,"server: attach desktop");ensure!(unsafe{SetThreadDesktop(raw as*mut c_void)}!=0,"server desktop attach");
                    stage(&stages,"server: register");
                    let mut server=Some(Server::register(&service,"Folders.Test",{let owned=owned.clone();Box::new(move||owned.load(Ordering::Acquire))},{let(admit,fail,accepted)=(admit.clone(),fail.clone(),accepted.clone());Box::new(move|path|{assert!(!fail.load(Ordering::Acquire),"DDE enqueue fixture panic");if !admit.load(Ordering::Acquire){return false;}accepted.lock().unwrap_or_else(|error|error.into_inner()).push(path);true})})?);
                    let _=ready.send(Ok(()));stage(&stages,"server: pumping");let mut stopped=false;let mut released=true;
                    while !stopped{
                        while let Ok(command)=commands.try_recv(){match command{
                            Control::Unregister(answer)=>{stage(&stages,"server: unregister requested");if let Some(mut value)=server.take(){released=value.close();}let empty=ACTIVE.with(|slot|slot.borrow().is_none());stage(&stages,&format!("server: unregister returned released={released} tls_empty={empty}"));let _=answer.try_send((released,empty));},
                            Control::Stop=>stopped=true,
                        }}
                        if Instant::now()>=until{abort.store(true,Ordering::Release);owned.store(false,Ordering::Release);}
                        if abort.load(Ordering::Acquire)&&client_done.load(Ordering::Acquire){stopped=true;}
                        pump();thread::sleep(Duration::from_millis(2));
                    }
                    stage(&stages,"server: final cleanup");if let Some(mut value)=server.take(){released&=value.close();}
                    ensure!(released&&ACTIVE.with(|slot|slot.borrow().is_none()),"server resources or TLS retained");stage(&stages,"server: stopped");Ok(())
                })).map_err(|_|anyhow::anyhow!("DDE server fixture panicked")).and_then(|result|result);
                if let Err(error)=&result{let _=ready.send(Err(format!("{error:#}")));}result
            }
        })?;
        let client=thread::Builder::new().name("dde-fixture-client".into()).spawn({
            let(owned,admit,fail,abort,client_done,accepted,panic_recovered,stages,controls,directory,file)=(owned.clone(),admit.clone(),fail.clone(),abort.clone(),client_done.clone(),accepted.clone(),panic_recovered.clone(),stages.clone(),controls.clone(),fixture.0.join("目录 空格"),fixture.0.join("file.txt"));
            move||->Result<()>{
                let result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||->Result<()>{
                    macro_rules! step{($name:literal,$action:expr)=>{{ensure!(!abort.load(Ordering::Acquire)&&Instant::now()<until,"DDE fixture deadline before {}",$name);stage(&stages,concat!("client: ",$name," begin"));let result=$action;stage(&stages,concat!("client: ",$name," returned"));result}};}
                    ensure!(unsafe{SetThreadDesktop(raw as*mut c_void)}!=0,"client desktop attach");
                    started.recv_timeout(Duration::from_secs(5)).context("server readiness timeout")?.map_err(anyhow::Error::msg)?;
                    let mut denied=step!("unowned initialize",Client::new(&service,"Folders.Test"))?;ensure!(!step!("unowned connect",denied.connect()),"unowned connection accepted");ensure!(step!("unowned close",denied.close()),"unowned client cleanup");
                    owned.store(true,Ordering::Release);let mut wrong=step!("wrong-topic initialize",Client::new(&service,"WrongTopic"))?;ensure!(!step!("wrong-topic connect",wrong.connect()),"wrong topic accepted");ensure!(step!("wrong-topic close",wrong.close()),"wrong-topic client cleanup");
                    let mut client=step!("owned initialize",Client::new(&service,"Folders.Test"))?;ensure!(step!("owned connect",client.connect()),"owned connection failed");
                    ensure!(step!("valid execute",client.execute(&open(&directory))),"valid directory not acknowledged");ensure!(*accepted.lock().unwrap_or_else(|error|error.into_inner())==vec![directory.clone()],"valid execute replayed or retargeted");
                    for bytes in [packet("[Open(\"relative\")]"),packet("[Open(\"C:\\\")][Open(\"C:\\\")]"),open(&file),vec![0,0xd8,0,0]]{ensure!(!step!("invalid execute",client.execute(&bytes)),"invalid command acknowledged");}
                    owned.store(false,Ordering::Release);ensure!(!step!("revoked execute",client.execute(&open(&directory))),"existing connection ignored revocation");
                    let mut denied=step!("revoked initialize",Client::new(&service,"Folders.Test"))?;ensure!(!step!("revoked connect",denied.connect()),"revoked new connection accepted");ensure!(step!("revoked close",denied.close()),"revoked client cleanup");
                    owned.store(true,Ordering::Release);admit.store(false,Ordering::Release);ensure!(!step!("queue-rejected execute",client.execute(&open(&directory))),"queue rejection acknowledged");
                    fail.store(true,Ordering::Release);ensure!(!step!("panic execute",client.execute(&open(&directory))),"FFI panic acknowledged");ensure!(accepted.lock().unwrap_or_else(|error|error.into_inner()).len()==1,"rejected request entered queue");
                    fail.store(false,Ordering::Release);admit.store(true,Ordering::Release);ensure!(step!("post-panic execute",client.execute(&open(&directory))),"service did not recover after panic");ensure!(*accepted.lock().unwrap_or_else(|error|error.into_inner())==vec![directory.clone(),directory.clone()],"transaction replayed");
                    panic_recovered.store(true,Ordering::Release);
                    ensure!(step!("conversation close",client.close()),"connected client resources retained");
                    let(answer,response)=mpsc::sync_channel(1);controls.send(Control::Unregister(answer))?;stage(&stages,"client: waiting for server unregister");
                    let(released,empty)=response.recv_timeout(Duration::from_secs(5)).context("server unregister timeout")?;ensure!(released&&empty,"server cleanup not confirmed");
                    let mut after=step!("after-unregister initialize",Client::new(&service,"Folders.Test"))?;ensure!(!step!("after-unregister connect",after.connect()),"unregistered service accepted connection");ensure!(step!("after-unregister close",after.close()),"final client resources retained");
                    stage(&stages,"client: stopped");Ok(())
                })).map_err(|_|anyhow::anyhow!("DDE client fixture panicked")).and_then(|result|result);
                client_done.store(true,Ordering::Release);let _=controls.send(Control::Stop);result
            }
        });
        let(mut client,spawn_error)=match client{Ok(thread)=>(Some(thread),None),Err(error)=>{abort.store(true,Ordering::Release);client_done.store(true,Ordering::Release);let _=controls.send(Control::Stop);(None,Some(anyhow::Error::from(error)))}};
        while !server.is_finished()||client.as_ref().is_some_and(|thread|!thread.is_finished()){
            if Instant::now()>=until{abort.store(true,Ordering::Release);owned.store(false,Ordering::Release);if client_done.load(Ordering::Acquire){let _=controls.send(Control::Stop);}}
            thread::sleep(Duration::from_millis(5));
        }
        let client_result=match client.take(){Some(thread)=>thread.join().map_err(|_|anyhow::anyhow!("DDE client thread panicked")).and_then(|result|result),None=>Err(spawn_error.unwrap())};
        let server_result=server.join().map_err(|_|anyhow::anyhow!("DDE server thread panicked")).and_then(|result|result);
        stage(&stages,"controller: threads joined, closing desktop");let closed=desktop.close();let mut foreground_pid=0;unsafe{GetWindowThreadProcessId(GetForegroundWindow(),Some(&mut foreground_pid));}
        drop(_watchdog);stage(&stages,"controller: watchdog joined");
        let client_verified=client_result.is_ok();let server_verified=server_result.is_ok();
        let result=client_result.and(server_result);let report=serde_json::json!({"state":if result.is_ok()&&closed&&!abort.load(Ordering::Acquire){"passed"}else{"failed"},"checks":result.as_ref().map_err(|error|format!("{error:#}")),"accepted":accepted.lock().unwrap_or_else(|error|error.into_inner()).len(),"ffi_panic_recovery_checked":panic_recovered.load(Ordering::Acquire),"client_cleanup_verified":client_verified,"server_cleanup_verified":server_verified,"threads_joined":true,"desktop_closed":closed,"private_desktop":desktop_name,"input_desktop_switched":false,"physical_input_generated":false,"test_did_not_activate":foreground_pid!=std::process::id(),"associations_changed":false});
        fs::write(root.join("verification.current.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");result?;ensure!(closed&&!abort.load(Ordering::Acquire),"DDE fixture cleanup/deadline failed");Ok(())
    }
}
