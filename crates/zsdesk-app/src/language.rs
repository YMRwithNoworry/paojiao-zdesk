use std::path::Path;
pub use zsdesk_core::i18n::LanguagePreference;
#[link(name="kernel32")]unsafe extern "system"{fn GetUserDefaultUILanguage()->u16;}
fn english(preference:LanguagePreference,system:u16)->bool{match preference{
    LanguagePreference::English=>true,LanguagePreference::Chinese=>false,LanguagePreference::System=>system&0x3ff!=0x04,
}}
pub fn initialize_environment(){
    let enabled=match std::env::var("ZSDESK_UI_LANGUAGE").as_deref(){Ok("en")=>true,Ok("zh-CN")=>false,_=>english(LanguagePreference::System,unsafe{GetUserDefaultUILanguage()})};
    zsdesk_core::i18n::set_english(enabled);
}
pub fn initialize_profile(path:&Path){
    let preference=crate::storage::load(path).map_or(LanguagePreference::System,|session|session.ui.language);
    let enabled=english(preference,unsafe{GetUserDefaultUILanguage()});zsdesk_core::i18n::set_english(enabled);
    // The process has not started its UI/worker threads at this point. Child
    // menu and preview processes inherit only this application's text choice.
    std::env::set_var("ZSDESK_UI_LANGUAGE",if enabled{"en"}else{"zh-CN"});
}
#[cfg(test)]mod tests{
    use super::*;
    #[test]fn non_chinese_systems_default_to_english_and_explicit_preferences_win(){
        for locale in [0x0409,0x0809,0x0407,0x0411]{assert!(english(LanguagePreference::System,locale));}
        for locale in [0x0804,0x0404,0x0c04]{assert!(!english(LanguagePreference::System,locale));}
        assert!(english(LanguagePreference::English,0x0804));assert!(!english(LanguagePreference::Chinese,0x0409));
    }
}
