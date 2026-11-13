//! File-location routing choices. Discovery and setup run on the controller
//! worker; selecting applications does not inject or enable anything by itself.
use super::*;
use crate::routing_control;
use zsdesk_ui::menus::{entry, menu, submenu, MenuItemSpec};

impl WindowState {
    pub(super) fn show_routing_settings(&mut self) {
        let mut settings = match routing_control::load(&self.config_path) {
            Ok(value) => value,
            Err(error) => {
                self.show_message(format!("无法读取文件位置转发配置：{error}"));
                return;
            }
        };
        let available = routing_control::bundle_available();
        let status = if settings.pending_changes {
            "选择已保存，尚未应用"
        } else if settings.enabled {
            "文件位置转发已启用"
        } else {
            "文件位置转发未启用"
        };
        let mut items = vec![
            entry(status, "status", false, false),
            MenuItemSpec::Separator,
        ];
        let mut known = Vec::new();
        let mut others = Vec::new();
        for app in &settings.applications {
            let name = app.label.replace('&', "&&");
            let filename=app.executable.file_name().unwrap_or_default().to_string_lossy();
            let detail=if name.eq_ignore_ascii_case(&filename){app.executable.parent().and_then(|p|p.file_name()).unwrap_or_default().to_string_lossy()}else{filename};
            let label=format!("{name} — {}",detail.replace('&',"&&"));
            let item = entry(
                &label,
                &format!("application-{}", app.id),
                true,
                app.enabled,
            );
            if app.label == "Codex" || app.label == "微信" {
                known.push(item);
            } else {
                others.push(item);
            }
        }
        if !known.is_empty() {
            items.push(submenu("Codex / 微信", known));
        }
        if !others.is_empty() {
            // Pages keep a long installed-application list within native menu bounds.
            if others.len() <= 20 {
                items.push(submenu("其他应用", others));
            } else {
                for (index, chunk) in others.chunks(20).enumerate() {
                    items.push(submenu(
                        &format!("其他应用 {}–{}", index * 20 + 1, index * 20 + chunk.len()),
                        chunk.to_vec(),
                    ));
                }
            }
        }
        items.extend([
            entry("发现可配置的应用…", "discover", available, false),
            entry("添加其他程序…", "add", available, false),
            MenuItemSpec::Separator,
            entry("登录时启用", "startup", true, settings.start_with_windows),
            entry(
                "一键启用所选应用",
                "apply",
                available && settings.applications.iter().any(|a| a.enabled),
                false,
            ),
            entry(
                "停用文件位置转发",
                "disable",
                settings.control_path.is_some(),
                false,
            ),
        ]);
        if !settings.applications.is_empty() {
            items.push(submenu("补充程序组件（高级）", settings.applications.iter().map(|app| {
                entry(&format!("{}{}", app.label.replace('&', "&&"), if app.profile == 4 { "（已配置）" } else { "" }),
                    &format!("caller-{}", app.id), available, false)
            }).collect()));
            items.push(submenu(
                "移除应用",
                settings
                    .applications
                    .iter()
                    .map(|a| {
                        entry(
                            &a.label.replace('&', "&&"),
                            &format!("remove-{}", a.id),
                            true,
                            false,
                        )
                    })
                    .collect(),
            ));
        }
        if !available {
            items.push(entry(
                "当前安装包未包含路由组件",
                "package-missing",
                false,
                false,
            ));
        }
        let choice = super::input::popup(self.hwnd, &menu(items), Some(self.layout.app_menu));
        let result = match choice.as_deref() {
            Some("discover") => routing_control::request_discovery(&self.config_path, self.hwnd),
            Some("add") => match pick_executable(self.hwnd) {
                Some(path) => routing_control::request_add(&self.config_path, path, self.hwnd),
                None => Ok(()),
            },
            Some("apply") => routing_control::request_apply(&self.config_path, self.hwnd),
            Some("disable") => {
                routing_control::configure(&self.config_path, false, self.hwnd).map(|_| ())
            }
            Some("startup") => {
                settings.start_with_windows = !settings.start_with_windows;
                routing_control::save_draft(&self.config_path, &settings)
                    .map(|_| self.show_message("登录启动选择已保存，应用设置后生效".into()))
            }
            Some(value) if value.starts_with("application-") => {
                if let Ok(id) = value[12..].parse::<u32>() {
                    if let Some(app) = settings.applications.iter_mut().find(|a| a.id == id) {
                        app.enabled = !app.enabled;
                    }
                }
                routing_control::save_draft(&self.config_path, &settings)
                    .map(|_| self.show_message("应用选择已保存，点击“一键启用所选应用”生效".into()))
            }
            Some(value) if value.starts_with("remove-") => {
                if let Ok(id) = value[7..].parse::<u32>() {
                    settings.applications.retain(|a| a.id != id);
                }
                routing_control::save_draft(&self.config_path, &settings)
                    .map(|_| self.show_message("应用已从选择中移除，应用设置后更新转发规则".into()))
            }
            Some(value) if value.starts_with("caller-") => {
                match value[7..].parse::<u32>().ok().and_then(|id| settings.applications.iter().find(|app| app.id == id)) {
                    Some(app) => match pick_module(self.hwnd) {
                        Some(module) => routing_control::request_add_with_caller(&self.config_path, app.executable.clone(), module, self.hwnd),
                        None => Ok(()),
                    },
                    None => Ok(()),
                }
            }
            _ => Ok(()),
        };
        if let Err(error) = result {
            self.show_message(format!("文件位置设置未完成：{error}"));
        }
    }
}

fn pick_executable(owner: HWND) -> Option<PathBuf> {
    pick_component(owner, false)
}

fn pick_module(owner: HWND) -> Option<PathBuf> {
    pick_component(owner, true)
}

fn pick_component(owner: HWND, module: bool) -> Option<PathBuf> {
    use windows::{
        core::w,
        Win32::{
            System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER},
            UI::Shell::{
                FileOpenDialog, IFileOpenDialog, FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM,
                SIGDN_FILESYSPATH,
            },
        },
    };
    let dialog: IFileOpenDialog =
        unsafe { CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()? };
    unsafe {
        let flags = dialog.GetOptions().ok()?;
        dialog
            .SetOptions(flags | FOS_FILEMUSTEXIST | FOS_FORCEFILESYSTEM)
            .ok()?;
        let _ = dialog.SetTitle(if module { w!("选择该程序目录内负责打开文件夹的组件（.dll）") }
            else { w!("选择需要转发文件定位的程序（.exe）") });
        dialog.Show(Some(owner)).ok()?;
        let item = dialog.GetResult().ok()?;
        let path = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let text = path.to_string();
        CoTaskMemFree(Some(path.0.cast()));
        let path = PathBuf::from(text.ok()?);
        if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case(if module { "dll" } else { "exe" }))
        {
            Some(path)
        } else {
            None
        }
    }
}
