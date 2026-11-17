//! Application text language; file names and Shell-owned strings remain untouched.
use std::{cell::Cell,sync::atomic::{AtomicU8,Ordering}};
use serde::{Deserialize,Serialize};

#[derive(Clone,Copy,Debug,Default,PartialEq,Eq,Serialize,Deserialize)]
pub enum LanguagePreference{
    #[default]#[serde(rename="system")]System,
    #[serde(rename="zh-CN")]Chinese,
    #[serde(rename="en")]English,
}
static ENGLISH:AtomicU8=AtomicU8::new(0);
thread_local!{static OVERRIDE:Cell<Option<bool>>=const{Cell::new(None)};}
pub fn set_english(enabled:bool){ENGLISH.store(u8::from(enabled),Ordering::Release);}
pub fn is_english()->bool{OVERRIDE.with(Cell::get).unwrap_or_else(||ENGLISH.load(Ordering::Acquire)!=0)}
pub fn text(value:&str)->&str{if is_english(){english(value).unwrap_or(value)}else{value}}
pub fn wide(value:&str)->Vec<u16>{text(value).encode_utf16().chain(Some(0)).collect()}
/// Thread-scoped language overrides also keep independent unit tests isolated.
pub struct Scope(Option<bool>);
impl Scope{pub fn english(enabled:bool)->Self{Self(OVERRIDE.with(|slot|slot.replace(Some(enabled))))}}
impl Drop for Scope{fn drop(&mut self){OVERRIDE.with(|slot|slot.set(self.0));}}
include!(concat!(env!("OUT_DIR"),"/translations.rs"));

#[cfg(test)]mod tests{
    use super::*;
    #[test]fn a_language_scope_preserves_unknown_user_text(){
        let _scope=Scope::english(true);assert_eq!(text("用户自己的文件名.txt"),"用户自己的文件名.txt");
        assert!(is_english());{let _chinese=Scope::english(false);assert!(!is_english());}assert!(is_english());
    }
    #[test]fn preferences_round_trip_without_changing_legacy_defaults(){
        assert_eq!(LanguagePreference::default(),LanguagePreference::System);
        for value in [LanguagePreference::System,LanguagePreference::Chinese,LanguagePreference::English]{
            assert_eq!(serde_json::from_str::<LanguagePreference>(&serde_json::to_string(&value).unwrap()).unwrap(),value);
        }
    }
    #[test]fn translated_formats_preserve_arguments_and_path_titles(){
        let _scope=Scope::english(true);
        assert_eq!(tr_format!("有 {denied} 个目录或项目无法读取，其余结果已显示",denied=7),"Could not read 7 folders or items; other results are shown");
        assert_eq!(crate::model::explorer::ShellLocation::Path("C:/文件夹/图片".into()).title(),"图片");
        assert_eq!(crate::model::explorer::ShellLocation::ThisPc.title(),"This PC");
    }
}
