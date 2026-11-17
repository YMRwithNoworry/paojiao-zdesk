//! Application-local context menu preferences shared by Explorer and fences.
use std::path::Path;
use zsdesk_core::model::explorer::{ContextMenuMode,ContextMenuPreferences};
use zsdesk_windows::shell::menu::{ContextMenuOptions,set_context_menu_options};
static SHARED:std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<std::path::PathBuf,ContextMenuPreferences>>>=std::sync::OnceLock::new();
pub fn register(profile:&Path,options:&ContextMenuPreferences){SHARED.get_or_init(Default::default).lock().unwrap_or_else(|e|e.into_inner()).entry(profile.into()).or_insert_with(||options.clone());}
pub fn publish(profile:&Path,options:&ContextMenuPreferences){SHARED.get_or_init(Default::default).lock().unwrap_or_else(|e|e.into_inner()).insert(profile.into(),options.clone());}
pub fn current(profile:&Path)->Option<ContextMenuPreferences>{SHARED.get()?.lock().unwrap_or_else(|e|e.into_inner()).get(profile).cloned()}
pub fn native(options:&ContextMenuPreferences)->ContextMenuOptions {
    let mut hidden=Vec::new();
    for (show,verbs) in [
        (options.show_cut_copy_paste,&["cut","copy","paste","pastelink"][..]),
        (options.show_copy_path,&["copyaspath","copypath"][..]),
        (options.show_rename,&["rename"][..]),(options.show_delete,&["delete"][..]),
        (options.show_properties,&["properties"][..]),(options.show_share,&["share","windows.modernshare"][..]),
        (options.show_new,&["new"][..]),
    ] {if !show {hidden.extend(verbs.iter().map(|v|(*v).to_string()));}}
    hidden.extend(options.hidden_commands.iter().cloned());
    ContextMenuOptions{modern:modern(options),drawn:options.mode==ContextMenuMode::DrawnWindows11,hidden_verbs:hidden,shown_verbs:options.shown_commands.clone(),show_seven_zip:options.show_seven_zip,show_wps:options.show_wps}
}
/// Visual style is independent of registered commands and customization rules.
pub fn modern(options:&ContextMenuPreferences)->bool{
    matches!(options.mode,ContextMenuMode::Windows11|ContextMenuMode::DrawnWindows11)
}
pub fn apply_from_profile(profile:&Path){if let Ok(session)=crate::storage::load(profile){set_context_menu_options(native(&session.ui.context_menu));}}
