//! Immediate common commands with explicit, isolated extension menus.
use super::*;
use std::collections::HashMap;
use windows::{core::{Interface,Error,HRESULT},Win32::{System::{Com::CoTaskMemFree,SystemServices::SFGAO_FOLDER},UI::Shell::{IFolderView2,IPersistFolder2,IShellItemArray,IShellView,SVGIO_SELECTION}}};
use zsdesk_windows::shell::{file_transfer,menu::{self,MenuInterceptions,MenuOutcome}};
use crate::context_menu_process::{AsyncClient as Client,Mode as WorkerMode};

pub(super) const TIMER:usize=19;
struct Session{generation:u64,identity:super::pane_input::Identity,view_window:HWND,foreground:HWND,view:IShellView,pidls:Vec<Vec<u8>>,folder:Option<PathBuf>,background:Option<Vec<u8>>,client:Client}
thread_local!{static ACTIVE:RefCell<HashMap<isize,Session>>=RefCell::new(HashMap::new());}
thread_local!{static GENERATIONS:RefCell<HashMap<isize,u64>>=RefCell::new(HashMap::new());static NEXT:Cell<u64>=const{Cell::new(1)};}
fn generation(owner:HWND)->Option<u64>{GENERATIONS.with(|generations|generations.borrow().get(&(owner.0 as isize)).copied())}
fn current(owner:HWND,value:u64)->bool{generation(owner)==Some(value)}
fn source(owner:HWND,view:HWND)->Option<usize>{
    let pointer=unsafe{GetWindowLongPtrW(owner,GWLP_USERDATA)}as*const Slot;
    if pointer.is_null(){return None;}
    unsafe{&*pointer}.try_borrow().ok().and_then(|slot|slot.as_ref().and_then(|state|state.source_for_view(view)))
}
fn live_source(owner:HWND,view:HWND,expected:super::pane_input::Identity)->bool{
    unsafe{IsWindow(Some(view))}.as_bool() && source(owner,view)==Some(expected.source)
        &&super::pane_input::identity(owner,expected.source)==Some(expected)
}
fn failure(message:&str)->Error{Error::new(HRESULT(0x80004005u32 as i32),message)}
fn snapshot(view:&IShellView)->windows::core::Result<Vec<Vec<u8>>>{
    let folder:IFolderView2=view.cast()?;
    if unsafe{folder.ItemCount(SVGIO_SELECTION)?}==0{return Ok(Vec::new());}
    let items:IShellItemArray=unsafe{folder.Items(SVGIO_SELECTION)?};let count=unsafe{items.GetCount()?};
    if count>8192{return Err(failure("选择数量过多，请减少后重试"));}
    let mut pidls=Vec::with_capacity(count as usize);
    for index in 0..count{let item=unsafe{items.GetItemAt(index)?};pidls.push(zsdesk_windows::shell::pidl::item_pidl(&item).ok_or_else(||failure("无法读取文件标识"))?);}
    Ok(pidls)
}
fn same_selection(view:&IShellView,pidls:&[Vec<u8>],folder:&Option<PathBuf>,background:&Option<Vec<u8>>)->bool{
    // Background verbs target this folder, not its current selection. DefView
    // can select its first drive asynchronously even after a blank-area click.
    let selected=pidls.is_empty()||snapshot(view).is_ok_and(|mut current|{let mut expected=pidls.to_vec();current.sort();expected.sort();current==expected});
    let path=file_transfer::folder_path(view).ok()==*folder;
    let root=background.as_ref().is_none_or(|expected|background_pidl(view).is_ok_and(|value|value==*expected));
    selected&&path&&root
}
fn root(view:HWND)->HWND{let owner=unsafe{GetAncestor(view,GA_ROOT)};if owner.0.is_null(){view}else{owner}}
fn point(view:HWND,position:LPARAM)->POINT{
    if position.0 != -1{return POINT{x:position.0 as i16 as i32,y:(position.0>>16)as i16 as i32};}
    let mut bounds=RECT::default();let _=unsafe{GetWindowRect(view,&mut bounds)};
    POINT{x:bounds.left+36,y:bounds.top+36}
}
pub(super) fn install(){
    menu::set_view_menu_delegate(Some(show));
    menu::set_diagnostic_delegate(Some(|phase,status|super::diagnostics::record("context-menu",phase,Some(status.0))));
}
#[cfg(test)]pub(super) fn active_pid(owner:HWND)->Option<u32>{ACTIVE.with(|active|active.borrow().get(&(owner.0 as isize)).and_then(|session|session.client.pid()))}
#[cfg(test)]pub(super) fn request_active(owner:HWND)->bool{ACTIVE.with(|active|active.borrow().contains_key(&(owner.0 as isize)))}
fn background_pidl(view:&IShellView)->windows::core::Result<Vec<u8>>{
    let folder:IFolderView2=view.cast()?;let persistent:IPersistFolder2=unsafe{folder.GetFolder()?};
    let value=unsafe{persistent.GetCurFolder()?};let bytes=zsdesk_windows::shell::pidl::pidl_bytes(value);unsafe{CoTaskMemFree(Some(value.cast()));}Ok(bytes)
}
pub(super) fn close(owner:HWND){
    let value=NEXT.with(|next|{let value=next.get();next.set(value.wrapping_add(1).max(1));value});
    GENERATIONS.with(|generations|{generations.borrow_mut().insert(owner.0 as isize,value);});
    ACTIVE.with(|active|{active.borrow_mut().remove(&(owner.0 as isize));});let _=unsafe{KillTimer(Some(owner),TIMER)};
}
pub(super) fn destroy(owner:HWND){close(owner);GENERATIONS.with(|generations|{generations.borrow_mut().remove(&(owner.0 as isize));});}
pub(super) fn show(window:HWND,view:&IShellView,position:LPARAM)->windows::core::Result<()>{
    let owner=root(window);
    let source=source(owner,window).ok_or_else(||failure("文件视图正在切换，请稍后重试"))?;
    let identity=super::pane_input::identity(owner,source).ok_or_else(||failure("文件视图已切换，请重新打开菜单"))?;
    start(owner,window,view,identity,point(window,position),false)
}
pub(super) fn show_new(owner:HWND,view:&IShellView,identity:super::pane_input::Identity,point:POINT)->windows::core::Result<()>{
    start(owner,unsafe{view.GetWindow()?},view,identity,point,true)
}
fn start(owner:HWND,window:HWND,view:&IShellView,identity:super::pane_input::Identity,point:POINT,new:bool)->windows::core::Result<()>{
    close(owner);let foreground=unsafe{GetForegroundWindow()};let generation=generation(owner).unwrap();
    let pidls=if new{Vec::new()}else{snapshot(view)?};let folder=file_transfer::folder_path(view).ok();
    let background=if pidls.is_empty(){Some(background_pidl(view)?)}else{None};
    let options=menu::context_menu_options();
    let mut mode=WorkerMode::More;
    let targets=if pidls.is_empty(){mode=if new{WorkerMode::New}else{WorkerMode::Background};background.clone().into_iter().collect()}else{pidls.clone()};
    let layout=if matches!(mode,WorkerMode::Background|WorkerMode::New){zsdesk_windows::shell::capture_view_layout(view)}else{None};
    let client=Client::spawn_with_layout(owner,point,targets,folder.clone(),options,MenuInterceptions{rename:true,open:true},mode,unsafe{GetKeyState(VK_SHIFT.0 as i32)}<0,layout)
        .map_err(|error|failure(&format!("无法启动系统菜单：{error:#}")))?;
    if !current(owner,generation){return Ok(());}
    ACTIVE.with(|active|{active.borrow_mut().insert(owner.0 as isize,Session{generation,identity,view_window:window,foreground,view:view.clone(),pidls,folder,background,client});});
    unsafe{SetTimer(Some(owner),TIMER,50,None)};Ok(())
}

fn open_folder(owner:HWND,identity:super::pane_input::Identity,pidls:&[Vec<u8>])->bool{
    if pidls.len()!=1{return false;}
    let Ok(item)=zsdesk_windows::shell::pidl::item_from_pidl(&pidls[0])else{return false;};
    if !unsafe{item.GetAttributes(SFGAO_FOLDER)}.is_ok_and(|attributes|attributes.0&SFGAO_FOLDER.0!=0){return false;}
    let Some(location)=(unsafe{zsdesk_windows::shell::location_from_shell_item(&item)})else{return false;};
    if !super::pane_input::post(owner,identity,super::pane_input::Intent::NavigateFolder(location)){
        menu::report_failure(owner,HRESULT(0x800700aau32 as i32));
    }
    // A recognized folder command must never fall through to a second Shell
    // Open just because its bounded navigation mailbox was temporarily full.
    true
}
pub(super) fn poll(owner:HWND){
    let Some(mut session)=ACTIVE.with(|active|active.borrow_mut().remove(&(owner.0 as isize)))else{let _=unsafe{KillTimer(Some(owner),TIMER)};return;};
    let generation=session.generation;let identity=session.identity;let window=session.view_window;let foreground=session.foreground;
    if !current(owner,generation)||!live_source(owner,window,identity){return;}
    let view=session.view.clone();let pidls=&session.pidls;let folder=&session.folder;let background=&session.background;
    let result=session.client.poll(&mut|before_invoke|{
        let source_valid=current(owner,generation)&&live_source(owner,window,identity);
        let focus_valid=before_invoke||unsafe{GetForegroundWindow()}==foreground;
        let selection_valid=same_selection(&view,pidls,folder,background);
        source_valid&&focus_valid&&selection_valid&&current(owner,generation)&&live_source(owner,window,identity)
    });
    if !current(owner,generation)||!live_source(owner,window,identity){return;}
    // Full PIDL enumeration belongs to validation barriers, not every 50 ms
    // while the user reads a menu. Native callbacks can be costly for multi-select.
    let layout=session.client.take_layout();
    let created=session.client.take_created();
    let created_any=!created.is_empty();
    let valid=if layout.is_some()||!created.is_empty()||matches!(&result,Some(Ok(MenuOutcome::Rename|MenuOutcome::Open|MenuOutcome::Explore))){
        same_selection(&session.view,&session.pidls,&session.folder,&session.background)
            &&current(owner,generation)&&live_source(owner,window,identity)
    }else{true};
    if valid&&matches!(&result,Some(Ok(MenuOutcome::ShellInvoked|MenuOutcome::NativeHandled))){
        if let Some(layout)=layout{zsdesk_windows::shell::restore_view_layout(&session.view,&layout);}
        for item in created{super::pane_input::post(owner,identity,super::pane_input::Intent::CreatedItem(item));}
    }
    match result{
        None=>{ACTIVE.with(|active|{active.borrow_mut().insert(owner.0 as isize,session);});},
        Some(Ok(MenuOutcome::Rename)) if valid=>{if let Ok(folder)=session.view.cast::<IFolderView2>(){let _=unsafe{folder.DoRename()};}},
        Some(Ok(MenuOutcome::Open|MenuOutcome::Explore)) if valid=>{
            if !open_folder(owner,identity,&session.pidls){
                let mut point=POINT::default();let _=unsafe{GetCursorPos(&mut point)};
                if let Ok(client)=Client::spawn(owner,point,session.pidls.clone(),session.folder.clone(),menu::context_menu_options(),MenuInterceptions::default(),WorkerMode::Open){
                    if current(owner,generation)&&live_source(owner,window,identity){session.client=client;ACTIVE.with(|active|{active.borrow_mut().insert(owner.0 as isize,session);});}
                }
            }
        },
        Some(Ok(MenuOutcome::ShellInvoked)) if !created_any=>{super::pane_input::post(owner,identity,super::pane_input::Intent::Action(Command::Refresh.id()as usize));},
        Some(Ok(MenuOutcome::NativeHandled)) if !created_any&&session.pidls.is_empty()=>{super::pane_input::post(owner,identity,super::pane_input::Intent::Action(Command::Refresh.id()as usize));},
        Some(Err(error))=>{eprintln!("isolated Shell menu: {error:#}");menu::report_failure(owner,HRESULT(0x80004005u32 as i32));},
        _=>{},
    }
}
