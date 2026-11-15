use super::*;
use anyhow::ensure;
use super::super::mouse_gestures::MouseGestures;

#[derive(Default)]struct Messages{buttons:Cell<u32>,menus:Cell<u32>}
unsafe extern "system" fn no_synthetic_menu(hwnd:HWND,message:u32,wp:WPARAM,lp:LPARAM,id:usize,data:usize)->LRESULT{
    let counts=unsafe{&*(data as*const Messages)};
    if matches!(message,WM_RBUTTONDOWN|WM_RBUTTONUP){counts.buttons.set(counts.buttons.get()+1);return LRESULT(0);}
    if message==WM_CONTEXTMENU{counts.menus.set(counts.menus.get()+1);return LRESULT(0);}
    if message==WM_NCDESTROY{let _=unsafe{RemoveWindowSubclass(hwnd,Some(no_synthetic_menu),id)};}
    unsafe{DefSubclassProc(hwnd,message,wp,lp)}
}
struct Fixture{owner:HWND,view:HWND,slot:Box<Slot>,messages:Box<Messages>}
impl Drop for Fixture{fn drop(&mut self){
    discard(self.owner);
    unsafe{if GetCapture()==self.owner{let _=ReleaseCapture();}SetWindowLongPtrW(self.owner,GWLP_USERDATA,0);let _=DestroyWindow(self.owner);}
}}
impl Fixture{
    fn new()->Result<Self>{
        unsafe{InitCommonControlsEx(&INITCOMMONCONTROLSEX{dwSize:size_of::<INITCOMMONCONTROLSEX>()as u32,dwICC:ICC_LISTVIEW_CLASSES})}.ok()?;
        let owner=unsafe{CreateWindowExW(WS_EX_TOOLWINDOW|WS_EX_NOACTIVATE,windows::core::w!("STATIC"),windows::core::w!("owned input fixture"),WS_POPUP,-30000,-30000,500,320,None,None,None,None)}?;
        let mut state=WindowState::new(ExplorerSession::default(),std::env::temp_dir().join(format!("zsdesk-input-fixture-{}.json",std::process::id())),None,false);
        state.hwnd=owner;state.browser_source=17;state.session.ui.mouse_gestures=true;state.session.ui.double_click_blank_up=true;
        let mut fixture=Self{owner,view:HWND::default(),slot:Box::new(RefCell::new(Some(state))),messages:Box::new(Messages::default())};
        fixture.view=unsafe{CreateWindowExW(WINDOW_EX_STYLE::default(),windows::core::w!("SysListView32"),None,
            WS_CHILD|WS_VISIBLE|WINDOW_STYLE(LVS_REPORT),10,10,450,260,Some(owner),None,None,None)}?;
        let column=LVCOLUMNW{mask:LVCF_WIDTH,cx:350,..Default::default()};
        ensure!(unsafe{SendMessageW(fixture.view,LVM_INSERTCOLUMNW,None,Some(LPARAM((&column as*const LVCOLUMNW)as isize)))}.0==0,"fixture column");
        let mut text=wide("file.txt");let item=LVITEMW{mask:LVIF_TEXT,pszText:PWSTR(text.as_mut_ptr()),..Default::default()};
        ensure!(unsafe{SendMessageW(fixture.view,LVM_INSERTITEMW,None,Some(LPARAM((&item as*const LVITEMW)as isize)))}.0==0,"fixture file row");
        fixture.slot.borrow_mut().as_mut().unwrap().view_hwnd=fixture.view;
        ensure!(unsafe{SetWindowSubclass(fixture.view,Some(no_synthetic_menu),1,(&*fixture.messages as*const Messages)as usize)}.as_bool(),"fixture subclass");
        unsafe{SetWindowLongPtrW(owner,GWLP_USERDATA,(&*fixture.slot as*const Slot)as isize);let _=ShowWindow(owner,SW_SHOWNOACTIVATE);}
        Ok(fixture)
    }
    fn event(&self,message:u32,time:u32,keys:usize,point:POINT)->Result<MSG>{
        let mut screen=point;unsafe{ClientToScreen(self.view,&mut screen)}.ok()?;
        Ok(MSG{hwnd:self.view,message,wParam:WPARAM(keys),lParam:LPARAM(((point.x as u16 as u32)|((point.y as u16 as u32)<<16))as isize),time,pt:screen,..Default::default()})
    }
    fn ticket(&self)->Option<MSG>{let mut message=MSG::default();unsafe{PeekMessageW(&mut message,Some(self.owner),MESSAGE,MESSAGE,PM_REMOVE)}.as_bool().then_some(message)}
}

#[test]
fn gesture_click_posts_one_context_ticket_without_native_button_replay()->Result<()>{
    let fixture=Fixture::new()?;let mut gestures=MouseGestures::default();let point=POINT{x:50,y:180};
    let down=fixture.event(WM_RBUTTONDOWN,100,2,point)?;let up=fixture.event(WM_RBUTTONUP,150,0,point)?;
    ensure!(gestures.process(fixture.owner,&down,&fixture.slot),"gesture did not retain down");
    ensure!(gestures.process(fixture.owner,&up,&fixture.slot),"ordinary right click was not completed");
    let message=fixture.ticket().context("pointer menu ticket missing when child does not synthesize menus")?;
    let envelope=take(fixture.owner,message.wParam.0).context("owned pointer request missing")?;
    ensure!(matches!(envelope.intent,Intent::ContextMenu(actual)if actual==down.pt),"wrong pointer target");
    ensure!(fixture.ticket().is_none(),"one right click generated duplicate requests");
    ensure!(!gestures.process(fixture.owner,&up,&fixture.slot)&&fixture.ticket().is_none(),"duplicate release submitted another request");
    ensure!(fixture.messages.buttons.get()==0&&fixture.messages.menus.get()==0,"right click depended on synthesized native mouse/menu messages");
    Ok(())
}

#[test]
fn context_ticket_rejects_changed_navigation_and_does_not_consume_other_inputs()->Result<()>{
    let fixture=Fixture::new()?;
    let identity=fixture.slot.borrow().as_ref().unwrap().identity_for_source(17).unwrap();
    ensure!(post(fixture.owner,identity,Intent::ContextMenu(POINT{x:-29900,y:-29900})),"post context");
    let message=fixture.ticket().context("context ticket")?;
    fixture.slot.borrow_mut().as_mut().unwrap().navigation_epoch+=1;
    ensure!(dispatch_context_menu(fixture.owner,message.wParam.0),"context ticket not consumed");
    ensure!(!dispatch_context_menu(fixture.owner,message.wParam.0),"context ticket was replayed");
    ensure!(fixture.messages.menus.get()==0,"stale pointer request reached the new view");
    let current=fixture.slot.borrow().as_ref().unwrap().identity_for_source(17).unwrap();
    ensure!(post(fixture.owner,current,Intent::Action(Command::Up.id()as usize)),"post action");
    let message=fixture.ticket().context("action ticket")?;
    ensure!(!dispatch_context_menu(fixture.owner,message.wParam.0),"pointer dispatch consumed a keyboard/action ticket");
    ensure!(matches!(take(fixture.owner,message.wParam.0).map(|entry|entry.intent),Some(Intent::Action(_))),"action ticket lost");
    Ok(())
}

#[test]
fn gesture_header_edit_and_changed_source_keep_their_native_contract()->Result<()>{
    let fixture=Fixture::new()?;let mut gestures=MouseGestures::default();let point=POINT{x:60,y:180};
    let down=fixture.event(WM_RBUTTONDOWN,100,2,point)?;let up=fixture.event(WM_RBUTTONUP,130,0,point)?;
    ensure!(gestures.process(fixture.owner,&down,&fixture.slot),"first down");
    fixture.slot.borrow_mut().as_mut().unwrap().navigation_epoch+=1;
    ensure!(gestures.process(fixture.owner,&up,&fixture.slot)&&fixture.ticket().is_none(),"changed navigation opened a menu");
    let header=HWND(unsafe{SendMessageW(fixture.view,LVM_GETHEADER,None,None)}.0 as*mut _);
    let mut screen=POINT{x:25,y:5};unsafe{ClientToScreen(header,&mut screen)}.ok()?;
    let header_down=MSG{hwnd:header,message:WM_RBUTTONDOWN,wParam:WPARAM(2),pt:screen,..Default::default()};
    ensure!(!gestures.process(fixture.owner,&header_down,&fixture.slot),"column header right click was captured");
    let edit=unsafe{CreateWindowExW(WINDOW_EX_STYLE::default(),windows::core::w!("EDIT"),None,WS_CHILD|WS_VISIBLE,280,110,100,24,Some(fixture.view),None,None,None)}?;
    let mut screen=POINT{x:10,y:10};unsafe{ClientToScreen(edit,&mut screen)}.ok()?;
    let edit_down=MSG{hwnd:edit,message:WM_RBUTTONDOWN,wParam:WPARAM(2),pt:screen,..Default::default()};
    ensure!(!gestures.process(fixture.owner,&edit_down,&fixture.slot),"text editor right click was captured");
    ensure!(fixture.ticket().is_none(),"native controls generated file actions");Ok(())
}

#[test]
fn raw_blank_click_pair_is_single_use_and_drag_or_disabled_mode_never_navigates()->Result<()>{
    let fixture=Fixture::new()?;let point=POINT{x:60,y:180};
    for native in [false,true]{
        let mut clicks=BlankClicks::default();
        ensure!(!blank_double_click(&fixture.slot,&fixture.event(WM_LBUTTONDOWN,100,1,point)?,&mut clicks),"single click navigated");
        ensure!(!blank_double_click(&fixture.slot,&fixture.event(WM_LBUTTONUP,120,0,point)?,&mut clicks),"release navigated");
        ensure!(blank_double_click(&fixture.slot,&fixture.event(if native{WM_LBUTTONDBLCLK}else{WM_LBUTTONDOWN},150,1,point)?,&mut clicks),"pair did not navigate");
        let message=fixture.ticket().context("parent-navigation request missing")?;
        ensure!(matches!(take(fixture.owner,message.wParam.0).map(|entry|entry.intent),Some(Intent::Action(action))if action==Command::Up.id()as usize),"wrong double-click action");
        ensure!(!blank_double_click(&fixture.slot,&fixture.event(WM_LBUTTONDBLCLK,151,1,point)?,&mut clicks)&&fixture.ticket().is_none(),"duplicate DBLCLK navigated twice");
    }
    let mut clicks=BlankClicks::default();
    blank_double_click(&fixture.slot,&fixture.event(WM_LBUTTONDOWN,100,1,point)?,&mut clicks);
    blank_double_click(&fixture.slot,&fixture.event(WM_MOUSEMOVE,110,1,POINT{x:point.x+40,..point})?,&mut clicks);
    blank_double_click(&fixture.slot,&fixture.event(WM_LBUTTONUP,120,0,point)?,&mut clicks);
    ensure!(!blank_double_click(&fixture.slot,&fixture.event(WM_LBUTTONDOWN,150,1,point)?,&mut clicks)&&fixture.ticket().is_none(),"marquee drag became a double click");
    fixture.slot.borrow_mut().as_mut().unwrap().session.ui.double_click_blank_up=false;
    blank_double_click(&fixture.slot,&fixture.event(WM_LBUTTONUP,160,0,point)?,&mut clicks);
    ensure!(!blank_double_click(&fixture.slot,&fixture.event(WM_LBUTTONDBLCLK,180,1,point)?,&mut clicks)&&fixture.ticket().is_none(),"disabled double click navigated");
    fixture.slot.borrow_mut().as_mut().unwrap().session.ui.double_click_blank_up=true;
    for (message,time,keys) in [(WM_LBUTTONDOWN,200,9),(WM_LBUTTONUP,220,8),(WM_LBUTTONDBLCLK,250,9)]{
        ensure!(!blank_double_click(&fixture.slot,&fixture.event(message,time,keys,point)?,&mut clicks),"modified selection click navigated");
    }
    ensure!(fixture.ticket().is_none(),"modified clicks queued parent navigation");
    let mut clicks=BlankClicks::default();
    blank_double_click(&fixture.slot,&fixture.event(WM_LBUTTONDOWN,300,1,point)?,&mut clicks);
    blank_double_click(&fixture.slot,&fixture.event(WM_LBUTTONUP,320,0,point)?,&mut clicks);
    invalidate_blank_clicks(fixture.owner);
    ensure!(!blank_double_click(&fixture.slot,&fixture.event(WM_LBUTTONDOWN,350,1,point)?,&mut clicks)&&fixture.ticket().is_none(),"clicks across deactivation were paired");
    discard(fixture.owner);
    ensure!(!BLANK_CONTEXTS.get().unwrap().lock().unwrap().contains_key(&(fixture.owner.0 as isize)),"closed/discarded window retained click context");
    Ok(())
}
