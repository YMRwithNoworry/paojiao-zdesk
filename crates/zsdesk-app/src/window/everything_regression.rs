//! End-to-end Everything queries through the main window's native ResultsFolder.
use super::*;
use anyhow::ensure;
use serde_json::{json, Value};
use std::{ffi::c_void, fs, path::Path, ptr, thread, time::{Duration, Instant}};
use crate::fast_search::everything::SearchSort;
use zsdesk_ui::chrome::SearchPanelHit;
use zsdesk_windows::shell::commands::Column;

#[link(name = "user32")]
unsafe extern "system" {
    fn CreateDesktopW(name: *const u16, device: *const u16, mode: *const c_void, flags: u32, access: u32, attributes: *const c_void) -> *mut c_void;
    fn SetThreadDesktop(desktop: *mut c_void) -> i32;
    fn CloseDesktop(desktop: *mut c_void) -> i32;
}

struct Apartment;
impl Drop for Apartment { fn drop(&mut self) { unsafe { OleUninitialize(); } } }
struct OwnedRunner { hwnd: HWND, slot: Box<Slot> }
impl Drop for OwnedRunner {
    fn drop(&mut self) {
        unsafe { let _ = ShowWindow(self.hwnd, SW_HIDE); let _ = DestroyWindow(self.hwnd); }
    }
}

fn pump(milliseconds: u64) {
    let end = Instant::now() + Duration::from_millis(milliseconds);
    while Instant::now() < end {
        for _ in 0..256 {
            let mut message = MSG::default();
            if !unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() { break; }
            if message.message != WM_QUIT { unsafe { let _ = TranslateMessage(&message); DispatchMessageW(&message); } }
        }
        thread::sleep(Duration::from_millis(3));
    }
}

fn send(owner: HWND, message: u32, wp: usize, lp: LPARAM) -> LRESULT {
    unsafe { SendMessageW(owner, message, Some(WPARAM(wp)), Some(lp)) }
}

fn wait_folder(runner: &OwnedRunner, folder: &Path) -> Result<()> {
    let started = Instant::now();
    loop {
        pump(30);
        let state = runner.slot.borrow();
        let state = state.as_ref().context("window destroyed during folder navigation")?;
        if state.pending.is_none() && state.queued.is_none() && state.shown_tab == Some(state.session.active_tab().id)
            && state.current_location() == &ShellLocation::Path(folder.to_owned()) { return Ok(()); }
        ensure!(started.elapsed() < Duration::from_secs(15), "folder navigation timed out: {:?}; {}", state.current_location(), state.status);
    }
}

fn wait_query(runner: &OwnedRunner, expected: usize) -> Result<Vec<PathBuf>> {
    let started = Instant::now();
    loop {
        pump(30);
        let state = runner.slot.borrow();
        let state = state.as_ref().context("window destroyed during search")?;
        let controls = state.search_controls();
        ensure!(controls.error.is_none(), "Everything query failed: {:?}", controls.error);
        if state.pending.is_none() && state.queued.is_none() && state.fast_generation != 0 && !controls.busy
            && state.fast_clearing.is_none() && state.fast_events.is_empty() {
            ensure!(matches!(state.current_location(), ShellLocation::Search { .. }), "query did not produce native search location");
            ensure!(controls.total == expected as u64 && controls.shown == expected && state.fast_found == expected,
                "wrong result counts: total={} shown={} found={} expected={expected}", controls.total, controls.shown, state.fast_found);
            let browser = state.browser.as_ref().context("native ResultsFolder missing")?;
            let paths = browser.item_paths_limit(512);
            if paths.len() == expected { return Ok(paths.into_iter().map(PathBuf::from).collect()); }
        }
        ensure!(started.elapsed() < Duration::from_secs(20), "search did not settle: total={} shown={} busy={} pending={} message={}",
            controls.total, controls.shown, controls.busy, state.pending.is_some(), state.status);
    }
}

fn submit(runner: &OwnedRunner, query: &str) -> Result<()> {
    let edit = runner.slot.borrow().as_ref().context("window state")?.search_edit;
    unsafe { SetWindowTextW(edit, PCWSTR(wide(query).as_ptr()))?; }
    send(edit, WM_KEYDOWN, VK_RETURN.0 as usize, LPARAM(1));
    Ok(())
}

fn panel(runner: &OwnedRunner, action: SearchPanelHit) {
    unsafe { runner.slot.borrow_mut().as_mut().unwrap().search_panel_action(action); }
}

fn scope(runner: &OwnedRunner, path: Option<PathBuf>) {
    let mut state = runner.slot.borrow_mut();
    let state = state.as_mut().unwrap();
    state.search_controls_mut().scope = path;
    unsafe { state.request_integrated_search(true); }
}

fn create_runner(output: &Path, folder: &Path) -> Result<OwnedRunner> {
    unsafe { InitCommonControlsEx(&INITCOMMONCONTROLSEX { dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
        dwICC: ICC_STANDARD_CLASSES | ICC_BAR_CLASSES | ICC_LISTVIEW_CLASSES }) }.ok()?;
    let mut session = storage::default_session();
    session.ui.search_backend=zsdesk_core::model::explorer::SearchBackendPreference::Everything;
    session.tabs = vec![ExplorerTab::new(1, ShellLocation::Path(folder.to_owned()))];
    session.active_tab = 0;
    session.next_id = 2;
    session.ui.show_preview = false;
    session.ui.show_details = false;
    session.ui.search_all_files = false;
    let slot = Box::new(RefCell::new(Some(WindowState::new(session, output.join("profile/explorer-session.json"), None, false))));
    let instance = HINSTANCE(unsafe { GetModuleHandleW(None)? }.0);
    let class = windows::core::w!("ZSDesk.Native.EverythingWindowRegression");
    unsafe { RegisterClassW(&WNDCLASSW { hInstance: instance, lpfnWndProc: Some(window_proc), lpszClassName: class, ..Default::default() }); }
    let hwnd = unsafe { CreateWindowExW(WS_EX_NOACTIVATE, class, windows::core::w!("Everything integration regression"),
        WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN, 140, 120, 1080, 740, None, None, Some(instance), Some((&*slot as *const Slot).cast()))? };
    let runner = OwnedRunner { hwnd, slot };
    unsafe {
        SetWindowPos(hwnd, None, 0, 0, 0, 0, SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE)?;
        runner.slot.borrow_mut().as_mut().unwrap().create(hwnd)?;
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }
    wait_folder(&runner, folder)?;
    Ok(runner)
}

fn write_surface(state: &WindowState, path: &Path) -> Result<()> {
    let surface = state.surface.as_ref().context("search paint surface")?;
    let length = (surface.width * surface.height * 4) as usize;
    let pixels = unsafe { std::slice::from_raw_parts(surface.bits, length) };
    let mut bmp = vec![0u8; 54];
    bmp[..2].copy_from_slice(b"BM");
    bmp[2..6].copy_from_slice(&((54 + length) as u32).to_le_bytes());
    bmp[10..14].copy_from_slice(&54u32.to_le_bytes());
    bmp[14..18].copy_from_slice(&40u32.to_le_bytes());
    bmp[18..22].copy_from_slice(&surface.width.to_le_bytes());
    bmp[22..26].copy_from_slice(&(-surface.height).to_le_bytes());
    bmp[26..28].copy_from_slice(&1u16.to_le_bytes());
    bmp[28..30].copy_from_slice(&32u16.to_le_bytes());
    bmp.extend_from_slice(pixels);
    fs::write(path, bmp)?;
    Ok(())
}

fn exercise(output: &Path, fixture: &Path, query: &str) -> Result<Value> {
    unsafe { OleInitialize(None)?; }
    let _apartment = Apartment;
    let folder = fixture.parent().context("fixture directory")?;
    let second = folder.join("ZSDeskEverything分页二号.txt");
    ensure!(fixture.is_file() && second.is_file(), "both indexed fixtures are required");
    let fixture_before = fs::read(fixture)?;
    let second_before = fs::read(&second)?;
    let runner = create_runner(output, folder)?;
    send(runner.hwnd, WM_ACTION, Command::FocusSearch.id() as usize, LPARAM(0));
    pump(40);
    {
        let state = runner.slot.borrow();
        let state = state.as_ref().unwrap();
        ensure!(state.search_panel.open && state.search_controls().scope.as_deref() == Some(folder), "current-folder search scope was not initialized");
    }

    let first_started=Instant::now();
    // Enter and debounce can arrive before the Shell announces navigation.
    // Neither should queue a second identical ResultsFolder transition.
    let edit=runner.slot.borrow().as_ref().unwrap().search_edit;
    unsafe{SetWindowTextW(edit,PCWSTR(wide("ZSDeskEverything中文 空格").as_ptr()))?;}
    send(runner.hwnd,WM_ACTION,ACTION_SEARCH,LPARAM(0));
    send(runner.hwnd,WM_ACTION,ACTION_SEARCH,LPARAM(0));
    ensure!(runner.slot.borrow().as_ref().unwrap().queued.is_none(),"duplicate query queued another native navigation");
    let paths = wait_query(&runner, 1)?;
    let first_query_microseconds=first_started.elapsed().as_micros()as u64;
    ensure!(crate::explorer_forwarder::same_path(&paths[0], fixture), "Chinese query selected another path: {paths:?}");

    let warm_started=Instant::now();
    submit(&runner, query)?;
    let paths = wait_query(&runner, 2)?;
    let warm_query_microseconds=warm_started.elapsed().as_micros()as u64;
    ensure!(paths.iter().any(|path| crate::explorer_forwarder::same_path(path, fixture))
        && paths.iter().any(|path| crate::explorer_forwarder::same_path(path, &second)), "native ResultsFolder omitted a fixture: {paths:?}");
    let busy_generation={
        let mut state=runner.slot.borrow_mut();let state=state.as_mut().unwrap();
        state.start_fast_search(query);
        let generation=state.fast_generation;
        state.start_fast_search(query);
        ensure!(state.fast_generation==generation,"duplicate busy query restarted the backend");
        generation
    };
    send(runner.hwnd,WM_ACTION,Command::Refresh.id()as usize,LPARAM(0));
    ensure!(runner.slot.borrow().as_ref().unwrap().fast_generation!=busy_generation,"explicit refresh was suppressed with a duplicate query");
    wait_query(&runner,2)?;
    panel(&runner, SearchPanelHit::Category(1));
    wait_query(&runner, 0)?;
    panel(&runner, SearchPanelHit::Category(2));
    wait_query(&runner, 2)?;

    let empty_scope = output.join("empty-scope");
    fs::create_dir_all(&empty_scope)?;
    scope(&runner, Some(empty_scope));
    wait_query(&runner, 0)?;
    scope(&runner, Some(folder.to_owned()));
    wait_query(&runner, 2)?;
    {
        let mut state = runner.slot.borrow_mut();
        let state = state.as_mut().unwrap();
        state.search_controls_mut().sort = SearchSort::NameDesc;
        unsafe { state.request_integrated_search(true); }
    }
    wait_query(&runner, 2)?;
    {
        let mut state = runner.slot.borrow_mut();
        let state = state.as_mut().unwrap();
        ensure!(state.browser.as_ref().and_then(|browser| browser.sort()) == Some((Column::Name, false)), "native Shell order differs from search sort");
        ensure!(state.layout.browser.y > state.layout.search_panel.card.bottom(), "search controls overlap result view");
        state.paint();
        write_surface(state, &output.join("integrated-search.bmp"))?;
    }

    submit(&runner, "ZSDeskDefinitelyAbsent_7e48c94c")?;
    wait_query(&runner, 0)?;
    panel(&runner, SearchPanelHit::Category(0));
    wait_query(&runner, 0)?;
    panel(&runner, SearchPanelHit::Clear);
    wait_folder(&runner, folder)?;
    {
        let state = runner.slot.borrow();
        let state = state.as_ref().unwrap();
        ensure!(state.search_panel.open && navigation::window_text(state.search_edit).is_empty(), "Clear should preserve the main-window search panel and clear text");
        ensure!(state.fast_generation == 0 && !state.search_controls().busy, "Clear left an active query");
    }
    submit(&runner, query)?;
    wait_query(&runner, 2)?;
    panel(&runner, SearchPanelHit::Close);
    wait_folder(&runner, folder)?;
    {
        let state = runner.slot.borrow();
        let state = state.as_ref().unwrap();
        ensure!(!state.search_panel.open && state.fast_generation == 0 && navigation::window_text(state.search_edit).is_empty(), "Close failed to restore ordinary folder browsing");
    }
    let materialization=runner.slot.borrow().as_ref().unwrap().browser.as_ref().unwrap().search_materialization_metrics();
    drop(runner);
    pump(30);
    ensure!(!output.join("profile/search-index.sqlite").exists(), "production query created a redundant SQLite index");
    ensure!(fs::read(fixture)? == fixture_before && fs::read(&second)? == second_before, "query changed fixture contents");
    Ok(json!({"chinese_result_path":true,"native_results":2,"current_folder_scope":true,"outside_scope_empty":true,
        "folder_filter_empty":true,"document_filter_matches":true,"sort_matches_shell":true,"no_results":true,
        "clear_returns_to_folder":true,"close_returns_to_folder":true,"no_native_sqlite_index":true,"fixture_contents_preserved":true,
        "physical_input_generated":false,"ime_candidate_composition":"not exercised","duplicate_pending_query_coalesced":true,
        "duplicate_busy_query_coalesced":true,"explicit_refresh_restarts_query":true,
        "first_query_microseconds":first_query_microseconds,"warm_query_microseconds":warm_query_microseconds,
        "materialization":materialization}))
}

fn materialization_benchmark(output:&Path)->Result<Value>{
    unsafe{OleInitialize(None)?;}let _apartment=Apartment;
    let folder=output.join("fixtures");fs::create_dir_all(&folder)?;
    let paths=(0..200).map(|index|folder.join(format!("ZSDesk 渲染 {index:03}.txt"))).collect::<Vec<_>>();
    for (index,path) in paths.iter().enumerate(){fs::write(path,format!("Native search materialization fixture {index}\n"))?;}
    let runner=create_runner(output,&folder)?;
    {
        let mut state=runner.slot.borrow_mut();let state=state.as_mut().unwrap();
        state.reset_browser_host()?;
        state.pending=None;state.queued=None;state.browser_role=BrowserRole::Results;
        // With no NavigationRequest, completion only publishes this owned view
        // and does not start a backend query. This isolates Shell display cost.
        unsafe{state.browser.as_ref().unwrap().navigate(&ShellLocation::Search{query:"fixture-display".into(),scope:Some(folder.clone())})?;}
    }
    let deadline=Instant::now()+Duration::from_secs(10);
    loop{
        pump(10);
        if matches!(runner.slot.borrow().as_ref().unwrap().current_location(),ShellLocation::Search{..}){break;}
        ensure!(Instant::now()<deadline,"benchmark ResultsFolder did not initialize");
    }
    ensure!(runner.slot.borrow().as_ref().unwrap().fast_search.is_none(),"display benchmark started a search backend");
    let mut rounds=Vec::new();
    for round in 0..2{
        if round!=0{
            runner.slot.borrow().as_ref().unwrap().browser.as_ref().unwrap().clear_search_paths()?;
            let deadline=Instant::now()+Duration::from_secs(5);
            loop{pump(10);if runner.slot.borrow().as_ref().unwrap().browser.as_ref().unwrap().item_paths_limit(256).is_empty(){break;}
                ensure!(Instant::now()<deadline,"display benchmark clear did not settle");}
        }
        let started=Instant::now();let mut batches=Vec::new();let mut added=0;
        for chunk in paths.chunks(64){
            let batch=Instant::now();
            added+=runner.slot.borrow().as_ref().unwrap().browser.as_ref().unwrap().append_search_paths(chunk)?;
            batches.push(batch.elapsed().as_micros()as u64);pump(1);
        }
        ensure!(added==paths.len(),"display benchmark lost existing fixture paths");
        let deadline=Instant::now()+Duration::from_secs(10);
        loop{pump(10);let visible=runner.slot.borrow().as_ref().unwrap().browser.as_ref().unwrap().item_paths_limit(256);
            if visible.len()==paths.len(){
                ensure!(visible.iter().all(|path|paths.contains(&PathBuf::from(path))),"display benchmark showed unrelated paths");break;
            }
            ensure!(Instant::now()<deadline,"display benchmark did not materialize all 200 paths");
        }
        rounds.push(json!({"round":round,"total_microseconds":started.elapsed().as_micros()as u64,"batch_microseconds":batches,"visible":200}));
    }
    let metrics=runner.slot.borrow().as_ref().unwrap().browser.as_ref().unwrap().search_materialization_metrics();
    drop(runner);pump(25);
    for (index,path) in paths.iter().enumerate(){ensure!(fs::read_to_string(path)?==format!("Native search materialization fixture {index}\n"),"display benchmark modified a fixture");}
    Ok(json!({"rounds":rounds,"metrics":metrics,"backend_started":false,"fixture_contents_preserved":true,"physical_input_generated":false}))
}

#[test]
#[ignore="Measures 200 local fixture paths in the native ResultsFolder on a private desktop; requires ZSDESK_NATIVE_TEST_OUTPUT"]
fn native_search_result_materialization_timing()->Result<()>{
    let output=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("isolated output required")?).join("search-materialization");
    ensure!(output.is_absolute(),"absolute output required");fs::create_dir_all(&output)?;
    let before=unsafe{GetForegroundWindow()};let name=wide(&format!("ZSDesk.SearchMaterialization.{}",std::process::id()));
    let desktop=unsafe{CreateDesktopW(name.as_ptr(),ptr::null(),ptr::null(),0,0x000f00ff,ptr::null())};
    ensure!(!desktop.is_null(),"private desktop creation failed");let handle=desktop as usize;let target=output.clone();
    let checks=thread::spawn(move||->Result<Value>{
        ensure!(unsafe{SetThreadDesktop(handle as*mut c_void)}!=0,"SetThreadDesktop failed");materialization_benchmark(&target)
    }).join().map_err(|_|anyhow::anyhow!("materialization test panicked")).and_then(|value|value);
    let closed=unsafe{CloseDesktop(desktop)}!=0;let after=unsafe{GetForegroundWindow()};let mut pid=0;
    unsafe{GetWindowThreadProcessId(after,Some(&mut pid));}let isolated=pid!=std::process::id();
    let report=json!({"state":if checks.is_ok()&&closed&&isolated{"passed"}else{"failed"},"checks":checks.as_ref().map_err(|error|format!("{error:#}")),
        "desktop_closed":closed,"desktop_switched":false,"foreground_unchanged":before==after,"test_did_not_activate":isolated});
    fs::write(output.join("verification.current.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");
    checks?;ensure!(closed&&isolated,"desktop cleanup or input isolation failed");Ok(())
}

#[test]
#[ignore = "Requires isolated fixture-only Everything, ZSDESK_EVERYTHING_TEST_PATH/QUERY and ZSDESK_NATIVE_TEST_OUTPUT"]
fn native_everything_main_window_search_controls_and_results() -> Result<()> {
    ensure!(std::env::var("ZSDESK_EVERYTHING_INSTANCE").is_ok_and(|instance| instance == "ZSDeskAcceptance"), "isolated fixture Everything instance required");
    let fixture = PathBuf::from(std::env::var_os("ZSDESK_EVERYTHING_TEST_PATH").context("fixture path required")?);
    let query = std::env::var("ZSDESK_EVERYTHING_TEST_QUERY").context("fixture query required")?;
    let output = PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("isolated output required")?).join("everything-window");
    ensure!(fixture.is_absolute() && output.is_absolute() && query == "ZSDeskEverything", "absolute test paths and fixture-only query required");
    fs::create_dir_all(&output)?;
    let foreground = unsafe { GetForegroundWindow() };
    let name = wide(&format!("ZSDesk.EverythingWindow.{}", std::process::id()));
    let desktop = unsafe { CreateDesktopW(name.as_ptr(), ptr::null(), ptr::null(), 0, 0x000f00ff, ptr::null()) };
    ensure!(!desktop.is_null(), "private desktop creation failed");
    let handle = desktop as usize;
    let destination = output.clone();
    let checks = thread::spawn(move || -> Result<Value> {
        ensure!(unsafe { SetThreadDesktop(handle as *mut c_void) } != 0, "SetThreadDesktop failed");
        exercise(&destination, &fixture, &query)
    }).join().map_err(|_| anyhow::anyhow!("Everything main-window test panicked")).and_then(|result| result);
    let closed = unsafe { CloseDesktop(desktop) } != 0;
    let current = unsafe { GetForegroundWindow() };
    let mut foreground_pid = 0;
    unsafe { GetWindowThreadProcessId(current, Some(&mut foreground_pid)); }
    let isolated = foreground_pid != std::process::id();
    let report = json!({"state":if checks.is_ok() && closed && isolated {"passed"} else {"failed"},
        "checks":checks.as_ref().map_err(|error|format!("{error:#}")),"desktop_closed":closed,
        "foreground_unchanged":foreground==current,"test_did_not_activate":isolated,"desktop_switched":false});
    fs::write(output.join("verification.current.json"), serde_json::to_vec_pretty(&report)?)?;
    println!("{report}");
    checks?;
    ensure!(closed && isolated, "private desktop cleanup or input isolation failed");
    Ok(())
}
