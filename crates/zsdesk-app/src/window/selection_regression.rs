//! Opt-in native rendering regression with an isolated profile and offscreen windows.
use super::*;
use std::{fs, path::Path, thread, time::{Duration, Instant}};
use anyhow::ensure;
use serde_json::{json, Value};
use windows::Win32::Foundation::PROPERTYKEY;
use windows::{core::Interface, Win32::UI::Shell::{IFolderView2, IShellView, IColumnManager, CM_COLUMNINFO, CM_MASK_WIDTH,
    FVM_DETAILS, SVGIO_ALLVIEW, SVGIO_SELECTION, SVSI_DESELECT, SVSI_SELECT,
    SVSI_FOCUSED, SVSI_DESELECTOTHERS}};

#[link(name = "user32")]
unsafe extern "system" { fn PrintWindow(window: HWND, dc: HDC, flags: u32) -> i32; }

struct Apartment;
impl Drop for Apartment { fn drop(&mut self) { unsafe { OleUninitialize(); } } }
struct OwnedWindow(HWND);
impl Drop for OwnedWindow {
    fn drop(&mut self) { unsafe { let _ = ShowWindow(self.0, SW_HIDE); let _ = DestroyWindow(self.0); } }
}

fn pump(milliseconds: u64) {
    let end = Instant::now() + Duration::from_millis(milliseconds);
    while Instant::now() < end {
        for _ in 0..512 {
            let mut message = MSG::default();
            if !unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() { break; }
            if message.message != WM_QUIT {
                unsafe { let _ = TranslateMessage(&message); DispatchMessageW(&message); }
            }
        }
        thread::sleep(Duration::from_millis(4));
    }
}

fn view(slot: &Slot) -> Result<IShellView> {
    let borrowed = slot.borrow();
    let state = borrowed.as_ref().context("window state")?;
    unsafe { state.browser.as_ref().context("browser")?.current_view() }.map_err(Into::into)
}

fn wait_navigation(slot: &Slot) -> Result<()> {
    let start = Instant::now();
    loop {
        pump(40);
        let ready = slot.borrow().as_ref().is_some_and(|state| state.pending.is_none() && state.shown_tab.is_some());
        if ready && view(slot)?.cast::<IFolderView2>().is_ok() { return Ok(()); }
        ensure!(start.elapsed() < Duration::from_secs(12), "navigation timeout");
    }
}

fn fixture(root: &Path) -> Result<()> {
    fs::create_dir_all(root)?;
    for name in ["01-alpha.txt", "02-unicode-文件.txt", "03-space name.txt"] {
        let file = root.join(name);
        if file.exists() { ensure!(fs::read(&file)? == b"ZSDesk selection regression fixture", "fixture changed"); }
        else { fs::write(&file, b"ZSDesk selection regression fixture")?; }
    }
    Ok(())
}

fn clear_selection(shell: &IShellView) -> Result<()> {
    let folder: IFolderView2 = shell.cast()?;
    for index in 0..unsafe { folder.ItemCount(SVGIO_ALLVIEW)? } {
        unsafe { folder.SelectItem(index, SVSI_DESELECT.0 as u32)?; }
    }
    Ok(())
}

fn select(shell: &IShellView, index: i32, replace: bool) -> Result<()> {
    let folder: IFolderView2 = shell.cast()?;
    let flags = SVSI_SELECT.0 | SVSI_FOCUSED.0 | if replace { SVSI_DESELECTOTHERS.0 } else { 0 };
    unsafe { folder.SelectItem(index, flags as u32)?; }
    Ok(())
}

fn capture(shell: &IShellView, root: &Path, name: &str) -> Result<(Value, Vec<u8>)> {
    pump(160);
    let window = unsafe { shell.GetWindow()? };
    // These windows are deliberately outside the visible desktop. DefView may
    // defer a buffer repaint there even after its selection model is current;
    // synchronize this owned view before reading pixels through PrintWindow.
    use windows::Win32::Graphics::Gdi::{RedrawWindow,RDW_INVALIDATE,RDW_UPDATENOW,RDW_ALLCHILDREN};
    ensure!(unsafe{RedrawWindow(Some(window),None,None,RDW_INVALIDATE|RDW_UPDATENOW|RDW_ALLCHILDREN)}.as_bool(),"native view repaint failed");
    pump(40);
    let mut rect = RECT::default();
    unsafe { GetClientRect(window, &mut rect)?; }
    let (width, height) = (rect.right, rect.bottom);
    ensure!(width > 100 && height > 100 && width * height < 4_000_000, "unexpected view bounds");
    let surface = Surface::new(width, height).context("capture surface")?;
    let bytes = unsafe { std::slice::from_raw_parts_mut(surface.bits, (width * height * 4) as usize) };
    bytes.fill(127);
    ensure!(unsafe { PrintWindow(window, surface.dc, 2) } != 0, "PrintWindow failed");
    for pixel in bytes.chunks_exact_mut(4) { pixel[3] = 255; }
    let pixels = bytes.to_vec();
    fs::write(root.join(format!("{name}.bgra")), &pixels)?;
    let folder: IFolderView2 = shell.cast()?;
    let record = json!({"frame": name, "width": width, "height": height,
        "selected": unsafe { folder.ItemCount(SVGIO_SELECTION)? },
        "focus": unsafe { GetFocus() }.0 as usize, "view_window": window.0 as usize,"explicit_native_repaint":true});
    Ok((record, pixels))
}

// A focus border or text-color change is insufficient: require a rectangular fill.
fn filled_rows(before: &[u8], after: &[u8], width: usize) -> usize {
    before.chunks_exact(width * 4).zip(after.chunks_exact(width * 4)).filter(|(a, b)| {
        let mut run = 0;
        let mut previous = None;
        for (left, right) in a.chunks_exact(4).zip(b.chunks_exact(4)) {
            let pair = (&left[..3], &right[..3]);
            if pair.0 == pair.1 { run = 0; previous = None; continue; }
            if previous == Some(pair) { run += 1; } else { run = 1; previous = Some(pair); }
            if run >= 40 { return true; }
        }
        false
    }).count()
}

fn snapshot(slot:&Slot)->Result<zsdesk_core::model::explorer::ShellViewState> {
    let borrowed=slot.borrow();
    let browser=borrowed.as_ref().context("state")?.browser.as_ref().context("browser")?;
    unsafe {browser.capture_view()}.context("view snapshot")
}

fn configure_layout(slot:&Slot,alternate:bool)->Result<zsdesk_core::model::explorer::ShellViewState> {
    use zsdesk_windows::shell::commands::{PKEY_NAME,PKEY_MODIFIED,PKEY_SIZE,PKEY_TYPE};
    let shell=view(slot)?;let folder:IFolderView2=shell.cast()?;let columns:IColumnManager=shell.cast()?;
    let keys=if alternate{[PKEY_NAME,PKEY_TYPE,PKEY_MODIFIED,PKEY_SIZE]}else{[PKEY_NAME,PKEY_SIZE,PKEY_MODIFIED,PKEY_TYPE]};
    let widths=if alternate{[185,135,215,105]}else{[275,125,175,145]};
    unsafe {columns.SetColumns(&keys)?;}
    for (key,width) in keys.iter().zip(widths) {
        let info=CM_COLUMNINFO{cbSize:size_of::<CM_COLUMNINFO>()as u32,dwMask:CM_MASK_WIDTH.0 as u32,uWidth:width,..Default::default()};
        unsafe {columns.SetColumnInfo(key,&info)?;}
    }
    let group=if alternate{PROPERTYKEY::default()}else{PKEY_TYPE};
    unsafe {folder.SetGroupBy(&group,true)?;folder.SetSortColumns(&[windows::Win32::UI::Shell::SORTCOLUMN{
        propkey:if alternate{PKEY_MODIFIED}else{PKEY_NAME},direction:if alternate{windows::Win32::UI::Shell::SORT_ASCENDING}else{windows::Win32::UI::Shell::SORT_DESCENDING}}])?;}
    pump(120);let saved=snapshot(slot)?;
    ensure!(saved.columns.len()==4 && !saved.sort_columns.is_empty() && saved.grouping.is_some(),"layout metadata was not captured");
    Ok(saved)
}

fn same_layout(actual:&zsdesk_core::model::explorer::ShellViewState,expected:&zsdesk_core::model::explorer::ShellViewState)->bool {
    actual.columns==expected.columns && actual.sort_columns==expected.sort_columns && actual.grouping==expected.grouping
        && actual.column_dpi==expected.column_dpi
}

fn check_same_folder_tabs_and_preview(slot:&Slot)->Result<Value> {
    let first_index=slot.borrow().as_ref().unwrap().session.active_tab;
    let location=slot.borrow().as_ref().unwrap().current_location().clone();
    let first=configure_layout(slot,false)?;
    unsafe {slot.borrow_mut().as_mut().unwrap().new_tab(location);}
    wait_navigation(slot)?;
    let second_index=slot.borrow().as_ref().unwrap().session.active_tab;
    let second=configure_layout(slot,true)?;
    ensure!(!same_layout(&first,&second),"two layouts must differ");
    unsafe {slot.borrow_mut().as_mut().unwrap().select_tab(first_index);}
    wait_navigation(slot)?;pump(160);
    ensure!(same_layout(&snapshot(slot)?,&first),"first same-directory tab layout was not restored");
    slot.borrow_mut().as_mut().unwrap().set_panes(true,false);
    wait_navigation(slot)?;pump(160);
    ensure!(same_layout(&snapshot(slot)?,&first),"preview opening changed column/sort/group state");
    slot.borrow_mut().as_mut().unwrap().set_panes(false,false);
    wait_navigation(slot)?;pump(160);
    ensure!(same_layout(&snapshot(slot)?,&first),"preview closing changed column/sort/group state");
    unsafe {slot.borrow_mut().as_mut().unwrap().select_tab(second_index);}
    wait_navigation(slot)?;pump(160);
    ensure!(same_layout(&snapshot(slot)?,&second),"second same-directory tab layout was not restored");
    Ok(json!({"state":"passed","same_directory_tabs":2,"preview_toggle_cycles":1,"visible_columns":4,
        "column_dpi":first.column_dpi,"column_order_width_sort_group_preserved":true}))
}

#[test]
#[ignore = "Requires a local DWM session and ZSDESK_NATIVE_TEST_OUTPUT; creates only offscreen windows"]
fn full_window_keeps_selection_background_after_navigation() -> Result<()> {
    let output = PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("set ZSDESK_NATIVE_TEST_OUTPUT")?);
    ensure!(output.is_absolute(), "output must be an absolute path");
    fs::create_dir_all(&output)?;
    let first = output.join("fixture-first");
    let second = output.join("fixture-second");
    fixture(&first)?;
    fixture(&second)?;
    let foreground = unsafe { GetForegroundWindow() };
    unsafe { OleInitialize(None)?; }
    let apartment = Apartment;
    unsafe { InitCommonControlsEx(&INITCOMMONCONTROLSEX { dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
        dwICC: ICC_BAR_CLASSES | ICC_STANDARD_CLASSES }) }.ok()?;
    let instance = HINSTANCE(unsafe { GetModuleHandleW(None)? }.0);
    let class = wide("ZSDesk.Native.SelectionRegression");
    let wc = WNDCLASSW { lpfnWndProc: Some(window_proc), hInstance: instance,
        lpszClassName: pcwstr(&class), ..Default::default() };
    unsafe { RegisterClassW(&wc); }
    let mut session = ExplorerSession::default();
    session.tabs = vec![ExplorerTab::new(1, ShellLocation::Path(first)), ExplorerTab::new(2, ShellLocation::Path(second))];
    session.next_id = 3;
    let slot = Box::new(RefCell::new(Some(WindowState::new(session, output.join("profile/explorer-session.json"), None, true))));
    let right = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) + GetSystemMetrics(SM_CXVIRTUALSCREEN) };
    let bottom = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) + GetSystemMetrics(SM_CYVIRTUALSCREEN) };
    let owner = OwnedWindow(unsafe { CreateWindowExW(WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        pcwstr(&class), windows::core::w!("Selection regression"), WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
        right + 1200, bottom + 1200, 1180, 760, None, None, Some(instance), Some((&*slot as *const Slot).cast()))? });
    unsafe { SetWindowPos(owner.0, None, 0, 0, 0, 0, SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE)?; }
    unsafe { slot.borrow_mut().as_mut().unwrap().create(owner.0)?; let _ = ShowWindow(owner.0, SW_SHOWNOACTIVATE); }
    let mut bounds = RECT::default();
    unsafe { GetWindowRect(owner.0, &mut bounds)?; }
    ensure!(bounds.left > right || bounds.top > bottom, "test window intersects the virtual screen");
    wait_navigation(&slot)?;
    let shell = view(&slot)?;
    let folder: IFolderView2 = shell.cast()?;
    slot.borrow().as_ref().unwrap().browser.as_ref().unwrap().set_view_mode(FVM_DETAILS.0,16)?;
    clear_selection(&shell)?;
    let (baseline, empty) = capture(&shell, &output, "unselected")?;
    let width = baseline["width"].as_u64().unwrap() as usize;
    select(&shell, 0, true)?;
    let (focused, focused_pixels) = capture(&shell, &output, "focused")?;
    let edit = slot.borrow().as_ref().unwrap().search_edit;
    unsafe { let _ = SetFocus(Some(edit)); }
    let (unfocused, unfocused_pixels) = capture(&shell, &output, "unfocused")?;
    select(&shell, 1, false)?;
    let (multiple, multiple_pixels) = capture(&shell, &output, "multiple")?;
    let selection_scan: Vec<Value> = (-1..=3).map(|start| {
        let mut index = -99;
        let status = unsafe { (folder.vtable().GetSelectedItem)(folder.as_raw(), start, &mut index) };
        json!({"start": start, "status": status.0, "index": index})
    }).collect();
    let item_sizes: Vec<Value> = (0..3).map(|index| {
        match unsafe { folder.Item(index) } {
            Ok(pointer) if !pointer.is_null() => {
                let bytes = unsafe { windows::Win32::UI::Shell::ILGetSize(Some(pointer)) };
                unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(pointer.cast())); }
                json!({"index": index, "bytes": bytes})
            }
            other => json!({"index": index, "result": format!("{other:?}")}),
        }
    }).collect();
    drop(folder);
    drop(shell);
    unsafe { slot.borrow_mut().as_mut().unwrap().select_tab(1); }
    let stored_selected = slot.borrow().as_ref().unwrap().session.tabs[0].view.as_ref().map(|state| state.selected.len());
    wait_navigation(&slot)?;
    unsafe { slot.borrow_mut().as_mut().unwrap().select_tab(0); }
    wait_navigation(&slot)?;
    let restored_shell = view(&slot)?;
    let (restored, restored_pixels) = capture(&restored_shell, &output, "restored")?;
    select(&restored_shell, 2, true)?;
    let (reselected, reselected_pixels) = capture(&restored_shell, &output, "reselected")?;
    let message = MSG { hwnd: unsafe { GetFocus() }, message: WM_KEYDOWN,
        wParam: WPARAM(VK_A.0 as usize), ..Default::default() };
    let shortcut_view = slot.borrow().as_ref().unwrap().keyboard_view(&message, true).context("file shortcut focus route")?;
    let action = keyboard::resolve(VK_A.0, keyboard::Modifiers { control: true, ..Default::default() }, true, false).unwrap();
    keyboard::execute_message(&shortcut_view, owner.0, action, &message)?;
    drop(shortcut_view);
    let (all_selected, all_pixels) = capture(&restored_shell, &output, "select-all")?;
    let all_ids=snapshot(&slot)?.selected;
    clear_selection(&restored_shell)?;
    select(&restored_shell,0,true)?;select(&restored_shell,1,false)?;pump(60);
    let original_ids=snapshot(&slot)?.selected;
    let action=keyboard::resolve(VK_A.0,keyboard::Modifiers{control:true,shift:true,alt:false},true,false).context("invert shortcut")?;
    keyboard::execute_message(&restored_shell,owner.0,action,&message)?;
    let (inverted,inverted_pixels)=capture(&restored_shell,&output,"inverted")?;
    let inverted_ids=snapshot(&slot)?.selected;
    let expected:Vec<_>=all_ids.into_iter().filter(|id|!original_ids.contains(id)).collect();
    let inversion_preserved=inverted_ids.len()==expected.len() && expected.iter().all(|id|inverted_ids.contains(id)) && inverted["selected"]==1;
    clear_selection(&restored_shell)?;
    let (cleared, cleared_pixels) = capture(&restored_shell, &output, "cleared")?;
    drop(restored_shell);
    unsafe { slot.borrow_mut().as_mut().unwrap().select_tab(1); }
    wait_navigation(&slot)?;
    unsafe { slot.borrow_mut().as_mut().unwrap().select_tab(0); }
    wait_navigation(&slot)?;
    let empty_shell = view(&slot)?;
    let (restored_empty, restored_empty_pixels) = capture(&empty_shell, &output, "restored-empty")?;
    for frame in [&focused, &unfocused, &multiple, &restored, &reselected, &all_selected, &inverted, &cleared, &restored_empty] {
        ensure!(frame["width"] == baseline["width"] && frame["height"] == baseline["height"], "capture dimensions changed");
    }
    let fills = json!({"focused": filled_rows(&empty, &focused_pixels, width),
        "unfocused": filled_rows(&empty, &unfocused_pixels, width),
        "multiple": filled_rows(&empty, &multiple_pixels, width),
        "restored": filled_rows(&empty, &restored_pixels, width),
        "reselected": filled_rows(&empty, &reselected_pixels, width),
        "select_all": filled_rows(&empty, &all_pixels, width),"inverted":filled_rows(&empty,&inverted_pixels,width)});
    let all_visible = fills.as_object().unwrap().values().all(|value| value.as_u64().unwrap_or(0) >= 6);
    let counts = stored_selected == Some(2) && focused["selected"] == 1 && unfocused["selected"] == 1 && multiple["selected"] == 2 && restored["selected"] == 2 && reselected["selected"] == 1 && all_selected["selected"] == 3 && cleared["selected"] == 0 && restored_empty["selected"] == 0;
    let single_rows = fills["focused"].as_u64().unwrap_or(0);
    let multiple_visible = ["multiple", "restored"].into_iter().all(|name|
        fills[name].as_u64().unwrap_or(0) >= single_rows.saturating_mul(2).saturating_sub(2))
        && fills["select_all"].as_u64().unwrap_or(0) >= single_rows.saturating_mul(3).saturating_sub(2);
    let cleared_rows = filled_rows(&empty, &cleared_pixels, width);
    let restored_empty_rows = filled_rows(&empty, &restored_empty_pixels, width);
    drop(empty_shell);
    let layout_checks=check_same_folder_tabs_and_preview(&slot).map_err(|error|format!("{error:#}"));
    let layout_preserved=layout_checks.is_ok();
    let report = json!({"state": if all_visible && counts && multiple_visible && inversion_preserved && layout_preserved && cleared_rows < 6 && restored_empty_rows < 6 {"passed"} else {"failed"},
        "pid": std::process::id(), "shell_operations": 0, "undo_executed": false,
        "native_selection_scan": selection_scan, "item_pidl_sizes": item_sizes,
        "selection_counts_preserved": counts, "selection_backgrounds_visible": all_visible,
        "all_selected_rows_visible": multiple_visible,
        "inversion_preserved":inversion_preserved,"layout_checks":layout_checks,
        "owner_bounds": [bounds.left, bounds.top, bounds.right, bounds.bottom],
        "frames": [baseline, focused, unfocused, multiple, restored, reselected, all_selected, inverted, cleared, restored_empty], "filled_rows": fills,
        "stored_selected": stored_selected, "cleared_fill_rows": cleared_rows, "restored_empty_fill_rows": restored_empty_rows,
        "foreground_before": foreground.0 as usize,
        "foreground_before_cleanup": unsafe { GetForegroundWindow() }.0 as usize});
    drop(owner);
    pump(80);
    drop(slot);
    drop(apartment);
    let mut report = report;
    report["foreground_after"] = json!(unsafe { GetForegroundWindow() }.0 as usize);
    report["foreground_unchanged"] = json!(foreground == unsafe { GetForegroundWindow() });
    if foreground != unsafe { GetForegroundWindow() } { report["state"] = json!("failed"); }
    fs::write(output.join("result.current.json"), serde_json::to_vec_pretty(&report)?)?;
    println!("{report}");
    ensure!(all_visible && multiple_visible, "selection background is not visible in all states");
    ensure!(counts, "selection counts were not preserved across navigation");
    ensure!(inversion_preserved,"inversion did not produce the original selection's complement");
    ensure!(layout_preserved,"tab/preview layout state was not preserved");
    ensure!(cleared_rows < 6 && restored_empty_rows < 6, "selection background remains after deselection");
    ensure!(foreground == unsafe { GetForegroundWindow() }, "foreground changed during the regression");
    Ok(())
}

#[test]
#[ignore="Requires ZSDESK_NATIVE_TEST_OUTPUT; run on a private desktop; no Shell file operation or Undo"]
fn search_reset_preserves_paths_and_respects_new_input()->Result<()> {
    use crate::fast_search::{BackendKind,IndexPhase,SearchEvent};
    use windows::Win32::{System::Com::CoTaskMemFree,UI::Shell::{IShellItem,SIGDN_FILESYSPATH}};
    fn search_pump(ms:u64,navigation:&mut usize){
        let end=Instant::now()+Duration::from_millis(ms);
        while Instant::now()<end{
            for _ in 0..512{
                let mut msg=MSG::default();if !unsafe{PeekMessageW(&mut msg,None,0,0,PM_REMOVE)}.as_bool(){break;}
                if msg.message==WM_SHELL_NAVIGATED{*navigation+=1;}
                if msg.message!=WM_QUIT{unsafe{let _=TranslateMessage(&msg);DispatchMessageW(&msg);}}
            }
            thread::sleep(Duration::from_millis(4));
        }
    }
    fn choose(slot:&Slot,path:&Path,replace:bool)->Result<()> {
        let shell=view(slot).context("choose/current view")?;let folder:IFolderView2=shell.cast().context("choose/IFolderView2")?;
        for index in 0..unsafe{folder.ItemCount(SVGIO_ALLVIEW)}.context("choose/item count")?{
            let item:IShellItem=unsafe{folder.GetItem(index)}.with_context(||format!("choose/GetItem({index})"))?;
            let text=unsafe{item.GetDisplayName(SIGDN_FILESYSPATH)}.with_context(||format!("choose/GetItem({index}) filesystem path"))?;
            let value=unsafe{text.to_string()};unsafe{CoTaskMemFree(Some(text.0.cast()));}
            if ExplorerBrowserHost::search_path_key(Path::new(&value?))==ExplorerBrowserHost::search_path_key(path){return select(&shell,index,replace).with_context(||format!("choose/SelectItem({index})"));}
        }
        anyhow::bail!("requested fixture path is not an actual GetItem result")
    }
    fn selected(slot:&Slot)->Result<std::collections::HashSet<Vec<u16>>>{
        let state=slot.borrow();let browser=state.as_ref().context("state")?.browser.as_ref().context("browser")?;
        Ok(browser.search_selection_snapshot().context("stable selection snapshot")?.0.iter().map(|p|ExplorerBrowserHost::search_path_key(p)).collect())
    }
    fn expected(paths:&[PathBuf])->std::collections::HashSet<Vec<u16>>{paths.iter().map(|p|ExplorerBrowserHost::search_path_key(p)).collect()}
    fn wait_selected(slot:&Slot,paths:&[PathBuf],navigation:&mut usize)->Result<()>{
        let expected=expected(paths);let baseline=*navigation;let deadline=Instant::now()+Duration::from_secs(3);
        loop{
            search_pump(20,navigation);
            ensure!(*navigation==baseline && slot.borrow().as_ref().unwrap().fast_generation==41,"selection wait navigated/restarted query");
            match selected(slot){
                Ok(actual) if actual==expected=>return Ok(()),
                Ok(actual)=>ensure!(Instant::now()<deadline,"selection did not settle: actual={actual:?} expected={expected:?}"),
                Err(error) if error.downcast_ref::<windows::core::Error>().is_some_and(|e|e.code().0 as u32==0x8000000A)=>{
                    ensure!(Instant::now()<deadline,"selection materialization timed out");
                },
                Err(error)=>return Err(error),
            }
        }
    }
    fn inject(slot:&Slot,paths:Vec<PathBuf>,reset:bool,finished:bool){
        let mut state=slot.borrow_mut();let state=state.as_mut().unwrap();
        state.fast_events.push_back(SearchEvent{generation:41,paths,scanned:3,indexed:3,denied:0,skipped_reparse:0,
            scanning:!finished,finished,truncated:false,error:None,reset,backend:BackendKind::DirectoryScan,
            phase:if finished{IndexPhase::Ready}else{IndexPhase::Scanning},volumes:Vec::new(),read_only:false});
        state.poll_fast_search();
    }
    let output=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("set isolated output")?);
    ensure!(output.is_absolute(),"output must be absolute");fs::create_dir_all(&output)?;
    let files=output.join(format!("fixture-search-{}",std::process::id()));fs::create_dir(&files)?;fixture(&files)?;
    let paths=vec![files.join("01-alpha.txt"),files.join("02-unicode-文件.txt"),files.join("03-space name.txt")];
    let foreground=unsafe{GetForegroundWindow()};unsafe{OleInitialize(None)?;}let apartment=Apartment;
    unsafe{InitCommonControlsEx(&INITCOMMONCONTROLSEX{dwSize:size_of::<INITCOMMONCONTROLSEX>()as u32,dwICC:ICC_BAR_CLASSES|ICC_STANDARD_CLASSES})}.ok()?;
    let instance=HINSTANCE(unsafe{GetModuleHandleW(None)?}.0);let class=wide("ZSDesk.Native.SearchSelectionRegression");
    unsafe{RegisterClassW(&WNDCLASSW{lpfnWndProc:Some(window_proc),hInstance:instance,lpszClassName:pcwstr(&class),..Default::default()});}
    let mut session=ExplorerSession::default();session.tabs=vec![ExplorerTab::new(1,ShellLocation::Path(files.clone()))];session.next_id=2;
    session.ui.show_tree=false;session.ui.show_preview=false;session.ui.show_details=false;
    let slot=Box::new(RefCell::new(Some(WindowState::new(session,output.join(format!("profile-search-{}/explorer-session.json",std::process::id())),None,false))));
    let right=unsafe{GetSystemMetrics(SM_XVIRTUALSCREEN)+GetSystemMetrics(SM_CXVIRTUALSCREEN)};
    let bottom=unsafe{GetSystemMetrics(SM_YVIRTUALSCREEN)+GetSystemMetrics(SM_CYVIRTUALSCREEN)};
    let owner=OwnedWindow(unsafe{CreateWindowExW(WS_EX_TOOLWINDOW|WS_EX_NOACTIVATE,pcwstr(&class),windows::core::w!("Search selection regression"),
        WS_OVERLAPPEDWINDOW|WS_CLIPCHILDREN,right+1200,bottom+1200,1180,760,None,None,Some(instance),Some((&*slot as *const Slot).cast()))?});
    unsafe{slot.borrow_mut().as_mut().unwrap().create(owner.0)?;let _=ShowWindow(owner.0,SW_SHOWNOACTIVATE);}
    let mut phase="initial navigation";let mut trace=Vec::new();
    let check=(||->Result<Value>{
        wait_navigation(&slot).context("initial navigation")?;
        phase="create fresh results host";
        {
            let mut state=slot.borrow_mut();let state=state.as_mut().unwrap();state.reset_browser_host().context("reset_browser_host")?;
            // Even an unexpected query start is restricted to our own tiny tree.
            state.fast_search=Some(crate::fast_search::FastSearch::new_with_roots_for_test(output.join("isolated-index/search-index.sqlite"),vec![files.clone()])?);
            state.pending=None;state.queued=None;state.browser_role=BrowserRole::Results;
            unsafe{state.browser.as_ref().unwrap().navigate(&ShellLocation::Search{query:"fixture-only".into(),scope:None})}.context("initial FillFromObject")?;
        }
        phase="wait for results host";
        let deadline=Instant::now()+Duration::from_secs(10);let mut navigations=0;
        loop{search_pump(40,&mut navigations);
            if matches!(slot.borrow().as_ref().unwrap().current_location(),ShellLocation::Search{scope:None,..}){break;}
            ensure!(Instant::now()<deadline,"results navigation timeout");}
        {
            let mut state=slot.borrow_mut();let state=state.as_mut().unwrap();state.cancel_fast_search();state.fast_generation=41;state.fast_reset_pending=false;
        }
        phase="append initial paths";inject(&slot,paths.clone(),false,true);search_pump(200,&mut navigations);
        {let state=slot.borrow();let state=state.as_ref().unwrap();trace.push(json!({"phase":phase,"added":state.fast_found,
            "items":state.browser.as_ref().unwrap().item_paths_limit(16),"status":state.status,"generation":state.fast_generation}));}
        phase="choose initial first path";choose(&slot,&paths[0],true)?;
        phase="choose initial second path";choose(&slot,&paths[1],false)?;search_pump(80,&mut navigations);
        phase="capture initial selection";ensure!(selected(&slot)?==expected(&paths[..2]),"initial native selection mismatch");navigations=0;
        phase="first reset batch";
        inject(&slot,vec![paths[0].clone()],true,false);wait_selected(&slot,&paths[..1],&mut navigations).context("first reset batch")?;
        phase="second reset batch";inject(&slot,paths[1..].to_vec(),false,true);wait_selected(&slot,&paths[..2],&mut navigations).context("second reset batch")?;
        for (kind,down,up,key) in [("keyboard",WM_KEYDOWN,WM_KEYUP,VK_F24.0 as usize),("mouse",WM_LBUTTONDOWN,WM_LBUTTONUP,0usize)]{
            phase=kind;
            choose(&slot,&paths[0],true)?;choose(&slot,&paths[1],false)?;search_pump(40,&mut navigations);
            inject(&slot,vec![paths[0].clone(),paths[2].clone()],true,false);wait_selected(&slot,&paths[..1],&mut navigations)?;
            // Input is posted only to our offscreen/private test window. The
            // replacement selection itself is made through an actual GetItem.
            let point=LPARAM((20isize)|((745isize)<<16));
            unsafe{PostMessageW(Some(owner.0),down,WPARAM(key),point)?;PostMessageW(Some(owner.0),up,WPARAM(key),point)?;}
            search_pump(30,&mut navigations);choose(&slot,&paths[2],true)?;search_pump(40,&mut navigations);
            inject(&slot,vec![paths[1].clone()],false,true);wait_selected(&slot,&paths[2..],&mut navigations)?;search_pump(160,&mut navigations);
            ensure!(selected(&slot)?==expected(&paths[2..]),"{kind} replacement was overwritten by old selection");
            ensure!(slot.borrow().as_ref().unwrap().fast_restore.is_none(),"{kind} restoration guard stayed armed");
        }
        let state=slot.borrow();let state=state.as_ref().unwrap();
        ensure!(navigations==0 && state.fast_generation==41 && state.pending.is_none(),"reset navigated or restarted query");
        ensure!(state.fast_found==3 && state.browser.as_ref().unwrap().item_paths_limit(16).len()==3,"reset duplicated/lost results");
        Ok(json!({"state":"passed","split_batch_selection_preserved":true,"keyboard_replacement_preserved":true,
            "mouse_replacement_preserved":true,"reset_navigation_messages":navigations,"generation":41,"results":3,
            "native_shell_view":true,"backend_events_synthetic":true,"posted_input_only_to_owned_window":true,
            "shell_file_operations":0,"undo_executed":false,"global_index_scanned":false}))
    })();
    let ending={let state=slot.borrow();let state=state.as_ref().unwrap();json!({"message":state.status,
        "generation":state.fast_generation,"clearing":state.fast_clearing.is_some(),"restore_pending":state.fast_restore.is_some(),
        "items":state.browser.as_ref().map(|browser|browser.item_paths_limit(16)).unwrap_or_default()})};
    drop(owner);pump(80);drop(slot);drop(apartment);
    let mut report=match &check{Ok(value)=>value.clone(),Err(error)=>json!({"state":"failed","error":format!("{error:#}")})};
    report["schema"]=json!("ZSDesk.SearchSelectionProbe.v1");report["pid"]=json!(std::process::id());
    report["phase"]=json!(phase);report["trace"]=json!(trace);report["ending"]=ending;
    report["foreground_before"]=json!(foreground.0 as usize);report["foreground_after"]=json!(unsafe{GetForegroundWindow()}.0 as usize);
    report["foreground_unchanged"]=json!(foreground==unsafe{GetForegroundWindow()});
    fs::write(output.join("search-reset.current.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");
    check?;ensure!(foreground==unsafe{GetForegroundWindow()},"foreground changed");Ok(())
}
