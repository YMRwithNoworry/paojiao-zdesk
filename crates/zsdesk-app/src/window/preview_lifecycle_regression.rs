use super::*;
use windows::core::{IUnknown,Ref};
use windows::Win32::{System::Ole::IObjectWithSite_Impl,
    UI::Shell::{IPreviewHandler_Impl,IPreviewHandlerVisuals_Impl,
        PropertiesSystem::IInitializeWithFile_Impl,SetWindowSubclass,RemoveWindowSubclass,DefSubclassProc}};

struct OwnedWindow(HWND);
impl Drop for OwnedWindow {
    fn drop(&mut self){if unsafe{IsWindow(Some(self.0))}.as_bool(){let _=unsafe{DestroyWindow(self.0)};}}
}
fn owner()->Result<OwnedWindow>{
    Ok(OwnedWindow(unsafe{CreateWindowExW(WS_EX_NOACTIVATE,windows::core::w!("STATIC"),
        windows::core::w!("Preview lifecycle test"),WS_POPUP,0,0,700,500,None,None,None,None)}?))
}

#[implement(IPreviewHandler,IPreviewHandlerVisuals,IInitializeWithFile,IObjectWithSite)]
struct ProbeHandler {
    events:Rc<RefCell<Vec<&'static str>>>,
    background:Cell<u32>,text:Cell<u32>,hwnd:Cell<HWND>,close_on_preview:bool,
}
#[allow(non_snake_case)]
impl IPreviewHandler_Impl for ProbeHandler_Impl {
    fn SetWindow(&self,hwnd:HWND,_rect:*const RECT)->windows::core::Result<()>{self.hwnd.set(hwnd);self.events.borrow_mut().push("window");Ok(())}
    fn SetRect(&self,_rect:*const RECT)->windows::core::Result<()>{Ok(())}
    fn DoPreview(&self)->windows::core::Result<()>{
        if self.background.get()!=0x202020||self.text.get()!=0xF2F2F2{
            return Err(Error::new(HRESULT(0x80004005u32 as i32),"handler painted before receiving the dark palette"));
        }
        self.events.borrow_mut().push("preview");
        if self.close_on_preview{unsafe{DestroyWindow(self.hwnd.get())}?;}
        Ok(())
    }
    fn Unload(&self)->windows::core::Result<()>{self.events.borrow_mut().push("unload");Ok(())}
    fn SetFocus(&self)->windows::core::Result<()>{Ok(())}
    fn QueryFocus(&self)->windows::core::Result<HWND>{Ok(self.hwnd.get())}
    fn TranslateAccelerator(&self,_message:*const MSG)->windows::core::Result<()>{Err(Error::from_hresult(HRESULT(1)))}
}
#[allow(non_snake_case)]
impl IPreviewHandlerVisuals_Impl for ProbeHandler_Impl {
    fn SetBackgroundColor(&self,color:COLORREF)->windows::core::Result<()>{self.background.set(color.0);self.events.borrow_mut().push("background");Ok(())}
    fn SetTextColor(&self,color:COLORREF)->windows::core::Result<()>{self.text.set(color.0);self.events.borrow_mut().push("text");Ok(())}
    fn SetFont(&self,_font:*const LOGFONTW)->windows::core::Result<()>{Ok(())}
}
#[allow(non_snake_case)]
impl IInitializeWithFile_Impl for ProbeHandler_Impl {
    fn Initialize(&self,_path:&PCWSTR,mode:u32)->windows::core::Result<()>{
        if mode!=STGM_READ.0{return Err(Error::from_hresult(HRESULT(0x80070057u32 as i32)));}
        self.events.borrow_mut().push("initialize");Ok(())
    }
}
#[allow(non_snake_case)]
impl IObjectWithSite_Impl for ProbeHandler_Impl {
    fn SetSite(&self,_site:Ref<IUnknown>)->windows::core::Result<()>{Ok(())}
    fn GetSite(&self,_iid:*const windows::core::GUID,result:*mut *mut core::ffi::c_void)->windows::core::Result<()>{
        if !result.is_null(){unsafe{*result=std::ptr::null_mut()};}
        Err(Error::from_hresult(HRESULT(0x80004002u32 as i32)))
    }
}

#[test]
#[ignore="Native hidden preview regression; run after implementation is complete"]
fn native_preview_sets_palette_before_first_frame_and_rejects_reentrant_close()->Result<()>{
    for close in [false,true]{
        let owner=owner()?;
        let mut surface=QuickPreview::open_impl(owner.0,Path::new(""),false,false,PreviewProviderPreference::System,true,false,None)?;
        let events=Rc::new(RefCell::new(Vec::new()));
        let handler:IPreviewHandler=ProbeHandler{events:events.clone(),background:Cell::new(0xFFFFFF),text:Cell::new(0),hwnd:Cell::new(HWND::default()),close_on_preview:close}.into();
        let result=initialize_preview(handler,surface.window(),Path::new(r"C:\preview-fixture.txt"),
            RECT{left:0,top:0,right:300,bottom:200},1,preview_colors(true,false,0,0));
        assert_eq!(result.is_err(),close);
        drop(result);
        assert_eq!(&*events.borrow(),&["initialize","background","text","window","preview","unload"]);
        assert_eq!(surface.is_open(),!close);
        surface.close();
    }
    Ok(())
}

unsafe extern "system" fn close_during_font(child:HWND,message:u32,wp:WPARAM,lp:LPARAM,id:usize,data:usize)->LRESULT{
    if message==WM_SETFONT{
        let _=unsafe{RemoveWindowSubclass(child,Some(close_during_font),id)};
        let _=unsafe{DestroyWindow(HWND(data as*mut _))};
        return LRESULT(0);
    }
    unsafe{DefSubclassProc(child,message,wp,lp)}
}

#[test]
#[ignore="Native hidden preview regression; run after implementation is complete"]
fn native_text_preview_is_themed_before_show_and_font_callback_can_close()->Result<()>{
    let owner=owner()?;
    let mut surface=QuickPreview::open_impl(owner.0,Path::new(""),false,false,PreviewProviderPreference::System,true,false,None)?;
    let text=surface.state.text_control("预览文本")?;let child=text.0;
    assert_eq!(unsafe{GetWindowLongPtrW(child,GWL_STYLE)}as u32&WS_VISIBLE.0,0,"text must stay hidden until installed in the themed host");
    *surface.state.content.borrow_mut()=Content::Text(text);
    surface.state.layout_content();
    assert_ne!(unsafe{GetWindowLongPtrW(child,GWL_STYLE)}as u32&WS_VISIBLE.0,0);
    let dc=unsafe{GetDC(Some(child))};
    let brush=unsafe{SendMessageW(surface.window(),WM_CTLCOLORSTATIC,Some(WPARAM(dc.0 as usize)),Some(LPARAM(child.0 as isize)))};
    let _=unsafe{ReleaseDC(Some(child),dc)};
    assert_eq!(brush.0,surface.state.brush.get().0 as isize);
    assert_eq!(surface.state.colors.get().background,current_preview_colors(true).background);
    unsafe{SetWindowSubclass(child,Some(close_during_font),997,surface.window().0 as usize)}.ok()?;
    surface.state.update_font();
    assert!(!surface.is_open(),"synchronous font callback should close without a RefCell panic");
    surface.close();Ok(())
}
