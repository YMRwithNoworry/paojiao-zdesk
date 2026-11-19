//! Registration-only discovery and activation of explicitly enabled Shell handlers.
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};
use windows::{
    Win32::{
        Foundation::ERROR_SUCCESS,
        System::{
            Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, IDataObject},
            Registry::*,
            SystemServices::{SFGAO_FILESYSTEM, SFGAO_FOLDER, SFGAO_STREAM},
        },
        UI::Shell::*,
    },
    core::{GUID, Interface, PCWSTR, PWSTR, Result},
};
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
pub(super) fn has_static_verb(extension: &str, verb: &str) -> bool {
    if extension.is_empty() {
        return false;
    }
    let extension = wide(extension);
    let verb = wide(verb);
    let mut length = 0;
    unsafe {
        AssocQueryStringW(
            ASSOCF_NONE,
            ASSOCSTR_COMMAND,
            PCWSTR(extension.as_ptr()),
            PCWSTR(verb.as_ptr()),
            None,
            &mut length,
        )
    }
    .is_ok()
        && length > 1
}
pub(super) fn has_image_rotation(extension: &str) -> bool {
    if extension.is_empty() {
        return false;
    }
    let key = format!(
        "SystemFileAssociations\\{extension}\\shellex\\ContextMenuHandlers\\ShellImagePreview"
    );
    default_value(&key)
        .is_some_and(|id| id.eq_ignore_ascii_case("{FFE2A43C-56B9-4bf5-9A79-CC6D4285608A}"))
}
pub(super) fn opening_application_icon(extension: &str) -> Option<String> {
    if extension.is_empty() {
        return None;
    }
    let ext = wide(extension);
    let mut buffer = [0u16; 2048];
    let mut count = buffer.len() as u32;
    let result = unsafe {
        AssocQueryStringW(
            ASSOCF_NONE,
            ASSOCSTR_EXECUTABLE,
            PCWSTR(ext.as_ptr()),
            None,
            Some(PWSTR(buffer.as_mut_ptr())),
            &mut count,
        )
    };
    if result != windows::Win32::Foundation::S_OK {
        return None;
    }
    let path = String::from_utf16(&buffer[..buffer.iter().position(|&unit| unit == 0)?]).ok()?;
    // The association can point at the selected file (%1) or a network
    // application. Opening the menu must not read either as a resource.
    let bytes = path.as_bytes();
    if bytes.len() < 3
        || !bytes[0].is_ascii_alphabetic()
        || &bytes[1..3] != b":\\"
        || path.contains('%')
    {
        return None;
    }
    let root = wide(&path[..3]);
    const FIXED_DRIVE: u32 = 3;
    (unsafe { windows::Win32::Storage::FileSystem::GetDriveTypeW(PCWSTR(root.as_ptr())) }
        == FIXED_DRIVE)
        .then_some(path)
}
struct Key(HKEY);
impl Drop for Key {
    fn drop(&mut self) {
        let _ = unsafe { RegCloseKey(self.0) };
    }
}
fn open(path: &str) -> Option<Key> {
    let mut key = HKEY::default();
    if unsafe {
        RegOpenKeyExW(
            HKEY_CLASSES_ROOT,
            PCWSTR(wide(path).as_ptr()),
            None,
            KEY_READ,
            &mut key,
        )
    } == ERROR_SUCCESS
    {
        Some(Key(key))
    } else {
        None
    }
}
fn names(key: HKEY) -> Vec<String> {
    let mut result = Vec::new();
    let mut text = [0u16; 512];
    for index in 0..32768 {
        let mut n = text.len() as u32;
        if unsafe {
            RegEnumKeyExW(
                key,
                index,
                Some(PWSTR(text.as_mut_ptr())),
                &mut n,
                None,
                None,
                None,
                None,
            )
        } != ERROR_SUCCESS
        {
            break;
        }
        result.push(String::from_utf16_lossy(&text[..n as usize]));
    }
    result
}
fn default_value(path: &str) -> Option<String> {
    let mut text = [0u16; 512];
    let mut bytes = (text.len() * 2) as u32;
    if unsafe {
        RegGetValueW(
            HKEY_CLASSES_ROOT,
            PCWSTR(wide(path).as_ptr()),
            PCWSTR::null(),
            RRF_RT_REG_SZ,
            None,
            Some(text.as_mut_ptr().cast()),
            Some(&mut bytes),
        )
    } != ERROR_SUCCESS
    {
        return None;
    }
    Some(String::from_utf16_lossy(
        &text[..text.iter().position(|v| *v == 0)?],
    ))
}
// Only these inbox registrations are consulted by the built-in command model.
pub(super) fn system_handler(name: &str) -> Option<GUID> {
    let key = format!(
        "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Explorer\\CommandStore\\shell\\Windows.{name}"
    );
    let mut text = [0u16; 80];
    let mut bytes = 160u32;
    if unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(wide(&key).as_ptr()),
            windows::core::w!("ExplorerCommandHandler"),
            RRF_RT_REG_SZ,
            None,
            Some(text.as_mut_ptr().cast()),
            Some(&mut bytes),
        )
    } != ERROR_SUCCESS
    {
        return None;
    }
    GUID::try_from(
        String::from_utf16_lossy(&text[..text.iter().position(|c| *c == 0)?])
            .trim_matches(['{', '}']),
    )
    .ok()
}
pub(super) fn system_icon(name: &str) -> Option<String> {
    let key = format!(
        "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Explorer\\CommandStore\\shell\\Windows.{name}"
    );
    let mut text = [0u16; 1024];
    let mut bytes = 2048u32;
    if unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(wide(&key).as_ptr()),
            windows::core::w!("Icon"),
            RRF_RT_REG_SZ,
            None,
            Some(text.as_mut_ptr().cast()),
            Some(&mut bytes),
        )
    } != ERROR_SUCCESS
    {
        return None;
    }
    Some(String::from_utf16_lossy(
        &text[..text.iter().position(|c| *c == 0)?],
    ))
}
fn handlers(scopes: impl IntoIterator<Item = String>) -> BTreeMap<String, (String, String)> {
    let mut result = BTreeMap::new();
    for scope in scopes {
        let path = format!("{scope}\\shellex\\ContextMenuHandlers");
        let Some(key) = open(&path) else {
            continue;
        };
        for label in names(key.0) {
            let parse = |s: &str| GUID::try_from(s.trim().trim_matches(['{', '}'])).ok();
            let Some(id) = default_value(&format!("{path}\\{label}"))
                .as_deref()
                .and_then(parse)
                .or_else(|| parse(&label))
            else {
                continue;
            };
            let name = if parse(&label).is_some() {
                default_value(&format!("CLSID\\{{{id:?}}}")).unwrap_or(label)
            } else {
                label
            };
            result
                .entry(format!("legacy:{id:?}"))
                .or_insert_with(|| (name, scope.clone()));
        }
    }
    result
}
fn folder_scopes() -> [String; 7] {
    [
        "*",
        "AllFilesystemObjects",
        "Folder",
        "Directory",
        "Directory\\Background",
        "Drive",
        "DesktopBackground",
    ]
    .map(String::from)
}
pub(super) fn inventory() -> Vec<(String, String)> {
    // Read registry registrations only, including handlers associated with an individual ProgID.
    let mut scopes = names(HKEY_CLASSES_ROOT);
    scopes.extend(folder_scopes());
    if let Some(key) = open("SystemFileAssociations") {
        scopes.extend(
            names(key.0)
                .into_iter()
                .map(|s| format!("SystemFileAssociations\\{s}")),
        );
    }
    handlers(scopes)
        .into_iter()
        .map(|(id, (label, _))| (id, format!("{label} (Shell)")))
        .collect()
}
pub(super) fn selected(
    view: &IShellView,
    background: bool,
    allowed: &[String],
) -> Result<Vec<(String, IContextMenu)>> {
    let choices: BTreeSet<&str> = allowed
        .iter()
        .filter(|v| v.starts_with("legacy:"))
        .map(String::as_str)
        .collect();
    if choices.is_empty() {
        return Ok(Vec::new());
    }
    let folder: IFolderView2 = view.cast()?;
    let mut registrations = BTreeMap::new();
    let data: Option<IDataObject> = if background {
        if super::file_transfer::filesystem_folder_path(view)?.is_none() {
            return Ok(Vec::new());
        }
        let mut scopes = vec!["Directory\\Background".to_owned()];
        if super::file_transfer::paste_folder(view)?.is_none() {
            scopes.push("DesktopBackground".into());
        }
        registrations = handlers(scopes);
        None
    } else {
        let items = unsafe { folder.GetSelection(false) }?;
        let count = unsafe { items.GetCount() }?;
        if count == 0 {
            return Ok(Vec::new());
        }
        let mut seen = BTreeSet::new();
        for i in 0..count {
            let item = unsafe { items.GetItemAt(i) }?;
            let attr =
                unsafe { item.GetAttributes(SFGAO_FILESYSTEM | SFGAO_FOLDER | SFGAO_STREAM) }?.0;
            if attr & SFGAO_FILESYSTEM.0 == 0 {
                return Ok(Vec::new());
            }
            let mut current = BTreeSet::from(["AllFilesystemObjects".to_owned()]);
            if attr & SFGAO_FOLDER.0 != 0 && attr & SFGAO_STREAM.0 == 0 {
                current.insert("Folder".to_owned());
                let drive = unsafe { item.GetDisplayName(SIGDN_FILESYSPATH) }
                    .is_ok_and(|p| Path::new(&unsafe { super::take(p) }).parent().is_none());
                current.insert(if drive { "Drive" } else { "Directory" }.into());
            } else {
                current.insert("*".into());
                if let Ok(p) = unsafe { item.GetDisplayName(SIGDN_FILESYSPATH) } {
                    let name = unsafe { super::take(p) };
                    if let Some(ext) = Path::new(&name).extension().and_then(|e| e.to_str()) {
                        let key = format!(".{}", ext.to_lowercase());
                        let input = wide(&key);
                        let input = PCWSTR(input.as_ptr());
                        let mut progid = [0u16; 512];
                        let mut size = progid.len() as u32;
                        if unsafe {
                            AssocQueryStringW(
                                ASSOCF_NONE,
                                ASSOCSTR_PROGID,
                                input,
                                None,
                                Some(PWSTR(progid.as_mut_ptr())),
                                &mut size,
                            )
                        } == windows::Win32::Foundation::S_OK
                            && size > 1
                            && size as usize <= progid.len()
                        {
                            current.insert(String::from_utf16_lossy(&progid[..size as usize - 1]));
                        } else if let Some(id) = default_value(&key).filter(|id| !id.is_empty()) {
                            current.insert(id);
                        } else {
                            current.insert("Unknown".into());
                        }
                        let mut kind = Default::default();
                        let mut flags = 0;
                        let mut name = PWSTR::null();
                        if unsafe {
                            AssocGetPerceivedType(input, &mut kind, &mut flags, Some(&mut name))
                        }
                        .is_ok()
                            && !name.is_null()
                        {
                            let name = unsafe { super::take(name) };
                            if !name.is_empty() {
                                current.insert(format!("SystemFileAssociations\\{name}"));
                                current.insert(name);
                            }
                        }
                        current.insert(format!("SystemFileAssociations\\{key}"));
                        current.insert(key);
                    }
                }
            }
            if seen.insert(current.clone()) {
                let supported = handlers(current);
                if i == 0 {
                    registrations = supported;
                } else {
                    registrations.retain(|id, _| supported.contains_key(id));
                }
                if !registrations.keys().any(|id| choices.contains(id.as_str())) {
                    return Ok(Vec::new());
                }
            }
        }
        Some(unsafe { view.GetItemObject(SVGIO_SELECTION) }?)
    };
    let root = if background {
        let persistent: IPersistFolder2 = unsafe { folder.GetFolder() }?;
        Some(unsafe { persistent.GetCurFolder() }?)
    } else {
        None
    };
    let mut enabled = Vec::new();
    for (key, (name, scope)) in registrations {
        if !choices.contains(key.as_str()) {
            continue;
        }
        let Ok(id) = GUID::try_from(&key[7..]) else {
            continue;
        };
        let Ok(init) =
            (unsafe { CoCreateInstance::<_, IShellExtInit>(&id, None, CLSCTX_INPROC_SERVER) })
        else {
            continue;
        };
        let reg = open(&scope);
        if unsafe {
            init.Initialize(
                root.map(|p| p as *const _),
                data.as_ref(),
                reg.as_ref().map(|k| k.0),
            )
        }
        .is_ok()
        {
            if let Ok(context) = init.cast::<IContextMenu>() {
                enabled.push((name, context));
            }
        }
    }
    if let Some(p) = root {
        unsafe {
            windows::Win32::System::Com::CoTaskMemFree(Some(p.cast()));
        }
    }
    Ok(enabled)
}
