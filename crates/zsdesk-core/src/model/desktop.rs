//! 桌面格子模型：格子配置、桌面项目分类与归属、布局吸附。
//!
//! 设计约束（规划 §6.2）：
//! - 桌面格子只保存项目引用（解析名），删除格子不会删除文件；
//! - 手动放入的项目优先于自动规则，规则未命中的项目进入“其他”格子，
//!   因而接管桌面图标后不会有项目“消失”；
//! - 文件夹映射格子直接显示真实目录内容，不复制、不移动文件。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::model::view::natural_cmp;

pub const CONFIG_VERSION: u32 = 1;
pub const MIN_FENCE_W: i32 = 160;
pub const MIN_FENCE_H: i32 = 96;
pub const TITLE_H: i32 = 38;

/// 桌面项目类别（一键整理使用）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Category { Apps, Folders, Documents, Images, Media, Archives, Others }

impl Category {
    pub const ALL: [Category; 7] = [Self::Apps, Self::Folders, Self::Documents, Self::Images, Self::Media, Self::Archives, Self::Others];
    pub const fn title(self) -> &'static str {
        match self {
            Self::Apps => "应用与快捷方式", Self::Folders => "文件夹", Self::Documents => "文档",
            Self::Images => "图片", Self::Media => "视频与音乐", Self::Archives => "压缩包", Self::Others => "其他",
        }
    }
}

/// Classifies a desktop item by its name and Shell attributes.
pub fn categorize(name: &str, is_folder: bool, is_file_system: bool) -> Category {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    const APPS: &[&str] = &["lnk", "url", "exe", "appref-ms", "bat", "cmd", "msi", "website", "pif", "com", "ps1"];
    const DOCS: &[&str] = &["doc", "docx", "docm", "dot", "dotx", "rtf", "odt", "wps", "wpt", "xls", "xlsx", "xlsm", "xlsb",
        "csv", "et", "ett", "ods", "ppt", "pptx", "pptm", "pps", "ppsx", "odp", "dps", "dpt", "pdf", "ofd", "txt", "md",
        "log", "xml", "json", "yaml", "yml", "ini", "htm", "html", "epub", "mobi", "caj", "vsd", "vsdx", "mpp", "one",
        "xmind", "pages", "numbers", "key", "tex"];
    const IMAGES: &[&str] = &["jpg", "jpeg", "jfif", "png", "gif", "bmp", "webp", "tif", "tiff", "svg", "ico", "heic",
        "heif", "avif", "raw", "cr2", "cr3", "nef", "arw", "dng", "psd", "ai", "cdr", "xcf"];
    const MEDIA: &[&str] = &["mp4", "mkv", "avi", "mov", "wmv", "flv", "webm", "m4v", "rmvb", "rm", "ts", "3gp", "mpg",
        "mpeg", "mp3", "wav", "flac", "aac", "ogg", "m4a", "wma", "ape", "opus", "mid", "midi", "amr"];
    const ARCHIVES: &[&str] = &["zip", "rar", "7z", "tar", "gz", "tgz", "bz2", "xz", "zst", "iso", "cab", "lz", "lzma", "arj", "wim"];
    if !is_file_system { return Category::Apps; }
    let has = |list: &[&str]| list.contains(&ext.as_str());
    // Compressed folders report SFGAO_FOLDER, so check archive extensions first.
    if has(ARCHIVES) { return Category::Archives; }
    if is_folder { return Category::Folders; }
    if has(APPS) { Category::Apps }
    else if has(DOCS) { Category::Documents }
    else if has(IMAGES) { Category::Images }
    else if has(MEDIA) { Category::Media }
    else { Category::Others }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FenceSource {
    /// 桌面视觉分组：显示桌面项目的引用。
    Desktop,
    /// 文件夹映射：显示一个真实目录的内容。
    Folder { root: PathBuf },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum FenceView { #[default] Icons, List }

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum IconSize { Small, #[default] Medium, Large }

impl IconSize {
    pub const fn pixels(self) -> i32 { match self { Self::Small => 32, Self::Medium => 48, Self::Large => 64 } }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum FenceSort { #[default] Manual, Name, Type, Size, Modified }

/// 位置：相对显示器工作区左上角的 DIP 坐标。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Placement {
    pub monitor: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fence {
    pub id: u64,
    pub title: String,
    pub source: FenceSource,
    pub placement: Placement,
    #[serde(default)] pub collapsed: bool,
    #[serde(default)] pub locked: bool,
    #[serde(default)] pub view: FenceView,
    #[serde(default)] pub icon_size: IconSize,
    #[serde(default)] pub sort: FenceSort,
    #[serde(default)] pub descending: bool,
    /// 手动放入及手动排序的项目（解析名）。
    #[serde(default)] pub members: Vec<String>,
    /// 自动收取的类别。
    #[serde(default)] pub rule: Option<Category>,
    /// 规则未命中的桌面项目进入此格子。
    #[serde(default)] pub catch_all: bool,
    /// 文件夹格子当前浏览的子目录（None 表示根目录）。
    #[serde(default)] pub browse: Option<PathBuf>,
}

impl Fence {
    pub fn new(id: u64, title: impl Into<String>, source: FenceSource, placement: Placement) -> Self {
        let sort = if source == FenceSource::Desktop { FenceSort::Manual } else { FenceSort::Name };
        Self { id, title: title.into(), source, placement, collapsed: false, locked: false, view: FenceView::Icons,
            icon_size: IconSize::Medium, sort, descending: false, members: Vec::new(), rule: None, catch_all: false, browse: None }
    }
    pub fn is_desktop(&self) -> bool { self.source == FenceSource::Desktop }
    pub fn folder(&self) -> Option<&PathBuf> {
        match &self.source { FenceSource::Folder { root } => Some(self.browse.as_ref().unwrap_or(root)), _ => None }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum FenceTheme { #[default] Dark, Light, System }

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesktopSettings {
    /// 整理模式：隐藏系统桌面图标，由格子显示全部桌面项目。
    pub organize: bool,
    /// 由 ZSDesk 隐藏了系统桌面图标；异常退出后由守护进程据此还原。
    pub icons_hidden_by_zsdesk: bool,
    pub hidden: bool,
    pub double_click_toggle: bool,
    pub theme: FenceTheme,
    pub opacity: u8,
    /// 文件夹格子内双击子文件夹时在格子内打开。
    pub browse_inside: bool,
    /// 鼠标悬停时临时展开折叠的格子，移开后自动收起。
    #[serde(default = "default_true")]
    pub hover_expand: bool,
    /// 隐藏通知区域（托盘）图标。
    #[serde(default)]
    pub hide_tray: bool,
    /// Show the item count in a fence header; hidden by default.
    #[serde(default)]
    pub show_counts: bool,
    /// Use the immediate ZSUI menu; the full Shell context menu opens on demand.
    #[serde(default)]
    pub zsui_fast_context_menu: bool,
}

fn default_true() -> bool { true }

impl Default for DesktopSettings {
    fn default() -> Self {
        Self { organize: false, icons_hidden_by_zsdesk: false, hidden: false, double_click_toggle: true,
            theme: FenceTheme::Dark, opacity: 55, browse_inside: true, hover_expand: true, hide_tray: false, show_counts: false, zsui_fast_context_menu: false }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesktopConfig {
    pub version: u32,
    #[serde(default)] pub settings: DesktopSettings,
    #[serde(default)] pub fences: Vec<Fence>,
    /// 退出整理模式时保存的桌面格子，再次整理时恢复。
    #[serde(default)] pub stash: Vec<Fence>,
    pub next_id: u64,
}

impl Default for DesktopConfig {
    fn default() -> Self { Self { version: CONFIG_VERSION, settings: DesktopSettings::default(), fences: Vec::new(), stash: Vec::new(), next_id: 1 } }
}

impl DesktopConfig {
    pub fn take_id(&mut self) -> u64 { let id = self.next_id; self.next_id += 1; id }
    pub fn normalize(&mut self) {
        let largest = self.fences.iter().chain(&self.stash).map(|f| f.id).max().unwrap_or(0);
        self.next_id = self.next_id.max(largest + 1);
        for fence in self.fences.iter_mut().chain(self.stash.iter_mut()) {
            fence.placement.width = fence.placement.width.max(MIN_FENCE_W);
            fence.placement.height = fence.placement.height.max(MIN_FENCE_H);
            fence.members.dedup();
            fence.members.truncate(5000);
        }
        self.settings.opacity = self.settings.opacity.clamp(10, 100);
    }
    pub fn fence(&self, id: u64) -> Option<&Fence> { self.fences.iter().find(|f| f.id == id) }
    pub fn fence_mut(&mut self, id: u64) -> Option<&mut Fence> { self.fences.iter_mut().find(|f| f.id == id) }

    /// Puts `keys` into fence `target` (manual membership), removing them from other desktop fences.
    /// `before` inserts ahead of an existing member to support manual reordering.
    pub fn assign(&mut self, target: u64, keys: &[String], before: Option<&str>) {
        for fence in self.fences.iter_mut().filter(|f| f.is_desktop()) {
            fence.members.retain(|m| !keys.contains(m));
        }
        let Some(fence) = self.fence_mut(target).filter(|f| f.is_desktop()) else { return };
        let at = before.and_then(|b| fence.members.iter().position(|m| m == b)).unwrap_or(fence.members.len());
        for (offset, key) in keys.iter().enumerate() { fence.members.insert(at + offset, key.clone()); }
    }

    /// Updates manual memberships after a rename.
    pub fn rename_member(&mut self, old: &str, new: &str) -> bool {
        let mut changed = false;
        for fence in self.fences.iter_mut() {
            for member in fence.members.iter_mut().filter(|m| m.eq_ignore_ascii_case(old)) { *member = new.to_string(); changed = true; }
        }
        changed
    }
}

/// Minimal description of a Shell item for assignment and sorting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemInfo {
    pub key: String,
    pub name: String,
    pub is_folder: bool,
    pub category: Category,
    pub size: Option<u64>,
    pub modified: Option<u64>,
}

/// Distributes desktop items into desktop fences. Returns, per fence in
/// `fences` order, the indices into `items` it displays (already ordered).
/// Priority: manual membership → category rule → catch-all fence.
pub fn distribute(fences: &[Fence], items: &[ItemInfo]) -> Vec<Vec<usize>> {
    let mut result = vec![Vec::new(); fences.len()];
    for (item_index, item) in items.iter().enumerate() {
        let manual = fences.iter().position(|f| f.is_desktop() && f.members.iter().any(|m| m.eq_ignore_ascii_case(&item.key)));
        let ruled = || fences.iter().position(|f| f.is_desktop() && f.rule == Some(item.category));
        let fallback = || fences.iter().position(|f| f.is_desktop() && f.catch_all);
        if let Some(fence) = manual.or_else(ruled).or_else(fallback) { result[fence].push(item_index); }
    }
    for (fence, indices) in fences.iter().zip(result.iter_mut()) { sort_items(fence, items, indices); }
    result
}

/// Orders `indices` according to the fence sort mode (folders first except in manual mode).
pub fn sort_items(fence: &Fence, items: &[ItemInfo], indices: &mut [usize]) {
    let member_rank = |key: &str| fence.members.iter().position(|m| m.eq_ignore_ascii_case(key)).unwrap_or(usize::MAX);
    let ext = |name: &str| name.rsplit_once('.').map(|(_, e)| e.to_lowercase()).unwrap_or_default();
    indices.sort_by(|&a, &b| {
        let (x, y) = (&items[a], &items[b]);
        let order = match fence.sort {
            FenceSort::Manual => member_rank(&x.key).cmp(&member_rank(&y.key))
                .then_with(|| y.is_folder.cmp(&x.is_folder)).then_with(|| natural_cmp(&x.name, &y.name)),
            _ => {
                let by = match fence.sort {
                    FenceSort::Type => ext(&x.name).cmp(&ext(&y.name)),
                    FenceSort::Size => x.size.cmp(&y.size),
                    FenceSort::Modified => x.modified.cmp(&y.modified),
                    _ => std::cmp::Ordering::Equal,
                }.then_with(|| natural_cmp(&x.name, &y.name));
                y.is_folder.cmp(&x.is_folder).then(if fence.descending { by.reverse() } else { by })
            }
        };
        order
    });
}

/// Integer rectangle used for snapping (pixels or DIP; caller decides).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Bounds { pub x: i32, pub y: i32, pub w: i32, pub h: i32 }

impl Bounds {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self { Self { x, y, w, h } }
    pub fn right(&self) -> i32 { self.x + self.w }
    pub fn bottom(&self) -> i32 { self.y + self.h }
}

/// Which edges are being dragged.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Edges { pub left: bool, pub top: bool, pub right: bool, pub bottom: bool }

impl Edges {
    pub const MOVE: Self = Self { left: true, top: true, right: true, bottom: true };
    pub fn is_move(&self) -> bool { *self == Self::MOVE }
}

/// Snaps a moved or resized rectangle to the work area and to other fences.
/// `gap` is the preferred spacing between neighbouring fences.
pub fn snap(rect: Bounds, edges: Edges, others: &[Bounds], area: Bounds, threshold: i32, gap: i32) -> Bounds {
    let mut xs = vec![area.x, area.right()];
    let mut ys = vec![area.y, area.bottom()];
    for o in others {
        let overlaps_y = rect.y < o.bottom() + threshold && rect.bottom() > o.y - threshold;
        let overlaps_x = rect.x < o.right() + threshold && rect.right() > o.x - threshold;
        // Edges always align; the spacing targets apply only to direct neighbours.
        xs.extend([o.x, o.right()]);
        ys.extend([o.y, o.bottom()]);
        if overlaps_y { xs.extend([o.x - gap, o.right() + gap]); }
        if overlaps_x { ys.extend([o.y - gap, o.bottom() + gap]); }
    }
    let best = |value: i32, targets: &[i32]| targets.iter().map(|t| t - value).filter(|d| d.abs() <= threshold).min_by_key(|d| d.abs());
    let mut r = rect;
    if edges.is_move() {
        let dx = [best(r.x, &xs), best(r.right(), &xs)].into_iter().flatten().min_by_key(|d| d.abs());
        let dy = [best(r.y, &ys), best(r.bottom(), &ys)].into_iter().flatten().min_by_key(|d| d.abs());
        r.x += dx.unwrap_or(0);
        r.y += dy.unwrap_or(0);
        // Keep the whole fence inside the work area.
        r.x = r.x.clamp(area.x, (area.right() - r.w).max(area.x));
        r.y = r.y.clamp(area.y, (area.bottom() - r.h).max(area.y));
        return r;
    }
    if edges.left { if let Some(d) = best(r.x, &xs) { r.x += d; r.w -= d; } }
    if edges.right { if let Some(d) = best(r.right(), &xs) { r.w += d; } }
    if edges.top { if let Some(d) = best(r.y, &ys) { r.y += d; r.h -= d; } }
    if edges.bottom { if let Some(d) = best(r.bottom(), &ys) { r.h += d; } }
    r
}

/// Applies a pointer delta to `start` for the dragged `edges`, honouring minimum size.
pub fn drag_rect(start: Bounds, edges: Edges, dx: i32, dy: i32, min_w: i32, min_h: i32) -> Bounds {
    if edges.is_move() { return Bounds { x: start.x + dx, y: start.y + dy, ..start }; }
    let mut r = start;
    if edges.left { let w = (start.w - dx).max(min_w); r.x = start.right() - w; r.w = w; }
    if edges.right { r.w = (start.w + dx).max(min_w); }
    if edges.top { let h = (start.h - dy).max(min_h); r.y = start.bottom() - h; r.h = h; }
    if edges.bottom { r.h = (start.h + dy).max(min_h); }
    r
}

/// Grid placement for freshly organized fences inside a work area (DIP).
pub fn auto_layout(count: usize, area_w: i32, area_h: i32) -> Vec<Bounds> {
    const MARGIN: i32 = 24;
    const GAP: i32 = 16;
    let w = 340.min((area_w - MARGIN * 2).max(MIN_FENCE_W));
    let columns = (((area_w - MARGIN * 2 + GAP) / (w + GAP)).max(1)) as usize;
    let rows = count.div_ceil(columns).max(1) as i32;
    let h = ((area_h - MARGIN * 2 - GAP * (rows - 1)) / rows).clamp(MIN_FENCE_H, 300);
    (0..count).map(|i| {
        let (c, r) = ((i % columns) as i32, (i / columns) as i32);
        Bounds::new(MARGIN + c * (w + GAP), MARGIN + r * (h + GAP), w, h)
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn existing_profiles_hide_counts_until_enabled() {
        let mut value = serde_json::to_value(DesktopConfig::default()).unwrap();
        value["settings"].as_object_mut().unwrap().remove("show_counts");
        let mut config: DesktopConfig = serde_json::from_value(value).unwrap();
        assert!(!config.settings.show_counts);
        config.settings.show_counts = true;
        let restored: DesktopConfig = serde_json::from_slice(&serde_json::to_vec(&config).unwrap()).unwrap();
        assert!(restored.settings.show_counts);
    }
    #[test]
    fn old_profiles_default_to_native_shell_context_menus() {
        let mut value = serde_json::to_value(DesktopConfig::default()).unwrap();
        value["settings"].as_object_mut().unwrap().remove("zsui_fast_context_menu");
        let mut config: DesktopConfig = serde_json::from_value(value).unwrap();
        assert!(!config.settings.zsui_fast_context_menu);
        config.settings.zsui_fast_context_menu = true;
        let restored: DesktopConfig = serde_json::from_slice(&serde_json::to_vec(&config).unwrap()).unwrap();
        assert!(restored.settings.zsui_fast_context_menu);
    }

    fn info(key: &str, folder: bool) -> ItemInfo {
        let name = key.rsplit('\\').next().unwrap().to_string();
        ItemInfo { key: key.into(), category: categorize(&name, folder, true), name, is_folder: folder, size: None, modified: None }
    }
    fn fence(id: u64, rule: Option<Category>, catch_all: bool) -> Fence {
        let mut f = Fence::new(id, "f", FenceSource::Desktop, Placement::default());
        f.rule = rule; f.catch_all = catch_all; f
    }

    #[test]
    fn categories_follow_extensions_and_attributes() {
        assert_eq!(categorize("报告.DOCX", false, true), Category::Documents);
        assert_eq!(categorize("资料.zip", true, true), Category::Archives);
        assert_eq!(categorize("项目", true, true), Category::Folders);
        assert_eq!(categorize("微信.lnk", false, true), Category::Apps);
        assert_eq!(categorize("此电脑", true, false), Category::Apps);
        assert_eq!(categorize("photo.HEIC", false, true), Category::Images);
        assert_eq!(categorize("README", false, true), Category::Others);
    }

    #[test]
    fn manual_membership_beats_rules_and_nothing_is_lost() {
        let mut docs = fence(1, Some(Category::Documents), false);
        let other = fence(2, None, true);
        docs.members.clear();
        let mut manual = fence(3, None, false);
        manual.members.push(r"C:\D\a.docx".into());
        let fences = vec![docs, other, manual];
        let items = vec![info(r"C:\D\a.docx", false), info(r"C:\D\b.docx", false), info(r"C:\D\c.bin", false), info(r"C:\D\Dir", true)];
        let out = distribute(&fences, &items);
        assert_eq!(out[2], vec![0]);
        assert_eq!(out[0], vec![1]);
        let mut rest = out[1].clone(); rest.sort();
        assert_eq!(rest, vec![2, 3]);
        assert_eq!(out.iter().map(Vec::len).sum::<usize>(), items.len());
    }

    #[test]
    fn folder_fences_never_claim_desktop_items() {
        let mut folder = Fence::new(1, "f", FenceSource::Folder { root: "C:\\x".into() }, Placement::default());
        folder.catch_all = true;
        let out = distribute(&[folder], &[info(r"C:\D\a.txt", false)]);
        assert!(out[0].is_empty());
    }

    #[test]
    fn assign_moves_between_fences_and_reorders() {
        let mut config = DesktopConfig { fences: vec![fence(1, None, false), fence(2, None, false)], ..Default::default() };
        config.assign(1, &["a".into(), "b".into()], None);
        config.assign(2, &["a".into()], None);
        assert_eq!(config.fence(1).unwrap().members, vec!["b".to_string()]);
        config.assign(1, &["c".into()], Some("b"));
        assert_eq!(config.fence(1).unwrap().members, vec!["c".to_string(), "b".to_string()]);
        assert!(config.rename_member("C", "d"));
        assert_eq!(config.fence(1).unwrap().members[0], "d");
    }

    #[test]
    fn manual_sort_keeps_member_order_then_folders() {
        let mut f = fence(1, None, true);
        f.members = vec!["z.txt".into(), "a.txt".into()];
        let items = vec![info("a.txt", false), info("z.txt", false), info("m", true), info("b.txt", false)];
        let mut idx = vec![0, 1, 2, 3];
        sort_items(&f, &items, &mut idx);
        assert_eq!(idx, vec![1, 0, 2, 3]);
        f.sort = FenceSort::Name; f.descending = true;
        sort_items(&f, &items, &mut idx);
        assert_eq!(idx, vec![2, 1, 3, 0]);
    }

    #[test]
    fn snapping_aligns_to_neighbours_and_work_area() {
        let area = Bounds::new(0, 0, 1920, 1040);
        let other = Bounds::new(100, 100, 300, 200);
        let moved = snap(Bounds::new(413, 105, 200, 150), Edges::MOVE, &[other], area, 10, 12);
        assert_eq!((moved.x, moved.y), (412, 100));
        let edge = snap(Bounds::new(4, 1000, 200, 150), Edges::MOVE, &[], area, 10, 12);
        assert_eq!((edge.x, edge.bottom()), (0, 1040));
        let resized = snap(Bounds::new(100, 400, 295, 100), Edges { right: true, ..Default::default() }, &[other], area, 10, 12);
        assert_eq!(resized.right(), 400);
    }

    #[test]
    fn drag_rect_respects_minimum_size() {
        let r = drag_rect(Bounds::new(100, 100, 200, 200), Edges { left: true, top: true, ..Default::default() }, 500, 500, 160, 96);
        assert_eq!((r.w, r.h, r.right(), r.bottom()), (160, 96, 300, 300));
        let m = drag_rect(Bounds::new(0, 0, 10, 10), Edges::MOVE, 5, -3, 1, 1);
        assert_eq!((m.x, m.y, m.w), (5, -3, 10));
    }

    #[test]
    fn auto_layout_fits_work_area() {
        let rects = auto_layout(7, 1920, 1040);
        assert_eq!(rects.len(), 7);
        assert!(rects.iter().all(|r| r.right() <= 1920 && r.bottom() <= 1040 && r.w >= MIN_FENCE_W));
        for (i, a) in rects.iter().enumerate() {
            for b in &rects[i + 1..] { assert!(a.right() <= b.x || b.right() <= a.x || a.bottom() <= b.y || b.bottom() <= a.y); }
        }
    }

    #[test]
    fn config_round_trips_and_normalizes() {
        let mut config = DesktopConfig::default();
        let id = config.take_id();
        config.fences.push(Fence::new(id, "文档", FenceSource::Desktop, Placement { width: 10, height: 10, ..Default::default() }));
        let text = serde_json::to_string(&config).unwrap();
        let mut back: DesktopConfig = serde_json::from_str(&text).unwrap();
        back.next_id = 0;
        back.normalize();
        assert_eq!(back.next_id, 2);
        assert_eq!(back.fences[0].placement.width, MIN_FENCE_W);
    }
}
