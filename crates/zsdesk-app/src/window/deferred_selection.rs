//! Selection requests outlive Shell's asynchronous directory enumeration.
use super::*;
use std::{ffi::OsString,time::{Duration,Instant}};

pub(super) const TIMER:usize=26;
pub(super) struct Pending {
    tab:u64,location:ShellLocation,names:Vec<OsString>,rename:bool,rename_requested:Option<Instant>,deadline:Instant,cursor:usize,input:Option<global_search::InputWatch>,
}
impl WindowState {
    pub(super) fn defer_selection(&mut self,names:Vec<OsString>,rename:bool){
        if names.is_empty(){return;}
        let duration=Duration::from_secs(8)+Duration::from_millis((names.len().min(10_000).div_ceil(32)as u64)*50);
        self.deferred_selection=Some(Pending{tab:self.session.active_tab().id,location:self.current_location().clone(),
            names,rename,rename_requested:None,deadline:Instant::now()+duration,cursor:0,input:global_search::InputWatch::new()});
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
        let Some(mut request)=self.deferred_selection.take()else{return;};
        if request.tab!=self.session.active_tab().id||!shell_registration::same_location(&request.location,self.current_location()){
            return;
        }
        if self.pending.is_some()||self.queued.is_some(){return;}
        if request.input.as_ref().is_some_and(global_search::InputWatch::changed){return;}
        if request.rename&&!self.background_pane&&request.rename_requested.is_some(){
            let focus=unsafe{GetFocus()};
            if text_input_window(focus)&&!self.view_hwnd.0.is_null()&&unsafe{IsChild(self.view_hwnd,focus)}.as_bool(){return;}
            // Shell can acknowledge SVSI_EDIT before its inline editor exists.
            // Keep the same guarded request alive while those messages drain.
            if request.rename_requested.is_some_and(|at|at.elapsed()<Duration::from_millis(200)){
                self.deferred_selection=Some(request);return;
            }
        }
        let end=(request.cursor+32).min(request.names.len());
        let result=self.browser.as_ref().map(|browser|{
            if request.rename&&!self.background_pane{browser.rename_child(&request.names[0])}
            else{browser.select_children_batch(&request.names[request.cursor..end],request.cursor==0,
                &||request.input.as_ref().is_none_or(|input|!input.changed()))}
        });
        if request.input.as_ref().is_some_and(global_search::InputWatch::changed){return;}
        if matches!(result,Some(Ok(()))){
            if request.rename&&!self.background_pane{
                request.rename_requested=Some(Instant::now());
                // Without an input observer, repeating an acknowledged rename
                // could reopen an editor the user just dismissed.
                if request.input.is_none(){return;}
            }else{if end==request.names.len(){return;}request.cursor=end;}
        }
        if Instant::now()<request.deadline{self.deferred_selection=Some(request);}
        else{diagnostics::record("selection","item-not-materialized",None);}
    }
}
