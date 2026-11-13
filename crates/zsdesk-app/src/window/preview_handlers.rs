//! Discovers installed preview extensions without changing file associations.
use std::{collections::HashSet, path::Path};
use windows::{core::{GUID, PCWSTR, PWSTR}, Win32::{
    System::{Com::{CLSIDFromString, CLSCTX, CLSCTX_ACTIVATE_32_BIT_SERVER,
        CLSCTX_ACTIVATE_64_BIT_SERVER, CLSCTX_LOCAL_SERVER}, Registry::{RegGetValueW,
        HKEY, HKEY_CLASSES_ROOT, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, REG_ROUTINE_FLAGS,
        RRF_RT_REG_SZ, RRF_SUBKEY_WOW6432KEY, RRF_SUBKEY_WOW6464KEY}},
    UI::Shell::{AssocQueryStringW, ASSOCF_NOTRUNCATE, ASSOCSTR_SHELLEXTENSION},
}};
use zsdesk_core::model::explorer::PreviewProviderPreference;

const PREVIEW_IID: &str = "{8895b1c6-b41f-4c1c-a562-0d564250836f}";

#[derive(Clone)]
pub(super) struct Candidate {
    pub clsid: GUID,
    pub context: CLSCTX,
    pub label: String,
}

pub(super) struct Discovery { pub candidates: Vec<Candidate>, pub unavailable: String }

fn wide(value: &str) -> Vec<u16> { value.encode_utf16().chain(Some(0)).collect() }

fn registry_text(root: HKEY, path: &str, name: Option<&str>, view: REG_ROUTINE_FLAGS) -> Option<String> {
    let path = wide(path);
    let name = name.map(wide);
    let name = name.as_ref().map_or(PCWSTR::null(), |value| PCWSTR(value.as_ptr()));
    let mut size = 0;
    let flags = RRF_RT_REG_SZ | view;
    let _ = unsafe { RegGetValueW(root, PCWSTR(path.as_ptr()), name, flags, None, None, Some(&mut size)) };
    if !(2..=32_768).contains(&size) { return None; }
    let mut value = vec![0u16; size as usize / 2 + 1];
    if !unsafe { RegGetValueW(root, PCWSTR(path.as_ptr()), name, flags, None,
        Some(value.as_mut_ptr().cast()), Some(&mut size)) }.is_ok() { return None; }
    let length = value.iter().position(|unit| *unit == 0).unwrap_or(value.len());
    Some(String::from_utf16_lossy(&value[..length]))
}

fn associated(extension: &str) -> Option<String> {
    let extension = wide(extension); let iid = wide(PREVIEW_IID); let mut size = 0;
    let _ = unsafe { AssocQueryStringW(ASSOCF_NOTRUNCATE, ASSOCSTR_SHELLEXTENSION,
        PCWSTR(extension.as_ptr()), PCWSTR(iid.as_ptr()), None, &mut size) };
    if !(2..=128).contains(&size) { return None; }
    let mut value = vec![0u16; size as usize];
    unsafe { AssocQueryStringW(ASSOCF_NOTRUNCATE, ASSOCSTR_SHELLEXTENSION,
        PCWSTR(extension.as_ptr()), PCWSTR(iid.as_ptr()), Some(PWSTR(value.as_mut_ptr())), &mut size).ok().ok()?; }
    let end = value.iter().position(|unit| *unit == 0).unwrap_or(value.len());
    Some(String::from_utf16_lossy(&value[..end]))
}

fn office_progids(extension: &str, wps: bool) -> &'static [&'static str] {
    match (extension, wps) {
        (".doc" | ".docx" | ".docm" | ".dot" | ".dotx" | ".rtf", false) => &["Word.Document.12", "Word.Document.8", "Word.RTF.8"],
        (".xls" | ".xlsx" | ".xlsm" | ".xlsb" | ".xlt" | ".xltx" | ".csv", false) => &["Excel.Sheet.12", "Excel.Sheet.8", "Excel.SheetBinaryMacroEnabled.12"],
        (".ppt" | ".pptx" | ".pptm" | ".pps" | ".ppsx" | ".potx", false) => &["PowerPoint.Show.12", "PowerPoint.Show.8"],
        (".doc" | ".docx" | ".docm" | ".dot" | ".dotx" | ".rtf", true) => &["KWPS.Document.12", "KWPS.Document.8", "WPS.Document.12", "WPS.Document.6"],
        (".xls" | ".xlsx" | ".xlsm" | ".xlsb" | ".xlt" | ".xltx" | ".csv", true) => &["KET.Sheet.12", "KET.Sheet.8", "ET.Xls.6"],
        (".ppt" | ".pptx" | ".pptm" | ".pps" | ".ppsx" | ".potx", true) => &["KWPP.Presentation.12", "KWPP.Presentation.8", "WPP.PPT.6"],
        _ => &[],
    }
}

pub(super) fn discover(path: &Path, preference: PreviewProviderPreference) -> Discovery {
    let extension = path.extension().map(|value| format!(".{}", value.to_string_lossy().to_lowercase())).unwrap_or_default();
    let mut candidates = Vec::new(); let mut seen = HashSet::new(); let mut stale = false; let mut blocked = false;
    let mut sources = Vec::new();
    let add_default = |sources: &mut Vec<(String, &'static str)>| {
        if let Some(id) = associated(&extension) { sources.push((id, "系统预览插件")); }
    };
    if preference == PreviewProviderPreference::System { add_default(&mut sources); }
    for wps in if preference == PreviewProviderPreference::Wps { [true, false] } else { [false, true] } {
        for progid in office_progids(&extension, wps) {
            for view in [RRF_SUBKEY_WOW6464KEY, RRF_SUBKEY_WOW6432KEY] {
                if let Some(id) = registry_text(HKEY_CLASSES_ROOT, &format!("{progid}\\shellex\\{PREVIEW_IID}"), None, view) {
                    sources.push((id, if wps { "WPS 预览插件" } else { "Office 预览插件" }));
                }
            }
        }
    }
    if preference != PreviewProviderPreference::System { add_default(&mut sources); }
    for view in [RRF_SUBKEY_WOW6464KEY, RRF_SUBKEY_WOW6432KEY] {
        let progid = registry_text(HKEY_CLASSES_ROOT, &extension, None, view);
        for key in [Some(extension.clone()), Some(format!("SystemFileAssociations\\{extension}")), progid].into_iter().flatten() {
            if let Some(id) = registry_text(HKEY_CLASSES_ROOT, &format!("{key}\\shellex\\{PREVIEW_IID}"), None, view) {
                sources.push((id, "已安装预览插件"));
            }
        }
    }
    for (id, source) in sources {
        let id = format!("{{{}}}", id.trim().trim_matches(['{', '}']).to_uppercase());
        let raw = wide(&id); let Ok(clsid) = (unsafe { CLSIDFromString(PCWSTR(raw.as_ptr())) }) else { continue };
        let policy = "Software\\Microsoft\\Windows\\CurrentVersion\\Shell Extensions\\Blocked";
        if [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE].into_iter().any(|root|
            [RRF_SUBKEY_WOW6464KEY, RRF_SUBKEY_WOW6432KEY].into_iter().any(|view|
                registry_text(root, policy, Some(&id), view).is_some())) { blocked = true; continue; }
        let mut registered = false;
        for (view, context, suffix) in [(RRF_SUBKEY_WOW6464KEY, CLSCTX_LOCAL_SERVER | CLSCTX_ACTIVATE_64_BIT_SERVER, "64 位"),
            (RRF_SUBKEY_WOW6432KEY, CLSCTX_LOCAL_SERVER | CLSCTX_ACTIVATE_32_BIT_SERVER, "32 位")] {
            let key = format!("CLSID\\{id}");
            let server = registry_text(HKEY_CLASSES_ROOT, &format!("{key}\\InprocServer32"), None, view)
                .or_else(|| registry_text(HKEY_CLASSES_ROOT, &format!("{key}\\LocalServer32"), None, view));
            if server.as_ref().is_none_or(|value| value.trim().is_empty()) { continue; }
            registered = true;
            if seen.insert((id.clone(), context.0)) {
                let label = registry_text(HKEY_CLASSES_ROOT, &key, None, view).filter(|value| !value.trim().is_empty()).unwrap_or_else(|| source.into());
                candidates.push(Candidate { clsid, context, label: format!("{label}（{suffix}）") });
            }
        }
        if !registered {
            stale = true;
            // Packaged COM registrations need not appear in the classic CLSID view.
            if seen.insert((id, CLSCTX_LOCAL_SERVER.0)) {
                candidates.push(Candidate { clsid, context: CLSCTX_LOCAL_SERVER, label: source.into() });
            }
        }
    }
    let unavailable = if blocked && candidates.is_empty() { "该格式的预览插件已被 Windows 或用户设置禁用。" }
        else if stale { "此格式的预览关联未能激活对应组件；请检查 Office/WPS 的预览组件注册。" }
        else if !candidates.is_empty() { "已找到预览组件，但未能显示此文件。" }
        else { "没有找到此格式的已安装预览插件；安装 Office/WPS 的预览组件后可直接使用。" };
    Discovery { candidates, unavailable: unavailable.into() }
}

#[cfg(test)] mod tests {
    use super::*;
    #[test] fn office_families_do_not_cross_document_types() {
        assert!(office_progids(".docx", false).iter().all(|value| value.starts_with("Word.")));
        assert!(office_progids(".xlsx", false).iter().all(|value| value.starts_with("Excel.")));
        assert!(office_progids(".pptx", false).iter().all(|value| value.starts_with("PowerPoint.")));
        assert!(office_progids(".exe", false).is_empty());
    }
}
