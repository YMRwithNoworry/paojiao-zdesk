//! Application menus (ZSUI specs) shown as Windows 11 style flyouts by the
//! file window and the desktop fences. High contrast keeps the native Win32
//! menu, which follows the system's contrast colors and assistive tooling.
use windows::{core::PCWSTR, Win32::{Foundation::{HWND, POINT}, UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, TrackPopupMenu, HMENU, MF_CHECKED, MF_ENABLED, MF_GRAYED, MF_POPUP, MF_SEPARATOR,
    MF_STRING, MF_UNCHECKED, TPM_LEFTALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON, TPM_TOPALIGN}}};
use zsdesk_ui::menus::{command_glyph, MenuItemSpec, MenuSpec, UiCommand};
use zsdesk_windows::{flyout::{self, FlyoutItem, Glyph}, flyout_layout::Anchor};

pub use zsdesk_windows::flyout_layout::{Anchor as MenuAnchor, Bounds as MenuBounds};

/// Shows `spec` and returns the chosen command id.
pub fn show(owner: HWND, spec: &MenuSpec, anchor: Anchor, dark: bool) -> Option<String> {
    let mut ids = Vec::new();
    if zsdesk_windows::theme::high_contrast() { return native(owner, spec, anchor); }
    let items = convert(&spec.items, &mut ids);
    flyout::track(owner, &items, anchor, dark).and_then(|index| ids.get(index).cloned())
}

/// Shows `spec`; a choice for which `apply` returns the updated spec (a
/// setting it just toggled) keeps the menu and its open submenu on screen.
/// Returns the choice that closed the menu.
pub fn show_persistent(owner: HWND, spec: &MenuSpec, anchor: Anchor, dark: bool,
        apply: &mut dyn FnMut(&str) -> Option<MenuSpec>) -> Option<String> {
    if zsdesk_windows::theme::high_contrast() { return native(owner, spec, anchor); }
    let mut first = Vec::new();
    let items = convert(&spec.items, &mut first);
    let ids = std::cell::RefCell::new(first);
    let chosen = flyout::track_persistent(owner, &items, anchor, dark, &mut |index| {
        let id = ids.borrow().get(index).cloned()?;
        let next = apply(&id)?;
        let mut renewed = Vec::new();
        let items = convert(&next.items, &mut renewed);
        *ids.borrow_mut() = renewed;
        Some(items)
    });
    chosen.and_then(|index| ids.borrow().get(index).cloned())
}

fn convert(items: &[MenuItemSpec], ids: &mut Vec<String>) -> Vec<FlyoutItem> {
    let mut result = Vec::with_capacity(items.len());
    for item in items {
        match item {
            MenuItemSpec::Command { label, command: UiCommand::Custom { id, .. }, checked, enabled, .. } => {
                ids.push(id.clone());
                result.push(FlyoutItem::Command { id: ids.len() - 1, label: label.clone(), enabled: *enabled, checked: *checked,
                    glyph: command_glyph(id).map(|(code, accent)| Glyph { code, accent }) });
            }
            MenuItemSpec::Submenu { label, menu, enabled, .. } => {
                let children = convert(&menu.items, ids);
                if children.iter().any(|child| !matches!(child, FlyoutItem::Separator)) {
                    result.push(FlyoutItem::Submenu { label: label.clone(), enabled: *enabled, glyph: None, items: children });
                }
            }
            MenuItemSpec::Separator => result.push(FlyoutItem::Separator),
            _ => {}
        }
    }
    tidy_separators(result)
}

/// Win32 menus tolerated leading, trailing and doubled separators; a drawn
/// flyout would show them as stray lines.
fn tidy_separators(items: Vec<FlyoutItem>) -> Vec<FlyoutItem> {
    let mut result: Vec<FlyoutItem> = Vec::with_capacity(items.len());
    for item in items {
        let separator = matches!(item, FlyoutItem::Separator);
        if separator && result.last().is_none_or(|last| matches!(last, FlyoutItem::Separator)) { continue; }
        result.push(item);
    }
    while result.last().is_some_and(|last| matches!(last, FlyoutItem::Separator)) { result.pop(); }
    result
}

fn wide(text: &str) -> Vec<u16> { text.encode_utf16().chain(Some(0)).collect() }

fn native(owner: HWND, spec: &MenuSpec, anchor: Anchor) -> Option<String> {
    let mut ids = Vec::new();
    let menu = build_native(spec, &mut ids)?;
    let (point, flags) = match anchor {
        Anchor::Point { x, y } => (POINT { x, y }, TPM_RETURNCMD | TPM_RIGHTBUTTON),
        Anchor::Below(button) => (POINT { x: button.left, y: button.bottom }, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_LEFTALIGN | TPM_TOPALIGN),
    };
    let selected = unsafe { TrackPopupMenu(menu, flags, point.x, point.y, None, owner, None) }.0;
    let _ = unsafe { DestroyMenu(menu) };
    selected.checked_sub(1).and_then(|index| ids.get(index as usize)).cloned()
}

fn build_native(spec: &MenuSpec, ids: &mut Vec<String>) -> Option<HMENU> {
    let menu = unsafe { CreatePopupMenu() }.ok()?;
    for item in &spec.items {
        match item {
            MenuItemSpec::Command { label, command: UiCommand::Custom { id, .. }, checked, enabled, .. } => {
                ids.push(id.clone());
                let flags = MF_STRING | if *checked { MF_CHECKED } else { MF_UNCHECKED } | if *enabled { MF_ENABLED } else { MF_GRAYED };
                let _ = unsafe { AppendMenuW(menu, flags, ids.len(), PCWSTR(wide(label).as_ptr())) };
            }
            MenuItemSpec::Submenu { label, menu: inner, enabled, .. } => {
                if let Some(sub) = build_native(inner, ids) {
                    let flags = MF_POPUP | if *enabled { MF_ENABLED } else { MF_GRAYED };
                    if unsafe { AppendMenuW(menu, flags, sub.0 as usize, PCWSTR(wide(label).as_ptr())) }.is_err() {
                        let _ = unsafe { DestroyMenu(sub) };
                    }
                }
            }
            MenuItemSpec::Separator => { let _ = unsafe { AppendMenuW(menu, MF_SEPARATOR, 0, None) }; }
            _ => {}
        }
    }
    Some(menu)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zsdesk_ui::menus::{entry, menu, submenu};

    #[test]
    fn specs_convert_with_ids_glyphs_and_clean_separators() {
        let spec = menu(vec![
            MenuItemSpec::Separator,
            entry("撤销\tCtrl+Z", "undo", true, false),
            MenuItemSpec::Separator, MenuItemSpec::Separator,
            submenu("空", vec![MenuItemSpec::Separator]),
            submenu("查看", vec![entry("列表", "list", true, true)]),
            MenuItemSpec::Separator,
        ]);
        let mut ids = Vec::new();
        let items = convert(&spec.items, &mut ids);
        assert_eq!(ids, ["undo", "list"]);
        assert_eq!(items.len(), 3, "leading, doubled and trailing separators and empty submenus are dropped");
        assert!(matches!(&items[0], FlyoutItem::Command { id: 0, glyph: Some(Glyph { accent: true, .. }), .. }));
        assert!(matches!(&items[1], FlyoutItem::Separator));
        assert!(matches!(&items[2], FlyoutItem::Submenu { items, .. } if matches!(items[0], FlyoutItem::Command { id: 1, checked: true, glyph: None, .. })));
    }
}
