//! Window and file-view actions, shared by pointer and keyboard input.
use super::*;
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST};

pub(super) const FULLSCREEN: PCWSTR = windows::core::w!("ZSDesk.Fullscreen");
#[derive(Clone,Copy)]
pub(super) struct FullscreenRestore { pub placement: WINDOWPLACEMENT, style: isize }

pub(super) fn fullscreen(hwnd: HWND) -> bool {
    !unsafe { GetPropW(hwnd, FULLSCREEN) }.0.is_null()
}

pub(super) fn view_preset(number: u8) -> Option<(i32, i32)> {
    use windows::Win32::UI::Shell::*;
    Some(match number {
        1 => (FVM_ICON.0, 256), 2 => (FVM_ICON.0, 128),
        3 => (FVM_ICON.0, 48), 4 => (FVM_SMALLICON.0, 16),
        5 => (FVM_LIST.0, 16), 6 => (FVM_DETAILS.0, 16),
        7 => (FVM_TILE.0, 48), 8 => (FVM_CONTENT.0, 48), _ => return None,
    })
}

pub(super) fn next_icon_size(size: i32, increase: bool) -> i32 {
    const SIZES: [i32; 9] = [16, 24, 32, 48, 64, 96, 128, 192, 256];
    if increase { SIZES.into_iter().find(|value| *value > size).unwrap_or(256) }
    else { SIZES.into_iter().rev().find(|value| *value < size).unwrap_or(16) }
}

impl WindowState {
    pub(super) fn set_view_dimensions(&mut self, mode: i32, size: i32) {
        if self.pending.is_some() { return; }
        if let Some(browser) = &self.browser {
            match browser.set_view_mode(mode, size) {
                Ok(()) => { self.view_mode = browser.view_mode().unwrap_or((mode, size)); unsafe { self.capture_active(); } self.save(); self.invalidate(); }
                Err(error) => self.show_message(format!("无法切换视图：{error}")),
            }
        }
    }
    pub(super) fn zoom_view(&mut self, increase: bool) {
        let Some((_, size)) = self.browser.as_ref().and_then(|browser| browser.view_mode()) else { return; };
        self.set_view_dimensions(windows::Win32::UI::Shell::FVM_ICON.0, next_icon_size(size, increase));
    }
    /// Mouse wheel coordinates are screen coordinates even for child HWNDs.
    pub(super) fn wheel_zoom(&mut self, message: &MSG) -> bool {
        if message.message != WM_MOUSEWHEEL || (message.wParam.0 & 8 == 0 && unsafe{GetKeyState(VK_CONTROL.0 as i32)}>=0) || self.pending.is_some() { return false; }
        if text_input_window(message.hwnd) { return false; }
        let Some(view) = self.browser.as_ref().and_then(|browser| browser.view_window()) else { return false; };
        let mut bounds = RECT::default();
        if unsafe { GetWindowRect(view, &mut bounds) }.is_err() { return false; }
        let p = POINT { x: x_lparam(message.lParam.0), y: y_lparam(message.lParam.0) };
        if p.x < bounds.left || p.x >= bounds.right || p.y < bounds.top || p.y >= bounds.bottom { return false; }
        self.apply_wheel_delta(((message.wParam.0 >> 16) as i16) as i32);
        true
    }
    pub(super) fn apply_wheel_delta(&mut self,delta:i32) {
        if self.pending.is_some() {return;}
        self.wheel_remainder = self.wheel_remainder.saturating_add(delta);
        let steps = (self.wheel_remainder / 120).clamp(-16, 16);
        self.wheel_remainder %= 120;
        if steps!=0 {
            if let Some((_,mut size))=self.browser.as_ref().and_then(|b|b.view_mode()) {
                for _ in 0..steps.unsigned_abs(){size=next_icon_size(size,steps>0);}
                self.set_view_dimensions(windows::Win32::UI::Shell::FVM_ICON.0,size);
            }
        }
    }
    pub(super) fn cycle_focus(&mut self, previous: bool) {
        let focus = unsafe { GetFocus() };
        let browser = self.browser.as_ref().and_then(|browser| browser.view_window());
        let tree = self.tree.as_ref().map(|tree| tree.hwnd());
        let mut panes = vec![self.address_edit, self.search_edit];
        if let Some(tree) = tree.filter(|_| self.session.ui.show_tree&&!self.layout.tree.is_empty()) { panes.push(tree); }
        if let Some(browser) = browser { panes.push(browser); }
        let current = panes.iter().position(|pane| *pane == focus || unsafe { IsChild(*pane, focus) }.as_bool());
        let next = match current { Some(i) => (i + panes.len() + if previous { panes.len() - 1 } else { 1 }) % panes.len(), None => 0 };
        if panes[next] == self.address_edit { unsafe { self.focus_address(); } }
        else {
            self.end_address_edit();
            if Some(panes[next]) == browser { self.focus_view(); }
            else { let _ = unsafe { SetFocus(Some(panes[next])) }; }
        }
    }
    pub(super) fn toggle_fullscreen(&mut self) {
        let result = (|| -> windows::core::Result<()> {
            if let Some(saved) = self.fullscreen_restore {
                unsafe {
                    let _ = RemovePropW(self.hwnd, FULLSCREEN);
                    SetWindowLongPtrW(self.hwnd, GWL_STYLE, saved.style);
                    SetWindowPlacement(self.hwnd, &saved.placement)?;
                    SetWindowPos(self.hwnd, None, 0, 0, 0, 0, SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE)?;
                }
                self.fullscreen_restore=None;
            } else {
                let mut placement = WINDOWPLACEMENT { length: size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
                unsafe { GetWindowPlacement(self.hwnd, &mut placement)?; }
                self.remember_placement();
                // Placement owns maximize/minimize state; do not reapply those
                // transient style bits after ShowWindow(SW_RESTORE).
                let style = unsafe { GetWindowLongPtrW(self.hwnd, GWL_STYLE) } & !((WS_MAXIMIZE|WS_MINIMIZE).0 as isize);
                let monitor = unsafe { MonitorFromWindow(self.hwnd, MONITOR_DEFAULTTONEAREST) };
                let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
                unsafe { GetMonitorInfoW(monitor, &mut info) }.ok()?;
                self.fullscreen_restore = Some(FullscreenRestore { placement, style });
                unsafe {
                    SetPropW(self.hwnd, FULLSCREEN, Some(windows::Win32::Foundation::HANDLE(1usize as *mut _)))?;
                    let _ = ShowWindow(self.hwnd, SW_RESTORE);
                    SetWindowLongPtrW(self.hwnd, GWL_STYLE, style & !((WS_CAPTION|WS_THICKFRAME).0 as isize));
                    let r = info.rcMonitor;
                    SetWindowPos(self.hwnd, None, r.left, r.top, r.right-r.left, r.bottom-r.top, SWP_FRAMECHANGED | SWP_NOZORDER | SWP_NOACTIVATE)?;
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            // Recover the original frame on every partial transition. Keep
            // the recovery state if restoring placement itself fails.
            if let Some(saved)=self.fullscreen_restore{
                unsafe{
                    let _=RemovePropW(self.hwnd,FULLSCREEN);
                    SetWindowLongPtrW(self.hwnd,GWL_STYLE,saved.style);
                    if SetWindowPlacement(self.hwnd,&saved.placement).is_ok(){self.fullscreen_restore=None;}
                    let _=SetWindowPos(self.hwnd,None,0,0,0,0,SWP_FRAMECHANGED|SWP_NOMOVE|SWP_NOSIZE|SWP_NOZORDER|SWP_NOACTIVATE);
                }
            }
            self.show_message(format!("无法切换全屏：{error}"));
        }
        unsafe { self.resize(); }
        self.remember_placement();self.save();
        // Repaint native child panes after DWM changes the frame dimensions.
        let _=unsafe{windows::Win32::Graphics::Gdi::RedrawWindow(Some(self.hwnd),None,None,
            windows::Win32::Graphics::Gdi::RDW_INVALIDATE|windows::Win32::Graphics::Gdi::RDW_FRAME|windows::Win32::Graphics::Gdi::RDW_ALLCHILDREN)};
    }
    pub(super) fn new_window(&mut self) {
        let location=self.current_location().clone();
        self.open_in_new_window(&location);
    }
    /// Starts another file window at `location` in the same profile.
    pub(super) fn open_in_new_window(&mut self,location:&ShellLocation)->bool {
        let profile=self.config_path.parent().unwrap_or(std::path::Path::new("."));
        let arguments=match crate::app::new_window_arguments(profile,location){Ok(args)=>args,Err(error)=>{self.show_message(error.to_string());return false;}};
        match crate::runtime::enqueue_local(&self.config_path,arguments.clone()){Ok(true)=>return true,Err(error)=>{self.show_message(error.to_string());return false;},Ok(false)=>{}}
        let executable=match std::env::current_exe(){Ok(path)=>path,Err(error)=>{self.show_message(error.to_string());return false;}};
        if !crate::app::window_command_line_fits(&executable,&arguments){self.show_message("窗口位置超过系统命令行容量，标签页已保留".into());return false;}
        let mut command = std::process::Command::new(executable);
        command.args(arguments);
        match command.spawn(){Ok(_)=>true,Err(error)=>{self.show_message(format!("无法打开新窗口：{error}"));false}}
    }
    /// A tab dragged out of the tab strip continues in its own window.
    pub(super) unsafe fn tear_off_tab(&mut self,index:usize) {
        let Some(tab)=self.session.tabs.get(index) else{return;};
        if self.session.tabs.len()<2{self.show_message("只有一个标签页，已保留在当前窗口".into());return;}
        if tab.locked{self.show_message("已锁定的标签页不能拖出为新窗口".into());return;}
        let location=tab.current().clone();
        // Open first: if the new window cannot start, the tab stays here.
        if self.open_in_new_window(&location){unsafe{self.close_tab(index);}}
    }
}

#[cfg(test)] mod tests {
    use super::*;
    #[test] fn wheel_sizes_are_reversible_and_clamped() {
        for size in [24,32,48,64,96,128,192] {assert_eq!(next_icon_size(next_icon_size(size,true),false),size);}
        assert_eq!(next_icon_size(16,false),16);assert_eq!(next_icon_size(256,true),256);
        assert_eq!(next_icon_size(60,true),64);assert_eq!(next_icon_size(60,false),48);
    }
    #[test] fn explorer_view_shortcuts_have_distinct_modes() {
        assert_eq!(view_preset(6),Some((windows::Win32::UI::Shell::FVM_DETAILS.0,16)));
        assert!(view_preset(0).is_none());assert!(view_preset(9).is_none());
    }
}
