use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::path::{Path, PathBuf};
use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};

#[cfg(all(target_arch = "x86_64", target_env = "msvc"))]
extern "C" {
    fn zs_menu_query(browser: *mut core::ffi::c_void, service: *const windows::core::GUID,
        iid: *const windows::core::GUID, out: *mut *mut core::ffi::c_void) -> HRESULT;
    fn zs_menu_shutdown();
    fn zs_menu_diagnostics(output: *mut MenuDiagnostics);
}
use windows::{
    core::{implement, Error, IUnknown, IUnknownImpl, Interface, Ref, Result, HRESULT},
    Win32::{
        Foundation::{HWND, LPARAM, RECT, WPARAM, PROPERTYKEY},
        System::{
            Com::{
                CoCreateInstance, IServiceProvider, IServiceProvider_Impl, CLSCTX_INPROC_SERVER,
            },
            Ole::IObjectWithSite,
        },
        UI::{
            Shell::{
                ICommDlgBrowser, ICommDlgBrowser2, ICommDlgBrowser2_Impl, ICommDlgBrowser_Impl,
                IExplorerBrowser, IExplorerBrowserEvents, IExplorerBrowserEvents_Impl, IShellView,
                EBO_ALWAYSNAVIGATE, EBO_NOBORDER, EBO_SHOWFRAMES, EBO_NAVIGATEONCE, EBO_NOPERSISTVIEWSTATE, EBF_NODROPTARGET,
                IFolderView2, IResultsFolder, IColumnManager, FVM_DETAILS,
                IExplorerPaneVisibility, IExplorerPaneVisibility_Impl, EP_NavPane, EP_StatusBar,
                EP_Commands, EP_DetailsPane, EP_PreviewPane, EP_Ribbon,
                EPS_DEFAULT_ON, EPS_DEFAULT_OFF, EPS_FORCE, EPS_DONTCARE,
            },
            WindowsAndMessaging::PostMessageW,
        },
    },
};
use zsdesk_core::model::explorer::ShellLocation;

use super::{
    events::{WM_SHELL_FOLDER_INVOKED, WM_SHELL_NAVIGATED, WM_SHELL_NAVIGATION_FAILED, WM_SHELL_SELECTION_CHANGED},
    shell_item::{
        location_from_pidl, shell_item, selected_shell_item, is_folder, location_from_shell_item,
    },
};
#[path="view_background.rs"]
mod view_background;

const S_FALSE: HRESULT = HRESULT(1);
const E_NOINTERFACE: HRESULT = HRESULT(0x8000_4002u32 as i32);

/// The application captures its tab/navigation identity before any Shell query
/// can reenter its message loop. The returned closure owns no borrowed UI state.
pub type FolderOpenCallback=Box<dyn FnOnce(ShellLocation)>;
pub type FolderOpenCapture=fn(HWND,usize,HWND)->Option<FolderOpenCallback>;
thread_local!{static FOLDER_OPEN_CAPTURE:Cell<Option<FolderOpenCapture>>=const{Cell::new(None)};}
pub fn set_folder_open_capture(capture:Option<FolderOpenCapture>){FOLDER_OPEN_CAPTURE.with(|slot|slot.set(capture));}

/// Optional Shell panes shown next to the file list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Panes { pub preview: bool, pub details: bool }

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct MenuDiagnostics {
    pub requests: u64,
    pub presenter_creations: u64,
    pub selection_snapshots: u64,
    pub agile_commands: u64,
    pub agile_failures: u64,
    pub last_query_microseconds: u64,
    pub last_prepare_microseconds: u64,
    pub runtime_initializations: u64,
    pub last_failure: i32,
    pub reserved: u32,
    pub modern_items_hidden: u64,
    pub modern_extension_loads_skipped: u64,
    pub modern_updates_skipped: u64,
    pub modern_filter_failures: u64,
    /// Presenter initialization only; excludes menu tracking/user dwell time.
    pub last_presenter_create_microseconds: u64,
    pub last_public_bind_microseconds: u64,
    pub last_public_populate_microseconds: u64,
}
pub fn menu_diagnostics() -> MenuDiagnostics {
    let mut value = MenuDiagnostics::default();
    #[cfg(all(target_arch = "x86_64", target_env = "msvc"))]
    unsafe { zs_menu_diagnostics(&mut value); }
    (value.last_public_bind_microseconds,value.last_public_populate_microseconds)=super::menu::public_menu_timings();
    value
}

/// Cumulative costs for this embedded results view, excluding backend queries.
#[derive(Clone,Copy,Debug,Default,serde::Serialize)]
pub struct SearchMaterializationMetrics{
    pub batches:u64,
    pub attempted_paths:u64,
    pub added_paths:u64,
    pub parse_microseconds:u64,
    pub add_item_microseconds:u64,
    pub longest_batch_microseconds:u64,
    pub clear_calls:u64,
    pub clear_microseconds:u64,
}

pub struct ExplorerBrowserHost {
    panes: Rc<Cell<Panes>>,
    modern: Rc<Cell<bool>>,
    pub(crate) selection_revision: Rc<Cell<u64>>,
    pub(crate) selection_size: RefCell<super::commands::SelectionSizeCache>,
    browser: Rc<IExplorerBrowser>,
    events: IExplorerBrowserEvents,
    site: IServiceProvider,
    advise_cookie: u32,
    destroyed: Cell<bool>,
    owner: isize,
    source: usize,
    background: Rc<Cell<view_background::Colors>>,
    streamed_results_used: Cell<bool>,
    results_active: Rc<Cell<bool>>,
    results_location: Rc<RefCell<Option<ShellLocation>>>,
    search_materializing_until: Cell<Option<std::time::Instant>>,
    search_metrics:Cell<SearchMaterializationMetrics>,
    bounds:Cell<RECT>,
}

impl ExplorerBrowserHost {
    /// Names the application's per-folder Shell view preferences. Set this
    /// before the first navigation, and keep the name stable across launches.
    /// Transient hosts retain EBO_NOPERSISTVIEWSTATE and never write the bag.
    pub fn set_view_state_name(&self, name: &str) -> Result<()> {
        if name.is_empty() || name.contains('\0') {
            return Err(Error::from_hresult(HRESULT(0x80070057u32 as i32)));
        }
        let name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        unsafe { self.browser.SetPropertyBag(windows::core::PCWSTR(name.as_ptr())) }
    }

    /// Creates a host that does not persist Shell-managed folder view settings.
    ///
    /// # Safety
    /// The same STA and parent-window lifetime requirements as `create` apply.
    pub unsafe fn create_transient(parent: HWND, rect: RECT, modern: bool, panes: Panes) -> Result<Self> {
        unsafe { Self::create_transient_with_source(parent, rect, modern, panes, 0) }
    }

    pub unsafe fn create_transient_with_source(parent: HWND, rect: RECT, modern: bool, panes: Panes, source: usize) -> Result<Self> {
        let host = unsafe { Self::create_with_source(parent, rect, modern, panes, source)? };
        let options = unsafe { host.browser.GetOptions()? };
        unsafe { host.browser.SetOptions(options | EBO_NOPERSISTVIEWSTATE)?; }
        Ok(host)
    }

    /// Creates and initializes the native Shell browser child window.
    ///
    /// # Safety
    ///
    /// COM must be initialized as STA on the current UI thread. `parent` must
    /// be a valid window owned by that thread and outlive this host.
    pub unsafe fn create(parent: HWND, rect: RECT, modern: bool, panes: Panes) -> Result<Self> {
        unsafe { Self::create_with_source(parent, rect, modern, panes, 0) }
    }

    /// Tags every asynchronous callback so sibling panes cannot consume each
    /// other's navigation or selection changes. Zero preserves the old API.
    pub unsafe fn create_with_source(parent: HWND, rect: RECT, modern: bool, panes: Panes, source: usize) -> Result<Self> {
        let browser: IExplorerBrowser = unsafe {
            CoCreateInstance(
                &windows::Win32::UI::Shell::ExplorerBrowser,
                None,
                CLSCTX_INPROC_SERVER,
            )?
        };

        let mut options = super::menu::context_menu_options(); options.modern = modern;
        super::menu::set_context_menu_options(options);
        let browser = Rc::new(browser);
        let panes = Rc::new(Cell::new(panes));
        let modern = Rc::new(Cell::new(modern));
        let selection_revision = Rc::new(Cell::new(0u64));
        let results_active = Rc::new(Cell::new(false));
        let results_location = Rc::new(RefCell::new(None));
        let this_pc = Rc::new(Cell::new(false));
        let background=Rc::new(Cell::new(view_background::Colors{
            background:if crate::theme::system_dark_mode(){0x202020}else{0xffffff},high_contrast:crate::theme::high_contrast()}));
        let site: IServiceProvider = BrowserSite {
            hwnd: parent.0 as isize,
            source,
            browser: Rc::downgrade(&browser),
            modern: modern.clone(),
            panes: panes.clone(),
            selection_pending: Rc::new(Cell::new(false)),
            selection_revision: selection_revision.clone(),
            this_pc: this_pc.clone(),
        }
        .into();
        let object_with_site: IObjectWithSite = browser.cast()?;
        unsafe { object_with_site.SetSite(&site)? };

        unsafe {
            // The application supplies its own navigation, commands and status
            // row. A wrapper frame would add another native status row to each
            // pane. Retain frames only for an explicitly requested native pane.
            let frames=if panes.get().details||panes.get().preview {EBO_SHOWFRAMES}else{windows::Win32::UI::Shell::EXPLORER_BROWSER_OPTIONS(0)};
            browser.SetOptions(frames | EBO_ALWAYSNAVIGATE | EBO_NOBORDER)?;
            browser.Initialize(parent, &rect, None)?;
        }

        let events: IExplorerBrowserEvents = BrowserEvents {
            hwnd: parent.0 as isize,
            source,
            background:background.clone(),
            browser: Rc::downgrade(&browser),
            modern: modern.clone(),
            selection_revision: selection_revision.clone(),
            results_active: results_active.clone(),
            results_location: results_location.clone(),
            this_pc,
        }
        .into();
        let advise_cookie = unsafe { browser.Advise(&events)? };

        let host = Self {
            panes,
            modern,
            selection_revision,
            selection_size: RefCell::new(Default::default()),
            browser,
            events,
            site,
            advise_cookie,
            destroyed: Cell::new(false),
            owner: parent.0 as isize,
            source,
            background,
            streamed_results_used: Cell::new(false),
            results_active,
            results_location,
            search_materializing_until: Cell::new(None),
            search_metrics:Cell::new(SearchMaterializationMetrics::default()),
            bounds:Cell::new(rect),
        };
        Ok(host)
    }

    /// Navigates the hosted Shell view without invoking an external Explorer.
    ///
    /// # Safety
    ///
    /// Must be called on the STA/UI thread that created this host while its
    /// parent window and COM apartment are still alive.
    pub unsafe fn navigate(&self, location: &ShellLocation) -> Result<()> {
        self.search_materializing_until.set(None);
        if let ShellLocation::Search { query, .. } = location {
            if query.contains('\0') {
                return Err(Error::from_hresult(HRESULT(0x80070057u32 as i32)));
            }
            *self.results_location.borrow_mut() = Some(location.clone());
            return self.begin_search_results();
        }
        self.results_active.set(false);
        *self.results_location.borrow_mut() = None;
        // FillFromObject adds NAVIGATEONCE internally. Remove it before normal
        // navigation, otherwise BrowseToObject fails after a search results view.
        let options = unsafe { self.browser.GetOptions()? };
        if options.0 & EBO_NAVIGATEONCE.0 != 0 {
            unsafe { self.browser.SetOptions(windows::Win32::UI::Shell::EXPLORER_BROWSER_OPTIONS(options.0 & !EBO_NAVIGATEONCE.0))?; }
        }
        self.selection_revision.set(self.selection_revision.get().wrapping_add(1));
        let item = unsafe { shell_item(location)? };
        unsafe { self.browser.BrowseToObject(&item, 0) }
    }

    /// True after this browser has hosted a streamed ResultsFolder. Windows
    /// does not reliably allow a second FillFromObject after leaving that view;
    /// recreate the host when entering global results from a normal folder.
    pub fn has_streamed_results(&self) -> bool { self.streamed_results_used.get() }

    /// Creates an empty native results view, or clears its current query.
    /// A fresh Fill sends the real Shell navigation completion. RemoveAll does
    /// not, so a reused results view posts exactly one logical completion here.
    /// Prefer navigate(Search{scope:None,..}) to retain the query in events.
    pub fn begin_search_results(&self) -> Result<()> {
        self.search_materializing_until.set(None);
        self.selection_revision.set(self.selection_revision.get().wrapping_add(1));
        if self.streamed_results_used.get() {
            if !self.results_active.get() {
                return Err(Error::from_hresult(HRESULT(0x8000FFFFu32 as i32)));
            }
            let results = self.results_folder()?;
            self.remove_all_search_results(&results)?;
            if let Some(location) = self.results_location.borrow().clone() {
                post_location(self.owner, self.source, WM_SHELL_NAVIGATED, location);
            }
            return Ok(());
        }
        self.streamed_results_used.set(true);
        self.results_active.set(true);
        let result = unsafe { self.browser.FillFromObject(None::<&IUnknown>, EBF_NODROPTARGET) };
        if result.is_err() { self.results_active.set(false); }
        result
    }

    fn results_folder(&self) -> Result<IResultsFolder> {
        let view = unsafe { self.current_view()? };
        let folder: IFolderView2 = view.cast()?;
        unsafe { folder.GetFolder() }
    }

    /// Appends one UI-thread batch of at most 64 paths. Disappeared or
    /// inaccessible entries are skipped. The caller consumes the entire batch
    /// and must reject stale query generations before calling this method.
    pub fn append_search_paths(&self, paths: &[PathBuf]) -> Result<usize> {
        if paths.len() > 64 { return Err(Error::from_hresult(HRESULT(0x80070057u32 as i32))); }
        if !self.results_active.get() { return Err(Error::from_hresult(HRESULT(0x8000FFFFu32 as i32))); }
        let results = self.results_folder().map_err(|_| Error::from_hresult(HRESULT(0x8000000Au32 as i32)))?;
        let started=std::time::Instant::now();
        let mut metrics=self.search_metrics.get();
        let mut added = 0;
        for path in paths {
            if !path.is_absolute() { continue; }
            metrics.attempted_paths+=1;
            let parse_started=std::time::Instant::now();
            let parsed=unsafe { shell_item(&ShellLocation::Path(path.clone())) };
            metrics.parse_microseconds+=parse_started.elapsed().as_micros()as u64;
            let Ok(item)=parsed else { continue };
            let add_started=std::time::Instant::now();
            if unsafe { results.AddItem(&item) }.is_ok() { added += 1; }
            metrics.add_item_microseconds+=add_started.elapsed().as_micros()as u64;
        }
        metrics.batches+=1;metrics.added_paths+=added as u64;
        metrics.longest_batch_microseconds=metrics.longest_batch_microseconds.max(started.elapsed().as_micros()as u64);
        self.search_metrics.set(metrics);
        if added > 0 {
            self.selection_revision.set(self.selection_revision.get().wrapping_add(1));
            self.search_materializing_until.set(Some(std::time::Instant::now()+std::time::Duration::from_secs(2)));
        }
        Ok(added)
    }

    /// Replaces streamed data in the existing results view. Unlike a navigation,
    /// this deliberately emits no logical or Shell navigation completion.
    pub fn clear_search_paths(&self) -> Result<()> {
        self.search_materializing_until.set(None);
        if !self.results_active.get() { return Err(Error::from_hresult(HRESULT(0x8000FFFFu32 as i32))); }
        let results = self.results_folder().map_err(|_| Error::from_hresult(HRESULT(0x8000000Au32 as i32)))?;
        self.remove_all_search_results(&results)?;
        self.selection_revision.set(self.selection_revision.get().wrapping_add(1));
        Ok(())
    }

    fn remove_all_search_results(&self,results:&IResultsFolder)->Result<()>{
        let started=std::time::Instant::now();
        let result=unsafe{results.RemoveAll()};
        let mut metrics=self.search_metrics.get();metrics.clear_calls+=1;
        metrics.clear_microseconds+=started.elapsed().as_micros()as u64;self.search_metrics.set(metrics);
        result
    }

    pub fn search_materialization_metrics(&self)->SearchMaterializationMetrics{self.search_metrics.get()}

    pub fn search_selection_revision(&self) -> u64 { self.selection_revision.get() }

    /// Normalize only Win32 spelling of the root and separators. Filenames stay
    /// case-sensitive so distinct names in an NTFS case-sensitive directory are
    /// never treated as the same restoration candidate.
    pub fn search_path_key(path: &Path) -> Vec<u16> {
        let mut units: Vec<u16> = path.as_os_str().encode_wide().map(|n| if n == 47 { 92 } else { n }).collect();
        if units.len() >= 7 && units[..4] == [92,92,63,92] && units[5] == 58 { units.drain(..4); }
        if units.len() >= 2 && units[1] == 58 && (97..=122).contains(&units[0]) { units[0] -= 32; }
        while units.len() > 3 && units.last() == Some(&92) { units.pop(); }
        units
    }

    /// Reads a stable, bounded selected-item array. A concurrent selection change
    /// declines the snapshot instead of preserving an arbitrary partial choice.
    pub fn search_selection_snapshot(&self) -> Result<(Vec<PathBuf>, u64)> {
        use windows::Win32::UI::Shell::{IShellItem, SVGIO_SELECTION};
        if self.results_active.get() {
            let (_,selected,revision)=self.search_items_snapshot()?;
            return Ok((selected,revision));
        }
        let revision = self.selection_revision.get();
        let view: IFolderView2 = unsafe { self.current_view()? }.cast()?;
        let count = match unsafe { view.ItemCount(SVGIO_SELECTION) } {
            Ok(count) => count.max(0) as u32,
            Err(error) if missing_selection(&error) => 0,
            Err(error) => return Err(error),
        };
        if count > 2000 { return Err(Error::from_hresult(HRESULT(0x8007000Eu32 as i32))); }
        // ResultsFolder can report ERROR_NOT_FOUND for an empty selection and
        // can briefly retain an old selection count after RemoveAll. Confirm
        // empty/unstable arrays against actual view-item PIDL state instead of
        // treating every NOT_FOUND as either failure or an assumed empty set.
        if count == 0 { return self.search_selection_from_items(&view, revision); }
        let mut paths = Vec::with_capacity(count as usize);
        let mut bytes = 0usize;
        {
            let items = match unsafe { view.GetSelection(false) } {
                Ok(items) => items,
                Err(error) if missing_selection(&error) => return self.search_selection_from_items(&view, revision),
                Err(error) => return Err(error),
            };
            if unsafe { items.GetCount()? } != count { return Err(search_pending()); }
            for index in 0..count {
                if self.selection_revision.get() != revision { return Err(search_pending()); }
                let item: IShellItem = unsafe { items.GetItemAt(index)? };
                if let Some(path) = search_item_path(&item)? {
                    bytes = bytes.saturating_add(path.capacity()).saturating_add(64);
                    if bytes > 2 * 1024 * 1024 { return Err(Error::from_hresult(HRESULT(0x8007000Eu32 as i32))); }
                    paths.push(path);
                }
            }
        }
        let after = match unsafe { view.ItemCount(SVGIO_SELECTION) } {
            Ok(count) => count.max(0) as u32,
            Err(error) if missing_selection(&error) => 0,
            Err(error) => return Err(error),
        };
        if self.selection_revision.get() != revision || after != count {
            return Err(search_pending());
        }
        Ok((paths,revision))
    }

    fn search_selection_from_items(&self, view:&IFolderView2, revision:u64)->Result<(Vec<PathBuf>,u64)>{
        use windows::Win32::UI::Shell::{IShellItem,SVGIO_ALLVIEW,SVSI_SELECT};
        let count=match unsafe{view.ItemCount(SVGIO_ALLVIEW)}{
            Ok(count)=>count.max(0),
            Err(error) if missing_selection(&error)=>{
                match unsafe{view.GetItem::<IShellItem>(0)}{
                    Ok(_)=>return Err(search_pending()),
                    Err(item_error) if missing_selection(&item_error) || item_error.code().0 as u32==0x80070057=>0,
                    Err(item_error)=>return Err(item_error),
                }
            },
            Err(error)=>return Err(error),
        };
        if count>2000{return Err(Error::from_hresult(HRESULT(0x8007000Eu32 as i32)));}
        let mut paths=Vec::new();let mut bytes=0usize;
        for index in 0..count{
            if self.selection_revision.get()!=revision{return Err(search_pending());}
            let pidl=SearchRelativePidl(unsafe{view.Item(index)}.map_err(materializing_error)?);
            if pidl.0.is_null(){return Err(search_pending());}
            let state=unsafe{view.GetSelectionState(pidl.0)}.map_err(materializing_error)?;
            if state&SVSI_SELECT.0 as u32!=0{
                let item:IShellItem=unsafe{view.GetItem(index)?};
                if let Some(path)=search_item_path(&item)?{
                    bytes+=path.capacity()+64;if bytes>2*1024*1024{return Err(Error::from_hresult(HRESULT(0x8007000Eu32 as i32)));}
                    paths.push(path);
                }
            }
        }
        if self.selection_revision.get()!=revision{return Err(search_pending());}
        Ok((paths,revision))
    }

    /// Snapshot current materialized rows and their actual PIDL selection state.
    /// Used while RemoveAll notifications are still draining; a cached selected
    /// IShellItemArray alone does not prove that those rows still exist.
    pub fn search_items_snapshot(&self)->Result<(Vec<PathBuf>,Vec<PathBuf>,u64)>{
        use windows::Win32::UI::Shell::{IShellItem,SVSI_SELECT};
        let revision=self.selection_revision.get();
        let shell=unsafe{self.current_view()}.map_err(|error|restore_context("snapshot current view",error))?;
        let view:IFolderView2=shell.cast().map_err(|error|restore_context("snapshot folder view",error))?;
        let count=materialized_item_count(&view).map_err(|error|restore_context("snapshot live row count",error))?;
        if count>2000{return Err(Error::from_hresult(HRESULT(0x8007000Eu32 as i32)));}
        let mut paths=Vec::new();let mut selected=Vec::new();let mut bytes=0usize;
        for index in 0..count{
            if self.selection_revision.get()!=revision{return Err(search_pending());}
            let item:IShellItem=unsafe{view.GetItem(index)}.map_err(|error|restore_context("snapshot live GetItem",materializing_error(error)))?;
            let Some(path)=search_item_path(&item).map_err(|error|restore_context("snapshot live filesystem path",materializing_error(error)))? else{return Err(search_pending());};
            let pidl=SearchRelativePidl(unsafe{view.Item(index)}.map_err(|error|restore_context("snapshot live relative PIDL",materializing_error(error)))?);
            if pidl.0.is_null(){return Err(search_pending());}
            let state=unsafe{view.GetSelectionState(pidl.0)}.map_err(|error|restore_context("snapshot live PIDL selection state",
                materializing_selection_error(error,self.search_materializing_until.get(),std::time::Instant::now())))?;
            bytes+=path.capacity()+64;if bytes>2*1024*1024{return Err(Error::from_hresult(HRESULT(0x8007000Eu32 as i32)));}
            if state&SVSI_SELECT.0 as u32!=0{selected.push(path.clone());}paths.push(path);
        }
        if self.selection_revision.get()!=revision || materialized_item_count(&view).map_err(|error|restore_context("snapshot final live row count",error))?!=count
            || unsafe{self.current_view()}.map_err(|error|restore_context("snapshot final current view",error))?.cast::<IUnknown>()?!=shell.cast::<IUnknown>()? {return Err(search_pending());}
        Ok((paths,selected,revision))
    }

    /// Select only existing results whose actual relative PIDL resolves to a
    /// requested path. No parsing-name guess, focus change or navigation occurs.
    /// `allowed` is checked around COM calls, including nested message pumps.
    pub fn restore_search_paths(&self, paths: &[PathBuf], mut revision: u64, allowed: impl Fn() -> bool)
        -> Result<(Vec<PathBuf>, u64)> {
        use windows::Win32::UI::Shell::{IShellFolder, IShellItem, SHCreateItemWithParent,
            SVGIO_ALLVIEW, SVSI_SELECT, SVSI_NOTAKEFOCUS};
        if paths.len() > 2000 { return Err(Error::from_hresult(HRESULT(0x80070057u32 as i32))); }
        if !self.results_active.get() || !allowed() || self.selection_revision.get() != revision { return Err(search_cancelled()); }
        let shell = unsafe { self.current_view()? };
        let view: IFolderView2 = shell.cast()?;
        let folder: IShellFolder = unsafe { view.GetFolder() }.map_err(|error|restore_context("results parent",error))?;
        let count = unsafe { view.ItemCount(SVGIO_ALLVIEW)? }.max(0);
        if count > 4096 { return Err(Error::from_hresult(HRESULT(0x8007000Eu32 as i32))); }
        let mut wanted: std::collections::HashSet<_> = paths.iter().map(|p| Self::search_path_key(p)).collect();
        let mut restored = Vec::new();
        for index in 0..count {
            if wanted.is_empty() { break; }
            if !allowed() || self.selection_revision.get() != revision { return Err(search_cancelled()); }
            let Ok(item) = (unsafe { view.GetItem::<IShellItem>(index) }) else { continue };
            let Some(path) = search_item_path(&item)? else { continue };
            let key = Self::search_path_key(&path);
            if !wanted.contains(&key) { continue; }
            let pidl = SearchRelativePidl(unsafe { view.Item(index) }.map_err(|error|restore_context("actual relative PIDL",error))?);
            if pidl.0.is_null() { continue; }
            // The view can reorder while an item object is obtained. Bind this
            // actual PIDL back through its owning results folder before selecting.
            let bound: IShellItem = unsafe { SHCreateItemWithParent(None, &folder, pidl.0) }.map_err(|error|restore_context("relative PIDL binding",error))?;
            let Some(bound_path) = search_item_path(&bound).map_err(|error|restore_context("bound filesystem path",error))? else { continue };
            if Self::search_path_key(&bound_path) != key { continue; }
            if !allowed() || self.selection_revision.get() != revision { return Err(search_cancelled()); }
            unsafe { shell.SelectItem(pidl.0, (SVSI_SELECT.0 | SVSI_NOTAKEFOCUS.0) as u32) }.map_err(|error|restore_context("select relative PIDL",error))?;
            revision = self.selection_revision.get();
            if !allowed() { return Err(search_cancelled()); }
            let selected = unsafe { view.GetSelectionState(pidl.0) }.map_err(|error|restore_context("selected relative PIDL state",error))? & SVSI_SELECT.0 as u32 != 0;
            if !allowed() || self.selection_revision.get() != revision { return Err(search_cancelled()); }
            if selected { wanted.remove(&key); restored.push(bound_path); }
        }
        Ok((restored,revision))
    }

    /// Resizes the hosted Shell view.
    ///
    /// # Safety
    ///
    /// Must be called on the STA/UI thread that created this host.
    pub unsafe fn resize(&self, rect: RECT) {
        // IExplorerBrowser may invalidate its child view even for the same
        // rectangle. Preserve native scrolling when chrome layout is unchanged.
        if self.bounds.get()!=rect && unsafe { self.browser.SetRect(None, rect) }.is_ok() {
            self.bounds.set(rect);
        }
    }

    /// Disconnects callbacks and destroys the hosted Shell view.
    ///
    /// # Safety
    ///
    /// Must be called exactly once on the STA/UI thread that created this host,
    /// before its parent window or COM apartment is destroyed.
    pub unsafe fn destroy(&self) {
        if self.destroyed.replace(true) { return; }
        // Block reentrant service queries while this particular browser is
        // being torn down; another/new browser may still enable its own mode.
        self.modern.set(false);
        #[cfg(all(target_arch = "x86_64", target_env = "msvc"))]
        unsafe { zs_menu_shutdown(); }
        let _keep_callbacks_alive = (&self.events, &self.site);
        let _ = unsafe { self.browser.Unadvise(self.advise_cookie) };
        if let Ok(object_with_site) = self.browser.cast::<IObjectWithSite>() {
            let _ = unsafe { object_with_site.SetSite(None::<&IUnknown>) };
        }
        let _ = unsafe { self.browser.Destroy() };
    }
}

fn search_pending() -> Error { Error::from_hresult(HRESULT(0x8000000Au32 as i32)) }
fn search_cancelled() -> Error { Error::from_hresult(HRESULT(0x800704C7u32 as i32)) }
fn missing_selection(error:&Error)->bool{error.code().0 as u32==0x80070490}
fn materializing_error(error:Error)->Error{if missing_selection(&error){search_pending()}else{error}}
fn materializing_selection_error(error:Error,deadline:Option<std::time::Instant>,now:std::time::Instant)->Error{
    // AddItem can expose a row/PIDL before its selection state exists. Only
    // that exact argument error during our own bounded append interval is
    // pending; it does not establish that the row is unselected.
    if error.code().0 as u32==0x80070057 && deadline.is_some_and(|limit|now<limit){search_pending()}
    else{materializing_error(error)}
}
fn materialized_item_count(view:&IFolderView2)->Result<i32>{
    use windows::Win32::UI::Shell::{IShellItem,SVGIO_ALLVIEW};
    match unsafe{view.ItemCount(SVGIO_ALLVIEW)}{
        Ok(count)=>Ok(count.max(0)),
        Err(error) if missing_selection(&error)=>match unsafe{view.GetItem::<IShellItem>(0)}{
            Ok(_)=>Err(search_pending()),
            Err(item_error) if missing_selection(&item_error) || item_error.code().0 as u32==0x80070057=>Ok(0),
            Err(item_error)=>Err(item_error),
        },
        Err(error)=>Err(error),
    }
}
fn restore_context(stage:&str,error:Error)->Error{Error::new(error.code(),format!("{stage}: {error}"))}
struct SearchRelativePidl(*mut windows::Win32::UI::Shell::Common::ITEMIDLIST);
impl Drop for SearchRelativePidl {
    fn drop(&mut self) { if !self.0.is_null() { unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(self.0.cast())); } } }
}
fn search_item_path(item: &windows::Win32::UI::Shell::IShellItem) -> Result<Option<PathBuf>> {
    let text = unsafe { item.GetDisplayName(windows::Win32::UI::Shell::SIGDN_FILESYSPATH)? };
    if text.is_null() { return Ok(None); }
    let path = unsafe {
        let units = text.as_wide();
        let result = (units.len() <= 32767).then(|| PathBuf::from(OsString::from_wide(units)));
        windows::Win32::System::Com::CoTaskMemFree(Some(text.0.cast()));
        result
    };
    Ok(path.filter(|p| p.is_absolute()))
}

#[cfg(test)]
mod search_path_tests {
    use super::*;
    #[test]
    fn roots_are_normalized_without_collapsing_case_sensitive_filenames() {
        assert_eq!(ExplorerBrowserHost::search_path_key(Path::new(r"\\?\c:\资料\甲.txt")),
            ExplorerBrowserHost::search_path_key(Path::new("C:/资料/甲.txt")));
        assert_ne!(ExplorerBrowserHost::search_path_key(Path::new(r"C:\case\a.txt")),
            ExplorerBrowserHost::search_path_key(Path::new(r"C:\case\A.txt")));
        assert_ne!(ExplorerBrowserHost::search_path_key(Path::new(r"C:\甲\same.txt")),
            ExplorerBrowserHost::search_path_key(Path::new(r"C:\乙\same.txt")));
    }
    #[test]
    fn append_selection_pending_is_bounded_and_hresult_specific(){
        let now=std::time::Instant::now();let deadline=now+std::time::Duration::from_secs(2);
        let argument=||Error::from_hresult(HRESULT(0x80070057u32 as i32));
        assert_eq!(materializing_selection_error(argument(),Some(deadline),now).code(),search_pending().code());
        assert_eq!(materializing_selection_error(argument(),None,now).code(),argument().code());
        assert_eq!(materializing_selection_error(argument(),Some(deadline),deadline).code(),argument().code());
        let denied=Error::from_hresult(HRESULT(0x80070005u32 as i32));
        assert_eq!(materializing_selection_error(denied.clone(),Some(deadline),now).code(),denied.code());
    }
}

impl ExplorerBrowserHost {
    pub fn panes(&self) -> Panes { self.panes.get() }
    /// Sets only the erasure surface; the native Shell remains responsible for
    /// item colors, selection backgrounds, accessibility and control painting.
    pub fn set_content_background(&self,_dark:bool,colorref:u32){
        self.background.set(view_background::Colors{background:colorref,high_contrast:crate::theme::high_contrast()});
        if let Ok(view)=unsafe{self.current_view()}{view_background::install(&view,self.background.clone());}
    }
    /// Changes presentation without rebuilding the file view or losing its
    /// selection. The native XAML runtime is retained once per UI thread.
    pub fn set_modern_menu(&self, modern: bool) {
        if self.modern.replace(modern) != modern {
            #[cfg(all(target_arch = "x86_64", target_env = "msvc"))]
            unsafe { zs_menu_shutdown(); }
        }
        let mut options = super::menu::context_menu_options(); options.modern = modern;
        super::menu::set_context_menu_options(options);
    }
    pub fn set_context_menu_options(&self, options: super::menu::ContextMenuOptions) {
        self.set_modern_menu(options.modern);
        super::menu::set_context_menu_options(options);
    }
    /// Changes optional panes; they apply on the next navigation (callers refresh).
    pub fn set_panes(&self, panes: Panes) { self.panes.set(panes); }
}

impl Drop for ExplorerBrowserHost {
    fn drop(&mut self) { unsafe { self.destroy(); } }
}

impl ExplorerBrowserHost {
    /// Clones the interface without entering Shell. Callers may release their
    /// UI-state borrow before querying a view that can dispatch COM callbacks.
    pub fn browser_handle(&self) -> IExplorerBrowser { self.browser.as_ref().clone() }

    pub unsafe fn current_view(&self) -> Result<IShellView> {
        unsafe { self.browser.GetCurrentView() }
    }
    pub unsafe fn translate(&self, message: &windows::Win32::UI::WindowsAndMessaging::MSG) -> bool {
        unsafe { self.current_view().map(|v| v.TranslateAccelerator(message) == HRESULT(0)).unwrap_or(false) }
    }
    pub unsafe fn focus(&self) {
        if let Ok(view) = unsafe { self.current_view() } {
            let _ = unsafe { view.UIActivate(windows::Win32::UI::Shell::SVUIA_ACTIVATE_FOCUS.0 as u32) };
        }
    }
}

static OPEN_ARCHIVES_EXTERNALLY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Process-wide: every file window shares the same preference file.
pub fn set_open_archives_externally(value: bool) { OPEN_ARCHIVES_EXTERNALLY.store(value, std::sync::atomic::Ordering::Relaxed); }
/// A browsable archive: a folder that is also a stream (zip, cab).
pub(crate) unsafe fn is_archive(item: &windows::Win32::UI::Shell::IShellItem) -> bool {
    use windows::Win32::System::SystemServices::{SFGAO_FOLDER, SFGAO_STREAM};
    unsafe { item.GetAttributes(SFGAO_FOLDER | SFGAO_STREAM) }
        .is_ok_and(|attributes| attributes.0 & (SFGAO_FOLDER.0 | SFGAO_STREAM.0) == SFGAO_FOLDER.0 | SFGAO_STREAM.0)
}

#[implement(IServiceProvider, ICommDlgBrowser2, IExplorerPaneVisibility)]
struct BrowserSite {
    hwnd: isize,
    source: usize,
    browser: Weak<IExplorerBrowser>,
    modern: Rc<Cell<bool>>,
    panes: Rc<Cell<Panes>>,
    selection_pending: Rc<Cell<bool>>,
    selection_revision: Rc<Cell<u64>>,
    /// The view being created shows This PC; only then are items filtered.
    this_pc: Rc<Cell<bool>>,
}

#[allow(non_snake_case)]
impl IServiceProvider_Impl for BrowserSite_Impl {
    fn QueryService(
        &self,
        guidservice: *const windows::core::GUID,
        riid: *const windows::core::GUID,
        ppvobject: *mut *mut core::ffi::c_void,
    ) -> Result<()> {
        if guidservice.is_null() || riid.is_null() || ppvobject.is_null() {
            return Err(Error::from_hresult(E_NOINTERFACE));
        }
        unsafe { *ppvobject = std::ptr::null_mut(); }
        if unsafe { *guidservice } != ICommDlgBrowser::IID && unsafe { *guidservice } != IExplorerPaneVisibility::IID {
            #[cfg(all(target_arch = "x86_64", target_env = "msvc"))]
            if let Some(browser) = self.browser.upgrade().filter(|_|self.modern.get() && !super::menu::view_menu_is_delegated()) {
                return unsafe { zs_menu_query(browser.as_raw(), guidservice, riid, ppvobject) }.ok();
            }
            return Err(Error::from_hresult(E_NOINTERFACE));
        }
        let result = unsafe { self.QueryInterface(riid, ppvobject) };
        result.ok()
    }
}

#[allow(non_snake_case)]
impl IExplorerPaneVisibility_Impl for BrowserSite_Impl {
    fn GetPaneState(&self, ep: *const windows::core::GUID) -> Result<u32> {
        if ep.is_null() { return Ok(EPS_DONTCARE.0 as u32); }
        let pane = unsafe { *ep };
        let panes = self.panes.get();
        let on = (EPS_DEFAULT_ON.0 | EPS_FORCE.0) as u32;
        let off = (EPS_DEFAULT_OFF.0 | EPS_FORCE.0) as u32;
        // ZSDesk draws its own navigation tree, command bar and status bar.
        if pane == EP_PreviewPane { Ok(if panes.preview { on } else { off }) }
        else if pane == EP_DetailsPane { Ok(if panes.details { on } else { off }) }
        else if [EP_NavPane, EP_StatusBar, EP_Commands, EP_Ribbon].contains(&pane) { Ok(off) }
        else { Ok(EPS_DONTCARE.0 as u32) }
    }
}

#[allow(non_snake_case)]
impl ICommDlgBrowser_Impl for BrowserSite_Impl {
    fn OnDefaultCommand(&self, view:Ref<IShellView>)->Result<()> {
        let Some(view)=view.as_ref() else{return Err(Error::from_hresult(E_NOINTERFACE));};
        // A zip/cab is a folder that is also a file. With the user's choice, the
        // Shell runs its default program instead of browsing it here.
        if OPEN_ARCHIVES_EXTERNALLY.load(std::sync::atomic::Ordering::Relaxed)
            && unsafe{selected_shell_item(view)}.is_ok_and(|item|unsafe{is_archive(&item)}) {return Err(Error::from_hresult(S_FALSE));}
        let capture=FOLDER_OPEN_CAPTURE.with(Cell::get);
        let callback=if let Some(capture)=capture {
            let window=unsafe{view.GetWindow()?};
            match capture(HWND(self.hwnd as *mut _),self.source,window){Some(callback)=>Some(callback),None=>return Ok(())}
        }else{None};
        let Ok(item)=(unsafe {selected_shell_item(view)}) else{return Err(Error::from_hresult(S_FALSE));};
        if !unsafe {is_folder(&item)} {return Err(Error::from_hresult(S_FALSE));}
        let Some(location)=(unsafe {location_from_shell_item(&item)}) else{return Err(Error::from_hresult(S_FALSE));};
        if let Some(callback)=callback{callback(location);}else{post_location(self.hwnd,self.source,WM_SHELL_FOLDER_INVOKED,location);}
        Ok(())
    }
    fn OnStateChange(&self,_:Ref<IShellView>,change:u32)->Result<()>{
        if [windows::Win32::UI::Shell::CDBOSC_SELCHANGE,
            windows::Win32::UI::Shell::CDBOSC_RENAME,
            windows::Win32::UI::Shell::CDBOSC_STATECHANGE].contains(&change) {
            self.selection_revision.set(self.selection_revision.get().wrapping_add(1));
        }
        // CDBOSC_SELCHANGE: coalesce bursts (select-all fires once per item).
        if change==windows::Win32::UI::Shell::CDBOSC_SELCHANGE && !self.selection_pending.replace(true) {
            let pending=self.selection_pending.clone();
            let pointer = Rc::into_raw(pending);
            if unsafe { PostMessageW(Some(HWND(self.hwnd as *mut _)), WM_SHELL_SELECTION_CHANGED, WPARAM(self.source), LPARAM(pointer as isize)) }.is_err() {
                unsafe { drop(Rc::from_raw(pointer)); }
                self.selection_pending.set(false);
            }
        }
        Ok(())
    }
    fn IncludeObject(&self,view:Ref<IShellView>,child:*const windows::Win32::UI::Shell::Common::ITEMIDLIST)->Result<()>{
        // This PC shows drives, devices and network locations, like Windows 11;
        // the user's folders are in Quick access.
        if !self.this_pc.get()||child.is_null(){return Ok(());}
        use windows::Win32::UI::Shell::{IFolderView,IShellFolder,IShellItem,SHCreateItemWithParent};
        let hidden=view.as_ref().and_then(|view|view.cast::<IFolderView>().ok())
            .and_then(|folder|unsafe{folder.GetFolder::<IShellFolder>()}.ok())
            .and_then(|folder|unsafe{SHCreateItemWithParent::<_,IShellItem>(None,&folder,child)}.ok())
            .is_some_and(|item|super::shell_item::is_this_pc_known_folder(&item));
        if hidden{Err(Error::from_hresult(S_FALSE))}else{Ok(())}
    }
}
#[allow(non_snake_case)]
impl ICommDlgBrowser2_Impl for BrowserSite_Impl {
    fn Notify(&self,_:Ref<IShellView>,_:u32)->Result<()>{Ok(())}
    fn GetDefaultMenuText(&self,_:Ref<IShellView>,_:windows::core::PWSTR,_:i32)->Result<()>{Err(Error::from_hresult(S_FALSE))}
    fn GetViewFlags(&self)->Result<u32>{
        // Keep folder navigation interception without file-dialog "Select".
        // Per-item filtering is requested only for the small This PC view.
        let include=if self.this_pc.get(){0}else{windows::Win32::UI::Shell::CDB2GVF_NOINCLUDEITEM};
        Ok(include | windows::Win32::UI::Shell::CDB2GVF_NOSELECTVERB)
    }
}
#[implement(IExplorerBrowserEvents)]
struct BrowserEvents {
    hwnd: isize,
    source: usize,
    background: Rc<Cell<view_background::Colors>>,
    browser: Weak<IExplorerBrowser>,
    modern: Rc<Cell<bool>>,
    selection_revision: Rc<Cell<u64>>,
    results_active: Rc<Cell<bool>>,
    results_location: Rc<RefCell<Option<ShellLocation>>>,
    this_pc: Rc<Cell<bool>>,
}

#[allow(non_snake_case)]
impl IExplorerBrowserEvents_Impl for BrowserEvents_Impl {
    fn OnNavigationPending(
        &self,
        pidl_folder: *const windows::Win32::UI::Shell::Common::ITEMIDLIST,
    ) -> Result<()> {
        // Decided before the new view is created and asks for its view flags.
        let this_pc=!pidl_folder.is_null()&&unsafe{windows::Win32::UI::Shell::SHCreateItemFromIDList::<windows::Win32::UI::Shell::IShellItem>(pidl_folder)}
            .is_ok_and(|folder|super::shell_item::is_this_pc_item(&folder));
        self.this_pc.set(this_pc);
        Ok(())
    }

    fn OnViewCreated(&self, shell_view: Ref<IShellView>) -> Result<()> {
        self.selection_revision.set(self.selection_revision.get().wrapping_add(1));
        if let Some(view) = shell_view.as_ref() {
            view_background::install(view,self.background.clone());
            unsafe { super::menu::attach_view_menu(view, self.modern.clone()); }
            if self.results_active.get() { configure_results_columns(view); }
            if let Ok(folder)=view.cast::<IFolderView2>(){let _=super::view_state::sync_column_header(&folder);}
        }
        Ok(())
    }

    fn OnNavigationComplete(
        &self,
        pidl_folder: *const windows::Win32::UI::Shell::Common::ITEMIDLIST,
    ) -> Result<()> {
        self.selection_revision.set(self.selection_revision.get().wrapping_add(1));
        if let Some(browser) = self.browser.upgrade() {
            if let Ok(view) = unsafe { browser.GetCurrentView::<IShellView>() } {
                if self.results_active.get(){configure_results_columns(&view);}
                if let Ok(folder)=view.cast::<IFolderView2>(){let _=super::view_state::sync_column_header(&folder);}
            }
        }
        let logical = if self.results_active.get() { self.results_location.borrow().clone() } else { None };
        if let Some(location) = logical.or_else(|| unsafe { location_from_pidl(pidl_folder) }) {
            post_location(self.hwnd, self.source, WM_SHELL_NAVIGATED, location);
        }
        Ok(())
    }

    fn OnNavigationFailed(
        &self,
        _pidl_folder: *const windows::Win32::UI::Shell::Common::ITEMIDLIST,
    ) -> Result<()> {
        self.results_active.set(false);
        let _ = unsafe { PostMessageW(Some(HWND(self.hwnd as *mut _)), WM_SHELL_NAVIGATION_FAILED, WPARAM(self.source), LPARAM(0)) };
        Ok(())
    }
}

fn configure_results_columns(view: &IShellView) {
    // Presentation capability varies by Windows release; none of these
    // preferences may prevent the results folder from being usable.
    let Ok(folder) = view.cast::<IFolderView2>() else { return };
    if unsafe { folder.GetFolder::<IResultsFolder>() }.is_err() { return; }
    let _ = unsafe { folder.SetViewModeAndIconSize(FVM_DETAILS, 16) };
    let _=super::view_state::sync_column_header(&folder);
    let Ok(columns) = view.cast::<IColumnManager>() else { return };
    let mut keys = Vec::new();
    for name in ["System.ItemNameDisplay", "System.ItemFolderPathDisplay", "System.DateModified", "System.Size"] {
        let text: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let mut key = PROPERTYKEY::default();
        if unsafe { windows::Win32::UI::Shell::PropertiesSystem::PSGetPropertyKeyFromName(windows::core::PCWSTR(text.as_ptr()), &mut key) }.is_err() { return; }
        keys.push(key);
    }
    let _ = unsafe { columns.SetColumns(&keys) };
}

fn post_location(hwnd: isize, source: usize, message: u32, location: ShellLocation) {
    let pointer = Box::into_raw(Box::new(location));
    if unsafe { PostMessageW(Some(HWND(hwnd as *mut _)), message, WPARAM(source), LPARAM(pointer as isize)) }.is_err() {
        unsafe { drop(Box::from_raw(pointer)); }
    }
}
