//! Routes compatible Open dialogs into the existing ZSDesk explorer window.
//! The original caller still performs its own validation and receives results.
use std::{collections::{HashSet,HashMap},path::{Path,PathBuf},sync::{Arc,atomic::{AtomicBool,AtomicU64,Ordering},mpsc},thread::{self,JoinHandle},time::{Duration,Instant}};
use anyhow::{Context,Result,ensure};
use windows::{core::{w,BOOL,Interface,PCWSTR},Win32::{Foundation::{HWND,LPARAM,WPARAM},System::{Com::{CoCreateInstance,CoInitializeEx,CoUninitialize,CLSCTX_INPROC_SERVER,COINIT_MULTITHREADED},Variant::VARIANT},UI::{Accessibility::*,Controls::*,Input::KeyboardAndMouse::IsWindowEnabled,WindowsAndMessaging::*}}};
use crate::window::picker::{Event,FileType,Options,Outcome,SelectionKind};
pub(crate) mod guard;
#[cfg(test)]#[path="file_dialog_takeover/native_tests.rs"]mod native_tests;
static SERIAL:AtomicU64=AtomicU64::new(1);
fn wide(value:&str)->Vec<u16>{value.encode_utf16().chain(Some(0)).collect()}
fn class(window:HWND)->String{let mut value=[0u16;128];let n=unsafe{GetClassNameW(window,&mut value)}.max(0)as usize;String::from_utf16_lossy(&value[..n])}
fn send(window:HWND,message:u32,wp:usize,lp:isize)->Option<usize>{let mut result=0;(unsafe{SendMessageTimeoutW(window,message,WPARAM(wp),LPARAM(lp),SMTO_ABORTIFHUNG|SMTO_BLOCK,250,Some(&mut result))}.0!=0).then_some(result)}
fn text(window:HWND)->Option<String>{let mut value=vec![0u16;32768];let n=send(window,WM_GETTEXT,value.len(),value.as_mut_ptr()as isize)?;Some(String::from_utf16_lossy(&value[..n.min(value.len()-1)]))}
#[derive(Default)]struct Parts{name:HWND,types:HWND,address:HWND,shell:bool,folder_label:bool}
fn directory_label(value:&str)->bool{matches!(value.replace('&',"").trim().trim_end_matches([':', '：']).trim().to_lowercase().as_str(),"文件夹"|"目录"|"folder"|"directory")}
fn parts(dialog:HWND)->Parts{
    unsafe extern "system" fn visit(window:HWND,p:LPARAM)->BOOL{let value=unsafe{&mut*(p.0 as*mut Parts)};let kind=class(window);let id=unsafe{GetDlgCtrlID(window)};
        // Common dialogs use either cmb13 (1148) or edt1 (1152). Folder
        // pickers expose the standalone edt1 instead of the file-name combo.
        if [1148,1152].contains(&id)&&kind=="Edit"{
            if value.name.0.is_null()||class(value.name)!="Edit"||unsafe{IsWindowVisible(window)}.as_bool(){value.name=window;}
        }else if id==1148&&value.name.0.is_null()&&kind=="ComboBox"{value.name=window;}
        if id==1136&&kind=="ComboBox"{value.types=window;}if id==1001&&kind=="ToolbarWindow32"{value.address=window;}
        if kind=="Static"&&text(window).is_some_and(|value|directory_label(&value)){value.folder_label=true;}
        if kind=="SHELLDLL_DefView"{value.shell=true;}true.into()}
    let mut value=Parts::default();let _=unsafe{EnumChildWindows(Some(dialog),Some(visit),LPARAM(&mut value as*mut _ as isize))};value
}
fn open_label(value:&str)->bool{let value=value.replace('&',"").to_lowercase();
    if value.contains("文件夹")||value.contains("folder"){return false;}
    value.starts_with("打开")||value.starts_with("open")||value.starts_with("选择文件")||value.starts_with("select file")}
fn selection_kind(accept:&str,visible_types:bool,folder_field:bool)->Option<SelectionKind>{
    let accept=accept.replace('&',"").to_lowercase();
    let folder=accept.starts_with("选择文件夹")||accept.starts_with("选择此文件夹")||accept.starts_with("选择目录")
        ||accept.starts_with("select folder")||accept.starts_with("choose folder")||accept.starts_with("select directory")||accept.starts_with("choose directory");
    let upload=accept=="上传"||accept=="upload";
    if !visible_types&&(folder||(upload&&folder_field)){return Some(SelectionKind::Folders);}
    if visible_types&&(open_label(&accept)||upload){return Some(SelectionKind::Files);}
    None
}
fn folder_field_label(automation:&IUIAutomation,name:HWND)->bool{
    let Ok(element)=(unsafe{automation.ElementFromHandle(name)})else{return false;};
    if unsafe{element.CurrentName()}.is_ok_and(|name|directory_label(&name.to_string())){return true;}
    unsafe{element.CurrentLabeledBy()}.and_then(|label|unsafe{label.CurrentName()}).is_ok_and(|name|directory_label(&name.to_string()))
}
fn dialog_kind(dialog:HWND,parts:&Parts,automation:Option<&IUIAutomation>)->Option<SelectionKind>{
    let accept=unsafe{GetDlgItem(Some(dialog),1)}.ok().and_then(text)?;
    let visible_types=!parts.types.0.is_null()&&unsafe{IsWindowVisible(parts.types)}.as_bool();
    let folder_field=parts.folder_label||(!visible_types&&automation.is_some_and(|automation|folder_field_label(automation,parts.name)));
    selection_kind(&accept,visible_types,folder_field)
}
fn initial_location(value:&str)->Option<zsdesk_core::model::explorer::ShellLocation>{
    use zsdesk_core::model::explorer::ShellLocation;
    let value=value.split_once(": ").or_else(||value.split_once('：')).map_or(value,|(_,value)|value).trim();
    if Path::new(value).is_absolute(){return Some(ShellLocation::Path(value.into()));}
    match value.to_lowercase().as_str(){"桌面"|"desktop"=>zsdesk_windows::desktop::user_desktop().map(ShellLocation::Path),"此电脑"|"this pc"=>Some(ShellLocation::ThisPc),
        "主文件夹"|"home"=>Some(ShellLocation::Shell{pidl:None,parsing_name:"shell:HomeFolder".into(),title:"主文件夹".into()}),_=>None}
}
fn pattern(label:&str)->String{
    let value=label.replace('（',"(").replace('）',")");
    value.split('(').skip(1).filter_map(|part|part.split(')').next()).filter(|part|part.contains('*')||part.contains('?'))
        .last().filter(|part|part.len()<=512).map(str::trim).unwrap_or("*").to_string()
}
fn types(window:HWND)->(Vec<FileType>,usize){
    let count=send(window,CB_GETCOUNT,0,0).filter(|count|*count<=64).unwrap_or(0);let mut result=Vec::new();
    for index in 0..count{let Some(len)=send(window,CB_GETLBTEXTLEN,index,0).filter(|len|*len<4096)else{continue;};let mut value=vec![0u16;len+1];
        if send(window,CB_GETLBTEXT,index,value.as_mut_ptr()as isize).is_none(){continue;}let label=String::from_utf16_lossy(&value[..len]);result.push(FileType{pattern:pattern(&label),label});}
    if result.is_empty(){result.push(FileType{label:"所有文件 (*.*)".into(),pattern:"*".into()});}
    let index=send(window,CB_GETCURSEL,0,0).unwrap_or(0).min(result.len()-1);(result,index)
}
fn multiple(automation:&IUIAutomation,dialog:HWND)->Option<bool>{
    let root=unsafe{automation.ElementFromHandle(dialog)}.ok()?;
    let list=unsafe{automation.CreatePropertyCondition(UIA_ControlTypePropertyId,&VARIANT::from(UIA_ListControlTypeId.0))}.ok()?;
    let grid=unsafe{automation.CreatePropertyCondition(UIA_ControlTypePropertyId,&VARIANT::from(UIA_DataGridControlTypeId.0))}.ok()?;
    let condition=unsafe{automation.CreateOrCondition(&list,&grid)}.ok()?;
    let lists=unsafe{root.FindAll(TreeScope_Descendants,&condition)}.ok()?;
    for index in 0..unsafe{lists.Length()}.ok()?.min(32){let item=unsafe{lists.GetElement(index)}.ok()?;
        if let Ok(selection)=unsafe{item.GetCurrentPatternAs::<IUIAutomationSelectionPattern>(UIA_SelectionPatternId)}{return unsafe{selection.CurrentCanSelectMultiple()}.ok().map(|value|value.as_bool());}}
    None
}
struct Session{source:guard::Identity,name:HWND,types:HWND,kind:SelectionKind,multiple:bool,accept:String,token:u64,picker:Option<isize>,guard:Option<guard::Guard>,worker:Option<JoinHandle<()>>,fallback:bool}
impl Session{
    fn restore(&mut self){self.guard.take();}
    fn picker_alive(&self)->bool{self.picker.is_some_and(|window|unsafe{GetPropW(HWND(window as*mut _),crate::window::picker::MARKER)}.0 as usize==self.token as usize)}
    fn close_picker(&mut self){self.restore();self.fallback=true;if let Some(window)=self.picker{let _=unsafe{PostMessageW(Some(HWND(window as*mut _)),WM_CLOSE,WPARAM(0),LPARAM(0))};}}
    fn poll(&mut self,enabled:bool){
        if !enabled||!self.source.matches(){self.close_picker();return;}
        if self.guard.as_mut().is_some_and(guard::Guard::exited){
            // The picker can close before its STA has finished COM teardown.
            // Its eventual Selected result must still reach the caller.
            if self.picker_alive(){self.close_picker();}else{self.restore();}
        }
    }
    fn finish(&mut self,outcome:Outcome)->Result<()>{
        self.restore();if self.fallback||!self.source.matches(){return Ok(());}
        match outcome{
            Outcome::Cancelled=>{let _=unsafe{PostMessageW(Some(self.source.hwnd()),WM_COMMAND,WPARAM(2),LPARAM(0))};},
            Outcome::Selected{paths,filter}=>{
                crate::window::picker::validate_paths(&paths,self.multiple,self.kind).map_err(anyhow::Error::msg)?;
                let actual=parts(self.source.hwnd());let accept=unsafe{GetDlgItem(Some(self.source.hwnd()),1)}.ok().and_then(text);
                ensure!(actual.name==self.name&&accept.as_deref()==Some(self.accept.as_str()),"文件选择窗口已改变");
                if !self.types.0.is_null()&&self.types==actual.types{
                    send(self.types,CB_SETCURSEL,filter,0).context("文件类型切换失败")?;
                    let owner=unsafe{GetParent(self.types)}.context("文件类型控件父窗口不可用")?;
                    send(owner,WM_COMMAND,1136|((CBN_SELCHANGE as usize)<<16),self.types.0 as isize).context("文件类型通知失败")?;
                    send(owner,WM_COMMAND,1136|((CBN_SELENDOK as usize)<<16),self.types.0 as isize).context("文件类型确认失败")?;
                    send(owner,WM_COMMAND,1136|((CBN_CLOSEUP as usize)<<16),self.types.0 as isize).context("文件类型收尾失败")?;
                    #[cfg(test)]eprintln!("filter owner={} control_id={} selected={:?}",class(owner),unsafe{GetDlgCtrlID(self.types)},send(self.types,CB_GETCURSEL,0,0));
                }
                let value=paths.iter().map(|path|format!("\"{}\"",path.to_string_lossy())).collect::<Vec<_>>().join(" ");let value=wide(&value);
                ensure!(send(self.name,WM_SETTEXT,0,value.as_ptr()as isize).is_some_and(|result|result!=0),"文件选择结果未能返回");
                unsafe{let _=SetForegroundWindow(self.source.hwnd());}
                // Only the caller's Open command can validate and accept paths.
                let _=unsafe{PostMessageW(Some(self.source.hwnd()),WM_COMMAND,WPARAM(1),LPARAM(0))};
            },
        }Ok(())
    }
}
impl Drop for Session{fn drop(&mut self){self.restore();if self.worker.as_ref().is_some_and(JoinHandle::is_finished){if let Some(worker)=self.worker.take(){let _=worker.join();}}}}
pub struct Bridge{stop:Arc<AtomicBool>,worker:Option<JoinHandle<()>>}
impl Bridge{
    pub fn start(profile:&Path)->Result<Option<Self>>{
        if crate::edition::DESKTOP_ONLY{return Ok(None);}
        let profile=profile.to_path_buf();let stop=Arc::new(AtomicBool::new(false));let signal=stop.clone();
        let worker=thread::Builder::new().name("zsdesk-file-dialogs".into()).spawn(move||{let _=run(&profile,&signal);})?;
        Ok(Some(Self{stop,worker:Some(worker)}))
    }
}
impl Drop for Bridge{fn drop(&mut self){self.stop.store(true,Ordering::Release);if self.worker.as_ref().is_some_and(JoinHandle::is_finished){if let Some(worker)=self.worker.take(){let _=worker.join();}}}}
fn run(profile:&Path,stop:&AtomicBool)->Result<()>{
    use windows::Win32::{Foundation::{CloseHandle,GetLastError,ERROR_ALREADY_EXISTS},System::Threading::CreateMutexW};
    let mutex=unsafe{CreateMutexW(None,false,w!("Local\\ZSDesk.CommonFilePicker"))}?;let existing=unsafe{GetLastError()}==ERROR_ALREADY_EXISTS;
    struct Owned(windows::Win32::Foundation::HANDLE);impl Drop for Owned{fn drop(&mut self){let _=unsafe{CloseHandle(self.0)};}}let _owned=Owned(mutex);if existing{return Ok(());}
    unsafe{CoInitializeEx(None,COINIT_MULTITHREADED)}.ok()?;struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{CoUninitialize();}}}let _apartment=Apartment;
    let mut automation:Option<IUIAutomation>=None;
    let(tx,rx)=mpsc::channel();let mut sessions=Vec::<Session>::new();let mut seen=HashSet::new();let mut preparing=HashMap::new();let mut enabled=false;
    let mut refresh=Instant::now()-Duration::from_secs(3);let mut last_error=None::<String>;let mut previous=String::new();let mut completed=0u64;
    while !stop.load(Ordering::Acquire){
        if refresh.elapsed()>=Duration::from_secs(1){
            enabled=crate::storage::load(profile).is_ok_and(|session|session.ui.file_dialog_takeover);refresh=Instant::now();
            if enabled&&automation.is_none(){match unsafe{CoCreateInstance(&CUIAutomation,None,CLSCTX_INPROC_SERVER)}{Ok(value)=>automation=Some(value),Err(error)=>last_error=Some(error.to_string())}}
            if !enabled{automation=None;seen.clear();preparing.clear();}
        }
        for event in rx.try_iter(){match event{
            Event::Ready{token,window}=>if let Some(session)=sessions.iter_mut().find(|session|session.token==token){
                session.picker=Some(window);
                if enabled&&session.source.matches(){match guard::Guard::hide(session.source.clone(),window,token){Ok(guard)=>session.guard=Some(guard),Err(error)=>{last_error=Some(error.to_string());if session.picker_alive(){session.close_picker();}}}}
                else{session.close_picker();}
            },
            Event::Finished{token,result}=>if let Some(index)=sessions.iter().position(|session|session.token==token){let mut session=sessions.remove(index);
                match result{Ok(outcome)=>match session.finish(outcome){Ok(())=>completed+=1,Err(error)=>last_error=Some(error.to_string())},Err(error)=>{session.restore();last_error=Some(error);}}
            },
        }}
        for session in &mut sessions{session.poll(enabled);}
        seen.retain(|(window,pid)|{let mut current=0;unsafe{GetWindowThreadProcessId(HWND(*window as*mut _),Some(&mut current));}current==*pid&&current!=0});
        preparing.retain(|(window,pid),_|{let mut current=0;unsafe{GetWindowThreadProcessId(HWND(*window as*mut _),Some(&mut current));}current==*pid&&current!=0});
        if enabled&&sessions.len()<4{
            let dialog=unsafe{GetForegroundWindow()};let mut pid=0;unsafe{GetWindowThreadProcessId(dialog,Some(&mut pid));}
            if pid!=0&&pid!=std::process::id()&&class(dialog)=="#32770"&&!seen.contains(&(dialog.0 as isize,pid))&&!sessions.iter().any(|session|session.source.window==dialog.0 as isize)&&unsafe{IsWindowEnabled(dialog)}.as_bool(){
                let part=parts(dialog);let kind=dialog_kind(dialog,&part,automation.as_ref());
                if let Some(kind)=kind.filter(|_|part.shell&&!part.name.0.is_null()){
                    let key=(dialog.0 as isize,pid);let since=*preparing.entry(key).or_insert_with(Instant::now);
                    if since.elapsed()<Duration::from_millis(150){thread::sleep(Duration::from_millis(30));continue;}
                    if let (Some(source),Some(initial),Some(multiple))=(guard::Identity::capture(dialog),text(part.address).and_then(|value|initial_location(&value)),automation.as_ref().and_then(|automation|multiple(automation,dialog))){
                        seen.insert(key);preparing.remove(&key);
                        let(token,(types,filter))=(SERIAL.fetch_add(1,Ordering::Relaxed).max(1),types(part.types));
                        // A folder dialog can display its current folder name
                        // in this field. It is not a child path to append again.
                        let initial_name=if kind.folders(){String::new()}else{text(part.name).filter(|name|!name.contains(['*','?'])).unwrap_or_default()};
                        let options=Options{token,title:kind.title().into(),initial,multiple,kind,initial_name,types,filter,events:tx.clone()};
                        let config=profile.to_path_buf();let report=tx.clone();
                        let worker=thread::Builder::new().name("zsdesk-picker-window".into()).spawn(move||{
                            let result=std::panic::catch_unwind(||crate::storage::load(&config).and_then(|session|crate::window::picker::run(session,config,options)))
                                .map_err(|_|"文件选择窗口异常退出".to_owned()).and_then(|result|result.map_err(|error|format!("{error:#}")));
                            let _=report.send(Event::Finished{token,result});
                        })?;
                        let accept=unsafe{GetDlgItem(Some(dialog),1)}.ok().and_then(text).unwrap_or_default();
                        sessions.push(Session{source,name:part.name,types:if kind.folders(){HWND::default()}else{part.types},kind,multiple,accept,token,picker:None,guard:None,worker:Some(worker),fallback:false});
                    }else if since.elapsed()>Duration::from_secs(8){seen.insert(key);preparing.remove(&key);last_error=Some("文件选择窗口的目录或选择模式不可读取，已保留系统窗口".into());
                    }
                }
            }
        }
        let status=serde_json::json!({"pid":std::process::id(),"enabled":enabled,"active_pickers":sessions.len(),"completed":completed,"last_error":last_error}).to_string();
        if status!=previous{let _=crate::quick_search::control::atomic_write(&profile.with_file_name("file-dialog-takeover-status.json"),status.as_bytes());previous=status;}
        thread::sleep(Duration::from_millis(100));
    }
    for session in &mut sessions{session.close_picker();}Ok(())
}
#[cfg(test)]mod tests{
    use super::*;
    #[test]fn only_open_dialogs_are_admitted(){for value in ["打开(&O)","Open","选择文件"]{assert!(open_label(value));}for value in ["保存(&S)","Save","确定","Cancel","选择文件夹","Select Folder"]{assert!(!open_label(value));}}
    #[test]fn folder_selection_requires_an_explicit_folder_contract(){
        for label in ["选择文件夹","选择此文件夹","Select Folder","Choose directory"]{
            assert_eq!(selection_kind(label,false,false),Some(SelectionKind::Folders));assert_eq!(selection_kind(label,true,false),None);
        }
        assert_eq!(selection_kind("Upload",false,true),Some(SelectionKind::Folders));
        assert_eq!(selection_kind("上传",false,true),Some(SelectionKind::Folders));
        assert_eq!(selection_kind("Upload",true,false),Some(SelectionKind::Files));
        assert_eq!(selection_kind("打开(&O)",true,false),Some(SelectionKind::Files));
        for label in ["保存(&S)","Save","Save As","确定","OK","替换","Cancel"]{for types in [false,true]{assert_eq!(selection_kind(label,types,true),None);}}
        assert_eq!(selection_kind("Upload",false,false),None);assert_eq!(selection_kind("Open",false,true),None);
        for label in ["文件夹：","目录:","Folder:","&Folder:"]{assert!(directory_label(label));}
        for label in ["文件名：","File name:","选择文件夹上传"]{assert!(!directory_label(label));}
    }
    #[test]fn filter_labels_preserve_masks_and_default_to_all(){assert_eq!(pattern("文本文件 (*.txt;*.log)"),"*.txt;*.log");assert_eq!(pattern("文档（*.docx）"),"*.docx");assert_eq!(pattern("Files"),"*");}
    #[test]fn initial_location_preserves_unc_and_chinese_paths(){use zsdesk_core::model::explorer::ShellLocation;assert_eq!(initial_location(r"地址: E:\中文 资料"),Some(ShellLocation::Path(r"E:\中文 资料".into())));assert_eq!(initial_location(r"Address: \\server\share"),Some(ShellLocation::Path(r"\\server\share".into())));}
}
