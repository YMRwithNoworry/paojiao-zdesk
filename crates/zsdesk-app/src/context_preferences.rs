//! Application-local context menu preferences shared by Explorer and fences.
use std::path::Path;
use zsdesk_core::model::explorer::{ContextMenuMode,ContextMenuPreferences};
use zsdesk_windows::shell::menu::{ContextMenuOptions,set_context_menu_options};
pub fn native(options:&ContextMenuPreferences)->ContextMenuOptions {
    let mut hidden=Vec::new();
    for (show,verbs) in [
        (options.show_cut_copy_paste,&["cut","copy","paste","pastelink"][..]),
        (options.show_copy_path,&["copyaspath","copypath"][..]),
        (options.show_rename,&["rename"][..]),(options.show_delete,&["delete"][..]),
        (options.show_properties,&["properties"][..]),(options.show_share,&["share","windows.modernshare"][..]),
        (options.show_new,&["new"][..]),
    ] {if !show {hidden.extend(verbs.iter().map(|v|(*v).to_string()));}}
    ContextMenuOptions{modern:modern(options),hidden_verbs:hidden,show_seven_zip:options.show_seven_zip,show_wps:options.show_wps}
}
/// The Windows 11 presentation is requested only where its presenter can run;
/// elsewhere every menu goes straight to the classic menu it would fall back to.
pub fn modern(options:&ContextMenuPreferences)->bool{
    options.mode==ContextMenuMode::Windows11&&zsdesk_windows::shell::menu::modern_presenter_supported()!=Some(false)
}
pub fn apply_from_profile(profile:&Path){if let Ok(session)=crate::storage::load(profile){set_context_menu_options(native(&session.ui.context_menu));}}
