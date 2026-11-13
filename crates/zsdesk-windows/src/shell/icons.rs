//! Shell icons and thumbnails.
//!
//! Tab, bookmark and address icons come from the system image list (same
//! icons Explorer shows). Desktop fences request icons/thumbnails from a
//! background STA thread through `IShellItemImageFactory`; completed bitmaps
//! remain in its queue and a generation-tagged message wakes the owner.

use std::{collections::{HashSet, VecDeque}, sync::{Arc, Condvar, Mutex, atomic::{AtomicUsize, Ordering}}, thread};

use windows::{
    core::{Interface, PCWSTR},
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, SIZE, WPARAM},
        Graphics::Gdi::{DeleteObject, GetObjectW, DIBSECTION, HBITMAP, BITMAP, BITMAPINFO, BITMAPINFOHEADER,
            CreateCompatibleDC,DeleteDC,GetDIBits,DIB_RGB_COLORS,BI_RGB},
        Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES,
        System::{Com::{CoCreateInstance,CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,CLSCTX_INPROC_SERVER}, Threading::GetCurrentThreadId},
        UI::{
            Controls::{IImageList, ILD_TRANSPARENT},
            Shell::{
                IShellItemImageFactory, SHGetFileInfoW, SHGetImageList, SHFILEINFOW, SHGFI_PIDL, SHGFI_SYSICONINDEX,
                IThumbnailCache,ISharedBitmap,LocalThumbnailCache,WTS_EXTRACT,WTS_ALPHATYPE,WTSAT_RGB,WTSAT_ARGB,WTSAT_UNKNOWN,
                SHIL_EXTRALARGE, SHIL_JUMBO, SHIL_LARGE, SHIL_SMALL, SIIGBF_BIGGERSIZEOK, SIIGBF_ICONONLY,
                SHGFI_USEFILEATTRIBUTES,
                DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass,
            },
            WindowsAndMessaging::{DestroyIcon, PostMessageW, HICON, WM_NCDESTROY},
        },
    },
};

use crate::gfx::BitmapAlpha;

/// System image list index for a PIDL.
pub fn icon_index(pidl: &[u8]) -> Option<i32> {
    let mut info = SHFILEINFOW::default();
    let ok = unsafe { SHGetFileInfoW(PCWSTR(pidl.as_ptr().cast()), FILE_FLAGS_AND_ATTRIBUTES(0), Some(&mut info),
        std::mem::size_of::<SHFILEINFOW>() as u32, SHGFI_PIDL | SHGFI_SYSICONINDEX) };
    (ok != 0).then_some(info.iIcon)
}

/// Icon index for a file name pattern without touching the disk.
pub fn icon_index_for_name(name: &str, directory: bool) -> Option<i32> {
    let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    let attrs = if directory { windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY } else { windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL };
    let mut info = SHFILEINFOW::default();
    let ok = unsafe { SHGetFileInfoW(PCWSTR(wide.as_ptr()), attrs, Some(&mut info),
        std::mem::size_of::<SHFILEINFOW>() as u32, SHGFI_SYSICONINDEX | SHGFI_USEFILEATTRIBUTES) };
    (ok != 0).then_some(info.iIcon)
}

/// Extracts an icon of at least `pixels` size from the system image list. Caller owns the HICON.
pub fn system_icon(index: i32, pixels: i32) -> Option<HICON> {
    let mut candidates = Vec::new();
    // SMALL/LARGE/EXTRALARGE already scale with DPI. Their nominal 16/32/48
    // names are not physical-pixel sizes on a high-DPI display.
    for list in [SHIL_SMALL, SHIL_LARGE, SHIL_EXTRALARGE, SHIL_JUMBO] {
        let Ok(images) = (unsafe { SHGetImageList::<IImageList>(list as i32) }) else { continue };
        let (mut width, mut height) = (0, 0);
        if unsafe { images.GetIconSize(&mut width, &mut height) }.is_err() { continue; }
        let Some(rank) = icon_size_rank(width, height, pixels) else { continue };
        if width == pixels && height == pixels {
            if let Ok(icon) = unsafe { images.GetIcon(index, ILD_TRANSPARENT.0) } {
                if !icon.is_invalid() { return Some(icon); }
            }
        } else { candidates.push((rank, images)); }
    }
    candidates.sort_by_key(|(rank, _)| *rank);
    candidates.into_iter().find_map(|(_, images)|
        unsafe { images.GetIcon(index, ILD_TRANSPARENT.0) }.ok().filter(|icon| !icon.is_invalid()))
}

fn icon_size_rank(width: i32, height: i32, requested: i32) -> Option<(bool, i32)> {
    let size = width.min(height);
    (size > 0).then(|| if size >= requested.max(1) { (false, size) } else { (true, -size) })
}

/// Owned icon handle.
pub struct OwnedIcon(pub HICON);
impl Drop for OwnedIcon { fn drop(&mut self) { let _ = unsafe { DestroyIcon(self.0) }; } }

pub fn pidl_icon(pidl: &[u8], pixels: i32) -> Option<OwnedIcon> {
    system_icon(icon_index(pidl)?, pixels).map(OwnedIcon)
}

/// A finished icon or thumbnail.
pub struct IconResult {
    pub key: String,
    pub size: u32,
    pub token: u64,
    pub bitmap: HBITMAP,
    pub alpha: BitmapAlpha,
    pub pixel_bytes: u64,
}

impl IconResult {
    /// Releases the bitmap if the result is discarded.
    pub fn discard(self) {}
}
impl Drop for IconResult {
    fn drop(&mut self) { if !self.bitmap.is_invalid() { let _ = unsafe { DeleteObject(self.bitmap.into()) }; } }
}
// The bitmap has one owner and is never selected into a DC on the loader thread.
// GDI bitmap handles may be transferred within a process; the receiver either
// materializes its WIC copy or drops this result before anyone else uses it.
unsafe impl Send for IconResult {}

pub struct IconRequest { pub key: String, pub pidl: Vec<u8>, pub size: u32, pub thumbnail: bool, pub token: u64 }

const MAX_PENDING: usize = 128;
const MAX_READY: usize = 16;
const MAX_READY_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Default)]
struct LoaderState {
    requests: VecDeque<IconRequest>,
    pending: HashSet<u64>,
    ready: VecDeque<IconResult>,
    ready_bytes: u64,
    stopping: bool,
}
impl LoaderState {
    fn enqueue(&mut self, request: IconRequest) -> bool {
        if self.stopping || self.pending.len() >= MAX_PENDING || !self.pending.insert(request.token) { return false; }
        self.requests.push_back(request);
        true
    }
    fn retain(&mut self, tokens: &HashSet<u64>) {
        self.pending.retain(|t| tokens.contains(t));
        self.requests.retain(|r| tokens.contains(&r.token));
        self.ready.retain(|r| tokens.contains(&r.token));
        self.ready_bytes = self.ready.iter().map(|r| r.pixel_bytes).sum();
    }
    fn stop(&mut self) {
        self.stopping = true;
        self.requests.clear(); self.pending.clear(); self.ready.clear(); self.ready_bytes = 0;
    }
    fn complete(&mut self, result: IconResult) -> bool {
        if self.stopping || !self.pending.contains(&result.token) { return false; }
        let post = self.ready.is_empty();
        self.ready_bytes = self.ready_bytes.saturating_add(result.pixel_bytes);
        self.ready.push_back(result);
        post
    }
}

#[derive(Clone, Copy)]
struct NotifyTarget { window: isize, message: u32, token: usize }
struct Notification {
    target: Mutex<Option<NotifyTarget>>,
    window: isize,
    owner_thread: u32,
    token: usize,
}
impl Notification {
    fn with_target<T>(&self, post: impl FnOnce(NotifyTarget) -> T) -> Option<T> {
        // WM_NCDESTROY and shutdown take the same short lock. No Shell/COM call
        // runs under it, and PostMessage does not wait for the target window.
        let target = self.target.lock().unwrap_or_else(|p| p.into_inner());
        target.map(post)
    }
    fn post(&self) -> bool {
        self.with_target(|target| unsafe { PostMessageW(Some(HWND(target.window as *mut _)), target.message, WPARAM(target.token), LPARAM(0)) }.is_ok()).unwrap_or(false)
    }
    fn revoke(&self) { *self.target.lock().unwrap_or_else(|p| p.into_inner()) = None; }
    fn detach(&self) {
        self.revoke();
        if self.owner_thread == unsafe { GetCurrentThreadId() }
            && unsafe { RemoveWindowSubclass(HWND(self.window as *mut _), Some(notification_proc), self.token) }.as_bool() {
            // The subclass owns exactly one Arc reference until removal/NCDESTROY.
            unsafe { drop(Arc::from_raw(self as *const Notification)); }
        }
    }
}

unsafe extern "system" fn notification_proc(hwnd: HWND, message: u32, wp: WPARAM, lp: LPARAM, id: usize, data: usize) -> LRESULT {
    if message == WM_NCDESTROY {
        let notification = unsafe { Arc::from_raw(data as *const Notification) };
        notification.revoke();
        let _ = unsafe { RemoveWindowSubclass(hwnd, Some(notification_proc), id) };
        return unsafe { DefSubclassProc(hwnd, message, wp, lp) };
    }
    unsafe { DefSubclassProc(hwnd, message, wp, lp) }
}

struct WorkerExit {
    shared: Arc<(Mutex<LoaderState>, Condvar)>, notification: Arc<Notification>, initialized: bool,
}
impl Drop for WorkerExit {
    fn drop(&mut self) {
        self.notification.revoke();
        self.shared.0.lock().unwrap_or_else(|p| p.into_inner()).stop();
        self.shared.1.notify_all();
        if self.initialized { unsafe { CoUninitialize() }; }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct IconLoaderStats { pub pending: usize, pub queued: usize, pub ready: usize, pub ready_pixel_bytes: u64 }

/// A bounded, cancellable queue. Window messages only wake the owner; this
/// object owns every completed bitmap, including results not consumed at exit.
pub struct IconLoader { shared: Arc<(Mutex<LoaderState>, Condvar)>, notification: Arc<Notification>, thread: Option<thread::JoinHandle<()>> }

impl IconLoader {
    /// WPARAM carries this loader's token; there is no bitmap/pointer payload.
    /// Validate it with `accepts_notification` before draining the result queue.
    pub fn spawn(hwnd: HWND, message: u32) -> Self {
        let shared = Arc::new((Mutex::new(LoaderState::default()), Condvar::new()));
        static NEXT_TOKEN: AtomicUsize = AtomicUsize::new(1);
        let token = loop { let value = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed); if value != 0 { break value; } };
        let notification = Arc::new(Notification { target: Mutex::new(Some(NotifyTarget { window: hwnd.0 as isize, message, token })),
            window: hwnd.0 as isize, owner_thread: unsafe { GetCurrentThreadId() }, token });
        let subclass = Arc::into_raw(notification.clone());
        if !unsafe { SetWindowSubclass(hwnd, Some(notification_proc), token, subclass as usize) }.as_bool() {
            unsafe { drop(Arc::from_raw(subclass)); }
            notification.revoke();
            shared.0.lock().unwrap_or_else(|p| p.into_inner()).stop();
            return Self { shared, notification, thread: None };
        }
        let worker = shared.clone();
        let notify = notification.clone();
        let thread = thread::Builder::new().name("zsdesk-icons".into()).spawn(move || {
            let initialized = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) }.is_ok();
            let _exit = WorkerExit { shared: worker.clone(), notification: notify.clone(), initialized };
            let (lock, wake) = &*worker;
            loop {
                let request = {
                    let mut state = lock.lock().unwrap_or_else(|p| p.into_inner());
                    while !state.stopping && state.requests.is_empty() { state = wake.wait(state).unwrap_or_else(|p| p.into_inner()); }
                    if state.stopping { break; }
                    state.requests.pop_front().unwrap()
                };
                let loaded = initialized.then(|| load(&request)).flatten();
                let (bitmap, alpha) = loaded.unwrap_or((HBITMAP::default(), BitmapAlpha::Ignore));
                let result = IconResult { key: request.key, size: request.size, token: request.token,
                    bitmap, alpha, pixel_bytes: bitmap_bytes(bitmap) };
                let should_post = {
                    let mut state = lock.lock().unwrap_or_else(|p| p.into_inner());
                    while !state.stopping && state.pending.contains(&result.token)
                        && (state.ready.len() >= MAX_READY || (!state.ready.is_empty() && state.ready_bytes.saturating_add(result.pixel_bytes) > MAX_READY_BYTES)) {
                        state = wake.wait(state).unwrap_or_else(|p| p.into_inner());
                    }
                    if state.stopping { break; }
                    if !state.pending.contains(&result.token) { continue; }
                    state.complete(result)
                };
                if should_post && !notify.post() {
                    lock.lock().unwrap_or_else(|p| p.into_inner()).stop();
                    wake.notify_all();
                    break;
                }
            }
        }).ok();
        if thread.is_none() { notification.detach();shared.0.lock().unwrap_or_else(|p| p.into_inner()).stop(); }
        Self { shared, notification, thread }
    }

    pub fn accepts_notification(&self, token: usize) -> bool {
        token == self.notification.token && !self.shared.0.lock().unwrap_or_else(|p| p.into_inner()).stopping
    }

    /// False means the bounded queue is full; retry on the next repaint.
    pub fn request(&self, request: IconRequest) -> bool {
        let accepted = self.shared.0.lock().unwrap_or_else(|p| p.into_inner()).enqueue(request);
        if accepted { self.shared.1.notify_one(); }
        accepted
    }

    pub fn take_ready(&self) -> Vec<IconResult> {
        let mut state = self.shared.0.lock().unwrap_or_else(|p| p.into_inner());
        let ready: Vec<_> = state.ready.drain(..).collect();
        for result in &ready { state.pending.remove(&result.token); }
        state.ready_bytes = 0;
        self.shared.1.notify_all();
        ready
    }

    /// Cancels queued/stale work without interrupting a Shell extension mid-call.
    pub fn retain_requests(&self, tokens: &HashSet<u64>) {
        self.shared.0.lock().unwrap_or_else(|p| p.into_inner()).retain(tokens);
        self.shared.1.notify_all();
    }

    pub fn stats(&self) -> IconLoaderStats {
        let state = self.shared.0.lock().unwrap_or_else(|p| p.into_inner());
        IconLoaderStats { pending: state.pending.len(), queued: state.requests.len(), ready: state.ready.len(), ready_pixel_bytes: state.ready_bytes }
    }

    pub fn shutdown(&mut self) {
        self.notification.detach();
        self.shared.0.lock().unwrap_or_else(|p| p.into_inner()).stop();
        self.shared.1.notify_all();
        // An in-flight third-party extractor may never return. Detach it after
        // revoking notifications; it owns only its stopped queue and COM scope.
        if let Some(thread) = self.thread.take() { if thread.is_finished() { let _ = thread.join(); } }
    }
}

impl Drop for IconLoader {
    fn drop(&mut self) { self.shutdown(); }
}

fn bitmap_bytes(bitmap: HBITMAP) -> u64 {
    if bitmap.is_invalid() { return 0; }
    let mut value = windows::Win32::Graphics::Gdi::BITMAP::default();
    let read = unsafe { GetObjectW(bitmap.into(), std::mem::size_of_val(&value) as i32, Some((&mut value as *mut windows::Win32::Graphics::Gdi::BITMAP).cast())) };
    if read <= 0 { 0 } else { u64::from(value.bmWidthBytes.unsigned_abs()) * u64::from(value.bmHeight.unsigned_abs()) }
}

fn load(request: &IconRequest) -> Option<(HBITMAP, BitmapAlpha)> {
    let item = super::pidl::item_from_pidl(&request.pidl).ok()?;
    let requested=request.size.clamp(1,1024);
    if request.thumbnail {
        // GetImage uses this same Shell cache but discards the alpha contract.
        // All-zero pixels can mean either transparent ARGB or opaque black RGB;
        // preserve the provider's declared type instead of guessing from color.
        if let Ok(cache)=unsafe{CoCreateInstance::<_,IThumbnailCache>(&LocalThumbnailCache,None,CLSCTX_INPROC_SERVER)}{
            let mut shared=None;
            if unsafe{cache.GetThumbnail(&item,requested,WTS_EXTRACT,Some(&mut shared),None,None)}.is_ok(){
                if let Some(shared)=shared{if let Some(result)=detach_thumbnail(&shared){return Some(result);}}
            }
        }
    }
    let factory: IShellItemImageFactory = item.cast().ok()?;
    let size = SIZE { cx: requested as i32, cy: requested as i32 };
    // Extraction failure falls back to the associated icon, not a second
    // thumbnail extraction that would lose its alpha declaration again.
    let flags = SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK;
    let bitmap = unsafe { factory.GetImage(size, flags).ok()? };
    Some((bitmap, classify_alpha(bitmap,WTSAT_UNKNOWN)))
}

fn detach_thumbnail(shared:&ISharedBitmap)->Option<(HBITMAP,BitmapAlpha)>{
    let kind=unsafe{shared.GetFormat()}.ok()?;
    let bitmap=unsafe{shared.Detach()}.ok()?;
    Some((bitmap,classify_alpha(bitmap,kind)))
}

/// Respect an explicit no-alpha/alpha contract, including fully transparent
/// images. Pixel inspection is used only to distinguish straight from premultiplied data.
fn classify_alpha(bitmap: HBITMAP,kind:WTS_ALPHATYPE) -> BitmapAlpha {
    if kind==WTSAT_RGB{return BitmapAlpha::Ignore;}
    let alpha_present=kind==WTSAT_ARGB;
    let fallback=if alpha_present{BitmapAlpha::Premultiplied}else{BitmapAlpha::Ignore};
    let mut section = DIBSECTION::default();
    let read = unsafe { GetObjectW(bitmap.into(), std::mem::size_of::<DIBSECTION>() as i32, Some((&mut section as *mut DIBSECTION).cast())) };
    if (read as usize)<std::mem::size_of::<BITMAP>()||section.dsBm.bmBitsPixel!=32{return fallback;}
    let width=section.dsBm.bmWidth;let height=section.dsBm.bmHeight.unsigned_abs();
    if width<=0||height==0{return fallback;}
    let row_bytes=(width as usize).checked_mul(4);let length=row_bytes.and_then(|row|row.checked_mul(height as usize));
    let Some(length)=length.filter(|length|*length<=MAX_READY_BYTES as usize)else{return fallback;};
    if read as usize==std::mem::size_of::<DIBSECTION>()&&!section.dsBm.bmBits.is_null()
        &&section.dsBm.bmWidthBytes as usize==row_bytes.unwrap(){
        let pixels=unsafe{std::slice::from_raw_parts(section.dsBm.bmBits.cast::<u8>(),length)};
        return classify_pixels(pixels,alpha_present);
    }
    // A DDB's unused high byte is not an alpha declaration. In particular,
    // legacy RGB bitmaps can contain arbitrary values there.
    if !alpha_present{return BitmapAlpha::Ignore;}
    // DDBs do not expose bmBits. Read them through GDI before choosing WIC's
    // alpha mode; absence of a directly mapped DIB does not mean opacity.
    let dc=unsafe{CreateCompatibleDC(None)};if dc.is_invalid(){return fallback;}
    let mut info=BITMAPINFO{bmiHeader:BITMAPINFOHEADER{biSize:std::mem::size_of::<BITMAPINFOHEADER>()as u32,
        biWidth:width,biHeight:-(height as i32),biPlanes:1,biBitCount:32,biCompression:BI_RGB.0,..Default::default()},..Default::default()};
    let mut pixels=vec![0u8;length];
    let rows=unsafe{GetDIBits(dc,bitmap,0,height,Some(pixels.as_mut_ptr().cast()),&mut info,DIB_RGB_COLORS)};
    let _=unsafe{DeleteDC(dc)};
    if rows==height as i32{classify_pixels(&pixels,alpha_present)}else{fallback}
}

fn classify_pixels(pixels:&[u8],alpha_present:bool)->BitmapAlpha{
    let mut any_alpha = alpha_present;
    for px in pixels.chunks_exact(4) {
        let a = px[3];
        if a != 0 { any_alpha = true; }
        if px[0] > a || px[1] > a || px[2] > a { return if any_alpha || a != 0 { BitmapAlpha::Straight } else { scan_rest(pixels) }; }
    }
    if any_alpha { BitmapAlpha::Premultiplied } else { BitmapAlpha::Ignore }
}

fn scan_rest(pixels: &[u8]) -> BitmapAlpha {
    if pixels.chunks_exact(4).any(|p| p[3] != 0) { BitmapAlpha::Straight } else { BitmapAlpha::Ignore }
}

#[cfg(test)]#[path="icons_alpha_tests.rs"]mod alpha_tests;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn icon_list_choice_uses_scaled_dimensions() {
        // At 150%, the small system list is already the exact 24px target.
        assert!(icon_size_rank(24,24,24) < icon_size_rank(48,48,24));
        assert!(icon_size_rank(32,32,20) < icon_size_rank(16,16,20));
        assert!(icon_size_rank(256,256,300) < icon_size_rank(48,48,300));
        assert_eq!(icon_size_rank(0,0,16), None);
    }
    fn request(token: u64) -> IconRequest { IconRequest { key: "same-file".into(), pidl: vec![0,0], size: 48, thumbnail: true, token } }
    #[test]
    fn pending_queue_is_bounded_and_cancellation_frees_capacity() {
        let mut state = LoaderState::default();
        for token in 1..=MAX_PENDING as u64 { assert!(state.enqueue(request(token))); }
        assert!(!state.enqueue(request(999)));
        state.retain(&HashSet::from([1, 2]));
        assert_eq!(state.requests.len(), 2);
        assert!(state.enqueue(request(999)));
    }
    #[test]
    fn cached_file_can_be_requested_again_after_completion_or_cancellation() {
        let mut state = LoaderState::default();
        assert!(state.enqueue(request(1)));
        state.retain(&HashSet::new());
        assert!(state.enqueue(request(2)));
        state.stop();
        assert!(state.pending.is_empty() && state.requests.is_empty());
        assert!(!state.enqueue(request(3)));
    }
    fn notification() -> Arc<Notification> {
        Arc::new(Notification { target: Mutex::new(Some(NotifyTarget { window: 7, message: 9, token: 11 })),
            window: 0, owner_thread: 0, token: 11 })
    }
    #[test]
    fn shutdown_revokes_late_wakes_and_discards_completions() {
        let notify = notification();
        let mut state = LoaderState::default();
        assert!(state.enqueue(request(1)));
        let in_flight = state.requests.pop_front().unwrap();
        notify.revoke();state.stop();
        assert!(notify.with_target(|_| panic!("late HWND notification")).is_none());
        assert!(!state.complete(IconResult { key: in_flight.key, size: 48, token: 1, bitmap: HBITMAP::default(),
            alpha: BitmapAlpha::Ignore, pixel_bytes: 0 }));
        assert!(state.ready.is_empty() && state.pending.is_empty());
    }
    #[test]
    fn worker_unwind_stops_its_queue_without_a_window_or_shell_call() {
        let shared = Arc::new((Mutex::new(LoaderState::default()), Condvar::new()));
        let notify = notification();
        shared.0.lock().unwrap().enqueue(request(1));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _exit = WorkerExit { shared: shared.clone(), notification: notify.clone(), initialized: false };
            panic!("simulated extractor failure");
        }));
        assert!(result.is_err());
        let state = shared.0.lock().unwrap();
        assert!(state.stopping && state.requests.is_empty() && state.pending.is_empty());
        assert!(notify.with_target(|_| ()).is_none());
    }
    #[test]
    fn shutdown_does_not_wait_for_an_in_flight_extractor() {
        use std::{sync::mpsc, time::Duration};
        let (release, waiting) = mpsc::channel();
        let (started, ready) = mpsc::channel();
        let (finished, exited) = mpsc::channel();
        let worker = thread::spawn(move || { started.send(()).unwrap(); let _ = waiting.recv(); let _ = finished.send(()); });
        ready.recv_timeout(Duration::from_secs(2)).unwrap();
        let mut loader = IconLoader { shared: Arc::new((Mutex::new(LoaderState::default()), Condvar::new())),
            notification: notification(), thread: Some(worker) };
        let (stopped, received) = mpsc::channel();
        let shutdown = thread::spawn(move || { loader.shutdown(); stopped.send(()).unwrap(); });
        let completed_without_release = received.recv_timeout(Duration::from_secs(2)).is_ok();
        let _ = release.send(());
        shutdown.join().unwrap();
        exited.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(completed_without_release);
    }
}
