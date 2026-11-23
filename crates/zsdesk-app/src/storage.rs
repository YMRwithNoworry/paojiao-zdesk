use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::{Mutex, atomic::{AtomicU64, Ordering}},
};

use anyhow::{Context, Result};
use zsdesk_core::model::explorer::{Bookmark,ExplorerSession,SearchPreferences};

// A process may host several window threads. Serialize their read/merge/write
// transactions, not only the final rename, and never share a temporary file.
static SAVE_LOCK: Mutex<()> = Mutex::new(());
static SAVE_SERIAL: AtomicU64 = AtomicU64::new(1);

fn profile_home()->PathBuf{
    dirs::config_dir().unwrap_or_else(||PathBuf::from(".")).join(crate::edition::profile_name())
}
fn portable_dir()->Option<PathBuf>{
    std::env::current_exe().ok().and_then(|exe|exe.parent().and_then(portable_dir_at))
}
fn portable_dir_at(dir:&Path)->Option<PathBuf>{
    dir.join("portable.flag").is_file().then(||dir.join("data"))
}
pub(crate) fn profile_for_install_dir(dir:&Path)->PathBuf{
    default_profile(&profile_home(),portable_dir_at(dir))
}
fn default_profile(home:&Path,portable:Option<PathBuf>)->PathBuf{
    let dir=portable.unwrap_or_else(||{
        let selector=home.join("profile-location.json");
        fs::metadata(&selector).ok().filter(|m|m.len()<=128*1024)
            .and_then(|_|fs::read(selector).ok()).and_then(|bytes|serde_json::from_slice::<PathBuf>(&bytes).ok())
            .filter(|dir|dir.is_absolute()).unwrap_or_else(||home.to_path_buf())
    });
    dir.join("explorer-session.json")
}
pub fn config_path()->PathBuf{
    static DEFAULT:std::sync::OnceLock<PathBuf>=std::sync::OnceLock::new();
    DEFAULT.get_or_init(||default_profile(&profile_home(),portable_dir())).clone()
}
pub fn choose_profile_directory(destination:Option<&Path>)->Result<()>{
    anyhow::ensure!(portable_dir().is_none(),"{}",zsdesk_core::i18n::text("便携模式使用程序旁的 data 目录"));
    save_profile_location(&profile_home(),destination)
}
fn save_profile_location(home:&Path,destination:Option<&Path>)->Result<()>{
    let selector=home.join("profile-location.json");
    let Some(directory)=destination else{
        return match fs::remove_file(selector){Ok(())=>Ok(()),Err(e) if e.kind()==io::ErrorKind::NotFound=>Ok(()),Err(e)=>Err(e.into())};
    };
    anyhow::ensure!(directory.is_absolute()&&directory.is_dir(),"{}",zsdesk_core::i18n::text("目标文件夹无效"));
    let session=directory.join("explorer-session.json");
    if session.try_exists()?{load(&session)?;}
    fs::create_dir_all(home)?;
    let temp=selector.with_extension(format!("{}-{}.tmp",std::process::id(),SAVE_SERIAL.fetch_add(1,Ordering::Relaxed)));
    let bytes=serde_json::to_vec(directory)?;
    if let Err(e)=fs::write(&temp,&bytes).and_then(|_|replace_file(&temp,&selector)){let _=fs::remove_file(&temp);return Err(e.into());}
    Ok(())
}

pub fn load(path: &Path) -> Result<ExplorerSession> {
    let text =
        fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    serde_json::from_str(&text).context("invalid explorer session")
}

pub fn default_session() -> ExplorerSession {
    let mut session = ExplorerSession::default();
    if crate::edition::THEMED {
        session.ui.appearance.backdrop = zsdesk_core::model::explorer::BackdropStyle::Acrylic;
    }
    session
}

pub fn save(path: &Path, session: &ExplorerSession) -> Result<()> {
    let _write = SAVE_LOCK.lock().map_err(|_| anyhow::anyhow!("会话保存锁异常"))?;
    save_unlocked(path, session)
}

pub fn save_startup_location(path:&Path,location:&zsdesk_core::model::explorer::StartupLocation)->Result<()> {
    let _write=SAVE_LOCK.lock().map_err(|_|anyhow::anyhow!("会话保存锁异常"))?;
    let mut session=if path.try_exists()?{load(path)?}else{default_session()};session.ui.startup_location=location.clone();save_unlocked(path,&session)
}
pub fn save_new_tab_location(path:&Path,location:&zsdesk_core::model::explorer::NewTabLocation)->Result<()> {
    let _write=SAVE_LOCK.lock().map_err(|_|anyhow::anyhow!("会话保存锁异常"))?;
    let mut session=if path.try_exists()?{load(path)?}else{default_session()};session.ui.new_tab_location=location.clone();save_unlocked(path,&session)
}
pub fn save_file_dialog_takeover(path:&Path,enabled:bool)->Result<()>{
    let _write=SAVE_LOCK.lock().map_err(|_|anyhow::anyhow!("会话保存锁异常"))?;
    let mut session=if path.try_exists()?{load(path)?}else{default_session()};session.ui.file_dialog_takeover=enabled;save_unlocked(path,&session)
}
pub fn save_forwarding(path:&Path,registered:bool,enabled:bool)->Result<()>{
    let _write=SAVE_LOCK.lock().map_err(|_|anyhow::anyhow!("会话保存锁异常"))?;
    let mut session=if path.try_exists()?{load(path)?}else{default_session()};
    if registered{session.ui.forward_unhandled_windows=enabled;}else{session.ui.forward_while_running=enabled;}
    save_unlocked(path,&session)
}
pub fn save_language(path:&Path,language:zsdesk_core::i18n::LanguagePreference)->Result<()>{
    let _write=SAVE_LOCK.lock().map_err(|_|anyhow::anyhow!("会话保存锁异常"))?;
    let mut session=if path.try_exists()?{load(path)?}else{default_session()};session.ui.language=language;save_unlocked(path,&session)
}
pub fn save_explorer_tray(path:&Path,enabled:bool)->Result<()>{
    let _write=SAVE_LOCK.lock().map_err(|_|anyhow::anyhow!("会话保存锁异常"))?;
    let mut session=if path.try_exists()?{load(path)?}else{default_session()};session.ui.explorer_tray=enabled;save_unlocked(path,&session)
}
pub fn toggle_tab_preference(path: &Path, field: fn(&mut zsdesk_core::model::explorer::ExplorerUi) -> &mut bool) -> Result<bool> {
    update_shared_ui(path, |ui| {
        let value = field(ui);
        *value = !*value;
        *value
    })
}

pub fn update_shared_ui<T>(path: &Path, edit: impl FnOnce(&mut zsdesk_core::model::explorer::ExplorerUi) -> T) -> Result<T> {
    let _write=SAVE_LOCK.lock().map_err(|_|anyhow::anyhow!("会话保存锁异常"))?;
    let mut session=if path.try_exists()?{load(path)?}else{default_session()};
    let value = edit(&mut session.ui);
    save_unlocked(path, &session)?;
    Ok(value)
}
pub fn save_context_menu(path:&Path,options:&zsdesk_core::model::explorer::ContextMenuPreferences)->Result<()> {
    let _write=SAVE_LOCK.lock().map_err(|_|anyhow::anyhow!("会话保存锁异常"))?;
    let mut session=load(path)?;session.ui.context_menu=options.clone();save_unlocked(path,&session)?;
    crate::context_preferences::publish(path,options);Ok(())
}

/// A secondary window owns its appearance and bookmarks, while the primary
/// window remains the owner of the persisted tab session.
pub fn save_window(path: &Path, session: &ExplorerSession, secondary: bool) -> Result<()> {
    let _write = SAVE_LOCK.lock().map_err(|_| anyhow::anyhow!("会话保存锁异常"))?;
    if secondary {
        let mut saved = load(path)?;
        saved.ui = session.ui.clone();
        saved.bookmarks = session.bookmarks.clone();
        save_unlocked(path, &saved)
    } else {
        save_unlocked(path, session)
    }
}

fn save_unlocked(path: &Path, session: &ExplorerSession) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let temporary = path.with_extension(format!("{}-{}.json.tmp",std::process::id(),SAVE_SERIAL.fetch_add(1,Ordering::Relaxed)));
    let data = serde_json::to_vec_pretty(session)?;
    if let Err(error) = fs::write(&temporary, data).and_then(|_|replace_file(&temporary, path)) {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    Ok(())
}

/// A navigation/session save must not restore a stale window's search backend.
/// Explicit search preference edits remain last-writer-wins under the same lock.
pub fn save_window_with_search(path:&Path,session:&ExplorerSession,secondary:bool,baseline:&zsdesk_core::model::explorer::SearchPreferences)
    ->Result<zsdesk_core::model::explorer::SearchPreferences>{
    save_window_preferences(path,session,secondary,baseline,None).map(|saved|saved.0)
}

/// Navigation from another window must not undo a saved bookmark category or
/// order. Explicit bookmark edits remain last-writer-wins under the same lock.
pub fn save_window_with_shared_preferences(path:&Path,session:&ExplorerSession,secondary:bool,
    baseline_search:&SearchPreferences,baseline_bookmarks:&[Bookmark])->Result<(SearchPreferences,Vec<Bookmark>)>{
    save_window_preferences(path,session,secondary,baseline_search,Some(baseline_bookmarks))
}

fn save_window_preferences(path:&Path,session:&ExplorerSession,secondary:bool,
    baseline:&SearchPreferences,baseline_bookmarks:Option<&[Bookmark]>)->Result<(SearchPreferences,Vec<Bookmark>)>{
    let _write=SAVE_LOCK.lock().map_err(|_|anyhow::anyhow!("会话保存锁异常"))?;
    let saved=load(path).ok();let mut next=session.clone();
    if let Some(saved)=&saved {next.ui.folder_layouts.clone_from(&saved.ui.folder_layouts);next.ui.explorer_tray=saved.ui.explorer_tray;next.ui.language=saved.ui.language;
        next.ui.startup_location=saved.ui.startup_location.clone();next.ui.new_tab_location=saved.ui.new_tab_location.clone();next.ui.context_menu=saved.ui.context_menu.clone();next.ui.forward_unhandled_windows=saved.ui.forward_unhandled_windows;next.ui.forward_while_running=saved.ui.forward_while_running;next.ui.tab_selection_emphasis=saved.ui.tab_selection_emphasis;
        next.ui.right_click_close_tab=saved.ui.right_click_close_tab;
        next.ui.single_click_blank_up=saved.ui.single_click_blank_up;
        next.ui.double_click_blank_up=saved.ui.double_click_blank_up;}
    if next.ui.search_preferences()==*baseline{if let Some(saved)=&saved{next.ui.apply_search_preferences(&saved.ui.search_preferences());}}
    if let (Some(baseline),Some(saved))=(baseline_bookmarks,&saved){
        next.bookmarks=merge_bookmarks(baseline,&next.bookmarks,&saved.bookmarks,next.next_id.max(saved.next_id));
        next.normalize_bookmarks();
    }
    let bookmark_next=next.bookmarks.iter().map(|bookmark|bookmark.id).max().unwrap_or(0).saturating_add(1);
    next.next_id=next.next_id.max(bookmark_next).max(saved.as_ref().map_or(0,|saved|saved.next_id));
    let preferences=next.ui.search_preferences();let bookmarks=next.bookmarks.clone();
    if secondary{let mut primary=saved.context("主窗口会话尚未保存")?;primary.ui=next.ui;primary.bookmarks=next.bookmarks;primary.next_id=primary.next_id.max(next.next_id);save_unlocked(path,&primary)?;}
    else{save_unlocked(path,&next)?;}Ok((preferences,bookmarks))
}

/// Field-wise edits preserve other windows' changes; deletion wins over a
/// stale edit. Newly allocated IDs are remapped with their child references.
fn merge_bookmarks(baseline:&[Bookmark],local:&[Bookmark],saved:&[Bookmark],mut next:u64)->Vec<Bookmark>{
    use std::collections::{HashMap,HashSet};
    let base:HashMap<_,_>=baseline.iter().map(|b|(b.id,b)).collect();
    let local_ids:HashSet<_>=local.iter().map(|b|b.id).collect();
    let mut result:Vec<_>=saved.iter().filter(|b|!base.contains_key(&b.id)||local_ids.contains(&b.id)).cloned().collect();
    next=next.max(local.iter().chain(saved).map(|b|b.id).max().unwrap_or(0).saturating_add(1));
    let mut remap=HashMap::new();
    for item in local.iter().filter(|b|!base.contains_key(&b.id)){
        if result.iter().any(|b|b.id==item.id){remap.insert(item.id,next);next=next.saturating_add(1);}
    }
    for item in local {
        let mut item=item.clone();
        if let Some(id)=item.parent_id.and_then(|id|remap.get(&id)){item.parent_id=Some(*id);}
        if let Some(old)=base.get(&item.id){
            if let Some(current)=result.iter_mut().find(|b|b.id==item.id){
                if item.display_name!=old.display_name{current.display_name=item.display_name;}
                if item.target!=old.target{current.target=item.target;}
                if item.parent_id!=old.parent_id{current.parent_id=item.parent_id;}
                if item.is_folder!=old.is_folder{current.is_folder=item.is_folder;}
                if item.collapsed!=old.collapsed{current.collapsed=item.collapsed;}
            }
        }else{if let Some(id)=remap.get(&item.id){item.id=*id;}result.push(item);}
    }
    let before:Vec<_>=baseline.iter().filter(|b|local_ids.contains(&b.id)).map(|b|b.id).collect();
    let after:Vec<_>=local.iter().map(|b|remap.get(&b.id).copied().unwrap_or(b.id)).collect();
    if before!=after {
        let order:HashMap<_,_>=after.iter().enumerate().map(|(i,id)|(*id,i)).collect();
        let mut changed:Vec<_>=result.iter().filter(|b|order.contains_key(&b.id)).cloned().collect();
        changed.sort_by_key(|b|order[&b.id]);let mut changed=changed.into_iter();
        for item in &mut result {if order.contains_key(&item.id){*item=changed.next().unwrap();}}
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::{Arc,Barrier},thread};
    use zsdesk_core::model::explorer::{ExplorerTab,ShellLocation};
    #[test]fn tray_preference_survives_other_windows_saving_their_sessions(){
        let root=std::env::temp_dir().join(format!("zsdesk-tray-{}-{}",std::process::id(),SAVE_SERIAL.fetch_add(1,Ordering::Relaxed)));
        fs::create_dir(&root).unwrap();let path=root.join("session.json");let original=default_session();save(&path,&original).unwrap();
        save_explorer_tray(&path,true).unwrap();
        save_window_with_shared_preferences(&path,&original,false,&original.ui.search_preferences(),&original.bookmarks).unwrap();
        assert!(load(&path).unwrap().ui.explorer_tray);
        save_explorer_tray(&path,false).unwrap();assert!(!load(&path).unwrap().ui.explorer_tray);
        fs::remove_file(path).unwrap();fs::remove_dir(root).unwrap();
    }
    #[test]
    fn category_deletion_survives_stale_edits_and_concurrent_ids_remain_distinct(){
        let base=vec![Bookmark::folder(10,"分类".into(),None),Bookmark::link(11,"保留".into(),ShellLocation::ThisPc,None)];
        let mut stale=base.clone();stale[0].collapsed=true;stale[1].display_name="已改名".into();
        let merged=merge_bookmarks(&base,&stale,&base[1..],12);
        assert_eq!(merged.len(),1);assert_eq!(merged[0].id,11);assert_eq!(merged[0].display_name,"已改名");
        let mut left=base.clone();left.push(Bookmark::folder(12,"左".into(),None));left.push(Bookmark::link(13,"子项".into(),ShellLocation::ThisPc,Some(12)));
        let mut right=base.clone();right.push(Bookmark::folder(12,"右".into(),None));
        let merged=merge_bookmarks(&base,&left,&right,14);
        let left_id=merged.iter().find(|b|b.display_name=="左").unwrap().id;
        assert_ne!(left_id,12);assert_eq!(merged.iter().find(|b|b.display_name=="子项").unwrap().parent_id,Some(left_id));
        assert_eq!(merged.iter().map(|b|b.id).collect::<std::collections::HashSet<_>>().len(),merged.len());
    }
    #[test]
    fn startup_location_is_preserved_by_stale_window_saves(){
        use zsdesk_core::model::explorer::StartupLocation;
        let root=std::env::temp_dir().join(format!("zsdesk-startup-{}-{}",std::process::id(),SAVE_SERIAL.fetch_add(1,Ordering::Relaxed)));
        fs::create_dir(&root).unwrap();let path=root.join("session.json");let original=default_session();save(&path,&original).unwrap();
        let chosen=StartupLocation::Folder(root.clone());save_startup_location(&path,&chosen).unwrap();
        let new_tab=zsdesk_core::model::explorer::NewTabLocation::Folder(root.join("新标签"));save_new_tab_location(&path,&new_tab).unwrap();
        save_window_with_shared_preferences(&path,&original,false,&original.ui.search_preferences(),&original.bookmarks).unwrap();
        assert_eq!(load(&path).unwrap().ui.startup_location,chosen);
        assert_eq!(load(&path).unwrap().ui.new_tab_location,new_tab);
        fs::remove_file(path).unwrap();fs::remove_dir(root).unwrap();
    }
    #[test]fn remembered_folder_layouts_survive_stale_saves_and_restart()->Result<()> {
        use zsdesk_core::model::explorer::FolderLayout;
        let root=std::env::temp_dir().join(format!("zsdesk-layout-memory-{}-{}",std::process::id(),SAVE_SERIAL.fetch_add(1,Ordering::Relaxed)));
        fs::create_dir(&root)?;let path=root.join("profile.json");let stale=default_session();save(&path,&stale)?;
        update_shared_ui(&path,|ui|ui.folder_layouts.insert(root.join("A"),FolderLayout::Dual))?;
        update_shared_ui(&path,|ui|ui.folder_layouts.insert(root.join("B"),FolderLayout::Columns))?;
        update_shared_ui(&path,|ui|ui.folder_layouts.insert(root.join("C"),FolderLayout::Single))?;
        for secondary in [true,false] { save_window_with_shared_preferences(&path,&stale,secondary,&stale.ui.search_preferences(),&stale.bookmarks)?; }
        let loaded=load(&path)?;assert_eq!(loaded.ui.folder_layouts.len(),3);
        for (name,layout) in [("A",FolderLayout::Dual),("B",FolderLayout::Columns),("C",FolderLayout::Single)] {assert_eq!(loaded.ui.folder_layouts[&root.join(name)],layout);}
        fs::remove_file(path)?;fs::remove_dir(root)?;Ok(())
    }
    #[test]fn first_launch_can_save_tab_defaults_before_a_session_exists(){
        let root=std::env::temp_dir().join(format!("zsdesk-first-tab-{}-{}",std::process::id(),SAVE_SERIAL.fetch_add(1,Ordering::Relaxed)));
        fs::create_dir(&root).unwrap();let path=root.join("session.json");
        save_new_tab_location(&path,&zsdesk_core::model::explorer::NewTabLocation::CurrentFolder).unwrap();
        assert_eq!(load(&path).unwrap().ui.new_tab_location,zsdesk_core::model::explorer::NewTabLocation::CurrentFolder);
        fs::remove_file(path).unwrap();fs::remove_dir(root).unwrap();
    }
    #[test]
    fn stale_window_cannot_restore_hidden_context_commands(){
        let root=std::env::temp_dir().join(format!("zsdesk-context-save-{}-{}",std::process::id(),SAVE_SERIAL.fetch_add(1,Ordering::Relaxed)));
        fs::create_dir(&root).unwrap();let path=root.join("session.json");let original=default_session();save(&path,&original).unwrap();
        let mut settings=original.ui.context_menu.clone();settings.hidden_commands=vec!["verb:custom-tool".into()];settings.shown_commands=vec!["label:other-tool".into()];
        save_context_menu(&path,&settings).unwrap();
        save_window_with_shared_preferences(&path,&original,false,&original.ui.search_preferences(),&original.bookmarks).unwrap();
        assert_eq!(load(&path).unwrap().ui.context_menu,settings);assert_eq!(crate::context_preferences::current(&path),Some(settings));
        fs::remove_file(path).unwrap();fs::remove_dir(root).unwrap();
    }
    #[test]
    fn stale_window_navigation_preserves_bookmark_groups_order_and_id_allocator(){
        let root=std::env::temp_dir().join(format!("zsdesk-bookmark-save-{}-{}",std::process::id(),SAVE_SERIAL.fetch_add(1,Ordering::Relaxed)));
        fs::create_dir(&root).unwrap();let path=root.join("explorer-session.json");
        let mut original=default_session();original.bookmarks=vec![Bookmark::link(40,"原收藏".into(),ShellLocation::ThisPc,None)];original.next_id=41;
        save(&path,&original).unwrap();let baseline=original.ui.search_preferences();
        let mut edited=original.clone();edited.bookmarks.insert(0,Bookmark::folder(41,"工程".into(),None));edited.bookmarks[1].parent_id=Some(41);edited.next_id=42;
        save_window_with_shared_preferences(&path,&edited,true,&baseline,&original.bookmarks).unwrap();
        let (_,merged)=save_window_with_shared_preferences(&path,&original,false,&baseline,&original.bookmarks).unwrap();
        assert_eq!(merged,edited.bookmarks);assert_eq!(load(&path).unwrap().bookmarks,edited.bookmarks);
        assert!(load(&path).unwrap().next_id>=42);
        let mut deliberate=original.clone();deliberate.bookmarks=merged.clone();deliberate.bookmarks[0].display_name="项目".into();
        let tabs=load(&path).unwrap().tabs;
        save_window_with_shared_preferences(&path,&deliberate,true,&baseline,&merged).unwrap();
        let restored=load(&path).unwrap();assert_eq!(restored.bookmarks[0].display_name,"项目");assert_eq!(restored.tabs,tabs);
        fs::remove_file(path).unwrap();fs::remove_dir(root).unwrap();
    }
    #[test]
    fn stale_navigation_save_cannot_reenable_an_unselected_backend(){
        use zsdesk_core::model::explorer::SearchBackendPreference;
        let root=std::env::temp_dir().join(format!("zsdesk-search-preference-{}-{}",std::process::id(),SAVE_SERIAL.fetch_add(1,Ordering::Relaxed)));
        fs::create_dir(&root).unwrap();let path=root.join("explorer-session.json");let mut original=default_session();original.ui.search_backend=SearchBackendPreference::Everything;
        save(&path,&original).unwrap();let baseline=original.ui.search_preferences();let mut chosen=original.clone();chosen.ui.search_backend=SearchBackendPreference::NativeNtfs;
        save_window_with_search(&path,&chosen,false,&baseline).unwrap();
        let restored=save_window_with_search(&path,&original,true,&baseline).unwrap();assert_eq!(restored.backend,SearchBackendPreference::NativeNtfs);
        assert_eq!(load(&path).unwrap().ui.search_backend,SearchBackendPreference::NativeNtfs);
        let mut explicit=original;explicit.ui.search_backend=SearchBackendPreference::Everything;
        save_window_with_search(&path,&explicit,true,&restored).unwrap();assert_eq!(load(&path).unwrap().ui.search_backend,SearchBackendPreference::Everything);
        fs::remove_file(path).unwrap();fs::remove_dir(root).unwrap();
    }
    #[test]
    fn stale_window_saves_preserve_both_forwarding_choices(){
        let root=std::env::temp_dir().join(format!("zsdesk-forwarding-{}-{}",std::process::id(),SAVE_SERIAL.fetch_add(1,Ordering::Relaxed)));
        fs::create_dir(&root).unwrap();let path=root.join("explorer-session.json");let original=default_session();save(&path,&original).unwrap();
        save_forwarding(&path,true,false).unwrap();save_forwarding(&path,false,true).unwrap();
        save_window_with_search(&path,&original,false,&original.ui.search_preferences()).unwrap();
        let saved=load(&path).unwrap();assert!(!saved.ui.forward_unhandled_windows);assert!(saved.ui.forward_while_running);
        fs::remove_file(path).unwrap();fs::remove_dir(root).unwrap();
    }
    #[test]
    fn tab_contrast_opt_in_survives_stale_window_save_and_can_be_disabled(){
        let root=std::env::temp_dir().join(format!("zsdesk-tab-emphasis-{}-{}",std::process::id(),SAVE_SERIAL.fetch_add(1,Ordering::Relaxed)));
        fs::create_dir(&root).unwrap();let path=root.join("explorer-session.json");let original=default_session();
        assert!(!original.ui.tab_selection_emphasis);save(&path,&original).unwrap();
        let mut legacy=serde_json::to_value(&original).unwrap();legacy["ui"].as_object_mut().unwrap().remove("tab_selection_emphasis");
        fs::write(&path,serde_json::to_vec(&legacy).unwrap()).unwrap();assert!(!load(&path).unwrap().ui.tab_selection_emphasis);
        assert!(toggle_tab_preference(&path, |ui| &mut ui.tab_selection_emphasis).unwrap());
        assert!(toggle_tab_preference(&path, |ui| &mut ui.right_click_close_tab).unwrap());
        save_window_with_search(&path,&original,false,&original.ui.search_preferences()).unwrap();
        let saved=load(&path).unwrap();assert!(saved.ui.tab_selection_emphasis);assert_eq!(saved.tabs,original.tabs);
        assert!(saved.ui.right_click_close_tab);
        assert!(!toggle_tab_preference(&path, |ui| &mut ui.tab_selection_emphasis).unwrap());
        assert!(load(&path).unwrap().ui.right_click_close_tab);
        assert!(!toggle_tab_preference(&path, |ui| &mut ui.right_click_close_tab).unwrap());
        fs::remove_file(path).unwrap();fs::remove_dir(root).unwrap();
    }

    #[test]
    fn concurrent_secondary_saves_preserve_primary_tabs_and_leave_no_pending_files() {
        let root=std::env::temp_dir().join(format!("zsdesk-session-{}-{}",std::process::id(),SAVE_SERIAL.fetch_add(1,Ordering::Relaxed)));
        fs::create_dir(&root).unwrap();
        struct Fixture(PathBuf);
        impl Drop for Fixture {
            fn drop(&mut self){
                if let Ok(items)=fs::read_dir(&self.0){for item in items.flatten(){if item.file_type().is_ok_and(|kind|kind.is_file()){let _=fs::remove_file(item.path());}}}
                let _=fs::remove_dir(&self.0);
            }
        }
        let _fixture=Fixture(root.clone());let path=root.join("explorer-session.json");
        let mut primary=default_session();
        primary.tabs=vec![ExplorerTab::new(1,ShellLocation::ThisPc),ExplorerTab::new(2,ShellLocation::Path(root.clone()))];
        primary.active_tab=1;primary.next_id=3;save(&path,&primary).unwrap();
        let barrier=Arc::new(Barrier::new(9));let mut threads=Vec::new();
        for index in 0..8 {
            let mut secondary=primary.clone();secondary.tabs=vec![ExplorerTab::new(100+index,ShellLocation::ThisPc)];
            secondary.active_tab=0;secondary.ui.tree_width=180+index as i32;
            let path=path.clone();let barrier=barrier.clone();
            threads.push(thread::spawn(move||{barrier.wait();for _ in 0..12{save_window(&path,&secondary,true).unwrap();}}));
        }
        barrier.wait();
        for _ in 0..12{save_window(&path,&primary,false).unwrap();}
        for thread in threads{thread.join().unwrap();}
        let actual=load(&path).unwrap();assert_eq!(actual.tabs,primary.tabs);assert_eq!(actual.active_tab,1);assert_eq!(actual.next_id,3);
        assert_eq!(fs::read_dir(&root).unwrap().count(),1);
    }
}

fn replace_file(temporary: &Path, destination: &Path) -> io::Result<()> {
    #[cfg(windows)] {
        use std::os::windows::ffi::OsStrExt;
        use windows::{core::PCWSTR,Win32::Storage::FileSystem::{MoveFileExW,MOVEFILE_REPLACE_EXISTING,MOVEFILE_WRITE_THROUGH}};
        let source:Vec<u16>=temporary.as_os_str().encode_wide().chain(Some(0)).collect();
        let target:Vec<u16>=destination.as_os_str().encode_wide().chain(Some(0)).collect();
        unsafe {MoveFileExW(PCWSTR(source.as_ptr()),PCWSTR(target.as_ptr()),MOVEFILE_REPLACE_EXISTING|MOVEFILE_WRITE_THROUGH)}
            .map_err(|e|io::Error::other(e.to_string()))
    }
    #[cfg(not(windows))] { fs::rename(temporary,destination) }
}

#[cfg(test)]mod profile_location_tests{
    use super::*;
    #[test]fn selected_profile_round_trips_and_respects_portable_and_reset()->Result<()>{
        let root=std::env::temp_dir().join(format!("zsdesk-profile-location-{}",std::process::id()));
        fs::create_dir(&root)?;
        struct Guard(PathBuf);impl Drop for Guard{fn drop(&mut self){let _=fs::remove_dir_all(&self.0);}}
        let _guard=Guard(root.clone());let home=root.join("initial");let selected=root.join("中文 folder");
        fs::create_dir(&selected)?;save_profile_location(&home,Some(&selected))?;
        assert_eq!(default_profile(&home,None),selected.join("explorer-session.json"));
        assert_eq!(default_profile(&home,Some(root.join("data"))),root.join("data/explorer-session.json"));
        fs::write(selected.join("explorer-session.json"),"invalid")?;
        assert!(save_profile_location(&home,Some(&selected)).is_err());
        save_profile_location(&home,None)?;
        assert_eq!(default_profile(&home,None),home.join("explorer-session.json"));
        Ok(())
    }
}
