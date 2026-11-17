//! A desktop drop surface exists only while a drag crosses from another app.
//! Normal desktop clicks and marquee selection keep reaching Explorer.
use super::*;
use std::time::{Duration,Instant};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState,VK_LBUTTON};

pub(super) const MESSAGE:u32=protocol::Message::ExternalDrop as u32;
pub(super) const TIMER:usize=protocol::HostTimer::ExternalDrop as usize;
#[derive(Clone,Copy)]struct Gesture{start:POINT,external:bool,requested:bool}
thread_local!{
    static GESTURE:Cell<Option<Gesture>>=const{Cell::new(None)};
    static SURFACES:RefCell<Option<return_to_desktop::DesktopDropSurfaces>>=const{RefCell::new(None)};
    static RELEASED:Cell<Option<Instant>>=const{Cell::new(None)};
}
fn background(point:POINT)->bool{
    let hit=unsafe{WindowFromPoint(point)};
    matches!(class_name(hit).as_str(),"Progman"|"WorkerW"|"SHELLDLL_DefView")
}
fn crossed(start:POINT,point:POINT)->bool{
    (i64::from(start.x)-i64::from(point.x)).abs()>=i64::from(unsafe{GetSystemMetrics(SM_CXDRAG)}.max(2))
        ||(i64::from(start.y)-i64::from(point.y)).abs()>=i64::from(unsafe{GetSystemMetrics(SM_CYDRAG)}.max(2))
}
pub(super) fn observe(message:u32,point:POINT){
    match message{
        WM_LBUTTONDOWN=>{
            let hit=unsafe{WindowFromPoint(point)};let mut pid=0;unsafe{GetWindowThreadProcessId(hit,Some(&mut pid));}
            let root=unsafe{GetAncestor(hit,GA_ROOT)};
            let external=pid!=0&&!matches!(class_name(root).as_str(),host::FENCE_CLASS|HOST_CLASS|"Progman"|"WorkerW")&&!background(point);
            GESTURE.with(|slot|slot.set(Some(Gesture{start:point,external,requested:false})));
        },
        WM_MOUSEMOVE=>GESTURE.with(|slot|{if let Some(mut gesture)=slot.get(){
            if gesture.external&&!gesture.requested&&crossed(gesture.start,point){
                gesture.requested=true;slot.set(Some(gesture));let owner=HOST_HWND.with(Cell::get);
                if owner!=0{let _=unsafe{PostMessageW(Some(HWND(owner as*mut _)),MESSAGE,WPARAM(0),LPARAM(0))};}
            }
        }}),
        WM_LBUTTONUP=>{GESTURE.with(|slot|slot.set(None));},
        _=>{},
    }
}
pub(super) fn cancel_observation(){GESTURE.with(|slot|slot.set(None));RELEASED.with(|slot|slot.set(Some(Instant::now())));}
pub(super) fn start(){
    if unsafe{GetAsyncKeyState(VK_LBUTTON.0 as i32)}>=0||unsafe{GetAsyncKeyState(VK_ESCAPE.0 as i32)}<0{return;}
    if !with_host(|host|host.config.settings.organize&&!host.config.settings.hidden&&host.internal_drag.is_none()).unwrap_or(false){return;}
    let owner=HWND(HOST_HWND.with(Cell::get)as*mut _);
    SURFACES.with(|slot|{if slot.borrow().is_none(){*slot.borrow_mut()=Some(return_to_desktop::DesktopDropSurfaces::external(owner));}});
    RELEASED.with(|slot|slot.set(None));unsafe{SetTimer(Some(owner),TIMER,100,None);}
}
pub(super) fn tick(){
    // OLE Drop may still be queued after the low-level mouse-up callback.
    // Leave time for it and keep the surfaces alive through Shell progress UI.
    if return_to_desktop::pending()>0{return;}
    if RELEASED.with(Cell::get).is_none()&&unsafe{GetAsyncKeyState(VK_LBUTTON.0 as i32)}<0&&unsafe{GetAsyncKeyState(VK_ESCAPE.0 as i32)}>=0{return;}
    SURFACES.with(|slot|{if let Some(surfaces)=slot.borrow().as_ref(){surfaces.pass_through();}});
    let done=RELEASED.with(|slot|match slot.get(){Some(at)=>at.elapsed()>=Duration::from_millis(500),None=>{slot.set(Some(Instant::now()));false}});
    if done{shutdown();}
}
pub(super) fn shutdown(){
    SURFACES.with(|slot|slot.borrow_mut().take());GESTURE.with(|slot|slot.set(None));RELEASED.with(|slot|slot.set(None));
    let owner=HOST_HWND.with(Cell::get);if owner!=0{let _=unsafe{KillTimer(Some(HWND(owner as*mut _)),TIMER)};}
}

#[cfg(test)]mod tests{
    use super::*;
    #[test]fn drag_threshold_handles_negative_monitor_coordinates(){
        assert!(!crossed(POINT{x:-1200,y:30},POINT{x:-1200,y:30}));
        assert!(crossed(POINT{x:-1200,y:30},POINT{x:400,y:500}));
    }
}
