//! Fonts is a merged Shell folder: system-wide and per-user installations.
use std::path::{Path,PathBuf};
use zsdesk_core::model::explorer::ShellLocation;

fn roots()->Option<(PathBuf,PathBuf)>{
    let system=std::env::var_os("WINDIR").or_else(||std::env::var_os("SystemRoot")).map(PathBuf::from)?.join("Fonts");
    let user=dirs::data_local_dir()?.join("Microsoft").join("Windows").join("Fonts");
    (system.is_absolute()&&user.is_absolute()).then_some((system,user))
}
fn same_path(a:&Path,b:&Path)->bool{a.to_string_lossy().trim_end_matches(['\\','/']).replace('/',"\\").eq_ignore_ascii_case(&b.to_string_lossy().trim_end_matches(['\\','/']).replace('/',"\\"))}
fn is_namespace(location:&ShellLocation)->bool{
    matches!(location,ShellLocation::Shell{parsing_name,..} if parsing_name.eq_ignore_ascii_case("shell:Fonts")||parsing_name.to_ascii_lowercase().contains("{bd84b380-8ca2-1069-ab1d-08000948f534}"))
}
pub(in crate::window) fn folder_scope(location:&ShellLocation)->Option<PathBuf>{
    if is_namespace(location){return roots().map(|(system,_)|system);}
    location.as_path().map(PathBuf::from)
}
fn scoped_expression(scope:&Path,system:&Path,user:&Path)->Option<String>{
    if !same_path(scope,system){return None;}
    let quote=|path:&Path|format!("\"{}\\\"",path.to_string_lossy().replace('/',"\\").trim_end_matches('\\'));
    Some(format!("<{} | {}> ext:ttf;otf;ttc;otc;fon;fnt",quote(system),quote(user)))
}
pub(super) fn expression(scope:&Path)->Option<String>{let(system,user)=roots()?;scoped_expression(scope,&system,&user)}

#[cfg(test)]mod tests{
    use super::*;
    #[test]fn fonts_scope_merges_locations_without_leaking_siblings_or_user_or_terms(){
        let system=Path::new(r"C:\Windows\Fonts");let user=Path::new(r"D:\用户 甲\AppData\Local\Microsoft\Windows\Fonts");
        let expression=scoped_expression(Path::new(r"c:\WINDOWS\Fonts\"),system,user).unwrap();
        let query=crate::fast_search::query::Query::parse(&format!("{expression} <alpha | beta>"),false).unwrap();
        assert!(query.matches("alpha.ttf",false,r"c:\windows\fonts\alpha.ttf"));
        assert!(query.matches("beta.otf",false,r"d:\用户 甲\appdata\local\microsoft\windows\fonts\beta.otf"));
        assert!(!query.matches("beta.ttf",false,r"c:\windows\fonts-backup\beta.ttf"));
        assert!(!query.matches("beta.docx",false,r"c:\windows\fonts\beta.docx"));
        assert!(scoped_expression(user,system,user).is_none(),"explicit user folder must not expand to system fonts");
        assert!(scoped_expression(Path::new(r"C:\Windows"),system,user).is_none());
    }
    #[test]fn fonts_namespace_is_recognized_without_guessing_by_translated_title(){
        let shell=|name:&str|ShellLocation::Shell{parsing_name:name.into(),title:"Fonts".into(),pidl:None};
        assert!(is_namespace(&shell("shell:Fonts")));assert!(is_namespace(&shell("::{BD84B380-8CA2-1069-AB1D-08000948F534}")));
        assert!(!is_namespace(&shell("shell:Downloads")));
    }
}
