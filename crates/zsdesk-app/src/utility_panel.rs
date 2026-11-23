//! Modeless, DPI-aware utility panels sharing the application palette.
use std::{cell::{RefCell,Cell},rc::Rc,path::{Path,PathBuf},sync::{Arc,Mutex,atomic::{AtomicBool,Ordering}}};
use anyhow::{Result,Context,ensure};
use windows::{core::{w,PCWSTR},Win32::{Foundation::*,Graphics::Gdi::*,System::LibraryLoader::GetModuleHandleW,
    UI::{Controls::*,HiDpi::GetDpiForWindow,Input::KeyboardAndMouse::*,Shell::{ShellExecuteW,SetWindowSubclass,RemoveWindowSubclass,DefSubclassProc},WindowsAndMessaging::*}}};
use crate::fast_search::{FastSearch,SearchOptions};
use zsdesk_ui::theme::{Palette,Mode};
#[path="utility_content.rs"]mod content;
use content::InfoRow;
use crate::{report_list,fast_search::everything::SearchSort,quick_search::details};
const PAGE_SIZE:u32=200;
const HEADINGS:[&str;4]=["名称","路径","大小","修改时间"];

const RESULTS:u32=WM_APP+101;const UPDATE:u32=WM_APP+102;const DEBOUNCE:usize=1;const DEFERRED_RESULTS:usize=2;const DEFERRED_DETAILS:usize=3;
thread_local!{static SEARCH:Cell<isize>=const{Cell::new(0)};}
#[cfg(test)]thread_local!{static TEST_NO_ACTIVATE:Cell<bool>=const{Cell::new(false)};}
#[derive(Clone,Copy,PartialEq)]enum Kind{Search,About,Shortcuts}
fn panel_title(kind:Kind)->&'static str{
    zsdesk_core::i18n::text(match kind{Kind::Search=>"快速搜索",Kind::About=>"关于 ZSDesk",Kind::Shortcuts=>"键盘快捷键"})
}
struct UpdateReply{alive:AtomicBool,value:Mutex<Option<Result<crate::update_check::Outcome,String>>>}
struct Panel{hwnd:HWND,edit:HWND,list:HWND,action:HWND,close:HWND,font:HFONT,title_font:HFONT,brush:HBRUSH,input_brush:HBRUSH,
    dpi:u32,palette:Palette,kind:Kind,profile:PathBuf,body:String,status:String,paths:Vec<PathBuf>,engine:Option<FastSearch>,generation:u64,reply:Arc<UpdateReply>,adopted:bool,
    sort:SearchSort,offset:u32,has_next:bool,detail_reader:Option<details::Reader>,
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
                panel.brush=unsafe{CreateSolidBrush(rgb(palette.layer))};panel.input_brush=unsafe{CreateSolidBrush(rgb(palette.input_focused))};panel.style_list();}
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
    show(owner,profile,Kind::Shortcuts,shortcut_body(profile, body)?).map(|_|())
}
fn shortcut_body(profile:&Path, body:&str)->Result<String>{
    let prefs=crate::quick_search::control::load(profile)?.preferences;
    Ok(zsdesk_core::tr_format!("{body}\n\n全局搜索\n{} 打开快速搜索（{}）" , prefs.hotkey, if prefs.enabled{zsdesk_core::i18n::text("已启用")}else{zsdesk_core::i18n::text("未启用")}, body=body))
}
fn show(owner:HWND,profile:&Path,kind:Kind,body:String)->Result<HWND>{
    let ui=crate::storage::load(profile)?.ui;
    let dark=match ui.appearance.color_scheme{zsdesk_core::model::explorer::ColorScheme::Dark=>true,zsdesk_core::model::explorer::ColorScheme::Light=>false,_=>zsdesk_windows::theme::system_dark_mode()};
    let dark=dark&&!zsdesk_windows::theme::high_contrast();let palette=Palette::for_mode(if dark{Mode::Dark}else{Mode::Light});
    let instance=HINSTANCE(unsafe{GetModuleHandleW(None)?}.0);
    unsafe{RegisterClassW(&WNDCLASSW{lpfnWndProc:Some(procedure),hInstance:instance,hCursor:LoadCursorW(None,IDC_ARROW)?,
        hIcon:LoadIconW(Some(instance),PCWSTR(101usize as*const u16)).unwrap_or_default(),lpszClassName:w!("ZSDesk.UtilityPanel"),..Default::default()});}
    let caption=panel_title(kind);
    let info=if kind==Kind::Shortcuts{content::shortcut_rows(&body)}else{Vec::new()};
    let state=Rc::new(RefCell::new(Panel{hwnd:HWND::default(),edit:HWND::default(),list:HWND::default(),action:HWND::default(),close:HWND::default(),
        font:HFONT::default(),title_font:HFONT::default(),brush:HBRUSH::default(),input_brush:HBRUSH::default(),dpi:96,palette,kind,profile:profile.into(),body,
        status:match kind{Kind::Search=>zsdesk_core::i18n::text("输入关键词搜索文件和文件夹").into(),Kind::Shortcuts=>zsdesk_core::i18n::text("滚动查看全部快捷键 · Esc 关闭").into(),Kind::About=>String::new()},paths:Vec::new(),engine:None,generation:0,
        reply:Arc::new(UpdateReply{alive:AtomicBool::new(true),value:Mutex::new(None)}),adopted:false,sort:SearchSort::NameAsc,offset:0,has_next:false,detail_reader:None,info,scroll:0,busy:false,search_error:false,update_release:None,update_package:None}));
    let pointer=Rc::into_raw(state.clone());
    let result=unsafe{CreateWindowExW(WINDOW_EX_STYLE::default(),w!("ZSDesk.UtilityPanel"),PCWSTR(wide(caption).as_ptr()),WS_OVERLAPPED|WS_CAPTION|WS_SYSMENU|WS_THICKFRAME|WS_CLIPCHILDREN|if kind==Kind::Search{WS_MAXIMIZEBOX|WS_MINIMIZEBOX}else{WINDOW_STYLE(0)},
        CW_USEDEFAULT,CW_USEDEFAULT,740,560,(!owner.0.is_null()).then_some(owner),None,Some(instance),Some(pointer.cast()))};
    let hwnd=match result{Ok(hwnd)=>hwnd,Err(error)=>{if !state.borrow().adopted{unsafe{drop(Rc::from_raw(pointer));}}return Err(error.into());}};
    struct Owner(HWND);impl Drop for Owner{fn drop(&mut self){if !self.0.0.is_null(){let _=unsafe{DestroyWindow(self.0)};}}}let mut owned=Owner(hwnd);
    let initialized={let mut state=unsafe{&*pointer}.borrow_mut();state.adopted=true;state.hwnd=hwnd;state.dpi=unsafe{GetDpiForWindow(hwnd)}.max(96);state.init()};
    if let Err(error)=initialized{return Err(error);}
    zsdesk_windows::theme::set_dark_frame(hwnd,dark);zsdesk_windows::theme::round_corners(hwnd,false);
    let mut cursor=POINT::default();let _=unsafe{GetCursorPos(&mut cursor)};let monitor=unsafe{MonitorFromPoint(cursor,MONITOR_DEFAULTTONEAREST)};
    let mut info=MONITORINFO{cbSize:std::mem::size_of::<MONITORINFO>()as u32,..Default::default()};unsafe{GetMonitorInfoW(monitor,&mut info)}.ok()?;
    let scale=|n:i32|n*unsafe{GetDpiForWindow(hwnd)}.max(96)as i32/96;
    let width=scale(match kind{Kind::About=>560,Kind::Shortcuts=>800,Kind::Search=>960}).min(info.rcWork.right-info.rcWork.left);
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
        self.offset=0;self.has_next=false;if let Some(reader)=&self.detail_reader{reader.reset(0);}report_list::clear(self.list);set_text(self.edit,"");unsafe{let _=EnableWindow(self.action,false);let _=ShowWindow(self.hwnd,SW_HIDE);}
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
                |if self.kind==Kind::Shortcuts{WS_TABSTOP|WS_VSCROLL|WINDOW_STYLE(windows::Win32::System::SystemServices::SS_NOTIFY.0)}else{WINDOW_STYLE(0)})?;
            let _=unsafe{SetWindowTheme(self.edit,if self.palette.mode==Mode::Dark{w!("DarkMode_Explorer")}else{w!("Explorer")},PCWSTR::null())};
        }
        if self.kind==Kind::Search{
            unsafe{InitCommonControlsEx(&INITCOMMONCONTROLSEX{dwSize:std::mem::size_of::<INITCOMMONCONTROLSEX>()as u32,dwICC:ICC_LISTVIEW_CLASSES}).ok()?;}
            self.list=self.child(WC_LISTVIEWW,"",11,WINDOW_STYLE(LVS_REPORT|LVS_SINGLESEL|LVS_SHOWSELALWAYS)|WS_TABSTOP)?;
            self.style_list();
            for (col,title) in HEADINGS.iter().enumerate(){report_list::heading(self.list,col,zsdesk_core::i18n::text(title),120,true);}
            self.child(w!("BUTTON"),zsdesk_core::i18n::text("上一页"),16,WS_TABSTOP)?;
            self.child(w!("BUTTON"),zsdesk_core::i18n::text("下一页"),17,WS_TABSTOP)?;
            self.refresh_headings();self.detail_reader=Some(details::Reader::new(self.hwnd)?);
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
    fn refresh_language(&mut self){
        use zsdesk_core::i18n::text as tr;
        set_text(self.hwnd,panel_title(self.kind));set_text(self.close,tr("关闭"));
        match self.kind{
            Kind::Shortcuts=>{
                set_text(self.action,tr("搜索快捷键设置"));
                if let Ok(body)=shortcut_body(&self.profile,crate::window::shortcut_help_text()){
                    self.info=content::shortcut_rows(&body);self.body=body;self.update_scroll();
                }
                self.status=tr("滚动查看全部快捷键 · Esc 关闭").into();
            }
            Kind::Search=>{
                set_text(self.action,tr("打开文件位置"));self.style_list();
                self.refresh_headings();
                for (id,label) in [(16,"上一页"),(17,"下一页")]{if let Ok(button)=unsafe{GetDlgItem(Some(self.hwnd),id)}{set_text(button,tr(label));}}
                let cue=wide(tr("搜索文件、文件夹和路径"));
                unsafe{SendMessageW(self.edit,EM_SETCUEBANNER,Some(WPARAM(1)),Some(LPARAM(cue.as_ptr() as isize)));}
                if text(self.edit).is_empty(){self.status=tr("输入关键词搜索文件和文件夹").into();}
                else if self.busy{self.status=tr("正在搜索…").into();}
                else if !self.search_error{self.status=zsdesk_core::tr_format!("{} 个结果 · Enter 打开 · Esc 关闭",self.paths.len());}
            }
            Kind::About=>{
                let action=match (&self.update_package,&self.update_release){
                    (Some((_,true,_)),_)=>"打开更新包",(Some(_),_)=>"安装更新",(None,Some(_))=>"下载更新",_=>"检查更新",
                };
                set_text(self.action,tr(action));
                for (id,label) in [(14,"项目主页"),(15,"启动时检查更新")]{
                    if let Ok(control)=unsafe{GetDlgItem(Some(self.hwnd),id)}{set_text(control,tr(label));}
                }
            }
        }
        unsafe{let _=RedrawWindow(Some(self.hwnd),None,None,RDW_INVALIDATE|RDW_ALLCHILDREN);}
    }
    fn layout(&mut self){let mut r=RECT::default();if unsafe{GetClientRect(self.hwnd,&mut r)}.is_err(){return;}
        let s=|n|self.s(n);let width=r.right.max(0);let height=r.bottom.max(0);
        if self.kind==Kind::Search{
            for (hwnd,x,y,w,h) in [(self.edit,s(39),s(15),width-s(60),s(23)),(self.list,s(8),s(52),width-s(16),height-s(108)),
                (self.action,s(12),height-s(38),s(105),s(28)),(self.close,width-s(72),height-s(38),s(60),s(28))]{
                let _=unsafe{SetWindowPos(hwnd,None,x,y,w.max(1),h.max(1),SWP_NOZORDER|SWP_NOACTIVATE)};
            }
            for (id,x) in [(16,218),(17,146)]{if let Ok(button)=unsafe{GetDlgItem(Some(self.hwnd),id)}{let _=unsafe{SetWindowPos(button,None,width-s(x),height-s(38),s(64),s(28),SWP_NOZORDER|SWP_NOACTIVATE)};}}
            let available=(width-s(36)).max(0);let size=s(89);let date=s(148);let names=(available-size-date).max(s(130));
            for (i,w) in [names*41/100,names-names*41/100,size,date].into_iter().enumerate(){report_list::width(self.list,i,w);}
            self.page_buttons();let _=unsafe{InvalidateRect(Some(self.hwnd),None,false)};return;
        }
        for (hwnd,x,y,w,h) in [(self.edit,s(if self.kind==Kind::Search{48}else{24}),s(if self.kind==Kind::Search{74}else{68}),width-s(if self.kind==Kind::Search{72}else{48}),if self.kind==Kind::Search{s(22)}else{height-s(160)}),
            (self.list,s(16),s(120),width-s(32),height-s(194)),(self.action,s(20),height-s(48),s(if self.kind==Kind::Shortcuts{176}else{148}),s(32)),(self.close,width-s(100),height-s(48),s(80),s(32))]{
            if !hwnd.0.is_null(){let _=unsafe{SetWindowPos(hwnd,None,x,y,w.max(1),h.max(1),SWP_NOZORDER|SWP_NOACTIVATE)};}}
        if let Ok(link)=unsafe{GetDlgItem(Some(self.hwnd),14)}{let _=unsafe{SetWindowPos(link,None,s(180),height-s(48),s(100),s(32),SWP_NOZORDER|SWP_NOACTIVATE)};}
        if let Ok(check)=unsafe{GetDlgItem(Some(self.hwnd),15)}{let _=unsafe{SetWindowPos(check,None,s(290),height-s(48),s(150),s(32),SWP_NOZORDER|SWP_NOACTIVATE)};}
        self.update_scroll();let _=unsafe{InvalidateRect(Some(self.edit),None,false)};let _=unsafe{InvalidateRect(Some(self.hwnd),None,false)};
    }
    fn search(&mut self){
        let query=text(self.edit);self.paths.clear();report_list::clear(self.list);self.has_next=false;self.page_buttons();
        if query.trim().is_empty(){if let Some(engine)=&self.engine{engine.cancel_query();}self.busy=false;self.search_error=false;self.status=zsdesk_core::i18n::text("输入关键词搜索文件和文件夹").into();return;}
        if self.engine.is_none(){match crate::storage::load(&self.profile).and_then(|session|FastSearch::for_profile(&self.profile,self.hwnd,RESULTS,SearchOptions::from_ui(&session.ui))){
            Ok(engine)=>self.engine=Some(engine),Err(error)=>{self.busy=false;self.search_error=true;self.status=zsdesk_core::tr_format!("搜索不可用：{error:#}" , error=error);return;}}}
        self.busy=true;self.search_error=false;self.generation=self.engine.as_ref().unwrap().start_page(&query,None,self.offset,PAGE_SIZE,self.sort);if let Some(rd)=&self.detail_reader{rd.reset(self.generation);}self.status=zsdesk_core::i18n::text("正在搜索…").into();
    }
    fn invalidate_query(&mut self){
        self.generation=0;if let Some(engine)=&self.engine{engine.cancel_query();}self.paths.clear();
        self.busy=!text(self.edit).trim().is_empty();self.search_error=false;
        let _=unsafe{EnableWindow(self.action,false)};
        report_list::clear(self.list);self.offset=0;self.has_next=false;self.page_buttons();if let Some(rd)=&self.detail_reader{rd.reset(0);}self.status=zsdesk_core::i18n::text("正在等待输入…").into();
    }
    fn dpi_changed(&mut self,dpi:u32,rect:RECT){
        self.scroll=self.scroll*dpi.max(96)as i32/self.dpi.max(96)as i32;self.dpi=dpi.max(96);let old=self.font;let old_title=self.title_font;
        let mut font=LOGFONTW{lfHeight:-self.s(16),..Default::default()};let family=wide(&zsdesk_windows::gfx::system_ui_font(None).0);font.lfFaceName[..family.len().min(32)].copy_from_slice(&family[..family.len().min(32)]);
        self.font=unsafe{CreateFontIndirectW(&font)};font.lfHeight=-self.s(23);font.lfWeight=600;self.title_font=unsafe{CreateFontIndirectW(&font)};
        for hwnd in [self.edit,self.list,self.action,self.close]{if !hwnd.0.is_null(){unsafe{SendMessageW(hwnd,WM_SETFONT,Some(WPARAM(self.font.0 as usize)),Some(LPARAM(1)));}}}
        if let Ok(link)=unsafe{GetDlgItem(Some(self.hwnd),14)}{unsafe{SendMessageW(link,WM_SETFONT,Some(WPARAM(self.font.0 as usize)),Some(LPARAM(1)));}}
        unsafe{DeleteObject(old.into());DeleteObject(old_title.into());for id in [16,17]{if let Ok(b)=GetDlgItem(Some(self.hwnd),id){SendMessageW(b,WM_SETFONT,Some(WPARAM(self.font.0 as usize)),Some(LPARAM(1)));}}
            SetWindowPos(self.hwnd,None,rect.left,rect.top,rect.right-rect.left,rect.bottom-rect.top,SWP_NOZORDER|SWP_NOACTIVATE).ok();}
        self.layout();
    }
    fn paint(&self,dc:HDC){let mut r=RECT::default();let _=unsafe{GetClientRect(self.hwnd,&mut r)};unsafe{FillRect(dc,&r,self.brush);}
        let title=match self.kind{Kind::Search=>zsdesk_core::i18n::text("快速搜索"),Kind::About=>zsdesk_core::i18n::text("关于 ZSDesk"),Kind::Shortcuts=>zsdesk_core::i18n::text("键盘快捷键")};
        if self.kind!=Kind::Search{draw_text(dc,title,RECT{left:self.s(22),top:self.s(16),right:r.right-self.s(20),bottom:self.s(54)},self.title_font,rgb(self.palette.text));}
        if self.kind==Kind::Search{
            let area=RECT{left:self.s(8),top:self.s(7),right:r.right-self.s(8),bottom:self.s(44)};
            content::rounded(dc,area,rgb(self.palette.input_focused),composite(self.palette.input_border,self.palette.layer),self.s(8));
            let pen=unsafe{CreatePen(PS_SOLID,self.s(1).max(1),composite(self.palette.text_secondary,self.palette.input_focused))};
            let old=unsafe{SelectObject(dc,pen.into())};let brush=unsafe{SelectObject(dc,GetStockObject(NULL_BRUSH))};
            unsafe{let _=Ellipse(dc,self.s(18),self.s(18),self.s(28),self.s(28));MoveToEx(dc,self.s(26),self.s(26),None);let _=LineTo(dc,self.s(32),self.s(32));SelectObject(dc,brush);SelectObject(dc,old);let _=DeleteObject(pen.into());}
            if unsafe{GetFocus()}==self.edit{let pen=unsafe{CreatePen(PS_SOLID,self.s(2).max(1),rgb(self.palette.accent))};let old=unsafe{SelectObject(dc,pen.into())};
                unsafe{MoveToEx(dc,area.left+self.s(6),area.bottom-self.s(2),None);let _=LineTo(dc,area.right-self.s(6),area.bottom-self.s(2));SelectObject(dc,old);let _=DeleteObject(pen.into());}}
        }
        draw_text(dc,&self.status,if self.kind==Kind::Search{RECT{left:self.s(125),top:r.bottom-self.s(40),right:r.right-self.s(224),bottom:r.bottom-self.s(8)}}else{RECT{left:self.s(22),top:r.bottom-self.s(77),right:r.right-self.s(20),bottom:r.bottom-self.s(52)}},self.font,
            if self.search_error{rgb(self.palette.danger)}else{composite(self.palette.text_secondary,self.palette.layer)});
    }
    fn results(&mut self){let Some(engine)=&self.engine else{return;};for event in engine.poll(){
        if event.generation!=self.generation{continue;}if event.reset{self.paths.clear();report_list::clear(self.list);if let Some(rd)=&self.detail_reader{rd.reset(self.generation);}}
        self.busy=!event.finished;self.search_error=event.error.is_some()&&event.paths.is_empty()&&self.paths.is_empty();
        for path in event.paths{if self.paths.len()>=PAGE_SIZE as usize{break;}if self.paths.contains(&path){continue;}
            let row=self.paths.len();
            report_list::cell(self.list,row,0,&path.file_name().unwrap_or(path.as_os_str()).to_string_lossy());
            report_list::cell(self.list,row,1,&path.parent().unwrap_or(&path).to_string_lossy());
            if let Some(rd)=&self.detail_reader{rd.submit(self.generation,row,path.clone());}self.paths.push(path);}
        self.has_next=event.truncated;
        let warning=event.volumes.iter().find_map(|volume|volume.warning.as_ref()).cloned();
        self.status=event.error.unwrap_or_else(||if event.finished{warning.map_or_else(||zsdesk_core::tr_format!("共 {total} 项 · 第 {page} 页",total=event.indexed,page=self.offset/PAGE_SIZE+1),|warning|zsdesk_core::tr_format!("{} 个结果 · {warning}" , self.paths.len(), warning=warning))}
            else if event.phase==crate::fast_search::IndexPhase::Snapshot{zsdesk_core::i18n::text("正在读取已有索引…").into()}
            else if event.scanning{if event.volumes.len()==1{zsdesk_core::tr_format!("补充 {} · 已处理 {} 项 · {} 个结果" , event.volumes[0].root.display(), event.scanned, self.paths.len())}
                else{zsdesk_core::tr_format!("{} 个结果 · 索引仍在更新" , self.paths.len())}}
            else{zsdesk_core::tr_format!("正在搜索 · {} 个结果" , self.paths.len())});
    }if !self.paths.is_empty()&&report_list::selected(self.list).is_none(){report_list::select_first(self.list);}self.page_buttons();
        let _=unsafe{EnableWindow(self.action,!self.paths.is_empty())};
    }
    fn style_list(&self){zsdesk_windows::theme::theme_control(self.list,self.palette.mode==Mode::Dark);
        report_list::style(self.list,rgb(self.palette.input_focused),rgb(self.palette.text));}
    fn refresh_headings(&self){
        for (col,title) in HEADINGS.iter().enumerate(){let arrow=match (col,self.sort){
            (0,SearchSort::NameAsc)|(2,SearchSort::SizeAsc)|(3,SearchSort::ModifiedAsc)=>" ↑",
            (0,SearchSort::NameDesc)|(2,SearchSort::SizeDesc)|(3,SearchSort::ModifiedDesc)=>" ↓",_=>""};
            report_list::heading(self.list,col,&format!("{}{arrow}",zsdesk_core::i18n::text(title)),0,false);}
    }
    fn page_buttons(&self){for (id,enabled) in [(16,self.offset>0),(17,self.has_next)]{if let Ok(b)=unsafe{GetDlgItem(Some(self.hwnd),id)}{let _=unsafe{EnableWindow(b,enabled)};}}}
    fn open_selected(&mut self,location:bool){let Some(path)=report_list::selected(self.list).and_then(|index|self.paths.get(index)).cloned()else{return;};
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
        if msg==details::READY{unsafe{SetTimer(Some(hwnd),DEFERRED_DETAILS,40,None);}return LRESULT(0);}
        if msg==RESULTS||msg==WM_TIMER&&wp.0==DEFERRED_RESULTS{unsafe{SetTimer(Some(hwnd),DEFERRED_RESULTS,50,None);}return LRESULT(0);}
        return unsafe{DefWindowProcW(hwnd,msg,wp,lp)};
    };
    match msg{
        crate::language::CHANGED=>state.refresh_language(),
        WM_SIZE=>state.layout(),WM_ERASEBKGND=>return LRESULT(1),
        WM_DPICHANGED if lp.0!=0=>state.dpi_changed((wp.0&0xffff)as u32,unsafe{*(lp.0 as*const RECT)}),
        WM_GETMINMAXINFO if lp.0!=0=>{let limits=unsafe{&mut*(lp.0 as*mut MINMAXINFO)};limits.ptMinTrackSize=POINT{x:state.s(if state.kind==Kind::About{560}else{600}),y:state.s(if state.kind==Kind::About{400}else{420})};},
        WM_MOUSEWHEEL if state.kind==Kind::Shortcuts=>state.scroll_info(0,((wp.0>>16)as i16).into()),
        WM_APPPLUS_SCROLL=>state.scroll_info(wp.0 as u32,lp.0 as i32),
        WM_CTLCOLOREDIT|WM_CTLCOLORSTATIC=>{let dc=HDC(wp.0 as*mut _);unsafe{SetTextColor(dc,rgb(state.palette.text));SetBkColor(dc,rgb(state.palette.input_focused));}return LRESULT(state.input_brush.0 as isize);},
        WM_CTLCOLORLISTBOX=>return LRESULT(state.input_brush.0 as isize),
        WM_PAINT=>{let mut ps=PAINTSTRUCT::default();let dc=unsafe{BeginPaint(hwnd,&mut ps)};state.paint(dc);unsafe{EndPaint(hwnd,&ps);}},
        WM_PRINTCLIENT=>{state.paint(HDC(wp.0 as*mut _));return LRESULT(0);},
        details::READY|WM_TIMER if msg==details::READY||wp.0==DEFERRED_DETAILS=>{
            let _=unsafe{KillTimer(Some(hwnd),DEFERRED_DETAILS)};
            if let Some(rd)=&state.detail_reader{for d in rd.poll(){
                if d.generation==state.generation && state.paths.get(d.row)==Some(&d.path){report_list::cell(state.list,d.row,2,&d.size);report_list::cell(state.list,d.row,3,&d.modified);}
            }}return LRESULT(0);
        }
        WM_NOTIFY if lp.0!=0 && state.kind==Kind::Search=>{
            let header=unsafe{&*(lp.0 as *const NMHDR)};
            if header.hwndFrom==state.list{
                if header.code==NM_DBLCLK && unsafe{(*(lp.0 as *const NMITEMACTIVATE)).iItem}>=0 {state.open_selected(false);}
                if header.code==LVN_COLUMNCLICK{let col=unsafe{(*(lp.0 as *const NMLISTVIEW)).iSubItem};
                    state.sort=match (col,state.sort){(0,SearchSort::NameAsc)=>SearchSort::NameDesc,(0,_)=>SearchSort::NameAsc,
                        (2,SearchSort::SizeDesc)=>SearchSort::SizeAsc,(2,_)=>SearchSort::SizeDesc,
                        (3,SearchSort::ModifiedDesc)=>SearchSort::ModifiedAsc,(3,_)=>SearchSort::ModifiedDesc,_=>return LRESULT(0)};
                    state.offset=0;state.refresh_headings();state.search();}
            }return LRESULT(0);
        }
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
        },
        WM_COMMAND=>{let id=wp.0&0xffff;let event=(wp.0>>16)as u32;
            if id==13{unsafe{PostMessageW(Some(hwnd),WM_CLOSE,WPARAM(0),LPARAM(0)).ok();}}
            else if id==12&&event==BN_CLICKED{match state.kind{Kind::Search=>state.open_selected(true),Kind::About=>state.updates(),Kind::Shortcuts=>{
                let profile=state.profile.clone();
                let mut rect=RECT::default();
                let anchor=unsafe{GetWindowRect(state.action,&mut rect)}.ok().and_then(|_|{
                    let mut point=POINT{x:rect.left,y:rect.top};
                    unsafe{ScreenToClient(hwnd,&mut point)}.as_bool().then(||zsdesk_ui::chrome::Rect::new(point.x,point.y,rect.right-rect.left,rect.bottom-rect.top))
                });
                drop(state);
                let mut status=None;
                crate::window::startup_settings::show(&profile,hwnd,anchor,|text|status=Some(text));
                if !unsafe{IsWindow(Some(hwnd))}.as_bool(){return LRESULT(0);}
                let mut panel=slot.borrow_mut();
                match shortcut_body(&profile,crate::window::shortcut_help_text()){
                    Ok(body)=>{panel.info=content::shortcut_rows(&body);panel.body=body;panel.update_scroll();}
                    Err(error)=>status=Some(error.to_string()),
                }
                if let Some(text)=status{panel.status=text;}
                let _=unsafe{InvalidateRect(Some(panel.edit),None,false)};
                let _=unsafe{InvalidateRect(Some(hwnd),None,false)};
                return LRESULT(0);
            }}}
            else if id==14&&state.kind==Kind::About{unsafe{ShellExecuteW(Some(hwnd),w!("open"),w!("https://github.com/qiu7824/zsdesk/releases"),None,None,SW_SHOWNORMAL);}}
            else if id==15&&state.kind==Kind::About{
                if let Ok(check)=unsafe{GetDlgItem(Some(hwnd),15)}{
                    let enabled=unsafe{SendMessageW(check,BM_GETCHECK,None,None)}.0!=0;
                    if let Err(error)=crate::update_check::set_automatic(&state.profile,enabled){state.status=zsdesk_core::tr_format!("无法保存更新设置：{error:#}" , error=error);unsafe{SendMessageW(check,BM_SETCHECK,Some(WPARAM(usize::from(!enabled))),None);}}
                }
            }
            else if id==10&&event==EN_CHANGE&&state.kind==Kind::Search{state.invalidate_query();unsafe{SetTimer(Some(hwnd),DEBOUNCE,200,None);}}
            else if state.kind==Kind::Search && event==BN_CLICKED && (id==16 || id==17){
                if id==16 && state.offset>0{state.offset=state.offset.saturating_sub(PAGE_SIZE);state.search();}
                else if id==17 && state.has_next{state.offset=state.offset.saturating_add(PAGE_SIZE);state.search();}
            }},
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
#[cfg(test)]#[path="utility_panel_tests.rs"]mod tests;
unsafe extern "system" fn control_proc(hwnd:HWND,msg:u32,wp:WPARAM,lp:LPARAM,id:usize,data:usize)->LRESULT{
    let parent=HWND(data as*mut _);
    if msg==WM_IME_STARTCOMPOSITION{let _=unsafe{SetPropW(hwnd,w!("ZSDesk.ImeActive"),Some(HANDLE(1usize as*mut _)))};}
    if msg==WM_IME_ENDCOMPOSITION{let _=unsafe{RemovePropW(hwnd,w!("ZSDesk.ImeActive"))};}
    if msg==WM_NCDESTROY{let _=unsafe{RemovePropW(hwnd,w!("ZSDesk.ImeActive"))};let _=unsafe{RemoveWindowSubclass(hwnd,Some(control_proc),id)};}
    let shortcut_page=unsafe{GetDlgItem(Some(parent),10)}.is_ok_and(|body|unsafe{GetWindowLongW(body,GWL_STYLE)}as u32&WS_TABSTOP.0!=0)
        &&unsafe{GetDlgItem(Some(parent),11)}.is_err();
    let info=shortcut_page&&unsafe{GetDlgCtrlID(hwnd)}==10;
    if info&&msg==WM_VSCROLL{let _=unsafe{SendMessageW(parent,WM_APPPLUS_SCROLL,Some(WPARAM(wp.0&0xffff)),None)};return LRESULT(0);}
    if shortcut_page&&msg==WM_MOUSEWHEEL{let delta=((wp.0>>16)as u16)as i16;let _=unsafe{SendMessageW(parent,WM_APPPLUS_SCROLL,None,Some(LPARAM(delta as isize)))};return LRESULT(0);}
    if !unsafe{GetPropW(hwnd,w!("ZSDesk.ImeActive"))}.0.is_null(){return unsafe{DefSubclassProc(hwnd,msg,wp,lp)};}
    if msg==WM_KEYDOWN{
        if wp.0==VK_ESCAPE.0 as usize{unsafe{PostMessageW(Some(parent),WM_CLOSE,WPARAM(0),LPARAM(0)).ok();}return LRESULT(0);}
        if wp.0==VK_RETURN.0 as usize&&matches!(unsafe{GetDlgCtrlID(hwnd)},12|13|14){let _=unsafe{PostMessageW(Some(parent),WM_COMMAND,WPARAM(GetDlgCtrlID(hwnd)as usize),LPARAM(hwnd.0 as isize))};return LRESULT(0);}
        if wp.0==VK_RETURN.0 as usize&&matches!(unsafe{GetDlgCtrlID(hwnd)},10|11)&&unsafe{GetDlgItem(Some(parent),11)}.is_ok(){unsafe{PostMessageW(Some(parent),WM_APPPLUS_OPEN,WPARAM(0),LPARAM(0)).ok();}return LRESULT(0);}
        if shortcut_page&&[VK_DOWN.0 as usize,VK_UP.0 as usize,VK_NEXT.0 as usize,VK_PRIOR.0 as usize,VK_HOME.0 as usize,VK_END.0 as usize].contains(&wp.0){
            let code=match wp.0 as u16{key if key==VK_UP.0=>0,key if key==VK_DOWN.0=>1,key if key==VK_PRIOR.0=>2,key if key==VK_NEXT.0=>3,key if key==VK_HOME.0=>6,_=>7};
            let _=unsafe{SendMessageW(parent,WM_APPPLUS_SCROLL,Some(WPARAM(code)),None)};return LRESULT(0);
        }
        if wp.0==VK_DOWN.0 as usize&&unsafe{GetDlgCtrlID(hwnd)}==10{if let Ok(list)=unsafe{GetDlgItem(Some(parent),11)}{let _=unsafe{SetFocus(Some(list))};return LRESULT(0);}}
        if wp.0==VK_TAB.0 as usize{if let Ok(next)=unsafe{GetNextDlgTabItem(parent,Some(hwnd),GetKeyState(VK_SHIFT.0 as i32)<0)}{let _=unsafe{SetFocus(Some(next))};}return LRESULT(0);}
    }
    unsafe{DefSubclassProc(hwnd,msg,wp,lp)}
}
