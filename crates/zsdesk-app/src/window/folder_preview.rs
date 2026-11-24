use std::path::Path;
use windows::{
    core::w,
    Win32::{
        Foundation::{COLORREF, HWND, LPARAM, WPARAM},
        Graphics::Gdi::HFONT,
        Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL},
        UI::{Controls::*, Shell::*, WindowsAndMessaging::*},
    },
};

pub(super) fn create(
    owner: HWND,
    path: &Path,
    font: HFONT,
    background: u32,
    foreground: u32,
    cancel: impl Fn() -> bool,
) -> anyhow::Result<(HWND, usize, usize)> {
    use super::folder_contents::{entries, Visibility};
    let (items, count) = entries(path, Visibility::current(), 500, cancel)?;
    unsafe {
        InitCommonControlsEx(&INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES,
        })
        .ok()?;
    }
    let list = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            WC_LISTVIEWW,
            None,
            WS_CHILD
                | WS_VISIBLE
                | WS_VSCROLL
                | WINDOW_STYLE(
                    LVS_REPORT | LVS_NOCOLUMNHEADER | LVS_SINGLESEL | LVS_SHAREIMAGELISTS,
                ),
            0,
            0,
            1,
            1,
            Some(owner),
            None,
            None,
            None,
        )
    }?;
    crate::report_list::style(list, COLORREF(background), COLORREF(foreground));
    crate::report_list::heading(list, 0, "", 320, true);
    unsafe {
        SendMessageW(
            list,
            WM_SETFONT,
            Some(WPARAM(font.0 as usize)),
            Some(LPARAM(0)),
        );
    }
    let mut icons = [-1i32; 2];
    for (i, attrs) in [FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_DIRECTORY]
        .into_iter()
        .enumerate()
    {
        let mut info = SHFILEINFOW::default();
        let images = unsafe {
            SHGetFileInfoW(
                w!("entry"),
                attrs,
                Some(&mut info),
                std::mem::size_of::<SHFILEINFOW>() as u32,
                SHGFI_SYSICONINDEX | SHGFI_SMALLICON | SHGFI_USEFILEATTRIBUTES,
            )
        };
        if images != 0 {
            icons[i] = info.iIcon;
            unsafe {
                SendMessageW(
                    list,
                    LVM_SETIMAGELIST,
                    Some(WPARAM(LVSIL_SMALL as usize)),
                    Some(LPARAM(images as isize)),
                );
            }
        }
    }
    for (row, entry) in items.iter().enumerate() {
        crate::report_list::cell(list, row, 0, &entry.name.to_string_lossy());
        let image = icons[entry.folder as usize];
        if image >= 0 {
            let item = LVITEMW {
                mask: LVIF_IMAGE,
                iItem: row as i32,
                iImage: image,
                ..Default::default()
            };
            unsafe {
                SendMessageW(
                    list,
                    LVM_SETITEMW,
                    None,
                    Some(LPARAM(&item as *const _ as isize)),
                );
            }
        }
    }
    Ok((list, count, items.len()))
}
