//! Native chrome regression on a private desktop that is never switched to.
use super::*;
use anyhow::ensure;
use serde_json::{json, Value};
use std::{ffi::c_void, fs, path::Path, ptr, thread, time::{Duration, Instant}};
use zsdesk_core::model::explorer::TabPlacement;

#[link(name = "user32")]
unsafe extern "system" {
    fn CreateDesktopW(name: *const u16, device: *const u16, mode: *const c_void, flags: u32, access: u32, attributes: *const c_void) -> *mut c_void;
    fn SetThreadDesktop(desktop: *mut c_void) -> i32;
    fn CloseDesktop(desktop: *mut c_void) -> i32;
}

struct Apartment;
impl Drop for Apartment { fn drop(&mut self) { unsafe { OleUninitialize(); } } }

/// The window is destroyed while its callback slot is still alive, including
/// every early-return path in the checks below.
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

fn packed(point: POINT) -> LPARAM { LPARAM(((point.x as u32 & 0xffff) | ((point.y as u32 & 0xffff) << 16)) as isize) }
fn center(rect: LayoutRect) -> POINT { POINT { x: rect.x + rect.width / 2, y: rect.y + rect.height / 2 } }
fn send(owner: HWND, message: u32, wp: usize, lp: LPARAM) -> LRESULT {
    unsafe { SendMessageW(owner, message, Some(WPARAM(wp)), Some(lp)) }
}
fn command(owner: HWND, command: Command) { send(owner, WM_ACTION, command.id() as usize, LPARAM(0)); }
fn click(owner: HWND, point: POINT) {
    send(owner, WM_LBUTTONDOWN, 1, packed(point));
    send(owner, WM_LBUTTONUP, 0, packed(point));
}

fn wait_navigation(runner: &OwnedRunner) -> Result<()> {
    let start = Instant::now();
    loop {
        pump(30);
        let state = runner.slot.borrow();
        let state = state.as_ref().context("window was destroyed")?;
        ensure!(state.fast_search.is_none(), "chrome regression must not start a search backend");
        if state.pending.is_none() && state.queued.is_none() && state.shown_tab == Some(state.session.active_tab().id) { return Ok(()); }
        ensure!(start.elapsed() < Duration::from_secs(12), "native navigation did not settle");
    }
}

fn child_bounds(owner: HWND, child: HWND) -> Result<LayoutRect> {
    let mut bounds = RECT::default();
    unsafe { GetWindowRect(child, &mut bounds)?; }
    let mut start = POINT { x: bounds.left, y: bounds.top };
    let mut end = POINT { x: bounds.right, y: bounds.bottom };
    unsafe { ScreenToClient(owner, &mut start) }.ok()?;
    unsafe { ScreenToClient(owner, &mut end) }.ok()?;
    Ok(LayoutRect::new(start.x, start.y, end.x - start.x, end.y - start.y))
}

fn contained(outer: LayoutRect, inner: LayoutRect) -> bool {
    !inner.is_empty() && inner.x >= outer.x && inner.y >= outer.y && inner.right() <= outer.right() && inner.bottom() <= outer.bottom()
}

fn create_runner(output: &Path) -> Result<OwnedRunner> {
    unsafe { InitCommonControlsEx(&INITCOMMONCONTROLSEX { dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
        dwICC: ICC_STANDARD_CLASSES | ICC_BAR_CLASSES | ICC_LISTVIEW_CLASSES }) }.ok()?;
    let folder = output.join("fixture");
    fs::create_dir_all(&folder)?;
    let file = folder.join("中文 文件.txt");
    let contents = b"ZSDesk native search chrome fixture";
    if file.exists() { ensure!(fs::read(&file)? == contents, "fixture contents changed"); }
    else { fs::write(&file, contents)?; }
    let mut session = storage::default_session();
    session.tabs = (1..=80).map(|id| ExplorerTab::new(id, ShellLocation::Path(folder.clone()))).collect();
    session.active_tab = 79;
    session.next_id = 81;
    session.ui.show_preview = false;
    session.ui.show_details = false;
    session.ui.show_tree = true;
    let slot = Box::new(RefCell::new(Some(WindowState::new(session, output.join("profile/explorer-session.json"), None, false))));
    let instance = HINSTANCE(unsafe { GetModuleHandleW(None)? }.0);
    let class = windows::core::w!("ZSDesk.Native.SearchChromeRegression");
    unsafe { RegisterClassW(&WNDCLASSW { hInstance: instance, lpfnWndProc: Some(window_proc), lpszClassName: class, ..Default::default() }); }
    let hwnd = unsafe { CreateWindowExW(WS_EX_NOACTIVATE, class, windows::core::w!("Search chrome regression"),
        WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN, 120, 100, 1080, 740, None, None, Some(instance), Some((&*slot as *const Slot).cast()))? };
    let runner = OwnedRunner { hwnd, slot };
    unsafe {
        SetWindowPos(hwnd, None, 0, 0, 0, 0, SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE)?;
        runner.slot.borrow_mut().as_mut().unwrap().create(hwnd)?;
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }
    wait_navigation(&runner)?;
    Ok(runner)
}

fn address_focus(runner: &OwnedRunner) -> Result<Value> {
    let edit = runner.slot.borrow().as_ref().unwrap().address_edit;
    ensure!(!unsafe { IsWindowVisible(edit) }.as_bool(), "address starts as an EDIT instead of breadcrumbs");
    for _ in 0..3 {
        command(runner.hwnd, Command::FocusAddress);
        pump(25);
        ensure!(unsafe { GetFocus() } == edit && unsafe { IsWindowVisible(edit) }.as_bool(), "address shortcut failed to focus the EDIT");
        // This reproduces synchronous EN_KILLFOCUS while WindowState is already
        // borrowed by a Shell call. The edit subclass must queue its own action.
        {
            let mut state = runner.slot.borrow_mut();
            let state = state.as_mut().unwrap();
            unsafe { state.browser.as_ref().context("browser")?.focus(); }
            ensure!(unsafe { GetFocus() } != edit, "Shell focus did not leave the address EDIT");
        }
        pump(35);
        let (before, point) = {
            let state = runner.slot.borrow();
            let state = state.as_ref().unwrap();
            ensure!(!state.address_editing && !unsafe { IsWindowVisible(edit) }.as_bool(), "lost focus notification left the address EDIT visible");
            (unsafe { GetFocus() }, center(state.layout.address))
        };
        send(runner.hwnd, WM_MOUSEMOVE, 0, packed(point));
        ensure!(unsafe { GetFocus() } == before, "hovering the address changed keyboard focus");
        ensure!(!runner.slot.borrow().as_ref().unwrap().address_editing, "hovering the address reopened editing");
    }
    command(runner.hwnd, Command::FocusAddress);
    pump(20);
    send(runner.hwnd, WM_ACTIVATE, WA_INACTIVE as usize, LPARAM(0));
    unsafe { let _ = SetFocus(None); }
    pump(25);
    send(runner.hwnd, WM_ACTIVATE, WA_ACTIVE as usize, LPARAM(0));
    pump(25);
    ensure!(!runner.slot.borrow().as_ref().unwrap().address_editing, "activation reopened a hidden address EDIT");
    ensure!(unsafe { GetFocus() } != edit, "activation restored focus to a hidden address EDIT");
    Ok(json!({"reentrant_focus_loss_cycles":3,"hover_preserves_focus":true,"activation_does_not_reopen_address":true}))
}

fn address_submit(runner: &OwnedRunner, output: &Path) -> Result<Value> {
    let edit = runner.slot.borrow().as_ref().unwrap().address_edit;
    let mut routes = Vec::new();
    for route in ["queued-key", "sent-key", "character-only"] {
        let target = output.join(format!("address-{route}"));
        fs::create_dir_all(&target)?;
        command(runner.hwnd, Command::FocusAddress); pump(20);
        unsafe { SetWindowTextW(edit, PCWSTR(wide(&target.to_string_lossy()).as_ptr()))?; }
        let flags = unsafe { SendMessageW(edit, WM_GETDLGCODE, Some(WPARAM(VK_RETURN.0 as usize)), None) }.0;
        ensure!(flags & DLGC_WANTMESSAGE as isize != 0, "address does not own Return in native dialog translation");
        if route == "queued-key" {
            let message = MSG { hwnd: edit, message: WM_KEYDOWN, wParam: WPARAM(VK_RETURN.0 as usize), ..Default::default() };
            ensure!(address_input::pre_translate(&message), "queued Return not consumed before Shell");
        } else {
            // A native modal loop can deliver input while the owner is borrowed.
            let _borrow = runner.slot.borrow_mut();
            send(edit, if route == "sent-key" { WM_KEYDOWN } else { WM_CHAR }, VK_RETURN.0 as usize, LPARAM(0));
        }
        if route != "character-only" {send(edit,WM_KEYUP,VK_RETURN.0 as usize,LPARAM(0));send(edit, WM_CHAR, 13, LPARAM(0));}
        let mut ticket = MSG::default();
        ensure!(unsafe { PeekMessageW(&mut ticket, Some(runner.hwnd), pane_input::MESSAGE, pane_input::MESSAGE, PM_REMOVE) }.as_bool(), "Return did not post an address ticket");
        let mut duplicate = MSG::default();
        ensure!(!unsafe { PeekMessageW(&mut duplicate, Some(runner.hwnd), pane_input::MESSAGE, pane_input::MESSAGE, PM_REMOVE) }.as_bool(), "Return and WM_CHAR submitted twice");
        // Reproduce a queued focus/update callback replacing the live EDIT.
        { let mut slot = runner.slot.borrow_mut(); let state = slot.as_mut().unwrap();
            unsafe { state.browser.as_ref().unwrap().focus(); state.update_address(); }
        }
        unsafe { SetWindowTextW(edit, windows::core::w!("shell:MyComputerFolder"))?; DispatchMessageW(&ticket); }
        wait_navigation(runner)?;
        ensure!(runner.slot.borrow().as_ref().unwrap().current_location() == &ShellLocation::Path(target), "deferred Return read the replaced address instead of the captured text");
        routes.push(route);
    }
    command(runner.hwnd, Command::FocusAddress); pump(20);
    unsafe { SetWindowTextW(edit, windows::core::w!("shell:MyComputerFolder"))?; }
    send(edit, WM_KEYDOWN, VK_RETURN.0 as usize, LPARAM(0));
    { let mut slot = runner.slot.borrow_mut(); unsafe { slot.as_mut().unwrap().select_tab(0); } }
    wait_navigation(runner)?;
    ensure!(runner.slot.borrow().as_ref().unwrap().current_location() == &ShellLocation::Path(output.join("fixture")), "old address navigated the new tab");
    Ok(json!({"routes":routes,"text_survives_focus_reset":true,"single_submission":true,"stale_tab_rejected":true}))
}

fn write_surface(state: &WindowState, path: &Path) -> Result<()> {
    let surface = state.surface.as_ref().context("paint surface")?;
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

fn side_tabs(runner: &OwnedRunner, output: &Path) -> Result<Value> {
    let mut records = Vec::new();
    for (placement, name) in [(TabPlacement::Left, "left"), (TabPlacement::Right, "right")] {
        {
            let mut state = runner.slot.borrow_mut();
            let state = state.as_mut().unwrap();
            state.session.ui.tab_placement = placement;
            state.relayout();
            state.position_children();
        }
        pump(35);
        let (first, selected, selected_id, panel, point) = {
            let state = runner.slot.borrow();
            let state = state.as_ref().unwrap();
            let layout = &state.layout;
            let selected = layout.tabs.get(1).context("second visible tab")?;
            let view = state.browser.as_ref().and_then(|browser| browser.view_window()).context("native Shell view")?;
            let bounds = child_bounds(runner.hwnd, view)?;
            ensure!(contained(layout.browser, bounds), "native Shell view crosses the {name} tab panel: {bounds:?}");
            ensure!(layout.tab_panel.right() <= bounds.x || bounds.right() <= layout.tab_panel.x, "native view overlaps {name} tabs");
            (layout.tabs[0].index, selected.index, state.session.tabs[selected.index].id, layout.tab_panel,
                POINT { x: selected.rect.x + state.s(36), y: selected.rect.y + selected.rect.height / 2 })
        };
        let mut screen = point;
        unsafe { ClientToScreen(runner.hwnd, &mut screen) }.ok()?;
        let hit = send(runner.hwnd, WM_NCHITTEST, 0, packed(screen)).0 as u32;
        ensure!(hit == HTCLIENT, "{name} tab was interpreted as a caption or resize edge: {hit}");
        click(runner.hwnd, point);
        wait_navigation(runner)?;
        {
            let state = runner.slot.borrow();
            let state = state.as_ref().unwrap();
            ensure!(state.session.active_tab == selected && state.session.active_tab().id == selected_id, "{name} tab click selected the wrong tab");
            ensure!(state.layout.tabs[0].index == first, "{name} tab click moved the visible list");
            ensure!(state.layout.tab_panel == panel, "{name} tab click moved the panel");
        }
        let (from, to, expected, original_ids, outside) = {
            let state = runner.slot.borrow();
            let state = state.as_ref().unwrap();
            let current = state.layout.tabs.iter().find(|tab| tab.index == selected).context("active tab slot")?;
            let target = &state.layout.tabs[3];
            (POINT { x: current.rect.x + state.s(36), y: current.rect.y + current.rect.height / 2 },
                POINT { x: target.rect.x + state.s(36), y: target.rect.y + target.rect.height / 2 }, target.index,
                state.session.tabs.iter().map(|tab| tab.id).collect::<Vec<_>>(),
                POINT { x: state.layout.address.x + 5, y: target.rect.y + target.rect.height / 2 })
        };
        send(runner.hwnd, WM_LBUTTONDOWN, 1, packed(from));
        send(runner.hwnd, WM_MOUSEMOVE, 1, packed(outside));
        send(runner.hwnd, WM_LBUTTONUP, 0, packed(outside));
        ensure!(runner.slot.borrow().as_ref().unwrap().session.tabs.iter().map(|tab| tab.id).collect::<Vec<_>>() == original_ids,
            "drag outside the {name} tab panel reordered tabs");
        send(runner.hwnd, WM_LBUTTONDOWN, 1, packed(from));
        send(runner.hwnd, WM_MOUSEMOVE, 1, packed(to));
        send(runner.hwnd, WM_LBUTTONUP, 0, packed(to));
        pump(30);
        let mut state = runner.slot.borrow_mut();
        let state = state.as_mut().unwrap();
        ensure!(state.session.active_tab == expected && state.session.active_tab().id == selected_id, "{name} in-panel drag did not preserve the active tab identity");
        ensure!(state.layout.tabs[0].index == first, "{name} drag moved the visible list");
        state.paint();
        let surface = state.surface.as_ref().context("painted surface")?;
        let sample = POINT { x: panel.x + state.s(2), y: state.layout.browser.y + state.s(20) };
        ensure!(sample.x >= 0 && sample.y >= 0 && sample.x < surface.width && sample.y < surface.height, "paint sample outside surface");
        let pixel = unsafe { std::slice::from_raw_parts(surface.bits.add(((sample.y * surface.width + sample.x) * 4) as usize), 4) };
        let expected_color = state.palette.strip;
        ensure!(pixel[..3] == [expected_color.b, expected_color.g, expected_color.r], "{name} sidebar was overpainted by the content layer: {pixel:?}");
        write_surface(state, &output.join(format!("tabs-{name}.bmp")))?;
        records.push(json!({"placement":name,"first_visible":first,"clicked_index":selected,"dragged_index":expected,
            "stable_viewport":true,"native_view_separate":true,"hit":hit,"sidebar_pixel":pixel}));
    }
    Ok(Value::Array(records))
}

fn dispatch_edit_action(runner: &OwnedRunner, edit: HWND, key: VIRTUAL_KEY, expected: usize) -> Result<()> {
    send(edit, WM_KEYDOWN, key.0 as usize, LPARAM(1));
    let mut action = MSG::default();
    ensure!(unsafe { PeekMessageW(&mut action, Some(runner.hwnd), WM_ACTION, WM_ACTION, PM_REMOVE) }.as_bool(), "edit key did not post an action");
    ensure!(action.wParam.0 == expected, "edit key posted {} instead of {expected}", action.wParam.0);
    send(runner.hwnd, action.message, action.wParam.0, action.lParam);
    Ok(())
}

fn search_close_race(runner: &OwnedRunner) -> Result<Value> {
    let original = runner.slot.borrow().as_ref().unwrap().current_location().clone();
    let (search_before,browser_before)={let slot=runner.slot.borrow();let state=slot.as_ref().unwrap();(state.layout.search,state.layout.browser)};
    command(runner.hwnd, Command::FocusSearch);
    pump(30);
    let edit = {
        let state = runner.slot.borrow();
        let state = state.as_ref().unwrap();
        ensure!(state.search_panel.open, "Ctrl+F did not activate integrated search");
        ensure!(state.layout.search_panel.card.is_empty()&&state.layout.search==search_before&&state.layout.browser==browser_before, "search expanded or moved the native edit/results");
        ensure!(contained(state.layout.search, child_bounds(runner.hwnd, state.search_edit)?), "native search EDIT is outside its address-row field");
        state.search_edit
    };
    unsafe { SetWindowTextW(edit, windows::core::w!(""))?; }
    for unit in "测试 搜索".encode_utf16() { send(edit, WM_CHAR, unit as usize, LPARAM(1)); }
    ensure!(navigation::window_text(edit) == "测试 搜索", "native EDIT corrupted Chinese text");
    // Do not pump navigation or debounce messages between Enter and Escape:
    // this exercises the actual close race without starting Everything.
    dispatch_edit_action(runner, edit, VK_RETURN, ACTION_SEARCH)?;
    {
        let state = runner.slot.borrow();
        let state = state.as_ref().unwrap();
        ensure!(state.pending.as_ref().is_some_and(|request| matches!(request.tab.current(), ShellLocation::Search { .. })), "search did not enter a pending native ResultsFolder navigation");
        ensure!(state.fast_search.is_none(), "backend started before the close-race check");
    }
    dispatch_edit_action(runner, edit, VK_ESCAPE, ACTION_CLOSE_SEARCH)?;
    {
        let state = runner.slot.borrow();
        let state = state.as_ref().unwrap();
        ensure!(!state.search_panel.open, "Escape left the search card open");
        ensure!(state.queued.as_ref().is_some_and(|request| request.tab.current() == &original), "closing pending Search did not queue the original folder");
        ensure!(state.fast_search.is_none(), "closing search started a backend");
    }
    wait_navigation(runner)?;
    pump(300);
    let state = runner.slot.borrow();
    let state = state.as_ref().unwrap();
    ensure!(!state.search_panel.open && state.current_location() == &original, "pending completion reopened the search panel");
    ensure!(navigation::window_text(edit).is_empty(), "closed search restored stale query text");
    ensure!(state.fast_search.is_none(), "debounce started a backend after Escape");
    Ok(json!({"native_edit_chinese_text":true,"enter_posts_search":true,"escape_posts_close":true,
        "pending_close_returns_to_original":true,"backend_started":false,"ime_candidate_composition":"not exercised"}))
}

/// Exercise queued search transitions with real EDIT notifications while the
/// native navigation is deliberately held pending. No query worker is needed.
fn search_tab_state(runner: &OwnedRunner, output: &Path) -> Result<Value> {
    use super::search_panel::{SearchControls, SearchPanel};
    let origin_a = ShellLocation::Path(output.join("origin-a"));
    let origin_b = ShellLocation::Path(output.join("origin-b"));
    for origin in [&origin_a, &origin_b] { fs::create_dir_all(origin.as_path().unwrap())?; }
    command(runner.hwnd, Command::FocusSearch);
    pump(30);
    let edit = runner.slot.borrow().as_ref().unwrap().search_edit;
    ensure!(unsafe { GetFocus() } == edit, "search EDIT is not focused for tab-state checks");
    let alpha = ShellLocation::Search { query: "alpha".into(), scope: None };
    let beta = ShellLocation::Search { query: "beta".into(), scope: None };
    {
        let mut state = runner.slot.borrow_mut();
        let state = state.as_mut().unwrap();
        ensure!(state.pending.is_none() && state.fast_search.is_none(), "tab-state regression needs an idle browser");
        state.session.tabs = vec![ExplorerTab::new(101, alpha.clone()), ExplorerTab::new(102, beta.clone())];
        state.session.active_tab = 0;
        state.session.next_id = 103;
        state.shown_tab = Some(101);
        state.search_panel = SearchPanel::default();
        state.search_panel.open = true;
        state.search_panel.tabs.insert(101, SearchControls { origin: Some(origin_a.clone()), draft: Some("alpha".into()), ..Default::default() });
        state.search_panel.tabs.insert(102, SearchControls { origin: Some(origin_b.clone()), draft: Some("beta".into()), ..Default::default() });
        unsafe { state.sync_search_tab_edit(); }
        let mut stale = state.session.active_tab().clone();
        stale.navigate(ShellLocation::Search { query: "beta-stale".into(), scope: None });
        state.pending = Some(NavigationRequest::new(stale));
        state.queued = None;
        // alpha -> pending beta -> alpha must still replace the pending queue.
        unsafe { state.request_integrated_search(true); }
        ensure!(state.queued.as_ref().is_some_and(|request| request.tab.id == 101 && request.tab.current() == &alpha),
            "returning to the committed query did not replace the pending request");
        ensure!(state.fast_search.is_none() && state.fast_generation == 0, "pending refinement started a query worker");
    }
    // EN_CHANGE schedules a genuine debounce while the shared EDIT has focus.
    unsafe { SetWindowTextW(edit, windows::core::w!("alpha draft"))?; }
    command(runner.hwnd, Command::NextTab);
    {
        let state = runner.slot.borrow();
        let state = state.as_ref().unwrap();
        ensure!(state.session.active_tab().id == 102 && unsafe { GetFocus() } == edit, "next-tab did not retain EDIT focus");
        ensure!(navigation::window_text(edit) == "beta", "old tab text leaked into the newly selected search tab");
        ensure!(state.search_panel.tabs.get(&101).and_then(|controls| controls.draft.as_deref()) == Some("alpha draft"), "old tab draft was not preserved");
        ensure!(state.search_scope.as_ref() == Some(&origin_b), "new tab retained another tab's return folder");
        ensure!(state.queued.as_ref().is_some_and(|request| request.tab.id == 102 && request.tab.current() == &beta), "next-tab queued the wrong search");
    }
    thread::sleep(Duration::from_millis(300));
    let mut search_timers = 0;
    for _ in 0..64 {
        let mut message = MSG::default();
        if !unsafe { PeekMessageW(&mut message, Some(runner.hwnd), WM_TIMER, WM_TIMER, PM_REMOVE) }.as_bool() { break; }
        if message.wParam.0 == TIMER_SEARCH { search_timers += 1; }
        unsafe { DispatchMessageW(&message); }
    }
    ensure!(search_timers == 0, "old tab debounce survived the tab switch");
    {
        let state = runner.slot.borrow();
        let state = state.as_ref().unwrap();
        ensure!(state.queued.as_ref().is_some_and(|request| request.tab.id == 102 && request.tab.current() == &beta), "old debounce changed the new tab request");
        ensure!(state.fast_search.is_none() && state.fast_generation == 0, "tab switch started a query worker");
    }
    command(runner.hwnd, Command::PreviousTab);
    ensure!(navigation::window_text(edit) == "alpha draft", "returning to a tab lost its unsubmitted draft");
    send(runner.hwnd, WM_ACTION, ACTION_CLOSE_SEARCH, LPARAM(0));
    {
        let state = runner.slot.borrow();
        let state = state.as_ref().unwrap();
        ensure!(state.queued.as_ref().is_some_and(|request| request.tab.id == 101 && request.tab.current() == &origin_a), "first tab closed into another tab's origin");
    }
    command(runner.hwnd, Command::NextTab);
    ensure!(navigation::window_text(edit) == "beta", "closing the first search cleared the second tab's query");
    send(runner.hwnd, WM_ACTION, ACTION_CLOSE_SEARCH, LPARAM(0));
    {
        let mut state = runner.slot.borrow_mut();
        let state = state.as_mut().unwrap();
        ensure!(state.queued.as_ref().is_some_and(|request| request.tab.id == 102 && request.tab.current() == &origin_b), "second tab did not return to its own origin");
        ensure!(state.fast_search.is_none() && state.fast_generation == 0, "closing tab searches started a query worker");
        // These are synthetic pending records: no Shell completion is expected.
        state.pending = None;
        state.queued = None;
        let _ = unsafe { KillTimer(Some(runner.hwnd), TIMER_SEARCH) };
    }
    Ok(json!({"pending_revert_queues_latest":true,"focused_tab_switch_restores_query":true,
        "old_debounce_cancelled":true,"draft_restored":true,"per_tab_origins_preserved":true,"backend_started":false}))
}

fn run(output: &Path) -> Result<Value> {
    unsafe { OleInitialize(None)?; }
    let _apartment = Apartment;
    let runner = create_runner(output)?;
    let focus = address_focus(&runner)?;
    let submit = address_submit(&runner, output)?;
    let tabs = side_tabs(&runner, output)?;
    let search = search_close_race(&runner)?;
    let tab_search = search_tab_state(&runner, output)?;
    drop(runner);
    pump(25);
    Ok(json!({"address":focus,"address_submit":submit,"tabs":tabs,"search":search,"tab_search":tab_search,"physical_input_generated":false}))
}

#[test]
#[ignore = "Native search/chrome regression on an unswitched private desktop; requires ZSDESK_NATIVE_TEST_OUTPUT"]
fn native_search_panel_address_and_side_tabs() -> Result<()> {
    let output = PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("set isolated ZSDESK_NATIVE_TEST_OUTPUT")?).join("search-chrome");
    ensure!(output.is_absolute(), "output must be absolute");
    fs::create_dir_all(&output)?;
    let foreground = unsafe { GetForegroundWindow() };
    let name = wide(&format!("ZSDesk.SearchChrome.{}", std::process::id()));
    let desktop = unsafe { CreateDesktopW(name.as_ptr(), ptr::null(), ptr::null(), 0, 0x000f00ff, ptr::null()) };
    ensure!(!desktop.is_null(), "private desktop creation failed");
    let handle = desktop as usize;
    let destination = output.clone();
    let checks = thread::spawn(move || -> Result<Value> {
        ensure!(unsafe { SetThreadDesktop(handle as *mut c_void) } != 0, "SetThreadDesktop failed");
        run(&destination)
    }).join().map_err(|_| anyhow::anyhow!("native search chrome test panicked")).and_then(|result| result);
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
