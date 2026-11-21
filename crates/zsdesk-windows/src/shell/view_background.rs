//! Theme native content and container gaps while preserving control buffering.
use std::{cell::Cell,rc::Rc};
use windows::{core::BOOL,Win32::{Foundation::{HWND,LPARAM,LRESULT,WPARAM,COLORREF,RECT,POINT},
    Graphics::Gdi::{CreateSolidBrush,DeleteObject,FillRect,HBRUSH,HDC,GetSysColorBrush,GetSysColor,COLOR_WINDOW,
        SaveDC,RestoreDC,ExcludeClipRect,MapWindowPoints,COLOR_WINDOWTEXT,RedrawWindow,RDW_INVALIDATE,RDW_ERASE,RDW_UPDATENOW,RDW_ALLCHILDREN},
    UI::{Controls::{LVM_SETBKCOLOR,LVM_SETTEXTBKCOLOR,LVM_SETTEXTCOLOR,LVM_SETEXTENDEDLISTVIEWSTYLE,LVS_EX_DOUBLEBUFFER,LVS_EX_TWOCLICKACTIVATE},
        Shell::{IShellView,SetWindowSubclass,GetWindowSubclass,RemoveWindowSubclass,DefSubclassProc},
        WindowsAndMessaging::{EnumChildWindows,GetClassNameW,GetClientRect,GetWindowRect,GetWindow,GetWindowLongPtrW,
            SendMessageW,GWL_STYLE,GW_CHILD,GW_HWNDNEXT,WS_VISIBLE,WM_ERASEBKGND,WM_NCDESTROY,WM_PARENTNOTIFY,WM_CREATE,WM_THEMECHANGED,WM_SETREDRAW}}}};

const SUBCLASS:usize=0x5a534247;
#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub(super) struct Colors{pub background:u32,pub high_contrast:bool,pub dark:bool}
#[derive(Clone,Copy,PartialEq,Eq)]
enum PaintOwner{Container,Native,ListView}
#[derive(Clone)]
pub(super) struct ThemePermit{pending:Rc<Cell<bool>>,generation:Rc<Cell<u64>>,expected:u64}
impl ThemePermit{
    pub(super) fn new(pending:Rc<Cell<bool>>,generation:Rc<Cell<u64>>)->Self{let expected=generation.get();Self{pending,generation,expected}}
    fn current(&self)->bool{!self.pending.get()&&self.generation.get()==self.expected}
}
fn permitted(apply:bool,permit:Option<&ThemePermit>)->bool{apply&&permit.is_none_or(ThemePermit::current)}
struct State{colors:Rc<Cell<Colors>>,brush:HBRUSH,last:Option<u32>,owner:PaintOwner,theme:Option<(bool,bool)>,apply_theme:bool,permit:Option<ThemePermit>,redraw_suppressed:bool,stage_new_renderers:bool}
impl Drop for State{fn drop(&mut self){if !self.brush.0.is_null(){let _=unsafe{DeleteObject(self.brush.into())};}}}
fn paint_owner(hwnd:HWND)->Option<PaintOwner>{
    let mut name=[0u16;96];let count=unsafe{GetClassNameW(hwnd,&mut name)}.max(0)as usize;
    match String::from_utf16_lossy(&name[..count]).as_str(){
        "SHELLDLL_DefView"|"ExplorerBrowserControl"=>Some(PaintOwner::Container),
        "DirectUIHWND"=>Some(PaintOwner::Native),
        "SysListView32"=>Some(PaintOwner::ListView),
        _=>None,
    }
}
fn prepare_list_view(hwnd:HWND,colors:Colors){
    let color=if colors.high_contrast{unsafe{GetSysColor(COLOR_WINDOW)}}else{colors.background};
    // Keep Shell-owned selection and checkbox styles while enabling native
    // hover feedback without selecting the hovered item.
    unsafe{
        let styles=LVS_EX_DOUBLEBUFFER|LVS_EX_TWOCLICKACTIVATE;
        SendMessageW(hwnd,LVM_SETEXTENDEDLISTVIEWSTYLE,Some(WPARAM(styles as usize)),Some(LPARAM(styles as isize)));
        SendMessageW(hwnd,LVM_SETBKCOLOR,None,Some(LPARAM(color as isize)));
        SendMessageW(hwnd,LVM_SETTEXTBKCOLOR,None,Some(LPARAM(color as isize)));
        let text=if colors.high_contrast{GetSysColor(COLOR_WINDOWTEXT)}else if colors.dark{0xffffff}else{0};
        SendMessageW(hwnd,LVM_SETTEXTCOLOR,None,Some(LPARAM(text as isize)));
    }
}
#[cfg(test)]
fn install_one_phase(hwnd:HWND,colors:Rc<Cell<Colors>>,owner:PaintOwner,apply_theme:bool,permit:Option<ThemePermit>){
    let stage_new_renderers=!apply_theme&&permit.is_none();
    install_one_staged(hwnd,colors,owner,apply_theme,permit,stage_new_renderers);
}
fn install_one_staged(hwnd:HWND,colors:Rc<Cell<Colors>>,owner:PaintOwner,apply_theme:bool,permit:Option<ThemePermit>,stage_new_renderers:bool){
    let palette=colors.get();let theme=(palette.dark,palette.high_contrast);
    let apply_theme=permitted(apply_theme,permit.as_ref());
    // A ResultsFolder renderer can be made visible before Shell reports
    // navigation completion. Hold its first paint until the final palette is
    // installed, rather than letting the default light frame flash onscreen.
    let stage_renderer=stage_new_renderers&&!apply_theme&&permit.is_none()&&matches!(owner,PaintOwner::Native|PaintOwner::ListView);
    let mut old=0usize;
    let (changed,begin_redraw_hold)=if unsafe{GetWindowSubclass(hwnd,Some(procedure),SUBCLASS,Some(&mut old))}.as_bool()&&old!=0{
        let state=unsafe{&mut*(old as*mut State)};
        let changed=apply_theme&&state.theme!=Some(theme);
        let begin_redraw_hold=stage_renderer&&!state.redraw_suppressed;
        state.colors=colors;state.apply_theme=apply_theme;state.permit=permit.clone();state.stage_new_renderers=stage_new_renderers;
        if changed{state.theme=Some(theme);}
        if begin_redraw_hold{state.redraw_suppressed=true;}
        (changed,begin_redraw_hold)
    }else{
        let pointer=Box::into_raw(Box::new(State{colors,brush:HBRUSH::default(),last:None,owner,theme:apply_theme.then_some(theme),apply_theme,permit:permit.clone(),redraw_suppressed:stage_renderer,stage_new_renderers}));
        if !unsafe{SetWindowSubclass(hwnd,Some(procedure),SUBCLASS,pointer as usize)}.as_bool(){unsafe{drop(Box::from_raw(pointer));}return;}
        (apply_theme,stage_renderer)
    };
    if begin_redraw_hold{unsafe{SendMessageW(hwnd,WM_SETREDRAW,Some(WPARAM(0)),None);}}
    // ResultsFolder can create its renderer after the original browser theme
    // was applied. Theme that new control as well, without resetting view state.
    // DirectUI is the ItemsView renderer. Set its explicit Explorer theme when
    // it is attached: after a process-mode switch, the native renderer can keep
    // the previous scheme even when the new browser/container is already themed.
    if changed{
        if matches!(owner,PaintOwner::ListView|PaintOwner::Native){crate::theme::theme_control(hwnd,palette.dark);}
        else{
            crate::theme::allow_dark_for_window(hwnd,palette.dark);
            if permitted(apply_theme,permit.as_ref()){
                unsafe{SendMessageW(hwnd,WM_THEMECHANGED,None,None);}
                if permitted(apply_theme,permit.as_ref()){let _=unsafe{windows::Win32::Graphics::Gdi::InvalidateRect(Some(hwnd),None,false)};}
            }
        }
    }
    if permitted(apply_theme,permit.as_ref()){
        if owner==PaintOwner::ListView{prepare_list_view(hwnd,palette);}
        release_redraw_hold(hwnd);
    }
}
fn release_redraw_hold(hwnd:HWND){
    let mut data=0usize;
    if !unsafe{GetWindowSubclass(hwnd,Some(procedure),SUBCLASS,Some(&mut data))}.as_bool()||data==0{return;}
    let should_release={
        let state=unsafe{&mut*(data as*mut State)};
        if !state.redraw_suppressed{false}else{state.redraw_suppressed=false;true}
    };
    if !should_release{return;}
    unsafe{
        SendMessageW(hwnd,WM_SETREDRAW,Some(WPARAM(1)),None);
        let _=RedrawWindow(Some(hwnd),None,None,RDW_INVALIDATE|RDW_ERASE|RDW_UPDATENOW|RDW_ALLCHILDREN);
    }
}
unsafe extern "system" fn enumerate(hwnd:HWND,lp:LPARAM)->BOOL{
    let (colors,apply_theme,permit,stage_new_renderers)=unsafe{&*(lp.0 as*const (Rc<Cell<Colors>>,bool,Option<ThemePermit>,bool))};
    if let Some(owner)=paint_owner(hwnd){install_one_staged(hwnd,colors.clone(),owner,*apply_theme,permit.clone(),*stage_new_renderers);}true.into()
}
fn install_children_phase(hwnd:HWND,colors:&Rc<Cell<Colors>>,apply_theme:bool,permit:Option<ThemePermit>,stage_new_renderers:bool){
    let context=(colors.clone(),apply_theme,permit,stage_new_renderers);
    let _=unsafe{EnumChildWindows(Some(hwnd),Some(enumerate),LPARAM((&context as*const (Rc<Cell<Colors>>,bool,Option<ThemePermit>,bool))as isize))};
}
fn install_phase(view:&IShellView,colors:Rc<Cell<Colors>>,apply_theme:bool,permit:Option<ThemePermit>,stage_new_renderers:bool){
    let Ok(hwnd)=(unsafe{view.GetWindow()})else{return;};
    if let Some(owner)=paint_owner(hwnd){install_one_staged(hwnd,colors.clone(),owner,apply_theme,permit.clone(),stage_new_renderers);}
    install_children_phase(hwnd,&colors,apply_theme,permit,stage_new_renderers);
}
pub(super) fn install(view:&IShellView,colors:Rc<Cell<Colors>>,permit:ThemePermit){install_phase(view,colors,true,Some(permit),false);}
/// Refresh the current view during navigation without hiding the old renderer.
pub(super) fn install_pending(view:&IShellView,colors:Rc<Cell<Colors>>){install_phase(view,colors,false,None,false);}
/// Hide only a newly created view's native renderers until its final theme is ready.
pub(super) fn install_new_view_pending(view:&IShellView,colors:Rc<Cell<Colors>>){install_phase(view,colors,false,None,true);}

#[cfg(test)]fn install_one(hwnd:HWND,colors:Rc<Cell<Colors>>,owner:PaintOwner){install_one_phase(hwnd,colors,owner,true,None);}
#[cfg(test)]fn install_children(hwnd:HWND,colors:&Rc<Cell<Colors>>){install_children_phase(hwnd,colors,true,None,false);}

#[cfg(test)]
pub(super) fn installed_colors(hwnd:HWND)->Option<Colors>{
    let mut data=0usize;
    (unsafe{GetWindowSubclass(hwnd,Some(procedure),SUBCLASS,Some(&mut data))}.as_bool()&&data!=0)
        .then(||unsafe{(*(data as*const State)).colors.get()})
}
#[cfg(test)]fn configured_theme(hwnd:HWND)->Option<(bool,bool)>{
    let mut data=0usize;
    if !unsafe{GetWindowSubclass(hwnd,Some(procedure),SUBCLASS,Some(&mut data))}.as_bool()||data==0{return None;}
    unsafe{(*(data as*const State)).theme}
}
#[cfg(test)]fn redraw_suppressed(hwnd:HWND)->bool{
    let mut data=0usize;
    if !unsafe{GetWindowSubclass(hwnd,Some(procedure),SUBCLASS,Some(&mut data))}.as_bool()||data==0{return false;}
    unsafe{(*(data as*const State)).redraw_suppressed}
}
#[cfg(test)]pub(super) fn applied_theme(hwnd:HWND)->Option<(bool,bool)>{
    let mut data=0usize;
    if !unsafe{GetWindowSubclass(hwnd,Some(procedure),SUBCLASS,Some(&mut data))}.as_bool()||data==0{return None;}
    let state=unsafe{&*(data as*const State)};permitted(state.apply_theme,state.permit.as_ref()).then_some(state.theme).flatten()
}

fn fill_container(hwnd:HWND,dc:HDC,brush:HBRUSH)->bool{
    let saved=unsafe{SaveDC(dc)};if saved==0{return false;}
    // Some Shell container versions do not use WS_CLIPCHILDREN. Never erase
    // already-drawn file rows or an embedded native preview under their HWNDs.
    let mut child=unsafe{GetWindow(hwnd,GW_CHILD)}.ok();
    while let Some(window)=child{
        if unsafe{GetWindowLongPtrW(window,GWL_STYLE)}as u32 & WS_VISIBLE.0!=0{
            let mut bounds=RECT::default();
            if unsafe{GetWindowRect(window,&mut bounds)}.is_ok(){
                let mut points=[POINT{x:bounds.left,y:bounds.top},POINT{x:bounds.right,y:bounds.bottom}];
                unsafe{MapWindowPoints(None,Some(hwnd),&mut points);
                    ExcludeClipRect(dc,points[0].x.min(points[1].x),points[0].y.min(points[1].y),
                        points[0].x.max(points[1].x),points[0].y.max(points[1].y));}
            }
        }
        child=unsafe{GetWindow(window,GW_HWNDNEXT)}.ok();
    }
    let mut rect=RECT::default();
    let filled=unsafe{GetClientRect(hwnd,&mut rect)}.is_ok()&&unsafe{FillRect(dc,&rect,brush)}!=0;
    let _=unsafe{RestoreDC(dc,saved)};filled
}
unsafe extern "system" fn procedure(hwnd:HWND,msg:u32,wp:WPARAM,lp:LPARAM,id:usize,data:usize)->LRESULT{
    if msg==WM_NCDESTROY{
        let _=unsafe{RemoveWindowSubclass(hwnd,Some(procedure),id)};
        unsafe{drop(Box::from_raw(data as*mut State));}
        return unsafe{DefSubclassProc(hwnd,msg,wp,lp)};
    }
    if msg==WM_ERASEBKGND && wp.0!=0{
        let state=unsafe{&mut*(data as*mut State)};let colors=state.colors.get();
        // DirectUI and ListView own both row painting and scroll buffering.
        // Filling their client DC here creates a visible blank intermediate
        // frame before their WM_PAINT, defeating that native buffering.
        if state.owner!=PaintOwner::Container{return unsafe{DefSubclassProc(hwnd,msg,wp,lp)};}
        let brush=if colors.high_contrast{unsafe{GetSysColorBrush(COLOR_WINDOW)}}else{
            if state.last!=Some(colors.background){
                let next=unsafe{CreateSolidBrush(COLORREF(colors.background))};
                if !next.0.is_null(){let old=std::mem::replace(&mut state.brush,next);state.last=Some(colors.background);
                    if !old.0.is_null(){let _=unsafe{DeleteObject(old.into())};}}
            }state.brush
        };
        if !brush.0.is_null()&&fill_container(hwnd,HDC(wp.0 as*mut _),brush){return LRESULT(1);}
    }
    if msg==WM_PARENTNOTIFY && (wp.0&0xffff)as u32==WM_CREATE{
        let (colors,apply_theme,permit,stage_new_renderers)={let state=unsafe{&*(data as*const State)};(state.colors.clone(),state.apply_theme,state.permit.clone(),state.stage_new_renderers)};let child=HWND(lp.0 as*mut _);
        if let Some(owner)=paint_owner(child){install_one_staged(child,colors.clone(),owner,apply_theme,permit.clone(),stage_new_renderers);}install_children_phase(child,&colors,apply_theme,permit,stage_new_renderers);
    }
    if msg==WM_THEMECHANGED{
        let (owner,colors,apply_theme,permit)={let state=unsafe{&*(data as*const State)};(state.owner,state.colors.clone(),state.apply_theme,state.permit.clone())};
        let result=unsafe{DefSubclassProc(hwnd,msg,wp,lp)};
        if permitted(apply_theme,permit.as_ref())&&owner==PaintOwner::ListView{prepare_list_view(hwnd,colors.get());}return result;
    }
    unsafe{DefSubclassProc(hwnd,msg,wp,lp)}
}

#[cfg(test)]mod tests{
    use super::*;
    use windows::Win32::{Graphics::Gdi::{CreateCompatibleDC,CreateCompatibleBitmap,SelectObject,DeleteDC,GetPixel,GetDC,ReleaseDC},
        UI::WindowsAndMessaging::{CreateWindowExW,DestroyWindow,SendMessageW,WS_POPUP,WS_CHILD,WINDOW_EX_STYLE}};
    #[link(name="user32")]unsafe extern "system"{fn PrintWindow(hwnd:HWND,dc:HDC,flags:u32)->BOOL;}
    struct Window(HWND);
    impl Drop for Window{fn drop(&mut self){let _=unsafe{DestroyWindow(self.0)};}}
    fn owner()->Window{Window(unsafe{CreateWindowExW(WINDOW_EX_STYLE::default(),windows::core::w!("STATIC"),
        windows::core::w!("hidden paint fixture"),WS_POPUP,0,0,8,8,None,None,None,None)}.unwrap())}
    struct MemoryDc{dc:HDC,bitmap:windows::Win32::Graphics::Gdi::HBITMAP,previous:windows::Win32::Graphics::Gdi::HGDIOBJ,width:i32,height:i32}
    impl MemoryDc{
        fn new()->Self{Self::sized(8,8)}
        fn sized(width:i32,height:i32)->Self{
            let screen=unsafe{GetDC(None)};let dc=unsafe{CreateCompatibleDC(Some(screen))};
            let bitmap=unsafe{CreateCompatibleBitmap(screen,width,height)};unsafe{ReleaseDC(None,screen);}
            let previous=unsafe{SelectObject(dc,bitmap.into())};Self{dc,bitmap,previous,width,height}
        }
        fn fill(&self,color:u32){let brush=unsafe{CreateSolidBrush(COLORREF(color))};
            unsafe{FillRect(self.dc,&RECT{left:0,top:0,right:self.width,bottom:self.height},brush);let _=DeleteObject(brush.into());}}
    }
    impl Drop for MemoryDc{fn drop(&mut self){unsafe{SelectObject(self.dc,self.previous);let _=DeleteObject(self.bitmap.into());let _=DeleteDC(self.dc);}}}

    #[test]
    fn native_content_erase_keeps_the_control_renderer(){
        let window=owner();let canvas=MemoryDc::new();let sentinel=0x182736;
        canvas.fill(sentinel);
        let expected=unsafe{SendMessageW(window.0,WM_ERASEBKGND,Some(WPARAM(canvas.dc.0 as usize)),None)};
        let expected_pixel=unsafe{GetPixel(canvas.dc,3,3)};
        install_one(window.0,Rc::new(Cell::new(Colors{background:0x010203,high_contrast:false,dark:false})),PaintOwner::Native);
        canvas.fill(sentinel);
        assert_eq!(unsafe{SendMessageW(window.0,WM_ERASEBKGND,Some(WPARAM(canvas.dc.0 as usize)),None)},expected);
        assert_eq!(unsafe{GetPixel(canvas.dc,3,3)},expected_pixel,"application filled a native renderer's scroll surface");
    }

    #[test]
    fn container_erase_preserves_child_pixels_and_dc_clip(){
        let window=owner();let canvas=MemoryDc::new();let sentinel=0x123456;
        // A hidden parent and a child with WS_VISIBLE exercise clipping without
        // showing a window or relying on the system's visible-window DC clip.
        let _child=Window(unsafe{CreateWindowExW(WINDOW_EX_STYLE::default(),windows::core::w!("STATIC"),None,
            WS_CHILD|WS_VISIBLE,2,2,4,4,Some(window.0),None,None,None)}.unwrap());
        canvas.fill(sentinel);let colors=Rc::new(Cell::new(Colors{background:0x202020,high_contrast:false,dark:true}));
        install_one(window.0,colors,PaintOwner::Container);
        assert_eq!(unsafe{SendMessageW(window.0,WM_ERASEBKGND,Some(WPARAM(canvas.dc.0 as usize)),None)}.0,1);
        assert_eq!(unsafe{GetPixel(canvas.dc,0,0)}.0,0x202020);
        assert_eq!(unsafe{GetPixel(canvas.dc,3,3)}.0,sentinel,"parent erase blanked a child content window");
        canvas.fill(0x556677);
        assert_eq!(unsafe{GetPixel(canvas.dc,3,3)}.0,0x556677,"erase leaked its temporary DC clipping region");
    }

    #[test]
    fn list_view_palette_uses_native_buffering_without_resetting_styles(){
        use windows::Win32::UI::Controls::{InitCommonControlsEx,INITCOMMONCONTROLSEX,ICC_LISTVIEW_CLASSES,
            LVM_GETBKCOLOR,LVM_GETEXTENDEDLISTVIEWSTYLE,LVS_EX_FULLROWSELECT,LVS_REPORT};
        assert!(unsafe{InitCommonControlsEx(&INITCOMMONCONTROLSEX{dwSize:std::mem::size_of::<INITCOMMONCONTROLSEX>()as u32,dwICC:ICC_LISTVIEW_CLASSES})}.as_bool());
        let window=Window(unsafe{CreateWindowExW(WINDOW_EX_STYLE::default(),windows::core::w!("SysListView32"),None,
            WS_POPUP|windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(LVS_REPORT),0,0,8,8,None,None,None,None)}.unwrap());
        unsafe{SendMessageW(window.0,LVM_SETEXTENDEDLISTVIEWSTYLE,Some(WPARAM(LVS_EX_FULLROWSELECT as usize)),Some(LPARAM(LVS_EX_FULLROWSELECT as isize)));}
        let colors=Rc::new(Cell::new(Colors{background:0x282828,high_contrast:false,dark:true}));
        for color in [0x282828,0xf0f0f0]{
            colors.set(Colors{background:color,high_contrast:false,dark:color==0x282828});
            install_one(window.0,colors.clone(),paint_owner(window.0).unwrap());
            let styles=unsafe{SendMessageW(window.0,LVM_GETEXTENDEDLISTVIEWSTYLE,None,None)}.0 as u32;
            assert_eq!(styles&(LVS_EX_DOUBLEBUFFER|LVS_EX_TWOCLICKACTIVATE|LVS_EX_FULLROWSELECT),
                LVS_EX_DOUBLEBUFFER|LVS_EX_TWOCLICKACTIVATE|LVS_EX_FULLROWSELECT);
            assert_eq!(unsafe{SendMessageW(window.0,LVM_GETBKCOLOR,None,None)}.0 as u32,color);
        }
    }
    #[test]
    fn native_view_erase_uses_current_palette_without_changing_theme(){
        let hwnd=unsafe{CreateWindowExW(WINDOW_EX_STYLE::default(),windows::core::w!("STATIC"),windows::core::w!("erase test"),WS_POPUP,0,0,8,8,None,None,None,None)}.unwrap();
        let colors=Rc::new(Cell::new(Colors{background:0x202020,high_contrast:false,dark:true}));install_one(hwnd,colors.clone(),PaintOwner::Container);
        let screen=unsafe{GetDC(None)};let dc=unsafe{CreateCompatibleDC(Some(screen))};let bitmap=unsafe{CreateCompatibleBitmap(screen,8,8)};
        unsafe{ReleaseDC(None,screen);}
        let previous=unsafe{SelectObject(dc,bitmap.into())};
        for value in [0x202020,0xeeeeee]{
            colors.set(Colors{background:value,high_contrast:false,dark:value==0x202020});
            assert_eq!(unsafe{SendMessageW(hwnd,WM_ERASEBKGND,Some(WPARAM(dc.0 as usize)),Some(LPARAM(0)))}.0,1);
            assert_eq!(unsafe{GetPixel(dc,2,2)}.0,value);
        }
        unsafe{SelectObject(dc,previous);let _=DeleteObject(bitmap.into());let _=DeleteDC(dc);let _=DestroyWindow(hwnd);}
    }

    #[test]
    fn late_classic_results_renderer_paints_palette_and_survives_theme_reset(){
        super::super::native_theme_test_context::private_desktop("classic",||{
            use windows::Win32::UI::{Controls::{LVM_GETBKCOLOR,LVM_GETTEXTCOLOR,LVM_GETTEXTBKCOLOR,LVS_REPORT},
                WindowsAndMessaging::{ShowWindow,SW_SHOWNOACTIVATE,WS_EX_TOOLWINDOW,WS_EX_NOACTIVATE,PW_RENDERFULLCONTENT,WM_PRINT,WM_PRINTCLIENT,WM_PAINT,PRF_CLIENT,PRF_ERASEBKGND,PRF_CHILDREN}};
            let controls=super::super::native_theme_test_context::CommonControls::activate(None)?;
            let parent=Window(unsafe{CreateWindowExW(WS_EX_TOOLWINDOW|WS_EX_NOACTIVATE,windows::core::w!("STATIC"),None,WS_POPUP,40,40,240,120,None,None,None,None)}?);
            let native_parent=Window(unsafe{CreateWindowExW(WS_EX_TOOLWINDOW|WS_EX_NOACTIVATE,windows::core::w!("STATIC"),None,WS_POPUP,300,40,240,120,None,None,None,None)}?);
            let colors=Rc::new(Cell::new(Colors{background:0x191919,high_contrast:false,dark:true}));install_one(parent.0,colors.clone(),PaintOwner::Container);
            let make_list=|parent:HWND|unsafe{CreateWindowExW(WINDOW_EX_STYLE::default(),windows::core::w!("SysListView32"),None,
                WS_CHILD|WS_VISIBLE|windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(LVS_REPORT),0,0,240,120,Some(parent),None,None,None)};
            let list=Window(make_list(parent.0)?);let native=Window(make_list(native_parent.0)?);
            unsafe{let _=ShowWindow(parent.0,SW_SHOWNOACTIVATE);let _=ShowWindow(native_parent.0,SW_SHOWNOACTIVATE);}
            assert!(installed_colors(native.0).is_none(),"negative control received an application paint hook");
            let canvas=MemoryDc::sized(240,120);
            let capture=|window:HWND,method:u32|{canvas.fill(0xff00ff);unsafe{match method{
                0=>{let _=PrintWindow(window,canvas.dc,1|PW_RENDERFULLCONTENT);},
                1=>{SendMessageW(window,WM_PRINT,Some(WPARAM(canvas.dc.0 as usize)),Some(LPARAM((PRF_CLIENT|PRF_ERASEBKGND|PRF_CHILDREN)as isize)));},
                2=>{SendMessageW(window,WM_PRINTCLIENT,Some(WPARAM(canvas.dc.0 as usize)),Some(LPARAM((PRF_CLIENT|PRF_ERASEBKGND)as isize)));},
                3=>{SendMessageW(window,WM_ERASEBKGND,Some(WPARAM(canvas.dc.0 as usize)),None);},
                _=>{SendMessageW(window,WM_PAINT,Some(WPARAM(canvas.dc.0 as usize)),None);},
            }}unsafe{GetPixel(canvas.dc,120,90)}.0};
            for (dark,color,text) in [(true,0x191919,0xffffff),(false,0xffffff,0),(true,0x242424,0xffffff)]{
                colors.set(Colors{background:color,high_contrast:false,dark});install_children(parent.0,&colors);
                assert_eq!(installed_colors(list.0),Some(colors.get()),"late child missed the current palette");
                unsafe{SendMessageW(list.0,LVM_SETBKCOLOR,None,Some(LPARAM(0xff00ff)));SendMessageW(list.0,LVM_SETTEXTBKCOLOR,None,Some(LPARAM(0xff00ff)));SendMessageW(list.0,LVM_SETTEXTCOLOR,None,Some(LPARAM(0xff00ff)));SendMessageW(list.0,WM_THEMECHANGED,None,None);}
                assert_eq!(unsafe{SendMessageW(list.0,LVM_GETBKCOLOR,None,None)}.0 as u32,color);
                assert_eq!(unsafe{SendMessageW(list.0,LVM_GETTEXTBKCOLOR,None,None)}.0 as u32,color);
                assert_eq!(unsafe{SendMessageW(list.0,LVM_GETTEXTCOLOR,None,None)}.0 as u32,text);
                // Establish the same requested palette directly on an unhooked
                // system control, so a failed capture is not blamed on the hook.
                crate::theme::theme_control(native.0,dark);
                unsafe{SendMessageW(native.0,LVM_SETEXTENDEDLISTVIEWSTYLE,Some(WPARAM(LVS_EX_DOUBLEBUFFER as usize)),Some(LPARAM(LVS_EX_DOUBLEBUFFER as isize)));
                    SendMessageW(native.0,LVM_SETBKCOLOR,None,Some(LPARAM(color as isize)));SendMessageW(native.0,LVM_SETTEXTBKCOLOR,None,Some(LPARAM(color as isize)));SendMessageW(native.0,LVM_SETTEXTCOLOR,None,Some(LPARAM(text as isize)));}
                let samples=(0..5).map(|method|capture(native.0,method)).collect::<Vec<_>>();
                // WM_PRINT is the control's synchronous raster path. The
                // PrintWindow/DWM snapshot can be black or stale for this
                // buffered child, so never select a method from one warm frame.
                let method=1usize;
                assert_eq!(samples[method],color,"unhooked native WM_PRINT did not paint the requested background: {samples:?}");
                assert_eq!(capture(list.0,method as u32),color,"application-hooked WM_PRINT differs from native raster");
                println!("{}",serde_json::json!({"classic_background":color,"capture_method":method,"native_negative_control":samples,"common_controls_version":controls.version()}));
            }
            Ok(())
        }).unwrap();
    }
    #[test]
    fn pending_navigation_holds_new_renderer_until_final_theme(){
        let window=owner();
        let colors=Rc::new(Cell::new(Colors{background:0x191919,high_contrast:false,dark:true}));
        install_one_phase(window.0,colors.clone(),PaintOwner::Native,true,None);
        assert_eq!(configured_theme(window.0),Some((true,false)));
        colors.set(Colors{background:0xfafafa,high_contrast:false,dark:false});
        install_one_staged(window.0,colors.clone(),PaintOwner::Native,false,None,true);
        assert_eq!(configured_theme(window.0),Some((true,false)),"pending navigation must not retheme an existing renderer");
        assert!(applied_theme(window.0).is_none(),"pending initialization must not mark navigation complete");
        assert!(redraw_suppressed(window.0),"new view should not paint using the previous theme");
        install_one_phase(window.0,colors.clone(),PaintOwner::Native,true,None);
        assert_eq!(configured_theme(window.0),Some((false,false)));
        assert!(!redraw_suppressed(window.0),"redraw should resume only after the final palette is installed");
    }

    #[test]
    fn superseded_navigation_cannot_retheme_a_classic_renderer(){
        use windows::Win32::UI::Controls::{InitCommonControlsEx,INITCOMMONCONTROLSEX,ICC_LISTVIEW_CLASSES,LVM_GETBKCOLOR,LVS_REPORT};
        assert!(unsafe{InitCommonControlsEx(&INITCOMMONCONTROLSEX{dwSize:std::mem::size_of::<INITCOMMONCONTROLSEX>()as u32,dwICC:ICC_LISTVIEW_CLASSES})}.as_bool());
        let window=Window(unsafe{CreateWindowExW(WINDOW_EX_STYLE::default(),windows::core::w!("SysListView32"),None,
            WS_POPUP|windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(LVS_REPORT),0,0,32,32,None,None,None,None)}.unwrap());
        let colors=Rc::new(Cell::new(Colors{background:0x191919,high_contrast:false,dark:true}));
        let pending=Rc::new(Cell::new(false));let generation=Rc::new(Cell::new(1u64));let old=ThemePermit::new(pending.clone(),generation.clone());
        install_one_phase(window.0,colors.clone(),PaintOwner::ListView,true,Some(old.clone()));
        assert_eq!(unsafe{SendMessageW(window.0,LVM_GETBKCOLOR,None,None)}.0 as u32,0x191919);
        generation.set(2);pending.set(true);colors.set(Colors{background:0xfafafa,high_contrast:false,dark:false});
        install_one_phase(window.0,colors.clone(),PaintOwner::ListView,true,Some(old));
        assert!(applied_theme(window.0).is_none());
        assert_eq!(unsafe{SendMessageW(window.0,LVM_GETBKCOLOR,None,None)}.0 as u32,0x191919,"stale completion changed native colors");
        pending.set(false);install_one_phase(window.0,colors,PaintOwner::ListView,true,Some(ThemePermit::new(pending,generation)));
        assert_eq!(applied_theme(window.0),Some((false,false)));
        assert_eq!(unsafe{SendMessageW(window.0,LVM_GETBKCOLOR,None,None)}.0 as u32,0xfafafa);
    }
}
