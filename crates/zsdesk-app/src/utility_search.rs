//! Search-specific chrome and result actions for the modeless file search window.
use super::*;
use std::collections::HashMap;
use std::ffi::OsString;
use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL};
use windows::Win32::UI::Shell::{
    SHGetFileInfoW, SHFILEINFOW, SHGFI_SMALLICON, SHGFI_SYSICONINDEX, SHGFI_USEFILEATTRIBUTES,
};

use zsui::{Color, ColorRole, Dpi, HorizontalAlign, NativeDrawCommand, NativeDrawFill,
    NativeDrawPlan, NativeDrawTextCommand, Rect, SemanticTextStyle, TextRole, WindowsGdiDrawSink,
    WindowsGdiPalette, ZsPlatformStyle};
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
    pub buttons:Rc<search_buttons::Buttons>,
    pub table:Rc<result_table::Table>,
    pub staged:Option<(Vec<PathBuf>,std::time::Instant)>,
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
    pub(super) fn icon(&mut self, list: HWND, path: &Path, folder: bool) -> i32 {
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
                return I_IMAGENONE;
            }
            unsafe {
                if SendMessageW(list,LVM_GETIMAGELIST,Some(WPARAM(LVSIL_SMALL as usize)),None).0 != handle as isize {
                SendMessageW(
                    list,
                    LVM_SETIMAGELIST,
                    Some(WPARAM(LVSIL_SMALL as usize)),
                    Some(LPARAM(handle as isize)),
                );
                }
            }
            if self.icons.len() >= 256 {
                self.icons.clear();
            }
            self.icons.insert((folder, ext), info.iIcon);
            info.iIcon
        };
        index
    }
}
pub(super) fn label(value: &str, bounds: Rect, role: TextRole, color: ColorRole, align: HorizontalAlign) -> NativeDrawCommand {
    let mut style = SemanticTextStyle::for_role(role);
    style.color = color;
    style.horizontal_align = align;
    NativeDrawCommand::Text(NativeDrawTextCommand::new(value, bounds, style))
}

pub(super) fn surface(p: Palette) -> WindowsGdiPalette {
    let opaque = |fg, bg| {
        let v = composite(fg, bg).0;
        Color::rgb(v as u8, (v >> 8) as u8, (v >> 16) as u8)
    };
    WindowsGdiPalette {
        primary_text: opaque(p.text, p.layer), secondary_text: opaque(p.text_secondary, p.layer),
        disabled_text: opaque(p.text_disabled, p.layer), accent: p.accent, accent_text: p.accent_text,
        surface: p.input_focused, surface_raised: p.content, control: p.layer,
        border: opaque(p.input_border, p.layer), strong_stroke: opaque(p.text, p.layer),
        danger: p.danger, success: p.accent, warning: p.danger,
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
    fn input_plan(&self, width: i32) -> zsui::ZsAutoSuggestRenderPlan {
        let bounds = Rect { x: self.s(16), y: self.s(12), width: (width - self.s(32)).max(1), height: self.s(44) };
        zsui::zs_auto_suggest_render_plan(bounds, 0, None, false, true, true, ZsPlatformStyle::current(), Dpi::new(self.dpi as f32))
    }
    fn draw_plan(&self, dc: HDC, palette: WindowsGdiPalette, plan: &NativeDrawPlan) {
        WindowsGdiDrawSink::with_palette_contrast_and_dpi(dc.0, palette,
            zsdesk_windows::theme::high_contrast(), Dpi::new(self.dpi as f32)).draw_native_plan(plan);
    }
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
        let input = self.input_plan(width).text_bounds;
        move_to(self.edit, input.x, input.y + (input.height - s(26)) / 2, input.width, s(26));
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
        let input = self.input_plan(r.right);
        let mut plan = zsui::zs_auto_suggest_header_native_draw_plan(&input, "", None);
        if unsafe { GetFocus() } == self.edit {
            plan.push(NativeDrawCommand::RoundFill {
                rect: Rect { x: input.bounds.x + s(4), y: input.bounds.y + input.bounds.height - s(2),
                    width: (input.bounds.width - s(8)).max(1), height: s(2) },
                fill: NativeDrawFill::Role(ColorRole::Accent), radius: s(1),
            });
        }
        plan.push(NativeDrawCommand::RoundRect {
            rect: Rect { x:s(15), y:s(103), width:(r.right-s(30)).max(1), height:(r.bottom-s(154)).max(1) },
            fill: NativeDrawFill::Role(ColorRole::SurfaceRaised),
            stroke: Some(NativeDrawFill::Role(ColorRole::Border)), radius:s(6),
        });
        plan.push(label(&self.status, Rect {x:s(18), y:r.bottom-s(42), width:(r.right-s(326)).max(1), height:s(33)},
            TextRole::Body, if self.search_error {ColorRole::Danger} else {ColorRole::SecondaryText}, HorizontalAlign::Start));
        self.draw_plan(dc, surface(self.palette), &plan);
    }

    pub(super) fn move_selection(&self, step: isize) {
        if self.awaiting_page || self.paths.is_empty() {
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
    fn zsui_search_chrome_has_antialiased_corners_at_multiple_scales() {
        for dpi in [96.0,120.0,144.0,192.0] {
            let surface_dc=zsdesk_windows::gfx::Surface::new(180,96).unwrap();
            let dpi=Dpi::new(dpi);
            let bounds=Rect{x:4,y:4,width:160,height:80};
            let geometry=zsui::zs_auto_suggest_render_plan(bounds,0,None,false,true,true,ZsPlatformStyle::current(),dpi);
            assert!(geometry.search_icon.is_some());
            let mut colors=surface(Palette::light());
            colors.surface=Color::rgb(255,255,255);
            colors.border=colors.surface;
            unsafe{let _=PatBlt(surface_dc.dc,0,0,180,96,BLACKNESS);}
            let plan=zsui::zs_auto_suggest_header_native_draw_plan(&geometry,"",None);
            WindowsGdiDrawSink::with_palette_and_dpi(surface_dc.dc.0,colors,dpi).draw_native_plan(&plan);
            let corner=(4..4+geometry.control_radius+1).flat_map(|x|(4..4+geometry.control_radius+1).map(move |y|(x,y)));
            assert!(corner.map(|(x,y)|unsafe{GetPixel(surface_dc.dc,x,y)}.0 & 255).any(|v|v>0 && v<255),"corner at {dpi:?}");
        }
    }
    #[test]
    fn filters_preserve_disjunctions_and_plain_queries() {
        assert_eq!(filtered_query("  a | b ", 0), "a | b");
        assert_eq!(filtered_query("a | b", 2), "<a | b> folder:");
        assert_eq!(filtered_query("", 1), "file:");
        assert!(filtered_query("", 0).is_empty());
    }
    #[test]
    fn all_file_folder_document_and_image_filters(){
        use crate::fast_search::query::Query;
        let corpus=[("标准文件夹",true),("标准.pdf",false),("标准.png",false),("标准.exe",false),("其他.pdf",false)];
        for (filter,expected) in [4,3,1,1,1].into_iter().enumerate() {
            let q=Query::parse(&filtered_query("标准",filter),false).unwrap();
            assert_eq!(corpus.iter().filter(|(name,dir)|q.matches(name,*dir,"")).count(),expected);
        }
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
