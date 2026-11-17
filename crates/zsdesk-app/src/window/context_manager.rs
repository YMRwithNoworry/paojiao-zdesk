//! Manage commands discovered from the actual Shell context, without editing
//! machine-wide registrations or maintaining a vendor allow/deny list.
use super::*;
use zsdesk_windows::shell::menu::{self,MenuCatalogItem};
use zsdesk_ui::menus::{self,MenuItemSpec,MenuSpec};
use zsdesk_core::model::explorer::ContextMenuPreferences;

fn entries(items:&[MenuCatalogItem],settings:&ContextMenuPreferences,flat:&mut Vec<MenuCatalogItem>,parent_visible:bool)->Vec<MenuItemSpec>{
    let options=crate::context_preferences::native(settings);
    items.iter().map(|item|{
        let index=flat.len();flat.push(item.clone());
        let label=item.label.replace('&',"&&");
        let visible=parent_visible&&!menu::catalog_item_hidden(item,&options);
        let command=menus::entry(if item.submenu{zsdesk_core::i18n::text("显示此菜单")}else{&label},&format!("catalog-{index}"),item.manageable,visible);
        if item.submenu{
            let mut children=vec![command,MenuItemSpec::Separator];children.extend(entries(&item.children,settings,flat,visible));
            if item.truncated{children.push(menus::entry(zsdesk_core::i18n::text("其余项目由系统动态提供"),"catalog-truncated",false,false));}
            menus::submenu(&label,children)
        }else{command}
    }).collect()
}
fn specification(catalog:&[MenuCatalogItem],settings:&ContextMenuPreferences)->(MenuSpec,Vec<MenuCatalogItem>){
    let mut flat=Vec::new();let mut items=vec![menus::entry(zsdesk_core::i18n::text("勾选表示显示；仅影响 ZSDesk"),"catalog-heading",false,false),MenuItemSpec::Separator];
    items.extend(entries(catalog,settings,&mut flat,true));
    items.extend([MenuItemSpec::Separator,menus::item(zsdesk_core::i18n::text("还原所有菜单命令"),"catalog-reset")]);
    (menus::menu(items),flat)
}
fn toggle(settings:&mut ContextMenuPreferences,item:&MenuCatalogItem,flat:&[MenuCatalogItem]){
    if !item.manageable{return;}
    let options=crate::context_preferences::native(settings);
    let show=menu::catalog_item_hidden(item,&options)||item.path.iter().any(|key|flat.iter().find(|entry|&entry.key==key).is_some_and(|parent|menu::catalog_item_hidden(parent,&options)));
    let keys:Vec<_>=if show{item.path.iter().chain(std::iter::once(&item.key)).cloned().collect()}else{vec![item.key.clone()]};
    for key in keys{
        settings.hidden_commands.retain(|value|value!=&key);settings.shown_commands.retain(|value|value!=&key);
        if show{settings.shown_commands.push(key);}else{settings.hidden_commands.push(key);}
    }
}
impl WindowState{
    pub(super) fn persist_context_menu(&mut self)->bool{
        if let Err(error)=storage::save_context_menu(&self.config_path,&self.session.ui.context_menu){
            if let Ok(saved)=storage::load(&self.config_path){self.session.ui.context_menu=saved.ui.context_menu;}
            self.show_message(zsdesk_core::tr_format!("右键菜单设置未保存：{error:#}", error=error));return false;
        }
        true
    }
    pub(super) fn load_context_catalog(&mut self){
        if self.pending.is_some(){self.show_message(zsdesk_core::i18n::text("请等待当前文件夹加载完成").into());return;}
        let result=(||->windows::core::Result<()>{
            let Some(identity)=self.identity_for_source(self.browser_source)else{return Ok(());};
            let Some(browser)=&self.browser else{return Ok(());};
            let view=unsafe{browser.current_view()?};let preferences=self.menu_source_preferences(identity.source)?;
            super::context_menu::show_catalog(self.hwnd,&view,identity,preferences)
        })();
        match result{Ok(())=>self.show_message(zsdesk_core::i18n::text("正在读取此处的系统右键菜单…").into()),Err(error)=>self.show_message(zsdesk_core::tr_format!("无法读取右键菜单：{error}", error=error))}
    }
    pub(super) fn manage_context_catalog(&mut self,catalog:Vec<MenuCatalogItem>){
        if catalog.is_empty(){self.show_message(zsdesk_core::i18n::text("此处没有可管理的系统菜单项").into());return;}
        let (spec,_)=specification(&catalog,&self.session.ui.context_menu);
        let hwnd=self.hwnd;let anchor=Some(self.layout.app_menu);
        super::input::popup_persistent(hwnd,&spec,anchor,&mut|action|{
            if action=="catalog-reset"{self.session.ui.context_menu=ContextMenuPreferences::default();}
            else{
                let (_,flat)=specification(&catalog,&self.session.ui.context_menu);
                let index=action.strip_prefix("catalog-")?.parse::<usize>().ok()?;let item=flat.get(index)?;
                if !item.manageable{return None;}
                toggle(&mut self.session.ui.context_menu,item,&flat);
            }
            if !self.persist_context_menu(){return None;}
            self.modern=crate::context_preferences::modern(&self.session.ui.context_menu);
            let options=crate::context_preferences::native(&self.session.ui.context_menu);
            menu::set_context_menu_options(options.clone());if let Some(browser)=&self.browser{browser.set_context_menu_options(options);}
            self.save();Some(specification(&catalog,&self.session.ui.context_menu).0)
        });
    }
}

#[cfg(test)]mod tests{
    use super::*;
    use zsdesk_core::model::explorer::ContextMenuMode;
    #[test]fn showing_a_discovered_child_makes_its_hidden_ancestors_visible(){
        let leaf=MenuCatalogItem{key:"verb:commit|path:test".into(),label:"提交".into(),verb:Some("commit".into()),path:vec!["label:tool".into()],enabled:false,submenu:false,manageable:true,truncated:false,children:Vec::new()};
        let parent=MenuCatalogItem{key:"label:tool".into(),label:"开发工具".into(),verb:None,path:Vec::new(),enabled:true,submenu:true,manageable:true,truncated:false,children:vec![leaf.clone()]};
        for mode in [ContextMenuMode::Windows11,ContextMenuMode::DrawnWindows11,ContextMenuMode::Original]{
        let mut settings=ContextMenuPreferences{mode,..Default::default()};settings.hidden_commands.push(parent.key.clone());
        let (_,flat)=specification(&[parent.clone()],&settings);toggle(&mut settings,&leaf,&flat);
        let native=crate::context_preferences::native(&settings);
        assert!(!menu::catalog_item_hidden(&parent,&native));assert!(!menu::catalog_item_hidden(&leaf,&native));
        assert!(settings.shown_commands.contains(&parent.key));assert_eq!(settings.mode,mode);
        toggle(&mut settings,&leaf,&flat);let native=crate::context_preferences::native(&settings);
        assert!(menu::catalog_item_hidden(&leaf,&native));assert!(!menu::catalog_item_hidden(&parent,&native));
        assert_eq!(settings.mode,mode);
        }
    }
}
