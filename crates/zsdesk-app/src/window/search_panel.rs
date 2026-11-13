//! Search controls embedded above the native Shell results view.
use super::*;
use crate::fast_search::everything::SearchSort;
use windows::Win32::{
    System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER},
    UI::Shell::{FileOpenDialog, IFileOpenDialog, FOS_FORCEFILESYSTEM, FOS_PATHMUSTEXIST,
        FOS_PICKFOLDERS, SIGDN_FILESYSPATH},
};
use zsdesk_ui::{chrome::SearchPanelHit, menus, theme::{glyph, FencePalette}};
use zsdesk_windows::gfx::{Align, Canvas, Color, RectF, TextStyle};

pub(super) const SEARCH_PAGE_SIZE: u32 = 200;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum SearchCategory {
    #[default]
    All,
    Folders,
    Documents,
    Images,
    Videos,
    Audio,
}

impl SearchCategory {
    pub(super) const ALL: [Self; 6] = [Self::All, Self::Folders, Self::Documents, Self::Images, Self::Videos, Self::Audio];

    pub(super) fn query_prefix(&self) -> &'static str {
        match self {
            Self::All => "",
            Self::Folders => "folder:",
            Self::Documents => "ext:doc;docx;docm;xls;xlsx;xlsm;ppt;pptx;pptm;pdf;txt;rtf;md;csv;odt;ods;odp;wps;et;dps",
            Self::Images => "ext:jpg;jpeg;png;gif;bmp;webp;heic;heif;avif;tif;tiff;svg;ico;raw;cr2;nef;arw;psd",
            Self::Videos => "ext:mp4;mkv;avi;mov;wmv;flv;webm;m4v;mpg;mpeg;ts;m2ts;3gp",
            Self::Audio => "ext:mp3;wav;flac;aac;m4a;ogg;opus;wma;aiff;ape;alac;mid;midi",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::All => "全部", Self::Folders => "文件夹", Self::Documents => "文档",
            Self::Images => "图片", Self::Videos => "视频", Self::Audio => "音频",
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct SearchControls {
    pub category: SearchCategory,
    pub scope: Option<PathBuf>,
    /// Folder to restore when this tab leaves its search results.
    pub origin: Option<ShellLocation>,
    /// The tab's native EDIT text, including an unsubmitted refinement.
    pub draft: Option<String>,
    pub sort: SearchSort,
    pub offset: u32,
    pub total: u64,
    pub shown: usize,
    pub busy: bool,
    pub error: Option<String>,
}

impl Default for SearchControls {
    fn default() -> Self {
        Self { category: SearchCategory::All, scope: None, origin: None, draft: None, sort: SearchSort::NameAsc,
            offset: 0, total: 0, shown: 0, busy: false, error: None }
    }
}

#[derive(Default)]
pub(super) struct SearchPanel {
    pub open: bool,
    pub tabs: HashMap<u64, SearchControls>,
    /// The owner of the shared native EDIT; independent of Shell navigation.
    pub edit_tab: Option<u64>,
}

fn rect(r: LayoutRect) -> RectF { RectF::new(r.x as f32, r.y as f32, r.width as f32, r.height as f32) }
fn color(c: zsdesk_ui::theme::Color) -> Color { Color::rgba8(c.r, c.g, c.b, c.a) }

fn sort_label(sort: SearchSort) -> &'static str {
    match sort {
        SearchSort::NameAsc => "名称升序", SearchSort::NameDesc => "名称降序",
        SearchSort::ModifiedDesc => "最近修改", SearchSort::SizeDesc => "大小降序",
    }
}

impl WindowState {
    pub(super) fn search_options_menu(&self) -> menus::MenuItemSpec {
        let controls=self.search_controls();
        let categories=SearchCategory::ALL.iter().enumerate().map(|(index,category)|
            menus::entry(category.label(),&format!("search-filter-{index}"),true,*category==controls.category)).collect();
        menus::submenu("搜索选项",vec![
            menus::submenu("文件类型",categories),
            menus::item("搜索范围…","search-scope"),
            menus::item("结果排序…","search-order"),
            menus::entry("上一页结果","search-page-previous",!controls.busy&&controls.offset>0,false),
            menus::entry("下一页结果","search-page-next",!controls.busy&&controls.shown>0&&u64::from(controls.offset)+(controls.shown as u64)<controls.total,false),
            menus::item("重新搜索","search-refresh"),
            menus::item("关闭搜索\tEsc","search-close"),
            menus::MenuItemSpec::Separator,
            if self.session.ui.search_backend.is_builtin(){menus::item("重建自带索引","search-native-rebuild")}else{menus::item("Everything 索引设置…","search-settings")},
        ])
    }

    pub(super) unsafe fn apply_search_option(&mut self,id:&str)->bool {
        if id=="search-native-rebuild"{self.rebuild_search_index();return true;}
        let hit=match id {
            "search-scope"=>SearchPanelHit::Scope,"search-order"=>SearchPanelHit::Sort,
            "search-page-previous"=>SearchPanelHit::Previous,"search-page-next"=>SearchPanelHit::Next,
            "search-close"=>SearchPanelHit::Close,"search-settings"=>SearchPanelHit::IndexSettings,
            "search-refresh"=>{self.cancel_fast_search();SearchPanelHit::Submit},
            _=>{let Some(index)=id.strip_prefix("search-filter-").and_then(|index|index.parse::<u8>().ok()).filter(|index|usize::from(*index)<SearchCategory::ALL.len()) else{return false;};SearchPanelHit::Category(index)},
        };
        unsafe{self.search_panel_action(hit);}true
    }

    pub(super) fn search_controls(&self) -> SearchControls {
        self.search_panel.tabs.get(&self.session.active_tab().id).cloned().unwrap_or_default()
    }

    pub(super) fn search_controls_mut(&mut self) -> &mut SearchControls {
        self.search_panel.tabs.entry(self.session.active_tab().id).or_default()
    }

    pub(super) fn paint_search_panel(&self, cv: &Canvas) {
        let layout = &self.layout.search_panel;
        if !self.search_panel.open || layout.card.is_empty() { return; }
        let controls = self.search_controls();
        let mut fence = FencePalette::new(if self.dark { Mode::Dark } else { Mode::Light }, 100);
        if let Some(accent) = zsdesk_windows::theme::system_accent(self.dark) { fence = fence.with_accent(accent); }
        let text = color(fence.text);
        let secondary = color(fence.text_secondary);
        let accent = color(fence.accent);
        let style = TextStyle::ui(self.sf(12.0));
        let small = TextStyle::ui(self.sf(11.0));
        let radius = self.sf(11.0);
        let card = rect(layout.card);
        let outer = RectF::new(self.layout.nav_row.x as f32, self.layout.command_row.bottom() as f32,
            self.layout.nav_row.width as f32, (self.layout.top - self.layout.command_row.bottom()).max(0) as f32);
        if crate::edition::THEMED && self.mica {
            cv.clear_rect(outer);
            cv.fill(outer, color(self.palette.layer).with_alpha(0.18));
        } else { cv.fill(outer, color(self.palette.layer)); }
        // Opaque body and input keep small text legible on every backdrop.
        cv.round(RectF::new(card.x, card.y + self.sf(2.0), card.w, card.h), radius,
            Color::rgba8(0, 0, 0, if self.dark { 32 } else { 12 }));
        cv.round(card, radius, color(fence.background));
        cv.stroke_round(card, radius, color(fence.border), 1.0);
        let title = rect(layout.title);
        cv.glyph(glyph::SEARCH, RectF::new(title.x, title.y, self.sf(22.0), title.h), self.sf(16.0), accent);
        cv.text("搜索", RectF::new(title.x + self.sf(30.0), title.y, title.w - self.sf(30.0), title.h),
            &TextStyle::ui(self.sf(15.0)).weight(600), text);

        self.search_panel_button(cv, layout.scope, SearchPanelHit::Scope, true, &fence);
        let scope = controls.scope.as_ref().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|| "全部磁盘".into());
        let scope_rect = rect(layout.scope);
        cv.glyph(glyph::FOLDER, RectF::new(scope_rect.x + self.sf(6.0), scope_rect.y, self.sf(16.0), scope_rect.h), self.sf(13.0), secondary);
        cv.text(&scope, RectF::new(scope_rect.x + self.sf(28.0), scope_rect.y,
            (scope_rect.w - self.sf(49.0)).max(0.0), scope_rect.h), &small, secondary);
        cv.glyph(glyph::CHEVRON_DOWN, RectF::new(scope_rect.right() - self.sf(20.0), scope_rect.y,
            self.sf(14.0), scope_rect.h), self.sf(9.0), secondary);
        self.search_panel_button(cv, layout.close, SearchPanelHit::Close, true, &fence);
        cv.glyph(glyph::CLOSE_SMALL, rect(layout.close), self.sf(11.0), secondary);

        let focused = unsafe { GetFocus() } == self.search_edit;
        cv.round(rect(layout.query), self.sf(7.0), color(if focused { self.palette.input_focused } else { self.palette.input }));
        cv.stroke_round(rect(layout.query), self.sf(7.0), if focused { accent } else { color(self.palette.input_border) },
            if focused { self.sf(1.5) } else { 1.0 });
        self.search_panel_button(cv, layout.submit, SearchPanelHit::Submit, true, &fence);
        cv.glyph(glyph::SEARCH, rect(layout.submit), self.sf(16.0), accent);
        self.search_panel_button(cv, layout.clear, SearchPanelHit::Clear, true, &fence);
        cv.glyph(glyph::CLOSE_SMALL, rect(layout.clear), self.sf(12.0), secondary);

        for (index, chip) in layout.categories.iter().enumerate() {
            let Some(category) = SearchCategory::ALL.get(index).copied() else { continue };
            let selected = controls.category == category;
            let hit = SearchPanelHit::Category(index as u8);
            cv.round(rect(*chip), self.sf(14.0), color(if selected { fence.selected } else { fence.chip }));
            self.search_panel_button(cv, *chip, hit, true, &fence);
            if selected { cv.stroke_round(rect(*chip), self.sf(14.0), color(fence.selected_border), 1.0); }
            cv.text(category.label(), rect(*chip), &style.clone().align(Align::Center).weight(if selected { 600 } else { 400 }),
                if selected { accent } else { text });
        }

        let is_query = matches!(self.current_location(), ShellLocation::Search { .. });
        let status = if let Some(error) = &controls.error {
            format!("搜索未完成 · {}", error.lines().next().unwrap_or("请重试"))
        } else if controls.busy {
            "正在搜索…".to_owned()
        } else if controls.total > 0 {
            let first = if controls.shown == 0 { 0 } else { u64::from(controls.offset) + 1 };
            let last = (u64::from(controls.offset) + controls.shown as u64).min(controls.total);
            format!("共 {} 项 · 显示 {first}–{last}", controls.total)
        } else if is_query {
            "没有匹配项目 · 尝试其他关键词或搜索范围".to_owned()
        } else {
            if self.session.ui.search_backend.is_builtin(){"自带搜索：输入文件名、路径或类型筛选".to_owned()}else{"Everything 搜索：输入文件名、路径或搜索语法".to_owned()}
        };
        let status_color = if controls.error.is_some() {
            if self.dark { Color::rgba8(255, 180, 171, 255) } else { Color::rgba8(165, 38, 44, 255) }
        } else if controls.busy { accent } else { secondary };
        cv.text(&status, rect(layout.status), &small, status_color);
        if controls.error.is_some() {
            self.search_panel_button(cv, layout.retry, SearchPanelHit::Retry, !controls.busy, &fence);
            cv.text("重试", rect(layout.retry), &small.clone().align(Align::Center), accent);
        }

        self.search_panel_button(cv, layout.sort, SearchPanelHit::Sort, true, &fence);
        let sorting = rect(layout.sort);
        cv.glyph(glyph::SORT, RectF::new(sorting.x + self.sf(5.0), sorting.y, self.sf(18.0), sorting.h), self.sf(12.0), secondary);
        cv.text(sort_label(controls.sort), RectF::new(sorting.x + self.sf(29.0), sorting.y,
            (sorting.w - self.sf(46.0)).max(0.0), sorting.h), &small, text);
        cv.glyph(glyph::CHEVRON_DOWN, RectF::new(sorting.right() - self.sf(16.0), sorting.y, self.sf(12.0), sorting.h), self.sf(8.0), secondary);
        self.search_panel_button(cv, layout.index_settings, SearchPanelHit::IndexSettings, true, &fence);
        cv.text("索引设置", rect(layout.index_settings), &small.clone().align(Align::Center), secondary);
        let previous = controls.offset > 0 && !controls.busy;
        let next = u64::from(controls.offset) + (controls.shown as u64) < controls.total
            && controls.shown > 0 && !controls.busy;
        self.search_panel_button(cv, layout.previous, SearchPanelHit::Previous, previous, &fence);
        self.search_panel_button(cv, layout.next, SearchPanelHit::Next, next, &fence);
        cv.glyph(glyph::BACK, rect(layout.previous), self.sf(12.0), if previous { text } else { secondary.with_alpha(0.35) });
        cv.glyph(glyph::FORWARD, rect(layout.next), self.sf(12.0), if next { text } else { secondary.with_alpha(0.35) });
        let pages = controls.total.div_ceil(u64::from(SEARCH_PAGE_SIZE));
        let page = if pages == 0 { 0 } else { u64::from(controls.offset / SEARCH_PAGE_SIZE) + 1 };
        cv.text(&format!("{page} / {pages}"), rect(layout.page), &small.align(Align::Center), secondary);
    }

    fn search_panel_button(&self, cv: &Canvas, bounds: LayoutRect, action: SearchPanelHit, enabled: bool, palette: &FencePalette) {
        let hit = Hit::SearchPanel(action);
        if !enabled || self.hover != hit { return; }
        let radius = if matches!(action, SearchPanelHit::Category(_)) { 14.0 } else { 6.0 };
        cv.round(rect(bounds), self.sf(radius), color(if self.pressed == Some(hit) { palette.chip } else { palette.hover }));
    }

    pub(super) unsafe fn search_panel_action(&mut self, hit: SearchPanelHit) {
        match hit {
            SearchPanelHit::Category(index) => {
                let Some(category) = SearchCategory::ALL.get(index as usize).copied() else { return };
                self.search_controls_mut().category = category;
                unsafe { self.request_integrated_search(true); }
            }
            SearchPanelHit::Scope => unsafe { self.search_scope_menu(); },
            SearchPanelHit::Sort => unsafe { self.search_sort_menu(); },
            SearchPanelHit::Previous => {
                let controls = self.search_controls_mut();
                if controls.busy || controls.offset == 0 { return; }
                controls.offset = controls.offset.saturating_sub(SEARCH_PAGE_SIZE);
                unsafe { self.request_integrated_search(false); }
            }
            SearchPanelHit::Next => {
                let controls = self.search_controls_mut();
                if controls.busy || controls.shown == 0 || u64::from(controls.offset) + controls.shown as u64 >= controls.total { return; }
                controls.offset = controls.offset.saturating_add(SEARCH_PAGE_SIZE);
                unsafe { self.request_integrated_search(false); }
            }
            SearchPanelHit::Clear => {
                let _ = unsafe { SetWindowTextW(self.search_edit, windows::core::w!("")) };
                unsafe { self.request_integrated_search(true); let _ = SetFocus(Some(self.search_edit)); }
            }
            SearchPanelHit::Close => unsafe { self.close_integrated_search(); },
            SearchPanelHit::Submit => unsafe { self.request_integrated_search(true); },
            SearchPanelHit::Retry => {
                let controls = self.search_controls();
                if controls.error.is_some() && !controls.busy { unsafe { self.request_integrated_search(true); } }
            }
            SearchPanelHit::IndexSettings => {
                self.launch_search_settings(false);
            }
        }
        self.invalidate();
    }

    unsafe fn search_scope_menu(&mut self) {
        let controls = self.search_controls();
        let current = self.current_location().as_path().map(PathBuf::from)
            .or_else(|| self.search_scope.as_ref().and_then(ShellLocation::as_path).map(PathBuf::from));
        let spec = menus::menu(vec![
            menus::entry("全部磁盘", "all", true, controls.scope.is_none()),
            menus::entry("当前文件夹", "current", current.is_some(), current.is_some() && controls.scope == current),
            menus::item("选择文件夹…", "choose"),
        ]);
        let choice = super::input::popup(self.hwnd, &spec, Some(self.layout.search_panel.scope));
        let scope = match choice.as_deref() {
            Some("all") => None,
            Some("current") => { let Some(path) = current else { return }; Some(path) },
            Some("choose") => match choose_scope(self.hwnd) {
                Ok(Some(path)) => Some(path), Ok(None) => return,
                Err(error) => { self.show_message(format!("无法选择搜索文件夹：{error:#}")); return; }
            },
            _ => return,
        };
        self.search_controls_mut().scope = scope;
        unsafe { self.request_integrated_search(true); }
    }

    unsafe fn search_sort_menu(&mut self) {
        let selected = self.search_controls().sort;
        let choices = [(SearchSort::NameAsc, "name"), (SearchSort::NameDesc, "name-desc"),
            (SearchSort::ModifiedDesc, "modified"), (SearchSort::SizeDesc, "size")];
        let spec = menus::menu(choices.iter().map(|(sort, id)| menus::entry(sort_label(*sort), id, true, *sort == selected)).collect());
        let choice = super::input::popup(self.hwnd, &spec, Some(self.layout.search_panel.sort));
        if let Some((sort, _)) = choices.iter().find(|(_, id)| Some(*id) == choice.as_deref()) {
            self.search_controls_mut().sort = *sort;
            unsafe { self.request_integrated_search(true); }
        }
    }
}

fn choose_scope(owner: HWND) -> Result<Option<PathBuf>> {
    let dialog: IFileOpenDialog = unsafe { CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER) }?;
    unsafe {
        dialog.SetTitle(windows::core::w!("选择搜索范围"))?;
        dialog.SetOptions(dialog.GetOptions()? | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST)?;
    }
    if let Err(error) = unsafe { dialog.Show(Some(owner)) } {
        if error.code().0 as u32 == 0x800704c7 { return Ok(None); }
        return Err(error.into());
    }
    let item = unsafe { dialog.GetResult()? };
    let value = unsafe { item.GetDisplayName(SIGDN_FILESYSPATH)? };
    let path = unsafe { value.to_string() }.map(PathBuf::from);
    unsafe { CoTaskMemFree(Some(value.0.cast())); }
    Ok(Some(path?))
}
