//! Immediate native commands; third-party extensions are an explicit action.
use super::{ContextMenuOptions,HMENU,HWND,POINT};
use windows::{core::{PCWSTR,Result},Win32::UI::WindowsAndMessaging::*};

#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub enum QuickAction{Open,Cut,Copy,Paste,CopyPath,Rename,Delete,Properties,NewFolder,Refresh,More}
impl QuickAction{
    pub fn id(self)->u32{0x6800+match self{Self::Open=>1,Self::Cut=>2,Self::Copy=>3,Self::Paste=>4,Self::CopyPath=>5,Self::Rename=>6,Self::Delete=>7,Self::Properties=>8,Self::NewFolder=>9,Self::Refresh=>10,Self::More=>11}}
    pub fn from_id(id:u32)->Option<Self>{[Self::Open,Self::Cut,Self::Copy,Self::Paste,Self::CopyPath,Self::Rename,Self::Delete,Self::Properties,Self::NewFolder,Self::Refresh,Self::More].into_iter().find(|action|action.id()==id)}
}
pub struct QuickMenu{menu:HMENU}
impl Drop for QuickMenu{fn drop(&mut self){let _=unsafe{DestroyMenu(self.menu)};}}
impl QuickMenu{
    pub fn new(selected:usize,folder:bool,options:&ContextMenuOptions)->Result<Self>{
        let result=Self{menu:unsafe{CreatePopupMenu()}?};
        let visible=|verbs:&[&str]|!options.hidden_verbs.iter().any(|hidden|verbs.iter().any(|verb|hidden.eq_ignore_ascii_case(verb)));
        if selected>0{
            result.item("打开",QuickAction::Open,true)?;
            result.separator()?;
            if visible(&["cut"]){result.item("剪切\tCtrl+X",QuickAction::Cut,true)?;}
            if visible(&["copy"]){result.item("复制\tCtrl+C",QuickAction::Copy,true)?;}
            if visible(&["copyaspath","copypath"]){result.item("复制路径",QuickAction::CopyPath,true)?;}
            if visible(&["rename"]){result.item("重命名\tF2",QuickAction::Rename,selected==1)?;}
            if visible(&["delete"]){result.item("删除\tDel",QuickAction::Delete,true)?;}
        }else{
            if visible(&["paste"]){result.item("粘贴\tCtrl+V",QuickAction::Paste,folder)?;}
            if visible(&["new"]){result.item("新建文件夹\tCtrl+Shift+N",QuickAction::NewFolder,folder)?;}
            result.item("刷新\tF5",QuickAction::Refresh,true)?;
        }
        // Virtual namespace folders have properties too. Only filesystem-only
        // actions above depend on the writable-folder hint.
        if visible(&["properties"]){result.item("属性\tAlt+Enter",QuickAction::Properties,true)?;}
        result.separator()?;
        result.item("更多系统/扩展命令(&M)…",QuickAction::More,true)?;
        Ok(result)
    }
    fn item(&self,label:&str,action:QuickAction,enabled:bool)->Result<()>{
        let label:Vec<_>=label.encode_utf16().chain(Some(0)).collect();
        unsafe{AppendMenuW(self.menu,MF_STRING|if enabled{MF_ENABLED}else{MF_GRAYED},action.id()as usize,PCWSTR(label.as_ptr()))}
    }
    fn separator(&self)->Result<()>{unsafe{AppendMenuW(self.menu,MF_SEPARATOR,0,None)}}
    pub fn hmenu(&self)->HMENU{self.menu}
    /// Returns an application ID too, when the caller inserted its own items.
    pub fn track_raw(&self,owner:HWND,point:POINT)->Option<u32>{
        let flags=TPM_RETURNCMD|TPM_RIGHTBUTTON|TPM_LEFTALIGN|TPM_TOPALIGN;
        let command=unsafe{TrackPopupMenuEx(self.menu,flags.0,point.x,point.y,owner,None)}.0 as u32;
        (command!=0).then_some(command)
    }
    pub fn track(&self,owner:HWND,point:POINT)->Option<QuickAction>{self.track_raw(owner,point).and_then(QuickAction::from_id)}
}

#[cfg(test)]mod tests{
    use super::*;
    #[test]fn immediate_menu_preserves_command_hiding_and_single_rename(){
        let options=ContextMenuOptions{hidden_verbs:vec!["copy".into(),"delete".into()],..Default::default()};
        let menu=QuickMenu::new(3,true,&options).unwrap();
        let ids:Vec<_>=(0..unsafe{GetMenuItemCount(Some(menu.hmenu()))}).map(|index|unsafe{GetMenuItemID(menu.hmenu(),index)}).collect();
        assert!(!ids.contains(&QuickAction::Copy.id()));assert!(!ids.contains(&QuickAction::Delete.id()));assert!(ids.contains(&QuickAction::More.id()));
        assert!(unsafe{GetMenuState(menu.hmenu(),QuickAction::Rename.id(),MF_BYCOMMAND)}&MF_GRAYED.0!=0);
    }
}
