//! Full native Shell/window interaction regression on an unswitched desktop.
use super::*;
use anyhow::ensure;
use std::{ffi::c_void,fs,ptr,sync::{Arc,atomic::{AtomicBool,Ordering}},thread,time::{Duration,Instant}};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW,MonitorFromWindow,MONITORINFO,MONITOR_DEFAULTTONEAREST};
#[link(name="user32")] unsafe extern "system" {
    fn CreateDesktopW(name:*const u16,device:*const u16,mode:*const c_void,flags:u32,access:u32,attributes:*const c_void)->*mut c_void;
    fn SetThreadDesktop(desktop:*mut c_void)->i32;
    fn CloseDesktop(desktop:*mut c_void)->i32;
    fn GetThreadDesktop(thread:u32)->*mut c_void;
}
struct Apartment;
impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}
struct OwnedWindow(HWND);
impl Drop for OwnedWindow{fn drop(&mut self){let _=unsafe{DestroyWindow(self.0)};}}
struct Accelerators(HACCEL);
impl Drop for Accelerators{fn drop(&mut self){let _=unsafe{DestroyAcceleratorTable(self.0)};}}
struct PaletteWindow{hwnd:HWND,slot:Box<Slot>}
impl Drop for PaletteWindow{fn drop(&mut self){let _=unsafe{DestroyWindow(self.hwnd)};}}
fn palette_window(output:&std::path::Path,name:&str)->Result<PaletteWindow>{
    let slot=Box::new(RefCell::new(Some(WindowState::new(storage::default_session(),output.join(format!("palette-{name}.json")),None,false))));
    let class=wide("ZSDesk.Native.InteractionRegression");let instance=HINSTANCE(unsafe{GetModuleHandleW(None)?}.0);
    let hwnd=unsafe{CreateWindowExW(WS_EX_NOACTIVATE,pcwstr(&class),windows::core::w!("palette isolation"),WS_POPUP,
        0,0,64,32,None,None,Some(instance),Some((&*slot as*const Slot).cast()))?};
    slot.borrow_mut().as_mut().unwrap().hwnd=hwnd;Ok(PaletteWindow{hwnd,slot})
}
fn edit_palette_isolation(output:&std::path::Path)->Result<()>{
    use windows::Win32::Graphics::Gdi::{GetBkColor,GetTextColor,GetObjectW,LOGBRUSH,GetDC,ReleaseDC};
    let first=palette_window(output,"first")?;let second=palette_window(output,"second")?;
    let edit_a=unsafe{child(first.hwnd,"EDIT","first",ID_ADDRESS,ES_AUTOHSCROLL as u32,false)}?;
    let edit_b=unsafe{child(second.hwnd,"EDIT","second",ID_ADDRESS,ES_AUTOHSCROLL as u32,false)}?;
    ensure!(replace_edit_colors(first.hwnd,0x123456,0x234567,0x345678)&&replace_edit_colors(second.hwnd,0x456789,0x56789a,0x6789ab),"palette brush allocation failed");
    let check=|owner:HWND,edit:HWND,background:u32,text:u32|->Result<isize>{
        let dc=unsafe{GetDC(Some(edit))};ensure!(!dc.0.is_null(),"edit DC missing");
        let brush=unsafe{SendMessageW(owner,WM_CTLCOLOREDIT,Some(WPARAM(dc.0 as usize)),Some(LPARAM(edit.0 as isize)))}.0;
        let actual_bg=unsafe{GetBkColor(dc)}.0;let actual_text=unsafe{GetTextColor(dc)}.0;
        unsafe{ReleaseDC(Some(edit),dc);}
        ensure!(actual_bg==background&&actual_text==text&&brush!=0,"edit colors leaked across windows");Ok(brush)
    };
    check(first.hwnd,edit_a,0x123456,0x345678)?;
    let second_brush=check(second.hwnd,edit_b,0x456789,0x6789ab)?;
    ensure!(replace_edit_colors(first.hwnd,0x112233,0x223344,0x334455),"palette replacement failed");
    ensure!(check(second.hwnd,edit_b,0x456789,0x6789ab)?==second_brush,"updating one window replaced another's brush");
    let first_borrow=first.slot.borrow_mut();
    unsafe{SendMessageW(first.hwnd,WM_PAINT,None,None);}
    {
        let _second_borrow=second.slot.borrow_mut();unsafe{SendMessageW(second.hwnd,WM_PAINT,None,None);}
    }
    ensure!(!unsafe{GetPropW(first.hwnd,DEFERRED_PAINT_PROPERTY)}.0.is_null()&&!unsafe{GetPropW(second.hwnd,DEFERRED_PAINT_PROPERTY)}.0.is_null(),"nested paint did not mark each window");
    unsafe{SendMessageW(second.hwnd,WM_NULL,None,None);}
    ensure!(!unsafe{GetPropW(first.hwnd,DEFERRED_PAINT_PROPERTY)}.0.is_null()&&unsafe{GetPropW(second.hwnd,DEFERRED_PAINT_PROPERTY)}.0.is_null(),"painting one window consumed another's deferred invalidation");
    drop(first_borrow);unsafe{SendMessageW(first.hwnd,WM_NULL,None,None);}
    ensure!(unsafe{GetPropW(first.hwnd,DEFERRED_PAINT_PROPERTY)}.0.is_null(),"deferred paint was not consumed by its owner");
    drop(first);
    let mut brush_info=LOGBRUSH::default();
    ensure!(unsafe{GetObjectW(HBRUSH(second_brush as*mut _).into(),size_of::<LOGBRUSH>()as i32,Some((&mut brush_info as*mut LOGBRUSH).cast()))}>0
        &&brush_info.lbColor.0==0x456789,"destroying one window deleted the other window's GDI brush");
    check(second.hwnd,edit_b,0x456789,0x6789ab)?;drop(second);Ok(())
}

fn last_tab_close_policy(output:&std::path::Path)->Result<()> {
    let runner=palette_window(output,"last-tab")?;
    unsafe{SetWindowPos(runner.hwnd,None,140,120,800,540,SWP_NOACTIVATE|SWP_NOZORDER)?;runner.slot.borrow_mut().as_mut().unwrap().create(runner.hwnd)?;}
    let ready=||->Result<()> {
        let end=Instant::now()+Duration::from_secs(12);
        loop{pump(25);if runner.slot.borrow().as_ref().is_some_and(|state|state.pending.is_none()&&state.queued.is_none()){return Ok(());}
            ensure!(Instant::now()<end,"last-tab fixture navigation timed out");}
    };
    ready()?;
    let old_id=runner.slot.borrow().as_ref().unwrap().session.active_tab().id;
    {
        let mut slot=runner.slot.borrow_mut();let state=slot.as_mut().unwrap();state.session.ui.close_window_on_last_tab=false;
        unsafe{state.close_tab(0);}
    }
    ready()?;
    ensure!(unsafe{IsWindow(Some(runner.hwnd))}.as_bool(),"disabled last-tab-close still closed the window");
    ensure!(runner.slot.borrow().as_ref().unwrap().session.active_tab().id!=old_id,"disabled last-tab-close did not retain a fresh tab");
    let close={let mut slot=runner.slot.borrow_mut();let state=slot.as_mut().unwrap();state.session.ui.close_window_on_last_tab=true;
        state.layout.tabs.iter().find(|tab|tab.index==state.session.active_tab).context("last tab close bounds")?.close};
    let x=close.x+close.width/2;let y=close.y+close.height/2;let packed=LPARAM(((x as u16 as u32)|((y as u16 as u32)<<16))as isize);
    unsafe{SendMessageW(runner.hwnd,WM_LBUTTONDOWN,Some(WPARAM(1)),Some(packed));SendMessageW(runner.hwnd,WM_LBUTTONUP,Some(WPARAM(0)),Some(packed));}
    pump(100);ensure!(!unsafe{IsWindow(Some(runner.hwnd))}.as_bool(),"closing the last visible tab did not close the file window");
    Ok(())
}
fn pump(ms:u64){
    let end=Instant::now()+Duration::from_millis(ms);
    while Instant::now()<end{
        let mut msg=MSG::default();
        for _ in 0..256{
            if !unsafe{PeekMessageW(&mut msg,None,0,0,PM_REMOVE)}.as_bool(){break;}
            if msg.message!=WM_QUIT{unsafe{let _=TranslateMessage(&msg);DispatchMessageW(&msg);}}
        }
        thread::sleep(Duration::from_millis(3));
    }
}
fn rectangle(hwnd:HWND)->Result<RECT>{let mut r=RECT::default();unsafe{GetWindowRect(hwnd,&mut r)?;}Ok(r)}
thread_local!{static CAPTION_TRACE:RefCell<Vec<serde_json::Value>>=const{RefCell::new(Vec::new())};}
unsafe extern "system" fn trace_caption(hwnd:HWND,msg:u32,wp:WPARAM,lp:LPARAM,_id:usize,_data:usize)->LRESULT{
    let selected=matches!(msg,WM_NCHITTEST|WM_NCLBUTTONDOWN|WM_NCLBUTTONUP|WM_LBUTTONUP|WM_CAPTURECHANGED|WM_CANCELMODE|WM_SYSCOMMAND);
    let record=|phase:&str,result:Option<isize>|{
        CAPTION_TRACE.with(|trace|{if let Ok(mut trace)=trace.try_borrow_mut(){if trace.len()<96{
            trace.push(serde_json::json!({"phase":phase,"message":msg,"wparam":wp.0,"lparam":lp.0,"result":result,
                "native_caption":native_caption(hwnd),"capture":unsafe{GetCapture()}.0 as usize,
                "caption_press":unsafe{GetPropW(hwnd,windows::core::w!("ZSDesk.CaptionPress"))}.0 as usize}));
        }}});
    };
    if selected{record("before",None);}
    let result=unsafe{DefSubclassProc(hwnd,msg,wp,lp)};
    if selected{record("after",Some(result.0));}result
}
struct CaptionTrace(HWND);
impl Drop for CaptionTrace{fn drop(&mut self){if unsafe{IsWindow(Some(self.0))}.as_bool(){let _=unsafe{RemoveWindowSubclass(self.0,Some(trace_caption),0x5a54)};}}}
fn caption_click(hwnd:HWND,index:usize)->Result<()>{
    CAPTION_TRACE.with(|trace|trace.borrow_mut().clear());
    unsafe{SetWindowSubclass(hwnd,Some(trace_caption),0x5a54,0)}.ok()?;let _trace=CaptionTrace(hwnd);
    let rect=caption_rects(hwnd).context("caption rectangles")?[index];
    let mut point=POINT{x:rect.x+rect.width/2,y:rect.y+rect.height/2};
    unsafe{ClientToScreen(hwnd,&mut point)}.ok()?;
    let packed=LPARAM((((point.x as u32)&0xffff)|(((point.y as u32)&0xffff)<<16))as isize);
    let hit=unsafe{SendMessageW(hwnd,WM_NCHITTEST,None,Some(packed))}.0 as u32;
    ensure!(hit==[HTMINBUTTON,HTMAXBUTTON,HTCLOSE][index],"painted caption is not clickable: {index} returned {hit}");
    let mut client=point;unsafe{ScreenToClient(hwnd,&mut client)}.ok()?;
    let client_packed=LPARAM((((client.x as u32)&0xffff)|(((client.y as u32)&0xffff)<<16))as isize);
    let done=Arc::new(AtomicBool::new(false));let expired=Arc::new(AtomicBool::new(false));
    let stopped=done.clone();let timed_out=expired.clone();let handle=hwnd.0 as usize;
    let desktop=unsafe{GetThreadDesktop(windows::Win32::System::Threading::GetCurrentThreadId())}as usize;
    ensure!(desktop!=0,"caption test desktop missing");
    let release=client_packed.0;
    let watchdog=thread::spawn(move||{
        if unsafe{SetThreadDesktop(desktop as*mut c_void)}==0{timed_out.store(true,Ordering::Release);return;}
        // The target can enter DWM's native modal tracking loop while dispatching
        // DOWN. Release is queued independently rather than waiting for it to return.
        for attempt in 0..30{
            if stopped.load(Ordering::Acquire){return;}
            if attempt==2{let _=unsafe{PostMessageW(Some(HWND(handle as*mut _)),WM_LBUTTONUP,WPARAM(0),LPARAM(release))};}
            thread::sleep(Duration::from_millis(20));
        }
        if !stopped.load(Ordering::Acquire){
            timed_out.store(true,Ordering::Release);
            let _=unsafe{PostMessageW(Some(HWND(handle as*mut _)),WM_CANCELMODE,WPARAM(0),LPARAM(0))};
            let _=unsafe{PostMessageW(Some(HWND(handle as*mut _)),WM_LBUTTONUP,WPARAM(0),LPARAM(release))};
        }
    });
    let queued=unsafe{PostMessageW(Some(hwnd),WM_NCLBUTTONDOWN,WPARAM(hit as usize),packed)
        .and_then(|_|PostMessageW(Some(hwnd),WM_LBUTTONUP,WPARAM(0),client_packed))};
    if let Err(error)=queued{done.store(true,Ordering::Release);let _=watchdog.join();return Err(error.into());}
    pump(150);done.store(true,Ordering::Release);let _=watchdog.join();
    let trace=CAPTION_TRACE.with(|trace|std::mem::take(&mut*trace.borrow_mut()));
    println!("{}",serde_json::json!({"caption_index":index,"hit":hit,"watchdog_expired":expired.load(Ordering::Acquire),"events":trace}));
    ensure!(!expired.load(Ordering::Acquire),"queued caption input did not complete before the native tracking deadline");Ok(())
}
fn custom_caption_capture(runner:HWND)->Result<()>{
    ensure!(!native_caption(runner),"capture regression requires the custom caption path");
    let bounds=caption_rects(runner).context("caption bounds")?[1];
    let inside=POINT{x:bounds.x+bounds.width/2,y:bounds.y+bounds.height/2};let mut screen=inside;
    unsafe{ClientToScreen(runner,&mut screen)}.ok()?;
    let pack=|point:POINT|LPARAM((((point.x as u32)&0xffff)|(((point.y as u32)&0xffff)<<16))as isize);
    unsafe{SendMessageW(runner,WM_NCLBUTTONDOWN,Some(WPARAM(HTMAXBUTTON as usize)),Some(pack(screen)));}
    ensure!(unsafe{GetCapture()}==runner&&caption_visual(runner)==(HTMAXBUTTON,true),"custom caption press did not capture");
    let outside=POINT{x:bounds.x-60,y:bounds.bottom()+40};
    unsafe{SendMessageW(runner,WM_MOUSEMOVE,Some(WPARAM(1)),Some(pack(outside)));}
    ensure!(caption_visual(runner)==(0,false),"dragging out left pressed feedback");
    unsafe{SendMessageW(runner,WM_MOUSEMOVE,Some(WPARAM(1)),Some(pack(inside)));}
    ensure!(caption_visual(runner)==(HTMAXBUTTON,true),"dragging back did not restore feedback");
    let peer=OwnedWindow(unsafe{CreateWindowExW(WS_EX_NOACTIVATE,windows::core::w!("STATIC"),windows::core::w!("caption state peer"),WS_POPUP,0,0,10,10,None,None,None,None)}?);
    track_caption_buttons(peer.0,WM_NCMOUSEMOVE,WPARAM(HTCLOSE as usize));
    ensure!(caption_visual(peer.0)==(HTCLOSE,false)&&caption_visual(runner)==(HTMAXBUTTON,true),"caption hover leaked between HWNDs");
    unsafe{SendMessageW(runner,WM_CANCELMODE,None,None);}
    ensure!(unsafe{GetCapture()}!=runner&&caption_visual(runner)==(0,false)
        &&unsafe{GetPropW(runner,windows::core::w!("ZSDesk.CaptionPress"))}.0.is_null(),"CancelMode retained capture or pressed state");
    unsafe{SendMessageW(runner,WM_LBUTTONUP,None,Some(pack(inside)));}
    pump(40);ensure!(window_actions::fullscreen(runner),"cancelled caption press executed restore");
    let _=unsafe{RemovePropW(peer.0,windows::core::w!("ZSDesk.CaptionVisual"))};Ok(())
}
fn key(hwnd:HWND,acc:HACCEL,key:VIRTUAL_KEY,ctrl:bool,shift:bool)->Result<()>{
    let mut original=[0u8;256];unsafe{GetKeyboardState(&mut original)?;}
    let mut state=[0u8;256];state[VK_CONTROL.0 as usize]=if ctrl{0x80}else{0};state[VK_SHIFT.0 as usize]=if shift{0x80}else{0};
    unsafe{SetKeyboardState(&state)?;}
    let message=MSG{hwnd,message:WM_KEYDOWN,wParam:WPARAM(key.0 as usize),lParam:LPARAM(1),..Default::default()};
    let handled=unsafe{TranslateAcceleratorW(hwnd,acc,&message)}!=0;
    unsafe{SetKeyboardState(&original)?;}
    ensure!(handled,"key {} was not bound",key.0);pump(100);Ok(())
}
fn run(output:&std::path::Path)->Result<serde_json::Value>{
    unsafe{OleInitialize(None)?;}let _apartment=Apartment;
    unsafe{InitCommonControlsEx(&INITCOMMONCONTROLSEX{dwSize:size_of::<INITCOMMONCONTROLSEX>()as u32,dwICC:ICC_STANDARD_CLASSES|ICC_BAR_CLASSES})}.ok()?;
    let files=output.join("files");fs::create_dir_all(&files)?;
    for name in ["中文 文件.txt","second.txt","third.txt"]{fs::write(files.join(name),b"Native regression fixture")?;}
    let mut session=storage::default_session();session.tabs=vec![ExplorerTab::new(1,ShellLocation::Path(files))];session.next_id=2;
    session.ui.show_tree=true;session.ui.show_preview=false;
    let config=output.join("profile/explorer-session.json");
    let slot=Box::new(RefCell::new(Some(WindowState::new(session,config.clone(),None,false))));
    let instance=HINSTANCE(unsafe{GetModuleHandleW(None)?}.0);let class=wide("ZSDesk.Native.InteractionRegression");
    unsafe{RegisterClassW(&WNDCLASSW{hInstance:instance,lpfnWndProc:Some(window_proc),lpszClassName:pcwstr(&class),..Default::default()});}
    let owner=OwnedWindow(unsafe{CreateWindowExW(WS_EX_NOACTIVATE,pcwstr(&class),windows::core::w!("Native interaction regression"),
        WS_OVERLAPPEDWINDOW|WS_CLIPCHILDREN,140,120,1080,720,None,None,Some(instance),Some((&*slot as*const Slot).cast()))?});
    let hwnd=owner.0;
    unsafe{SetWindowPos(hwnd,None,0,0,0,0,SWP_FRAMECHANGED|SWP_NOMOVE|SWP_NOSIZE|SWP_NOZORDER|SWP_NOACTIVATE)?;slot.borrow_mut().as_mut().unwrap().create(hwnd)?;let _=ShowWindow(hwnd,SW_SHOWNOACTIVATE);}
    let deadline=Instant::now()+Duration::from_secs(12);
    loop{pump(40);if slot.borrow().as_ref().is_some_and(|s|s.pending.is_none()&&s.shown_tab.is_some()){break;}ensure!(Instant::now()<deadline,"Shell navigation timed out");}
    edit_palette_isolation(output)?;
    last_tab_close_policy(output)?;
    let acc=Accelerators(unsafe{CreateAcceleratorTableW(&accelerator_table())?});
    let mode=||slot.borrow().as_ref().unwrap().browser.as_ref().unwrap().view_mode().unwrap();
    let normal=rectangle(hwnd)?;let mut frames=Vec::new();
    // Exercise notifications without modifying the user's Windows theme or
    // contrast settings. The borrowed-state case covers nested native pumps.
    for message in [WM_THEMECHANGED,WM_DWMCOMPOSITIONCHANGED,WM_SYSCOLORCHANGE]{
        unsafe{SendMessageW(hwnd,message,None,None);}pump(30);
    }
    {let _borrow=slot.borrow_mut();unsafe{SendMessageW(hwnd,WM_SETTINGCHANGE,Some(WPARAM(SPI_SETHIGHCONTRAST.0 as usize)),Some(LPARAM(0)));}}
    unsafe{PostMessageW(Some(hwnd),WM_NULL,WPARAM(0),LPARAM(0))?;}pump(40);
    ensure!(rectangle(hwnd)?==normal,"theme notifications changed window geometry");
    ensure!(native_caption(hwnd)==slot.borrow().as_ref().unwrap().mica,"theme notification left mismatched caption input/paint mode");
    for (w,h) in [(900,600),(1240,820),(1080,720)]{
        unsafe{SetWindowPos(hwnd,None,140,120,w,h,SWP_NOACTIVATE|SWP_NOZORDER)?;SendMessageW(hwnd,WM_EXITSIZEMOVE,None,None);}pump(150);
        let state=slot.borrow();let state=state.as_ref().unwrap();let view=state.browser.as_ref().unwrap().view_window().context("view HWND")?;
        let bounds=rectangle(view)?;let r=state.layout.browser;
        ensure!(bounds.right-bounds.left>100&&bounds.bottom-bounds.top>100,"Shell view collapsed during resize");
        ensure!(r.width>100&&r.height>100,"content layout collapsed");
        frames.push(serde_json::json!({"window":[w,h],"view":[bounds.right-bounds.left,bounds.bottom-bounds.top],"layout":[r.width,r.height]}));
    }
    ensure!(rectangle(hwnd)?==normal,"manual size roundtrip changed normal bounds");
    let saved=storage::load(&config)?.ui.window.context("saved window placement")?;
    ensure!(saved.width==1080&&saved.height==720,"window size was not persisted");
    ensure!(initial_rect(Some(saved))==(normal.left,normal.top,1080,720),"reopened geometry differs from saved size");
    for (hit,id) in [(HTLEFT,IDC_SIZEWE),(HTRIGHT,IDC_SIZEWE),(HTTOP,IDC_SIZENS),(HTBOTTOM,IDC_SIZENS),(HTTOPLEFT,IDC_SIZENWSE),(HTBOTTOMRIGHT,IDC_SIZENWSE),(HTTOPRIGHT,IDC_SIZENESW),(HTBOTTOMLEFT,IDC_SIZENESW),(HTCAPTION,IDC_ARROW)]{
        let result=unsafe{SendMessageW(hwnd,WM_SETCURSOR,Some(WPARAM(hwnd.0 as usize)),Some(LPARAM((hit|(WM_MOUSEMOVE<<16))as isize)))};
        ensure!(result.0==1&&unsafe{GetCursor()}==unsafe{LoadCursorW(None,id)?},"resize cursor missing for hit {hit}");
    }
    for _ in 0..3{
        caption_click(hwnd,1)?;
        ensure!(unsafe{IsZoomed(hwnd)}.as_bool(),"maximize failed");
        caption_click(hwnd,0)?;ensure!(unsafe{IsIconic(hwnd)}.as_bool(),"maximized minimize button failed");
        unsafe{let _=ShowWindow(hwnd,SW_RESTORE);}pump(100);
        caption_click(hwnd,1)?;
        ensure!(rectangle(hwnd)?==normal,"maximize/restore changed normal bounds");
    }
    key(hwnd,acc.0,VK_F11,false,false)?;custom_caption_capture(hwnd)?;caption_click(hwnd,0)?;
    ensure!(unsafe{IsIconic(hwnd)}.as_bool(),"fullscreen minimize button failed");
    unsafe{let _=ShowWindow(hwnd,SW_RESTORE);}pump(100);caption_click(hwnd,1)?;
    ensure!(!window_actions::fullscreen(hwnd)&&rectangle(hwnd)?==normal,"fullscreen caption restore failed");
    let mut monitor=MONITORINFO{cbSize:size_of::<MONITORINFO>()as u32,..Default::default()};
    unsafe{GetMonitorInfoW(MonitorFromWindow(hwnd,MONITOR_DEFAULTTONEAREST),&mut monitor)}.ok()?;
    for maximized in [false,true]{
        if maximized{unsafe{let _=ShowWindow(hwnd,SW_SHOWMAXIMIZED);}pump(80);}
        key(hwnd,acc.0,VK_F11,false,false)?;
        ensure!(window_actions::fullscreen(hwnd)&&rectangle(hwnd)?==monitor.rcMonitor,"F11 did not fill the monitor");
        key(hwnd,acc.0,VK_F11,false,false)?;
        ensure!(!window_actions::fullscreen(hwnd)&&unsafe{IsZoomed(hwnd)}.as_bool()==maximized,"F11 restore lost maximize state");
        if maximized{unsafe{let _=ShowWindow(hwnd,SW_RESTORE);}pump(100);}
        ensure!(rectangle(hwnd)?==normal,"fullscreen restore changed normal bounds");
    }
    for placement in [zsdesk_core::model::explorer::TabPlacement::Left,zsdesk_core::model::explorer::TabPlacement::Right,zsdesk_core::model::explorer::TabPlacement::Top]{
        {let mut state=slot.borrow_mut();let state=state.as_mut().unwrap();state.session.ui.tab_placement=placement;unsafe{state.resize();}}
        pump(100);let state=slot.borrow();let state=state.as_ref().unwrap();let actual=rectangle(state.browser.as_ref().unwrap().view_window().unwrap())?;
        ensure!(actual.right-actual.left>100&&actual.bottom-actual.top>100,"side tabs collapsed the native view");
        ensure!(rectangle(hwnd)?==normal,"tab placement moved the main window");
    }
    key(hwnd,acc.0,VK_0,true,false)?;ensure!(mode().1==48,"reset zoom");
    let view=slot.borrow().as_ref().unwrap().browser.as_ref().unwrap().view_window().unwrap();let bounds=rectangle(view)?;
    let packed=LPARAM((((bounds.left+30)as u32&0xffff)|(((bounds.top+60)as u32&0xffff)<<16))as isize);
    let wheel=|delta:i16,target:HWND|MSG{hwnd:target,message:WM_MOUSEWHEEL,wParam:WPARAM(((delta as u16 as usize)<<16)|8),lParam:packed,..Default::default()};
    for (delta,expected) in [(60,48),(60,64),(-120,48),(240,96),(-240,48)]{
        ensure!(slot.borrow_mut().as_mut().unwrap().wheel_zoom(&wheel(delta,view)),"wheel not consumed");pump(100);ensure!(mode().1==expected,"wheel {delta}: actual {:?}, expected {expected}",mode());
    }
    let edit=slot.borrow().as_ref().unwrap().address_edit;
    ensure!(!slot.borrow_mut().as_mut().unwrap().wheel_zoom(&wheel(120,edit)),"wheel hijacked text editor");
    let mut presets=Vec::new();
    for n in 1..=8u8{key(hwnd,acc.0,VIRTUAL_KEY(VK_0.0+n as u16),true,true)?;presets.push(mode());ensure!(mode().0==window_actions::view_preset(n).unwrap().0,"view preset {n} mismatch: {:?}",mode());}
    key(hwnd,acc.0,VK_L,true,false)?;ensure!(unsafe{GetFocus()}==edit,"Ctrl+L focus");
    key(hwnd,acc.0,VK_F6,false,false)?;ensure!(unsafe{GetFocus()}==slot.borrow().as_ref().unwrap().search_edit,"F6 search focus");
    key(hwnd,acc.0,VK_F6,false,true)?;ensure!(unsafe{GetFocus()}==edit,"Shift+F6 address focus");
    key(hwnd,acc.0,VK_T,true,false)?;pump(200);ensure!(slot.borrow().as_ref().unwrap().session.tabs.len()==2,"Ctrl+T tab");
    key(hwnd,acc.0,VK_W,true,false)?;pump(200);ensure!(slot.borrow().as_ref().unwrap().session.tabs.len()==1,"Ctrl+W tab");
    key(hwnd,acc.0,VK_T,true,true)?;pump(200);ensure!(slot.borrow().as_ref().unwrap().session.tabs.len()==2,"Ctrl+Shift+T reopen");
    let mut materials=Vec::new();
    if crate::edition::THEMED{
        use zsdesk_core::model::explorer::{BackdropStyle,ColorScheme};
        for (backdrop,scheme) in [(BackdropStyle::Solid,ColorScheme::Light),(BackdropStyle::Solid,ColorScheme::Dark),(BackdropStyle::Mica,ColorScheme::System),(BackdropStyle::Acrylic,ColorScheme::System)]{
            {let mut state=slot.borrow_mut();let state=state.as_mut().unwrap();state.session.ui.appearance.backdrop=backdrop;state.session.ui.appearance.color_scheme=scheme;state.apply_theme();state.theme_children();unsafe{state.resize();}}
            pump(80);let mut actual=0i32;
            let available=unsafe{windows::Win32::Graphics::Dwm::DwmGetWindowAttribute(hwnd,windows::Win32::Graphics::Dwm::DWMWA_SYSTEMBACKDROP_TYPE,(&mut actual as*mut i32).cast(),4)}.is_ok();
            if available{
                let expected=if !theme::transparency_available()||backdrop==BackdropStyle::Solid{1}else if backdrop==BackdropStyle::Acrylic{3}else{2};
                ensure!(actual==expected,"DWM did not apply requested material: {backdrop:?} {actual}");
            }else{ensure!(!slot.borrow().as_ref().unwrap().mica,"unsupported system did not fall back to solid");}
            materials.push(serde_json::json!({"backdrop":backdrop,"color":scheme,"dwm_supported":available,"dwm_value":actual}));
        }
    }
    let dual=dual_pane::native_exercise(&slot,output,pump)?;
    caption_click(hwnd,2)?;ensure!(!unsafe{IsWindow(Some(hwnd))}.as_bool(),"caption close failed");
    ensure!(storage::load(&config)?.ui.window==Some(saved),"closing changed saved normal geometry");
    let report=serde_json::json!({"dual_pane":dual,"caption_button_clicks":true,"all_resize_cursor_handles":true,"saved_geometry_reopens":true,"manual_resize":frames,"saved_size":[saved.width,saved.height],"maximize_restore_cycles":3,"fullscreen_normal_and_maximized":true,"materials":materials,
        "ctrl_wheel_high_resolution_and_direction":true,"wheel_text_edit_preserved":true,"view_presets":presets,"focus_cycle":true,"tab_shortcuts":true,"native_shell_loaded":true,"tab_positions":["left","right","top"],
        "caption_input":"queued down/up with bounded tracking watchdog","caption_cancel_releases_capture":true,"caption_drag_out_and_back":true,"caption_visual_per_hwnd":true,
        "theme_composition_systemcolor_notifications":true,"high_contrast_notification_reentrant":true,"windows_theme_preferences_changed":false,
        "edit_palette_per_hwnd":true,"owned_brush_teardown":true,"deferred_paint_per_hwnd":true,"last_tab_close_setting":true});
    drop(acc);drop(owner);pump(60);drop(slot);Ok(report)
}

#[test]
#[ignore="Full native window/Shell test on a private desktop; requires ZSDESK_NATIVE_TEST_OUTPUT"]
fn native_full_window_resize_zoom_and_shortcuts()->Result<()>{
    let output=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("set isolated output")?).join("interaction");
    ensure!(output.is_absolute(),"output must be absolute");fs::create_dir_all(&output)?;
    let foreground=unsafe{GetForegroundWindow()};let name=wide(&format!("ZSDesk.Interaction.{}",std::process::id()));
    let desktop=unsafe{CreateDesktopW(name.as_ptr(),ptr::null(),ptr::null(),0,0x000f00ff,ptr::null())};ensure!(!desktop.is_null(),"private desktop creation failed");
    let handle=desktop as usize;let destination=output.clone();
    let result=thread::spawn(move||->Result<serde_json::Value>{ensure!(unsafe{SetThreadDesktop(handle as*mut c_void)}!=0,"SetThreadDesktop failed");run(&destination)}).join().map_err(|_|anyhow::anyhow!("native test panicked")).and_then(|r|r);
    let closed=unsafe{CloseDesktop(desktop)}!=0;let current=unsafe{GetForegroundWindow()};let unchanged=foreground==current;let mut foreground_pid=0;
    unsafe{GetWindowThreadProcessId(current,Some(&mut foreground_pid));}
    // The user can switch applications while this private-desktop test runs.
    // Activation of this test process would violate isolation; unrelated
    // application switches are recorded without producing a false failure.
    let isolated=foreground_pid!=std::process::id();
    let report=serde_json::json!({"state":if result.is_ok()&&closed&&isolated{"passed"}else{"failed"},"checks":result.as_ref().map_err(|e|format!("{e:#}")),"desktop_closed":closed,"foreground_unchanged":unchanged,"foreground_pid":foreground_pid,"test_pid":std::process::id(),"test_did_not_activate":isolated});
    fs::write(output.join("verification.current.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");result?;ensure!(closed&&isolated,"desktop cleanup/focus failed");Ok(())
}

fn locked_tabs_and_bookmark_drag(output:&std::path::Path)->Result<serde_json::Value>{
    use zsdesk_core::model::explorer::Bookmark;
    unsafe{OleInitialize(None)?;}let _apartment=Apartment;
    unsafe{InitCommonControlsEx(&INITCOMMONCONTROLSEX{dwSize:size_of::<INITCOMMONCONTROLSEX>()as u32,dwICC:ICC_STANDARD_CLASSES|ICC_BAR_CLASSES})}.ok()?;
    let class=wide("ZSDesk.Native.InteractionRegression");let instance=HINSTANCE(unsafe{GetModuleHandleW(None)?}.0);
    unsafe{RegisterClassW(&WNDCLASSW{hInstance:instance,lpfnWndProc:Some(window_proc),lpszClassName:pcwstr(&class),..Default::default()});}
    let runner=palette_window(output,"locked-bookmarks")?;
    {
        let mut slot=runner.slot.borrow_mut();let state=slot.as_mut().unwrap();
        state.session.tabs=(1..=3).map(|id|ExplorerTab::new(id,ShellLocation::Path(output.into()))).collect();state.session.tabs[0].locked=true;
        let mut hidden=ExplorerTab::new(10,ShellLocation::ThisPc);hidden.locked=true;state.session.secondary_pane.tabs.push(hidden);
        state.session.bookmarks=vec![Bookmark::folder(20,"分类".into(),None),Bookmark::link(21,"甲".into(),ShellLocation::ThisPc,None),Bookmark::link(22,"乙".into(),ShellLocation::Path(output.into()),None)];
        state.session.next_id=23;
    }
    unsafe{SetWindowPos(runner.hwnd,None,0,0,900,600,SWP_NOACTIVATE|SWP_NOZORDER)?;runner.slot.borrow_mut().as_mut().unwrap().create(runner.hwnd)?;}
    let end=Instant::now()+Duration::from_secs(12);
    loop{pump(20);if runner.slot.borrow().as_ref().is_some_and(|s|s.pending.is_none()&&s.queued.is_none()){break;}ensure!(Instant::now()<end,"lock fixture navigation timed out");}
    let acc=Accelerators(unsafe{CreateAcceleratorTableW(&accelerator_table())?});key(runner.hwnd,acc.0,VK_W,true,false)?;
    {
        let mut slot=runner.slot.borrow_mut();let state=slot.as_mut().unwrap();
        let tab=state.layout.tabs.iter().find(|tab|tab.index==0).context("locked tab rect")?.rect;
        state.middle_click(POINT{x:tab.x+tab.width/2,y:tab.y+tab.height/2});state.double_click(POINT{x:tab.x+tab.width/2,y:tab.y+tab.height/2});
        ensure!(state.session.tabs.len()==3&&state.session.tabs[0].locked,"shortcut/middle/double click closed a locked tab");
        unsafe{state.close_group(1,false);}ensure!(state.session.tabs.iter().map(|tab|tab.id).collect::<Vec<_>>()==vec![1,2],"close others removed a locked tab");
        unsafe{state.close_group(0,true);}ensure!(state.session.tabs.len()==1&&state.session.tabs[0].locked,"close right removed a locked tab");
    }
    for (message,wp) in [(WM_CLOSE,0usize),(WM_SYSCOMMAND,SC_CLOSE as usize)]{unsafe{SendMessageW(runner.hwnd,message,Some(WPARAM(wp)),None);}ensure!(unsafe{IsWindow(Some(runner.hwnd))}.as_bool(),"window close bypassed a tab lock");}
    {runner.slot.borrow_mut().as_mut().unwrap().session.tabs[0].locked=false;}
    unsafe{SendMessageW(runner.hwnd,WM_CLOSE,None,None);}ensure!(unsafe{IsWindow(Some(runner.hwnd))}.as_bool(),"hidden pane lock was ignored");
    ensure!(unsafe{SendMessageW(runner.hwnd,WM_QUERYENDSESSION,None,None)}.0==1,"tab lock blocked OS session shutdown");
    let rect=|id:u64|->Result<LayoutRect>{let slot=runner.slot.borrow();let state=slot.as_ref().unwrap();let index=state.session.bookmarks.iter().position(|b|b.id==id).context("bookmark id")?;Ok(state.bookmark_rect(index))};
    let drag=|from:LayoutRect,to:POINT|{
        let pack=|p:POINT|LPARAM(((p.x as u16 as u32)|((p.y as u16 as u32)<<16))as isize);
        let start=POINT{x:from.x+from.width/2,y:from.y+from.height/2};unsafe{
            SendMessageW(runner.hwnd,WM_LBUTTONDOWN,Some(WPARAM(1)),Some(pack(start)));
            SendMessageW(runner.hwnd,WM_MOUSEMOVE,Some(WPARAM(1)),Some(pack(to)));
            SendMessageW(runner.hwnd,WM_LBUTTONUP,None,Some(pack(to)));
        }pump(30);
    };
    let folder=rect(20)?;drag(rect(21)?,POINT{x:folder.x+folder.width/2,y:folder.y+folder.height/2});
    ensure!(runner.slot.borrow().as_ref().unwrap().session.bookmarks.iter().find(|b|b.id==21).unwrap().parent_id==Some(20),"drag into category failed");
    let root={let slot=runner.slot.borrow();let row=slot.as_ref().unwrap().layout.bookmarks_row;POINT{x:row.right()-10,y:row.y+row.height/2}};
    drag(rect(21)?,root);
    ensure!(runner.slot.borrow().as_ref().unwrap().session.bookmarks.iter().find(|b|b.id==21).unwrap().parent_id.is_none(),"drag out to root failed");
    let folder=rect(20)?;drag(rect(22)?,POINT{x:folder.x+1,y:folder.y+folder.height/2});
    ensure!(runner.slot.borrow().as_ref().unwrap().session.bookmarks[0].id==22,"sibling drag ordering failed");
    {
        let mut slot=runner.slot.borrow_mut();let state=slot.as_mut().unwrap();state.save();
        let saved=storage::load(&state.config_path)?;ensure!(saved.bookmarks==state.session.bookmarks,"category order did not persist");ensure!(saved.secondary_pane.tabs[0].locked,"hidden lock did not persist");
        state.session.secondary_pane.tabs[0].locked=false;state.session.tabs[0].locked=true;
    }
    unsafe{SendMessageW(runner.hwnd,WM_ENDSESSION,Some(WPARAM(1)),None);SendMessageW(runner.hwnd,WM_CLOSE,None,None);}
    ensure!(!unsafe{IsWindow(Some(runner.hwnd))}.as_bool(),"OS session completion was blocked by a lock");
    Ok(serde_json::json!({"locked_shortcuts_middle_double":true,"locked_close_groups":true,"locked_window_and_system_close":true,"hidden_pane_lock":true,
        "os_session_shutdown_allowed":true,"bookmark_drag_into_category":true,"bookmark_drag_to_root":true,"bookmark_sibling_order":true,"persisted_categories_and_locks":true}))
}

#[test]
#[ignore="Locked-tab close paths and category drag on a private unswitched desktop; isolated output required"]
fn native_locked_tabs_and_bookmark_category_drag()->Result<()>{
    let output=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("set isolated output")?).join("locked-bookmarks");
    ensure!(output.is_absolute(),"absolute output required");fs::create_dir_all(&output)?;
    let name=wide(&format!("ZSDesk.LockedBookmarks.{}",std::process::id()));
    let desktop=unsafe{CreateDesktopW(name.as_ptr(),ptr::null(),ptr::null(),0,0x000f00ff,ptr::null())};ensure!(!desktop.is_null(),"private desktop creation failed");
    let handle=desktop as usize;let destination=output.clone();
    let result=thread::spawn(move||->Result<serde_json::Value>{ensure!(unsafe{SetThreadDesktop(handle as*mut c_void)}!=0,"private desktop attachment failed");locked_tabs_and_bookmark_drag(&destination)}).join().map_err(|_|anyhow::anyhow!("native lock test panicked")).and_then(|r|r);
    let closed=unsafe{CloseDesktop(desktop)}!=0;let mut pid=0;unsafe{GetWindowThreadProcessId(GetForegroundWindow(),Some(&mut pid));}
    let isolated=pid!=std::process::id();let report=serde_json::json!({"state":if result.is_ok()&&closed&&isolated{"passed"}else{"failed"},"checks":result.as_ref().map_err(|e|format!("{e:#}")),"desktop_closed":closed,"test_did_not_activate":isolated});
    fs::write(output.join("verification.current.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");result?;ensure!(closed&&isolated,"private desktop cleanup/focus failed");Ok(())
}
