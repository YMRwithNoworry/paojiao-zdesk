//! Search-specific chrome and result actions for the modeless file search window.
use super::*;
use std::collections::HashMap;
use std::ffi::OsString;
use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL};
use windows::Win32::UI::Shell::{
    SHGetFileInfoW, SHFILEINFOW, SHGFI_SMALLICON, SHGFI_SYSICONINDEX, SHGFI_USEFILEATTRIBUTES,
};

pub(super) const SHORTCUT: i32 = 18;
pub(super) const FIRST_FILTER: i32 = 30;
pub(super) const FILTERS: [(&str, &str); 5] = [
    ("全部", ""),
    ("文件", "file:"),
    ("文件夹", "folder:"),
    (
        "文档",
        "ext:pdf;doc;docx;xls;xlsx;ppt;pptx;txt;md;rtf;odt;wps;et;dps",
    ),
    (
        "图片",
        "ext:jpg;jpeg;png;webp;gif;bmp;tif;tiff;heic;avif;svg",
    ),
];
pub(super) const WIDTH_CHANGED: u32 = WM_APP + 109;
pub(super) const COPY_PATH: u32 = WM_APP + 110;
pub(super) const MOVE_SELECTION: u32 = WM_APP + 111;

pub(super) fn palette(dark: bool) -> Palette {
    let mut p = Palette::for_mode(if dark { Mode::Dark } else { Mode::Light });
    if zsdesk_windows::theme::high_contrast() {
        let system = |index| {
            let v = unsafe { GetSysColor(index) };
            zsdesk_ui::theme::Color::rgb(v as u8, (v >> 8) as u8, (v >> 16) as u8)
        };
        p.layer = system(COLOR_BTNFACE);
        p.content = system(COLOR_WINDOW);
        p.input_focused = p.content;
        p.text = system(COLOR_WINDOWTEXT);
        p.text_secondary = p.text;
        p.text_disabled = system(COLOR_GRAYTEXT);
        p.accent = system(COLOR_HIGHLIGHT);
        p.accent_text = system(COLOR_HIGHLIGHTTEXT);
        p.input_border = p.text;
        p.divider = p.text;
        p.hover = p.layer;
        p.pressed = p.layer;
    }
    p
}
#[derive(Default)]
pub(super) struct SearchView {
    pub filter: usize,
    widths: [Option<i32>; 4],
    icons: HashMap<(bool, OsString), i32>,
    pub last_query: String,
}
fn settings_file(profile: &Path) -> PathBuf {
    profile.with_file_name("quick-search-view.json")
}
impl SearchView {
    pub(super) fn load(profile: &Path) -> Self {
        let file = settings_file(profile);
        let widths = std::fs::metadata(&file)
            .ok()
            .filter(|m| m.len() < 2048)
            .and_then(|_| std::fs::read(file).ok())
            .and_then(|b| serde_json::from_slice::<[Option<i32>; 4]>(&b).ok())
            .unwrap_or_default()
            .map(|w| w.filter(|v| (32..=4096).contains(v)));
        Self {
            widths,
            ..Default::default()
        }
    }
    pub(super) fn reset_widths(&mut self, profile: &Path) -> Result<()> {
        self.widths = [None; 4];
        self.save(profile)
    }
    fn save(&self, profile: &Path) -> Result<()> {
        crate::quick_search::control::atomic_write(
            &settings_file(profile),
            &serde_json::to_vec(&self.widths)?,
        )
    }
    pub(super) fn note_width(
        &mut self,
        profile: &Path,
        col: usize,
        width: i32,
        dpi: u32,
    ) -> Result<()> {
        if let Some(slot) = self.widths.get_mut(col) {
            *slot = Some(
                ((i64::from(width) * 96 + i64::from(dpi) / 2) / i64::from(dpi.max(1)))
                    .clamp(32, 4096) as i32,
            );
            self.save(profile)?;
        }
        Ok(())
    }
    fn measured_widths(&self, available: i32, dpi: u32) -> [i32; 4] {
        let px = |dip: i32| (i64::from(dip) * i64::from(dpi) / 96) as i32;
        let mut widths = self.widths.map(|v| v.map(px).unwrap_or(0));
        if self.widths[2].is_none() {
            widths[2] = px(88);
        }
        if self.widths[3].is_none() {
            widths[3] = px(154);
        }
        let flexible = (available - widths[2] - widths[3]).max(0);
        match (self.widths[0], self.widths[1]) {
            (None, None) => {
                widths[0] = (flexible * 40 / 100).max(px(110));
                widths[1] = (flexible - widths[0]).max(px(145));
            }
            (None, Some(_)) => widths[0] = (flexible - widths[1]).max(px(110)),
            (Some(_), None) => widths[1] = (flexible - widths[0]).max(px(145)),
            _ => {}
        }
        widths
    }
    pub(super) fn icon(&mut self, list: HWND, path: &Path, folder: bool, row: usize) {
        let ext = if folder {
            OsString::new()
        } else {
            path.extension().unwrap_or_default().to_ascii_lowercase()
        };
        let index = if let Some(index) = self.icons.get(&(folder, ext.clone())) {
            *index
        } else {
            let mut name = OsString::from("item");
            if !ext.is_empty() {
                name.push(".");
                name.push(&ext);
            }
            use std::os::windows::ffi::OsStrExt;
            let name: Vec<_> = name.encode_wide().chain(Some(0)).collect();
            let mut info = SHFILEINFOW::default();
            let handle = unsafe {
                SHGetFileInfoW(
                    PCWSTR(name.as_ptr()),
                    if folder {
                        FILE_ATTRIBUTE_DIRECTORY
                    } else {
                        FILE_ATTRIBUTE_NORMAL
                    },
                    Some(&mut info),
                    std::mem::size_of::<SHFILEINFOW>() as u32,
                    SHGFI_USEFILEATTRIBUTES | SHGFI_SYSICONINDEX | SHGFI_SMALLICON,
                )
            };
            if handle == 0 {
                return;
            }
            unsafe {
                SendMessageW(
                    list,
                    LVM_SETIMAGELIST,
                    Some(WPARAM(LVSIL_SMALL as usize)),
                    Some(LPARAM(handle as isize)),
                );
            }
            if self.icons.len() >= 256 {
                self.icons.clear();
            }
            self.icons.insert((folder, ext), info.iIcon);
            info.iIcon
        };
        let item = LVITEMW {
            mask: LVIF_IMAGE,
            iItem: row as i32,
            iImage: index,
            ..Default::default()
        };
        unsafe {
            SendMessageW(
                list,
                LVM_SETITEMW,
                None,
                Some(LPARAM(&item as *const _ as isize)),
            );
        }
    }
}
fn centered(dc: HDC, value: &str, mut rect: RECT, font: HFONT, color: COLORREF) {
    if value.is_empty() {
        return;
    }
    let mut units = wide(value);
    units.pop();
    unsafe {
        let old = SelectObject(dc, font.into());
        SetTextColor(dc, color);
        SetBkMode(dc, TRANSPARENT);
        DrawTextW(
            dc,
            &mut units,
            &mut rect,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
        );
        SelectObject(dc, old);
    }
}
pub(super) fn filtered_query(query: &str, filter: usize) -> String {
    let q = query.trim();
    let restrict = FILTERS.get(filter).map(|f| f.1).unwrap_or("");
    match (q.is_empty(), restrict.is_empty()) {
        (_, true) => q.to_owned(),
        (true, false) => restrict.to_owned(),
        _ => format!("<{q}> {restrict}"),
    }
}
impl Panel {
    pub(super) fn search_layout(&self, width: i32, height: i32) {
        let s = |v| self.s(v);
        let move_to = |hwnd: HWND, x: i32, y: i32, w: i32, h: i32| {
            if !hwnd.is_invalid() {
                let _ = unsafe {
                    SetWindowPos(
                        hwnd,
                        None,
                        x,
                        y,
                        w.max(1),
                        h.max(1),
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    )
                };
            }
        };
        let child = |id| unsafe { GetDlgItem(Some(self.hwnd), id) }.unwrap_or_default();
        move_to(self.edit, s(55), s(22), width - s(208), s(26));
        move_to(child(SHORTCUT), width - s(132), s(13), s(116), s(42));
        let mut left = s(16);
        for (i, w) in [60, 60, 75, 60, 60].into_iter().enumerate() {
            move_to(child(FIRST_FILTER + i as i32), left, s(65), s(w), s(29));
            left += s(w + 5);
        }
        move_to(self.list, s(16), s(104), width - s(32), height - s(156));
        let available = (width - s(52)).max(0);
        for (i, w) in self
            .search_view
            .measured_widths(available, self.dpi)
            .into_iter()
            .enumerate()
        {
            report_list::width(self.list, i, w);
        }
        move_to(self.action, width - s(292), height - s(40), s(118), s(30));
        move_to(child(16), width - s(162), height - s(40), s(68), s(30));
        move_to(child(17), width - s(88), height - s(40), s(72), s(30));
        self.page_buttons();
    }
    pub(super) fn draw_search(&self, dc: HDC, r: RECT) {
        let s = |n| self.s(n);
        let area = RECT {
            left: s(16),
            top: s(12),
            right: r.right - s(146),
            bottom: s(56),
        };
        let p = self.palette;
        content::rounded(
            dc,
            area,
            rgb(p.input_focused),
            composite(p.input_border, p.layer),
            s(8),
        );
        let pen = unsafe {
            CreatePen(
                PS_SOLID,
                s(2).max(1),
                composite(p.text_secondary, p.input_focused),
            )
        };
        unsafe {
            let old = SelectObject(dc, pen.into());
            let brush = SelectObject(dc, GetStockObject(NULL_BRUSH));
            let _ = Ellipse(dc, s(31), s(25), s(43), s(37));
            let _ = MoveToEx(dc, s(40), s(35), None);
            let _ = LineTo(dc, s(47), s(42));
            SelectObject(dc, brush);
            SelectObject(dc, old);
            let _ = DeleteObject(pen.into());
        }
        if unsafe { GetFocus() } == self.edit {
            let line = RECT {
                left: area.left + s(4),
                right: area.right - s(4),
                top: area.bottom - s(2),
                bottom: area.bottom,
            };
            let b = unsafe { CreateSolidBrush(rgb(p.accent)) };
            unsafe {
                FillRect(dc, &line, b);
                let _ = DeleteObject(b.into());
            }
        }
        // A card separates the results from the command and status surfaces.
        content::rounded(
            dc,
            RECT {
                left: s(15),
                top: s(103),
                right: r.right - s(15),
                bottom: r.bottom - s(51),
            },
            rgb(p.content),
            composite(p.divider, p.layer),
            s(6),
        );
        if self.paths.is_empty() {
            let top = s(104);
            let bottom = r.bottom - s(52);
            let center = top + (bottom - top) / 2;
            let idle = text(self.edit).trim().is_empty() && self.search_view.filter == 0;
            let (title, hint) = if self.search_error {
                ("搜索不可用", "")
            } else if self.busy {
                ("正在搜索…", "")
            } else if idle {
                (
                    "输入名称或路径",
                    "用 ↑ ↓ 选择结果，Enter 打开，Ctrl+Enter 定位",
                )
            } else {
                ("未找到匹配项", "可更换关键词或选择“全部”")
            };
            centered(
                dc,
                zsdesk_core::i18n::text(title),
                RECT {
                    left: s(28),
                    right: r.right - s(28),
                    top: center - s(30),
                    bottom: center + s(2),
                },
                self.title_font,
                composite(p.text, p.content),
            );
            centered(
                dc,
                zsdesk_core::i18n::text(hint),
                RECT {
                    left: s(28),
                    right: r.right - s(28),
                    top: center + s(4),
                    bottom: center + s(29),
                },
                self.font,
                composite(p.text_secondary, p.content),
            );
        }
        draw_text(
            dc,
            &self.status,
            RECT {
                left: s(18),
                top: r.bottom - s(42),
                right: r.right - s(308),
                bottom: r.bottom - s(9),
            },
            self.font,
            if self.search_error {
                rgb(p.danger)
            } else {
                composite(p.text_secondary, p.layer)
            },
        );
    }
    pub(super) fn draw_search_button(&self, d: &DRAWITEMSTRUCT) -> bool {
        let id = d.CtlID as i32;
        if id != SHORTCUT
            && ![12, 16, 17].contains(&id)
            && !(FIRST_FILTER..FIRST_FILTER + FILTERS.len() as i32).contains(&id)
        {
            return false;
        }
        let selected = id == FIRST_FILTER + self.search_view.filter as i32;
        let disabled = d.itemState.0 & ODS_DISABLED.0 != 0;
        let pressed = d.itemState.0 & ODS_SELECTED.0 != 0;
        let mut cursor = POINT::default();
        let hover = unsafe { GetCursorPos(&mut cursor) }.is_ok()
            && unsafe { WindowFromPoint(cursor) } == d.hwndItem;
        let p = self.palette;
        let bg = if selected {
            p.accent
        } else if pressed {
            p.pressed
        } else if hover && !disabled {
            p.hover
        } else {
            p.layer
        };
        let fill = if selected {
            rgb(bg)
        } else {
            composite(bg, p.layer)
        };
        let border = if selected {
            rgb(p.accent)
        } else if id == SHORTCUT || id == 12 || hover {
            composite(p.divider, p.layer)
        } else {
            fill
        };
        unsafe {
            FillRect(d.hDC, &d.rcItem, self.brush);
        }
        content::rounded(d.hDC, d.rcItem, fill, border, self.s(8));
        let fg = if disabled {
            composite(p.text_disabled, p.layer)
        } else if selected {
            rgb(p.accent_text)
        } else {
            composite(p.text, p.layer)
        };
        let mut title = wide(&text(d.hwndItem));
        title.pop();
        let mut rect = d.rcItem;
        unsafe {
            let old = SelectObject(d.hDC, self.font.into());
            SetBkMode(d.hDC, TRANSPARENT);
            SetTextColor(d.hDC, fg);
            DrawTextW(
                d.hDC,
                &mut title,
                &mut rect,
                DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
            );
            SelectObject(d.hDC, old);
            if d.itemState.0 & ODS_FOCUS.0 != 0 {
                let _ = InflateRect(&mut rect, -self.s(3), -self.s(3));
                let _ = DrawFocusRect(d.hDC, &rect);
            }
        }
        true
    }
    pub(super) fn move_selection(&self, step: isize) {
        if self.paths.is_empty() {
            return;
        }
        let selected =
            report_list::selected(self.list).map_or(if step > 0 { -1 } else { 0 }, |n| n as isize);
        let next = (selected + step).clamp(0, self.paths.len() as isize - 1) as usize;
        report_list::set_selection(self.list, false);
        let flags = LIST_VIEW_ITEM_STATE_FLAGS(LVIS_SELECTED.0 | LVIS_FOCUSED.0);
        let item = LVITEMW {
            state: flags,
            stateMask: flags,
            ..Default::default()
        };
        unsafe {
            SendMessageW(
                self.list,
                LVM_SETITEMSTATE,
                Some(WPARAM(next)),
                Some(LPARAM(&item as *const _ as isize)),
            );
            SendMessageW(self.list, LVM_ENSUREVISIBLE, Some(WPARAM(next)), None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn filters_preserve_disjunctions_and_plain_queries() {
        assert_eq!(filtered_query("  a | b ", 0), "a | b");
        assert_eq!(filtered_query("a | b", 2), "<a | b> folder:");
        assert_eq!(filtered_query("", 1), "file:");
        assert!(filtered_query("", 0).is_empty());
    }
    #[test]
    fn resized_columns_keep_manual_width_and_scale_across_monitors() {
        let view = SearchView {
            widths: [Some(215), None, None, None],
            ..Default::default()
        };
        assert_eq!(view.measured_widths(1000, 96), [215, 543, 88, 154]);
        assert_eq!(view.measured_widths(750, 96)[0], 215);
        assert_eq!(view.measured_widths(1500, 144)[0], 322);
    }
}
