use super::*;
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
thread_local!{static RESULTS:std::cell::RefCell<Vec<u32>>=const{std::cell::RefCell::new(Vec::new())};}
unsafe extern "system" fn proc(hwnd:HWND,msg:u32,wp:WPARAM,lp:LPARAM)->LRESULT{
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
    Request{owner:owner.0 as isize,thread:unsafe{GetCurrentThreadId()},paths,destination,operation,clipboard}
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
    for(index,path)in paths.iter().enumerate(){ensure!(digest(&destination.join(path.file_name().unwrap()))?==expected[index],"copy SHA256 mismatch");}
    // A user's newer text must survive completion of an earlier cut request.
    set_text(owner.0,"later clipboard survives move")?;
    let outcome=perform(&request(owner.0,paths.clone(),Some(moved.clone()),Operation::Move,Some(sequence)),true)?;
    ensure!(!outcome.aborted&&text_clipboard()?=="later clipboard survives move","move cleared a newer clipboard");
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
    {
        let _clipboard=Clipboard::open(Some(owner.0))?;unsafe{EmptyClipboard()?;}
    }
    Ok(serde_json::json!({"unicode_multiple_files":true,"copy_sha256":expected,"copy_bytes":32*1024*1024u64,
        "copy_elapsed_ms":elapsed.as_millis(),"copy_mib_per_second":32.0/elapsed.as_secs_f64().max(0.000001),
        "clipboard_with_files_locked_microseconds":clipboard_micros,"copy_did_not_open_file_contents":true,"static_cut_snapshot":true,
        "move_verified":true,"newer_clipboard_preserved":true,"non_file_clipboard_preserved":true,"cancel_not_success":true,
        "errors_not_success":true,"per_item_skip_recorded":true,"new_folder_unique":true,"permanent_fixture_delete":true,
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
