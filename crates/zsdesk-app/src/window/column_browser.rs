//! Linked parent listing for the three-column layout. The centre remains the normal Shell view.
use super::*;
use std::path::Path;
use std::{
    io,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Arc,
    },
    thread,
};
use windows::Win32::Graphics::Gdi::{FillRect, SelectObject};
use windows::Win32::UI::Shell::IShellView;
use windows::Win32::{
    Graphics::Gdi::{
        CreateSolidBrush, DrawFocusRect, DrawTextW, SetBkMode, DT_END_ELLIPSIS, DT_NOPREFIX,
        DT_SINGLELINE, DT_VCENTER, TRANSPARENT,
    },
    Storage::FileSystem::GetLogicalDrives,
    UI::Shell::{
        SHGetStockIconInfo, SHGSI_ICON, SHGSI_SMALLICON, SHSTOCKICONINFO, SIID_DOCNOASSOC,
        SIID_FOLDER,
    },
};
use zsdesk_core::model::explorer::FolderLayout;
use zsdesk_core::model::explorer::{ShellViewState, ViewColumn, ViewPropertyKey};

pub(super) const READY: u32 = WM_APP + 185;
pub(super) const CHANGED: u32 = WM_APP + 186;
pub(super) const RELOAD: usize = 0x5ec8;
const ID: usize = 1085;
use super::folder_contents::{self, Entry, Visibility};
type Reply = (u64, io::Result<Vec<Entry>>);
struct Loader {
    requests: mpsc::Sender<(u64, Option<PathBuf>, Visibility)>,
    replies: mpsc::Receiver<Reply>,
    latest: Arc<AtomicU64>,
    worker: thread::JoinHandle<()>,
}
impl Drop for Loader {
    fn drop(&mut self) {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::{Foundation::HANDLE, System::IO::CancelSynchronousIo};
        self.latest.fetch_add(1, Ordering::Release);
        // Interrupt a pending directory read. Dropping the sender also wakes an idle worker.
        let _ = unsafe { CancelSynchronousIo(HANDLE(self.worker.as_raw_handle())) };
    }
}
impl Loader {
    fn new(owner: HWND) -> io::Result<Self> {
        let (requests, rx) = mpsc::channel::<(u64, Option<PathBuf>, Visibility)>();
        let (tx, replies) = mpsc::channel();
        let latest = Arc::new(AtomicU64::new(0));
        let revision = latest.clone();
        let window = owner.0 as isize;
        let worker = thread::Builder::new()
            .name("zsdesk-parent-column".into())
            .spawn(move || {
                while let Ok(first) = rx.recv() {
                    let (id, parent, hidden) = rx.try_iter().last().unwrap_or(first);
                    if revision.load(Ordering::Acquire) != id {
                        continue;
                    }
                    let load = || -> io::Result<Vec<Entry>> {
                        let mut rows = Vec::new();
                        if let Some(path) = parent {
                            return folder_contents::entries(&path, hidden, usize::MAX, || {
                                revision.load(Ordering::Acquire) != id
                            })
                            .map(|(rows, _)| rows);
                        } else {
                            let mask = unsafe { GetLogicalDrives() };
                            for i in 0..26 {
                                if mask & (1 << i) != 0 {
                                    rows.push(Entry {
                                        name: format!("{}:\\", char::from(b'A' + i)).into(),
                                        folder: true,
                                    });
                                }
                            }
                        }
                        Ok(rows)
                    };
                    let result = load();
                    if revision.load(Ordering::Acquire) == id {
                        if tx.send((id, result)).is_err() {
                            break;
                        }
                        let _ = unsafe {
                            PostMessageW(Some(HWND(window as *mut _)), READY, WPARAM(0), LPARAM(0))
                        };
                    }
                }
            })?;
        Ok(Self {
            requests,
            replies,
            latest,
            worker,
        })
    }
}
#[derive(Default)]
pub(super) struct ParentColumn {
    pub active: bool,
    pub bounds: LayoutRect,
    pub preview_file: Option<PathBuf>,
    placed: Option<(LayoutRect, HFONT, u32)>,
    watch: Option<zsdesk_windows::shell::notify::ChangeNotify>,
    middle_layout: Option<(IShellView, ShellViewState)>,
    middle_width: Option<u32>,
    list: HWND,
    loader: Option<Loader>,
    parent: Option<PathBuf>,
    requested: bool,
    rows: Vec<Entry>,
    icons: [HICON; 2],
}
impl Drop for ParentColumn {
    fn drop(&mut self) {
        for icon in self.icons {
            if !icon.is_invalid() {
                let _ = unsafe { DestroyIcon(icon) };
            }
        }
    }
}
pub(super) fn column_rects(area: LayoutRect, gap: i32) -> (LayoutRect, LayoutRect, LayoutRect) {
    let gap = gap.min(area.width / 2);
    let space = area.width - 2 * gap;
    let left = space / 5;
    let preview = space * 27 / 100;
    let middle = space - left - preview;
    (
        LayoutRect::new(area.x, area.y, left, area.height),
        LayoutRect::new(area.x + left + gap, area.y, middle, area.height),
        LayoutRect::new(area.right() - preview, area.y, preview, area.height),
    )
}
impl ParentColumn {
    fn create(&mut self, owner: HWND) -> anyhow::Result<()> {
        if !self.list.is_invalid() {
            return Ok(());
        }
        let loader = Loader::new(owner)?;
        self.list = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                windows::core::w!("LISTBOX"),
                None,
                WS_CHILD
                    | WS_VSCROLL
                    | WS_TABSTOP
                    | WINDOW_STYLE(
                        (LBS_NOTIFY | LBS_OWNERDRAWFIXED | LBS_NODATA | LBS_NOINTEGRALHEIGHT)
                            as u32,
                    ),
                0,
                0,
                0,
                0,
                Some(owner),
                Some(HMENU(ID as *mut _)),
                None,
                None,
            )
        }?;
        self.loader = Some(loader);
        for (slot, id) in self.icons.iter_mut().zip([SIID_DOCNOASSOC, SIID_FOLDER]) {
            let mut info = SHSTOCKICONINFO {
                cbSize: std::mem::size_of::<SHSTOCKICONINFO>() as u32,
                ..Default::default()
            };
            if unsafe { SHGetStockIconInfo(id, SHGSI_ICON | SHGSI_SMALLICON, &mut info) }.is_ok() {
                *slot = info.hIcon;
            }
        }
        Ok(())
    }
    pub(super) fn fit_middle(&mut self, width: i32) {
        use windows::{
            core::Interface,
            Win32::UI::Shell::{IColumnManager, CM_COLUMNINFO, CM_MASK_WIDTH},
        };
        let width = width.clamp(48, 16384) as u32;
        if self.middle_width == Some(width) {
            return;
        }
        if let Some((view, _)) = &self.middle_layout {
            if let Ok(manager) = view.cast::<IColumnManager>() {
                let info = CM_COLUMNINFO {
                    cbSize: std::mem::size_of::<CM_COLUMNINFO>() as u32,
                    dwMask: CM_MASK_WIDTH.0 as u32,
                    uWidth: width,
                    ..Default::default()
                };
                if unsafe {
                    manager.SetColumnInfo(&zsdesk_windows::shell::commands::PKEY_NAME, &info)
                }
                .is_ok()
                {
                    self.middle_width = Some(width);
                }
            }
        }
    }
    pub(super) fn release_middle(&mut self) {
        self.middle_width = None;
        if let Some((view, original)) = self.middle_layout.take() {
            zsdesk_windows::shell::restore_view_layout(&view, &original);
        }
    }
    pub(super) fn preserve_tab_view(&self, view: &mut ShellViewState) {
        if let Some((_, original)) = &self.middle_layout {
            view.mode = original.mode;
            view.icon_size = original.icon_size;
            view.columns.clone_from(&original.columns);
            view.column_dpi = original.column_dpi;
        }
    }
    fn clear(&mut self) {
        self.release_middle();
        self.active = false;
        self.preview_file = None;
        self.requested = false;
        self.parent = None;
        self.rows = Vec::new();
        self.placed = None;
        self.watch = None;
        self.loader = None;
        if !self.list.is_invalid() {
            unsafe {
                let _ = DestroyWindow(self.list);
            }
            self.list = HWND::default();
        }
        for icon in &mut self.icons {
            if !icon.is_invalid() {
                let _ = unsafe { DestroyIcon(*icon) };
                *icon = HICON::default();
            }
        }
    }
    fn select_current(&self, path: &Path) {
        let name = path.file_name().unwrap_or(path.as_os_str());
        let row = self.rows.iter().position(|e| e.name == name);
        unsafe {
            SendMessageW(
                self.list,
                LB_SETCURSEL,
                Some(WPARAM(row.unwrap_or(usize::MAX))),
                None,
            );
        }
    }
    pub(super) fn invalidate(&mut self) {
        self.requested = false;
    }
    fn sync(&mut self, path: Option<&Path>) {
        let parent = path.and_then(Path::parent).map(Path::to_path_buf);
        if self.requested && self.parent == parent {
            if let Some(path) = path {
                self.select_current(path);
            } else {
                unsafe {
                    SendMessageW(self.list, LB_SETCURSEL, Some(WPARAM(usize::MAX)), None);
                }
            }
            return;
        }
        let Some(loader) = &self.loader else { return };
        let id = loader.latest.fetch_add(1, Ordering::AcqRel) + 1;
        if self.parent != parent || self.watch.is_none() {
            let folder = parent
                .clone()
                .map_or(ShellLocation::ThisPc, ShellLocation::Path);
            self.watch = zsdesk_windows::shell::pidl::location_pidl(&folder).and_then(|id| {
                zsdesk_windows::shell::notify::ChangeNotify::register(
                    unsafe { GetParent(self.list) }.ok()?,
                    CHANGED,
                    &[id],
                    false,
                )
            });
        }
        self.parent = parent.clone();
        self.requested = true;
        self.rows = Vec::new();
        self.preview_file = None;
        unsafe {
            SendMessageW(self.list, LB_SETCOUNT, Some(WPARAM(0)), None);
        }
        let _ = loader.requests.send((id, parent, Visibility::current()));
    }
    pub(super) fn position(&mut self, font: HFONT, dpi: u32) {
        if !self.active || self.list.is_invalid() {
            return;
        }
        let r = self.bounds;
        if self.placed == Some((r, font, dpi)) {
            return;
        }
        self.placed = Some((r, font, dpi));
        unsafe {
            SendMessageW(self.list, WM_SETFONT, Some(WPARAM(font.0 as usize)), None);
            SendMessageW(
                self.list,
                LB_SETITEMHEIGHT,
                Some(WPARAM(0)),
                Some(LPARAM(scale(26, dpi) as isize)),
            );
            let _ = SetWindowPos(
                self.list,
                None,
                r.x,
                r.y,
                r.width,
                r.height,
                SWP_NOACTIVATE | SWP_NOZORDER | SWP_SHOWWINDOW,
            );
        }
    }
    pub(super) fn is_selection(&self, wp: WPARAM, lp: LPARAM) -> bool {
        self.active
            && lp.0 == self.list.0 as isize
            && wp.0 & 0xffff == ID
            && wp.0 >> 16 == LBN_SELCHANGE as usize
    }
    pub(super) fn owns(&self, hwnd: HWND) -> bool {
        self.active && hwnd == self.list
    }
    pub(super) fn draw(&self, lp: LPARAM, font: HFONT, p: &Palette, dpi: u32) -> bool {
        if !self.active || lp.0 == 0 {
            return false;
        }
        let d = unsafe { &*(lp.0 as *const DRAWITEMSTRUCT) };
        if d.hwndItem != self.list {
            return false;
        }
        let selected = d.itemState.0 & ODS_SELECTED.0 != 0;
        use windows::Win32::Graphics::Gdi::{GetSysColor, COLOR_HIGHLIGHT, COLOR_HIGHLIGHTTEXT};
        let bg = if selected {
            unsafe { GetSysColor(COLOR_HIGHLIGHT) }
        } else {
            colorref(p.content)
        };
        let fg = if selected {
            unsafe { GetSysColor(COLOR_HIGHLIGHTTEXT) }
        } else {
            colorref(p.text)
        };
        let brush = unsafe { CreateSolidBrush(COLORREF(bg)) };
        unsafe {
            FillRect(d.hDC, &d.rcItem, brush);
            let _ = DeleteObject(brush.into());
        }
        if let Some(row) = self.rows.get(d.itemID as usize) {
            let sz = scale(16, dpi);
            let mut r = d.rcItem;
            r.left += scale(6, dpi);
            let icon = self.icons[row.folder as usize];
            if !icon.is_invalid() {
                let _ = unsafe {
                    DrawIconEx(
                        d.hDC,
                        r.left,
                        r.top + (r.bottom - r.top - sz) / 2,
                        icon,
                        sz,
                        sz,
                        0,
                        None,
                        DI_NORMAL,
                    )
                };
            }
            r.left += sz + scale(6, dpi);
            r.right -= scale(4, dpi);
            use std::os::windows::ffi::OsStrExt;
            let mut label: Vec<_> = row.name.encode_wide().collect();
            unsafe {
                let old = SelectObject(d.hDC, font.into());
                SetBkMode(d.hDC, TRANSPARENT);
                SetTextColor(d.hDC, COLORREF(fg));
                if row.folder {
                    let mut mark = [0x203a_u16];
                    let mut end = r;
                    end.left = (end.right - scale(13, dpi)).max(end.left);
                    r.right = end.left;
                    DrawTextW(
                        d.hDC,
                        &mut mark,
                        &mut end,
                        DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
                    );
                }
                DrawTextW(
                    d.hDC,
                    &mut label,
                    &mut r,
                    DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX,
                );
                SelectObject(d.hDC, old);
                if d.itemState.0 & ODS_FOCUS.0 != 0 {
                    let _ = DrawFocusRect(d.hDC, &d.rcItem);
                }
            }
        }
        true
    }
}
fn remembered(
    map: &std::collections::BTreeMap<PathBuf, FolderLayout>,
    path: &Path,
) -> Option<FolderLayout> {
    map.get(path).copied().or_else(|| {
        map.iter()
            .find(|(key, _)| crate::explorer_forwarder::same_path(key, path))
            .map(|(_, v)| *v)
    })
}
impl WindowState {
    fn current_layout(&self) -> FolderLayout {
        if self.columns.active {
            FolderLayout::Columns
        } else if self.session.ui.dual_pane {
            FolderLayout::Dual
        } else {
            FolderLayout::Single
        }
    }
    pub(super) fn toggle_columns(&mut self) {
        self.choose_folder_layout(if self.columns.active {
            FolderLayout::Single
        } else {
            FolderLayout::Columns
        });
    }
    pub(super) fn choose_folder_layout(&mut self, layout: FolderLayout) {
        if self.picker.is_some() || self.pending.is_some() || self.queued.is_some() {
            return;
        }
        let path = self.current_location().as_path().map(Path::to_path_buf);
        if layout == FolderLayout::Columns
            && path.is_none()
            && !matches!(self.current_location(), ShellLocation::ThisPc)
        {
            self.show_message(zsdesk_core::i18n::text("请在文件夹中开启多栏显示").into());
            return;
        }
        if !self.apply_folder_layout(layout) {
            return;
        }
        if let Some(path) = path {
            match storage::update_shared_ui(&self.config_path, |ui| {
                let key = if ui.folder_layouts.contains_key(&path) {
                    path
                } else {
                    ui.folder_layouts
                        .keys()
                        .find(|p| crate::explorer_forwarder::same_path(p, &path))
                        .cloned()
                        .unwrap_or(path)
                };
                ui.folder_layouts.insert(key, layout);
                ui.folder_layouts.clone()
            }) {
                Ok(map) => self.session.ui.folder_layouts = map,
                Err(e) => self.show_message(e.to_string()),
            }
        }
        self.save();
    }
    fn apply_folder_layout(&mut self, layout: FolderLayout) -> bool {
        if self.current_layout() != layout {
            if layout == FolderLayout::Columns {
                if let Err(e) = self.columns.create(self.hwnd) {
                    self.show_message(e.to_string());
                    return false;
                }
            }
            let changing_columns = self.columns.active || layout == FolderLayout::Columns;
            if self.columns.active {
                self.columns.clear();
            }
            if self.session.ui.dual_pane {
                // Keep the active folder as the primary tab when collapsing either side.
                self.active_pane_right = false;
                self.session.ui.dual_pane = false;
                self.ensure_dual_pane();
            }
            self.columns.active = layout == FolderLayout::Columns;
            if changing_columns && self.session.ui.show_details {
                self.set_panes_with_rebuild(
                    self.session.ui.show_preview,
                    self.session.ui.show_details,
                    true,
                );
            }
            if layout == FolderLayout::Dual {
                self.session.ui.dual_pane = true;
                self.ensure_dual_pane();
            }
            self.relayout();
            self.position_children();
            self.invalidate();
            let _ = unsafe { PostMessageW(Some(self.hwnd), WM_LAYOUT, WPARAM(0), LPARAM(0)) };
        }
        if self.columns.active {
            self.refresh_parent_column();
            if self.columns.middle_layout.is_none() {
                if let Some(view) = self
                    .browser
                    .as_ref()
                    .and_then(|b| unsafe { b.current_view() }.ok())
                {
                    if let Some(original) = zsdesk_windows::shell::capture_background_layout(&view)
                    {
                        if !original.columns.is_empty() {
                            let key = zsdesk_windows::shell::commands::PKEY_NAME;
                            let mut list = original.clone();
                            list.mode = 4;
                            list.icon_size = 16;
                            list.column_dpi = self.session.ui.interface_dpi(self.dpi);
                            list.columns = vec![ViewColumn {
                                key: ViewPropertyKey {
                                    format_id: format!("{:?}", key.fmtid),
                                    property_id: key.pid,
                                },
                                width: (self.layout.browser.width - self.s(24)).clamp(48, 16384)
                                    as u32,
                            }];
                            self.columns.middle_layout = Some((view.clone(), original));
                            zsdesk_windows::shell::restore_view_layout(&view, &list);
                        }
                    }
                }
            }
        }
        if self.preview_rect.is_some() {
            unsafe {
                SetTimer(Some(self.hwnd), preview_process::TIMER, 100, None);
            }
        }
        self.current_layout() == layout
    }
    pub(super) fn restore_folder_layout(&mut self) {
        if self.picker.is_some() {
            return;
        }
        let path = self.current_location().as_path().map(Path::to_path_buf);
        let layout = if let Some(path) = path {
            remembered(&self.session.ui.folder_layouts, &path)
                .unwrap_or_else(|| self.current_layout())
        } else if self.columns.active && !matches!(self.current_location(), ShellLocation::ThisPc) {
            FolderLayout::Single
        } else {
            self.current_layout()
        };
        self.columns.preview_file = None;
        self.apply_folder_layout(layout);
    }
    pub(super) fn refresh_parent_column(&mut self) {
        let path = self.current_location().as_path().map(Path::to_path_buf);
        self.columns.sync(path.as_deref());
    }
    pub(super) fn column_results(&mut self) {
        let col = &mut self.columns;
        let Some(loader) = &col.loader else { return };
        let id = loader.latest.load(Ordering::Acquire);
        let Some((_, res)) = loader.replies.try_iter().filter(|(n, _)| *n == id).last() else {
            return;
        };
        match res {
            Ok(rows) => {
                col.rows = rows;
                unsafe {
                    SendMessageW(col.list, LB_SETCOUNT, Some(WPARAM(col.rows.len())), None);
                }
                if let Some(path) = self.current_location().as_path() {
                    self.columns.select_current(path);
                }
            }
            Err(e) => {
                self.columns.requested = false;
                self.show_message(e.to_string());
            }
        }
    }
    pub(super) fn column_selection(&mut self) {
        let col = &self.columns;
        let n = unsafe { SendMessageW(col.list, LB_GETCURSEL, None, None) }.0 as usize;
        let Some(entry) = col.rows.get(n) else { return };
        let path = col
            .parent
            .as_ref()
            .map_or_else(|| PathBuf::from(&entry.name), |dir| dir.join(&entry.name));
        if entry.folder {
            self.columns.preview_file = None;
            unsafe {
                self.navigate_to(ShellLocation::Path(path));
            }
        } else {
            self.columns.preview_file = Some(path);
            unsafe {
                SetTimer(Some(self.hwnd), preview_process::TIMER, 100, None);
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn clearing_columns_releases_the_worker_while_the_window_state_remains_alive() {
        let mut column = ParentColumn::default();
        for _ in 0..8 {
            let loader = Loader::new(HWND::default()).unwrap();
            let revision = Arc::downgrade(&loader.latest);
            column.loader = Some(loader);
            column.clear();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            while revision.upgrade().is_some() {
                assert!(std::time::Instant::now() < deadline, "parent-column worker still alive");
                thread::sleep(std::time::Duration::from_millis(2));
            }
        }
    }

    #[test]
    fn folder_layouts_remember_explicit_single_dual_and_columns() {
        let mut map = std::collections::BTreeMap::new();
        for (path, layout) in [
            (r"C:\项目", FolderLayout::Columns),
            (r"C:\项目\归档", FolderLayout::Single),
            (r"D:\Data", FolderLayout::Dual),
        ] {
            map.insert(PathBuf::from(path), layout);
        }
        let restored = serde_json::from_slice(&serde_json::to_vec(&map).unwrap()).unwrap();
        assert_eq!(
            remembered(&restored, Path::new(r"C:\项目")),
            Some(FolderLayout::Columns)
        );
        assert_eq!(
            remembered(&restored, Path::new(r"C:\项目\归档")),
            Some(FolderLayout::Single)
        );
        assert_eq!(
            remembered(&restored, Path::new(r"d:\data")),
            Some(FolderLayout::Dual)
        );
        assert_eq!(remembered(&restored, Path::new(r"D:\Elsewhere")), None);
    }
    #[test]
    fn three_columns_stay_inside_the_browser_at_every_width() {
        for width in [0, 1, 2, 3, 99, 800, 2049] {
            let a = LayoutRect::new(15, 33, width, 400);
            let (l, m, r) = column_rects(a, 2);
            assert!(l.right() <= m.x && m.right() <= r.x);
            assert_eq!(r.right(), a.right());
            assert_eq!(l.x, a.x);
        }
    }
}
