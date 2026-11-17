//! Desktop-background marquee for the loose icons owned by organize mode.
//! The mouse hook only observes input; no persistent desktop overlay captures it.
use super::*;
use std::collections::{HashMap,HashSet};
use windows::Win32::{Foundation::{RECT,SIZE},Graphics::Gdi::{GetDC,ReleaseDC,AC_SRC_ALPHA,AC_SRC_OVER,BLENDFUNCTION},
    UI::Input::KeyboardAndMouse::{GetAsyncKeyState,VK_LBUTTON}};
use zsdesk_windows::gfx::{Color,RectF,Surface};

pub(super) const MESSAGE:u32=super::protocol::Message::OutsideSelection as u32;
pub(super) const TIMER:usize=super::protocol::HostTimer::OutsideSelection as usize;
const CLASS:PCWSTR=w!("ZSDesk.DesktopSelection");
#[derive(Clone,Copy)]
struct Observation{generation:u64,desktop:HWND,start:POINT,current:POINT,control:bool,shift:bool,finished:bool,cancelled:bool}
#[derive(Clone)]
struct Original{selection:HashSet<String>,focus:Option<String>,anchor:Option<String>}
struct Gesture{generation:u64,desktop:HWND,start:POINT,control:bool,shift:bool,original:HashMap<u64,Original>,dragged:bool,overlay:Option<Overlay>}
struct Overlay{desktop:HWND,segments:Vec<(HWND,Option<Surface>)>}
impl Drop for Overlay{fn drop(&mut self){for (window,_) in &self.segments{let _=unsafe{DestroyWindow(*window)};}}}
thread_local!{
    static ENABLED:Cell<bool>=const{Cell::new(false)};
    static NEXT:Cell<u64>=const{Cell::new(1)};
    static OBSERVATION:Cell<Option<Observation>>=const{Cell::new(None)};
    static POSTED:Cell<bool>=const{Cell::new(false)};
    static GESTURE:RefCell<Option<Gesture>>=const{RefCell::new(None)};
    static DISPATCHING:Cell<bool>=const{Cell::new(false)};
    static CANCEL_PENDING:Cell<bool>=const{Cell::new(false)};
}

pub(super) fn configure(enabled:bool){ENABLED.with(|slot|slot.set(enabled));if !enabled{cancel();}}
pub(super) fn active()->bool{OBSERVATION.with(|slot|slot.get().is_some_and(|event|!event.finished&&!event.cancelled))}
fn post(){
    let owner=HOST_HWND.with(Cell::get);
    if owner!=0&&!POSTED.with(|slot|slot.replace(true)){
        if unsafe{PostMessageW(Some(HWND(owner as*mut _)),MESSAGE,WPARAM(0),LPARAM(0))}.is_err(){POSTED.with(|slot|slot.set(false));}
    }
}
fn background_root(point:POINT)->Option<HWND>{
    let hit=unsafe{WindowFromPoint(point)};
    if !matches!(class_name(hit).as_str(),"SHELLDLL_DefView"|"WorkerW"|"Progman"){return None;}
    let root=unsafe{GetAncestor(hit,GA_ROOT)};
    matches!(class_name(root).as_str(),"WorkerW"|"Progman").then_some(root)
}
fn exceeded(start:POINT,current:POINT)->bool{
    let dx=unsafe{GetSystemMetrics(SM_CXDRAG)}.max(2) as i64;let dy=unsafe{GetSystemMetrics(SM_CYDRAG)}.max(2) as i64;
    (i64::from(start.x)-i64::from(current.x)).abs()>=dx||(i64::from(start.y)-i64::from(current.y)).abs()>=dy
}
/// Called from the existing low-level hook. Never consumes the input or calls COM.
#[cfg(test)]pub(super) fn observe(message:u32,point:POINT,double:bool){
    observe_with_modifiers(message,point,double,unsafe{GetAsyncKeyState(VK_CONTROL.0 as i32)}<0,unsafe{GetAsyncKeyState(VK_SHIFT.0 as i32)}<0);
}
pub(super) fn observe_with_modifiers(message:u32,point:POINT,double:bool,control:bool,shift:bool){
    if !ENABLED.with(Cell::get){return;}
    match message{
        WM_LBUTTONDOWN=>{
            if double{cancel_observation();return;}
            let Some(desktop)=background_root(point)else{cancel_observation();return;};
            let generation=NEXT.with(|slot|{let value=slot.get();slot.set(value.wrapping_add(1).max(1));value});
            let event=Observation{generation,desktop,start:point,current:point,control,shift,finished:false,cancelled:false};
            CANCEL_PENDING.with(|slot|slot.set(false));OBSERVATION.with(|slot|slot.set(Some(event)));post();
        },
        WM_MOUSEMOVE|WM_LBUTTONUP=>{
            OBSERVATION.with(|slot|{if let Some(mut event)=slot.get(){
                if event.finished||event.cancelled{return;}event.current=point;event.finished=message==WM_LBUTTONUP;
                if exceeded(event.start,point){LAST_CLICK.with(|click|click.set(None));}
                slot.set(Some(event));post();
            }});
        },
        WM_RBUTTONDOWN|WM_MBUTTONDOWN|WM_XBUTTONDOWN|WM_MOUSEWHEEL|WM_MOUSEHWHEEL=>cancel_observation(),
        _=>{},
    }
}
fn cancel_observation(){OBSERVATION.with(|slot|{if let Some(mut event)=slot.get(){event.cancelled=true;slot.set(Some(event));post();}});}
fn original(host:&DesktopHost)->HashMap<u64,Original>{
    host.fences.iter().filter(|window|window.standalone).map(|window|(window.id,Original{
        selection:window.selection.clone(),focus:window.focus.and_then(|index|window.entries.get(index)).map(|entry|entry.key.clone()),
        anchor:window.anchor.and_then(|index|window.entries.get(index)).map(|entry|entry.key.clone())})).collect()
}
fn restore(gesture:&Gesture){with_host(|host|{
    let mut changed=Vec::new();
    for window in host.fences.iter_mut().filter(|window|window.standalone){if let Some(saved)=gesture.original.get(&window.id){
        window.selection=saved.selection.iter().filter(|key|window.entries.iter().any(|entry|&entry.key==*key)).cloned().collect();
        window.focus=saved.focus.as_ref().and_then(|key|window.entries.iter().position(|entry|&entry.key==key));
        window.anchor=saved.anchor.as_ref().and_then(|key|window.entries.iter().position(|entry|&entry.key==key));changed.push(window.id);
    }}for id in changed{host.render(id);}
});}
pub(super) fn cancel(){
    CANCEL_PENDING.with(|slot|slot.set(true));
    // Defer restoration through the host message. Synchronous dispatch from
    // WM_CANCELMODE/WM_CAPTURECHANGED can re-enter the same window procedure
    // while its HOST borrow is active and recurse through render/message pumps.
    post();
}
fn retry(){
    let owner=HOST_HWND.with(Cell::get);
    if owner!=0&&unsafe{IsWindow(Some(HWND(owner as*mut _)))}.as_bool(){unsafe{SetTimer(Some(HWND(owner as*mut _)),TIMER,40,None);}}
}
fn clear_pending(restore_selection:bool){
    OBSERVATION.with(|slot|slot.set(None));POSTED.with(|slot|slot.set(false));
    CANCEL_PENDING.with(|slot|slot.set(false));
    if let Some(gesture)=GESTURE.with(|slot|slot.borrow_mut().take()){if restore_selection{restore(&gesture);}}
    let owner=HOST_HWND.with(Cell::get);if owner!=0{let _=unsafe{KillTimer(Some(HWND(owner as*mut _)),TIMER)};}
    if OBSERVATION.with(|slot|slot.get().is_none()){CANCEL_PENDING.with(|slot|slot.set(false));}
}
fn desktop_foreground(desktop:HWND)->bool{
    foreground_is_desktop(desktop,unsafe{GetForegroundWindow()})
}
#[derive(Clone,Copy,Debug,PartialEq,Eq)]
enum ForegroundKind{Unknown,Desktop,OtherApplication}
fn foreground_kind(desktop:HWND,foreground:HWND)->ForegroundKind{
    if foreground.0.is_null()||!unsafe{IsWindow(Some(foreground))}.as_bool()||!unsafe{IsWindowVisible(foreground)}.as_bool(){return ForegroundKind::Unknown;}
    if foreground==desktop{return ForegroundKind::Desktop;}
    let class=class_name(foreground);if class.is_empty(){return ForegroundKind::Unknown;}
    if !matches!(class.as_str(),"WorkerW"|"Progman"){return ForegroundKind::OtherApplication;}
    let mut source=0;let mut active=0;
    unsafe{GetWindowThreadProcessId(desktop,Some(&mut source));GetWindowThreadProcessId(foreground,Some(&mut active));}
    if source==0||active==0{ForegroundKind::Unknown}else if source==active{ForegroundKind::Desktop}else{ForegroundKind::OtherApplication}
}
fn foreground_is_desktop(desktop:HWND,foreground:HWND)->bool{
    foreground_kind(desktop,foreground)==ForegroundKind::Desktop
}
fn foreground_cancels_marquee(desktop:HWND,foreground:HWND)->bool{
    foreground_kind(desktop,foreground)==ForegroundKind::OtherApplication
}
pub(super) fn tick(){
    if let Some(event)=OBSERVATION.with(Cell::get){
        // Losing activation may briefly yield no foreground window. That is
        // neither permission to activate an icon nor evidence of another app.
        if !event.finished&&(unsafe{GetAsyncKeyState(VK_ESCAPE.0 as i32)}<0||unsafe{GetAsyncKeyState(VK_LBUTTON.0 as i32)}>=0||foreground_cancels_marquee(event.desktop,unsafe{GetForegroundWindow()})){
            cancel_observation();
        }
    }
    dispatch();
}
pub(super) fn dispatch(){
    POSTED.with(|slot|slot.set(false));
    // Native enumeration/rendering can pump messages while HOST is borrowed.
    // Keep the complete gesture, including MouseUp/cancel, until it can apply.
    if DISPATCHING.with(|slot|slot.replace(true)){retry();return;}
    struct Dispatch;impl Drop for Dispatch{fn drop(&mut self){DISPATCHING.with(|slot|slot.set(false));}}let _dispatch=Dispatch;
    match HOST.with(|slot|slot.try_borrow_mut().map(|host|host.is_some())){
        Err(_)=>{retry();return;},Ok(false)=>{clear_pending(false);return;},Ok(true)=>{},
    }
    if CANCEL_PENDING.with(Cell::get){clear_pending(true);return;}
    let Some(event)=OBSERVATION.with(Cell::get)else{return;};
    if event.cancelled||unsafe{GetAsyncKeyState(VK_ESCAPE.0 as i32)}<0||!ENABLED.with(Cell::get)||!unsafe{IsWindow(Some(event.desktop))}.as_bool(){clear_pending(true);return;}
    let mut gesture=GESTURE.with(|slot|slot.borrow_mut().take());
    if gesture.as_ref().is_some_and(|state|state.generation!=event.generation){if let Some(old)=gesture.take(){restore(&old);}}
    if gesture.is_none(){
        let snapshot=with_host(|host|{
            host.can_start_desktop_selection().then(||{
                super::note_selection_input(host.hwnd,WM_LBUTTONDOWN);
                for window in host.fences.iter().filter(|window|window.standalone){zsdesk_windows::shell::file_transfer::note_user_input(window.hwnd);}
                original(host)
            })
        }).flatten();
        let Some(original)=snapshot else{OBSERVATION.with(|slot|slot.set(None));return;};
        gesture=Some(Gesture{generation:event.generation,desktop:event.desktop,start:event.start,control:event.control,shift:event.shift,original,dragged:false,overlay:None});
        let owner=HOST_HWND.with(Cell::get);if owner!=0{unsafe{SetTimer(Some(HWND(owner as*mut _)),TIMER,40,None);}}
    }
    let mut gesture=gesture.unwrap();gesture.dragged|=exceeded(gesture.start,event.current);
    apply(&gesture,event.current);
    if gesture.dragged&&!event.finished{
        if gesture.overlay.is_none(){gesture.overlay=Overlay::new(gesture.desktop);}
        if let Some(overlay)=gesture.overlay.as_mut(){overlay.draw(gesture.start,event.current);}
    }
    if event.finished{
        OBSERVATION.with(|slot|if slot.get().is_some_and(|current|current.generation==event.generation){slot.set(None)});
        let owner=HOST_HWND.with(Cell::get);if owner!=0{let _=unsafe{KillTimer(Some(HWND(owner as*mut _)),TIMER)};}
        // Remove transient windows before activation and layer reconciliation.
        // They must never be mistaken for an ordinary application in that pass.
        drop(gesture.overlay.take());
        // A completed desktop selection owns the next keyboard action, just as
        // selecting an icon by clicking it does. The marquee never activates.
        if gesture.dragged&&desktop_foreground(gesture.desktop){
            let selected=with_host(|host|{
                host.raise_above(gesture.desktop);
                host.fences.iter().find(|window|window.standalone&&!window.selection.is_empty()).map(|window|window.hwnd)
            }).flatten();
            if let Some(window)=selected{
                let _=unsafe{SetForegroundWindow(window)};
                // A denied activation must not be followed by SetFocus, which
                // can reorder an owned icon behind its desktop owner.
                if unsafe{GetForegroundWindow()}==window{let _=unsafe{windows::Win32::UI::Input::KeyboardAndMouse::SetFocus(Some(window))};}
                with_host(|host|host.repair_layer());
            }
        }
    }else{GESTURE.with(|slot|*slot.borrow_mut()=Some(gesture));}
}
fn union_rect(a:POINT,b:POINT)->RECT{RECT{left:a.x.min(b.x),top:a.y.min(b.y),right:a.x.max(b.x),bottom:a.y.max(b.y)}}
fn intersects(a:RECT,b:RECT)->bool{a.left<b.right&&a.right>b.left&&a.top<b.bottom&&a.bottom>b.top}
fn apply(gesture:&Gesture,point:POINT){
    let band=union_rect(gesture.start,point);
    with_host(|host|{
        let mut changed=Vec::new();
        for window in host.fences.iter_mut().filter(|window|window.standalone){
            let mut hits=HashSet::new();
            if gesture.dragged&&unsafe{IsWindowVisible(window.hwnd)}.as_bool(){
                if let Some(config)=host.config.fence(window.id){
                    let rect=window.window_rect();let geometry=window.geometry(config);
                    for (index,cell) in geometry.cells.iter().enumerate(){
                        let bounds=RECT{left:rect.left+cell.cell.x,top:rect.top+cell.cell.y,right:rect.left+cell.cell.right(),bottom:rect.top+cell.cell.bottom()};
                        if !intersects(band,bounds){continue;}
                        // A loose icon covered by another application or a
                        // fence must not be selected through that window.
                        let hit=POINT{x:((i64::from(bounds.left.max(band.left))+i64::from(bounds.right.min(band.right)))/2)as i32,
                            y:((i64::from(bounds.top.max(band.top))+i64::from(bounds.bottom.min(band.bottom)))/2)as i32};
                        if unsafe{WindowFromPoint(hit)}!=window.hwnd{continue;}
                        if let Some(entry)=window.entries.get(index){hits.insert(entry.key.clone());}
                    }
                }
            }
            let empty=HashSet::new();let base=gesture.original.get(&window.id).map_or(&empty,|saved|&saved.selection);
            let wanted=if gesture.control{base.symmetric_difference(&hits).cloned().collect()}else if gesture.shift{base.union(&hits).cloned().collect()}else{hits};
            if window.selection!=wanted{
                window.selection=wanted;window.focus=window.entries.iter().position(|entry|window.selection.contains(&entry.key));window.anchor=window.focus;
                window.pending_selection=None;changed.push(window.id);
            }
        }
        for id in changed{host.render(id);}
    });
}
unsafe extern "system" fn overlay_proc(hwnd:HWND,message:u32,wp:WPARAM,lp:LPARAM)->LRESULT{
    if message==WM_NCHITTEST{return LRESULT(HTTRANSPARENT as isize);}
    if message==WM_MOUSEACTIVATE{return LRESULT(MA_NOACTIVATE as isize);}
    unsafe{DefWindowProcW(hwnd,message,wp,lp)}
}
impl Overlay{
    fn new(desktop:HWND)->Option<Self>{
        let instance=HINSTANCE(unsafe{GetModuleHandleW(None)}.ok()?.0);
        unsafe{RegisterClassW(&WNDCLASSW{hInstance:instance,lpfnWndProc:Some(overlay_proc),lpszClassName:CLASS,..Default::default()});}
        let mut overlay=Self{desktop,segments:Vec::new()};
        for _ in 0..4{
            let hwnd=unsafe{CreateWindowExW(WS_EX_LAYERED|WS_EX_TRANSPARENT|WS_EX_TOOLWINDOW|WS_EX_NOACTIVATE,CLASS,PCWSTR::null(),WS_POPUP,0,0,1,1,Some(desktop),None,Some(instance),None)}.ok()?;
            overlay.segments.push((hwnd,None));
            place_border(hwnd,desktop,false);
        }Some(overlay)
    }
    fn draw(&mut self,start:POINT,current:POINT){
        let rect=union_rect(start,current);let width=(i64::from(rect.right)-i64::from(rect.left)).clamp(1,32767)as i32;let height=(i64::from(rect.bottom)-i64::from(rect.top)).clamp(1,32767)as i32;
        // A border costs O(width+height), avoiding a multi-monitor bitmap for
        // every mouse move. The centre remains completely transparent.
        let thickness=(unsafe{windows::Win32::UI::HiDpi::GetDpiForWindow(self.segments[0].0)}.max(96)/96).max(1).min(4)as i32;
        let rectangles=[(rect.left,rect.top,width,thickness),(rect.left,rect.top+height-thickness,width,thickness),
            (rect.left,rect.top,thickness,height),(rect.left+width-thickness,rect.top,thickness,height)];
        for ((window,slot),(x,y,width,height)) in self.segments.iter_mut().zip(rectangles){
            let Some(surface)=Surface::ensure(slot,width,height)else{continue;};
            let drawn=with_host(|host|host.gfx.draw(surface,|canvas|{
                canvas.fill(RectF::new(0.0,0.0,width as f32,height as f32),Color::rgba8(0,120,215,230));
            }).is_ok()).unwrap_or(false);
            if !drawn{continue;}
            let screen=unsafe{GetDC(None)};let size=SIZE{cx:width,cy:height};let origin=POINT::default();let position=POINT{x,y};
            let blend=BLENDFUNCTION{BlendOp:AC_SRC_OVER as u8,BlendFlags:0,SourceConstantAlpha:255,AlphaFormat:AC_SRC_ALPHA as u8};
            let _=unsafe{UpdateLayeredWindow(*window,Some(screen),Some(&position),Some(&size),Some(surface.dc),Some(&origin),Default::default(),Some(&blend),ULW_ALPHA)};
            unsafe{ReleaseDC(None,screen);}place_border(*window,self.desktop,true);
        }
    }
}
fn place_border(window:HWND,desktop:HWND,show:bool){
    let mut preceding=unsafe{GetWindow(desktop,GW_HWNDPREV)}.unwrap_or(HWND_TOP);
    while !preceding.0.is_null()&&class_name(preceding)=="ZSDesk.DesktopSelection"{
        preceding=unsafe{GetWindow(preceding,GW_HWNDPREV)}.unwrap_or(HWND_TOP);
    }
    let flags=SWP_NOACTIVATE|SWP_NOMOVE|SWP_NOSIZE|if show{SWP_SHOWWINDOW}else{SET_WINDOW_POS_FLAGS(0)};
    let _=unsafe{SetWindowPos(window,Some(preceding),0,0,0,0,flags)};
}

#[cfg(test)]#[path="outside_selection_tests.rs"]mod tests;
