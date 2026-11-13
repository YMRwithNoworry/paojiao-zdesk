use std::{borrow::Cow, path::PathBuf};

use windows::{
    core::{Error, HRESULT, Interface, Result, PCWSTR, PWSTR},
    Win32::{
        Foundation::HWND,
        System::{Com::CoTaskMemFree, SystemServices::SFGAO_FOLDER},
        UI::{
            Shell::{
                FOLDERID_ComputerFolder, IFolderView2, IShellItem, IShellView,
                SHCreateItemFromIDList, SHCreateItemFromParsingName, SHGetKnownFolderItem, SHGetNameFromIDList,
                ShellExecuteW, KF_FLAG_DEFAULT, SIGDN_DESKTOPABSOLUTEPARSING, SIGDN_FILESYSPATH,
                SIGDN_NORMALDISPLAY,
            },
            WindowsAndMessaging::SW_SHOWNORMAL,
        },
    },
};
use zsdesk_core::model::explorer::ShellLocation;

/// Resolves an application location into a Windows Shell item.
///
/// # Safety
///
/// COM must be initialized on the current thread for the duration of this call
/// and while the returned interface is used.
pub unsafe fn shell_item(location: &ShellLocation) -> Result<IShellItem> {
    match location {
        ShellLocation::ThisPc => unsafe {
            SHGetKnownFolderItem(&FOLDERID_ComputerFolder, KF_FLAG_DEFAULT, None)
        },
        ShellLocation::Path(path) => {
            let text = wide(&path.to_string_lossy());
            unsafe { SHCreateItemFromParsingName(PCWSTR(text.as_ptr()), None) }
        }
        ShellLocation::Shell { parsing_name, pidl, .. } => {
            let identity_error=if let Some(identity)=pidl{
                match super::pidl::item_from_pidl(identity){Ok(item)=>return Ok(item),Err(error)=>Some(error)}
            }else{None};
            if parsing_name.trim().is_empty(){return Err(identity_error.unwrap_or_else(||Error::from_hresult(HRESULT(0x80070057u32 as i32))));}
            let text = wide(&normalized_shell_parsing_name(parsing_name));
            unsafe { SHCreateItemFromParsingName(PCWSTR(text.as_ptr()), None) }
        }
        ShellLocation::Search { query, scope } => super::search::search_item(scope.as_deref(), query),
    }
}

/// Whether `folder` is This PC.
pub(crate) fn is_this_pc_item(folder: &IShellItem) -> bool {
    unsafe { folder.GetDisplayName(SIGDN_DESKTOPABSOLUTEPARSING) }.ok()
        .is_some_and(|name| is_this_pc(&unsafe { take_pwstr(name) }))
}

/// This PC's folder shortcuts (3D Objects, Desktop, Documents, Downloads,
/// Music, Pictures, Videos). Drives, devices and network locations are kept;
/// the folders themselves stay reachable from Quick access and the Desktop.
pub(crate) fn is_this_pc_known_folder(item: &IShellItem) -> bool {
    use windows::Win32::UI::Shell::{SHGetKnownFolderPath, FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads,
        FOLDERID_Music, FOLDERID_Objects3D, FOLDERID_Pictures, FOLDERID_Videos, KNOWN_FOLDER_FLAG};
    let Some(path) = (unsafe { item.GetDisplayName(SIGDN_FILESYSPATH) }).ok().map(|name| unsafe { take_pwstr(name) }) else { return false };
    [&FOLDERID_Objects3D, &FOLDERID_Desktop, &FOLDERID_Documents, &FOLDERID_Downloads, &FOLDERID_Music, &FOLDERID_Pictures, &FOLDERID_Videos]
        .into_iter().any(|folder| unsafe { SHGetKnownFolderPath(folder, KNOWN_FOLDER_FLAG(0), None) }
            .is_ok_and(|known| unsafe { take_pwstr(known) }.eq_ignore_ascii_case(path.trim_end_matches('\\'))))
}

/// Tree filter form: the child of This PC that is one of its folder shortcuts.
pub(crate) fn hidden_in_this_pc(item: &IShellItem) -> bool {
    unsafe { item.GetParent() }.is_ok_and(|parent| is_this_pc_item(&parent)) && is_this_pc_known_folder(item)
}

fn normalized_shell_parsing_name(value: &str) -> Cow<'_, str> {
    // Recent Windows builds return the legacy Quick access CLSID as a parsing
    // name, but SHCreateItemFromParsingName accepts it only with the shell:
    // prefix. Apply the correction at the parse boundary so Shell callbacks,
    // restored sessions and refresh all resolve the same namespace again.
    if value.eq_ignore_ascii_case("::{679F85CB-0220-4080-B29B-5540CC05AAB6}") {
        Cow::Owned(format!("shell:{value}"))
    } else {
        Cow::Borrowed(value)
    }
}

pub fn is_recycle_bin_name(name:&str)->bool{
    let name=name.get(..6).filter(|prefix|prefix.eq_ignore_ascii_case("shell:")).map_or(name,|_|&name[6..]);
    name.eq_ignore_ascii_case("RecycleBinFolder")||name.eq_ignore_ascii_case("::{645FF040-5081-101B-9F08-00AA002F954E}")
}
pub(crate) fn is_recycle_bin_item(item:&IShellItem)->bool{
    unsafe{item.GetDisplayName(SIGDN_DESKTOPABSOLUTEPARSING)}.ok().is_some_and(|name|is_recycle_bin_name(&unsafe{take_pwstr(name)}))
}

pub unsafe fn selected_shell_item(view: &IShellView) -> Result<IShellItem> {
    let folder_view: IFolderView2 = view.cast()?;
    let selected = unsafe { folder_view.GetSelection(false)? };
    if unsafe { selected.GetCount()? } == 0 { return Err(Error::from_hresult(HRESULT(1))); }
    unsafe { selected.GetItemAt(0) }
}

pub unsafe fn is_folder(item: &IShellItem) -> bool {
    unsafe { item.GetAttributes(SFGAO_FOLDER) }
        .map(|attributes| attributes.0 & SFGAO_FOLDER.0 != 0)
        .unwrap_or(false)
}

/// Opens a file through its registered Windows Shell association.
///
/// # Safety
///
/// `owner` must be null or a valid window handle. The caller must ensure the
/// path remains valid for the duration of this synchronous call.
pub unsafe fn open_path(owner: HWND, path: &std::path::Path) {
    let path = wide(&path.to_string_lossy());
    unsafe {
        let _ = ShellExecuteW(
            Some(owner),
            PCWSTR::null(),
            PCWSTR(path.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

/// Converts a Shell item into a serializable application location.
///
/// # Safety
///
/// COM must be initialized on the current thread and `item` must belong to an
/// apartment that can be called from that thread.
pub unsafe fn location_from_shell_item(item: &IShellItem) -> Option<ShellLocation> {
    let pidl=super::pidl::item_pidl(item);
    // The namespace Desktop root also advertises a filesystem path, but its
    // empty PIDL denotes the virtual parent of This PC and other namespaces.
    let desktop_root=pidl.as_deref().is_some_and(|bytes|bytes==[0,0]);
    let filesystem=unsafe{item.GetAttributes(windows::Win32::System::SystemServices::SFGAO_FILESYSTEM)}
        .is_ok_and(|flags|flags.0&windows::Win32::System::SystemServices::SFGAO_FILESYSTEM.0!=0);
    if let Ok(path) = if filesystem&&!desktop_root{unsafe { item.GetDisplayName(SIGDN_FILESYSPATH) }}else{Err(Error::from_hresult(HRESULT(0x80004005u32 as i32)))} {
        let path = unsafe { take_pwstr(path) };
        if !path.is_empty() {
            return Some(ShellLocation::Path(PathBuf::from(path)));
        }
    }

    let parsing_name = unsafe { item.GetDisplayName(SIGDN_DESKTOPABSOLUTEPARSING) }.ok()
        .map(|name|unsafe{take_pwstr(name)}).unwrap_or_default();
    if parsing_name.trim().is_empty()&&pidl.is_none(){return None;}
    if is_this_pc(&parsing_name) {
        return Some(ShellLocation::ThisPc);
    }

    let title = unsafe { item.GetDisplayName(SIGDN_NORMALDISPLAY).ok() }
        .map(|value| unsafe { take_pwstr(value) })
        .filter(|value| !value.is_empty())
        .or_else(|| parsing_title(&parsing_name))
        .unwrap_or_else(|| "Shell".to_string());

    Some(ShellLocation::Shell {
        parsing_name,
        title,
        pidl,
    })
}

pub unsafe fn location_from_pidl(
    pidl: *const windows::Win32::UI::Shell::Common::ITEMIDLIST,
) -> Option<ShellLocation> {
    if pidl.is_null() {
        return None;
    }

    // The navigation callback supplies a PIDL, not a title. Resolve it through
    // the same normal-display-name path as clicks, including virtual folders.
    if let Ok(item) = unsafe { SHCreateItemFromIDList::<IShellItem>(pidl) } {
        if let Some(location) = unsafe { location_from_shell_item(&item) } { return Some(location); }
    }

    let identity=super::pidl::checked_pidl_bytes(pidl);
    let parsing_name = unsafe { SHGetNameFromIDList(pidl, SIGDN_DESKTOPABSOLUTEPARSING) }.ok()
        .map(|name|unsafe{take_pwstr(name)}).unwrap_or_default();
    if parsing_name.trim().is_empty()&&identity.is_none(){return None;}
    if is_this_pc(&parsing_name) {
        return Some(ShellLocation::ThisPc);
    }

    let title = unsafe { SHGetNameFromIDList(pidl, SIGDN_NORMALDISPLAY) }.ok()
        .map(|value| unsafe { take_pwstr(value) }).filter(|title| !title.trim().is_empty())
        .or_else(|| parsing_title(&parsing_name)).unwrap_or_default();
    Some(ShellLocation::Shell {
        parsing_name,
        title,
        pidl:identity,
    })
}

/// One breadcrumb segment: display name and target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Crumb { pub title: String, pub location: ShellLocation }

/// Shell parent chain of `location`, root first (the desktop root is omitted
/// unless it is the location itself).
///
/// # Safety
///
/// COM must be initialized on the current thread.
pub unsafe fn breadcrumb(location: &ShellLocation) -> Vec<Crumb> {
    let Ok(mut item) = (unsafe { shell_item(location) }) else { return Vec::new() };
    let mut chain = Vec::new();
    for _ in 0..64 {
        let title = unsafe { item.GetDisplayName(SIGDN_NORMALDISPLAY) }.map(|v| unsafe { take_pwstr(v) }).unwrap_or_default();
        let Some(target) = (unsafe { location_from_shell_item(&item) }) else { break };
        let parent = unsafe { item.GetParent() };
        let is_root = parent.is_err();
        if is_root && !chain.is_empty() { break; }
        chain.push(Crumb { title: if title.is_empty() { target.title() } else { title }, location: target });
        match parent { Ok(p) => item = p, Err(_) => break }
    }
    chain.reverse();
    chain
}

/// Child folders of a location for the breadcrumb chevron menu.
///
/// # Safety
///
/// COM must be initialized on the current thread.
pub unsafe fn child_folders(location: &ShellLocation, limit: usize) -> Vec<Crumb> {
    use windows::Win32::UI::Shell::{BHID_EnumItems, IEnumShellItems};
    use windows::Win32::System::SystemServices::{SFGAO_FOLDER as FOLDER, SFGAO_STREAM, SFGAO_HIDDEN};
    let Ok(item) = (unsafe { shell_item(location) }) else { return Vec::new() };
    let Ok(items) = (unsafe { item.BindToHandler::<_, IEnumShellItems>(None, &BHID_EnumItems) }) else { return Vec::new() };
    let mut result = Vec::new();
    loop {
        let mut fetched = [None];
        if unsafe { items.Next(&mut fetched, None) }.is_err() { break; }
        let Some(child) = fetched[0].take() else { break };
        let attrs = unsafe { child.GetAttributes(FOLDER | SFGAO_STREAM | SFGAO_HIDDEN) }.map(|a| a.0).unwrap_or(0);
        if attrs & FOLDER.0 == 0 || attrs & (SFGAO_STREAM.0 | SFGAO_HIDDEN.0) != 0 { continue; }
        let title = unsafe { child.GetDisplayName(SIGDN_NORMALDISPLAY) }.map(|v| unsafe { take_pwstr(v) }).unwrap_or_default();
        if let Some(target) = unsafe { location_from_shell_item(&child) } { result.push(Crumb { title, location: target }); }
        if result.len() >= limit { break; }
    }
    result.sort_by(|a, b| zsdesk_core::model::view::natural_cmp(&a.title, &b.title));
    result
}

fn is_this_pc(parsing_name: &str) -> bool {
    let name = parsing_name.get(..6).filter(|prefix| prefix.eq_ignore_ascii_case("shell:"))
        .map_or(parsing_name, |_| &parsing_name[6..]);
    ["::{20D04FE0-3AEA-1069-A2D8-08002B30309D}", "::{0AC0837C-BBF8-452A-850D-79D08E667CA7}", "MyComputerFolder"]
        .iter().any(|candidate| name.eq_ignore_ascii_case(candidate))
}

fn parsing_title(parsing_name: &str) -> Option<String> {
    parsing_name
        .rsplit(['\\', '/'])
        .next()
        .filter(|value| !value.is_empty())
        .filter(|value| !value.starts_with("::{") && !value.to_ascii_lowercase().starts_with("shell:"))
        .map(str::to_owned)
}

pub(crate) unsafe fn take_pwstr(value: PWSTR) -> String {
    if value.0.is_null() {
        return String::new();
    }

    let mut len = 0usize;
    unsafe {
        while *value.0.add(len) != 0 {
            len += 1;
        }
    }
    let text = unsafe { String::from_utf16_lossy(std::slice::from_raw_parts(value.0, len)) };
    unsafe { CoTaskMemFree(Some(value.0.cast())) };
    text
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
#[path="shell_item_contract_tests.rs"]
mod contract_tests;

#[cfg(test)]
mod tests {
    use super::{is_this_pc, normalized_shell_parsing_name, parsing_title};

    #[test]
    fn namespace_identity_does_not_require_a_reparseable_display_name()->windows::core::Result<()>{
        use zsdesk_core::model::explorer::ShellLocation;
        use windows::Win32::{System::Ole::{OleInitialize,OleUninitialize},UI::Shell::{SHGetKnownFolderItem,FOLDERID_RecycleBinFolder,KF_FLAG_DEFAULT,IShellItem}};
        unsafe{OleInitialize(None)}?;
        struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}let _apartment=Apartment;
        let item:IShellItem=unsafe{SHGetKnownFolderItem(&FOLDERID_RecycleBinFolder,KF_FLAG_DEFAULT,None)}?;
        let mut location=unsafe{super::location_from_shell_item(&item)}.unwrap();
        let ShellLocation::Shell{parsing_name,pidl,..}=&mut location else{panic!("namespace was converted to a filesystem path");};
        assert!(pidl.as_ref().is_some_and(|pidl|super::super::pidl::valid(pidl)));
        *parsing_name="portable provider: non-reparseable display name".into();
        let persisted=serde_json::to_string(&location).unwrap();let restored=serde_json::from_str(&persisted).unwrap();
        let opened=unsafe{super::shell_item(&restored)}?;
        assert_eq!(super::super::pidl::item_pidl(&opened),super::super::pidl::item_pidl(&item));
        let legacy:ShellLocation=serde_json::from_str(r#"{"Shell":{"parsing_name":"shell:RecycleBinFolder","title":"Recycle Bin"}}"#).unwrap();
        unsafe{super::shell_item(&legacy)}?;
        Ok(())
    }

    #[test]
    fn this_pc_hides_known_folders_but_keeps_drives() {
        use windows::Win32::UI::Shell::{IEnumShellItems, IShellItem, BHID_EnumItems};
        unsafe { windows::Win32::System::Ole::OleInitialize(None) }.unwrap();
        struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{windows::Win32::System::Ole::OleUninitialize();}}}let _apartment=Apartment;
        let this_pc = unsafe { super::shell_item(&zsdesk_core::model::explorer::ShellLocation::ThisPc) }.unwrap();
        assert!(super::is_this_pc_item(&this_pc));
        let children: IEnumShellItems = unsafe { this_pc.BindToHandler(None, &BHID_EnumItems) }.unwrap();
        let (mut hidden, mut drives) = (Vec::new(), Vec::new());
        loop {
            let mut item = [None::<IShellItem>]; let mut fetched = 0;
            if unsafe { children.Next(&mut item, Some(&mut fetched)) }.is_err() || fetched == 0 { break; }
            let item = item[0].take().unwrap();
            let path = unsafe { item.GetDisplayName(super::SIGDN_FILESYSPATH) }.ok().map(|name| unsafe { super::take_pwstr(name) });
            if super::hidden_in_this_pc(&item) { hidden.push(path.clone()); }
            if path.as_deref().is_some_and(|path| path.len() == 3 && path.ends_with(":\\")) {
                assert!(!super::hidden_in_this_pc(&item), "drive {path:?} hidden"); drives.push(path);
            }
        }
        assert!(!drives.is_empty(), "This PC listed no drives");
        // Current Windows builds may omit these shortcuts from This PC already.
        // Exercise classification with the actual known folder independently.
        let downloads:IShellItem=unsafe{windows::Win32::UI::Shell::SHGetKnownFolderItem(
            &windows::Win32::UI::Shell::FOLDERID_Downloads,windows::Win32::UI::Shell::KF_FLAG_DEFAULT,None)}.unwrap();
        assert!(super::is_this_pc_known_folder(&downloads),"Downloads not recognised");
        // Quick access lists the same folders; they must stay visible there.
        let quick = unsafe { super::shell_item(&zsdesk_core::model::explorer::ShellLocation::Shell{
            pidl:None, parsing_name:"shell:::{679F85CB-0220-4080-B29B-5540CC05AAB6}".into(),title:String::new()}) }.unwrap();
        let children: IEnumShellItems = unsafe { quick.BindToHandler(None, &BHID_EnumItems) }.unwrap();
        let mut quick_items = 0;
        loop {
            let mut item = [None::<IShellItem>]; let mut fetched = 0;
            if unsafe { children.Next(&mut item, Some(&mut fetched)) }.is_err() || fetched == 0 { break; }
            let item = item[0].take().unwrap(); quick_items += 1;
            assert!(!super::hidden_in_this_pc(&item), "a Quick access entry was hidden");
        }
        assert!(quick_items > 0, "Quick access listed nothing on this machine");
        let windows = unsafe { super::shell_item(&zsdesk_core::model::explorer::ShellLocation::Path(std::env::var_os("WINDIR").unwrap().into())) }.unwrap();
        assert!(!super::is_this_pc_known_folder(&windows), "an ordinary folder was treated as a This PC shortcut");
    }

    #[test]
    fn this_pc_detection_does_not_capture_nested_virtual_folders() {
        assert!(is_this_pc("::{20D04FE0-3AEA-1069-A2D8-08002B30309D}"));
        assert!(is_this_pc("shell:::{20d04fe0-3aea-1069-a2d8-08002b30309d}"));
        assert!(!is_this_pc("::{20D04FE0-3AEA-1069-A2D8-08002B30309D}\\::{11111111-1111-1111-1111-111111111111}"));
        assert_eq!(parsing_title("::{645FF040-5081-101B-9F08-00AA002F954E}"), None);
        assert!(super::is_recycle_bin_name("shell:RecycleBinFolder"));
        assert!(super::is_recycle_bin_name("::{645FF040-5081-101B-9F08-00AA002F954E}"));
        assert!(!super::is_recycle_bin_name("::{645FF040-5081-101B-9F08-00AA002F954E}\\deleted-file"));
        assert!(!super::is_recycle_bin_name(r"C:\RecycleBinFolder"));
    }

    #[test]
    fn quick_access_callback_name_is_resolvable_after_roundtrip() {
        for value in [
            "::{679F85CB-0220-4080-B29B-5540CC05AAB6}",
            "::{679f85cb-0220-4080-b29b-5540cc05aab6}",
        ] {
            let normalized = normalized_shell_parsing_name(value);
            assert_eq!(normalized, format!("shell:{value}"));
            assert_eq!(normalized_shell_parsing_name(&normalized), normalized);
        }
    }

    #[test]
    fn other_shell_names_are_not_rewritten() {
        for value in [
            "shell:::{679F85CB-0220-4080-B29B-5540CC05AAB6}",
            "::{F874310E-B6B7-47DC-BC84-B9E6B38F5903}",
            "::{20D04FE0-3AEA-1069-A2D8-08002B30309D}",
            "shell:Downloads",
        ] {
            assert_eq!(normalized_shell_parsing_name(value), value);
        }
    }
}
