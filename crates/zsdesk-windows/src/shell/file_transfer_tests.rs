use super::*;
#[test]fn native_file_data_preserves_shell_formats_and_move_intent_without_changing_clipboard(){
    use windows::Win32::System::{Com::{DVASPECT_CONTENT,FORMATETC,TYMED_HGLOBAL},Ole::ReleaseStgMedium};
    unsafe{OleInitialize(None)}.unwrap();
    struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}let _apartment=Apartment;
    let root=std::env::temp_dir().join(format!("zsdesk-data-object-{}-{}",std::process::id(),unsafe{GetCurrentThreadId()}));
    std::fs::create_dir_all(root.join("另一目录")).unwrap();
    struct Fixture(PathBuf);impl Drop for Fixture{fn drop(&mut self){let _=std::fs::remove_dir_all(&self.0);}}let _fixture=Fixture(root.clone());
    let paths=vec![root.join("文件一.txt"),root.join("另一目录").join("文件二.txt")];
    for path in &paths{std::fs::write(path,b"clipboard-format-fixture").unwrap();}
    let sequence=unsafe{GetClipboardSequenceNumber()};
    for cut in [false,true]{
        let data=file_data_object(HWND::default(),&paths,cut).unwrap();
        for name in [CFSTR_SHELLIDLIST,CFSTR_PREFERREDDROPEFFECT]{
            let format=unsafe{RegisterClipboardFormatW(name)};
            let request=FORMATETC{cfFormat:format as u16,ptd:std::ptr::null_mut(),dwAspect:DVASPECT_CONTENT.0,lindex:-1,tymed:TYMED_HGLOBAL.0 as u32};
            unsafe{data.QueryGetData(&request)}.unwrap();
            let mut medium=unsafe{data.GetData(&request)}.unwrap();
            if name==CFSTR_PREFERREDDROPEFFECT{
                let memory=unsafe{medium.u.hGlobal};let pointer=unsafe{GlobalLock(memory)};assert!(!pointer.is_null());
                assert_eq!(unsafe{pointer.cast::<u32>().read_unaligned()},if cut{2}else{1});let _=unsafe{GlobalUnlock(memory)};
            }
            unsafe{ReleaseStgMedium(&mut medium)};
        }
        // Shell expands 8.3 parent paths (for example a runner's TEMP path).
        // Verify the same files in the same order, independent of that spelling.
        let actual=crate::ole::hdrop_paths(&data).iter().map(std::fs::canonicalize).collect::<std::io::Result<Vec<_>>>().unwrap();
        let expected=paths.iter().map(std::fs::canonicalize).collect::<std::io::Result<Vec<_>>>().unwrap();
        assert_eq!(actual,expected);
    }
    assert_eq!(unsafe{GetClipboardSequenceNumber()},sequence);
}
#[test]fn shell_cancellation_and_production_collision_policy(){
    for code in [0x80270000u32,0x80270001,0x800704c7,0x80004004]{assert!(is_cancelled(HRESULT(code as i32)));}
    for code in [0u32,1,0x80070005,0x80070020,0x80270003]{assert!(!is_cancelled(HRESULT(code as i32)));}
    for action in [Operation::Copy,Operation::Move,Operation::Delete{permanent:false},Operation::NewFolder]{
        let flags=operation_flags(action,false);
        assert_eq!((flags&(FOF_SILENT|FOF_NOCONFIRMATION|FOF_NOERRORUI|FOFX_KEEPNEWERFILE|FOF_RENAMEONCOLLISION)).0,0);
    }
    assert!(!Completion{operation:Operation::Copy,requested:4,completed:2,aborted:true,cancelled:true,skipped:false,outputs:Vec::new(),outputs_truncated:false,origin:None,error:None,error_detail:None}.message().contains("失败"));
    assert!(Completion{operation:Operation::Copy,requested:4,completed:4,aborted:false,cancelled:false,skipped:false,outputs:Vec::new(),outputs_truncated:false,origin:None,error:None,error_detail:None}.message().contains("完成"));
}
#[test]fn completion_matches_shell_extended_paths_without_folding_unicode(){
    assert_eq!(completion_key(Path::new(r"\\?\C:\资料\甲")),completion_key(Path::new("c:/资料/甲/")));
    assert_eq!(completion_key(Path::new(r"\\?\UNC\server\share\目录")),completion_key(Path::new(r"\\server\share\目录")));
    assert_ne!(completion_key(Path::new(r"C:\Ä")),completion_key(Path::new(r"C:\ä")));
}
#[test]fn transfer_dependencies_serialize_only_overlapping_files(){
    let a=Access::new(&[r"E:\source\a.txt".into()],Some(Path::new(r"E:\destination")),Operation::Copy);
    let b=Access::new(&[r"D:\other\b.txt".into()],Some(Path::new(r"E:\destination")),Operation::Copy);
    let dependent=Access::new(&[r"E:\destination\a.txt".into()],Some(Path::new(r"E:\final")),Operation::Move);
    assert!(!a.conflicts(&b));assert!(a.conflicts(&dependent));
    let deletion=Access::new(&[r"E:\destination".into()],None,Operation::Delete{permanent:false});assert!(a.conflicts(&deletion));
    let sibling=Access::new(&[r"E:\destination-other".into()],None,Operation::Delete{permanent:false});assert!(!a.conflicts(&sibling));
    let root=Access::new(&[r"E:\".into()],None,Operation::Delete{permanent:false});assert!(a.conflicts(&root));
}
#[test]fn delayed_failure_cannot_consume_another_operations_completion(){
    let owner=window().unwrap();let thread=unsafe{GetCurrentThreadId()};
    let failed=NEXT_OPERATION.fetch_add(1,Ordering::Relaxed);let succeeded=NEXT_OPERATION.fetch_add(1,Ordering::Relaxed);
    COMPLETIONS.lock().unwrap().push_back((succeeded,owner.0.0 as isize,thread,Completion{operation:Operation::Copy,requested:2,completed:2,aborted:false,cancelled:false,skipped:false,outputs:Vec::new(),outputs_truncated:false,origin:None,error:None,error_detail:None}));
    assert!(take_completion(owner.0,failed).is_none());
    assert_eq!(take_completion(owner.0,succeeded).unwrap().completed,2);
    assert!(take_completion(owner.0,succeeded).is_none());
}
#[test]fn pending_callback_followed_by_success_is_not_a_skipped_operation(){
    let state=Arc::new(Mutex::new(ProgressState::default()));let progress=Progress{state:state.clone(),cancel_before:false};
    for code in [COPYENGINE_S_PENDING,COPYENGINE_S_PENDING_DELETE,HRESULT(0)]{
        progress.after(windows::core::Ref::default(),code,windows::core::Ref::default(),false).unwrap();
    }
    let state=state.lock().unwrap();assert!(!state.skipped);assert!(state.failure.is_none());
}
#[test]fn outputs_are_top_level_deduplicated_and_bound_to_the_original_view(){
    let owner=window().unwrap();
    let view=Window(unsafe{CreateWindowExW(WINDOW_EX_STYLE::default(),w!("STATIC"),None,WS_CHILD,0,0,1,1,Some(owner.0),None,None,None)}.unwrap());
    bind_origin(view.0,17,u64::MAX-1,u64::MAX-2).unwrap();
    let origin=capture_origin(view.0).unwrap();assert_eq!(origin.tab,u64::MAX-1);assert_eq!(origin.epoch,u64::MAX-2);
    let root=PathBuf::from(r"E:\source\folder");let another=PathBuf::from(r"E:\source\other.txt");
    let request=request(view.0,vec![root.clone(),another.clone()],Some(r"E:\destination".into()),Operation::Copy,None);
    assert_eq!(request.owner,owner.0.0 as isize);assert_eq!(request.origin,Some(origin));
    bind_origin(view.0,18,2,3).unwrap();assert_ne!(capture_origin(view.0),request.origin);
    let mut state=ProgressState::for_request(&request);
    state.success(Some(&root.join("child.txt")),Some(Path::new(r"E:\destination\folder\child.txt")),false);
    assert!(state.outputs.is_empty()&&state.completed.is_empty());
    for _ in 0..3{state.success(Some(&root),Some(Path::new(r"E:\destination\folder")),false);}
    state.success(Some(&another),Some(Path::new(r"E:\destination\other (2).txt")),false);
    state.finished=Some(HRESULT(0));let outcome=finish_outcome(&mut state,Ok(()),Ok(false));
    let completion=completion_for(&request,Ok(outcome));
    assert_eq!(completion.origin,Some(origin));assert_eq!(completion.completed,2);assert!(!completion.aborted);
    assert_eq!(completion.outputs,vec![PathBuf::from(r"E:\destination\folder"),PathBuf::from(r"E:\destination\other (2).txt")]);
    let handle=view.0.0 as isize;
    assert!(thread::spawn(move||capture_origin(HWND(handle as*mut _))).join().unwrap().is_none());
    clear_origin(view.0);assert!(capture_origin(view.0).is_none());
}
#[test]fn output_limits_preserve_completion_and_case_sensitive_names(){
    let owner=window().unwrap();
    let request=request(owner.0,vec![r"E:\source\A.txt".into(),r"E:\source\a.txt".into()],Some(r"E:\destination".into()),Operation::Copy,None);
    let mut state=ProgressState::for_request(&request);
    state.success(Some(Path::new(r"E:\source\A.txt")),Some(Path::new(r"E:\destination\A.txt")),false);
    state.success(Some(Path::new(r"E:\source\a.txt")),Some(Path::new(r"E:\destination\a.txt")),false);
    assert_eq!(state.completed.len(),2);assert_eq!(state.outputs.len(),2);
    state.output_bytes=MAX_PATH_BYTES;state.output(Path::new(r"E:\destination\another.txt"));
    assert!(state.outputs_truncated);assert_eq!(state.outputs.len(),2);assert_eq!(state.completed.len(),2);
    let mut count=ProgressState{track_outputs:true,..Default::default()};
    for index in 0..=MAX_ITEMS{count.output(Path::new(&format!(r"E:\destination\item-{index}")));}
    assert_eq!(count.outputs.len(),MAX_ITEMS);assert!(count.outputs_truncated);
}
#[test]fn partial_failure_keeps_successful_outputs_and_original_hresult(){
    let owner=window().unwrap();let first=PathBuf::from(r"E:\source\first.txt");let second=PathBuf::from(r"E:\source\second.txt");
    let request=request(owner.0,vec![first.clone(),second],Some(r"E:\destination".into()),Operation::Copy,None);
    let mut state=ProgressState::for_request(&request);
    state.success(Some(&first),Some(Path::new(r"E:\destination\first.txt")),false);
    state.fail(COPYENGINE_E_USER_CANCELLED);let failure=HRESULT(0x80070005u32 as i32);state.fail(failure);state.finished=Some(COPYENGINE_E_USER_CANCELLED);
    let completion=completion_for(&request,Ok(finish_outcome(&mut state,Err(Error::from_hresult(COPYENGINE_E_USER_CANCELLED)),Ok(true))));
    assert!(completion.aborted&&completion.cancelled);assert_eq!(completion.error,Some(failure));assert_eq!(completion.result_code(),failure.0 as u32);
    assert_eq!(completion.completed,1);assert_eq!(completion.outputs,[PathBuf::from(r"E:\destination\first.txt")]);
    assert!(completion.message().contains("未完成")&&completion.message().contains("1 / 2"));
}
#[test]fn user_input_changes_only_the_bound_views_completion_activity(){
    let owner=window().unwrap();
    let child=|parent|Window(unsafe{CreateWindowExW(WINDOW_EX_STYLE::default(),w!("STATIC"),None,WS_CHILD,0,0,1,1,Some(parent),None,None,None)}.unwrap());
    let view=child(owner.0);let editor=child(view.0);let other=child(owner.0);
    bind_origin(view.0,17,33,5).unwrap();bind_origin(other.0,18,34,5).unwrap();
    let submitted=request(view.0,vec![r"E:\source\file.txt".into()],Some(r"E:\destination".into()),Operation::Copy,None);
    let original=submitted.origin.unwrap();assert!(origin_current(&original));
    bind_origin(view.0,17,33,5).unwrap();assert_eq!(capture_origin(view.0),Some(original));
    note_user_input(other.0);assert!(origin_current(&original),"another pane cancelled this pane's automatic selection");
    note_user_input(editor.0);let changed=capture_origin(view.0).unwrap();
    assert_eq!((changed.view,changed.pane,changed.tab,changed.epoch),(original.view,original.pane,original.tab,original.epoch));
    assert!(changed.activity>original.activity);assert!(!origin_current(&original));assert!(origin_current(&changed));
    bind_origin(view.0,17,33,5).unwrap();assert_eq!(capture_origin(view.0),Some(changed));
    assert_eq!(submitted.origin,Some(original),"authorized operation snapshot was mutated by later input");
    clear_origin(view.0);assert!(!origin_current(&changed));
    bind_origin(view.0,17,33,5).unwrap();assert_ne!(capture_origin(view.0),Some(changed));
}
use anyhow::{Context,ensure};
use sha2::{Digest,Sha256};
use std::{fs,io::{Read,Write},time::{Duration,Instant}};
use windows::Win32::{Foundation::{HINSTANCE,LRESULT},System::LibraryLoader::GetModuleHandleW,UI::WindowsAndMessaging::*};

#[test]fn clipboard_paths_and_text_are_bounded_before_replacement(){
    assert!(hdrop_bytes(&[]).is_err());
    assert!(hdrop_bytes(&[PathBuf::from("relative.txt")]).is_err());
    let paths=vec![PathBuf::from(r"E:\中文 路径\甲.txt"),PathBuf::from(r"E:\中文 路径\乙.txt")];
    let bytes=hdrop_bytes(&paths).unwrap();assert_eq!(&bytes[..4],&20u32.to_le_bytes());
    assert_eq!(&bytes[16..20],&1u32.to_le_bytes());assert!(bytes.ends_with(&[0,0,0,0]));
    assert_eq!(bytes.len(),20+paths.iter().map(|path|wide(path).len()*2).sum::<usize>()+2);
}
#[test]fn hdrop_parser_accepts_unicode_and_ansi_and_rejects_truncated_payloads(){
    let paths=vec![PathBuf::from(r"E:\中文 空格\甲.txt"),PathBuf::from(r"\\server\share\乙.txt")];
    let bytes=hdrop_bytes(&paths).unwrap();assert_eq!(parse_hdrop(&bytes).unwrap(),paths);
    for end in 0..bytes.len(){assert!(parse_hdrop(&bytes[..end]).is_err(),"accepted truncated path list at {end}");}
    let mut ansi=vec![0u8;20];ansi[..4].copy_from_slice(&20u32.to_le_bytes());
    ansi.extend(b"E:\\alpha.txt\0E:\\beta.txt\0\0");
    assert_eq!(parse_hdrop(&ansi).unwrap(),[PathBuf::from(r"E:\alpha.txt"),PathBuf::from(r"E:\beta.txt")]);
    assert!(parse_hdrop(&ansi[..ansi.len()-1]).is_err());
    let mut bad=bytes.clone();bad[..4].copy_from_slice(&21u32.to_le_bytes());assert!(parse_hdrop(&bad).is_err());
    let mut bad=bytes;bad[..4].copy_from_slice(&u32::MAX.to_le_bytes());assert!(parse_hdrop(&bad).is_err());
    let mut relative=vec![0u8;20];relative[..4].copy_from_slice(&20u32.to_le_bytes());relative.extend(b"relative.txt\0\0");assert!(parse_hdrop(&relative).is_err());
}
#[test]fn zip_contents_leave_the_direct_path_to_the_shell_verb(){
    // Items inside a zip have no filesystem path, and the zip itself is a file:
    // both must fail the direct path so the caller uses the folder's Shell verb.
    let _=unsafe{windows::Win32::System::Com::CoInitializeEx(None,windows::Win32::System::Com::COINIT_APARTMENTTHREADED)};
    let root=std::env::temp_dir().join(format!("zsdesk-zip-direct-{}",std::process::id()));fs::create_dir_all(&root).unwrap();
    fs::write(root.join("inner.txt"),b"zip").unwrap();let zip=root.join("archive.zip");
    use std::os::windows::process::CommandExt;
    let made=std::process::Command::new("powershell").args(["-NoProfile","-NonInteractive","-Command","Compress-Archive -LiteralPath $env:ZSDESK_TEST_ZIP_SOURCE -DestinationPath $env:ZSDESK_TEST_ZIP_DESTINATION"])
        .env("ZSDESK_TEST_ZIP_SOURCE",root.join("inner.txt")).env("ZSDESK_TEST_ZIP_DESTINATION",&zip)
        .creation_flags(0x08000000).status().is_ok_and(|status|status.success());
    assert!(made,"ZIP fixture creation failed");
    if made{
        let inner:Result<IShellItem>=unsafe{SHCreateItemFromParsingName(PCWSTR(wide(&zip.join("inner.txt")).as_ptr()),None)};
        if let Ok(inner)=inner{assert!(item_path(&inner).is_err(),"a zip entry reported a filesystem path");}
        let hwnd=HWND::default();
        assert!(paste(hwnd,&zip).is_err(),"the direct paste accepted a zip file as its folder");
    }
    let _=fs::remove_dir_all(&root);
}
#[test]fn hdrop_parser_rejects_excess_items_in_a_single_pass(){
    let mut bytes=vec![0u8;20];bytes[..4].copy_from_slice(&20u32.to_le_bytes());
    for _ in 0..=MAX_ITEMS{bytes.extend(b"E:\\a\0");}bytes.push(0);
    assert!(parse_hdrop(&bytes).is_err());
}
#[test]fn recursive_progress_retains_only_the_bounded_top_level_set(){
    let expected=(0..MAX_ITEMS).map(|index|completion_key(Path::new(&format!(r"E:\批量\顶层{index}")))).collect();
    let mut state=ProgressState{expected,..Default::default()};
    for index in 0..MAX_ITEMS*10{state.record_completed(Path::new(&format!(r"E:\批量\顶层0\child-{index}")));}
    assert!(state.completed.is_empty());assert_eq!(state.expected.len(),MAX_ITEMS);
    for index in 0..MAX_ITEMS{state.record_completed(Path::new(&format!(r"E:\批量\顶层{index}")));}
    for _ in 0..100{state.record_completed(Path::new(r"e:\批量\顶层0"));}
    assert_eq!(state.completed.len(),MAX_ITEMS);
    assert_ne!(completion_key(Path::new(r"E:\Ä")),completion_key(Path::new(r"E:\ä")));
}
#[test]fn child_failures_and_skips_survive_top_level_progress_filtering(){
    let state=Arc::new(Mutex::new(ProgressState{expected:[completion_key(Path::new(r"E:\folder"))].into(),..Default::default()}));
    let sink=Progress{state:state.clone(),cancel_before:false};
    sink.after(windows::core::Ref::default(),COPYENGINE_E_ACCESS_DENIED_SRC,windows::core::Ref::default(),false).unwrap();
    sink.after(windows::core::Ref::default(),COPYENGINE_S_USER_IGNORED,windows::core::Ref::default(),false).unwrap();
    let state=state.lock().unwrap();assert_eq!(state.failure,Some(COPYENGINE_E_ACCESS_DENIED_SRC));
    assert!(state.skipped&&state.completed.is_empty());assert_eq!(state.expected.len(),1);
}

#[link(name="user32")]
unsafe extern "system"{
    fn CreateWindowStationW(name:*const u16,flags:u32,access:u32,security:*const core::ffi::c_void)->*mut core::ffi::c_void;
    fn GetProcessWindowStation()->*mut core::ffi::c_void;
    fn SetProcessWindowStation(station:*mut core::ffi::c_void)->i32;
    fn CloseWindowStation(station:*mut core::ffi::c_void)->i32;
    fn CreateDesktopW(name:*const u16,device:*const u16,mode:*const core::ffi::c_void,flags:u32,access:u32,security:*const core::ffi::c_void)->*mut core::ffi::c_void;
    fn SetThreadDesktop(desktop:*mut core::ffi::c_void)->i32;
    fn CloseDesktop(desktop:*mut core::ffi::c_void)->i32;
}
struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}
struct Window(HWND);impl Drop for Window{fn drop(&mut self){let _=unsafe{DestroyWindow(self.0)};}}
const PUMP_DROP:u32=WM_APP+319;
thread_local!{
    static RESULTS:std::cell::RefCell<Vec<u32>>=const{std::cell::RefCell::new(Vec::new())};
    static PUMP_SIGNAL:std::cell::RefCell<Option<Arc<ReplySignal>>>=const{std::cell::RefCell::new(None)};
    static PUMP_COUNT:std::cell::Cell<usize>=const{std::cell::Cell::new(0)};
}
unsafe extern "system" fn proc(hwnd:HWND,msg:u32,wp:WPARAM,lp:LPARAM)->LRESULT{
    if msg==PUMP_DROP{PUMP_COUNT.with(|count|count.set(count.get()+1));note_user_input(hwnd);
        PUMP_SIGNAL.with(|signal|{if let Some(signal)=signal.borrow().as_ref(){let _=unsafe{SetEvent(HANDLE(signal.0 as*mut _))};}});return LRESULT(0);}
    if msg==result_message(){RESULTS.with(|result|result.borrow_mut().push(wp.0 as u32));return LRESULT(0);}
    unsafe{DefWindowProcW(hwnd,msg,wp,lp)}
}
fn pump(){
    for _ in 0..128{let mut message=MSG::default();if !unsafe{PeekMessageW(&mut message,None,0,0,PM_REMOVE)}.as_bool(){break;}
        if message.message!=WM_QUIT{unsafe{let _=TranslateMessage(&message);DispatchMessageW(&message);}}}
}
fn window()->anyhow::Result<Window>{
    let instance=HINSTANCE(unsafe{GetModuleHandleW(None)}?.0);let class=w!("ZSDesk.FileTransfer.NativeRegression");
    unsafe{RegisterClassW(&WNDCLASSW{hInstance:instance,lpszClassName:class,lpfnWndProc:Some(proc),..Default::default()});}
    Ok(Window(unsafe{CreateWindowExW(WS_EX_NOACTIVATE,class,w!("File transfer regression"),WS_POPUP,0,0,1,1,None,None,Some(instance),None)}?))
}
fn request(owner:HWND,paths:Vec<PathBuf>,destination:Option<PathBuf>,operation:Operation,clipboard:Option<u32>)->Request{
    let origin=capture_origin(owner);let root=unsafe{GetAncestor(owner,GA_ROOT)};let owner=if root.0.is_null(){owner}else{root};
    let access=Access::new(&paths,destination.as_deref(),operation);
    Request{token:NEXT_OPERATION.fetch_add(1,Ordering::Relaxed),owner:owner.0 as isize,thread:unsafe{GetCurrentThreadId()},origin,paths,destination,operation,clipboard,reply:None,test_gate:None,access}
}
fn digest(path:&Path)->anyhow::Result<String>{
    let mut hash=Sha256::new();let mut file=fs::File::open(path)?;let mut buffer=[0u8;65536];
    loop{let count=file.read(&mut buffer)?;if count==0{break;}hash.update(&buffer[..count]);}Ok(format!("{:x}",hash.finalize()))
}
fn fixture(path:&Path,seed:u8,bytes:usize)->anyhow::Result<String>{
    let block=(0..65536usize).map(|index|seed.wrapping_add((index%251)as u8)).collect::<Vec<_>>();
    let mut hash=Sha256::new();for _ in 0..bytes/block.len(){hash.update(&block);}let expected=format!("{:x}",hash.finalize());
    if path.exists(){ensure!(fs::metadata(path)?.len()==bytes as u64&&digest(path)?==expected,"existing fixture content differs: {}",path.display());}
    else{let mut file=fs::OpenOptions::new().write(true).create_new(true).open(path)?;for _ in 0..bytes/block.len(){file.write_all(&block)?;}}
    Ok(expected)
}
fn text_clipboard()->anyhow::Result<String>{
    let _clipboard=Clipboard::open(None)?;let memory=HGLOBAL(unsafe{GetClipboardData(13)}?.0);let bytes=unsafe{GlobalSize(memory)};
    ensure!(bytes>=2&&bytes<=4096,"bounded text clipboard required");let pointer=unsafe{GlobalLock(memory)};ensure!(!pointer.is_null(),"text GlobalLock failed");
    let units=unsafe{std::slice::from_raw_parts(pointer.cast::<u16>(),bytes/2)};let result=String::from_utf16_lossy(&units[..units.iter().position(|unit|*unit==0).unwrap_or(units.len())]);
    let _=unsafe{GlobalUnlock(memory)};Ok(result)
}

fn exercise(root:&Path)->anyhow::Result<serde_json::Value>{
    unsafe{OleInitialize(None)}?;let _apartment=Apartment;let owner=window()?;
    // This runs in a separate window station, whose clipboard is distinct from
    // WinSta0. No saved user IDataObject or delayed stream is ever requested.
    set_text(owner.0,"隔离剪贴板")?;
    ensure!(!persist_owned_file_clipboard(owner.0)?&&text_clipboard()?=="隔离剪贴板","non-file clipboard was changed");
    ensure!(!has_paste_content(),"plain text was treated as a file paste");
    {
        let _clipboard=Clipboard::open(Some(owner.0))?;unsafe{EmptyClipboard()?;}
        let format=unsafe{RegisterClipboardFormatW(CFSTR_FILEDESCRIPTORW)};ensure!(format!=0,"virtual descriptor format");
        let mut descriptor=FILEGROUPDESCRIPTORW::default();descriptor.cItems=1;
        descriptor.fgd[0].dwFlags=FD_FILESIZE.0 as u32;
        let name:Vec<u16>="virtual-fixture.txt".encode_utf16().collect();let mut filename=[0u16;260];filename[..name.len()].copy_from_slice(&name);descriptor.fgd[0].cFileName=filename;
        let bytes=unsafe{std::slice::from_raw_parts((&descriptor as*const FILEGROUPDESCRIPTORW).cast::<u8>(),std::mem::size_of::<FILEGROUPDESCRIPTORW>())};
        Memory::new(bytes)?.publish(format)?;
    }
    ensure!(has_paste_content()&&!has_filesystem_clipboard(),"virtual-file paste did not retain its native route");
    ensure!(!persist_owned_file_clipboard(owner.0)?,"virtual provider was converted to a path snapshot");
    set_text(owner.0,"隔离剪贴板")?;
    let source=root.join("source");let destination=root.join("copied");let moved=root.join("moved");let cancelled=root.join("cancelled");
    for folder in [&source,&destination,&moved,&cancelled]{fs::create_dir_all(folder)?;}
    let a=source.join("中文 文件甲.bin");let b=source.join("多文件乙.bin");
    let expected=[fixture(&a,17,16*1024*1024)?,fixture(&b,93,16*1024*1024)?];let paths=vec![a.clone(),b.clone()];
    use std::os::windows::fs::OpenOptionsExt;
    let locks=paths.iter().map(|path|fs::OpenOptions::new().read(true).share_mode(0).open(path)).collect::<std::io::Result<Vec<_>>>()?;
    let started=Instant::now();copy_paths(owner.0,&paths,true)?;let clipboard_micros=started.elapsed().as_micros();
    ensure!(persist_owned_file_clipboard(owner.0)?,"file clipboard did not become static");
    let(snapshot,effect,sequence)=clipboard_files()?;ensure!(snapshot==paths&&effect&2!=0,"Unicode multi-file cut intent changed");
    ensure!(clipboard_micros<500_000,"clipboard snapshot blocked");
    drop(locks);
    let copied=request(owner.0,paths.clone(),Some(destination.clone()),Operation::Copy,None);
    let started=Instant::now();let outcome=perform(&copied,true)?;ensure!(!outcome.aborted,"copy was cancelled");let elapsed=started.elapsed();
    let expected_outputs=paths.iter().map(|path|exact_completion_key(&destination.join(path.file_name().unwrap()))).collect::<HashSet<_>>();
    ensure!(outcome.outputs.iter().map(|path|exact_completion_key(path)).collect::<HashSet<_>>()==expected_outputs&&!outcome.outputs_truncated,"copy did not report exact successful targets");
    for(index,path)in paths.iter().enumerate(){ensure!(digest(&destination.join(path.file_name().unwrap()))?==expected[index],"copy SHA256 mismatch");}
    // A user's newer text must survive completion of an earlier cut request.
    set_text(owner.0,"later clipboard survives move")?;
    let outcome=perform(&request(owner.0,paths.clone(),Some(moved.clone()),Operation::Move,Some(sequence)),true)?;
    ensure!(!outcome.aborted&&text_clipboard()?=="later clipboard survives move","move cleared a newer clipboard");
    ensure!(outcome.outputs.len()==2&&outcome.outputs.iter().all(|path|path.parent()==Some(moved.as_path())),"move targets were lost after source deletion");
    for(index,path)in paths.iter().enumerate(){ensure!(!path.exists()&&digest(&moved.join(path.file_name().unwrap()))?==expected[index],"move result/source mismatch");}
    let moved_paths=paths.iter().map(|path|moved.join(path.file_name().unwrap())).collect::<Vec<_>>();
    let aborted=perform_inner(&request(owner.0,moved_paths.clone(),Some(cancelled.clone()),Operation::Copy,None),true,true);
    ensure!(matches!(&aborted,Err(_)|Ok(Outcome{aborted:true,..})),"cancel was reported as successful");
    ensure!(moved_paths.iter().all(|path|!cancelled.join(path.file_name().unwrap()).exists()),"cancelled transfer wrote destination files");
    let missing=request(owner.0,vec![source.join("never-exists.bin")],Some(destination.clone()),Operation::Copy,None);
    ensure!(perform(&missing,true).is_err(),"missing source reported successful");
    let skipped=Arc::new(Mutex::new(ProgressState::default()));let progress=Progress{state:skipped.clone(),cancel_before:false};
    progress.after(windows::core::Ref::default(),COPYENGINE_S_USER_IGNORED,windows::core::Ref::default(),false)?;
    ensure!(skipped.lock().unwrap().skipped,"per-item skip was not recorded");

    let folders=root.join("new-folders");fs::create_dir_all(&folders)?;
    let created1=perform(&request(owner.0,Vec::new(),Some(folders.clone()),Operation::NewFolder,None),true)?;
    let created2=perform(&request(owner.0,Vec::new(),Some(folders.clone()),Operation::NewFolder,None),true)?;
    ensure!(!created1.aborted&&!created2.aborted&&created1.created!=created2.created,"new folder did not get a unique name");
    ensure!(created1.created.as_ref().is_some_and(|path|path.is_dir())&&created2.created.as_ref().is_some_and(|path|path.is_dir()),"new folder callback returned no directory");
    ensure!(created1.outputs==created1.created.clone().into_iter().collect::<Vec<_>>()&&created2.outputs==created2.created.clone().into_iter().collect::<Vec<_>>(),"new-folder outputs do not match actual unique names");

    // Quiet mode is confined to isolated fixtures. A same-name replacement
    // must report the actual destination and retain its new content hash.
    let collision_source=root.join("collision-source");let collision_target=root.join("collision-target");
    for folder in [&collision_source,&collision_target]{fs::create_dir_all(folder)?;}
    let collision=collision_source.join("replace.txt");let replacement=collision_target.join("replace.txt");
    fs::write(&collision,b"replacement fixture")?;fs::write(&replacement,b"previous fixture")?;
    let collision_result=perform(&request(owner.0,vec![collision],Some(collision_target.clone()),Operation::Copy,None),true)?;
    ensure!(!collision_result.aborted&&collision_result.outputs.len()==1&&exact_completion_key(&collision_result.outputs[0])==exact_completion_key(&replacement)
        &&fs::read(&replacement)?==b"replacement fixture","replacement result and output path differ");

    let recursive_source=root.join("recursive-source").join("package");let recursive_target=root.join("recursive-target");
    fs::create_dir_all(recursive_source.join("nested"))?;fs::create_dir_all(&recursive_target)?;
    fs::write(recursive_source.join("top.txt"),b"top")?;fs::write(recursive_source.join("nested/child.txt"),b"child")?;
    let recursive=perform(&request(owner.0,vec![recursive_source],Some(recursive_target.clone()),Operation::Copy,None),true)?;
    ensure!(!recursive.aborted&&recursive.completed==1&&recursive.outputs.len()==1&&exact_completion_key(&recursive.outputs[0])==exact_completion_key(&recursive_target.join("package")),"recursive children escaped the top-level output contract: {recursive:?}");
    ensure!(fs::read(recursive_target.join("package/nested/child.txt"))?==b"child","recursive directory content mismatch");

    let partial_source=root.join("partial-source");let partial_target=root.join("partial-target");
    for folder in [&partial_source,&partial_target]{fs::create_dir_all(folder)?;}
    let partial_paths=[partial_source.join(format!("first-{}.txt",std::process::id())),partial_source.join(format!("second-{}.txt",std::process::id()))];
    for path in &partial_paths{fs::write(path,b"partial transfer fixture")?;}
    struct Injection;impl Drop for Injection{fn drop(&mut self){FAIL_AFTER_SUCCESS.with(|flag|flag.set(false));}}
    let partial={FAIL_AFTER_SUCCESS.with(|flag|flag.set(true));let _injection=Injection;
        perform(&request(owner.0,partial_paths.to_vec(),Some(partial_target.clone()),Operation::Copy,None),true)?};
    ensure!(partial.aborted&&partial.error==Some(HRESULT(0x80070005u32 as i32))&&partial.completed==1,"injected partial failure lost its actual result: {partial:?}");
    ensure!(partial.outputs.len()==1&&partial.outputs[0].is_file()&&partial.outputs[0].parent()==Some(partial_target.as_path()),"partial success output was lost");
    ensure!(fs::read(&partial.outputs[0])?==b"partial transfer fixture","partial output content changed");
    let actual=partial_paths.iter().filter(|path|partial_target.join(path.file_name().unwrap()).exists()).count();
    ensure!(actual==1,"partial failure was reported before/after the wrong number of copies");
    let remove=root.join("delete-fixture.bin");fixture(&remove,38,65536)?;
    ensure!(!perform(&request(owner.0,vec![remove.clone()],None,Operation::Delete{permanent:true},None),true)?.aborted&&!remove.exists(),"permanent fixture deletion failed");

    let fifo_source=root.join("fifo-source");let fifo_mid=root.join("fifo-mid");let fifo_final=root.join("fifo-final");
    for folder in [&fifo_source,&fifo_mid,&fifo_final]{fs::create_dir_all(folder)?;}
    let fifo=fifo_source.join(format!("队列顺序-{}.bin",std::process::id()));let fifo_hash=fixture(&fifo,25,65536)?;
    RESULTS.with(|results|results.borrow_mut().clear());
    let submitted=Instant::now();transfer_paths(owner.0,vec![fifo.clone()],fifo_mid.clone(),false)?;
    transfer_paths(owner.0,vec![fifo_mid.join(fifo.file_name().unwrap())],fifo_final.clone(),true)?;
    let queue_micros=submitted.elapsed().as_micros();let deadline=Instant::now()+Duration::from_secs(20);let mut heartbeats=0;
    loop{pump();heartbeats+=1;if pending_operations()==0{break;}ensure!(Instant::now()<deadline,"FIFO operation queue did not retire");thread::sleep(Duration::from_millis(5));}
    pump();let results=RESULTS.with(|results|results.borrow().clone());ensure!(results==[0,0],"FIFO operation errors: {results:?}");
    ensure!(!fifo_mid.join(fifo.file_name().unwrap()).exists()&&digest(&fifo_final.join(fifo.file_name().unwrap()))?==fifo_hash,"FIFO copy-then-move ordering failed");
    let parallel_source=root.join("parallel-source");let parallel_target=root.join("parallel-target");
    fs::create_dir_all(&parallel_source)?;fs::create_dir_all(&parallel_target)?;
    let first_path=parallel_source.join(format!("blocked-{}.bin",std::process::id()));let second_path=parallel_source.join(format!("independent-{}.bin",std::process::id()));
    let first_hash=fixture(&first_path,83,65536)?;let second_hash=fixture(&second_path,91,65536)?;
    let gate=Arc::new(ReplySignal(unsafe{CreateEventW(None,true,false,PCWSTR::null())}?.0 as usize));
    struct OpenGate(Arc<ReplySignal>);impl Drop for OpenGate{fn drop(&mut self){let _=unsafe{SetEvent(HANDLE(self.0.0 as*mut _))};}}let release=OpenGate(gate.clone());
    let mut first=request(owner.0,vec![first_path.clone()],Some(parallel_target.clone()),Operation::Copy,Some(777));first.test_gate=Some(gate);
    let mut duplicate=first.clone();duplicate.token=NEXT_OPERATION.fetch_add(1,Ordering::Relaxed);
    RESULTS.with(|values|values.borrow_mut().clear());enqueue(first)?;enqueue(duplicate)?;
    enqueue(request(owner.0,vec![second_path.clone()],Some(parallel_target.clone()),Operation::Copy,Some(778)))?;
    let until=Instant::now()+Duration::from_secs(3);
    while !parallel_target.join(second_path.file_name().unwrap()).exists(){pump();ensure!(Instant::now()<until,"independent paste waited behind an unrelated operation");thread::sleep(Duration::from_millis(5));}
    ensure!(!parallel_target.join(first_path.file_name().unwrap()).exists(),"first operation was not blocked by the test gate");
    {let queue=QUEUE.lock().unwrap();ensure!(queue.active.iter().chain(queue.waiting.iter()).filter(|request|request.clipboard==Some(777)).count()==1,"repeated paste queued a second identical operation");ensure!(queue.workers<=MAX_WORKERS,"worker bound exceeded");}
    drop(release);let until=Instant::now()+Duration::from_secs(8);
    while pending_operations()!=0{pump();ensure!(Instant::now()<until,"parallel workers did not retire");thread::sleep(Duration::from_millis(5));}
    pump();ensure!(RESULTS.with(|values|values.borrow().clone())==[0,0],"parallel or duplicate paste completion mismatch");
    ensure!(digest(&parallel_target.join(first_path.file_name().unwrap()))?==first_hash&&digest(&parallel_target.join(second_path.file_name().unwrap()))?==second_hash,"parallel copy contents differ");
    // A synchronous drop queued behind an asynchronous copy must pump UI,
    // wait for physical completion and emit no duplicate global completion.
    let drop_source=root.join("drop-source");let drop_mid=root.join("drop-mid");let drop_final=root.join("drop-final");
    for folder in [&drop_source,&drop_mid,&drop_final]{fs::create_dir_all(folder)?;}
    let name=format!("drop-fifo-{}.bin",std::process::id());let source_path=drop_source.join(&name);let drop_hash=fixture(&source_path,71,65536)?;
    let gate=Arc::new(ReplySignal(unsafe{CreateEventW(None,true,false,PCWSTR::null())}?.0 as usize));
    bind_origin(owner.0,51,77,9)?;let origin=capture_origin(owner.0).context("drop origin")?;
    let mut first=request(owner.0,vec![source_path.clone()],Some(drop_mid.clone()),Operation::Copy,None);first.test_gate=Some(gate.clone());
    let earlier=COMPLETIONS.lock().unwrap().iter().filter(|(_,window,_,_)|*window==owner.0.0 as isize).count();
    RESULTS.with(|values|values.borrow_mut().clear());PUMP_COUNT.with(|value|value.set(0));PUMP_SIGNAL.with(|value|*value.borrow_mut()=Some(gate));
    enqueue(first)?;unsafe{PostMessageW(Some(owner.0),PUMP_DROP,WPARAM(0),LPARAM(0))?;}
    let dropped=transfer_drop(owner.0,vec![drop_mid.join(&name)],drop_final.clone(),true)?;
    PUMP_SIGNAL.with(|value|{value.borrow_mut().take();});pump();
    ensure!(PUMP_COUNT.with(std::cell::Cell::get)==1,"synchronous drop did not pump owner messages");
    ensure!(!dropped.aborted&&dropped.error.is_none()&&dropped.completed==1&&matches!(dropped.operation,Operation::Move),"synchronous drop reported no real completion: {dropped:?}");
    ensure!(dropped.origin==Some(origin)&&!origin_current(&origin),"user input during drop changed the operation snapshot instead of only automatic selection eligibility");
    ensure!(dropped.outputs.len()==1&&exact_completion_key(&dropped.outputs[0])==exact_completion_key(&drop_final.join(&name))
        &&digest(&drop_final.join(&name))?==drop_hash&&!drop_mid.join(&name).exists(),"synchronous FIFO move target/source mismatch");
    ensure!(RESULTS.with(|values|values.borrow().clone())==[0],"synchronous drop also posted a global result");
    let after=COMPLETIONS.lock().unwrap().iter().filter(|(_,window,_,_)|*window==owner.0.0 as isize).count();
    ensure!(after==earlier+1,"synchronous drop stored a duplicate global completion");
    let setup_failure=transfer_drop(owner.0,vec![drop_source.join("missing-source")],drop_final.clone(),false);
    ensure!(setup_failure.is_err(),"drop setup failure was mistaken for a completed move");
    pump();ensure!(RESULTS.with(|values|values.borrow().clone())==[0],"synchronous setup failure posted a global result");
    let deadline=Instant::now()+Duration::from_secs(5);while pending_operations()!=0{pump();ensure!(Instant::now()<deadline,"drop worker did not retire");thread::sleep(Duration::from_millis(5));}
    {
        let _clipboard=Clipboard::open(Some(owner.0))?;unsafe{EmptyClipboard()?;}
    }
    Ok(serde_json::json!({"unicode_multiple_files":true,"copy_sha256":expected,"copy_bytes":32*1024*1024u64,
        "copy_elapsed_ms":elapsed.as_millis(),"copy_mib_per_second":32.0/elapsed.as_secs_f64().max(0.000001),
        "clipboard_with_files_locked_microseconds":clipboard_micros,"copy_did_not_open_file_contents":true,"static_cut_snapshot":true,
        "move_verified":true,"newer_clipboard_preserved":true,"non_file_clipboard_preserved":true,"cancel_not_success":true,
        "errors_not_success":true,"per_item_skip_recorded":true,"new_folder_unique":true,"permanent_fixture_delete":true,
        "exact_copy_move_new_outputs":true,"recursive_outputs_are_top_level_only":true,"collision_overwrite_fixture":true,"partial_error_keeps_successful_outputs":true,"partial_error_injected_by_progress_sink":true,
        "synchronous_drop_uses_same_fifo":true,"synchronous_drop_pumps_ui":true,"synchronous_drop_has_no_duplicate_notification":true,"drop_wait_does_not_override_new_selection":true,"drop_setup_error_is_not_move":true,
        "independent_pastes_start_without_waiting":true,"repeated_inflight_paste_deduplicated":true,"parallel_worker_limit":MAX_WORKERS,
        "fifo_submit_microseconds":queue_micros,"fifo_pump_iterations":heartbeats,"fifo_order":true,"worker_retired":true,"user_clipboard_accessed":false}))
}

#[test]
#[ignore="Native 32 MiB file-operation/clipboard regression in a separate window station; requires ZSDESK_NATIVE_TEST_OUTPUT"]
fn native_file_transfers_are_batched_bounded_and_clipboard_safe()->anyhow::Result<()>{
    let root=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("isolated output required")?).join("file-transfer");
    ensure!(root.is_absolute(),"absolute output required");fs::create_dir_all(&root)?;
    let original=unsafe{GetProcessWindowStation()};ensure!(!original.is_null(),"current window station missing");
    let station_name=wide(Path::new(&format!("ZSDeskTransfer{}",std::process::id())));
    let station=unsafe{CreateWindowStationW(station_name.as_ptr(),0,0x000f037f,std::ptr::null())};ensure!(!station.is_null(),"isolated window station creation failed");
    if unsafe{SetProcessWindowStation(station)}==0{let _=unsafe{CloseWindowStation(station)};anyhow::bail!("SetProcessWindowStation failed");}
    let name=wide(Path::new("Default"));let desktop=unsafe{CreateDesktopW(name.as_ptr(),std::ptr::null(),std::ptr::null(),0,0x000f00ff,std::ptr::null())};
    if desktop.is_null(){let _=unsafe{SetProcessWindowStation(original)};let _=unsafe{CloseWindowStation(station)};anyhow::bail!("private desktop creation failed");}
    let handle=desktop as usize;let destination=root.clone();
    let result=thread::spawn(move||->anyhow::Result<serde_json::Value>{ensure!(unsafe{SetThreadDesktop(handle as*mut _)}!=0,"SetThreadDesktop failed");exercise(&destination)})
        .join().map_err(|_|anyhow::anyhow!("file transfer native test panicked")).and_then(|value|value);
    let restored=unsafe{SetProcessWindowStation(original)}!=0;let desktop_closed=unsafe{CloseDesktop(desktop)}!=0;let station_closed=unsafe{CloseWindowStation(station)}!=0;
    let report=serde_json::json!({"state":if result.is_ok()&&restored&&desktop_closed&&station_closed{"passed"}else{"failed"},
        "checks":result.as_ref().map_err(|error|format!("{error:#}")),"station_restored":restored,"desktop_closed":desktop_closed,"station_closed":station_closed});
    fs::write(root.join("verification.current.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");
    result?;ensure!(restored&&desktop_closed&&station_closed,"window station cleanup failed");Ok(())
}

#[test]
fn ordinary_fence_copy_and_move_preserve_the_expected_source() -> anyhow::Result<()> {
    let root=std::env::temp_dir().join(format!("zsdesk-fence-move-{}",std::process::id()));
    std::fs::create_dir(&root)?;
    struct Fixture(PathBuf);impl Drop for Fixture{fn drop(&mut self){let _=std::fs::remove_dir_all(&self.0);}}
    let _fixture=Fixture(root.clone());
    let dest=root.join("目标格子");std::fs::create_dir(&dest)?;
    for (operation,name) in [(Operation::Copy,"copy.bin"),(Operation::Move,"move.bin")]{
        let source=root.join(name);std::fs::write(&source,b"fixture data")?;
        let job=request(HWND::default(),vec![source.clone()],Some(dest.clone()),operation,None);
        let result=perform(&job,true)?;
        assert!(!result.aborted && result.error.is_none());
        assert_eq!(std::fs::read(dest.join(name))?,b"fixture data");
        assert_eq!(source.exists(),operation==Operation::Copy);
    }
    Ok(())
}

#[test]fn explicit_move_and_reordered_cut_selection_are_distinct_from_copy_permissions(){
    assert!(move_intent(2));for mask in [0,1,3,4,5,7]{assert!(!move_intent(mask));}
    let a=PathBuf::from(r"C:\first");let b=PathBuf::from(r"C:\second");
    assert!(same_cut_selection(&[a.clone(),b.clone()],&[b.clone(),a.clone()]));
    assert!(!same_cut_selection(&[a.clone(),b.clone()],&[a.clone(),a]));
}
