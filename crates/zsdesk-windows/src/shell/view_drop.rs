//! Preserve the Shell view's own hit testing and copy/move/link semantics.
use windows::{core::{implement,Interface,Ref,Result},Win32::{Foundation::{HWND,LPARAM,POINTL},System::{Com::IDataObject,
    Ole::{IDropTarget,IDropTarget_Impl,RegisterDragDrop,RevokeDragDrop,DROPEFFECT,DROPEFFECT_NONE},SystemServices::MODIFIERKEYS_FLAGS},UI::Shell::IShellView}};
use super::menu::{self,ViewMenuTarget};

pub(super) struct Registration{window:HWND,_target:IDropTarget}
impl Registration{
    /// A frame-less hosted view can expose its native target without registering
    /// its HWND. Never revoke or replace a target already registered by Shell.
    pub(super) fn ensure(view:&IShellView,results:bool)->Result<Option<Self>>{
        let window=unsafe{view.GetWindow()?};
        let target:IDropTarget=view.cast()?;
        let target=if results{ResultsTarget{window,target}.into()}else{target};
        match unsafe{RegisterDragDrop(window,&target)}{
            Ok(())=>Ok(Some(Self{window,_target:target})),
            Err(error) if error.code().0 as u32==0x80040101=>Ok(None), // DRAGDROP_E_ALREADYREGISTERED
            Err(error)=>Err(error),
        }
    }
}
impl Drop for Registration{fn drop(&mut self){let _=unsafe{RevokeDragDrop(self.window)};}}

fn result_effect(hit:ViewMenuTarget,effect:DROPEFFECT)->DROPEFFECT{
    if hit==ViewMenuTarget::Selection{effect}else{DROPEFFECT_NONE}
}
#[implement(IDropTarget)]
struct ResultsTarget{window:HWND,target:IDropTarget}
impl ResultsTarget{
    fn hit(&self,point:POINTL)->ViewMenuTarget{
        let position=LPARAM(((point.x as u16 as u32)|((point.y as u16 as u32)<<16))as isize);
        menu::classify_view_menu_point(self.window,self.window,position)
    }
}
#[allow(non_snake_case)]
impl IDropTarget_Impl for ResultsTarget_Impl{
    fn DragEnter(&self,data:Ref<IDataObject>,keys:MODIFIERKEYS_FLAGS,point:&POINTL,effect:*mut DROPEFFECT)->Result<()>{
        unsafe{self.target.DragEnter(data.as_ref(),keys,*point,effect)?;*effect=result_effect(self.hit(*point),*effect);}Ok(())
    }
    fn DragOver(&self,keys:MODIFIERKEYS_FLAGS,point:&POINTL,effect:*mut DROPEFFECT)->Result<()>{
        unsafe{self.target.DragOver(keys,*point,effect)?;*effect=result_effect(self.hit(*point),*effect);}Ok(())
    }
    fn DragLeave(&self)->Result<()>{unsafe{self.target.DragLeave()}}
    fn Drop(&self,data:Ref<IDataObject>,keys:MODIFIERKEYS_FLAGS,point:&POINTL,effect:*mut DROPEFFECT)->Result<()>{
        // Search has no physical background destination. Only Shell's item
        // target may accept a drop; never just append files to IResultsFolder.
        if self.hit(*point)!=ViewMenuTarget::Selection{unsafe{*effect=DROPEFFECT_NONE;}return unsafe{self.target.DragLeave()};}
        unsafe{self.target.Drop(data.as_ref(),keys,*point,effect)}
    }
}

#[cfg(test)]mod tests{
    use super::*;
    #[test]fn results_preserve_native_folder_effects_but_never_accept_the_virtual_background(){
        use windows::Win32::System::Ole::{DROPEFFECT_COPY,DROPEFFECT_MOVE,DROPEFFECT_LINK};
        for effect in [DROPEFFECT_NONE,DROPEFFECT_COPY,DROPEFFECT_MOVE,DROPEFFECT_LINK]{
            assert_eq!(result_effect(ViewMenuTarget::Selection,effect),effect);
            for hit in [ViewMenuTarget::Background,ViewMenuTarget::NativeControl,ViewMenuTarget::Unresolved]{assert_eq!(result_effect(hit,effect),DROPEFFECT_NONE);}
        }
    }
    #[test]fn filesystem_view_exposes_native_target_without_creating_a_window(){
        use windows::Win32::{System::Ole::{OleInitialize,OleUninitialize},UI::Shell::IShellFolder};
        unsafe{OleInitialize(None)}.unwrap();
        struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}let _apartment=Apartment;
        let folder:IShellFolder=crate::desktop::folder_for_path(&std::env::temp_dir()).unwrap();
        let view:IShellView=unsafe{folder.CreateViewObject(HWND::default())}.unwrap();
        assert!(view.cast::<IDropTarget>().is_ok());
    }
}
