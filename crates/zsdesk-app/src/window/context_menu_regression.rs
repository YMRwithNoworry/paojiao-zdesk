//! Both native menu styles isolate extensions for selection and background requests.
use super::*;
use anyhow::ensure;
use std::{ffi::c_void,fs,path::Path,thread,time::{Duration,Instant}};
use crate::context_menu_process::isolation_tests::{Environment,sparse};

#[link(name="user32")]unsafe extern "system"{
    fn CreateDesktopW(name:*const u16,device:*const u16,mode:*const c_void,flags:u32,access:u32,attributes:*const c_void)->*mut c_void;
    fn SetThreadDesktop(desktop:*mut c_void)->i32;
    fn CloseDesktop(desktop:*mut c_void)->i32;
}
struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}
struct Runner{hwnd:HWND,slot:Box<Slot>}

#[test]
#[ignore="Exercises a real Shell listing and inline editor on an unswitched private desktop"]
fn large_folder_rename_reveals_sorted_file_and_directory() -> Result<()> {
    use windows::{core::Interface, Win32::UI::Shell::{IFolderView2, IShellItem, SIGDN_FILESYSPATH}};
    let out = PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("test output")?);
    let root = out.join(format!("sorted-reveal-{}", std::process::id()));
    let name = wide(&format!("ZSDesk.SortedReveal.{}", std::process::id()));
    let desktop = unsafe { CreateDesktopW(name.as_ptr(), std::ptr::null(), std::ptr::null(), 0, 0x000f00ff, std::ptr::null()) };
    ensure!(!desktop.is_null(), "create isolated test desktop");
    let raw = desktop as usize;
    let run = thread::spawn(move || -> Result<()> {
        ensure!(unsafe { SetThreadDesktop(raw as *mut c_void) } != 0, "attach isolated test desktop");
        unsafe { OleInitialize(None) }?; let _com = Apartment;
        let folder = root.join("listing"); fs::create_dir_all(&folder)?;
        for name in ["M-00000.txt","M-00001.txt","M-00002.txt"] { fs::write(folder.join(name), b"")?; }
        let runner = create(&root, &folder, true)?;
        for i in 3..1200 { fs::write(folder.join(format!("M-{i:05}.txt")), b"")?; }
        let shell = unsafe { runner.slot.borrow().as_ref().unwrap().browser.as_ref().unwrap().current_view()? };
        unsafe { shell.Refresh()?; }
        let view: IFolderView2 = shell.cast()?;
        let ready = Instant::now()+Duration::from_secs(30);
        while runner.slot.borrow().as_ref().unwrap().browser.as_ref().unwrap().status().items != 1200 {
            ensure!(Instant::now()<ready,"large folder did not finish enumerating"); pump(50);
        }
        for (old, new, is_folder, expected_index) in [("new-folder","000-folder",true,0),("new-file.txt","L-first-file.txt",false,1)] {
            let original = folder.join(old);
            eprintln!("large-list test: creating {old}");
            if is_folder { fs::create_dir(&original)?; } else { fs::write(&original,b"")?; }
            use windows::Win32::UI::Shell::{SHChangeNotify,SHCNE_MKDIR,SHCNE_CREATE,SHCNF_PATHW,SHCNF_FLUSHNOWAIT};
            let native_path=wide(original.as_os_str().to_string_lossy().as_ref());
            unsafe {SHChangeNotify(if is_folder{SHCNE_MKDIR}else{SHCNE_CREATE}, SHCNF_PATHW|SHCNF_FLUSHNOWAIT, Some(native_path.as_ptr().cast()), None);}
            runner.slot.borrow_mut().as_mut().unwrap().select_created_item(original.clone(),true);
            let deadline = Instant::now()+Duration::from_secs(15);
            let edit = loop {
                pump(35);
                let focus = unsafe { GetFocus() };
                let slot = runner.slot.borrow();
                let state = slot.as_ref().unwrap();
                if text_input_window(focus) && unsafe { IsChild(state.view_hwnd,focus) }.as_bool() &&
                    state.browser.as_ref().unwrap().selection_paths() == [original.to_string_lossy().to_string()] { break focus; }
                if Instant::now()>=deadline {
                    anyhow::bail!("new item did not start editing: {old}, selection={:?}, count={}, focus={focus:?}, view={:?}, deferred={}",
                        state.browser.as_ref().unwrap().selection_paths_limit(2),state.browser.as_ref().unwrap().status().items,
                        state.view_hwnd,state.deferred_selection.is_some());
                }
            };
            // Simulate only the scoped editor; the desktop is never switched or activated.
            runner.slot.borrow_mut().as_mut().unwrap().observe_rename_input(edit,edit,WM_KEYDOWN);
            ensure!(runner.slot.borrow().as_ref().unwrap().rename_follow.is_some(),"edit identity was not captured");
            eprintln!("large-list test: committing {old} -> {new}");
            unsafe {
                let mut existing=[0u16;260];
                let len=GetWindowTextW(edit,&mut existing) as usize;
                let name=if !is_folder && !String::from_utf16_lossy(&existing[..len]).ends_with(".txt") {new.trim_end_matches(".txt")} else {new};
                SetWindowTextW(edit, PCWSTR(wide(name).as_ptr()))?;
                SendMessageW(edit,WM_KEYDOWN,Some(WPARAM(VK_RETURN.0 as usize)),Some(LPARAM(1)));
                SendMessageW(edit,WM_KEYUP,Some(WPARAM(VK_RETURN.0 as usize)),Some(LPARAM(1)));
            }
            let deadline=Instant::now()+Duration::from_secs(15);
            loop {
                pump(60);
                let focused=unsafe { view.GetFocusedItem() }.unwrap_or(-1);
                let first=unsafe { view.GetVisibleItem(-1,false) }.unwrap_or(-1);
                let item=unsafe { view.GetItem::<IShellItem>(expected_index) }.ok().and_then(|item| unsafe { item.GetDisplayName(SIGDN_FILESYSPATH) }.ok());
                let exact=item.is_some_and(|text| {
                    let name=unsafe {text.to_string()}.ok();
                    unsafe {windows::Win32::System::Com::CoTaskMemFree(Some(text.0.cast()))};
                    name.as_deref().is_some_and(|p| Path::new(p)==folder.join(new))
                });
                if exact && focused==expected_index && first>=0 && (first..first+32).contains(&focused) && folder.join(new).exists() {
                    eprintln!("large-list test: {new} sorted={focused}, first-visible={first}");break;
                }
                if Instant::now()>=deadline { eprintln!("large-list failure: {new} focused={focused} first={first} expected={expected_index} exact={exact}"); }
                ensure!(Instant::now()<deadline,"renamed item not at sorted visible position: {new}, focused={focused}, first={first}, expected={expected_index}, exact={exact}");
            }
            // Wait for DefView's refresh and the bounded post-sort follow to finish
            // before starting a second, independent edit in this same fixture.
            pump(900);
        }
        drop(view);drop(shell);
        for details in [false, true] {
            runner.slot.borrow_mut().as_mut().unwrap().set_panes(false,details);
            let deadline = Instant::now()+Duration::from_secs(15);
            loop {
                pump(80);
                let slot=runner.slot.borrow();let state=slot.as_ref().unwrap();
                let outer=state.layout.status;
                ensure!(!outer.is_empty() && !state.layout.status_dual_pane.is_empty() &&
                        !state.layout.status_details.is_empty() && !state.layout.status_tiles.is_empty(),
                        "the ZSDesk outer status bar or its buttons disappeared");
                for (rect,action) in [(state.layout.status_dual_pane,Hit::StatusDualPane),
                    (state.layout.status_details,Hit::StatusDetails),(state.layout.status_tiles,Hit::StatusTiles)] {
                    ensure!(state.layout.hit(rect.x+rect.width/2,rect.y+rect.height/2)==action,"outer footer button lost its hit target");
                }
                let expected=state.layout.browser.bottom();
                let mut bottom=0;
                let mut view_width=0;
                if let Some(view)=state.browser.as_ref().and_then(|b| b.view_window()){
                    let mut r=RECT::default();unsafe{GetWindowRect(view,&mut r)}?;
                    view_width=r.right-r.left;let mut p=POINT{x:r.left,y:r.bottom};let _=unsafe{ScreenToClient(runner.hwnd,&mut p)};bottom=p.y;
                }
                let details_present=!details || (view_width>0 && view_width<state.layout.browser.width-50);
                if state.pending.is_none() && (bottom-expected).abs()<=1 && details_present {
                    eprintln!("outer ZSDesk footer: details={details}, file-list bottom={bottom}, footer top={}, three original controls present",outer.y);
                    break;
                }
                ensure!(Instant::now()<deadline,"native file list does not reach the outer bar: details={details}, bottom={bottom}, expected={expected}, width={view_width}");
            }
            if !details {
                #[link(name="user32")] unsafe extern "system" {fn PrintWindow(window: HWND, dc: windows::Win32::Graphics::Gdi::HDC, flags:u32)->i32;}
                let mut rect=RECT::default();unsafe{GetWindowRect(runner.hwnd,&mut rect)}?;
                let (width,height)=(rect.right-rect.left,rect.bottom-rect.top);
                let surface=Surface::new(width,height).context("capture private test window")?;
                ensure!(unsafe{PrintWindow(runner.hwnd,surface.dc,2)}!=0,"capture private status bar");
                let pixels=unsafe{std::slice::from_raw_parts(surface.bits,(4*width*height) as usize)};
                fs::write(root.join("outer-status.bgra"),pixels)?;
                fs::write(root.join("outer-status.json"),format!("[{width},{height}]"))?;
            }
        }
        Ok(())
    }).join();
    ensure!(unsafe {CloseDesktop(desktop)}!=0,"close isolated test desktop");
    run.map_err(|_|anyhow::anyhow!("isolated rename test panicked"))?
}

#[test]
#[ignore="Exercises tab menus and empty-strip closure on an unswitched private desktop"]
fn tab_menu_and_empty_strip_have_distinct_right_clicks()->Result<()>{
    use zsdesk_core::model::explorer::TabPlacement;
    let out=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("test output")?);
    let name=wide(&format!("ZSDesk.TabRight.{}",std::process::id()));
    let desktop=unsafe{CreateDesktopW(name.as_ptr(),std::ptr::null(),std::ptr::null(),0,0x000f00ff,std::ptr::null())};
    ensure!(!desktop.is_null(),"private desktop");let handle=desktop as usize;
    let result=thread::spawn(move||->Result<()>{
        ensure!(unsafe{SetThreadDesktop(handle as *mut c_void)}!=0,"private desktop attachment");
        unsafe{OleInitialize(None)}?;let _com=Apartment;
        let files=out.join("files");fs::create_dir_all(&files)?;
        for name in ["one.txt","two.txt","three.txt"]{fs::write(files.join(name),b"tab menu check")?;}
        let runner=create(&out,&files,true)?;let owner=runner.hwnd.0 as isize;
        let packed=|p:POINT|LPARAM(((p.y as u16 as u32)<<16|p.x as u16 as u32)as isize);
        let click=|mut p:POINT|->Result<()>{
            let client=packed(p);let _=unsafe{ClientToScreen(runner.hwnd,&mut p)};let screen=packed(p);
            let ht=unsafe{SendMessageW(runner.hwnd,WM_NCHITTEST,None,Some(screen))}.0 as u32;
            let(down,up,wp,lp)=if ht==HTCAPTION{(WM_NCRBUTTONDOWN,WM_NCRBUTTONUP,WPARAM(ht as usize),screen)}
                else{ensure!(ht==HTCLIENT,"unexpected hit {ht}");(WM_RBUTTONDOWN,WM_RBUTTONUP,WPARAM(0),client)};
            unsafe{SendMessageW(runner.hwnd,down,Some(wp),Some(lp));SendMessageW(runner.hwnd,up,Some(wp),Some(lp));}Ok(())
        };
        for placement in [TabPlacement::Top,TabPlacement::Left,TabPlacement::Right]{
            let(tab,blank)={
                let mut slot=runner.slot.borrow_mut();let state=slot.as_mut().unwrap();
                state.session.tabs=(1..=3).map(|i|ExplorerTab::new(i,ShellLocation::Path(files.clone()))).collect();
                state.session.active_tab=0;state.session.next_id=4;state.session.ui.tab_placement=placement;
                unsafe{state.rebuild_tabs();}let l=&state.layout;
                let r=l.tabs.iter().find(|s|s.index==1).context("inactive tab")?.rect;
                let area=if l.vertical_tabs{l.tab_panel}else{l.strip};
                let blank=(area.y+8..area.bottom()-3).find_map(|y|(area.x+5..area.right()-5).find(|&x|
                    l.tab_strip_right_hit(x,y,false)==Some(Hit::Caption) && (l.vertical_tabs||x>l.new_tab.right())
                ).map(|x|POINT{x,y})).context("empty tab strip")?;
                (POINT{x:r.x+r.width/2,y:r.y+r.height/2},blank)
            };
            let observer=thread::spawn(move||->Result<()>{
                ensure!(unsafe{SetThreadDesktop(handle as *mut c_void)}!=0,"observe private desktop");
                let owner=HWND(owner as *mut _);let deadline=Instant::now()+Duration::from_secs(4);
                loop{
                    if let Ok(menu)=unsafe{FindWindowExW(None,None,windows::core::w!("ZSDesk.Win11Flyout"),None)}{
                        if unsafe{GetWindow(menu,GW_OWNER)}.ok()==Some(owner)&&unsafe{IsWindowVisible(menu)}.as_bool(){
                            for key in [VK_HOME,VK_DOWN,VK_DOWN,VK_DOWN,VK_RETURN]{unsafe{PostMessageW(Some(menu),WM_KEYDOWN,WPARAM(key.0 as usize),LPARAM(0))}?;}
                            return Ok(());
                        }
                    }
                    if Instant::now()>=deadline{let _=unsafe{PostMessageW(Some(owner),WM_KEYDOWN,WPARAM(VK_ESCAPE.0 as usize),LPARAM(0))};anyhow::bail!("tab menu did not appear");}
                    thread::sleep(Duration::from_millis(5));
                }
            });
            click(tab)?;observer.join().map_err(|_|anyhow::anyhow!("menu observation failed"))??;
            {let slot=runner.slot.borrow();let state=slot.as_ref().unwrap();
                ensure!(state.session.tabs.len()==3&&state.session.active_tab().id==1&&state.session.tabs[1].locked,"menu command targeted wrong tab: {placement:?}");
            }
            click(blank)?;pump(40);
            {let slot=runner.slot.borrow();let state=slot.as_ref().unwrap();
                ensure!(state.session.tabs.iter().map(|t|t.id).eq([2,3])&&state.session.tabs[0].locked,"blank click did not close only the active tab: {placement:?}");
            }
        }
        Ok(())
    }).join().map_err(|_|anyhow::anyhow!("tab test panicked")).and_then(|r|r);
    ensure!(unsafe{CloseDesktop(desktop)}!=0,"private desktop retained");result
}

#[test]
#[ignore="Compares the complete window chrome against 96-pixel bands on an isolated desktop"]
fn bounded_chrome_surface_matches_full_raster()->Result<()>{
    use zsdesk_core::model::explorer::TabPlacement;
    use windows::Win32::Graphics::Gdi::GdiFlush;
    let root=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("test output")?);
    let name=wide(&format!("ZSDesk.BandedPaint.{}",std::process::id()));
    let desktop=unsafe{CreateDesktopW(name.as_ptr(),std::ptr::null(),std::ptr::null(),0,0x000f00ff,std::ptr::null())};
    ensure!(!desktop.is_null(),"private desktop");let handle=desktop as usize;
    let result=thread::spawn(move||->Result<()>{
        ensure!(unsafe{SetThreadDesktop(handle as *mut _)}!=0,"attach private desktop");
        unsafe{OleInitialize(None)}?;let _apartment=Apartment;
        let files=root.join("files");fs::create_dir_all(&files)?;
        for name in ["one.txt","two.txt","three.txt"]{fs::write(files.join(name),b"window chrome")?;}
        let runner=create(&root,&files,true)?;
        let mut slot=runner.slot.borrow_mut();let state=slot.as_mut().unwrap();
        ensure!(state.tree.is_none(),"hidden navigation tree was created");
        state.apply_interface_choice("interface-navigation");
        ensure!(state.tree.is_some()&&state.session.ui.show_tree,"navigation tree did not open");
        state.apply_interface_choice("interface-navigation");
        ensure!(state.tree.is_none()&&!state.session.ui.show_tree,"navigation tree was retained after hiding it");
        for placement in [TabPlacement::Top,TabPlacement::Left,TabPlacement::Right]{
            state.session.ui.tab_placement=placement;
            state.relayout();state.paint();
            let reference=state.surface.as_ref().context("full raster")?;
            let(w,h)=(reference.width,reference.height);let bytes=(w*h*4)as usize;
            let _=unsafe{GdiFlush()};
            let expected=unsafe{std::slice::from_raw_parts(reference.bits,bytes)}.to_vec();
            let output=Surface::new(w,h).context("destination DIB")?;
            state.paint_update(output.dc,RECT{left:0,top:0,right:w,bottom:h});
            let _=unsafe{GdiFlush()};
            let actual=unsafe{std::slice::from_raw_parts(output.bits,bytes)};
            let changed=actual.chunks_exact(4).zip(expected.chunks_exact(4)).filter(|(a,b)|a!=b).count();
            ensure!(changed==0,"{placement:?}: {changed} differing pixels in {w}x{h}");
            ensure!(state.surface.as_ref().is_some_and(|s|s.height<=96),"full backing store retained");
            let clip=RECT{left:7,top:83,right:w-11,bottom:h.min(213)};
            unsafe{std::ptr::write_bytes(output.bits,0x6c,bytes);}
            state.paint_update(output.dc,clip);let _=unsafe{GdiFlush()};
            let pixels=unsafe{std::slice::from_raw_parts(output.bits,bytes)};
            for (i,pixel) in pixels.chunks_exact(4).enumerate(){
                let(x,y)=((i%w as usize)as i32,(i/w as usize)as i32);
                let inside=x>=clip.left&&x<clip.right&&y>=clip.top&&y<clip.bottom;
                let target=if inside{&expected[4*i..4*i+4]}else{&[0x6c;4]};
                ensure!(pixel==target,"{placement:?}: incorrect partial repaint at {x},{y}");
            }
        }
        Ok(())
    }).join().map_err(|_|anyhow::anyhow!("paint test panicked")).and_then(|r|r);
    ensure!(unsafe{CloseDesktop(desktop)}!=0,"private desktop retained");result
}

#[test]
#[ignore="Real shell contexts and opt-in archive provider on an isolated desktop"]
fn drawn_pages_and_registered_submenus()->Result<()>{
    use crate::context_menu_process as worker;
    let out=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("test output")?);
    let name=format!("ZSDesk.DrawnPages.{}",std::process::id());
    let desktop=unsafe{CreateDesktopW(wide(&name).as_ptr(),std::ptr::null(),std::ptr::null(),0,0x000f00ff,std::ptr::null())};
    ensure!(!desktop.is_null(),"private test desktop");let raw=desktop as usize;
    let result=thread::spawn(move||->Result<()>{
        ensure!(unsafe{SetThreadDesktop(raw as *mut c_void)}!=0,"attach test desktop");
        let _desktop=Environment::set("ZSDESK_MENU_TEST_DESKTOP",&name);
        let workers=out.join("workers");fs::create_dir_all(&workers)?;
        let _root=Environment::set("ZSDESK_MENU_ROOT",&workers);
        unsafe{OleInitialize(None)}?;let _com=Apartment;
        struct Workers;impl Drop for Workers{fn drop(&mut self){worker::shutdown_idle_worker();let until=Instant::now()+Duration::from_secs(6);while worker::pending_workers()>0&&Instant::now()<until{pump(8);}}}let _workers=Workers;
        let files=out.join("files");fs::create_dir_all(files.join("folder"))?;fs::write(files.join("one.txt"),b"ZSDesk archive selection trial")?;fs::write(files.join("two.txt"),b"second target")?;
        let runner=create(&out,&files,true)?;
        let(profile,mut prefs,view,window)={let state=runner.slot.borrow();let state=state.as_ref().unwrap();let b=state.browser.as_ref().unwrap();
            (state.config_path.clone(),state.session.ui.context_menu.clone(),unsafe{b.current_view()?},b.view_window().context("view")?)};
        let registrations=zsdesk_windows::shell::menu::native_app_registrations()?;
        let extension=registrations.iter().find(|(id,_)|id.starts_with("pkg:")&&id.to_lowercase().contains("local.archivemenu.sevenzip")).context("registered 7-Zip modern provider required")?.0.clone();
        for (case,selection,depth,enabled) in [
            ("visual-file",vec!["one.txt"],1,false),("visual-folder",vec!["folder"],1,false),("visual-multi",vec!["one.txt","two.txt"],1,false),
            ("visual-background",vec![],1,false),("visual-mixed",vec!["one.txt","folder"],1,false),("visual-seven",vec!["one.txt"],2,true),
            ("visual-more",vec!["one.txt"],2,false),("visual-more-zip",vec!["one.txt"],3,false),
            ("visual-more-archive",vec!["two.txt"],4,false),("visual-openwith",vec!["one.txt"],2,false),
            ("visual-deep",vec!["one.txt"],3,true),("visual-legacy",vec!["one.txt"],2,true),("visual-archive",vec!["one.txt"],2,true),("visual-new",vec![],2,false)]{
            if let Ok(scenes)=std::env::var("ZSDESK_VISUAL_SCENES"){if !scenes.split(',').any(|s|s==case){continue;}}
            prefs.native_first_page_apps=if case=="visual-legacy"{vec![registrations.iter().find(|(id,_)|id.eq_ignore_ascii_case("legacy:23170F69-40C1-278A-1000-000100020000")).context("registered 7-Zip Shell handler required")?.0.clone()]}else if enabled{vec![extension.clone()]}else{Vec::new()};crate::context_preferences::publish(&profile,&prefs);
            {let state=runner.slot.borrow();let b=state.as_ref().unwrap().browser.as_ref().unwrap();b.select_all(false);
                if !selection.is_empty(){b.select_children(&selection.iter().map(|s|std::ffi::OsString::from(s)).collect::<Vec<_>>())?;}}
            pump(30);let _probe=Environment::set("ZSDESK_MENU_TEST_PROBE",case);
            let target=if selection.is_empty(){zsdesk_windows::shell::menu::ViewMenuTarget::Background}else{zsdesk_windows::shell::menu::ViewMenuTarget::Selection};
            super::context_menu::show(window,&view,window,LPARAM(-1),target)?;
            let deadline=Instant::now()+Duration::from_secs(9);
            while super::context_menu::request_active(runner.hwnd){if Instant::now()>=deadline{super::context_menu::close(runner.hwnd);anyhow::bail!("timed out: {case}");}pump(5);}
            let snap:serde_json::Value=serde_json::from_slice(&fs::read(out.join(format!("{case}.json")))?)?;
            let levels=snap["levels"].as_array().context("painted levels")?;
            ensure!(snap["renderer"]=="drawn-shell-hmenu"&&levels.len()>=depth&&levels.iter().all(|l|l["native_fallback"]==false),"{case} missing a drawn level");
            let labels=levels[0]["rows"].as_array().context("rows")?;
            if case.starts_with("visual-more"){
                ensure!(levels[0]["visible"]==false&&levels[1]["visible"]==true,"More did not replace the first page");
                let rows=levels[1]["rows"].as_array().context("more options")?;
                let mut seen=std::collections::HashSet::new();
                for row in rows.iter().filter(|r|r["height"].as_f64().unwrap_or(0.)>0.){
                    if let Some(label)=row["label"].as_str().filter(|s|!s.is_empty()){
                        ensure!(seen.insert(label.to_lowercase()),"duplicate displayed in More: {label}");
                    }
                }
                if levels.len()>=3{
                    let variants=levels[2]["rows"].as_array().context("command implementations")?;
                    let ids:std::collections::HashSet<_>=variants.iter().filter(|r|r["height"].as_f64().unwrap_or(0.)>0.).filter_map(|r|r["id"].as_u64()).collect();
                    ensure!(ids.len()>=2,"different extension command IDs were lost");
                }
            }
            let has=|name:&str|labels.iter().any(|row|row["label"]==name);
            match case{
                "visual-file"=>ensure!(has("打开方式")&&!has("在新标签页中打开"),"file received folder actions"),
                "visual-folder"=>ensure!(has("在新标签页中打开")&&!has("打开方式"),"folder received file actions"),
                "visual-multi"|"visual-mixed"=>ensure!(!has("重命名")&&!has("打开方式"),"multi-selection received single-item actions"),
                "visual-background"=>ensure!(has("查看")&&has("新建")&&!has("显示设置"),"wrong directory background"),
                _=>{}
            }
            ensure!(labels.iter().any(|r|r["label"].as_str().is_some_and(|l|l.to_lowercase().contains("7-zip")))==enabled,"unexpected archive visibility for {case}");
            if matches!(case,"visual-seven"|"visual-legacy"|"visual-deep"){
                let children=levels[1]["rows"].as_array().context("7-Zip children")?;
                ensure!(children.iter().any(|r|r["submenu"]==true&&r["label"].as_str().is_some_and(|s|s.contains("CRC"))),"missing checksum submenu in {case}");
                ensure!(children.iter().any(|r|r["submenu"]==false&&r["label"].as_str().is_some_and(|s|s.contains("one.7z"))),"missing selection-specific archive command in {case}");
            }
            if case=="visual-deep"{
                let checksums=levels[2]["rows"].as_array().context("checksum commands")?;
                for algorithm in ["CRC-32","SHA-256"]{
                    ensure!(checksums.iter().any(|r|r["label"]==algorithm&&r["enabled"]==true&&r["submenu"]==false),"missing checksum command {algorithm}");
                }
            }
            let diagnostic=worker::read_recent_diagnostics()["current"].clone();
            ensure!(diagnostic["error"].is_null()&&diagnostic["native_menu"]["runtime_initializations"]==0,"{case}: {diagnostic}");
            fs::write(out.join(format!("{case}-diagnostic.json")),serde_json::to_vec_pretty(&diagnostic)?)?;
            if case=="visual-archive"{let until=Instant::now()+Duration::from_secs(8);while !files.join("one.7z").is_file(){ensure!(Instant::now()<until,"archive command had no effect");pump(20);}}
            if case=="visual-more-archive"{let until=Instant::now()+Duration::from_secs(8);while !files.join("two.7z").is_file(){ensure!(Instant::now()<until,"grouped archive command had no effect");pump(20);}}
        }
        use zsdesk_windows::shell::{menu::MenuInterceptions,pidl::location_pidl};
        let bin=ShellLocation::Shell{parsing_name:"::{645FF040-5081-101B-9F08-00AA002F954E}".into(),title:"回收站".into(),pidl:None};
        let bin=location_pidl(&bin).context("Recycle Bin PIDL")?;
        let root=files.ancestors().last().context("drive")?.to_path_buf();
        let drive=location_pidl(&ShellLocation::Path(root)).context("drive PIDL")?;
        let mut contexts=vec![("visual-desktop",vec![0,0],true),("visual-drive",drive,false),
            ("visual-bin-icon",bin.clone(),false),("visual-bin-background",bin,true)];
        if let Some(extra)=std::env::var_os("ZSDESK_CONTEXT_FILE_FIXTURES").map(PathBuf::from){
            for (scene,path) in [("visual-image",extra.join("three.png")),("visual-zip",extra.join("sample.zip")),
                ("visual-link",extra.join("sample.lnk")),("visual-executable",std::env::current_exe()?)]{
                let id=location_pidl(&ShellLocation::Path(path)).context("sample item identity")?;
                contexts.push((scene,id,false));
            }
        }
        for (case,identity,background) in contexts{
            if let Ok(scenes)=std::env::var("ZSDESK_VISUAL_SCENES"){if !scenes.split(',').any(|s|s==case){continue;}}
            let _probe=Environment::set("ZSDESK_MENU_TEST_PROBE",case);
            prefs.native_first_page_apps.clear();
            let mut client=worker::Client::spawn(window,POINT{x:180,y:160},vec![identity],None,
                crate::context_preferences::native(&prefs),MenuInterceptions::default(),if background{worker::Mode::Background}else{worker::Mode::More})?;
            let until=Instant::now()+Duration::from_secs(12);
            loop{if let Some(result)=client.poll(&mut |commit|!commit){result?;break;}ensure!(Instant::now()<until,"timed out: {case}");pump(5);}
            let snapshot:serde_json::Value=serde_json::from_slice(&fs::read(out.join(format!("{case}.json"))).with_context(||case.to_string())?)?;
            let rows=snapshot["levels"][0]["rows"].as_array().context("rows")?;
            let has=|fragment:&str|rows.iter().any(|r|r["label"].as_str().is_some_and(|s|s.contains(fragment)));
            match case{
                "visual-desktop"=>ensure!(has("显示设置")&&has("个性化")&&has("新建"),"desktop namespace was lost"),
                "visual-drive"=>ensure!(has("打开")&&!has("固定到“开始”"),"drive received the folder-only pin action"),
                "visual-image"=>ensure!(has("向右旋转")&&has("设置为桌面背景")&&!has("以管理员身份运行"),"image actions"),
                "visual-zip"=>ensure!(has("全部解压缩")&&!has("向右旋转"),"archive actions"),
                "visual-link"=>ensure!(has("打开文件所在的位置"),"shortcut actions"),
                "visual-executable"=>ensure!(has("以管理员身份运行")&&!has("全部解压缩")&&!has("打开方式"),"executable actions"),
                _=>ensure!(has("清空")&&!has("新建"),"Recycle Bin received a filesystem menu: {case}"),
            }
            fs::write(out.join(format!("{case}-diagnostic.json")),serde_json::to_vec_pretty(&worker::read_recent_diagnostics()["current"])?)?;
        }
        Ok(())
    }).join().map_err(|_|anyhow::anyhow!("menu case panicked")).and_then(|r|r);
    ensure!(unsafe{CloseDesktop(desktop)}!=0,"test desktop retained");result
}

#[test]
#[ignore="On-demand drawn worker across click-away and a 185-second pause on a private desktop"]
fn drawn_menu_exits_after_dismissal_and_reopens_after_idle()->Result<()>{
    use crate::context_menu_process as worker;
    let out=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("test output")?);
    let name=format!("ZSDesk.MenuReuse.{}",std::process::id());
    let desktop=unsafe{CreateDesktopW(wide(&name).as_ptr(),std::ptr::null(),std::ptr::null(),0,0x000f00ff,std::ptr::null())};
    ensure!(!desktop.is_null(),"test desktop");let raw=desktop as usize;
    let result=thread::spawn(move||->Result<()>{
        ensure!(unsafe{SetThreadDesktop(raw as *mut c_void)}!=0,"private desktop attachment");
        let _desktop=Environment::set("ZSDESK_MENU_TEST_DESKTOP",&name);
        let workers=out.join("workers");fs::create_dir_all(&workers)?;
        let _root=Environment::set("ZSDESK_MENU_ROOT",&workers);
        unsafe{OleInitialize(None)}?;let _ole=Apartment;
        struct Pool;impl Drop for Pool{fn drop(&mut self){worker::shutdown_idle_worker();let until=Instant::now()+Duration::from_secs(5);
            while worker::pending_workers()>0&&Instant::now()<until{pump(5);}
        }}let _pool=Pool;
        let files=out.join("files");fs::create_dir_all(&files)?;
        for name in ["one.txt","two.txt","three.txt"]{fs::write(files.join(name),b"native menu selection")?;}
        let runner=create(&out,&files,true)?;
        let (window,view)={let slot=runner.slot.borrow();let browser=slot.as_ref().unwrap().browser.as_ref().unwrap();
            (browser.view_window().context("file view")?,unsafe{browser.current_view()?})};
        let mut records=Vec::new();
        for stage in ["first","consecutive","outside","after-outside","other-file","after-idle"]{
            if stage=="after-idle"{pump(185_000);}
            let file=if stage=="other-file"{"two.txt"}else{"one.txt"};
            {let slot=runner.slot.borrow();slot.as_ref().unwrap().browser.as_ref().unwrap().select_children(&[file.into()])?;}
            let selected=super::context_menu::selected_identities(&view)?;
            ensure!(zsdesk_windows::shell::file_transfer::paths_from_pidls(&selected)?==[files.join(file)],"wrong new target");
            let outside=stage=="outside";
            let _probe=Environment::set("ZSDESK_MENU_TEST_PROBE",if outside{"outside-dismiss"}else{"menu-dismiss"});
            unsafe{SendMessageW(window,WM_CONTEXTMENU,Some(WPARAM(window.0 as usize)),Some(LPARAM(-1)));}
            let until=Instant::now()+Duration::from_secs(12);let mut clicked=false;
            while super::context_menu::request_active(runner.hwnd){
                pump(2);ensure!(Instant::now()<until,"menu did not finish: {stage}");
                if !outside||clicked{continue;}
                for entry in fs::read_dir(&workers)?.flatten(){
                    let read=|name:&str|fs::read(entry.path().join(name)).ok().and_then(|b|serde_json::from_slice::<serde_json::Value>(&b).ok());
                    let (Some(request),Some(opened))=(read("request.json"),read("opened.json"))else{continue;};
                    if opened["token"]!=request["token"] || request["probe"]!="outside-dismiss" || opened["observation"]!="drawn-win11-menu"{continue;}
                    let mut rc=RECT::default();unsafe{GetClientRect(window,&mut rc)}?;
                    let xy=LPARAM((15u32|(((rc.bottom-10)as u16 as u32)<<16))as isize);
                    unsafe{PostMessageW(Some(window),WM_LBUTTONDOWN,WPARAM(1),xy)?;PostMessageW(Some(window),WM_LBUTTONUP,WPARAM(0),xy)?;}
                    pump(5);super::context_menu::poll(runner.hwnd);clicked=true;break;
                }
            }
            ensure!(!outside||clicked,"outside dismissal not exercised");
            // The UI is free before the worker acknowledges dismissal. Use the same
            // short gap as the reproduction, without waiting for pool readiness.
            pump(70);
            let released=Instant::now();
            while worker::pending_workers()>0{ensure!(released.elapsed()<Duration::from_secs(5),"drawn menu child did not exit after {stage}");pump(5);}
            ensure!(worker::idle_worker_diagnostics().as_array().is_some_and(Vec::is_empty),"drawn menu child was parked");
            let log=worker::read_recent_diagnostics()["current"].clone();
            if outside{
                ensure!(!log["committed"].as_bool().unwrap_or(true),"outside input authorized a command");
                continue;
            }
            let pid=log["worker_pid"].as_u64().context("worker PID")?;
            ensure!(log["parent_pid"]==std::process::id()&&log["timing"]["menu_open_observation"]=="drawn-win11-menu"&&
                log["error"].is_null()&&log["committed"]==false,"native presentation failed: {log}");
            ensure!(!records.iter().any(|previous:&serde_json::Value|previous["pid"]==pid && previous["created"]==log["worker_created"]),
                "a displayed drawn menu worker was reused at {stage}: {log}");
            records.push(serde_json::json!({"stage":stage,"pid":pid,"created":log["worker_created"],"timing":log["timing"],"target":file,"parked":0}));
            fs::write(out.join("drawn-on-demand.json"),serde_json::to_vec_pretty(&records)?)?;
        }
        drop(runner);
        let until=Instant::now()+Duration::from_secs(5);
        while !worker::idle_worker_diagnostics().as_array().is_some_and(Vec::is_empty){
            worker::pending_workers();ensure!(Instant::now()<until,"closed owner retained native worker");pump(10);
        }
        Ok(())
    }).join().map_err(|_|anyhow::anyhow!("native reuse test panicked")).and_then(|r|r);
    let until=Instant::now()+Duration::from_secs(3);
    while unsafe{CloseDesktop(desktop)}==0{ensure!(Instant::now()<until,"test desktop retained");thread::sleep(Duration::from_millis(20));}
    result
}

#[test]
#[ignore="Native prewarm and a different file selection in an isolated Win32 desktop"]
fn selecting_files_prewarms_shell_without_showing_a_menu()->Result<()>{
    use crate::context_menu_process as worker;
    let output=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("test output")?);
    let name=format!("ZSDesk.Prewarm.{}",std::process::id());
    let desktop=unsafe{CreateDesktopW(wide(&name).as_ptr(),std::ptr::null(),std::ptr::null(),0,0x000f00ff,std::ptr::null())};
    ensure!(!desktop.is_null(),"create test desktop");
    let raw=desktop as usize;
    let result=thread::spawn(move||->Result<()>{
        ensure!(unsafe{SetThreadDesktop(raw as *mut c_void)}!=0,"attach test desktop");
        let _desktop=Environment::set("ZSDESK_MENU_TEST_DESKTOP",&name);
        let workers=output.join("workers");fs::create_dir_all(&workers)?;
        let _root=Environment::set("ZSDESK_MENU_ROOT",&workers);
        unsafe{OleInitialize(None)}?;let _apartment=Apartment;
        struct Pool;impl Drop for Pool{fn drop(&mut self){
            worker::shutdown_idle_worker();let deadline=Instant::now()+Duration::from_secs(5);
            while worker::pending_workers()>0 && Instant::now()<deadline{pump(10);}
        }}let _pool=Pool;
        let folder=output.join("files");fs::create_dir_all(&folder)?;
        for name in ["one.txt","two.txt","three.txt"]{fs::write(folder.join(name),b"menu")?;}
        let runner=create(&output,&folder,true)?;
        let (view,window)={
            let slot=runner.slot.borrow();let browser=slot.as_ref().unwrap().browser.as_ref().unwrap();
            browser.select_children(&["one.txt".into()])?;
            (unsafe{browser.current_view()?},browser.view_window().context("view window")?)
        };
        super::context_menu::prewarm_active_view(runner.hwnd);
        let until=Instant::now()+Duration::from_secs(6);
        while worker::idle_worker_diagnostics().as_array().is_none_or(Vec::is_empty){
            ensure!(Instant::now()<until,"prepared worker did not become idle");pump(10);
        }
        let prepared=worker::idle_worker_diagnostics()[0]["pid"].clone();
        {
            let slot=runner.slot.borrow();slot.as_ref().unwrap().browser.as_ref().unwrap().select_children(&["two.txt".into()])?;
        }
        let selected=super::context_menu::selected_identities(&view)?;
        ensure!(zsdesk_windows::shell::file_transfer::paths_from_pidls(&selected)?==[folder.join("two.txt")],"selection did not change");
        let _probe=Environment::set("ZSDESK_MENU_TEST_PROBE","menu-dismiss");
        unsafe{SendMessageW(window,WM_CONTEXTMENU,Some(WPARAM(window.0 as usize)),Some(LPARAM(-1)));}
        let deadline=Instant::now()+Duration::from_secs(10);
        while super::context_menu::request_active(runner.hwnd){ensure!(Instant::now()<deadline,"native popup did not close");pump(5);}
        let shown=worker::read_recent_diagnostics()["current"].clone();
        ensure!(shown["worker_pid"]==prepared && shown["timing"]["reused_process"]==true &&
            shown["timing"]["menu_open_observation"]=="drawn-win11-menu" && shown["committed"]==false &&
            shown["error"].is_null(),"on-demand drawn popup failed: {shown}");
        ensure!(super::context_menu::selected_identities(&view)?==selected,"presentation changed its target");
        Ok(())
    }).join().map_err(|_|anyhow::anyhow!("menu prewarm test panicked")).and_then(|r|r);
    let deadline=Instant::now()+Duration::from_secs(3);
    while unsafe{CloseDesktop(desktop)}==0{ensure!(Instant::now()<deadline,"test desktop retained");thread::sleep(Duration::from_millis(20));}
    result
}
impl Drop for Runner{fn drop(&mut self){let _=unsafe{DestroyWindow(self.hwnd)};}}
fn pump(ms:u64){let end=Instant::now()+Duration::from_millis(ms);while Instant::now()<end{
    let mut message=MSG::default();for _ in 0..256{if !unsafe{PeekMessageW(&mut message,None,0,0,PM_REMOVE)}.as_bool(){break;}if message.message!=WM_QUIT{unsafe{let _=TranslateMessage(&message);DispatchMessageW(&message);}}}thread::sleep(Duration::from_millis(3));}}
fn create(root:&Path,folder:&Path,modern:bool)->Result<Runner>{
    unsafe{InitCommonControlsEx(&INITCOMMONCONTROLSEX{dwSize:size_of::<INITCOMMONCONTROLSEX>()as u32,dwICC:ICC_STANDARD_CLASSES|ICC_BAR_CLASSES|ICC_LISTVIEW_CLASSES})}.ok()?;
    let mut session=storage::default_session();session.tabs=vec![ExplorerTab::new(1,ShellLocation::Path(folder.into()))];session.active_tab=0;session.next_id=2;
    session.ui.context_menu.mode=if modern{zsdesk_core::model::explorer::ContextMenuMode::Windows11}else{zsdesk_core::model::explorer::ContextMenuMode::Original};
    session.ui.show_preview=false;session.ui.hover_preview=false;session.ui.show_details=false;session.ui.show_tree=false;
    let slot=Box::new(RefCell::new(Some(WindowState::new(session,root.join("profile/explorer-session.json"),None,modern))));
    let instance=HINSTANCE(unsafe{GetModuleHandleW(None)?}.0);let class=windows::core::w!("ZSDesk.Native.QuickMenuRegression");
    unsafe{RegisterClassW(&WNDCLASSW{hInstance:instance,lpfnWndProc:Some(window_proc),lpszClassName:class,..Default::default()});}
    let hwnd=unsafe{CreateWindowExW(WS_EX_NOACTIVATE,class,windows::core::w!("Quick context menu regression"),WS_OVERLAPPEDWINDOW|WS_CLIPCHILDREN,100,100,1040,720,None,None,Some(instance),Some((&*slot as*const Slot).cast()))}?;
    let runner=Runner{hwnd,slot};unsafe{runner.slot.borrow_mut().as_mut().unwrap().create(hwnd)?;let _=ShowWindow(hwnd,SW_SHOWNOACTIVATE);}
    let deadline=Instant::now()+Duration::from_secs(12);
    loop{pump(25);if runner.slot.borrow().as_ref().is_some_and(|state|state.pending.is_none()&&state.browser.as_ref().is_some_and(|browser|browser.status().items==3)){break;}
        ensure!(Instant::now()<deadline,"main Shell view did not materialize sparse files");}
    Ok(runner)
}
fn exercise(root:&Path,desktop_name:&str)->Result<serde_json::Value>{
    struct Workers;
    impl Drop for Workers{fn drop(&mut self){
        crate::context_menu_process::shutdown_idle_worker();let deadline=Instant::now()+Duration::from_secs(5);
        while crate::context_menu_process::pending_workers()>0&&Instant::now()<deadline{pump(10);}
    }}
    let _workers=Workers;
    let _desktop=Environment::set("ZSDESK_MENU_TEST_DESKTOP",desktop_name);
    let _root=Environment::set("ZSDESK_MENU_ROOT",root.join("workers"));
    unsafe{OleInitialize(None)?;}let _apartment=Apartment;
    let folder=root.join("files");fs::create_dir_all(&folder)?;let(files,_,records)=sparse(&folder)?;
    let mut rows=Vec::new();
    for modern in [false,true]{
        let runner=create(&root.join(if modern{"modern"}else{"classic"}),&folder,modern)?;
        let exercised=(||->Result<()>{
        for selected in [false,true]{
            let view={let state=runner.slot.borrow();let browser=state.as_ref().unwrap().browser.as_ref().unwrap();
                browser.select_all(selected);browser.view_window().context("view HWND")?};
            pump(60);
            {let state=runner.slot.borrow();let status=state.as_ref().unwrap().browser.as_ref().unwrap().status();
                ensure!(status.selected==if selected{3}else{0},"selection/background setup failed");
                if selected{ensure!(status.selected_bytes==Some(24u64*1024*1024*1024),"large-selection size truncated");}}
            for crash in [true,false]{
                println!("context regression: modern={modern}, selected={selected}, injected_crash={crash}");
                let _probe=Environment::set("ZSDESK_MENU_TEST_PROBE",if crash{"crash"}else{"dismiss"});
                let before=zsdesk_windows::shell::menu_diagnostics();
                let mut bounds=RECT::default();unsafe{GetWindowRect(view,&mut bounds)?;}
                let position=if selected{LPARAM(-1)}else{LPARAM((((bounds.left+60)as u16 as u32)|(((bounds.bottom-40)as u16 as u32)<<16))as isize)};
                let started=Instant::now();
                ensure!(unsafe{SendMessageW(view,WM_CONTEXTMENU,Some(WPARAM(view.0 as usize)),Some(position))}.0==0,"view did not consume menu request");
                let dispatch_microseconds=started.elapsed().as_micros()as u64;
                ensure!(super::context_menu::request_active(runner.hwnd),"main view bypassed async menu worker");
                let mut pid=None;
                let deadline=Instant::now()+Duration::from_secs(15);
                while super::context_menu::request_active(runner.hwnd){
                    if let Some(value)=super::context_menu::active_pid(runner.hwnd){pid=Some(value);ensure!(value!=std::process::id(),"extension ran in main process");}
                    pump(10);ensure!(Instant::now()<deadline,"isolated menu result did not settle");
                }
                let recorded=crate::context_menu_process::read_recent_diagnostics();
                let pid=pid.or_else(||recorded["current"]["worker_pid"].as_u64().map(|value|value as u32)).context("async worker PID not recorded")?;
                ensure!(pid!=std::process::id(),"extension ran in main process");
                let after=zsdesk_windows::shell::menu_diagnostics();
                ensure!(before.presenter_creations==after.presenter_creations&&before.last_public_populate_microseconds==after.last_public_populate_microseconds,
                    "modern/classic menu constructed extension state in the main process");
                ensure!(unsafe{IsWindow(Some(runner.hwnd))}.as_bool()&&unsafe{IsWindow(Some(view))}.as_bool(),"extension crash closed main view");
                if crash{let guard=runner.slot.borrow();let status=&guard.as_ref().unwrap().status;
                    ensure!(status.contains("系统扩展菜单异常退出")&&status.contains("0xC0000005")&&status.contains("文件管理窗口可继续使用"),
                        "crash did not preserve the process failure code and recovery message: {status}");}
                rows.push(serde_json::json!({"modern_preference":modern,"target":if selected{"selection"}else{"background"},
                    "injected_crash":crash,"child_pid":pid,"parent_survived":true,"dispatch_microseconds":dispatch_microseconds,
                    "main_presenter_or_extension_created":false,"worker_rendering_exercised":false}));
            }
        }
        // Real filesystem background menus must keep view commands. ShellNew
        // completion must return to this visible view, including inline edit.
        let mut cases=vec![("background-menu","menu-dismiss",false),("view-details","background-details",false),
            ("new-menu","menu-dismiss",true),("new-folder","new-folder",true),("new-text","new-text",true),
            ("background-new-folder","new-folder",false),("background-new-text","new-text",false)];
        if modern{cases.push(("modern-background-new-folder","modern-owner-new-folder",false));}
        for (case,probe,command_bar) in cases {
            println!("context regression: modern={modern}, case={case}");
            let _probe=Environment::set("ZSDESK_MENU_TEST_PROBE",probe);
            let before:std::collections::HashSet<_>=fs::read_dir(&folder)?.map(|entry|entry.unwrap().path()).collect();
            {
                let mut slot=runner.slot.borrow_mut();let state=slot.as_mut().unwrap();
                state.browser.as_ref().unwrap().select_all(false);
                if probe=="background-details"{state.browser.as_ref().unwrap().set_view_mode(1,48)?;}
            }
            pump(30);
            if command_bar {
                unsafe{runner.slot.borrow_mut().as_mut().unwrap().command(zsdesk_ui::chrome::CommandId::New);}
            }else{
                let view=runner.slot.borrow().as_ref().unwrap().browser.as_ref().unwrap().view_window().unwrap();
                unsafe{SendMessageW(view,WM_CONTEXTMENU,Some(WPARAM(view.0 as usize)),Some(LPARAM(-1)));}
            }
            ensure!(super::context_menu::request_active(runner.hwnd),"filesystem/new menu did not reach the worker");
            let deadline=Instant::now()+Duration::from_secs(15);
            while super::context_menu::request_active(runner.hwnd){pump(10);ensure!(Instant::now()<deadline,"filesystem/new menu did not settle");}
            let diagnostic=crate::context_menu_process::read_recent_diagnostics();let current=&diagnostic["current"];
            ensure!(current["error"].is_null(),"filesystem/new menu failed ({probe}, modern={modern}): {current}");
            if probe=="menu-dismiss"{ensure!(current["timing"]["menu_open_observation"].is_string(),"filesystem popup not presented");}
            if probe=="background-details"{ensure!(runner.slot.borrow().as_ref().unwrap().browser.as_ref().unwrap().view_mode().is_some_and(|(mode,_)|mode==4),"filesystem view command lost");}
            if probe.starts_with("new-")||probe=="modern-owner-new-folder"{
                let added:Vec<_>=fs::read_dir(&folder)?.map(|entry|entry.unwrap().path()).filter(|path|!before.contains(path)).collect();
                ensure!(added.len()==1,"expected one newly created item: {added:?}");
                let deadline=Instant::now()+Duration::from_secs(10);
                loop {pump(25);let slot=runner.slot.borrow();let state=slot.as_ref().unwrap();
                    let selected=state.browser.as_ref().unwrap().selection_paths();
                    if selected.iter().any(|path|PathBuf::from(path)==added[0])&&text_input_window(unsafe{GetFocus()}){break;}
                    ensure!(Instant::now()<deadline,"created item was not selected for inline rename ({probe}): selected={selected:?}");
                }
                unsafe{SendMessageW(GetFocus(),WM_KEYDOWN,Some(WPARAM(VK_ESCAPE.0 as usize)),Some(LPARAM(0)));}pump(30);
                if added[0].is_dir(){fs::remove_dir(&added[0])?;}else{fs::remove_file(&added[0])?;}
                unsafe{runner.slot.borrow().as_ref().unwrap().browser.as_ref().unwrap().current_view()?.Refresh()?;}pump(120);
            }
            rows.push(serde_json::json!({"modern_preference":modern,"target":"filesystem/new","case":case,"probe":probe,"diagnostic":current}));
        }
        // Every new tab creates another Shell view. Repeat real selection
        // menus across tab switches and both presentation modes.
        for cycle in 0..4{
            unsafe{runner.slot.borrow_mut().as_mut().unwrap().new_tab(ShellLocation::Path(folder.clone()));}
            let deadline=Instant::now()+Duration::from_secs(12);
            loop{pump(25);if runner.slot.borrow().as_ref().is_some_and(|s|s.pending.is_none()&&s.browser.as_ref().is_some_and(|b|b.status().items==3)){break;}
                ensure!(Instant::now()<deadline,"new tab view did not settle");}
            let _probe=Environment::set("ZSDESK_MENU_TEST_PROBE","menu-dismiss");
            let view={let slot=runner.slot.borrow();let b=slot.as_ref().unwrap().browser.as_ref().unwrap();b.select_all(true);b.view_window().unwrap()};
            pump(30);unsafe{SendMessageW(view,WM_CONTEXTMENU,Some(WPARAM(view.0 as usize)),Some(LPARAM(-1)));}
            ensure!(super::context_menu::request_active(runner.hwnd),"new tab context request lost");
            let deadline=Instant::now()+Duration::from_secs(15);
            while super::context_menu::request_active(runner.hwnd){pump(10);ensure!(Instant::now()<deadline,"new tab menu stalled");}
            let diagnostic=crate::context_menu_process::read_recent_diagnostics();let current=&diagnostic["current"];
            ensure!(current["error"].is_null()&&current["timing"]["menu_open_observation"].is_string(),"new tab menu missing: {current}");
            rows.push(serde_json::json!({"modern_preference":modern,"target":"new-tab-selection","cycle":cycle,"diagnostic":current}));
        }
        // A virtual root has no filesystem path; render its actual background
        // menu repeatedly, including after a previous worker has been reused.
        unsafe{runner.slot.borrow_mut().as_mut().unwrap().navigate_to(ShellLocation::ThisPc);}
        let deadline=Instant::now()+Duration::from_secs(12);
        loop {pump(20);let slot=runner.slot.borrow();let state=slot.as_ref().unwrap();
            if state.pending.is_none()&&state.current_location()==&ShellLocation::ThisPc{break;}
            ensure!(Instant::now()<deadline,"This PC navigation did not settle");
        }
        for cycle in 0..5 {
            let command=cycle==4;
            let _probe=Environment::set("ZSDESK_MENU_TEST_PROBE",if command{"background-details"}else{"menu-dismiss"});
            if command{runner.slot.borrow().as_ref().unwrap().browser.as_ref().unwrap().set_view_mode(1,48)?;pump(20);}
            let view={let slot=runner.slot.borrow();let browser=slot.as_ref().unwrap().browser.as_ref().unwrap();browser.select_all(false);browser.view_window().context("This PC view")?};
            let mut bounds=RECT::default();unsafe{GetWindowRect(view,&mut bounds)?;}
            let position=LPARAM((((bounds.left+80)as u16 as u32)|(((bounds.bottom-60)as u16 as u32)<<16))as isize);
            unsafe{SendMessageW(view,WM_CONTEXTMENU,Some(WPARAM(view.0 as usize)),Some(position));}
            ensure!(super::context_menu::request_active(runner.hwnd),"This PC background did not reach its worker");
            let deadline=Instant::now()+Duration::from_secs(15);
            while super::context_menu::request_active(runner.hwnd){pump(10);ensure!(Instant::now()<deadline,"This PC menu did not dismiss");}
            let diagnostic=crate::context_menu_process::read_recent_diagnostics();let current=&diagnostic["current"];
            ensure!(current["error"].is_null(),"This PC background menu failed (modern={modern}, cycle={cycle}): {current}");
            if command{ensure!(runner.slot.borrow().as_ref().unwrap().browser.as_ref().unwrap().view_mode().is_some_and(|(mode,_)|mode==4),"background view command did not reach the main view: {current}");}
            else{ensure!(current["timing"]["menu_open_observation"].is_string(),"This PC background never displayed a menu (modern={modern}, cycle={cycle}): {current}");}
            rows.push(serde_json::json!({"modern_preference":modern,"target":"this-pc-background","cycle":cycle,"diagnostic":current}));
        }
        Ok(())})();
        if let Err(error)=&exercised{eprintln!("context regression failed before teardown (modern={modern}): {error:#}");}
        drop(runner);pump(30);
        crate::context_menu_process::shutdown_idle_worker();
        let deadline=Instant::now()+Duration::from_secs(5);
        while crate::context_menu_process::pending_workers()>0&&Instant::now()<deadline{pump(10);}
        ensure!(crate::context_menu_process::pending_workers()==0,"context worker remains running");
        exercised?;
    }
    drop(files);
    Ok(serde_json::json!({"selection_bytes":24u64*1024*1024*1024,"files":records,"routes":rows,
        "main_delegate_installed":true,"fixtures_removed":true,"rendering_scope":"classic/modern filesystem and This PC backgrounds; ShellNew popup and document/folder creation with visible inline rename; selection fault containment"}))
}

#[test]
#[ignore="Main view classic/modern selection/background isolation; requires ZSDESK_NATIVE_TEST_OUTPUT"]
fn native_main_window_background_and_selection_menu_isolation()->Result<()>{
    let root=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("output")?).join("main-context-menu");ensure!(root.is_absolute(),"absolute output required");fs::create_dir_all(&root)?;
    let before=unsafe{GetForegroundWindow()};let name=format!("ZSDesk.MainContextMenu.{}",std::process::id());let desktop=unsafe{CreateDesktopW(wide(&name).as_ptr(),std::ptr::null(),std::ptr::null(),0,0x000f00ff,std::ptr::null())};ensure!(!desktop.is_null(),"CreateDesktop failed");let raw=desktop as usize;let destination=root.clone();
    let result=thread::spawn(move||->Result<serde_json::Value>{ensure!(unsafe{SetThreadDesktop(raw as*mut c_void)}!=0,"SetThreadDesktop failed");exercise(&destination,&name)}).join().map_err(|_|anyhow::anyhow!("main quick menu test panicked")).and_then(|value|value);
    // Shell releases auxiliary STA windows after the owning UI thread exits.
    let deadline=Instant::now()+Duration::from_secs(3);
    let closed=loop{if unsafe{CloseDesktop(desktop)}!=0{break true;}if Instant::now()>=deadline{break false;}thread::sleep(Duration::from_millis(20));};
    let current=unsafe{GetForegroundWindow()};let mut process=0;unsafe{GetWindowThreadProcessId(current,Some(&mut process));}
    let isolated=process!=std::process::id();let report=serde_json::json!({"state":if result.is_ok()&&closed&&isolated{"passed"}else{"failed"},"checks":result.as_ref().map_err(|error|format!("{error:#}")),"desktop_closed":closed,"foreground_unchanged":before==current,"test_did_not_activate":isolated});
    fs::write(root.join("verification.current.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");result?;ensure!(closed&&isolated,"desktop cleanup/focus failed");Ok(())
}

#[test]
#[ignore="Native/classic handoff with source selection and screen-anchor verification on a private desktop"]
fn native_more_options_preserves_source_selection_and_anchor()->Result<()>{
    use windows::core::Interface;
    use windows::Win32::UI::Shell::IFolderView2;
    use zsdesk_windows::shell::{file_transfer,menu::{self,ViewMenuTarget}};
    fn keys(pidls:&[Vec<u8>])->Result<Vec<Vec<u16>>>{
        if pidls.is_empty(){return Ok(Vec::new());}
        let mut values=file_transfer::paths_from_pidls(pidls)?.iter().map(|path|ExplorerBrowserHost::search_path_key(path)).collect::<Vec<_>>();
        values.sort();Ok(values)
    }
    fn request(root:&Path,owner:HWND,point:POINT,targets:&[Vec<u8>])->Result<Option<(PathBuf,serde_json::Value)>>{
        let wanted=keys(targets)?;
        for entry in fs::read_dir(root)?.flatten(){
            let path=entry.path().join("request.json");let Ok(bytes)=fs::read(path)else{continue;};
            let Ok(value)=serde_json::from_slice::<serde_json::Value>(&bytes)else{continue;};
            if value["owner"].as_i64()!=Some(owner.0 as isize as i64)||value["point"]!=serde_json::json!([point.x,point.y]){continue;}
            let Ok(pidls)=serde_json::from_value::<Vec<Vec<u8>>>(value["pidls"].clone())else{continue;};
            if keys(&pidls)?==wanted{return Ok(Some((entry.path(),value)));}
        }Ok(None)
    }
    fn status(workspace:&Path,token:&str)->Option<serde_json::Value>{
        let bytes=fs::read(workspace.join("status.json")).ok()?;
        let value:serde_json::Value=serde_json::from_slice(&bytes).ok()?;
        (value["token"].as_str()==Some(token)).then_some(value)
    }
    #[derive(Default)]struct Popup{pid:u32,rectangle:Option<RECT>}
    unsafe extern "system" fn find_classic(window:HWND,data:LPARAM)->windows::core::BOOL{
        let found=unsafe{&mut*(data.0 as*mut Popup)};let mut pid=0;unsafe{GetWindowThreadProcessId(window,Some(&mut pid));}
        if pid==found.pid&&unsafe{IsWindowVisible(window)}.as_bool(){
            let mut class=[0u16;64];let length=unsafe{GetClassNameW(window,&mut class)}.max(0)as usize;
            if String::from_utf16_lossy(&class[..length])=="#32768"{let mut rect=RECT::default();if unsafe{GetWindowRect(window,&mut rect)}.is_ok(){found.rectangle=Some(rect);}}
        }true.into()
    }
    let root=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("output")?).join("more-options-source");fs::create_dir_all(&root)?;
    let name=format!("ZSDesk.MoreOptionsSource.{}",std::process::id());
    let desktop=unsafe{CreateDesktopW(wide(&name).as_ptr(),std::ptr::null(),std::ptr::null(),0,0x000f00ff,std::ptr::null())};ensure!(!desktop.is_null(),"private desktop");
    let raw=desktop as usize;let target=root.clone();let foreground=unsafe{GetForegroundWindow()};
    let result=thread::spawn(move||->Result<serde_json::Value>{
        ensure!(unsafe{SetThreadDesktop(raw as*mut c_void)}!=0,"private desktop attach");unsafe{OleInitialize(None)}?;let _apartment=Apartment;
        struct Workers;impl Drop for Workers{fn drop(&mut self){crate::context_menu_process::shutdown_idle_worker();let until=Instant::now()+Duration::from_secs(5);while crate::context_menu_process::pending_workers()>0&&Instant::now()<until{pump(10);}}}let _workers=Workers;
        let _desktop=Environment::set("ZSDESK_MENU_TEST_DESKTOP",&name);
        let workers=target.join("workers");fs::create_dir_all(&workers)?;let _root=Environment::set("ZSDESK_MENU_ROOT",&workers);
        let _probe=Environment::set("ZSDESK_MENU_TEST_PROBE","modern-expand");
        let files=target.join("files");fs::create_dir_all(&files)?;
        for name in ["one.txt","two.txt","three.txt"]{fs::write(files.join(name),b"menu handoff fixture")?;}
        let runner=create(&target,&files,true)?;let mut checks=Vec::new();
        for count in [0usize,1,2]{for keyboard in [false,true]{
            let (view,window)={let slot=runner.slot.borrow();let browser=slot.as_ref().unwrap().browser.as_ref().unwrap();
                browser.select_all(false);if count>0{browser.select_children(&["one.txt","two.txt"][..count].iter().map(|name|std::ffi::OsString::from(*name)).collect::<Vec<_>>())?;}
                (unsafe{browser.current_view()}?,browser.view_window().context("source view")?)};
            pump(35);
            let expected=super::context_menu::selected_identities(&view)?;ensure!(expected.len()==count,"fixture selection count");
            let folder:IFolderView2=view.cast()?;let focused=unsafe{folder.GetFocusedItem()}.ok();
            let mut bounds=RECT::default();unsafe{GetWindowRect(window,&mut bounds)}?;
            let mouse=POINT{x:bounds.left+150,y:bounds.top+70};
            let position=if keyboard{LPARAM(-1)}else{LPARAM(((mouse.x as u32&0xffff)|((mouse.y as u32&0xffff)<<16))as isize)};
            let anchor=menu::context_menu_point(&view,position)?;
            let target_kind=if count==0{ViewMenuTarget::Background}else{ViewMenuTarget::Selection};
            let targets=if count==0{vec![zsdesk_windows::shell::pidl::location_pidl(&ShellLocation::Path(files.clone())).context("background PIDL")?]}else{expected.clone()};
            super::context_menu::show(window,&view,window,position,target_kind)?;
            ensure!(super::context_menu::request_active(runner.hwnd),"menu request not active");
            let mut frozen=None;let mut popup=None;let mut restored=false;let mut cleared=false;let mut worker_owner=None;let mut worker_pid=None;
            let until=Instant::now()+Duration::from_secs(20);
            while super::context_menu::request_active(runner.hwnd){
                if frozen.is_none(){frozen=request(&workers,runner.hwnd,anchor,&targets)?;}
                if let Some((workspace,request))=&frozen{
                    if let Some(value)=request["token"].as_str().and_then(|token|status(workspace,token)){
                        if let Some(owner)=value["window"].as_u64().filter(|owner|*owner!=0){worker_owner=Some(owner);}
                    }
                }
                if let Some(pid)=super::context_menu::active_pid(runner.hwnd){
                    worker_pid=Some(pid);
                    let mut found=Popup{pid,..Default::default()};let _=unsafe{EnumWindows(Some(find_classic),LPARAM((&mut found as*mut Popup)as isize))};
                    if let Some(rect)=found.rectangle{popup=Some(rect);
                        if count>0&&!cleared{
                            // Model DefView's asynchronous highlight clear at
                            // the native/classic transition, without user input.
                            unsafe{view.SelectItem(std::ptr::null(),windows::Win32::UI::Shell::SVSI_DESELECTOTHERS.0 as u32)}?;
                            ensure!(super::context_menu::selected_identities(&view)?.is_empty(),"clear fixture did not clear");
                            cleared=true;super::context_menu::poll(runner.hwnd);
                            restored=keys(&super::context_menu::selected_identities(&view)?)?==keys(&expected)?;
                        }
                    }
                }
                pump(5);ensure!(Instant::now()<until,"menu handoff timed out");
            }
            let (workspace,frozen)=frozen.context("immutable worker request not observed")?;
            ensure!(frozen["view_context"]["keyboard"]==keyboard,"keyboard origin changed");
            let token=frozen["token"].as_str().context("request token")?;
            let until=Instant::now()+Duration::from_secs(5);
            let finished=loop{
                if let Some(value)=status(&workspace,token){if matches!(value["phase"].as_str(),Some("idle"|"done")){break value;}}
                ensure!(Instant::now()<until,"request {token} has no matching terminal status");pump(5);
            };
            ensure!(finished["pid"].as_u64()==worker_pid.map(u64::from)&&finished["error"].is_null()&&finished["before_invoke"]==false&&finished["outcome"]=="Dismissed","expansion executed or failed: {finished}");
            // The retained diagnostic intentionally contains only the first
            // 24 stderr lines. A reused worker's later menus require its own
            // token-matched terminal status and the full bounded fixture log.
            let log=workspace.join("worker.stderr.log");ensure!(fs::metadata(&log)?.len()<=1024*1024,"fixture log exceeds bound");
            let all_stderr=fs::read_to_string(log)?;
            let stderr=&all_stderr[all_stderr.rfind("expand anchor=").context("current expansion trace missing")?..];
            let classic=stderr.lines().filter(|line|line.starts_with("classic-popup tracking ")).last().unwrap_or_default();
            let expanded=stderr.lines().filter(|line|line.starts_with("expand anchor=")).last().unwrap_or_default();
            ensure!(classic==format!("classic-popup tracking owner={} anchor={},{}",worker_owner.context("worker owner window not observed")?,anchor.x,anchor.y),"classic menu lost original owner/anchor: {stderr}");
            ensure!(expanded.contains(&format!("anchor={},{}",anchor.x,anchor.y))&&expanded.ends_with(&format!("callback={},{}",anchor.x.saturating_add(43),anchor.y.saturating_add(89))),"offset expansion callback not exercised: {stderr}");
            ensure!(popup.is_some(),"classic popup was not observed");
            ensure!(count==0||cleared&&restored,"source highlight was not restored");
            ensure!(keys(&super::context_menu::selected_identities(&view)?)?==keys(&expected)?,"source selection changed across expansion");
            if count>0{ensure!(unsafe{folder.GetFocusedItem()}.ok()==focused,"focused source row changed");}
            let rect=popup.unwrap();checks.push(serde_json::json!({"selected":count,"keyboard":keyboard,"request_token":token,"worker_pid":worker_pid,"terminal_phase":finished["phase"],"anchor":[anchor.x,anchor.y],"classic_rect":[rect.left,rect.top,rect.right,rect.bottom],"visual_selection_restored":restored,"target_pidls_verified":true,"command_executed":false,"rebuilt_menu":stderr.contains("rebuilt=true")}));
        }}
        // A real new selection must not be overwritten by the saved selection.
        let _probe=Environment::set("ZSDESK_MENU_TEST_PROBE","dismiss");
        let (view,window)={let slot=runner.slot.borrow();let browser=slot.as_ref().unwrap().browser.as_ref().unwrap();browser.select_children(&["one.txt".into()])?;(unsafe{browser.current_view()}?,browser.view_window().unwrap())};
        super::context_menu::show(window,&view,window,LPARAM(-1),ViewMenuTarget::Selection)?;
        {let slot=runner.slot.borrow();slot.as_ref().unwrap().browser.as_ref().unwrap().select_children(&["three.txt".into()])?;}
        let replacement=super::context_menu::selected_identities(&view)?;super::context_menu::poll(runner.hwnd);
        ensure!(keys(&super::context_menu::selected_identities(&view)?)?==keys(&replacement)?,"replacement selection was overwritten");
        super::context_menu::close(runner.hwnd);
        // An observed source input cancels restoration of a now-empty selection.
        {let slot=runner.slot.borrow();slot.as_ref().unwrap().browser.as_ref().unwrap().select_children(&["one.txt".into()])?;}
        super::context_menu::show(window,&view,window,LPARAM(-1),ViewMenuTarget::Selection)?;
        unsafe{PostMessageW(Some(runner.hwnd),WM_LBUTTONDOWN,WPARAM(0),LPARAM(0))}?;let mut message=MSG::default();
        ensure!(unsafe{PeekMessageW(&mut message,Some(runner.hwnd),WM_LBUTTONDOWN,WM_LBUTTONDOWN,PM_REMOVE)}.as_bool(),"source input not observed");
        unsafe{view.SelectItem(std::ptr::null(),windows::Win32::UI::Shell::SVSI_DESELECTOTHERS.0 as u32)}?;super::context_menu::poll(runner.hwnd);
        ensure!(super::context_menu::selected_identities(&view)?.is_empty()&&!super::context_menu::request_active(runner.hwnd),"user-cleared selection was restored");
        drop(runner);pump(20);Ok(serde_json::json!({"cases":checks,"replacement_selection_preserved":true,"user_clear_not_restored":true,"clipboard_untouched":true}))
    }).join().map_err(|_|anyhow::anyhow!("source menu regression panicked")).and_then(|value|value);
    let closed=unsafe{CloseDesktop(desktop)}!=0;
    let report=serde_json::json!({"state":if result.is_ok()&&closed{"passed"}else{"failed"},"checks":result.as_ref().map_err(|error|format!("{error:#}")),"desktop_closed":closed,"foreground_unchanged":foreground==unsafe{GetForegroundWindow()}});
    fs::write(root.join("verification.current.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");result?;ensure!(closed,"private desktop retained");Ok(())
}

#[test]
#[ignore="Blank double-click navigation and folder-search scope in a private desktop"]
fn native_blank_double_click_and_folder_search_scope()->Result<()>{
    let root=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("output")?).join("blank-doubleclick");fs::create_dir_all(&root)?;
    let name=format!("ZSDesk.BlankClick.{}",std::process::id());let desktop=unsafe{CreateDesktopW(wide(&name).as_ptr(),std::ptr::null(),std::ptr::null(),0,0x000f00ff,std::ptr::null())};ensure!(!desktop.is_null(),"private desktop");
    let raw=desktop as usize;let target=root.clone();
    let result=thread::spawn(move||->Result<()>{
        ensure!(unsafe{SetThreadDesktop(raw as*mut c_void)}!=0,"private desktop attach");unsafe{OleInitialize(None)?;}let _apartment=Apartment;
        let folder=target.join("files");fs::create_dir_all(&folder)?;for name in ["one.txt","two.txt","three.txt"]{fs::write(folder.join(name),b"blank click fixture")?;}
        let runner=create(&target,&folder,false)?;super::pane_input::native_blank_double_click(&runner.slot,&target,pump)?;
        use zsdesk_core::model::explorer::NewTabLocation;
        let fixed=target.join("fixed-tab");fs::create_dir_all(&fixed)?;
        for (policy,expected) in [(NewTabLocation::CurrentFolder,ShellLocation::Path(folder.clone())),
            (NewTabLocation::ThisPc,ShellLocation::ThisPc),(NewTabLocation::Folder(fixed.clone()),ShellLocation::Path(fixed))]{
            {let slot=runner.slot.borrow();let state=slot.as_ref().unwrap();storage::save_new_tab_location(&state.config_path,&policy)?;}
            let before=runner.slot.borrow().as_ref().unwrap().session.tabs.len();
            unsafe{SendMessageW(runner.hwnd,WM_COMMAND,Some(WPARAM(Command::NewTab.id()as usize)),Some(LPARAM(0)));}
            let deadline=Instant::now()+Duration::from_secs(12);
            loop{pump(20);let slot=runner.slot.borrow();let state=slot.as_ref().unwrap();
                if state.pending.is_none()&&state.queued.is_none()&&state.current_location()==&expected{
                    ensure!(state.session.tabs.len()==before+1,"new-tab command did not add exactly one tab");break;}
                ensure!(Instant::now()<deadline,"new-tab policy did not navigate to {}",expected.address());}
        }
        drop(runner);pump(30);Ok(())
    }).join().map_err(|_|anyhow::anyhow!("blank navigation panicked")).and_then(|result|result);
    let closed=unsafe{CloseDesktop(desktop)}!=0;
    let report=serde_json::json!({"state":if result.is_ok()&&closed{"passed"}else{"failed"},"checks":result.as_ref().map_err(|error|format!("{error:#}")),"desktop_closed":closed,"blank_double_click_enabled_and_disabled":result.is_ok(),"search_scope_follows_folder":result.is_ok(),"new_tab_this_pc_current_and_fixed_folder":result.is_ok()});
    fs::write(root.join("verification.current.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");result?;ensure!(closed,"desktop retained");Ok(())
}
