//! Manage commands discovered from the actual Shell context, without editing
//! machine-wide registrations or maintaining a vendor allow/deny list.
use super::*;
use zsdesk_windows::shell::menu::{self,MenuCatalogItem};
use zsdesk_ui::menus::{self,MenuItemSpec,MenuSpec};
use zsdesk_core::model::explorer::ContextMenuPreferences;

fn entries(items:&[MenuCatalogItem],settings:&ContextMenuPreferences,flat:&mut Vec<MenuCatalogItem>,parent_visible:bool)->Vec<MenuItemSpec>{
    let options=crate::context_preferences::native(settings);
    let make_row=|item:&MenuCatalogItem,label:&str,flat:&mut Vec<MenuCatalogItem>|{
        let index=flat.len();flat.push(item.clone());
        let label=label.replace('&',"&&");
        let visible=parent_visible&&!menu::catalog_item_hidden(item,&options);
        let command=menus::entry(if item.submenu{zsdesk_core::i18n::text("显示此菜单")}else{&label},&format!("catalog-{index}"),item.manageable,visible);
        if item.submenu{
            let mut children=vec![command,MenuItemSpec::Separator];children.extend(entries(&item.children,settings,flat,visible));
            if item.truncated{children.push(menus::entry(zsdesk_core::i18n::text("其余项目由系统动态提供"),"catalog-truncated",false,false));}
            menus::submenu(&label,children)
        }else{command}
    };
    let mut by_name=std::collections::HashMap::<String,Vec<&MenuCatalogItem>>::new();
    for item in items{by_name.entry(item.label.to_lowercase()).or_default().push(item);}
    let mut result=Vec::new();
    for item in items{
        let Some(siblings)=by_name.remove(&item.label.to_lowercase())else{continue;};
        if siblings.len()==1{result.push(make_row(item,&item.label,flat));continue;}
        // Separate settings identities remain individually selectable, including
        // same-caption verbs with different implementations or submenu trees.
        let choices=siblings.iter().enumerate().map(|(i,item)|{
            let caption=match item.verb.as_deref(){Some(verb)=>format!("{} · {verb}",i+1),None=>format!("{} · {}",i+1,item.label)};
            make_row(item,&caption,flat)
        }).collect();
        result.push(menus::submenu(&item.label.replace('&',"&&"),choices));
    }
    result
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
fn app_choices(apps:&[(String,String)])->Vec<(String,Vec<String>)>{
    let mut choices=std::collections::BTreeMap::<String,(String,Vec<String>)>::new();
    for (id,label) in apps{
        let app=id.strip_prefix("pkg:").and_then(|key|key.split_once(':')).map_or(id.as_str(),|(_,app)|app);
        let row=choices.entry(app.to_owned()).or_insert_with(||(label.clone(),Vec::new()));
        if !row.1.contains(id){row.1.push(id.clone());}
    }
    let mut rows=choices.into_values().collect::<Vec<_>>();
    rows.sort_by(|a,b|a.0.cmp(&b.0));rows
}
fn app_sections(choices:&[(String,Vec<String>)])->Vec<(String,Vec<usize>)>{
    let mut names=std::collections::BTreeMap::new();
    for (index,(label,ids)) in choices.iter().enumerate(){
        let caption=if ids.iter().all(|id|id.starts_with("legacy:")){label.strip_suffix(" (Shell)").unwrap_or(label)}else{label};
        let group=names.entry(caption.to_lowercase()).or_insert_with(||(caption.to_owned(),Vec::new()));
        group.1.push(index);
    }
    names.into_values().collect()
}
fn toggle_app(selected:&mut Vec<String>,ids:&[String],apps:&[(String,String)]){
    let was_enabled=ids.iter().all(|id|menu::native_app_selected(id,selected));
    let mut next=Vec::new();
    for old in selected.iter(){
        let mut migrated=false;
        for (key,_) in apps.iter().filter(|(key,_)|key.starts_with("pkg:")&&key.split(':').nth(1).is_some_and(|class|class.eq_ignore_ascii_case(old))){
            migrated=true;if !next.contains(key){next.push(key.clone());}
        }
        if !migrated&&!next.contains(old){next.push(old.clone());}
    }
    next.retain(|key|!ids.iter().any(|id|id.eq_ignore_ascii_case(key)));
    if !was_enabled{next.extend(ids.iter().cloned());}
    *selected=next;
}
impl WindowState{
    pub(super) fn load_native_apps(&mut self){
        let Some(identity)=self.identity_for_source(self.browser_source)else{return;};
        let owner=self.hwnd.0 as isize;
        let started=std::thread::Builder::new().name("zsdesk-menu-registrations".into()).spawn(move||{
            let result=match menu::native_app_registrations(){
                Ok(list)=>super::pane_input::Intent::NativeApps(list),
                Err(e)=>super::pane_input::Intent::TransferMessage(e.to_string()),
            };
            super::pane_input::post(HWND(owner as *mut _),identity,result);
        });
        if let Err(error)=started{self.show_message(error.to_string());}
    }
    pub(super) fn manage_native_apps(&mut self,apps:Vec<(String,String)>){
        let choices=app_choices(&apps);
        let sections=app_sections(&choices);
        let spec=|prefs:&ContextMenuPreferences|{
            let mut rows=vec![menus::entry(zsdesk_core::i18n::text("选中要加载的应用扩展"),"native-apps-heading",false,false)];
            let entry=|label:&str,index:usize|menus::entry(&label.replace('&',"&&"),&format!("native-app-{index}"),true,
                choices[index].1.iter().all(|id|menu::native_app_selected(id,&prefs.native_first_page_apps)));
            for (label,indices) in &sections{
                if let [index]=indices[..]{rows.push(entry(&choices[index].0,index));continue;}
                let mut branches=Vec::new();
                for &index in indices{
                    let id=&choices[index].1[0];let legacy=id.starts_with("legacy:");
                    let kind=zsdesk_core::i18n::text(if legacy{"传统 Shell 扩展"}else{"应用包扩展"});
                    let same_kind=indices.iter().filter(|&&i|choices[i].1[0].starts_with("legacy:")==legacy).count();
                    let caption=if same_kind==1{kind.to_owned()}else{format!("{kind} · {}",id.splitn(3,':').last().unwrap_or(id))};
                    branches.push(entry(&caption,index));
                }
                rows.push(menus::submenu(&label.replace('&',"&&"),branches));
            }
            rows.push(MenuItemSpec::Separator);
            rows.push(menus::item(zsdesk_core::i18n::text("清除首屏应用选择"),"native-app-clear"));
            menus::menu(rows)
        };
        let initial=spec(&self.session.ui.context_menu);
        super::input::popup_persistent(self.hwnd,&initial,Some(self.layout.app_menu),&mut|choice|{
            let selected=&mut self.session.ui.context_menu.native_first_page_apps;
            if choice=="native-app-clear"{selected.clear();}
            else{
                let index=choice.strip_prefix("native-app-")?.parse::<usize>().ok()?;
                toggle_app(selected,&choices.get(index)?.1,&apps);
            }
            if !self.persist_context_menu(){return None;}
            let options=crate::context_preferences::native(&self.session.ui.context_menu);
            menu::set_context_menu_options(options.clone());if let Some(browser)=&self.browser{browser.set_context_menu_options(options);}
            Some(spec(&self.session.ui.context_menu))
        });
    }
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
    #[test]fn app_rules_preserve_shared_class_and_distinct_packages(){
        let a="pkg:class-1:familyA/app".to_owned();let b="pkg:class-2:familyA/app".to_owned();
        let c="pkg:class-1:familyB/app".to_owned();
        let apps=vec![(a.clone(),"A".into()),(b.clone(),"A".into()),(c.clone(),"B".into())];
        let choices=app_choices(&apps);assert_eq!(choices.len(),2);
        let mut rules=vec!["class-1".into()];
        toggle_app(&mut rules,&choices[0].1,&apps);
        assert!(rules.contains(&a)&&rules.contains(&b)&&rules.contains(&c));assert!(!rules.contains(&"class-1".into()));
        toggle_app(&mut rules,&choices[0].1,&apps);assert_eq!(rules,vec![c]);
    }
    #[test]fn matching_extension_names_share_one_settings_row(){
        let apps=vec![("legacy:cls".into(),"7-Zip (Shell)".into()),("pkg:other:family/app".into(),"7-Zip".into())];
        let choices=app_choices(&apps);let groups=app_sections(&choices);
        assert_eq!(groups.len(),1);assert_eq!(groups[0].0,"7-Zip");assert_eq!(groups[0].1.len(),2);
        let mut selected=vec![];toggle_app(&mut selected,&choices[0].1,&apps);assert_eq!(selected,choices[0].1);
    }
    #[test]
    #[ignore="Reads both installed 7-Zip registrations; uses no menu invocation"]
    fn installed_archive_settings_keep_both_registrations_under_one_name()->Result<()>{
        let apps=menu::native_app_registrations()?;
        let choices=app_choices(&apps);
        let sections=app_sections(&choices);
        let rows:Vec<_>=sections.iter().filter(|(name,_)|name.eq_ignore_ascii_case("7-Zip")).collect();
        anyhow::ensure!(rows.len()==1,"expected one 7-Zip display name");
        let ids=rows[0].1.iter().flat_map(|&i|choices[i].1.iter()).collect::<Vec<_>>();
        anyhow::ensure!(ids.iter().any(|id|id.starts_with("pkg:"))&&ids.iter().any(|id|id.starts_with("legacy:")),"missing registered implementation");
        Ok(())
    }
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
