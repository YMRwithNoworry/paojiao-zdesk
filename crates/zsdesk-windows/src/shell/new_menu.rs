//! Public ShellNew menu and the documented selection/edit completion contract.
use std::{cell::RefCell,ffi::c_void,path::PathBuf};
use windows::{core::{implement,Interface,IUnknown,IUnknownImpl,GUID,Result,Error,HRESULT},Win32::{System::{Com::{CoCreateInstance,IServiceProvider,IServiceProvider_Impl,CLSCTX_INPROC_SERVER},Ole::IObjectWithSite},UI::Shell::{
    IContextMenu,IShellExtInit,IShellView,IFolderView2,IPersistFolder2,INewMenuClient,INewMenuClient_Impl,Common::ITEMIDLIST,
    NMCII_ITEMS,NMCII_FOLDERS,NMCSAEI_EDIT,SIGDN_FILESYSPATH}}};
use super::menu::ShellMenu;

#[derive(Clone,Debug,serde::Serialize,serde::Deserialize)]
pub struct CreatedItem {pub path:PathBuf,pub rename:bool}
thread_local! {static CREATED:RefCell<Vec<CreatedItem>>=const{RefCell::new(Vec::new())};}

unsafe extern "system" fn record(pidl:*const ITEMIDLIST,flags:i32)->HRESULT {
    let result=std::panic::catch_unwind(||->Result<()>{
        if pidl.is_null(){return Err(Error::from_hresult(HRESULT(0x80070057u32 as i32)));}
        let item=unsafe{windows::Win32::UI::Shell::SHCreateItemFromIDList::<windows::Win32::UI::Shell::IShellItem>(pidl)}?;
        let path=PathBuf::from(unsafe{super::take(item.GetDisplayName(SIGDN_FILESYSPATH)?)});
        CREATED.with(|created|{let mut created=created.borrow_mut();if created.len()<32{created.push(CreatedItem{path,rename:flags&NMCSAEI_EDIT.0!=0});}});Ok(())
    });
    match result{Ok(Ok(()))=>HRESULT(0),Ok(Err(error))=>error.code(),Err(_)=>HRESULT(0x8000ffffu32 as i32)}
}
#[cfg(all(target_arch="x86_64",target_env="msvc"))]
extern "C" {fn zs_menu_set_new_item_callback(callback:Option<unsafe extern "system" fn(*const ITEMIDLIST,i32)->HRESULT>);}

#[implement(IServiceProvider,INewMenuClient)]
struct Site {previous:Option<IUnknown>}
impl IServiceProvider_Impl for Site_Impl {
    fn QueryService(&self,service:*const GUID,iid:*const GUID,out:*mut *mut c_void)->Result<()> {
        if service.is_null()||iid.is_null()||out.is_null(){return Err(Error::from_hresult(HRESULT(0x80004003u32 as i32)));}
        unsafe{*out=std::ptr::null_mut();}
        if unsafe{*service}==INewMenuClient::IID {
            let client:INewMenuClient=self.to_interface();return unsafe{client.query(iid,out).ok()};
        }
        if let Some(previous)=&self.previous {
            if let Ok(provider)=previous.cast::<IServiceProvider>() {return unsafe{(provider.vtable().QueryService)(provider.as_raw(),service,iid,out).ok()};}
        }
        Err(Error::from_hresult(HRESULT(0x80004002u32 as i32)))
    }
}
impl INewMenuClient_Impl for Site_Impl {
    fn IncludeItems(&self)->Result<i32>{Ok(NMCII_ITEMS.0|NMCII_FOLDERS.0)}
    fn SelectAndEditItem(&self,pidl:*const ITEMIDLIST,flags:i32)->Result<()>{unsafe{record(pidl,flags).ok()}}
}

/// Retains the original services and restores the exact site on every exit.
pub struct CreationCapture {object:IObjectWithSite,previous:Option<IUnknown>,_site:IUnknown}
impl CreationCapture {
    pub fn attach(menu:&ShellMenu,view:&IShellView)->Result<Self>{
        let object=menu.context().cast::<IObjectWithSite>()?;
        let previous=unsafe{object.GetSite::<IUnknown>()}.ok();
        let fallback=previous.clone().or_else(||view.cast().ok());
        let site:IUnknown=Site{previous:fallback}.into();unsafe{object.SetSite(&site)?;}
        CREATED.with(|created|created.borrow_mut().clear());
        #[cfg(all(target_arch="x86_64",target_env="msvc"))]unsafe{zs_menu_set_new_item_callback(Some(record));}
        Ok(Self{object,previous,_site:site})
    }
    pub fn take(&self)->Vec<CreatedItem>{CREATED.with(|created|std::mem::take(&mut *created.borrow_mut()))}
}
impl Drop for CreationCapture {fn drop(&mut self){
    let _=unsafe{self.object.SetSite(self.previous.as_ref())};
    #[cfg(all(target_arch="x86_64",target_env="msvc"))]unsafe{zs_menu_set_new_item_callback(None);}
    CREATED.with(|created|created.borrow_mut().clear());
}}

pub fn for_view(view:&IShellView)->Result<ShellMenu>{
    let folder:IFolderView2=view.cast()?;let persistent:IPersistFolder2=unsafe{folder.GetFolder()?};
    let pidl=unsafe{persistent.GetCurFolder()?};
    let result=(||{
        let menu:IContextMenu=unsafe{CoCreateInstance(&GUID::from_u128(0xd969a300_e7ff_11d0_a93b_00a0c90f2719),None,CLSCTX_INPROC_SERVER)}?;
        unsafe{menu.cast::<IShellExtInit>()?.Initialize(Some(pidl),None,None)?;}
        ShellMenu::new_background(menu)
    })();
    unsafe{windows::Win32::System::Com::CoTaskMemFree(Some(pidl.cast()));}result
}
