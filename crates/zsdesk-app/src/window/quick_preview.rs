//! Modeless, read-only file preview with bounded local fallbacks.

use std::{cell::{Cell, RefCell}, fs::{self, File}, io::Read, mem::size_of,
    path::{Path, PathBuf}, ptr, rc::Rc};
use anyhow::{bail, Context, Result};
use windows::{core::{implement, Error, Interface, PCWSTR, PWSTR, HRESULT}, Win32::{
    Foundation::{COLORREF, ERROR_CLASS_ALREADY_EXISTS, GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM},
    Globalization::{MultiByteToWideChar, CP_ACP, MB_ERR_INVALID_CHARS},
    Graphics::Gdi::*,
    System::{Com::{CLSIDFromString, CoCreateInstance, IStream, CLSCTX_INPROC_SERVER,
        STGM_READ, STGM_SHARE_DENY_NONE}, LibraryLoader::GetModuleHandleW, Ole::{IObjectWithSite, IOleWindow, IOleWindow_Impl}},
    UI::{Controls::{EM_SETLIMITTEXT, EM_SETREADONLY}, HiDpi::{GetDpiForWindow, SystemParametersInfoForDpi,
        DPI_AWARENESS_CONTEXT, GetThreadDpiAwarenessContext, GetWindowDpiAwarenessContext, SetThreadDpiAwarenessContext},
        Input::KeyboardAndMouse::{VK_ESCAPE, VK_SPACE},
        Shell::{AssocQueryStringW, ASSOCF_NOTRUNCATE, ASSOCSTR_SHELLEXTENSION, IPreviewHandler,
            IPreviewHandlerFrame, IPreviewHandlerFrame_Impl, IPreviewHandlerVisuals, IInitializeWithItem, PREVIEWHANDLERFRAMEINFO, SHCreateStreamOnFileEx,
            PropertiesSystem::{IInitializeWithFile, IInitializeWithStream}}, WindowsAndMessaging::*},
}};
use zsdesk_core::model::explorer::{PreviewProviderPreference, ShellLocation};

const MAX_HANDLER_BYTES: u64 = 128 * 1024 * 1024;
const MAX_TEXT_BYTES: usize = 256 * 1024;
const MAX_IMAGE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_BITMAP_WIDTH: u32 = 1600;
const MAX_BITMAP_HEIGHT: u32 = 1200;
const CLASS_NAME: PCWSTR = windows::core::w!("ZSDesk.QuickPreview");
const RECOLOR_IMAGE:u32=WM_APP+55;
#[path="image_decode.rs"]
mod image_decode;
use image_decode::ImagePreview;

/// Renderer window on its host's STA; production uses a disposable helper process.
pub(super) struct QuickPreview { state: Rc<PreviewState> }

impl QuickPreview {
    pub(super) fn create_worker(owner:HWND,provider:PreviewProviderPreference,dark:bool,hover:bool,bounds:Option<RECT>)->Result<Self>{
        let preview=Self::open_impl(owner,Path::new(""),true,true,provider,dark,hover,bounds)?;
        *preview.state.notice.borrow_mut()="正在加载预览…".into();preview.state.invalidate();Ok(preview)
    }
    pub(super) fn window(&self)->HWND{self.state.hwnd.get()}
    pub(super) fn status(&self)->(String,String){
        let kind=match &*self.state.content.borrow(){Content::Empty=>"information",Content::Handler(preview)=>if preview.rendered.is_some(){"wps"}else{"system"},Content::Text(_)=>"text",Content::Image(_)=>"image"};
        (kind.into(),self.state.notice.borrow().clone())
    }
    pub(super) fn open(owner: HWND, path: &Path) -> Result<Self> {
        Self::open_with_options(owner, path, PreviewProviderPreference::System, zsdesk_windows::theme::system_dark_mode(), false)
    }

    pub(super) fn open_with_options(owner: HWND, path: &Path, provider: PreviewProviderPreference, dark: bool, hover: bool) -> Result<Self> {
        Self::open_impl(owner, path, true, true, provider, dark, hover,None)
    }

    fn open_impl(owner: HWND, path: &Path, show: bool, allow_system_handler: bool,
        provider: PreviewProviderPreference, dark: bool, hover: bool, embedded:Option<RECT>) -> Result<Self> {
        let _dpi = PreviewDpiScope::for_window(owner);
        let instance = HINSTANCE(unsafe { GetModuleHandleW(None)? }.0);
        let class = WNDCLASSW { lpfnWndProc: Some(window_proc), hInstance: instance,
            hCursor: unsafe { LoadCursorW(None, IDC_ARROW)? }, lpszClassName: CLASS_NAME, ..Default::default() };
        if unsafe { RegisterClassW(&class) } == 0 && unsafe { GetLastError() } != ERROR_CLASS_ALREADY_EXISTS {
            return Err(Error::from_thread().into());
        }
        let colors=current_preview_colors(dark);
        let state = Rc::new(PreviewState { hwnd: Cell::new(HWND::default()), path: RefCell::new(PathBuf::new()),
            summary: RefCell::new(String::new()), notice: RefCell::new(String::new()), content: RefCell::new(Content::Empty),
            font: Cell::new(HFONT::default()), brush:Cell::new(unsafe { CreateSolidBrush(COLORREF(colors.background)) }),
            colors:Cell::new(colors),requested_dark:Cell::new(dark),refreshing_theme:Cell::new(false),laying_out:Cell::new(false),allow_system_handler, provider, hover,embedded:embedded.is_some() });
        let bounds = embedded.unwrap_or_else(||initial_bounds(owner));
        let hwnd = unsafe { CreateWindowExW(if embedded.is_some(){WINDOW_EX_STYLE::default()}else{WS_EX_TOOLWINDOW | if hover { WS_EX_NOACTIVATE } else { WINDOW_EX_STYLE::default() }}, CLASS_NAME, windows::core::w!("快速预览"),
            if embedded.is_some(){WS_CHILD|WS_CLIPCHILDREN}else{WS_CAPTION | WS_SYSMENU | WS_THICKFRAME | WS_CLIPCHILDREN},
            bounds.left, bounds.top, bounds.right - bounds.left, bounds.bottom - bounds.top,
            Some(owner), None, Some(instance), Some((&state as *const Rc<PreviewState>).cast()))? };
        state.hwnd.set(hwnd);
        state.refresh_theme();
        let mut preview = Self { state };
        // Keeps arrow-key selection in Explorer available while the preview is open.
        if show {
            let _ = unsafe { ShowWindow(hwnd, SW_SHOWNOACTIVATE) };
            preview.state.paint_pending();
        }
        if !path.as_os_str().is_empty(){preview.update(path)?;}
        Ok(preview)
    }

    pub(super) fn is_open(&self) -> bool {
        let hwnd = self.state.hwnd.get();
        !hwnd.0.is_null() && unsafe { IsWindow(Some(hwnd)) }.as_bool()
    }

    pub(super) fn is_hover(&self) -> bool { self.state.hover }

    pub(super) fn contains_point(&self, point: windows::Win32::Foundation::POINT) -> bool {
        let mut bounds = RECT::default();
        self.is_open() && unsafe { GetWindowRect(self.state.hwnd.get(), &mut bounds) }.is_ok()
            && point.x >= bounds.left && point.x < bounds.right && point.y >= bounds.top && point.y < bounds.bottom
    }

    pub(super) fn owns_window(&self, hwnd: HWND) -> bool {
        let preview = self.state.hwnd.get();
        !preview.0.is_null() && (hwnd == preview || unsafe { IsChild(preview, hwnd) }.as_bool())
    }

    pub(super) fn set_dark(&self,dark:bool){if self.state.requested_dark.replace(dark)!=dark{self.state.refresh_theme();}}
    pub(super) fn update(&mut self, path: &Path) -> Result<()> {
        let _dpi=PreviewDpiScope::for_window(self.state.hwnd.get());
        if path.as_os_str().is_empty(){self.state.release_content();self.state.path.borrow_mut().clear();self.state.summary.borrow_mut().clear();*self.state.notice.borrow_mut()="选择文件以显示预览".into();self.state.invalidate();return Ok(());}
        if !self.is_open() || self.state.path.borrow().as_path() == path { return Ok(()); }
        self.state.release_content();
        if !self.is_open() { return Ok(()); }
        *self.state.path.borrow_mut() = path.to_path_buf();
        *self.state.notice.borrow_mut()="正在加载预览…".into();
        // Paint the themed empty surface before a decoder/COM handler pumps its
        // own messages. The old content must not leave an unpainted white frame.
        self.state.paint_pending();
        let title = format!("{} — 快速预览", path.file_name().unwrap_or(path.as_os_str()).to_string_lossy());
        let _ = unsafe { SetWindowTextW(self.state.hwnd.get(), PCWSTR(wide(&title).as_ptr())) };
        let metadata = match fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) => {
                self.state.summary.borrow_mut().clear();
                *self.state.notice.borrow_mut() = format!("无法读取此文件。\r\n{error}");
                self.state.invalidate();
                return Ok(());
            }
        };
        *self.state.summary.borrow_mut() = if metadata.is_dir() { "文件夹".into() }
            else { format!("{} · {}", file_type(path), zsdesk_ui::chrome::format_size(metadata.len())) };
        if metadata.is_dir() {
            *self.state.notice.borrow_mut() = "请选择文件以显示内容预览。".into();
            self.state.invalidate();
            return Ok(());
        }
        let hwnd = self.state.hwnd.get();
        let rect = self.state.content_rect();
        let mut handler_error = None;
        // WIC images use the bounded local pipeline before third-party handlers.
        // This keeps large photographs in the disposable preview worker and
        // avoids handlers decoding a full-resolution bitmap unnecessarily.
        let content = if is_image_path(path) && metadata.len() <= MAX_IMAGE_BYTES {
            let bounds=((rect.right-rect.left).max(1)as u32,(rect.bottom-rect.top).max(1)as u32);
            match image_decode::decode(path,self.state.colors.get().background,bounds,||self.state.hwnd.get()!=hwnd||image_request_cancelled(path)) {
                Ok(image)=>Some(Content::Image(image)),
                Err(error)=>{handler_error=Some(format!("图片预览失败：{error:#}"));None}
            }
        } else {None};
        let content = content.or_else(||if self.state.allow_system_handler && !is_image_path(path) && metadata.len() <= MAX_HANDLER_BYTES
            && self.is_open() && !image_request_cancelled(path) {
            match system_preview_with_provider(hwnd, path, rect, self.state.provider,self.state.colors.get()) {
                Ok(preview) => Some(Content::Handler(preview)),
                Err(error) => { handler_error = Some(format!("{error:#}")); None }
            }
        } else { None });
        let content = content.or_else(|| {
            if !super::wps_preview::supported(path)||metadata.len()>MAX_HANDLER_BYTES||!self.is_open()||image_request_cancelled(path){return None;}
            let workspace=std::env::var_os("ZSDESK_PREVIEW_WORKSPACE").map(PathBuf::from)?;
            match super::wps_preview::render(path,&workspace).and_then(|rendered|{
                let mut handler=system_preview_with_provider(hwnd,&rendered.pdf,rect,PreviewProviderPreference::System,self.state.colors.get())?;handler.rendered=Some(rendered);Ok(handler)
            }){
                Ok(handler)=>Some(Content::Handler(handler)),
                Err(error)=>{handler_error=Some(format!("WPS 文档预览失败：{error:#}"));None}
            }
        });
        let content = content.or_else(|| {
            if self.state.hwnd.get() != hwnd || !unsafe { IsWindow(Some(hwnd)) }.as_bool()||image_request_cancelled(path) { return None; }
            if is_text_path(path) {
                read_text(path).ok().and_then(|text| self.state.text_control(&text).ok()).map(Content::Text)
            } else { None }
        });
        // A handler may dispatch WM_CLOSE or a newer request during Initialize /
        // DoPreview. Do not attach its result to a closed or superseded surface.
        if !self.is_open()||self.state.hwnd.get()!=hwnd||self.state.path.borrow().as_path()!=path||image_request_cancelled(path){
            drop(content);return Ok(());
        }
        match content {
            Some(content) => { self.state.notice.borrow_mut().clear();*self.state.content.borrow_mut() = content; }
            None => {
                *self.state.notice.borrow_mut() = if metadata.len() > MAX_HANDLER_BYTES {
                    "文件较大，快速预览仅显示文件信息。".into()
                } else { handler_error.unwrap_or_else(|| "此文件暂无可用的内容预览。".into()) };
            }
        }
        self.state.refresh_theme();
        self.state.layout_content();
        self.state.invalidate();
        Ok(())
    }

    /// Called before the Explorer accelerator table for messages owned by this preview.
    pub(super) fn pre_translate(&mut self, message: &MSG) -> bool {
        if !self.owns_window(message.hwnd) { return false; }
        if !self.state.embedded && closes_preview(message) { self.close(); return true; }
        let _dpi = PreviewDpiScope::for_window(self.state.hwnd.get());
        let handler = self.state.handler();
        handler.is_some_and(|handler| unsafe {
            (Interface::vtable(&handler).TranslateAccelerator)(Interface::as_raw(&handler), message) == HRESULT(0)
        })
    }

    pub(super) fn close(&mut self) {
        let hwnd = self.state.hwnd.get();
        if !hwnd.0.is_null() && unsafe { IsWindow(Some(hwnd)) }.as_bool() { let _ = unsafe { DestroyWindow(hwnd) }; }
        self.state.hwnd.set(HWND::default());
        self.state.release_content();
    }

    #[cfg(test)]
    #[allow(dead_code)] // Invoked by the isolated native-host probe.
    pub(super) fn open_hidden(owner: HWND, path: &Path) -> Result<Self> { Self::open_impl(owner, path, false, true, PreviewProviderPreference::System, false, false,None) }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(super) fn open_hidden_fallback(owner: HWND, path: &Path) -> Result<Self> { Self::open_impl(owner, path, false, false, PreviewProviderPreference::System, false, false,None) }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(super) fn snapshot(&self) -> PreviewSnapshot {
        let (kind, bytes, child) = match &*self.state.content.borrow() {
            Content::Empty => ("information", 0, None),
            Content::Handler(_) => ("system", 0, None),
            Content::Text(text) => ("text", 0, Some(text.0)),
            Content::Image(image) => ("image", image.pixels.len(), None),
        };
        PreviewSnapshot { hwnd: self.state.hwnd.get(), kind, image_bytes: bytes, text_child: child,
            notice: self.state.notice.borrow().clone(), summary: self.state.summary.borrow().clone() }
    }
}

#[cfg(test)]
#[derive(Debug)]
#[allow(dead_code)]
pub(super) struct PreviewSnapshot {
    pub hwnd: HWND, pub kind: &'static str, pub image_bytes: usize, pub text_child: Option<HWND>,
    pub notice: String, pub summary: String,
}

impl Drop for QuickPreview { fn drop(&mut self) { self.close(); } }

struct PreviewState {
    hwnd: Cell<HWND>, path: RefCell<PathBuf>, summary: RefCell<String>, notice: RefCell<String>,
    content: RefCell<Content>, font: Cell<HFONT>, brush: Cell<HBRUSH>, colors:Cell<PreviewColors>,requested_dark:Cell<bool>,refreshing_theme:Cell<bool>,laying_out:Cell<bool>,
    allow_system_handler: bool,
    provider: PreviewProviderPreference,
    hover: bool,
    embedded: bool,
}

#[derive(Clone,Copy,Debug,PartialEq,Eq)]
struct PreviewColors{background:u32,text:u32,dark:bool}
fn preview_colors(dark:bool,high_contrast:bool,window:u32,text:u32)->PreviewColors{
    if high_contrast{PreviewColors{background:window,text,dark:false}}
    else if dark{PreviewColors{background:0x202020,text:0xF2F2F2,dark:true}}
    else{PreviewColors{background:0xFFFFFF,text:0x202020,dark:false}}
}
fn current_preview_colors(dark:bool)->PreviewColors{
    use windows::Win32::UI::Accessibility::{HIGHCONTRASTW,HCF_HIGHCONTRASTON};
    let mut contrast=HIGHCONTRASTW{cbSize:size_of::<HIGHCONTRASTW>()as u32,..Default::default()};
    let high_contrast=unsafe{SystemParametersInfoW(SPI_GETHIGHCONTRAST,contrast.cbSize,Some((&mut contrast as*mut HIGHCONTRASTW).cast()),SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0))}.is_ok()
        &&contrast.dwFlags.0&HCF_HIGHCONTRASTON.0!=0;
    preview_colors(dark,high_contrast,unsafe{GetSysColor(COLOR_WINDOW)},unsafe{GetSysColor(COLOR_WINDOWTEXT)})
}
fn dpi_suggestion(rect:&RECT)->Option<(i32,i32,i32,i32)>{
    let width=rect.right.checked_sub(rect.left)?;let height=rect.bottom.checked_sub(rect.top)?;
    (width>0&&height>0).then_some((rect.left,rect.top,width,height))
}

/// A third-party handler must not leave the application's UI thread virtualized.
struct PreviewDpiScope(DPI_AWARENESS_CONTEXT);
impl PreviewDpiScope {
    fn for_window(window: HWND) -> Self {
        let previous = unsafe { GetThreadDpiAwarenessContext() };
        let desired = unsafe { GetWindowDpiAwarenessContext(window) };
        if !desired.0.is_null() { unsafe { SetThreadDpiAwarenessContext(desired); } }
        Self(previous)
    }
}
impl Drop for PreviewDpiScope {
    fn drop(&mut self) { if !self.0.0.is_null() { unsafe { SetThreadDpiAwarenessContext(self.0); } } }
}

enum Content { Empty, Handler(SystemPreview), Text(TextPreview), Image(ImagePreview) }
struct TextPreview(HWND);
impl Drop for TextPreview {
    fn drop(&mut self) { if unsafe { IsWindow(Some(self.0)) }.as_bool() { let _ = unsafe { DestroyWindow(self.0) }; } }
}
struct SystemPreview { handler: IPreviewHandler, _stream: Option<IStream>, _site: IPreviewHandlerFrame, owner: HWND, rendered:Option<super::wps_preview::RenderedDocument> }
impl Drop for SystemPreview {
    fn drop(&mut self) {
        let _dpi = PreviewDpiScope::for_window(self.owner);
        let _ = unsafe { self.handler.Unload() };
        if let Ok(site) = self.handler.cast::<IObjectWithSite>() { let _ = unsafe { site.SetSite(None) }; }
    }
}

impl PreviewState {
    fn s(&self, value: i32) -> i32 { zsdesk_ui::chrome::scale(value, unsafe { GetDpiForWindow(self.hwnd.get()) }.max(96)) }
    fn release_content(&self) { let old = self.content.replace(Content::Empty); drop(old); }
    fn handler(&self) -> Option<IPreviewHandler> {
        match &*self.content.borrow() { Content::Handler(preview) => Some(preview.handler.clone()), _ => None }
    }
    fn content_rect(&self) -> RECT {
        let mut rect = RECT::default();
        let _ = unsafe { GetClientRect(self.hwnd.get(), &mut rect) };
        let margin = self.s(12);
        RECT { left: margin, top: self.s(54), right: (rect.right - margin).max(margin), bottom: (rect.bottom - self.s(32)).max(self.s(54)) }
    }
    fn invalidate(&self) {
        let hwnd = self.hwnd.get();
        if !hwnd.0.is_null() { let _ = unsafe { InvalidateRect(Some(hwnd), None, false) }; }
    }
    fn paint_pending(&self){
        self.invalidate();
        if !self.hwnd.get().0.is_null(){let _=unsafe{UpdateWindow(self.hwnd.get())};}
    }
    fn refresh_theme(&self){
        if self.refreshing_theme.replace(true){return;}
        struct Refresh<'a>(&'a Cell<bool>);impl Drop for Refresh<'_>{fn drop(&mut self){self.0.set(false);}}let _refresh=Refresh(&self.refreshing_theme);
        let mut colors=current_preview_colors(self.requested_dark.get());let previous=self.colors.get();
        if colors.background!=previous.background||self.brush.get().0.is_null(){
            let next=unsafe{CreateSolidBrush(COLORREF(colors.background))};
            if !next.0.is_null(){let old=self.brush.replace(next);if !old.0.is_null(){let _=unsafe{DeleteObject(old.into())};}}else{colors=previous;}
        }
        self.colors.set(colors);
        zsdesk_windows::theme::set_dark_frame(self.hwnd.get(),colors.dark);
        let text=match &*self.content.borrow(){Content::Text(text)=>Some(text.0),_=>None};
        if let Some(text)=text{zsdesk_windows::theme::theme_control(text,colors.dark);let _=unsafe{InvalidateRect(Some(text),None,true)};}
        if let Some(visuals)=self.handler().and_then(|handler|handler.cast::<IPreviewHandlerVisuals>().ok()){
            let _=unsafe{visuals.SetBackgroundColor(COLORREF(colors.background))};let _=unsafe{visuals.SetTextColor(COLORREF(colors.text))};
        }
        self.update_font();self.layout_content();self.invalidate();
        if colors.background!=previous.background&&matches!(&*self.content.borrow(),Content::Image(_)){
            // Alpha was composited against the previous background. Reuse the
            // bounded decoder after returning from this theme notification.
            let _=unsafe{PostMessageW(Some(self.hwnd.get()),RECOLOR_IMAGE,WPARAM(0),LPARAM(0))};
        }
    }
    fn recolor_image(&self){
        if !matches!(&*self.content.borrow(),Content::Image(_)){return;}
        let hwnd=self.hwnd.get();let path=self.path.borrow().clone();let rect=self.content_rect();
        let bounds=((rect.right-rect.left).max(1)as u32,(rect.bottom-rect.top).max(1)as u32);
        if let Ok(image)=image_decode::decode(&path,self.colors.get().background,bounds,||self.hwnd.get()!=hwnd||image_request_cancelled(&path)){
            let current=self.hwnd.get()==hwnd&&*self.path.borrow()==path&&matches!(&*self.content.borrow(),Content::Image(_));
            if current{
                *self.content.borrow_mut()=Content::Image(image);self.invalidate();
            }
        }
    }
    fn layout_content(&self) {
        if self.hwnd.get().0.is_null()||self.laying_out.replace(true){return;}
        struct Layout<'a>(&'a Cell<bool>);impl Drop for Layout<'_>{fn drop(&mut self){self.0.set(false);}}let _layout=Layout(&self.laying_out);
        let rect = self.content_rect();
        let _dpi = PreviewDpiScope::for_window(self.hwnd.get());
        let handler = self.handler();
        let text = match &*self.content.borrow() { Content::Text(text) => Some(text.0), _ => None };
        if let Some(handler) = handler { let _ = unsafe { handler.SetRect(&rect) }; }
        if let Some(hwnd) = text {
            let _ = unsafe { MoveWindow(hwnd, rect.left, rect.top, rect.right - rect.left, rect.bottom - rect.top, true) };
            let _ = unsafe { ShowWindow(hwnd, SW_SHOWNOACTIVATE) };
        }
    }
    fn update_font(&self) {
        let mut metrics = NONCLIENTMETRICSW { cbSize: size_of::<NONCLIENTMETRICSW>() as u32, ..Default::default() };
        if unsafe { SystemParametersInfoForDpi(SPI_GETNONCLIENTMETRICS.0, metrics.cbSize,
            Some((&mut metrics as *mut NONCLIENTMETRICSW).cast()), 0, GetDpiForWindow(self.hwnd.get()).max(96)) }.is_ok() {
            let next = unsafe { CreateFontIndirectW(&metrics.lfMessageFont) };
            if !next.0.is_null() {
                let previous = self.font.replace(next);
                let text=match &*self.content.borrow(){Content::Text(text)=>Some(text.0),_=>None};
                // WM_SETFONT can synchronously dispatch color/destruction
                // callbacks; retain only the HWND across that call.
                if let Some(text)=text{unsafe { SendMessageW(text, WM_SETFONT, Some(WPARAM(next.0 as usize)), Some(LPARAM(1))); }}
                if !previous.0.is_null() { let _ = unsafe { DeleteObject(previous.into()) }; }
            }
            if let Some(visuals)=self.handler().and_then(|handler|handler.cast::<IPreviewHandlerVisuals>().ok()){
                let _=unsafe{visuals.SetFont(&metrics.lfMessageFont)};
            }
        }
    }
    fn text_control(&self, text: &str) -> Result<TextPreview> {
        let rect = self.content_rect();
        let text = wide(text);
        let child = unsafe { CreateWindowExW(WINDOW_EX_STYLE::default(), windows::core::w!("EDIT"), PCWSTR(text.as_ptr()),
            WS_CHILD | WS_VSCROLL | WS_HSCROLL | WINDOW_STYLE((ES_MULTILINE | ES_READONLY | ES_AUTOVSCROLL | ES_AUTOHSCROLL) as u32),
            rect.left, rect.top, rect.right - rect.left, rect.bottom - rect.top, Some(self.hwnd.get()), None, None, None)? };
        zsdesk_windows::theme::theme_control(child, self.colors.get().dark);
        unsafe {
            SendMessageW(child, EM_SETREADONLY, Some(WPARAM(1)), None);
            SendMessageW(child, EM_SETLIMITTEXT, Some(WPARAM(MAX_TEXT_BYTES * 2)), None);
            SendMessageW(child, WM_SETFONT, Some(WPARAM(self.font.get().0 as usize)), Some(LPARAM(1)));
        }
        Ok(TextPreview(child))
    }
    fn paint(&self) {
        let hwnd = self.hwnd.get();
        let mut paint = PAINTSTRUCT::default();
        let dc = unsafe { BeginPaint(hwnd, &mut paint) };
        let mut client = RECT::default();
        let _ = unsafe { GetClientRect(hwnd, &mut client) };
        unsafe { FillRect(dc, &client, self.brush.get()); SetBkMode(dc, TRANSPARENT); SetTextColor(dc, COLORREF(self.colors.get().text)); }
        let old_font = if !self.font.get().0.is_null() { Some(unsafe { SelectObject(dc, self.font.get().into()) }) } else { None };
        let mut header = RECT { left: self.s(12), top: self.s(10), right: client.right - self.s(12), bottom: self.s(42) };
        draw_text(dc, &self.summary.borrow(), &mut header, DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX);
        let mut footer = RECT { left: self.s(12), top: (client.bottom - self.s(28)).max(0), right: client.right - self.s(12), bottom: client.bottom };
        draw_text(dc, if self.embedded{"Alt+P 关闭预览窗格"}else{"空格 / Esc 关闭"}, &mut footer, DT_SINGLELINE | DT_VCENTER | DT_RIGHT | DT_NOPREFIX);
        let content = self.content_rect();
        match &*self.content.borrow() {
            Content::Image(image) => {
                let (w, h) = fit_dimensions(image.width, image.height, (content.right - content.left).max(1) as u32, (content.bottom - content.top).max(1) as u32);
                let x = content.left + ((content.right - content.left) - w as i32) / 2;
                let y = content.top + ((content.bottom - content.top) - h as i32) / 2;
                let bitmap = BITMAPINFO { bmiHeader: BITMAPINFOHEADER { biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: image.width as i32, biHeight: -(image.height as i32), biPlanes: 1, biBitCount: 32,
                    biCompression: BI_RGB.0, ..Default::default() }, ..Default::default() };
                unsafe { SetStretchBltMode(dc, HALFTONE); StretchDIBits(dc, x, y, w as i32, h as i32, 0, 0,
                    image.width as i32, image.height as i32, Some(image.pixels.as_ptr().cast()), &bitmap, DIB_RGB_COLORS, SRCCOPY); }
            }
            Content::Empty => { let mut area = content; draw_text(dc, &self.notice.borrow(), &mut area, DT_WORDBREAK | DT_NOPREFIX); }
            _ => {}
        }
        if let Some(font) = old_font { unsafe { SelectObject(dc, font); } }
        let _ = unsafe { EndPaint(hwnd, &paint) };
    }
}

impl Drop for PreviewState {
    fn drop(&mut self) {
        self.release_content();
        if !self.font.get().0.is_null() { let _ = unsafe { DeleteObject(self.font.get().into()) }; }
        if !self.brush.get().0.is_null() { let _ = unsafe { DeleteObject(self.brush.get().into()) }; }
    }
}

unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if message == WM_NCCREATE {
        let create = unsafe { &*(lp.0 as *const CREATESTRUCTW) };
        let state = unsafe { &*(create.lpCreateParams as *const Rc<PreviewState>) }.clone();
        state.hwnd.set(hwnd);
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, Rc::into_raw(state) as isize); }
        return LRESULT(1);
    }
    let raw = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const PreviewState;
    if raw.is_null() { return unsafe { DefWindowProcW(hwnd, message, wp, lp) }; }
    unsafe { Rc::increment_strong_count(raw); }
    let state = unsafe { Rc::from_raw(raw) };
    match message {
        WM_MOUSEACTIVATE if state.hover => LRESULT(MA_NOACTIVATE as isize),
        WM_SETCURSOR if wp.0==hwnd.0 as usize && lp.0 as u16==HTCLIENT as u16=>{
            if let Ok(cursor)=unsafe{LoadCursorW(None,IDC_ARROW)}{unsafe{SetCursor(Some(cursor));}LRESULT(1)}
            else{unsafe{DefWindowProcW(hwnd,message,wp,lp)}}
        }
        WM_ERASEBKGND => {
            let mut rect=RECT::default();let _=unsafe{GetClientRect(hwnd,&mut rect)};
            unsafe{FillRect(HDC(wp.0 as*mut _),&rect,state.brush.get());}LRESULT(1)
        }
        WM_PAINT => { state.paint(); LRESULT(0) }
        WM_GETMINMAXINFO => {
            if lp.0 != 0 {
                let info = unsafe { &mut *(lp.0 as *mut MINMAXINFO) };
                info.ptMinTrackSize.x = state.s(320);
                info.ptMinTrackSize.y = state.s(220);
            }
            LRESULT(0)
        }
        WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT => {
            let text = matches!(&*state.content.borrow(), Content::Text(text) if text.0.0 as isize == lp.0);
            if text {
                let dc = HDC(wp.0 as *mut _);
                let colors=state.colors.get();unsafe { SetTextColor(dc, COLORREF(colors.text)); SetBkColor(dc, COLORREF(colors.background)); }
                LRESULT(state.brush.get().0 as isize)
            } else { unsafe { DefWindowProcW(hwnd, message, wp, lp) } }
        }
        WM_SIZE => { state.layout_content(); state.invalidate(); LRESULT(0) }
        WM_DPICHANGED => {
            if lp.0!=0&&!state.embedded{
                if let Some((x,y,width,height))=dpi_suggestion(unsafe{&*(lp.0 as*const RECT)}){
                    let _=unsafe{SetWindowPos(hwnd,None,x,y,width,height,SWP_NOACTIVATE|SWP_NOZORDER)};
                }
            }
            state.update_font(); state.layout_content();state.invalidate(); LRESULT(0)
        }
        WM_DPICHANGED_AFTERPARENT=>{state.update_font();state.layout_content();state.invalidate();LRESULT(0)}
        WM_THEMECHANGED|WM_SYSCOLORCHANGE|WM_DWMCOMPOSITIONCHANGED=>{state.refresh_theme();LRESULT(0)}
        WM_SETTINGCHANGE if wp.0==0||wp.0==SPI_SETHIGHCONTRAST.0 as usize||wp.0==SPI_SETNONCLIENTMETRICS.0 as usize=>{state.refresh_theme();LRESULT(0)}
        RECOLOR_IMAGE=>{state.recolor_image();LRESULT(0)}
        WM_KEYDOWN if !state.embedded && (wp.0 == VK_ESCAPE.0 as usize || wp.0 == VK_SPACE.0 as usize) => {
            let _ = unsafe { PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) }; LRESULT(0)
        }
        WM_CLOSE => { let _ = unsafe { DestroyWindow(hwnd) }; LRESULT(0) }
        WM_SYSCOMMAND if wp.0 & 0xfff0 == SC_CLOSE as usize => { let _ = unsafe { DestroyWindow(hwnd) }; LRESULT(0) }
        WM_DESTROY => { state.hwnd.set(HWND::default()); state.release_content();
            if std::env::var_os("ZSDESK_PREVIEW_WORKSPACE").is_some(){unsafe{PostQuitMessage(0);}} LRESULT(0) }
        WM_NCDESTROY => {
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0); drop(Rc::from_raw(raw)); }
            unsafe { DefWindowProcW(hwnd, message, wp, lp) }
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wp, lp) },
    }
}

#[implement(IPreviewHandlerFrame, IOleWindow)]
struct PreviewFrame { hwnd: isize, embedded: bool }
#[allow(non_snake_case)]
impl IPreviewHandlerFrame_Impl for PreviewFrame_Impl {
    fn GetWindowContext(&self) -> windows::core::Result<PREVIEWHANDLERFRAMEINFO> { Ok(PREVIEWHANDLERFRAMEINFO::default()) }
    fn TranslateAccelerator(&self, message: *const MSG) -> windows::core::Result<()> {
        if !self.embedded && !message.is_null() && closes_preview(unsafe { &*message }) {
            unsafe { PostMessageW(Some(HWND(self.hwnd as *mut _)), WM_CLOSE, WPARAM(0), LPARAM(0))?; }
            Ok(())
        } else { Err(Error::from_hresult(HRESULT(1))) }
    }
}
#[allow(non_snake_case)]
impl IOleWindow_Impl for PreviewFrame_Impl {
    fn GetWindow(&self) -> windows::core::Result<HWND> { Ok(HWND(self.hwnd as *mut _)) }
    fn ContextSensitiveHelp(&self, _enter: windows::core::BOOL) -> windows::core::Result<()> { Err(Error::from_hresult(HRESULT(0x80004001u32 as i32))) }
}

fn closes_preview(message: &MSG) -> bool {
    message.message == WM_KEYDOWN && (message.wParam.0 == VK_ESCAPE.0 as usize || message.wParam.0 == VK_SPACE.0 as usize)
}

fn system_preview(hwnd: HWND, path: &Path, rect: RECT) -> Result<SystemPreview> {
    system_preview_with_provider(hwnd, path, rect, PreviewProviderPreference::System,current_preview_colors(zsdesk_windows::theme::system_dark_mode()))
}

fn system_preview_with_provider(hwnd: HWND, path: &Path, rect: RECT, preference: PreviewProviderPreference,colors:PreviewColors) -> Result<SystemPreview> {
    let _dpi = PreviewDpiScope::for_window(hwnd);
    let discovery = super::preview_handlers::discover(path, preference);
    let mut failures = Vec::new();
    for candidate in discovery.candidates.iter().take(12) {
        if !unsafe{IsWindow(Some(hwnd))}.as_bool(){bail!("预览窗口已关闭");}
        eprintln!("preview activate {:?} {:?} {}",candidate.clsid,candidate.context,candidate.label);
        // Never load an Office or third-party document decoder into this process.
        let first: IPreviewHandler = match unsafe { CoCreateInstance(&candidate.clsid, None, candidate.context) } {
            Ok(handler) => handler,
            Err(error) => { failures.push(format!("{}：0x{:08X}", candidate.label, error.code().0 as u32)); continue; }
        };
        let supported = [first.cast::<IInitializeWithStream>().is_ok(), first.cast::<IInitializeWithFile>().is_ok(), first.cast::<IInitializeWithItem>().is_ok()];
        eprintln!("preview interfaces {supported:?}");
        if !supported.iter().any(|value| *value) { failures.push(format!("{} 未提供兼容的只读预览接口", candidate.label)); continue; }
        let mut first = Some(first);
        for (kind, supported) in supported.into_iter().enumerate() {
            if !unsafe{IsWindow(Some(hwnd))}.as_bool(){bail!("预览窗口已关闭");}
            if !supported { continue; }
            let handler = match first.take() {
                Some(handler) => handler,
                None => match unsafe { CoCreateInstance(&candidate.clsid, None, candidate.context) } {
                    Ok(handler) => handler, Err(_) => break,
                },
            };
            match initialize_preview(handler, hwnd, path, rect, kind,colors) {
                Ok(preview) => return Ok(preview),
                Err(error) => failures.push(format!("{}：{error:#}", candidate.label)),
            }
        }
    }
    if failures.is_empty() { bail!("{}", discovery.unavailable); }
    bail!("{}\r\n{}", discovery.unavailable, failures.into_iter().take(3).collect::<Vec<_>>().join("\r\n"))
}

fn initialize_preview(handler: IPreviewHandler, hwnd: HWND, path: &Path, rect: RECT, kind: usize,colors:PreviewColors) -> Result<SystemPreview> {
    eprintln!("preview initialize {kind}");
    let _dpi = PreviewDpiScope::for_window(hwnd);
    let site: IPreviewHandlerFrame = PreviewFrame { hwnd: hwnd.0 as isize,embedded:unsafe{GetWindowLongPtrW(hwnd,GWL_STYLE)}as u32&WS_CHILD.0!=0 }.into();
    let mut preview = SystemPreview { handler, _stream: None, _site: site, owner: hwnd, rendered:None };
    let with_site = preview.handler.cast::<IObjectWithSite>().context("预览器未提供宿主连接")?;
    unsafe { with_site.SetSite(&preview._site) }.context("预览器连接宿主失败")?;
    if !unsafe{IsWindow(Some(hwnd))}.as_bool(){bail!("预览窗口已关闭");}
    eprintln!("preview site connected");
    match kind {
        0 => {
            let initialize = preview.handler.cast::<IInitializeWithStream>()?;
            let stream = read_stream(path)?;
            unsafe { initialize.Initialize(&stream, STGM_READ.0) }.context("预览器只读流初始化失败")?;
            preview._stream = Some(stream);
        }
        1 => {
            let initialize = preview.handler.cast::<IInitializeWithFile>().context("预览器未提供只读文件接口")?;
            let file = wide_path(path);
            unsafe { initialize.Initialize(PCWSTR(file.as_ptr()), STGM_READ.0) }.context("预览器只读文件初始化失败")?;
        }
        _ => {
            let initialize = preview.handler.cast::<IInitializeWithItem>()?;
            let item = unsafe { zsdesk_windows::shell::shell_item(&ShellLocation::Path(path.into()))? };
            unsafe { initialize.Initialize(&item, STGM_READ.0) }.context("预览器只读项目初始化失败")?;
        }
    }
    if !unsafe { IsWindow(Some(hwnd)) }.as_bool() { bail!("预览窗口已关闭"); }
    // Set the requested palette before the handler creates/paints its child.
    // Applying it only after DoPreview permits a light first frame in dark mode.
    if let Ok(visuals)=preview.handler.cast::<IPreviewHandlerVisuals>(){
        let _=unsafe{visuals.SetBackgroundColor(COLORREF(colors.background))};
        let _=unsafe{visuals.SetTextColor(COLORREF(colors.text))};
    }
    eprintln!("preview initialized; binding window");
    unsafe { preview.handler.SetWindow(hwnd, &rect) }.context("预览器绑定窗口失败")?;
    if !unsafe{IsWindow(Some(hwnd))}.as_bool(){bail!("预览窗口已关闭");}
    eprintln!("preview bound; loading content");
    unsafe { preview.handler.DoPreview() }.context("预览器加载内容失败")?;
    if !unsafe{IsWindow(Some(hwnd))}.as_bool(){bail!("预览窗口已关闭");}
    eprintln!("preview loaded");
    Ok(preview)
}

#[cfg(test)]
#[path="preview_lifecycle_regression.rs"]
mod preview_lifecycle_regression;

fn preview_handler_clsid(path: &Path) -> Result<windows::core::GUID> {
    let extension = path.extension().and_then(|v| v.to_str()).context("文件没有预览类型")?;
    let extension = wide(&format!(".{extension}"));
    let iid = windows::core::w!("{8895B1C6-B41F-4C1C-A562-0D564250836F}");
    let mut length = 0;
    let _ = unsafe { AssocQueryStringW(ASSOCF_NOTRUNCATE, ASSOCSTR_SHELLEXTENSION, PCWSTR(extension.as_ptr()), iid, None, &mut length) };
    if !(2..=1024).contains(&length) { bail!("没有已注册的预览器"); }
    let mut clsid = vec![0u16; length as usize];
    unsafe { AssocQueryStringW(ASSOCF_NOTRUNCATE, ASSOCSTR_SHELLEXTENSION, PCWSTR(extension.as_ptr()), iid, Some(PWSTR(clsid.as_mut_ptr())), &mut length).ok()?; }
    Ok(unsafe { CLSIDFromString(PCWSTR(clsid.as_ptr()))? })
}

#[cfg(test)]
#[allow(dead_code)]
pub(super) fn handler_registration_for_test(path: &Path) -> Result<String> { Ok(format!("{:?}", preview_handler_clsid(path)?)) }

#[cfg(test)]
#[allow(dead_code)]
pub(super) fn handler_open_for_test(owner: HWND, path: &Path) -> Result<()> {
    let handler = system_preview(owner, path, RECT { left: 0, top: 0, right: 400, bottom: 300 })?;
    drop(handler);
    Ok(())
}

fn read_stream(path: &Path) -> windows::core::Result<IStream> {
    let name = wide_path(path);
    unsafe { SHCreateStreamOnFileEx(PCWSTR(name.as_ptr()), (STGM_READ | STGM_SHARE_DENY_NONE).0, 0, false, None) }
}

fn read_text(path: &Path) -> Result<String> {
    let mut bytes = Vec::new();
    File::open(path)?.take((MAX_TEXT_BYTES + 1) as u64).read_to_end(&mut bytes)?;
    let truncated = bytes.len() > MAX_TEXT_BYTES;
    bytes.truncate(MAX_TEXT_BYTES);
    let mut text = decode_text(&bytes).context("文件不是可显示的文本")?;
    if truncated { text.push_str("\r\n\r\n…仅显示前 256 KB"); }
    Ok(text)
}

fn decode_text(bytes: &[u8]) -> Option<String> {
    if bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF]) {
        let little = bytes[0] == 0xFF;
        let values: Vec<u16> = bytes[2..].chunks_exact(2).map(|v| if little { u16::from_le_bytes([v[0], v[1]]) } else { u16::from_be_bytes([v[0], v[1]]) }).collect();
        return Some(String::from_utf16_lossy(&values).replace('\0', "�"));
    }
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    if bytes.contains(&0) { return None; }
    match std::str::from_utf8(bytes) {
        Ok(text) => return Some(text.to_string()),
        Err(error) if error.error_len().is_none() => return Some(String::from_utf8_lossy(&bytes[..error.valid_up_to()]).into_owned()),
        Err(_) => {}
    }
    if bytes.is_empty() { return Some(String::new()); }
    let length = unsafe { MultiByteToWideChar(CP_ACP, MB_ERR_INVALID_CHARS, bytes, None) };
    if length <= 0 { return Some(String::from_utf8_lossy(bytes).into_owned()); }
    let mut text = vec![0u16; length as usize];
    let converted = unsafe { MultiByteToWideChar(CP_ACP, MB_ERR_INVALID_CHARS, bytes, Some(&mut text)) };
    (converted > 0).then(|| String::from_utf16_lossy(&text[..converted as usize]))
}

fn image_request_cancelled(path:&Path)->bool{
    let Some(workspace)=std::env::var_os("ZSDESK_PREVIEW_WORKSPACE")else{return false;};
    let request=PathBuf::from(workspace).join("request.json");
    let Ok(metadata)=fs::metadata(&request)else{return false;};
    if metadata.len()>65536{return true;}
    let Ok(bytes)=fs::read(request)else{return false;};
    let Ok(value)=serde_json::from_slice::<serde_json::Value>(&bytes)else{return false;};
    value["close"].as_bool()==Some(true)||value["path"].as_str().is_some_and(|current|Path::new(current)!=path)
}
fn fit_dimensions(width: u32, height: u32, max_width: u32, max_height: u32) -> (u32, u32) {
    if width == 0 || height == 0 || max_width == 0 || max_height == 0 { return (0, 0); }
    let ratio = (max_width as f64 / width as f64).min(max_height as f64 / height as f64).min(1.0);
    ((width as f64 * ratio).round().max(1.0) as u32, (height as f64 * ratio).round().max(1.0) as u32)
}
fn is_text_path(path: &Path) -> bool {
    matches!(extension(path).as_str(), "txt"|"md"|"log"|"csv"|"tsv"|"json"|"toml"|"ini"|"cfg"|"xml"|"yaml"|"yml"|
        "rs"|"c"|"h"|"cpp"|"hpp"|"py"|"js"|"ts"|"css"|"html"|"htm"|"sql"|"ps1"|"bat"|"cmd"|"sh")
}
fn is_image_path(path: &Path) -> bool { matches!(extension(path).as_str(), "png"|"jpg"|"jpeg"|"bmp"|"gif"|"tif"|"tiff"|"webp"|"ico") }
fn extension(path: &Path) -> String { path.extension().unwrap_or_default().to_string_lossy().to_ascii_lowercase() }
fn file_type(path: &Path) -> String { let ext = extension(path); if ext.is_empty() { "文件".into() } else { format!("{} 文件", ext.to_uppercase()) } }
fn wide(value: &str) -> Vec<u16> { value.encode_utf16().chain(Some(0)).collect() }
fn wide_path(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}
fn draw_text(dc: HDC, text: &str, rect: &mut RECT, format: DRAW_TEXT_FORMAT) {
    // An empty Vec<u16> has the non-null dangling address 0x2. DrawTextW can
    // inspect its first character even for a zero count during a nested paint.
    // COM activation pumps WM_PAINT while summary/notice can both be empty.
    if text.is_empty() || rect.right<=rect.left || rect.bottom<=rect.top {return;}
    let mut text: Vec<u16> = text.encode_utf16().collect();
    let length=text.len();text.push(0);
    unsafe { DrawTextW(dc, &mut text[..length], rect, format); }
}
fn initial_bounds(owner: HWND) -> RECT {
    let dpi = unsafe { GetDpiForWindow(owner) }.max(96);
    let scale = |value| zsdesk_ui::chrome::scale(value, dpi);
    let monitor = unsafe { MonitorFromWindow(owner, MONITOR_DEFAULTTONEAREST) };
    let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
    let mut owner_rect = RECT::default();
    let _ = unsafe { GetWindowRect(owner, &mut owner_rect) };
    let work = if unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() { info.rcWork }
        else { RECT { left: 0, top: 0, right: 1024, bottom: 768 } };
    let width = scale(760).min((work.right - work.left - scale(32)).max(1));
    let height = scale(600).min((work.bottom - work.top - scale(32)).max(1));
    let x = (owner_rect.left + (owner_rect.right - owner_rect.left - width) / 2).clamp(work.left, (work.right - width).max(work.left));
    let y = (owner_rect.top + (owner_rect.bottom - owner_rect.top - height) / 2).clamp(work.top, (work.bottom - height).max(work.top));
    RECT { left: x, top: y, right: x + width, bottom: y + height }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dpi_rectangles_and_high_contrast_colors_are_validated(){
        assert_eq!(dpi_suggestion(&RECT{left:10,top:20,right:410,bottom:320}),Some((10,20,400,300)));
        assert!(dpi_suggestion(&RECT{left:20,top:20,right:10,bottom:30}).is_none());
        assert!(dpi_suggestion(&RECT{left:i32::MIN,top:0,right:i32::MAX,bottom:10}).is_none());
        let contrast=preview_colors(true,true,0x102030,0xDDEEFF);
        assert_eq!(contrast,PreviewColors{background:0x102030,text:0xDDEEFF,dark:false});
        assert_eq!(preview_colors(true,false,0,0),PreviewColors{background:0x202020,text:0xF2F2F2,dark:true});
    }
    #[test]
    fn null_dpi_and_theme_notifications_refresh_hidden_native_preview()->Result<()>{
        use windows::Win32::System::Ole::{OleInitialize,OleUninitialize};
        unsafe{OleInitialize(None)}?;
        struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}let _apartment=Apartment;
        struct Owner(HWND);impl Drop for Owner{fn drop(&mut self){let _=unsafe{DestroyWindow(self.0)};}}
        let owner=Owner(unsafe{CreateWindowExW(WS_EX_NOACTIVATE,windows::core::w!("STATIC"),windows::core::w!("Preview message regression"),WS_POPUP,0,0,20,20,None,None,None,None)}?);
        let preview=QuickPreview::open_impl(owner.0,Path::new(""),false,false,PreviewProviderPreference::System,true,false,None)?;
        let text=preview.state.text_control("中文只读预览")?;let child=text.0;
        *preview.state.content.borrow_mut()=Content::Text(text);
        let hwnd=preview.window();let mut before=RECT::default();unsafe{GetWindowRect(hwnd,&mut before)}?;
        unsafe{SendMessageW(hwnd,WM_DPICHANGED,Some(WPARAM((144<<16)|144)),Some(LPARAM(0)));}
        let invalid=RECT{left:i32::MIN,top:0,right:i32::MAX,bottom:10};
        unsafe{SendMessageW(hwnd,WM_DPICHANGED,Some(WPARAM((144<<16)|144)),Some(LPARAM(&invalid as*const RECT as isize)));}
        let mut after=RECT::default();unsafe{GetWindowRect(hwnd,&mut after)}?;assert_eq!(before,after);
        for(message,value)in [(WM_THEMECHANGED,0),(WM_DWMCOMPOSITIONCHANGED,0),(WM_SYSCOLORCHANGE,0),
            (WM_SETTINGCHANGE,SPI_SETHIGHCONTRAST.0 as usize),(WM_DPICHANGED_AFTERPARENT,0)]{
            unsafe{SendMessageW(hwnd,message,Some(WPARAM(value)),Some(LPARAM(0)));}
            assert!(!preview.state.refreshing_theme.get());
            assert_eq!(preview.state.colors.get(),current_preview_colors(true));
            assert_eq!(unsafe{SendMessageW(child,WM_GETFONT,None,None)}.0,preview.state.font.get().0 as isize);
        }
        let mut brush=LOGBRUSH::default();
        assert_eq!(unsafe{GetObjectW(preview.state.brush.get().into(),size_of::<LOGBRUSH>()as i32,Some((&mut brush as*mut LOGBRUSH).cast()))},size_of::<LOGBRUSH>()as i32);
        assert_eq!(brush.lbColor.0,preview.state.colors.get().background);
        assert!(!unsafe{IsWindowVisible(hwnd)}.as_bool());
        Ok(())
    }
    #[test]
    fn empty_loading_text_is_safe_with_a_real_gdi_context() {
        let dc=unsafe{CreateCompatibleDC(None)};
        assert!(!dc.0.is_null());
        let mut rect=RECT{left:0,top:0,right:300,bottom:80};
        draw_text(dc,"",&mut rect,DT_WORDBREAK|DT_NOPREFIX);
        draw_text(dc,"中文 loading",&mut rect,DT_SINGLELINE|DT_END_ELLIPSIS);
        assert!(unsafe{DeleteDC(dc)}.as_bool());
    }
    #[test]
    fn text_decoding_preserves_unicode_and_rejects_binary_nuls() {
        assert_eq!(decode_text("施工记录".as_bytes()).unwrap(), "施工记录");
        assert_eq!(decode_text(&[0xEF,0xBB,0xBF,b'A']).unwrap(), "A");
        assert_eq!(decode_text(&[0xFF,0xFE,0x2D,0x4E,0x87,0x65]).unwrap(), "中文");
        assert_eq!(decode_text(&[0xFE,0xFF,0x4E,0x2D,0x65,0x87]).unwrap(), "中文");
        assert_eq!(decode_text(&"中文".as_bytes()[..5]).unwrap(), "中");
        assert!(decode_text(&[1,0,2,0]).is_none());
    }
    #[test]
    fn image_limits_prevent_overflow_and_bound_the_retained_bitmap() {
        assert!(!image_decode::dimensions_allowed(0,100));
        assert!(!image_decode::dimensions_allowed(u32::MAX,u32::MAX));
        assert!(image_decode::dimensions_allowed(9203,6135));
        let (w,h) = fit_dimensions(6000,3000,MAX_BITMAP_WIDTH,MAX_BITMAP_HEIGHT);
        assert_eq!((w,h),(1600,800));
        assert!(u64::from(w)*u64::from(h)*4 <= u64::from(MAX_BITMAP_WIDTH)*u64::from(MAX_BITMAP_HEIGHT)*4);
        assert_eq!(fit_dimensions(100,100,1600,1200),(100,100));
    }
    #[test]
    fn executable_files_are_not_treated_as_text_or_images() {
        assert!(!is_text_path(Path::new("工具.exe")) && !is_image_path(Path::new("工具.exe")));
        assert!(is_text_path(Path::new("日志.TXT")) && is_image_path(Path::new("图纸.PNG")));
    }
}
