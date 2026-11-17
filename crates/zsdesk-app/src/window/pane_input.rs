//! Bounded input tickets bind delayed actions to a specific pane and tab.
use super::*;
use std::sync::{Mutex,OnceLock,atomic::{AtomicUsize,Ordering}};
pub(super) const MESSAGE:u32=WM_APP+50;
const LIMIT:usize=128;
static NEXT:AtomicUsize=AtomicUsize::new(1);
static PENDING:OnceLock<Mutex<HashMap<usize,Envelope>>>=OnceLock::new();
static BLANK_CONTEXTS:OnceLock<Mutex<HashMap<isize,usize>>>=OnceLock::new();

#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub(super) struct Identity {pub source:usize,pub tab:u64,pub epoch:u64}
pub(super) enum Intent {Action(usize),Wheel(i32),OpenFolder(PathBuf),OpenFolders(Vec<PathBuf>),NavigateFolder(ShellLocation),SubmitAddress(String),CreatedItem(zsdesk_windows::shell::new_menu::CreatedItem),ContextCatalog(Vec<zsdesk_windows::shell::menu::MenuCatalogItem>),ContextMenu(POINT),RenameChild(PathBuf),TransferMessage(String)}
struct Envelope {owner:isize,identity:Identity,intent:Intent}

pub(super) fn post(owner:HWND,identity:Identity,intent:Intent)->bool {
    let token=NEXT.fetch_add(1,Ordering::Relaxed).max(1);
    let pending=PENDING.get_or_init(Default::default);
    {
        let mut pending=pending.lock().unwrap_or_else(|e|e.into_inner());
        if pending.len()>=LIMIT{return false;}
        pending.insert(token,Envelope{owner:owner.0 as isize,identity,intent});
    }
    if unsafe{PostMessageW(Some(owner),MESSAGE,WPARAM(token),LPARAM(0))}.is_ok(){return true;}
    pending.lock().unwrap_or_else(|e|e.into_inner()).remove(&token);false
}

pub(super) fn discard(owner:HWND) {
    if let Some(pending)=PENDING.get(){pending.lock().unwrap_or_else(|e|e.into_inner()).retain(|_,envelope|envelope.owner!=owner.0 as isize);}
    if let Some(contexts)=BLANK_CONTEXTS.get(){contexts.lock().unwrap_or_else(|e|e.into_inner()).remove(&(owner.0 as isize));}
}

fn blank_context(owner:HWND)->Option<usize>{
    let mut contexts=BLANK_CONTEXTS.get_or_init(Default::default).lock().unwrap_or_else(|e|e.into_inner());
    let key=owner.0 as isize;if let Some(value)=contexts.get(&key){return Some(*value);}
    if contexts.len()>=LIMIT{return None;}
    let value=NEXT.fetch_add(1,Ordering::Relaxed).max(1);contexts.insert(key,value);Some(value)
}
fn current_blank_context(owner:HWND)->Option<usize>{
    BLANK_CONTEXTS.get().and_then(|contexts|contexts.lock().unwrap_or_else(|e|e.into_inner()).get(&(owner.0 as isize)).copied())
}
/// Sent activation messages do not pass through GetMessage. Invalidate the
/// click pair at that boundary without retaining records for untouched windows.
pub(super) fn invalidate_blank_clicks(owner:HWND){
    if let Some(contexts)=BLANK_CONTEXTS.get(){
        if let Some(value)=contexts.lock().unwrap_or_else(|e|e.into_inner()).get_mut(&(owner.0 as isize)){
            *value=NEXT.fetch_add(1,Ordering::Relaxed).max(1);
        }
    }
}

#[derive(Clone,Copy)]struct BlankClick{owner:isize,identity:Identity,context:usize,point:POINT,time:u32,released:bool,fired:bool}
#[derive(Default)]pub(super) struct BlankClicks{last:Option<BlankClick>}
impl BlankClicks{
    fn near(first:POINT,second:POINT)->bool{
        (i64::from(first.x)-i64::from(second.x)).abs()<=i64::from(unsafe{GetSystemMetrics(SM_CXDOUBLECLK)}.max(1))/2
            &&(i64::from(first.y)-i64::from(second.y)).abs()<=i64::from(unsafe{GetSystemMetrics(SM_CYDOUBLECLK)}.max(1))/2
    }
    fn release(&mut self,owner:HWND,identity:Identity){
        let current=current_blank_context(owner);
        if let Some(last)=self.last.as_mut(){if last.owner==owner.0 as isize&&last.identity==identity&&current==Some(last.context){last.released=true;}else{self.last=None;}}
    }
    fn pair(&mut self,owner:HWND,identity:Identity,point:POINT,time:u32,double:bool)->Option<POINT>{
        let Some(context)=blank_context(owner)else{self.last=None;return None;};
        let matches=self.last.as_ref().is_some_and(|last|last.owner==owner.0 as isize&&last.identity==identity&&last.context==context
            &&time.wrapping_sub(last.time)<=unsafe{GetDoubleClickTime()}&&Self::near(last.point,point));
        if matches{
            let last=self.last.as_mut().unwrap();
            if last.fired&&double{return None;}
            if last.released&&!last.fired{last.fired=true;last.released=false;return Some(last.point);}
        }
        // A DBLCLK from another view/epoch, or one without its initial down/up,
        // must not navigate the current folder. Ordinary third clicks can start
        // another pair after the preceding one has completed.
        self.last=(!double).then_some(BlankClick{owner:owner.0 as isize,identity,context,point,time,released:false,fired:false});None
    }
}
pub(super) fn blank_double_click(slot:&Slot,message:&MSG,clicks:&mut BlankClicks)->bool{
    if message.message==WM_MOUSEMOVE&&message.wParam.0&1!=0{
        if clicks.last.as_ref().is_some_and(|last|!BlankClicks::near(last.point,message.pt)){clicks.last=None;}
    }
    if matches!(message.message,WM_RBUTTONDOWN|WM_MBUTTONDOWN|WM_XBUTTONDOWN|WM_MOUSEWHEEL|WM_MOUSEHWHEEL|WM_KEYDOWN|WM_SYSKEYDOWN|WM_CANCELMODE|WM_KILLFOCUS){clicks.last=None;}
    if !matches!(message.message,WM_LBUTTONDOWN|WM_LBUTTONUP|WM_LBUTTONDBLCLK){return false;}
    // Modified clicks belong to native multi-selection/chord behavior.
    if message.wParam.0&!1usize!=0{clicks.last=None;return false;}
    if text_input_window(message.hwnd){clicks.last=None;return false;}
    let captured=(||{
        let guard=slot.try_borrow().ok()?;let state=guard.as_ref()?;
        if !state.session.ui.double_click_blank_up{return None;}
        let(view,source)=state.file_view_for_input(message.hwnd)?;Some((state.hwnd,view,state.identity_for_source(source)?))
    })();
    let Some((owner,view,identity))=captured else{clicks.last=None;return false;};
    if message.message==WM_LBUTTONUP{clicks.release(owner,identity);return false;}
    let mut point=POINT{x:message.lParam.0 as i16 as i32,y:(message.lParam.0>>16)as i16 as i32};
    if !unsafe{windows::Win32::Graphics::Gdi::ClientToScreen(message.hwnd,&mut point)}.as_bool(){clicks.last=None;return false;}
    let Some(first)=clicks.pair(owner,identity,point,message.time,message.message==WM_LBUTTONDBLCLK)else{return false;};
    let background=|point:POINT|{
        let position=LPARAM(((point.x as u16 as u32)|((point.y as u16 as u32)<<16))as isize);
        zsdesk_windows::shell::menu::classify_view_menu_point(view,message.hwnd,position)==zsdesk_windows::shell::menu::ViewMenuTarget::Background
    };
    if !background(first)||!background(point){clicks.last=None;return false;}
    if !clicks.last.as_ref().is_some_and(|last|current_blank_context(owner)==Some(last.context)){clicks.last=None;return false;}
    post(owner,identity,Intent::Action(Command::Up.id()as usize))
}

#[cfg(test)]pub(super) fn native_blank_double_click(slot:&Slot,root:&std::path::Path,pump:fn(u64))->Result<()>{
    use anyhow::ensure;
    let parent=root.join("blank-parent");let nested=parent.join("nested");std::fs::create_dir_all(&nested)?;
    std::fs::write(nested.join("item.txt"),b"blank navigation fixture")?;
    let(original,enabled,all_files)={let state=slot.borrow();let state=state.as_ref().unwrap();(state.current_location().clone(),state.session.ui.double_click_blank_up,state.session.ui.search_all_files)};
    slot.borrow_mut().as_mut().unwrap().session.ui.search_all_files=false;
    let navigate=|location:ShellLocation|{let mut state=slot.borrow_mut();let state=state.as_mut().unwrap();unsafe{state.navigate_to(location);}};
    let ready=|expected:&ShellLocation|->Result<()>{let until=std::time::Instant::now()+std::time::Duration::from_secs(10);
        loop{pump(20);if slot.borrow().as_ref().is_some_and(|state|state.pending.is_none()&&state.queued.is_none()&&state.current_location()==expected){break;}
            if std::time::Instant::now()>=until{let state=slot.borrow();let state=state.as_ref().unwrap();anyhow::bail!("blank-click navigation did not settle: expected={}, actual={}, pending={:?}, queued={:?}, status={}",expected.address(),state.current_location().address(),state.pending.as_ref().map(|request|request.tab.current().address()),state.queued.as_ref().map(|request|request.tab.current().address()),state.status);}}Ok(())};
    for (flag,native) in [(false,false),(true,false),(false,true),(true,true)]{
        let target=ShellLocation::Path(nested.clone());navigate(target.clone());ready(&target)?;
        ensure!(slot.borrow().as_ref().unwrap().search_controls().scope.as_deref()==Some(nested.as_path()),"folder search still points to a previous directory");
        let view={let mut state=slot.borrow_mut();let state=state.as_mut().unwrap();state.session.ui.double_click_blank_up=flag;state.view_hwnd};
        let mut rect=RECT::default();unsafe{GetClientRect(view,&mut rect)?;}
        let point=POINT{x:rect.right-30,y:rect.bottom-30};
        let mut clicks=BlankClicks::default();
        let mut message=MSG{hwnd:view,message:WM_LBUTTONDOWN,time:100,lParam:LPARAM(((point.x as u16 as u32)|((point.y as u16 as u32)<<16))as isize),..Default::default()};
        ensure!(!blank_double_click(slot,&message,&mut clicks),"single click navigated");
        message.message=WM_LBUTTONUP;message.time=120;
        ensure!(!blank_double_click(slot,&message,&mut clicks),"button release navigated");
        message.message=if native{WM_LBUTTONDBLCLK}else{WM_LBUTTONDOWN};message.time=150;
        ensure!(blank_double_click(slot,&message,&mut clicks)==flag,"blank double-click setting not honored (native={native})");
        message.message=WM_LBUTTONDBLCLK;message.time=151;
        ensure!(!blank_double_click(slot,&message,&mut clicks),"duplicate double-click submitted a second navigation");
        if flag{ready(&ShellLocation::Path(parent.clone()))?;ensure!(slot.borrow().as_ref().unwrap().search_controls().scope.as_deref()==Some(parent.as_path()),"folder search scope did not follow parent navigation");}else{pump(50);ensure!(slot.borrow().as_ref().unwrap().current_location()==&target,"disabled double-click navigated");}
    }
    {let mut state=slot.borrow_mut();let state=state.as_mut().unwrap();state.session.ui.double_click_blank_up=enabled;state.session.ui.search_all_files=all_files;}
    navigate(original.clone());ready(&original)?;Ok(())
}

pub(super) fn identity(owner:HWND,source:usize)->Option<Identity> {
    let pointer=unsafe{GetWindowLongPtrW(owner,GWLP_USERDATA)}as *const Slot;
    if pointer.is_null(){return None;}
    unsafe{&*pointer}.try_borrow().ok().and_then(|slot|slot.as_ref().and_then(|state|state.identity_for_source(source)))
}

/// A single drop is one ticket: opening its first tab must not invalidate the
/// other paths from that same drop. Identity is captured before the OLE call.
pub(super) fn post_tab_paths(owner:HWND,identity:Identity,paths:Vec<PathBuf>)->bool{
    if paths.is_empty()||paths.len()>32{return false;}
    post(owner,identity,Intent::OpenFolders(paths))
}

fn take(owner:HWND,token:usize)->Option<Envelope>{
    let mut pending=PENDING.get()?.lock().unwrap_or_else(|e|e.into_inner());
    if pending.get(&token)?.owner!=owner.0 as isize{return None;}
    pending.remove(&token)
}

/// Run only pointer-menu envelopes before window_proc borrows WindowState.
/// Accessibility selection may pump messages; revalidate the same source/tab
/// after it returns instead of posting an unguarded second WM_CONTEXTMENU.
pub(super) fn dispatch_context_menu(owner:HWND,token:usize)->bool{
    let envelope={
        let Some(pending)=PENDING.get()else{return false;};
        let mut pending=pending.lock().unwrap_or_else(|error|error.into_inner());
        if !pending.get(&token).is_some_and(|item|item.owner==owner.0 as isize&&matches!(item.intent,Intent::ContextMenu(_))){return false;}
        pending.remove(&token).unwrap()
    };
    let Intent::ContextMenu(point)=envelope.intent else{return false;};
    let pointer=unsafe{GetWindowLongPtrW(owner,GWLP_USERDATA)}as*const Slot;
    if pointer.is_null(){return true;}
    let slot=unsafe{&*pointer};
    let captured=(||{
        let mut value=slot.try_borrow_mut().ok()?;let state=value.as_mut()?;
        if state.hwnd!=owner||state.identity_for_source(envelope.identity.source)!=Some(envelope.identity){return None;}
        if envelope.identity.source!=state.browser_source&&!state.activate_shell_source(envelope.identity.source){return None;}
        if state.session.active_tab().id!=envelope.identity.tab||state.pending.is_some()||state.queued.is_some(){return None;}
        Some((state.view_hwnd,state.identity_for_source(envelope.identity.source)?))
    })();
    let Some((window,identity))=captured.filter(|(window,_)|!window.0.is_null())else{return true;};
    let position=LPARAM(((point.x as u16 as u32)|((point.y as u16 as u32)<<16))as isize);
    let result=zsdesk_windows::shell::menu::prepare_pointer_menu(window,position);
    let current=||unsafe{IsWindow(Some(owner))}.as_bool()&&unsafe{GetWindowLongPtrW(owner,GWLP_USERDATA)}as*const Slot==pointer
        &&slot.try_borrow().ok().is_some_and(|value|value.as_ref().is_some_and(|state|
            state.hwnd==owner&&state.source_for_view(window)==Some(identity.source)&&state.identity_for_source(identity.source)==Some(identity)));
    if !current(){return true;}
    match result{
        Ok((view,target)) if matches!(target,zsdesk_windows::shell::menu::ViewMenuTarget::Selection|zsdesk_windows::shell::menu::ViewMenuTarget::Background)=>{
            let shown=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||super::context_menu::show(window,&view,window,position,target)))
                .unwrap_or_else(|_|Err(windows::core::Error::from_hresult(windows::core::HRESULT(0x8000ffffu32 as i32))));
            if let Err(error)=shown{zsdesk_windows::shell::menu::report_failure(window,error.code());}
        },
        Ok((_,zsdesk_windows::shell::menu::ViewMenuTarget::Unresolved))=>{
            zsdesk_windows::shell::menu::report_failure(window,windows::core::HRESULT(0x8000000au32 as i32));
        },
        Err(error)=>zsdesk_windows::shell::menu::report_failure(window,error.code()),
        _=>{},
    }
    true
}

pub(super) fn capture_folder_open(owner:HWND,source:usize,view:HWND)->Option<zsdesk_windows::shell::FolderOpenCallback> {
    let pointer=unsafe{GetWindowLongPtrW(owner,GWLP_USERDATA)}as *const Slot;
    if pointer.is_null(){return None;}
    let snapshot={let slot=unsafe{&*pointer}.try_borrow().ok()?;let state=slot.as_ref()?;
        if state.source_for_view(view)!=Some(source){return None;}
        state.identity_for_source(source)?
    };
    Some(Box::new(move|location|{
        if !post(owner,snapshot,Intent::NavigateFolder(location)) {
            diagnostics::record("navigation","native-open-mailbox-full",Some(0x800700aau32 as i32));
        }
    }))
}

impl WindowState {
    pub(super) fn identity_for_source(&self,source:usize)->Option<Identity> {
        if source==0{return None;}
        if source==self.browser_source {
            return self.session.tabs.get(self.session.active_tab).map(|tab|Identity{source,tab:tab.id,epoch:self.navigation_epoch});
        }
        if self.session.ui.dual_pane && source==self.other_source() {
            return self.other_identity();
        }
        None
    }

    pub(super) fn queue_pane_action(&self,source:usize,action:usize)->bool {
        self.identity_for_source(source).is_some_and(|identity|post(self.hwnd,identity,Intent::Action(action)))
    }

    pub(super) fn receive_pane_input(&mut self,token:usize) {
        let Some(envelope)=take(self.hwnd,token)else{return;};
        if self.identity_for_source(envelope.identity.source)!=Some(envelope.identity){return;}
        match envelope.intent {
            Intent::TransferMessage(message)=>self.show_message(message),
            Intent::RenameChild(path)=>{
                if envelope.identity.source!=self.browser_source{self.activate_pane(!self.active_pane_right);}
                // Activating the validated source pane advances its epoch;
                // retain the exact source/tab after that deliberate switch.
                if envelope.identity.source!=self.browser_source||self.session.active_tab().id!=envelope.identity.tab{return;}
                let (Some(parent),Some(name))=(path.parent(),path.file_name())else{return;};
                let location=ShellLocation::Path(parent.into());
                if !shell_registration::same_location(self.current_location(),&location){return;}
                self.defer_selection(vec![name.to_os_string()],true);
            },
            Intent::ContextCatalog(catalog)=>self.manage_context_catalog(catalog),
            Intent::ContextMenu(_)=>{}, // Dispatched before the WindowState borrow.
            Intent::Action(action)=>self.pane_action(envelope.identity.source,action),
            Intent::Wheel(delta)=>self.pane_wheel(envelope.identity.source,delta),
            Intent::OpenFolder(path)=>self.pane_open_folder(envelope.identity.source,path),
            Intent::OpenFolders(paths)=>self.pane_open_folders(envelope.identity.source,paths),
            Intent::NavigateFolder(location)=>self.pane_navigate_folder(envelope.identity.source,location),
            Intent::CreatedItem(item)=>{
                if envelope.identity.source==self.browser_source{self.select_created_item(item.path,item.rename);}
                else if let (Some(parent),Some(name))=(item.path.parent(),item.path.file_name()){
                    self.select_created_in_other_pane(&ShellLocation::Path(parent.into()),name);
                }
            },
            Intent::SubmitAddress(text)=>if envelope.identity.source==self.browser_source {
                address_input::delivered(self.address_edit);
                diagnostics::record("navigation","address-submit-dispatched",None);
                unsafe{self.commit_address_text(&text);}
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]fn native_and_directui_double_click_sequences_share_one_action(){
        let owner=HWND(1usize as*mut _);let identity=Identity{source:1,tab:2,epoch:3};let point=POINT{x:10,y:20};
        discard(owner);
        for native in [false,true]{let mut clicks=BlankClicks::default();
            assert!(clicks.pair(owner,identity,point,100,false).is_none());
            clicks.release(owner,identity);
            assert!(clicks.pair(owner,identity,point,150,native).is_some());
            assert!(clicks.pair(owner,identity,point,151,true).is_none());
            assert!(clicks.pair(owner,identity,point,152,true).is_none());
        }
        let mut clicks=BlankClicks::default();clicks.pair(owner,identity,point,100,false);
        clicks.release(owner,identity);
        assert!(clicks.pair(owner,Identity{epoch:4,..identity},point,150,false).is_none());
        clicks.release(owner,Identity{epoch:4,..identity});
        assert!(clicks.pair(owner,identity,POINT{x:100,y:20},151,false).is_none());
        clicks.release(owner,identity);
        assert!(clicks.pair(owner,identity,point,150+unsafe{GetDoubleClickTime()},false).is_none());
        let mut clicks=BlankClicks::default();assert!(clicks.pair(owner,identity,point,10,true).is_none());
        clicks.pair(owner,identity,point,100,false);
        assert!(clicks.pair(owner,identity,point,120,false).is_none(),"down without release cannot form a double click");
        let mut clicks=BlankClicks::default();clicks.pair(owner,identity,point,100,false);clicks.release(owner,identity);
        assert!(clicks.pair(owner,identity,point,101+unsafe{GetDoubleClickTime()},false).is_none(),"slow clicks navigated");
        discard(owner);
    }
    #[test]
    fn tab_path_ticket_is_owned_single_use_and_discarded_after_window_close(){
        let owner=unsafe{CreateWindowExW(WINDOW_EX_STYLE::default(),windows::core::w!("STATIC"),None,WS_POPUP,0,0,8,8,None,None,None,None)}.unwrap();
        let mut state=WindowState::new(ExplorerSession::default(),PathBuf::new(),None,false);state.hwnd=owner;state.browser_source=17;
        let expected=PathBuf::from(r"E:\中文 空格\folder");
        let identity=state.identity_for_source(17).unwrap();
        let batch=vec![expected.clone(),PathBuf::from(r"E:\second"),expected];
        assert!(post_tab_paths(owner,identity,batch.clone()));
        let mut message=MSG::default();
        assert!(unsafe{PeekMessageW(&mut message,Some(owner),MESSAGE,MESSAGE,PM_REMOVE)}.as_bool());
        assert_eq!(message.lParam.0,0);
        assert!(take(HWND::default(),message.wParam.0).is_none(),"another window consumed the request");
        let envelope=take(owner,message.wParam.0).unwrap();
        assert_eq!(envelope.identity.source,17);
        assert!(matches!(envelope.intent,Intent::OpenFolders(paths)if paths==batch));
        assert!(take(owner,message.wParam.0).is_none(),"duplicate delivery reused the payload");
        assert!(post_tab_paths(owner,identity,batch));
        assert!(unsafe{PeekMessageW(&mut message,Some(owner),MESSAGE,MESSAGE,PM_REMOVE)}.as_bool());
        discard(owner);assert!(take(owner,message.wParam.0).is_none());
        unsafe{let _=DestroyWindow(owner);}
    }
    #[test]
    fn stale_open_folder_ticket_does_not_change_the_current_tab(){
        let owner=unsafe{CreateWindowExW(WINDOW_EX_STYLE::default(),windows::core::w!("STATIC"),None,WS_POPUP,0,0,8,8,None,None,None,None)}.unwrap();
        let mut state=WindowState::new(ExplorerSession::default(),PathBuf::new(),None,false);state.hwnd=owner;state.browser_source=19;
        let identity=state.identity_for_source(19).unwrap();let tabs=state.session.tabs.len();
        assert!(post(owner,identity,Intent::OpenFolder(PathBuf::from(r"E:\delayed"))));
        let mut message=MSG::default();assert!(unsafe{PeekMessageW(&mut message,Some(owner),MESSAGE,MESSAGE,PM_REMOVE)}.as_bool());
        state.navigation_epoch+=1;state.receive_pane_input(message.wParam.0);state.receive_pane_input(message.wParam.0);
        assert_eq!(state.session.tabs.len(),tabs);assert!(take(owner,message.wParam.0).is_none());
        unsafe{let _=DestroyWindow(owner);}
    }
    #[test]
    fn rename_ticket_requires_the_exact_live_folder_and_epoch(){
        let owner=unsafe{CreateWindowExW(WINDOW_EX_STYLE::default(),windows::core::w!("STATIC"),None,WS_POPUP,0,0,8,8,None,None,None,None)}.unwrap();
        let mut session=ExplorerSession::default();session.tabs[0].replace_current(ShellLocation::Path(r"E:\source".into()));
        let mut state=WindowState::new(session,PathBuf::new(),None,false);state.hwnd=owner;state.browser_source=19;
        let identity=state.identity_for_source(19).unwrap();let mut message=MSG::default();
        for (path,accepted) in [(r"E:\different\target.txt",false),(r"E:\source\target.txt",true)]{
            assert!(post(owner,identity,Intent::RenameChild(path.into())));
            assert!(unsafe{PeekMessageW(&mut message,Some(owner),MESSAGE,MESSAGE,PM_REMOVE)}.as_bool());
            state.receive_pane_input(message.wParam.0);assert_eq!(state.deferred_selection.is_some(),accepted);state.deferred_selection=None;
        }
        assert!(post(owner,identity,Intent::RenameChild(r"E:\source\target.txt".into())));
        assert!(unsafe{PeekMessageW(&mut message,Some(owner),MESSAGE,MESSAGE,PM_REMOVE)}.as_bool());
        state.navigation_epoch+=1;state.receive_pane_input(message.wParam.0);assert!(state.deferred_selection.is_none());
        discard(owner);unsafe{let _=DestroyWindow(owner);}
    }
    #[test]
    fn stale_tab_and_navigation_inputs_are_rejected_without_consuming_new_state() {
        let mut state=WindowState::new(ExplorerSession::default(),PathBuf::new(),None,false);state.browser_source=17;
        let first=state.identity_for_source(17).unwrap();
        state.session.tabs.push(ExplorerTab::new(2,ShellLocation::ThisPc));state.session.active_tab=1;
        assert_ne!(state.identity_for_source(17),Some(first));
        state.session.active_tab=0;state.navigation_epoch+=1;
        assert_ne!(state.identity_for_source(17),Some(first));
        assert!(state.identity_for_source(18).is_none());
    }
}

#[cfg(test)]#[path="input_compat_tests.rs"]mod input_compat_tests;
