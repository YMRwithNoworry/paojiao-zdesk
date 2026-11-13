//! Route side buttons before the native Shell view handles them.
use super::*;
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::System::SystemServices::{APPCOMMAND_BROWSER_BACKWARD, APPCOMMAND_BROWSER_FORWARD};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Source { SideButton, AppCommand }
#[derive(Clone, Copy)]
struct LastNavigation { owner: isize, pane:usize, back: bool, source: Source, tick: u64 }
thread_local! { static LAST: Cell<Option<LastNavigation>> = const { Cell::new(None) }; }

fn owned(owner: HWND, target: HWND) -> bool {
    target == owner || unsafe { IsChild(owner, target) }.as_bool()
}

fn editing_or_modal(target: HWND) -> bool {
    if text_input_window(target) || text_input_window(unsafe { GetFocus() }) || zsdesk_windows::flyout::is_open() { return true; }
    let mut info = GUITHREADINFO { cbSize: size_of::<GUITHREADINFO>() as u32, ..Default::default() };
    unsafe { GetGUIThreadInfo(0, &mut info) }.is_ok()
        && info.flags.0 & (GUI_INMENUMODE.0 | GUI_INMOVESIZE.0 | GUI_POPUPMENUMODE.0 | GUI_SYSTEMMENUMODE.0) != 0
}

fn navigate(owner: HWND, target: HWND, back: bool, source: Source) {
    if editing_or_modal(target) { return; }
    let pointer=unsafe{GetWindowLongPtrW(owner,GWLP_USERDATA)}as *const Slot;
    if pointer.is_null(){return;}
    let identity=unsafe{&*pointer}.try_borrow().ok().and_then(|slot|slot.as_ref().and_then(|state|state.source_for_input(target).and_then(|source|state.identity_for_source(source))));
    let Some(identity)=identity else{return;};let pane=identity.source;
    let tick = unsafe { GetTickCount64() };
    // Some mouse drivers deliver both native button input and an app command.
    // Only suppress a matching pair of different forms, never repeated clicks.
    let duplicate = LAST.with(|last| {
        let duplicate = last.get().is_some_and(|previous| previous.owner == owner.0 as isize && previous.pane==pane && previous.back == back
            && previous.source != source && tick.saturating_sub(previous.tick) <= 150);
        last.set(if duplicate { None } else { Some(LastNavigation { owner: owner.0 as isize, pane, back, source, tick }) });
        duplicate
    });
    if !duplicate { pane_input::post(owner,identity,pane_input::Intent::Action(if back { Command::Back } else { Command::Forward }.id()as usize)); }
}

/// Called for queued input targeting the main window or any native child.
pub(super) fn pre_translate(owner: HWND, message: &MSG) -> bool {
    if !owned(owner, message.hwnd) { return false; }
    match message.message {
        WM_XBUTTONDOWN | WM_XBUTTONUP | WM_XBUTTONDBLCLK => {
            let button = (message.wParam.0 >> 16) as u16;
            if button != XBUTTON1 && button != XBUTTON2 { return false; }
            if message.message == WM_XBUTTONUP { navigate(owner, message.hwnd, button == XBUTTON1, Source::SideButton); }
            // Consuming the whole button sequence prevents DefWindowProc from
            // generating a second WM_APPCOMMAND or the Shell from navigating.
            true
        }
        WM_APPCOMMAND => app_command(owner, message.wParam, message.lParam),
        _ => false,
    }
}

/// WM_APPCOMMAND is also sent synchronously by native children and drivers.
pub(super) fn app_command(owner: HWND, wp: WPARAM, lp: LPARAM) -> bool {
    let command = ((lp.0 as u32 >> 16) & 0x0fff) as u32;
    let back = if command == APPCOMMAND_BROWSER_BACKWARD.0 { true }
        else if command == APPCOMMAND_BROWSER_FORWARD.0 { false } else { return false };
    let source = HWND(wp.0 as *mut _);
    if !source.0.is_null() && !owned(owner, source) { return false; }
    navigate(owner, if source.0.is_null() { owner } else { source }, back, Source::AppCommand);
    true
}
