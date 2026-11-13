//! Own Return before Shell translation and snapshot text before deferred dispatch.
use super::*;

#[derive(Clone, Copy)]
struct Editing { owner: HWND, identity: pane_input::Identity, return_down: bool, submitted: bool }
thread_local! { static EDITS: RefCell<HashMap<isize, Editing>> = RefCell::new(HashMap::new()); }

pub(super) fn begin(edit: HWND, owner: HWND, identity: pane_input::Identity) {
    EDITS.with(|edits| { edits.borrow_mut().insert(edit.0 as isize, Editing { owner, identity, return_down: false, submitted: false }); });
}
pub(super) fn end(edit: HWND) {
    EDITS.with(|edits| { edits.borrow_mut().remove(&(edit.0 as isize)); });
}
pub(super) fn delivered(edit:HWND) {
    EDITS.with(|edits| {if let Some(context)=edits.borrow_mut().get_mut(&(edit.0 as isize)){context.submitted=false;}});
}

pub(super) fn pre_translate(message: &MSG) -> bool {
    handle(message.hwnd, message.message, message.wParam)
}

pub(super) fn handle(edit: HWND, message: u32, key: WPARAM) -> bool {
    if !matches!(message, WM_KEYDOWN | WM_KEYUP | WM_CHAR) || key.0 != VK_RETURN.0 as usize { return false; }
    let Some(context) = EDITS.with(|edits| edits.borrow().get(&(edit.0 as isize)).copied()) else { return false; };
    if message == WM_KEYUP {
        EDITS.with(|edits| { if let Some(context) = edits.borrow_mut().get_mut(&(edit.0 as isize)) { context.return_down = false; } });
        return true;
    }
    if edit_composing(edit) { return false; }
    if context.return_down || context.submitted { return true; }
    if message == WM_KEYDOWN {
        EDITS.with(|edits| { if let Some(context) = edits.borrow_mut().get_mut(&(edit.0 as isize)) { context.return_down = true; } });
    }
    // No WindowState borrow is held across native EDIT calls. A focus callback
    // may hide/reset the EDIT before the owner processes its mailbox ticket.
    let text = navigation::window_text(edit);
    let identity = pane_input::identity(context.owner, context.identity.source).unwrap_or(context.identity);
    if pane_input::post(context.owner, identity, pane_input::Intent::SubmitAddress(text)) {
        EDITS.with(|edits| {if let Some(context)=edits.borrow_mut().get_mut(&(edit.0 as isize)){context.submitted=true;}});
        diagnostics::record("navigation", "address-submit-captured", None);
    } else {
        diagnostics::record("navigation", "address-mailbox-full", Some(0x800700aau32 as i32));
    }
    true
}
