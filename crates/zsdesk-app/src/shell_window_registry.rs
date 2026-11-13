//! Asynchronous, per-window Shell registration of the visible folder panes.
//! COM objects and cookies belong to one message-pumping STA. The UI exchanges
//! owned snapshots and bounded relative-PIDL selection requests only.

use std::{collections::{BTreeMap, VecDeque}, ffi::c_void, path::{Path, PathBuf},
    sync::{Arc, Mutex, atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering}}, thread::{self, JoinHandle},
    time::{Duration, Instant}, os::windows::ffi::OsStrExt};
use windows::{core::{implement, w, BSTR, BOOL, Error, GUID, HRESULT, IUnknownImpl, Interface, PCWSTR, Ref, Result},
    Win32::{Foundation::{CloseHandle, HANDLE, HWND, LPARAM, LRESULT, RECT, SHANDLE_PTR, VARIANT_BOOL, WPARAM},
        System::{Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, CoEnableCallCancellation,
            CoDisableCallCancellation, CoCancelCall, CoTaskMemFree, CLSCTX_LOCAL_SERVER,
            COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, IDispatch, IDispatch_Impl,
            IServiceProvider, IServiceProvider_Impl, ITypeInfo, IStream, DISPATCH_FLAGS, DISPPARAMS, EXCEPINFO},
            Diagnostics::Debug::ReadProcessMemory,
            Ole::{IOleWindow_Impl, OLEMENUGROUPWIDTHS, DispGetIDsOfNames, DispInvoke, LoadRegTypeLib},
            Threading::{CreateEventW, GetCurrentProcess, GetCurrentThreadId, SetEvent, WaitForSingleObject},
            Variant::{InitVariantFromBuffer, VARIANT}},
        UI::{Controls::{LPFNSVADDPROPSHEETPAGE, TBBUTTON},
            Shell::{Common::ITEMIDLIST, IShellWindows, ShellWindows, SWC_BROWSER,
                IWebBrowserApp, IWebBrowserApp_Impl, IWebBrowser_Impl,
                IShellBrowser, IShellBrowser_Impl, IShellView, IShellView_Impl, IFolderView,
                FOLDERSETTINGS, _SVGIO, SID_STopLevelBrowser, ILCreateFromPathW,
                SHParseDisplayName, SHGetKnownFolderIDList, FOLDERID_ComputerFolder},
            WindowsAndMessaging::*}}};
use zsdesk_core::model::explorer::ShellLocation;

pub const WM_SELECT: u32 = WM_APP + 0x3A0;
const MAX_PANES: usize = 2;
const MAX_PIDL_BYTES: usize = 64 * 1024;
const MAX_REQUESTS: usize = 256;
const MAX_REQUEST_BYTES: usize = 1024 * 1024;
const MAX_WORKERS: usize = 16;
static ACTIVE_WORKERS: AtomicUsize = AtomicUsize::new(0);
const SELECT_FLAGS: u32 = 0x1 | 0x4 | 0x8 | 0x10; // SELECT, DESELECTOTHERS, FOCUSED, ENSUREVISIBLE
const E_FAIL: HRESULT = HRESULT(0x80004005u32 as i32);
const E_INVALIDARG: HRESULT = HRESULT(0x80070057u32 as i32);
const E_NOINTERFACE: HRESULT = HRESULT(0x80004002u32 as i32);
const E_NOTIMPL: HRESULT = HRESULT(0x80004001u32 as i32);

struct WorkerPermit;
impl WorkerPermit {
    fn acquire() -> Result<Self> {
        ACTIVE_WORKERS.fetch_update(Ordering::AcqRel, Ordering::Acquire,
            |count| (count < MAX_WORKERS).then_some(count + 1))
            .map(|_| Self).map_err(|_| Error::new(HRESULT(0x800700AAu32 as i32),
                "Shell 窗口登记线程已达到上限；此窗口尚未登记"))
    }
}
impl Drop for WorkerPermit { fn drop(&mut self) { ACTIVE_WORKERS.fetch_sub(1, Ordering::AcqRel); } }

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Identity { pub source: usize, pub tab: u64, pub epoch: u64 }

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry { pub identity: Identity, pub location: ShellLocation }

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectRequest {
    pub identity: Identity,
    pub location: ShellLocation,
    /// Relative to the registered folder; None is a clear-selection request.
    pub child_pidl: Option<Vec<u8>>,
    /// Selection-only flags; external requests never start an edit operation.
    pub flags: u32,
}

impl SelectRequest {
    pub fn matches(&self, entry: &Entry) -> bool {
        self.identity == entry.identity && self.location == entry.location
    }
}

#[derive(Default)]
struct SharedState {
    wanted: Vec<Entry>, revision: u64, generation: u64,
    requests: VecDeque<SelectRequest>, request_bytes: usize,
    last_error: Option<String>,
}
struct Shared {
    owner: isize, wake: isize, stopped: AtomicBool, thread_id: AtomicU32,
    state: Mutex<SharedState>, cancellation: Mutex<()>,
    #[cfg(test)] probe_shell:Mutex<Option<(isize,u32)>>,
}
impl Shared {
    fn signal(&self) { if self.wake != 0 { let _ = unsafe { SetEvent(HANDLE(self.wake as *mut _)) }; } }
    fn error(&self, error: &Error) {
        if let Ok(mut state) = self.state.lock() { state.last_error = Some(error.to_string()); }
    }
    fn post(&self, request: SelectRequest, generation: u64) -> Result<()> {
        if self.stopped.load(Ordering::Acquire) { return Err(Error::from_hresult(E_FAIL)); }
        let mut state = self.state.lock().map_err(|_| Error::from_hresult(E_FAIL))?;
        if self.stopped.load(Ordering::Acquire) || generation != state.generation
            || !state.wanted.iter().any(|entry| request.matches(entry)) {
            return Err(Error::from_hresult(E_FAIL));
        }
        if state.requests.back() == Some(&request) { return Ok(()); }
        let bytes = request.child_pidl.as_ref().map_or(0, Vec::len);
        if state.requests.len() >= MAX_REQUESTS || state.request_bytes + bytes > MAX_REQUEST_BYTES {
            return Err(Error::from_hresult(HRESULT(0x800700AAu32 as i32)));
        }
        state.request_bytes += bytes;
        state.requests.push_back(request);
        // The message carries no pointer. Only this owned queue is trusted.
        if let Err(error) = unsafe { PostMessageW(Some(HWND(self.owner as *mut _)), WM_SELECT, WPARAM(0), LPARAM(0)) } {
            state.requests.pop_back(); state.request_bytes -= bytes; return Err(error);
        }
        Ok(())
    }
}
impl Drop for Shared {
    fn drop(&mut self) { if self.wake != 0 { let _ = unsafe { CloseHandle(HANDLE(self.wake as *mut _)) }; } }
}

pub struct Registry { shared: Arc<Shared>, worker: Option<JoinHandle<()>> }
impl Registry {
    #[cfg(test)]
    pub(crate) fn new_for_fixture(owner:HWND,default_shell:(isize,u32))->Result<Self>{
        let window=HWND(default_shell.0 as*mut _);let mut pid=0;
        unsafe{GetWindowThreadProcessId(window,Some(&mut pid));}
        if default_shell.0==0||pid!=default_shell.1||!unsafe{IsWindow(Some(window))}.as_bool(){return Err(Error::from_hresult(E_INVALIDARG));}
        let registry=Self::new(owner)?;
        *registry.shared.probe_shell.lock().unwrap_or_else(|error|error.into_inner())=Some(default_shell);
        registry.shared.signal();Ok(registry)
    }
    #[cfg(test)]pub(crate) fn fixture_worker_finished(&self)->bool{self.worker.as_ref().is_none_or(JoinHandle::is_finished)}
    /// Call only for an opted-in real file window. This module does not change
    /// takeover preferences, registry associations, or other applications.
    pub fn new(owner: HWND) -> Result<Self> {
        if !unsafe { IsWindow(Some(owner)) }.as_bool() { return Err(Error::from_hresult(E_INVALIDARG)); }
        let permit = WorkerPermit::acquire()?;
        let wake = unsafe { CreateEventW(None, false, false, None)? };
        let shared = Arc::new(Shared { owner: owner.0 as isize, wake: wake.0 as isize,
            stopped: AtomicBool::new(false), thread_id: AtomicU32::new(0), state: Mutex::new(SharedState::default()), cancellation: Mutex::new(()),
            #[cfg(test)] probe_shell:Mutex::new(None) });
        let worker_shared = shared.clone();
        let worker = thread::Builder::new().name("zsdesk-shell-windows".into()).spawn(move || {
            // Admission remains occupied until this thread actually exits,
            // including a noncancellable Shell namespace parse.
            let _permit = permit; worker_main(worker_shared);
        })
            .map_err(|error| Error::new(E_FAIL, error.to_string()))?;
        Ok(Self { shared, worker: Some(worker) })
    }

    /// Replaces the desired active panes. Unchanged snapshots cause no work;
    /// stale callbacks stop being eligible as soon as this method returns.
    pub fn publish(&self, wanted: Vec<Entry>) {
        if self.shared.stopped.load(Ordering::Acquire) { return; }
        let wanted = normalize_entries(wanted);
        if let Ok(mut state) = self.shared.state.lock() {
            if state.wanted == wanted { return; }
            state.requests.retain(|request| wanted.iter().any(|entry| request.matches(entry)));
            state.request_bytes = state.requests.iter().filter_map(|request| request.child_pidl.as_ref()).map(Vec::len).sum();
            state.wanted = wanted;
            state.revision = state.revision.wrapping_add(1);
        }
        self.shared.signal();
    }

    /// Invalidates old leases immediately; the STA reconnects and republishes.
    pub fn restart(&self) {
        if let Ok(mut state) = self.shared.state.lock() {
            state.generation = state.generation.wrapping_add(1);
            state.revision = state.revision.wrapping_add(1);
            state.requests.clear(); state.request_bytes = 0;
        }
        self.shared.signal();
    }

    pub fn take_requests(&self) -> Vec<SelectRequest> {
        let Ok(mut state) = self.shared.state.lock() else { return Vec::new() };
        let requests = state.requests.drain(..).collect::<Vec<_>>();
        state.request_bytes = 0;
        if self.shared.stopped.load(Ordering::Acquire) { return Vec::new(); }
        requests.into_iter().filter(|request| state.wanted.iter().any(|entry| request.matches(entry))).collect()
    }

    pub fn take_error(&self) -> Option<String> {
        self.shared.state.lock().ok().and_then(|mut state| state.last_error.take())
    }

    /// Does not wait for an unresponsive Explorer. A running worker keeps its
    /// own state and its worker permit until it actually exits. COM cancellation
    /// is best effort; namespace parsing may not support cancellation.
    pub fn stop(&mut self) {
        if !self.shared.stopped.swap(true, Ordering::AcqRel) {
            if let Ok(mut state) = self.shared.state.lock() {
                state.wanted.clear(); state.requests.clear(); state.request_bytes = 0;
                state.generation = state.generation.wrapping_add(1);
            }
            self.shared.signal();
            if !self.worker.as_ref().is_some_and(JoinHandle::is_finished) {
                // The worker's exit guard holds this same short-lived lock
                // while clearing its ID, preventing cancellation of a reused ID.
                if let Ok(_cancel) = self.shared.cancellation.lock() {
                    let thread = self.shared.thread_id.load(Ordering::Acquire);
                    if thread != 0 { let _ = unsafe { CoCancelCall(thread, 0) }; }
                }
            }
        }
        if self.worker.as_ref().is_some_and(JoinHandle::is_finished) {
            if let Some(worker) = self.worker.take() { let _ = worker.join(); }
        }
    }
}
impl Drop for Registry { fn drop(&mut self) { self.stop(); } }

#[cfg(test)]pub(crate) fn fixture_window_snapshot()->anyhow::Result<Vec<(isize,u32)>>{tests::probe_windows()}

/// Test-only recovery for an API which may start Explorer on lookup failure.
/// Existing windows on both desktops remain protected by the captured set.
#[cfg(test)]pub(crate) struct RevealFixtureScope{before:Vec<(isize,u32)>,folders:Vec<PathBuf>}
#[cfg(test)]impl RevealFixtureScope{
    pub(crate) fn new(before:Vec<(isize,u32)>,folders:Vec<PathBuf>)->Self{Self{before,folders}}
}
#[cfg(test)]impl Drop for RevealFixtureScope{fn drop(&mut self){
    use windows::Win32::UI::Shell::{IWebBrowser2,IShellFolderViewDual,Folder2};
    let Ok(shell)=(unsafe{CoCreateInstance::<_,IShellWindows>(&ShellWindows,None,CLSCTX_LOCAL_SERVER)})else{return;};
    let browsers=(0..unsafe{shell.Count()}.unwrap_or(0).clamp(0,128)).filter_map(|index|{
        let browser=unsafe{shell.Item(&VARIANT::from(index))}.ok()?.cast::<IWebBrowser2>().ok()?;
        let handle=unsafe{browser.HWND()}.ok()?;Some((HWND(handle.0 as*mut _),browser))
    }).collect::<Vec<_>>();
    for(window,browser)in &browsers{
        let mut pid=0;unsafe{GetWindowThreadProcessId(*window,Some(&mut pid));}
        if self.before.contains(&(window.0 as isize,pid))||browsers.iter().filter(|(other,_)|other==window).count()!=1{continue;}
        let mut class=[0u16;80];let n=unsafe{GetClassNameW(*window,&mut class)}.max(0)as usize;
        if !matches!(String::from_utf16_lossy(&class[..n]).as_str(),"CabinetWClass"|"ExploreWClass"){continue;}
        let location=(||->Result<PathBuf>{let view=unsafe{browser.Document()?}.cast::<IShellFolderViewDual>()?;
            let folder=unsafe{view.Folder()?}.cast::<Folder2>()?;Ok(PathBuf::from(unsafe{folder.Self_()?.Path()?}.to_string()))})();
        if location.is_ok_and(|path|self.folders.iter().any(|folder|crate::explorer_forwarder::same_path(&path,folder))){let _=unsafe{browser.Quit()};}
    }
}}

fn normalize_entries(wanted: Vec<Entry>) -> Vec<Entry> {
    let mut result = Vec::new();
    for entry in wanted {
        let supported = match &entry.location {
            ShellLocation::ThisPc => true,
            ShellLocation::Path(path) => path.is_absolute() && !path.as_os_str().encode_wide().any(|unit| unit == 0),
            ShellLocation::Shell { parsing_name, .. } => !parsing_name.is_empty() && !parsing_name.contains('\0'),
            ShellLocation::Search { .. } => false,
        };
        if supported && entry.identity.source != 0 && !result.iter().any(|had: &Entry| had.identity.source == entry.identity.source) {
            result.push(entry);
            if result.len() == MAX_PANES { break; }
        }
    }
    result.sort_by_key(|entry| entry.identity.source);
    result
}

/// ReadProcessMemory makes malformed input fail instead of dereferencing an
/// unchecked foreign pointer. No Shell parsing or UI calls occur here.
fn copy_pidl(pointer: *const ITEMIDLIST) -> Result<Vec<u8>> {
    if pointer.is_null() { return Err(Error::from_hresult(E_INVALIDARG)); }
    let mut bytes = Vec::new();
    for _ in 0..256 {
        let address = (pointer as usize).checked_add(bytes.len()).ok_or_else(|| Error::from_hresult(E_INVALIDARG))?;
        let mut size = [0u8; 2];
        read_memory(address, &mut size)?;
        let count = u16::from_le_bytes(size) as usize;
        if count == 0 {
            bytes.extend_from_slice(&size);
            return if valid_pidl_bytes(&bytes) { Ok(bytes) } else { Err(Error::from_hresult(E_INVALIDARG)) };
        }
        if count < 2 || bytes.len().saturating_add(count).saturating_add(2) > MAX_PIDL_BYTES {
            return Err(Error::from_hresult(E_INVALIDARG));
        }
        let offset = bytes.len();
        bytes.resize(offset + count, 0);
        read_memory(address, &mut bytes[offset..])?;
    }
    Err(Error::from_hresult(E_INVALIDARG))
}
fn valid_pidl_bytes(bytes: &[u8]) -> bool {
    if bytes.len() > MAX_PIDL_BYTES { return false; }
    let mut offset = 0usize;
    for _ in 0..=256 {
        let Some(header) = bytes.get(offset..offset.saturating_add(2)) else { return false };
        let count = u16::from_le_bytes([header[0], header[1]]) as usize;
        if count == 0 { return offset + 2 == bytes.len(); }
        if count < 2 { return false; }
        let Some(next) = offset.checked_add(count).filter(|next| *next <= bytes.len()) else { return false };
        offset = next;
    }
    false
}
fn read_memory(address: usize, bytes: &mut [u8]) -> Result<()> {
    let mut count = 0;
    unsafe { ReadProcessMemory(GetCurrentProcess(), address as *const _, bytes.as_mut_ptr().cast(), bytes.len(), Some(&mut count))?; }
    if count != bytes.len() { return Err(Error::from_hresult(E_INVALIDARG)); }
    Ok(())
}

struct Pidl(*mut ITEMIDLIST);
impl Drop for Pidl { fn drop(&mut self) { unsafe { CoTaskMemFree(Some(self.0.cast())); } } }
fn take_pidl(pointer: *mut ITEMIDLIST) -> Result<Vec<u8>> {
    if pointer.is_null() { return Err(Error::from_hresult(E_FAIL)); }
    let owned = Pidl(pointer);
    copy_pidl(owned.0)
}
fn parse_pidl(name: &[u16]) -> Result<Vec<u8>> {
    let mut pointer = std::ptr::null_mut();
    if let Err(error) = unsafe { SHParseDisplayName(PCWSTR(name.as_ptr()), None, &mut pointer, 0, None) } {
        if !pointer.is_null() { unsafe { CoTaskMemFree(Some(pointer.cast())); } }
        return Err(error);
    }
    take_pidl(pointer)
}
fn push_distinct(forms: &mut Vec<Vec<u8>>, bytes: Vec<u8>) {
    if !forms.contains(&bytes) { forms.push(bytes); }
}
fn location_forms(location: &ShellLocation) -> Result<Vec<Vec<u8>>> {
    let mut forms = Vec::new();
    match location {
        ShellLocation::ThisPc => {
            forms.push(take_pidl(unsafe { SHGetKnownFolderIDList(&FOLDERID_ComputerFolder, 0, None)? })?);
        }
        ShellLocation::Path(path) => {
            let name = shell_path(path);
            if let Ok(bytes) = take_pidl(unsafe { ILCreateFromPathW(PCWSTR(name.as_ptr())) }) { forms.push(bytes); }
            if let Ok(bytes) = parse_pidl(&name) { push_distinct(&mut forms, bytes); }
        }
        ShellLocation::Shell { parsing_name, .. } => {
            forms.push(parse_pidl(&parsing_name.encode_utf16().chain(Some(0)).collect::<Vec<_>>())?);
        }
        ShellLocation::Search { .. } => return Err(Error::from_hresult(E_INVALIDARG)),
    }
    if forms.is_empty() { return Err(Error::from_hresult(E_FAIL)); }
    Ok(forms)
}
fn shell_path(path: &Path) -> Vec<u16> {
    let mut path = path.as_os_str().encode_wide().map(|unit| if unit == b'/' as u16 { b'\\' as u16 } else { unit }).collect::<Vec<_>>();
    let unc = r"\\?\UNC\".encode_utf16().collect::<Vec<_>>();
    let extended = r"\\?\".encode_utf16().collect::<Vec<_>>();
    if wide_ascii_prefix(&path, &unc) { path.splice(..unc.len(), [b'\\' as u16, b'\\' as u16]); }
    else if path.starts_with(&extended) { path.drain(..extended.len()); }
    path.push(0); path
}
fn wide_ascii_prefix(text: &[u16], prefix: &[u16]) -> bool {
    let lower = |unit: u16| if (b'A' as u16..=b'Z' as u16).contains(&unit) { unit + 32 } else { unit };
    text.len() >= prefix.len() && text.iter().zip(prefix).all(|(left, right)| lower(*left) == lower(*right))
}

struct Proxy(HWND);
impl Proxy {
    fn new() -> Result<Self> {
        Ok(Self(unsafe { CreateWindowExW(WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW, w!("STATIC"),
            w!("ZSDesk Shell registration"), WS_POPUP, 0, 0, 0, 0, None, None, None, None)? }))
    }
}
impl Drop for Proxy { fn drop(&mut self) { let _ = unsafe { DestroyWindow(self.0) }; } }

struct Registered {
    entry: Entry, bytes: Vec<u8>, pending: Option<i32>, bound: Option<i32>, ready: bool,
    active: Arc<AtomicBool>, app: IWebBrowserApp, _proxy: Proxy,
}
impl Registered {
    fn new(shared: Arc<Shared>, generation: u64, entry: Entry, bytes: Vec<u8>, type_info: Option<ITypeInfo>) -> Result<Self> {
        let proxy = Proxy::new()?;
        let active = Arc::new(AtomicBool::new(false));
        let app: IWebBrowserApp = WindowObject { shared, generation, entry: entry.clone(), active: active.clone(), type_info }.into();
        Ok(Self { entry, bytes, pending: None, bound: None, ready: false, active, app, _proxy: proxy })
    }
    fn advance(&mut self, shell: &IShellWindows) -> Result<()> {
        if self.ready { return Ok(()); }
        let location = unsafe { InitVariantFromBuffer(self.bytes.as_ptr().cast(), self.bytes.len() as u32)? };
        if self.pending.is_none() {
            let root = VARIANT::default();
            self.pending = Some(unsafe { shell.RegisterPending(GetCurrentThreadId() as i32, &location, &root, SWC_BROWSER)? });
        }
        if self.bound.is_none() {
            // Register completes a location that may already have waiting
            // callers. The real UI view is ready before it is published.
            self.active.store(true, Ordering::Release);
            self.bound = Some(unsafe { shell.Register(&self.app, self._proxy.0.0 as isize as i32, SWC_BROWSER)? });
        }
        let cookie = self.bound.unwrap();
        unsafe { shell.OnNavigate(cookie, &location)?; }
        // Usually Register completes the pending cookie. If the Shell assigned
        // another cookie, retain only the completed window after OnNavigate.
        if let Some(pending) = self.pending.filter(|pending| *pending != cookie) {
            unsafe { shell.Revoke(pending)?; }
            self.pending = None;
        }
        self.ready = true; self.active.store(true, Ordering::Release);
        Ok(())
    }
    fn revoke(&mut self, shell: &IShellWindows) -> Result<()> {
        self.active.store(false, Ordering::Release); self.ready = false;
        if let Some(cookie) = self.pending {
            unsafe { shell.Revoke(cookie)?; }
            self.pending = None;
            if self.bound == Some(cookie) { self.bound = None; }
        }
        if let Some(cookie) = self.bound { unsafe { shell.Revoke(cookie)?; } self.bound = None; }
        Ok(())
    }
}
impl Drop for Registered { fn drop(&mut self) { self.active.store(false, Ordering::Release); } }

fn disconnected(error: &Error) -> bool {
    matches!(error.code().0 as u32, 0x80010108 | 0x800401FD | 0x800706BA | 0x80010007 | 0x80010012)
}
fn shell_identity() -> (isize, u32) {
    let hwnd = unsafe { GetShellWindow() };
    let mut process = 0;
    if !hwnd.0.is_null() { unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process)); } }
    (hwnd.0 as isize, process)
}
fn observed_shell(shared:&Shared)->(isize,u32){
    let identity=shell_identity();
    #[cfg(test)]
    if identity.0==0{
        // An unswitched test desktop has no own shell HWND, but uses the same
        // default-desktop IShellWindows server. Preserve its observed identity
        // solely for the worker's restart/liveness gate; COM calls stay real.
        if let Some(expected)=*shared.probe_shell.lock().unwrap_or_else(|e|e.into_inner()){
            let window=HWND(expected.0 as*mut _);let mut pid=0;
            unsafe{GetWindowThreadProcessId(window,Some(&mut pid));}
            if pid==expected.1&&unsafe{IsWindow(Some(window))}.as_bool(){return expected;}
        }
    }
    #[cfg(not(test))]let _=shared;
    identity
}
fn type_info() -> Option<ITypeInfo> {
    let library = GUID::from_u128(0xeab22ac0_30c1_11cf_a7eb_0000c05bae0b);
    unsafe { LoadRegTypeLib(&library, 1, 1, 0).and_then(|library| library.GetTypeInfoOfGuid(&IWebBrowserApp::IID)).ok() }
}
fn worker_main(shared: Arc<Shared>) {
    struct ThreadIdentity(Arc<Shared>);
    impl Drop for ThreadIdentity {
        fn drop(&mut self) {
            let _cancel = self.0.cancellation.lock();
            self.0.thread_id.store(0, Ordering::Release);
        }
    }
    let _identity = ThreadIdentity(shared.clone());
    shared.thread_id.store(unsafe { GetCurrentThreadId() }, Ordering::Release);
    let mut init_wait = 250;
    loop {
        if shared.stopped.load(Ordering::Acquire) { return; }
        match unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) }.ok() {
            Ok(()) => break,
            Err(error) => shared.error(&error),
        }
        unsafe { WaitForSingleObject(HANDLE(shared.wake as *mut _), init_wait); }
        init_wait = (init_wait * 2).min(5000);
    }
    struct Apartment;
    impl Drop for Apartment { fn drop(&mut self) { unsafe { let _ = CoDisableCallCancellation(None); CoUninitialize(); } } }
    let _apartment = Apartment;
    let _ = unsafe { CoEnableCallCancellation(None) };
    let type_info = type_info();
    let taskbar_created = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
    let mut shell: Option<IShellWindows> = None;
    let mut registered: BTreeMap<(usize, usize), Registered> = BTreeMap::new();
    let mut prepared: Vec<(Entry, Vec<Vec<u8>>)> = Vec::new();
    let mut revision = u64::MAX;
    let mut generation = 0;
    let mut explorer = observed_shell(&shared);
    let mut dirty = true;
    let mut retry = Duration::from_millis(250);
    let mut due = Instant::now();
    while !shared.stopped.load(Ordering::Acquire) {
        let current_explorer = observed_shell(&shared);
        let snapshot = shared.state.lock().ok().map(|state| (state.revision, state.generation, state.wanted.clone()));
        let Some((next_revision, next_generation, wanted)) = snapshot else { break };
        if current_explorer != explorer || next_generation != generation {
            for record in registered.values_mut() { record.active.store(false, Ordering::Release); }
            if let Some(old) = &shell { for record in registered.values_mut() { let _ = record.revoke(old); } }
            registered.clear(); shell = None; prepared.clear();
            explorer = current_explorer; generation = next_generation; revision = u64::MAX;
            dirty = true; due = Instant::now(); retry = Duration::from_millis(250);
        }
        if revision != next_revision {
            prepared.clear(); revision = next_revision; dirty = true;
            due = Instant::now(); retry = Duration::from_millis(250);
            // Invalidate removed/navigating panes before any potentially slow RPC.
            for record in registered.values() {
                if !wanted.contains(&record.entry) { record.active.store(false, Ordering::Release); }
            }
        }
        if dirty && Instant::now() >= due && explorer.0 != 0 {
            let result = (|| -> Result<()> {
                if shell.is_none() && (!wanted.is_empty() || !registered.is_empty()) {
                    shell = Some(unsafe { CoCreateInstance(&ShellWindows, None, CLSCTX_LOCAL_SERVER)? });
                }
                let Some(shell) = shell.as_ref() else { return Ok(()) };
                let mut failure = None;
                for entry in &wanted {
                    if prepared.iter().any(|(had, _)| had == entry) { continue; }
                    match location_forms(&entry.location) {
                        Ok(forms) => prepared.push((entry.clone(), forms)),
                        Err(error) => failure = Some(error),
                    }
                }
                let mut target = BTreeMap::new();
                for (entry, forms) in &prepared {
                    for (form, bytes) in forms.iter().enumerate() { target.insert((entry.identity.source, form), (entry, bytes)); }
                }
                let removed = registered.iter().filter(|(key, record)| target.get(*key)
                    .is_none_or(|(entry, bytes)| **entry != record.entry || **bytes != record.bytes))
                    .map(|(key, _)| *key).collect::<Vec<_>>();
                for key in removed {
                    if shared.stopped.load(Ordering::Acquire) { return Ok(()); }
                    match registered.get_mut(&key).unwrap().revoke(shell) {
                        Ok(()) => { registered.remove(&key); },
                        Err(error) if disconnected(&error) => return Err(error),
                        Err(error) => failure = Some(error),
                    }
                }
                for (key, (entry, bytes)) in target {
                    if shared.stopped.load(Ordering::Acquire) { return Ok(()); }
                    if registered.get(&key).is_some_and(|record| record.entry != *entry || record.bytes != *bytes) { continue; }
                    if !registered.contains_key(&key) {
                        match Registered::new(shared.clone(), generation, entry.clone(), bytes.clone(), type_info.clone()) {
                            Ok(record) => { registered.insert(key, record); },
                            Err(error) => { failure = Some(error); continue; },
                        }
                    }
                    match registered.get_mut(&key).unwrap().advance(shell) {
                        Ok(()) => {},
                        Err(error) if disconnected(&error) => return Err(error),
                        Err(error) => {
                            // Pending/Register is a serial pair per STA. Do
                            // not leave an unmatched pending cookie while
                            // registering another pane on this same thread.
                            registered.get_mut(&key).unwrap().revoke(shell)?;
                            registered.remove(&key);
                            failure = Some(error);
                        },
                    }
                }
                failure.map_or(Ok(()), Err)
            })();
            match result {
                Ok(()) => { dirty = false; retry = Duration::from_millis(250); },
                Err(error) => {
                    shared.error(&error);
                    if disconnected(&error) {
                        for record in registered.values() { record.active.store(false, Ordering::Release); }
                        if let Some(old) = &shell { for record in registered.values_mut() { let _ = record.revoke(old); } }
                        registered.clear(); shell = None;
                    }
                    due = Instant::now() + retry; retry = (retry * 2).min(Duration::from_secs(5));
                }
            }
        }
        let wait = if dirty && explorer.0 != 0 { due.saturating_duration_since(Instant::now()).as_millis().clamp(50, 1000) as u32 } else { 1000 };
        unsafe { MsgWaitForMultipleObjectsEx(Some(&[HANDLE(shared.wake as *mut _)]), wait, QS_ALLINPUT, MWMO_INPUTAVAILABLE); }
        let mut message = MSG::default();
        for _ in 0..512 {
            if !unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() { break; }
            if taskbar_created != 0 && message.message == taskbar_created {
                if let Ok(mut state) = shared.state.lock() { state.generation = state.generation.wrapping_add(1); }
            } else if message.message != WM_QUIT {
                unsafe { let _ = TranslateMessage(&message); DispatchMessageW(&message); }
            }
        }
    }
    if let Some(shell) = &shell {
        for record in registered.values_mut() { let _ = record.revoke(shell); }
    }
    registered.clear();
}

#[implement(IWebBrowserApp, IServiceProvider, IShellBrowser, IShellView)]
struct WindowObject {
    shared: Arc<Shared>, generation: u64, entry: Entry,
    active: Arc<AtomicBool>, type_info: Option<ITypeInfo>,
}
fn unsupported<T>() -> Result<T> { Err(Error::from_hresult(E_NOTIMPL)) }
impl IDispatch_Impl for WindowObject_Impl {
    fn GetTypeInfoCount(&self) -> Result<u32> { Ok(u32::from(self.type_info.is_some())) }
    fn GetTypeInfo(&self, index: u32, _: u32) -> Result<ITypeInfo> {
        self.type_info.as_ref().filter(|_| index == 0).cloned().ok_or_else(|| Error::from_hresult(HRESULT(0x8002000Bu32 as i32)))
    }
    fn GetIDsOfNames(&self, _: *const GUID, names: *const PCWSTR, count: u32, _: u32, ids: *mut i32) -> Result<()> {
        let info = self.type_info.as_ref().ok_or_else(|| Error::from_hresult(HRESULT(0x80020006u32 as i32)))?;
        unsafe { DispGetIDsOfNames(info, names, count, ids) }
    }
    fn Invoke(&self, id: i32, _: *const GUID, _: u32, flags: DISPATCH_FLAGS, parameters: *const DISPPARAMS,
        result: *mut VARIANT, exception: *mut EXCEPINFO, argument: *mut u32) -> Result<()> {
        let info = self.type_info.as_ref().ok_or_else(|| Error::from_hresult(HRESULT(0x80020003u32 as i32)))?;
        let browser: IWebBrowserApp = self.to_interface();
        unsafe { DispInvoke(browser.as_raw(), info, id, flags.0, parameters.cast_mut(), result, exception, argument) }
    }
}
impl IServiceProvider_Impl for WindowObject_Impl {
    fn QueryService(&self, service: *const GUID, iid: *const GUID, out: *mut *mut c_void) -> Result<()> {
        if service.is_null() || iid.is_null() || out.is_null() { return Err(Error::from_hresult(E_INVALIDARG)); }
        unsafe { *out = std::ptr::null_mut(); }
        let service = unsafe { *service };
        let known = service == SID_STopLevelBrowser || service == IShellBrowser::IID
            || service == IShellView::IID || service == IFolderView::IID;
        if !known { return Err(Error::from_hresult(E_NOINTERFACE)); }
        unsafe { self.QueryInterface(iid, out).ok() }
    }
}
impl IOleWindow_Impl for WindowObject_Impl {
    fn GetWindow(&self) -> Result<HWND> {
        if self.shared.stopped.load(Ordering::Acquire) { return Err(Error::from_hresult(E_FAIL)); }
        Ok(HWND(self.shared.owner as *mut _))
    }
    fn ContextSensitiveHelp(&self, _: BOOL) -> Result<()> { unsupported() }
}
impl IShellBrowser_Impl for WindowObject_Impl {
    fn InsertMenusSB(&self, _: HMENU, _: *mut OLEMENUGROUPWIDTHS) -> Result<()> { unsupported() }
    fn SetMenuSB(&self, _: HMENU, _: isize, _: HWND) -> Result<()> { unsupported() }
    fn RemoveMenusSB(&self, _: HMENU) -> Result<()> { unsupported() }
    fn SetStatusTextSB(&self, _: &PCWSTR) -> Result<()> { unsupported() }
    fn EnableModelessSB(&self, _: BOOL) -> Result<()> { Ok(()) }
    fn TranslateAcceleratorSB(&self, _: *const MSG, _: u16) -> Result<()> { unsupported() }
    fn BrowseObject(&self, _: *const ITEMIDLIST, _: u32) -> Result<()> { unsupported() }
    fn GetViewStateStream(&self, _: u32) -> Result<IStream> { unsupported() }
    fn GetControlWindow(&self, _: u32) -> Result<HWND> { unsupported() }
    fn SendControlMsg(&self, _: u32, _: u32, _: WPARAM, _: LPARAM, _: *mut LRESULT) -> Result<()> { unsupported() }
    fn QueryActiveShellView(&self) -> Result<IShellView> { Ok(self.to_interface()) }
    fn OnViewWindowActive(&self, _: Ref<IShellView>) -> Result<()> { Ok(()) }
    fn SetToolbarItems(&self, _: *const TBBUTTON, _: u32, _: u32) -> Result<()> { unsupported() }
}
impl IShellView_Impl for WindowObject_Impl {
    fn TranslateAccelerator(&self, _: *const MSG) -> HRESULT { HRESULT(1) }
    fn EnableModeless(&self, _: BOOL) -> Result<()> { Ok(()) }
    fn UIActivate(&self, _: u32) -> Result<()> { unsupported() }
    fn Refresh(&self) -> Result<()> { unsupported() }
    fn CreateViewWindow(&self, _: Ref<IShellView>, _: *const FOLDERSETTINGS, _: Ref<IShellBrowser>, _: *const RECT) -> Result<HWND> { unsupported() }
    fn DestroyViewWindow(&self) -> Result<()> { unsupported() }
    fn GetCurrentInfo(&self) -> Result<FOLDERSETTINGS> { unsupported() }
    fn AddPropertySheetPages(&self, _: u32, _: LPFNSVADDPROPSHEETPAGE, _: LPARAM) -> Result<()> { unsupported() }
    fn SaveViewState(&self) -> Result<()> { unsupported() }
    fn SelectItem(&self, child: *const ITEMIDLIST, flags: u32) -> Result<()> {
        if !self.active.load(Ordering::Acquire) { return Err(Error::from_hresult(E_FAIL)); }
        let flags = flags & SELECT_FLAGS;
        if flags == 0 { return Ok(()); }
        let child_pidl = if child.is_null() {
            if flags & 0x4 == 0 { return Ok(()); }
            None
        } else { Some(copy_pidl(child)?) };
        self.shared.post(SelectRequest { identity: self.entry.identity, location: self.entry.location.clone(), child_pidl, flags }, self.generation)
    }
    fn GetItemObject(&self, _: &_SVGIO, _: *const GUID, out: *mut *mut c_void) -> Result<()> {
        if !out.is_null() { unsafe { *out = std::ptr::null_mut(); } } unsupported()
    }
}

impl IWebBrowser_Impl for WindowObject_Impl {
    fn GoBack(&self) -> Result<()> { unsupported() }
    fn GoForward(&self) -> Result<()> { unsupported() }
    fn GoHome(&self) -> Result<()> { unsupported() }
    fn GoSearch(&self) -> Result<()> { unsupported() }
    fn Navigate(&self, _: &BSTR, _: *const VARIANT, _: *const VARIANT, _: *const VARIANT, _: *const VARIANT) -> Result<()> { unsupported() }
    fn Refresh(&self) -> Result<()> { unsupported() }
    fn Refresh2(&self, _: *const VARIANT) -> Result<()> { unsupported() }
    fn Stop(&self) -> Result<()> { unsupported() }
    fn Application(&self) -> Result<IDispatch> { self.to_interface::<IWebBrowserApp>().cast() }
    fn Parent(&self) -> Result<IDispatch> { unsupported() }
    fn Container(&self) -> Result<IDispatch> { unsupported() }
    fn Document(&self) -> Result<IDispatch> { self.to_interface::<IWebBrowserApp>().cast() }
    fn TopLevelContainer(&self) -> Result<VARIANT_BOOL> { Ok(VARIANT_BOOL(-1)) }
    fn Type(&self) -> Result<BSTR> { Ok(BSTR::from("Folder")) }
    fn Left(&self) -> Result<i32> { unsupported() }
    fn SetLeft(&self, _: i32) -> Result<()> { unsupported() }
    fn Top(&self) -> Result<i32> { unsupported() }
    fn SetTop(&self, _: i32) -> Result<()> { unsupported() }
    fn Width(&self) -> Result<i32> { unsupported() }
    fn SetWidth(&self, _: i32) -> Result<()> { unsupported() }
    fn Height(&self) -> Result<i32> { unsupported() }
    fn SetHeight(&self, _: i32) -> Result<()> { unsupported() }
    fn LocationName(&self) -> Result<BSTR> { Ok(BSTR::from(self.entry.location.title())) }
    fn LocationURL(&self) -> Result<BSTR> { Ok(BSTR::from(location_url(&self.entry.location))) }
    fn Busy(&self) -> Result<VARIANT_BOOL> { Ok(VARIANT_BOOL(0)) }
}
impl IWebBrowserApp_Impl for WindowObject_Impl {
    fn Quit(&self) -> Result<()> { unsupported() }
    fn ClientToWindow(&self, _: *mut i32, _: *mut i32) -> Result<()> { unsupported() }
    fn PutProperty(&self, _: &BSTR, _: &VARIANT) -> Result<()> { unsupported() }
    fn GetProperty(&self, _: &BSTR) -> Result<VARIANT> { unsupported() }
    fn Name(&self) -> Result<BSTR> { Ok(BSTR::from("ZSDesk")) }
    fn HWND(&self) -> Result<SHANDLE_PTR> {
        if self.shared.stopped.load(Ordering::Acquire) { return Err(Error::from_hresult(E_FAIL)); }
        Ok(SHANDLE_PTR(self.shared.owner))
    }
    fn FullName(&self) -> Result<BSTR> { Ok(BSTR::from(std::env::current_exe().unwrap_or_default().to_string_lossy().as_ref())) }
    fn Path(&self) -> Result<BSTR> { Ok(BSTR::from(std::env::current_exe().ok().and_then(|path| path.parent().map(PathBuf::from)).unwrap_or_default().to_string_lossy().as_ref())) }
    fn Visible(&self) -> Result<VARIANT_BOOL> { Ok(VARIANT_BOOL(if unsafe { IsWindowVisible(HWND(self.shared.owner as *mut _)) }.as_bool() { -1 } else { 0 })) }
    fn SetVisible(&self, _: VARIANT_BOOL) -> Result<()> { unsupported() }
    fn StatusBar(&self) -> Result<VARIANT_BOOL> { Ok(VARIANT_BOOL(0)) }
    fn SetStatusBar(&self, _: VARIANT_BOOL) -> Result<()> { unsupported() }
    fn StatusText(&self) -> Result<BSTR> { Ok(BSTR::new()) }
    fn SetStatusText(&self, _: &BSTR) -> Result<()> { unsupported() }
    fn ToolBar(&self) -> Result<i32> { Ok(0) }
    fn SetToolBar(&self, _: i32) -> Result<()> { unsupported() }
    fn MenuBar(&self) -> Result<VARIANT_BOOL> { Ok(VARIANT_BOOL(0)) }
    fn SetMenuBar(&self, _: VARIANT_BOOL) -> Result<()> { unsupported() }
    fn FullScreen(&self) -> Result<VARIANT_BOOL> { Ok(VARIANT_BOOL(0)) }
    fn SetFullScreen(&self, _: VARIANT_BOOL) -> Result<()> { unsupported() }
}
fn location_url(location: &ShellLocation) -> String {
    let ShellLocation::Path(path) = location else { return String::new() };
    let path = String::from_utf16_lossy(&shell_path(path));
    let path = path.trim_end_matches('\0').replace('\\', "/");
    let prefix = if path.starts_with("//") { "file:" } else { "file:///" };
    let mut result = prefix.to_string();
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/:".contains(&byte) { result.push(byte as char); }
        else { use std::fmt::Write; let _ = write!(result, "%{byte:02X}"); }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(source: usize, tab: u64, epoch: u64) -> Entry {
        Entry { identity: Identity { source, tab, epoch }, location: ShellLocation::Path(std::env::temp_dir().join(format!("zsdesk-shell-registration-{source}"))) }
    }
    fn shared(owner: isize) -> Arc<Shared> {
        Arc::new(Shared { owner, wake: 0, stopped: AtomicBool::new(false), thread_id: AtomicU32::new(0), state: Mutex::new(SharedState::default()), cancellation: Mutex::new(()),probe_shell:Mutex::new(None) })
    }

    #[test]
    fn publishing_same_panes_is_idle_and_navigation_rejects_old_selection() {
        let shared = shared(0);
        let mut registry = Registry { shared: shared.clone(), worker: None };
        let first = entry(1, 2, 3);
        let second = entry(2, 4, 5);
        registry.publish(vec![first.clone(), second.clone()]);
        let revision = shared.state.lock().unwrap().revision;
        registry.publish(vec![second.clone(), first.clone()]);
        assert_eq!(shared.state.lock().unwrap().revision, revision);
        let request = SelectRequest { identity: first.identity, location: first.location.clone(), child_pidl: Some(vec![4,0,1,2,0,0]), flags: 0x1D };
        assert!(request.matches(&first));
        let changed = Entry { identity: Identity { epoch: 4, ..first.identity }, ..first.clone() };
        assert!(!request.matches(&changed));
        shared.state.lock().unwrap().requests.push_back(request);
        registry.publish(vec![changed, second]);
        assert!(registry.take_requests().is_empty());
        registry.stop(); registry.stop();
        registry.publish(vec![first]);
        assert!(shared.state.lock().unwrap().wanted.is_empty());
    }

    #[test]
    fn aliases_are_deduplicated_by_complete_pidl_bytes_only() {
        let primary = vec![4,0,1,2,0,0];
        let alias = vec![4,0,3,4,0,0];
        let mut forms = vec![primary.clone()];
        push_distinct(&mut forms, primary.clone());
        push_distinct(&mut forms, alias.clone());
        push_distinct(&mut forms, alias.clone());
        assert_eq!(forms, vec![primary, alias]);
    }

    #[test]
    fn foreign_pidls_are_bounded_and_copied_as_owned_bytes() {
        let valid = vec![4u8,0,1,2,3,0,9,0,0];
        assert_eq!(copy_pidl(valid.as_ptr().cast()).unwrap(), valid);
        for bytes in [vec![], vec![0], vec![1,0,0,0], vec![4,0,1,0], vec![0,0,1], vec![255,255,0,0]] {
            assert!(!valid_pidl_bytes(&bytes), "{bytes:?}");
        }
        assert!(copy_pidl(std::ptr::null()).is_err());
        assert!(copy_pidl(usize::MAX as *const ITEMIDLIST).is_err());
        assert!(!valid_pidl_bytes(&vec![0;MAX_PIDL_BYTES+1]));
    }

    #[test]
    fn both_service_routes_expose_the_real_window_and_stop_invalidates_callbacks() {
        let shared = shared(37);
        let item = entry(1, 2, 3);
        shared.state.lock().unwrap().wanted = vec![item.clone()];
        let app: IWebBrowserApp = WindowObject { shared: shared.clone(), generation: 0, entry: item,
            active: Arc::new(AtomicBool::new(true)), type_info: None }.into();
        assert_eq!(unsafe { app.HWND() }.unwrap().0, 37);
        let provider: IServiceProvider = app.cast().unwrap();
        let browser: IShellBrowser = unsafe { provider.QueryService(&SID_STopLevelBrowser) }.unwrap();
        let view = unsafe { browser.QueryActiveShellView() }.unwrap();
        assert_eq!(unsafe { view.GetWindow() }.unwrap().0 as isize, 37);
        let document = unsafe { app.Document() }.unwrap();
        let document_provider: IServiceProvider = document.cast().unwrap();
        let second: IShellView = unsafe { document_provider.QueryService(&IFolderView::IID) }.unwrap();
        assert_eq!(unsafe { second.GetWindow() }.unwrap().0 as isize, 37);
        shared.stopped.store(true, Ordering::Release);
        assert!(unsafe { app.HWND() }.is_err());
        assert!(unsafe { view.GetWindow() }.is_err());
        let child = [4u8,0,1,2,0,0];
        assert!(unsafe { second.SelectItem(child.as_ptr().cast(), 0x1D) }.is_err());
        assert!(shared.state.lock().unwrap().requests.is_empty());
    }

    #[test]
    #[ignore = "Opt-in hidden Shell registration probe; uses only fixture paths, FindWindowSW/QI/SelectItem, never SHOpenFolderAndSelectItems"]
    fn native_hidden_shell_registration_routes_selection_and_revokes() -> anyhow::Result<()> {
        use anyhow::{Context, ensure};
        use windows::Win32::UI::Shell::{SWFO_NEEDDISPATCH, SWC_EXPLORER, ShellWindowTypeConstants};
        use std::os::windows::fs::MetadataExt;
        let output = PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("isolated output required")?);
        ensure!(output.is_absolute(), "test output must be absolute");
        ensure!(shell_identity().0 != 0, "an existing Explorer shell is required; the probe does not start one");
        let root = output.join("shell-registration-fixtures");
        let first = root.join("A"); let second = root.join("B");
        std::fs::create_dir_all(&first)?; std::fs::create_dir_all(&second)?;
        for path in [&root, &first, &second] {
            ensure!(std::fs::symlink_metadata(path)?.file_attributes() & 0x400 == 0, "fixtures must use real directories");
        }
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) }.ok()?;
        struct Apartment;
        impl Drop for Apartment { fn drop(&mut self) { unsafe { CoUninitialize(); } } }
        let _apartment = Apartment;
        let owner = Proxy::new()?;
        ensure!(!unsafe { IsWindowVisible(owner.0) }.as_bool(), "probe owner must remain hidden");
        let mut registry = Registry::new(owner.0)?;
        let a = Entry { identity: Identity { source: 1, tab: 10, epoch: 1 }, location: ShellLocation::Path(first) };
        let b = Entry { identity: Identity { source: 2, tab: 20, epoch: 1 }, location: ShellLocation::Path(second) };
        let forms_a = location_forms(&a.location)?; let forms_b = location_forms(&b.location)?;
        let shell: IShellWindows = unsafe { CoCreateInstance(&ShellWindows, None, CLSCTX_LOCAL_SERVER)? };
        let find_as = |bytes: &[u8], class: ShellWindowTypeConstants| -> Option<(IWebBrowserApp, i32)> {
            let location = unsafe { InitVariantFromBuffer(bytes.as_ptr().cast(), bytes.len() as u32) }.ok()?;
            let mut hwnd = 0;
            let dispatch = unsafe { shell.FindWindowSW(&location, &VARIANT::default(), class, &mut hwnd, SWFO_NEEDDISPATCH) }.ok()?;
            let app: IWebBrowserApp = dispatch.cast().ok()?;
            (unsafe { app.HWND() }.ok()?.0 == owner.0.0 as isize).then_some((app, hwnd))
        };
        let find = |bytes: &[u8]| find_as(bytes, SWC_BROWSER);
        registry.publish(vec![a.clone(), b.clone()]);
        wait_until(|| forms_a.iter().chain(&forms_b).all(|bytes| find(bytes).is_some()))?;
        let mut handles = std::collections::BTreeSet::new();
        // FindWindowSW canonicalizes equivalent PIDLs and may return the real
        // browser HWND from IWebBrowserApp. Its returned HWND is not a unique
        // key for our registration proxies; verify the routed pane instead.
        let child = [4u8,0,1,2,0,0];
        for (entry,forms) in [(&a,&forms_a),(&b,&forms_b)] {
            for bytes in forms {
                let (app,handle)=find(bytes).context("registered path")?;handles.insert(handle);
                ensure!(unsafe{app.LocationURL()?}.to_string()==location_url(&entry.location),"PIDL alias resolved to the wrong folder");
                let provider:IServiceProvider=app.cast()?;
                let browser:IShellBrowser=unsafe{provider.QueryService(&SID_STopLevelBrowser)?};
                unsafe{browser.QueryActiveShellView()?.SelectItem(child.as_ptr().cast(),0x1D)?;}
                let requests=registry.take_requests();
                ensure!(requests.len()==1&&requests[0].matches(entry),"PIDL alias selected the wrong pane");
            }
        }
        // Enumeration class names alone do not establish the class used by
        // SHOpenFolderAndSelectItems. Keep both lookups observable; only the
        // separately gated real API probe can establish that call's behavior.
        let explorer_class_matches = forms_a.iter().chain(&forms_b).filter(|bytes| find_as(bytes, SWC_EXPLORER).is_some()).count();
        let browser_class_matches = forms_a.iter().chain(&forms_b).filter(|bytes| find_as(bytes, SWC_BROWSER).is_some()).count();
        let app = find(&forms_a[0]).context("first pane")?.0;
        let provider: IServiceProvider = app.cast()?;
        let browser: IShellBrowser = unsafe { provider.QueryService(&SID_STopLevelBrowser)? };
        let view = unsafe { browser.QueryActiveShellView()? };
        ensure!(unsafe { view.GetWindow()? } == owner.0, "IOleWindow must expose the real window");
        let document: IServiceProvider = unsafe { app.Document()? }.cast()?;
        let document_view: IShellView = unsafe { document.QueryService(&IFolderView::IID)? };
        ensure!(unsafe { document_view.GetWindow()? } == owner.0, "document route must expose the same window");
        // A structurally valid child ID exercises the bridge only. It is not
        // submitted to a real file view or used to open/select a user file.
        unsafe { view.SelectItem(child.as_ptr().cast(), 0x1D)?; }
        let requests = registry.take_requests();
        ensure!(requests.len() == 1 && requests[0].matches(&a)
            && requests[0].child_pidl.as_deref() == Some(child.as_slice()) && requests[0].flags == 0x1D, "selection was not relayed exactly once");
        let replacement = Entry { identity: Identity { epoch: 2, ..a.identity }, ..a.clone() };
        registry.publish(vec![replacement.clone(), b.clone()]);
        ensure!(unsafe { view.SelectItem(child.as_ptr().cast(), 0x1D) }.is_err(), "old epoch accepted selection");
        registry.restart();
        wait_until(|| find(&forms_a[0]).is_some_and(|(app, _)| {
            let Ok(provider) = app.cast::<IServiceProvider>() else { return false };
            let Ok(browser) = (unsafe { provider.QueryService::<IShellBrowser>(&SID_STopLevelBrowser) }) else { return false };
            let Ok(view) = (unsafe { browser.QueryActiveShellView() }) else { return false };
            unsafe { view.SelectItem(child.as_ptr().cast(), 0x1D) }.is_ok()
        }))?;
        let reconnected_requests = registry.take_requests();
        ensure!(!reconnected_requests.is_empty() && reconnected_requests.iter().all(|request| request.matches(&replacement)), "reconnect retained old lease");
        registry.publish(Vec::new());
        wait_until(|| forms_a.iter().chain(&forms_b).all(|bytes| find(bytes).is_none()))?;
        registry.stop();
        wait_until(|| registry.worker.as_ref().is_none_or(JoinHandle::is_finished))?;
        registry.stop();
        ensure!(!unsafe { IsWindowVisible(owner.0) }.as_bool(), "probe showed its owner");
        std::fs::write(output.join("shell-window-registry.current.json"), serde_json::to_vec_pretty(&serde_json::json!({
            "state":"passed", "fixture_root":root, "panes":2, "registration_forms":forms_a.len()+forms_b.len(),
            "registration_class":"SWC_BROWSER", "swc_browser_matches":browser_class_matches, "distinct_lookup_hwnds":handles.len(), "swc_explorer_matches":explorer_class_matches,
            "service_routes":true, "selection_queue":true, "old_epoch_rejected":true, "reconnect":true,
            "revoked":true, "windows_hidden":true, "full_shell_open_and_select_tested":false
        }))?)?;
        Ok(())
    }

    #[test]
    #[ignore = "Requires ZSDESK_SHELL_API_FIXTURE_PROBE=1; only an owned unique fixture registration and newly created fixture Explorer windows are exercised"]
    fn native_registered_folder_reveal_uses_shell_api() -> anyhow::Result<()> {
        use anyhow::{Context,ensure};
        ensure!(std::env::var("ZSDESK_SHELL_API_FIXTURE_PROBE").as_deref()==Ok("1"),"actual reveal API probe requires explicit fixture opt-in");
        let default_shell=shell_identity();ensure!(default_shell.0!=0,"an existing default-desktop Shell is required");
        let default_windows=probe_windows().context("enumerate existing default-desktop windows before isolation")?;
        if std::env::var("ZSDESK_SHELL_API_OFFSCREEN_PROBE").as_deref()==Ok("1"){
            return run_registered_folder_reveal_probe(default_shell,default_windows);
        }
        #[link(name="user32")]unsafe extern "system"{
            fn CreateDesktopW(name:*const u16,device:*const u16,mode:*const c_void,flags:u32,access:u32,attributes:*const c_void)->*mut c_void;
            fn GetThreadDesktop(thread:u32)->*mut c_void;
            fn SetThreadDesktop(desktop:*mut c_void)->i32;
            fn CloseDesktop(desktop:*mut c_void)->i32;
        }
        let name=format!("ZSDesk.RevealProbe.{}",std::process::id()).encode_utf16().chain(Some(0)).collect::<Vec<_>>();
        let desktop=unsafe{CreateDesktopW(name.as_ptr(),std::ptr::null(),std::ptr::null(),0,0x000f00ff,std::ptr::null())};
        ensure!(!desktop.is_null(),"private reveal desktop creation failed");let handle=desktop as usize;
        let result=thread::spawn(move||->anyhow::Result<()>{
            let original=unsafe{GetThreadDesktop(GetCurrentThreadId())};
            ensure!(unsafe{SetThreadDesktop(handle as*mut c_void)}!=0,"cannot attach private reveal desktop");
            let result=run_registered_folder_reveal_probe(default_shell,default_windows);
            let restored=unsafe{SetThreadDesktop(original)}!=0;
            result?;ensure!(restored,"cannot release private reveal desktop from its worker thread");Ok(())
        }).join().map_err(|_|anyhow::anyhow!("private reveal thread panicked")).and_then(|result|result);
        let closed=unsafe{CloseDesktop(desktop)}!=0;result.context("private-desktop Shell reveal probe")?;
        ensure!(closed,"private reveal desktop cleanup failed (Win32 {})",unsafe{windows::Win32::Foundation::GetLastError()}.0);Ok(())
    }

    fn run_registered_folder_reveal_probe(default_shell:(isize,u32),default_windows:Vec<(isize,u32)>)->anyhow::Result<()>{
        use anyhow::{Context, ensure};
        use windows::Win32::UI::Shell::{SHOpenFolderAndSelectItems, SWFO_NEEDDISPATCH};
        use std::os::windows::fs::MetadataExt;
        // The real API may fall back to a new Explorer window. This opt-in
        // probe uses a unique owned folder and records existing HWNDs before
        // the call; cleanup can close only a new Explorer at that exact folder.
        ensure!(std::env::var("ZSDESK_SHELL_API_FIXTURE_PROBE").as_deref()==Ok("1"),"actual reveal API probe requires explicit fixture opt-in");
        #[link(name="user32")]unsafe extern "system"{
            fn GetThreadDesktop(thread:u32)->*mut c_void;
            fn GetUserObjectInformationW(handle:*mut c_void,index:i32,info:*mut c_void,length:u32,needed:*mut u32)->i32;
        }
        let desktop=unsafe{GetThreadDesktop(GetCurrentThreadId())};
        let mut desktop_name=[0u16;256];let mut needed=0;
        ensure!(unsafe{GetUserObjectInformationW(desktop,2,desktop_name.as_mut_ptr().cast(),std::mem::size_of_val(&desktop_name)as u32,&mut needed)}!=0,"private desktop identity unavailable");
        let desktop_name=String::from_utf16_lossy(&desktop_name[..desktop_name.iter().position(|unit|*unit==0).unwrap_or(desktop_name.len())]);
        let mut windows_before=probe_windows().context("enumerate isolated desktop before reveal")?;
        // Shell fallback may run on the default desktop even though this
        // caller is isolated. Protect its pre-existing windows as well.
        windows_before.extend(default_windows);windows_before.sort_unstable();windows_before.dedup();
        let processes_before=probe_processes().context("snapshot process inventory before reveal")?;
        let foreground_before=unsafe{GetForegroundWindow()};
        let output=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("isolated output required")?);
        ensure!(output.is_absolute(),"test output must be absolute");
        let folder=output.join(format!("shell-api-reveal-fixture-{}",std::process::id()));
        std::fs::create_dir_all(&folder).context("create unique reveal fixture directory")?;
        for directory in [&output,&folder]{ensure!(std::fs::symlink_metadata(directory).with_context(||format!("inspect fixture directory {}",directory.display()))?.file_attributes()&0x400==0,"fixtures must use real directories");}
        let paths=[folder.join("下载 文档.txt"),folder.join("second item.txt")];
        for path in &paths{
            if path.exists(){ensure!(std::fs::symlink_metadata(path).context("inspect existing reveal fixture")?.file_attributes()&0x400==0&&std::fs::read(path).context("read existing reveal fixture")?==b"ZSDesk shell reveal fixture","existing fixture differs");}
            else{std::fs::write(path,b"ZSDesk shell reveal fixture").context("write reveal fixture")?;}
        }
        unsafe{CoInitializeEx(None,COINIT_APARTMENTTHREADED|COINIT_DISABLE_OLE1DDE)}.ok().context("initialize reveal STA")?;
        struct Apartment;
        impl Drop for Apartment{fn drop(&mut self){unsafe{CoUninitialize();}}}
        let _apartment=Apartment;
        let fixture_windows=RevealFixtureScope::new(windows_before.clone(),vec![folder.clone()]);
        let owner=Proxy::new().context("create visible reveal fixture owner")?;
        unsafe{SetWindowPos(owner.0,None,-30000,-30000,16,16,SWP_NOACTIVATE|SWP_NOZORDER).context("position reveal owner outside the screen")?;
            let _=ShowWindow(owner.0,SW_SHOWNOACTIVATE);}
        ensure!(unsafe{IsWindowVisible(owner.0)}.as_bool(),"probe must match a visible production owner");
        let mut registry=Registry::new_for_fixture(owner.0,default_shell).context("create real Shell window registry worker")?;
        let entry=Entry{identity:Identity{source:1,tab:50,epoch:1},location:ShellLocation::Path(folder.clone())};
        let forms=location_forms(&entry.location).context("resolve fixture folder registration PIDLs")?;
        let shell:IShellWindows=unsafe{CoCreateInstance(&ShellWindows,None,CLSCTX_LOCAL_SERVER)}.context("connect default ShellWindows server")?;
        let find=|bytes:&[u8]|->bool{
            let Ok(location)=(unsafe{InitVariantFromBuffer(bytes.as_ptr().cast(),bytes.len()as u32)})else{return false};
            let mut hwnd=0;
            let Ok(dispatch)=(unsafe{shell.FindWindowSW(&location,&VARIANT::default(),SWC_BROWSER,&mut hwnd,SWFO_NEEDDISPATCH)})else{return false};
            let Ok(app)=dispatch.cast::<IWebBrowserApp>()else{return false};
            unsafe{app.HWND()}.is_ok_and(|window|window.0==owner.0.0 as isize)
        };
        registry.publish(vec![entry.clone()]);
        wait_until(||forms.iter().all(|bytes|find(bytes))).context("wait for owned fixture registration before invoking Shell API")?;
        // Only a confirmed, owned registration is submitted. Its visible owner
        // is on the unswitched desktop (or explicitly opted-in screen exterior).
        let absolute=paths.iter().map(|path|parse_pidl(&shell_path(path))).collect::<Result<Vec<_>>>().context("parse absolute fixture item PIDLs")?;
        let children=absolute.iter().map(|bytes|{
            let mut at=0;let mut last=0;
            while at+2<=bytes.len(){let count=u16::from_le_bytes([bytes[at],bytes[at+1]])as usize;if count==0{break;}last=at;at+=count;}
            bytes[last..].to_vec()
        }).collect::<Vec<_>>();
        ensure!(children.iter().all(|child|valid_pidl_bytes(child)),"child PIDLs are invalid");
        let raw_children=children.iter().map(|bytes|bytes.as_ptr().cast::<ITEMIDLIST>()).collect::<Vec<_>>();
        let mut observed=Vec::new();
        let relative_result=unsafe{SHOpenFolderAndSelectItems(forms[0].as_ptr().cast(),Some(&raw_children),0)};
        std::fs::write(output.join("shell-reveal-api.current.json"),serde_json::to_vec_pretty(&serde_json::json!({
            "state":"relative-call-returned","fixture_root":folder,"relative_hresult":relative_result.as_ref().err().map(|error|format!("{:08X}",error.code().0 as u32)).unwrap_or_else(||"00000000".into())
        })).context("serialize relative Shell call diagnostic")?).context("write relative Shell call diagnostic")?;
        relative_result.context("SHOpenFolderAndSelectItems with folder and relative children")?;
        wait_until(||{
            observed.extend(registry.take_requests());
            children.iter().all(|child|observed.iter().any(|request|request.matches(&entry)&&request.flags&1!=0&&request.child_pidl.as_ref()==Some(child)))
        }).context("wait for relative-child Shell selection callbacks")?;
        ensure!(observed.iter().all(|request|request.matches(&entry)),"Shell selected another pane");
        observed.clear();registry.take_requests();
        // Chromium ShowItemInFolder supplies an absolute child PIDL in the
        // array as well as the parent folder PIDL. Cover that actual caller
        // shape separately from the documented relative-child form.
        let absolute_children=[absolute[0].as_ptr().cast::<ITEMIDLIST>()];
        unsafe{SHOpenFolderAndSelectItems(forms[0].as_ptr().cast(),Some(&absolute_children),0)}.context("Chromium-style folder plus absolute child PIDL")?;
        wait_until(||{
            observed.extend(registry.take_requests());
            observed.iter().any(|request|request.matches(&entry)&&request.flags&1!=0&&request.child_pidl.as_ref()==Some(&children[0]))
        }).context("wait for Chromium absolute-child Shell selection callback")?;
        observed.clear();registry.take_requests();
        ensure!(forms.iter().all(|bytes|find(bytes)),"registration disappeared before absolute item request");
        unsafe{SHOpenFolderAndSelectItems(absolute[0].as_ptr().cast(),None,0)}.context("SHOpenFolderAndSelectItems with absolute item and zero children")?;
        wait_until(||{
            observed.extend(registry.take_requests());
            observed.iter().any(|request|request.matches(&entry)&&request.flags&1!=0&&request.child_pidl.as_ref()==Some(&children[0]))
        }).context("wait for absolute-item Shell selection callback")?;
        registry.publish(Vec::new());
        wait_until(||forms.iter().all(|bytes|!find(bytes))).context("wait for fixture registration revocation")?;
        registry.stop();wait_until(||registry.worker.as_ref().is_none_or(JoinHandle::is_finished)).context("wait for reveal registry worker cleanup")?;registry.stop();
        ensure!(unsafe{IsWindowVisible(owner.0)}.as_bool(),"probe owner lost its visible production state");
        drop(registry);drop(owner);
        drop(fixture_windows);
        let windows_after=probe_windows().context("enumerate isolated desktop after reveal")?;
        let processes_after=probe_processes().context("snapshot process inventory after reveal")?;
        // Other applications remain free to create/close windows while this
        // test runs. Global inventory differences do not establish causation.
        let foreground_unchanged=unsafe{GetForegroundWindow()}==foreground_before;
        std::fs::write(output.join("shell-reveal-api.current.json"),serde_json::to_vec_pretty(&serde_json::json!({
            "state":"passed","fixture_root":folder,"registration_class":"SWC_BROWSER","relative_children":2,
            "absolute_item_zero_children":true,"chromium_absolute_child_array":true,"selection_queue":true,"revoked":true,
            "owner_visible":true,"owner_offscreen":true,"on_unswitched_private_desktop":desktop_name.starts_with("ZSDesk.RevealProbe."),"full_shell_open_and_select_tested":true,
            "default_shell_identity":default_shell,
            "private_desktop":desktop_name,"windows_before":windows_before,"windows_after":windows_after,
            "processes_before":processes_before,"processes_after":processes_after,"foreground_unchanged":foreground_unchanged
        })).context("serialize Shell reveal verification")?).context("write Shell reveal verification")?;
        Ok(())
    }

    pub(super) fn probe_windows()->anyhow::Result<Vec<(isize,u32)>>{
        unsafe extern "system" fn collect(hwnd:HWND,param:LPARAM)->BOOL{
            let output=unsafe{&mut*(param.0 as *mut Vec<(isize,u32)>)};let mut pid=0;
            unsafe{GetWindowThreadProcessId(hwnd,Some(&mut pid));}output.push((hwnd.0 as isize,pid));BOOL(1)
        }
        let mut windows=Vec::<(isize,u32)>::new();
        unsafe{windows::Win32::Foundation::SetLastError(windows::Win32::Foundation::WIN32_ERROR(0));}
        if let Err(error)=unsafe{EnumWindows(Some(collect),LPARAM(&mut windows as *mut _ as isize))}{
            // An empty unswitched desktop returns FALSE with ERROR_SUCCESS on
            // Windows. The callback always continues, so only this precise
            // zero-window/zero-error case is a successful empty inventory.
            if !windows.is_empty()||error.code().0!=0{return Err(anyhow::anyhow!(error).context("EnumWindows inventory failed"));}
        }
        windows.sort_unstable();Ok(windows)
    }
    fn probe_processes()->anyhow::Result<Vec<u32>>{
        #[link(name="kernel32")]unsafe extern "system"{fn K32EnumProcesses(pids:*mut u32,bytes:u32,written:*mut u32)->i32;}
        let mut pids=vec![0u32;65536];let mut written=0;
        anyhow::ensure!(unsafe{K32EnumProcesses(pids.as_mut_ptr(),(pids.len()*4)as u32,&mut written)}!=0,"process inventory unavailable");
        anyhow::ensure!((written as usize)<pids.len()*4&&written%4==0,"process inventory truncated");
        pids.truncate(written as usize/4);pids.sort_unstable();Ok(pids)
    }

    fn wait_until(mut predicate: impl FnMut() -> bool) -> anyhow::Result<()> {
        let start = Instant::now();
        loop {
            let mut message = MSG::default();
            while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
                if message.message != WM_SELECT && message.message != WM_QUIT {
                    unsafe { let _ = TranslateMessage(&message); DispatchMessageW(&message); }
                }
            }
            if predicate() { return Ok(()); }
            anyhow::ensure!(start.elapsed() < Duration::from_secs(10), "Shell registration probe timed out");
            thread::sleep(Duration::from_millis(20));
        }
    }
}
