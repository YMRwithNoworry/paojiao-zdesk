//! Right-button gestures are opt-out and retain an ordinary context click.
use super::*;

#[derive(Default)]
pub(super) struct MouseGestures { drag:Option<Drag> }
struct Drag {
    target:HWND, view:HWND, start:POINT, last:POINT,
    directions:Vec<u8>, threshold:i32, identity:pane_input::Identity,
}

fn direction(dx:i32,dy:i32,threshold:i32)->Option<u8> {
    if dx.abs().max(dy.abs())<threshold { return None; }
    Some(if dx.abs()>dy.abs() {if dx<0 {b'L'}else{b'R'}}else if dy<0{b'U'}else{b'D'})
}
fn command(directions:&[u8])->Option<Command> {
    match directions {
        b"L"=>Some(Command::Back),b"R"=>Some(Command::Forward),
        b"U"=>Some(Command::Up),b"D"=>Some(Command::Refresh),_=>None,
    }
}

impl MouseGestures {
    fn cancel(&mut self,hwnd:HWND) {
        self.drag=None;
        if unsafe{GetCapture()}==hwnd {let _=unsafe{ReleaseCapture()};}
    }
    pub(super) fn process(&mut self,hwnd:HWND,msg:&MSG,slot:&Slot)->bool {
        if self.drag.is_some()&&unsafe{GetCapture()}!=hwnd{self.drag=None;}
        if self.drag.is_some() && (msg.message==WM_CANCELMODE || (msg.message==WM_CAPTURECHANGED && msg.lParam.0!=hwnd.0 as isize)
            || (msg.message==WM_KEYDOWN && msg.wParam.0==VK_ESCAPE.0 as usize)) {
            let consume=msg.message==WM_KEYDOWN;self.cancel(hwnd);return consume;
        }
        if msg.message==WM_RBUTTONDOWN && self.drag.is_none() {
            let context=slot.try_borrow().ok().and_then(|slot|slot.as_ref().and_then(|state|{
                let(view,source)=state.file_view_for_input(msg.hwnd)?;
                (state.session.ui.mouse_gestures&&!text_input_window(msg.hwnd)
                    &&!zsdesk_windows::shell::menu::control_owns_context_menu(view,msg.hwnd))
                    .then(||state.identity_for_source(source).map(|identity|(view,identity,state.s(24)))).flatten()
            }));
            let Some((view,identity,threshold))=context else{return false;};
            let point=LPARAM(((msg.pt.x as u16 as u32)|((msg.pt.y as u16 as u32)<<16))as isize);
            if !matches!(zsdesk_windows::shell::menu::classify_view_menu_point(view,msg.hwnd,point),
                zsdesk_windows::shell::menu::ViewMenuTarget::Selection|zsdesk_windows::shell::menu::ViewMenuTarget::Background){return false;}
            let current=slot.try_borrow().ok().is_some_and(|slot|slot.as_ref().is_some_and(|state|
                state.file_view_for_input(msg.hwnd)==Some((view,identity.source))&&state.identity_for_source(identity.source)==Some(identity)));
            if !current{return false;}
            self.drag=Some(Drag{target:msg.hwnd,view,start:msg.pt,last:msg.pt,
                directions:Vec::new(),threshold,identity});
            unsafe{SetCapture(hwnd);}
            if unsafe{GetCapture()}!=hwnd{self.drag=None;return false;}
            return true;
        }
        if msg.message==WM_MOUSEMOVE {
            if let Some(drag)=self.drag.as_mut() {
                if msg.wParam.0 & 2 == 0 { self.cancel(hwnd);return false; }
                let dx=msg.pt.x-drag.last.x;let dy=msg.pt.y-drag.last.y;
                if let Some(next)=direction(dx,dy,drag.threshold) {
                    if drag.directions.last()!=Some(&next) && drag.directions.len()<8 {drag.directions.push(next);}
                    drag.last=msg.pt;
                }
                return true;
            }
        }
        if msg.message==WM_RBUTTONUP {
            let Some(drag)=self.drag.take() else{return false;};
            if unsafe{GetCapture()}==hwnd {let _=unsafe{ReleaseCapture()};}
            let current=slot.try_borrow().ok().and_then(|slot|slot.as_ref().map(|state|
                state.identity_for_source(drag.identity.source)==Some(drag.identity)&&state.file_view_for_input(drag.target)==Some((drag.view,drag.identity.source))
                    &&state.session.ui.mouse_gestures)).unwrap_or(false);
            if !current || !unsafe{IsWindow(Some(drag.target))}.as_bool(){return true;}
            if let Some(action)=command(&drag.directions) {
                pane_input::post(hwnd,drag.identity,pane_input::Intent::Action(action.id()as usize));
            } else if drag.directions.is_empty()
                && (msg.pt.x-drag.start.x).abs().max((msg.pt.y-drag.start.y).abs())<drag.threshold {
                // DirectUI versions differ in whether replayed button messages
                // synthesize WM_CONTEXTMENU. Dispatch one owned request instead;
                // the menu target resolver selects the clicked item itself.
                pane_input::post(hwnd,drag.identity,pane_input::Intent::ContextMenu(drag.start));
            }
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn jitter_and_unknown_gestures_never_invoke_commands() {
        assert_eq!(direction(12,-8,24),None);
        assert_eq!(direction(-40,3,24),Some(b'L'));
        assert_eq!(direction(1,40,24),Some(b'D'));
        assert_eq!(command(b""),None);assert_eq!(command(b"LR"),None);
        assert_eq!(command(b"L"),Some(Command::Back));assert_eq!(command(b"R"),Some(Command::Forward));
        assert_eq!(command(b"U"),Some(Command::Up));assert_eq!(command(b"D"),Some(Command::Refresh));
    }
}
