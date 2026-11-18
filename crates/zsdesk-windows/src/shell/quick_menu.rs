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
    pub fn new(selected:usize,folder:bool,options:&ContextMenuOptions)->Result<Self>{Self::new_for_targets(selected,selected==0&&folder,folder,options)}
    pub fn new_for_targets(selected:usize,paste_target:bool,can_create_folder:bool,options:&ContextMenuOptions)->Result<Self>{
        let result=Self{menu:unsafe{CreatePopupMenu()}?};
        let visible=|verbs:&[&str],label:&str|{
            let explicit=verbs.iter().filter_map(|verb|super::menu_catalog::explicit_visibility(Some(verb),label,&[],options)).collect::<Vec<_>>();
            if explicit.contains(&true){return true;}
            if explicit.contains(&false){return false;}
            !options.hidden_verbs.iter().any(|hidden|verbs.iter().any(|verb|hidden.eq_ignore_ascii_case(verb))
                ||verbs.contains(&"newfolder")&&hidden.eq_ignore_ascii_case("new"))
        };
        if selected>0{
            if visible(&["open"],"打开"){result.item(zsdesk_core::i18n::text("打开"),QuickAction::Open,true)?;}
            result.separator()?;
            if visible(&["cut"],"剪切"){result.item(zsdesk_core::i18n::text("剪切\tCtrl+X"),QuickAction::Cut,true)?;}
            if visible(&["copy"],"复制"){result.item(zsdesk_core::i18n::text("复制\tCtrl+C"),QuickAction::Copy,true)?;}
            if visible(&["copyaspath","copypath"],"复制路径"){result.item(zsdesk_core::i18n::text("复制路径"),QuickAction::CopyPath,true)?;}
            if visible(&["rename"],"重命名"){result.item(zsdesk_core::i18n::text("重命名\tF2"),QuickAction::Rename,selected==1)?;}
            if visible(&["delete"],"删除"){result.item(zsdesk_core::i18n::text("删除\tDel"),QuickAction::Delete,true)?;}
            if selected==1&&paste_target&&visible(&["paste"],"粘贴"){result.item(zsdesk_core::i18n::text("粘贴\tCtrl+V"),QuickAction::Paste,true)?;}
        }else{
            if visible(&["paste"],"粘贴"){result.item(zsdesk_core::i18n::text("粘贴\tCtrl+V"),QuickAction::Paste,paste_target)?;}
            if visible(&["new","newfolder"],"新建文件夹"){result.item(zsdesk_core::i18n::text("新建文件夹\tCtrl+Shift+N"),QuickAction::NewFolder,can_create_folder)?;}
            if visible(&["refresh"],"刷新"){result.item(zsdesk_core::i18n::text("刷新\tF5"),QuickAction::Refresh,true)?;}
        }
        // Virtual namespace folders have properties too. Only filesystem-only
        // actions above depend on the writable-folder hint.
        if visible(&["properties"],"属性"){result.item(zsdesk_core::i18n::text("属性\tAlt+Enter"),QuickAction::Properties,true)?;}
        result.separator()?;
        result.item(zsdesk_core::i18n::text("更多系统/扩展命令(&M)…"),QuickAction::More,true)?;
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
    fn ids(menu:&QuickMenu)->Vec<u32>{(0..unsafe{GetMenuItemCount(Some(menu.hmenu()))}).map(|index|unsafe{GetMenuItemID(menu.hmenu(),index)}).collect()}
    #[test]fn background_folder_actions_follow_shell_target_kind(){
        let namespace=QuickMenu::new_for_targets(0,false,false,&ContextMenuOptions::default()).unwrap();
        for action in [QuickAction::Paste,QuickAction::NewFolder]{assert_ne!(unsafe{GetMenuState(namespace.hmenu(),action.id(),MF_BYCOMMAND)}&MF_GRAYED.0,0);}
        let folder=QuickMenu::new_for_targets(0,true,true,&ContextMenuOptions::default()).unwrap();
        for action in [QuickAction::Paste,QuickAction::NewFolder]{assert_eq!(unsafe{GetMenuState(folder.hmenu(),action.id(),MF_BYCOMMAND)}&MF_GRAYED.0,0);}
    }
    #[test]fn selected_folder_offers_paste_but_multiple_selection_does_not(){
        let folder=QuickMenu::new_for_targets(1,true,false,&ContextMenuOptions::default()).unwrap();
        let ordinary=QuickMenu::new_for_targets(1,false,false,&ContextMenuOptions::default()).unwrap();
        let multiple=QuickMenu::new_for_targets(2,true,false,&ContextMenuOptions::default()).unwrap();
        assert!(ids(&folder).contains(&QuickAction::Paste.id()));
        assert!(!ids(&ordinary).contains(&QuickAction::Paste.id()));
        assert!(!ids(&multiple).contains(&QuickAction::Paste.id()));
    }
    #[test]fn explicit_hidden_verbs_apply_to_immediate_commands(){
        let options=ContextMenuOptions{hidden_verbs:vec!["open".into(),"new".into(),"refresh".into()],..Default::default()};
        let selected=QuickMenu::new_for_targets(1,true,false,&options).unwrap();
        assert!(!ids(&selected).contains(&QuickAction::Open.id()));
        let background=QuickMenu::new_for_targets(0,true,true,&options).unwrap();
        assert!(!ids(&background).contains(&QuickAction::NewFolder.id()));
        assert!(!ids(&background).contains(&QuickAction::Refresh.id()));
    }
    #[test]fn explicit_command_visibility_overrides_legacy_hidden_verbs(){
        let options=ContextMenuOptions{hidden_verbs:vec!["copy".into()],shown_verbs:vec!["verb:copy".into()],..Default::default()};
        let menu=QuickMenu::new_for_targets(1,false,false,&options).unwrap();
        assert!(ids(&menu).contains(&QuickAction::Copy.id()));
    }
    #[test]fn immediate_menu_preserves_command_hiding_and_single_rename(){
        let options=ContextMenuOptions{hidden_verbs:vec!["copy".into(),"delete".into()],..Default::default()};
        let menu=QuickMenu::new_for_targets(3,false,false,&options).unwrap();let ids=ids(&menu);
        assert!(!ids.contains(&QuickAction::Copy.id()));assert!(!ids.contains(&QuickAction::Delete.id()));assert!(ids.contains(&QuickAction::More.id()));
        assert!(unsafe{GetMenuState(menu.hmenu(),QuickAction::Rename.id(),MF_BYCOMMAND)}&MF_GRAYED.0!=0);
    }
}
