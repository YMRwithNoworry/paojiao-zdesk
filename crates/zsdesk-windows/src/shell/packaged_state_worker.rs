//! Isolated slow-state protocol and STA evaluator. No command invocation.
use super::{
    packaged_commands::{self, Registration},
    pidl,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    cell::RefCell,
    collections::HashSet,
    ffi::{c_void, OsString},
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};
use windows::{
    core::{
        implement, Error, IUnknown, IUnknownImpl, Interface, Ref, Result, BOOL, GUID, HRESULT,
        PCWSTR, PWSTR,
    },
    Management::Deployment::PackageManager,
    Win32::{
        Foundation::{HWND, RECT},
        System::{
            Com::{
                CoCreateInstance, CoTaskMemFree, IErrorLog, IServiceProvider,
                IServiceProvider_Impl,
                StructuredStorage::{IPropertyBag, IPropertyBag_Impl},
                CLSCTX_INPROC_SERVER,
            },
            Ole::{IObjectWithSite, IOleWindow, IOleWindow_Impl, OleInitialize, OleUninitialize},
            Threading::{
                GetCurrentProcess, GetCurrentThreadId, GetProcessTimes, OpenProcess,
                QueryFullProcessImageNameW, TerminateProcess, PROCESS_NAME_WIN32,
                PROCESS_QUERY_LIMITED_INFORMATION,
            },
            Variant::VARIANT,
            WinRT::{RoInitialize, RoUninitialize, RO_INIT_SINGLETHREADED},
        },
        UI::{Shell::*, WindowsAndMessaging::*},
    },
};
use zsdesk_core::model::explorer::ShellViewState;

pub const WORKER_FLAG: &str = "--zsdesk-packaged-state-worker";
const MAX_REQUEST: u64 = 16 * 1024 * 1024;
const DEADLINE: Duration = Duration::from_secs(5);
const PENDING: HRESULT = HRESULT(0x8000000au32 as i32);
fn fail(code: u32, text: &str) -> Error {
    Error::new(HRESULT(code as i32), text)
}
fn io(error: std::io::Error) -> Error {
    fail(0x80004005, &error.to_string())
}
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
fn guid_text(value: GUID) -> String {
    format!(
        "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        value.data1,
        value.data2,
        value.data3,
        value.data4[0],
        value.data4[1],
        value.data4[2],
        value.data4[3],
        value.data4[4],
        value.data4[5],
        value.data4[6],
        value.data4[7]
    )
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct WireRegistration {
    pub family: String,
    pub full_name: String,
    pub application: String,
    pub verb: String,
    pub selector: String,
    pub clsid: String,
}
impl From<&Registration> for WireRegistration {
    fn from(value: &Registration) -> Self {
        Self {
            family: value.package_family.clone(),
            full_name: value.package_full_name.clone(),
            application: value.application_id.clone(),
            verb: value.verb_id.clone(),
            selector: value.item_type.clone(),
            clsid: guid_text(value.clsid),
        }
    }
}
impl WireRegistration {
    fn registration(&self) -> Result<Registration> {
        Ok(Registration {
            package_family: self.family.clone(),
            package_full_name: self.full_name.clone(),
            application_id: self.application.clone(),
            verb_id: self.verb.clone(),
            item_type: self.selector.clone(),
            clsid: GUID::try_from(self.clsid.as_str())?,
        })
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) enum Origin {
    Folder(Vec<u8>),
    Results,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct SelectedItem {
    pub absolute: Vec<u8>,
    pub relative: Vec<u8>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Snapshot {
    pub origin: Origin,
    pub selected: Vec<SelectedItem>,
    pub background: bool,
    pub layout: Option<ShellViewState>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct ChildStep {
    pub ordinal: u32,
    pub canonical: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Request {
    pub schema: u32,
    pub nonce: String,
    pub parent: u32,
    pub parent_created: String,
    pub generation: u64,
    pub command_id: u32,
    pub registration: WireRegistration,
    pub context: Snapshot,
    pub children: Vec<ChildStep>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Response {
    pub schema: u32,
    pub nonce: String,
    pub generation: u64,
    pub command_id: u32,
    pub request_sha256: String,
    pub pid: u32,
    pub created: String,
    pub hresult: i32,
    pub state: Option<u32>,
    pub attempts: u32,
    pub elapsed_ms: u64,
    pub phase: String,
    pub optional_properties: Vec<String>,
    pub error: Option<String>,
}
impl Request {
    pub(super) fn validate(&self) -> Result<()> {
        if self.schema != 1
            || self.parent == 0
            || self.generation == 0
            || self.command_id == 0
            || self.command_id > 0x6fff
            || self.parent_created.parse::<u64>().is_err()
            || self.nonce.len() != 32
            || !self.nonce.bytes().all(|ch| ch.is_ascii_hexdigit())
            || self.children.len() > 16
        {
            return Err(fail(0x80070057, "invalid slow-state envelope"));
        }
        let registration = self.registration.registration()?;
        if registration.clsid == GUID::zeroed() {
            return Err(fail(0x80070057, "null command class"));
        }
        for value in [
            &self.registration.family,
            &self.registration.full_name,
            &self.registration.application,
            &self.registration.verb,
            &self.registration.selector,
        ] {
            if value.is_empty() || value.len() > 4096 || value.contains('\0') {
                return Err(fail(0x80070057, "invalid registration field"));
            }
        }
        if self.context.selected.len() > 10_000
            || self.context.background && !self.context.selected.is_empty()
            || !self.context.background && self.context.selected.is_empty()
        {
            return Err(fail(0x80070057, "invalid context selection"));
        }
        let mut bytes = 0usize;
        let mut identities = HashSet::new();
        if let Origin::Folder(folder) = &self.context.origin {
            if !pidl::valid(folder) {
                return Err(fail(0x80070057, "invalid source folder PIDL"));
            }
            bytes += folder.len();
        }
        for item in &self.context.selected {
            if !pidl::valid(&item.absolute)
                || !pidl::valid(&item.relative)
                || item.relative.len() <= 2
                || !identities.insert(&item.absolute)
            {
                return Err(fail(0x80070057, "invalid or repeated selected PIDL"));
            }
            bytes = bytes.saturating_add(item.absolute.len() + item.relative.len());
        }
        if bytes > 4 * 1024 * 1024 {
            return Err(fail(0x8007000e, "selection snapshot exceeds bound"));
        }
        for step in &self.children {
            if step.ordinal >= 2048 || GUID::try_from(step.canonical.as_str())? == GUID::zeroed() {
                return Err(fail(
                    0x80004002,
                    "slow subcommand identity cannot be verified",
                ));
            }
        }
        Ok(())
    }
}

struct Pidl(*mut Common::ITEMIDLIST);
impl Drop for Pidl {
    fn drop(&mut self) {
        unsafe {
            CoTaskMemFree(Some(self.0.cast()));
        }
    }
}
fn pump() {
    let mut message = MSG::default();
    for _ in 0..128 {
        if !unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
            break;
        }
        if message.message != WM_QUIT {
            unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }
}
fn checkpoint(deadline: Instant) -> Result<()> {
    if Instant::now() >= deadline {
        Err(fail(0x800705b4, "slow-state context timed out"))
    } else {
        Ok(())
    }
}
fn materializing(error: &Error) -> bool {
    matches!(
        error.code().0 as u32,
        0x8000000a | 0x80004005 | 0x80070490 | 0x80070057
    )
}
fn same(first: &IShellItem, second: &IShellItem) -> bool {
    unsafe { first.Compare(second, SICHINT_CANONICAL.0 as u32) }.is_ok_and(|order| order == 0)
}
pub(super) fn capture(
    view: &IShellView,
    selection: Option<&IShellItemArray>,
    background: bool,
) -> Result<Snapshot> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let folder: IFolderView2 = view.cast()?;
    let origin = if unsafe { folder.GetFolder::<IResultsFolder>() }.is_ok() {
        Origin::Results
    } else {
        let persisted: IPersistFolder2 = unsafe { folder.GetFolder() }?;
        let item = Pidl(unsafe { persisted.GetCurFolder() }?);
        Origin::Folder(
            pidl::checked_pidl_bytes(item.0)
                .ok_or_else(|| fail(0x80070057, "unavailable folder identity"))?,
        )
    };
    let mut selected = Vec::new();
    if !background {
        let expected = selection.ok_or_else(|| fail(0x80070057, "missing selected-item array"))?;
        let count = unsafe { expected.GetCount() }?;
        if count == 0 || count > 10_000 {
            return Err(fail(0x80070057, "selection count unsupported"));
        }
        let expected: Vec<_> = (0..count)
            .map(|index| unsafe { expected.GetItemAt(index) })
            .collect::<Result<_>>()?;
        let mut used = vec![false; count as usize];
        let parent: IShellFolder = unsafe { folder.GetFolder() }?;
        let enumeration: IEnumIDList = unsafe { folder.Items(SVGIO_SELECTION) }?;
        loop {
            checkpoint(deadline)?;
            let mut raw = [std::ptr::null_mut()];
            let mut fetched = 0;
            unsafe { enumeration.Next(&mut raw, Some(&mut fetched)) }.ok()?;
            if fetched == 0 {
                break;
            }
            let relative = Pidl(raw[0]);
            let bytes = pidl::checked_pidl_bytes(relative.0)
                .ok_or_else(|| fail(0x80070057, "invalid relative selection identity"))?;
            let item: IShellItem = unsafe { SHCreateItemWithParent(None, &parent, relative.0) }?;
            let index = expected
                .iter()
                .enumerate()
                .find(|(index, target)| !used[*index] && same(&item, target))
                .map(|(index, _)| index)
                .ok_or_else(|| fail(0x800704c7, "selection changed during snapshot"))?;
            used[index] = true;
            selected.push(SelectedItem {
                absolute: pidl::item_pidl(&item)
                    .ok_or_else(|| fail(0x80070057, "unavailable absolute item identity"))?,
                relative: bytes,
            });
        }
        if selected.len() != expected.len() {
            return Err(fail(0x800704c7, "selection changed during snapshot"));
        }
    }
    Ok(Snapshot {
        origin,
        selected,
        background,
        layout: super::view_state::capture_view_layout(view),
    })
}

#[link(name = "user32")]
unsafe extern "system" {
    fn CreateDesktopW(
        name: *const u16,
        device: *const u16,
        mode: *const c_void,
        flags: u32,
        access: u32,
        security: *const c_void,
    ) -> *mut c_void;
    fn SetThreadDesktop(desktop: *mut c_void) -> i32;
    fn GetThreadDesktop(thread: u32) -> *mut c_void;
    fn CloseDesktop(desktop: *mut c_void) -> i32;
}
struct Desktop {
    original: *mut c_void,
    owned: *mut c_void,
}
impl Desktop {
    fn enter(nonce: &str) -> Result<Self> {
        let original = unsafe { GetThreadDesktop(GetCurrentThreadId()) };
        let name = wide(&format!(
            "ZSDesk.PackageState.{}.{}",
            std::process::id(),
            nonce
        ));
        let owned = unsafe {
            CreateDesktopW(
                name.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                0x000f00ff,
                std::ptr::null(),
            )
        };
        if owned.is_null() {
            return Err(Error::from_thread());
        }
        let value = Self { original, owned };
        if unsafe { SetThreadDesktop(owned) } == 0 {
            return Err(Error::from_thread());
        }
        Ok(value)
    }
}
impl Drop for Desktop {
    fn drop(&mut self) {
        unsafe {
            SetThreadDesktop(self.original);
            CloseDesktop(self.owned);
        }
    }
}
struct Apartment;
impl Apartment {
    fn enter() -> Result<Self> {
        unsafe { OleInitialize(None) }?;
        if let Err(error) = unsafe { RoInitialize(RO_INIT_SINGLETHREADED) } {
            unsafe {
                OleUninitialize();
            }
            return Err(error);
        }
        Ok(Self)
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe {
            RoUninitialize();
            OleUninitialize();
        }
    }
}
struct View {
    browser: IExplorerBrowser,
    view: IShellView,
    window: HWND,
}
impl Drop for View {
    fn drop(&mut self) {
        let _ = unsafe { self.browser.Destroy() };
        let _ = unsafe { DestroyWindow(self.window) };
    }
}
impl View {
    fn build(snapshot: &Snapshot, deadline: Instant) -> Result<Self> {
        let window = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                windows::core::w!("STATIC"),
                windows::core::w!(""),
                WS_POPUP | WS_CLIPCHILDREN,
                0,
                0,
                640,
                480,
                None,
                None,
                None,
                None,
            )
        }?;
        let browser: IExplorerBrowser =
            match unsafe { CoCreateInstance(&ExplorerBrowser, None, CLSCTX_INPROC_SERVER) } {
                Ok(value) => value,
                Err(error) => {
                    let _ = unsafe { DestroyWindow(window) };
                    return Err(error);
                }
            };
        let build = (|| -> Result<IShellView> {
            unsafe {
                browser.SetOptions(EBO_NOBORDER | EBO_NOPERSISTVIEWSTATE)?;
                browser.Initialize(
                    window,
                    &RECT {
                        left: 0,
                        top: 0,
                        right: 640,
                        bottom: 480,
                    },
                    None,
                )?;
            }
            let target = match &snapshot.origin {
                Origin::Folder(folder) => {
                    let item = pidl::item_from_pidl(folder)?;
                    unsafe { browser.BrowseToObject(&item, 0) }?;
                    Some(item)
                }
                Origin::Results => {
                    unsafe { browser.FillFromObject(None, EBF_NODROPTARGET) }?;
                    None
                }
            };
            let view = loop {
                checkpoint(deadline)?;
                pump();
                if let Ok(view) = unsafe { browser.GetCurrentView::<IShellView>() } {
                    if let Ok(folder) = view.cast::<IFolderView2>() {
                        let matches = if let Some(target) = &target {
                            unsafe { folder.GetFolder::<IPersistFolder2>() }
                                .ok()
                                .and_then(|persist| unsafe { persist.GetCurFolder() }.ok())
                                .map(Pidl)
                                .and_then(|current| {
                                    unsafe { SHCreateItemFromIDList::<IShellItem>(current.0) }.ok()
                                })
                                .is_some_and(|current| same(&current, target))
                        } else {
                            unsafe { folder.GetFolder::<IResultsFolder>() }.is_ok()
                        };
                        if matches {
                            break view;
                        }
                    }
                }
                thread::sleep(Duration::from_millis(5));
            };
            if let Some(layout) = &snapshot.layout {
                super::view_state::restore_view_layout(&view, layout);
            }
            let folder: IFolderView2 = view.cast()?;
            unsafe { folder.SelectItem(-1, SVSI_DESELECTOTHERS.0 as u32) }?;
            if !snapshot.background {
                let mut relatives = Vec::<Pidl>::new();
                if matches!(snapshot.origin, Origin::Results) {
                    let results: IResultsFolder = unsafe { folder.GetFolder() }?;
                    for item in &snapshot.selected {
                        checkpoint(deadline)?;
                        let mut relative = std::ptr::null_mut();
                        unsafe {
                            results.AddIDList(item.absolute.as_ptr().cast(), Some(&mut relative))
                        }?;
                        if relative.is_null() {
                            return Err(fail(0x80004003, "results item identity unavailable"));
                        }
                        relatives.push(Pidl(relative));
                    }
                }
                let pointers: Vec<_> = snapshot
                    .selected
                    .iter()
                    .enumerate()
                    .map(|(index, item)| {
                        relatives
                            .get(index)
                            .map_or(item.relative.as_ptr().cast(), |relative| {
                                relative.0 as *const _
                            })
                    })
                    .collect();
                let expected: Vec<_> = snapshot
                    .selected
                    .iter()
                    .map(|item| pidl::item_from_pidl(&item.absolute))
                    .collect::<Result<_>>()?;
                loop {
                    checkpoint(deadline)?;
                    pump();
                    if let Err(error) = unsafe {
                        folder.SelectAndPositionItems(
                            pointers.len() as u32,
                            pointers.as_ptr(),
                            None,
                            (SVSI_SELECT.0 | SVSI_DESELECTOTHERS.0 | SVSI_NOTAKEFOCUS.0) as u32,
                        )
                    } {
                        if !materializing(&error) {
                            return Err(error);
                        }
                    }
                    let matched = (|| -> Result<bool> {
                        let selected = unsafe { folder.GetSelection(false) }?;
                        if unsafe { selected.GetCount() }? as usize != expected.len() {
                            return Ok(false);
                        }
                        let mut used = vec![false; expected.len()];
                        for index in 0..expected.len() {
                            let item = unsafe { selected.GetItemAt(index as u32) }?;
                            let Some(found) = expected
                                .iter()
                                .enumerate()
                                .find(|(index, target)| !used[*index] && same(&item, target))
                                .map(|(index, _)| index)
                            else {
                                return Ok(false);
                            };
                            used[found] = true;
                        }
                        Ok(true)
                    })();
                    match matched {
                        Ok(true) => break,
                        Ok(false) => (),
                        Err(error) if materializing(&error) => (),
                        Err(error) => return Err(error),
                    }
                    thread::sleep(Duration::from_millis(10));
                }
            }
            Ok(view)
        })();
        match build {
            Ok(view) => Ok(Self {
                browser,
                view,
                window,
            }),
            Err(error) => {
                let _ = unsafe { browser.Destroy() };
                let _ = unsafe { DestroyWindow(window) };
                Err(error)
            }
        }
    }
}
#[implement(IServiceProvider, IOleWindow, Agile = false)]
struct Site {
    browser: IExplorerBrowser,
    view: IShellView,
    owner: HWND,
}
impl IServiceProvider_Impl for Site_Impl {
    fn QueryService(
        &self,
        service: *const GUID,
        iid: *const GUID,
        out: *mut *mut c_void,
    ) -> Result<()> {
        if service.is_null() || iid.is_null() || out.is_null() {
            return Err(fail(0x80004003, "null site request"));
        }
        unsafe {
            *out = std::ptr::null_mut();
        }
        let service = unsafe { *service };
        if service == IFolderView::IID {
            return unsafe { self.view.query(iid, out).ok() };
        }
        if service == SID_STopLevelBrowser || service == IShellBrowser::IID {
            return unsafe { self.browser.query(iid, out).ok() };
        }
        if service == IOleWindow::IID {
            let window: IOleWindow = self.to_interface();
            return unsafe { window.query(iid, out).ok() };
        }
        if let Ok(provider) = self.browser.cast::<IServiceProvider>() {
            return unsafe {
                (provider.vtable().QueryService)(provider.as_raw(), &service, iid, out).ok()
            };
        }
        Err(fail(0x80004002, "site service unsupported"))
    }
}
impl IOleWindow_Impl for Site_Impl {
    fn GetWindow(&self) -> Result<HWND> {
        Ok(self.owner)
    }
    fn ContextSensitiveHelp(&self, _: BOOL) -> Result<()> {
        Err(fail(0x80004001, "help unsupported"))
    }
}
#[implement(IPropertyBag, Agile = false)]
struct Properties {
    reads: Rc<RefCell<Vec<String>>>,
}
impl IPropertyBag_Impl for Properties_Impl {
    fn Read(&self, name: &PCWSTR, _: *mut VARIANT, _: Ref<IErrorLog>) -> Result<()> {
        if let Ok(name) = unsafe { name.to_string() } {
            let mut reads = self.reads.borrow_mut();
            if reads.len() < 128 {
                reads.push(name);
            }
        }
        Err(fail(0x80070057, "optional property unavailable"))
    }
    fn Write(&self, _: &PCWSTR, _: *const VARIANT) -> Result<()> {
        Err(fail(0x80030005, "properties are read only"))
    }
}
struct Command {
    value: IExplorerCommand,
}
impl Drop for Command {
    fn drop(&mut self) {
        if let Ok(site) = self.value.cast::<IObjectWithSite>() {
            let _ = unsafe { site.SetSite(None) };
        }
    }
}
fn evaluate(request: &Request, response: &mut Response, deadline: Instant) -> Result<u32> {
    let _desktop = Desktop::enter(&request.nonce)?;
    let _apartment = Apartment::enter()?;
    response.phase = "registration".into();
    let registration = request.registration.registration()?;
    let manager = PackageManager::new()?;
    let package = manager.FindPackageByUserSecurityIdPackageFullName(
        &windows::core::HSTRING::new(),
        &windows::core::HSTRING::from(registration.package_full_name.as_str()),
    ).map_err(|error| if error.code().is_ok() {
        fail(0x80040154, "registered package was not found")
    } else { error })?;
    if !package.Status()?.VerifyIsOK()?
        || package.Id()?.FamilyName()?.to_string_lossy() != registration.package_family
        || package.Id()?.FullName()?.to_string_lossy() != registration.package_full_name
    {
        return Err(fail(0x80040154, "registered command no longer available"));
    }
    response.phase = "view".into();
    let view = View::build(&request.context, deadline)?;
    let site: IUnknown = Site {
        browser: view.browser.clone(),
        view: view.view.clone(),
        owner: view.window,
    }
    .into();
    let items = if request.context.background {
        None
    } else {
        let pointers: Vec<_> = request
            .context
            .selected
            .iter()
            .map(|item| item.absolute.as_ptr().cast())
            .collect();
        Some(unsafe { SHCreateShellItemArrayFromIDLists(&pointers) }?)
    };
    response.phase = "activation".into();
    let root = Command {
        value: packaged_commands::activate(&registration)?,
    };
    if let Ok(initializer) = root.value.cast::<IInitializeCommand>() {
        response.phase = "Initialize".into();
        let reads = Rc::new(RefCell::new(Vec::new()));
        let bag: IPropertyBag = Properties {
            reads: reads.clone(),
        }
        .into();
        let name = wide(&registration.verb_id);
        let initialized = unsafe { initializer.Initialize(PCWSTR(name.as_ptr()), &bag) };
        response.optional_properties = reads.borrow().clone();
        packaged_commands::optional_initialization(initialized)?;
    }
    if let Ok(object) = root.value.cast::<IObjectWithSite>() {
        response.phase = "SetSite".into();
        packaged_commands::optional_initialization(unsafe { object.SetSite(&site) })?;
    }
    let mut chain = vec![root];
    for step in &request.children {
        response.phase = "subcommand".into();
        checkpoint(deadline)?;
        let enumeration = unsafe { chain.last().unwrap().value.EnumSubCommands() }?;
        let mut selected = None;
        for index in 0..=step.ordinal {
            let mut items = [None];
            let mut fetched = 0;
            unsafe { enumeration.Next(&mut items, Some(&mut fetched)) }.ok()?;
            if fetched == 0 {
                return Err(fail(0x80070490, "subcommand disappeared"));
            }
            if index == step.ordinal {
                selected = items[0].take();
            }
        }
        let command = Command {
            value: selected.ok_or_else(|| fail(0x80004003, "subcommand missing"))?,
        };
        let expected = GUID::try_from(step.canonical.as_str())?;
        if unsafe { command.value.GetCanonicalName() }? != expected {
            return Err(fail(0x800704c7, "subcommand identity changed"));
        }
        if let Ok(object) = command.value.cast::<IObjectWithSite>() {
            packaged_commands::optional_initialization(unsafe { object.SetSite(&site) })?;
        }
        chain.push(command);
    }
    response.phase = "GetState(TRUE)".into();
    for attempt in 1..=3 {
        checkpoint(deadline)?;
        response.attempts = attempt;
        let result = unsafe { chain.last().unwrap().value.GetState(items.as_ref(), true) };
        match result {
            Err(error) if error.code() == PENDING && attempt < 3 => {
                pump();
                thread::sleep(Duration::from_millis(75));
            }
            other => return other,
        }
    }
    Err(Error::from_hresult(PENDING))
}

fn created(process: windows::Win32::Foundation::HANDLE) -> Result<String> {
    let mut first = Default::default();
    let mut end = Default::default();
    let mut kernel = Default::default();
    let mut user = Default::default();
    unsafe { GetProcessTimes(process, &mut first, &mut end, &mut kernel, &mut user) }?;
    Ok(((u64::from(first.dwHighDateTime) << 32) | u64::from(first.dwLowDateTime)).to_string())
}
fn parent_matches(request: &Request) -> Result<()> {
    use windows::Win32::Foundation::CloseHandle;
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, request.parent) }?;
    let result = (|| {
        if created(process)? != request.parent_created {
            return Err(fail(0x800704c7, "parent process was replaced"));
        }
        let mut path = vec![0u16; 32768];
        let mut length = path.len() as u32;
        unsafe {
            QueryFullProcessImageNameW(
                process,
                PROCESS_NAME_WIN32,
                PWSTR(path.as_mut_ptr()),
                &mut length,
            )
        }?;
        let actual = String::from_utf16(&path[..length as usize])
            .map_err(|_| fail(0x80070057, "parent image path invalid"))?;
        let current = std::env::current_exe().map_err(io)?;
        if !actual.eq_ignore_ascii_case(&current.to_string_lossy()) {
            return Err(fail(0x80070005, "parent image differs"));
        }
        Ok(())
    })();
    let _ = unsafe { CloseHandle(process) };
    result
}
fn read_request(path: &Path) -> Result<(Request, String)> {
    if !path.is_absolute() || path.file_name().is_none_or(|name| name != "request.json") {
        return Err(fail(0x80070057, "invalid request path"));
    }
    use std::os::windows::fs::MetadataExt;
    let metadata = fs::symlink_metadata(path).map_err(io)?;
    if metadata.file_attributes() & 0x400 != 0 || metadata.len() > MAX_REQUEST {
        return Err(fail(0x80070057, "request file boundary invalid"));
    }
    let mut bytes = Vec::new();
    fs::File::open(path)
        .map_err(io)?
        .take(MAX_REQUEST + 1)
        .read_to_end(&mut bytes)
        .map_err(io)?;
    if bytes.len() as u64 > MAX_REQUEST {
        return Err(fail(0x8007000e, "request exceeds bound"));
    }
    let request: Request =
        serde_json::from_slice(&bytes).map_err(|error| fail(0x80070057, &error.to_string()))?;
    request.validate()?;
    let directory = path
        .parent()
        .ok_or_else(|| fail(0x80070057, "request directory unavailable"))?;
    if fs::symlink_metadata(directory)
        .map_err(io)?
        .file_attributes()
        & 0x400
        != 0
        || directory.file_name().is_none_or(|name| {
            name.to_string_lossy() != format!("{}-{}", request.parent, request.nonce)
        })
    {
        return Err(fail(0x80070057, "request ownership directory mismatch"));
    }
    let marker = directory.join("owner");
    let marker_metadata = fs::symlink_metadata(&marker).map_err(io)?;
    if marker_metadata.file_attributes() & 0x400 != 0 || marker_metadata.len() > 128 {
        return Err(fail(0x80070005, "invalid owner marker"));
    }
    if fs::read_to_string(marker).map_err(io)?
        != format!("ZSDesk.PackageState.v1\n{}", request.nonce)
    {
        return Err(fail(0x80070005, "request owner marker mismatch"));
    }
    parent_matches(&request)?;
    Ok((request, format!("{:x}", Sha256::digest(&bytes))))
}

struct Watchdog {
    done: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Watchdog {
    fn start(start: Instant) -> Result<Self> {
        let done = Arc::new(AtomicBool::new(false));
        let wake = done.clone();
        let worker = thread::Builder::new()
            .name("zsdesk-package-state-deadline".into())
            .spawn(move || {
                while !wake.load(Ordering::Acquire) {
                    if start.elapsed() > DEADLINE + Duration::from_secs(1) {
                        let _ = unsafe { TerminateProcess(GetCurrentProcess(), 0xE0530001) };
                        return;
                    }
                    thread::park_timeout(Duration::from_millis(25));
                }
            })
            .map_err(io)?;
        Ok(Self {
            done,
            thread: Some(worker),
        })
    }
}
impl Drop for Watchdog {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Release);
        if let Some(worker) = self.thread.take() {
            worker.thread().unpark();
            let _ = worker.join();
        }
    }
}

/// Receives arguments excluding argv[0]. The application must call this before
/// creating windows/COM apartments. This worker only queries state, never Invoke.
pub fn maybe_handle_packaged_state_worker(args: &[OsString]) -> Option<Result<()>> {
    if args.first().is_none_or(|value| value != WORKER_FLAG) {
        return None;
    }
    Some((|| {
        if args.len() != 2 {
            return Err(fail(0x80070057, "invalid slow-state worker arguments"));
        }
        let path = PathBuf::from(&args[1]);
        let (request, request_sha256) = read_request(&path)?;
        let start = Instant::now();
        let _watchdog = Watchdog::start(start)?;
        let mut response = Response {
            schema: 1,
            nonce: request.nonce.clone(),
            generation: request.generation,
            command_id: request.command_id,
            request_sha256,
            pid: std::process::id(),
            created: created(unsafe { GetCurrentProcess() })?,
            hresult: 0,
            state: None,
            attempts: 0,
            elapsed_ms: 0,
            phase: "start".into(),
            optional_properties: Vec::new(),
            error: None,
        };
        match evaluate(&request, &mut response, start + DEADLINE) {
            Ok(state) => response.state = Some(state),
            Err(error) => {
                // A WinRT null interface is represented by Error::empty(),
                // whose HRESULT is S_OK. An absent value is never a success.
                response.hresult = if error.code().is_err() { error.code().0 } else { 0x80004005u32 as i32 };
                response.error = Some(if error.code().is_ok() { "state query returned no value".into() }
                    else { error.message().chars().take(1024).collect() });
            }
        }
        response.elapsed_ms = start.elapsed().as_millis().min(u64::MAX as u128) as u64;
        let bytes =
            serde_json::to_vec(&response).map_err(|error| fail(0x80004005, &error.to_string()))?;
        let output = path.with_file_name("result.json");
        let temporary = path.with_file_name("result.tmp");
        if output.exists() {
            return Err(fail(0x800700b7, "state reply already exists"));
        }
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(io)?;
        file.write_all(&bytes).map_err(io)?;
        file.sync_all().map_err(io)?;
        drop(file);
        fs::rename(temporary, output).map_err(io)?;
        Ok(())
    })())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request() -> Request {
        Request {
            schema: 1,
            nonce: "a".repeat(32),
            parent: 1,
            parent_created: "123".into(),
            generation: 1,
            command_id: 0x6800,
            registration: WireRegistration {
                family: "family".into(),
                full_name: "full".into(),
                application: "App".into(),
                verb: "Verb".into(),
                selector: "Directory\\Background".into(),
                clsid: "01234567-89ab-cdef-0123-456789abcdef".into(),
            },
            context: Snapshot {
                origin: Origin::Folder(vec![0, 0]),
                selected: Vec::new(),
                background: true,
                layout: None,
            },
            children: Vec::new(),
        }
    }
    #[test]
    fn envelope_rejects_stale_or_ambiguous_identity_without_shell_calls() {
        let mut value = request();
        assert!(value.validate().is_ok());
        value.generation = 0;
        assert!(value.validate().is_err());
        value.generation = 1;
        value.children.push(ChildStep {
            ordinal: 0,
            canonical: guid_text(GUID::zeroed()),
        });
        assert!(value.validate().is_err());
        value.children.clear();
        value.context.selected.push(SelectedItem {
            absolute: vec![0, 0],
            relative: vec![0, 0],
        });
        assert!(value.validate().is_err());
    }
    #[test]
    fn envelope_preserves_opaque_pidls_and_registration_fields() {
        let value = request();
        let bytes = serde_json::to_vec(&value).unwrap();
        let decoded: Request = serde_json::from_slice(&bytes).unwrap();
        decoded.validate().unwrap();
        assert_eq!(decoded.registration.clsid, value.registration.clsid);
        assert_eq!(decoded.nonce, value.nonce);
        assert!(matches!(decoded.context.origin,Origin::Folder(ref pidl)if pidl==&[0,0]));
    }
}

// Hidden worker ownership, response validation and cancellation. COM interfaces
// are never transferred as raw pointers between processes or threads.
mod client {
    use super::*;
    use std::{
        cell::RefCell,
        collections::VecDeque,
        ffi::OsStr,
        marker::PhantomData,
        mem::{size_of, size_of_val},
        os::windows::{
            ffi::OsStrExt,
            fs::MetadataExt,
            io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle},
        },
        sync::{
            atomic::{AtomicU64, AtomicUsize},
            Mutex, OnceLock,
        },
    };
    use windows::Win32::{
        Foundation::{DuplicateHandle, DUPLICATE_SAME_ACCESS, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT},
        Globalization::{CompareStringOrdinal, CSTR_GREATER_THAN, CSTR_LESS_THAN},
        System::{
            Com::CoCreateGuid,
            Environment::{FreeEnvironmentStringsW, GetEnvironmentStringsW},
            JobObjects::*,
            Threading::*,
        },
    };
    const CONCURRENT: usize = 2;
    const WAIT_LIMIT: Duration = Duration::from_secs(7);
    const TEST_REQUEST: &str = "ZSDESK_PACKAGED_STATE_TEST_REQUEST";
    static ACTIVE: AtomicUsize = AtomicUsize::new(0);
    static ENTRY_ID: AtomicU64 = AtomicU64::new(1);
    static CLEANUP_ERRORS: OnceLock<Mutex<VecDeque<String>>> = OnceLock::new();
    thread_local! {static TEST_ENTRIES:RefCell<Vec<(u64,Vec<OsString>)>>=const{RefCell::new(Vec::new())};}

    /// Install only inside the application's cfg(test) menu-worker entry, on the
    /// STA which creates the overlay. Descendant libtest processes do not run main.
    pub struct TestEntryGuard {
        id: u64,
        _thread: PhantomData<Rc<()>>,
    }
    impl Drop for TestEntryGuard {
        fn drop(&mut self) {
            TEST_ENTRIES.with(|entries| entries.borrow_mut().retain(|(id, _)| *id != self.id));
        }
    }
    pub fn test_entry(prefix: &[OsString]) -> Result<TestEntryGuard> {
        if prefix.is_empty() || prefix.len() > 8 {
            return Err(fail(0x80070057, "invalid state test-entry prefix"));
        }
        for part in prefix {
            if part.encode_wide().any(|unit| unit == 0) {
                return Err(fail(0x80070057, "NUL in state test-entry prefix"));
            }
        }
        let id = ENTRY_ID.fetch_add(1, Ordering::Relaxed);
        TEST_ENTRIES.with(|entries| entries.borrow_mut().push((id, prefix.to_vec())));
        Ok(TestEntryGuard {
            id,
            _thread: PhantomData,
        })
    }
    /// Called by a single explicitly named, ignored application test function.
    pub fn run_test_entry() -> Result<()> {
        let path = std::env::var_os(TEST_REQUEST)
            .ok_or_else(|| fail(0x80070057, "state test request missing"))?;
        maybe_handle_packaged_state_worker(&[OsString::from(WORKER_FLAG), path])
            .ok_or_else(|| fail(0x80004005, "state test entry was not handled"))?
    }
    pub fn pending() -> usize {
        ACTIVE.load(Ordering::Acquire)
    }
    pub fn cleanup_errors() -> Vec<String> {
        CLEANUP_ERRORS
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .cloned()
            .collect()
    }
    fn note_cleanup(error: impl ToString) {
        let mut errors = CLEANUP_ERRORS
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if errors.len() == 16 {
            errors.pop_front();
        }
        errors.push_back(error.to_string().chars().take(1024).collect());
    }
    struct Permit;
    impl Permit {
        fn acquire() -> Option<Self> {
            let mut count = ACTIVE.load(Ordering::Acquire);
            loop {
                if count >= CONCURRENT {
                    return None;
                }
                match ACTIVE.compare_exchange_weak(
                    count,
                    count + 1,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => return Some(Self),
                    Err(next) => count = next,
                }
            }
        }
    }
    impl Drop for Permit {
        fn drop(&mut self) {
            ACTIVE.fetch_sub(1, Ordering::AcqRel);
        }
    }
    struct Workspace {
        directory: PathBuf,
        nonce: String,
    }
    impl Workspace {
        fn root() -> Result<PathBuf> {
            let root = if let Some(root) = std::env::var_os("ZSDESK_PACKAGED_STATE_ROOT") {
                PathBuf::from(root)
            } else {
                let value =
                    unsafe { SHGetKnownFolderPath(&FOLDERID_LocalAppData, KF_FLAG_DEFAULT, None) }?;
                let path = unsafe { value.to_string() };
                unsafe {
                    CoTaskMemFree(Some(value.0.cast()));
                }
                PathBuf::from(path?).join("ZSDesk").join("package-state")
            };
            if !root.is_absolute() {
                return Err(fail(0x80070057, "state workspace root must be absolute"));
            }
            fs::create_dir_all(&root).map_err(io)?;
            fs::canonicalize(root).map_err(io)
        }
        fn create(nonce: &str) -> Result<Self> {
            let root = Self::root()?;
            let directory = root.join(format!("{}-{nonce}", std::process::id()));
            fs::create_dir(&directory).map_err(io)?;
            let value = Self {
                directory,
                nonce: nonce.into(),
            };
            let mut owner = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(value.directory.join("owner"))
                .map_err(io)?;
            owner
                .write_all(format!("ZSDesk.PackageState.v1\n{nonce}").as_bytes())
                .map_err(io)?;
            Ok(value)
        }
        fn validate(&self) -> Result<()> {
            if !self.directory.is_absolute()
                || fs::symlink_metadata(&self.directory)
                    .map_err(io)?
                    .file_attributes()
                    & 0x400
                    != 0
            {
                return Err(fail(0x80070057, "state workspace changed"));
            }
            let owner = self.directory.join("owner");
            let metadata = fs::symlink_metadata(&owner).map_err(io)?;
            if metadata.file_attributes() & 0x400 != 0
                || metadata.len() > 128
                || fs::read_to_string(owner).map_err(io)?
                    != format!("ZSDesk.PackageState.v1\n{}", self.nonce)
            {
                return Err(fail(0x80070005, "state workspace owner changed"));
            }
            Ok(())
        }
        fn remove(self) {
            if let Err(error) = self.validate() {
                note_cleanup(format!("{}: {error}", self.directory.display()));
                return;
            }
            for name in ["request.json", "result.json", "result.tmp", "stderr.log"] {
                let path = self.directory.join(name);
                match fs::symlink_metadata(&path) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => {
                        note_cleanup(error);
                        return;
                    }
                    Ok(metadata) => {
                        if !metadata.is_file() || metadata.file_attributes() & 0x400 != 0 {
                            note_cleanup(format!("unexpected state file: {}", path.display()));
                            return;
                        }
                    }
                }
                if let Err(error) = fs::remove_file(path) {
                    note_cleanup(error);
                    return;
                }
            }
            let remaining = match fs::read_dir(&self.directory) {
                Ok(entries) => entries.collect::<std::result::Result<Vec<_>, _>>(),
                Err(error) => {
                    note_cleanup(error);
                    return;
                }
            };
            if !remaining
                .as_ref()
                .is_ok_and(|entries| entries.len() == 1 && entries[0].file_name() == "owner")
            {
                note_cleanup(format!(
                    "unrecognized state workspace members: {}",
                    self.directory.display()
                ));
                return;
            }
            if let Err(error) = fs::remove_file(self.directory.join("owner"))
                .and_then(|_| fs::remove_dir(&self.directory))
            {
                note_cleanup(error);
            }
        }
    }
    struct Child {
        process: OwnedHandle,
        _job: OwnedHandle,
        pid: u32,
        creation: String,
        _permit: Permit,
    }
    impl Child {
        fn handle(&self) -> HANDLE {
            HANDLE(self.process.as_raw_handle())
        }
        fn poll(&self) -> Result<Option<u32>> {
            match unsafe { WaitForSingleObject(self.handle(), 0) } {
                WAIT_OBJECT_0 => {
                    let mut exit = 0;
                    unsafe { GetExitCodeProcess(self.handle(), &mut exit) }?;
                    Ok(Some(exit))
                }
                WAIT_TIMEOUT => Ok(None),
                _ => Err(Error::from_thread()),
            }
        }
        fn cancel(&self) {
            if self.poll().is_ok_and(|state| state.is_some()) {
                return;
            }
            if let Err(error) = unsafe { TerminateProcess(self.handle(), 0xE0530002) } {
                if self.poll().is_ok_and(|state| state.is_some()) {
                    return;
                }
                note_cleanup(format!("state helper {} termination: {error}", self.pid));
            }
        }
    }
    struct Retired {
        child: Child,
        workspace: Workspace,
    }
    static RETIRED: OnceLock<Mutex<Vec<Retired>>> = OnceLock::new();
    static REAPER: AtomicBool = AtomicBool::new(false);
    fn retire(child: Child, workspace: Workspace) {
        child.cancel();
        RETIRED
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(Retired { child, workspace });
        if REAPER
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        if let Err(error) = thread::Builder::new()
            .name("zsdesk-package-state-reaper".into())
            .spawn(|| loop {
                let mut completed = Vec::new();
                let empty = {
                    let mut values = RETIRED
                        .get_or_init(Default::default)
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    let mut index = 0;
                    while index < values.len() {
                        if values[index].child.poll().is_ok_and(|exit| exit.is_some()) {
                            completed.push(values.swap_remove(index));
                        } else {
                            index += 1;
                        }
                    }
                    if values.is_empty() {
                        REAPER.store(false, Ordering::Release);
                        true
                    } else {
                        false
                    }
                };
                for value in completed {
                    value.workspace.remove();
                    drop(value.child);
                }
                if empty {
                    break;
                }
                thread::sleep(Duration::from_millis(25));
            })
        {
            REAPER.store(false, Ordering::Release);
            note_cleanup(format!("state reaper thread: {error}"));
        }
    }
    pub(in super::super) struct Ticket {
        child: Option<Child>,
        workspace: Option<Workspace>,
        pub request: Request,
        digest: String,
        started: Instant,
    }
    impl Ticket {
        pub fn pid(&self) -> u32 {
            self.child.as_ref().map_or(0, |child| child.pid)
        }
        pub fn poll(&mut self) -> Option<Result<Response>> {
            let child = self.child.as_ref()?;
            let exit = match child.poll() {
                Ok(Some(exit)) => exit,
                Ok(None) if self.started.elapsed() < WAIT_LIMIT => return None,
                Ok(None) => {
                    self.cancel();
                    return Some(Err(fail(
                        0x800705b4,
                        "packaged slow-state helper timed out",
                    )));
                }
                Err(error) => {
                    self.cancel();
                    return Some(Err(error));
                }
            };
            let result = (|| {
                let workspace = self
                    .workspace
                    .as_ref()
                    .ok_or_else(|| fail(0x80004005, "missing state workspace"))?;
                workspace.validate()?;
                if exit != 0 {
                    return Err(fail(
                        if exit == 0xE0530001 {
                            0x800705b4
                        } else {
                            0x80004005
                        },
                        &format!("packaged state helper exited 0x{exit:08X}"),
                    ));
                }
                let path = workspace.directory.join("result.json");
                let metadata=fs::symlink_metadata(&path).map_err(|error|fail(0x80004005,&format!("helper completed without a valid state reply; check the configured entry: {error}")))?;
                if !metadata.is_file()
                    || metadata.file_attributes() & 0x400 != 0
                    || metadata.len() > 256 * 1024
                {
                    return Err(fail(0x80070057, "state reply boundary invalid"));
                }
                let mut bytes = Vec::new();
                fs::File::open(path)
                    .map_err(io)?
                    .take(256 * 1024 + 1)
                    .read_to_end(&mut bytes)
                    .map_err(io)?;
                if bytes.len() > 256 * 1024 {
                    return Err(fail(0x8007000e, "state reply exceeds bound"));
                }
                let response: Response = serde_json::from_slice(&bytes)
                    .map_err(|error| fail(0x80070057, &error.to_string()))?;
                validate_response(
                    &self.request,
                    &self.digest,
                    child.pid,
                    &child.creation,
                    &response,
                )?;
                Ok(response)
            })();
            let child = self.child.take().unwrap();
            let workspace = self.workspace.take().unwrap();
            workspace.remove();
            drop(child);
            Some(result)
        }
        pub fn cancel(&mut self) {
            if let (Some(child), Some(workspace)) = (self.child.take(), self.workspace.take()) {
                retire(child, workspace);
            }
        }
    }
    impl Drop for Ticket {
        fn drop(&mut self) {
            self.cancel();
        }
    }
    pub(super) fn validate_response(
        request: &Request,
        digest: &str,
        pid: u32,
        creation: &str,
        response: &Response,
    ) -> Result<()> {
        if response.schema != 1
            || response.nonce != request.nonce
            || response.generation != request.generation
            || response.command_id != request.command_id
            || response.request_sha256 != digest
            || response.pid != pid
            || response.created != creation
            || response.attempts > 3
            || response.hresult == 0 && response.state.is_none()
            || response.hresult != 0 && response.state.is_some()
        {
            return Err(fail(0x80070057, &format!(
                "mismatched slow-state reply: schema={}, nonce={}, generation={}, command={}, digest={}, pid={} expected={}, created={} expected={}, attempts={}, hresult={}, state={:?}",
                response.schema, response.nonce == request.nonce, response.generation == request.generation,
                response.command_id == request.command_id, response.request_sha256 == digest,
                response.pid, pid, response.created, creation, response.attempts, response.hresult, response.state)));
        }
        Ok(())
    }

    fn units(value: &OsStr) -> Result<Vec<u16>> {
        let value: Vec<_> = value.encode_wide().collect();
        if value.contains(&0) {
            Err(fail(0x80070057, "NUL in worker argument"))
        } else {
            Ok(value)
        }
    }
    fn quoted(target: &mut Vec<u16>, value: &[u16]) {
        target.push(34);
        let mut slashes = 0;
        for &unit in value {
            if unit == 92 {
                slashes += 1;
                continue;
            }
            target.extend(std::iter::repeat_n(
                92,
                if unit == 34 { slashes * 2 + 1 } else { slashes },
            ));
            target.push(unit);
            slashes = 0;
        }
        target.extend(std::iter::repeat_n(92, slashes * 2));
        target.push(34);
    }
    fn command_line(executable: &Path, args: &[OsString]) -> Result<(Vec<u16>, Vec<u16>)> {
        let mut name = units(executable.as_os_str())?;
        let mut command = Vec::new();
        quoted(&mut command, &name);
        for argument in args {
            command.push(32);
            quoted(&mut command, &units(argument)?);
        }
        command.push(0);
        name.push(0);
        if command.len() > 32767 {
            return Err(fail(
                0x80070057,
                "worker command line exceeds Windows bound",
            ));
        }
        Ok((name, command))
    }
    fn env_key(value: &[u16]) -> &[u16] {
        let at = value
            .iter()
            .enumerate()
            .skip(1)
            .find(|(_, unit)| **unit == 61)
            .map_or(value.len(), |(index, _)| index);
        &value[..at]
    }
    fn ordinal(a: &[u16], b: &[u16]) -> std::cmp::Ordering {
        match unsafe { CompareStringOrdinal(a, b, true) } {
            CSTR_LESS_THAN => std::cmp::Ordering::Less,
            CSTR_GREATER_THAN => std::cmp::Ordering::Greater,
            _ => std::cmp::Ordering::Equal,
        }
    }
    fn environment(test_request: Option<&Path>) -> Result<Vec<u16>> {
        struct Env(PWSTR);
        impl Drop for Env {
            fn drop(&mut self) {
                let _ = unsafe { FreeEnvironmentStringsW(PCWSTR(self.0 .0)) };
            }
        }
        let raw = Env(unsafe { GetEnvironmentStringsW() });
        if raw.0.is_null() {
            return Err(Error::from_thread());
        }
        let mut rows = Vec::new();
        let mut offset = 0usize;
        loop {
            if offset >= 16 * 1024 * 1024 {
                return Err(fail(0x8007000e, "environment exceeds bound"));
            }
            if unsafe { *raw.0 .0.add(offset) } == 0 {
                break;
            }
            let first = offset;
            while offset < 16 * 1024 * 1024 && unsafe { *raw.0 .0.add(offset) } != 0 {
                offset += 1;
            }
            if offset >= 16 * 1024 * 1024 {
                return Err(fail(0x8007000e, "environment exceeds bound"));
            }
            rows.push(
                unsafe { std::slice::from_raw_parts(raw.0 .0.add(first), offset - first) }.to_vec(),
            );
            offset += 1;
        }
        let key: Vec<_> = TEST_REQUEST.encode_utf16().collect();
        rows.retain(|row| ordinal(env_key(row), &key) != std::cmp::Ordering::Equal);
        if let Some(path) = test_request {
            let mut row = key;
            row.push(61);
            row.extend(units(path.as_os_str())?);
            rows.push(row);
        }
        rows.sort_by(|a, b| ordinal(env_key(a), env_key(b)));
        let mut result = Vec::new();
        for row in rows {
            result.extend(row);
            result.push(0);
        }
        result.push(0);
        Ok(result)
    }
    fn inheritable(handle: RawHandle) -> Result<OwnedHandle> {
        let mut result = HANDLE::default();
        let process = unsafe { GetCurrentProcess() };
        unsafe {
            DuplicateHandle(
                process,
                HANDLE(handle),
                process,
                &mut result,
                0,
                true,
                DUPLICATE_SAME_ACCESS,
            )
        }?;
        Ok(unsafe { OwnedHandle::from_raw_handle(result.0) })
    }
    struct Attributes {
        _storage: Vec<usize>,
        raw: LPPROC_THREAD_ATTRIBUTE_LIST,
    }
    impl Attributes {
        fn new(handles: &[HANDLE]) -> Result<Self> {
            let mut bytes = 0;
            let _ = unsafe { InitializeProcThreadAttributeList(None, 1, None, &mut bytes) };
            if bytes == 0 || bytes > 65536 {
                return Err(fail(0x80004005, "process attribute size invalid"));
            }
            let mut storage = vec![0usize; bytes.div_ceil(size_of::<usize>())];
            let raw = LPPROC_THREAD_ATTRIBUTE_LIST(storage.as_mut_ptr().cast());
            unsafe { InitializeProcThreadAttributeList(Some(raw), 1, None, &mut bytes) }?;
            let result = Self {
                _storage: storage,
                raw,
            };
            unsafe {
                UpdateProcThreadAttribute(
                    raw,
                    0,
                    PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                    Some(handles.as_ptr().cast()),
                    size_of_val(handles),
                    None,
                    None,
                )
            }?;
            Ok(result)
        }
    }
    impl Drop for Attributes {
        fn drop(&mut self) {
            unsafe {
                DeleteProcThreadAttributeList(self.raw);
            }
        }
    }
    fn spawn(
        executable: &Path,
        request_path: &Path,
        workspace: &Workspace,
        permit: Permit,
    ) -> Result<Child> {
        let prefix =
            TEST_ENTRIES.with(|entries| entries.borrow().last().map(|(_, prefix)| prefix.clone()));
        let mut args = prefix.clone().unwrap_or_else(|| {
            vec![
                OsString::from(WORKER_FLAG),
                request_path.as_os_str().to_owned(),
            ]
        });
        // The hidden libtest entry has no terminal. Avoid probing terminfo
        // paths before its test function starts (outside the worker watchdog).
        if prefix.is_some() { args.push("--color=never".into()); }
        let (name, mut command) = command_line(executable, &args)?;
        let environment = environment(prefix.as_ref().map(|_| request_path))?;
        let input = fs::File::open("NUL").map_err(io)?;
        let output = fs::OpenOptions::new().write(true).open("NUL").map_err(io)?;
        let error_file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(workspace.directory.join("stderr.log"))
            .map_err(io)?;
        let input = inheritable(input.as_raw_handle())?;
        let output = inheritable(output.as_raw_handle())?;
        let error_handle = inheritable(error_file.as_raw_handle())?;
        let handles = [
            HANDLE(input.as_raw_handle()),
            HANDLE(output.as_raw_handle()),
            HANDLE(error_handle.as_raw_handle()),
        ];
        let attributes = Attributes::new(&handles)?;
        let job = unsafe { CreateJobObjectW(None, PCWSTR::null()) }?;
        let job = unsafe { OwnedHandle::from_raw_handle(job.0) };
        let limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
            BasicLimitInformation: JOBOBJECT_BASIC_LIMIT_INFORMATION {
                LimitFlags: JOB_OBJECT_LIMIT_PROCESS_MEMORY
                    | JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK
                    | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
                ..Default::default()
            },
            ProcessMemoryLimit: 192 * 1024 * 1024,
            ..Default::default()
        };
        unsafe {
            SetInformationJobObject(
                HANDLE(job.as_raw_handle()),
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        }?;
        let mut desktop = if prefix.is_some() {
            std::env::var_os("ZSDESK_MENU_TEST_STATION")
                .and_then(|mut station| {
                    std::env::var_os("ZSDESK_MENU_TEST_DESKTOP").map(|desktop| {
                        station.push("\\");
                        station.push(desktop);
                        station
                    })
                })
                .map(|name| units(&name))
                .transpose()?
        } else {
            None
        };
        if let Some(desktop) = &mut desktop {
            desktop.push(0);
        }
        let startup = STARTUPINFOEXW {
            StartupInfo: STARTUPINFOW {
                cb: size_of::<STARTUPINFOEXW>() as u32,
                dwFlags: STARTF_FORCEOFFFEEDBACK | STARTF_USESTDHANDLES | STARTF_USESHOWWINDOW,
                wShowWindow: SW_HIDE.0 as u16,
                lpDesktop: desktop
                    .as_mut()
                    .map_or(PWSTR::null(), |value| PWSTR(value.as_mut_ptr())),
                hStdInput: handles[0],
                hStdOutput: handles[1],
                hStdError: handles[2],
                ..Default::default()
            },
            lpAttributeList: attributes.raw,
        };
        let mut info = PROCESS_INFORMATION::default();
        unsafe {
            CreateProcessW(
                PCWSTR(name.as_ptr()),
                Some(PWSTR(command.as_mut_ptr())),
                None,
                None,
                true,
                CREATE_NO_WINDOW
                    | CREATE_UNICODE_ENVIRONMENT
                    | EXTENDED_STARTUPINFO_PRESENT
                    | CREATE_SUSPENDED,
                Some(environment.as_ptr().cast()),
                // Requests and outputs are absolute. Inherit the normal parent
                // working directory rather than an extended-length workspace
                // path, which changes rooted-path handling in child libraries.
                PCWSTR::null(),
                &startup.StartupInfo,
                &mut info,
            )
        }?;
        let process = unsafe { OwnedHandle::from_raw_handle(info.hProcess.0) };
        let thread = unsafe { OwnedHandle::from_raw_handle(info.hThread.0) };
        let bound = (|| {
            unsafe {
                AssignProcessToJobObject(
                    HANDLE(job.as_raw_handle()),
                    HANDLE(process.as_raw_handle()),
                )
            }?;
            let creation = created(HANDLE(process.as_raw_handle()))?;
            if unsafe { ResumeThread(HANDLE(thread.as_raw_handle())) } == u32::MAX {
                return Err(Error::from_thread());
            }
            Ok(creation)
        })();
        match bound {
            Ok(creation) => Ok(Child {
                process,
                _job: job,
                pid: info.dwProcessId,
                creation,
                _permit: permit,
            }),
            Err(error) => {
                let _ = unsafe { TerminateProcess(HANDLE(process.as_raw_handle()), 0xE0530003) };
                let _ = unsafe { WaitForSingleObject(HANDLE(process.as_raw_handle()), 1000) };
                Err(error)
            }
        }
    }
    pub(in super::super) fn start(
        registration: &Registration,
        context: &Snapshot,
        children: &[ChildStep],
        generation: u64,
        command_id: u32,
    ) -> Result<Option<Ticket>> {
        let Some(permit) = Permit::acquire() else {
            return Ok(None);
        };
        let nonce = format!("{:032x}", u128::from(unsafe { CoCreateGuid() }?));
        let request = Request {
            schema: 1,
            nonce: nonce.clone(),
            parent: std::process::id(),
            parent_created: created(unsafe { GetCurrentProcess() })?,
            generation,
            command_id,
            registration: registration.into(),
            context: context.clone(),
            children: children.to_vec(),
        };
        request.validate()?;
        let bytes =
            serde_json::to_vec(&request).map_err(|error| fail(0x80070057, &error.to_string()))?;
        if bytes.len() as u64 > MAX_REQUEST {
            return Err(fail(0x8007000e, "state request exceeds bound"));
        }
        let workspace = Workspace::create(&nonce)?;
        let request_path = workspace.directory.join("request.json");
        let prepared = (|| {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&request_path)
                .map_err(io)?;
            file.write_all(&bytes).map_err(io)?;
            drop(file);
            let executable = std::env::current_exe().map_err(io)?;
            spawn(&executable, &request_path, &workspace, permit)
        })();
        match prepared {
            Ok(child) => Ok(Some(Ticket {
                child: Some(child),
                workspace: Some(workspace),
                request,
                digest: format!("{:x}", Sha256::digest(&bytes)),
                started: Instant::now(),
            })),
            Err(error) => {
                workspace.remove();
                Err(error)
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        #[ignore = "Explicit isolated state-helper entry"]
        fn native_packaged_state_entry() -> Result<()> {
            run_test_entry()
        }
        #[test]
        #[ignore = "Owned cancellation fixture"]
        fn native_packaged_state_wait_entry() {
            assert!(std::env::var_os(TEST_REQUEST).is_some());
            thread::sleep(Duration::from_secs(20));
        }
        #[test]
        #[ignore = "Starts owned hidden helpers; requires ZSDESK_NATIVE_TEST_OUTPUT"]
        fn native_packaged_state_roundtrip_and_cancellation() -> anyhow::Result<()> {
            use anyhow::{ensure, Context};
            let output = PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("output")?).join("packaged-state");
            ensure!(output.is_absolute(), "absolute output required");
            fs::create_dir_all(&output)?;
            let workers = output.join("workers");
            let previous = std::env::var_os("ZSDESK_PACKAGED_STATE_ROOT");
            std::env::set_var("ZSDESK_PACKAGED_STATE_ROOT", &workers);
            struct Restore(Option<OsString>);
            impl Drop for Restore {
                fn drop(&mut self) {
                    if let Some(value) = &self.0 { std::env::set_var("ZSDESK_PACKAGED_STATE_ROOT", value); }
                    else { std::env::remove_var("ZSDESK_PACKAGED_STATE_ROOT"); }
                }
            }
            let _restore = Restore(previous);
            struct Drain;
            impl Drop for Drain {
                fn drop(&mut self) {
                    let until = Instant::now() + Duration::from_secs(3);
                    while pending() != 0 && Instant::now() < until { thread::sleep(Duration::from_millis(10)); }
                }
            }
            let _drain = Drain;
            let registration = Registration {
                package_family: "ZSDesk.UnregisteredFixture".into(),
                package_full_name: "ZSDesk.UnregisteredFixture_1.0.0.0_x64__fixture".into(),
                application_id: "App".into(), verb_id: "Fixture".into(),
                item_type: "Directory\\Background".into(), clsid: GUID::from_u128(1),
            };
            let snapshot = Snapshot { origin: Origin::Folder(vec![0, 0]), selected: Vec::new(), background: true, layout: None };
            let entry = test_entry(&["shell::packaged_state_worker::client::tests::native_packaged_state_entry".into(), "--exact".into(), "--ignored".into(), "--nocapture".into()])?;
            let mut ticket = start(&registration, &snapshot, &[], 8, 0x6800)?.context("permit")?;
            let directory = ticket.workspace.as_ref().unwrap().directory.clone();
            let deadline = Instant::now() + Duration::from_secs(10);
            let response = loop {
                if let Some(response) = ticket.poll() { break response?; }
                ensure!(Instant::now() < deadline, "helper response timed out");
                thread::sleep(Duration::from_millis(20));
            };
            ensure!(response.phase == "registration" && response.hresult < 0 && response.state.is_none(), "unregistered command was not rejected");
            ensure!(!directory.exists() && pending() == 0, "completed helper leaked workspace or permit");
            let mut registered_checks = Vec::new();
            {
                let _apartment = Apartment::enter()?;
                let fixture = output.join("folder"); fs::create_dir_all(&fixture)?;
                let path = wide(&fixture.to_string_lossy());
                let folder: IShellItem = unsafe { SHCreateItemFromParsingName(PCWSTR(path.as_ptr()), None) }?;
                let actual = Snapshot { origin: Origin::Folder(pidl::item_pidl(&folder).context("folder PIDL")?), selected: Vec::new(), background: true, layout: None };
                for registration in packaged_commands::discover()?.registrations.into_iter()
                    .filter(|registration| registration.item_type == "Directory\\Background").take(4) {
                    let mut ticket = start(&registration, &actual, &[], 8, 0x6801)?.context("registered permit")?;
                    let deadline = Instant::now() + Duration::from_secs(10);
                    let reply = loop {
                        if let Some(reply) = ticket.poll() { break reply?; }
                        ensure!(Instant::now() < deadline, "registered helper timed out");
                        thread::sleep(Duration::from_millis(20));
                    };
                    registered_checks.push(serde_json::json!({"package":registration.package_full_name,"phase":reply.phase,"hresult":reply.hresult,"state":reply.state}));
                    // A registered provider may explicitly decline GetState.
                    // It must remain unavailable, never become an enabled item.
                    ensure!(reply.phase == "GetState(TRUE)" &&
                        (reply.hresult == 0 && reply.state.is_some() ||
                         reply.hresult == 0x80004001u32 as i32 && reply.state.is_none()),
                        "registered state failed: {:?}", registered_checks.last());
                }
            }
            ensure!(registered_checks.is_empty() || registered_checks.iter().any(|row| row["hresult"] == 0),
                "no registered command completed a state query");
            drop(entry);
            let _entry = test_entry(&["shell::packaged_state_worker::client::tests::native_packaged_state_wait_entry".into(), "--exact".into(), "--ignored".into()])?;
            let first = start(&registration, &snapshot, &[], 9, 0x6800)?.context("first permit")?;
            let second = start(&registration, &snapshot, &[], 9, 0x6801)?.context("second permit")?;
            ensure!(start(&registration, &snapshot, &[], 9, 0x6802)?.is_none(), "concurrency bound exceeded");
            let paths = [first.workspace.as_ref().unwrap().directory.clone(), second.workspace.as_ref().unwrap().directory.clone()];
            drop(first); drop(second);
            let deadline = Instant::now() + Duration::from_secs(5);
            while pending() != 0 && Instant::now() < deadline { thread::sleep(Duration::from_millis(20)); }
            ensure!(pending() == 0 && paths.iter().all(|path| !path.exists()), "cancelled helper leaked process or workspace");
            ensure!(cleanup_errors().is_empty(), "cleanup errors: {:?}", cleanup_errors());
            let report = serde_json::json!({"state":"passed", "registration_rejected":true, "response_identity_verified":true,
                "max_concurrent_workers":2, "cancelled_helpers_reaped":true, "workspaces_removed":true, "registered_commands":registered_checks, "commands_invoked":0});
            fs::write(output.join("verification.current.json"), serde_json::to_vec_pretty(&report)?)?;
            println!("{report}");
            Ok(())
        }
        #[test]
        fn explicit_test_entries_are_scoped_without_filename_heuristics() {
            let first = test_entry(&["first".into(), "--exact".into()]).unwrap();
            let second = test_entry(&["second".into(), "--exact".into()]).unwrap();
            drop(first);
            assert_eq!(
                TEST_ENTRIES.with(|entries| entries.borrow().last().unwrap().1[0].clone()),
                OsString::from("second")
            );
            drop(second);
            assert!(TEST_ENTRIES.with(|entries| entries.borrow().is_empty()));
        }
        #[test]
        fn reply_identity_requires_nonce_generation_process_and_payload() {
            let request = Request {
                schema: 1,
                nonce: "a".repeat(32),
                parent: 1,
                parent_created: "123".into(),
                generation: 1,
                command_id: 0x6800,
                registration: WireRegistration {
                    family: "family".into(),
                    full_name: "full".into(),
                    application: "App".into(),
                    verb: "Verb".into(),
                    selector: "Directory\\Background".into(),
                    clsid: "01234567-89ab-cdef-0123-456789abcdef".into(),
                },
                context: Snapshot {
                    origin: Origin::Folder(vec![0, 0]),
                    selected: Vec::new(),
                    background: true,
                    layout: None,
                },
                children: Vec::new(),
            };
            let mut response = Response {
                schema: 1,
                nonce: request.nonce.clone(),
                generation: request.generation,
                command_id: request.command_id,
                request_sha256: "hash".into(),
                pid: 2,
                created: "456".into(),
                hresult: 0,
                state: Some(0),
                attempts: 1,
                elapsed_ms: 1,
                phase: "GetState(TRUE)".into(),
                optional_properties: Vec::new(),
                error: None,
            };
            assert!(validate_response(&request, "hash", 2, "456", &response).is_ok());
            response.generation += 1;
            assert!(validate_response(&request, "hash", 2, "456", &response).is_err());
            response.generation = request.generation;
            response.pid = 3;
            assert!(validate_response(&request, "hash", 2, "456", &response).is_err());
        }
    }
}
pub use client::{
    cleanup_errors as packaged_state_cleanup_errors, pending as pending_packaged_state_workers,
};
pub use client::{
    run_test_entry as run_packaged_state_test_worker,
    test_entry as packaged_state_worker_test_entry, TestEntryGuard,
};
pub(super) use client::{start, Ticket};
