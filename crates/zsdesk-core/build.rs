use std::{collections::BTreeMap,fs,path::PathBuf};
fn main(){
    println!("cargo:rerun-if-changed=resources/strings.en.json");
    let source=fs::read_to_string("resources/strings.en.json").expect("English text catalog");
    let entries:BTreeMap<String,String>=serde_json::from_str(&source).expect("valid English text catalog");
    let mut output=String::from("fn english(value:&str)->Option<&'static str>{match value{\n");
    for(zh,en)in &entries{assert!(!zh.is_empty()&&!en.trim().is_empty(),"empty translation");output.push_str(&format!("{zh:?}=>Some({en:?}),\n"));}
    output.push_str("_=>None,}}\n#[macro_export]\nmacro_rules! tr_format{\n");
    for(zh,en)in &entries{
        output.push_str(&format!("({zh:?})=>{{{{if $crate::i18n::is_english(){{format!({en:?})}}else{{format!({zh:?})}}}}}};\n"));
        output.push_str(&format!("({zh:?}, $($args:tt)*)=>{{{{if $crate::i18n::is_english(){{format!({en:?},$($args)*)}}else{{format!({zh:?},$($args)*)}}}}}};\n"));
    }
    output.push_str("($($unknown:tt)*)=>{compile_error!(\"Missing localized format catalog entry\")};\n}\n");
    fs::write(PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("translations.rs"),output).expect("generated translations");
}
