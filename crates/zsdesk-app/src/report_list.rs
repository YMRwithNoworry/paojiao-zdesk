//! Small helpers for native report controls. The control copies row and column text synchronously.
use windows::{
    core::PWSTR,
    Win32::{
        Foundation::{COLORREF, HWND, LPARAM, WPARAM},
        UI::{Controls::*, WindowsAndMessaging::SendMessageW},
    },
};
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}
pub fn style(list: HWND, bg: COLORREF, fg: COLORREF) {
    unsafe {
        SendMessageW(
            list,
            LVM_SETEXTENDEDLISTVIEWSTYLE,
            None,
            Some(LPARAM(
                (LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER | LVS_EX_LABELTIP) as isize,
            )),
        );
        for (msg, color) in [
            (LVM_SETBKCOLOR, bg),
            (LVM_SETTEXTBKCOLOR, bg),
            (LVM_SETTEXTCOLOR, fg),
        ] {
            SendMessageW(list, msg, None, Some(LPARAM(color.0 as isize)));
        }
    }
}
pub fn heading(list: HWND, index: usize, text: &str, width: i32, insert: bool) {
    let mut label = wide(text);
    let mut col = LVCOLUMNW {
        mask: if insert {
            LVCF_TEXT | LVCF_WIDTH
        } else {
            LVCF_TEXT
        },
        cx: width,
        pszText: PWSTR(label.as_mut_ptr()),
        ..Default::default()
    };
    unsafe {
        SendMessageW(
            list,
            if insert {
                LVM_INSERTCOLUMNW
            } else {
                LVM_SETCOLUMNW
            },
            Some(WPARAM(index)),
            Some(LPARAM(&mut col as *mut _ as isize)),
        );
    }
}
pub fn width(list: HWND, col: usize, w: i32) {
    unsafe {
        SendMessageW(
            list,
            LVM_SETCOLUMNWIDTH,
            Some(WPARAM(col)),
            Some(LPARAM(w as isize)),
        );
    }
}
pub fn cell(list: HWND, row: usize, col: i32, text: &str) {
    let mut label = wide(text);
    let mut item = LVITEMW {
        mask: LVIF_TEXT,
        iItem: row as i32,
        iSubItem: col,
        pszText: PWSTR(label.as_mut_ptr()),
        ..Default::default()
    };
    unsafe {
        SendMessageW(
            list,
            if col == 0 {
                LVM_INSERTITEMW
            } else {
                LVM_SETITEMTEXTW
            },
            Some(WPARAM(row)),
            Some(LPARAM(&mut item as *mut _ as isize)),
        );
    }
}
pub fn clear(list: HWND) {
    unsafe {
        SendMessageW(list, LVM_DELETEALLITEMS, None, None);
    }
}
pub fn selected(list: HWND) -> Option<usize> {
    usize::try_from(
        unsafe {
            SendMessageW(
                list,
                LVM_GETNEXTITEM,
                Some(WPARAM(usize::MAX)),
                Some(LPARAM(LVNI_SELECTED as isize)),
            )
        }
        .0,
    )
    .ok()
}
pub fn selected_rows(list: HWND) -> Vec<usize> {
    let mut indices = Vec::new();
    let mut current = usize::MAX;
    loop {
        let next = unsafe {
            SendMessageW(
                list,
                LVM_GETNEXTITEM,
                Some(WPARAM(current)),
                Some(LPARAM(LVNI_SELECTED as isize)),
            )
        }
        .0;
        if next < 0 || current != usize::MAX && next as usize <= current {
            break;
        }
        current = next as usize;
        indices.push(current);
    }
    indices
}
pub fn set_selection(list: HWND, all: bool) {
    let item = LVITEMW {
        stateMask: LVIS_SELECTED,
        state: if all {
            LVIS_SELECTED
        } else {
            LIST_VIEW_ITEM_STATE_FLAGS(0)
        },
        ..Default::default()
    };
    unsafe {
        SendMessageW(
            list,
            LVM_SETITEMSTATE,
            Some(WPARAM(usize::MAX)),
            Some(LPARAM(&item as *const _ as isize)),
        );
    }
}
pub fn select_first(list: HWND) {
    select(list, 0);
}
pub fn select(list: HWND, row: usize) {
    let state = LIST_VIEW_ITEM_STATE_FLAGS(LVIS_SELECTED.0 | LVIS_FOCUSED.0);
    let mut item = LVITEMW {
        stateMask: state,
        state: state,
        ..Default::default()
    };
    unsafe {
        SendMessageW(
            list,
            LVM_SETITEMSTATE,
            Some(WPARAM(row)),
            Some(LPARAM(&mut item as *mut _ as isize)),
        );
    }
}
