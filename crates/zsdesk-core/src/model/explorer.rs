//! Explorer-compatible locations, tabs, bookmarks, and session state.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ShellLocation {
    ThisPc,
    Path(PathBuf),
    Shell { parsing_name: String, title: String,
        /// Namespace identities (including MTP objects) need not round-trip
        /// through a filesystem path or provider display name.
        #[serde(default,skip_serializing_if="Option::is_none")]
        pidl:Option<Vec<u8>>,
    },
    /// Stable query parameters; transient Shell search addresses are not persisted.
    Search { query: String, scope: Option<PathBuf> },
}
impl ShellLocation {
    pub fn title(&self) -> String {
        match self {
            Self::ThisPc => "此电脑".to_string(),
            Self::Path(path) => path_title(path),
            Self::Shell { parsing_name, title, .. } => shell_display_title(parsing_name, title),
            Self::Search { query, .. } => format!("搜索：{query}"),
        }
    }

    pub fn address(&self) -> String {
        match self {
            Self::ThisPc => "此电脑".to_string(),
            Self::Path(path) => path.to_string_lossy().into_owned(),
            Self::Shell { parsing_name, title, .. } => if parsing_name.trim().is_empty(){shell_display_title(parsing_name,title)}else{parsing_name.clone()},
            Self::Search { query, scope } => format!("搜索 {}：{}", scope.as_ref().map(|p|p.display().to_string()).unwrap_or_else(||"全部本地磁盘".into()), query),
        }
    }

    pub fn as_path(&self) -> Option<&Path> {
        match self {
            Self::Path(path) => Some(path),
            _ => None,
        }
    }
}

fn shell_display_title(parsing_name: &str, title: &str) -> String {
    let title = title.trim();
    if !title.is_empty() && !title.eq_ignore_ascii_case("shell") && !title.starts_with("::{")
        && !title.to_ascii_lowercase().starts_with("shell:") && !title.eq_ignore_ascii_case(parsing_name) {
        return title.to_string();
    }
    let name = parsing_name.get(..6).filter(|prefix| prefix.eq_ignore_ascii_case("shell:"))
        .map_or(parsing_name, |_| &parsing_name[6..]);
    let known = [
        ("::{645FF040-5081-101B-9F08-00AA002F954E}", "回收站"), ("RecycleBinFolder", "回收站"),
        ("::{20D04FE0-3AEA-1069-A2D8-08002B30309D}", "此电脑"), ("MyComputerFolder", "此电脑"),
        ("::{F02C1A0D-BE21-4350-88B0-7367FC96EF3C}", "网络"), ("NetworkPlacesFolder", "网络"),
        ("::{679F85CB-0220-4080-B29B-5540CC05AAB6}", "主文件夹"),
        ("::{F874310E-B6B7-47DC-BC84-B9E6B38F5903}", "主文件夹"),
        ("::{031E4825-7B94-4DC3-B131-E946B44C8DD5}", "库"), ("Libraries", "库"),
        ("::{26EE0668-A00A-44D7-9371-BEB064C98683}", "控制面板"), ("ControlPanelFolder", "控制面板"),
        ("Desktop", "桌面"), ("Downloads", "下载"),
    ];
    known.into_iter().find(|(candidate, _)| name.eq_ignore_ascii_case(candidate))
        .map_or("系统位置", |(_, label)| label).to_string()
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExplorerTab {
    pub id: u64,
    #[serde(default)]
    pub locked: bool,
    history: Vec<ShellLocation>,
    history_index: usize,
    #[serde(default)]
    pub view: Option<ShellViewState>,
}

impl ExplorerTab {
    pub fn new(id: u64, location: ShellLocation) -> Self {
        Self {
            id,
            locked: false,
            history: vec![location],
            history_index: 0,
            view: None,
        }
    }

    pub fn current(&self) -> &ShellLocation {
        self.history
            .get(self.history_index)
            .unwrap_or(&self.history[0])
    }

    pub fn navigate(&mut self, location: ShellLocation) -> bool {
        if self.current() == &location {
            return false;
        }
        self.history.truncate(self.history_index + 1);
        self.history.push(location);
        if self.history.len() > 100 { self.history.remove(0); }
        self.history_index = self.history.len() - 1;
        self.view = None;
        true
    }

    pub fn replace_current(&mut self, location: ShellLocation) {
        self.normalize();
        self.history[self.history_index] = location;
    }

    pub fn can_go_back(&self) -> bool {
        self.history_index > 0
    }

    pub fn can_go_forward(&self) -> bool {
        self.history_index + 1 < self.history.len()
    }

    pub fn go_back(&mut self) -> Option<&ShellLocation> {
        if !self.can_go_back() {
            return None;
        }
        self.history_index -= 1;
        Some(self.current())
    }

    pub fn go_forward(&mut self) -> Option<&ShellLocation> {
        if !self.can_go_forward() {
            return None;
        }
        self.history_index += 1;
        Some(self.current())
    }

    pub fn normalize(&mut self) {
        if self.history.is_empty() {
            self.history.push(ShellLocation::ThisPc);
        }
        self.history_index = self.history_index.min(self.history.len() - 1);
    }
}

/// Serializable Shell property identity, independent of COM object lifetimes.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ViewPropertyKey { pub format_id: String, pub property_id: u32 }

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ViewSortColumn { pub key: ViewPropertyKey, pub ascending: bool }

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ViewColumn { pub key: ViewPropertyKey, pub width: u32 }

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ViewGrouping { pub key: Option<ViewPropertyKey>, pub ascending: bool }

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShellViewState {
    pub mode: i32,
    pub icon_size: i32,
    pub selected: Vec<Vec<u8>>,
    pub focused: Option<Vec<u8>>,
    pub first_visible: Option<Vec<u8>>,
    #[serde(default)]
    pub sort_columns: Vec<ViewSortColumn>,
    #[serde(default)]
    pub columns: Vec<ViewColumn>,
    #[serde(default)]
    pub grouping: Option<ViewGrouping>,
    #[serde(default)]
    pub column_dpi: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Bookmark {
    pub id: u64,
    pub display_name: String,
    pub target: ShellLocation,
    /// Virtual organization only: this never creates a filesystem directory.
    #[serde(default)]
    pub is_folder: bool,
    #[serde(default)]
    pub parent_id: Option<u64>,
    #[serde(default)]
    pub collapsed: bool,
}

impl Bookmark {
    pub fn link(id:u64,display_name:String,target:ShellLocation,parent_id:Option<u64>)->Self {
        Self{id,display_name,target,is_folder:false,parent_id,collapsed:false}
    }
    pub fn folder(id:u64,display_name:String,parent_id:Option<u64>)->Self {
        Self{id,display_name,target:ShellLocation::ThisPc,is_folder:true,parent_id,collapsed:false}
    }
}

/// Restored window rectangle in screen pixels.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WindowPlacement {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub maximized: bool,
}

/// Individually configurable actions in the Explorer command bar.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum CommandBarItem { New, Cut, Copy, Paste, Rename, Share, Delete, Sort, View, More, PreviewPane, DetailsPane }

impl CommandBarItem {
    pub const ALL: [Self; 11] = [Self::New, Self::Cut, Self::Copy, Self::Paste, Self::Rename, Self::Share,
        Self::Delete, Self::Sort, Self::View, Self::More, Self::PreviewPane];

    pub const fn key(self) -> &'static str {
        match self {
            Self::New => "new", Self::Cut => "cut", Self::Copy => "copy", Self::Paste => "paste",
            Self::Rename => "rename", Self::Share => "share", Self::Delete => "delete", Self::Sort => "sort",
            Self::View => "view", Self::More => "more", Self::PreviewPane => "preview-pane", Self::DetailsPane => "details-pane",
        }
    }

    pub fn from_key(value: &str) -> Option<Self> { Self::ALL.into_iter().find(|item| item.key() == value) }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct CommandBarPreferences {
    pub visible: bool,
    pub hidden_items: Vec<CommandBarItem>,
}

impl Default for CommandBarPreferences {
    fn default() -> Self { Self { visible: true, hidden_items: Vec::new() } }
}

impl CommandBarPreferences {
    pub fn item_visible(&self, item: CommandBarItem) -> bool { item != CommandBarItem::DetailsPane && !self.hidden_items.contains(&item) }
    pub fn set_item_visible(&mut self, item: CommandBarItem, visible: bool) {
        self.hidden_items.retain(|hidden| *hidden != item);
        if !visible { self.hidden_items.push(item); }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum ContextMenuMode {
    #[serde(rename = "original")]
    Original,
    #[default]
    #[serde(rename = "windows11")]
    Windows11,
}

/// Menu visibility does not disable commands or keyboard shortcuts.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ContextMenuPreferences {
    pub mode: ContextMenuMode,
    pub show_cut_copy_paste: bool,
    pub show_copy_path: bool,
    pub show_rename: bool,
    pub show_delete: bool,
    pub show_properties: bool,
    pub show_share: bool,
    pub show_new: bool,
    pub show_seven_zip: bool,
    pub show_wps: bool,
}

impl Default for ContextMenuPreferences {
    fn default() -> Self {
        Self { mode: ContextMenuMode::Windows11, show_cut_copy_paste: true, show_copy_path: true,
            show_rename: true, show_delete: true, show_properties: true, show_share: true,
            show_new: true, show_seven_zip: true, show_wps: false }
    }
}

/// Legacy variants remain readable so existing profiles migrate without data loss.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SearchBackendPreference { Auto, #[default] NativeNtfs, DirectoryScan, Everything }
impl SearchBackendPreference {
    pub fn is_builtin(self)->bool {self!=Self::Everything}
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ColorScheme { #[default] System, Light, Dark }

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BackdropStyle { #[default] System, Solid, Mica, Acrylic }

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all="snake_case")]
pub enum TabPlacement { #[default] Top, Left, Right }

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PreviewProviderPreference { #[default] System, Office, Wps }

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct AppearancePreferences { pub color_scheme: ColorScheme, pub backdrop: BackdropStyle }

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct QuickSearchPreferences {
    pub enabled: bool,
    pub start_with_windows: bool,
    pub hotkey: String,
}
impl Default for QuickSearchPreferences {
    fn default() -> Self { Self { enabled: false, start_with_windows: false, hotkey: "Ctrl+Alt+Space".into() } }
}

/// Chrome preferences shared by every tab.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ExplorerUi {
    pub startup_location: StartupLocation,
    pub tab_placement: TabPlacement,
    pub dual_pane: bool,
    /// Split position in thousandths, independent of monitor scaling.
    pub pane_ratio: u16,
    pub double_click_close_tab: bool,
    pub close_window_on_last_tab: bool,
    pub show_tab_count: bool,
    pub mouse_gestures: bool,
    /// Uses indexed metadata only; never starts a directory-size scan.
    pub everything_folder_sizes: bool,
    pub show_tree: bool,
    /// Navigation tree width in DIP.
    pub tree_width: i32,
    pub show_bookmarks: bool,
    pub show_preview: bool,
    pub show_details: bool,
    pub command_bar: CommandBarPreferences,
    pub context_menu: ContextMenuPreferences,
    pub search_all_files: bool,
    pub search_backend: SearchBackendPreference,
    pub search_include_development: bool,
    pub search_index_metadata: bool,
    pub search_pinyin_initials: bool,
    pub search_idle_seconds: u32,
    pub everything_executable: Option<PathBuf>,
    pub space_preview: bool,
    pub hover_preview: bool,
    /// Double-clicking a .zip/.cab runs its default program instead of
    /// browsing it as a folder in ZSDesk.
    pub archives_open_externally: bool,
    pub forward_unhandled_windows: bool,
    pub preview_provider: PreviewProviderPreference,
    pub navigation_auto_expand: Option<bool>,
    pub navigation_show_recycle_bin: bool,
    pub navigation_show_network: bool,
    pub navigation_expand_this_pc: Option<bool>,
    pub appearance: AppearancePreferences,
    pub window: Option<WindowPlacement>,
}

#[derive(Clone,Debug,PartialEq,Eq)]
pub struct SearchPreferences {
    pub backend:SearchBackendPreference,pub include_development:bool,pub index_metadata:bool,pub pinyin_initials:bool,
    pub idle_seconds:u32,pub everything_executable:Option<PathBuf>,pub folder_sizes:bool,
}
impl ExplorerUi{
    pub fn search_preferences(&self)->SearchPreferences{SearchPreferences{backend:self.search_backend,include_development:self.search_include_development,
        index_metadata:self.search_index_metadata,pinyin_initials:self.search_pinyin_initials,idle_seconds:self.search_idle_seconds,
        everything_executable:self.everything_executable.clone(),folder_sizes:self.everything_folder_sizes}}
    pub fn apply_search_preferences(&mut self,prefs:&SearchPreferences){self.search_backend=prefs.backend;self.search_include_development=prefs.include_development;
        self.search_index_metadata=prefs.index_metadata;self.search_pinyin_initials=prefs.pinyin_initials;self.search_idle_seconds=prefs.idle_seconds;
        self.everything_executable=prefs.everything_executable.clone();self.everything_folder_sizes=prefs.folder_sizes;}
}

impl Default for ExplorerUi {
    fn default() -> Self {
        Self { startup_location:StartupLocation::default(), tab_placement:TabPlacement::Top, dual_pane:false, pane_ratio:500,
            double_click_close_tab:true, mouse_gestures:true, everything_folder_sizes:false,
            close_window_on_last_tab:true,show_tab_count:true,
            show_tree: true, tree_width: 220, show_bookmarks: true, show_preview: false, show_details: false,
            command_bar: CommandBarPreferences::default(), context_menu: ContextMenuPreferences::default(),
            search_all_files: true, search_backend: SearchBackendPreference::NativeNtfs, space_preview: true,
            search_include_development:false,search_index_metadata:false,search_pinyin_initials:false,search_idle_seconds:300,
            everything_executable:None,
            hover_preview: false, archives_open_externally: false, preview_provider: PreviewProviderPreference::System,
            forward_unhandled_windows: true,
            navigation_auto_expand: None,navigation_show_recycle_bin:true,navigation_show_network:true,navigation_expand_this_pc:None,
            appearance: AppearancePreferences::default(),
            window: None }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum StartupLocation {
    #[default]
    RestoreSession,
    ThisPc,
    Folder(PathBuf),
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ExplorerPaneSession {
    pub tabs: Vec<ExplorerTab>,
    pub active_tab: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExplorerSession {
    pub bookmarks: Vec<Bookmark>,
    pub tabs: Vec<ExplorerTab>,
    pub active_tab: usize,
    pub next_id: u64,
    #[serde(default)]
    pub secondary_pane: ExplorerPaneSession,
    #[serde(default)]
    pub ui: ExplorerUi,
}

impl Default for ExplorerSession {
    fn default() -> Self {
        Self {
            bookmarks: Vec::new(),
            tabs: vec![ExplorerTab::new(1, ShellLocation::ThisPc)],
            active_tab: 0,
            next_id: 2,
            secondary_pane: ExplorerPaneSession::default(),
            ui: ExplorerUi::default(),
        }
    }
}

impl ExplorerSession {
    /// Applied only when opening a window without an explicit Shell target.
    pub fn apply_startup_location(&mut self) {
        let location=match &self.ui.startup_location {
            StartupLocation::RestoreSession=>return,
            StartupLocation::ThisPc=>ShellLocation::ThisPc,
            StartupLocation::Folder(path)=>ShellLocation::Path(path.clone()),
        };
        let id=self.take_id();self.tabs=vec![ExplorerTab::new(id,location)];self.active_tab=0;
        self.secondary_pane=ExplorerPaneSession::default();
    }

    pub fn normalize(&mut self) {
        if self.ui.search_backend.is_builtin(){self.ui.search_backend=SearchBackendPreference::NativeNtfs;}
        self.ui.search_idle_seconds=self.ui.search_idle_seconds.clamp(30,3600);
        if self.tabs.is_empty() {
            let id = self.take_id();
            self.tabs.push(ExplorerTab::new(id, ShellLocation::ThisPc));
        }
        for tab in &mut self.tabs {
            tab.normalize();
        }
        for tab in &mut self.secondary_pane.tabs { tab.normalize(); }
        self.secondary_pane.active_tab = self.secondary_pane.active_tab.min(self.secondary_pane.tabs.len().saturating_sub(1));
        self.ui.pane_ratio = self.ui.pane_ratio.clamp(200, 800);
        self.active_tab = self.active_tab.min(self.tabs.len() - 1);
        self.ui.tree_width = self.ui.tree_width.clamp(120, 800);
        let largest = self.tabs.iter().chain(self.secondary_pane.tabs.iter()).map(|t|t.id)
            .chain(self.bookmarks.iter().map(|b|b.id)).max().unwrap_or(0);
        self.next_id = self.next_id.max(largest.saturating_add(1));
        self.normalize_bookmarks();
    }

    pub fn has_locked_tabs(&self)->bool {
        self.tabs.iter().chain(&self.secondary_pane.tabs).any(|tab|tab.locked)
    }
    pub fn closable_tab_group(&self,index:usize,right_only:bool)->Vec<u64>{
        if index>=self.tabs.len(){return Vec::new();}
        self.tabs.iter().enumerate().filter(|(position,tab)|!tab.locked&&*position!=index&&(!right_only||*position>index)).map(|(_,tab)|tab.id).collect()
    }

    /// A malformed or deleted parent is promoted to the root, never discarded.
    pub fn normalize_bookmarks(&mut self) {
        self.next_id=self.next_id.max(self.bookmarks.iter().map(|b|b.id).max().unwrap_or(0).saturating_add(1));
        let mut seen=std::collections::HashSet::new();
        for index in 0..self.bookmarks.len() {
            if !seen.insert(self.bookmarks[index].id) {
                let id=self.take_id();self.bookmarks[index].id=id;seen.insert(id);
            }
        }
        for index in 0..self.bookmarks.len() {
            let id=self.bookmarks[index].id;let mut parent=self.bookmarks[index].parent_id;
            let mut ancestors=std::collections::HashSet::from([id]);let mut valid=true;
            while let Some(next)=parent {
                if !ancestors.insert(next){valid=false;break;}
                let Some(folder)=self.bookmarks.iter().find(|item|item.id==next&&item.is_folder)else{valid=false;break;};
                parent=folder.parent_id;
            }
            if !valid {self.bookmarks[index].parent_id=None;}
        }
    }

    /// Visible source indices in sibling order; children follow expanded folders.
    pub fn visible_bookmarks(&self)->Vec<usize> {
        let mut result=Vec::new();let mut visited=std::collections::HashSet::new();
        let mut stack:Vec<usize>=self.bookmarks.iter().enumerate().filter(|(_,b)|b.parent_id.is_none()).map(|(i,_)|i).rev().collect();
        while let Some(index)=stack.pop(){
            let item=&self.bookmarks[index];if !visited.insert(item.id){continue;}result.push(index);
            if item.is_folder&&!item.collapsed{stack.extend(self.bookmarks.iter().enumerate().filter(|(_,b)|b.parent_id==Some(item.id)).map(|(i,_)|i).rev());}
        }
        result
    }

    pub fn bookmark_depth(&self,index:usize)->usize {
        let Some(item)=self.bookmarks.get(index)else{return 0;};let mut parent=item.parent_id;let mut depth=0;
        while let Some(id)=parent {if depth>=self.bookmarks.len(){break;}depth+=1;parent=self.bookmarks.iter().find(|b|b.id==id).and_then(|b|b.parent_id);}
        depth
    }

    /// Move one node, preserving its subtree and rejecting cycles/invalid targets.
    pub fn move_bookmark_to(&mut self,id:u64,parent_id:Option<u64>,before:Option<u64>)->bool {
        if !self.can_move_bookmark_to(id,parent_id){return false;}
        let Some(from)=self.bookmarks.iter().position(|b|b.id==id)else{return false;};
        if before==Some(id){return false;}
        if let Some(before)=before {if !self.bookmarks.iter().any(|b|b.id==before&&b.parent_id==parent_id){return false;}}
        let mut item=self.bookmarks.remove(from);item.parent_id=parent_id;
        let at=before.and_then(|before|self.bookmarks.iter().position(|b|b.id==before)).unwrap_or(self.bookmarks.len());
        self.bookmarks.insert(at,item);true
    }

    pub fn can_move_bookmark_to(&self,id:u64,parent_id:Option<u64>)->bool {
        if !self.bookmarks.iter().any(|b|b.id==id){return false;}
        let mut ancestor=parent_id;let mut visited=std::collections::HashSet::new();
        while let Some(parent)=ancestor {
            if parent==id||!visited.insert(parent){return false;}
            let Some(folder)=self.bookmarks.iter().find(|b|b.id==parent&&b.is_folder)else{return false;};ancestor=folder.parent_id;
        }
        true
    }

    /// Every link inside a category, nested categories included, in bar order.
    pub fn bookmark_folder_links(&self,folder:u64)->Vec<ShellLocation> {
        let mut result=Vec::new();let mut visited=std::collections::HashSet::from([folder]);
        let mut stack:Vec<_>=self.bookmarks.iter().filter(|b|b.parent_id==Some(folder)).rev().collect();
        while let Some(item)=stack.pop() {
            if !visited.insert(item.id){continue;}
            if item.is_folder {stack.extend(self.bookmarks.iter().filter(|b|b.parent_id==Some(item.id)).rev());}
            else {result.push(item.target.clone());}
        }
        result
    }

    /// Stores tabs into `folder` (an existing category) or a new one named
    /// `name`; locations already linked there are not duplicated.
    pub fn save_tabs_to_bookmarks(&mut self,folder:Option<u64>,name:&str,parent:Option<u64>,tabs:&[ShellLocation])->u64 {
        let folder=match folder.filter(|id|self.bookmarks.iter().any(|b|b.id==*id&&b.is_folder)) {
            Some(id)=>id,
            None=>{let id=self.take_id();self.bookmarks.push(Bookmark::folder(id,name.into(),parent));id}
        };
        for target in tabs {
            if matches!(target,ShellLocation::Search{..}){continue;}
            if self.bookmarks.iter().any(|b|!b.is_folder&&b.parent_id==Some(folder)&&&b.target==target){continue;}
            let id=self.take_id();self.bookmarks.push(Bookmark::link(id,target.title(),target.clone(),Some(folder)));
        }
        if let Some(item)=self.bookmarks.iter_mut().find(|b|b.id==folder){item.collapsed=false;}
        folder
    }

    pub fn remove_bookmark(&mut self,id:u64)->bool {
        let Some(index)=self.bookmarks.iter().position(|b|b.id==id)else{return false;};
        let parent=self.bookmarks[index].parent_id;self.bookmarks.remove(index);
        for child in &mut self.bookmarks {if child.parent_id==Some(id){child.parent_id=parent;}}
        true
    }

    /// Removes saved links only; never deletes their filesystem targets.
    pub fn remove_bookmark_subtree(&mut self,id:u64)->bool {
        if !self.bookmarks.iter().any(|b|b.id==id){return false;}
        let mut removed=std::collections::HashSet::from([id]);let mut stack=vec![id];
        while let Some(parent)=stack.pop(){
            for item in &self.bookmarks {if item.parent_id==Some(parent)&&removed.insert(item.id){stack.push(item.id);}}
        }
        self.bookmarks.retain(|item|!removed.contains(&item.id));true
    }

    pub fn active_tab(&self) -> &ExplorerTab {
        &self.tabs[self.active_tab]
    }

    pub fn active_tab_mut(&mut self) -> &mut ExplorerTab {
        &mut self.tabs[self.active_tab]
    }

    pub fn current_location(&self) -> &ShellLocation {
        self.active_tab().current()
    }

    pub fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        id
    }
}

pub fn path_title(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_namespace_titles_are_readable_without_rewriting_addresses() {
        let address = "::{645FF040-5081-101B-9F08-00AA002F954E}";
        let location = ShellLocation::Shell { pidl:None, parsing_name: address.into(), title: address.into() };
        assert_eq!(location.title(), "回收站");
        assert_eq!(location.address(), address);
        let restored: ShellLocation = serde_json::from_str(&serde_json::to_string(&location).unwrap()).unwrap();
        assert_eq!(restored.title(), "回收站");
        assert_eq!(ShellLocation::Shell { pidl:None, parsing_name: "shell:RecycleBinFolder".into(), title: "Shell".into() }.title(), "回收站");
        assert_eq!(ShellLocation::Shell { pidl:None, parsing_name: address.into(), title: "已删除的文件".into() }.title(), "已删除的文件");
        assert_eq!(ShellLocation::Shell { pidl:None, parsing_name: "::{11111111-1111-1111-1111-111111111111}".into(), title: String::new() }.title(), "系统位置");
    }

    #[test]
    fn shell_locations_participate_in_tab_history() {
        let mut tab = ExplorerTab::new(1, ShellLocation::ThisPc);
        let drive = ShellLocation::Path(PathBuf::from(r"D:\"));
        assert!(tab.navigate(drive.clone()));
        assert_eq!(tab.go_back(), Some(&ShellLocation::ThisPc));
        assert_eq!(tab.go_forward(), Some(&drive));
    }

    #[test]
    fn navigating_to_current_location_does_not_duplicate_history() {
        let mut tab = ExplorerTab::new(1, ShellLocation::ThisPc);
        assert!(!tab.navigate(ShellLocation::ThisPc));
        assert!(!tab.can_go_back());
    }

    #[test]
    fn sessions_without_ui_section_still_load() {
        let text = r#"{"bookmarks":[],"tabs":[{"id":1,"history":["ThisPc"],"history_index":0}],"active_tab":0,"next_id":2}"#;
        let session: ExplorerSession = serde_json::from_str(text).unwrap();
        assert!(session.ui.show_tree && session.ui.show_bookmarks);
        assert_eq!(session.ui.tree_width, 220);
    }

    #[test]
    fn older_ui_preferences_keep_new_defaults_without_losing_existing_choices() {
        let old = r#"{"show_tree":false,"tree_width":268,"show_bookmarks":false,"show_preview":true,"show_details":false,"window":null}"#;
        let ui: ExplorerUi = serde_json::from_str(old).unwrap();
        assert!(!ui.show_tree && !ui.show_bookmarks && ui.show_preview);
        assert_eq!(ui.tree_width, 268);
        assert_eq!(ui.command_bar, CommandBarPreferences::default());
        assert_eq!(ui.context_menu, ContextMenuPreferences::default());
        assert!(ui.search_all_files && ui.space_preview);
    }

    #[test]
    fn customized_ui_preferences_survive_serialization() {
        let mut ui = ExplorerUi::default();
        ui.command_bar.visible = false;
        ui.command_bar.set_item_visible(CommandBarItem::Share, false);
        ui.command_bar.set_item_visible(CommandBarItem::PreviewPane, false);
        ui.context_menu.mode = ContextMenuMode::Original;
        ui.context_menu.show_seven_zip = false;
        ui.search_all_files = false;
        ui.space_preview = false;
        let restored: ExplorerUi = serde_json::from_str(&serde_json::to_string(&ui).unwrap()).unwrap();
        assert_eq!(restored, ui);
        assert!(restored.command_bar.item_visible(CommandBarItem::New));
        assert!(!restored.command_bar.item_visible(CommandBarItem::PreviewPane));
    }

    #[test]
    fn session_normalization_recovers_empty_tabs() {
        let mut session = ExplorerSession {
            tabs: Vec::new(),
            ..ExplorerSession::default()
        };
        session.normalize();
        assert_eq!(session.tabs.len(), 1);
        assert_eq!(session.current_location(), &ShellLocation::ThisPc);
    }

    #[test]
    fn old_preferences_do_not_enable_hover_or_background_features() {
        let ui: ExplorerUi = serde_json::from_str(r#"{"tree_width":271,"space_preview":false}"#).unwrap();
        assert_eq!(ui.tree_width, 271);
        assert!(!ui.space_preview && !ui.hover_preview && ui.navigation_auto_expand.is_none());
        assert_eq!(ui.appearance, AppearancePreferences::default());
        assert_eq!(ui.search_backend, SearchBackendPreference::NativeNtfs);
        assert!(!ui.dual_pane && !ui.everything_folder_sizes);
        assert!(ui.close_window_on_last_tab && ui.show_tab_count);
        assert!(ui.navigation_show_recycle_bin && ui.navigation_show_network);
        assert_eq!(ui.navigation_expand_this_pc,None);
        let background: QuickSearchPreferences = serde_json::from_str("{}").unwrap();
        assert!(!background.enabled && !background.start_with_windows);
        assert_eq!(background.hotkey, "Ctrl+Alt+Space");
    }

    #[test]
    fn old_view_snapshots_keep_selection_without_invented_column_settings() {
        let snapshot: ShellViewState = serde_json::from_str(r#"{"mode":4,"icon_size":16,"selected":[[4,0,1,2,0,0]],"focused":null,"first_visible":null}"#).unwrap();
        assert_eq!(snapshot.selected, vec![vec![4,0,1,2,0,0]]);
        assert!(snapshot.columns.is_empty() && snapshot.sort_columns.is_empty() && snapshot.grouping.is_none());
        assert_eq!(snapshot.column_dpi, 0);
    }

    #[test]
    fn date_sort_preference_survives_session_serialization() {
        let mut session = ExplorerSession::default();
        session.active_tab_mut().view = Some(ShellViewState {
            mode: 4,
            icon_size: 16,
            sort_columns: vec![ViewSortColumn {
                key: ViewPropertyKey {
                    format_id: "{B725F130-47EF-101A-A5F1-02608C9EEBAC}".into(),
                    property_id: 14,
                },
                ascending: false,
            }],
            ..ShellViewState::default()
        });

        let saved = serde_json::to_vec(&session).unwrap();
        let restored: ExplorerSession = serde_json::from_slice(&saved).unwrap();
        assert_eq!(restored, session);
        let sort = &restored.active_tab().view.as_ref().unwrap().sort_columns[0];
        assert_eq!(sort.key.property_id, 14);
        assert!(!sort.ascending);
    }

    #[test]
    fn locked_tabs_survive_restart_and_protect_groups_and_hidden_pane(){
        let mut session=ExplorerSession::default();
        session.tabs=(1..=4).map(|id|ExplorerTab::new(id,ShellLocation::ThisPc)).collect();
        session.tabs[1].locked=true;session.tabs[3].locked=true;
        assert_eq!(session.closable_tab_group(0,false),vec![3]);
        assert_eq!(session.closable_tab_group(1,true),vec![3]);
        assert!(session.closable_tab_group(10,false).is_empty());
        let restored:ExplorerSession=serde_json::from_str(&serde_json::to_string(&session).unwrap()).unwrap();
        assert!(restored.tabs[1].locked&&restored.tabs[3].locked&&restored.has_locked_tabs());
        session.tabs.iter_mut().for_each(|tab|tab.locked=false);
        let mut hidden=ExplorerTab::new(9,ShellLocation::ThisPc);hidden.locked=true;session.secondary_pane.tabs.push(hidden);
        assert!(!session.ui.dual_pane&&session.has_locked_tabs());
        session.secondary_pane.tabs[0].locked=false;assert!(!session.has_locked_tabs());
        let old:ExplorerTab=serde_json::from_str(r#"{"id":1,"history":["ThisPc"],"history_index":0}"#).unwrap();assert!(!old.locked);
    }

    #[test]
    fn bookmark_categories_migrate_flat_entries_and_move_subtrees_without_losing_targets(){
        let mut session=ExplorerSession::default();
        let legacy:Bookmark=serde_json::from_str(r#"{"id":10,"display_name":"资料","target":{"Path":"E:\\资料"}}"#).unwrap();
        assert!(!legacy.is_folder&&legacy.parent_id.is_none());
        session.bookmarks=vec![legacy.clone(),Bookmark::folder(11,"工程".into(),None),Bookmark::folder(12,"图纸".into(),Some(11)),
            Bookmark::link(13,"施工图".into(),ShellLocation::Path(PathBuf::from(r"E:\图纸.pdf")),Some(12)),Bookmark::folder(14,"归档".into(),None)];
        session.normalize();
        assert_eq!(session.visible_bookmarks().iter().map(|i|session.bookmarks[*i].id).collect::<Vec<_>>(),vec![10,11,12,13,14]);
        assert!(session.move_bookmark_to(11,None,Some(10)));
        assert_eq!(session.visible_bookmarks().iter().map(|i|session.bookmarks[*i].id).collect::<Vec<_>>(),vec![11,12,13,10,14]);
        assert!(!session.move_bookmark_to(11,Some(12),None),"a category cannot move into its descendant");
        assert!(!session.move_bookmark_to(13,Some(10),None),"a link is not a category");
        assert!(session.move_bookmark_to(10,Some(14),None));
        assert_eq!(session.bookmarks.iter().find(|b|b.id==10).unwrap().target,legacy.target);
        session.bookmarks.iter_mut().find(|b|b.id==11).unwrap().collapsed=true;
        assert_eq!(session.visible_bookmarks().iter().map(|i|session.bookmarks[*i].id).collect::<Vec<_>>(),vec![11,14,10]);
        let restored:ExplorerSession=serde_json::from_str(&serde_json::to_string(&session).unwrap()).unwrap();assert_eq!(restored,session);
        assert!(session.move_bookmark_to(10,None,Some(11)));
        assert!(session.remove_bookmark(12));
        assert_eq!(session.bookmarks.iter().find(|b|b.id==13).unwrap().parent_id,Some(11));
        assert_eq!(session.bookmarks.iter().find(|b|b.id==13).unwrap().target.as_path(),Some(Path::new(r"E:\图纸.pdf")));
    }

    #[test]
    fn bookmark_categories_open_all_links_and_store_tabs_without_duplicates(){
        let mut session=ExplorerSession::default();session.next_id=20;
        let a=ShellLocation::Path(PathBuf::from(r"E:\甲"));let b=ShellLocation::Path(PathBuf::from(r"E:\乙"));let c=ShellLocation::Path(PathBuf::from(r"E:\丙"));
        session.bookmarks=vec![Bookmark::folder(10,"工程".into(),None),Bookmark::folder(11,"子类".into(),Some(10)),
            Bookmark::link(12,"甲".into(),a.clone(),Some(10)),Bookmark::link(13,"丙".into(),c.clone(),Some(11)),Bookmark::link(14,"乙".into(),b.clone(),Some(10)),
            Bookmark::link(15,"外部".into(),ShellLocation::ThisPc,None)];
        assert_eq!(session.bookmark_folder_links(10),vec![c.clone(),a.clone(),b.clone()]);
        assert!(session.bookmark_folder_links(15).is_empty());
        let search=ShellLocation::Search{query:"x".into(),scope:None};
        let created=session.save_tabs_to_bookmarks(None,"暂存",None,&[a.clone(),search,b.clone(),a.clone()]);
        assert!(session.bookmarks.iter().any(|x|x.id==created&&x.is_folder&&x.display_name=="暂存"));
        assert_eq!(session.bookmark_folder_links(created),vec![a.clone(),b.clone()],"search results skipped, duplicates merged");
        assert_eq!(session.save_tabs_to_bookmarks(Some(10),"忽略",None,&[a.clone(),c.clone()]),10);
        assert_eq!(session.bookmark_folder_links(10).iter().filter(|x|**x==c).count(),2,"one in 工程, one in 子类");
        assert_eq!(session.bookmark_folder_links(10).iter().filter(|x|**x==a).count(),1);
    }

    #[test]
    fn bookmark_normalization_preserves_orphans_and_breaks_cycles(){
        let mut session=ExplorerSession::default();session.bookmarks=vec![Bookmark::folder(10,"甲".into(),Some(11)),
            Bookmark::folder(11,"乙".into(),Some(10)),Bookmark::link(12,"孤立".into(),ShellLocation::ThisPc,Some(999)),
            Bookmark::link(12,"重复编号".into(),ShellLocation::ThisPc,None)];
        session.normalize();assert_eq!(session.bookmarks.len(),4);
        assert_eq!(session.bookmarks.iter().map(|b|b.id).collect::<std::collections::HashSet<_>>().len(),4);
        assert_eq!(session.visible_bookmarks().len(),4);
        assert!(session.bookmarks.iter().find(|b|b.display_name=="孤立").unwrap().parent_id.is_none());
        assert!(session.next_id>session.bookmarks.iter().map(|b|b.id).max().unwrap());
    }

    #[test]
    fn category_subtree_deletion_preserves_external_links_and_targets(){
        let mut session=ExplorerSession::default();let target=ShellLocation::Path(PathBuf::from(r"E:\保留文件夹"));
        session.bookmarks=vec![Bookmark::folder(10,"分类".into(),None),Bookmark::folder(11,"子分类".into(),Some(10)),
            Bookmark::link(12,"内".into(),target.clone(),Some(11)),Bookmark::link(13,"外".into(),target.clone(),None)];
        assert!(session.remove_bookmark_subtree(10));assert_eq!(session.bookmarks.len(),1);
        assert_eq!(session.bookmarks[0].id,13);assert_eq!(session.bookmarks[0].target,target);
        assert!(!session.remove_bookmark_subtree(10));
    }

    #[test]
    fn startup_default_migration_restore_and_custom_location(){
        let mut session=ExplorerSession::default();let old=ShellLocation::Path(PathBuf::from(r"E:\旧位置"));
        session.tabs[0].navigate(old.clone());session.bookmarks.push(Bookmark::folder(10,"分类".into(),None));session.normalize();
        let original=session.clone();session.apply_startup_location();assert_eq!(session,original);
        session.ui.startup_location=StartupLocation::ThisPc;session.apply_startup_location();assert_eq!(session.tabs.len(),1);assert_eq!(session.active_tab().current(),&ShellLocation::ThisPc);
        assert_eq!(session.bookmarks,original.bookmarks);
        let path=PathBuf::from(r"E:\自定义 空格");session.ui.startup_location=StartupLocation::Folder(path.clone());session.apply_startup_location();
        assert_eq!(session.active_tab().current(),&ShellLocation::Path(path));
        let restored:ExplorerSession=serde_json::from_str(&serde_json::to_string(&session).unwrap()).unwrap();assert_eq!(restored,session);
    }
}
