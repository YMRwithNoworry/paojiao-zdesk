//! Menu definitions shared by the native Explorer chrome and desktop fences.
pub use zsui::{core::Command as UiCommand, menu::{MenuItemSpec, MenuSpec}};
use zsdesk_core::model::explorer::{AppearancePreferences, BackdropStyle, ColorScheme, CommandBarItem, CommandBarPreferences, ContextMenuMode, ContextMenuPreferences, ExplorerUi, PreviewProviderPreference, SearchBackendPreference};

pub fn entry(label: &str, id: &str, enabled: bool, checked: bool) -> MenuItemSpec {
    let item = MenuItemSpec::command(label, UiCommand::custom(id)).checked(checked);
    if enabled { item } else { item.disabled() }
}
pub fn item(label: &str, id: &str) -> MenuItemSpec { entry(label, id, true, false) }
pub fn menu(items: Vec<MenuItemSpec>) -> MenuSpec { let mut m = MenuSpec::new(); m.items = items; m }
pub fn submenu(label: &str, items: Vec<MenuItemSpec>) -> MenuItemSpec {
    MenuItemSpec::Submenu { id: None, label: label.into(), enabled: true, menu: menu(items) }
}

pub fn tab_menu(count: usize, index: usize, can_restore: bool,locked:bool) -> MenuSpec {
    menu(vec![
        entry("新建标签页\tCtrl+T", "new", true, false),
        entry("复制此标签页", "duplicate", true, false),
        entry("添加到收藏栏", "bookmark", true, false),
        entry(if locked{"解锁此标签页"}else{"锁定此标签页"},"lock",true,locked),
        MenuItemSpec::Separator,
        entry("关闭此标签页\tCtrl+W", "close", !locked, false),
        entry("关闭其他标签页\tCtrl+Alt+W", "close-others", count > 1, false),
        entry("关闭右侧标签页", "close-right", index + 1 < count, false),
        entry("重新打开关闭的标签页\tCtrl+Shift+T", "restore", can_restore, false),
    ])
}

/// Folder view modes understood by `IFolderView2::SetViewModeAndIconSize`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ViewChoice { pub mode: i32, pub icon_size: i32 }

pub const VIEW_CHOICES: &[(&str, &str, ViewChoice)] = &[
    ("超大图标", "xl", ViewChoice { mode: 1, icon_size: 256 }),
    ("大图标", "large", ViewChoice { mode: 1, icon_size: 96 }),
    ("中等图标", "medium", ViewChoice { mode: 1, icon_size: 48 }),
    ("小图标", "small", ViewChoice { mode: 2, icon_size: 16 }),
    ("列表", "list", ViewChoice { mode: 3, icon_size: 16 }),
    ("详细信息", "details", ViewChoice { mode: 4, icon_size: 16 }),
    ("平铺", "tiles", ViewChoice { mode: 6, icon_size: 48 }),
    ("内容", "content", ViewChoice { mode: 8, icon_size: 32 }),
];

pub fn view_choice(id: &str) -> Option<ViewChoice> {
    VIEW_CHOICES.iter().find(|(_, key, _)| *key == id).map(|(_, _, v)| *v)
}

fn current_view(mode: i32, icon: i32) -> &'static str {
    match mode {
        1 if icon >= 192 => "xl", 1 if icon >= 72 => "large", 1 => "medium", 2 => "small", 3 => "list",
        4 => "details", 6 => "tiles", 8 => "content", _ => "",
    }
}

/// Current pane and folder display toggles.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ShowState {
    pub navigation: bool, pub preview: bool, pub details: bool, pub bookmarks: bool,
    pub checkboxes: bool, pub extensions: bool, pub hidden: bool,
}

pub fn view_menu(mode: i32, icon: i32, show: ShowState) -> MenuSpec {
    let current = current_view(mode, icon);
    let mut items: Vec<MenuItemSpec> = VIEW_CHOICES.iter().map(|(label, id, _)| entry(label, id, true, *id == current)).collect();
    items.push(MenuItemSpec::Separator);
    items.push(submenu("显示", vec![
        entry("导航窗格", "show-nav", true, show.navigation),
        entry("预览窗格\tAlt+P", "show-preview", true, show.preview),
        entry("详细信息窗格\tAlt+Shift+P", "show-details", true, show.details),
        entry("收藏栏", "show-bookmarks", true, show.bookmarks),
        MenuItemSpec::Separator,
        entry("项目复选框", "show-checkboxes", true, show.checkboxes),
        entry("文件扩展名", "show-extensions", true, show.extensions),
        entry("隐藏的项目", "show-hidden", true, show.hidden),
    ]));
    menu(items)
}

/// Sort keys understood by the Shell host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey { Name, Modified, Type, Size, Created }

pub const SORT_KEYS: &[(&str, &str, SortKey)] = &[
    ("名称", "sort-name", SortKey::Name), ("修改日期", "sort-modified", SortKey::Modified),
    ("类型", "sort-type", SortKey::Type), ("大小", "sort-size", SortKey::Size), ("创建日期", "sort-created", SortKey::Created),
];

pub fn sort_menu(current: Option<(SortKey, bool)>, group: Option<SortKey>) -> MenuSpec {
    let mut items: Vec<MenuItemSpec> = SORT_KEYS.iter()
        .map(|(label, id, key)| entry(label, id, true, current.is_some_and(|(k, _)| k == *key))).collect();
    let ascending = current.is_none_or(|(_, asc)| asc);
    items.push(MenuItemSpec::Separator);
    items.push(entry("递增", "sort-asc", true, ascending));
    items.push(entry("递减", "sort-desc", true, !ascending));
    items.push(MenuItemSpec::Separator);
    let mut groups = vec![entry("（无）", "group-none", true, group.is_none())];
    groups.extend(SORT_KEYS.iter().map(|(label, id, key)| entry(label, &id.replace("sort-", "group-"), true, group == Some(*key))));
    items.push(submenu("分组依据", groups));
    menu(items)
}

pub fn sort_key(id: &str) -> Option<SortKey> {
    let name = id.strip_prefix("group-").or_else(|| id.strip_prefix("sort-"))?;
    SORT_KEYS.iter().find(|(_, key, _)| key.strip_prefix("sort-") == Some(name)).map(|(_, _, k)| *k)
}

/// Command bar "…" menu, ordered like Windows 11 Explorer's "See more".
pub fn more_menu(has_selection: bool, network: bool) -> MenuSpec {
    let mut items = vec![
        entry("撤销\tCtrl+Z", "undo", true, false),
        MenuItemSpec::Separator,
        entry("全部选择\tCtrl+A", "select-all", true, false),
        entry("全部取消", "select-none", has_selection, false),
        entry("反向选择\tCtrl+Shift+A", "select-invert", true, false),
        MenuItemSpec::Separator,
        entry("复制路径\tCtrl+Shift+C", "copy-path", true, false),
        entry("在终端中打开", "terminal", true, false),
        entry("属性\tAlt+Enter", "properties", true, false),
        MenuItemSpec::Separator,
    ];
    if network {
        items.extend([
            item("映射网络驱动器…", "map-drive"),
            item("断开网络驱动器…", "disconnect-drive"),
            item("添加网络位置…", "add-netplace"),
            MenuItemSpec::Separator,
        ]);
    }
    items.push(item("选项", "options"));
    menu(items)
}

/// Fluent glyph (and whether it uses the accent color) for the few commands
/// that carry one in Windows 11 Explorer's "See more" menu: undo, the
/// selection commands, properties and options. Other menus stay text-only,
/// like their Windows 11 counterparts.
pub fn command_glyph(id: &str) -> Option<(&'static str, bool)> {
    Some(match id {
        "undo" => ("\u{E7A7}", true),
        "select-all" => ("\u{E8B3}", true),
        "select-none" => ("\u{E8E6}", true),
        "select-invert" => ("\u{E762}", true),
        "copy-path" => ("\u{E8C8}", false),
        "terminal" => ("\u{E756}", false),
        "properties" => ("\u{E90F}", false),
        "options" => ("\u{E713}", false),
        "map-drive" => ("\u{E8CE}", false),
        "disconnect-drive" => ("\u{E8CD}", false),
        "add-netplace" => ("\u{E968}", false),
        _ => return None,
    })
}

pub const COMMAND_BAR_CHOICES: &[(&str, CommandBarItem)] = &[
    ("新建", CommandBarItem::New), ("剪切", CommandBarItem::Cut), ("复制", CommandBarItem::Copy),
    ("粘贴", CommandBarItem::Paste), ("重命名", CommandBarItem::Rename), ("共享", CommandBarItem::Share),
    ("删除", CommandBarItem::Delete), ("排序", CommandBarItem::Sort), ("查看", CommandBarItem::View),
    ("更多", CommandBarItem::More), ("预览", CommandBarItem::PreviewPane),
];

/// A submenu whose id selects its glyph in [`command_glyph`].
pub fn group(label: &str, id: &str, items: Vec<MenuItemSpec>) -> MenuItemSpec {
    MenuItemSpec::Submenu { id: Some(id.into()), label: label.into(), enabled: true, menu: menu(items) }
}

/// The interface settings, grouped the way Windows 11 Settings groups them.
/// Every entry is a checkable state or a radio choice; actions that open a
/// window end with "…".
pub fn interface_settings_menu(ui: &ExplorerUi) -> MenuSpec {
    menu(vec![layout_settings(ui), tab_settings(ui), preview_settings(ui), search_settings(ui)])
}

fn layout_settings(ui: &ExplorerUi) -> MenuItemSpec {
    let mut buttons: Vec<MenuItemSpec> = COMMAND_BAR_CHOICES.iter().map(|(label, key)| {
        entry(label, &format!("command-bar-item-{}", key.key()), ui.command_bar.visible, ui.command_bar.item_visible(*key))
    }).collect();
    buttons.push(MenuItemSpec::Separator);
    buttons.push(item("恢复默认按钮", "command-bar-defaults"));
    group("布局", "settings-layout", vec![
        entry("命令栏", "command-bar-visible", true, ui.command_bar.visible),
        submenu("命令栏按钮", buttons),
        entry("收藏栏", "interface-bookmarks", true, ui.show_bookmarks),
        entry("双栏布局", "dual-pane", true, ui.dual_pane),
        MenuItemSpec::Separator,
        entry("导航窗格", "interface-navigation", true, ui.show_tree),
        submenu("导航窗格选项", vec![
            entry("展开到当前文件夹", "navigation-auto-expand", true, ui.navigation_auto_expand.unwrap_or(false)),
            entry("显示回收站", "navigation-recycle-bin", true, ui.navigation_show_recycle_bin),
            entry("显示网络", "navigation-network", true, ui.navigation_show_network),
            MenuItemSpec::Separator,
            entry("此电脑：跟随起始位置", "navigation-this-pc-auto", true, ui.navigation_expand_this_pc.is_none()),
            entry("此电脑：始终展开", "navigation-this-pc-expand", true, ui.navigation_expand_this_pc == Some(true)),
            entry("此电脑：始终折叠", "navigation-this-pc-collapse", true, ui.navigation_expand_this_pc == Some(false)),
        ]),
    ])
}

fn tab_settings(ui: &ExplorerUi) -> MenuItemSpec {
    use zsdesk_core::model::explorer::TabPlacement;
    group("标签页", "settings-tabs", vec![
        entry("标签栏在上方", "tab-placement-top", true, ui.tab_placement == TabPlacement::Top),
        entry("标签栏在左侧", "tab-placement-left", true, ui.tab_placement == TabPlacement::Left),
        entry("标签栏在右侧", "tab-placement-right", true, ui.tab_placement == TabPlacement::Right),
        MenuItemSpec::Separator,
        entry("显示标签页数量", "show-tab-count", true, ui.show_tab_count),
        entry("双击关闭标签页", "double-click-close-tab", true, ui.double_click_close_tab),
        entry("关闭最后一个标签页时关闭窗口", "close-window-last-tab", true, ui.close_window_on_last_tab),
        MenuItemSpec::Separator,
        submenu("重新打开窗口时",vec![
            entry("恢复上次的标签页","startup-restore",true,matches!(ui.startup_location,zsdesk_core::model::explorer::StartupLocation::RestoreSession)),
            entry("打开此电脑","startup-this-pc",true,matches!(ui.startup_location,zsdesk_core::model::explorer::StartupLocation::ThisPc)),
            entry(&match &ui.startup_location {zsdesk_core::model::explorer::StartupLocation::Folder(path)=>format!("默认文件夹：{}…",path.display().to_string().replace('&',"&&")),_=>"选择默认文件夹…".into()},
                "startup-folder",true,matches!(ui.startup_location,zsdesk_core::model::explorer::StartupLocation::Folder(_))),
        ]),
    ])
}

fn preview_settings(ui: &ExplorerUi) -> MenuItemSpec {
    group("预览与操作", "settings-preview", vec![
        entry("空格键快速预览", "space-preview", true, ui.space_preview),
        entry("悬停预览文件", "hover-preview", true, ui.hover_preview),
        submenu("Office 文档预览", vec![
            entry("系统默认", "preview-provider-system", true, ui.preview_provider == PreviewProviderPreference::System),
            entry("Office 优先", "preview-provider-office", true, ui.preview_provider == PreviewProviderPreference::Office),
            entry("WPS 优先", "preview-provider-wps", true, ui.preview_provider == PreviewProviderPreference::Wps),
        ]),
        MenuItemSpec::Separator,
        entry("压缩包：在 ZSDesk 中浏览", "archive-browse", true, !ui.archives_open_externally),
        entry("压缩包：用默认解压软件打开", "archive-external", true, ui.archives_open_externally),
        MenuItemSpec::Separator,
        entry("鼠标手势（按住右键拖动）", "mouse-gestures", true, ui.mouse_gestures),
    ])
}

fn search_settings(ui: &ExplorerUi) -> MenuItemSpec {
    let builtin=ui.search_backend.is_builtin();
    group("搜索", "settings-search", vec![
        entry("自带搜索", "search-backend-native", true, builtin),
        entry("Everything 搜索", "search-backend-everything", true, !builtin),
        submenu("自带搜索选项",vec![
            entry("收录开发和缓存目录内容", "search-native-development",builtin,ui.search_include_development),
            entry("索引文件大小和修改时间", "search-native-metadata",builtin,ui.search_index_metadata),
            entry("拼音首字母匹配", "search-native-pinyin",builtin,ui.search_pinyin_initials),
            entry("空闲 1 分钟释放索引", "search-native-idle-60",builtin,ui.search_idle_seconds==60),
            entry("空闲 5 分钟释放索引", "search-native-idle-300",builtin,ui.search_idle_seconds==300),
            entry("空闲 15 分钟释放索引", "search-native-idle-900",builtin,ui.search_idle_seconds==900),
        ]),
        MenuItemSpec::Separator,
        entry("搜索全部本地磁盘", "search-all-files", true, ui.search_all_files),
        entry("仅搜索当前文件夹", "search-current-folder", true, !ui.search_all_files),
        entry("显示文件夹大小（Everything）", "everything-folder-sizes", !builtin, ui.everything_folder_sizes),
        MenuItemSpec::Separator,
        item("重建文件名索引", "search-rebuild-index"),
        entry("Everything 索引设置…", "search-index-settings",!builtin,false),
        entry("启用 Everything 索引服务…", "search-everything-service",!builtin,false),
        entry("选择已安装的 Everything…", "search-everything-program",!builtin,false),
    ])
}

/// Applies a recognized settings choice; the host then saves and relayouts.
pub fn interface_requires_dialog(choice:&str)->bool{matches!(choice,"search-everything-program"|"startup-folder")}

/// Settings entries that run an action instead of toggling a preference.
pub fn interface_action(choice:&str)->bool{
    matches!(choice,"search-rebuild-index"|"search-index-settings"|"search-everything-service"|"dual-pane"|"startup-restore"|"startup-this-pc")||interface_requires_dialog(choice)
}

pub fn apply_interface_setting(ui: &mut ExplorerUi, choice: &str) -> bool {
    match choice {
        "command-bar-visible" => ui.command_bar.visible = !ui.command_bar.visible,
        "command-bar-defaults" => ui.command_bar = CommandBarPreferences::default(),
        "interface-navigation" => ui.show_tree = !ui.show_tree,
        "navigation-auto-expand" => ui.navigation_auto_expand = Some(!ui.navigation_auto_expand.unwrap_or(false)),
        "navigation-expand-system" => ui.navigation_auto_expand = None,
        "navigation-recycle-bin"=>ui.navigation_show_recycle_bin=!ui.navigation_show_recycle_bin,
        "navigation-network"=>ui.navigation_show_network=!ui.navigation_show_network,
        "navigation-this-pc-auto"=>ui.navigation_expand_this_pc=None,
        "navigation-this-pc-expand"=>ui.navigation_expand_this_pc=Some(true),
        "navigation-this-pc-collapse"=>ui.navigation_expand_this_pc=Some(false),
        "interface-bookmarks" => ui.show_bookmarks = !ui.show_bookmarks,
        "dual-pane" => ui.dual_pane = !ui.dual_pane,
        "double-click-close-tab" => ui.double_click_close_tab = !ui.double_click_close_tab,
        "close-window-last-tab"=>ui.close_window_on_last_tab=!ui.close_window_on_last_tab,
        "show-tab-count"=>ui.show_tab_count=!ui.show_tab_count,
        "mouse-gestures" => ui.mouse_gestures = !ui.mouse_gestures,
        "everything-folder-sizes" => ui.everything_folder_sizes = !ui.everything_folder_sizes,
        "tab-placement-top"=>ui.tab_placement=zsdesk_core::model::explorer::TabPlacement::Top,
        "tab-placement-left"=>ui.tab_placement=zsdesk_core::model::explorer::TabPlacement::Left,
        "tab-placement-right"=>ui.tab_placement=zsdesk_core::model::explorer::TabPlacement::Right,
        "space-preview" => ui.space_preview = !ui.space_preview,
        "archive-browse" => ui.archives_open_externally = false,
        "archive-external" => ui.archives_open_externally = true,
        "preview-provider-system" => ui.preview_provider = PreviewProviderPreference::System,
        "preview-provider-office" => ui.preview_provider = PreviewProviderPreference::Office,
        "preview-provider-wps" => ui.preview_provider = PreviewProviderPreference::Wps,
        "search-backend-auto"|"search-backend-native"|"search-backend-directory" => ui.search_backend = SearchBackendPreference::NativeNtfs,
        "search-backend-everything"=>ui.search_backend=SearchBackendPreference::Everything,
        "search-native-development"=>ui.search_include_development=!ui.search_include_development,
        "search-native-metadata"=>ui.search_index_metadata=!ui.search_index_metadata,
        "search-native-pinyin"=>ui.search_pinyin_initials=!ui.search_pinyin_initials,
        "search-native-idle-60"=>ui.search_idle_seconds=60,
        "search-native-idle-300"=>ui.search_idle_seconds=300,
        "search-native-idle-900"=>ui.search_idle_seconds=900,
        "hover-preview" => ui.hover_preview = !ui.hover_preview,
        "search-all-files" => ui.search_all_files = true,
        "search-current-folder" => ui.search_all_files = false,
        _ => {
            let Some(item) = choice.strip_prefix("command-bar-item-").and_then(CommandBarItem::from_key) else { return false };
            let visible = ui.command_bar.item_visible(item);
            ui.command_bar.set_item_visible(item, !visible);
        }
    }
    true
}

pub fn context_menu_settings_menu(options: &ContextMenuPreferences) -> MenuSpec {
    context_menu_settings_menu_for(options, true)
}

/// `modern_available` is false where the Windows 11 presenter cannot run
/// (Windows 10, or a system build the compatibility layer does not support):
/// the menu then shows the classic style that is actually used.
pub fn context_menu_settings_menu_for(options: &ContextMenuPreferences, modern_available: bool) -> MenuSpec {
    let original = options.mode == ContextMenuMode::Original || !modern_available;
    let modern = if modern_available { entry("Windows 11 原生菜单", "context-menu-windows11", true, !original) }
        else { entry("Windows 11 原生菜单（当前系统版本不可用）", "context-menu-windows11", false, false) };
    menu(vec![
        modern,
        entry("经典系统菜单", "context-menu-original", true, original),
        MenuItemSpec::Separator,
        submenu("显示的命令", vec![
            entry("剪切、复制和粘贴", "context-action-cut-copy-paste", true, options.show_cut_copy_paste),
            entry("复制路径", "context-action-copy-path", true, options.show_copy_path),
            entry("重命名", "context-action-rename", true, options.show_rename),
            entry("删除", "context-action-delete", true, options.show_delete),
            entry("属性", "context-action-properties", true, options.show_properties),
            entry("共享", "context-action-share", true, options.show_share),
            entry("新建", "context-action-new", true, options.show_new),
            MenuItemSpec::Separator,
            entry("7-Zip", "context-action-seven-zip", true, options.show_seven_zip),
            entry("WPS", "context-action-wps", true, options.show_wps),
        ]),
        item("恢复默认", "context-menu-defaults"),
    ])
}

pub fn apply_context_menu_setting(options: &mut ContextMenuPreferences, choice: &str) -> bool {
    match choice {
        "context-menu-original" => options.mode = ContextMenuMode::Original,
        "context-menu-windows11" => options.mode = ContextMenuMode::Windows11,
        "context-menu-defaults" => *options = ContextMenuPreferences::default(),
        "context-action-cut-copy-paste" => options.show_cut_copy_paste = !options.show_cut_copy_paste,
        "context-action-copy-path" => options.show_copy_path = !options.show_copy_path,
        "context-action-rename" => options.show_rename = !options.show_rename,
        "context-action-delete" => options.show_delete = !options.show_delete,
        "context-action-properties" => options.show_properties = !options.show_properties,
        "context-action-share" => options.show_share = !options.show_share,
        "context-action-new" => options.show_new = !options.show_new,
        "context-action-seven-zip" => options.show_seven_zip = !options.show_seven_zip,
        "context-action-wps" => options.show_wps = !options.show_wps,
        _ => return false,
    }
    true
}

/// State shown in the application menu.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AppMenuState {
    pub forward_unhandled_windows: bool,
    /// `None` when this edition has no desktop fences.
    pub desktop_running: Option<bool>,
    /// 0 = 不接管, 1 = 右键菜单, 2 = 默认打开文件夹与磁盘.
    pub integration: u8,
    /// Whether a background login agent is registered to intercept Win+E.
    pub win_e_agent: bool,
    /// The Windows 11 menu presenter cannot run on this system.
    pub modern_menu_unavailable: bool,
}

/// Application menu (≡) next to the window buttons.
pub fn app_menu(state: AppMenuState) -> MenuSpec {
    app_menu_with_capabilities(state, &ExplorerUi::default(), false)
}

/// The ≡ menu: window actions first, then one submenu per settings area, then
/// help. Settings open in place as submenus instead of a second popup.
pub fn app_menu_with_capabilities(state: AppMenuState, ui: &ExplorerUi, themed: bool) -> MenuSpec {
    let mut items = vec![
        item("新建标签页\tCtrl+T", "new-tab"),
        item("新建窗口\tCtrl+N", "new-window"),
        item("收藏当前文件夹\tCtrl+D", "bookmark"),
        MenuItemSpec::Separator,
    ];
    items.extend(interface_settings_menu(ui).items);
    items.push(group("右键菜单", "settings-context-menu",
        context_menu_settings_menu_for(&ui.context_menu, !state.modern_menu_unavailable).items));
    if themed { items.push(group("主题与材质", "settings-appearance", appearance_menu(&ui.appearance).items)); }
    if let Some(running) = state.desktop_running {
        items.push(group("桌面格子", "settings-desktop", vec![
            item(if running { "显示桌面格子" } else { "启动桌面格子" }, "desktop-start"),
            item("一键整理桌面", "desktop-organize"),
            item("将当前文件夹映射为格子", "desktop-map"),
            MenuItemSpec::Separator,
            entry("显示托盘图标", "desktop-tray", running, false),
            entry("退出桌面格子", "desktop-quit", running, false),
        ]));
    }
    items.push(group("系统集成", "settings-integration", vec![
        entry("不接管系统资源管理器", "integration-off", true, state.integration == 0),
        entry("右键菜单“使用 ZSDesk 打开”", "integration-context", true, state.integration == 1),
        entry("默认打开文件夹、磁盘和此电脑", "integration-default", true, state.integration == 2),
        entry("同时接管新出现的系统浏览窗口", "integration-forward-unhandled", state.integration == 2, state.forward_unhandled_windows),
        MenuItemSpec::Separator,
        entry("Win+E 打开 ZSDesk（需后台常驻）", "integration-win-e", true, state.win_e_agent),
        MenuItemSpec::Separator,
        entry("恢复系统资源管理器", "integration-restore", state.integration != 0, false),
        MenuItemSpec::Separator,
        item("开机启动与全局快捷键…", "quick-search-settings"),
        item("文件位置转发…", "routing-settings"),
    ]));
    items.extend([
        MenuItemSpec::Separator,
        item("文件夹选项", "folder-options"),
        item("键盘快捷键", "shortcuts"),
        item("诊断日志…", "diagnostics"),
        item("关于 ZSDesk", "about"),
    ]);
    menu(items)
}

/// Material and color are two independent choices.
pub fn appearance_menu(preferences: &AppearancePreferences) -> MenuSpec {
    let backdrop = preferences.backdrop;
    let scheme = preferences.color_scheme;
    menu(vec![
        entry("材质：跟随系统", "appearance-backdrop-system", true, backdrop == BackdropStyle::System),
        entry("材质：Mica", "appearance-mica", true, backdrop == BackdropStyle::Mica),
        entry("材质：亚克力", "appearance-acrylic", true, backdrop == BackdropStyle::Acrylic),
        entry("材质：纯色", "appearance-solid", true, backdrop == BackdropStyle::Solid),
        MenuItemSpec::Separator,
        entry("颜色：跟随系统", "appearance-color-system", true, scheme == ColorScheme::System),
        entry("颜色：浅色", "appearance-color-light", true, scheme == ColorScheme::Light),
        entry("颜色：深色", "appearance-color-dark", true, scheme == ColorScheme::Dark),
    ])
}

pub fn apply_appearance(preferences: &mut AppearancePreferences, choice: &str) -> bool {
    match choice {
        "appearance-acrylic" => preferences.backdrop = BackdropStyle::Acrylic,
        "appearance-mica" => preferences.backdrop = BackdropStyle::Mica,
        "appearance-light" => { preferences.backdrop = BackdropStyle::Solid; preferences.color_scheme = ColorScheme::Light; }
        "appearance-dark" => { preferences.backdrop = BackdropStyle::Solid; preferences.color_scheme = ColorScheme::Dark; }
        "appearance-system" => *preferences = AppearancePreferences::default(),
        "appearance-backdrop-system" => preferences.backdrop = BackdropStyle::System,
        "appearance-solid" => preferences.backdrop = BackdropStyle::Solid,
        "appearance-color-system" => preferences.color_scheme = ColorScheme::System,
        "appearance-color-light" => preferences.color_scheme = ColorScheme::Light,
        "appearance-color-dark" => preferences.color_scheme = ColorScheme::Dark,
        _ => return false,
    }
    true
}

/// Every open tab plus restore.
pub fn tab_list_menu(titles: &[String], active: usize, can_restore: bool) -> MenuSpec {
    let mut items: Vec<MenuItemSpec> = titles.iter().enumerate()
        .map(|(i, t)| entry(&t.replace('&', "&&"), &format!("tab-{i}"), true, i == active)).collect();
    items.push(MenuItemSpec::Separator);
    items.push(entry("重新打开关闭的标签页\tCtrl+Shift+T", "restore", can_restore, false));
    menu(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ids(spec: &MenuSpec) -> Vec<String> {
        spec.items.iter().filter_map(|i| match i {
            MenuItemSpec::Command { command: UiCommand::Custom { id, .. }, .. } => Some(id.clone()), _ => None }).collect()
    }
    #[test]
    fn view_menu_marks_current_layout() {
        let spec = view_menu(4, 16, ShowState::default());
        let checked: Vec<_> = spec.items.iter().filter_map(|i| match i {
            MenuItemSpec::Command { checked: true, command: UiCommand::Custom { id, .. }, .. } => Some(id.clone()), _ => None }).collect();
        assert_eq!(checked, vec!["details".to_string()]);
        assert_eq!(view_choice("large"), Some(ViewChoice { mode: 1, icon_size: 96 }));
    }
    #[test]
    fn sort_and_group_ids_round_trip() {
        assert_eq!(sort_key("sort-size"), Some(SortKey::Size));
        assert_eq!(sort_key("group-type"), Some(SortKey::Type));
        assert_eq!(sort_key("group-none"), None);
        assert!(ids(&sort_menu(Some((SortKey::Name, true)), None)).contains(&"sort-asc".to_string()));
    }
    #[test]
    fn more_menu_follows_windows11_order_with_glyphs() {
        let spec = more_menu(false, false);
        let order = ids(&spec);
        assert_eq!(&order[..4], ["undo", "select-all", "select-none", "select-invert"]);
        assert_eq!(order.last().map(String::as_str), Some("options"));
        for id in &order {
            let (glyph, _) = command_glyph(id).unwrap_or_else(|| panic!("{id} has no glyph"));
            assert!(glyph.chars().all(|c| (0xE700..=0xE9FF).contains(&(c as u32))), "{id} must exist in Segoe MDL2 Assets");
        }
        assert!(command_glyph("new-tab").is_none() && command_glyph("folder-options").is_none(), "the ≡ menu stays text-only");
        assert!(spec.items.iter().any(|item| matches!(item, MenuItemSpec::Command { label, enabled: false, .. } if label == "全部取消")));
    }
    #[test]
    fn tab_list_escapes_mnemonics() {
        let spec = tab_list_menu(&["A&B".into()], 0, false);
        match &spec.items[0] { MenuItemSpec::Command { label, .. } => assert_eq!(label, "A&&B"), _ => panic!() }
    }

    fn nested_ids(spec: &MenuSpec) -> Vec<String> {
        let mut result = Vec::new();
        for item in &spec.items {
            match item {
                MenuItemSpec::Command { enabled: true, command: UiCommand::Custom { id, .. }, .. } => result.push(id.clone()),
                MenuItemSpec::Submenu { enabled: true, menu, .. } => result.extend(nested_ids(menu)),
                _ => {}
            }
        }
        result
    }


    #[test]
    fn win_e_agent_choice_is_visible_and_reflects_registration_state() {
        fn integration_entry(spec: &MenuSpec) -> Option<(bool, bool)> {
            for item in &spec.items {
                match item {
                    MenuItemSpec::Submenu { label, menu, .. } if label == "系统集成" => {
                        return menu.items.iter().find_map(|item| match item {
                            MenuItemSpec::Command { label, enabled, checked, command: UiCommand::Custom { id, .. }, .. }
                                if id == "integration-win-e" => Some((label.contains("Win+E"), *enabled && *checked)),
                            _ => None,
                        });
                    }
                    _ => {}
                }
            }
            None
        }
        assert_eq!(integration_entry(&app_menu(AppMenuState::default())), Some((true, false)));
        assert_eq!(integration_entry(&app_menu(AppMenuState { win_e_agent: true, ..Default::default() })), Some((true, true)));
    }
    #[test]
    fn settings_remain_reachable_when_every_command_button_is_hidden() {
        let mut ui = ExplorerUi::default();
        ui.show_bookmarks = false;
        ui.command_bar.visible = false;
        ui.command_bar.hidden_items = CommandBarItem::ALL.to_vec();
        let app = nested_ids(&app_menu_with_capabilities(AppMenuState::default(), &ui, false));
        for id in ["command-bar-visible", "command-bar-defaults", "interface-bookmarks", "context-menu-windows11", "context-menu-defaults", "folder-options"] {
            assert!(app.contains(&id.to_string()), "{id} must stay reachable from ≡");
        }
        for id in nested_ids(&interface_settings_menu(&ui)) {
            assert!(apply_interface_setting(&mut ui.clone(), &id)||interface_action(&id), "unhandled setting {id}");
        }
        assert!(apply_interface_setting(&mut ui, "command-bar-defaults"));
        assert_eq!(ui.command_bar, CommandBarPreferences::default());
        assert!(!ui.show_bookmarks, "restoring command buttons must preserve the bookmark preference");
        assert_eq!(COMMAND_BAR_CHOICES.len(), CommandBarItem::ALL.len());
    }
    #[test]
    fn navigation_expansion_is_an_explicit_reversible_preference() {
        let mut ui=ExplorerUi::default();
        assert_eq!(ui.navigation_auto_expand, None);
        assert!(nested_ids(&interface_settings_menu(&ui)).contains(&"navigation-auto-expand".into()));
        assert!(apply_interface_setting(&mut ui,"navigation-auto-expand"));
        assert_eq!(ui.navigation_auto_expand, Some(true));
        assert!(apply_interface_setting(&mut ui,"navigation-auto-expand"));
        assert_eq!(ui.navigation_auto_expand, Some(false));
    }

    #[test]
    fn app_menu_groups_settings_without_duplicates() {
        let ui = ExplorerUi::default();
        let spec = app_menu_with_capabilities(AppMenuState { desktop_running: Some(false), ..Default::default() }, &ui, true);
        let all = nested_ids(&spec);
        let mut unique = all.clone();
        unique.sort(); unique.dedup();
        assert_eq!(all.len(), unique.len(), "every command appears once");
        // Removed duplicates: "restore" equals "off"; system expansion equals the checkbox default.
        for removed in ["integration-restore", "navigation-expand-system", "interface-settings", "context-menu-settings", "appearance-settings", "quick-search"] {
            assert!(!all.contains(&removed.to_string()), "{removed} is no longer a menu entry");
        }
        let groups: Vec<_> = spec.items.iter().filter_map(|item| match item {
            MenuItemSpec::Submenu { id: Some(id), .. } => Some(id.as_str()), _ => None }).collect();
        assert_eq!(groups, ["settings-layout", "settings-tabs", "settings-preview", "settings-search", "settings-context-menu",
            "settings-appearance", "settings-desktop", "settings-integration"]);
        assert!(spec.items.len() <= 20, "the top level stays short");
    }

    #[test]
    fn appearance_material_and_color_are_independent() {
        let mut preferences = AppearancePreferences::default();
        for id in nested_ids(&appearance_menu(&preferences)) { assert!(apply_appearance(&mut preferences.clone(), &id), "{id}"); }
        assert!(apply_appearance(&mut preferences, "appearance-color-dark"));
        assert!(apply_appearance(&mut preferences, "appearance-acrylic"));
        assert_eq!((preferences.backdrop, preferences.color_scheme), (BackdropStyle::Acrylic, ColorScheme::Dark));
        assert!(apply_appearance(&mut preferences, "appearance-backdrop-system"));
        assert_eq!(preferences.color_scheme, ColorScheme::Dark, "material does not reset the color");
    }

    #[test]
    fn archive_opening_is_a_radio_choice_that_defaults_to_browsing() {
        let mut ui = ExplorerUi::default();
        assert!(!ui.archives_open_externally);
        assert!(apply_interface_setting(&mut ui, "archive-external") && ui.archives_open_externally);
        assert!(apply_interface_setting(&mut ui, "archive-browse") && !ui.archives_open_externally);
        assert!(!interface_action("archive-external"), "the choice stays in the open settings menu");
        let tabs = tab_menu(3, 0, false, false);
        assert!(tabs.items.iter().any(|item| matches!(item, MenuItemSpec::Command { label, .. } if label.contains("Ctrl+Alt+W"))));
    }

    #[test]
    fn hover_preview_starts_disabled_and_does_not_change_space_preview() {
        let mut ui=ExplorerUi::default();
        assert!(!ui.hover_preview);
        assert!(apply_interface_setting(&mut ui,"hover-preview"));
        assert!(ui.hover_preview && ui.space_preview);
        assert!(apply_interface_setting(&mut ui,"hover-preview"));
        assert!(!ui.hover_preview && ui.space_preview);
    }
    #[test]
    fn search_backends_are_explicit_and_never_map_native_to_everything(){
        let mut ui=ExplorerUi::default();assert!(ui.search_backend.is_builtin());
        assert!(apply_interface_setting(&mut ui,"search-backend-everything"));assert_eq!(ui.search_backend,SearchBackendPreference::Everything);
        for id in ["search-backend-native","search-backend-auto","search-backend-directory"]{
            assert!(apply_interface_setting(&mut ui,id));assert_eq!(ui.search_backend,SearchBackendPreference::NativeNtfs);
        }
        assert!(!ui.search_include_development&&!ui.search_index_metadata&&!ui.search_pinyin_initials);
        assert!(apply_interface_setting(&mut ui,"search-native-metadata"));assert!(ui.search_index_metadata);assert!(ui.search_backend.is_builtin());
    }

    #[test]
    fn every_context_setting_has_an_action_and_defaults_keep_wps_off() {
        let options = ContextMenuPreferences { mode: ContextMenuMode::Original, ..Default::default() };
        for id in nested_ids(&context_menu_settings_menu(&options)) {
            assert!(apply_context_menu_setting(&mut options.clone(), &id), "unhandled context setting {id}");
        }
        assert!(!options.show_wps);
        assert!(options.show_seven_zip);
    }

    #[test]
    fn unavailable_windows11_menu_shows_the_classic_menu_in_use() {
        let options = ContextMenuPreferences::default();
        let spec = context_menu_settings_menu_for(&options, false);
        let checked: Vec<_> = spec.items.iter().filter_map(|item| match item {
            MenuItemSpec::Command { checked: true, command: UiCommand::Custom { id, .. }, .. } => Some(id.as_str()), _ => None }).collect();
        assert_eq!(checked, ["context-menu-original"]);
        assert!(!nested_ids(&spec).contains(&"context-menu-windows11".to_string()), "the unavailable style is not selectable");
        assert!(nested_ids(&spec).contains(&"context-action-wps".to_string()), "command visibility stays editable");
    }

    #[test]
    fn windows11_mode_keeps_menu_customization_available() {
        let mut options = ContextMenuPreferences::default();
        let spec = context_menu_settings_menu(&options);
        let available = nested_ids(&spec);
        assert!(available.contains(&"context-action-delete".into()));
        assert!(apply_context_menu_setting(&mut options, "context-action-delete"));
        assert!(!options.show_delete);
        assert_eq!(options.mode, ContextMenuMode::Windows11);
        assert!(spec.items.iter().any(|item| matches!(item,
            MenuItemSpec::Submenu { label, enabled: true, .. } if label == "显示的命令")));
        assert!(!available.contains(&"context-menu-windows11-hint".into()));
    }
}
