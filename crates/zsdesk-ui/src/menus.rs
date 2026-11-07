//! Menu definitions shared by the native Explorer chrome and desktop fences.
pub use zsui::{core::Command as UiCommand, menu::{MenuItemSpec, MenuSpec}};
use zsdesk_core::model::explorer::{CommandBarItem, CommandBarPreferences, ContextMenuMode, ContextMenuPreferences, ExplorerUi};

pub fn entry(label: &str, id: &str, enabled: bool, checked: bool) -> MenuItemSpec {
    let item = MenuItemSpec::command(label, UiCommand::custom(id)).checked(checked);
    if enabled { item } else { item.disabled() }
}
pub fn item(label: &str, id: &str) -> MenuItemSpec { entry(label, id, true, false) }
pub fn menu(items: Vec<MenuItemSpec>) -> MenuSpec { let mut m = MenuSpec::new(); m.items = items; m }
pub fn submenu(label: &str, items: Vec<MenuItemSpec>) -> MenuItemSpec {
    MenuItemSpec::Submenu { id: None, label: label.into(), enabled: true, menu: menu(items) }
}

pub fn tab_menu(count: usize, index: usize, can_restore: bool) -> MenuSpec {
    menu(vec![
        entry("新建标签页\tCtrl+T", "new", true, false),
        entry("复制此标签页", "duplicate", true, false),
        entry("添加到收藏栏", "bookmark", true, false),
        MenuItemSpec::Separator,
        entry("关闭此标签页\tCtrl+W", "close", true, false),
        entry("关闭其他标签页", "close-others", count > 1, false),
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

pub fn more_menu(has_selection: bool, network: bool) -> MenuSpec {
    let mut items = vec![
        entry("全选\tCtrl+A", "select-all", true, false),
        entry("全部取消", "select-none", has_selection, false),
        entry("反向选择", "select-invert", true, false),
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
    items.push(item("选项", "folder-options"));
    menu(items)
}

pub const COMMAND_BAR_CHOICES: &[(&str, CommandBarItem)] = &[
    ("新建", CommandBarItem::New), ("剪切", CommandBarItem::Cut), ("复制", CommandBarItem::Copy),
    ("粘贴", CommandBarItem::Paste), ("重命名", CommandBarItem::Rename), ("共享", CommandBarItem::Share),
    ("删除", CommandBarItem::Delete), ("排序", CommandBarItem::Sort), ("查看", CommandBarItem::View),
    ("更多", CommandBarItem::More), ("预览", CommandBarItem::PreviewPane),
];

pub fn interface_settings_menu(ui: &ExplorerUi) -> MenuSpec {
    let buttons = COMMAND_BAR_CHOICES.iter().map(|(label, key)| {
        entry(label, &format!("command-bar-item-{}", key.key()), true, ui.command_bar.item_visible(*key))
    }).collect();
    menu(vec![
        entry("显示命令栏", "command-bar-visible", true, ui.command_bar.visible),
        submenu("命令栏按钮", buttons),
        item("恢复命令栏默认设置", "command-bar-defaults"),
        MenuItemSpec::Separator,
        entry("导航窗格", "interface-navigation", true, ui.show_tree),
        entry("收藏栏", "interface-bookmarks", true, ui.show_bookmarks),
        entry("空格快速预览", "space-preview", true, ui.space_preview),
        submenu("搜索范围", vec![
            entry("搜索全部本地磁盘", "search-all-files", true, ui.search_all_files),
            entry("仅搜索当前文件夹", "search-current-folder", true, !ui.search_all_files),
        ]),
    ])
}

/// Applies a recognized settings choice; the host then saves and relayouts.
pub fn apply_interface_setting(ui: &mut ExplorerUi, choice: &str) -> bool {
    match choice {
        "command-bar-visible" => ui.command_bar.visible = !ui.command_bar.visible,
        "command-bar-defaults" => ui.command_bar = CommandBarPreferences::default(),
        "interface-navigation" => ui.show_tree = !ui.show_tree,
        "interface-bookmarks" => ui.show_bookmarks = !ui.show_bookmarks,
        "space-preview" => ui.space_preview = !ui.space_preview,
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
    let original = options.mode == ContextMenuMode::Original;
    let functions = submenu("菜单功能", vec![
        entry("剪切、复制和粘贴", "context-action-cut-copy-paste", true, options.show_cut_copy_paste),
        entry("复制路径", "context-action-copy-path", true, options.show_copy_path),
        entry("重命名", "context-action-rename", true, options.show_rename),
        entry("删除", "context-action-delete", true, options.show_delete),
        entry("属性", "context-action-properties", true, options.show_properties),
        entry("共享", "context-action-share", true, options.show_share),
        entry("新建", "context-action-new", true, options.show_new),
        entry("7-Zip", "context-action-seven-zip", true, options.show_seven_zip),
        entry("WPS", "context-action-wps", true, options.show_wps),
    ]);
    menu(vec![
        submenu("文件管理器菜单样式", vec![
            entry("原版菜单（可自定义）", "context-menu-original", true, original),
            entry("Windows 11（可自定义）", "context-menu-windows11", true, !original),
        ]),
        MenuItemSpec::Separator,
        functions,
        entry("功能开关对原版与 Win11 菜单均生效", "context-menu-windows11-hint", false, false),
        MenuItemSpec::Separator,
        item("恢复默认右键设置", "context-menu-defaults"),
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
    /// `None` when this edition has no desktop fences.
    pub desktop_running: Option<bool>,
    /// 0 = 不接管, 1 = 右键菜单, 2 = 默认打开文件夹与磁盘.
    pub integration: u8,
    /// Whether a background login agent is registered to intercept Win+E.
    pub win_e_agent: bool,
}

/// Application menu (≡) next to the window buttons.
pub fn app_menu(state: AppMenuState) -> MenuSpec {
    let mut items = vec![item("新建标签页\tCtrl+T", "new-tab"),item("快速搜索\tCtrl+F","quick-search"),item("重建文件名索引","search-rebuild-index"), MenuItemSpec::Separator];
    if let Some(running) = state.desktop_running {
        items.push(submenu("桌面格子", vec![
            item(if running { "显示桌面格子" } else { "启动桌面格子" }, "desktop-start"),
            item("一键整理桌面", "desktop-organize"),
            item("将当前文件夹映射为格子", "desktop-map"),
            MenuItemSpec::Separator,
            entry("显示托盘图标", "desktop-tray", running, false),
            entry("退出桌面格子", "desktop-quit", running, false),
        ]));
    }
    items.push(submenu("系统集成", vec![
        entry("不接管", "integration-off", true, state.integration == 0),
        entry("右键菜单“使用 ZSDesk 打开”", "integration-context", true, state.integration == 1),
        entry("默认用 ZSDesk 打开文件夹和磁盘", "integration-default", true, state.integration == 2),
        MenuItemSpec::Separator,
        entry("Win+E 打开 ZSDesk（需后台常驻）", "integration-win-e", true, state.win_e_agent),
        MenuItemSpec::Separator,
        entry("恢复系统资源管理器", "integration-restore", state.integration != 0, false),
        MenuItemSpec::Separator,
        item("文件位置转发…", "routing-settings"),
    ]));
    let mut rest = vec![
        MenuItemSpec::Separator,
        item("界面设置", "interface-settings"),
        item("右键菜单设置", "context-menu-settings"),
        MenuItemSpec::Separator,
        item("收藏当前文件夹\tCtrl+D", "bookmark"),
        item("文件夹选项", "folder-options"),
        item("键盘快捷键", "shortcuts"),
        MenuItemSpec::Separator,
        item("关于 ZSDesk", "about"),
    ];
    items.append(&mut rest);
    menu(items)
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
        let app = ids(&app_menu(AppMenuState::default()));
        assert!(app.contains(&"interface-settings".into()));
        assert!(app.contains(&"context-menu-settings".into()));
        for id in nested_ids(&interface_settings_menu(&ui)) {
            assert!(apply_interface_setting(&mut ui.clone(), &id), "unhandled setting {id}");
        }
        assert!(apply_interface_setting(&mut ui, "command-bar-defaults"));
        assert_eq!(ui.command_bar, CommandBarPreferences::default());
        assert!(!ui.show_bookmarks, "restoring command buttons must preserve the bookmark preference");
        assert_eq!(COMMAND_BAR_CHOICES.len(), CommandBarItem::ALL.len());
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
    fn windows11_mode_keeps_menu_customization_available() {
        let mut options = ContextMenuPreferences::default();
        let spec = context_menu_settings_menu(&options);
        let available = nested_ids(&spec);
        assert!(available.contains(&"context-action-delete".into()));
        assert!(apply_context_menu_setting(&mut options, "context-action-delete"));
        assert!(!options.show_delete);
        assert_eq!(options.mode, ContextMenuMode::Windows11);
        assert!(spec.items.iter().any(|item| matches!(item,
            MenuItemSpec::Submenu { label, enabled: true, .. } if label == "菜单功能")));
        assert!(spec.items.iter().any(|item| matches!(item,
            MenuItemSpec::Command { label, enabled: false, .. } if label == "功能开关对原版与 Win11 菜单均生效")));
    }
}
