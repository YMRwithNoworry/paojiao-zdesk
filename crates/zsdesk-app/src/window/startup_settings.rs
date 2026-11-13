//! Search shortcut and login switches exposed in the main file-manager window.
use super::WindowState;
use crate::quick_search::{self, control};
use zsdesk_core::model::explorer::QuickSearchPreferences;
use zsdesk_ui::menus::{self, MenuItemSpec, MenuSpec};

const HOTKEYS: &[(&str, &str)] = &[
    ("search-hotkey-default", "Ctrl+Alt+Space"),
    ("search-hotkey-shift-space", "Ctrl+Shift+Space"),
    ("search-hotkey-alt-f", "Ctrl+Alt+F"),
    ("search-hotkey-alt-f11", "Alt+F11"),
];

fn settings_menu(preferences: &QuickSearchPreferences, main_startup: Option<bool>, desktop_startup: Option<Option<bool>>) -> MenuSpec {
    let mut items = vec![
        menus::entry("搜索与开机启动（当前配置）", "startup-heading", false, false),
        menus::entry("启用全局搜索快捷键（唤出主窗口）", "search-hotkey-enabled", true, preferences.enabled),
        menus::submenu(&format!("全局搜索快捷键：{}", preferences.hotkey), HOTKEYS.iter().map(|(id, hotkey)| {
            menus::entry(hotkey, id, true, preferences.hotkey.eq_ignore_ascii_case(hotkey))
        }).collect()),
        menus::entry("登录 Windows 时启用后台快捷键（不打开窗口）", "search-background-login", preferences.enabled, preferences.enabled && preferences.start_with_windows),
        MenuItemSpec::Separator,
        menus::entry(if main_startup.is_some() { "登录 Windows 时打开文件管理主窗口" } else { "主窗口登录启动（无法读取状态）" },
            "main-window-login", main_startup.is_some(), main_startup.unwrap_or(false)),
    ];
    if let Some(enabled) = desktop_startup {
        items.push(menus::entry(if enabled.is_some() { "登录 Windows 时启动桌面格子" } else { "桌面格子登录启动（无法读取状态）" },
            "desktop-login", enabled.is_some(), enabled.unwrap_or(false)));
    }
    items.extend([MenuItemSpec::Separator, menus::item("查看全局快捷键运行状态", "search-hotkey-status")]);
    menus::menu(items)
}

/// Selecting a key changes only the chord; enabling login requires a separate
/// deliberate action. Disabling the shortcut also disables its login entry.
fn preference_action(preferences: &mut QuickSearchPreferences, id: &str) -> bool {
    match id {
        "search-hotkey-enabled" => {
            preferences.enabled = !preferences.enabled;
            if !preferences.enabled { preferences.start_with_windows = false; }
        }
        "search-background-login" if preferences.enabled => preferences.start_with_windows = !preferences.start_with_windows,
        _ => if let Some((_, hotkey)) = HOTKEYS.iter().find(|(choice, _)| *choice == id) {
            preferences.hotkey = (*hotkey).into();
        } else { return false; },
    }
    true
}

impl WindowState {
    pub(super) unsafe fn search_settings_menu(&mut self) {
        let mut preferences = match control::load(&self.config_path) {
            Ok(configuration) => configuration.preferences,
            Err(error) => { self.show_message(format!("无法读取搜索与启动设置：{error:#}")); return; }
        };
        let main_startup = control::main_window_startup::enabled(&self.config_path);
        let desktop_config = self.config_path.with_file_name("desktop-fences.json");
        let desktop_startup = crate::edition::DESKTOP.then(|| crate::desktop::startup_enabled(&desktop_config));
        let spec = settings_menu(&preferences, main_startup.as_ref().ok().copied(),
            desktop_startup.as_ref().map(|result| result.as_ref().ok().copied()));
        if let Err(error) = &main_startup { self.show_message(format!("无法读取主窗口登录启动设置：{error:#}")); }
        if let Some(Err(error)) = &desktop_startup { self.show_message(format!("无法读取桌面格子登录启动设置：{error:#}")); }
        let Some(choice) = super::input::popup(self.hwnd, &spec, Some(self.layout.app_menu)) else { return; };

        if preference_action(&mut preferences, &choice) {
            let enabled = preferences.enabled;
            if let Err(error) = control::apply(&self.config_path, preferences) {
                self.show_message(format!("搜索与启动设置未保存：{error:#}")); return;
            }
            // Keep the integration receiver alive when the global shortcut is
            // disabled; its reload path unregisters only the search shortcut.
            let action = if enabled || crate::explorer_forwarder::owns_integration(&self.config_path) { "reload" } else { "stop" };
            let result = control::signal(&self.config_path, action)
                .and_then(|_| quick_search::start_if_enabled(&self.config_path));
            match result {
                Ok(()) => self.show_message(if enabled { "设置已保存；后台正在应用全局快捷键，可在运行状态中检查是否注册成功。".into() }
                    else { "全局搜索快捷键及其后台登录启动已关闭。主窗口和桌面格子的登录启动分别设置。".into() }),
                Err(error) => self.show_message(format!("设置已保存，后台应用失败：{error:#}")),
            }
            return;
        }
        match choice.as_str() {
            "main-window-login" => if let Ok(current) = main_startup {
                match control::main_window_startup::set_enabled(&self.config_path, !current) {
                    Ok(()) => self.show_message(if current { "主窗口登录启动已关闭" } else { "下次登录 Windows 时将打开文件管理主窗口" }.into()),
                    Err(error) => self.show_message(format!("主窗口登录启动未更改：{error:#}")),
                }
            },
            "desktop-login" => if let Some(Ok(current)) = desktop_startup {
                match crate::desktop::set_startup_enabled(&desktop_config, !current) {
                    Ok(()) => self.show_message(if current { "桌面格子登录启动已关闭" } else { "下次登录 Windows 时将启动桌面格子" }.into()),
                    Err(error) => self.show_message(format!("桌面格子登录启动未更改：{error:#}")),
                }
            },
            "search-hotkey-status" => {
                let status = control::status(&self.config_path);
                let saved = &status["last_status"];
                let live = status["running"].as_bool().unwrap_or(false);
                let matching_key = saved["hotkey"].as_str().is_some_and(|value| value.eq_ignore_ascii_case(&preferences.hotkey));
                let registered = preferences.enabled && live && matching_key && saved["hotkey_registered"].as_bool().unwrap_or(false);
                let state = if !preferences.enabled { "已关闭" } else if registered { "已注册" }
                    else if live && !matching_key { "后台正在应用快捷键" } else if live { "后台运行，快捷键未注册" } else { "后台未运行" };
                let detail = if live { saved["error"].as_str().unwrap_or("") } else { "" };
                self.show_message(format!("全局搜索快捷键 {}：{}{}{}", preferences.hotkey, state,
                    if detail.is_empty() { "" } else { "；" }, detail));
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zsdesk_ui::menus::UiCommand;

    fn command<'a>(menu: &'a MenuSpec, id: &str) -> Option<(&'a str, bool, bool)> {
        menu.items.iter().find_map(|item| match item {
            MenuItemSpec::Command { label, command: UiCommand::Custom { id: value, .. }, enabled, checked, .. } if value == id => Some((label.as_str(), *enabled, *checked)),
            MenuItemSpec::Submenu { menu, .. } => command(menu, id),
            _ => None,
        })
    }

    #[test]
    fn login_switches_are_separate_and_desktop_is_edition_gated() {
        let preferences = QuickSearchPreferences::default();
        let standard = settings_menu(&preferences, Some(true), None);
        assert_eq!(command(&standard, "main-window-login").map(|(_, enabled, checked)| (enabled, checked)), Some((true, true)));
        assert_eq!(command(&standard, "search-background-login").map(|(_, enabled, checked)| (enabled, checked)), Some((false, false)));
        assert!(command(&standard, "desktop-login").is_none());
        let desktop = settings_menu(&preferences, None, Some(Some(false)));
        assert_eq!(command(&desktop, "main-window-login").map(|(_, enabled, _)| enabled), Some(false));
        assert_eq!(command(&desktop, "desktop-login").map(|(_, enabled, checked)| (enabled, checked)), Some((true, false)));
    }

    #[test]
    fn shortcut_changes_do_not_silently_enable_login() {
        let mut preferences = QuickSearchPreferences::default();
        assert!(preference_action(&mut preferences, "search-hotkey-alt-f"));
        assert!(!preferences.enabled && !preferences.start_with_windows);
        assert!(!preference_action(&mut preferences, "search-background-login"));
        assert!(preference_action(&mut preferences, "search-hotkey-enabled"));
        assert!(preferences.enabled && !preferences.start_with_windows);
        assert!(preference_action(&mut preferences, "search-background-login"));
        assert!(preferences.start_with_windows);
        assert!(preference_action(&mut preferences, "search-hotkey-enabled"));
        assert!(!preferences.enabled && !preferences.start_with_windows);
        for (_, hotkey) in HOTKEYS { assert!(control::validate_hotkey(hotkey).is_ok()); }
    }
}
