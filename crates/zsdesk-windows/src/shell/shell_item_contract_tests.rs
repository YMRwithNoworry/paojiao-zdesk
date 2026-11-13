//! A non-filesystem COM provider may expose identity without a parsing name.
//! These fixtures exercise that Shell contract; they do not emulate a USB driver.
use super::*;
use std::{cell::Cell,ffi::c_void,rc::Rc};
use windows::{core::{implement,GUID,Ref},Win32::{
    Foundation::{E_NOTIMPL,E_OUTOFMEMORY},
    System::{Com::{CoTaskMemAlloc,IBindCtx,IPersist_Impl},Ole::{OleInitialize,OleUninitialize},
        SystemServices::{SFGAO_FILESYSTEM,SFGAO_FLAGS}},
    UI::Shell::{Common::ITEMIDLIST,IPersistIDList,IPersistIDList_Impl,IShellItem_Impl,
        FOLDERID_RecycleBinFolder,SIGDN,SICHINT_CANONICAL},
}};

struct Apartment;
impl Apartment{fn new()->Result<Self>{unsafe{OleInitialize(None)}?;Ok(Self)}}
impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}

#[implement(IShellItem,IPersistIDList)]
struct IdentityOnlyItem{
    provider_pidl:Option<Vec<u8>>,
    backing:IShellItem,
    filesystem_name_calls:Rc<Cell<usize>>,
}
#[allow(non_snake_case)]
impl IShellItem_Impl for IdentityOnlyItem_Impl{
    fn BindToHandler(&self,_:Ref<IBindCtx>,_:*const GUID,_:*const GUID,result:*mut*mut c_void)->Result<()>{
        if !result.is_null(){unsafe{*result=std::ptr::null_mut();}}
        Err(Error::from_hresult(E_NOTIMPL))
    }
    fn GetParent(&self)->Result<IShellItem>{unsafe{self.backing.GetParent()}}
    fn GetDisplayName(&self,kind:SIGDN)->Result<PWSTR>{
        if kind==SIGDN_FILESYSPATH{
            self.filesystem_name_calls.set(self.filesystem_name_calls.get()+1);
        }
        if kind!=SIGDN_NORMALDISPLAY{return Err(Error::from_hresult(E_NOTIMPL));}
        let value=wide("设备存储 中文");
        let pointer=unsafe{CoTaskMemAlloc(value.len()*2)}.cast::<u16>();
        if pointer.is_null(){return Err(Error::from_hresult(E_OUTOFMEMORY));}
        unsafe{std::ptr::copy_nonoverlapping(value.as_ptr(),pointer,value.len());}
        Ok(PWSTR(pointer))
    }
    fn GetAttributes(&self,mask:SFGAO_FLAGS)->Result<SFGAO_FLAGS>{Ok(SFGAO_FLAGS(mask.0&SFGAO_FOLDER.0))}
    fn Compare(&self,_:Ref<IShellItem>,_:u32)->Result<i32>{Err(Error::from_hresult(E_NOTIMPL))}
}
#[allow(non_snake_case)]
impl IPersist_Impl for IdentityOnlyItem_Impl{
    fn GetClassID(&self)->Result<GUID>{Ok(GUID::zeroed())}
}
#[allow(non_snake_case)]
impl IPersistIDList_Impl for IdentityOnlyItem_Impl{
    fn SetIDList(&self,_:*const ITEMIDLIST)->Result<()>{Err(Error::from_hresult(E_NOTIMPL))}
    fn GetIDList(&self)->Result<*mut ITEMIDLIST>{
        let bytes=self.provider_pidl.as_ref().ok_or_else(||Error::from_hresult(E_NOTIMPL))?;
        let pointer=unsafe{CoTaskMemAlloc(bytes.len())}.cast::<u8>();
        if pointer.is_null(){return Err(Error::from_hresult(E_OUTOFMEMORY));}
        unsafe{std::ptr::copy_nonoverlapping(bytes.as_ptr(),pointer,bytes.len());}
        Ok(pointer.cast())
    }
}
fn virtual_item()->Result<IShellItem>{unsafe{SHGetKnownFolderItem(&FOLDERID_RecycleBinFolder,KF_FLAG_DEFAULT,None)}}
fn provider(backing:IShellItem,identity:Option<Vec<u8>>)->(IShellItem,Rc<Cell<usize>>){
    let calls=Rc::new(Cell::new(0));
    (IdentityOnlyItem{provider_pidl:identity,backing,filesystem_name_calls:calls.clone()}.into(),calls)
}

#[test]
fn namespace_provider_without_parsing_name_keeps_identity_and_parent()->Result<()>{
    let _apartment=Apartment::new()?;
    let backing=virtual_item()?;
    assert_eq!(unsafe{backing.GetAttributes(SFGAO_FILESYSTEM)}?.0&SFGAO_FILESYSTEM.0,0);
    let expected=super::super::pidl::item_pidl(&backing).unwrap();
    let expected_parent=unsafe{backing.GetParent()}?;
    let (item,calls)=provider(backing,Some(expected.clone()));
    assert!(unsafe{item.GetDisplayName(SIGDN_DESKTOPABSOLUTEPARSING)}.is_err());
    let location=unsafe{location_from_shell_item(&item)}.expect("a provider PIDL must survive unavailable parsing names");
    let ShellLocation::Shell{parsing_name,pidl,..}=&location else{panic!("virtual provider became a filesystem path");};
    assert!(parsing_name.is_empty());assert_eq!(pidl.as_ref(),Some(&expected));
    assert_eq!(location.address(),"设备存储 中文");assert_eq!(calls.get(),0);
    let saved=serde_json::to_string(&location).unwrap();
    let restored:ShellLocation=serde_json::from_str(&saved).unwrap();
    let opened=unsafe{shell_item(&restored)}?;
    assert_eq!(super::super::pidl::item_pidl(&opened),Some(expected));
    assert!(unsafe{opened.GetDisplayName(SIGDN_FILESYSPATH)}.is_err());
    let parent=unsafe{opened.GetParent()}?;
    let parent_location=unsafe{location_from_shell_item(&parent)}.unwrap();
    let reopened_parent=unsafe{shell_item(&parent_location)}?;
    assert_eq!(unsafe{reopened_parent.Compare(&expected_parent,SICHINT_CANONICAL.0 as u32)}?,0);
    Ok(())
}

#[test]
fn namespace_provider_without_any_usable_identity_is_rejected()->Result<()>{
    let _apartment=Apartment::new()?;
    let (item,calls)=provider(virtual_item()?,None);
    assert!(unsafe{location_from_shell_item(&item)}.is_none());assert_eq!(calls.get(),0);
    let corrupt=ShellLocation::Shell{parsing_name:String::new(),title:"设备".into(),pidl:Some(vec![8,0,0,0])};
    assert!(unsafe{shell_item(&corrupt)}.is_err(),"invalid identity must not open the Desktop through an empty parsing string");
    Ok(())
}

#[test]
fn namespace_provider_identity_budget_is_enforced_before_copying()->Result<()>{
    let _apartment=Apartment::new()?;
    // One maximum-size SHITEMID plus its terminator is already over 64 KiB.
    let mut oversized=vec![0u8;u16::MAX as usize+2];oversized[..2].copy_from_slice(&u16::MAX.to_le_bytes());
    let (item,_)=provider(virtual_item()?,Some(oversized));
    assert!(super::super::pidl::item_pidl(&item).is_none());
    assert!(unsafe{location_from_shell_item(&item)}.is_none());
    Ok(())
}

#[test]
fn namespace_contract_keeps_real_filesystem_locations_as_paths()->Result<()>{
    let _apartment=Apartment::new()?;
    let path=PathBuf::from(std::env::var_os("WINDIR").unwrap());
    let item=unsafe{shell_item(&ShellLocation::Path(path.clone()))}?;
    let location=unsafe{location_from_shell_item(&item)}.unwrap();
    assert!(matches!(location,ShellLocation::Path(found) if found.to_string_lossy().eq_ignore_ascii_case(&path.to_string_lossy())));
    Ok(())
}
