//! File shortcuts are separate from menu visibility and native text editing.

use windows::{core::{Error, HRESULT, Interface, Result}, Win32::{Foundation::{HWND, E_NOTIMPL, S_FALSE, S_OK},
    UI::{Input::KeyboardAndMouse::*, Shell::{IFolderView2, IShellView, SVGIO_ALLVIEW, SVGIO_SELECTION, SVSI_SELECT},
        WindowsAndMessaging::MSG}}};
use zsdesk_windows::shell::menu::{HistoryAction, ShellMenu};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Modifiers { pub control: bool, pub shift: bool, pub alt: bool }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FileShortcut { Copy, Cut, Paste, SelectAll, InvertSelection, Rename, Delete { permanent: bool }, Properties, Undo, Redo }

pub(super) fn resolve(key: u16, modifiers: Modifiers, file_context: bool, text_editing: bool) -> Option<FileShortcut> {
    if !file_context || text_editing { return None; }
    let Modifiers { control, shift, alt } = modifiers;
    if control && shift && !alt && key == VK_A.0 { return Some(FileShortcut::InvertSelection); }
    if control && !shift && !alt {
        return match key {
            key if key == VK_C.0 || key == VK_INSERT.0 => Some(FileShortcut::Copy),
            key if key == VK_X.0 => Some(FileShortcut::Cut),
            key if key == VK_V.0 => Some(FileShortcut::Paste),
            key if key == VK_A.0 => Some(FileShortcut::SelectAll),
            key if key == VK_Z.0 => Some(FileShortcut::Undo),
            key if key == VK_Y.0 => Some(FileShortcut::Redo),
            _ => None,
        };
    }
    if shift && !control && !alt && key == VK_INSERT.0 { return Some(FileShortcut::Paste); }
    if !control && !alt && key == VK_DELETE.0 { return Some(FileShortcut::Delete { permanent: shift }); }
    if !control && !shift && !alt && key == VK_F2.0 { return Some(FileShortcut::Rename); }
    if alt && !control && !shift && key == VK_RETURN.0 { return Some(FileShortcut::Properties); }
    None
}

/// Native views own their undo/redo routing. Give them the original message
/// before considering a currently available background-menu command.
pub(super) fn execute_message(view: &IShellView, owner: HWND, action: FileShortcut, message: &MSG) -> Result<()> {
    if matches!(action, FileShortcut::Undo | FileShortcut::Redo | FileShortcut::SelectAll) {
        return translate_history_then_invoke(|| unsafe { view.TranslateAccelerator(message) },
            || execute(view, owner, action));
    }
    execute(view, owner, action)
}

fn translate_history_then_invoke(translate: impl FnOnce() -> HRESULT, invoke: impl FnOnce() -> Result<()>) -> Result<()> {
    match translate() {
        S_OK => Ok(()),
        // S_FALSE means unhandled. A view may also explicitly lack this method.
        S_FALSE | E_NOTIMPL => invoke(),
        failure => Err(Error::from_hresult(failure)),
    }
}

/// Must run on the view's STA without holding the application state RefCell.
pub(super) fn execute(view: &IShellView, owner: HWND, action: FileShortcut) -> Result<()> {
    use zsdesk_windows::shell::file_transfer;
    // The direct path needs filesystem paths on both sides. Items inside a zip
    // (or another virtual folder), a zip as paste target and a clipboard that
    // holds only virtual file contents have none: the folder's own Shell verb
    // handles those, exactly as the toolbar buttons already do.
    if matches!(action,FileShortcut::Copy|FileShortcut::Cut|FileShortcut::Delete{..})&&file_transfer::filesystem_selection(view)?{
        return match action{
            FileShortcut::Copy=>file_transfer::copy_view(view,owner,false),
            FileShortcut::Cut=>file_transfer::copy_view(view,owner,true),
            FileShortcut::Delete{permanent}=>file_transfer::delete_view(view,unsafe{view.GetWindow()?},permanent),
            _=>unreachable!(),
        };
    }
    if action==FileShortcut::Paste&&file_transfer::has_filesystem_clipboard(){
        if let Some(path)=file_transfer::filesystem_folder_path(view)?{return file_transfer::paste(unsafe{view.GetWindow()?},&path);}
    }
    if action == FileShortcut::InvertSelection { return zsdesk_windows::shell::commands::invert_view_selection(view); }
    if matches!(action, FileShortcut::Undo | FileShortcut::Redo) {
        let menu = ShellMenu::for_view(view, false)?;
        let action = if action == FileShortcut::Undo { HistoryAction::Undo } else { HistoryAction::Redo };
        return menu.invoke_history(owner, action);
    }
    if matches!(action, FileShortcut::Copy | FileShortcut::Cut | FileShortcut::Rename | FileShortcut::Delete { .. } | FileShortcut::Properties) {
        if let Ok(folder) = view.cast::<IFolderView2>() {
            if unsafe { folder.ItemCount(SVGIO_SELECTION) }.is_ok_and(|count| count == 0) { return Ok(()); }
        }
    }
    if action == FileShortcut::Rename { return unsafe { view.cast::<IFolderView2>()?.DoRename() }; }
    if action == FileShortcut::SelectAll {
        let folder: IFolderView2 = view.cast()?;
        let count = unsafe { folder.ItemCount(SVGIO_ALLVIEW)? }.max(0);
        for index in 0..count { unsafe { folder.SelectItem(index, SVSI_SELECT.0 as u32)?; } }
        return Ok(());
    }
    let (selection, verb, shift) = match action {
        FileShortcut::Copy => (true, "copy", false), FileShortcut::Cut => (true, "cut", false),
        FileShortcut::Paste => (false, "paste", false), FileShortcut::Delete { permanent } => (true, "delete", permanent),
        FileShortcut::Properties => (true, "properties", false), _ => unreachable!(),
    };
    let menu = ShellMenu::for_view(view, selection)?;
    menu.populate_for_invoke(shift)?;
    menu.invoke_verb(owner, verb, shift)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn common_file_keys_resolve_without_menu_preferences() {
        let ctrl = Modifiers { control: true, ..Default::default() };
        for (key, action) in [(VK_C, FileShortcut::Copy), (VK_X, FileShortcut::Cut), (VK_V, FileShortcut::Paste),
            (VK_A, FileShortcut::SelectAll), (VK_Z, FileShortcut::Undo), (VK_Y, FileShortcut::Redo)] {
            assert_eq!(resolve(key.0, ctrl, true, false), Some(action));
        }
        assert_eq!(resolve(VK_F2.0, Modifiers::default(), true, false), Some(FileShortcut::Rename));
        assert_eq!(resolve(VK_DELETE.0, Modifiers::default(), true, false), Some(FileShortcut::Delete { permanent: false }));
        assert_eq!(resolve(VK_DELETE.0, Modifiers { shift: true, ..Default::default() }, true, false), Some(FileShortcut::Delete { permanent: true }));
    }
    #[test]
    fn native_history_handling_precedes_fallback_and_runs_once() {
        use std::cell::Cell;
        let calls = Cell::new(0);
        translate_history_then_invoke(|| { calls.set(1); HRESULT(0) }, || {
            calls.set(100); Ok(())
        }).unwrap();
        assert_eq!(calls.get(), 1);
        translate_history_then_invoke(|| { calls.set(1); HRESULT(1) }, || {
            assert_eq!(calls.get(), 1); calls.set(2); Ok(())
        }).unwrap();
        assert_eq!(calls.get(), 2);
    }
    #[test]
    fn native_history_failure_does_not_repeat_the_operation() {
        let failure = HRESULT(0x80004005u32 as i32);
        let error = translate_history_then_invoke(|| failure, || panic!("failed command must not run again")).unwrap_err();
        assert_eq!(error.code(), failure);
        let disabled = HRESULT(0x80040101u32 as i32);
        let error = translate_history_then_invoke(|| HRESULT(1), || Err(Error::from_hresult(disabled))).unwrap_err();
        assert_eq!(error.code(), disabled);
    }
    #[test]
    fn editors_and_other_panes_keep_their_native_shortcuts() {
        for key in [VK_C, VK_X, VK_V, VK_A, VK_Z, VK_Y, VK_DELETE, VK_F2] {
            for control in [false, true] {
                let keys = Modifiers { control, ..Default::default() };
                assert_eq!(resolve(key.0, keys, true, true), None);
                assert_eq!(resolve(key.0, keys, false, false), None);
            }
        }
        assert_eq!(resolve(VK_C.0, Modifiers { control: true, shift: true, alt: false }, true, false), None);
        let invert = Modifiers { control: true, shift: true, alt: false };
        assert_eq!(resolve(VK_A.0, invert, true, false), Some(FileShortcut::InvertSelection));
        assert_eq!(resolve(VK_A.0, invert, true, true), None);
        assert_eq!(resolve(VK_A.0, invert, false, false), None);
        assert_eq!(resolve(VK_A.0, Modifiers { alt: true, ..invert }, true, false), None);
    }
}
