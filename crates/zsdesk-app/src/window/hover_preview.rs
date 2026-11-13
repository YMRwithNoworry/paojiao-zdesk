//! Delayed, non-activating preview. UI Automation never runs on the UI thread.

use std::{path::PathBuf, sync::{Arc, Condvar, Mutex}, thread::{self, JoinHandle}, time::{Duration, Instant}};
use windows::{core::{HRESULT, Interface, PWSTR}, Win32::{
    Foundation::{HWND, LPARAM, POINT, RECT, WPARAM},
    System::{Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED}, SystemServices::{SFGAO_FILESYSTEM, SFGAO_FOLDER}},
    UI::{Accessibility::{CUIAutomation8, IUIAutomation2, UIA_DataItemControlTypeId, UIA_ListItemControlTypeId},
        Input::KeyboardAndMouse::{GetAsyncKeyState, GetCapture, GetFocus, VK_CONTROL, VK_SHIFT, VK_MENU, VK_LBUTTON, VK_MBUTTON, VK_RBUTTON},
        Shell::{IFolderView2, IShellItem, IShellView, SIGDN_FILESYSPATH, SIGDN_NORMALDISPLAY, SVGIO_ALLVIEW},
        WindowsAndMessaging::*},
}};
use zsdesk_core::model::explorer::PreviewProviderPreference;
use super::{preview_process::PreviewProcess as QuickPreview, Slot};
use super::preview_process::selection::ViewSnapshot;

const TIMER:usize=5;
const READY:u32=WM_APP+45;
const DWELL:Duration=Duration::from_millis(650);
const MAX_AGE:Duration=Duration::from_millis(800);
const UIA_BUDGET:Duration=Duration::from_millis(500);
const MAX_VISIBLE:usize=256;

struct Context { view:HWND, bounds:RECT, tab:u64, provider:PreviewProviderPreference, dark:bool, snapshot:ViewSnapshot }
#[derive(Clone,Copy)]
struct Request { generation:u64, view:isize, bounds:RECT, point:POINT, created:Instant }
struct Hit { name:String, _automation_id:String, bounds:RECT }
struct Reply { generation:u64, hit:Option<Hit> }
#[derive(Default)]
struct Mailbox { stop:bool, generation:u64, request:Option<Request>, reply:Option<Reply> }
struct Shared { mailbox:Mutex<Mailbox>, ready:Condvar }

struct Worker { shared:Arc<Shared>, thread:Option<JoinHandle<()>> }
impl Worker {
    fn new(owner:HWND,message:u32)->Option<Self> {
        let shared=Arc::new(Shared{mailbox:Mutex::new(Mailbox::default()),ready:Condvar::new()});
        let work=shared.clone();let owner=owner.0 as isize;
        let thread=thread::Builder::new().name("zsdesk-item-hit-test".into()).spawn(move||worker_main(owner,work,message)).ok()?;
        Some(Self{shared,thread:Some(thread)})
    }
    fn request(&self,request:Request) {
        if let Ok(mut mailbox)=self.shared.mailbox.lock() {
            if mailbox.stop {return;}
            mailbox.generation=request.generation;mailbox.reply=None;mailbox.request=Some(request);
            self.shared.ready.notify_one();
        }
    }
    fn cancel(&self,generation:u64) {
        if let Ok(mut mailbox)=self.shared.mailbox.lock() {
            mailbox.generation=generation;mailbox.request=None;mailbox.reply=None;
        }
    }
    fn stop(&self) {
        if let Ok(mut mailbox)=self.shared.mailbox.lock() {
            mailbox.stop=true;mailbox.request=None;mailbox.reply=None;self.shared.ready.notify_one();
        }
    }
    fn stopped(&self)->bool {self.shared.mailbox.lock().map_or(true,|m|m.stop)}
    fn finished(&self)->bool {self.thread.as_ref().is_none_or(JoinHandle::is_finished)}
    fn take(&self)->Option<Reply> {self.shared.mailbox.lock().ok()?.reply.take()}
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.stop();
        // A provider call cannot be force-cancelled. Never wait on it while the
        // main STA is needed to answer the provider's cross-thread messages.
        if self.finished() {if let Some(thread)=self.thread.take(){let _=thread.join();}}
    }
}

/// Lives beside QuickPreview, outside WindowState's RefCell and COM callbacks.
pub(super) struct HoverPreview {
    owner:HWND, worker:Option<Worker>, timer:bool, generation:u64,
    candidate:Option<(Request,u64)>, sent:bool, shown:Option<RECT>,
    options:Option<(PreviewProviderPreference,bool)>,
}
impl HoverPreview {
    pub(super) fn new(owner:HWND)->Self {
        Self{owner,worker:None,timer:false,generation:0,candidate:None,sent:false,shown:None,options:None}
    }
    fn timer(&mut self,enabled:bool) {
        if enabled==self.timer{return;}
        self.timer=enabled;
        unsafe {if enabled {self.timer=SetTimer(Some(self.owner),TIMER,100,None)!=0;}
            else {let _=KillTimer(Some(self.owner),TIMER);}}
    }
    fn reset(&mut self,preview:&mut Option<QuickPreview>) {
        self.generation=self.generation.wrapping_add(1);self.candidate=None;self.sent=false;self.shown=None;self.timer(false);
        if let Some(worker)=&self.worker {worker.cancel(self.generation);}
        if preview.as_ref().is_some_and(QuickPreview::is_hover) {*preview=None;}
    }
    pub(super) fn dismiss(&mut self,preview:&mut Option<QuickPreview>) {self.reset(preview);}

    /// Returns true only for this component's payload-free wake/timer messages.
    pub(super) fn process(&mut self,msg:&MSG,slot:&Slot,preview:&mut Option<QuickPreview>)->bool {
        let ours=msg.hwnd==self.owner && (msg.message==READY || (msg.message==WM_TIMER && msg.wParam.0==TIMER));
        let enabled=slot.try_borrow().ok().and_then(|slot|slot.as_ref().map(|state|state.session.ui.hover_preview)).unwrap_or(false);
        if !enabled {
            if self.candidate.is_some() || self.shown.is_some() {self.reset(preview);}
            if let Some(worker)=&self.worker {worker.stop();}
            if self.worker.as_ref().is_some_and(Worker::finished) {self.worker=None;}
            return ours;
        }
        if preview.as_ref().is_some_and(|preview|preview.is_open() && !preview.is_hover()) {
            self.reset(preview);return ours;
        }
        let input=(WM_KEYFIRST..=WM_KEYLAST).contains(&msg.message)
            || [WM_LBUTTONDOWN,WM_RBUTTONDOWN,WM_MBUTTONDOWN,WM_XBUTTONDOWN,WM_MOUSEWHEEL,WM_MOUSEHWHEEL,
                WM_NCLBUTTONDOWN,WM_NCRBUTTONDOWN].contains(&msg.message);
        let changing=[super::WM_SHELL_NAVIGATED,super::WM_SHELL_NAVIGATION_FAILED,super::WM_LAYOUT,WM_DPICHANGED,WM_SETTINGCHANGE].contains(&msg.message);
        if input || changing {self.reset(preview);return ours;}
        if !ours && msg.message!=WM_MOUSEMOVE {return false;}
        let Some(context)=context(slot,self.owner) else {self.reset(preview);return ours;};
        let options=(context.provider,context.dark);
        if self.options.is_some_and(|old|old!=options) {self.reset(preview);}
        self.options=Some(options);
        let mut point=POINT::default();
        if unsafe {GetCursorPos(&mut point)}.is_err() {self.reset(preview);return ours;}
        if preview.as_ref().is_some_and(|preview|preview.is_hover() && preview.is_open() && preview.contains_point(point)) {
            return ours;
        }
        if !contains(context.bounds,point) {self.reset(preview);return ours;}
        if self.shown.is_some_and(|bounds|contains(bounds,point)) && preview.as_ref().is_some_and(QuickPreview::is_open) {return ours;}
        if self.shown.is_some() {self.reset(preview);}
        let same=self.candidate.is_some_and(|(request,tab)|request.view==context.view.0 as isize
            && tab==context.tab && request.bounds==context.bounds && near(request.point,point));
        if !same {
            // Timer-only activity never begins a preview after typing/clicking.
            if msg.message!=WM_MOUSEMOVE {self.reset(preview);return ours;}
            self.reset(preview);
            self.candidate=Some((Request{generation:self.generation,view:context.view.0 as isize,
                bounds:context.bounds,point,created:Instant::now()},context.tab));
            self.timer(true);return ours;
        }
        let (request,_)=self.candidate.unwrap();
        if !self.sent && request.created.elapsed()>=DWELL {
            if self.worker.as_ref().is_some_and(|worker|worker.stopped() && worker.finished()) {self.worker=None;}
            if self.worker.is_none() {self.worker=Worker::new(self.owner,READY);}
            if let Some(worker)=self.worker.as_ref().filter(|worker|!worker.stopped()) {
                let request=Request{created:Instant::now(),..request};
                self.candidate=Some((request,context.tab));worker.request(request);self.sent=true;
            } else {self.reset(preview);}
        }
        if self.sent {
            let reply=self.worker.as_ref().and_then(Worker::take);
            if let Some(reply)=reply.filter(|reply|reply.generation==self.generation) {
                let Some(hit)=reply.hit.filter(|hit|contains(hit.bounds,point)) else {self.reset(preview);return ours;};
                if self.candidate.is_none_or(|(request,_)|request.created.elapsed()>MAX_AGE) {self.reset(preview);return ours;}
                let shell=context.snapshot.view(slot);
                // All provider calls have finished. Resolve only real Shell items
                // on the STA, never by turning an automation ID into an index.
                let path=shell.as_ref().filter(|shell|unsafe {shell.GetWindow()}.ok()==Some(context.view))
                    .and_then(|shell|resolve_item(shell,&hit.name,false));
                let still_current=context_matches(slot,self.owner,&context) && unsafe {GetCursorPos(&mut point)}.is_ok()
                    && contains(hit.bounds,point) && !busy(self.owner);
                if let Some(path)=path.filter(|_|still_current) {
                    if let Ok(opened)=QuickPreview::open_with_options(self.owner,&path,context.provider,context.dark,true) {
                        // A preview provider can pump messages while opening.
                        // Discard the result if navigation/input moved on.
                        if !context_matches(slot,self.owner,&context) || unsafe {GetCursorPos(&mut point)}.is_err()
                            || (!contains(hit.bounds,point) && !opened.contains_point(point)) {drop(opened);self.reset(preview);return ours;}
                        *preview=Some(opened);self.shown=Some(hit.bounds);return ours;
                    }
                }
                self.reset(preview);
            } else if self.candidate.is_some_and(|(request,_)|request.created.elapsed()>MAX_AGE) {
                self.reset(preview);
            }
        }
        ours
    }
}
impl Drop for HoverPreview {fn drop(&mut self){self.timer(false);if let Some(worker)=&self.worker{worker.stop();}}}

fn contains(rect:RECT,point:POINT)->bool {
    point.x>=rect.left && point.x<rect.right && point.y>=rect.top && point.y<rect.bottom
}
fn near(a:POINT,b:POINT)->bool {(i64::from(a.x)-i64::from(b.x)).abs()<=3 && (i64::from(a.y)-i64::from(b.y)).abs()<=3}
fn busy(owner:HWND)->bool {
    unsafe {GetForegroundWindow()!=owner || IsIconic(owner).as_bool() || !GetCapture().0.is_null()
        || [VK_LBUTTON,VK_RBUTTON,VK_MBUTTON,VK_CONTROL,VK_SHIFT,VK_MENU].iter().any(|key|GetAsyncKeyState(key.0 as i32)<0)
        || super::text_input_window(GetFocus())}
}
fn context(slot:&Slot,owner:HWND)->Option<Context> {
    if busy(owner) {return None;}
    input_context(slot,owner)
}
fn input_context(slot:&Slot,owner:HWND)->Option<Context> {
    {
        let slot=slot.try_borrow().ok()?;let state=slot.as_ref()?;
        if state.tab_drag.is_some() || state.splitter_drag.is_some()
            || state.address_editing || state.rename_id.is_some() {return None;}
    }
    let snapshot=ViewSnapshot::capture(slot)?;
    if snapshot.owner()!=owner{return None;}
    // Navigation maintains this handle. Mouse movement does not need to enter
    // Shell just to obtain its bounds; a missing/destroyed view is not a hit.
    let view=snapshot.window();let mut bounds=RECT::default();
    if !unsafe{IsWindow(Some(view))}.as_bool(){return None;}
    unsafe {GetWindowRect(view,&mut bounds)}.ok()?;
    if !snapshot.current(slot,false){return None;}
    Some(Context{view,bounds,tab:snapshot.tab(),provider:snapshot.provider,dark:snapshot.dark,snapshot})
}
fn context_matches(slot:&Slot,owner:HWND,expected:&Context)->bool {
    if busy(owner)||!expected.snapshot.current(slot,false){return false;}
    let mut bounds=RECT::default();
    if unsafe{GetWindowRect(expected.view,&mut bounds)}.is_err()||bounds!=expected.bounds{return false;}
    ViewSnapshot::capture(slot).is_some_and(|actual|actual.provider==expected.provider&&actual.dark==expected.dark)
}

fn worker_main(owner:isize,shared:Arc<Shared>,message:u32) {
    struct Exit(Arc<Shared>);
    impl Drop for Exit {fn drop(&mut self){if let Ok(mut mailbox)=self.0.mailbox.lock(){mailbox.stop=true;mailbox.request=None;}}}
    let _exit=Exit(shared.clone());
    if unsafe {CoInitializeEx(None,COINIT_MULTITHREADED)}.is_err() {return;}
    struct Apartment;
    impl Drop for Apartment {fn drop(&mut self){unsafe {CoUninitialize();}}}
    let _apartment=Apartment;
    let Ok(automation)= (unsafe {CoCreateInstance::<_,IUIAutomation2>(&CUIAutomation8,None,CLSCTX_INPROC_SERVER)}) else {return;};
    // Queries only; never let UI Automation focus an element implicitly.
    let _=unsafe {automation.SetAutoSetFocus(false)};
    if unsafe {automation.SetConnectionTimeout(200)}.is_err() || unsafe {automation.SetTransactionTimeout(200)}.is_err() {return;}
    loop {
        let request={
            let Ok(mut mailbox)=shared.mailbox.lock() else {break;};
            while !mailbox.stop && mailbox.request.is_none() {
                let Ok(next)=shared.ready.wait(mailbox) else {return;};mailbox=next;
            }
            if mailbox.stop {break;} let mut request=mailbox.request.take().unwrap();request.created=Instant::now();request
        };
        let hit=automation_hit(&automation,request);
        let Ok(mut mailbox)=shared.mailbox.lock() else {break;};
        if mailbox.stop {break;}
        if mailbox.generation!=request.generation {continue;}
        mailbox.reply=Some(Reply{generation:request.generation,hit});
        let _=unsafe {PostMessageW(Some(HWND(owner as *mut _)),message,WPARAM(0),LPARAM(0))};
    }
}
fn automation_hit(automation:&IUIAutomation2,request:Request)->Option<Hit> {
    if request.created.elapsed()>UIA_BUDGET {return None;}
    let mut element=unsafe {automation.ElementFromPoint(request.point)}.ok()?;
    let walker=unsafe {automation.ControlViewWalker()}.ok()?;
    for _ in 0..6 {
        if request.created.elapsed()>UIA_BUDGET || unsafe {element.CurrentProcessId()}.ok()?!=std::process::id() as i32 {return None;}
        let kind=unsafe {element.CurrentControlType()}.ok()?;
        if kind==UIA_ListItemControlTypeId || kind==UIA_DataItemControlTypeId {
            let raw_bounds=unsafe {element.CurrentBoundingRectangle()}.ok()?;
            let bounds=RECT{left:raw_bounds.left.max(request.bounds.left),right:raw_bounds.right.min(request.bounds.right),top:raw_bounds.top.max(request.bounds.top),bottom:raw_bounds.bottom.min(request.bounds.bottom)};
            let name=unsafe {element.CurrentName()}.ok()?.to_string();
            let automation_id=unsafe {element.CurrentAutomationId()}.ok()?.to_string();
            if name.is_empty() || name.len()>1024 || automation_id.len()>1024 || !contains(bounds,request.point)
                || bounds.left<request.bounds.left || bounds.right>request.bounds.right
                || bounds.top<request.bounds.top || bounds.bottom>request.bounds.bottom {return None;}
            // Confirm ancestry belongs to the exact DefView, not another list in
            // the same process. Crossing a foreign HWND invalidates the hit.
            let mut ancestor=element;
            for _ in 0..10 {
                if request.created.elapsed()>UIA_BUDGET {return None;}
                let window=unsafe {ancestor.CurrentNativeWindowHandle()}.ok()?;
                if !window.0.is_null() {
                    let view=HWND(request.view as *mut _);
                    if window==view || unsafe {IsChild(view,window)}.as_bool() {
                        return Some(Hit{name,_automation_id:automation_id,bounds});
                    }
                    return None;
                }
                ancestor=unsafe {walker.GetParentElement(&ancestor)}.ok()?;
            }
            return None;
        }
        element=unsafe {walker.GetParentElement(&element)}.ok()?;
    }
    None
}

fn take_string(text:PWSTR)->Option<String> {
    let result=unsafe {text.to_string()}.ok();unsafe {CoTaskMemFree(Some(text.0.cast()))};result
}
fn resolve_item(shell:&IShellView,name:&str,folder_only:bool)->Option<PathBuf> {
    let view:IFolderView2=shell.cast().ok()?;
    let count=unsafe {view.ItemCount(SVGIO_ALLVIEW)}.ok()?;
    if count<=0 {return None;}
    let started=Instant::now();let mut matched=None;let mut last=-1;
    for position in 0..=MAX_VISIBLE {
        if started.elapsed()>Duration::from_millis(40) {return None;}
        let index=if count as usize<=MAX_VISIBLE {
            if position>=count as usize {return matched;}
            position as i32
        } else {
            let mut index=-1;
            let status=unsafe {(view.vtable().GetVisibleItem)(view.as_raw(),last,false.into(),&mut index)};
            if status==HRESULT(1) {return matched;}
            if status!=HRESULT(0) || index<=last || index>=count || position==MAX_VISIBLE {return None;}
            last=index;index
        };
        let item:IShellItem=unsafe {view.GetItem(index)}.ok()?;
        let label=take_string(unsafe {item.GetDisplayName(SIGDN_NORMALDISPLAY)}.ok()?)?;
        if label!=name {continue;}
        // Two visible items can share a label when extensions are hidden.
        // Ambiguous labels must not preview either item.
        if matched.is_some() {return None;}
        let attributes=unsafe {item.GetAttributes(SFGAO_FILESYSTEM|SFGAO_FOLDER)}.ok()?;
        if attributes.0&SFGAO_FILESYSTEM.0==0 || (attributes.0&SFGAO_FOLDER.0!=0)!=folder_only {return None;}
        let path=take_string(unsafe {item.GetDisplayName(SIGDN_FILESYSPATH)}.ok()?)?;
        let path=PathBuf::from(path);if !path.is_absolute() {return None;}
        matched=Some(path);
    }
    None
}

const MIDDLE_READY:u32=WM_APP+46;
pub(super) struct MiddleClick{owner:HWND,worker:Option<Worker>,down:Option<POINT>,pending:Option<(Context,Instant)>,generation:u64}
impl MiddleClick{
    pub(super) fn new(owner:HWND)->Self{Self{owner,worker:None,down:None,pending:None,generation:0}}
    pub(super) fn process(&mut self,msg:&MSG,slot:&Slot)->bool{
        if matches!(msg.message,WM_LBUTTONDOWN|WM_RBUTTONDOWN|WM_KEYDOWN|WM_SYSKEYDOWN|WM_CANCELMODE|WM_CAPTURECHANGED)
            || [super::WM_SHELL_NAVIGATED,super::WM_SHELL_NAVIGATION_FAILED,super::WM_LAYOUT,WM_DPICHANGED].contains(&msg.message){
            self.generation=self.generation.wrapping_add(1);self.pending=None;self.down=None;
            if let Some(worker)=&self.worker{worker.cancel(self.generation);}
        }
        if msg.hwnd==self.owner&&msg.message==MIDDLE_READY{
            let Some(reply)=self.worker.as_ref().and_then(Worker::take)else{return true;};
            let Some((context,created))=self.pending.take()else{return true;};
            if created.elapsed()>MAX_AGE||reply.generation!=self.generation||!context_matches(slot,self.owner,&context){return true;}
            let Some(hit)=reply.hit else{return true;};
            let shell=context.snapshot.view(slot);
            if let Some(path)=shell.as_ref().filter(|shell|unsafe{shell.GetWindow()}.ok()==Some(context.view)).and_then(|shell|resolve_item(shell,&hit.name,true)){
                // GetItem/GetDisplayName may re-enter the owner or complete a
                // navigation. Never post a path resolved from that old view.
                if !context_matches(slot,self.owner,&context){return true;}
                super::pane_input::post(self.owner,context.snapshot.input_identity(),super::pane_input::Intent::OpenFolder(path));
            }
            return true;
        }
        if !matches!(msg.message,WM_MBUTTONDOWN|WM_MBUTTONUP){return false;}
        let mut point=POINT::default();if unsafe{GetCursorPos(&mut point)}.is_err(){return false;}
        let context=input_context(slot,self.owner).filter(|context|contains(context.bounds,point));
        let Some(context)=context else{self.down=None;return false;};
        if msg.message==WM_MBUTTONDOWN{self.down=Some(point);return true;}
        if !self.down.take().is_some_and(|start|near(start,point)){return true;}
        if self.worker.as_ref().is_some_and(Worker::finished){self.worker=None;}
        if self.worker.is_none(){self.worker=Worker::new(self.owner,MIDDLE_READY);}
        self.generation=self.generation.wrapping_add(1);
        if let Some(worker)=&self.worker{let created=Instant::now();worker.request(Request{generation:self.generation,view:context.view.0 as isize,bounds:context.bounds,point,created});self.pending=Some((context,created));}
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dwell_hit_bounds_handle_negative_monitors_and_small_jitter() {
        let bounds=RECT{left:-1200,top:40,right:-800,bottom:80};
        assert!(contains(bounds,POINT{x:-1000,y:50}));
        assert!(!contains(bounds,POINT{x:-800,y:50}));
        assert!(!contains(bounds,POINT{x:-1000,y:80}));
        assert!(near(POINT{x:i32::MIN,y:4},POINT{x:i32::MIN+3,y:7}));
        assert!(!near(POINT{x:i32::MIN,y:0},POINT{x:i32::MAX,y:0}));
    }
    #[test]
    fn mailbox_replaces_requests_and_cancel_discards_stale_results() {
        let shared=Arc::new(Shared{mailbox:Mutex::new(Mailbox::default()),ready:Condvar::new()});
        let worker=Worker{shared:shared.clone(),thread:None};
        let request=Request{generation:1,view:0,bounds:RECT::default(),point:POINT::default(),created:Instant::now()};
        worker.request(request);worker.request(Request{generation:2,..request});
        assert_eq!(shared.mailbox.lock().unwrap().request.unwrap().generation,2);
        shared.mailbox.lock().unwrap().reply=Some(Reply{generation:1,hit:None});
        worker.cancel(3);assert!(worker.take().is_none());
        assert!(shared.mailbox.lock().unwrap().request.is_none());
        worker.stop();worker.request(Request{generation:4,..request});
        assert!(shared.mailbox.lock().unwrap().request.is_none());
    }

}
