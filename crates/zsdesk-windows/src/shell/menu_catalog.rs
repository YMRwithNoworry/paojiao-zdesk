//! Bounded, read-only discovery of the actual Shell menu in this context.
use super::{canonical_verb,ActiveMenuScope,ContextMenuOptions,ShellMenu,APP_FIRST};
use std::collections::{HashMap,HashSet};
use sha2::{Digest,Sha256};
use windows::Win32::UI::{Shell::IContextMenu,WindowsAndMessaging::{
    GetMenuItemCount,GetMenuItemID,GetMenuState,GetMenuStringW,GetSubMenu,
    HMENU,MF_BYPOSITION,MF_DISABLED,MF_GRAYED,MF_SEPARATOR}};

const MAX_ITEMS:usize=512;
const MAX_DEPTH:usize=2;
const LABEL_UNITS:usize=256;
const MAX_KEY_BYTES:usize=128;

#[derive(Clone,Debug,serde::Serialize,serde::Deserialize)]
pub struct MenuCatalogItem {
    pub key:String,
    pub label:String,
    pub verb:Option<String>,
    /// Stable ancestor keys; human-readable ancestry is supplied by `children`.
    pub path:Vec<String>,
    pub enabled:bool,
    pub submenu:bool,
    /// False for an unnamed/truncated identity; its native command stays intact.
    pub manageable:bool,
    /// The display label or a deeper/larger cascade was not fully enumerated.
    pub truncated:bool,
    pub children:Vec<MenuCatalogItem>,
}

fn without_accelerator_suffix(label:&str)->&str{
    let label=label.trim();
    for (open,close) in [('(',')'),('（','）')]{
        if let Some(body)=label.strip_suffix(close){if let Some(index)=body.rfind(open){
            let suffix=body[index+open.len_utf8()..].strip_prefix('&');
            if suffix.is_some_and(|value|value.len()==1&&value.as_bytes()[0].is_ascii_alphanumeric()){
                return body[..index].trim_end();
            }
        }}
    }
    label
}
fn display_label(label:&str)->String{
    let label=without_accelerator_suffix(label.split('\t').next().unwrap_or(label));
    let mut output=String::with_capacity(label.len());let mut chars=label.chars().peekable();
    while let Some(character)=chars.next(){
        if character=='&'{if chars.peek()==Some(&'&'){chars.next();output.push('&');}}
        else{output.push(character);}
    }
    output.trim().into()
}
fn normalize_label(label:&str)->String{
    display_label(label).trim_end_matches('…').trim_end_matches("...")
        .split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}
pub(super) fn identity_key(verb:Option<&str>,label:&str,path:&[String])->Option<String>{
    let (kind,value)=match verb.map(str::trim).filter(|value|!value.is_empty()){
        Some(verb)=>("verb",verb.to_ascii_lowercase()),
        None=>("label",normalize_label(label)),
    };
    if value.is_empty()||value.contains('\0'){return None;}
    if path.is_empty(){
        let key=format!("{kind}:{value}");
        if key.len()<=MAX_KEY_BYTES{return Some(key);}
        return Some(format!("{kind}-sha256:{:x}",Sha256::digest(value.as_bytes())));
    }
    // The typed prefix and length-delimited JSON prevent a label containing
    // separators from colliding with another item's ancestry.
    let bytes=serde_json::to_vec(&(path,&value)).ok()?;
    Some(format!("{kind}-path-sha256:{:x}",Sha256::digest(bytes)))
}
pub(super) fn explicit_visibility(verb:Option<&str>,label:&str,path:&[String],options:&ContextMenuOptions)->Option<bool>{
    let key=identity_key(verb,label,path)?;
    if options.shown_verbs.iter().any(|rule|rule==&key){return Some(true);}
    if options.hidden_verbs.iter().any(|rule|rule==&key){return Some(false);}
    None
}
/// Reports the entry's own setting. A hidden ancestor still hides its cascade;
/// an interface that shows a child should also explicitly show its ancestors.
pub fn catalog_item_hidden(item:&MenuCatalogItem,options:&ContextMenuOptions)->bool{
    if !item.manageable{return false;}
    if options.shown_verbs.contains(&item.key){return false;}
    if options.hidden_verbs.contains(&item.key){return true;}
    item.path.is_empty()&&super::legacy_should_hide(item.verb.as_deref(),&item.label,options)
}

pub(super) struct RawItem{
    pub label:String,pub verb:Option<String>,pub submenu:HMENU,pub enabled:bool,pub complete:bool,
}
pub(super) fn read_item(menu:HMENU,index:i32,context:&IContextMenu)->Option<RawItem>{
    let state=unsafe{GetMenuState(menu,index as u32,MF_BYPOSITION)};
    if state==u32::MAX||state&MF_SEPARATOR.0!=0{return None;}
    let id=unsafe{GetMenuItemID(menu,index)};
    if id>=APP_FIRST&&id!=u32::MAX{return None;}
    let mut buffer=[0u16;LABEL_UNITS+1];
    let count=unsafe{GetMenuStringW(menu,index as u32,Some(&mut buffer),MF_BYPOSITION)}.max(0)as usize;
    let units=&buffer[..count.min(LABEL_UNITS)];let decoded=String::from_utf16(units);
    let complete=count<LABEL_UNITS&&decoded.is_ok();
    let label=decoded.unwrap_or_else(|_|String::from_utf16_lossy(units));
    let verb=canonical_verb(context,id).filter(|value|!value.trim().is_empty());
    Some(RawItem{label,verb,submenu:unsafe{GetSubMenu(menu,index)},enabled:state&(MF_DISABLED.0|MF_GRAYED.0)==0,complete})
}

impl ShellMenu{
    /// Call on a freshly populated, unfiltered context menu in its owning STA.
    /// No command is invoked; only at most two submenu levels are initialized.
    pub fn catalog(&self)->Vec<MenuCatalogItem>{
        let _unfiltered=ActiveMenuScope::enter(&self.menu,false,self.hmenu);
        let mut remaining=MAX_ITEMS;let mut visited=HashSet::new();
        let mut result=collect(self,self.hmenu,&[],true,0,&mut remaining,&mut visited);
        fn count(items:&[MenuCatalogItem],counts:&mut HashMap<String,usize>){for item in items{if item.manageable{*counts.entry(item.key.clone()).or_default()+=1;}count(&item.children,counts);}}
        fn unambiguous(items:&mut [MenuCatalogItem],counts:&HashMap<String,usize>,parent:bool){for item in items{
            item.manageable&=parent&&counts.get(&item.key)==Some(&1);unambiguous(&mut item.children,counts,item.manageable);
        }}
        let mut counts=HashMap::new();count(&result,&mut counts);unambiguous(&mut result,&counts,true);result
    }
}
fn collect(shell:&ShellMenu,menu:HMENU,path:&[String],valid_path:bool,depth:usize,remaining:&mut usize,visited:&mut HashSet<isize>)->Vec<MenuCatalogItem>{
    if *remaining==0||!visited.insert(menu.0 as isize){return Vec::new();}
    let count=unsafe{GetMenuItemCount(Some(menu))}.max(0)as usize;
    let mut output=Vec::new();
    for index in 0..count.min(*remaining){
        if *remaining==0{break;}*remaining-=1;
        let Some(raw)=read_item(menu,index as i32,&shell.menu)else{continue;};
        let key=if valid_path&&(raw.complete||raw.verb.is_some()){identity_key(raw.verb.as_deref(),&raw.label,path)}else{None};
        let manageable=key.is_some();let key=key.unwrap_or_default();
        let submenu=!raw.submenu.is_invalid();let mut truncated=!raw.complete;
        let children=if submenu&&depth<MAX_DEPTH&&*remaining>0&&!visited.contains(&(raw.submenu.0 as isize)){
            shell.init_popup(raw.submenu,index as i32);
            let mut ancestry=path.to_vec();ancestry.push(key.clone());
            collect(shell,raw.submenu,&ancestry,manageable,depth+1,remaining,visited)
        }else{if submenu{truncated=true;}Vec::new()};
        if *remaining==0&&index+1<count{truncated=true;}
        let label=display_label(&raw.label);
        output.push(MenuCatalogItem{key,label:if label.is_empty(){raw.verb.clone().unwrap_or_else(||"未命名菜单项".into())}else{label},verb:raw.verb,
            path:path.to_vec(),enabled:raw.enabled,submenu,manageable,truncated,children});
    }
    output
}

/// Locate an already-present popup without opening unrelated lazy cascades.
pub(super) fn popup_path(root:HMENU,target:HMENU,context:&IContextMenu)->Option<Vec<String>>{
    fn walk(menu:HMENU,target:HMENU,context:&IContextMenu,path:&[String],remaining:&mut usize,seen:&mut HashSet<isize>)->Option<Vec<String>>{
        if menu==target{return Some(path.to_vec());}
        if path.len()>=MAX_DEPTH||*remaining==0||!seen.insert(menu.0 as isize){return None;}
        let count=unsafe{GetMenuItemCount(Some(menu))}.max(0)as usize;
        for index in 0..count.min(*remaining){
            if *remaining==0{break;}*remaining-=1;
            let Some(raw)=read_item(menu,index as i32,context)else{continue;};
            if raw.submenu.is_invalid()||(!raw.complete&&raw.verb.is_none()){continue;}
            let Some(key)=identity_key(raw.verb.as_deref(),&raw.label,path)else{continue;};
            let mut next=path.to_vec();next.push(key);
            if let Some(found)=walk(raw.submenu,target,context,&next,remaining,seen){return Some(found);}
        }None
    }
    let mut remaining=MAX_ITEMS;let mut seen=HashSet::new();
    walk(root,target,context,&[],&mut remaining,&mut seen)
}

#[cfg(test)]mod tests{
    use super::*;
    use std::{cell::Cell,rc::Rc};
    use windows::{core::{implement,Interface,Error,HRESULT,PCWSTR,PSTR,Result},Win32::{Foundation::{WPARAM,LPARAM,LRESULT},
        UI::{Shell::{IContextMenu3,IContextMenu_Impl,IContextMenu2_Impl,IContextMenu3_Impl,CMINVOKECOMMANDINFO},
            WindowsAndMessaging::{AppendMenuW,CreatePopupMenu,MF_STRING,MF_POPUP,WM_INITMENUPOPUP}}}};
    #[implement(IContextMenu3)]struct Fixture{initialized:Rc<Cell<usize>>,invoked:Rc<Cell<usize>>,large:bool}
    #[allow(non_snake_case)]impl IContextMenu_Impl for Fixture_Impl{
        fn QueryContextMenu(&self,menu:HMENU,_:u32,first:u32,_:u32,_:u32)->HRESULT{
            let fill=(||->Result<()>{
                let append=|id:usize,label:&str,flags|{let text:Vec<_>=label.encode_utf16().chain(Some(0)).collect();unsafe{AppendMenuW(menu,flags,id,PCWSTR(text.as_ptr()))}};
                if self.large{for index in 0..600{append((first+index)as usize,&format!("Extension {index}"),MF_STRING)?;}return Ok(());}
                append(first as usize,"打开(&O)",MF_STRING)?;append((first+1)as usize,"打开(&X)",MF_STRING)?;
                let child=unsafe{CreatePopupMenu()}?;append(child.0 as usize,"任意扩展(&T)",MF_POPUP)?;Ok(())
            })();match fill{Ok(())=>HRESULT(3),Err(error)=>error.code()}
        }
        fn InvokeCommand(&self,_:*const CMINVOKECOMMANDINFO)->Result<()>{self.invoked.set(self.invoked.get()+1);Ok(())}
        fn GetCommandString(&self,id:usize,_:u32,_:*const u32,text:PSTR,size:u32)->Result<()>{
            if self.large||!matches!(id,0|2){return Err(Error::from_hresult(HRESULT(0x80004001u32 as i32)));}
            let value=[111u16,112,101,110,0];if size<value.len()as u32{return Err(Error::from_hresult(HRESULT(0x8007007au32 as i32)));}
            unsafe{std::ptr::copy_nonoverlapping(value.as_ptr(),text.0.cast(),value.len());}Ok(())
        }
    }
    #[allow(non_snake_case)]impl IContextMenu2_Impl for Fixture_Impl{
        fn HandleMenuMsg(&self,message:u32,wp:WPARAM,lp:LPARAM)->Result<()>{self.HandleMenuMsg2(message,wp,lp,std::ptr::null_mut())}
    }
    #[allow(non_snake_case)]impl IContextMenu3_Impl for Fixture_Impl{
        fn HandleMenuMsg2(&self,message:u32,wp:WPARAM,_:LPARAM,result:*mut LRESULT)->Result<()>{
            if !result.is_null(){unsafe{*result=LRESULT(0);}}
            if message!=WM_INITMENUPOPUP{return Ok(());}
            self.initialized.set(self.initialized.get()+1);let popup=HMENU(wp.0 as*mut _);
            if unsafe{GetMenuItemCount(Some(popup))}==0{
                unsafe{AppendMenuW(popup,MF_STRING,3,windows::core::w!("打开(&O)"))?;
                    let child=CreatePopupMenu()?;AppendMenuW(popup,MF_POPUP,child.0 as usize,windows::core::w!("更深层"))?;}
            }Ok(())
        }
    }
    #[test]fn real_hmenu_catalog_is_unfiltered_bounded_and_never_invokes()->Result<()>{
        let initialized=Rc::new(Cell::new(0));let invoked=Rc::new(Cell::new(0));
        let context:IContextMenu3=Fixture{initialized:initialized.clone(),invoked:invoked.clone(),large:false}.into();
        let menu=ShellMenu::new(context.cast()?)?;menu.populate(true,false)?;
        let catalog=menu.catalog();assert_eq!(catalog.len(),3);assert_eq!(initialized.get(),2);assert_eq!(invoked.get(),0);
        assert_eq!(catalog[0].key,"verb:open");assert_eq!(catalog[1].key,"label:打开");
        assert!(catalog[2].children[1].children[1].truncated);
        let leaf=&catalog[2].children[0];assert_ne!(leaf.key,catalog[0].key);
        let options=ContextMenuOptions{hidden_verbs:vec![catalog[1].key.clone(),leaf.key.clone()],..Default::default()};
        assert!(catalog_item_hidden(&catalog[1],&options));assert!(!catalog_item_hidden(&catalog[0],&options));
        let popup=unsafe{GetSubMenu(menu.hmenu(),2)};
        super::super::filter_native_menu_at(popup,menu.context(),&options,&leaf.path,false);
        assert_eq!(unsafe{GetMenuItemCount(Some(popup))},1,"only the exact nested key is hidden");
        menu.filter(menu.hmenu(),&options);assert_eq!(unsafe{GetMenuItemCount(Some(menu.hmenu()))},2);
        assert_eq!(invoked.get(),0);Ok(())
    }
    #[test]fn real_hmenu_catalog_stops_at_global_item_limit()->Result<()>{
        let context:IContextMenu3=Fixture{initialized:Rc::new(Cell::new(0)),invoked:Rc::new(Cell::new(0)),large:true}.into();
        let menu=ShellMenu::new(context.cast()?)?;menu.populate(false,false)?;let catalog=menu.catalog();
        assert_eq!(catalog.len(),512);assert!(catalog.last().unwrap().truncated);
        assert_eq!(unsafe{GetMenuItemCount(Some(menu.hmenu()))},600,"discovery must not delete source commands");Ok(())
    }
    #[test]fn truncated_and_ambiguous_labels_are_not_manageable()->Result<()>{
        let context:IContextMenu3=Fixture{initialized:Rc::new(Cell::new(0)),invoked:Rc::new(Cell::new(0)),large:true}.into();
        let menu=ShellMenu::new(context.cast()?)?;
        let large:Vec<_>="长名称".repeat(120).encode_utf16().chain(Some(0)).collect();
        unsafe{AppendMenuW(menu.hmenu(),MF_STRING,1,PCWSTR(large.as_ptr()))?;
            AppendMenuW(menu.hmenu(),MF_STRING,2,windows::core::w!("重复扩展"))?;
            AppendMenuW(menu.hmenu(),MF_STRING,3,windows::core::w!("重复扩展"))?;}
        let catalog=menu.catalog();assert_eq!(catalog.len(),3);
        assert!(catalog[0].truncated);assert!(catalog.iter().all(|entry|!entry.manageable));Ok(())
    }
    #[test]fn identities_are_vendor_independent_and_do_not_match_builtin_names(){
        let label=identity_key(None,"打开(&O)\tEnter",&[]).unwrap();
        assert_eq!(label,"label:打开");
        let options=ContextMenuOptions{hidden_verbs:vec![label],..Default::default()};
        assert_eq!(explicit_visibility(None,"打开",&[],&options),Some(false));
        assert_eq!(explicit_visibility(Some("open"),"打开",&[],&options),None);
        assert_ne!(identity_key(Some("open"),"打开",&[]),identity_key(Some("open"),"打开",&["label:任意扩展".into()]));
        assert_eq!(identity_key(None,"Acme && Tools…",&[]),identity_key(None,"Acme && Tools...",&[]));
    }
    #[test]fn long_keys_are_complete_bounded_and_scoped(){
        let name="目录".repeat(150);let different=format!("{name}甲");
        let a=identity_key(None,&name,&[]).unwrap();let b=identity_key(None,&different,&[]).unwrap();
        assert!(a.len()<=128&&b.len()<=128);assert_ne!(a,b);
        assert_ne!(identity_key(None,"命令",&["label:a/b".into()]),identity_key(None,"命令",&["label:a".into(),"label:b".into()]));
    }
    #[test]fn explicit_show_wins_over_legacy_and_hidden_flags(){
        let key=identity_key(None,"WPS 文件压缩",&[]).unwrap();
        let options=ContextMenuOptions{show_wps:false,shown_verbs:vec![key.clone()],hidden_verbs:vec![key],..Default::default()};
        assert_eq!(explicit_visibility(None,"WPS 文件压缩",&[],&options),Some(true));
        assert!(!super::super::should_hide(None,"WPS 文件压缩",&options));
    }
}
