//! Common actions use the host's validated target. Shell extension discovery
//! starts only when the full menu is requested.
use super::{Mode, Request};
use zsdesk_ui::menus::{self, MenuItemSpec, MenuSpec};
use zsdesk_windows::shell::{file_transfer, menu::{ContextMenuOptions, MenuOutcome}};

pub(super) fn enabled(request: &Request) -> bool {
    request.options.quick_first && !request.extended && matches!(request.mode, Mode::More | Mode::Background)
}

pub(super) fn specification(request: &Request) -> MenuSpec {
    let background = request.mode == Mode::Background;
    let files = request.file_actions && (background || file_transfer::paths_from_pidls(&request.pidls).is_ok());
    let paste = background && file_transfer::has_paste_content();
    command_menu(&request.options, background, request.pidls.len(), files, request.open, request.rename, paste)
}

fn command_menu(options: &ContextMenuOptions, background: bool, count: usize, files: bool, open: bool, rename: bool, paste: bool) -> MenuSpec {
    let mut commands = Vec::new();
    for (available, verb, caption) in [
        (!background && count == 1 && open, "open", "打开"),
        (!background && files, "cut", "剪切"),
        (!background && files, "copy", "复制"),
        (paste, "paste", "粘贴"),
        (!background && count == 1 && files && rename, "rename", "重命名"),
        (!background && files, "delete", "删除"),
        (true, "properties", "属性"),
    ] {
        let label=zsdesk_core::i18n::text(caption);
        if available && zsdesk_windows::shell::menu::common_command_visible(options,verb,label) {
            commands.push(menus::item(label,verb));
        }
    }
    if !commands.is_empty() { commands.push(MenuItemSpec::Separator); }
    commands.push(menus::item(zsdesk_core::i18n::text("完整右键菜单…"),"more"));
    menus::menu(commands)
}

pub(super) fn outcome(id: &str) -> Option<MenuOutcome> {
    Some(match id {
        "open" => MenuOutcome::Open,
        "cut" => MenuOutcome::Cut,
        "copy" => MenuOutcome::Copy,
        "paste" => MenuOutcome::Paste,
        "rename" => MenuOutcome::Rename,
        "delete" => MenuOutcome::Delete { permanent:false },
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use zsdesk_ui::menus::UiCommand;

    fn ids(spec: MenuSpec) -> Vec<String> {
        spec.items.into_iter().filter_map(|row| match row {
            MenuItemSpec::Command { command: UiCommand::Custom { id,.. },.. } => Some(id),
            _ => None,
        }).collect()
    }

    #[test]
    fn common_page_respects_targets_and_visibility() {
        let mut options=ContextMenuOptions::default();
        options.hidden_verbs.push("copy".into());
        let multi=ids(command_menu(&options,false,2,true,true,true,false));
        assert_eq!(multi,["cut","delete","properties","more"]);
        let folder=ids(command_menu(&options,true,1,true,false,false,true));
        assert_eq!(folder,["paste","properties","more"]);
        let virtual_item=ids(command_menu(&options,false,1,false,false,false,false));
        assert_eq!(virtual_item,["properties","more"]);
        assert!(outcome("more").is_none() && outcome("properties").is_none());
    }
}
