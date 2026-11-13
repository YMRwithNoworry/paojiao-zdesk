//! Navigation tree (`INameSpaceTreeControl`) with system-aware navigation density.
//!
//! The system Quick access namespace provides the user's pinned/frequent
//! folders. Known folders remain available on systems without that namespace.
//! Root expansion follows the initial location. Windows 11 navigation density
//! follows compact mode, per-monitor DPI and the control's actual system font;
//! older Shell versions retain their native item height and indentation.

use std::{cell::{Cell, RefCell}, collections::HashMap, rc::{Rc,Weak}, time::{Duration,Instant}};

use windows::{
    core::{implement, Error, Interface, Ref, Result, BOOL, HRESULT, PCWSTR, PWSTR},
    Win32::{
        Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM},
        Graphics::Gdi::{GetObjectW, HGDIOBJ, LOGFONTW},
        Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES,
        System::{
            Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER},
            Ole::IOleWindow,
            Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD},
            SystemServices::SFGAO_STREAM,
        },
        UI::{
            Controls::{TVGN_CARET, TVM_SELECTITEM, TVM_SETBKCOLOR, TVM_SETTEXTCOLOR, TVM_SETEXTENDEDSTYLE,
                TVS_EX_DOUBLEBUFFER, TVM_EXPAND, TVE_COLLAPSE, TVM_SETITEMHEIGHT, TVM_GETITEMHEIGHT},
            HiDpi::GetDpiForWindow,
            Input::KeyboardAndMouse::{GetFocus, SetFocus, IsWindowEnabled},
            Shell::{
                IContextMenu, INameSpaceTreeControl, INameSpaceTreeControl2, INameSpaceTreeControlEvents,
                INameSpaceTreeControlEvents_Impl, IShellItem, IShellItemArray, IShellItemFilter, IShellItemFilter_Impl,
                NamespaceTreeControl, SHGetFileInfoW, SHGetIDListFromObject, SHGetKnownFolderItem, NSTCECT_BUTTON,
                DefSubclassProc, SetWindowSubclass, RemoveWindowSubclass,
                NSTCECT_LBUTTON, NSTCECT_MBUTTON, NSTCEHT_ONITEM, NSTCEHT_ONITEMBUTTON, NSTCEHT_ONITEMINDENT,
                NSTCEHT_ONITEMRIGHT, NSTCIS_EXPANDED, NSTCIS_SELECTED, NSTCIS_SELECTEDNOEXPAND, NSTCRS_EXPANDED, NSTCRS_VISIBLE,
                SICHINT_CANONICAL, SICHINT_TEST_FILESYSPATH_IF_NOT_EQUAL, NSTCGNI_PARENT, NSTCGNI_NEXTVISIBLE,
                NSTCS_AUTOHSCROLL, NSTCS_FULLROWSELECT, NSTCS_HASEXPANDOS, NSTCS_NOEDITLABELS,
                NSTCS_SHOWSELECTIONALWAYS, NSTCS_FADEINOUTEXPANDOS, NSTCS_TABSTOP, NSTCS_ROOTHASEXPANDO,
                NSTCS_ALLOWJUNCTIONS, NSTCS2_INTERRUPTNOTIFICATIONS, NSTCS2_DISPLAYPADDING,
                NTSCS2_NOSINGLETONAUTOEXPAND,
                SHCONTF_FOLDERS, SHCONTF_NAVIGATION_ENUM, SHFILEINFOW, SHGFI_OPENICON, SHGFI_PIDL, SHGFI_SMALLICON, SHGFI_SYSICONINDEX,
                FOLDERID_Desktop, FOLDERID_Downloads, FOLDERID_Documents, FOLDERID_Pictures,
                FOLDERID_Music, FOLDERID_Videos, FOLDERID_RecycleBinFolder, FOLDERID_NetworkFolder,
                KF_FLAG_DEFAULT,
            },
            WindowsAndMessaging::{EnumChildWindows, FindWindowExW, GetClassNameW, GetWindowRect, MoveWindow,
                PostMessageW, ShowWindow, SW_HIDE, SW_SHOW, SendMessageW,
                IsChild, IsWindow, IsWindowVisible, SetTimer, KillTimer, WM_GETFONT, WM_KEYDOWN, WM_SYSKEYDOWN, WM_NCDESTROY},
        },
    },
};
use zsdesk_core::model::explorer::ShellLocation;

use super::{
    events::WM_TREE_NAVIGATE,
    shell_item::{location_from_shell_item, shell_item},
};

const S_FALSE: HRESULT = HRESULT(1);
const SYNC_TIMER:usize=0x5a53_5359;
const QUICK_ACCESS:&str="shell:::{679F85CB-0220-4080-B29B-5540CC05AAB6}";

#[derive(Clone,Copy,Debug,PartialEq,Eq)]
enum InitialRoot { ThisPc, QuickAccess, Neither }

fn initial_root(location:&ShellLocation)->InitialRoot {
    match location {
        ShellLocation::ThisPc | ShellLocation::Path(_) => InitialRoot::ThisPc,
        ShellLocation::Search{scope:Some(_),..} => InitialRoot::ThisPc,
        ShellLocation::Shell{parsing_name,..} => {
            let name=parsing_name.get(..6).filter(|prefix|prefix.eq_ignore_ascii_case("shell:"))
                .map_or(parsing_name.as_str(),|_|&parsing_name[6..]);
            if name.eq_ignore_ascii_case("MyComputerFolder") || name.eq_ignore_ascii_case("::{20D04FE0-3AEA-1069-A2D8-08002B30309D}") {InitialRoot::ThisPc}
            else if name.eq_ignore_ascii_case("QuickAccess") || name.eq_ignore_ascii_case("Home")
                || name.eq_ignore_ascii_case("::{679F85CB-0220-4080-B29B-5540CC05AAB6}")
                || name.eq_ignore_ascii_case("::{F874310E-B6B7-47DC-BC84-B9E6B38F5903}") {InitialRoot::QuickAccess}
            else {InitialRoot::Neither}
        }
        _ => InitialRoot::Neither,
    }
}

fn root_style(root:InitialRoot,initial:InitialRoot)->u32 {
    (NSTCRS_VISIBLE.0 | if root==initial {NSTCRS_EXPANDED.0}else{0}) as u32
}

#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub struct NamespaceTreeOptions {
    pub show_recycle_bin:bool,
    pub show_network:bool,
    pub expand_this_pc:Option<bool>,
}
impl Default for NamespaceTreeOptions {
    fn default()->Self {Self{show_recycle_bin:true,show_network:true,expand_this_pc:None}}
}

thread_local! {
    static PENDING_SYNCS:RefCell<HashMap<isize,Weak<PendingTreeSync>>>=RefCell::new(HashMap::new());
}

#[cfg(test)]
thread_local! {
    static CLICK_TRACE: RefCell<Vec<(u32, u32, u32)>> = const { RefCell::new(Vec::new()) };
}

#[cfg(test)]
pub fn take_click_trace_for_test() -> Vec<(u32, u32, u32)> {
    CLICK_TRACE.with(|trace| std::mem::take(&mut *trace.borrow_mut()))
}

pub struct NamespaceTreeHost {
    tree: INameSpaceTreeControl,
    events: INameSpaceTreeControlEvents,
    _filter: IShellItemFilter,
    advise_cookie: u32,
    hwnd: HWND,
    last_location: Rc<RefCell<Option<IShellItem>>>,
    last_auto_expand: Cell<bool>,
    syncing: Rc<Cell<u32>>,
    pending_sync:Rc<PendingTreeSync>,
    bounds:Cell<RECT>,
}

impl NamespaceTreeHost {
    /// Creates the navigation tree.
    ///
    /// # Safety
    ///
    /// COM must be initialized as STA on the current UI thread. `parent` must
    /// be a valid window owned by that thread and outlive this host.
    pub unsafe fn create(parent: HWND, rect: RECT) -> Result<Self> {
        unsafe {Self::create_for_location(parent,rect,&Self::system_launch_location())}
    }

    /// Creates roots for the first displayed folder. This affects startup only;
    /// later navigation preserves manual expansion and the auto-expand policy.
    /// # Safety
    /// The same STA and parent-window lifetime requirements as `create` apply.
    pub unsafe fn create_for_location(parent:HWND,rect:RECT,location:&ShellLocation)->Result<Self> {
        unsafe{Self::create_with_options(parent,rect,location,NamespaceTreeOptions::default())}
    }

    /// Root visibility is local to this tree and never changes Windows policy.
    pub unsafe fn create_with_options(parent:HWND,rect:RECT,location:&ShellLocation,options:NamespaceTreeOptions)->Result<Self> {
        let initial=initial_root(location);
        let tree: INameSpaceTreeControl = unsafe { CoCreateInstance(&NamespaceTreeControl, None, CLSCTX_INPROC_SERVER)? };
        let style = (NSTCS_HASEXPANDOS.0 | NSTCS_ROOTHASEXPANDO.0 | NSTCS_FULLROWSELECT.0
            | NSTCS_SHOWSELECTIONALWAYS.0 | NSTCS_AUTOHSCROLL.0 | NSTCS_ALLOWJUNCTIONS.0
            | NSTCS_NOEDITLABELS.0 | NSTCS_FADEINOUTEXPANDOS.0 | NSTCS_TABSTOP.0) as u32;
        let bounds = navigation_rect(rect, unsafe { GetDpiForWindow(parent) });
        unsafe { tree.Initialize(parent, &bounds, style)? };
        let hwnd = unsafe { tree.cast::<IOleWindow>().and_then(|w| w.GetWindow()).unwrap_or_default() };

        if let Ok(extended) = tree.cast::<INameSpaceTreeControl2>() {
            // A bare namespace control has no root separation by default.
            // Ask the Shell to supply its own group padding instead of adding
            // application-owned blank rows or pixel offsets between roots.
            let mut styles = NSTCS2_INTERRUPTNOTIFICATIONS | NTSCS2_NOSINGLETONAUTOEXPAND;
            if crate::theme::os_build() >= 22000 { styles |= NSTCS2_DISPLAYPADDING; }
            let _ = unsafe { extended.SetControlStyle2(styles, styles) };
        }

        let filter: IShellItemFilter = FolderFilter.into();
        let enum_flags = navigation_enum_flags();

        let quick_access = ShellLocation::Shell {
            pidl:None, parsing_name: QUICK_ACCESS.into(),
            title: "快速访问".into(),
        };
        let quick_access_added = unsafe { shell_item(&quick_access) }.is_ok_and(|item| {
            // Quick access is where the user's folders live now that This PC
            // shows drives only; keep its pinned folders visible at startup.
            unsafe { tree.AppendRoot(&item, enum_flags, (NSTCRS_VISIBLE.0 | NSTCRS_EXPANDED.0) as u32, &filter) }.is_ok()
        });
        if !quick_access_added {
            for folder in [&FOLDERID_Desktop, &FOLDERID_Downloads, &FOLDERID_Documents,
                &FOLDERID_Pictures, &FOLDERID_Music, &FOLDERID_Videos] {
                if let Ok(item) = unsafe { SHGetKnownFolderItem::<IShellItem>(folder, KF_FLAG_DEFAULT, None) } {
                    let _ = unsafe { tree.AppendRoot(&item, enum_flags, NSTCRS_VISIBLE.0 as u32, &filter) };
                }
            }
        }
        let this_pc = unsafe { shell_item(&ShellLocation::ThisPc)? };
        let this_pc_style=options.expand_this_pc.map_or_else(||root_style(InitialRoot::ThisPc,initial),|expanded|
            (NSTCRS_VISIBLE.0 | if expanded{NSTCRS_EXPANDED.0}else{0})as u32);
        unsafe { tree.AppendRoot(&this_pc, enum_flags, this_pc_style, &filter)?; }
        for (folder,visible) in [(&FOLDERID_RecycleBinFolder,options.show_recycle_bin),(&FOLDERID_NetworkFolder,options.show_network)] {
            if !visible{continue;}
            if let Ok(item) = unsafe { SHGetKnownFolderItem::<IShellItem>(folder, KF_FLAG_DEFAULT, None) } {
                let _ = unsafe { tree.AppendRoot(&item, enum_flags, NSTCRS_VISIBLE.0 as u32, &filter) };
            }
        }

        let syncing=Rc::new(Cell::new(0));
        let last_location=Rc::new(RefCell::new(None));
        let pending_sync=Rc::new(PendingTreeSync{tree:tree.clone(),hwnd,native_tree:Cell::new(HWND::default()),syncing:syncing.clone(),
            generation:Rc::new(Cell::new(0)),request:RefCell::new(None),last_location:last_location.clone()});
        let events: INameSpaceTreeControlEvents = TreeEvents { hwnd: parent.0 as isize, syncing: syncing.clone(),pending_sync:Rc::downgrade(&pending_sync) }.into();
        let advise_cookie = unsafe { tree.TreeAdvise(&events)? };
        PENDING_SYNCS.with(|pending|{pending.borrow_mut().insert(hwnd.0 as isize,Rc::downgrade(&pending_sync));});
        let host = Self { tree, events, _filter: filter, advise_cookie, hwnd, last_location,
            last_auto_expand: Cell::new(false), syncing,pending_sync,bounds:Cell::new(bounds) };
        if let Some(view) = host.tree_view() {
            unsafe {SetWindowSubclass(view,Some(tree_sync_subclass),SYNC_TIMER,hwnd.0 as usize)}.ok()?;
            host.pending_sync.native_tree.set(view);
            unsafe { SendMessageW(view, TVM_SETEXTENDEDSTYLE, Some(WPARAM(TVS_EX_DOUBLEBUFFER as usize)), Some(LPARAM(TVS_EX_DOUBLEBUFFER as isize))); }
        }
        host.refresh_metrics();
        Ok(host)
    }

    fn tree_view(&self) -> Option<HWND> {
        if self.hwnd.0.is_null() { return None; }
        if is_tree_view(self.hwnd) { return Some(self.hwnd); }
        if let Ok(view) = unsafe { FindWindowExW(Some(self.hwnd), None, windows::core::w!("SysTreeView32"), PCWSTR::null()) } {
            return Some(view);
        }
        // Shell versions can introduce an intermediate host window.
        let mut found = HWND::default();
        let _ = unsafe { EnumChildWindows(Some(self.hwnd), Some(find_tree_view), LPARAM((&mut found as *mut HWND) as isize)) };
        (!found.0.is_null()).then_some(found)
    }

    pub fn hwnd(&self) -> HWND { self.hwnd }

    /// Re-read display preferences without changing any Windows settings.
    pub fn refresh_metrics(&self) {
        let Some(view) = self.tree_view() else { return };
        let build = crate::theme::os_build();
        if build < 22000 { return; }
        let mut compact = 0u32;
        let mut bytes = std::mem::size_of_val(&compact) as u32;
        let _ = unsafe { RegGetValueW(HKEY_CURRENT_USER,
            windows::core::w!("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\Advanced"),
            windows::core::w!("UseCompactMode"), RRF_RT_REG_DWORD, None,
            Some((&mut compact as *mut u32).cast()), Some(&mut bytes)) };
        let mut font = LOGFONTW::default();
        let handle = unsafe { SendMessageW(view, WM_GETFONT, None, None) };
        if handle.0 != 0 {
            unsafe { GetObjectW(HGDIOBJ(handle.0 as *mut _), std::mem::size_of::<LOGFONTW>() as i32,
                Some((&mut font as *mut LOGFONTW).cast())); }
        }
        if let Some(height) = navigation_row_height(build, unsafe { GetDpiForWindow(view) },
            compact != 0, font.lfHeight.saturating_abs()) {
            if unsafe { SendMessageW(view, TVM_GETITEMHEIGHT, None, None) }.0 != height as isize {
                unsafe { SendMessageW(view, TVM_SETITEMHEIGHT, Some(WPARAM(height as usize)), None); }
            }
        }
        // Indentation is left to the current Shell/theme rather than applying
        // one fixed indentation to every OS and accessibility configuration.
    }

    /// Applies light/dark colors to the tree.
    pub fn apply_theme(&self, dark: bool, background: u32, text: u32) {
        crate::theme::theme_control(self.hwnd, dark);
        if let Some(view) = self.tree_view() {
            crate::theme::theme_control(view, dark);
            unsafe {
                SendMessageW(view, TVM_SETBKCOLOR, None, Some(LPARAM(COLORREF(background).0 as isize)));
                SendMessageW(view, TVM_SETTEXTCOLOR, None, Some(LPARAM(COLORREF(text).0 as isize)));
            }
        }
        self.refresh_metrics();
    }

    /// Resizes the native namespace tree.
    ///
    /// # Safety
    ///
    /// Must be called on the STA/UI thread that created this host.
    pub unsafe fn resize(&self, rect: RECT) {
        if self.hwnd.0.is_null() { return; }
        let rect = navigation_rect(rect, unsafe { GetDpiForWindow(self.hwnd) });
        // Layout notifications can repeat while the native view scrolls or
        // updates its selection. An unchanged rectangle must not force a
        // synchronous erase/repaint of the entire navigation tree.
        if self.bounds.get()!=rect && unsafe { MoveWindow(self.hwnd, rect.left, rect.top, rect.right - rect.left, rect.bottom - rect.top, true) }.is_ok() {
            self.bounds.set(rect);
        }
        self.refresh_metrics();
    }

    pub fn set_visible(&self, visible: bool) {
        if !self.hwnd.0.is_null() { let _ = unsafe { ShowWindow(self.hwnd, if visible { SW_SHOW } else { SW_HIDE }) }; }
    }

    /// Synchronizes a visible item without opening collapsed branches.
    ///
    /// # Safety
    ///
    /// Must be called on the STA/UI thread that created this host while its COM
    /// apartment remains initialized.
    pub unsafe fn select_location(&self, location: &ShellLocation) {
        unsafe { self.select_location_with_policy(location,Self::system_auto_expand()); }
    }

    /// `auto_expand` permits expanding the destination's ancestors. Sync never
    /// navigates the right pane or transfers keyboard focus to this tree.
    ///
    /// # Safety
    /// Must be called on the creating STA while the host and its parent live.
    pub unsafe fn select_location_with_policy(&self, location: &ShellLocation, auto_expand: bool) {
        self.pending_sync.cancel();
        let _sync=TreeSyncScope::new(self.hwnd,self.syncing.clone(),self.pending_sync.generation.clone());
        let Ok(item) = (unsafe { shell_item(location) }) else { self.clear_selection(); return };
        self.last_auto_expand.set(auto_expand);
        // Prefer an existing Quick access entry instead of expanding the same
        // folder again beneath its drive and pushing quick links off-screen.
        if let Some(row)=unsafe {self.shown_row(&item)} {
            if unsafe {self.select_exposed(&row)} {
                *self.last_location.borrow_mut() = Some(item);
                return;
            }
        }
        if !auto_expand {self.clear_selection();return;}
        let mut chain = Vec::new();
        let mut cursor = unsafe { item.GetParent() };
        while let Ok(parent) = cursor {
            let exposed=unsafe {self.item_is_exposed(&parent)};
            cursor = unsafe { parent.GetParent() };
            chain.push(parent);
            // A visible shortcut/root is the anchor. Expanding beyond it would
            // also open a second copy of the path beneath This PC.
            if exposed || chain.len() >= 64 { break; }
        }
        self.pending_sync.begin(item,chain);
    }

    /// Leaves a clicked tree row intact when its navigation completes.
    /// # Safety
    /// Call on the creating STA while its COM apartment remains initialized.
    pub unsafe fn adopt_location(&self,location:&ShellLocation) {
        self.pending_sync.cancel();
        *self.last_location.borrow_mut()=unsafe {shell_item(location)}.ok();
    }

    unsafe fn shown_row(&self,item:&IShellItem)->Option<IShellItem> {
        let hint=(SICHINT_CANONICAL.0|SICHINT_TEST_FILESYSPATH_IF_NOT_EQUAL.0)as u32;
        let mut row=unsafe {self.tree.GetRootItems().and_then(|items|items.GetItemAt(0))}.ok();
        for _ in 0..4096 {
            let current=row?;
            if unsafe {current.Compare(item,hint)}.is_ok_and(|order|order==0){return Some(current);}
            row=unsafe {self.tree.GetNextItem(&current,NSTCGNI_NEXTVISIBLE)}.ok();
        }
        None
    }

    fn clear_selection(&self) {
        *self.last_location.borrow_mut()=None;
        if let Some(view)=self.tree_view() {
            unsafe {SendMessageW(view,TVM_SELECTITEM,Some(WPARAM(TVGN_CARET as usize)),Some(LPARAM(0)));}
        }
    }

    pub fn system_auto_expand()->bool {
        let mut value=0u32;let mut size=4u32;
        let result=unsafe {RegGetValueW(HKEY_CURRENT_USER,windows::core::w!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\Advanced"),
            windows::core::w!("NavPaneExpandToCurrentFolder"),RRF_RT_REG_DWORD,None,Some((&mut value as *mut u32).cast()),Some(&mut size))};
        result.is_ok() && value!=0
    }

    /// Reads Explorer's startup preference without changing the user's setting.
    /// LaunchTo=1 means This PC; missing or other values keep the Home default.
    pub fn system_launch_location()->ShellLocation {
        let mut value=0u32;let mut size=4u32;
        let result=unsafe {RegGetValueW(HKEY_CURRENT_USER,windows::core::w!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\Advanced"),
            windows::core::w!("LaunchTo"),RRF_RT_REG_DWORD,None,Some((&mut value as *mut u32).cast()),Some(&mut size))};
        launch_location(result.is_ok().then_some(value))
    }

    unsafe fn select_exposed(&self,item:&IShellItem)->bool {
        unsafe {select_exposed(&self.tree,self.hwnd,item)}
    }

    unsafe fn item_is_exposed(&self,item:&IShellItem)->bool {
        unsafe {item_is_exposed(&self.tree,item)}
    }

    /// Disconnects the namespace tree event sink.
    ///
    /// # Safety
    ///
    /// Must be called exactly once on the STA/UI thread that created this host,
    /// before the COM apartment is uninitialized.
    pub unsafe fn destroy(&self) {
        self.pending_sync.cancel();
        let view=self.pending_sync.native_tree.replace(HWND::default());
        if !view.0.is_null() {let _=unsafe {RemoveWindowSubclass(view,Some(tree_sync_subclass),SYNC_TIMER)};}
        PENDING_SYNCS.with(|pending|{
            let mut pending=pending.borrow_mut();
            if pending.get(&(self.hwnd.0 as isize)).is_some_and(|state|Weak::ptr_eq(state,&Rc::downgrade(&self.pending_sync))) {
                pending.remove(&(self.hwnd.0 as isize));
            }
        });
        let _keep_events_alive = &self.events;
        let _ = unsafe { self.tree.TreeUnadvise(self.advise_cookie) };
    }
}

struct TreeSyncRequest {
    item:IShellItem,
    ancestors:Vec<IShellItem>,
    expansion_requested:Vec<bool>,
    deadline:Instant,
    attempts:u32,
}

struct PendingTreeSync {
    tree:INameSpaceTreeControl,
    hwnd:HWND,
    native_tree:Cell<HWND>,
    syncing:Rc<Cell<u32>>,
    generation:Rc<Cell<u64>>,
    request:RefCell<Option<TreeSyncRequest>>,
    last_location:Rc<RefCell<Option<IShellItem>>>,
}

impl PendingTreeSync {
    fn stop_timer(&self) {
        let owned=PENDING_SYNCS.with(|pending|pending.borrow().get(&(self.hwnd.0 as isize))
            .is_some_and(|state|state.as_ptr()==self as *const Self));
        if owned {let _=unsafe {KillTimer(Some(self.hwnd),SYNC_TIMER)};}
    }
    fn cancel(&self) {
        self.generation.set(self.generation.get().wrapping_add(1));
        self.request.borrow_mut().take();self.stop_timer();
    }
    fn begin(&self,item:IShellItem,ancestors:Vec<IShellItem>) {
        let expansion_requested=vec![false;ancestors.len()];
        *self.request.borrow_mut()=Some(TreeSyncRequest{item,ancestors,expansion_requested,
            deadline:Instant::now()+Duration::from_secs(5),attempts:0});
        self.advance();
    }
    fn advance(&self) {
        // COM can pump messages. Never hold a request borrow across native calls;
        // a new navigation, click or collapse must be able to cancel this work.
        let Some(mut request)=self.request.borrow_mut().take() else {self.stop_timer();return;};
        if Instant::now()>=request.deadline || request.attempts>=100 || !unsafe {IsWindow(Some(self.hwnd))}.as_bool() {
            self.stop_timer();return;
        }
        request.attempts+=1;
        let generation=self.generation.get();
        let _sync=TreeSyncScope::new(self.hwnd,self.syncing.clone(),self.generation.clone());
        // NamespaceTreeControl materializes each level asynchronously. Request a
        // level once and let its native enumeration finish before descending.
        for index in (0..request.ancestors.len()).rev() {
            if self.generation.get()!=generation {return;}
            let ancestor=&request.ancestors[index];
            let state=unsafe {self.tree.GetItemState(ancestor,NSTCIS_EXPANDED.0 as u32)};
            let Ok(state)=state else {break;};
            if state&NSTCIS_EXPANDED.0 as u32==0 {
                if !request.expansion_requested[index] {
                    let result=unsafe {self.tree.SetItemState(ancestor,NSTCIS_EXPANDED.0 as u32,NSTCIS_EXPANDED.0 as u32)};
                    request.expansion_requested[index]=result.is_ok();
                    #[cfg(test)]
                    eprintln!("tree-sync async expand attempt={} item={:?} result={result:?}",request.attempts,unsafe {location_from_shell_item(ancestor)});
                }
                break;
            }
        }
        if self.generation.get()!=generation {return;}
        if unsafe {select_exposed(&self.tree,self.hwnd,&request.item)} {
            if self.generation.get()==generation {*self.last_location.borrow_mut()=Some(request.item);self.stop_timer();}
            return;
        }
        if self.generation.get()!=generation {return;}
        // While descendants load, keep the nearest exposed ancestor selected.
        for ancestor in &request.ancestors {
            if unsafe {select_exposed(&self.tree,self.hwnd,ancestor)} {
                if self.generation.get()==generation {*self.last_location.borrow_mut()=Some(ancestor.clone());}
                break;
            }
            if self.generation.get()!=generation {return;}
        }
        if self.generation.get()!=generation {return;}
        *self.request.borrow_mut()=Some(request);
        if unsafe {SetTimer(Some(self.hwnd),SYNC_TIMER,50,Some(tree_sync_timer))}==0 {self.request.borrow_mut().take();}
    }
}

unsafe extern "system" fn tree_sync_timer(hwnd:HWND,_message:u32,id:usize,_time:u32) {
    if id!=SYNC_TIMER {return;}
    let pending=PENDING_SYNCS.with(|pending|pending.borrow().get(&(hwnd.0 as isize)).and_then(Weak::upgrade));
    if let Some(pending)=pending {pending.advance();}
}

unsafe extern "system" fn tree_sync_subclass(hwnd:HWND,message:u32,wp:WPARAM,lp:LPARAM,id:usize,reference:usize)->LRESULT {
    let pending=PENDING_SYNCS.with(|pending|pending.borrow().get(&(reference as isize)).and_then(Weak::upgrade));
    if let Some(pending)=pending.filter(|pending|pending.native_tree.get()==hwnd) {
        // Native SetItemState collapse does not consistently raise the namespace
        // state-changing event. Observe the underlying TreeView command instead.
        // TVE_TOGGLE also contains COLLAPSE's bit and represents a user decision.
        if message==TVM_EXPAND && wp.0&TVE_COLLAPSE.0 as usize!=0 {
            #[cfg(test)]
            eprintln!("tree-sync native-collapse flags={} pending={}",wp.0,pending.request.borrow().is_some());
            pending.cancel();
        }
        if message==WM_NCDESTROY {pending.cancel();pending.native_tree.set(HWND::default());}
    }
    if message==WM_NCDESTROY {let _=unsafe {RemoveWindowSubclass(hwnd,Some(tree_sync_subclass),id)};}
    unsafe {DefSubclassProc(hwnd,message,wp,lp)}
}

unsafe fn item_is_exposed(tree:&INameSpaceTreeControl,item:&IShellItem)->bool {
    if !unsafe {tree.GetItemRect(item)}.is_ok_and(|rect|rect.bottom>rect.top) {return false;}
    let mut cursor=item.clone();
    for _ in 0..64 {
        let Ok(parent)=(unsafe {tree.GetNextItem(&cursor,NSTCGNI_PARENT)}) else {return true;};
        if !unsafe {tree.GetItemState(&parent,NSTCIS_EXPANDED.0 as u32)}.is_ok_and(|state|state&NSTCIS_EXPANDED.0 as u32!=0) {return false;}
        cursor=parent;
    }
    false
}

unsafe fn select_exposed(tree:&INameSpaceTreeControl,hwnd:HWND,item:&IShellItem)->bool {
    if !unsafe {item_is_exposed(tree,item)} {return false;}
    let selected=unsafe {tree.GetItemState(item,NSTCIS_SELECTED.0 as u32)}.is_ok_and(|state|state&NSTCIS_SELECTED.0 as u32!=0);
    if !selected && (unsafe {tree.SetItemState(item,NSTCIS_SELECTEDNOEXPAND.0 as u32,NSTCIS_SELECTEDNOEXPAND.0 as u32)}.is_err()
        || !unsafe {tree.GetItemState(item,NSTCIS_SELECTED.0 as u32)}.is_ok_and(|state|state&NSTCIS_SELECTED.0 as u32!=0)) {return false;}
    let mut bounds=RECT::default();
    // GetItemRect reports screen coordinates, including for offscreen hosts.
    let visible=unsafe {GetWindowRect(hwnd,&mut bounds)}.is_ok() && unsafe {tree.GetItemRect(item)}.is_ok_and(|rect|
        rect.bottom>rect.top && rect.top>=bounds.top && rect.bottom<=bounds.bottom);
    if !visible {let _=unsafe {tree.EnsureItemVisible(item)};}
    true
}

fn scale(value: i32, dpi: u32) -> i32 {
    ((i64::from(value) * i64::from(dpi.max(96)) + 48) / 96) as i32
}

fn navigation_row_height(build: u32, dpi: u32, compact: bool, font_height: i32) -> Option<i32> {
    if build < 22000 { return None; }
    // Density is expressed in logical pixels, never in the host machine's
    // physical pixels. Larger accessibility fonts must not be clipped.
    let (row, padding) = if compact { (24, 8) } else { (32, 12) };
    let height = scale(row, dpi).max(font_height.saturating_add(scale(padding, dpi)));
    Some(height.saturating_add(1) & !1)
}

fn launch_location(value:Option<u32>)->ShellLocation {
    if value==Some(1) {ShellLocation::ThisPc}
    else {ShellLocation::Shell{pidl:None, parsing_name:QUICK_ACCESS.into(),title:"快速访问".into()}}
}

fn navigation_rect(rect: RECT, dpi: u32) -> RECT {
    let horizontal = scale(4, dpi).min((rect.right - rect.left).max(0) / 2);
    let vertical = scale(4, dpi).min((rect.bottom - rect.top).max(0) / 2);
    RECT { left: rect.left + horizontal, top: rect.top + vertical,
        right: (rect.right - horizontal).max(rect.left + horizontal),
        bottom: (rect.bottom - vertical).max(rect.top + vertical) }
}

fn is_tree_view(hwnd: HWND) -> bool {
    let mut name = [0u16; 32];
    let length = unsafe { GetClassNameW(hwnd, &mut name) }.max(0) as usize;
    String::from_utf16_lossy(&name[..length]) == "SysTreeView32"
}

unsafe extern "system" fn find_tree_view(hwnd: HWND, result: LPARAM) -> BOOL {
    if is_tree_view(hwnd) {
        unsafe { *(result.0 as *mut HWND) = hwnd; }
        BOOL(0)
    } else { BOOL(1) }
}

impl Drop for NamespaceTreeHost {
    fn drop(&mut self) { unsafe { self.destroy(); } }
}

fn navigation_enum_flags()->u32 {
    // Namespace providers distinguish their navigation hierarchy (including
    // network containers and library roots) from an ordinary contents view.
    (SHCONTF_FOLDERS.0 | SHCONTF_NAVIGATION_ENUM.0) as u32
}

/// The Shell enumerator selects folders; only positively identified streams
/// are removed here. An unavailable optional attribute must not hide a remote
/// container or prevent the Shell from expanding it after authentication.
#[implement(IShellItemFilter)]
struct FolderFilter;

#[allow(non_snake_case)]
impl IShellItemFilter_Impl for FolderFilter_Impl {
    fn IncludeItem(&self, item: Ref<IShellItem>) -> Result<()> {
        let Some(item) = item.as_ref() else { return Err(Error::from_hresult(S_FALSE)) };
        let stream=unsafe { item.GetAttributes(SFGAO_STREAM) }.is_ok_and(|attrs|attrs.0 & SFGAO_STREAM.0 != 0);
        // Quick access already lists the user's folders; This PC keeps its drives.
        if stream || super::shell_item::hidden_in_this_pc(item) { Err(Error::from_hresult(S_FALSE)) } else { Ok(()) }
    }
    fn GetEnumFlagsForItem(&self, _item: Ref<IShellItem>) -> Result<u32> { Ok(navigation_enum_flags()) }
}

#[implement(INameSpaceTreeControlEvents)]
struct TreeEvents {
    hwnd: isize,
    syncing: Rc<Cell<u32>>,
    pending_sync:Weak<PendingTreeSync>,
}

impl TreeEvents {
    fn cancel_sync(&self) {if let Some(pending)=self.pending_sync.upgrade(){pending.cancel();}}
    fn post_item(&self, item: &IShellItem, new_tab: bool) {
        let Some(location) = (unsafe { location_from_shell_item(item) }) else { return };
        let location = Box::into_raw(Box::new(location));
        if unsafe { PostMessageW(Some(HWND(self.hwnd as *mut _)), WM_TREE_NAVIGATE, WPARAM(new_tab as usize), LPARAM(location as isize)) }.is_err() {
            unsafe { drop(Box::from_raw(location)); }
        }
    }
}

struct TreeSyncScope { tree: HWND, previous_focus: HWND, syncing: Rc<Cell<u32>>, generation:Rc<Cell<u64>>, initial_generation:u64 }
impl TreeSyncScope {
    fn new(tree:HWND,syncing:Rc<Cell<u32>>,generation:Rc<Cell<u64>>)->Self {
        syncing.set(syncing.get().saturating_add(1));
        let initial_generation=generation.get();
        Self {tree,previous_focus:unsafe{GetFocus()},syncing,generation,initial_generation}
    }
}
impl Drop for TreeSyncScope {
    fn drop(&mut self) {
        let current=unsafe {GetFocus()};
        if self.generation.get()==self.initial_generation && current!=self.previous_focus
            && (current==self.tree || unsafe {IsChild(self.tree,current)}.as_bool()) {
            let previous=self.previous_focus;
            let target=(!previous.0.is_null() && unsafe {IsWindow(Some(previous))}.as_bool()
                && unsafe {IsWindowVisible(previous)}.as_bool() && unsafe {IsWindowEnabled(previous)}.as_bool()).then_some(previous);
            let _=unsafe {SetFocus(target)};
        }
        self.syncing.set(self.syncing.get().saturating_sub(1));
    }
}

/// Returns whether an item click opens a new tab. Structural tree regions stay
/// under native TreeView control and must never navigate the folder view.
fn navigation_click(hit_test: u32, click_type: u32) -> Option<bool> {
    let structural = (NSTCEHT_ONITEMBUTTON.0 | NSTCEHT_ONITEMINDENT.0) as u32;
    // FULLROWSELECT includes the blank area after the label. ONITEM alone only
    // contains the icon, label and state icon, so it misses that part of a row.
    let item_row = (NSTCEHT_ONITEM.0 | NSTCEHT_ONITEMRIGHT.0) as u32;
    if hit_test & structural != 0 || hit_test & item_row == 0 { return None; }
    match click_type & NSTCECT_BUTTON.0 as u32 {
        button if button == NSTCECT_LBUTTON.0 as u32 => Some(false),
        button if button == NSTCECT_MBUTTON.0 as u32 => Some(true),
        _ => None,
    }
}

unsafe fn system_icon_indices(item: &IShellItem) -> Option<(i32, i32)> {
    let pidl = unsafe { SHGetIDListFromObject(item).ok()? };
    let path = PCWSTR(pidl.cast());
    let flags = SHGFI_PIDL | SHGFI_SYSICONINDEX | SHGFI_SMALLICON;
    let mut default_info = SHFILEINFOW::default();
    let mut open_info = SHFILEINFOW::default();
    let info_size = std::mem::size_of::<SHFILEINFOW>() as u32;
    let default_result = unsafe { SHGetFileInfoW(path, FILE_FLAGS_AND_ATTRIBUTES(0), Some(&mut default_info), info_size, flags) };
    let open_result = unsafe { SHGetFileInfoW(path, FILE_FLAGS_AND_ATTRIBUTES(0), Some(&mut open_info), info_size, flags | SHGFI_OPENICON) };
    unsafe { CoTaskMemFree(Some(pidl.cast())) };
    (default_result != 0 && open_result != 0).then_some((default_info.iIcon, open_info.iIcon))
}

#[allow(non_snake_case)]
impl INameSpaceTreeControlEvents_Impl for TreeEvents_Impl {
    fn OnItemClick(&self, shell_item: Ref<IShellItem>, hit_test: u32, click_type: u32) -> Result<()> {
        // Click events represent actual input; unlike selection notifications,
        // they must override any pending programmatic synchronization.
        self.cancel_sync();
        #[cfg(test)]
        CLICK_TRACE.with(|trace| trace.borrow_mut().push((hit_test, click_type,
            unsafe { windows::Win32::UI::WindowsAndMessaging::GetMessagePos() })));
        if let Some(new_tab) = navigation_click(hit_test, click_type) {
            if let Some(item) = shell_item.as_ref() { self.post_item(item, new_tab); }
            // Middle click is fully handled here; do not change tree selection.
            if new_tab { return Ok(()); }
        }
        // S_OK consumes TreeView's default processing, including expand/collapse.
        Err(Error::from_hresult(S_FALSE))
    }
    fn OnPropertyItemCommit(&self, _item: Ref<IShellItem>) -> Result<()> { Err(Error::from_hresult(S_FALSE)) }
    fn OnItemStateChanging(&self, _item: Ref<IShellItem>, mask: u32, state: u32) -> Result<()> {
        if self.syncing.get()==0 && mask&NSTCIS_EXPANDED.0 as u32!=0 && state&NSTCIS_EXPANDED.0 as u32==0 {self.cancel_sync();}
        Ok(())
    }
    fn OnItemStateChanged(&self, _item: Ref<IShellItem>, _mask: u32, _state: u32) -> Result<()> { Ok(()) }
    fn OnSelectionChanged(&self, _selection: Ref<IShellItemArray>) -> Result<()> { Ok(()) }
    fn OnKeyboardInput(&self, message: u32, _wparam: WPARAM, _lparam: LPARAM) -> Result<()> {
        if message==WM_KEYDOWN || message==WM_SYSKEYDOWN {self.cancel_sync();}
        Err(Error::from_hresult(S_FALSE))
    }
    fn OnBeforeExpand(&self, _item: Ref<IShellItem>) -> Result<()> { Ok(()) }
    fn OnAfterExpand(&self, _item: Ref<IShellItem>) -> Result<()> { Ok(()) }
    fn OnBeginLabelEdit(&self, _item: Ref<IShellItem>) -> Result<()> { Ok(()) }
    fn OnEndLabelEdit(&self, _item: Ref<IShellItem>) -> Result<()> { Ok(()) }
    fn OnGetToolTip(&self, _item: Ref<IShellItem>, _tip: PWSTR, _length: i32) -> Result<()> { Err(Error::from_hresult(windows::core::HRESULT(0x80004001u32 as i32))) }
    fn OnBeforeItemDelete(&self, _item: Ref<IShellItem>) -> Result<()> { Ok(()) }
    fn OnItemAdded(&self, _item: Ref<IShellItem>, _is_root: BOOL) -> Result<()> { Ok(()) }
    fn OnItemDeleted(&self, _item: Ref<IShellItem>, _is_root: BOOL) -> Result<()> { Ok(()) }
    fn OnBeforeContextMenu(&self, _item: Ref<IShellItem>, _riid: *const windows::core::GUID, result: *mut *mut core::ffi::c_void) -> Result<()> {
        if !result.is_null() { unsafe { *result = std::ptr::null_mut(); } }
        Err(Error::from_hresult(windows::core::HRESULT(0x80004001u32 as i32)))
    }
    fn OnAfterContextMenu(&self, _item: Ref<IShellItem>, _menu: Ref<IContextMenu>, _riid: *const windows::core::GUID, result: *mut *mut core::ffi::c_void) -> Result<()> {
        if !result.is_null() { unsafe { *result = std::ptr::null_mut(); } }
        Err(Error::from_hresult(windows::core::HRESULT(0x80004001u32 as i32)))
    }
    fn OnBeforeStateImageChange(&self, _item: Ref<IShellItem>) -> Result<()> { Ok(()) }
    fn OnGetDefaultIconIndex(&self, item: Ref<IShellItem>, default_icon: *mut i32, open_icon: *mut i32) -> Result<()> {
        let Some(item) = item.as_ref() else { return Err(Error::from_hresult(S_FALSE)) };
        let Some((default_index, open_index)) = (unsafe { system_icon_indices(item) }) else { return Err(Error::from_hresult(S_FALSE)) };
        if default_icon.is_null() || open_icon.is_null() { return Err(Error::from_hresult(S_FALSE)); }
        unsafe { *default_icon = default_index; *open_icon = open_index; }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::{System::{Com::IBindCtx,SystemServices::{SFGAO_FLAGS,SFGAO_FOLDER}},
        UI::Shell::{IShellItem_Impl,SIGDN}};
    use windows::Win32::UI::Shell::{NSTCECT_DBLCLICK, NSTCECT_RBUTTON,
        NSTCEHT_NOWHERE, NSTCEHT_ONITEMICON, NSTCEHT_ONITEMLABEL, NSTCEHT_ONITEMSTATEICON};

    #[implement(IShellItem)]
    struct FilterItem{attributes:Option<u32>}
    #[allow(non_snake_case)]
    impl IShellItem_Impl for FilterItem_Impl{
        fn BindToHandler(&self,_:Ref<IBindCtx>,_:*const windows::core::GUID,_:*const windows::core::GUID,_:*mut*mut core::ffi::c_void)->Result<()>{Err(Error::from_hresult(HRESULT(0x80004001u32 as i32)))}
        fn GetParent(&self)->Result<IShellItem>{Err(Error::from_hresult(HRESULT(0x80004001u32 as i32)))}
        fn GetDisplayName(&self,_:SIGDN)->Result<PWSTR>{Err(Error::from_hresult(HRESULT(0x80004001u32 as i32)))}
        fn GetAttributes(&self,mask:SFGAO_FLAGS)->Result<SFGAO_FLAGS>{self.attributes.map(|value|SFGAO_FLAGS(value&mask.0))
            .ok_or_else(||Error::from_hresult(HRESULT(0x80070035u32 as i32)))}
        fn Compare(&self,_:Ref<IShellItem>,_:u32)->Result<i32>{Err(Error::from_hresult(HRESULT(0x80004001u32 as i32)))}
    }

    #[test]
    fn namespace_filter_preserves_deferred_network_containers(){
        let filter:IShellItemFilter=FolderFilter.into();
        // The namespace enumerator already restricted candidates to folders.
        // Remote providers may not expose optional stream attributes until a
        // connection is ready; do not turn that into a permanent empty branch.
        for attributes in [None,Some(0),Some(SFGAO_FOLDER.0)]{
            let item:IShellItem=FilterItem{attributes}.into();
            let result=unsafe{(Interface::vtable(&filter).IncludeItem)(Interface::as_raw(&filter),Interface::as_raw(&item))};
            assert_eq!(result,HRESULT(0));
            let flags=unsafe{filter.GetEnumFlagsForItem(&item)}.unwrap();
            assert_ne!(flags&SHCONTF_NAVIGATION_ENUM.0 as u32,0);
            assert_ne!(flags&SHCONTF_FOLDERS.0 as u32,0);
            assert_eq!(flags&windows::Win32::UI::Shell::SHCONTF_NONFOLDERS.0 as u32,0);
        }
        let archive:IShellItem=FilterItem{attributes:Some(SFGAO_FOLDER.0|SFGAO_STREAM.0)}.into();
        assert_eq!(unsafe{(Interface::vtable(&filter).IncludeItem)(Interface::as_raw(&filter),Interface::as_raw(&archive))},S_FALSE);
    }

    #[test]
    #[ignore="Hidden namespace hierarchy probe over owned local fixture directories; no network access or input"]
    fn native_navigation_tree_expands_nested_folders()->std::result::Result<(),Box<dyn std::error::Error>>{
        use std::{path::PathBuf,os::windows::fs::MetadataExt};
        use windows::Win32::{System::Ole::{OleInitialize,OleUninitialize},UI::WindowsAndMessaging::*};
        let output=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").ok_or("isolated output required")?);
        assert!(output.is_absolute());
        let root=output.join("namespace-hierarchy-fixture");
        let server=root.join("server");let share=server.join("share");let child=share.join("child");
        struct Dirs(Vec<PathBuf>);impl Drop for Dirs{fn drop(&mut self){for path in self.0.iter().rev(){let _=std::fs::remove_dir(path);}}}
        let mut directories=Dirs(Vec::new());
        for path in [&root,&server,&share,&child]{
            if !path.exists(){std::fs::create_dir(path)?;directories.0.push(path.clone());}
            assert_eq!(std::fs::symlink_metadata(path)?.file_attributes()&0x400,0,"fixture is a reparse point");
        }
        unsafe{OleInitialize(None)?;}
        struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}let _apartment=Apartment;
        struct Window(HWND);impl Drop for Window{fn drop(&mut self){let _=unsafe{DestroyWindow(self.0)};}}
        let owner=Window(unsafe{CreateWindowExW(WS_EX_NOACTIVATE,windows::core::w!("STATIC"),windows::core::w!("Namespace hierarchy"),
            WS_POPUP,0,0,320,400,None,None,None,None)?});
        let tree:INameSpaceTreeControl=unsafe{CoCreateInstance(&NamespaceTreeControl,None,CLSCTX_INPROC_SERVER)?};
        unsafe{tree.Initialize(owner.0,&RECT{left:0,top:0,right:300,bottom:380},(NSTCS_HASEXPANDOS.0|NSTCS_ROOTHASEXPANDO.0)as u32)?;}
        let items=[&root,&server,&share,&child].map(|path|unsafe{shell_item(&ShellLocation::Path(path.clone()))}).into_iter().collect::<Result<Vec<_>>>()?;
        let filter:IShellItemFilter=FolderFilter.into();
        unsafe{tree.AppendRoot(&items[0],navigation_enum_flags(),NSTCRS_VISIBLE.0 as u32,&filter)?;}
        for pair in items.windows(2){
            unsafe{tree.SetItemState(&pair[0],NSTCIS_EXPANDED.0 as u32,NSTCIS_EXPANDED.0 as u32)?;}
            let deadline=Instant::now()+Duration::from_secs(3);
            loop{
                let mut message=MSG::default();while unsafe{PeekMessageW(&mut message,None,0,0,PM_REMOVE)}.as_bool(){
                    unsafe{let _=TranslateMessage(&message);DispatchMessageW(&message);}
                }
                if unsafe{tree.GetItemRect(&pair[1])}.is_ok(){break;}
                assert!(Instant::now()<deadline,"folder expansion did not expose its child");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        assert!(!unsafe{IsWindowVisible(owner.0)}.as_bool(),"hierarchy probe became visible");
        unsafe{tree.RemoveAllRoots()?;}drop(tree);drop(items);drop(owner);
        Ok(())
    }

    #[test]
    fn initial_root_follows_explicit_location() {
        assert_eq!(initial_root(&ShellLocation::ThisPc),InitialRoot::ThisPc);
        assert_eq!(initial_root(&ShellLocation::Path(r"C:\\".into())),InitialRoot::ThisPc);
        for name in [QUICK_ACCESS,"SHELL:Home","shell:::{F874310E-B6B7-47DC-BC84-B9E6B38F5903}"] {
            assert_eq!(initial_root(&ShellLocation::Shell{pidl:None, parsing_name:name.into(),title:String::new()}),InitialRoot::QuickAccess);
        }
        let recycle=ShellLocation::Shell{pidl:None, parsing_name:"shell:RecycleBinFolder".into(),title:String::new()};
        assert_eq!(initial_root(&recycle),InitialRoot::Neither);
        assert_eq!(root_style(InitialRoot::QuickAccess,InitialRoot::ThisPc)&NSTCRS_EXPANDED.0 as u32,0);
        assert_ne!(root_style(InitialRoot::ThisPc,InitialRoot::ThisPc)&NSTCRS_EXPANDED.0 as u32,0);
    }

    #[test]
    fn explorer_launch_preference_maps_this_pc_without_registry_writes() {
        assert_eq!(launch_location(Some(1)),ShellLocation::ThisPc);
        for value in [None,Some(0),Some(2),Some(3),Some(u32::MAX)] {
            assert_eq!(initial_root(&launch_location(value)),InitialRoot::QuickAccess);
        }
    }

    #[test]
    fn entire_selected_row_opens_the_folder() {
        for hit in [NSTCEHT_ONITEMICON, NSTCEHT_ONITEMLABEL, NSTCEHT_ONITEMSTATEICON, NSTCEHT_ONITEMRIGHT] {
            assert_eq!(navigation_click(hit.0 as u32, NSTCECT_LBUTTON.0 as u32), Some(false));
            assert_eq!(navigation_click(hit.0 as u32, NSTCECT_MBUTTON.0 as u32), Some(true));
        }
    }

    #[test]
    fn expansion_and_indent_never_navigate() {
        for hit in [NSTCEHT_ONITEMBUTTON.0, NSTCEHT_ONITEMINDENT.0, NSTCEHT_NOWHERE.0,
            NSTCEHT_ONITEMBUTTON.0 | NSTCEHT_ONITEMLABEL.0,
            NSTCEHT_ONITEMINDENT.0 | NSTCEHT_ONITEMRIGHT.0] {
            for button in [NSTCECT_LBUTTON.0, NSTCECT_MBUTTON.0, NSTCECT_RBUTTON.0] {
                assert_eq!(navigation_click(hit as u32, button as u32), None);
            }
        }
    }

    #[test]
    fn double_click_flag_preserves_button_identity() {
        let hit = NSTCEHT_ONITEMRIGHT.0 as u32;
        assert_eq!(navigation_click(hit, (NSTCECT_LBUTTON.0 | NSTCECT_DBLCLICK.0) as u32), Some(false));
        assert_eq!(navigation_click(hit, (NSTCECT_MBUTTON.0 | NSTCECT_DBLCLICK.0) as u32), Some(true));
        assert_eq!(navigation_click(hit, (NSTCECT_RBUTTON.0 | NSTCECT_DBLCLICK.0) as u32), None);
        assert_eq!(navigation_click(hit, NSTCECT_RBUTTON.0 as u32), None);
        assert_eq!(navigation_click(hit, 0), None);
    }

    #[test]
    fn navigation_density_adapts_to_os_dpi_compact_mode_and_large_text() {
        for build in [0, 7601, 19045] {
            assert_eq!(navigation_row_height(build, 96, false, 12), None);
            assert_eq!(navigation_row_height(build, 192, true, 40), None);
        }
        for build in [22000, 22631, 26100] {
            for (dpi, normal, compact) in [(96, 32, 24), (120, 40, 30), (144, 48, 36), (192, 64, 48)] {
                let font = scale(12, dpi);
                assert_eq!(navigation_row_height(build, dpi, false, font), Some(normal));
                assert_eq!(navigation_row_height(build, dpi, true, font), Some(compact));
            }
        }
        assert_eq!(navigation_row_height(22631, 96, false, 45), Some(58));
        assert_eq!(navigation_row_height(22631, 96, true, 45), Some(54));
        assert_eq!(navigation_row_height(22631, 0, false, 0), Some(32));
    }

    #[test]
    #[ignore="Hidden native Shell metric comparison; no desktop input"]
    fn native_navigation_metrics_follow_display_policy()->std::result::Result<(),Box<dyn std::error::Error>> {
        use windows::Win32::{System::Ole::{OleInitialize,OleUninitialize},UI::{WindowsAndMessaging::*,
            Controls::{TVM_GETITEMHEIGHT,TVM_GETINDENT},Shell::NSTCS2_DISPLAYPADDING}};
        unsafe{OleInitialize(None)?;}
        struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}let _apartment=Apartment;
        struct Window(HWND);impl Drop for Window{fn drop(&mut self){let _=unsafe{DestroyWindow(self.0)};}}
        let owner=Window(unsafe{CreateWindowExW(WS_EX_NOACTIVATE,windows::core::w!("STATIC"),windows::core::w!("Tree metrics"),WS_POPUP,0,0,600,500,None,None,None,None)?});
        let rect=RECT{left:0,top:0,right:250,bottom:480};
        let native:INameSpaceTreeControl=unsafe{CoCreateInstance(&NamespaceTreeControl,None,CLSCTX_INPROC_SERVER)?};
        unsafe{native.Initialize(owner.0,&rect,(NSTCS_HASEXPANDOS.0|NSTCS_ROOTHASEXPANDO.0|NSTCS_FULLROWSELECT.0|NSTCS_SHOWSELECTIONALWAYS.0
            |NSTCS_AUTOHSCROLL.0|NSTCS_ALLOWJUNCTIONS.0|NSTCS_NOEDITLABELS.0|NSTCS_FADEINOUTEXPANDOS.0|NSTCS_TABSTOP.0)as u32)?;}
        let raw=unsafe{native.cast::<IOleWindow>()?.GetWindow()?};let mut baseline=HWND::default();
        let _=unsafe{EnumChildWindows(Some(raw),Some(find_tree_view),LPARAM((&mut baseline as*mut HWND)as isize))};
        assert!(!baseline.0.is_null());
        let host=unsafe{NamespaceTreeHost::create_for_location(owner.0,rect,&ShellLocation::ThisPc)?};
        let actual=host.tree_view().ok_or("native tree window")?;
        unsafe extern "system" fn count_layout(window:HWND,message:u32,wp:WPARAM,lp:LPARAM,_:usize,data:usize)->LRESULT{
            if message==WM_WINDOWPOSCHANGED||message==TVM_SETITEMHEIGHT{
                let count=unsafe{&*(data as*const Cell<u32>)};count.set(count.get()+1);
            }
            unsafe{DefSubclassProc(window,message,wp,lp)}
        }
        let changes=Cell::new(0u32);let data=(&changes as*const Cell<u32>)as usize;
        let probe=0x5a53_5245;
        unsafe{SetWindowSubclass(host.hwnd,Some(count_layout),probe,data).ok()?;
            SetWindowSubclass(actual,Some(count_layout),probe,data).ok()?;}
        for _ in 0..16{unsafe{host.resize(rect);}}
        unsafe{RemoveWindowSubclass(host.hwnd,Some(count_layout),probe);
            RemoveWindowSubclass(actual,Some(count_layout),probe);}
        assert_eq!(changes.get(),0,"unchanged layout forced a native tree repaint");
        let metrics=|hwnd|[TVM_GETITEMHEIGHT,TVM_GETINDENT].map(|message|unsafe{SendMessageW(hwnd,message,None,None)}.0);
        let actual_metrics=metrics(actual);
        assert_eq!(actual_metrics[1],metrics(baseline)[1],"application replaced Shell indentation");
        if crate::theme::os_build()<22000 {
            assert_eq!(actual_metrics[0],metrics(baseline)[0],"legacy Shell height was changed");
        } else {
            assert!(actual_metrics[0]>=scale(24,unsafe{GetDpiForWindow(actual)})as isize,"Windows 11 reverted to bare TreeView density");
        }
        let padding=unsafe{host.tree.cast::<INameSpaceTreeControl2>()?.GetControlStyle2(NSTCS2_DISPLAYPADDING)?};
        if crate::theme::os_build()>=22000 {assert_eq!(padding,NSTCS2_DISPLAYPADDING,"Shell root separation was not enabled");}
        else {assert_eq!(padding,unsafe{native.cast::<INameSpaceTreeControl2>()?.GetControlStyle2(NSTCS2_DISPLAYPADDING)?},"legacy Shell root padding was changed");}
        for dark in [false,true,false] {
            crate::theme::theme_control(raw,dark);crate::theme::theme_control(baseline,dark);
            host.apply_theme(dark,0xffffff,0);unsafe{host.resize(rect);}
            for hwnd in [baseline,actual] {unsafe{SendMessageW(hwnd,WM_THEMECHANGED,None,None);}}
            assert_eq!(metrics(actual),actual_metrics,"theme/resize lost navigation density or indentation");
        }
        eprintln!("native tree dpi={} height/indent={:?}",unsafe{GetDpiForWindow(actual)},metrics(actual));
        drop(host);drop(native);Ok(())
    }

    #[test]
    #[ignore="Local Shell focus/expansion regression; requires ZSDESK_NATIVE_TEST_OUTPUT"]
    fn navigation_sync_preserves_focus_and_expansion_policy()->std::result::Result<(),Box<dyn std::error::Error>> {
        use windows::Win32::{Foundation::{HINSTANCE,LRESULT}, System::{LibraryLoader::GetModuleHandleW,Ole::{OleInitialize,OleUninitialize}},UI::WindowsAndMessaging::*};
        use std::{path::PathBuf,time::{Duration,Instant},thread};
        thread_local! {static NAVIGATIONS:Cell<u32>=const {Cell::new(0)};}
        unsafe extern "system" fn owner_proc(window:HWND,message:u32,wp:WPARAM,lp:LPARAM)->LRESULT {
            if message==WM_TREE_NAVIGATE {
                if lp.0!=0 {unsafe {drop(Box::from_raw(lp.0 as*mut ShellLocation));}}
                NAVIGATIONS.with(|n|n.set(n.get()+1));return LRESULT(0);
            }
            unsafe {DefWindowProcW(window,message,wp,lp)}
        }
        fn pump_for(milliseconds:u64){let start=Instant::now();while start.elapsed()<Duration::from_millis(milliseconds){let mut message=MSG::default();while unsafe{PeekMessageW(&mut message,None,0,0,PM_REMOVE)}.as_bool(){unsafe{let _=TranslateMessage(&message);DispatchMessageW(&message);}}thread::sleep(Duration::from_millis(4));}}
        fn pump(){pump_for(120);}
        fn trace(host:&NamespaceTreeHost,stage:&str,items:&[(&str,&IShellItem)]) {
            let selected=unsafe {host.tree.GetSelectedItems()}.ok().map(|items|(0..unsafe {items.GetCount()}.unwrap_or(0))
                .filter_map(|index|unsafe {items.GetItemAt(index)}.ok()).filter_map(|item|unsafe {location_from_shell_item(&item)}).collect::<Vec<_>>());
            eprintln!("tree-sync stage={stage} selected={selected:?} pending={}",host.pending_sync.request.borrow().is_some());
            for (label,item) in items {eprintln!("tree-sync stage={stage} item={label} state={:?} rect={:?} exposed={}",
                unsafe {host.tree.GetItemState(*item,(NSTCIS_EXPANDED.0|NSTCIS_SELECTED.0|NSTCIS_SELECTEDNOEXPAND.0)as u32)},
                unsafe {host.tree.GetItemRect(*item)},unsafe {host.item_is_exposed(item)});}
        }
        struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}
        struct Window(HWND);impl Drop for Window{fn drop(&mut self){let _=unsafe{DestroyWindow(self.0)};}}
        let output=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").ok_or("set native test output")?);
        assert!(output.is_absolute(),"native test output must be absolute");
        let fixture=output.join("tree-sync-fixture");let branch=fixture.join("branch");let target=branch.join("target");
        let target_child=target.join("child");let unrelated=fixture.join("unrelated");let unrelated_child=unrelated.join("child");
        std::fs::create_dir_all(&target_child)?;std::fs::create_dir_all(&unrelated_child)?;
        struct Dirs(Vec<PathBuf>);impl Drop for Dirs{fn drop(&mut self){for directory in self.0.iter().rev(){let _=std::fs::remove_dir(directory);}}}
        let _directories=Dirs(vec![fixture.clone(),branch.clone(),target.clone(),target_child,unrelated.clone(),unrelated_child]);
        let foreground=unsafe{GetForegroundWindow()};
        unsafe{OleInitialize(None)?;}let apartment=Apartment;
        let instance=HINSTANCE(unsafe{GetModuleHandleW(None)?}.0);
        let class=windows::core::w!("ZSDesk.Tree.SyncRegression");
        let wc=WNDCLASSW{lpfnWndProc:Some(owner_proc),hInstance:instance,lpszClassName:class,..Default::default()};unsafe{RegisterClassW(&wc);}
        let x=unsafe{GetSystemMetrics(SM_XVIRTUALSCREEN)+GetSystemMetrics(SM_CXVIRTUALSCREEN)}+1000;
        let y=unsafe{GetSystemMetrics(SM_YVIRTUALSCREEN)+GetSystemMetrics(SM_CYVIRTUALSCREEN)}+1000;
        let owner=Window(unsafe{CreateWindowExW(WS_EX_TOOLWINDOW|WS_EX_NOACTIVATE,class,windows::core::w!("tree sync"),WS_POPUP,x,y,520,600,None,None,Some(instance),None)?});
        let host=unsafe{NamespaceTreeHost::create_for_location(owner.0,RECT{left:0,top:0,right:280,bottom:570},&ShellLocation::ThisPc)?};
        pump();
        let pc_item=unsafe {shell_item(&ShellLocation::ThisPc)?};
        assert!(unsafe {host.tree.GetItemState(&pc_item,NSTCIS_EXPANDED.0 as u32)?}&NSTCIS_EXPANDED.0 as u32!=0,
            "This PC startup did not expand the This PC root");
        if let Ok(quick)=unsafe {shell_item(&launch_location(Some(2)))} {
            if let Ok(state)=unsafe {host.tree.GetItemState(&quick,NSTCIS_EXPANDED.0 as u32)} {
                assert_eq!(state&NSTCIS_EXPANDED.0 as u32,0,"This PC startup expanded Quick access");
            }
        }
        unsafe {host.tree.SetItemState(&pc_item,NSTCIS_EXPANDED.0 as u32,0)?;
            host.select_location_with_policy(&ShellLocation::ThisPc,false);}
        pump();
        assert_eq!(unsafe {host.tree.GetItemState(&pc_item,NSTCIS_EXPANDED.0 as u32)?}&NSTCIS_EXPANDED.0 as u32,0,
            "later navigation reset a manually collapsed startup root");
        unsafe{host.tree.RemoveAllRoots()?;}
        let root_item=unsafe{shell_item(&ShellLocation::Path(fixture.clone()))?};
        unsafe{host.tree.AppendRoot(&root_item,SHCONTF_FOLDERS.0 as u32,NSTCRS_VISIBLE.0 as u32,&host._filter)?;}
        let edit=unsafe{CreateWindowExW(WINDOW_EX_STYLE::default(),windows::core::w!("EDIT"),windows::core::w!("focus stays here"),WS_CHILD|WS_VISIBLE,300,20,200,24,Some(owner.0),None,Some(instance),None)?};
        unsafe{let _=ShowWindow(owner.0,SW_SHOWNOACTIVATE);let _=SetFocus(Some(edit));}pump();
        assert!(unsafe{GetFocus()}==edit,"owned edit did not receive focus");
        NAVIGATIONS.with(|n|n.set(0));
        let location=ShellLocation::Path(target.clone());
        let branch_item=unsafe{shell_item(&ShellLocation::Path(branch.clone()))?};
        let target_item=unsafe{shell_item(&location)?};
        let unrelated_item=unsafe{shell_item(&ShellLocation::Path(unrelated))?};
        let trace_items=[("root",&root_item),("branch",&branch_item),("target",&target_item),("unrelated",&unrelated_item)];
        trace(&host,"before-default",&trace_items);
        unsafe{host.select_location_with_policy(&location,false);}pump();
        trace(&host,"after-default",&trace_items);
        assert!(unsafe{host.tree.GetItemState(&root_item,NSTCIS_EXPANDED.0 as u32)?}&NSTCIS_EXPANDED.0 as u32==0,"default synchronization expanded a branch");
        assert!(unsafe{host.tree.GetItemState(&root_item,NSTCIS_SELECTED.0 as u32)?}&NSTCIS_SELECTED.0 as u32==0,"outside navigation selected an unrelated ancestor");
        assert!(unsafe{GetFocus()}==edit,"default synchronization stole focus");
        unsafe{host.select_location_with_policy(&location,true);}
        trace(&host,"auto-immediate",&trace_items);
        let started=Instant::now();while host.pending_sync.request.borrow().is_some() && started.elapsed()<Duration::from_secs(3) {pump();}
        trace(&host,"auto-pumped",&trace_items);
        let selected_by_production=unsafe{host.tree.GetItemState(&target_item,NSTCIS_SELECTED.0 as u32)?}&NSTCIS_SELECTED.0 as u32!=0;
        assert!(selected_by_production,"explicit auto expansion did not select its destination");
        assert!(host.pending_sync.request.borrow().is_none(),"successful synchronization retained a retry");
        assert!(unsafe{host.tree.GetItemState(&target_item,NSTCIS_EXPANDED.0 as u32)?}&NSTCIS_EXPANDED.0 as u32==0,"synchronization expanded the destination itself");
        assert!(unsafe{host.tree.GetItemState(&unrelated_item,NSTCIS_EXPANDED.0 as u32)?}&NSTCIS_EXPANDED.0 as u32==0,"synchronization expanded an unrelated branch");
        assert!(unsafe{GetFocus()}==edit,"expanded synchronization stole focus");
        assert!(NAVIGATIONS.with(Cell::get)==0,"programmatic synchronization navigated the right pane");
        let fresh=||->Result<()> {host.pending_sync.cancel();unsafe {host.tree.RemoveAllRoots()?;
            host.tree.AppendRoot(&root_item,SHCONTF_FOLDERS.0 as u32,NSTCRS_VISIBLE.0 as u32,&host._filter)?;}
            *host.last_location.borrow_mut()=None;pump();unsafe {host.select_location_with_policy(&location,true);}
            assert!(host.pending_sync.request.borrow().is_some(),"cancellation fixture did not enter asynchronous loading");Ok(())};
        fresh()?;
        let _=unsafe {host.events.OnItemClick(&root_item,NSTCEHT_ONITEMBUTTON.0 as u32,NSTCECT_LBUTTON.0 as u32)};
        pump();trace(&host,"cancel-click",&trace_items);
        assert!(host.pending_sync.request.borrow().is_none(),"user tree click retained a pending sync");
        assert!(unsafe {host.tree.GetItemState(&target_item,NSTCIS_SELECTED.0 as u32)}.unwrap_or(0)&NSTCIS_SELECTED.0 as u32==0,"click cancellation later selected the old destination");
        fresh()?;
        let _=unsafe {host.events.OnKeyboardInput(WM_KEYDOWN,WPARAM(0x28),LPARAM(0))};
        pump();assert!(host.pending_sync.request.borrow().is_none(),"keyboard input retained a pending sync");
        fresh()?;
        let started=Instant::now();while unsafe {host.tree.GetItemState(&root_item,NSTCIS_EXPANDED.0 as u32)}.unwrap_or(0)&NSTCIS_EXPANDED.0 as u32==0
            && started.elapsed()<Duration::from_secs(2) {pump_for(5);}
        assert!(host.pending_sync.request.borrow().is_some(),"collapse fixture finished before user cancellation");
        unsafe {host.tree.SetItemState(&root_item,NSTCIS_EXPANDED.0 as u32,0)?;}
        pump();trace(&host,"cancel-collapse",&trace_items);
        assert!(host.pending_sync.request.borrow().is_none(),"user collapse retained a pending sync");
        assert!(unsafe {host.tree.GetItemState(&root_item,NSTCIS_EXPANDED.0 as u32)?}&NSTCIS_EXPANDED.0 as u32==0,"cancelled sync reopened the collapsed root");
        assert!(unsafe {GetFocus()}==edit,"cancelled sync stole focus");
        assert!(NAVIGATIONS.with(Cell::get)==0,"structural cancellation navigated the right pane");
        fresh()?;let tree_hwnd=host.hwnd.0 as isize;
        drop(host);assert!(PENDING_SYNCS.with(|pending|!pending.borrow().contains_key(&tree_hwnd)),"destroyed tree retained its retry route");
        drop(owner);pump();drop(apartment);
        assert!(foreground==unsafe{GetForegroundWindow()},"foreground changed during tree regression");
        Ok(())
    }
}
