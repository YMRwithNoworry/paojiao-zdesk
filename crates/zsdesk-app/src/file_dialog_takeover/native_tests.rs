use super::*;
use std::{cell::Cell,fs};
use windows::{core::{implement,Ref},Win32::{System::{Com::CoTaskMemFree,Ole::{OleInitialize,OleUninitialize},Threading::GetCurrentThreadId},UI::Shell::*}};
pub(super) static FIRST_ACTIVATION_ALPHA:std::sync::atomic::AtomicU64=std::sync::atomic::AtomicU64::new(999);
unsafe extern "system" fn offscreen(code:i32,wp:WPARAM,lp:LPARAM)->windows::Win32::Foundation::LRESULT{
    let window=HWND(wp.0 as*mut _);
    if code==HCBT_ACTIVATE as i32{
        // Let the product's global hook run before blocking fixture activation.
        let _=unsafe{CallNextHookEx(None,code,wp,lp)};
        if class(window)=="#32770"{let mut alpha=255;let _=unsafe{GetLayeredWindowAttributes(window,None,Some(&mut alpha),None)};
            let _=FIRST_ACTIVATION_ALPHA.compare_exchange(999,u64::from(alpha),Ordering::Relaxed,Ordering::Relaxed);}
        return windows::Win32::Foundation::LRESULT(1);
    }
    if lp.0!=0&&class(window)=="#32770"{
        if code==HCBT_CREATEWND as i32{let create=unsafe{&mut*(lp.0 as*mut CBT_CREATEWNDW)};if !create.lpcs.is_null(){unsafe{(*create.lpcs).x=-30000;(*create.lpcs).y=-30000;(*create.lpcs).dwExStyle|=WS_EX_NOACTIVATE|WS_EX_TOOLWINDOW;}}}
        if code==HCBT_MOVESIZE as i32{let rect=unsafe{&mut*(lp.0 as*mut windows::Win32::Foundation::RECT)};let width=rect.right-rect.left;let height=rect.bottom-rect.top;rect.left=-30000;rect.top=-30000;rect.right=rect.left+width;rect.bottom=rect.top+height;}
    }unsafe{CallNextHookEx(None,code,wp,lp)}
}
#[implement(IFileDialogEvents)]struct Events{ready:mpsc::Sender<isize>,sent:Cell<bool>}
#[allow(non_snake_case)]impl IFileDialogEvents_Impl for Events_Impl{
    fn OnFileOk(&self,_:Ref<IFileDialog>)->windows::core::Result<()>{Ok(())}
    fn OnFolderChanging(&self,_:Ref<IFileDialog>,_:Ref<IShellItem>)->windows::core::Result<()>{Ok(())}
    fn OnFolderChange(&self,_:Ref<IFileDialog>)->windows::core::Result<()>{
        if !self.sent.get(){
            unsafe extern "system" fn visit(window:HWND,slot:LPARAM)->BOOL{if class(window)=="#32770"{unsafe{*(slot.0 as*mut HWND)=window};return false.into();}true.into()}
            let mut window=HWND::default();let _=unsafe{EnumThreadWindows(GetCurrentThreadId(),Some(visit),LPARAM(&mut window as*mut _ as isize))};
            if !window.0.is_null(){self.sent.set(true);let _=self.ready.send(window.0 as isize);}
        }Ok(())
    }
    fn OnSelectionChange(&self,_:Ref<IFileDialog>)->windows::core::Result<()>{Ok(())}
    fn OnShareViolation(&self,_:Ref<IFileDialog>,_:Ref<IShellItem>)->windows::core::Result<FDE_SHAREVIOLATION_RESPONSE>{Ok(FDESVR_DEFAULT)}
    fn OnTypeChange(&self,dialog:Ref<IFileDialog>)->windows::core::Result<()>{if let Some(dialog)=dialog.as_ref(){eprintln!("native type changed: {:?}",unsafe{dialog.GetFileTypeIndex()});}Ok(())}
    fn OnOverwrite(&self,_:Ref<IFileDialog>,_:Ref<IShellItem>)->windows::core::Result<FDE_OVERWRITE_RESPONSE>{Ok(FDEOR_DEFAULT)}
}
pub(super) fn original(directory:PathBuf,ready:mpsc::Sender<isize>,kind:SelectionKind,accept_label:Option<&str>)->Result<(Vec<PathBuf>,u32,bool)>{
    let hook=unsafe{SetWindowsHookExW(WH_CBT,Some(offscreen),None,GetCurrentThreadId())}?;struct Hook(HHOOK);impl Drop for Hook{fn drop(&mut self){let _=unsafe{UnhookWindowsHookEx(self.0)};}}let _hook=Hook(hook);
    unsafe{OleInitialize(None)}?;struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}let _apartment=Apartment;
    let dialog:IFileOpenDialog=unsafe{CoCreateInstance(&FileOpenDialog,None,CLSCTX_INPROC_SERVER)}?;
    let folder:IShellItem=unsafe{SHCreateItemFromParsingName(PCWSTR(wide(&directory.to_string_lossy()).as_ptr()),None)}?;
    let sink:IFileDialogEvents=Events{ready,sent:Cell::new(false)}.into();
    let cookie=unsafe{dialog.Advise(&sink)}?;
    unsafe{dialog.SetOptions(if kind.folders(){FOS_PICKFOLDERS|FOS_PATHMUSTEXIST|FOS_FORCEFILESYSTEM}else{FOS_ALLOWMULTISELECT|FOS_FILEMUSTEXIST|FOS_FORCEFILESYSTEM})?;dialog.SetFolder(&folder)?;
        dialog.SetTitle(w!("ZSDesk Original Picker Test"))?;
        if let Some(label)=accept_label{dialog.SetOkButtonLabel(PCWSTR(wide(label).as_ptr()))?;}
        if !kind.folders(){dialog.SetFileTypes(&[Common::COMDLG_FILTERSPEC{pszName:w!("文本文件"),pszSpec:w!("*.txt")},Common::COMDLG_FILTERSPEC{pszName:w!("所有文件"),pszSpec:w!("*.*")}])?;}}
    let shown=unsafe{dialog.Show(None)};unsafe{dialog.Unadvise(cookie)}?;
    if let Err(error)=shown{ensure!(error.code().0 as u32==0x800704c7,"native dialog failed: {error}");return Ok((Vec::new(),0,true));}
    let items=unsafe{dialog.GetResults()}?;let mut paths=Vec::new();for index in 0..unsafe{items.GetCount()}?{
        let item=unsafe{items.GetItemAt(index)}?;let raw=unsafe{item.GetDisplayName(SIGDN_FILESYSPATH)}?;let path=unsafe{raw.to_string()};unsafe{CoTaskMemFree(Some(raw.0.cast()))};paths.push(PathBuf::from(path?));
    }Ok((paths,if kind.folders(){0}else{unsafe{dialog.GetFileTypeIndex()}?},false))
}
fn exercise(root:&Path)->Result<serde_json::Value>{
    unsafe{CoInitializeEx(None,COINIT_MULTITHREADED)}.ok()?;struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{CoUninitialize();}}}let _apartment=Apartment;
    let automation:IUIAutomation=unsafe{CoCreateInstance(&CUIAutomation,None,CLSCTX_INPROC_SERVER)}?;
    let first=root.join("native source");let second=root.join("selected 中文");fs::create_dir_all(&first)?;fs::create_dir_all(&second)?;
    let expected=vec![second.join("first file.txt"),second.join("第二个.txt")];for path in &expected{fs::write(path,b"native return fixture")?;}
    for (cancelled,label) in [(false,None),(true,None),(false,Some("上传(&U)")),(true,Some("Upload (&U)"))]{
        let(tx,rx)=mpsc::channel();let folder=first.clone();let(done_tx,done_rx)=mpsc::channel();
        let worker=thread::spawn(move||{let _=done_tx.send(original(folder,tx,SelectionKind::Files,label));});
        let window=HWND(rx.recv_timeout(Duration::from_secs(10)).context("native dialog ready")?as*mut _);
        struct Cancel(HWND);impl Drop for Cancel{fn drop(&mut self){if unsafe{IsWindow(Some(self.0))}.as_bool(){let _=unsafe{PostMessageW(Some(self.0),WM_COMMAND,WPARAM(2),LPARAM(0))};}}}let _cancel=Cancel(window);
        let deadline=Instant::now()+Duration::from_secs(8);let part=loop{let part=parts(window);if !part.shell.0.is_null()&&!part.name.0.is_null()&&!part.address.0.is_null(){break part;}ensure!(Instant::now()<deadline,"native controls unavailable");thread::sleep(Duration::from_millis(20));};
        let until=Instant::now()+Duration::from_secs(3);let mode=loop{let mode=multiple(&automation,parts(window).shell);if mode==Some(true)||Instant::now()>=until{break mode;}thread::sleep(Duration::from_millis(30));};
        if mode!=Some(true){
            if let Ok(root)=unsafe{automation.ElementFromHandle(window)}{if let Ok(condition)=unsafe{automation.CreateTrueCondition()}{if let Ok(elements)=unsafe{root.FindAll(TreeScope_Descendants,&condition)}{
                for index in 0..unsafe{elements.Length()}.unwrap_or(0).min(64){if let Ok(item)=unsafe{elements.GetElement(index)}{eprintln!("uia kind={:?} class={:?} id={:?}",unsafe{item.CurrentControlType()},unsafe{item.CurrentClassName()},unsafe{item.CurrentAutomationId()});}}
            }}}
        }
        ensure!(mode==Some(true),"native multi-selection mode was not recognized: {mode:?}");
        let location=text(part.address).and_then(|value|initial_location(&value));ensure!(location.as_ref().and_then(|location|location.as_path()).is_some_and(|path|crate::explorer_forwarder::same_path(path,&first)),"native initial folder mismatch: {location:?}");
        let (filters,index)=types(part.types);ensure!(filters.len()==2&&index==0&&filters[0].pattern=="*.txt","native file filters mismatch");
        let accept=unsafe{GetDlgItem(Some(window),1)}.ok().and_then(text).context("original accept label")?;
        ensure!(dialog_kind(window,&part,Some(&automation))==Some(SelectionKind::Files),"file upload was not recognized as files: {accept}");
        let mut session=Session{source:guard::Identity::capture(window).context("source identity")?,name:part.name,types:part.types,kind:SelectionKind::Files,multiple:true,accept,token:1,picker:None,guard:None,early:None,worker:None,fallback:false};
        if !cancelled{if let Some(executable)=std::env::var_os("ZSDESK_GUARD_TEST_EXE"){
            let picker=unsafe{CreateWindowExW(WS_EX_TOOLWINDOW|WS_EX_NOACTIVATE,w!("STATIC"),None,WS_POPUP,-30000,-30000,50,50,None,None,None,None)}?;
            struct Picker(HWND);impl Drop for Picker{fn drop(&mut self){let _=unsafe{DestroyWindow(self.0)};}}let _picker=Picker(picker);
            unsafe{SetPropW(picker,crate::window::picker::MARKER,Some(windows::Win32::Foundation::HANDLE(session.token as usize as*mut _)))}?;
            session.picker=Some(picker.0 as isize);session.guard=Some(guard::Guard::hide_using(session.source.clone(),picker.0 as isize,session.token,Path::new(&executable))?);
            let _=unsafe{RemovePropW(picker,crate::window::picker::MARKER)};
            let until=Instant::now()+Duration::from_secs(3);while !session.guard.as_mut().unwrap().exited(){ensure!(Instant::now()<until,"picker guard did not finish");thread::sleep(Duration::from_millis(10));}
            session.poll(true);ensure!(!session.fallback,"normal picker close discarded the pending selection result");
        }}
        if cancelled{
            session.close_picker();session.finish(Outcome::Cancelled)?;
            ensure!(unsafe{IsWindow(Some(window))}.as_bool(),"switching takeover off cancelled the caller's dialog");session.fallback=false;session.finish(Outcome::Cancelled)?;
        }else{session.finish(Outcome::Selected{paths:expected.clone(),filter:1})?;}
        let result=done_rx.recv_timeout(Duration::from_secs(10)).context("native dialog result timeout")??;
        worker.join().map_err(|_|anyhow::anyhow!("native source thread panicked"))?;
        ensure!(!unsafe{IsWindow(Some(window))}.as_bool(),"native caller window remained alive");
        if cancelled{ensure!(result.2&&result.0.is_empty(),"Cancel did not propagate");}else{ensure!(!result.2&&result.0==expected&&result.1==2,"native caller received wrong files or filter: {result:?}");}
    }
    for (cancelled,label) in [
        (false,None),(true,None),(false,Some("上传")),(true,Some("上传")),
        (false,Some("上传(&U)")),(true,Some("Upload (&U)")),
        (false,Some("Open")),(true,Some("OK")),(false,Some("添加到项目"))]{
        let(tx,rx)=mpsc::channel();let folder=first.clone();let(done_tx,done_rx)=mpsc::channel();
        let worker=thread::spawn(move||{let _=done_tx.send(original(folder,tx,SelectionKind::Folders,label));});
        let window=HWND(rx.recv_timeout(Duration::from_secs(10)).context("native folder dialog ready")?as*mut _);
        struct Cancel(HWND);impl Drop for Cancel{fn drop(&mut self){if unsafe{IsWindow(Some(self.0))}.as_bool(){let _=unsafe{PostMessageW(Some(self.0),WM_COMMAND,WPARAM(2),LPARAM(0))};}}}let _cancel=Cancel(window);
        let deadline=Instant::now()+Duration::from_secs(8);let part=loop{
            let part=parts(window);if !part.shell.0.is_null()&&!part.name.0.is_null()&&!part.address.0.is_null()&&dialog_kind(window,&part,Some(&automation))==Some(SelectionKind::Folders){break part;}
            if Instant::now()>=deadline{
                unsafe extern "system" fn dump(window:HWND,_:LPARAM)->BOOL{
                    eprintln!("folder control hwnd={:?} class={} id={} visible={} text={:?}",window,class(window),unsafe{GetDlgCtrlID(window)},unsafe{IsWindowVisible(window)}.as_bool(),text(window));true.into()
                }
                let _=unsafe{EnumChildWindows(Some(window),Some(dump),LPARAM(0))};
                anyhow::bail!("native folder contract was not recognized: shell={:?}, name={:?}, address={:?}, kind={:?}, accept={:?}, types-visible={}",part.shell,part.name,part.address,dialog_kind(window,&part,Some(&automation)),unsafe{GetDlgItem(Some(window),1)}.ok().and_then(text),unsafe{IsWindowVisible(part.types)}.as_bool());
            }thread::sleep(Duration::from_millis(30));
        };
        let accept=unsafe{GetDlgItem(Some(window),1)}.ok().and_then(text).context("folder accept label")?;
        let mut session=Session{source:guard::Identity::capture(window).context("folder source identity")?,name:part.name,types:HWND::default(),kind:SelectionKind::Folders,multiple:false,accept,token:2,picker:None,guard:None,early:None,worker:None,fallback:false};
        if cancelled{session.finish(Outcome::Cancelled)?;}else{
            ensure!(session.finish(Outcome::Selected{paths:vec![expected[0].clone()],filter:0}).is_err(),"folder caller accepted a file");
            ensure!(unsafe{IsWindow(Some(window))}.as_bool(),"rejected result closed native folder dialog");
            session.finish(Outcome::Selected{paths:vec![second.clone()],filter:0})?;
        }
        let result=done_rx.recv_timeout(Duration::from_secs(10)).context("native folder dialog result timeout")??;
        worker.join().map_err(|_|anyhow::anyhow!("native folder source thread panicked"))?;
        ensure!(!unsafe{IsWindow(Some(window))}.as_bool(),"native folder caller stayed open");
        if cancelled{ensure!(result.2&&result.0.is_empty(),"folder Cancel did not propagate");}else{ensure!(!result.2&&result.0==vec![second.clone()],"native caller received wrong folder: {result:?}");}
    }
    let mut foreground=0;unsafe{GetWindowThreadProcessId(GetForegroundWindow(),Some(&mut foreground));}ensure!(foreground!=std::process::id(),"test became the foreground application");
    Ok(serde_json::json!({"original_dialog_received_files":true,"original_folder_dialog_received_directory":true,"browser_style_upload_folder_contract":true,"accelerator_and_custom_accept_labels":true,"file_and_folder_modes_distinct":true,"wrong_kind_rejected":true,"unicode_multiple_paths":true,"type_filter_preserved":true,"cancel_propagated":true,"off_retains_native_dialog":true,"offscreen_no_input":true,"owned_windows_destroyed":true}))
}
#[test]
#[ignore="Owned offscreen IFileOpenDialog result contract without input injection"]
fn native_file_dialog_return_and_cancel_preserve_caller_contract()->Result<()>{
    use std::os::windows::fs::MetadataExt;
    let output=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("output")?);
    ensure!(output.is_absolute()&&output.parent().is_some_and(|parent|parent.parent().is_some()),"dedicated absolute output directory required");
    let root=output.join("native-picker-return");
    for path in root.ancestors(){if path.exists(){ensure!(fs::symlink_metadata(path)?.file_attributes()&0x400==0,"fixture output must use real directories");}}
    fs::create_dir_all(&root)?;
    let output=root.clone();let result=thread::spawn(move||exercise(&output)).join().map_err(|_|anyhow::anyhow!("caller contract test panicked")).and_then(|result|result);
    let report=serde_json::json!({"checks":result.as_ref().map_err(|error|format!("{error:#}")),"created_desktop":false});fs::write(root.join("verification.current.json"),serde_json::to_vec_pretty(&report)?)?;result?;Ok(())
}
