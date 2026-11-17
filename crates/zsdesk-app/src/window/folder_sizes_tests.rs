use super::*;
use anyhow::{Result,ensure,Context};
use windows::{core::PCWSTR,Win32::{Foundation::{HINSTANCE,LPARAM,LRESULT,RECT,WPARAM},System::{LibraryLoader::GetModuleHandleW,Ole::{OleInitialize,OleUninitialize}},
    UI::{Shell::{IShellItem2,IShellItemArray,_SVGIO,SVGIO_FLAG_VIEWORDER,SORTCOLUMN,SORT_ASCENDING,SORT_DESCENDING,CM_COLUMNINFO,CM_MASK_WIDTH},WindowsAndMessaging::*}}};
use zsdesk_windows::shell::{Panes,commands::{PKEY_NAME,PKEY_SIZE,Column},WM_SHELL_NAVIGATED,WM_SHELL_FOLDER_INVOKED,WM_SHELL_SELECTION_CHANGED,acknowledge_selection_changed,shell_item};
thread_local!{static CHANGES:std::cell::Cell<u32>=const{std::cell::Cell::new(0)};}
thread_local!{static MULTI_CHANGES:std::cell::RefCell<Vec<String>>=const{std::cell::RefCell::new(Vec::new())};}
fn pump(){let mut message=MSG::default();for _ in 0..128{if !unsafe{PeekMessageW(&mut message,None,0,0,PM_REMOVE)}.as_bool(){break;}if message.message!=WM_QUIT{unsafe{let _=TranslateMessage(&message);DispatchMessageW(&message);}}}}
unsafe extern "system" fn owner(window:HWND,message:u32,wp:WPARAM,lp:LPARAM)->LRESULT{
    if matches!(message,WM_SHELL_NAVIGATED|WM_SHELL_FOLDER_INVOKED){if lp.0!=0{unsafe{drop(Box::from_raw(lp.0 as*mut ShellLocation));}}return LRESULT(0);}
    if message==WM_SHELL_SELECTION_CHANGED{unsafe{acknowledge_selection_changed(lp.0);}return LRESULT(0);}
    if message==WM_CHANGED{let change=zsdesk_windows::shell::notify::read_change(wp,lp);if change.event!=0{CHANGES.with(|value|value.set(value.get()+1));}return LRESULT(0);}
    if message==WM_CHANGED+1{let change=zsdesk_windows::shell::notify::read_change(wp,lp);MULTI_CHANGES.with(|paths|paths.borrow_mut().extend(change.first.into_iter().chain(change.second)));return LRESULT(0);}
    unsafe{DefWindowProcW(window,message,wp,lp)}
}
struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}
struct Window(HWND);impl Drop for Window{fn drop(&mut self){let _=unsafe{DestroyWindow(self.0)};}}
struct Fixture(PathBuf);impl Drop for Fixture{fn drop(&mut self){if self.0.is_absolute()&&std::fs::read(self.0.join("owner")).is_ok_and(|v|v==b"zsdesk-folder-total-native"){
    let _=std::fs::remove_dir_all(&self.0);}}}
fn order(folder:&IFolderView2)->Result<Vec<String>>{
    let array:IShellItemArray=unsafe{folder.Items(_SVGIO(SVGIO_ALLVIEW.0|SVGIO_FLAG_VIEWORDER.0))}?;let mut rows=Vec::new();
    for index in 0..unsafe{array.GetCount()}?{let item=unsafe{array.GetItemAt(index)}?;let value=unsafe{item.GetDisplayName(SIGDN_FILESYSPATH)}?;
        let path=unsafe{PathBuf::from(OsString::from_wide(value.as_wide()))};unsafe{CoTaskMemFree(Some(value.0.cast()))};rows.push(path.file_name().unwrap().to_string_lossy().into_owned());}Ok(rows)
}
fn ready(controller:&mut FolderSizes,browser:&ExplorerBrowserHost,path:&Path,profile:&Path)->Result<()>{
    let until=Instant::now()+Duration::from_secs(15);loop{pump();if CHANGES.with(|value|value.replace(0))>0{controller.invalidate();}
        controller.poll(Some(browser),&ShellLocation::Path(path.into()),true,false,profile);
        if matches!(controller.status(),Status::Ready{unknown:0,..})&&!controller.busy(){return Ok(());}
        ensure!(Instant::now()<until,"folder totals did not finish: {:?}",controller.status());thread::sleep(Duration::from_millis(15));}
}
#[test]fn disabled_folder_sizes_have_no_worker_or_query(){let mut controller=FolderSizes::default();
    assert!(controller.poll(None,&ShellLocation::ThisPc,false,false,Path::new(r"C:\profile.json")).is_none());assert!(controller.worker.is_none());assert!(!controller.busy());assert_eq!(controller.status(),&Status::Disabled);}
#[test]fn cancellation_discards_queued_and_completed_size_replies(){
    let state=Arc::new(Shared{mailbox:Mutex::new(Mailbox{request:Some(Request{generation:1,profile:PathBuf::new(),directory:PathBuf::new(),rows:Arc::new(Vec::new()),watch_needed:false}),replies:VecDeque::from([Reply{generation:1,value:ResultValue::Done(None)}])}),wake:Condvar::new(),generation:AtomicU64::new(1),stop:AtomicBool::new(false)});
    let worker=Worker{shared:state,thread:None};worker.cancel(2);assert!(worker.take().is_empty());assert!(worker.shared.mailbox.lock().unwrap().request.is_none());assert_eq!(worker.shared.generation.load(Ordering::Acquire),2);
}
#[test]fn total_size_unknown_is_distinct_from_zero_and_large_values(){for bytes in [0,u32::MAX as u64+1,u64::MAX-1]{let value=Variant::size(bytes);assert_eq!(unsafe{PropVariantToUInt64(&value.0)}.unwrap(),bytes);}assert_ne!(None,Some(0u64));}

#[test]#[ignore="Owned native view, native total calculations, sort/update/selection/layout restoration; requires ZSDESK_NATIVE_TEST_OUTPUT"]
fn native_folder_size_column_is_view_only_and_reversible()->Result<()>{
    let output=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("native output")?);ensure!(output.is_absolute(),"absolute output required");
    let root=output.join(format!("folder-totals-{}",std::process::id()));ensure!(!root.exists(),"owned fixture already exists");std::fs::create_dir(&root)?;std::fs::write(root.join("owner"),b"zsdesk-folder-total-native")?;let fixture=Fixture(root.clone());
    let directory=root.join("files");std::fs::create_dir(&directory)?;
    for (name,size) in [("a-large",5000),("m-middle",2000),("z-small",2)]{std::fs::create_dir(directory.join(name))?;
        let data=if name=="z-small"{let nested=directory.join(name).join("nested");std::fs::create_dir(&nested)?;nested.join("data.bin")}else{directory.join(name).join("data.bin")};
        std::fs::write(data,vec![0u8;size])?;}
    std::fs::write(directory.join("small.bin"),[0u8;101])?;std::fs::write(directory.join("large.bin"),[0u8;4096])?;
    let profile=root.join("explorer-session.json");crate::storage::save(&profile,&crate::storage::default_session())?;
    unsafe{OleInitialize(None)}?;let apartment=Apartment;let instance=HINSTANCE(unsafe{GetModuleHandleW(None)}?.0);let class=windows::core::w!("ZSDesk.TotalSize.Regression");
    unsafe{RegisterClassW(&WNDCLASSW{hInstance:instance,lpfnWndProc:Some(owner),lpszClassName:class,..Default::default()});}
    let original_foreground=unsafe{GetForegroundWindow()};let window=Window(unsafe{CreateWindowExW(WS_EX_NOACTIVATE|WS_EX_TOOLWINDOW,class,PCWSTR::null(),WS_POPUP|WS_CLIPCHILDREN,-28000,-28000,900,560,None,None,Some(instance),None)}?);
    let browser=unsafe{ExplorerBrowserHost::create_transient(window.0,RECT{left:0,top:0,right:900,bottom:560},false,Panes::default())}?;
    unsafe{let _=ShowWindow(window.0,SW_SHOWNOACTIVATE);browser.navigate(&ShellLocation::Path(directory.clone()))?;}
    let until=Instant::now()+Duration::from_secs(10);let (shell,folder)=loop{pump();if let Ok(shell)=unsafe{browser.current_view()}{if let Ok(folder)=shell.cast::<IFolderView2>(){if unsafe{folder.ItemCount(SVGIO_ALLVIEW)}.unwrap_or(0)==5{break (shell,folder);}}}
        ensure!(Instant::now()<until,"native view did not enumerate fixture");thread::sleep(Duration::from_millis(10));};
    browser.set_view_mode(4,16)?;let columns:IColumnManager=shell.cast()?;unsafe{columns.SetColumns(&[PKEY_NAME,PKEY_SIZE])}?;
    let requested_width=CM_COLUMNINFO{cbSize:std::mem::size_of::<CM_COLUMNINFO>()as u32,dwMask:CM_MASK_WIDTH.0 as u32,uWidth:137,..Default::default()};unsafe{columns.SetColumnInfo(&PKEY_SIZE,&requested_width)}?;
    browser.select_child(std::ffi::OsStr::new("m-middle"))?;let native_window=browser.view_window();
    let original:IShellItem2=unsafe{shell_item(&ShellLocation::Path(directory.join("m-middle")))}?.cast()?;let underlying=unsafe{original.GetUInt64(&PKEY_SIZE)}.ok();
    let mut controller=FolderSizes::default();ready(&mut controller,&browser,&directory,&profile)?;ensure!(folder_totals::enabled(&shell),"total-size column not active");
    let mut orders=Vec::new();
    for (ascending,expected) in [(true,vec!["z-small","m-middle","a-large"]),(false,vec!["a-large","m-middle","z-small"])]{
        browser.set_sort(Column::Size,ascending)?;let until=Instant::now()+Duration::from_secs(3);let actual=loop{pump();let actual=order(&folder)?;let folders:Vec<_>=actual.iter().filter(|name|!name.ends_with(".bin")).map(String::as_str).collect();if folders==expected{break actual;}ensure!(Instant::now()<until,"native total-size order incorrect: {actual:?}");thread::sleep(Duration::from_millis(10));};
        let files:Vec<_>=actual.iter().filter(|name|name.ends_with(".bin")).map(String::as_str).collect();ensure!(files==if ascending{vec!["small.bin","large.bin"]}else{vec!["large.bin","small.bin"]},"file sizes disappeared from sorting");
        orders.push(serde_json::json!({"ascending":ascending,"actual":actual}));
    }
    ensure!(browser.selection_paths_limit(8)==[directory.join("m-middle")],"sort changed selected identity");
    let captured=unsafe{browser.capture_view()}.context("view layout")?;ensure!(captured.columns.len()==2&&captured.columns[1].key.property_id==12,"presentation property leaked into persisted columns: {:?}",captured.columns);
    // Let creation/sort notifications drain before changing a deep descendant.
    // A queued creation event must not accidentally validate recursive watching.
    let drain_started=Instant::now();let mut quiet=Instant::now();
    loop{pump();if CHANGES.with(|count|count.replace(0))>0{quiet=Instant::now();}
        if drain_started.elapsed()>=Duration::from_secs(2)&&quiet.elapsed()>=Duration::from_secs(1){break;}
        ensure!(drain_started.elapsed()<Duration::from_secs(8),"initial Shell notifications did not settle");thread::sleep(Duration::from_millis(20));}
    ensure!(controller.watch.is_some(),"native filesystem size watcher did not start");
    let changed_at=Instant::now();std::fs::write(directory.join("z-small/nested/data.bin"),vec![0u8;12000])?;
    let until=Instant::now()+Duration::from_secs(6);loop{pump();if CHANGES.with(|count|count.replace(0))>0{controller.invalidate();}
        controller.poll(Some(&browser),&ShellLocation::Path(directory.clone()),true,false,&profile);
        if controller.rows.iter().zip(&controller.values).any(|(row,value)|row.item.name=="z-small"&&value.as_ref().and_then(|value|value.bytes)==Some(12000)){break;}
        ensure!(Instant::now()<until,"recursive filesystem size change notification missing");thread::sleep(Duration::from_millis(20));}
    let notification_ms=changed_at.elapsed().as_millis();
    ready(&mut controller,&browser,&directory,&profile)?;println!("updated totals: {:?}",controller.rows.iter().zip(&controller.values).map(|(row,value)|(&row.item.name,value.as_ref().and_then(|value|value.bytes))).collect::<Vec<_>>());let until=Instant::now()+Duration::from_secs(3);loop{pump();let actual=order(&folder)?;let first=actual.iter().find(|name|!name.ends_with(".bin")).unwrap();if first=="z-small"{break;}ensure!(Instant::now()<until,"modified folder was not resorted: {actual:?}");thread::sleep(Duration::from_millis(10));}
    ensure!(browser.view_window()==native_window,"total sizes replaced the native file view");ensure!(browser.selection_paths_limit(8)==[directory.join("m-middle")],"refresh changed selection");
    let peer_window=Window(unsafe{CreateWindowExW(WS_EX_NOACTIVATE|WS_EX_TOOLWINDOW,class,PCWSTR::null(),WS_POPUP|WS_CLIPCHILDREN,-27000,-27000,700,500,None,None,Some(instance),None)}?);
    let peer=unsafe{ExplorerBrowserHost::create_transient(peer_window.0,RECT{left:0,top:0,right:700,bottom:500},false,Panes::default())}?;
    unsafe{let _=ShowWindow(peer_window.0,SW_SHOWNOACTIVATE);peer.navigate(&ShellLocation::Path(directory.clone()))?;}
    let until=Instant::now()+Duration::from_secs(5);loop{pump();if peer.status().items==5{break;}ensure!(Instant::now()<until,"peer view did not initialize");thread::sleep(Duration::from_millis(10));}
    peer.set_view_mode(4,16)?;let peer_shell=unsafe{peer.current_view()}?;let peer_columns:IColumnManager=peer_shell.cast()?;unsafe{peer_columns.SetColumns(&[PKEY_NAME,PKEY_SIZE])}?;
    let mut peer_sizes=FolderSizes::default();ready(&mut peer_sizes,&peer,&directory,&profile)?;
    peer.set_sort(Column::Size,true)?;peer_sizes.cancel();ensure!(folder_totals::enabled(&shell)&&!folder_totals::enabled(&peer_shell),"one pane disabled another pane's total sizes");
    for (row,value) in controller.rows.iter().zip(&controller.values){if let Some(expected)=value.as_ref().and_then(|value|value.bytes){let actual=Variant(unsafe{folder.GetViewProperty(row.pidl.pointer(),&PKEY_TOTAL_SIZE)}?);ensure!(unsafe{PropVariantToUInt64(&actual.0)}?==expected,"peer view cleared this view's size cache");}}
    drop(peer_sizes);drop(peer_columns);drop(peer_shell);drop(peer);drop(peer_window);pump();
    for row in controller.rows.iter(){if row.item.name=="m-middle"{write_size(&folder,&row.pidl,9_000_000_000)?;}if row.item.name=="a-large"{write_size(&folder,&row.pidl,5_000_000_000)?;}}
    folder_totals::refresh_order(&shell)?;let until=Instant::now()+Duration::from_secs(3);loop{pump();if order(&folder)?.iter().find(|name|!name.ends_with(".bin")).is_some_and(|name|name=="m-middle"){break;}ensure!(Instant::now()<until,"64-bit totals were not compared exactly");thread::sleep(Duration::from_millis(10));}
    for row in controller.rows.iter(){if row.item.name=="a-large"{write_size(&folder,&row.pidl,9_000_000_000)?;}}
    unsafe{folder.SetSortColumns(&[SORTCOLUMN{propkey:PKEY_TOTAL_SIZE,direction:SORT_DESCENDING},SORTCOLUMN{propkey:PKEY_NAME,direction:SORT_DESCENDING}])}?;
    pump();folder_totals::refresh_order(&shell)?;pump();
    let multiple=unsafe{browser.capture_view()}.context("multi-column layout")?;ensure!(multiple.sort_columns.len()==2&&multiple.sort_columns[0].key.property_id==12&&multiple.sort_columns[1].key.property_id==10&&multiple.sort_columns.iter().all(|column|!column.ascending),"secondary sort keys were changed");
    browser.set_sort(Column::Modified,true)?;folder_totals::refresh_order(&shell)?;ensure!(browser.sort()==Some((Column::Modified,true)),"background update overrode a new user sort");
    browser.set_sort(Column::Size,false)?;folder_totals::refresh_order(&shell)?;pump();
    let logical=unsafe{browser.capture_view()}.context("updated logical layout")?;ensure!(logical.sort_columns.len()==1&&logical.sort_columns[0].key.property_id==12,"managed refresh key leaked into saved sorting");
    let originals:Vec<_>=controller.rows.iter().zip(&controller.originals).map(|(row,original)|{let value=original.as_ref().unwrap();(row.pidl.clone(),unsafe{value.0.Anonymous.Anonymous.vt},unsafe{PropVariantToUInt64(&value.0)}.ok())}).collect();
    controller.cancel();ensure!(!folder_totals::enabled(&shell),"total-size mode was not released");
    let mut visible=vec![PKEY_NAME;unsafe{columns.GetColumnCount(CM_ENUM_VISIBLE)}?as usize];unsafe{columns.GetColumns(CM_ENUM_VISIBLE,&mut visible)}?;ensure!(visible==[PKEY_NAME,PKEY_SIZE],"original columns not restored");
    let mut width=CM_COLUMNINFO{cbSize:std::mem::size_of::<CM_COLUMNINFO>()as u32,dwMask:CM_MASK_WIDTH.0 as u32,..Default::default()};unsafe{columns.GetColumnInfo(&PKEY_SIZE,&mut width)}?;ensure!(width.uWidth==137,"column width lost");
    for (row,kind,bytes) in &originals{let value=Variant(unsafe{folder.GetViewProperty(row.pointer(),&PKEY_TOTAL_SIZE)}.unwrap_or_default());ensure!(unsafe{value.0.Anonymous.Anonymous.vt}==*kind&&unsafe{PropVariantToUInt64(&value.0)}.ok()==*bytes,"original total property was not restored");}
    ensure!(unsafe{folder.GetSortColumnCount()}?==1,"managed refresh key remained after disabling");let mut sort=[SORTCOLUMN::default()];unsafe{folder.GetSortColumns(&mut sort)}?;ensure!(sort[0].propkey==PKEY_SIZE&&sort[0].direction==SORT_DESCENDING,"logical size sort was not restored");
    ensure!(unsafe{original.GetUInt64(&PKEY_SIZE)}.ok()==underlying,"filesystem property changed");ensure!(std::fs::metadata(directory.join("m-middle/data.bin"))?.len()==2000,"file contents changed");
    let watched=[root.join("watch-a"),root.join("watch-b")];for path in &watched{std::fs::create_dir(path)?;}
    let ids:Vec<_>=watched.iter().map(|path|zsdesk_windows::shell::pidl::location_pidl(&ShellLocation::Path(path.clone())).context("watch PIDL")).collect::<Result<_>>()?;
    let watcher=ChangeNotify::register(window.0,WM_CHANGED+1,&ids,false).context("multiple folder registrations")?;
    let until=Instant::now()+Duration::from_secs(2);while Instant::now()<until{pump();thread::sleep(Duration::from_millis(20));}
    MULTI_CHANGES.with(|paths|paths.borrow_mut().clear());for path in &watched{std::fs::write(path.join("created.bin"),[1u8])?;}
    let until=Instant::now()+Duration::from_secs(6);loop{pump();let both=MULTI_CHANGES.with(|paths|watched.iter().all(|root|paths.borrow().iter().any(|path|Path::new(path).starts_with(root))));
        if both{break;}ensure!(Instant::now()<until,"one of the registered folders did not notify");thread::sleep(Duration::from_millis(20));}
    drop(watcher);pump();MULTI_CHANGES.with(|paths|paths.borrow_mut().clear());
    for path in &watched{std::fs::write(path.join("after-drop.bin"),[2u8])?;}
    let until=Instant::now()+Duration::from_secs(2);while Instant::now()<until{pump();thread::sleep(Duration::from_millis(20));}
    ensure!(MULTI_CHANGES.with(|paths|paths.borrow().is_empty()),"released folder registration still delivers changes");
    let report=serde_json::json!({"state":"passed","sorting":true,"orders":orders,"native_backend":true,"mixed_files_and_folders":true,"large_u64":true,"recursive_change_resorted":true,"raw_nested_write_notified":true,"raw_nested_write_notification_ms":notification_ms,"multiple_watch_roots":true,"all_watch_roots_deregistered":true,"secondary_keys_preserved":true,"new_user_sort_respected":true,"same_folder_panes_independent":true,"selection_preserved":true,"same_native_view":true,"columns_and_width_restored":true,"metadata_read_only":true});
    std::fs::write(output.join("folder-size-sort.current.json"),serde_json::to_vec_pretty(&report)?)?;
    drop(controller);drop(original);drop(originals);drop(columns);drop(folder);drop(shell);drop(browser);drop(window);pump();drop(apartment);drop(fixture);
    let after=unsafe{GetForegroundWindow()};let mut pid=0;unsafe{GetWindowThreadProcessId(after,Some(&mut pid));}ensure!(pid!=std::process::id(),"test activated an owned window");println!("{}",serde_json::json!({"result":report,"foreground_unchanged":after==original_foreground}));Ok(())
}
