//! Read-only local media playback inside the disposable preview worker.
use std::{cell::{Cell,RefCell},path::Path,rc::Rc,sync::{Arc,atomic::{AtomicBool,AtomicUsize,Ordering}},time::{Duration,Instant}};
use anyhow::{Context,Result,bail};
use windows::{core::{implement,BSTR,Interface,PCWSTR,PWSTR,HRESULT},Win32::{
    Foundation::{HWND,HINSTANCE,HMODULE,LPARAM,LRESULT,RECT,WPARAM,COLORREF,FreeLibrary},
    Graphics::Gdi::*,Media::MediaFoundation::*,
    System::{Com::{CoCreateInstance,CLSCTX_INPROC_SERVER},LibraryLoader::{GetModuleHandleW,LoadLibraryExW,GetProcAddress,LOAD_LIBRARY_SEARCH_SYSTEM32}},
    UI::{Shell::UrlCreateFromPathW,WindowsAndMessaging::*}}};

const CLASS:PCWSTR=windows::core::w!("ZSDesk.MediaPreview");
const EVENT:u32=WM_APP+1;
const TIMER:usize=1;
static TOKEN:AtomicUsize=AtomicUsize::new(1);
pub(super) fn supported(path:&Path)->bool{
    path.extension().is_some_and(|extension|matches!(extension.to_string_lossy().to_ascii_lowercase().as_str(),
        "mp4"|"m4v"|"mov"|"mkv"|"avi"|"wmv"|"webm"|"mp3"|"m4a"|"aac"|"wav"|"wma"|"flac"|"ogg"|
        "3gp"|"3g2"|"3gp2"|"3gpp"|"asf"|"adts"))
}
fn wide(value:&str)->Vec<u16>{value.encode_utf16().chain(Some(0)).collect()}
fn audio_only(path:&Path)->bool{path.extension().is_some_and(|extension|matches!(extension.to_string_lossy().to_ascii_lowercase().as_str(),"mp3"|"m4a"|"aac"|"wav"|"wma"|"flac"|"ogg"|"adts"))}
fn playback_error(code:Option<HRESULT>)->String{
    let detail=code.map(|code|format!("（0x{:08X}）",code.0 as u32)).unwrap_or_default();
    let message=match code{
        Some(MF_E_CANNOT_CREATE_SINK)=>zsdesk_core::i18n::text("无法创建媒体播放输出，请检查音频输出设备或使用默认播放器"),
        Some(MF_E_AUDIO_PLAYBACK_DEVICE_INVALIDATED)=>zsdesk_core::i18n::text("音频输出设备已断开，请连接设备后重新打开预览"),
        Some(MF_E_AUDIO_PLAYBACK_DEVICE_IN_USE)=>zsdesk_core::i18n::text("音频输出设备正被占用，请释放设备后重新打开预览"),
        Some(MF_E_TOPO_CODEC_NOT_FOUND|MF_E_UNSUPPORTED_BYTESTREAM_TYPE)=>zsdesk_core::i18n::text("当前 Windows 不支持此文件的媒体格式，可使用默认播放器或已安装的 QuickLook"),
        _=>zsdesk_core::i18n::text("无法播放此媒体文件，可使用默认播放器或已安装的 QuickLook"),
    };format!("{message}{detail}")
}
fn local_url(path:&Path)->Result<BSTR>{
    use std::os::windows::ffi::OsStrExt;
    if !path.is_absolute(){bail!("{}", zsdesk_core::tr_format!("媒体预览需要完整文件路径"));}
    let name:Vec<_>=path.as_os_str().encode_wide().collect();
    if name.len()>32767||name.contains(&0){bail!("{}", zsdesk_core::tr_format!("媒体路径无效"));}
    let name:Vec<_>=name.into_iter().chain(Some(0)).collect();let mut result=vec![0u16;65536];let mut count=result.len()as u32;
    unsafe{UrlCreateFromPathW(PCWSTR(name.as_ptr()),PWSTR(result.as_mut_ptr()),&mut count,0)}?;
    let end=result.iter().position(|unit|*unit==0).unwrap_or(count as usize);
    Ok(BSTR::from_wide(&result[..end]))
}
type Startup=unsafe extern "system" fn(u32,u32)->HRESULT;
type Shutdown=unsafe extern "system" fn()->HRESULT;
type CreateAttributes=unsafe extern "system" fn(*mut *mut core::ffi::c_void,u32)->HRESULT;
struct Module(HMODULE);impl Drop for Module{fn drop(&mut self){let _=unsafe{FreeLibrary(self.0)};}}
/// The file manager must also start on Windows N without the media feature
/// pack. Loading these optional exports only when a media preview is requested
/// avoids adding mfplat.dll to the executable's mandatory import table.
struct Foundation{shutdown:Shutdown,create_attributes:CreateAttributes,_module:Module}
impl Foundation{
    fn start()->Result<Self>{Self::start_from(windows::core::w!("mfplat.dll"))}
    fn start_from(name:PCWSTR)->Result<Self>{
        let module=Module(unsafe{LoadLibraryExW(name,None,LOAD_LIBRARY_SEARCH_SYSTEM32)}
            .context(zsdesk_core::i18n::text("Windows 媒体组件不可用；可使用 QuickLook 或默认播放器"))?);
        let startup:Startup=unsafe{std::mem::transmute(GetProcAddress(module.0,windows::core::s!("MFStartup"))
            .context(zsdesk_core::i18n::text("Windows 媒体组件缺少 MFStartup"))?)};
        let shutdown:Shutdown=unsafe{std::mem::transmute(GetProcAddress(module.0,windows::core::s!("MFShutdown"))
            .context(zsdesk_core::i18n::text("Windows 媒体组件缺少 MFShutdown"))?)};
        let create_attributes:CreateAttributes=unsafe{std::mem::transmute(GetProcAddress(module.0,windows::core::s!("MFCreateAttributes"))
            .context(zsdesk_core::i18n::text("Windows 媒体组件缺少 MFCreateAttributes"))?)};
        unsafe{startup(MF_VERSION,MFSTARTUP_FULL)}.ok().context(zsdesk_core::i18n::text("Windows 媒体组件初始化失败"))?;
        Ok(Self{shutdown,create_attributes,_module:module})
    }
    fn attributes(&self)->Result<IMFAttributes>{
        let mut raw=std::ptr::null_mut();unsafe{(self.create_attributes)(&mut raw,2)}.ok()?;
        if raw.is_null(){bail!("{}", zsdesk_core::tr_format!("媒体组件未返回属性对象"));}
        Ok(unsafe{IMFAttributes::from_raw(raw)})
    }
}
impl Drop for Foundation{fn drop(&mut self){let _=unsafe{(self.shutdown)()};}}
#[implement(IMFMediaEngineNotify)]
struct Notify{window:isize,token:usize,alive:Arc<AtomicBool>}
#[allow(non_snake_case)]impl IMFMediaEngineNotify_Impl for Notify_Impl{
    fn EventNotify(&self,event:u32,_:usize,_:u32)->windows::core::Result<()>{
        if self.alive.load(Ordering::Acquire){let _=unsafe{PostMessageW(Some(HWND(self.window as*mut _)),EVENT,WPARAM(self.token),LPARAM(event as isize))};}Ok(())
    }
}
struct State{
    hwnd:Cell<HWND>,video:Cell<HWND>,engine:RefCell<Option<IMFMediaEngine>>,alive:Arc<AtomicBool>,token:usize,
    ready:Cell<bool>,notice:RefCell<String>,dark:Cell<bool>,started:Instant,_foundation:Foundation,
}
impl State{
    fn engine(&self)->Option<IMFMediaEngine>{self.engine.borrow().clone()}
    fn shutdown(&self){self.alive.store(false,Ordering::Release);let engine=self.engine.replace(None);if let Some(engine)=engine{let _=unsafe{engine.Shutdown()};}}
    fn repaint(&self){let _=unsafe{InvalidateRect(Some(self.hwnd.get()),None,false)};}
    fn bounds(&self)->RECT{let mut rect=RECT::default();let _=unsafe{GetClientRect(self.hwnd.get(),&mut rect)};rect}
    fn footer_height(&self)->i32{zsdesk_ui::chrome::scale(40,unsafe{windows::Win32::UI::HiDpi::GetDpiForWindow(self.hwnd.get())}.max(96))}
    fn layout(&self){
        let rect=self.bounds();let height=(rect.bottom-self.footer_height()).max(1);
        let _=unsafe{MoveWindow(self.video.get(),0,0,rect.right.max(1),height,true)};
        if let Some(engine)=self.engine().and_then(|engine|engine.cast::<IMFMediaEngineEx>().ok()){
            let(mut native_width,mut native_height)=(0,0);let _=unsafe{engine.GetNativeVideoSize(Some(&mut native_width),Some(&mut native_height))};
            let output=video_rect(rect.right.max(1),height,native_width,native_height);let border=MFARGB{rgbBlue:0,rgbGreen:0,rgbRed:0,rgbAlpha:255};
            let _=unsafe{engine.UpdateVideoStream(None,Some(&output),Some(&border))};
        }
    }
    fn event(&self,event:u32){
        if event==MF_MEDIA_ENGINE_EVENT_ERROR.0 as u32{
            let error=self.engine().and_then(|engine|unsafe{engine.GetError()}.ok());
            let code=error.as_ref().and_then(|error|unsafe{error.GetExtendedErrorCode()}.err()).map(|error|error.code());
            *self.notice.borrow_mut()=playback_error(code);
            self.ready.set(false);let _=unsafe{ShowWindow(self.video.get(),SW_HIDE)};
        }else if event==MF_MEDIA_ENGINE_EVENT_CANPLAY.0 as u32||event==MF_MEDIA_ENGINE_EVENT_LOADEDDATA.0 as u32{
            self.ready.set(true);self.notice.borrow_mut().clear();let _=unsafe{ShowWindow(self.video.get(),SW_SHOWNOACTIVATE)};self.layout();
        }
        self.repaint();
    }
    fn toggle(&self){if !self.ready.get(){return;}if let Some(engine)=self.engine(){let result=unsafe{if engine.IsPaused().as_bool(){engine.Play()}else{engine.Pause()}};
        if let Err(error)=result{*self.notice.borrow_mut()=zsdesk_core::tr_format!("媒体播放失败（0x{:08X}）", error.code().0 as u32);}}
        self.repaint();
    }
    fn paint(&self){
        let mut ps=PAINTSTRUCT::default();let dc=unsafe{BeginPaint(self.hwnd.get(),&mut ps)};let rect=self.bounds();
        let old_font=unsafe{SelectObject(dc,GetStockObject(DEFAULT_GUI_FONT))};
        let color=if self.dark.get(){0x202020}else{0xf7f7f7};let brush=unsafe{CreateSolidBrush(COLORREF(color))};
        unsafe{FillRect(dc,&rect,brush);DeleteObject(brush.into());SetBkMode(dc,TRANSPARENT);SetTextColor(dc,COLORREF(if self.dark.get(){0xf0f0f0}else{0x202020}));}
        let footer=RECT{left:12,top:(rect.bottom-self.footer_height()).max(0),right:(rect.right-12).max(12),bottom:rect.bottom};
        let text=if !self.notice.borrow().is_empty(){self.notice.borrow().clone()}else if let Some(engine)=self.engine(){
            let current=unsafe{engine.GetCurrentTime()};let duration=unsafe{engine.GetDuration()};
            zsdesk_core::tr_format!("{}    {} / {}    点击进度区域定位", if unsafe{engine.IsPaused()}.as_bool(){zsdesk_core::i18n::text("▶ 播放")}else{zsdesk_core::i18n::text("Ⅱ 暂停")}, time_text(current), time_text(duration))
        }else{zsdesk_core::i18n::text("媒体已关闭").into()};
        let mut text:Vec<_>=text.encode_utf16().collect();let mut area=if self.notice.borrow().is_empty(){footer}else{RECT{left:12,top:12,right:rect.right-12,bottom:rect.bottom-12}};
        unsafe{DrawTextW(dc,&mut text,&mut area,DT_WORDBREAK|DT_NOPREFIX);SelectObject(dc,old_font);EndPaint(self.hwnd.get(),&ps);}
    }
}
impl Drop for State{fn drop(&mut self){self.shutdown();}}
fn time_text(seconds:f64)->String{if !seconds.is_finite()||seconds<0.0{return "--:--".into();}let s=seconds as u64;format!("{}:{:02}",s/60,s%60)}
fn video_rect(width:i32,height:i32,native_width:u32,native_height:u32)->RECT{
    if native_width==0||native_height==0{return RECT{left:0,top:0,right:width,bottom:height};}
    let scale=(f64::from(width)/f64::from(native_width)).min(f64::from(height)/f64::from(native_height));
    let w=(f64::from(native_width)*scale).round()as i32;let h=(f64::from(native_height)*scale).round()as i32;
    RECT{left:(width-w)/2,top:(height-h)/2,right:(width+w)/2,bottom:(height+h)/2}
}
pub(super) struct MediaPreview{state:Rc<State>}
impl MediaPreview{
    pub(super) fn open(parent:HWND,path:&Path,bounds:RECT,dark:bool,hover:bool)->Result<Self>{
        let url=local_url(path)?;let foundation=Foundation::start()?;
        let instance=HINSTANCE(unsafe{GetModuleHandleW(None)}?.0);
        unsafe{RegisterClassW(&WNDCLASSW{hInstance:instance,lpfnWndProc:Some(procedure),hCursor:LoadCursorW(None,IDC_ARROW)?,lpszClassName:CLASS,..Default::default()});}
        let state=Rc::new(State{hwnd:Cell::new(HWND::default()),video:Cell::new(HWND::default()),engine:RefCell::new(None),alive:Arc::new(AtomicBool::new(true)),
            token:TOKEN.fetch_add(1,Ordering::Relaxed),ready:Cell::new(false),notice:RefCell::new(zsdesk_core::i18n::text("正在读取媒体…").into()),dark:Cell::new(dark),started:Instant::now(),_foundation:foundation});
        let hwnd=unsafe{CreateWindowExW(WS_EX_NOPARENTNOTIFY,CLASS,PCWSTR::null(),WS_CHILD|WS_CLIPCHILDREN,bounds.left,bounds.top,bounds.right-bounds.left,bounds.bottom-bounds.top,
            Some(parent),None,Some(instance),Some((&state as*const Rc<State>).cast()))}?;state.hwnd.set(hwnd);
        let preview=Self{state};
        let video=unsafe{CreateWindowExW(WS_EX_NOPARENTNOTIFY,windows::core::w!("STATIC"),PCWSTR::null(),WS_CHILD,0,0,1,1,Some(hwnd),None,Some(instance),None)}?;
        preview.state.video.set(video);
        let attributes=preview.state._foundation.attributes()?;
        let notify:IMFMediaEngineNotify=Notify{window:hwnd.0 as isize,token:preview.state.token,alive:preview.state.alive.clone()}.into();
        let audio=audio_only(path);
        unsafe{attributes.SetUnknown(&MF_MEDIA_ENGINE_CALLBACK,&notify)?;if !audio{attributes.SetUINT64(&MF_MEDIA_ENGINE_PLAYBACK_HWND,video.0 as usize as u64)?;}}
        let factory:IMFMediaEngineClassFactory=unsafe{CoCreateInstance(&CLSID_MFMediaEngineClassFactory,None,CLSCTX_INPROC_SERVER)}?;
        let engine=unsafe{factory.CreateInstance(if audio{MF_MEDIA_ENGINE_AUDIOONLY.0 as u32}else{0},&attributes)}?;
        *preview.state.engine.borrow_mut()=Some(engine.clone());
        unsafe{engine.SetAutoPlay(false)?;engine.SetMuted(hover)?;engine.SetPreload(MF_MEDIA_ENGINE_PRELOAD_AUTOMATIC)?;engine.SetSource(&url)?;engine.Load()?;}
        preview.state.layout();unsafe{SetTimer(Some(hwnd),TIMER,250,None);let _=ShowWindow(hwnd,SW_SHOWNOACTIVATE);}
        Ok(preview)
    }
    pub(super) fn layout(&self,rect:RECT){let _=unsafe{MoveWindow(self.state.hwnd.get(),rect.left,rect.top,rect.right-rect.left,rect.bottom-rect.top,true)};self.state.layout();}
    pub(super) fn set_dark(&self,dark:bool){self.state.dark.set(dark);self.state.repaint();}
    #[cfg(test)]pub(super) fn ready(&self)->bool{self.state.ready.get()}
}
impl Drop for MediaPreview{fn drop(&mut self){self.state.shutdown();let hwnd=self.state.hwnd.replace(HWND::default());let _=unsafe{DestroyWindow(hwnd)};}}
unsafe extern "system" fn procedure(hwnd:HWND,message:u32,wp:WPARAM,lp:LPARAM)->LRESULT{
    if message==WM_NCCREATE{let create=unsafe{&*(lp.0 as*const CREATESTRUCTW)};let state=unsafe{&*(create.lpCreateParams as*const Rc<State>)}.clone();state.hwnd.set(hwnd);unsafe{SetWindowLongPtrW(hwnd,GWLP_USERDATA,Rc::into_raw(state)as isize);}return LRESULT(1);}
    let raw=unsafe{GetWindowLongPtrW(hwnd,GWLP_USERDATA)}as*const State;if raw.is_null(){return unsafe{DefWindowProcW(hwnd,message,wp,lp)};}
    unsafe{Rc::increment_strong_count(raw);}let state=unsafe{Rc::from_raw(raw)};
    match message{
        EVENT if wp.0==state.token=>{state.event(lp.0 as u32);LRESULT(0)},
        WM_SIZE=>{state.layout();LRESULT(0)},
        WM_TIMER if wp.0==TIMER=>{if !state.ready.get()&&state.started.elapsed()>Duration::from_secs(20)&&state.notice.borrow().as_str()=="正在读取媒体…"{
            *state.notice.borrow_mut()=zsdesk_core::i18n::text("读取媒体超时，可使用 QuickLook 或默认播放器打开。").into();state.shutdown();let _=unsafe{ShowWindow(state.video.get(),SW_HIDE)};
        }state.repaint();LRESULT(0)},
        WM_LBUTTONUP=>{let y=((lp.0>>16)as u16 as i16)as i32;let x=lp.0 as u16 as i16 as i32;let rect=state.bounds();
            if y>=rect.bottom-state.footer_height(){if x<100{state.toggle();}else if let Some(engine)=state.engine(){let duration=unsafe{engine.GetDuration()};
                if duration.is_finite()&&duration>0.0{let fraction=f64::from((x-100).max(0))/f64::from((rect.right-112).max(1));let _=unsafe{engine.SetCurrentTime(duration*fraction.clamp(0.0,1.0))};}}}LRESULT(0)},
        WM_PAINT=>{state.paint();LRESULT(0)},WM_ERASEBKGND=>LRESULT(1),
        WM_NCDESTROY=>{state.shutdown();state.hwnd.set(HWND::default());unsafe{SetWindowLongPtrW(hwnd,GWLP_USERDATA,0);drop(Rc::from_raw(raw));DefWindowProcW(hwnd,message,wp,lp)}},
        _=>unsafe{DefWindowProcW(hwnd,message,wp,lp)},
    }
}

#[cfg(test)]mod tests{
    use super::*;
    #[test]fn missing_media_runtime_is_a_local_error_not_a_loader_failure(){
        let result=Foundation::start_from(windows::core::w!("ZSDesk.Absent.MediaFoundation.Probe.dll"));
        assert!(result.is_err());let error=result.err().unwrap().to_string();assert!(error.contains("Windows 媒体组件不可用"));
        let absent_export=Foundation::start_from(windows::core::w!("kernel32.dll"));
        assert!(absent_export.is_err());assert!(absent_export.err().unwrap().to_string().contains("缺少 MFStartup"));
    }
    #[test]fn media_detection_and_time_labels_are_bounded(){assert!(supported(Path::new("电影.MP4")));assert!(supported(Path::new("声音.flac")));assert!(!supported(Path::new("表格.xlsx")));assert_eq!(time_text(f64::NAN),"--:--");assert_eq!(time_text(65.0),"1:05");}
    #[test]fn output_failure_is_not_misreported_as_missing_codec(){assert!(audio_only(Path::new("声音.WAV")));assert!(!audio_only(Path::new("电影.mp4")));
        let text=playback_error(Some(MF_E_CANNOT_CREATE_SINK));assert!(text.contains("播放输出")&&text.contains("0xC00D36FA")&&!text.contains("格式"));
        assert!(playback_error(Some(MF_E_TOPO_CODEC_NOT_FOUND)).contains("媒体格式"));}
    #[test]fn local_media_url_preserves_reserved_path_characters()->Result<()>{let url=local_url(Path::new(r"C:\视频 空格\a#b%片.mp4"))?.to_string();assert!(url.starts_with("file:"));assert!(url.contains("%23")&&url.contains("%25"));assert!(local_url(Path::new("https://example.com/a.mp4")).is_err());Ok(())}
    #[test]fn media_viewport_keeps_portrait_and_landscape_aspect(){
        let r=video_rect(600,400,1920,1080);assert_eq!(r.right-r.left,600);assert!((r.bottom-r.top-338).abs()<=1);
        let r=video_rect(600,400,1080,1920);assert_eq!(r.bottom-r.top,400);assert_eq!(r.right-r.left,225);
    }
    #[test]#[ignore="Owned hidden Media Foundation audio fixture; requires ZSDESK_NATIVE_TEST_OUTPUT"]
    fn native_media_preview_decodes_pauses_and_releases_file()->Result<()>{
        use std::{fs,thread};use anyhow::ensure;
        use windows::Win32::System::Ole::{OleInitialize,OleUninitialize};
        let root=std::path::PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("isolated output")?).join("media-preview");ensure!(root.is_absolute(),"absolute fixture root");fs::create_dir_all(&root)?;
        let path=root.join("中文 媒体.wav");let sample_rate=8000u32;let data=vec![0u8;sample_rate as usize*2];
        let mut wave=Vec::new();wave.extend(b"RIFF");wave.extend((36+data.len()as u32).to_le_bytes());wave.extend(b"WAVEfmt ");wave.extend(16u32.to_le_bytes());wave.extend(1u16.to_le_bytes());wave.extend(1u16.to_le_bytes());
        wave.extend(sample_rate.to_le_bytes());wave.extend((sample_rate*2).to_le_bytes());wave.extend(2u16.to_le_bytes());wave.extend(16u16.to_le_bytes());wave.extend(b"data");wave.extend((data.len()as u32).to_le_bytes());wave.extend(data);fs::write(&path,&wave)?;
        let foreground=unsafe{GetForegroundWindow()};let file=path.clone();
        let result=thread::spawn(move||->Result<()>{
            unsafe{OleInitialize(None)}?;struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}let _apartment=Apartment;
            struct Owner(HWND);impl Drop for Owner{fn drop(&mut self){let _=unsafe{DestroyWindow(self.0)};}}
            let owner=Owner(unsafe{CreateWindowExW(WS_EX_NOACTIVATE,windows::core::w!("STATIC"),PCWSTR::null(),WS_POPUP,20,20,640,480,None,None,None,None)}?);
            let media=MediaPreview::open(owner.0,&file,RECT{left:0,top:0,right:640,bottom:480},true,true)?;
            let pump=|ms:u64|{let until=Instant::now()+Duration::from_millis(ms);while Instant::now()<until{let mut msg=MSG::default();while unsafe{PeekMessageW(&mut msg,None,0,0,PM_REMOVE)}.as_bool(){if msg.message!=WM_QUIT{unsafe{let _=TranslateMessage(&msg);DispatchMessageW(&msg);}}}thread::sleep(Duration::from_millis(2));}};
            let deadline=Instant::now()+Duration::from_secs(12);while !media.ready(){pump(20);ensure!(Instant::now()<deadline,"media did not decode: {}",media.state.notice.borrow());}
            let engine=media.state.engine().context("engine")?;ensure!(unsafe{engine.IsPaused()}.as_bool(),"preview started playing without user input");ensure!((unsafe{engine.GetDuration()}-1.0).abs()<0.1,"wrong media duration");
            ensure!(unsafe{engine.GetMuted()}.as_bool(),"hover preview was not muted");media.state.toggle();pump(100);ensure!(!unsafe{engine.IsPaused()}.as_bool(),"play did not begin");media.state.toggle();ensure!(unsafe{engine.IsPaused()}.as_bool(),"pause did not stop playback");drop(engine);drop(media);pump(30);
            let moved=file.with_extension("released.wav");fs::rename(&file,&moved)?;fs::rename(&moved,&file)?;Ok(())
        }).join().map_err(|_|anyhow::anyhow!("media fixture thread panicked"))?;
        let mut foreground_pid=0;unsafe{GetWindowThreadProcessId(GetForegroundWindow(),Some(&mut foreground_pid));}
        let report=serde_json::json!({"state":if result.is_ok()&&foreground_pid!=std::process::id(){"passed"}else{"failed"},"result":result.as_ref().map_err(|error|format!("{error:#}")),"foreground_unchanged":foreground==unsafe{GetForegroundWindow()},"owned_hidden_window":true,"user_file_modified":false});
        fs::write(root.join("verification.current.json"),serde_json::to_vec_pretty(&report)?)?;result?;ensure!(foreground_pid!=std::process::id(),"hidden fixture activated");ensure!(fs::read(&path)?==wave,"fixture contents changed");fs::remove_file(path)?;println!("{report}");Ok(())
    }
    #[test]#[ignore="Copies ZSDESK_MEDIA_TEST_VIDEO into the owned fixture, decodes and seeks muted on a private desktop"]
    fn native_media_preview_supplied_video_decodes_seeks_and_releases_copy()->Result<()>{
        use std::{fs,ffi::c_void,io::Read,thread};use anyhow::ensure;use sha2::{Digest,Sha256};
        use windows::Win32::System::Ole::{OleInitialize,OleUninitialize};
        #[link(name="user32")]unsafe extern "system"{fn CreateDesktopW(name:*const u16,device:*const u16,mode:*const c_void,flags:u32,access:u32,attributes:*const c_void)->*mut c_void;fn SetThreadDesktop(desktop:*mut c_void)->i32;fn CloseDesktop(desktop:*mut c_void)->i32;}
        fn digest(path:&Path)->Result<String>{let mut file=fs::File::open(path)?;let mut hash=Sha256::new();let mut block=[0u8;65536];loop{let size=file.read(&mut block)?;if size==0{break;}hash.update(&block[..size]);}Ok(format!("{:x}",hash.finalize()))}
        let source=std::path::PathBuf::from(std::env::var_os("ZSDESK_MEDIA_TEST_VIDEO").context("explicit video fixture required")?);
        ensure!(source.is_absolute()&&source.is_file()&&supported(&source),"explicit media fixture is invalid");
        let before=source.metadata()?;let source_hash=digest(&source)?;
        let root=std::path::PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("isolated output")?).join("media-preview");ensure!(root.is_absolute(),"absolute output required");fs::create_dir_all(&root)?;
        let owned=root.join(format!("supplied-video-{}.mp4",std::process::id()));let released=owned.with_extension("released.mp4");
        ensure!(!owned.exists()&&!released.exists()&&source!=owned,"owned fixture path already exists");
        struct Files(Vec<std::path::PathBuf>);impl Drop for Files{fn drop(&mut self){for path in &self.0{if path.exists(){let _=fs::remove_file(path);}}}}let _files=Files(vec![owned.clone(),released.clone()]);
        fs::copy(&source,&owned)?;ensure!(digest(&owned)?==source_hash,"fixture copy differs from source");
        let expected_size=std::env::var("ZSDESK_MEDIA_TEST_WIDTH").ok().zip(std::env::var("ZSDESK_MEDIA_TEST_HEIGHT").ok())
            .map(|(width,height)|Ok::<_,anyhow::Error>((width.parse::<u32>()?,height.parse::<u32>()?))).transpose()?;
        let foreground=unsafe{GetForegroundWindow()};let name=wide(&format!("ZSDesk.VideoPreview.{}",std::process::id()));let desktop=unsafe{CreateDesktopW(name.as_ptr(),std::ptr::null(),std::ptr::null(),0,0x000f00ff,std::ptr::null())};ensure!(!desktop.is_null(),"private desktop creation");
        let raw=desktop as usize;let file=owned.clone();
        let result=thread::spawn(move||->Result<serde_json::Value>{
            ensure!(unsafe{SetThreadDesktop(raw as*mut c_void)}!=0,"private desktop attach");unsafe{OleInitialize(None)}?;struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}let _apartment=Apartment;
            struct Owner(HWND);impl Drop for Owner{fn drop(&mut self){let _=unsafe{DestroyWindow(self.0)};}}
            let owner=Owner(unsafe{CreateWindowExW(WS_EX_NOACTIVATE,windows::core::w!("STATIC"),PCWSTR::null(),WS_POPUP,20,20,800,520,None,None,None,None)}?);
            let media=MediaPreview::open(owner.0,&file,RECT{left:0,top:0,right:800,bottom:520},true,true)?;
            let pump=|ms:u64|{let until=Instant::now()+Duration::from_millis(ms);while Instant::now()<until{let mut msg=MSG::default();while unsafe{PeekMessageW(&mut msg,None,0,0,PM_REMOVE)}.as_bool(){if msg.message!=WM_QUIT{unsafe{let _=TranslateMessage(&msg);DispatchMessageW(&msg);}}}thread::sleep(Duration::from_millis(2));}};
            let deadline=Instant::now()+Duration::from_secs(15);while !media.ready(){pump(20);ensure!(Instant::now()<deadline,"video did not decode: {}",media.state.notice.borrow());}
            let engine=media.state.engine().context("engine")?;ensure!(unsafe{engine.HasVideo()}.as_bool(),"supplied video was treated as audio-only");
            ensure!(unsafe{engine.IsPaused()}.as_bool()&&unsafe{engine.GetMuted()}.as_bool(),"video preview played or enabled audio without input");
            let(mut width,mut height)=(0,0);unsafe{engine.GetNativeVideoSize(Some(&mut width),Some(&mut height))}?;
            ensure!(width>0&&height>0,"decoded video has no dimensions");if let Some(expected)=expected_size{ensure!((width,height)==expected,"native dimensions differ from independent probe: {width}x{height}");}
            let duration=unsafe{engine.GetDuration()};ensure!(duration.is_finite()&&duration>0.5,"video duration invalid");
            let target=duration/2.0;unsafe{engine.SetCurrentTime(target)}?;let deadline=Instant::now()+Duration::from_secs(8);
            loop{pump(20);if !unsafe{engine.IsSeeking()}.as_bool()&&(unsafe{engine.GetCurrentTime()}-target).abs()<0.25{break;}ensure!(Instant::now()<deadline,"paused video seek did not complete");}
            ensure!(unsafe{engine.IsPaused()}.as_bool(),"seek started playback");
            let ready_state=unsafe{engine.GetReadyState()};ensure!(i32::from(ready_state)>=MF_MEDIA_ENGINE_READY_HAVE_CURRENT_DATA.0,"seek has no current video data");
            media.state.layout();let position=unsafe{engine.GetCurrentTime()};drop(engine);drop(media);pump(50);
            let released=file.with_extension("released.mp4");fs::rename(&file,&released)?;fs::rename(&released,&file)?;
            Ok(serde_json::json!({"width":width,"height":height,"duration_seconds":duration,"paused_seek_seconds":position,"current_frame_data_ready":true,"muted":true,"remained_paused":true,"file_released":true}))
        }).join().map_err(|_|anyhow::anyhow!("video fixture thread panicked")).and_then(|value|value);
        let closed=unsafe{CloseDesktop(desktop)}!=0;let mut foreground_pid=0;unsafe{GetWindowThreadProcessId(GetForegroundWindow(),Some(&mut foreground_pid));}
        let after=source.metadata()?;let unchanged=before.len()==after.len()&&before.modified()?==after.modified()?&&source_hash==digest(&source)?;
        let copy_unchanged=digest(&owned)?==source_hash;
        let report=serde_json::json!({"state":if result.is_ok()&&closed&&unchanged&&copy_unchanged&&foreground_pid!=std::process::id(){"passed"}else{"failed"},
            "checks":result.as_ref().map_err(|error|format!("{error:#}")),"source":source,"source_sha256":source_hash,"source_bytes":before.len(),"user_source_unchanged":unchanged,"owned_copy_unchanged":copy_unchanged,
            "foreground_unchanged":foreground==unsafe{GetForegroundWindow()},"private_desktop_closed":closed,"audio_played":false});
        fs::write(root.join("video-verification.current.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");result?;ensure!(closed&&unchanged&&copy_unchanged&&foreground_pid!=std::process::id(),"source/isolation/cleanup verification failed");Ok(())
    }
}
