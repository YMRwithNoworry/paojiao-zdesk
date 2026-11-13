//! Color container gaps while preserving the native content renderer's buffering.
use std::{cell::Cell,rc::Rc};
use windows::{core::BOOL,Win32::{Foundation::{HWND,LPARAM,LRESULT,WPARAM,COLORREF,RECT,POINT},
    Graphics::Gdi::{CreateSolidBrush,DeleteObject,FillRect,HBRUSH,HDC,GetSysColorBrush,GetSysColor,COLOR_WINDOW,
        SaveDC,RestoreDC,ExcludeClipRect,MapWindowPoints},
    UI::{Controls::{LVM_SETBKCOLOR,LVM_SETTEXTBKCOLOR,LVM_SETEXTENDEDLISTVIEWSTYLE,LVS_EX_DOUBLEBUFFER},
        Shell::{IShellView,SetWindowSubclass,GetWindowSubclass,RemoveWindowSubclass,DefSubclassProc},
        WindowsAndMessaging::{EnumChildWindows,GetClassNameW,GetClientRect,GetWindowRect,GetWindow,GetWindowLongPtrW,
            SendMessageW,GWL_STYLE,GW_CHILD,GW_HWNDNEXT,WS_VISIBLE,WM_ERASEBKGND,WM_NCDESTROY,WM_PARENTNOTIFY,WM_CREATE}}}};

const SUBCLASS:usize=0x5a534247;
#[derive(Clone,Copy)]
pub(super) struct Colors{pub background:u32,pub high_contrast:bool}
#[derive(Clone,Copy,PartialEq,Eq)]
enum PaintOwner{Container,Native,ListView}
struct State{colors:Rc<Cell<Colors>>,brush:HBRUSH,last:Option<u32>,owner:PaintOwner}
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
    // Mask only the buffering bit: keep Shell-owned selection, checkboxes and
    // layout styles. Let the control erase within its own buffered paint.
    unsafe{
        SendMessageW(hwnd,LVM_SETEXTENDEDLISTVIEWSTYLE,Some(WPARAM(LVS_EX_DOUBLEBUFFER as usize)),Some(LPARAM(LVS_EX_DOUBLEBUFFER as isize)));
        SendMessageW(hwnd,LVM_SETBKCOLOR,None,Some(LPARAM(color as isize)));
        SendMessageW(hwnd,LVM_SETTEXTBKCOLOR,None,Some(LPARAM(color as isize)));
    }
}
fn install_one(hwnd:HWND,colors:Rc<Cell<Colors>>,owner:PaintOwner){
    if owner==PaintOwner::ListView{prepare_list_view(hwnd,colors.get());}
    let mut old=0usize;
    if unsafe{GetWindowSubclass(hwnd,Some(procedure),SUBCLASS,Some(&mut old))}.as_bool(){
        if old!=0 {unsafe{(*(old as*mut State)).colors=colors;}}return;
    }
    let pointer=Box::into_raw(Box::new(State{colors,brush:HBRUSH::default(),last:None,owner}));
    if !unsafe{SetWindowSubclass(hwnd,Some(procedure),SUBCLASS,pointer as usize)}.as_bool(){unsafe{drop(Box::from_raw(pointer));}}
}
unsafe extern "system" fn enumerate(hwnd:HWND,lp:LPARAM)->BOOL{
    if let Some(owner)=paint_owner(hwnd){install_one(hwnd,unsafe{&*(lp.0 as*const Rc<Cell<Colors>>)}.clone(),owner);}true.into()
}
fn install_children(hwnd:HWND,colors:&Rc<Cell<Colors>>){
    let _=unsafe{EnumChildWindows(Some(hwnd),Some(enumerate),LPARAM((colors as*const Rc<Cell<Colors>>)as isize))};
}
pub(super) fn install(view:&IShellView,colors:Rc<Cell<Colors>>){
    let Ok(hwnd)=(unsafe{view.GetWindow()})else{return;};
    if let Some(owner)=paint_owner(hwnd){install_one(hwnd,colors.clone(),owner);}
    install_children(hwnd,&colors);
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
        let colors=unsafe{&*(data as*const State)}.colors.clone();let child=HWND(lp.0 as*mut _);
        if let Some(owner)=paint_owner(child){install_one(child,colors.clone(),owner);}install_children(child,&colors);
    }
    unsafe{DefSubclassProc(hwnd,msg,wp,lp)}
}

#[cfg(test)]mod tests{
    use super::*;
    use windows::Win32::{Graphics::Gdi::{CreateCompatibleDC,CreateCompatibleBitmap,SelectObject,DeleteDC,GetPixel,GetDC,ReleaseDC},
        UI::WindowsAndMessaging::{CreateWindowExW,DestroyWindow,SendMessageW,WS_POPUP,WS_CHILD,WINDOW_EX_STYLE}};
    struct Window(HWND);
    impl Drop for Window{fn drop(&mut self){let _=unsafe{DestroyWindow(self.0)};}}
    fn owner()->Window{Window(unsafe{CreateWindowExW(WINDOW_EX_STYLE::default(),windows::core::w!("STATIC"),
        windows::core::w!("hidden paint fixture"),WS_POPUP,0,0,8,8,None,None,None,None)}.unwrap())}
    struct MemoryDc{dc:HDC,bitmap:windows::Win32::Graphics::Gdi::HBITMAP,previous:windows::Win32::Graphics::Gdi::HGDIOBJ}
    impl MemoryDc{
        fn new()->Self{
            let screen=unsafe{GetDC(None)};let dc=unsafe{CreateCompatibleDC(Some(screen))};
            let bitmap=unsafe{CreateCompatibleBitmap(screen,8,8)};unsafe{ReleaseDC(None,screen);}
            let previous=unsafe{SelectObject(dc,bitmap.into())};Self{dc,bitmap,previous}
        }
        fn fill(&self,color:u32){let brush=unsafe{CreateSolidBrush(COLORREF(color))};
            unsafe{FillRect(self.dc,&RECT{left:0,top:0,right:8,bottom:8},brush);let _=DeleteObject(brush.into());}}
    }
    impl Drop for MemoryDc{fn drop(&mut self){unsafe{SelectObject(self.dc,self.previous);let _=DeleteObject(self.bitmap.into());let _=DeleteDC(self.dc);}}}

    #[test]
    fn native_content_erase_keeps_the_control_renderer(){
        let window=owner();let canvas=MemoryDc::new();let sentinel=0x182736;
        canvas.fill(sentinel);
        let expected=unsafe{SendMessageW(window.0,WM_ERASEBKGND,Some(WPARAM(canvas.dc.0 as usize)),None)};
        let expected_pixel=unsafe{GetPixel(canvas.dc,3,3)};
        install_one(window.0,Rc::new(Cell::new(Colors{background:0x010203,high_contrast:false})),PaintOwner::Native);
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
        canvas.fill(sentinel);let colors=Rc::new(Cell::new(Colors{background:0x202020,high_contrast:false}));
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
        let colors=Rc::new(Cell::new(Colors{background:0x282828,high_contrast:false}));
        for color in [0x282828,0xf0f0f0]{
            colors.set(Colors{background:color,high_contrast:false});
            install_one(window.0,colors.clone(),paint_owner(window.0).unwrap());
            let styles=unsafe{SendMessageW(window.0,LVM_GETEXTENDEDLISTVIEWSTYLE,None,None)}.0 as u32;
            assert_eq!(styles&(LVS_EX_DOUBLEBUFFER|LVS_EX_FULLROWSELECT),LVS_EX_DOUBLEBUFFER|LVS_EX_FULLROWSELECT);
            assert_eq!(unsafe{SendMessageW(window.0,LVM_GETBKCOLOR,None,None)}.0 as u32,color);
        }
    }
    #[test]
    fn native_view_erase_uses_current_palette_without_changing_theme(){
        let hwnd=unsafe{CreateWindowExW(WINDOW_EX_STYLE::default(),windows::core::w!("STATIC"),windows::core::w!("erase test"),WS_POPUP,0,0,8,8,None,None,None,None)}.unwrap();
        let colors=Rc::new(Cell::new(Colors{background:0x202020,high_contrast:false}));install_one(hwnd,colors.clone(),PaintOwner::Container);
        let screen=unsafe{GetDC(None)};let dc=unsafe{CreateCompatibleDC(Some(screen))};let bitmap=unsafe{CreateCompatibleBitmap(screen,8,8)};
        unsafe{ReleaseDC(None,screen);}
        let previous=unsafe{SelectObject(dc,bitmap.into())};
        for value in [0x202020,0xeeeeee]{
            colors.set(Colors{background:value,high_contrast:false});
            assert_eq!(unsafe{SendMessageW(hwnd,WM_ERASEBKGND,Some(WPARAM(dc.0 as usize)),Some(LPARAM(0)))}.0,1);
            assert_eq!(unsafe{GetPixel(dc,2,2)}.0,value);
        }
        unsafe{SelectObject(dc,previous);let _=DeleteObject(bitmap.into());let _=DeleteDC(dc);let _=DestroyWindow(hwnd);}
    }
}
