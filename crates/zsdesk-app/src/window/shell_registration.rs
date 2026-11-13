//! UI-side ownership and selection for the asynchronous Shell window registry.
use super::*;
use crate::shell_window_registry::{Entry, Registry};
#[cfg(test)]#[path="shell_reveal_regression.rs"]mod native_reveal_tests;

pub(super) fn taskbar_created_message()->u32 {
    static MESSAGE:std::sync::OnceLock<u32>=std::sync::OnceLock::new();
    *MESSAGE.get_or_init(||unsafe{RegisterWindowMessageW(windows::core::w!("TaskbarCreated"))})
}

pub(super) fn same_location(left:&ShellLocation,right:&ShellLocation)->bool {
    match (left,right) {
        (ShellLocation::Path(left),ShellLocation::Path(right))=>crate::explorer_forwarder::same_path(left,right),
        (ShellLocation::Shell{parsing_name:left,pidl:left_id,..},ShellLocation::Shell{parsing_name:right,pidl:right_id,..})=>{
            if !left.trim().is_empty()&&!right.trim().is_empty(){left==right}
            else{left_id.as_deref().zip(right_id.as_deref()).is_some_and(|(left,right)|
                zsdesk_windows::shell::pidl::valid(left)&&zsdesk_windows::shell::pidl::valid(right)&&left==right)}
        },
        _=>left==right,
    }
}

#[cfg(test)]mod namespace_identity_tests{
    use super::*;
    #[test]fn nameless_virtual_locations_compare_identities_not_labels(){
        let location=|bytes:Vec<u8>|ShellLocation::Shell{parsing_name:String::new(),title:"同名设备目录".into(),pidl:Some(bytes)};
        let first=location(vec![4,0,1,2,0,0]);let second=location(vec![4,0,3,4,0,0]);
        assert!(same_location(&first,&first));assert!(!same_location(&first,&second));
        let named=ShellLocation::Shell{parsing_name:"provider:folder".into(),title:"同一目录".into(),pidl:Some(vec![4,0,1,2,0,0])};
        assert!(same_location(&first,&named));
        let invalid=location(vec![5,0]);assert!(!same_location(&invalid,&invalid));
    }
}

impl WindowState {
    pub(super) fn update_shell_registry(&mut self) {
        if cfg!(test){return;}
        let default_profile=std::path::absolute(&self.config_path).ok().zip(std::path::absolute(storage::config_path()).ok())
            .is_some_and(|(actual,default)|crate::explorer_forwarder::same_path(&actual,&default));
        let executable=std::env::current_exe().ok();
        let owned=default_profile&&executable.as_ref().is_some_and(|exe|zsdesk_windows::integration::owns_folder_integration(exe));
        if !owned {
            if let Some(mut registry)=self.shell_registry.take(){registry.stop();}
            let changed=default_profile&&executable.as_ref().is_some_and(|exe|zsdesk_windows::integration::folder_integration_needs_attention(exe));
            if changed&&!self.shell_registration_warning {
                self.show_message("文件夹接管的处理程序或隐藏标志已被修改；可在“系统集成”中恢复后重新启用。".into());
            }
            self.shell_registration_warning=changed;
            return;
        }
        self.shell_registration_warning=false;
        if self.shell_registry.is_none(){
            match Registry::new(self.hwnd){
                Ok(registry)=>self.shell_registry=Some(registry),
                Err(error)=>diagnostics::record("shell-registration","start-failed",Some(error.code().0)),
            }
        }
        if let Some(registry)=&self.shell_registry {
            if let Some(error)=registry.take_error(){diagnostics::record("shell-registration",&error,None);}
        }
        self.publish_shell_windows();
    }

    pub(super) fn publish_shell_windows(&mut self) {
        if self.shell_registry.is_none(){return;}
        let entries=self.shell_entries();
        if let Some(registry)=&self.shell_registry{registry.publish(entries);}
    }

    pub(super) fn apply_shell_selections(&mut self) {
        let Some(registry)=&self.shell_registry else{return;};
        let requests=registry.take_requests();
        for request in requests {
            let current=self.shell_entries();
            if !current.iter().any(|entry|request.matches(entry)){continue;}
            if !self.activate_shell_source(request.identity.source){continue;}
            // Switching focus between two visible panes is not navigation.
            // Preserve the registration identity across a multi-item callback.
            if !self.shell_entries().iter().any(|entry|request.matches(entry)){continue;}
            let Some(browser)=&self.browser else{continue;};
            let Ok(view)=(unsafe{browser.current_view()})else{continue;};
            if request.child_pidl.as_deref().is_some_and(|bytes|!valid_relative_pidl(bytes)){continue;}
            crate::instance::request_activation(self.hwnd);
            unsafe{browser.focus();}
            let pidl=request.child_pidl.as_ref().map_or(std::ptr::null(),|bytes|bytes.as_ptr().cast());
            // Keep the Shell's per-item selection semantics. In particular,
            // only its first callback may ask to deselect previous items.
            if let Err(error)=unsafe{view.SelectItem(pidl,request.flags&(1|4|8|16))}{
                diagnostics::record("shell-registration","selection-failed",Some(error.code().0));
            }
        }
    }
}

pub(super) fn ready_entry(source:usize,tab:&ExplorerTab,epoch:u64,shown:Option<u64>,pending:bool)->Option<Entry>{
    if source==0||pending||shown!=Some(tab.id){return None;}
    let supported=match tab.current(){ShellLocation::Path(path)=>path.is_absolute(),ShellLocation::ThisPc=>true,_=>false};
    supported.then(||Entry{identity:crate::shell_window_registry::Identity{source,tab:tab.id,epoch},location:tab.current().clone()})
}

fn valid_relative_pidl(bytes:&[u8])->bool {
    if bytes.len()<2||bytes.len()>65536{return false;}
    let mut at=0usize;
    while at+2<=bytes.len(){
        let count=u16::from_le_bytes([bytes[at],bytes[at+1]])as usize;
        if count==0{return at+2==bytes.len();}
        if count<2||count>bytes.len()-at{return false;}
        at+=count;
    }
    false
}

#[cfg(test)]mod tests{
    use super::*;
    #[test]fn only_completed_visible_folder_tabs_are_registered(){
        let path=std::env::temp_dir().join("registered-view");let tab=ExplorerTab::new(7,ShellLocation::Path(path));
        assert!(ready_entry(1,&tab,3,Some(7),false).is_some());
        assert!(ready_entry(1,&tab,3,Some(7),true).is_none());
        assert!(ready_entry(1,&tab,3,Some(8),false).is_none());
        assert!(ready_entry(0,&tab,3,Some(7),false).is_none());
        let search=ExplorerTab::new(9,ShellLocation::Search{query:"example".into(),scope:None});
        assert!(ready_entry(1,&search,3,Some(9),false).is_none());
    }
    #[test]fn selection_pidls_are_bounded_and_terminated(){
        assert!(valid_relative_pidl(&[0,0]));assert!(valid_relative_pidl(&[4,0,42,17,0,0]));
        for invalid in [&[][..],&[0][..],&[1,0,0,0][..],&[9,0,1,0,0][..],&[0,0,1][..]]{assert!(!valid_relative_pidl(invalid));}
    }
    #[test]fn folder_open_reuses_different_windows_path_spellings(){
        let folder=std::env::temp_dir().join("folder-alias");
        let alternate=PathBuf::from(folder.to_string_lossy().replace('\\',"/").to_ascii_uppercase()+"/");
        assert!(same_location(&ShellLocation::Path(folder),&ShellLocation::Path(alternate)));
    }
}
