//! A dedicated hook thread never waits for Shell, rendering, or the desktop STA.
use std::{cell::RefCell,collections::VecDeque,sync::{Arc,Mutex,mpsc,atomic::{AtomicBool,AtomicIsize,AtomicU32,AtomicU64,Ordering}},thread,time::{Duration,Instant}};
use windows::{core::Result,Win32::{Foundation::{HINSTANCE,HWND,LPARAM,LRESULT,POINT,WPARAM},System::{LibraryLoader::GetModuleHandleW,Threading::GetCurrentThreadId},
    UI::{Input::KeyboardAndMouse::{GetAsyncKeyState,VK_CONTROL,VK_SHIFT},WindowsAndMessaging::*}}};
pub(super) const MESSAGE:u32=super::protocol::Message::MouseInput as u32;
const LIMIT:usize=128;
#[derive(Clone,Copy,Debug)]
pub(super) struct Event{pub message:u32,pub point:POINT,pub time:u32,pub control:bool,pub shift:bool}
struct Buffer{events:VecDeque<Event>,coalesced:u64}
impl Buffer{
    fn new()->Self{Self{events:VecDeque::with_capacity(LIMIT),coalesced:0}}
    fn push(&mut self,event:Event)->bool{
        if event.message==WM_MOUSEMOVE&&self.events.back().is_some_and(|last|last.message==WM_MOUSEMOVE){
            *self.events.back_mut().unwrap()=event;self.coalesced+=1;return true;
        }
        if self.events.len()==LIMIT{return false;}
        self.events.push_back(event);true
    }
}
struct Shared{buffer:Mutex<Buffer>,owner:AtomicIsize,posted:AtomicBool,lost:AtomicBool,stopping:AtomicBool,left_down:AtomicBool,thread:AtomicU32,received:AtomicU64,dropped:AtomicU64}
impl Shared{
    fn new(owner:HWND)->Self{Self{buffer:Mutex::new(Buffer::new()),owner:AtomicIsize::new(owner.0 as isize),posted:AtomicBool::new(false),lost:AtomicBool::new(false),stopping:AtomicBool::new(false),left_down:AtomicBool::new(false),thread:AtomicU32::new(0),received:AtomicU64::new(0),dropped:AtomicU64::new(0)}}
    fn interested(&self,message:u32)->bool{match message{
        WM_LBUTTONDOWN=>{self.left_down.store(true,Ordering::Relaxed);true},
        WM_LBUTTONUP=>{self.left_down.store(false,Ordering::Relaxed);true},
        WM_MOUSEMOVE=>self.left_down.load(Ordering::Relaxed),
        WM_RBUTTONDOWN|WM_RBUTTONUP|WM_MBUTTONDOWN|WM_XBUTTONDOWN|WM_MOUSEWHEEL|WM_MOUSEHWHEEL=>{self.left_down.store(false,Ordering::Relaxed);true},
        _=>false,
    }}
    fn push(&self,event:Event){
        self.received.fetch_add(1,Ordering::Relaxed);
        // A busy UI may hold the drain lock briefly. Never wait for it inside
        // WH_MOUSE_LL: cancel the logical gesture on loss instead of stalling input.
        let accepted=self.buffer.try_lock().is_ok_and(|mut buffer|buffer.push(event));
        if !accepted{self.lost.store(true,Ordering::Release);self.dropped.fetch_add(1,Ordering::Relaxed);}
        let owner=self.owner.load(Ordering::Acquire);
        if owner!=0&&!self.posted.swap(true,Ordering::AcqRel){
            if unsafe{PostMessageW(Some(HWND(owner as*mut _)),MESSAGE,WPARAM(0),LPARAM(0))}.is_err(){self.posted.store(false,Ordering::Release);}
        }
    }
    fn take(&self)->(Vec<Event>,bool){
        let mut events=Vec::with_capacity(LIMIT);
        let mut buffer=self.buffer.lock().unwrap_or_else(|error|error.into_inner());
        self.posted.store(false,Ordering::Release);
        let lost=self.lost.swap(false,Ordering::AcqRel);
        if lost{buffer.events.clear();}else{events.extend(buffer.events.drain(..));}
        (events,lost)
    }
}
thread_local!{static ROUTE:RefCell<Option<Arc<Shared>>>=const{RefCell::new(None)};}
unsafe extern "system" fn callback(code:i32,wp:WPARAM,lp:LPARAM)->LRESULT{
    if code>=0&&lp.0!=0{
        let raw=unsafe{&*(lp.0 as*const MSLLHOOKSTRUCT)};
        ROUTE.with(|slot|if let Some(shared)=slot.borrow().as_ref(){
            if shared.owner.load(Ordering::Relaxed)!=0&&!shared.stopping.load(Ordering::Relaxed)&&shared.interested(wp.0 as u32){
                shared.push(Event{message:wp.0 as u32,point:raw.pt,time:raw.time,
                    control:unsafe{GetAsyncKeyState(VK_CONTROL.0 as i32)}<0,shift:unsafe{GetAsyncKeyState(VK_SHIFT.0 as i32)}<0});
            }
        });
    }
    unsafe{CallNextHookEx(None,code,wp,lp)}
}
pub(super) struct Monitor{shared:Arc<Shared>,worker:Option<thread::JoinHandle<()>>}
impl Monitor{
    pub(super) fn start(owner:HWND)->anyhow::Result<Self>{
        let shared=Arc::new(Shared::new(owner));let state=shared.clone();let(tx,rx)=mpsc::sync_channel(1);
        let worker=thread::Builder::new().name("zsdesk-mouse-observer".into()).spawn(move||{
            let result=(||->Result<()>{
                let mut message=MSG::default();let _=unsafe{PeekMessageW(&mut message,None,0,0,PM_NOREMOVE)};
                state.thread.store(unsafe{GetCurrentThreadId()},Ordering::Release);
                let module=HINSTANCE(unsafe{GetModuleHandleW(None)?}.0);
                ROUTE.with(|route|*route.borrow_mut()=Some(state.clone()));
                let hook=unsafe{SetWindowsHookExW(WH_MOUSE_LL,Some(callback),Some(module),0)?};
                struct Hook(HHOOK);impl Drop for Hook{fn drop(&mut self){let _=unsafe{UnhookWindowsHookEx(self.0)};}}let _hook=Hook(hook);
                let _=tx.send(Ok(()));
                while !state.stopping.load(Ordering::Acquire)&&unsafe{GetMessageW(&mut message,None,0,0)}.0>0{
                    unsafe{let _=TranslateMessage(&message);DispatchMessageW(&message);}
                }
                Ok(())
            })();
            if let Err(error)=result{let _=tx.send(Err(error.to_string()));}
            ROUTE.with(|route|route.borrow_mut().take());
        })?;
        let mut monitor=Self{shared,worker:Some(worker)};
        match rx.recv_timeout(Duration::from_secs(2)){
            Ok(Ok(()))=>Ok(monitor),
            result=>{monitor.stop();anyhow::bail!("{}", zsdesk_core::tr_format!("无法启动鼠标观察线程：{result:?}" , result=result))}
        }
    }
    pub(super) fn thread_id(&self)->u32{self.shared.thread.load(Ordering::Acquire)}
    pub(super) fn take(&self)->(Vec<Event>,bool){self.shared.take()}
    pub(super) fn diagnostics(&self)->serde_json::Value{
        let(queue,coalesced)={let buffer=self.shared.buffer.lock().unwrap_or_else(|e|e.into_inner());(buffer.events.len(),buffer.coalesced)};
        serde_json::json!({"thread_id":self.thread_id(),"queue":queue,"limit":LIMIT,"coalesced_moves":coalesced,
            "received":self.shared.received.load(Ordering::Relaxed),"dropped":self.shared.dropped.load(Ordering::Relaxed),"dedicated_thread":true})
    }
    fn stop(&mut self){
        self.shared.owner.store(0,Ordering::Release);self.shared.stopping.store(true,Ordering::Release);
        let id=self.thread_id();if id!=0{let _=unsafe{PostThreadMessageW(id,WM_QUIT,WPARAM(0),LPARAM(0))};}
        if let Some(worker)=self.worker.take(){
            let deadline=Instant::now()+Duration::from_millis(250);
            while !worker.is_finished()&&Instant::now()<deadline{thread::sleep(Duration::from_millis(1));}
            if worker.is_finished(){let _=worker.join();}
        }
    }
}
impl Drop for Monitor{fn drop(&mut self){self.stop();}}

#[cfg(test)]mod tests{
    use super::*;
    fn event(message:u32,x:i32)->Event{Event{message,point:POINT{x,y:0},time:x as u32,control:false,shift:false}}
    #[test]fn ordinary_hover_does_not_wake_the_desktop_thread(){
        let shared=Shared::new(HWND::default());assert!(!shared.interested(WM_MOUSEMOVE));
        assert!(shared.interested(WM_LBUTTONDOWN));assert!(shared.interested(WM_MOUSEMOVE));
        assert!(shared.interested(WM_LBUTTONUP));assert!(!shared.interested(WM_MOUSEMOVE));
    }
    #[test]fn a_slow_renderer_cannot_make_hook_delivery_wait_for_its_mutex(){
        let shared=Arc::new(Shared::new(HWND::default()));let held=shared.buffer.lock().unwrap();
        let(tx,rx)=mpsc::channel();let producer=shared.clone();
        let worker=thread::spawn(move||{producer.push(event(WM_MOUSEMOVE,42));tx.send(()).unwrap();});
        assert!(rx.recv_timeout(Duration::from_secs(1)).is_ok());drop(held);worker.join().unwrap();
        let(events,lost)=shared.take();assert!(lost&&events.is_empty());
    }
    #[test]fn high_rate_motion_is_bounded_without_losing_button_boundaries(){
        let mut buffer=Buffer::new();assert!(buffer.push(event(WM_LBUTTONDOWN,0)));
        for x in 1..100_000{assert!(buffer.push(event(WM_MOUSEMOVE,x)));}
        assert!(buffer.push(event(WM_LBUTTONUP,100_000)));
        assert_eq!(buffer.events.len(),3);assert_eq!(buffer.events[1].point.x,99_999);assert_eq!(buffer.events[2].message,WM_LBUTTONUP);
        assert_eq!(buffer.events.capacity(),LIMIT);
    }
    #[test]fn input_overflow_cancels_the_gesture_instead_of_replaying_a_partial_drag(){
        let shared=Shared::new(HWND::default());for x in 0..LIMIT+1{shared.push(event(WM_LBUTTONDOWN,x as i32));}
        let(events,lost)=shared.take();assert!(lost&&events.is_empty());
        shared.push(event(WM_LBUTTONDOWN,1000));let(events,lost)=shared.take();assert!(!lost&&events.len()==1);
    }
}
