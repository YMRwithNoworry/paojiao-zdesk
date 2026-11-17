//! Search folders retain their parameters rather than an ephemeral Shell address.
use std::path::Path;
use windows::{core::{PCWSTR,Result},Win32::{
    Foundation::PROPERTYKEY,
    System::{Com::{CoCreateInstance,CLSCTX_INPROC_SERVER},Search::{ConditionFactory,IConditionFactory2,ICondition,CONDITION_CREATION_DEFAULT,Common::COP_VALUE_CONTAINS}},
    UI::Shell::{ISearchFolderItemFactory,SearchFolderItemFactory,IShellItem,IShellItemArray,SHCreateShellItemArrayFromIDLists},
}};
use zsdesk_core::model::explorer::ShellLocation;

pub fn search_location(scope:&ShellLocation,query:&str)->Result<ShellLocation>{
    let folder=match scope{ShellLocation::Path(p)=>Some(p.clone()),ShellLocation::Search{scope,..}=>scope.clone(),_=>None};
    if query.trim().is_empty() || query.contains('\0'){return Err(windows::core::Error::from_hresult(windows::core::HRESULT(0x80070057u32 as i32)));}
    Ok(ShellLocation::Search{query:query.trim().into(),scope:folder})
}

pub(super) fn search_item(scope:Option<&Path>,query:&str)->Result<IShellItem>{
    let query=query.trim();
    if query.is_empty() || query.contains('\0'){return Err(windows::core::Error::from_hresult(windows::core::HRESULT(0x80070057u32 as i32)));}
    // Global search is backed by FastSearch and the native ResultsFolder.
    // A persisted query must never silently fall back to an index-only scope.
    let folder=scope.ok_or_else(||windows::core::Error::from_hresult(windows::core::HRESULT(0x80004001u32 as i32)))?;
    let paths=vec![folder.to_path_buf()];
    let pidls:Vec<Vec<u8>>=paths.into_iter().filter_map(|p|super::pidl::location_pidl(&ShellLocation::Path(p))).collect();
    if pidls.is_empty(){return Err(windows::core::Error::from_hresult(windows::core::HRESULT(0x80070003u32 as i32)));}
    let pointers:Vec<_>=pidls.iter().map(|p|p.as_ptr().cast()).collect();
    let title:Vec<u16>=zsdesk_core::tr_format!("搜索：{query}", query=query).encode_utf16().chain(Some(0)).collect();
    let query:Vec<u16>=query.encode_utf16().chain(Some(0)).collect();
    let key=PROPERTYKEY{fmtid:windows::core::GUID::from_u128(0x41cf5ae0_f75a_4806_bd87_59c7d9248eb9),pid:100};
    unsafe {
        let array:IShellItemArray=SHCreateShellItemArrayFromIDLists(&pointers)?;
        let conditions:IConditionFactory2=CoCreateInstance(&ConditionFactory,None,CLSCTX_INPROC_SERVER)?;
        let condition:ICondition=conditions.CreateStringLeaf(&key,COP_VALUE_CONTAINS,PCWSTR(query.as_ptr()),None,CONDITION_CREATION_DEFAULT)?;
        let factory:ISearchFolderItemFactory=CoCreateInstance(&SearchFolderItemFactory,None,CLSCTX_INPROC_SERVER)?;
        factory.SetScope(&array)?;factory.SetCondition(&condition)?;
        factory.SetDisplayName(PCWSTR(title.as_ptr()))?;
        factory.GetShellItem()
    }
}

#[cfg(test)]mod tests{
    use super::*;
    #[test]fn search_state_keeps_query_and_scope_without_shell_serialization(){
        let local=search_location(&ShellLocation::Path(r"E:\资料".into()),"  图纸  ").unwrap();
        assert_eq!(local,ShellLocation::Search{query:"图纸".into(),scope:Some(r"E:\资料".into())});
        assert_eq!(search_location(&ShellLocation::ThisPc,"report").unwrap(),ShellLocation::Search{query:"report".into(),scope:None});
        assert!(search_location(&ShellLocation::ThisPc," ").is_err());
        let json=serde_json::to_string(&local).unwrap();assert_eq!(serde_json::from_str::<ShellLocation>(&json).unwrap(),local);
    }
    #[test]fn global_search_requires_the_streamed_results_host(){
        assert_eq!(search_item(None,"report").unwrap_err().code(),windows::core::HRESULT(0x80004001u32 as i32));
    }
}
