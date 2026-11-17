//! Native fence lifecycle checks on an unswitched private desktop and owned files.
use super::*;
use anyhow::{Context,Result,ensure};
use std::{ffi::c_void,fs,thread};
use windows::{core::w,Win32::{Foundation::HINSTANCE,System::{LibraryLoader::GetModuleHandleW,Ole::{OleInitialize,OleUninitialize}},
    UI::Input::KeyboardAndMouse::{SetCapture,ReleaseCapture}}};
use zsdesk_windows::ole::DropHandler;
use super::super::{HOST,HOST_HWND,MENU_WORKER,with_host,menu_worker,FenceDrop};
#[link(name="user32")]unsafe extern "system"{
    fn CreateDesktopW(name:*const u16,device:*const u16,mode:*const c_void,flags:u32,access:u32,attributes:*const c_void)->*mut c_void;
    fn SetThreadDesktop(desktop:*mut c_void)->i32;
    fn CloseDesktop(desktop:*mut c_void)->i32;
}
struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}
struct Fixture(HWND);impl Drop for Fixture{fn drop(&mut self){
    let worker=MENU_WORKER.with(|slot|slot.borrow_mut().take());drop(worker);
    if let Some(mut host)=HOST.with(|slot|slot.borrow_mut().take()){host.config.settings.icons_hidden_by_zsdesk=false;host.shutdown();}
    HOST_HWND.with(|slot|slot.set(0));let _=unsafe{DestroyWindow(self.0)};
}}
fn pump(ms:u64){let deadline=Instant::now()+Duration::from_millis(ms);while Instant::now()<deadline{
    let mut message=MSG::default();for _ in 0..128{if !unsafe{PeekMessageW(&mut message,None,0,0,PM_REMOVE)}.as_bool(){break;}
        if message.message!=WM_QUIT{unsafe{let _=TranslateMessage(&message);DispatchMessageW(&message);}}}thread::sleep(Duration::from_millis(2));}}
fn setup(root:&Path)->Result<(Fixture,Vec<PathBuf>)>{
    let files=root.join("files");let mapped=root.join("mapped");fs::create_dir_all(&files)?;fs::create_dir_all(&mapped)?;
    let paths:Vec<_>=["01-owned.txt","02-kept.txt","03-end.txt"].iter().map(|name|files.join(name)).collect();
    for path in &paths{fs::write(path,b"isolated desktop lifecycle")?;}
    let entries=desktop::enumerate_folder(&files)?;let key=|path:&Path|entries.iter().find(|entry|entry.path.as_deref()==Some(path)).map(|entry|entry.key.clone()).context("fixture entry");
    let instance=HINSTANCE(unsafe{GetModuleHandleW(None)?}.0);
    unsafe{RegisterClassW(&WNDCLASSW{hInstance:instance,lpfnWndProc:Some(super::super::host_proc),lpszClassName:w!("ZSDesk.FenceLifecycle.Host"),..Default::default()});
        RegisterClassW(&WNDCLASSW{hInstance:instance,lpfnWndProc:Some(super::super::fence_proc),lpszClassName:w!("ZSDesk.Native.Fence"),..Default::default()});}
    let hwnd=unsafe{CreateWindowExW(WS_EX_NOACTIVATE|WS_EX_TOOLWINDOW,w!("ZSDesk.FenceLifecycle.Host"),w!("Lifecycle fixture"),WS_POPUP,0,0,0,0,None,None,Some(instance),None)}?;
    let owned=Fixture(hwnd);let mut config=DesktopConfig::default();config.settings.hide_tray=true;config.settings.double_click_toggle=false;config.next_id=4;
    for id in 1..=3{
        let source=if id==3{FenceSource::Folder{root:mapped.clone()}}else{FenceSource::Desktop};
        let mut fence=Fence::new(id,format!("Fixture {id}"),source,Placement{x:60+(id as i32-1)*380,y:100,width:340,height:260,..Default::default()});
        fence.collapsed=true;fence.view=FenceView::List;
        if id==1{fence.members.push(key(&paths[0])?);fence.catch_all=true;}
        if id==2{fence.rule=Some(Category::Documents);}
        config.fences.push(fence);
    }
    let mut host=DesktopHost::new(hwnd,root.join("profile/desktop-fences.json"),root.join("profile/explorer-session.json"),config,Gfx::new("Segoe UI")?);
    host.desktop_test_entries=Some(entries);host.sync_windows();host.refresh_desktop();
    for id in 1..=3{ensure!(host.win_index(id).is_some(),"fixture fence missing");}
    HOST.with(|slot|*slot.borrow_mut()=Some(host));HOST_HWND.with(|slot|slot.set(hwnd.0 as isize));Ok((owned,paths))
}
fn window(id:u64)->HWND{with_host(|host|host.fences[host.win_index(id).unwrap()].hwnd).unwrap()}
fn peeked(id:u64)->bool{with_host(|host|host.fences[host.win_index(id).unwrap()].peek).unwrap_or(false)}
fn outside_tick(id:u64){with_host(|host|host.peek_check_observed(id,POINT{x:-10000,y:-10000},false));}
fn end_point(id:u64)->POINT{let mut rect=RECT::default();let _=unsafe{GetWindowRect(window(id),&mut rect)};POINT{x:rect.left+24,y:rect.bottom-20}}
fn returned_effect(data:&IDataObject,name:PCWSTR)->Result<u32>{
    use windows::Win32::System::{Com::{FORMATETC,TYMED_HGLOBAL,DVASPECT_CONTENT},DataExchange::RegisterClipboardFormatW,Memory::{GlobalLock,GlobalSize,GlobalUnlock},Ole::ReleaseStgMedium};
    let format=unsafe{RegisterClipboardFormatW(name)};let format=FORMATETC{cfFormat:format as u16,dwAspect:DVASPECT_CONTENT.0,lindex:-1,tymed:TYMED_HGLOBAL.0 as u32,..Default::default()};
    let mut medium=unsafe{data.GetData(&format)}?;
    let result=(||{ensure!(medium.tymed==TYMED_HGLOBAL.0 as u32,"unexpected drop-result medium");let memory=unsafe{medium.u.hGlobal};
        ensure!(unsafe{GlobalSize(memory)}>=4,"truncated drop result");let value=unsafe{GlobalLock(memory)};ensure!(!value.is_null(),"drop-result lock failed");
        let result=unsafe{value.cast::<u32>().read_unaligned()};let _=unsafe{GlobalUnlock(memory)};Ok(result)})();
    unsafe{ReleaseStgMedium(&mut medium)};result
}
fn exercise(root:&Path)->Result<serde_json::Value>{
    unsafe{OleInitialize(None)?;}let _apartment=Apartment;let (owned,paths)=setup(root)?;
    with_host(|host|{
        use zsdesk_core::model::explorer::{ViewSortColumn,ViewPropertyKey};
        let key=zsdesk_windows::shell::commands::PKEY_NAME;
        {let fence=host.config.fence_mut(3).unwrap();fence.view=FenceView::List;fence.sort=FenceSort::Manual;fence.icon_size=IconSize::Medium;}
        let mut layout=host.menu_layout(3).unwrap();layout.icon_size=16;
        layout.sort_columns=vec![ViewSortColumn{key:ViewPropertyKey{format_id:format!("{:?}",key.fmtid),property_id:key.pid},ascending:true}];
        host.apply_menu_layout(3,&layout,false);
        assert_eq!(host.config.fence(3).unwrap().sort,FenceSort::Manual,"a view-only menu changed manual order");
        host.apply_menu_layout(3,&layout,true);
        assert_eq!(host.config.fence(3).unwrap().sort,FenceSort::Name,"native sort action was lost to list icon-size normalization");
        assert_eq!(host.config.fence(3).unwrap().icon_size,IconSize::Medium);
        host.config.fence_mut(3).unwrap().sort=FenceSort::Manual;
    });
    with_host(|host|host.peek(1,true));ensure!(peeked(1),"hover did not expand");
    with_host(|host|host.begin_modal(1));outside_tick(1);ensure!(peeked(1),"synchronous menu hold collapsed");
    with_host(|host|host.end_modal(1));outside_tick(1);ensure!(!peeked(1),"menu hold did not retire");
    with_host(|host|{host.peek(1,true);host.menu_started(1,40);host.menu_started(1,41);host.menu_finished(1,40);});outside_tick(1);ensure!(peeked(1),"stale completion released a newer menu");
    with_host(|host|host.menu_finished(1,41));outside_tick(1);ensure!(!peeked(1),"async menu hold leaked");

    let key=with_host(|host|host.fences[host.win_index(1).unwrap()].entries[0].key.clone()).unwrap();
    let identifier=pidl::location_pidl(&zsdesk_core::model::explorer::ShellLocation::Path(paths[0].clone())).context("drag PIDL")?;
    let data=desktop::data_object(window(1),&[identifier.clone()])?;
    with_host(|host|{host.peek(1,true);host.peek(2,true);host.begin_internal_drag(1,vec![key.clone()],&data);});
    outside_tick(1);ensure!(peeked(1),"drag source collapsed after pointer left");
    let point=end_point(2);let allowed=windows::Win32::System::Ole::DROPEFFECT_COPY|DROPEFFECT_MOVE;
    ensure!(FenceDrop{fence:2}.enter(&data,0,point,allowed)==DROPEFFECT_MOVE,"internal visual drag did not advertise move");
    outside_tick(2);ensure!(peeked(2),"drag receiver collapsed");
    FenceDrop{fence:1}.leave();outside_tick(2);ensure!(peeked(2),"stale DragLeave cleared another receiver");
    let effect=FenceDrop{fence:2}.dropped(&data,0,point,allowed);
    ensure!(effect==DROPEFFECT_MOVE,"successful visual drop reported cancellation");
    with_host(|host|host.end_internal_drag());
    ensure!(with_host(|host|host.config.fences.iter().filter(|fence|fence.members.contains(&key)).count())==Some(1),"reference migrated more than once");
    ensure!(with_host(|host|host.config.fence(2).unwrap().members.last()==Some(&key))==Some(true),"dropping at end lost the displayed manual order");
    outside_tick(1);outside_tick(2);ensure!(!peeked(1)&&!peeked(2),"completed drag kept fences open forever");

    let foreign=desktop::data_object(window(3),&[identifier])?;
    with_host(|host|{host.peek(2,true);host.peek(3,true);host.begin_internal_drag(2,vec![key.clone()],&data);});
    let point=end_point(3);let _=FenceDrop{fence:3}.enter(&foreign,0,point,allowed);
    ensure!(with_host(|host|host.drag_is_internal)==Some(false),"external data object inherited internal ownership");
    outside_tick(3);ensure!(peeked(3),"external drag receiver collapsed");
    FenceDrop{fence:3}.leave();with_host(|host|host.end_internal_drag());outside_tick(3);ensure!(!peeked(3),"cancelled external drag retained hover state");
    ensure!(fs::read_dir(root.join("mapped"))?.next().is_none(),"DragEnter/Leave moved or copied files");
    let external=root.join("external");fs::create_dir_all(&external)?;
    // Ctrl-copy within a mapped fence creates a distinct physical output.
    // Plain dragging within that same fence only changes its manual order.
    let internal_source=root.join("mapped").join("ctrl-copy.txt");fs::write(&internal_source,b"physical-copy-fixture")?;
    with_host(|host|host.refresh_folder(3));
    let internal_entry=with_host(|host|host.fences[host.win_index(3).unwrap()].entries.iter().find(|entry|entry.path.as_ref()==Some(&internal_source)).cloned()).flatten().context("internal copy entry")?;
    let internal_data=desktop::data_object(window(3),&[internal_entry.pidl.clone()])?;
    with_host(|host|{host.peek(3,true);host.begin_internal_drag(3,vec![internal_entry.key.clone()],&internal_data);});
    let point=end_point(3);
    ensure!(FenceDrop{fence:3}.enter(&internal_data,8,point,allowed)==windows::Win32::System::Ole::DROPEFFECT_COPY,"Ctrl-copy was reported as a move");
    ensure!(FenceDrop{fence:3}.dropped(&internal_data,8,point,allowed)==windows::Win32::System::Ole::DROPEFFECT_COPY,"Ctrl-copy did not complete as copy");
    with_host(|host|host.end_internal_drag());
    let copies:Vec<_>=fs::read_dir(root.join("mapped"))?.filter_map(|entry|entry.ok().map(|entry|entry.path())).collect();
    ensure!(copies.len()==2&&copies.iter().all(|path|fs::read(path).is_ok_and(|data|data==b"physical-copy-fixture")),"Ctrl-copy failed to retain source and create a distinct file");
    with_host(|host|host.begin_internal_drag(3,vec![internal_entry.key.clone()],&internal_data));
    ensure!(FenceDrop{fence:3}.enter(&internal_data,0,point,allowed)==DROPEFFECT_MOVE,"same-folder manual order rejected drag");
    ensure!(FenceDrop{fence:3}.dropped(&internal_data,0,point,allowed)==DROPEFFECT_MOVE,"same-folder manual order failed");
    with_host(|host|host.end_internal_drag());
    ensure!(copies.iter().all(|path|fs::read(path).is_ok_and(|data|data==b"physical-copy-fixture")),"manual ordering altered files");
    drop(internal_data);for path in copies{fs::remove_file(path)?;}with_host(|host|host.refresh_folder(3));
    for moving in [false,true]{
        let name=format!("{}-{}.txt",if moving{"move"}else{"copy"},std::process::id());let source=external.join(&name);let destination=root.join("mapped").join(&name);
        ensure!(!destination.exists(),"external transfer fixture already exists");fs::write(&source,b"external owned transfer")?;
        let identifier=pidl::location_pidl(&zsdesk_core::model::explorer::ShellLocation::Path(source.clone())).context("external PIDL")?;
        let data=desktop::data_object(window(3),&[identifier])?;
        let allowed=if moving{DROPEFFECT_MOVE}else{windows::Win32::System::Ole::DROPEFFECT_COPY};
        with_host(|host|host.peek(3,true));let point=end_point(3);let accepted=FenceDrop{fence:3}.enter(&data,0,point,allowed);
        ensure!(accepted==allowed,"native target rejected ordinary owned HDROP");
        let effect=FenceDrop{fence:3}.dropped(&data,0,point,allowed);
        let expected=if moving{DROPEFFECT_NONE}else{windows::Win32::System::Ole::DROPEFFECT_COPY};
        ensure!(effect==expected,"optimized transfer reported a source-cleanup MOVE");
        ensure!(returned_effect(&data,windows::Win32::UI::Shell::CFSTR_PERFORMEDDROPEFFECT)?==expected.0,"performed-effect contract mismatch");
        ensure!(returned_effect(&data,windows::Win32::UI::Shell::CFSTR_LOGICALPERFORMEDDROPEFFECT)?==allowed.0,"logical completion missing");
        ensure!(fs::read(&destination)?==b"external owned transfer"&&source.exists()!=moving,"physical transfer/source state mismatch");
        ensure!(with_host(|host|host.fences[host.win_index(3).unwrap()].selected_entries().iter().any(|entry|entry.path.as_ref()==Some(&destination)))==Some(true),"exact external output was not selected");
        ensure!(with_host(|host|host.fences[host.win_index(3).unwrap()].notice.is_none())==Some(true),"successful transfer displayed a text banner");
        outside_tick(3);ensure!(!peeked(3),"completed transfer kept hover preview open");
        drop(data);fs::remove_file(destination)?;if source.exists(){fs::remove_file(source)?;}with_host(|host|{host.clear_notice(3);host.refresh_folder(3);});
    }

    with_host(|host|{
        let index=host.win_index(2).unwrap();let key=host.fences[index].entries[0].key.clone();
        host.fences[index].selection.insert(key);host.fences[index].focus=Some(0);host.fences[index].anchor=Some(0);
    });
    let focused=with_host(|host|{let win=&host.fences[host.win_index(2).unwrap()];win.entries[win.focus.unwrap()].key.clone()}).unwrap();
    with_host(|host|{host.menu_command(Some(2),"sort-name",POINT::default());host.menu_command(Some(2),"sort-desc",POINT::default());});
    ensure!(with_host(|host|{let win=&host.fences[host.win_index(2).unwrap()];win.focus.is_some_and(|index|win.entries[index].key==focused)&&win.anchor.is_some_and(|index|win.entries[index].key==focused)&&win.selection.contains(&focused)})==Some(true),"sort changed selected/focused identity");
    with_host(|host|{host.menu_command(Some(1),"rule-none",POINT::default());host.menu_command(Some(2),"rule-none",POINT::default());host.delete_fence(1);});
    ensure!(with_host(|host|host.config.fences.iter().filter(|fence|fence.is_desktop()).all(|fence|fence.rule.is_none()&&!fence.catch_all))==Some(true),"deletion silently re-enabled auto collection");
    let saved=with_host(|host|{host.save_now();host.config_path.clone()}).unwrap();let restored=load_config(&saved);
    ensure!(restored.fences.iter().filter(|fence|fence.is_desktop()).all(|fence|fence.rule.is_none()&&!fence.catch_all),"restart changed disabled collection");

    // A real asynchronous menu constructs and tracks on a separate STA while
    // the fence UI continues handling its outside-pointer close checks.
    let (pidls,profile)=with_host(|host|{host.peek(2,true);let index=host.win_index(2).unwrap();host.fences[index].selection.clear();let key=host.fences[index].entries[0].key.clone();host.fences[index].selection.insert(key);
        (host.selected_pidls(2),host.explorer_config.clone())}).unwrap();
    let was_locked=with_host(|host|host.config.fence(2).unwrap().locked).unwrap();
    let mut request=menu_worker::Request::new(2,window(2),end_point(2),profile,vec![entry("锁定格子(&Z)","lock",true,false)],menu_worker::Target::Items{pidls,key:None},false);
    request.probe=Some(menu_worker::Probe{delay_ms:250,copy:false});super::super::queue_menu(request);
    let deadline=Instant::now()+Duration::from_secs(12);let mut held_ticks=0;
    loop{if !with_host(|host|host.pending_menu.is_some()).unwrap_or(false){break;}
        outside_tick(2);ensure!(peeked(2),"live asynchronous menu caused auto collapse");held_ticks+=1;pump(10);ensure!(Instant::now()<deadline,"owned menu did not finish");}
    ensure!(held_ticks>1,"async construction did not exercise timer interleaving");outside_tick(2);ensure!(!peeked(2),"finished menu did not release fence");
    ensure!(with_host(|host|host.config.fence(2).unwrap().locked==!was_locked)==Some(true),"real host_proc lost menu READY validation/completion or application command");
    with_host(|host|{host.peek(2,true);host.menu_started(2,99);host.menu_cancelled();});unsafe{SetCapture(window(2));SendMessageW(window(2),WM_CANCELMODE,None,None);let _=ReleaseCapture();}
    outside_tick(2);ensure!(!peeked(2),"cancellation retained a modal/capture hold");

    // Exact completion ownership rejects an old folder epoch and never takes
    // an unrelated file merely because it appeared within a time window.
    let mapped=root.join("mapped");let created=mapped.join("new.txt");
    with_host(|host|{host.bind_operation(3).unwrap();host.select_completed_paths(3,&[created.clone()],false);});
    ensure!(with_host(|host|host.fences[host.win_index(3).unwrap()].pending_selection.is_some())==Some(true),"unmaterialized completion was discarded");
    fs::write(&created,b"exact output")?;with_host(|host|{host.refresh_folder(3);host.poll_completed_selection(3);});
    ensure!(with_host(|host|{let win=&host.fences[host.win_index(3).unwrap()];win.pending_selection.is_none()&&win.selected_entries().iter().any(|entry|entry.path.as_ref()==Some(&created))})==Some(true),"late completion did not select exact file");
    let selection=with_host(|host|host.fences[host.win_index(3).unwrap()].selection.clone()).unwrap();
    with_host(|host|host.select_completed_paths(3,&[mapped.join("never-created.txt")],false));
    unsafe{SendMessageW(window(3),WM_CHAR,Some(WPARAM(b'v' as usize)),Some(LPARAM(1)));SendMessageW(window(3),WM_KEYUP,Some(WPARAM(0x56)),Some(LPARAM(1)));}
    with_host(|host|host.poll_completed_selection(3));
    ensure!(with_host(|host|host.fences[host.win_index(3).unwrap()].pending_selection.is_some())==Some(true),"initiating command tail cancelled pending selection");
    unsafe{SendMessageW(window(3),WM_KEYDOWN,Some(WPARAM(0x87)),Some(LPARAM(1)));}
    with_host(|host|host.poll_completed_selection(3));
    ensure!(with_host(|host|{let window=&host.fences[host.win_index(3).unwrap()];window.pending_selection.is_none()&&window.selection==selection})==Some(true),"new user input did not cancel late selection");
    let old=with_host(|host|{let win=&host.fences[host.win_index(3).unwrap()];zsdesk_windows::shell::file_transfer::TransferOrigin{view:win.hwnd.0 as isize,pane:3,tab:3,epoch:win.operation_epoch,activity:0}}).unwrap();
    with_host(|host|host.add_folder_tab(3,mapped.clone()));
    let other=with_host(|host|host.config.active_tab(3).unwrap()).unwrap();ensure!(other!=3,"second same-folder tab was not activated");
    let other_selection=with_host(|host|host.fences[host.win_index(other).unwrap()].selection.clone()).unwrap();
    with_host(|host|{let index=host.win_index(3).unwrap();host.fences[index].selection.clear();
        assert!(host.operation_current(3,&old));let accepted=host.adopt_completed_paths(3,&[created.clone()]);host.select_completed_paths(3,&accepted,false);});
    ensure!(with_host(|host|host.fences[host.win_index(3).unwrap()].selection.is_empty()&&host.fences[host.win_index(other).unwrap()].selection==other_selection)==Some(true),"completion for hidden A changed same-directory B selection");
    with_host(|host|host.switch_tab(3));
    let nested=mapped.join("nested");fs::create_dir_all(&nested)?;with_host(|host|host.browse_into(3,nested.clone()));
    ensure!(with_host(|host|!host.operation_current(3,&old)&&host.adopt_completed_paths(3,&[created.clone()]).is_empty())==Some(true),"old completion crossed mapping-folder navigation");
    for path in &paths{ensure!(fs::read(path)?==b"isolated desktop lifecycle","visual drag changed source file bytes");}
    ensure!(fs::read(&created)?==b"exact output","selection changed output bytes");
    drop(data);drop(foreign);drop(owned);pump(30);
    ensure!(fs::read(&created)?==b"exact output","fixture changed before cleanup");fs::remove_file(&created)?;fs::remove_dir(&nested)?;
    let deadline=Instant::now()+Duration::from_secs(5);while super::super::pending_menu_threads()>0&&Instant::now()<deadline{pump(10);}
    ensure!(super::super::pending_menu_threads()==0,"owned menu thread remains active");
    Ok(serde_json::json!({"async_menu_keeps_fence_open":true,"real_host_proc_menu_ready_dispatch":true,"async_lock_command_completed":true,"stale_menu_completion_ignored":true,"source_and_target_drag_holds":true,"cancel_releases_holds":true,
        "internal_move_effect_and_single_membership":true,"external_object_not_treated_as_internal":true,"manual_end_insertion_preserves_order":true,
        "real_external_copy_and_optimized_move":true,"performed_and_logical_drop_effects_verified":true,
        "sort_preserves_selection_focus_anchor":true,"collection_disable_survives_deletion_restart":true,"exact_delayed_selection":true,"new_input_cancels_late_selection":true,"command_tail_keeps_pending_selection":true,"stale_folder_epoch_rejected":true,"same_directory_tab_completion_isolated":true,
        "filesystem_contents_unchanged":true,"real_desktop_modified":false,"held_timer_ticks":held_ticks}))
}
#[test]
#[ignore="Private desktop fence menu/drag/collection lifecycle regression; requires ZSDESK_NATIVE_TEST_OUTPUT"]
fn native_fence_collection_drag_and_menu_lifecycle()->Result<()>{
    let root=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("native output")?).join("fence-lifecycle");ensure!(root.is_absolute(),"absolute fixture root required");fs::create_dir_all(&root)?;
    let foreground=unsafe{GetForegroundWindow()};let name:Vec<u16>=format!("ZSDesk.FenceLifecycle.{}",std::process::id()).encode_utf16().chain(Some(0)).collect();
    let desktop=unsafe{CreateDesktopW(name.as_ptr(),std::ptr::null(),std::ptr::null(),0,0x000f00ff,std::ptr::null())};ensure!(!desktop.is_null(),"private desktop unavailable");
    let raw=desktop as usize;let target=root.clone();let result=thread::spawn(move||->Result<serde_json::Value>{ensure!(unsafe{SetThreadDesktop(raw as*mut c_void)}!=0,"SetThreadDesktop failed");exercise(&target)}).join().map_err(|_|anyhow::anyhow!("lifecycle test panicked")).and_then(|value|value);
    let deadline=Instant::now()+Duration::from_secs(3);let closed=loop{if unsafe{CloseDesktop(desktop)}!=0{break true;}if Instant::now()>=deadline{break false;}thread::sleep(Duration::from_millis(20));};
    let current=unsafe{GetForegroundWindow()};let mut process=0;unsafe{GetWindowThreadProcessId(current,Some(&mut process));}
    let isolated=process!=std::process::id();let report=serde_json::json!({"state":if result.is_ok()&&closed&&isolated{"passed"}else{"failed"},"checks":result.as_ref().map_err(|error|format!("{error:#}")),"private_desktop_closed":closed,"foreground_unchanged":foreground==current,"test_did_not_activate":isolated});
    fs::write(root.join("verification.current.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");result?;ensure!(closed&&isolated,"owned desktop lifecycle failed");Ok(())
}
#[test]
fn exact_output_selection_keeps_case_distinct_names(){
    let entry=|path:&str|ShellEntry{key:path.into(),name:path.into(),pidl:vec![0,0],is_folder:false,is_file_system:true,is_link:false,is_stream:false,can_drop:false,ghosted:false,path:Some(path.into()),size:Some(0),modified:None};
    let entries=vec![entry(r"C:\fixture\A.TXT"),entry(r"C:\fixture\a.txt")];
    assert_eq!(matching_output_indices(&entries,&[r"C:\fixture\A.TXT".into()]),vec![0]);
    assert_eq!(matching_output_indices(&entries,&[r"C:\fixture\a.txt".into()]),vec![1]);
    assert!(matching_output_indices(&entries,&[r"C:\fixture\a.TXT".into()]).is_empty());
    assert_eq!(matching_output_indices(&entries[..1],&[r"c:\FIXTURE\a.txt".into()]),vec![0]);
}
