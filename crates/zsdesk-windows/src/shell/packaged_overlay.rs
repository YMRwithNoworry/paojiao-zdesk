//! A common legacy/packaged command source for application-drawn menus.
//! Native Explorer presentation must not wrap its own packaged discovery again.
use super::packaged_commands::{self, ItemKind, Registration, Scene, SelectorMatch};
use super::packaged_state_worker as state_worker;
use sha2::{Digest, Sha256};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    ffi::c_void,
    mem::size_of,
    rc::{Rc, Weak},
    time::{Duration, Instant},
};
use windows::{
    core::{
        implement, Error, IUnknown, IUnknownImpl, Interface, Ref, Result, BOOL, GUID, HRESULT,
        PCWSTR, PSTR, PWSTR,
    },
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, PROPERTYKEY, WPARAM},
        Graphics::Gdi::*,
        System::{
            Com::{
                CoTaskMemFree, IErrorLog, IServiceProvider, IServiceProvider_Impl,
                StructuredStorage::{IPropertyBag, IPropertyBag_Impl},
            },
            Ole::{IObjectWithSite, IObjectWithSite_Impl, IOleWindow, IOleWindow_Impl},
            SystemServices::{SFGAO_FILESYSTEM, SFGAO_FOLDER, SFGAO_STREAM},
            Variant::VARIANT,
            Threading::GetCurrentThreadId,
        },
        UI::{HiDpi::GetDpiForWindow, Shell::*, WindowsAndMessaging::*},
    },
};

const FIRST_ALLOWED: u32 = 1;
const LAST_ALLOWED: u32 = 0x6fff;
const PACKAGE_CAPACITY: u32 = 2048;
const CMIC_MASK_UNICODE: u32 = 0x4000;
const MAX_DEPTH: usize = 16;
const MAX_MENU_ROWS: usize = 100_000;
const MAX_TEXT: usize = 4096;
const PENDING: HRESULT = HRESULT(0x8000000au32 as i32);
const ABORT: HRESULT = HRESULT(0x80004004u32 as i32);
fn error(code: u32, text: &str) -> Error {
    Error::new(HRESULT(code as i32), text)
}
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

#[derive(Clone, Debug)]
pub(super) struct Diagnostic {
    pub package_family: Option<String>,
    pub verb_id: Option<String>,
    pub stage: &'static str,
    pub hresult: Option<i32>,
    pub detail: String,
    pub incomplete: bool,
}
#[derive(Clone, Debug, Default)]
pub(super) struct Diagnostics {
    pub supported_sources_complete: bool,
    pub query_ready: bool,
    pub packaged_registrations: usize,
    pub visible_commands: usize,
    pub hidden_commands: usize,
    pub pending_commands: usize,
    pub dropped_diagnostics: usize,
    pub issues: Vec<Diagnostic>,
}
#[derive(Clone, Debug)]
pub(super) struct CommandMetadata {
    pub absolute_id: u32,
    pub registration: Registration,
    pub canonical_name: Option<GUID>,
    pub stable_identity: bool,
    pub command_string: String,
    pub icon_resource: Option<String>,
    pub icon_hresult: Option<i32>,
    pub flags: u32,
    pub state: Option<u32>,
    pub pending: bool,
    pub active: bool,
    pub submenu: Option<usize>,
    pub depth: usize,
}

thread_local! {static OVERLAYS:RefCell<HashMap<usize,Weak<Shared>>>=RefCell::new(HashMap::new());}
fn identity(menu: &IContextMenu) -> Option<usize> {
    menu.cast::<IUnknown>()
        .ok()
        .map(|value| value.as_raw() as usize)
}
fn find(menu: &IContextMenu) -> Option<Rc<Shared>> {
    let id = identity(menu)?;
    OVERLAYS.with(|items| items.try_borrow().ok()?.get(&id)?.upgrade())
}
pub(super) fn diagnostics(menu: &IContextMenu) -> Option<Diagnostics> {
    let shared = find(menu)?;
    let state = shared.state.try_borrow().ok()?;
    let mut result = state.diagnostics.clone();
    result.pending_commands = state.nodes.values().filter(|node| node.active.get() && node.pending.get()).count();
    result.supported_sources_complete &= result.pending_commands == 0;
    result.dropped_diagnostics += shared.dropped.get();
    result.supported_sources_complete &= shared.dropped.get() == 0;
    Some(result)
}
pub(super) fn command_metadata(menu: &IContextMenu, absolute_id: u32) -> Option<CommandMetadata> {
    let shared = find(menu)?;
    let node = shared.node(absolute_id).ok()??;
    let mut metadata = node.metadata();
    let data = shared.state.try_borrow().ok()?;
    if data
        .nodes
        .values()
        .filter(|candidate| candidate.active.get() && candidate.verb == node.verb)
        .count()
        != 1
    {
        metadata.stable_identity = false;
    }
    Some(metadata)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IdRange {
    first: u32,
    last: u32,
    legacy_last: u32,
    next: u32,
    legacy_used: u32,
}
impl IdRange {
    fn new(first: u32, last: u32, packages: bool) -> Result<Self> {
        if first < FIRST_ALLOWED || last > LAST_ALLOWED || first > last {
            return Err(error(0x80070057, "invalid context-menu command range"));
        }
        let available = last - first + 1;
        let reserve = if packages {
            PACKAGE_CAPACITY.min(available / 2)
        } else {
            0
        };
        if packages && reserve == 0 {
            return Err(error(
                0x8007000e,
                "no command range for packaged extensions",
            ));
        }
        Ok(Self {
            first,
            last,
            legacy_last: last - reserve,
            next: last - reserve + 1,
            legacy_used: 0,
        })
    }
    fn allocate(&mut self) -> Result<u32> {
        if self.next > self.last {
            return Err(error(0x8007000e, "packaged command ID range exhausted"));
        }
        let id = self.next;
        self.next += 1;
        Ok(id)
    }
    fn absolute(&self, offset: usize) -> Option<u32> {
        u32::try_from(offset)
            .ok()
            .and_then(|offset| self.first.checked_add(offset))
            .filter(|id| *id <= self.last)
    }
    fn legacy(&self, id: u32) -> bool {
        id >= self.first && id <= self.legacy_last && id - self.first < self.legacy_used
    }
    fn used(&self) -> u32 {
        if self.next == self.legacy_last + 1 {
            self.legacy_used
        } else {
            self.legacy_used.max(self.next.saturating_sub(self.first))
        }
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
unsafe fn borrowed_text(pointer: *const u16) -> Result<String> {
    if pointer.is_null() {
        return Err(error(0x80004003, "null command text"));
    }
    for length in 0..=MAX_TEXT {
        if unsafe { *pointer.add(length) } == 0 {
            return String::from_utf16(unsafe { std::slice::from_raw_parts(pointer, length) })
                .map_err(|_| error(0x80070057, "invalid command text"));
        }
    }
    Err(error(0x8007007a, "command text exceeds bound"))
}
fn owned_text(text: PWSTR) -> Result<String> {
    let result = unsafe { borrowed_text(text.0) };
    unsafe {
        CoTaskMemFree(Some(text.0.cast()));
    }
    result
}

enum Kind {
    File(Option<String>),
    UnknownExtension,
    Directory,
    Drive,
    Virtual,
}
enum ContextScene {
    Selection(Vec<Kind>),
    DirectoryBackground,
    DesktopBackground,
    VirtualBackground,
}
struct Context {
    selection: Option<IShellItemArray>,
    scene: ContextScene,
}
impl Context {
    fn capture(view: &IShellView, background: bool) -> Result<Self> {
        let folder: IFolderView2 = view.cast()?;
        if background {
            let persistent: IPersistFolder2 = unsafe { folder.GetFolder() }?;
            let pidl = Pidl(unsafe { persistent.GetCurFolder() }?);
            if pidl.0.is_null() {
                return Err(error(0x80004003, "missing background identity"));
            }
            if unsafe { *pidl.0.cast::<u16>() } == 0 {
                return Ok(Self {
                    selection: None,
                    scene: ContextScene::DesktopBackground,
                });
            }
            let item: IShellItem = unsafe { SHCreateItemFromIDList(pidl.0) }?;
            let attributes =
                unsafe { item.GetAttributes(SFGAO_FILESYSTEM | SFGAO_FOLDER | SFGAO_STREAM) }?.0;
            let filesystem = attributes & (SFGAO_FILESYSTEM.0 | SFGAO_FOLDER.0)
                == (SFGAO_FILESYSTEM.0 | SFGAO_FOLDER.0)
                && attributes & SFGAO_STREAM.0 == 0;
            return Ok(Self {
                selection: None,
                scene: if filesystem {
                    ContextScene::DirectoryBackground
                } else {
                    ContextScene::VirtualBackground
                },
            });
        }
        let selection = unsafe { folder.GetSelection(false) }?;
        let count = unsafe { selection.GetCount() }?;
        if count == 0 || count > 10_000 {
            return Err(error(
                0x80070057,
                "selected-item snapshot is empty or exceeds supported bound",
            ));
        }
        let computer = unsafe { SHGetKnownFolderIDList(&FOLDERID_ComputerFolder, 0, None) }
            .ok()
            .map(Pidl);
        let mut extension_key = PROPERTYKEY::default();
        let extension_key = unsafe {
            PropertiesSystem::PSGetPropertyKeyFromName(
                windows::core::w!("System.FileExtension"),
                &mut extension_key,
            )
        }
        .ok()
        .map(|_| extension_key);
        let mut kinds = Vec::with_capacity(count as usize);
        for index in 0..count {
            let item = unsafe { selection.GetItemAt(index) }?;
            let attributes =
                unsafe { item.GetAttributes(SFGAO_FILESYSTEM | SFGAO_FOLDER | SFGAO_STREAM) }?.0;
            if attributes & SFGAO_FILESYSTEM.0 == 0 {
                kinds.push(Kind::Virtual);
                continue;
            }
            if attributes & SFGAO_FOLDER.0 != 0 && attributes & SFGAO_STREAM.0 == 0 {
                let parent = unsafe { item.GetParent() }
                    .ok()
                    .and_then(|parent| unsafe { SHGetIDListFromObject(&parent) }.ok())
                    .map(Pidl);
                match (computer.as_ref(), parent) {
                    (Some(computer), Some(parent)) => {
                        kinds.push(if unsafe { ILIsEqual(computer.0, parent.0) }.as_bool() {
                            Kind::Drive
                        } else {
                            Kind::Directory
                        })
                    }
                    _ => kinds.push(Kind::Virtual),
                }
                continue;
            }
            let extension = extension_key
                .as_ref()
                .and_then(|key| {
                    item.cast::<IShellItem2>()
                        .ok()
                        .and_then(|item| unsafe { item.GetString(key) }.ok())
                })
                .and_then(|value| owned_text(value).ok());
            kinds.push(match extension {
                Some(value) => Kind::File((!value.is_empty()).then_some(value)),
                None => Kind::UnknownExtension,
            });
        }
        Ok(Self {
            selection: Some(selection),
            scene: ContextScene::Selection(kinds),
        })
    }
    fn matches(&self, registration: &Registration) -> SelectorMatch {
        match &self.scene {
            ContextScene::DirectoryBackground => {
                packaged_commands::matches_scene(registration, &Scene::DirectoryBackground)
            }
            ContextScene::DesktopBackground => {
                packaged_commands::matches_scene(registration, &Scene::DesktopBackground)
            }
            ContextScene::VirtualBackground => SelectorMatch::DoesNotMatch,
            ContextScene::Selection(kinds) => {
                if registration.item_type.starts_with('.')
                    && kinds
                        .iter()
                        .any(|kind| matches!(kind, Kind::UnknownExtension))
                {
                    return SelectorMatch::Unknown("file extension property unavailable");
                }
                let values: Vec<_> = kinds
                    .iter()
                    .map(|kind| match kind {
                        Kind::File(extension) => ItemKind::File {
                            extension: extension.as_deref(),
                        },
                        Kind::UnknownExtension => ItemKind::File { extension: None },
                        Kind::Directory => ItemKind::Directory,
                        Kind::Drive => ItemKind::Drive,
                        Kind::Virtual => ItemKind::Virtual,
                    })
                    .collect();
                packaged_commands::matches_scene(registration, &Scene::Selection(&values))
            }
        }
    }
}

#[implement(IServiceProvider, IOleWindow, Agile = false)]
struct RealSite {
    browser: IExplorerBrowser,
    view: IShellView,
    owner: HWND,
    previous: Option<IUnknown>,
}
impl IServiceProvider_Impl for RealSite_Impl {
    fn QueryService(
        &self,
        service: *const GUID,
        iid: *const GUID,
        out: *mut *mut c_void,
    ) -> Result<()> {
        if service.is_null() || iid.is_null() || out.is_null() {
            return Err(error(0x80004003, "invalid service request"));
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
        for candidate in self
            .previous
            .iter()
            .cloned()
            .chain(self.browser.cast::<IUnknown>().ok())
        {
            if let Ok(provider) = candidate.cast::<IServiceProvider>() {
                let result = unsafe {
                    (provider.vtable().QueryService)(provider.as_raw(), &service, iid, out)
                };
                if result.is_ok() {
                    return Ok(());
                }
                unsafe {
                    *out = std::ptr::null_mut();
                }
            }
        }
        Err(error(0x80004002, "service unavailable"))
    }
}
impl IOleWindow_Impl for RealSite_Impl {
    fn GetWindow(&self) -> Result<HWND> {
        Ok(self.owner)
    }
    fn ContextSensitiveHelp(&self, _: BOOL) -> Result<()> {
        Err(error(0x80004001, "context help unsupported"))
    }
}

#[implement(IPropertyBag, Agile = false)]
struct ReadOnlyBag {
    reads: Rc<RefCell<Vec<String>>>,
}
impl IPropertyBag_Impl for ReadOnlyBag_Impl {
    fn Read(&self, name: &PCWSTR, _value: *mut VARIANT, _error: Ref<IErrorLog>) -> Result<()> {
        let name = unsafe { borrowed_text(name.0) }?;
        if let Ok(mut reads) = self.reads.try_borrow_mut() {
            if reads.len() < 128 {
                reads.push(name);
            }
        }
        Err(error(
            0x80070057,
            "optional registered property not supplied by package manifest",
        ))
    }
    fn Write(&self, _name: &PCWSTR, _value: *const VARIANT) -> Result<()> {
        Err(error(0x80030005, "command properties are read only"))
    }
}

struct Bitmap(HBITMAP);
impl Drop for Bitmap {
    fn drop(&mut self) {
        let _ = unsafe { DeleteObject(self.0.into()) };
    }
}
fn bitmap_from_icon(resource: &str, owner: HWND) -> Result<Rc<Bitmap>> {
    if resource.contains('\0') {
        return Err(error(0x80070057, "invalid icon resource"));
    }
    let mut path = wide(resource);
    let index = unsafe { PathParseIconLocationW(PWSTR(path.as_mut_ptr())) };
    let dpi = unsafe { GetDpiForWindow(owner) }.max(96);
    let size = ((16u64 * dpi as u64 + 48) / 96).clamp(16, 64) as i32;
    let mut icon = HICON::default();
    unsafe {
        SHDefExtractIconW(
            PCWSTR(path.as_ptr()),
            index,
            0,
            Some(&mut icon),
            None,
            size as u32,
        )
    }
    .ok()?;
    if icon.0.is_null() {
        return Err(error(0x80004005, "icon extraction returned no icon"));
    }
    struct Icon(HICON);
    impl Drop for Icon {
        fn drop(&mut self) {
            let _ = unsafe { DestroyIcon(self.0) };
        }
    }
    let icon = Icon(icon);
    let dc = unsafe { CreateCompatibleDC(None) };
    if dc.is_invalid() {
        return Err(Error::from_thread());
    }
    struct Dc(HDC);
    impl Drop for Dc {
        fn drop(&mut self) {
            let _ = unsafe { DeleteDC(self.0) };
        }
    }
    let dc = Dc(dc);
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size,
            biHeight: -size,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits = std::ptr::null_mut();
    let bitmap =
        Bitmap(unsafe { CreateDIBSection(Some(dc.0), &info, DIB_RGB_COLORS, &mut bits, None, 0) }?);
    if bits.is_null() {
        return Err(error(0x80004003, "icon bitmap has no pixels"));
    }
    unsafe {
        std::ptr::write_bytes(bits.cast::<u8>(), 0, (size * size * 4) as usize);
    }
    let previous = unsafe { SelectObject(dc.0, bitmap.0.into()) };
    let result = unsafe { DrawIconEx(dc.0, 0, 0, icon.0, size, size, 0, None, DI_NORMAL) };
    unsafe {
        SelectObject(dc.0, previous);
    }
    result?;
    Ok(Rc::new(bitmap))
}

struct Node {
    id: u32,
    command: IExplorerCommand,
    registration: Registration,
    canonical: Option<GUID>,
    path: Vec<u32>,
    stable: bool,
    verb: String,
    title: String,
    icon: Option<String>,
    icon_hresult: Option<i32>,
    bitmap: Option<Rc<Bitmap>>,
    flags: u32,
    state: Cell<Option<u32>>,
    pending: Cell<bool>,
    active: Cell<bool>,
    submenu: Option<HMENU>,
    parent_menu: HMENU,
    children: RefCell<Vec<u32>>,
    marker: Rc<u8>,
    requested: Cell<bool>,
    proof: Cell<Option<SlowProof>>,
}
impl Node {
    fn token(&self) -> usize {
        (&*self.marker as *const u8) as usize
    }
    fn metadata(&self) -> CommandMetadata {
        CommandMetadata {
            absolute_id: self.id,
            registration: self.registration.clone(),
            canonical_name: self.canonical,
            stable_identity: self.stable,
            command_string: self.verb.clone(),
            icon_resource: self.icon.clone(),
            icon_hresult: self.icon_hresult,
            flags: self.flags,
            state: self.state.get(),
            pending: self.pending.get(),
            active: self.active.get(),
            submenu: self.submenu.map(|menu| menu.0 as usize),
            depth: self.path.len(),
        }
    }
}
#[derive(Default)]
struct State {
    range: Option<IdRange>,
    root: Option<HMENU>,
    ready: bool,
    querying: bool,
    busy_menus: HashSet<usize>,
    nodes: HashMap<u32, Rc<Node>>,
    menus: HashMap<usize, u32>,
    diagnostics: Diagnostics,
    retired_bitmaps: Vec<Rc<Bitmap>>,
}
struct Shared {
    base: IContextMenu,
    // Production construction always retains both real interfaces. Test doubles
    // can exercise COM/menu lifetime contracts without creating an Explorer view.
    browser: Option<IExplorerBrowser>,
    view: Option<IShellView>,
    owner: HWND,
    context: Context,
    original_site: Option<IUnknown>,
    active_site: RefCell<Option<IUnknown>>,
    state: RefCell<State>,
    identity: Cell<usize>,
    dropped: Cell<usize>,
    generation: Cell<u64>,
    slow: RefCell<SlowState>,
    slow_window: Cell<HWND>,
    polling: Cell<bool>,
}
impl Drop for Shared {
    fn drop(&mut self) {
        self.cancel_slow(true);
        let hwnd = self.slow_window.replace(HWND::default());
        if !hwnd.0.is_null() {
            SLOW_WINDOWS.with(|windows| windows.borrow_mut().remove(&(hwnd.0 as usize)));
            let _ = unsafe { DestroyWindow(hwnd) };
        }
        OVERLAYS.with(|items| {
            if let Ok(mut items) = items.try_borrow_mut() {
                items.remove(&self.identity.get());
            }
        });
        if let Ok(site) = self.base.cast::<IObjectWithSite>() {
            let _ = unsafe { site.SetSite(self.original_site.as_ref()) };
        }
    }
}
fn command_key(
    registration: &Registration,
    path: &[u32],
    canonical: Option<GUID>,
) -> (String, bool) {
    let stable = path.is_empty() || canonical.is_some_and(|id| id != GUID::zeroed());
    let mut hash = Sha256::new();
    for text in [
        &registration.package_family,
        &registration.application_id,
        &registration.verb_id,
    ] {
        hash.update(text.as_bytes());
        hash.update([0]);
    }
    hash.update(u128::from(registration.clsid).to_le_bytes());
    if let Some(id) = canonical.filter(|id| *id != GUID::zeroed()) {
        hash.update(u128::from(id).to_le_bytes());
    } else {
        for index in path {
            hash.update(index.to_le_bytes());
        }
    }
    (format!("zsdesk.packaged.{:x}", hash.finalize()), stable)
}
impl Shared {
    fn invalidate_query(&self) {
        self.cancel_slow(true);
        if let Ok(mut data) = self.state.try_borrow_mut() {
            data.ready = false;
            data.diagnostics.query_ready = false;
            for node in data.nodes.values() {
                node.active.set(false);
            }
        }
    }

    fn disable_node(&self, node: &Node) {
        node.state.set(None);
        node.proof.set(None);
        let info = MENUITEMINFOW {
            cbSize: size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_STATE,
            fState: MFS_DISABLED,
            ..Default::default()
        };
        if node.active.get() {
            let _ = unsafe { SetMenuItemInfoW(node.parent_menu, node.id, false, &info) };
        }
    }

    fn issue(
        &self,
        registration: Option<&Registration>,
        stage: &'static str,
        code: Option<HRESULT>,
        detail: impl ToString,
        incomplete: bool,
    ) {
        let Ok(mut state) = self.state.try_borrow_mut() else {
            self.dropped.set(self.dropped.get().saturating_add(1));
            return;
        };
        if incomplete {
            state.diagnostics.supported_sources_complete = false;
        }
        if state.diagnostics.issues.len() >= 512 {
            state.diagnostics.dropped_diagnostics += 1;
            return;
        }
        state.diagnostics.issues.push(Diagnostic {
            package_family: registration.map(|row| row.package_family.clone()),
            verb_id: registration.map(|row| row.verb_id.clone()),
            stage,
            hresult: code.map(|code| code.0),
            detail: detail.to_string().chars().take(1024).collect(),
            incomplete,
        });
    }
    fn node(&self, id: u32) -> Result<Option<Rc<Node>>> {
        Ok(self
            .state
            .try_borrow()
            .map_err(|_| error(0x8000000a, "menu state is reentrant"))?
            .nodes
            .get(&id)
            .cloned())
    }
    fn allocate(&self) -> Result<u32> {
        self.state
            .try_borrow_mut()
            .map_err(|_| error(0x8000000a, "menu state is reentrant"))?
            .range
            .as_mut()
            .ok_or_else(|| error(0x80004005, "menu has not been queried"))?
            .allocate()
    }
    fn site(&self) -> Result<Option<IUnknown>> {
        Ok(self
            .active_site
            .try_borrow()
            .map_err(|_| error(0x8000000a, "site is reentrant"))?
            .clone())
    }
    fn prepare_command(
        &self,
        command: &IExplorerCommand,
        registration: &Registration,
        initialize: bool,
    ) -> Result<()> {
        if initialize {
            if let Ok(initializer) = command.cast::<IInitializeCommand>() {
                let reads = Rc::new(RefCell::new(Vec::new()));
                let bag: IPropertyBag = ReadOnlyBag {
                    reads: reads.clone(),
                }
                .into();
                let name = wide(&registration.verb_id);
                let result = unsafe { initializer.Initialize(PCWSTR(name.as_ptr()), &bag) };
                for name in reads.borrow().iter() {
                    self.issue(
                        Some(registration),
                        "optional-initialization-property",
                        Some(HRESULT(0x80070057u32 as i32)),
                        name,
                        false,
                    );
                }
                packaged_commands::optional_initialization(result)?;
            }
        }
        if let Ok(site) = command.cast::<IObjectWithSite>() {
            packaged_commands::optional_initialization(unsafe { site.SetSite(self.site()?.as_ref()) })?;
        }
        Ok(())
    }
    fn add(
        &self,
        menu: HMENU,
        position: u32,
        command: IExplorerCommand,
        registration: Registration,
        path: Vec<u32>,
    ) -> Result<Option<u32>> {
        let status = unsafe { command.GetState(self.context.selection.as_ref(), false) };
        let (state, pending) = match status {
            Ok(state) if state & ECS_HIDDEN.0 as u32 != 0 => {
                if let Ok(mut data) = self.state.try_borrow_mut() {
                    data.diagnostics.hidden_commands += 1;
                }
                return Ok(None);
            }
            Ok(state) => (Some(state), false),
            Err(error) if error.code() == PENDING => {
                self.issue(
                    Some(&registration),
                    "GetState",
                    Some(PENDING),
                    "slow state requested; command remains disabled until resolved",
                    false,
                );
                if let Ok(mut data) = self.state.try_borrow_mut() {
                    data.diagnostics.pending_commands += 1;
                }
                (None, true)
            }
            Err(error) => return Err(error),
        };
        let flags = if pending {
            unsafe { command.GetFlags() }.unwrap_or_default()
        } else { unsafe { command.GetFlags() }? };
        if flags & ECF_ISSEPARATOR.0 as u32 != 0 {
            insert_separator(menu, position)?;
            return Ok(None);
        }
        let title = if pending {
            unsafe { command.GetTitle(self.context.selection.as_ref()) }.and_then(owned_text)
                .unwrap_or_else(|_| "应用扩展正在加载…".into())
        } else {
            owned_text(unsafe { command.GetTitle(self.context.selection.as_ref()) }?)?
        };
        let canonical = unsafe { command.GetCanonicalName() }.ok();
        let (verb, stable) = command_key(&registration, &path, canonical);
        let (icon, icon_hresult) = if pending {
            (None, None)
        } else {
            match unsafe { command.GetIcon(self.context.selection.as_ref()) } {
                Ok(value) => match owned_text(value) {
                    Ok(value) if !value.is_empty() => (Some(value), None),
                    Ok(_) => (None, None),
                    Err(error) => (None, Some(error.code().0)),
                },
                Err(error) => (None, Some(error.code().0)),
            }
        };
        let bitmap =
            icon.as_ref()
                .and_then(|resource| match bitmap_from_icon(resource, self.owner) {
                    Ok(bitmap) => Some(bitmap),
                    Err(error) => {
                        self.issue(
                            Some(&registration),
                            "icon-render",
                            Some(error.code()),
                            resource,
                            false,
                        );
                        None
                    }
                });
        let id = self.allocate()?;
        let submenu = if flags & ECF_HASSUBCOMMANDS.0 as u32 != 0 {
            let submenu = unsafe { CreatePopupMenu() }?;
            if let Err(error) = insert_notice(submenu, "正在读取应用命令…") {
                let _ = unsafe { DestroyMenu(submenu) };
                return Err(error);
            }
            Some(submenu)
        } else {
            None
        };
        let node = Rc::new(Node {
            id,
            command,
            registration,
            canonical,
            path,
            stable,
            verb,
            title,
            icon,
            icon_hresult,
            bitmap,
            flags,
            state: Cell::new(state),
            pending: Cell::new(pending),
            active: Cell::new(true),
            submenu,
            parent_menu: menu,
            children: RefCell::new(Vec::new()),
            marker: Rc::new(0),
            requested: Cell::new(false),
            proof: Cell::new(None),
        });
        let mut text = wide(&node.title);
        let mut info = MENUITEMINFOW {
            cbSize: size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_ID | MIIM_STATE | MIIM_STRING | MIIM_FTYPE | MIIM_DATA,
            wID: id,
            dwTypeData: PWSTR(text.as_mut_ptr()),
            dwItemData: node.token(),
            ..Default::default()
        };
        info.fState = menu_state(state, pending);
        if state.is_some_and(|value| value & ECS_RADIOCHECK.0 as u32 != 0) {
            info.fType |= MFT_RADIOCHECK;
        }
        if let Some(submenu) = submenu {
            info.fMask |= MIIM_SUBMENU;
            info.hSubMenu = submenu;
        }
        if let Some(bitmap) = &node.bitmap {
            info.fMask |= MIIM_BITMAP;
            info.hbmpItem = bitmap.0;
        }
        let position = if flags & ECF_SEPARATORBEFORE.0 as u32 != 0 && position > 0 {
            insert_separator(menu, position)?;
            position + 1
        } else {
            position
        };
        if let Err(error) = unsafe { InsertMenuItemW(menu, position, true, &info) } {
            if let Some(submenu) = submenu {
                let _ = unsafe { DestroyMenu(submenu) };
            }
            return Err(error);
        }
        if flags & ECF_SEPARATORAFTER.0 as u32 != 0 {
            insert_separator(menu, position + 1)?;
        }
        let mut data = self
            .state
            .try_borrow_mut()
            .map_err(|_| error(0x8000000a, "menu state is reentrant"))?;
        if let Some(submenu) = submenu {
            data.menus.insert(submenu.0 as usize, id);
        }
        data.diagnostics.visible_commands += 1;
        data.nodes.insert(id, node);
        Ok(Some(id))
    }
    fn query(&self, menu: HMENU, index: u32, first: u32, last: u32, flags: u32) -> Result<u32> {
        let packages = flags & (CMF_DEFAULTONLY | CMF_OPTIMIZEFORINVOKE) == 0;
        let range = IdRange::new(first, last, packages)?;
        let old = {
            let mut data = self
                .state
                .try_borrow_mut()
                .map_err(|_| error(0x8000000a, "menu state is reentrant"))?;
            std::mem::replace(
                &mut *data,
                State {
                    range: Some(range),
                    root: Some(menu),
                    querying: true,
                    diagnostics: Diagnostics {
                        supported_sources_complete: true,
                        ..Default::default()
                    },
                    ..Default::default()
                },
            )
        };
        drop(old);
        self.dropped.set(0);
        let before = unsafe { GetMenuItemCount(Some(menu)) };
        if before < 0 {
            return Err(Error::from_thread());
        }
        let base = unsafe {
            self.base
                .QueryContextMenu(menu, index, first, range.legacy_last, flags)
        };
        base.ok()?;
        let used = base.0 as u32 & 0xffff;
        if used > range.legacy_last - first + 1 {
            return Err(error(
                0x80070057,
                "legacy handler exceeded its command range",
            ));
        }
        let after = unsafe { GetMenuItemCount(Some(menu)) };
        if after < before {
            return Err(error(
                0x80004005,
                "legacy handler removed caller menu items",
            ));
        }
        if packages && menu_has_reserved_command(menu, range.legacy_last + 1, last)? {
            return Err(error(
                0x80070057,
                "legacy handler used reserved packaged IDs",
            ));
        }
        {
            let mut data = self
                .state
                .try_borrow_mut()
                .map_err(|_| error(0x8000000a, "menu state is reentrant"))?;
            data.range.as_mut().unwrap().legacy_used = used;
        }
        if packages {
            let mut at = index
                .saturating_add((after - before) as u32)
                .min(after as u32);
            let mut separated = false;
            match packaged_commands::discover() {
                Err(error) => self.issue(
                    None,
                    "package-discovery",
                    Some(error.code()),
                    error.message(),
                    true,
                ),
                Ok(discovery) => {
                    if let Ok(mut data) = self.state.try_borrow_mut() {
                        data.diagnostics.packaged_registrations = discovery.registrations.len();
                    }
                    for failure in discovery.failures {
                        self.issue(
                            None,
                            "package-discovery",
                            failure.hresult.map(HRESULT),
                            format!("{}: {}", failure.stage, failure.message),
                            true,
                        );
                    }
                    if !discovery.enumeration_complete {
                        self.issue(
                            None,
                            "package-discovery",
                            None,
                            "package enumeration did not complete",
                            true,
                        );
                    }
                    for registration in discovery.registrations {
                        match self.context.matches(&registration) {
                            SelectorMatch::DoesNotMatch => continue,
                            SelectorMatch::Unknown(reason) => {
                                self.issue(Some(&registration), "selector", None, reason, true);
                                continue;
                            }
                            SelectorMatch::Matches => (),
                        }
                        let command = match packaged_commands::activate(&registration) {
                            Ok(command) => command,
                            Err(error) => {
                                self.issue(
                                    Some(&registration),
                                    "activation",
                                    Some(error.code()),
                                    error.message(),
                                    true,
                                );
                                continue;
                            }
                        };
                        if let Err(error) = self.prepare_command(&command, &registration, true) {
                            self.issue(
                                Some(&registration),
                                "command-initialization",
                                Some(error.code()),
                                error.message(),
                                true,
                            );
                            continue;
                        }
                        // Inspect state before inserting the separator: a hidden package
                        // must not add either a title or an empty section.
                        let before_count = unsafe { GetMenuItemCount(Some(menu)) };
                        match self.add(menu, at, command, registration.clone(), Vec::new()) {
                            Ok(_) => {
                                let added = unsafe { GetMenuItemCount(Some(menu)) } - before_count;
                                if added > 0 {
                                    if !separated && before_count > 0 {
                                        insert_separator(menu, at)?;
                                        at += 1;
                                        separated = true;
                                    }
                                    at += added as u32;
                                }
                            }
                            Err(error) => self.issue(
                                Some(&registration),
                                "command-construction",
                                Some(error.code()),
                                error.message(),
                                true,
                            ),
                        }
                    }
                }
            }
            // Provider failures remain in diagnostics. They are not commands
            // and must not add a synthetic row to the user's context menu.
        }
        let mut data = self
            .state
            .try_borrow_mut()
            .map_err(|_| error(0x8000000a, "menu state is reentrant"))?;
        data.ready = true;
        data.diagnostics.query_ready = true;
        let used = if packages { data.range.unwrap().used() } else { used };
        drop(data);
        if let Err(error) = self.arm_slow() { self.fail_waiting(&error); }
        Ok(used)
    }
    fn populate_submenu(&self, menu: HMENU, parent_id: u32) -> Result<()> {
        let parent = self
            .node(parent_id)?
            .ok_or_else(|| error(0x80070057, "unknown submenu command"))?;
        if !parent.active.get() {
            return Err(error(0x80070057, "retired submenu command"));
        }
        if parent.path.len() >= MAX_DEPTH {
            self.issue(
                Some(&parent.registration),
                "submenu-depth",
                None,
                "submenu nesting exceeds supported bound",
                true,
            );
            while unsafe { GetMenuItemCount(Some(menu)) } > 0 {
                unsafe { DeleteMenu(menu, 0, MF_BYPOSITION) }?;
            }
            insert_notice(menu, "应用子菜单层级过深")?;
            return Ok(());
        }
        {
            let mut data = self
                .state
                .try_borrow_mut()
                .map_err(|_| error(0x8000000a, "submenu state is reentrant"))?;
            if !data.busy_menus.insert(menu.0 as usize) {
                return Err(error(
                    0x8000000a,
                    "submenu initialization is already active",
                ));
            }
        }
        let result: Result<()> = (|| {
            let old = std::mem::take(
                &mut *parent
                    .children
                    .try_borrow_mut()
                    .map_err(|_| error(0x8000000a, "submenu children are reentrant"))?,
            );
            let mut pending = old;
            let mut visited = HashSet::new();
            while let Some(id) = pending.pop() {
                if !visited.insert(id) {
                    continue;
                }
                if let Some(node) = self.node(id)? {
                    node.active.set(false);
                    pending.extend(node.children.borrow().iter().copied());
                    if let Some(submenu) = node.submenu {
                        if let Ok(mut data) = self.state.try_borrow_mut() {
                            data.menus.remove(&(submenu.0 as usize));
                        }
                    }
                }
            }
            while unsafe { GetMenuItemCount(Some(menu)) } > 0 {
                unsafe { DeleteMenu(menu, 0, MF_BYPOSITION) }?;
            }
            let state = self.current_state(&parent)?;
            parent.state.set(Some(state));
            if state & (ECS_HIDDEN.0 | ECS_DISABLED.0) as u32 != 0 {
                insert_notice(menu, "此应用命令当前不可用")?;
                return Ok(());
            }
            let enumeration = unsafe { parent.command.EnumSubCommands() }?;
            let mut ordinal = 0u32;
            let mut partial = false;
            loop {
                let mut items = [None];
                let mut fetched = 0;
                let result = unsafe { enumeration.Next(&mut items, Some(&mut fetched)) };
                result.ok()?;
                if fetched == 0 {
                    break;
                }
                let command = items[0]
                    .take()
                    .ok_or_else(|| error(0x80004003, "null subcommand"))?;
                let mut path = parent.path.clone();
                path.push(ordinal);
                ordinal = ordinal
                    .checked_add(1)
                    .ok_or_else(|| error(0x8007000e, "subcommand enumeration overflow"))?;
                if ordinal > PACKAGE_CAPACITY {
                    partial = true;
                    self.issue(
                        Some(&parent.registration),
                        "subcommand-capacity",
                        None,
                        "subcommand count exceeds supported command budget",
                        true,
                    );
                    break;
                }
                if let Err(error) = self.prepare_command(&command, &parent.registration, false) {
                    partial = true;
                    self.issue(
                        Some(&parent.registration),
                        "subcommand-initialization",
                        Some(error.code()),
                        error.message(),
                        true,
                    );
                    if result == HRESULT(1) {
                        break;
                    }
                    continue;
                }
                let at = unsafe { GetMenuItemCount(Some(menu)) }.max(0) as u32;
                match self.add(menu, at, command, parent.registration.clone(), path) {
                    Ok(Some(id)) => parent.children.borrow_mut().push(id),
                    Ok(None) => (),
                    Err(error) => {
                        partial = true;
                        self.issue(
                            Some(&parent.registration),
                            "subcommand-construction",
                            Some(error.code()),
                            error.message(),
                            true,
                        );
                    }
                }
                if result == HRESULT(1) {
                    break;
                }
            }
            if partial {
                insert_notice(menu, "部分应用命令暂不可用")?;
            } else if unsafe { GetMenuItemCount(Some(menu)) } == 0 {
                insert_notice(menu, "没有可用的应用命令")?;
            }
            Ok(())
        })();
        if let Ok(mut data) = self.state.try_borrow_mut() {
            data.busy_menus.remove(&(menu.0 as usize));
        }
        if let Err(error) = result {
            self.issue(
                Some(&parent.registration),
                "EnumSubCommands",
                Some(error.code()),
                error.message(),
                true,
            );
            insert_notice(
                menu,
                if error.code() == PENDING {
                    "应用扩展正在加载…"
                } else {
                    "应用子菜单暂不可用"
                },
            )?;
        }
        if let Err(error) = self.arm_slow() { self.fail_waiting(&error); }
        Ok(())
    }
    fn invoke(&self, info: *const CMINVOKECOMMANDINFO) -> Result<()> {
        if info.is_null() {
            return Err(error(0x80004003, "null invocation"));
        }
        let basic = unsafe { &*info };
        if basic.cbSize < size_of::<CMINVOKECOMMANDINFO>() as u32 {
            return Err(error(0x80070057, "short invocation structure"));
        }
        let (range, ready, root) = {
            let data = self
                .state
                .try_borrow()
                .map_err(|_| error(0x8000000a, "menu is reentrant"))?;
            (data.range, data.ready && !data.querying, data.root)
        };
        let range = range.ok_or_else(|| error(0x80070057, "menu has not been queried"))?;
        if !ready {
            return Err(error(0x8000000a, "menu query is not complete"));
        }
        let raw = basic.lpVerb.0 as usize;
        let node = if raw <= 0xffff {
            let id = range
                .absolute(raw)
                .ok_or_else(|| error(0x80070057, "invalid command offset"))?;
            if range.legacy(id) {
                return unsafe { self.base.InvokeCommand(info) };
            }
            self.node(id)?
                .ok_or_else(|| error(0x80070057, "unknown packaged command offset"))?
        } else {
            let name = if basic.fMask & CMIC_MASK_UNICODE != 0 {
                if basic.cbSize < size_of::<CMINVOKECOMMANDINFOEX>() as u32 {
                    return Err(error(0x80070057, "short Unicode invocation"));
                }
                unsafe { borrowed_text((*(info.cast::<CMINVOKECOMMANDINFOEX>())).lpVerbW.0) }?
            } else {
                unsafe { borrowed_ansi(basic.lpVerb.0) }?
            };
            if !name.starts_with("zsdesk.packaged.") {
                return unsafe { self.base.InvokeCommand(info) };
            }
            let data = self
                .state
                .try_borrow()
                .map_err(|_| error(0x8000000a, "menu is reentrant"))?;
            let mut matches = data
                .nodes
                .values()
                .filter(|node| node.verb == name && node.active.get());
            let found = matches
                .next()
                .cloned()
                .ok_or_else(|| error(0x80070057, "unknown packaged canonical command"))?;
            if matches.next().is_some() {
                return Err(error(0x80070057, "ambiguous packaged canonical command"));
            }
            found
        };
        if !node.active.get() {
            return Err(error(0x80070057, "retired packaged command"));
        }
        let root = root.ok_or_else(|| error(0x80070057, "missing root menu"))?;
        if !command_tokens_match(root, node.id, node.token())? {
            return Err(error(0x80070057, "command ID collision or detached item"));
        }
        if node.pending.get() {
            return Err(Error::from_hresult(PENDING));
        }
        if node.state.get().is_none() {
            return Err(Error::new(
                ABORT,
                "应用命令的站点或状态已失效，请重新打开菜单",
            ));
        }
        let state = self.current_state(&node)?;
        if state & (ECS_HIDDEN.0 | ECS_DISABLED.0) as u32 != 0 {
            return Err(Error::new(ABORT, "应用命令当前不可用"));
        }
        let result = unsafe { node.command.Invoke(self.context.selection.as_ref(), None) };
        if let Err(error) = &result {
            self.issue(
                Some(&node.registration),
                "Invoke",
                Some(error.code()),
                error.message(),
                false,
            );
        }
        result
    }
    fn message(&self, message: u32, wp: WPARAM, lp: LPARAM, out: *mut LRESULT) -> Result<()> {
        if !out.is_null() {
            unsafe {
                *out = LRESULT(0);
            }
        }
        if message == WM_UNINITMENUPOPUP && self.state.try_borrow().is_ok_and(|data| data.root.is_some_and(|menu| menu.0 as usize == wp.0)) {
            self.cancel_slow(false);
        }
        if message == WM_EXITMENULOOP { self.cancel_slow(false); }
        if message == WM_INITMENUPOPUP || message == WM_UNINITMENUPOPUP {
            let owned = self
                .state
                .try_borrow()
                .ok()
                .and_then(|data| data.menus.get(&wp.0).copied());
            if let Some(id) = owned {
                if message == WM_INITMENUPOPUP {
                    self.populate_submenu(HMENU(wp.0 as *mut _), id)?;
                }
                return Ok(());
            }
        }
        if let Ok(menu) = self.base.cast::<IContextMenu3>() {
            return unsafe {
                menu.HandleMenuMsg2(message, wp, lp, (!out.is_null()).then_some(out))
            };
        }
        if let Ok(menu) = self.base.cast::<IContextMenu2>() {
            return unsafe { menu.HandleMenuMsg(message, wp, lp) };
        }
        Err(error(
            0x80004001,
            "legacy menu does not handle this message",
        ))
    }
}

const SLOW_TIMER: usize = 0x5a535053;
const MAX_SLOW_REQUESTS: usize = 32;
const SLOW_QUEUE_LIMIT: Duration = Duration::from_secs(120);
#[derive(Clone, Copy, Debug)]
struct SlowProof { generation: u64, state: u32, snapshot: [u8; 32] }
#[derive(Default)]
struct SlowState {
    jobs: HashMap<u32, state_worker::Ticket>,
    snapshot: Option<state_worker::Snapshot>,
    started: Option<Instant>,
    submitted: usize,
    closed: bool,
}
thread_local! {static SLOW_WINDOWS: RefCell<HashMap<usize, Weak<Shared>>> = RefCell::new(HashMap::new());}
fn snapshot_key(value: &state_worker::Snapshot) -> Result<[u8; 32]> {
    let bytes = serde_json::to_vec(value).map_err(|error| Error::new(ABORT, error.to_string()))?;
    Ok(Sha256::digest(bytes).into())
}
fn proven_state(fast: Result<u32>, proof: Option<SlowProof>, generation: u64, snapshot: [u8; 32]) -> Result<u32> {
    match fast {
        Err(error) if error.code() == PENDING => proof
            .filter(|proof| proof.generation == generation && proof.snapshot == snapshot)
            .map(|proof| proof.state).ok_or(error),
        result => result,
    }
}
unsafe extern "system" fn slow_timer(hwnd: HWND, _: u32, id: usize, _: u32) {
    if id != SLOW_TIMER { return; }
    let shared = SLOW_WINDOWS.with(|windows| windows.try_borrow().ok()?.get(&(hwnd.0 as usize))?.upgrade());
    if let Some(shared) = shared {
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| shared.poll_slow())).is_err() {
            shared.cancel_slow(false);
            shared.issue(None, "slow-state-poll", Some(ABORT), "slow-state callback panicked", true);
        }
    }
}
fn repaint_menu(menu: HMENU, owner: HWND) {
    // Use only public window/menu APIs. Do not replay WM_INITMENUPOPUP or send
    // undocumented MN_* messages: those can rebuild a live extension menu.
    let mut rect = RECT::default();
    if unsafe { GetMenuItemRect(None, menu, 0, &mut rect) }.is_err() { return; }
    let hwnd = unsafe { WindowFromPoint(POINT { x: rect.left + 1, y: rect.top + 1 }) };
    let mut process = 0;
    let thread = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process)) };
    let mut class = [0u16; 32];
    let count = unsafe { GetClassNameW(hwnd, &mut class) }.max(0) as usize;
    if process == std::process::id() && thread == unsafe { GetCurrentThreadId() }
        && String::from_utf16_lossy(&class[..count]) == "#32768" {
        let _ = unsafe { InvalidateRect(Some(hwnd), None, true) };
        let _ = unsafe { UpdateWindow(hwnd) };
    }
    if !owner.0.is_null() { let _ = unsafe { DrawMenuBar(owner) }; }
}
impl Shared {
    fn cancel_slow(&self, invalidate: bool) {
        if invalidate {
            // Wrapping generation zero is never usable in a worker envelope.
            self.generation.set(self.generation.get().wrapping_add(1).max(1));
            if let Ok(data) = self.state.try_borrow() {
                for node in data.nodes.values() { node.proof.set(None); node.requested.set(false); }
            }
        }
        let jobs = if let Ok(mut slow) = self.slow.try_borrow_mut() {
            slow.closed = true;
            if invalidate { slow.snapshot = None; slow.started = None; slow.submitted = 0; }
            std::mem::take(&mut slow.jobs)
        } else { HashMap::new() };
        drop(jobs); // Ticket transfers only its owned process to the reaper.
        let hwnd = self.slow_window.get();
        if !hwnd.0.is_null() { let _ = unsafe { KillTimer(Some(hwnd), SLOW_TIMER) }; }
    }
    fn arm_slow(&self) -> Result<()> {
        let pending = self.state.try_borrow().map_err(|_| Error::from_hresult(PENDING))?
            .nodes.values().any(|node| node.active.get() && node.pending.get());
        if !pending { return Ok(()); }
        let mut hwnd = self.slow_window.get();
        if hwnd.0.is_null() {
            let weak = OVERLAYS.with(|items| items.try_borrow().ok()?.get(&self.identity.get()).cloned())
                .ok_or_else(|| Error::new(ABORT, "slow-state overlay is not registered"))?;
            hwnd = unsafe { CreateWindowExW(WINDOW_EX_STYLE::default(), windows::core::w!("STATIC"),
                windows::core::w!("ZSDesk packaged command state"), WINDOW_STYLE::default(),
                0, 0, 0, 0, Some(HWND_MESSAGE), None, None, None) }?;
            self.slow_window.set(hwnd);
            SLOW_WINDOWS.with(|windows| windows.borrow_mut().insert(hwnd.0 as usize, weak));
        }
        {
            let mut slow = self.slow.try_borrow_mut().map_err(|_| Error::from_hresult(PENDING))?;
            slow.closed = false;
            slow.started.get_or_insert_with(Instant::now);
        }
        if unsafe { SetTimer(Some(hwnd), SLOW_TIMER, 60, Some(slow_timer)) } == 0 {
            return Err(Error::from_thread());
        }
        Ok(())
    }
    fn state_snapshot(&self) -> Result<state_worker::Snapshot> {
        let view = self.view.as_ref().ok_or_else(|| Error::new(ABORT, "real view is unavailable"))?;
        state_worker::capture(view, self.context.selection.as_ref(), self.context.selection.is_none())
    }
    fn child_steps(&self, node: &Node) -> Result<Vec<state_worker::ChildStep>> {
        let data = self.state.try_borrow().map_err(|_| Error::from_hresult(PENDING))?;
        let mut result = Vec::new();
        for depth in 1..=node.path.len() {
            let mut matches = data.nodes.values().filter(|candidate| candidate.active.get()
                && candidate.path == node.path[..depth]
                && candidate.registration.package_full_name == node.registration.package_full_name
                && candidate.registration.application_id == node.registration.application_id
                && candidate.registration.verb_id == node.registration.verb_id
                && candidate.registration.clsid == node.registration.clsid);
            let candidate = matches.next().ok_or_else(|| Error::new(ABORT, "subcommand path is retired"))?;
            if matches.next().is_some() { return Err(Error::new(ABORT, "subcommand identity is ambiguous")); }
            let canonical = candidate.canonical.filter(|id| *id != GUID::zeroed())
                .ok_or_else(|| Error::new(ABORT, "slow subcommand requires a non-null canonical identity"))?;
            result.push(state_worker::ChildStep { ordinal: node.path[depth-1], canonical: format!("{canonical:?}") });
        }
        Ok(result)
    }
    fn finish_slow_error(&self, node: &Node, error: &Error) {
        node.pending.set(false);
        node.requested.set(true);
        node.proof.set(None);
        self.disable_node(node);
        let mut text = wide(&format!("{}（状态暂不可用）", node.title.trim_end_matches('…')));
        let info = MENUITEMINFOW { cbSize: size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_STRING | MIIM_STATE, fState: MFS_DISABLED,
            dwTypeData: PWSTR(text.as_mut_ptr()), ..Default::default() };
        if node.active.get() { let _ = unsafe { SetMenuItemInfoW(node.parent_menu, node.id, false, &info) }; }
        self.issue(Some(&node.registration), "slow-state", Some(error.code()), error.message(), true);
        repaint_menu(node.parent_menu, self.owner);
    }
    fn fail_waiting(&self, error: &Error) {
        let nodes: Vec<_> = self.state.try_borrow().map(|data| data.nodes.values()
            .filter(|node| node.active.get() && node.pending.get()).cloned().collect()).unwrap_or_default();
        for node in nodes { self.finish_slow_error(&node, error); }
        self.cancel_slow(false);
    }
    fn poll_slow(&self) {
        if self.polling.replace(true) { return; }
        struct Reset<'a>(&'a Cell<bool>); impl Drop for Reset<'_> { fn drop(&mut self) { self.0.set(false); } }
        let _reset = Reset(&self.polling);
        let ready = self.state.try_borrow().is_ok_and(|data| data.ready && !data.querying);
        if !ready { return; }
        if self.slow.try_borrow().is_ok_and(|slow| slow.closed) { return; }
        let expired = self.slow.try_borrow().is_ok_and(|slow| slow.started.is_some_and(|at| at.elapsed() > SLOW_QUEUE_LIMIT));
        if expired { self.fail_waiting(&error(0x800705b4, "packaged state queue deadline exceeded")); return; }
        let generation = self.generation.get();
        let mut jobs = match self.slow.try_borrow_mut() { Ok(mut slow) => std::mem::take(&mut slow.jobs), Err(_) => return };
        let mut completed = Vec::new();
        jobs.retain(|id, ticket| {
            let current = self.node(*id).ok().flatten().is_some_and(|node| node.active.get() && node.pending.get());
            if ticket.request.generation != generation || !current { return false; }
            if let Some(response) = ticket.poll() { completed.push((*id, response)); false } else { true }
        });
        if let Ok(mut slow) = self.slow.try_borrow_mut() { slow.jobs = jobs; }
        for (id, result) in completed {
            let Some(node) = self.node(id).ok().flatten().filter(|node| node.active.get()) else { continue; };
            let outcome = result.and_then(|response| {
                if self.generation.get() != generation || response.generation != generation || response.command_id != id {
                    return Err(Error::new(ABORT, "stale packaged state response"));
                }
                HRESULT(response.hresult).ok()?;
                let state = response.state.ok_or_else(|| Error::new(ABORT, "slow state response has no state"))?;
                let expected = self.slow.try_borrow().map_err(|_| Error::from_hresult(PENDING))?.snapshot.clone()
                    .ok_or_else(|| Error::new(ABORT, "slow state snapshot was cancelled"))?;
                let snapshot = snapshot_key(&expected)?;
                if snapshot_key(&self.state_snapshot()?)? != snapshot { return Err(Error::new(ABORT, "view changed during slow state query")); }
                self.apply_slow(&node, SlowProof { generation, state, snapshot })
            });
            if let Err(error) = outcome { self.finish_slow_error(&node, &error); }
        }
        let waiting: Vec<_> = match self.state.try_borrow() {
            Ok(data) => { let mut nodes: Vec<_> = data.nodes.values()
                .filter(|node| node.active.get() && node.pending.get() && !node.requested.get()).cloned().collect();
                nodes.sort_by_key(|node| node.id); nodes }, Err(_) => return,
        };
        for node in waiting {
            if self.generation.get() != generation { return; }
            if self.slow.try_borrow().is_ok_and(|slow| slow.submitted >= MAX_SLOW_REQUESTS) {
                self.finish_slow_error(&node, &error(0x8007000e, "packaged slow-state request budget exhausted")); continue;
            }
            let result = (|| {
                let snapshot = self.slow.try_borrow().map_err(|_| Error::from_hresult(PENDING))?.snapshot.clone();
                let snapshot = match snapshot {
                    Some(snapshot) => snapshot,
                    None => { let snapshot = self.state_snapshot()?;
                        self.slow.try_borrow_mut().map_err(|_| Error::from_hresult(PENDING))?.snapshot = Some(snapshot.clone()); snapshot }
                };
                state_worker::start(&node.registration, &snapshot, &self.child_steps(&node)?, generation, node.id)
            })();
            match result {
                Ok(Some(ticket)) => { node.requested.set(true);
                    if let Ok(mut slow) = self.slow.try_borrow_mut() { slow.submitted += 1; slow.jobs.insert(node.id, ticket); }
                }
                Ok(None) => break,
                Err(error) => self.finish_slow_error(&node, &error),
            }
        }
        let pending = self.state.try_borrow().is_ok_and(|data| data.nodes.values().any(|node| node.active.get() && node.pending.get()));
        if !pending { let hwnd = self.slow_window.get(); if !hwnd.0.is_null() { let _ = unsafe { KillTimer(Some(hwnd), SLOW_TIMER) }; } }
    }
    fn current_state(&self, node: &Node) -> Result<u32> {
        let fast = unsafe { node.command.GetState(self.context.selection.as_ref(), false) };
        if fast.as_ref().is_err_and(|error| error.code() == PENDING) {
            let snapshot = snapshot_key(&self.state_snapshot()?)?;
            proven_state(fast, node.proof.get(), self.generation.get(), snapshot)
        } else { fast }
    }
    fn apply_slow(&self, old: &Rc<Node>, proof: SlowProof) -> Result<()> {
        if proof.generation != self.generation.get() || !old.active.get() { return Err(Error::new(ABORT, "retired slow state")); }
        let root = self.state.try_borrow().map_err(|_| Error::from_hresult(PENDING))?.root
            .ok_or_else(|| Error::new(ABORT, "menu is detached"))?;
        if !command_tokens_match(root, old.id, old.token())? { return Err(Error::new(ABORT, "slow command identity no longer matches menu")); }
        if proof.state & ECS_HIDDEN.0 as u32 != 0 {
            unsafe { DeleteMenu(old.parent_menu, old.id, MF_BYCOMMAND) }?;
            old.active.set(false); old.pending.set(false); old.proof.set(None);
            self.state.borrow_mut().diagnostics.hidden_commands += 1;
            repaint_menu(old.parent_menu, self.owner);
            return Ok(());
        }
        // Resolve metadata again after a successful slow query. A pending title
        // is allowed to be provisional; the item token remains unchanged.
        let flags = unsafe { old.command.GetFlags() }?;
        let title = if flags & ECF_ISSEPARATOR.0 as u32 != 0 { String::new() }
            else { owned_text(unsafe { old.command.GetTitle(self.context.selection.as_ref()) }?)? };
        let (icon, icon_hresult) = match unsafe { old.command.GetIcon(self.context.selection.as_ref()) }.and_then(owned_text) {
            Ok(value) => ((!value.is_empty()).then_some(value), None), Err(error) => (None, Some(error.code().0)),
        };
        let bitmap = icon.as_ref().and_then(|resource| bitmap_from_icon(resource, self.owner).ok());
        let submenu = if flags & ECF_HASSUBCOMMANDS.0 as u32 != 0 {
            match old.submenu { Some(menu) => Some(menu), None => {
                let menu = unsafe { CreatePopupMenu() }?;
                if let Err(error) = insert_notice(menu, "正在读取应用命令…") { let _ = unsafe { DestroyMenu(menu) }; return Err(error); }
                Some(menu)
            } }
        } else { None };
        let node = Rc::new(Node { id: old.id, command: old.command.clone(), registration: old.registration.clone(),
            canonical: old.canonical, path: old.path.clone(), stable: old.stable, verb: old.verb.clone(), title,
            icon, icon_hresult, bitmap, flags, state: Cell::new(Some(proof.state)), pending: Cell::new(false),
            active: Cell::new(flags & ECF_ISSEPARATOR.0 as u32 == 0), requested: Cell::new(true), proof: Cell::new(Some(proof)),
            submenu, parent_menu: old.parent_menu, children: RefCell::new(old.children.borrow().clone()), marker: old.marker.clone() });
        let mut text = wide(&node.title);
        let info = MENUITEMINFOW { cbSize: size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_STATE | MIIM_STRING | MIIM_FTYPE | MIIM_BITMAP | MIIM_SUBMENU,
            fState: menu_state(Some(proof.state), false),
            fType: if flags & ECF_ISSEPARATOR.0 as u32 != 0 { MFT_SEPARATOR }
                else if proof.state & ECS_RADIOCHECK.0 as u32 != 0 { MFT_RADIOCHECK } else { MFT_STRING },
            dwTypeData: PWSTR(text.as_mut_ptr()), hbmpItem: node.bitmap.as_ref().map_or(HBITMAP::default(), |bitmap| bitmap.0),
            hSubMenu: submenu.unwrap_or_default(), ..Default::default() };
        if let Err(error) = unsafe { SetMenuItemInfoW(node.parent_menu, node.id, false, &info) } {
            if submenu != old.submenu { if let Some(menu) = submenu { let _ = unsafe { DestroyMenu(menu) }; } }
            return Err(error);
        }
        {
            let mut data = self.state.try_borrow_mut().map_err(|_| Error::from_hresult(PENDING))?;
            if let Some(bitmap) = &old.bitmap { data.retired_bitmaps.push(bitmap.clone()); }
            if let Some(menu) = old.submenu.filter(|menu| Some(*menu) != submenu) {
                data.menus.remove(&(menu.0 as usize)); let _ = unsafe { DestroyMenu(menu) };
            }
            if let Some(menu) = submenu { data.menus.insert(menu.0 as usize, node.id); }
            data.nodes.insert(node.id, node.clone());
        }
        old.active.set(false); old.pending.set(false);
        repaint_menu(node.parent_menu, self.owner);
        Ok(())
    }
}

fn menu_state(state: Option<u32>, pending: bool) -> MENU_ITEM_STATE {
    let mut flags = if pending
        || state.is_none_or(|state| state & (ECS_DISABLED.0 | ECS_HIDDEN.0) as u32 != 0)
    {
        MFS_DISABLED
    } else {
        MFS_ENABLED
    };
    if state.is_some_and(|state| state & ECS_CHECKED.0 as u32 != 0) {
        flags |= MFS_CHECKED;
    }
    flags
}
fn insert_separator(menu: HMENU, index: u32) -> Result<()> {
    let info = MENUITEMINFOW {
        cbSize: size_of::<MENUITEMINFOW>() as u32,
        fMask: MIIM_FTYPE,
        fType: MFT_SEPARATOR,
        ..Default::default()
    };
    unsafe { InsertMenuItemW(menu, index, true, &info) }
}
fn insert_notice(menu: HMENU, text: &str) -> Result<()> {
    let mut text = wide(text);
    let info = MENUITEMINFOW {
        cbSize: size_of::<MENUITEMINFOW>() as u32,
        fMask: MIIM_STATE | MIIM_STRING,
        fState: MFS_DISABLED,
        dwTypeData: PWSTR(text.as_mut_ptr()),
        ..Default::default()
    };
    unsafe { InsertMenuItemW(menu, u32::MAX, true, &info) }
}
fn walk_menu(menu: HMENU, mut visitor: impl FnMut(&MENUITEMINFOW) -> Result<()>) -> Result<()> {
    let mut pending = vec![(menu, 0usize)];
    let mut seen = HashSet::new();
    let mut rows = 0;
    while let Some((menu, depth)) = pending.pop() {
        if depth > MAX_DEPTH {
            return Err(error(0x80070057, "excessive menu nesting"));
        }
        if !seen.insert(menu.0 as usize) {
            continue;
        }
        let count = unsafe { GetMenuItemCount(Some(menu)) };
        if count < 0 {
            return Err(Error::from_thread());
        }
        for index in 0..count as u32 {
            rows += 1;
            if rows > MAX_MENU_ROWS {
                return Err(error(0x8007000e, "menu inspection bound exceeded"));
            }
            let mut info = MENUITEMINFOW {
                cbSize: size_of::<MENUITEMINFOW>() as u32,
                fMask: MIIM_ID | MIIM_SUBMENU | MIIM_DATA | MIIM_FTYPE,
                ..Default::default()
            };
            unsafe { GetMenuItemInfoW(menu, index, true, &mut info) }?;
            visitor(&info)?;
            if !info.hSubMenu.0.is_null() {
                pending.push((info.hSubMenu, depth + 1));
            }
        }
    }
    Ok(())
}
fn menu_has_reserved_command(menu: HMENU, first: u32, last: u32) -> Result<bool> {
    let mut conflict = false;
    walk_menu(menu, |item| {
        if item.fType & MFT_SEPARATOR != MFT_SEPARATOR && item.wID >= first && item.wID <= last {
            conflict = true;
        }
        Ok(())
    })?;
    Ok(conflict)
}
fn command_tokens_match(menu: HMENU, id: u32, token: usize) -> Result<bool> {
    let mut count = 0;
    let mut valid = true;
    walk_menu(menu, |item| {
        if item.wID == id {
            count += 1;
            valid &= item.dwItemData == token;
        }
        Ok(())
    })?;
    Ok(count > 0 && valid)
}
unsafe fn borrowed_ansi(pointer: *const u8) -> Result<String> {
    if pointer.is_null() {
        return Err(error(0x80004003, "null command text"));
    }
    for len in 0..=MAX_TEXT {
        if unsafe { *pointer.add(len) } == 0 {
            return Ok(String::from_utf8_lossy(unsafe {
                std::slice::from_raw_parts(pointer, len)
            })
            .into_owned());
        }
    }
    Err(error(0x8007007a, "command text exceeds bound"))
}
unsafe fn write_command_text(target: PSTR, count: u32, text: &str, unicode: bool) -> Result<()> {
    if target.0.is_null() || count == 0 {
        return Err(error(0x80070057, "invalid command string buffer"));
    }
    if unicode {
        let value = wide(text);
        unsafe {
            *target.0.cast::<u16>() = 0;
        }
        if value.len() > count as usize {
            return Err(error(0x8007007a, "command string buffer is too small"));
        }
        unsafe {
            std::ptr::copy_nonoverlapping(value.as_ptr(), target.0.cast(), value.len());
        }
    } else {
        unsafe {
            *target.0 = 0;
        }
        if !text.is_ascii() {
            return Err(error(
                0x80004001,
                "Unicode command help requires GCS_HELPTEXTW",
            ));
        }
        if text.len() + 1 > count as usize {
            return Err(error(0x8007007a, "command string buffer is too small"));
        }
        unsafe {
            std::ptr::copy_nonoverlapping(text.as_ptr(), target.0, text.len());
            *target.0.add(text.len()) = 0;
        }
    }
    Ok(())
}

#[implement(IContextMenu3, IObjectWithSite, Agile = false)]
struct Overlay {
    shared: Rc<Shared>,
}
impl IContextMenu_Impl for Overlay_Impl {
    fn QueryContextMenu(
        &self,
        menu: HMENU,
        index: u32,
        first: u32,
        last: u32,
        flags: u32,
    ) -> HRESULT {
        {
            let Ok(mut state) = self.shared.state.try_borrow_mut() else {
                return PENDING;
            };
            if state.querying {
                return PENDING;
            }
            state.querying = true;
            state.ready = false;
            state.diagnostics.query_ready = false;
            for node in state.nodes.values() {
                node.active.set(false);
            }
        }
        self.shared.cancel_slow(true);
        let result = self.shared.query(menu, index, first, last, flags);
        if let Ok(mut state) = self.shared.state.try_borrow_mut() {
            state.querying = false;
            if result.is_err() {
                state.ready = false;
                state.diagnostics.query_ready = false;
            }
        }
        match result {
            Ok(used) => HRESULT(used as i32),
            Err(error) => {
                self.shared.issue(
                    None,
                    "QueryContextMenu",
                    Some(error.code()),
                    error.message(),
                    true,
                );
                error.code()
            }
        }
    }
    fn InvokeCommand(&self, info: *const CMINVOKECOMMANDINFO) -> Result<()> {
        self.shared.invoke(info)
    }
    fn GetCommandString(
        &self,
        offset: usize,
        kind: u32,
        reserved: *const u32,
        target: PSTR,
        count: u32,
    ) -> Result<()> {
        let range = {
            let state = self
                .shared
                .state
                .try_borrow()
                .map_err(|_| error(0x8000000a, "menu is reentrant"))?;
            if !state.ready || state.querying {
                return Err(Error::from_hresult(PENDING));
            }
            state
                .range
                .ok_or_else(|| error(0x80070057, "menu has not been queried"))?
        };
        let id = range
            .absolute(offset)
            .ok_or_else(|| error(0x80070057, "invalid command offset"))?;
        if range.legacy(id) {
            return unsafe {
                self.shared.base.GetCommandString(
                    offset,
                    kind,
                    (!reserved.is_null()).then_some(reserved),
                    target,
                    count,
                )
            };
        }
        let node = self
            .shared
            .node(id)?
            .filter(|node| node.active.get())
            .ok_or_else(|| error(0x80070057, "unknown packaged command"))?;
        match kind {
            GCS_VALIDATEA | GCS_VALIDATEW => Ok(()),
            GCS_VERBA | GCS_VERBW => {
                let data = self
                    .shared
                    .state
                    .try_borrow()
                    .map_err(|_| error(0x8000000a, "menu is reentrant"))?;
                if data
                    .nodes
                    .values()
                    .filter(|candidate| candidate.active.get() && candidate.verb == node.verb)
                    .count()
                    != 1
                {
                    return Err(error(
                        0x80004001,
                        "packaged canonical command is not unique",
                    ));
                }
                drop(data);
                unsafe { write_command_text(target, count, &node.verb, kind == GCS_VERBW) }
            }
            GCS_HELPTEXTA | GCS_HELPTEXTW => {
                let text = unsafe {
                    node.command
                        .GetToolTip(self.shared.context.selection.as_ref())
                }
                .and_then(owned_text)?;
                unsafe { write_command_text(target, count, &text, kind == GCS_HELPTEXTW) }
            }
            _ => Err(error(0x80070057, "unsupported command string kind")),
        }
    }
}
impl IContextMenu2_Impl for Overlay_Impl {
    fn HandleMenuMsg(&self, message: u32, wp: WPARAM, lp: LPARAM) -> Result<()> {
        self.shared.message(message, wp, lp, std::ptr::null_mut())
    }
}
impl IContextMenu3_Impl for Overlay_Impl {
    fn HandleMenuMsg2(
        &self,
        message: u32,
        wp: WPARAM,
        lp: LPARAM,
        result: *mut LRESULT,
    ) -> Result<()> {
        self.shared.message(message, wp, lp, result)
    }
}
impl IObjectWithSite_Impl for Overlay_Impl {
    fn SetSite(&self, site: Ref<IUnknown>) -> Result<()> {
        self.shared.cancel_slow(true);
        let site = site.cloned();
        let old = {
            let mut current = self
                .shared
                .active_site
                .try_borrow_mut()
                .map_err(|_| error(0x8000000a, "site is reentrant"))?;
            std::mem::replace(&mut *current, site.clone())
        };
        drop(old);
        if let Ok(base) = self.shared.base.cast::<IObjectWithSite>() {
            if let Err(error) = unsafe { base.SetSite(site.as_ref()) } {
                self.shared.invalidate_query();
                self.shared.issue(
                    None,
                    "legacy-SetSite",
                    Some(error.code()),
                    error.message(),
                    true,
                );
                return Err(error);
            }
        }
        let nodes: Vec<_> = self
            .shared
            .state
            .try_borrow()
            .map_err(|_| error(0x8000000a, "menu is reentrant"))?
            .nodes
            .values()
            .cloned()
            .collect();
        for node in nodes {
            if let Ok(object) = node.command.cast::<IObjectWithSite>() {
                if let Err(error) = unsafe { object.SetSite(site.as_ref()) } {
                    self.shared.disable_node(&node);
                    self.shared.issue(
                        Some(&node.registration),
                        "SetSite",
                        Some(error.code()),
                        error.message(),
                        true,
                    );
                }
            }
        }
        Ok(())
    }
    fn GetSite(&self, iid: *const GUID, out: *mut *mut c_void) -> Result<()> {
        if iid.is_null() || out.is_null() {
            return Err(error(0x80004003, "invalid site query"));
        }
        unsafe {
            *out = std::ptr::null_mut();
        }
        let site = self
            .shared
            .site()?
            .ok_or_else(|| error(0x80004005, "site is not set"))?;
        unsafe { site.query(iid, out).ok() }
    }
}

/// Captures the supplied real view and immutable selected-item array. Call only
/// for full context menus, never for a dedicated ShellNew template menu or the
/// native Win11 presenter (which performs its own packaged discovery).
pub(super) fn create(
    base: IContextMenu,
    browser: IExplorerBrowser,
    view: IShellView,
    owner: HWND,
    background: bool,
) -> Result<IContextMenu> {
    if find(&base).is_some() {
        return Err(error(
            0x80070057,
            "context menu already has a packaged overlay",
        ));
    }
    let context = Context::capture(&view, background)?;
    let object = base.cast::<IObjectWithSite>().ok();
    let original_site = object
        .as_ref()
        .and_then(|object| unsafe { object.GetSite::<IUnknown>() }.ok());
    let site: IUnknown = RealSite {
        browser: browser.clone(),
        view: view.clone(),
        owner,
        previous: original_site.clone(),
    }
    .into();
    if let Some(object) = &object {
        unsafe { object.SetSite(&site) }?;
    }
    let shared = Rc::new(Shared {
        base,
        browser: Some(browser),
        view: Some(view),
        owner,
        context,
        original_site,
        active_site: RefCell::new(Some(site)),
        state: RefCell::new(State::default()),
        identity: Cell::new(0),
        dropped: Cell::new(0),
        generation: Cell::new(1),
        slow: RefCell::new(SlowState::default()),
        slow_window: Cell::new(HWND::default()),
        polling: Cell::new(false),
    });
    let menu: IContextMenu3 = Overlay {
        shared: shared.clone(),
    }
    .into();
    let menu = menu.cast::<IContextMenu>()?;
    let id = identity(&menu).ok_or_else(|| error(0x80004002, "context identity unavailable"))?;
    shared.identity.set(id);
    OVERLAYS.with(|items| items.borrow_mut().insert(id, Rc::downgrade(&shared)));
    Ok(menu)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Com::{CoTaskMemAlloc, IBindCtx};
    struct Menu(HMENU);
    impl Menu {
        fn new() -> Self {
            Self(unsafe { CreatePopupMenu() }.unwrap())
        }
    }
    impl Drop for Menu {
        fn drop(&mut self) {
            let _ = unsafe { DestroyMenu(self.0) };
        }
    }
    #[implement(IContextMenu)]
    struct MockLegacy {
        calls: Rc<Cell<usize>>,
    }
    impl IContextMenu_Impl for MockLegacy_Impl {
        fn QueryContextMenu(
            &self,
            menu: HMENU,
            index: u32,
            first: u32,
            _last: u32,
            _flags: u32,
        ) -> HRESULT {
            let mut text = wide("Legacy");
            let info = MENUITEMINFOW {
                cbSize: size_of::<MENUITEMINFOW>() as u32,
                fMask: MIIM_ID | MIIM_STRING,
                wID: first,
                dwTypeData: PWSTR(text.as_mut_ptr()),
                ..Default::default()
            };
            match unsafe { InsertMenuItemW(menu, index, true, &info) } {
                Ok(()) => HRESULT(1),
                Err(error) => error.code(),
            }
        }
        fn InvokeCommand(&self, _: *const CMINVOKECOMMANDINFO) -> Result<()> {
            self.calls.set(self.calls.get() + 1);
            Ok(())
        }
        fn GetCommandString(
            &self,
            _: usize,
            kind: u32,
            _: *const u32,
            target: PSTR,
            count: u32,
        ) -> Result<()> {
            unsafe { write_command_text(target, count, "mock.legacy", kind & GCS_UNICODE != 0) }
        }
    }
    #[implement(IExplorerCommand, IObjectWithSite)]
    struct MockCommand {
        calls: Rc<Cell<usize>>,
        state_calls: Rc<Cell<usize>>,
        fail_site: Rc<Cell<bool>>,
        fail_title: bool,
        children: Vec<IExplorerCommand>,
    }
    impl IExplorerCommand_Impl for MockCommand_Impl {
        fn GetTitle(&self, _: Ref<IShellItemArray>) -> Result<PWSTR> {
            if self.fail_title {
                return Err(error(0x80004005, "mock title failure"));
            }
            let text = wide("Package command");
            let pointer = unsafe { CoTaskMemAlloc(text.len() * 2) }.cast::<u16>();
            if pointer.is_null() {
                return Err(error(0x8007000e, "allocation failed"));
            }
            unsafe {
                std::ptr::copy_nonoverlapping(text.as_ptr(), pointer, text.len());
            }
            Ok(PWSTR(pointer))
        }
        fn GetIcon(&self, _: Ref<IShellItemArray>) -> Result<PWSTR> {
            Err(error(0x80004001, "no icon"))
        }
        fn GetToolTip(&self, _: Ref<IShellItemArray>) -> Result<PWSTR> {
            Err(error(0x80004001, "no tooltip"))
        }
        fn GetCanonicalName(&self) -> Result<GUID> {
            Ok(GUID::zeroed())
        }
        fn GetState(&self, _: Ref<IShellItemArray>, _: BOOL) -> Result<u32> {
            self.state_calls.set(self.state_calls.get() + 1);
            Ok(0)
        }
        fn Invoke(&self, _: Ref<IShellItemArray>, _: Ref<IBindCtx>) -> Result<()> {
            self.calls.set(self.calls.get() + 1);
            Ok(())
        }
        fn GetFlags(&self) -> Result<u32> {
            Ok(if self.children.is_empty() {
                0
            } else {
                ECF_HASSUBCOMMANDS.0 as u32
            })
        }
        fn EnumSubCommands(&self) -> Result<IEnumExplorerCommand> {
            Ok(MockEnumeration {
                items: self.children.clone(),
                next: Cell::new(0),
            }
            .into())
        }
    }
    impl IObjectWithSite_Impl for MockCommand_Impl {
        fn SetSite(&self, _: Ref<IUnknown>) -> Result<()> {
            if self.fail_site.get() {
                Err(error(0x80004005, "mock site failure"))
            } else {
                Ok(())
            }
        }
        fn GetSite(&self, _: *const GUID, out: *mut *mut c_void) -> Result<()> {
            if !out.is_null() {
                unsafe {
                    *out = std::ptr::null_mut();
                }
            }
            Err(error(0x80004002, "mock has no site"))
        }
    }
    #[implement(IEnumExplorerCommand)]
    struct MockEnumeration {
        items: Vec<IExplorerCommand>,
        next: Cell<usize>,
    }
    impl IEnumExplorerCommand_Impl for MockEnumeration_Impl {
        fn Next(
            &self,
            count: u32,
            items: *mut Option<IExplorerCommand>,
            fetched: *mut u32,
        ) -> HRESULT {
            if items.is_null() || fetched.is_null() {
                return HRESULT(0x80004003u32 as i32);
            }
            let first = self.next.get();
            let size = (count as usize).min(self.items.len().saturating_sub(first));
            for index in 0..size {
                unsafe {
                    items
                        .add(index)
                        .write(Some(self.items[first + index].clone()));
                }
            }
            self.next.set(first + size);
            unsafe {
                *fetched = size as u32;
            }
            if size == count as usize {
                HRESULT(0)
            } else {
                HRESULT(1)
            }
        }
        fn Skip(&self, count: u32) -> Result<()> {
            self.next.set(
                self.next
                    .get()
                    .saturating_add(count as usize)
                    .min(self.items.len()),
            );
            Ok(())
        }
        fn Reset(&self) -> Result<()> {
            self.next.set(0);
            Ok(())
        }
        fn Clone(&self) -> Result<IEnumExplorerCommand> {
            Ok(MockEnumeration {
                items: self.items.clone(),
                next: Cell::new(self.next.get()),
            }
            .into())
        }
    }
    fn shared_mock(menu: HMENU) -> (Rc<Shared>, IContextMenu, Rc<Cell<usize>>) {
        let calls = Rc::new(Cell::new(0));
        let base: IContextMenu = MockLegacy {
            calls: calls.clone(),
        }
        .into();
        let shared = Rc::new(Shared {
            base,
            browser: None,
            view: None,
            owner: HWND::default(),
            context: Context {
                selection: None,
                scene: ContextScene::DirectoryBackground,
            },
            original_site: None,
            active_site: RefCell::new(None),
            state: RefCell::new(State {
                range: Some(IdRange::new(1, LAST_ALLOWED, true).unwrap()),
                root: Some(menu),
                ready: true,
                ..Default::default()
            }),
            identity: Cell::new(0),
            dropped: Cell::new(0),
        generation: Cell::new(1),
        slow: RefCell::new(SlowState::default()),
        slow_window: Cell::new(HWND::default()),
        polling: Cell::new(false),
        });
        let context: IContextMenu3 = Overlay {
            shared: shared.clone(),
        }
        .into();
        (shared, context.cast().unwrap(), calls)
    }
    fn command_mock(
        fail_title: bool,
        fail_site: Rc<Cell<bool>>,
        children: Vec<IExplorerCommand>,
    ) -> (IExplorerCommand, Rc<Cell<usize>>, Rc<Cell<usize>>) {
        let calls = Rc::new(Cell::new(0));
        let states = Rc::new(Cell::new(0));
        (
            MockCommand {
                calls: calls.clone(),
                state_calls: states.clone(),
                fail_site,
                fail_title,
                children,
            }
            .into(),
            calls,
            states,
        )
    }
    fn invocation(offset: u32) -> CMINVOKECOMMANDINFO {
        CMINVOKECOMMANDINFO {
            cbSize: size_of::<CMINVOKECOMMANDINFO>() as u32,
            lpVerb: windows::core::PCSTR(offset as usize as *const u8),
            ..Default::default()
        }
    }

    #[test]
    fn failed_requery_invalidates_previous_commands_and_strings() {
        let menu = Menu::new();
        let (_shared, context, calls) = shared_mock(menu.0);
        unsafe { context.QueryContextMenu(menu.0, 0, 1, LAST_ALLOWED, CMF_DEFAULTONLY) }
            .ok()
            .unwrap();
        unsafe { context.InvokeCommand(&invocation(0)) }.unwrap();
        assert_eq!(calls.get(), 1);
        assert!(
            unsafe { context.QueryContextMenu(menu.0, 0, 0, LAST_ALLOWED, CMF_DEFAULTONLY) }
                .is_err()
        );
        assert!(unsafe { context.InvokeCommand(&invocation(0)) }.is_err());
        assert_eq!(calls.get(), 1);
        let mut text = [0u16; 128];
        assert!(unsafe {
            context.GetCommandString(0, GCS_VERBW, None, PSTR(text.as_mut_ptr().cast()), 128)
        }
        .is_err());
    }
    #[test]
    fn site_failure_disables_package_command_and_blocks_invoke_before_state_recheck() {
        let menu = Menu::new();
        let (shared, context, _) = shared_mock(menu.0);
        let site_failure = Rc::new(Cell::new(false));
        let (command, calls, states) = command_mock(false, site_failure.clone(), Vec::new());
        let id = shared
            .add(menu.0, 0, command, registration("site"), Vec::new())
            .unwrap()
            .unwrap();
        let states_before = states.get();
        site_failure.set(true);
        let object: IObjectWithSite = context.cast().unwrap();
        unsafe { object.SetSite(None) }.unwrap();
        assert!(unsafe { context.InvokeCommand(&invocation(id - 1)) }.is_err());
        assert_eq!(calls.get(), 0);
        assert_eq!(states.get(), states_before);
        let mut info = MENUITEMINFOW {
            cbSize: size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_STATE,
            ..Default::default()
        };
        unsafe { GetMenuItemInfoW(menu.0, id, false, &mut info) }.unwrap();
        assert_eq!(info.fState & MFS_DISABLED, MFS_DISABLED);
    }
    #[test]
    fn partial_submenu_failure_keeps_good_child_and_adds_visible_disabled_notice() {
        let menu = Menu::new();
        let (shared, _context, _) = shared_mock(menu.0);
        let no_failure = Rc::new(Cell::new(false));
        let (good, _, _) = command_mock(false, no_failure.clone(), Vec::new());
        let (bad, _, _) = command_mock(true, no_failure.clone(), Vec::new());
        let (parent, _, _) = command_mock(false, no_failure, vec![good, bad]);
        let id = shared
            .add(menu.0, 0, parent, registration("children"), Vec::new())
            .unwrap()
            .unwrap();
        let node = shared.node(id).unwrap().unwrap();
        let submenu = node.submenu.unwrap();
        shared.populate_submenu(submenu, id).unwrap();
        assert_eq!(node.children.borrow().len(), 1);
        assert_eq!(unsafe { GetMenuItemCount(Some(submenu)) }, 2);
        let mut text = [0u16; 128];
        let length = unsafe { GetMenuStringW(submenu, 1, Some(&mut text), MF_BYPOSITION) };
        assert!(String::from_utf16_lossy(&text[..length as usize]).contains("部分应用命令"));
        let mut info = MENUITEMINFOW {
            cbSize: size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_STATE,
            ..Default::default()
        };
        unsafe { GetMenuItemInfoW(submenu, 1, true, &mut info) }.unwrap();
        assert_eq!(info.fState & MFS_DISABLED, MFS_DISABLED);
        assert!(!shared.state.borrow().diagnostics.supported_sources_complete);
    }
    fn registration(verb: &str) -> Registration {
        Registration {
            package_family: "test-family".into(),
            package_full_name: "test-full".into(),
            application_id: "App".into(),
            verb_id: verb.into(),
            item_type: "*".into(),
            clsid: GUID::from_u128(1),
        }
    }
    #[test]
    fn id_ranges_never_overlap_or_accept_out_of_range_commands() {
        let mut range = IdRange::new(1, 0x6fff, true).unwrap();
        assert_eq!(range.legacy_last, 0x67ff);
        range.legacy_used = 5;
        assert!(range.legacy(5));
        assert!(!range.legacy(6));
        for id in 0x6800..=0x6fff {
            assert_eq!(range.allocate().unwrap(), id);
        }
        assert!(range.allocate().is_err());
        assert_eq!(range.absolute(0x6ffe), Some(0x6fff));
        assert_eq!(range.absolute(0x6fff), None);
        assert!(IdRange::new(0, 10, true).is_err());
        assert!(IdRange::new(1, 0x7000, true).is_err());
    }
    #[test]
    fn null_guids_and_equal_labels_do_not_merge_distinct_commands() {
        let first = command_key(&registration("first"), &[], Some(GUID::zeroed()));
        let second = command_key(&registration("second"), &[], Some(GUID::zeroed()));
        assert_ne!(first.0, second.0);
        assert!(first.1 && second.1);
        let child = command_key(&registration("first"), &[0], Some(GUID::zeroed()));
        assert!(!child.1);
        assert_ne!(
            child.0,
            command_key(&registration("first"), &[1], Some(GUID::zeroed())).0
        );
    }
    #[test]
    fn pending_and_hidden_states_are_never_enabled() {
        assert!(menu_state(None, true) & MFS_DISABLED == MFS_DISABLED);
        assert!(menu_state(Some(ECS_HIDDEN.0 as u32), false) & MFS_DISABLED == MFS_DISABLED);
        assert!(menu_state(Some(ECS_CHECKED.0 as u32), false) & MFS_CHECKED == MFS_CHECKED);
        assert_eq!(menu_state(Some(0), false), MFS_ENABLED);
    }
    #[test]
    fn slow_state_requires_the_same_selection_and_query_generation() {
        let proof = SlowProof { generation: 4, state: 0, snapshot: [7; 32] };
        let pending = || Err(Error::from_hresult(PENDING));
        assert_eq!(proven_state(pending(), Some(proof), 4, [7; 32]).unwrap(), 0);
        assert!(proven_state(pending(), Some(proof), 5, [7; 32]).is_err());
        assert!(proven_state(pending(), Some(proof), 4, [8; 32]).is_err());
        assert!(proven_state(pending(), None, 4, [7; 32]).is_err());
        assert_eq!(proven_state(Ok(ECS_DISABLED.0 as u32), Some(proof), 4, [7; 32]).unwrap(), ECS_DISABLED.0 as u32);
        assert!(proven_state(Err(Error::from_hresult(ABORT)), Some(proof), 4, [7; 32]).is_err());
    }
    #[test]
    fn resolved_slow_state_preserves_item_identity_and_cancel_invalidates_it() {
        let menu = Menu::new();
        let (shared, context, _) = shared_mock(menu.0);
        let (command, calls, _) = command_mock(false, Rc::new(Cell::new(false)), Vec::new());
        let id = shared.add(menu.0, 0, command, registration("slow"), Vec::new()).unwrap().unwrap();
        let old = shared.node(id).unwrap().unwrap();
        old.pending.set(true);
        shared.disable_node(&old);
        let proof = SlowProof { generation: shared.generation.get(), state: 0, snapshot: [0; 32] };
        shared.apply_slow(&old, proof).unwrap();
        let current = shared.node(id).unwrap().unwrap();
        assert_eq!(old.token(), current.token());
        assert!(!old.active.get() && current.active.get() && !current.pending.get());
        assert!(command_tokens_match(menu.0, id, current.token()).unwrap());
        unsafe { context.InvokeCommand(&invocation(id - 1)) }.unwrap();
        assert_eq!(calls.get(), 1);
        shared.cancel_slow(true);
        assert!(current.proof.get().is_none());
        assert!(shared.apply_slow(&current, proof).is_err());
        assert!(shared.slow.borrow().closed);
    }
    #[test]
    fn hidden_slow_result_removes_the_command_and_rejects_old_invocation() {
        let menu = Menu::new();
        let (shared, context, _) = shared_mock(menu.0);
        let (command, calls, _) = command_mock(false, Rc::new(Cell::new(false)), Vec::new());
        let id = shared.add(menu.0, 0, command, registration("hidden-slow"), Vec::new()).unwrap().unwrap();
        let node = shared.node(id).unwrap().unwrap();
        shared.apply_slow(&node, SlowProof { generation: shared.generation.get(), state: ECS_HIDDEN.0 as u32, snapshot: [0; 32] }).unwrap();
        assert_eq!(unsafe { GetMenuItemCount(Some(menu.0)) }, 0);
        assert!(unsafe { context.InvokeCommand(&invocation(id - 1)) }.is_err());
        assert_eq!(calls.get(), 0);
    }
    #[test]
    fn command_text_respects_utf16_buffer_boundaries() {
        let mut output = [0xbeefu16; 6];
        unsafe { write_command_text(PSTR(output.as_mut_ptr().cast()), 6, "中文", true) }.unwrap();
        assert_eq!(&output[..3], &[0x4e2d, 0x6587, 0]);
        assert_eq!(output[3], 0xbeef);
        assert!(
            unsafe { write_command_text(PSTR(output.as_mut_ptr().cast()), 2, "中文", true) }
                .is_err()
        );
        assert_eq!(output[0], 0);
    }
}
