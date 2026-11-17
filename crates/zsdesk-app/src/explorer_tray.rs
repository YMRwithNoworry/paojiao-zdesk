//! Optional notification-area entry for the file manager host.
use windows::{core::{w,PCWSTR},Win32::{Foundation::{HWND,LPARAM,WPARAM},System::LibraryLoader::GetModuleHandleW,
    UI::{Shell::*,WindowsAndMessaging::*}}};

pub const MESSAGE:u32=WM_APP+114;
const ICON:u32=2;
const NIN_KEYSELECT:u32=NIN_SELECT|1;
#[derive(Default)]
pub struct Tray{window:HWND,added:bool,version4:bool}
#[derive(Clone,Copy)]pub enum Action{Open,Menu}
impl Tray{
    fn data(&self)->NOTIFYICONDATAW{NOTIFYICONDATAW{cbSize:std::mem::size_of::<NOTIFYICONDATAW>()as u32,hWnd:self.window,uID:ICON,..Default::default()}}
    pub fn enabled(&self)->bool{self.added}
    pub fn update(&mut self,window:HWND,enabled:bool){
        self.window=window;
        if !enabled{self.remove();return;}if self.added{return;}
        let mut data=self.data();data.uFlags=NIF_ICON|NIF_MESSAGE|NIF_TIP;data.uCallbackMessage=MESSAGE;
        data.hIcon=unsafe{GetModuleHandleW(None)}.ok().and_then(|module|unsafe{LoadIconW(Some(windows::Win32::Foundation::HINSTANCE(module.0)),PCWSTR(101usize as*const u16))}.ok()).unwrap_or_default();
        for (slot,value) in data.szTip.iter_mut().zip(zsdesk_core::i18n::text("ZSDesk 文件管理器").encode_utf16()){*slot=value;}
        self.added=unsafe{Shell_NotifyIconW(NIM_ADD,&data)}.as_bool();
        if self.added{data.Anonymous.uVersion=NOTIFYICON_VERSION_4;self.version4=unsafe{Shell_NotifyIconW(NIM_SETVERSION,&data)}.as_bool();}
    }
    pub fn recreated(&mut self){self.added=false;self.version4=false;}
    pub fn remove(&mut self){if self.added{let _=unsafe{Shell_NotifyIconW(NIM_DELETE,&self.data())};}self.added=false;self.version4=false;}
    pub fn action(&self,wp:WPARAM,lp:LPARAM)->Option<Action>{
        let (id,event)=if self.version4{(((lp.0 as usize)>>16)as u32,(lp.0 as u32)&0xffff)}else{(wp.0 as u32,lp.0 as u32)};
        if id!=ICON{return None;}
        match event{NIN_SELECT|NIN_KEYSELECT=>Some(Action::Open),WM_LBUTTONUP if !self.version4=>Some(Action::Open),
            WM_CONTEXTMENU=>Some(Action::Menu),WM_RBUTTONUP if !self.version4=>Some(Action::Menu),_=>None}
    }
    pub fn focus(&self){if self.added{let _=unsafe{Shell_NotifyIconW(NIM_SETFOCUS,&self.data())};}}
}
impl Drop for Tray{fn drop(&mut self){self.remove();}}
pub fn menu(window:HWND)->u32{
    let Ok(menu)=(unsafe{CreatePopupMenu()})else{return 0;};
    unsafe{
        let _=AppendMenuW(menu,MF_STRING,1,windows::core::PCWSTR(zsdesk_core::i18n::wide("打开文件管理器").as_ptr()));
        let _=AppendMenuW(menu,MF_STRING,2,windows::core::PCWSTR(zsdesk_core::i18n::wide("新建窗口").as_ptr()));
        let _=AppendMenuW(menu,MF_SEPARATOR,0,PCWSTR::null());
        let _=AppendMenuW(menu,MF_STRING,3,windows::core::PCWSTR(zsdesk_core::i18n::wide("退出 ZSDesk").as_ptr()));
        let mut point=windows::Win32::Foundation::POINT::default();let _=GetCursorPos(&mut point);let _=SetForegroundWindow(window);
        let choice=TrackPopupMenu(menu,TPM_RETURNCMD|TPM_RIGHTBUTTON,point.x,point.y,None,window,None).0 as u32;
        let _=DestroyMenu(menu);let _=PostMessageW(Some(window),WM_NULL,WPARAM(0),LPARAM(0));choice
    }
}

#[cfg(test)]mod tests{
    use super::*;
    #[test]fn tray_notifications_activate_once_and_ignore_other_icons(){
        let mut tray=Tray{version4:true,..Default::default()};
        let event=|id:u32,code:u32|LPARAM(((id<<16)|code)as isize);
        assert!(matches!(tray.action(WPARAM(0),event(ICON,NIN_SELECT)),Some(Action::Open)));
        assert!(matches!(tray.action(WPARAM(0),event(ICON,NIN_KEYSELECT)),Some(Action::Open)));
        assert!(tray.action(WPARAM(0),event(ICON,WM_LBUTTONUP)).is_none());
        assert!(tray.action(WPARAM(0),event(ICON+1,NIN_SELECT)).is_none());
        assert!(matches!(tray.action(WPARAM(0),event(ICON,WM_CONTEXTMENU)),Some(Action::Menu)));
        tray.version4=false;
        assert!(matches!(tray.action(WPARAM(ICON as usize),LPARAM(WM_LBUTTONUP as isize)),Some(Action::Open)));
        assert!(tray.action(WPARAM(ICON as usize),LPARAM(WM_LBUTTONDBLCLK as isize)).is_none());
    }
}
