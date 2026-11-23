//! ZSUI owns result pixels. The native list supplies column resizing, scrolling,
//! accessibility, selection, edit labels and Shell hit testing.
use super::*;
use std::collections::{HashMap, VecDeque};
use windows::Win32::UI::Shell::GetWindowSubclass;
use zsui::{Dp, Dpi, Rect, WindowsGdiDrawSink, ZsPlatformStyle, ZsTableColumn, ZsTableRow};

const SUBCLASS: usize = 0x5a54;
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Metadata {
    pub size: String,
    pub modified: String,
    pub icon: i32,
}

struct Row {
    path: PathBuf,
    cells: ZsTableRow,
    icon: i32,
}
impl Row {
    fn new(index: usize, path: &Path, meta: Option<&Metadata>) -> Self {
        let name = path
            .file_name()
            .unwrap_or(path.as_os_str())
            .to_string_lossy()
            .into_owned();
        let parent = path.parent().unwrap_or(path).to_string_lossy().into_owned();
        let (size, modified, icon) = meta.map_or_else(
            || ("…".to_owned(), "…".to_owned(), I_IMAGENONE),
            |m| (m.size.clone(), m.modified.clone(), m.icon),
        );
        Self {
            path: path.into(),
            cells: ZsTableRow::new(index as u64, [name, parent, size, modified]),
            icon,
        }
    }
}

#[derive(Default)]
struct Model {
    rows: Vec<Row>,
    meta: HashMap<PathBuf, Metadata>,
    order: VecDeque<PathBuf>,
}
impl Model {
    fn put(&mut self, path: &Path, value: Metadata) {
        if !self.meta.contains_key(path) {
            if self.order.len() >= PAGE_SIZE as usize * 5 {
                if let Some(first) = self.order.pop_front() {
                    self.meta.remove(&first);
                }
            }
            self.order.push_back(path.into());
        }
        self.meta.insert(path.into(), value);
    }
    fn replace(&mut self, paths: &[PathBuf]) {
        self.rows = paths
            .iter()
            .enumerate()
            .map(|(i, p)| Row::new(i, p, self.meta.get(p)))
            .collect();
    }
}

#[derive(Clone, Copy, Default, PartialEq)]
pub(super) enum Empty {
    #[default]
    Idle,
    Searching,
    Missing,
    Failed,
}
impl Empty {
    fn labels(self) -> (&'static str, &'static str) {
        match self {
            Self::Idle => (
                "输入名称或路径",
                "用 ↑ ↓ 选择结果，Enter 打开，Ctrl+Enter 定位",
            ),
            Self::Searching => ("正在搜索…", ""),
            Self::Failed => ("搜索不可用", ""),
            Self::Missing => ("未找到匹配项", "可更换关键词或选择“全部”"),
        }
    }
}
#[derive(Default)]
pub(super) struct Table {
    model: RefCell<Model>,
    palette: Cell<Option<Palette>>,
    empty: Cell<Empty>,
    render_dpi:Cell<u32>,
}
impl Table {
    pub fn attach(self: &Rc<Self>, list: HWND, p: Palette) -> Result<()> {
        self.palette.set(Some(p));
        let ptr = Rc::into_raw(Rc::clone(self));
        if !unsafe { SetWindowSubclass(list, Some(procedure), SUBCLASS, ptr as usize) }.as_bool() {
            unsafe {
                drop(Rc::from_raw(ptr));
            }
            anyhow::bail!("无法初始化搜索结果表格");
        }
        Ok(())
    }
    pub fn empty(&self, list: HWND, state: Empty) {
        if self.empty.replace(state) != state && self.model.borrow().rows.is_empty() {
            unsafe {
                let _ = InvalidateRect(Some(list), None, false);
            }
        }
    }
    pub fn dpi(&self,dpi:u32){self.render_dpi.set(dpi.max(96));}
    pub fn theme(&self, p: Palette) {
        self.palette.set(Some(p));
    }
    pub fn has_meta(&self, path: &Path) -> bool {
        self.model.borrow().meta.contains_key(path)
    }
    pub fn metadata(&self, row: usize, path: &Path, value: Metadata, visible: bool) -> bool {
        let mut model = self.model.borrow_mut();
        let change = visible
            && model.rows.get(row).is_some_and(|r| {
                r.path == path
                    && (r.icon != value.icon
                        || r.cells.cell(2) != value.size
                        || r.cells.cell(3) != value.modified)
            });
        if change {
            model.rows[row] = Row::new(row, path, Some(&value));
        }
        model.put(path, value);
        change
    }
    pub fn release(&self, list: HWND) {
        self.replace(list, &[]);
        *self.model.borrow_mut() = Model::default();
    }
    pub fn replace(&self, list: HWND, paths: &[PathBuf]) {
        self.model.borrow_mut().replace(paths);
        if unsafe { SendMessageW(list, LVM_GETITEMCOUNT, None, None) }.0 != paths.len() as isize {
            unsafe {
                SendMessageW(
                    list,
                    LVM_SETITEMCOUNT,
                    Some(WPARAM(paths.len())),
                    Some(LPARAM((LVSICF_NOINVALIDATEALL | LVSICF_NOSCROLL) as isize)),
                );
            }
        }
        unsafe {
            let _ = InvalidateRect(Some(list), None, false);
        }
    }
    pub fn rename(&self, row: usize, path: &Path) {
        let mut m = self.model.borrow_mut();
        if let Some(prior) = m.rows.get(row) {
            let old = prior.path.clone();
            let meta = m.meta.get(&old).cloned();
            m.rows[row] = Row::new(row, path, meta.as_ref());
            if let Some(meta) = meta {
                m.put(path, meta);
            }
        }
    }

    fn draw(&self, hwnd: HWND, dc: HDC) -> bool {
        let Some(p) = self.palette.get() else {
            return false;
        };
        let mut client = RECT::default();
        if unsafe { GetClientRect(hwnd, &mut client) }.is_err() {
            return false;
        }
        let header = HWND(unsafe { SendMessageW(hwnd, LVM_GETHEADER, None, None) }.0 as _);
        let mut origin = [POINT::default()];
        unsafe {
            MapWindowPoints(Some(header), Some(hwnd), &mut origin);
        }
        let mut hr = RECT::default();
        let _ = unsafe { GetClientRect(header, &mut hr) };
        let top = (origin[0].y + hr.bottom).max(0);
        let body = Rect {
            x: 0,
            y: top,
            width: client.right,
            height: (client.bottom - top).max(0),
        };
        if body.height == 0 || body.width == 0 {
            return true;
        }
        let dpi=self.render_dpi.get().max(96);
        let widths: [i32; 4] = std::array::from_fn(
            |i| unsafe { SendMessageW(hwnd, LVM_GETCOLUMNWIDTH, Some(WPARAM(i)), None) }.0 as i32,
        );
        let columns: Vec<_> = widths
            .iter()
            .enumerate()
            .map(|(i, w)| {
                ZsTableColumn::new(i as u64, "").fixed_width(Dp::new(*w as f32 * 96.0 / dpi as f32))
            })
            .collect();
        let first = unsafe { SendMessageW(hwnd, LVM_GETTOPINDEX, None, None) }
            .0
            .max(0) as usize;
        let count = unsafe { SendMessageW(hwnd, LVM_GETCOUNTPERPAGE, None, None) }
            .0
            .max(0) as usize
            + 2;
        let (start, rows, image_indices) = {
            let model = self.model.borrow();
            let start = first.min(model.rows.len());
            let visible = &model.rows[start..(start + count).min(model.rows.len())];
            (
                start,
                visible.iter().map(|r| r.cells.clone()).collect::<Vec<_>>(),
                visible.iter().map(|r| r.icon).collect::<Vec<_>>(),
            )
        };
        let mut plan = zsui::zs_table_render_plan(
            Rect {
                x: origin[0].x,
                y: top,
                width: widths.iter().sum(),
                height: body.height,
            },
            &columns,
            &rows,
            None,
            None,
            ZsPlatformStyle::current(),
            Dpi::new(dpi as f32),
        );
        // The native header owns its pixels. Align the framework's rows with the
        // native hit rectangles, including partially visible rows and horizontal scroll.
        plan.bounds = body;
        plan.radius = 0;
        plan.header_bounds = Rect {
            x: 0,
            y: top - 1,
            width: body.width,
            height: 0,
        };
        for col in &mut plan.columns {
            col.label_bounds.height = 0;
        }
        let images = HIMAGELIST(
            unsafe {
                SendMessageW(
                    hwnd,
                    LVM_GETIMAGELIST,
                    Some(WPARAM(LVSIL_SMALL as usize)),
                    None,
                )
            }
            .0,
        );
        let (mut iw, mut ih) = (16, 16);
        if images.0 != 0 {
            unsafe {
                let _ = ImageList_GetIconSize(images, Some(&mut iw), Some(&mut ih));
            }
        }
        let mut icons = Vec::new();
        let mut focus = None;
        for (n, row) in plan.rows.iter_mut().enumerate() {
            let i = start + n;
            let mut r = RECT::default();
            if unsafe {
                SendMessageW(
                    hwnd,
                    LVM_GETITEMRECT,
                    Some(WPARAM(i)),
                    Some(LPARAM(&mut r as *mut _ as isize)),
                )
            }
            .0 == 0
            {
                return false;
            }
            row.bounds = Rect {
                x: origin[0].x,
                y: r.top,
                width: widths.iter().sum(),
                height: r.bottom - r.top,
            };
            let flags = unsafe {
                SendMessageW(
                    hwnd,
                    LVM_GETITEMSTATE,
                    Some(WPARAM(i)),
                    Some(LPARAM((LVIS_SELECTED.0 | LVIS_FOCUSED.0) as isize)),
                )
            }
            .0 as u32;
            row.selected = flags & LVIS_SELECTED.0 != 0;
            if flags & LVIS_FOCUSED.0 != 0 && unsafe { GetFocus() } == hwnd {
                focus = Some(r);
            }
            for (col, cell) in row.cells.iter_mut().enumerate() {
                cell.bounds.y = r.top;
                cell.bounds.height = r.bottom - r.top;
                cell.text_bounds.y = r.top;
                cell.text_bounds.height = r.bottom - r.top;
                if col == 0 {
                    let dx = iw + 2;
                    cell.text_bounds.x += dx;
                    cell.text_bounds.width = (cell.text_bounds.width - dx).max(0);
                    if widths[0] >= dx {
                        icons.push((
                            image_indices[n],
                            origin[0].x + 3,
                            r.top + (r.bottom - r.top - ih) / 2,
                        ));
                    }
                }
            }
        }
        let mut palette = search_ui::surface(p);
        palette.surface = p.content;
        let save = unsafe { SaveDC(dc) };
        unsafe {
            IntersectClipRect(dc, body.x, body.y, body.width, client.bottom);
        }
        // A buffered paint surface can be reused with unspecified contents.
        // Initialize the entire damaged body before ZSUI blends text and edges.
        unsafe {
            let brush = GetStockObject(DC_BRUSH);
            SetDCBrushColor(dc, rgb(p.content));
            FillRect(
                dc,
                &RECT {
                    left: body.x,
                    top: body.y,
                    right: client.right,
                    bottom: client.bottom,
                },
                HBRUSH(brush.0),
            );
        }
        let mut draw = zsui::zs_table_native_draw_plan(&plan);
        for &(idx, x, y) in &icons {
            if idx < 0 || images.0 == 0 {
                draw.push(zsui::NativeDrawCommand::Icon(
                    zsui::NativeDrawIconCommand::new(
                        zsui::ZsIcon::File,
                        Rect {
                            x,
                            y,
                            width: iw,
                            height: ih,
                        },
                        zsui::NativeIconColorMode::ThemeAware,
                    )
                    .with_color(zsui::ColorRole::SecondaryText),
                ));
            }
        }
        if rows.is_empty() {
            let (title, hint) = self.empty.get().labels();
            let s = |n: i32| n * dpi as i32 / 96;
            let center = body.y + body.height / 2;
            for (text, role, color, y, height) in [
                (
                    title,
                    zsui::TextRole::Subtitle,
                    zsui::ColorRole::PrimaryText,
                    center - s(30),
                    s(32),
                ),
                (
                    hint,
                    zsui::TextRole::Body,
                    zsui::ColorRole::SecondaryText,
                    center + s(4),
                    s(25),
                ),
            ] {
                if !text.is_empty() {
                    draw.push(search_ui::label(
                        zsdesk_core::i18n::text(text),
                        Rect {
                            x: body.x + s(12),
                            y,
                            width: (body.width - s(24)).max(1),
                            height,
                        },
                        role,
                        color,
                        zsui::HorizontalAlign::Center,
                    ));
                }
            }
        }
        WindowsGdiDrawSink::with_palette_contrast_and_dpi(
            dc.0,
            palette,
            zsdesk_windows::theme::high_contrast(),
            Dpi::new(dpi as f32),
        )
        .draw_native_plan(&draw);
        for (idx, x, y) in icons
            .into_iter()
            .filter(|(idx, _, _)| *idx >= 0 && images.0 != 0)
        {
            unsafe {
                let _ = ImageList_Draw(images, idx, dc, x, y, ILD_TRANSPARENT);
            }
        }
        if let Some(rect) = focus {
            unsafe {
                let _ = DrawFocusRect(dc, &rect);
            }
        }
        unsafe {
            let _ = RestoreDC(dc, save);
        }
        true
    }
    fn buffered(&self, hwnd: HWND, dc: HDC, r: RECT) -> bool {
        if r.right <= r.left || r.bottom <= r.top {
            return true;
        }
        let band = windows_sys::Win32::Foundation::RECT {
            left: r.left,
            top: r.top,
            right: r.right,
            bottom: r.bottom,
        };
        let mut buffer = unsafe { zsui::WindowsBufferedPaint::begin(dc.0, &band) };
        let ok = self.draw(hwnd, buffer.as_ref().map_or(dc, |b| HDC(b.hdc())));
        if !ok {
            if let Some(b) = buffer.as_mut() {
                b.set_update_target(false);
            }
        }
        ok
    }
}

unsafe extern "system" fn procedure(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    id: usize,
    data: usize,
) -> LRESULT {
    if msg == WM_NCDESTROY {
        unsafe {
            let _ = RemoveWindowSubclass(hwnd, Some(procedure), id);
            drop(Rc::from_raw(data as *const Table));
        }
        return unsafe { DefSubclassProc(hwnd, msg, wp, lp) };
    }
    if msg == WM_ERASEBKGND {
        return LRESULT(1);
    }
    if !matches!(msg, WM_PAINT | WM_PRINTCLIENT) {
        return unsafe { DefSubclassProc(hwnd, msg, wp, lp) };
    }
    unsafe {
        Rc::increment_strong_count(data as *const Table);
    }
    let table = unsafe { Rc::from_raw(data as *const Table) };
    if msg == WM_PAINT {
        let mut ps = PAINTSTRUCT::default();
        let dc = unsafe { BeginPaint(hwnd, &mut ps) };
        if !table.buffered(hwnd, dc, ps.rcPaint) {
            unsafe {
                DefSubclassProc(
                    hwnd,
                    WM_PRINTCLIENT,
                    WPARAM(dc.0 as usize),
                    LPARAM(PRF_CLIENT as isize),
                );
            }
        }
        unsafe {
            let _ = EndPaint(hwnd, &ps);
        }
        if unsafe { IsWindowVisible(hwnd) }.as_bool()
            && unsafe { GetParent(hwnd) }.ok() == Some(unsafe { GetForegroundWindow() })
        {
            crate::quick_search::timing::intermediate(false, || table.model.borrow().rows.len());
            crate::quick_search::timing::painted(table.model.borrow().rows.is_empty());
        }
        return LRESULT(0);
    }
    let mut r = RECT::default();
    if unsafe { GetClientRect(hwnd, &mut r) }.is_ok() && table.buffered(hwnd, HDC(wp.0 as _), r) {
        return LRESULT(0);
    }
    unsafe { DefSubclassProc(hwnd, msg, wp, lp) }
}

// A virtual list may ask for text synchronously during a layout, hit test or
// accessibility call. Its read-only data stays accessible while Panel is updating.
pub(super) unsafe fn notify(lp: LPARAM) -> Option<LRESULT> {
    if lp.0 == 0 {
        return None;
    }
    let header = unsafe { &*(lp.0 as *const NMHDR) };
    if header.idFrom != 11 || !matches!(header.code, LVN_GETDISPINFOW | LVN_ODFINDITEMW) {
        return None;
    }
    let mut ptr = 0;
    if !unsafe { GetWindowSubclass(header.hwndFrom, Some(procedure), SUBCLASS, Some(&mut ptr)) }
        .as_bool()
    {
        return None;
    }
    let model = unsafe { &*(ptr as *const Table) }.model.borrow();
    if header.code == LVN_ODFINDITEMW {
        let q = unsafe { &*(lp.0 as *const NMLVFINDITEMW) };
        if !q.lvfi.flags.contains(LVFI_STRING) || q.lvfi.psz.is_null() || model.rows.is_empty() {
            return Some(LRESULT(-1));
        }
        let needle = unsafe { q.lvfi.psz.to_string() }.ok()?.to_lowercase();
        let start = q.iStart.max(0) as usize;
        let count = model.rows.len();
        let start = start.min(count);
        let n = if q.lvfi.flags.contains(LVFI_WRAP) {
            count
        } else {
            count - start
        };
        let found = (0..n).map(|i| (start + i) % count).find(|i| {
            let label = model.rows[*i].cells.cell(0).to_lowercase();
            if q.lvfi.flags.contains(LVFI_PARTIAL) {
                label.starts_with(&needle)
            } else if q.lvfi.flags.contains(LVFI_SUBSTRING) {
                label.contains(&needle)
            } else {
                label == needle
            }
        });
        return Some(LRESULT(found.map_or(-1, |i| i as isize)));
    }
    let d = unsafe { &mut *(lp.0 as *mut NMLVDISPINFOW) };
    if let Some(row) = model.rows.get(d.item.iItem as usize) {
        if d.item.mask.contains(LVIF_TEXT) && !d.item.pszText.is_null() && d.item.cchTextMax > 0 {
            let buf = unsafe {
                std::slice::from_raw_parts_mut(d.item.pszText.0, d.item.cchTextMax as usize)
            };
            let end = buf.len() - 1;
            let mut n = 0;
            for u in row
                .cells
                .cell(d.item.iSubItem as usize)
                .encode_utf16()
                .take(end)
            {
                buf[n] = u;
                n += 1;
            }
            if n != 0 && (0xd800..=0xdbff).contains(&buf[n - 1]) {
                n -= 1;
            }
            buf[n] = 0;
        }
        if d.item.mask.contains(LVIF_IMAGE) {
            d.item.iImage = row.icon;
        }
    }
    Some(LRESULT(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uncached_metadata_has_placeholders_and_reused_paths_keep_their_values() {
        let path = PathBuf::from(r"C:\标准.pdf");
        let mut model = Model::default();
        model.replace(std::slice::from_ref(&path));
        assert_eq!(model.rows[0].cells.cell(2), "…");
        assert_eq!(model.rows[0].cells.cell(3), "…");
        let meta = Metadata {
            size: "42 KB".into(),
            modified: "2026-11-23".into(),
            icon: 7,
        };
        model.put(&path, meta);
        model.replace(&[PathBuf::from(r"C:\目录"), path]);
        assert_eq!(model.rows[1].cells.cell(2), "42 KB");
        assert_eq!(model.rows[1].icon, 7);
    }
}
