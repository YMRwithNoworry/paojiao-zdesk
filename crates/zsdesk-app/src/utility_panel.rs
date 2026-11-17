//! Modeless, DPI-aware utility panels sharing the application palette.
use std::{cell::{RefCell,Cell},rc::Rc,path::{Path,PathBuf},sync::{Arc,Mutex,atomic::{AtomicBool,Ordering}}};
use anyhow::{Result,Context,ensure};
use windows::{core::{w,PCWSTR},Win32::{Foundation::*,Graphics::Gdi::*,System::LibraryLoader::GetModuleHandleW,
    UI::{Controls::*,HiDpi::GetDpiForWindow,Input::KeyboardAndMouse::*,Shell::{ShellExecuteW,SetWindowSubclass,RemoveWindowSubclass,DefSubclassProc},WindowsAndMessaging::*}}};
use crate::fast_search::{FastSearch,SearchOptions};
use zsdesk_ui::theme::{Palette,Mode};
#[path="utility_content.rs"]mod content;
use content::InfoRow;

const RESULTS:u32=WM_APP+101;const UPDATE:u32=WM_APP+102;const DEBOUNCE:usize=1;const DEFERRED_RESULTS:usize=2;
thread_local!{static SEARCH:Cell<isize>=const{Cell::new(0)};}
#[cfg(test)]thread_local!{static TEST_NO_ACTIVATE:Cell<bool>=const{Cell::new(false)};}
#[derive(Clone,Copy,PartialEq)]enum Kind{Search,About,Shortcuts}
struct UpdateReply{alive:AtomicBool,value:Mutex<Option<Result<crate::update_check::Outcome,String>>>}
struct Panel{hwnd:HWND,edit:HWND,list:HWND,action:HWND,close:HWND,font:HFONT,title_font:HFONT,brush:HBRUSH,input_brush:HBRUSH,
    dpi:u32,palette:Palette,kind:Kind,profile:PathBuf,body:String,status:String,paths:Vec<PathBuf>,engine:Option<FastSearch>,generation:u64,reply:Arc<UpdateReply>,adopted:bool,
    info:Vec<InfoRow>,scroll:i32,busy:bool,search_error:bool,update_release:Option<crate::update_check::Release>,update_package:Option<(PathBuf,bool,String)>}
impl Drop for Panel{fn drop(&mut self){self.reply.alive.store(false,Ordering::Release);if let Some(engine)=&self.engine{engine.cancel();}
    unsafe{let _=DeleteObject(self.font.into());let _=DeleteObject(self.title_font.into());let _=DeleteObject(self.brush.into());let _=DeleteObject(self.input_brush.into());}}}
fn wide(s:&str)->Vec<u16>{s.encode_utf16().chain(Some(0)).collect()}
fn rgb(c:zsdesk_ui::theme::Color)->COLORREF{COLORREF(u32::from(c.r)|(u32::from(c.g)<<8)|(u32::from(c.b)<<16))}
fn composite(fg:zsdesk_ui::theme::Color,bg:zsdesk_ui::theme::Color)->COLORREF{
    let mix=|a:u8,b:u8|(u32::from(a)*u32::from(fg.a)+u32::from(b)*(255-u32::from(fg.a))+127)/255;
    COLORREF(mix(fg.r,bg.r)|(mix(fg.g,bg.g)<<8)|(mix(fg.b,bg.b)<<16))
}
fn text(hwnd:HWND)->String{let mut b=vec![0;unsafe{GetWindowTextLengthW(hwnd)}.max(0)as usize+1];let n=unsafe{GetWindowTextW(hwnd,&mut b)}.max(0)as usize;String::from_utf16_lossy(&b[..n])}
fn set_text(hwnd:HWND,value:&str){let _=unsafe{SetWindowTextW(hwnd,PCWSTR(wide(value).as_ptr()))};}
pub(crate) fn dispatch_if_owned(message:&MSG)->bool{
    let root=unsafe{GetAncestor(message.hwnd,GA_ROOT)};let mut name=[0u16;64];let count=unsafe{GetClassNameW(root,&mut name)}.max(0)as usize;
    if String::from_utf16_lossy(&name[..count])!="ZSDesk.UtilityPanel"{return false;}
    unsafe{let _=TranslateMessage(message);DispatchMessageW(message);}true
}
fn search_exists()->bool{SEARCH.with(|slot|unsafe{IsWindow(Some(HWND(slot.get()as*mut _)))}.as_bool())}
pub(crate) fn search_open()->bool{SEARCH.with(|slot|{let hwnd=HWND(slot.get()as*mut _);unsafe{IsWindow(Some(hwnd))}.as_bool()&&(unsafe{GetWindowLongW(hwnd,GWL_STYLE)}as u32&WS_VISIBLE.0!=0)})}
pub(crate) fn close_search(){SEARCH.with(|slot|{let hwnd=HWND(slot.get()as*mut _);if unsafe{IsWindow(Some(hwnd))}.as_bool(){let _=unsafe{DestroyWindow(hwnd)};}});}
pub(crate) fn show_search(profile:&Path)->Result<()>{
    if search_exists(){
        let hwnd=SEARCH.with(|slot|HWND(slot.get()as*mut _));let ui=crate::storage::load(profile)?.ui;
        let dark=match ui.appearance.color_scheme{zsdesk_core::model::explorer::ColorScheme::Dark=>true,zsdesk_core::model::explorer::ColorScheme::Light=>false,_=>zsdesk_windows::theme::system_dark_mode()};
        let dark=dark&&!zsdesk_windows::theme::high_contrast();
        let pointer=unsafe{GetWindowLongPtrW(hwnd,GWLP_USERDATA)}as*const RefCell<Panel>;let slot=unsafe{&*pointer};
        {let mut panel=slot.borrow_mut();let palette=Palette::for_mode(if dark{Mode::Dark}else{Mode::Light});
            if palette.mode!=panel.palette.mode{panel.palette=palette;unsafe{DeleteObject(panel.brush.into());DeleteObject(panel.input_brush.into());}
                panel.brush=unsafe{CreateSolidBrush(rgb(palette.layer))};panel.input_brush=unsafe{CreateSolidBrush(rgb(palette.input_focused))};}
        }
        zsdesk_windows::theme::set_dark_frame(hwnd,dark);
        let activate=true;#[cfg(test)]let activate=activate&&!TEST_NO_ACTIVATE.with(Cell::get);
        unsafe{let _=ShowWindow(hwnd,if activate{SW_SHOWNORMAL}else{SW_SHOWNOACTIVATE});RedrawWindow(Some(hwnd),None,None,RDW_INVALIDATE|RDW_ALLCHILDREN|RDW_ERASE);
            if activate{let _=SetForegroundWindow(hwnd);PostMessageW(Some(hwnd),WM_APPPLUS_FOCUS,WPARAM(0),LPARAM(0)).ok();}}
        return Ok(());
    }
    show(HWND::default(),profile,Kind::Search,String::new())?;Ok(())
}
pub(crate) fn about(owner:HWND,profile:&Path)->Result<()>{show(owner,profile,Kind::About,
    zsdesk_core::tr_format!("ZSDesk {} {}\r\n\r\n多标签文件管理器与桌面格子\r\n\r\n项目主页  github.com/qiu7824/zsdesk" , env!("CARGO_PKG_VERSION"), crate::edition::name())).map(|_|())}
pub(crate) fn offer_update(owner:HWND,profile:&Path,release:crate::update_check::Release)->Result<()>{
    let hwnd=show(owner,profile,Kind::About,format!("ZSDesk {} {}",env!("CARGO_PKG_VERSION"),crate::edition::name()))?;
    let pointer=unsafe{GetWindowLongPtrW(hwnd,GWLP_USERDATA)}as*const RefCell<Panel>;
    if !pointer.is_null(){unsafe{&*pointer}.borrow_mut().update_result(Ok(crate::update_check::Outcome::Available(release)));}
    Ok(())
}
pub(crate) fn shortcuts(owner:HWND,profile:&Path,body:&str)->Result<()>{
    let prefs=crate::quick_search::control::load(profile)?.preferences;
    let body=zsdesk_core::tr_format!("{body}\n\n全局搜索\n{} 打开快速搜索（{}）" , prefs.hotkey, if prefs.enabled{zsdesk_core::i18n::text("已启用")}else{zsdesk_core::i18n::text("未启用")}, body=body);
    show(owner,profile,Kind::Shortcuts,body).map(|_|())
}
fn show(owner:HWND,profile:&Path,kind:Kind,body:String)->Result<HWND>{
    let ui=crate::storage::load(profile)?.ui;
    let dark=match ui.appearance.color_scheme{zsdesk_core::model::explorer::ColorScheme::Dark=>true,zsdesk_core::model::explorer::ColorScheme::Light=>false,_=>zsdesk_windows::theme::system_dark_mode()};
    let dark=dark&&!zsdesk_windows::theme::high_contrast();let palette=Palette::for_mode(if dark{Mode::Dark}else{Mode::Light});
    let instance=HINSTANCE(unsafe{GetModuleHandleW(None)?}.0);
    unsafe{RegisterClassW(&WNDCLASSW{lpfnWndProc:Some(procedure),hInstance:instance,hCursor:LoadCursorW(None,IDC_ARROW)?,
        hIcon:LoadIconW(Some(instance),PCWSTR(101usize as*const u16)).unwrap_or_default(),lpszClassName:w!("ZSDesk.UtilityPanel"),..Default::default()});}
    let caption=match kind{Kind::Search=>zsdesk_core::i18n::text("快速搜索"),Kind::About=>zsdesk_core::i18n::text("关于 ZSDesk"),Kind::Shortcuts=>zsdesk_core::i18n::text("键盘快捷键")};
    let info=if kind==Kind::Shortcuts{content::shortcut_rows(&body)}else{Vec::new()};
    let state=Rc::new(RefCell::new(Panel{hwnd:HWND::default(),edit:HWND::default(),list:HWND::default(),action:HWND::default(),close:HWND::default(),
        font:HFONT::default(),title_font:HFONT::default(),brush:HBRUSH::default(),input_brush:HBRUSH::default(),dpi:96,palette,kind,profile:profile.into(),body,
        status:match kind{Kind::Search=>zsdesk_core::i18n::text("输入关键词搜索文件和文件夹").into(),Kind::Shortcuts=>zsdesk_core::i18n::text("滚动查看全部快捷键 · Esc 关闭").into(),Kind::About=>String::new()},paths:Vec::new(),engine:None,generation:0,
        reply:Arc::new(UpdateReply{alive:AtomicBool::new(true),value:Mutex::new(None)}),adopted:false,info,scroll:0,busy:false,search_error:false,update_release:None,update_package:None}));
    let pointer=Rc::into_raw(state.clone());
    let result=unsafe{CreateWindowExW(WINDOW_EX_STYLE::default(),w!("ZSDesk.UtilityPanel"),PCWSTR(wide(caption).as_ptr()),WS_OVERLAPPED|WS_CAPTION|WS_SYSMENU|WS_THICKFRAME|WS_CLIPCHILDREN,
        CW_USEDEFAULT,CW_USEDEFAULT,740,560,(!owner.0.is_null()).then_some(owner),None,Some(instance),Some(pointer.cast()))};
    let hwnd=match result{Ok(hwnd)=>hwnd,Err(error)=>{if !state.borrow().adopted{unsafe{drop(Rc::from_raw(pointer));}}return Err(error.into());}};
    struct Owner(HWND);impl Drop for Owner{fn drop(&mut self){if !self.0.0.is_null(){let _=unsafe{DestroyWindow(self.0)};}}}let mut owned=Owner(hwnd);
    let initialized={let mut state=unsafe{&*pointer}.borrow_mut();state.adopted=true;state.hwnd=hwnd;state.dpi=unsafe{GetDpiForWindow(hwnd)}.max(96);state.init()};
    if let Err(error)=initialized{return Err(error);}
    zsdesk_windows::theme::set_dark_frame(hwnd,dark);zsdesk_windows::theme::round_corners(hwnd,false);
    let mut cursor=POINT::default();let _=unsafe{GetCursorPos(&mut cursor)};let monitor=unsafe{MonitorFromPoint(cursor,MONITOR_DEFAULTTONEAREST)};
    let mut info=MONITORINFO{cbSize:std::mem::size_of::<MONITORINFO>()as u32,..Default::default()};unsafe{GetMonitorInfoW(monitor,&mut info)}.ok()?;
    let scale=|n:i32|n*unsafe{GetDpiForWindow(hwnd)}.max(96)as i32/96;
    let width=scale(match kind{Kind::About=>560,Kind::Shortcuts=>800,Kind::Search=>760}).min(info.rcWork.right-info.rcWork.left);
    let height=scale(match kind{Kind::About=>430,Kind::Shortcuts=>640,Kind::Search=>540}).min(info.rcWork.bottom-info.rcWork.top);
    unsafe{SetWindowPos(hwnd,None,info.rcWork.left+(info.rcWork.right-info.rcWork.left-width)/2,info.rcWork.top+(info.rcWork.bottom-info.rcWork.top-height)/3,width,height,SWP_NOZORDER)?;}
    if kind==Kind::Search{SEARCH.with(|slot|slot.set(hwnd.0 as isize));}
    let activate=true;
    #[cfg(test)]let activate=activate&&!TEST_NO_ACTIVATE.with(Cell::get);
    unsafe{let _=ShowWindow(hwnd,if activate{SW_SHOWNORMAL}else{SW_SHOWNOACTIVATE});if activate{let _=SetForegroundWindow(hwnd);PostMessageW(Some(hwnd),WM_APP+103,WPARAM(0),LPARAM(0))?;}}
    owned.0=HWND::default();Ok(hwnd)
}
impl Panel{
    fn hide_search(&mut self){
        let _=unsafe{KillTimer(Some(self.hwnd),DEBOUNCE)};
        if let Some(engine)=&self.engine{engine.cancel();}
        self.generation=0;self.paths.clear();self.busy=false;self.search_error=false;self.status=zsdesk_core::i18n::text("输入关键词搜索文件和文件夹").into();
        set_text(self.edit,"");unsafe{SendMessageW(self.list,LB_RESETCONTENT,None,None);let _=EnableWindow(self.action,false);let _=ShowWindow(self.hwnd,SW_HIDE);}
    }
    fn s(&self,n:i32)->i32{n*self.dpi as i32/96}
    fn child(&self,class:PCWSTR,label:&str,id:usize,style:WINDOW_STYLE)->Result<HWND>{
        let style=if matches!(id,12|13|14){style|WINDOW_STYLE(BS_OWNERDRAW as u32)}else{style};
        let hwnd=unsafe{CreateWindowExW(WINDOW_EX_STYLE::default(),class,PCWSTR(wide(label).as_ptr()),WS_CHILD|WS_VISIBLE|style,0,0,0,0,Some(self.hwnd),Some(HMENU(id as*mut _)),None,None)}?;
        unsafe{SendMessageW(hwnd,WM_SETFONT,Some(WPARAM(self.font.0 as usize)),Some(LPARAM(0)));SetWindowSubclass(hwnd,Some(control_proc),1,self.hwnd.0 as usize).ok()?;}
        Ok(hwnd)
    }
    fn init(&mut self)->Result<()>{
        let mut font=LOGFONTW{lfHeight:-self.s(16),..Default::default()};let family=wide(&zsdesk_windows::gfx::system_ui_font(None).0);font.lfFaceName[..family.len().min(32)].copy_from_slice(&family[..family.len().min(32)]);
        self.font=unsafe{CreateFontIndirectW(&font)};font.lfHeight=-self.s(23);font.lfWeight=600;self.title_font=unsafe{CreateFontIndirectW(&font)};
        self.brush=unsafe{CreateSolidBrush(rgb(self.palette.layer))};self.input_brush=unsafe{CreateSolidBrush(rgb(self.palette.input_focused))};
        if self.kind==Kind::Search{
            self.edit=self.child(w!("EDIT"),"",10,WINDOW_STYLE(ES_AUTOHSCROLL as u32)|WS_TABSTOP)?;
            unsafe{SetWindowTheme(self.edit,w!(""),w!(""))?;SendMessageW(self.edit,EM_SETLIMITTEXT,Some(WPARAM(512)),None);}
            let cue=wide(zsdesk_core::i18n::text("搜索文件、文件夹和路径"));unsafe{SendMessageW(self.edit,EM_SETCUEBANNER,Some(WPARAM(1)),Some(LPARAM(cue.as_ptr()as isize)));}
        }else{
            self.edit=self.child(w!("STATIC"),&self.body,10,WINDOW_STYLE(windows::Win32::System::SystemServices::SS_OWNERDRAW.0)
                |if self.kind==Kind::Shortcuts{WS_TABSTOP|WS_VSCROLL}else{WINDOW_STYLE(0)})?;
            let _=unsafe{SetWindowTheme(self.edit,if self.palette.mode==Mode::Dark{w!("DarkMode_Explorer")}else{w!("Explorer")},PCWSTR::null())};
        }
        if self.kind==Kind::Search{
            self.list=self.child(w!("LISTBOX"),"",11,WINDOW_STYLE((LBS_NOTIFY|LBS_NOINTEGRALHEIGHT|LBS_OWNERDRAWFIXED|LBS_HASSTRINGS)as u32)|WS_VSCROLL|WS_TABSTOP)?;
            unsafe{SendMessageW(self.list,LB_SETITEMHEIGHT,Some(WPARAM(0)),Some(LPARAM(self.s(58)as isize)));}
            self.action=self.child(w!("BUTTON"),zsdesk_core::i18n::text("打开文件位置"),12,WS_TABSTOP)?;
            let _=unsafe{EnableWindow(self.action,false)};
        }else if self.kind==Kind::About{
            self.action=self.child(w!("BUTTON"),zsdesk_core::i18n::text("检查更新"),12,WS_TABSTOP)?;self.child(w!("BUTTON"),zsdesk_core::i18n::text("项目主页"),14,WS_TABSTOP)?;
            let automatic=self.child(w!("BUTTON"),zsdesk_core::i18n::text("启动时检查更新"),15,WS_TABSTOP|WINDOW_STYLE(BS_AUTOCHECKBOX as u32))?;
            unsafe{SendMessageW(automatic,BM_SETCHECK,Some(WPARAM(usize::from(crate::update_check::automatic(&self.profile)))),None);}
        }
        else{self.action=self.child(w!("BUTTON"),zsdesk_core::i18n::text("搜索快捷键设置"),12,WS_TABSTOP)?;}
        self.close=self.child(w!("BUTTON"),zsdesk_core::i18n::text("关闭"),13,WS_TABSTOP)?;self.layout();Ok(())
    }
    fn layout(&mut self){let mut r=RECT::default();if unsafe{GetClientRect(self.hwnd,&mut r)}.is_err(){return;}
        let s=|n|self.s(n);let width=r.right.max(0);let height=r.bottom.max(0);
        for (hwnd,x,y,w,h) in [(self.edit,s(if self.kind==Kind::Search{48}else{24}),s(if self.kind==Kind::Search{74}else{68}),width-s(if self.kind==Kind::Search{72}else{48}),if self.kind==Kind::Search{s(22)}else{height-s(160)}),
            (self.list,s(16),s(120),width-s(32),height-s(194)),(self.action,s(20),height-s(48),s(if self.kind==Kind::Shortcuts{176}else{148}),s(32)),(self.close,width-s(100),height-s(48),s(80),s(32))]{
            if !hwnd.0.is_null(){let _=unsafe{SetWindowPos(hwnd,None,x,y,w.max(1),h.max(1),SWP_NOZORDER|SWP_NOACTIVATE)};}}
        if let Ok(link)=unsafe{GetDlgItem(Some(self.hwnd),14)}{let _=unsafe{SetWindowPos(link,None,s(180),height-s(48),s(100),s(32),SWP_NOZORDER|SWP_NOACTIVATE)};}
        if let Ok(check)=unsafe{GetDlgItem(Some(self.hwnd),15)}{let _=unsafe{SetWindowPos(check,None,s(290),height-s(48),s(150),s(32),SWP_NOZORDER|SWP_NOACTIVATE)};}
        self.update_scroll();let _=unsafe{InvalidateRect(Some(self.edit),None,false)};let _=unsafe{InvalidateRect(Some(self.hwnd),None,false)};
    }
    fn search(&mut self){
        let query=text(self.edit);self.paths.clear();unsafe{SendMessageW(self.list,LB_RESETCONTENT,None,None);}
        if query.trim().is_empty(){if let Some(engine)=&self.engine{engine.cancel_query();}self.busy=false;self.search_error=false;self.status=zsdesk_core::i18n::text("输入关键词搜索文件和文件夹").into();return;}
        if self.engine.is_none(){match crate::storage::load(&self.profile).and_then(|session|FastSearch::for_profile(&self.profile,self.hwnd,RESULTS,SearchOptions::from_ui(&session.ui))){
            Ok(engine)=>self.engine=Some(engine),Err(error)=>{self.busy=false;self.search_error=true;self.status=zsdesk_core::tr_format!("搜索不可用：{error:#}" , error=error);return;}}}
        self.busy=true;self.search_error=false;self.generation=self.engine.as_ref().unwrap().start_page(&query,None,0,200,crate::fast_search::everything::SearchSort::NameAsc);self.status=zsdesk_core::i18n::text("正在搜索…").into();
    }
    fn invalidate_query(&mut self){
        self.generation=0;if let Some(engine)=&self.engine{engine.cancel_query();}self.paths.clear();
        self.busy=!text(self.edit).trim().is_empty();self.search_error=false;
        let _=unsafe{EnableWindow(self.action,false)};
        unsafe{SendMessageW(self.list,LB_RESETCONTENT,None,None);}self.status=zsdesk_core::i18n::text("正在等待输入…").into();
    }
    fn dpi_changed(&mut self,dpi:u32,rect:RECT){
        self.scroll=self.scroll*dpi.max(96)as i32/self.dpi.max(96)as i32;self.dpi=dpi.max(96);let old=self.font;let old_title=self.title_font;
        let mut font=LOGFONTW{lfHeight:-self.s(16),..Default::default()};let family=wide(&zsdesk_windows::gfx::system_ui_font(None).0);font.lfFaceName[..family.len().min(32)].copy_from_slice(&family[..family.len().min(32)]);
        self.font=unsafe{CreateFontIndirectW(&font)};font.lfHeight=-self.s(23);font.lfWeight=600;self.title_font=unsafe{CreateFontIndirectW(&font)};
        for hwnd in [self.edit,self.list,self.action,self.close]{if !hwnd.0.is_null(){unsafe{SendMessageW(hwnd,WM_SETFONT,Some(WPARAM(self.font.0 as usize)),Some(LPARAM(1)));}}}
        if let Ok(link)=unsafe{GetDlgItem(Some(self.hwnd),14)}{unsafe{SendMessageW(link,WM_SETFONT,Some(WPARAM(self.font.0 as usize)),Some(LPARAM(1)));}}
        unsafe{DeleteObject(old.into());DeleteObject(old_title.into());if !self.list.0.is_null(){SendMessageW(self.list,LB_SETITEMHEIGHT,Some(WPARAM(0)),Some(LPARAM(self.s(58)as isize)));}
            SetWindowPos(self.hwnd,None,rect.left,rect.top,rect.right-rect.left,rect.bottom-rect.top,SWP_NOZORDER|SWP_NOACTIVATE).ok();}
        self.layout();
    }
    fn paint(&self,dc:HDC){let mut r=RECT::default();let _=unsafe{GetClientRect(self.hwnd,&mut r)};unsafe{FillRect(dc,&r,self.brush);}
        let title=match self.kind{Kind::Search=>zsdesk_core::i18n::text("快速搜索"),Kind::About=>zsdesk_core::i18n::text("关于 ZSDesk"),Kind::Shortcuts=>zsdesk_core::i18n::text("键盘快捷键")};
        draw_text(dc,title,RECT{left:self.s(22),top:self.s(16),right:r.right-self.s(20),bottom:self.s(54)},self.title_font,rgb(self.palette.text));
        if self.kind==Kind::Search{
            let area=RECT{left:self.s(20),top:self.s(66),right:r.right-self.s(20),bottom:self.s(104)};
            content::rounded(dc,area,rgb(self.palette.input_focused),composite(self.palette.input_border,self.palette.layer),self.s(8));
            let pen=unsafe{CreatePen(PS_SOLID,self.s(1).max(1),composite(self.palette.text_secondary,self.palette.input_focused))};
            let old=unsafe{SelectObject(dc,pen.into())};let brush=unsafe{SelectObject(dc,GetStockObject(NULL_BRUSH))};
            unsafe{let _=Ellipse(dc,self.s(30),self.s(77),self.s(40),self.s(87));MoveToEx(dc,self.s(38),self.s(85),None);let _=LineTo(dc,self.s(44),self.s(91));SelectObject(dc,brush);SelectObject(dc,old);let _=DeleteObject(pen.into());}
            if unsafe{GetFocus()}==self.edit{let pen=unsafe{CreatePen(PS_SOLID,self.s(2).max(1),rgb(self.palette.accent))};let old=unsafe{SelectObject(dc,pen.into())};
                unsafe{MoveToEx(dc,area.left+self.s(6),area.bottom-self.s(2),None);let _=LineTo(dc,area.right-self.s(6),area.bottom-self.s(2));SelectObject(dc,old);let _=DeleteObject(pen.into());}}
        }
        draw_text(dc,&self.status,RECT{left:self.s(22),top:r.bottom-self.s(77),right:r.right-self.s(20),bottom:r.bottom-self.s(52)},self.font,
            if self.search_error{rgb(self.palette.danger)}else{composite(self.palette.text_secondary,self.palette.layer)});
    }
    fn paint_empty_results(&self,dc:HDC,rect:RECT){
        unsafe{FillRect(dc,&rect,self.input_brush);}
        let(query,title,detail)=(text(self.edit),if self.busy{zsdesk_core::i18n::text("正在搜索")}else if self.search_error{zsdesk_core::i18n::text("搜索暂不可用")}else{zsdesk_core::i18n::text("未找到匹配项")},zsdesk_core::i18n::text("可尝试缩短关键词，或检查搜索设置。"));
        let(title,detail)=if query.trim().is_empty(){(zsdesk_core::i18n::text("搜索你的文件"),zsdesk_core::i18n::text("输入文件名，或使用 ext:pdf 等条件筛选。"))}else{(title,detail)};
        let center=(rect.bottom/2-self.s(25)).max(self.s(12));
        for (value,y,color) in [(title,center,composite(self.palette.text,self.palette.input_focused)),(detail,center+self.s(34),composite(self.palette.text_secondary,self.palette.input_focused))]{
            let old=unsafe{SelectObject(dc,self.font.into())};let mut area=RECT{left:self.s(16),top:y,right:rect.right-self.s(16),bottom:y+self.s(28)};let mut text:Vec<_>=value.encode_utf16().collect();
            unsafe{SetBkMode(dc,TRANSPARENT);SetTextColor(dc,color);DrawTextW(dc,&mut text,&mut area,DT_CENTER|DT_SINGLELINE|DT_VCENTER|DT_END_ELLIPSIS);SelectObject(dc,old);}
        }
    }
    fn results(&mut self){let Some(engine)=&self.engine else{return;};for event in engine.poll(){
        if event.generation!=self.generation{continue;}if event.reset{self.paths.clear();unsafe{SendMessageW(self.list,LB_RESETCONTENT,None,None);}}
        self.busy=!event.finished;self.search_error=event.error.is_some()&&event.paths.is_empty()&&self.paths.is_empty();
        for path in event.paths{if self.paths.len()>=200{break;}if self.paths.contains(&path){continue;}
            let label=path.file_name().unwrap_or(path.as_os_str()).to_string_lossy().into_owned();self.paths.push(path);
            unsafe{SendMessageW(self.list,LB_ADDSTRING,None,Some(LPARAM(wide(&label).as_ptr()as isize)));}}
        let warning=event.volumes.iter().find_map(|volume|volume.warning.as_ref()).cloned();
        self.status=event.error.unwrap_or_else(||if event.finished{warning.map_or_else(||zsdesk_core::tr_format!("{} 个结果 · Enter 打开 · Esc 关闭" , self.paths.len()),|warning|zsdesk_core::tr_format!("{} 个结果 · {warning}" , self.paths.len(), warning=warning))}
            else if event.phase==crate::fast_search::IndexPhase::Snapshot{zsdesk_core::i18n::text("正在读取已有索引…").into()}
            else if event.scanning{if event.volumes.len()==1{zsdesk_core::tr_format!("补充 {} · 已处理 {} 项 · {} 个结果" , event.volumes[0].root.display(), event.scanned, self.paths.len())}
                else{zsdesk_core::tr_format!("{} 个结果 · 索引仍在更新" , self.paths.len())}}
            else{zsdesk_core::tr_format!("正在搜索 · {} 个结果" , self.paths.len())});
    }if !self.paths.is_empty()&&unsafe{SendMessageW(self.list,LB_GETCURSEL,None,None)}.0<0{unsafe{SendMessageW(self.list,LB_SETCURSEL,Some(WPARAM(0)),None);}}
        let _=unsafe{EnableWindow(self.action,!self.paths.is_empty())};
    }
    fn open_selected(&mut self,location:bool){let index=unsafe{SendMessageW(self.list,LB_GETCURSEL,None,None)}.0;let Some(path)=usize::try_from(index).ok().and_then(|index|self.paths.get(index)).cloned()else{return;};
        if location||path.is_dir(){let args=location_arguments(&path,location);
            match crate::runtime::enqueue_local(&self.profile,args){Ok(true)=>{let _=unsafe{PostMessageW(Some(self.hwnd),WM_CLOSE,WPARAM(0),LPARAM(0))};},Ok(false)=>self.status=zsdesk_core::i18n::text("文件窗口暂时不可用，请重试").into(),Err(error)=>self.status=zsdesk_core::tr_format!("无法打开位置：{error:#}" , error=error)}
        }else{let result=unsafe{ShellExecuteW(Some(self.hwnd),w!("open"),PCWSTR(wide(&path.to_string_lossy()).as_ptr()),None,None,SW_SHOWNORMAL)};
            if result.0 as isize<=32{self.status=zsdesk_core::i18n::text("无法打开文件，请检查关联程序和文件位置").into();}else{let _=unsafe{PostMessageW(Some(self.hwnd),WM_CLOSE,WPARAM(0),LPARAM(0))};}}
    }
    fn update_result(&mut self,result:Result<crate::update_check::Outcome,String>){
        use crate::update_check::Outcome;
        match result{
            Ok(Outcome::Available(release))=>{self.status=zsdesk_core::tr_format!("发现新版本 {} · 是否下载更新？" , release.tag);self.update_release=Some(release);set_text(self.action,zsdesk_core::i18n::text("下载更新"));},
            Ok(Outcome::Current(message))=>{self.status=message;self.update_release=None;set_text(self.action,zsdesk_core::i18n::text("检查更新"));},
            Ok(Outcome::Downloaded(path,portable,hash))=>{
                self.status=if portable{zsdesk_core::i18n::text("校验完成 · 退出程序后解压更新包，保留 data 目录").into()}else{zsdesk_core::i18n::text("校验完成 · 安装前请退出正在运行的 ZSDesk").into()};
                self.update_package=Some((path,portable,hash));set_text(self.action,if portable{zsdesk_core::i18n::text("打开更新包")}else{zsdesk_core::i18n::text("安装更新")});
            },Err(error)=>self.status=error,
        }
        unsafe{EnableWindow(self.action,true);InvalidateRect(Some(self.hwnd),None,false);}
    }
    fn updates(&mut self){
        if let Some((path,_,hash))=&self.update_package{
            if let Err(error)=crate::update_check::open_download(self.hwnd,path,hash){self.status=zsdesk_core::tr_format!("无法打开更新包：{error:#}" , error=error);self.update_package=None;set_text(self.action,zsdesk_core::i18n::text("重新下载"));}return;
        }
        let release=self.update_release.clone();
        self.status=if release.is_some(){zsdesk_core::i18n::text("正在下载并校验更新…").into()}else{zsdesk_core::i18n::text("正在检查更新…").into()};unsafe{EnableWindow(self.action,false);}
        let reply=self.reply.clone();let hwnd=self.hwnd.0 as isize;
        if std::thread::Builder::new().name("zsdesk-update-check".into()).spawn(move||{
            let result=match release{Some(release)=>crate::update_check::download(&release,&reply.alive),None=>crate::update_check::check()}.map_err(|error|zsdesk_core::tr_format!("更新失败：{error:#}" , error=error));
            if reply.alive.load(Ordering::Acquire){*reply.value.lock().unwrap_or_else(|e|e.into_inner())=Some(result);let _=unsafe{PostMessageW(Some(HWND(hwnd as*mut _)),UPDATE,WPARAM(0),LPARAM(0))};}
        }).is_err(){self.status=zsdesk_core::i18n::text("无法启动更新检查").into();unsafe{EnableWindow(self.action,true);}}
    }
}
fn location_arguments(path:&Path,reveal:bool)->Vec<std::ffi::OsString>{
    if reveal{vec!["--select".into(),path.as_os_str().to_owned()]}else{vec![path.as_os_str().to_owned()]}
}
fn draw_text(dc:HDC,value:&str,rect:RECT,font:HFONT,color:COLORREF){let old=unsafe{SelectObject(dc,font.into())};let mut r=rect;let mut value=wide(value);value.pop();unsafe{SetBkMode(dc,TRANSPARENT);SetTextColor(dc,color);DrawTextW(dc,&mut value,&mut r,DT_LEFT|DT_VCENTER|DT_SINGLELINE|DT_END_ELLIPSIS);SelectObject(dc,old);}}
unsafe extern "system" fn procedure(hwnd:HWND,msg:u32,wp:WPARAM,lp:LPARAM)->LRESULT{
    if msg==WM_NCCREATE{let create=unsafe{&*(lp.0 as*const CREATESTRUCTW)};let pointer=create.lpCreateParams as*const RefCell<Panel>;
        unsafe{&*pointer}.borrow_mut().adopted=true;unsafe{SetWindowLongPtrW(hwnd,GWLP_USERDATA,create.lpCreateParams as isize);}return unsafe{DefWindowProcW(hwnd,msg,wp,lp)};}
    let pointer=unsafe{GetWindowLongPtrW(hwnd,GWLP_USERDATA)}as*const RefCell<Panel>;
    if pointer.is_null(){return unsafe{DefWindowProcW(hwnd,msg,wp,lp)};}
    unsafe{Rc::increment_strong_count(pointer);}let slot=unsafe{Rc::from_raw(pointer)};
    if msg==WM_NCDESTROY{unsafe{SetWindowLongPtrW(hwnd,GWLP_USERDATA,0);let _=RemovePropW(hwnd,w!("ZSDesk.Utility.ClosePending"));}SEARCH.with(|slot|if slot.get()==hwnd.0 as isize{slot.set(0)});
        unsafe{drop(Rc::from_raw(pointer));}return unsafe{DefWindowProcW(hwnd,msg,wp,lp)};}
    if msg==WM_CLOSE{
        let Ok(mut state)=slot.try_borrow_mut()else{
            let _=unsafe{SetPropW(hwnd,w!("ZSDesk.Utility.ClosePending"),Some(windows::Win32::Foundation::HANDLE(1usize as*mut _)))};return LRESULT(0);
        };
        if state.kind==Kind::Search&&!crate::runtime::exiting(){state.hide_search();}else{drop(state);let _=unsafe{DestroyWindow(hwnd)};}return LRESULT(0);
    }
    if msg==WM_SETFOCUS{let _=unsafe{PostMessageW(Some(hwnd),WM_APPPLUS_FOCUS,WPARAM(0),LPARAM(0))};return LRESULT(0);}
    if msg==WM_APPPLUS_FOCUS{
        let target={let state=slot.borrow();if state.kind==Kind::About{state.action}else{state.edit}};
        if !target.0.is_null()&&unsafe{GetWindowLongW(hwnd,GWL_STYLE)}as u32&WS_VISIBLE.0!=0{let _=unsafe{SetFocus(Some(target))};}return LRESULT(0);
    }
    // DefWindowProc synchronously requests WM_PRINTCLIENT and child drawing.
    // System caption commands synchronously dispatch WM_CLOSE, WM_SIZE and
    // focus messages. They must never run while Panel is mutably borrowed.
    if matches!(msg,WM_PRINT|WM_SYSCOMMAND|WM_NCLBUTTONDOWN|WM_NCLBUTTONDBLCLK){return unsafe{DefWindowProcW(hwnd,msg,wp,lp)};}
    let Ok(mut state)=slot.try_borrow_mut()else{
        if msg==RESULTS||msg==WM_TIMER&&wp.0==DEFERRED_RESULTS{unsafe{SetTimer(Some(hwnd),DEFERRED_RESULTS,50,None);}return LRESULT(0);}
        return unsafe{DefWindowProcW(hwnd,msg,wp,lp)};
    };
    match msg{
        WM_SIZE=>state.layout(),WM_ERASEBKGND=>return LRESULT(1),
        WM_DPICHANGED if lp.0!=0=>state.dpi_changed((wp.0&0xffff)as u32,unsafe{*(lp.0 as*const RECT)}),
        WM_GETMINMAXINFO if lp.0!=0=>{let limits=unsafe{&mut*(lp.0 as*mut MINMAXINFO)};limits.ptMinTrackSize=POINT{x:state.s(if state.kind==Kind::About{560}else{600}),y:state.s(if state.kind==Kind::About{400}else{420})};},
        WM_APPPLUS_SCROLL=>state.scroll_info(wp.0 as u32,lp.0 as i32),
        WM_APPPLUS_EMPTY if lp.0==state.list.0 as isize=>{let mut rect=RECT::default();if unsafe{GetClientRect(state.list,&mut rect)}.is_ok(){state.paint_empty_results(HDC(wp.0 as*mut _),rect);}return LRESULT(0);},
        WM_CTLCOLOREDIT|WM_CTLCOLORSTATIC=>{let dc=HDC(wp.0 as*mut _);unsafe{SetTextColor(dc,rgb(state.palette.text));SetBkColor(dc,rgb(state.palette.input_focused));}return LRESULT(state.input_brush.0 as isize);},
        WM_CTLCOLORLISTBOX=>return LRESULT(state.input_brush.0 as isize),
        WM_PAINT=>{let mut ps=PAINTSTRUCT::default();let dc=unsafe{BeginPaint(hwnd,&mut ps)};state.paint(dc);unsafe{EndPaint(hwnd,&ps);}},
        WM_PRINTCLIENT=>{state.paint(HDC(wp.0 as*mut _));return LRESULT(0);},
        WM_DRAWITEM if lp.0!=0=>{let item=unsafe{&*(lp.0 as*const DRAWITEMSTRUCT)};
            if item.CtlID==10&&state.kind!=Kind::Search{state.paint_info(item.hDC,item.rcItem);return LRESULT(1);}
            if matches!(item.CtlID,12|13|14){let r=item.rcItem;let color=if item.itemState.0&ODS_SELECTED.0!=0{composite(state.palette.pressed,state.palette.layer)}else{rgb(state.palette.input_focused)};
                let primary=item.CtlID==12&&state.kind==Kind::About;let color=if primary{rgb(state.palette.accent)}else{color};
                unsafe{FillRect(item.hDC,&r,state.brush);}
                let brush=unsafe{CreateSolidBrush(color)};let pen=unsafe{CreatePen(PS_SOLID,1,composite(state.palette.divider,state.palette.input_focused))};let old_brush=unsafe{SelectObject(item.hDC,brush.into())};let old_pen=unsafe{SelectObject(item.hDC,pen.into())};
                unsafe{RoundRect(item.hDC,r.left,r.top,r.right-1,r.bottom-1,state.s(8),state.s(8));SelectObject(item.hDC,old_brush);SelectObject(item.hDC,old_pen);DeleteObject(brush.into());DeleteObject(pen.into());}
                draw_text(item.hDC,&text(item.hwndItem),RECT{left:r.left+state.s(12),right:r.right-state.s(8),..r},state.font,
                    if primary{rgb(state.palette.accent_text)}else{composite(if item.itemState.0&ODS_DISABLED.0!=0{state.palette.text_disabled}else{state.palette.text},state.palette.input_focused)});
                if item.itemState.0&ODS_FOCUS.0!=0{let focus=RECT{left:r.left+3,top:r.top+3,right:r.right-4,bottom:r.bottom-4};let _=unsafe{DrawFocusRect(item.hDC,&focus)};}
                return LRESULT(1);
            }
            if item.CtlID==11{if let Some(path)=state.paths.get(item.itemID as usize){
            let selected=item.itemState.0&ODS_SELECTED.0!=0;let brush=unsafe{CreateSolidBrush(if selected{composite(state.palette.hover,state.palette.input_focused)}else{rgb(state.palette.input_focused)})};unsafe{FillRect(item.hDC,&item.rcItem,brush);DeleteObject(brush.into());}
            let r=item.rcItem;draw_text(item.hDC,&path.file_name().unwrap_or(path.as_os_str()).to_string_lossy(),RECT{left:r.left+state.s(12),top:r.top+state.s(4),right:r.right-state.s(8),bottom:r.top+state.s(29)},state.font,rgb(state.palette.text));
            draw_text(item.hDC,&path.to_string_lossy(),RECT{left:r.left+state.s(12),top:r.top+state.s(29),right:r.right-state.s(8),bottom:r.bottom-state.s(4)},state.font,composite(state.palette.text_secondary,state.palette.input_focused));return LRESULT(1);}}},
        WM_COMMAND=>{let id=wp.0&0xffff;let event=(wp.0>>16)as u32;
            if id==13{unsafe{PostMessageW(Some(hwnd),WM_CLOSE,WPARAM(0),LPARAM(0)).ok();}}
            else if id==12{match state.kind{Kind::Search=>state.open_selected(true),Kind::About=>state.updates(),Kind::Shortcuts=>{
                if let Err(error)=crate::quick_search::launch(&state.profile,true){state.status=zsdesk_core::tr_format!("无法打开设置：{error:#}" , error=error);}
            }}}
            else if id==14&&state.kind==Kind::About{unsafe{ShellExecuteW(Some(hwnd),w!("open"),w!("https://github.com/qiu7824/zsdesk/releases"),None,None,SW_SHOWNORMAL);}}
            else if id==15&&state.kind==Kind::About{
                if let Ok(check)=unsafe{GetDlgItem(Some(hwnd),15)}{
                    let enabled=unsafe{SendMessageW(check,BM_GETCHECK,None,None)}.0!=0;
                    if let Err(error)=crate::update_check::set_automatic(&state.profile,enabled){state.status=zsdesk_core::tr_format!("无法保存更新设置：{error:#}" , error=error);unsafe{SendMessageW(check,BM_SETCHECK,Some(WPARAM(usize::from(!enabled))),None);}}
                }
            }
            else if id==10&&event==EN_CHANGE&&state.kind==Kind::Search{state.invalidate_query();unsafe{SetTimer(Some(hwnd),DEBOUNCE,200,None);}}
            else if id==11&&event==LBN_DBLCLK{state.open_selected(false);}},
        WM_TIMER if wp.0==DEBOUNCE=>{if unsafe{GetPropW(state.edit,w!("ZSDesk.ImeActive"))}.0.is_null(){let _=unsafe{KillTimer(Some(hwnd),DEBOUNCE)};state.search();}},
        WM_TIMER if wp.0==DEFERRED_RESULTS=>{let _=unsafe{KillTimer(Some(hwnd),DEFERRED_RESULTS)};state.results();},
        RESULTS=>state.results(),UPDATE=>{let result=state.reply.value.lock().unwrap_or_else(|e|e.into_inner()).take();if let Some(result)=result{state.update_result(result);}},
        WM_APPPLUS_OPEN=>state.open_selected(wp.0!=0),
        _=>{drop(state);return unsafe{DefWindowProcW(hwnd,msg,wp,lp)};},
    }
    if !matches!(msg,WM_PAINT|WM_ERASEBKGND){let _=unsafe{InvalidateRect(Some(hwnd),None,false)};if !state.list.0.is_null(){let _=unsafe{InvalidateRect(Some(state.list),None,false)};}if !state.action.0.is_null(){let _=unsafe{InvalidateRect(Some(state.action),None,false)};}}
    drop(state);
    if unsafe{RemovePropW(hwnd,w!("ZSDesk.Utility.ClosePending"))}.is_ok_and(|value|!value.0.is_null()){let _=unsafe{PostMessageW(Some(hwnd),WM_CLOSE,WPARAM(0),LPARAM(0))};}
    LRESULT(0)
}
const WM_APPPLUS_FOCUS:u32=WM_APP+103;const WM_APPPLUS_OPEN:u32=WM_APP+104;
const WM_APPPLUS_SCROLL:u32=WM_APP+105;
const WM_APPPLUS_EMPTY:u32=WM_APP+106;
#[cfg(test)]#[path="utility_panel_tests.rs"]mod tests;
unsafe extern "system" fn control_proc(hwnd:HWND,msg:u32,wp:WPARAM,lp:LPARAM,id:usize,data:usize)->LRESULT{
    let parent=HWND(data as*mut _);
    if msg==WM_IME_STARTCOMPOSITION{let _=unsafe{SetPropW(hwnd,w!("ZSDesk.ImeActive"),Some(HANDLE(1usize as*mut _)))};}
    if msg==WM_IME_ENDCOMPOSITION{let _=unsafe{RemovePropW(hwnd,w!("ZSDesk.ImeActive"))};}
    if msg==WM_NCDESTROY{let _=unsafe{RemovePropW(hwnd,w!("ZSDesk.ImeActive"))};let _=unsafe{RemoveWindowSubclass(hwnd,Some(control_proc),id)};}
    if matches!(msg,WM_PAINT|WM_PRINTCLIENT)&&unsafe{GetDlgCtrlID(hwnd)}==11&&unsafe{SendMessageW(hwnd,LB_GETCOUNT,None,None)}.0==0{
        let mut ps=PAINTSTRUCT::default();let dc=if msg==WM_PAINT{unsafe{BeginPaint(hwnd,&mut ps)}}else{HDC(wp.0 as*mut _)};
        unsafe{SendMessageW(parent,WM_APPPLUS_EMPTY,Some(WPARAM(dc.0 as usize)),Some(LPARAM(hwnd.0 as isize)));if msg==WM_PAINT{let _=EndPaint(hwnd,&ps);}}
        return LRESULT(0);
    }
    let info=unsafe{GetDlgCtrlID(hwnd)}==10&&unsafe{GetDlgItem(Some(parent),11)}.is_err();
    if info&&msg==WM_VSCROLL{let _=unsafe{SendMessageW(parent,WM_APPPLUS_SCROLL,Some(WPARAM(wp.0&0xffff)),None)};return LRESULT(0);}
    if info&&msg==WM_MOUSEWHEEL{let delta=((wp.0>>16)as u16)as i16;let _=unsafe{SendMessageW(parent,WM_APPPLUS_SCROLL,None,Some(LPARAM(delta as isize)))};return LRESULT(0);}
    if !unsafe{GetPropW(hwnd,w!("ZSDesk.ImeActive"))}.0.is_null(){return unsafe{DefSubclassProc(hwnd,msg,wp,lp)};}
    if msg==WM_KEYDOWN{
        if wp.0==VK_ESCAPE.0 as usize{unsafe{PostMessageW(Some(parent),WM_CLOSE,WPARAM(0),LPARAM(0)).ok();}return LRESULT(0);}
        if wp.0==VK_RETURN.0 as usize&&matches!(unsafe{GetDlgCtrlID(hwnd)},12|13|14){let _=unsafe{PostMessageW(Some(parent),WM_COMMAND,WPARAM(GetDlgCtrlID(hwnd)as usize),LPARAM(hwnd.0 as isize))};return LRESULT(0);}
        if wp.0==VK_RETURN.0 as usize&&matches!(unsafe{GetDlgCtrlID(hwnd)},10|11)&&unsafe{GetDlgItem(Some(parent),11)}.is_ok(){unsafe{PostMessageW(Some(parent),WM_APPPLUS_OPEN,WPARAM(0),LPARAM(0)).ok();}return LRESULT(0);}
        if info&&[VK_DOWN.0 as usize,VK_UP.0 as usize,VK_NEXT.0 as usize,VK_PRIOR.0 as usize,VK_HOME.0 as usize,VK_END.0 as usize].contains(&wp.0){
            let code=match wp.0 as u16{key if key==VK_UP.0=>0,key if key==VK_DOWN.0=>1,key if key==VK_PRIOR.0=>2,key if key==VK_NEXT.0=>3,key if key==VK_HOME.0=>6,_=>7};
            let _=unsafe{SendMessageW(parent,WM_APPPLUS_SCROLL,Some(WPARAM(code)),None)};return LRESULT(0);
        }
        if wp.0==VK_DOWN.0 as usize&&unsafe{GetDlgCtrlID(hwnd)}==10{if let Ok(list)=unsafe{GetDlgItem(Some(parent),11)}{let _=unsafe{SetFocus(Some(list))};return LRESULT(0);}}
        if wp.0==VK_TAB.0 as usize{if let Ok(next)=unsafe{GetNextDlgTabItem(parent,Some(hwnd),GetKeyState(VK_SHIFT.0 as i32)<0)}{let _=unsafe{SetFocus(Some(next))};}return LRESULT(0);}
    }
    unsafe{DefSubclassProc(hwnd,msg,wp,lp)}
}
