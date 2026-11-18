use std::path::Path;
pub use zsdesk_core::i18n::LanguagePreference;
#[link(name="kernel32")]unsafe extern "system"{fn GetUserDefaultUILanguage()->u16;}
fn english(preference:LanguagePreference,system:u16)->bool{match preference{
    LanguagePreference::English=>true,LanguagePreference::Chinese=>false,LanguagePreference::System=>system&0x3ff!=0x04,
}}
pub fn choice(id:&str)->Option<LanguagePreference>{match id{
    "language-system"=>Some(LanguagePreference::System),"language-en"=>Some(LanguagePreference::English),
    "language-zh"=>Some(LanguagePreference::Chinese),_=>None
}}
pub fn save_and_apply(profile:&Path,preference:LanguagePreference)->anyhow::Result<()>{
    static CHANGE:std::sync::Mutex<()>=std::sync::Mutex::new(());
    let _change=CHANGE.lock().unwrap_or_else(|e|e.into_inner());
    crate::storage::save_language(profile,preference)?;
    apply(preference);notify_windows();Ok(())
}
pub fn initialize_environment(){
    let enabled=match std::env::var("ZSDESK_UI_LANGUAGE").as_deref(){Ok("en")=>true,Ok("zh-CN")=>false,_=>english(LanguagePreference::System,unsafe{GetUserDefaultUILanguage()})};
    zsdesk_core::i18n::set_english(enabled);
}
pub fn initialize_profile(path:&Path){
    let preference=crate::storage::load(path).map_or(LanguagePreference::System,|session|session.ui.language);
    apply(preference);
}
pub fn apply(preference:LanguagePreference){
    let enabled=english(preference,unsafe{GetUserDefaultUILanguage()});
    zsdesk_core::i18n::set_english(enabled);
    std::env::set_var("ZSDESK_UI_LANGUAGE",if enabled{"en"}else{"zh-CN"});
}

pub const CHANGED:u32=windows::Win32::UI::WindowsAndMessaging::WM_APP+219;
pub fn notify_windows(){
    use windows::core::BOOL;
    use windows::Win32::{Foundation::{HWND,LPARAM,WPARAM},UI::WindowsAndMessaging::*};
    unsafe extern "system" fn each(hwnd:HWND,_:LPARAM)->BOOL{
        let mut pid=0;unsafe{GetWindowThreadProcessId(hwnd,Some(&mut pid));}
        if pid==std::process::id(){
            let mut buf=[0u16;64];let n=unsafe{GetClassNameW(hwnd,&mut buf)}.max(0) as usize;
            if matches!(String::from_utf16_lossy(&buf[..n]).as_str(),"ZSDesk.Native.Explorer"|"ZSDesk.UtilityPanel"|"ZSDesk.Native.DesktopHost"){
                let _=unsafe{PostMessageW(Some(hwnd),CHANGED,WPARAM(0),LPARAM(0))};
            }
        }
        true.into()
    }
    let _=unsafe{EnumWindows(Some(each),LPARAM(0))};
    crate::runtime::refresh_tray();
}
#[cfg(test)]mod tests{
    use super::*;
    #[test]fn non_chinese_systems_default_to_english_and_explicit_preferences_win(){
        for locale in [0x0409,0x0809,0x0407,0x0411]{assert!(english(LanguagePreference::System,locale));}
        for locale in [0x0804,0x0404,0x0c04]{assert!(!english(LanguagePreference::System,locale));}
        assert!(english(LanguagePreference::English,0x0804));assert!(!english(LanguagePreference::Chinese,0x0409));
    }
}

#[cfg(test)]mod live_tests{
    use super::*;
    #[test]#[ignore="Process-wide language setting; run in its own test process"]
    fn active_language_changes_immediately_and_is_inherited_by_workers(){
        for (preference,word,tag) in [(LanguagePreference::English,"Copy","en"),(LanguagePreference::Chinese,"复制","zh-CN")]{
            apply(preference);
            assert_eq!(zsdesk_core::i18n::text("复制"),word);
            assert_eq!(std::thread::spawn(||zsdesk_core::i18n::text("复制")).join().unwrap(),word);
            assert_eq!(std::env::var("ZSDESK_UI_LANGUAGE").unwrap(),tag);
        }
    }
}
