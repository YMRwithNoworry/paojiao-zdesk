//! Covers sent wheel messages and native modal pumps as well as the main queue.
use super::*;
use windows::Win32::UI::Shell::GetWindowSubclass;
const SUBCLASS:usize=0x5a57484c;
#[derive(Clone,Copy)]
struct Route { owner:HWND, identity:pane_input::Identity }

unsafe extern "system" fn procedure(hwnd:HWND,msg:u32,wp:WPARAM,lp:LPARAM,id:usize,data:usize)->LRESULT {
    let route=unsafe{&*(data as *const Route)};
    if msg==WM_NCDESTROY {
        let _=unsafe{RemoveWindowSubclass(hwnd,Some(procedure),id)};
        unsafe{drop(Box::from_raw(data as *mut Route));}
        return unsafe{DefSubclassProc(hwnd,msg,wp,lp)};
    }
    if msg==WM_MOUSEWHEEL && (wp.0&8!=0 || unsafe{GetKeyState(VK_CONTROL.0 as i32)}<0) && !text_input_window(hwnd) {
        let delta=((wp.0>>16)as i16)as i32;
        let identity=pane_input::identity(route.owner,route.identity.source).unwrap_or(route.identity);
        if pane_input::post(route.owner,identity,pane_input::Intent::Wheel(delta)){return LRESULT(0);}
    }
    unsafe{DefSubclassProc(hwnd,msg,wp,lp)}
}

fn install_one(hwnd:HWND,route:Route) {
    if text_input_window(hwnd){return;}
    let mut previous=0usize;
    if unsafe{GetWindowSubclass(hwnd,Some(procedure),SUBCLASS,Some(&mut previous))}.as_bool() {
        // The view owns the registration; refresh its source after recreation.
        if previous!=0 {unsafe{*(previous as *mut Route)=route;}}return;
    }
    let pointer=Box::into_raw(Box::new(route));
    if !unsafe{SetWindowSubclass(hwnd,Some(procedure),SUBCLASS,pointer as usize)}.as_bool(){unsafe{drop(Box::from_raw(pointer));}}
}
unsafe extern "system" fn child_proc(hwnd:HWND,lp:LPARAM)->windows::core::BOOL {
    let route=unsafe{*(lp.0 as *const Route)};
    install_one(hwnd,route);true.into()
}
pub(super) fn install(view:HWND,owner:HWND,identity:pane_input::Identity) {
    if view.0.is_null(){return;}
    let route=Route{owner,identity};install_one(view,route);
    let _=unsafe{EnumChildWindows(Some(view),Some(child_proc),LPARAM((&route as *const Route)as isize))};
}
