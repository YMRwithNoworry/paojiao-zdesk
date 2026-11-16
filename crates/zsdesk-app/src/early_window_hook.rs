//! Synchronous show notifications with a small, inert native hook DLL.
use anyhow::{Context,Result};
use windows::{core::{s,PCWSTR},Win32::{Foundation::{FreeLibrary,HINSTANCE,HMODULE,HWND},System::LibraryLoader::{GetModuleHandleW,GetProcAddress,LoadLibraryExW,LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,LOAD_LIBRARY_SEARCH_SYSTEM32},UI::{Accessibility::{SetWinEventHook,UnhookWinEvent,HWINEVENTHOOK},WindowsAndMessaging::*}}};
pub(crate) const MESSAGE:u32=WM_APP+119;
pub(crate) struct Hook{window:HWND,class:PCWSTR,instance:HINSTANCE,module:HMODULE,event:HWINEVENTHOOK,cbt:HHOOK}
impl Hook{
    pub fn install(class:PCWSTR,proc:WNDPROC,explorer:bool)->Result<Self>{
        let instance=HINSTANCE(unsafe{GetModuleHandleW(None)}?.0);
        anyhow::ensure!(unsafe{RegisterClassW(&WNDCLASSW{hInstance:instance,lpszClassName:class,lpfnWndProc:proc,..Default::default()})}!=0,"early-window endpoint registration");
        let mut result=Self{window:HWND::default(),class,instance,module:HMODULE::default(),event:HWINEVENTHOOK::default(),cbt:HHOOK::default()};
        result.window=unsafe{CreateWindowExW(WS_EX_TOOLWINDOW|WS_EX_NOACTIVATE,class,None,WS_POPUP,-30000,-30000,1,1,None,None,Some(instance),None)}?;
        let path=std::env::current_exe()?.parent().context("executable directory")?.join(env!("ZSDESK_WINDOW_HOOK"));
        let path:Vec<u16>=path.as_os_str().to_string_lossy().encode_utf16().chain(Some(0)).collect();
        result.module=unsafe{LoadLibraryExW(PCWSTR(path.as_ptr()),None,LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR|LOAD_LIBRARY_SEARCH_SYSTEM32)}?;
        let event=unsafe{GetProcAddress(result.module,if explorer{s!("ZSDeskExplorerEvent")}else{s!("ZSDeskDialogEvent")})}.context("early-window event export")?;
        let cbt=unsafe{GetProcAddress(result.module,if explorer{s!("ZSDeskExplorerCbt")}else{s!("ZSDeskDialogCbt")})}.context("early-window activation export")?;
        result.event=unsafe{SetWinEventHook(EVENT_OBJECT_SHOW,EVENT_OBJECT_SHOW,Some(result.module),Some(std::mem::transmute(event)),0,0,WINEVENT_INCONTEXT|WINEVENT_SKIPOWNPROCESS)};
        anyhow::ensure!(!result.event.0.is_null(),"synchronous window event hook");
        result.cbt=unsafe{SetWindowsHookExW(WH_CBT,Some(std::mem::transmute(cbt)),Some(HINSTANCE(result.module.0)),0)}?;
        Ok(result)
    }
}
impl Drop for Hook{fn drop(&mut self){unsafe{
    if !self.cbt.0.is_null(){let _=UnhookWindowsHookEx(self.cbt);}
    if !self.event.0.is_null(){UnhookWinEvent(self.event);}
    if !self.window.0.is_null(){let _=DestroyWindow(self.window);}
    let _=UnregisterClassW(self.class,Some(self.instance));
    if !self.module.0.is_null(){let _=FreeLibrary(self.module);}
}}}
