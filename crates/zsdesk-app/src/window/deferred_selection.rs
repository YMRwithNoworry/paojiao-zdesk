//! Selection requests outlive Shell's asynchronous directory enumeration.
use super::*;
use std::{ffi::OsString,time::{Duration,Instant}};

pub(super) const TIMER:usize=26;
pub(super) struct Pending {
    tab:u64,location:ShellLocation,names:Vec<OsString>,rename:bool,deadline:Instant,
}
impl WindowState {
    pub(super) fn defer_selection(&mut self,names:Vec<OsString>,rename:bool){
        if names.is_empty(){return;}
        self.deferred_selection=Some(Pending{tab:self.session.active_tab().id,location:self.current_location().clone(),
            names,rename,deadline:Instant::now()+Duration::from_secs(8)});
        unsafe{SetTimer(Some(self.hwnd),TIMER,50,None);}
    }
    pub(super) fn select_created_folder(&mut self,path:PathBuf){
        self.select_created_item(path,true);
    }
    pub(super) fn select_created_item(&mut self,path:PathBuf,rename:bool){
        let (Some(parent),Some(name))=(path.parent(),path.file_name())else{return;};
        let location=ShellLocation::Path(parent.into());
        // A completed operation must never rename an unrelated current folder.
        if shell_registration::same_location(self.current_location(),&location){
            self.defer_selection(vec![name.to_os_string()],rename);
        }else{self.select_created_in_other_pane(&location,name);}
    }
    pub(super) fn poll_deferred_selection(&mut self){
        let Some(request)=self.deferred_selection.take()else{return;};
        if request.tab!=self.session.active_tab().id||!shell_registration::same_location(&request.location,self.current_location()){
            return;
        }
        if self.pending.is_some()||self.queued.is_some(){return;}
        let result=self.browser.as_ref().map(|browser|{
            if request.rename&&!self.background_pane{browser.rename_child(&request.names[0])}
            else{browser.select_children(&request.names)}
        });
        if matches!(result,Some(Ok(()))){return;}
        if Instant::now()<request.deadline{self.deferred_selection=Some(request);}
        else{diagnostics::record("selection","item-not-materialized",None);}
    }
}
