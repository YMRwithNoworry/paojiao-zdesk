//! Immediate common commands with explicit, isolated extension menus.
use super::*;
use std::collections::HashMap;
use windows::{core::{Interface,Error,HRESULT},Win32::{System::{Com::CoTaskMemFree,SystemServices::SFGAO_FOLDER},UI::Shell::{Common::ITEMIDLIST,IFolderView2,IEnumIDList,IPersistFolder2,IShellFolder,IShellItem,IShellItemArray,IShellView,ILGetSize,SHCreateItemWithParent,SVGIO_SELECTION,SVSI_SELECT,SVSI_NOTAKEFOCUS,SVSI_FOCUSED,SVSI_SELECTIONMARK}}};
use zsdesk_windows::shell::{file_transfer,menu::{self,MenuInterceptions,MenuOutcome}};
use crate::context_menu_process::{AsyncClient as Client,Mode as WorkerMode};

pub(super) const TIMER:usize=19;
pub(super) type SourcePreferences=(bool,menu::ContextMenuOptions);
impl WindowState{
    pub(super) fn menu_source_preferences(&self,source:usize)->windows::core::Result<SourcePreferences>{
        let results=self.menu_source_is_results(source).ok_or_else(||failure("菜单源视图已改变"))?;
        Ok((results,crate::context_preferences::native(&self.session.ui.context_menu)))
    }
}
struct Session{generation:u64,identity:super::pane_input::Identity,view_window:HWND,foreground:HWND,view:IShellView,pidls:Vec<Vec<u8>>,folder:Option<PathBuf>,background:Option<Vec<u8>>,client:Client,catalog:bool,input:Option<super::global_search::InputWatch>,visual:Option<VisualSelection>,invocation_authorized:bool}
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
fn same_identities(expected:&[Vec<u8>],current:&[Vec<u8>])->bool{
    menu::same_item_identities(expected,current)
}
struct ChildPidl(*mut ITEMIDLIST);
impl Drop for ChildPidl{fn drop(&mut self){if !self.0.is_null(){unsafe{CoTaskMemFree(Some(self.0.cast()));}}}}
impl ChildPidl{
    fn bytes(&self)->windows::core::Result<Vec<u8>>{
        if self.0.is_null(){return Err(failure("菜单选区返回空标识"));}
        let size=unsafe{ILGetSize(Some(self.0))}as usize;
        if !(2..=65536).contains(&size){return Err(failure("菜单选区标识大小无效"));}
        let bytes=unsafe{std::slice::from_raw_parts(self.0.cast::<u8>(),size)};
        if !zsdesk_windows::shell::pidl::valid(bytes){return Err(failure("菜单选区标识无效"));}Ok(bytes.to_vec())
    }
}
struct VisualSelection{folder:Vec<u8>,children:Vec<Vec<u8>>,focused:Option<Vec<u8>>}
impl VisualSelection{
    fn capture(view:&IShellView,expected:&[Vec<u8>])->windows::core::Result<Self>{
        let folder_view:IFolderView2=view.cast()?;let folder:IShellFolder=unsafe{folder_view.GetFolder()?};
        let root=background_pidl(view)?;let selected:IEnumIDList=unsafe{folder_view.Items(SVGIO_SELECTION)?};
        let mut children=Vec::with_capacity(expected.len());let mut absolute=Vec::with_capacity(expected.len());let mut bytes=0usize;
        loop{
            let mut raw=[std::ptr::null_mut()];let mut fetched=0;
            let status=unsafe{selected.Next(&mut raw,Some(&mut fetched))};let child=ChildPidl(raw[0]);status.ok()?;
            if fetched==0{break;}
            if children.len()>=8192{return Err(failure("菜单选区数量过多"));}
            let id=child.bytes()?;bytes=bytes.saturating_add(id.len());if bytes>3*1024*1024{return Err(failure("菜单选区标识过大"));}
            let item:IShellItem=unsafe{SHCreateItemWithParent(None,&folder,child.0)}?;
            absolute.push(zsdesk_windows::shell::pidl::item_pidl(&item).ok_or_else(||failure("无法确认菜单选区标识"))?);
            children.push(id);
        }
        if !same_identities(expected,&absolute){return Err(failure("菜单选区在读取期间已改变"));}
        let focused=unsafe{folder_view.GetFocusedItem()}.ok().filter(|index|*index>=0)
            .and_then(|index|unsafe{folder_view.Item(index)}.ok()).and_then(|item|ChildPidl(item).bytes().ok())
            .filter(|item|children.contains(item));
        Ok(Self{folder:root,children,focused})
    }
    fn restore_if_cleared(&self,view:&IShellView,expected:&[Vec<u8>],permit:&dyn Fn()->bool)->bool{
        let restore=(||->windows::core::Result<bool>{
            if !permit(){return Ok(false);}
            let folder:IFolderView2=view.cast()?;
            // This count is cheap while a menu is open. Enumerate identities
            // only after a clear or at an invocation validation barrier.
            if unsafe{folder.ItemCount(SVGIO_SELECTION)?}!=0{return Ok(true);}
            if !same_identities(std::slice::from_ref(&self.folder),&[background_pidl(view)?])||!permit(){return Ok(false);}
            let parent:IShellFolder=unsafe{folder.GetFolder()?};let mut live=Vec::with_capacity(self.children.len());
            for child in &self.children{
                if !permit(){return Ok(false);}
                let item:IShellItem=unsafe{SHCreateItemWithParent(None,&parent,child.as_ptr().cast())}?;
                live.push(zsdesk_windows::shell::pidl::item_pidl(&item).ok_or_else(||failure("菜单选区标识已失效"))?);
            }
            if !same_identities(expected,&live)||!permit(){return Ok(false);}
            for (index,child) in self.children.iter().enumerate(){
                if !permit()||unsafe{folder.ItemCount(SVGIO_SELECTION)?}!=index as i32{return Ok(false);}
                let mut flags=SVSI_SELECT.0|SVSI_NOTAKEFOCUS.0;
                if self.focused.as_ref()==Some(child){flags|=SVSI_FOCUSED.0|SVSI_SELECTIONMARK.0;}
                unsafe{view.SelectItem(child.as_ptr().cast(),flags as u32)}?;
            }
            Ok(same_identities(expected,&snapshot(view)?)&&permit())
        })();
        restore.unwrap_or(false)
    }
}
fn same_selection(view:&IShellView,pidls:&[Vec<u8>],folder:&Option<PathBuf>,background:&Option<Vec<u8>>,input:Option<&super::global_search::InputWatch>)->bool{
    // Background verbs target this folder, not its current selection. DefView
    // can select its first drive asynchronously even after a blank-area click.
    let selected=pidls.is_empty()||snapshot(view).is_ok_and(|current|{
        let same=same_identities(pidls,&current);
        #[cfg(test)]if !same{eprintln!("context selection changed: expected_count={} actual_count={}",pidls.len(),current.len());}
        same
    });
    let path=file_transfer::folder_path(view).ok()==*folder;
    let root=background.as_ref().is_none_or(|expected|background_pidl(view).is_ok_and(|value|same_identities(std::slice::from_ref(expected),&[value])));
    #[cfg(test)]if !(selected&&path&&root){eprintln!("context validation identity: selection={selected} folder={path} namespace={root} expected_items={}",pidls.len());}
    // Shell identity queries can pump input; validate the watch again after
    // those calls, immediately before granting the worker's request.
    selected&&path&&root&&input.is_none_or(|watch|!watch.changed())
}
fn root(view:HWND)->HWND{let owner=unsafe{GetAncestor(view,GA_ROOT)};if owner.0.is_null(){view}else{owner}}
pub(super) fn install(){
    menu::set_view_menu_delegate(Some(show));
    menu::set_diagnostic_delegate(Some(|phase,status|super::diagnostics::record("context-menu",phase,Some(status.0))));
}
#[cfg(test)]pub(super) fn active_pid(owner:HWND)->Option<u32>{ACTIVE.with(|active|active.borrow().get(&(owner.0 as isize)).and_then(|session|session.client.pid()))}
#[cfg(test)]pub(super) fn request_active(owner:HWND)->bool{ACTIVE.with(|active|active.borrow().contains_key(&(owner.0 as isize)))}
#[cfg(test)]pub(super) fn selected_identities(view:&IShellView)->windows::core::Result<Vec<Vec<u8>>>{snapshot(view)}
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
pub(super) fn show(window:HWND,view:&IShellView,_source_window:HWND,position:LPARAM,target:menu::ViewMenuTarget)->windows::core::Result<()>{
    let owner=root(window);
    let source=source(owner,window).ok_or_else(||failure("文件视图正在切换，请稍后重试"))?;
    let identity=super::pane_input::identity(owner,source).ok_or_else(||failure("文件视图已切换，请重新打开菜单"))?;
    start(owner,window,view,identity,menu::context_menu_point(view,position)?,false,false,position.0==-1,target==menu::ViewMenuTarget::Background,None)
}
pub(super) fn show_new(owner:HWND,view:&IShellView,identity:super::pane_input::Identity,point:POINT,preferences:SourcePreferences)->windows::core::Result<()>{
    start(owner,unsafe{view.GetWindow()?},view,identity,point,true,false,false,true,Some(preferences))
}
pub(super) fn show_catalog(owner:HWND,view:&IShellView,identity:super::pane_input::Identity,preferences:SourcePreferences)->windows::core::Result<()>{
    start(owner,unsafe{view.GetWindow()?},view,identity,POINT::default(),false,true,false,false,Some(preferences))
}
fn start(owner:HWND,window:HWND,view:&IShellView,identity:super::pane_input::Identity,point:POINT,new:bool,catalog:bool,keyboard:bool,background_click:bool,captured:Option<SourcePreferences>)->windows::core::Result<()>{
    close(owner);let foreground=unsafe{GetForegroundWindow()};let generation=generation(owner).unwrap();
    // A newly materialized DefView can asynchronously clear its highlight.
    // Restore its visual selection only while the same source has had no input
    // and before any command is authorized. Frozen targets alone are not proof
    // that the visible selection still agrees with the command.
    let input=super::global_search::InputWatch::new();
    let pidls=if new||background_click{Vec::new()}else{snapshot(view)?};let folder=file_transfer::folder_path(view).ok();
    let visual=if !pidls.is_empty()&&input.is_some(){VisualSelection::capture(view,&pidls).ok()}else{None};
    let background=if pidls.is_empty(){Some(background_pidl(view)?)}else{None};
    let options=if let Some((_,options))=&captured{options.clone()}else{
        let pointer=unsafe{GetWindowLongPtrW(owner,GWLP_USERDATA)}as*const Slot;
        let profile=if pointer.is_null(){None}else{unsafe{&*pointer}.try_borrow().ok().and_then(|slot|slot.as_ref().map(|state|state.config_path.clone()))};
        profile.as_deref().and_then(crate::context_preferences::current).map(|settings|crate::context_preferences::native(&settings)).unwrap_or_else(menu::context_menu_options)
    };
    let mut mode=if catalog{WorkerMode::CatalogSelection}else{WorkerMode::More};
    let targets=if pidls.is_empty(){mode=if new{WorkerMode::New}else if catalog{WorkerMode::CatalogBackground}else{WorkerMode::Background};background.clone().into_iter().collect()}else{pidls.clone()};
    let layout=zsdesk_windows::shell::capture_view_layout(view);
    let pointer=unsafe{GetWindowLongPtrW(owner,GWLP_USERDATA)}as*const Slot;
    let results=captured.as_ref().map(|(results,_)|*results).or_else(||if pointer.is_null(){None}else{unsafe{&*pointer}.try_borrow().ok().and_then(|slot|slot.as_ref().and_then(|state|state.menu_source_is_results(identity.source)))})
        .ok_or_else(||failure("菜单源视图已改变"))?;
    let view_context=crate::context_menu_process::MenuViewContext{folder:Some(background_pidl(view)?),results,keyboard,
        f10:keyboard&&unsafe{GetKeyState(VK_F10.0 as i32)}<0};
    let file_actions=!new&&!catalog&&if pidls.is_empty(){file_transfer::filesystem_folder_path(view).ok().flatten().is_some()}else{file_transfer::paths_from_pidls(&pidls).is_ok()};
    let client=Client::spawn_with_context(owner,point,targets,folder.clone(),options,MenuInterceptions{rename:true,open:true,file_actions},mode,unsafe{GetKeyState(VK_SHIFT.0 as i32)}<0,layout,Some(view_context))
        .map_err(|error|failure(&format!("无法启动系统菜单：{error:#}")))?;
    if !current(owner,generation){return Ok(());}
    ACTIVE.with(|active|{active.borrow_mut().insert(owner.0 as isize,Session{generation,identity,view_window:window,foreground,view:view.clone(),pidls,folder,background,client,catalog,input,visual,invocation_authorized:false});});
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
fn paste_destination(session:&Session)->Option<PathBuf>{
    if session.pidls.is_empty(){return file_transfer::filesystem_folder_path(&session.view).ok().flatten();}
    selected_paste_destination(&session.pidls)
}
fn selected_paste_destination(pidls:&[Vec<u8>])->Option<PathBuf>{
    if pidls.len()!=1{return None;}
    let item=zsdesk_windows::shell::pidl::item_from_pidl(&pidls[0]).ok()?;
    use windows::Win32::System::SystemServices::{SFGAO_FILESYSTEM,SFGAO_STREAM};
    let attributes=unsafe{item.GetAttributes(SFGAO_FOLDER|SFGAO_FILESYSTEM|SFGAO_STREAM)}.ok()?.0;
    if attributes&(SFGAO_FOLDER.0|SFGAO_FILESYSTEM.0)!=(SFGAO_FOLDER.0|SFGAO_FILESYSTEM.0)||attributes&SFGAO_STREAM.0!=0{return None;}
    file_transfer::paths_from_pidls(pidls).ok()?.into_iter().next()
}
pub(super) fn poll(owner:HWND){
    let pointer=unsafe{GetWindowLongPtrW(owner,GWLP_USERDATA)}as*const Slot;
    if !pointer.is_null()&&unsafe{&*pointer}.try_borrow().is_err(){return;}
    let Some(mut session)=ACTIVE.with(|active|active.borrow_mut().remove(&(owner.0 as isize)))else{let _=unsafe{KillTimer(Some(owner),TIMER)};return;};
    let generation=session.generation;let identity=session.identity;let window=session.view_window;let foreground=session.foreground;
    if !current(owner,generation)||!live_source(owner,window,identity){return;}
    let view=session.view.clone();let pidls=&session.pidls;let folder=&session.folder;let background=&session.background;let input=&session.input;
    let visual=&session.visual;let invocation_authorized=&mut session.invocation_authorized;
    let preserve=||visual.as_ref().is_none_or(|saved|saved.restore_if_cleared(&view,pidls,&||{
        current(owner,generation)&&live_source(owner,window,identity)&&input.as_ref().is_some_and(|watch|!watch.changed())
    }));
    if !*invocation_authorized&&!preserve(){return;}
    let result=session.client.poll(&mut|before_invoke|{
        let source_valid=current(owner,generation)&&live_source(owner,window,identity);
        let focus_valid=before_invoke||unsafe{GetForegroundWindow()}==foreground;
        let selection_valid=source_valid&&focus_valid&&(*invocation_authorized||preserve())
            &&same_selection(&view,pidls,folder,background,input.as_ref());
        #[cfg(test)]if !(source_valid&&focus_valid&&selection_valid){
            eprintln!("context validation rejected: before_invoke={before_invoke} source={source_valid} focus={focus_valid} selection={selection_valid} source_id={} tab={} epoch={}",identity.source,identity.tab,identity.epoch);
        }
        let permit=source_valid&&focus_valid&&selection_valid&&current(owner,generation)&&live_source(owner,window,identity);
        if permit&&before_invoke{*invocation_authorized=true;}permit
    });
    if !current(owner,generation)||!live_source(owner,window,identity){return;}
    // Full PIDL enumeration belongs to validation barriers, not every 50 ms
    // while the user reads a menu. Native callbacks can be costly for multi-select.
    let layout=session.client.take_layout();
    let created=session.client.take_created();
    let catalog=session.client.take_catalog();
    let created_any=!created.is_empty();
    let valid=if layout.is_some()||!created.is_empty()||matches!(&result,Some(Ok(MenuOutcome::Rename|MenuOutcome::Open|MenuOutcome::Explore|MenuOutcome::Copy|MenuOutcome::Cut|MenuOutcome::Paste|MenuOutcome::Delete{..}))){
        same_selection(&session.view,&session.pidls,&session.folder,&session.background,session.input.as_ref())
            &&current(owner,generation)&&live_source(owner,window,identity)
    }else{true};
    if valid&&matches!(&result,Some(Ok(MenuOutcome::ShellInvoked|MenuOutcome::NativeHandled))){
        if let Some(layout)=layout{zsdesk_windows::shell::restore_view_layout(&session.view,&layout);}
        for item in created{super::pane_input::post(owner,identity,super::pane_input::Intent::CreatedItem(item));}
    }
    match result{
        Some(Ok(_)) if session.catalog=>{super::pane_input::post(owner,identity,super::pane_input::Intent::ContextCatalog(catalog));},
        None=>{ACTIVE.with(|active|{active.borrow_mut().insert(owner.0 as isize,session);});},
        Some(Ok(action @ (MenuOutcome::Copy|MenuOutcome::Cut|MenuOutcome::Paste|MenuOutcome::Delete{..}))) if valid=>{
            if session.input.as_ref().is_some_and(|watch|watch.changed()){return;}
            file_transfer::note_user_input(window);
            let destination=if action==MenuOutcome::Paste{paste_destination(&session)}else{None};
            if action==MenuOutcome::Paste&&(!file_transfer::has_filesystem_clipboard()||destination.is_none()){
                match Client::spawn(owner,POINT::default(),session.pidls.clone(),session.folder.clone(),menu::context_menu_options(),MenuInterceptions::default(),WorkerMode::Paste){
                    Ok(client)=>{session.client=client;ACTIVE.with(|active|{active.borrow_mut().insert(owner.0 as isize,session);});unsafe{SetTimer(Some(owner),TIMER,50,None);}},
                    Err(error)=>{super::pane_input::post(owner,identity,super::pane_input::Intent::TransferMessage(format!("无法启动系统粘贴：{error:#}")));},
                }
                return;
            }
            let operation=(||->windows::core::Result<()>{
                match action{
                    MenuOutcome::Copy|MenuOutcome::Cut=>file_transfer::copy_items(window,&session.pidls,action==MenuOutcome::Cut),
                    MenuOutcome::Delete{permanent}=>file_transfer::delete_paths(window,file_transfer::paths_from_pidls(&session.pidls)?,permanent),
                    MenuOutcome::Paste=>{
                        file_transfer::paste(window,destination.as_deref().ok_or_else(||failure("当前目录不支持路径粘贴"))?)
                    },_=>unreachable!(),
                }
            })();
            let message=match operation{
                Ok(())=>match action{MenuOutcome::Copy=>format!("已复制 {} 个项目到剪贴板",session.pidls.len()),MenuOutcome::Cut=>format!("已剪切 {} 个项目到剪贴板",session.pidls.len()),MenuOutcome::Paste=>"正在粘贴…".into(),_=>"正在删除…".into()},
                Err(error)=>format!("文件操作未完成：{}",file_transfer::error_message(&error)),
            };
            super::pane_input::post(owner,identity,super::pane_input::Intent::TransferMessage(message));
        },
        Some(Ok(MenuOutcome::Rename)) if valid=>{
            let exact=if session.pidls.len()==1{file_transfer::paths_from_pidls(&session.pidls).ok().and_then(|paths|paths.into_iter().next())}else{None};
            let child=exact.filter(|path|path.parent().zip(session.folder.as_deref()).is_some_and(|(parent,folder)|{
                super::shell_registration::same_location(&ShellLocation::Path(parent.into()),&ShellLocation::Path(folder.into()))
            }));
            if session.input.as_ref().is_some_and(|watch|watch.changed()){return;}
            if let Some(path)=child{
                super::pane_input::post(owner,identity,super::pane_input::Intent::RenameChild(path));
            }else if snapshot(&session.view).is_ok_and(|selected|!selected.is_empty()&&same_identities(&session.pidls,&selected))
                &&session.input.as_ref().is_none_or(|watch|!watch.changed()){
                // Namespace/search views can retain their own rename handler,
                // but only with the exact original selection still confirmed.
                if let Ok(folder)=session.view.cast::<IFolderView2>(){let _=unsafe{folder.DoRename()};}
            }else{menu::report_failure(owner,HRESULT(0x80070490u32 as i32));}
        },
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
        Some(Err(error))=>{eprintln!("isolated Shell menu: {error:#}");
            super::diagnostics::record("context-menu","worker-failed",Some(crate::context_menu_process::failure_code(&error).0));
            super::pane_input::post(owner,identity,super::pane_input::Intent::TransferMessage(crate::context_menu_process::failure_message(&error)));
        },
        _=>{},
    }
}

#[cfg(test)]mod identity_tests{
    use super::*;
    #[test]fn selected_folder_paste_targets_the_child_and_never_its_parent()->Result<()>{
        unsafe{OleInitialize(None)}?;struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}let _apartment=Apartment;
        let root=std::env::temp_dir().join(format!("zsdesk-paste-target-{}",std::process::id()));std::fs::create_dir(&root)?;
        struct Fixture(PathBuf);impl Drop for Fixture{fn drop(&mut self){let _=std::fs::remove_file(self.0.join("file.txt"));let _=std::fs::remove_dir(self.0.join("子目录"));let _=std::fs::remove_dir(&self.0);}}let _fixture=Fixture(root.clone());
        let target=root.join("子目录");std::fs::create_dir(&target)?;let file=root.join("file.txt");std::fs::write(&file,b"fixture")?;
        let folder=zsdesk_windows::shell::pidl::location_pidl(&ShellLocation::Path(target.clone())).context("folder PIDL")?;
        let plain=zsdesk_windows::shell::pidl::location_pidl(&ShellLocation::Path(file)).context("file PIDL")?;
        let actual=selected_paste_destination(&[folder.clone()]).context("selected folder destination")?;
        assert_eq!(std::fs::canonicalize(actual)?,std::fs::canonicalize(target)?);assert!(selected_paste_destination(&[plain.clone()]).is_none());
        assert!(selected_paste_destination(&[folder,plain]).is_none());Ok(())
    }
    #[test]fn selection_uses_canonical_identity_when_shell_metadata_changes()->Result<()>{
        unsafe{OleInitialize(None)}?;
        struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}let _apartment=Apartment;
        let root=std::env::temp_dir().join(format!("zsdesk-menu-identity-{}-{}",std::process::id(),unsafe{windows::Win32::System::Threading::GetCurrentThreadId()}));
        std::fs::create_dir(&root)?;
        struct Fixture(PathBuf);impl Drop for Fixture{fn drop(&mut self){let _=std::fs::remove_file(self.0.join("同名文件.txt"));let _=std::fs::remove_file(self.0.join("其它文件.txt"));let _=std::fs::remove_dir(&self.0);}}let _fixture=Fixture(root.clone());
        let path=root.join("同名文件.txt");std::fs::write(&path,b"a")?;
        let first=zsdesk_windows::shell::pidl::location_pidl(&ShellLocation::Path(path.clone())).context("first identity")?;
        std::fs::write(&path,b"changed cached size and attributes")?;
        let second=zsdesk_windows::shell::pidl::location_pidl(&ShellLocation::Path(path)).context("changed identity")?;
        assert_ne!(first,second,"fixture must exercise metadata-bearing PIDL differences");
        assert!(same_identities(&[first.clone()],&[second.clone()]));
        let other=root.join("其它文件.txt");std::fs::write(&other,b"a")?;
        let different=zsdesk_windows::shell::pidl::location_pidl(&ShellLocation::Path(other)).context("different identity")?;
        assert!(same_identities(&[first.clone(),different.clone()],&[different.clone(),second]),"ordering and cached metadata do not change the selected set");
        assert!(!same_identities(&[first.clone(),different.clone()],&[different.clone()]),"a removed target changes the selected set");
        assert!(!same_identities(&[first.clone()],&[different]),"a real replacement selection cannot use the frozen snapshot");
        assert!(!same_identities(&[first],&[]),"a cleared selection must be restored before granting a command");
        Ok(())
    }
    #[test]fn menu_input_watch_observes_source_thread_input()->Result<()>{
        let window=unsafe{CreateWindowExW(WINDOW_EX_STYLE::default(),windows::core::w!("STATIC"),None,WS_POPUP,0,0,1,1,None,None,None,None)}?;
        struct Window(HWND);impl Drop for Window{fn drop(&mut self){let _=unsafe{DestroyWindow(self.0)};}}let _window=Window(window);
        let watch=super::super::global_search::InputWatch::new().context("input watch installation")?;
        assert!(!watch.changed());
        unsafe{PostMessageW(Some(window),WM_LBUTTONDOWN,WPARAM(1),LPARAM(0))?;}
        let mut message=MSG::default();
        assert!(unsafe{PeekMessageW(&mut message,Some(window),WM_LBUTTONDOWN,WM_LBUTTONDOWN,PM_REMOVE)}.as_bool());
        assert!(watch.changed(),"queued source input must invalidate a cleared-selection fallback");Ok(())
    }
}
