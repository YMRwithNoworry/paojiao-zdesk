use super::*;
use anyhow::{Context,ensure};
use std::{fs,sync::atomic::{AtomicU64,Ordering}};

#[test]
fn surviving_window_saves_its_tabs_only_after_primary_owner_closes()->Result<()>{
    static NEXT:AtomicU64=AtomicU64::new(1);
    let root=std::env::temp_dir().join(format!("zsdesk-session-owner-{}-{}",std::process::id(),NEXT.fetch_add(1,Ordering::Relaxed)));
    fs::create_dir(&root)?;
    struct Fixture(PathBuf);impl Drop for Fixture{fn drop(&mut self){let _=fs::remove_file(self.0.join("explorer-session.json"));let _=fs::remove_dir(&self.0);}}
    let _fixture=Fixture(root.clone());let profile=root.join("explorer-session.json");
    let original=storage::default_session();storage::save(&profile,&original)?;
    let owner=crate::instance::try_own_session(&profile)?.context("first session owner")?;
    let mut local=original.clone();let id=local.take_id();local.tabs.push(ExplorerTab::new(id,ShellLocation::Path(root.clone())));local.active_tab=1;
    let mut secondary=WindowState::new(local.clone(),profile.clone(),None,true);secondary.secondary_window=true;
    secondary.save();ensure!(storage::load(&profile)?.tabs==original.tabs,"secondary window overwrote a live primary");
    ensure!(secondary.secondary_window,"secondary claimed an existing owner");
    drop(owner);
    secondary.save();ensure!(!secondary.secondary_window&&secondary.session_ownership.is_some(),"surviving window did not become the owner");
    let saved=storage::load(&profile)?;ensure!(saved.tabs==local.tabs&&saved.active_tab==1,"surviving window lost its tabs");
    ensure!(crate::instance::try_own_session(&profile)?.is_none(),"new owner lease was not retained");
    drop(secondary);
    ensure!(crate::instance::try_own_session(&profile)?.is_some(),"closed owner retained its lease");
    Ok(())
}
