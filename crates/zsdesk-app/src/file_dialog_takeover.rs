//! Routes compatible Open dialogs into the existing ZSDesk explorer window.
//! The original caller still performs its own validation and receives results.
use std::{collections::{HashSet,HashMap},path::{Path,PathBuf},sync::{Arc,atomic::{AtomicBool,Ordering},mpsc},thread::{self,JoinHandle},time::{Duration,Instant}};
use anyhow::{Context,Result,ensure};
use windows::{core::{w,BOOL,Interface,PCWSTR},Win32::{Foundation::{HWND,LPARAM,WPARAM},System::{Com::{CoCreateInstance,CoInitializeEx,CoUninitialize,CLSCTX_INPROC_SERVER,COINIT_MULTITHREADED},Variant::VARIANT},UI::{Accessibility::*,Controls::*,Input::KeyboardAndMouse::IsWindowEnabled,WindowsAndMessaging::*}}};
use crate::window::picker::{Event,FileType,NativeReply,Options,Outcome,SelectionKind};
pub(crate) mod guard;
static DIALOG_BUSY:AtomicBool=AtomicBool::new(false);
pub(crate) fn has_active_dialog()->bool{DIALOG_BUSY.load(Ordering::Acquire)}
mod watcher;
mod capabilities;
#[cfg(test)]#[path="file_dialog_takeover/native_tests.rs"]mod native_tests;
fn wide(value:&str)->Vec<u16>{value.encode_utf16().chain(Some(0)).collect()}
fn class(window:HWND)->String{let mut value=[0u16;128];let n=unsafe{GetClassNameW(window,&mut value)}.max(0)as usize;String::from_utf16_lossy(&value[..n])}
fn send(window:HWND,message:u32,wp:usize,lp:isize)->Option<usize>{let mut result=0;(unsafe{SendMessageTimeoutW(window,message,WPARAM(wp),LPARAM(lp),SMTO_ABORTIFHUNG|SMTO_BLOCK,250,Some(&mut result))}.0!=0).then_some(result)}
fn text(window:HWND)->Option<String>{let mut value=vec![0u16;32768];let n=send(window,WM_GETTEXT,value.len(),value.as_mut_ptr()as isize)?;Some(String::from_utf16_lossy(&value[..n.min(value.len()-1)]))}
#[derive(Default)]struct Parts{name:HWND,types:HWND,address:HWND,shell:HWND,native:bool,folder_label:bool}
// Child visibility is meaningful before the top-level dialog is first shown.
// Do not mistake a hidden file-type control for a folder picker (or vice versa).
fn control_visible(mut window:HWND)->bool{
    if window.0.is_null(){return false;}
    let root=unsafe{GetAncestor(window,GA_ROOT)};
    for _ in 0..32{
        if window.0.is_null(){return false;}
        // Common dialogs can contain hidden child dialogs. Only the actual
        // top-level window may be ignored during pre-show recognition.
        if window==root{return class(root)=="#32770";}
        if unsafe{GetWindowLongPtrW(window,GWL_STYLE)}as u32&WS_VISIBLE.0==0{return false;}
        window=unsafe{GetParent(window)}.unwrap_or_default();
    }false
}
fn control_label(value:&str)->String{
    let value=value.replace('&',"");let mut value=value.trim().trim_end_matches([':', '：']).trim();
    // Native dialogs can append a localized accelerator, e.g. 上传(&U).
    // It is presentation, not part of the caller's selection contract.
    if let Some((prefix,suffix))=value.rsplit_once('('){
        if let Some(key)=suffix.strip_suffix(')'){
            if key.len()==1&&key.as_bytes()[0].is_ascii_alphanumeric(){value=prefix.trim_end();}
        }
    }
    value.to_lowercase()
}
fn directory_label(value:&str)->bool{matches!(control_label(value).as_str(),"文件夹"|"目录"|"folder"|"directory")}
fn folder_title(value:&str)->bool{matches!(control_label(value).as_str(),"选择文件夹"|"选择目录"|"select folder"|"choose folder"|"select directory"|"choose directory")}
fn parts(dialog:HWND)->Parts{
    unsafe extern "system" fn visit(window:HWND,p:LPARAM)->BOOL{let value=unsafe{&mut*(p.0 as*mut Parts)};let kind=class(window);let id=unsafe{GetDlgCtrlID(window)};
        // Common dialogs use either cmb13 (1148) or edt1 (1152). Folder
        // pickers expose the standalone edt1 instead of the file-name combo.
        if [1148,1152].contains(&id)&&kind=="Edit"{
            if value.name.0.is_null()||class(value.name)!="Edit"||control_visible(window){value.name=window;}
        }else if id==1148&&value.name.0.is_null()&&kind=="ComboBox"{value.name=window;}
        if id==1136&&kind=="ComboBox"{value.types=window;}if id==1001&&kind=="ToolbarWindow32"{value.address=window;}
        if kind=="Static"&&control_visible(window)&&text(window).is_some_and(|value|directory_label(&value)){value.folder_label=true;}
        if kind=="DUIViewWndClassName"{value.native=true;}
        if kind=="SHELLDLL_DefView"&&control_visible(window){value.shell=window;}true.into()}
    let mut value=Parts::default();let _=unsafe{EnumChildWindows(Some(dialog),Some(visit),LPARAM(&mut value as*mut _ as isize))};value
}
fn open_label(value:&str)->bool{let value=control_label(value);
    if value.contains("文件夹")||value.contains("folder"){return false;}
    value.starts_with("打开")||value.starts_with("open")||value.starts_with("选择文件")||value.starts_with("select file")}
fn selection_kind(accept:&str,visible_types:bool,folder_field:bool)->Option<SelectionKind>{
    let accept=control_label(accept);
    if accept.is_empty()||accept.starts_with("保存")||accept.starts_with("另存")||accept.starts_with("save")
        ||matches!(accept.as_str(),"取消"|"cancel"|"替换"|"replace"){return None;}
    let folder=accept.starts_with("选择文件夹")||accept.starts_with("选择此文件夹")||accept.starts_with("选择目录")
        ||accept.starts_with("select folder")||accept.starts_with("choose folder")||accept.starts_with("select directory")||accept.starts_with("choose directory");
    let upload=accept=="上传"||accept=="upload";
    // The associated folder field is the explicit directory contract. The
    // caller may independently name its acceptance button Upload, Open or OK.
    // Conflicting directory/file evidence must leave the native dialog intact.
    if folder||folder_field{return (!visible_types).then_some(SelectionKind::Folders);}
    if visible_types&&(open_label(&accept)||upload){return Some(SelectionKind::Files);}
    None
}
fn folder_field_label(automation:&IUIAutomation,name:HWND)->bool{
    let Ok(element)=(unsafe{automation.ElementFromHandle(name)})else{return false;};
    if unsafe{element.CurrentName()}.is_ok_and(|name|directory_label(&name.to_string())){return true;}
    unsafe{element.CurrentLabeledBy()}.and_then(|label|unsafe{label.CurrentName()}).is_ok_and(|name|directory_label(&name.to_string()))
}
fn accept_label(dialog:HWND)->Option<String>{
    unsafe{GetDlgItem(Some(dialog),1)}.ok().and_then(text)
}
fn selection_kind_for(dialog:HWND,parts:&Parts,accept:&str,automation:Option<&IUIAutomation>)->Option<SelectionKind>{
    let visible_types=control_visible(parts.types);
    let folder_field=parts.folder_label||text(dialog).is_some_and(|title|folder_title(&title))
        ||(!visible_types&&automation.is_some_and(|automation|folder_field_label(automation,parts.name)));
    selection_kind(accept,visible_types,folder_field)
}
fn dialog_kind(dialog:HWND,parts:&Parts,automation:Option<&IUIAutomation>)->Option<SelectionKind>{
    selection_kind_for(dialog,parts,&accept_label(dialog)?,automation)
}
fn address_value(value:&str)->&str{
    // Strip the toolbar's localized prefix, not a drive letter or UNC path.
    let value=value.trim();
    if Path::new(value).is_absolute(){return value;}
    value.split_once(": ").or_else(||value.split_once('：')).map_or_else(
        ||if matches!(value,"地址:"|"Address:"){""}else{value},
        |(_,value)|value).trim()
}
fn profile_display_name()->Option<String>{
    use windows::Win32::{System::Com::CoTaskMemFree,UI::Shell::{IShellItem,SHGetKnownFolderItem,FOLDERID_UsersFiles,KF_FLAG_DEFAULT,SIGDN_NORMALDISPLAY}};
    let item:IShellItem=unsafe{SHGetKnownFolderItem(&FOLDERID_UsersFiles,KF_FLAG_DEFAULT,None)}.ok()?;
    let raw=unsafe{item.GetDisplayName(SIGDN_NORMALDISPLAY)}.ok()?;
    let result=unsafe{raw.to_string()}.ok();
    unsafe{CoTaskMemFree(Some(raw.0.cast()));}
    result.filter(|name|!name.trim().is_empty())
}
fn initial_location(value:&str)->Option<zsdesk_core::model::explorer::ShellLocation>{
    use zsdesk_core::model::explorer::ShellLocation;
    let value=address_value(value);
    if Path::new(value).is_absolute(){return Some(ShellLocation::Path(value.into()));}
    match value.to_lowercase().as_str(){"桌面"|"desktop"=>zsdesk_windows::desktop::user_desktop().map(ShellLocation::Path),"此电脑"|"this pc"=>Some(ShellLocation::ThisPc),
        "主文件夹"|"home"=>Some(ShellLocation::Shell{pidl:None,parsing_name:"shell:HomeFolder".into(),title:zsdesk_core::i18n::text("主文件夹").into()}),
        _=>profile_display_name().filter(|name|name.eq_ignore_ascii_case(value)).and_then(|_|dirs::home_dir()).map(ShellLocation::Path)}
}
fn known_location(value:&str)->Option<zsdesk_core::model::explorer::ShellLocation>{
    use windows::Win32::{System::Com::CoTaskMemFree,UI::Shell::*};
    use zsdesk_core::model::explorer::ShellLocation;
    let value=address_value(value);
    if value.is_empty(){return None;}
    let folded=value.to_lowercase();
    for folder in [&FOLDERID_Profile,&FOLDERID_Downloads,&FOLDERID_Pictures,&FOLDERID_Documents,&FOLDERID_Music,&FOLDERID_Videos]{
        let Ok(item)=(unsafe{SHGetKnownFolderItem::<IShellItem>(folder,KF_FLAG_DONT_VERIFY,None)})else{continue;};
        let name=|kind|{
            let raw=unsafe{item.GetDisplayName(kind)}.ok()?;
            let result=unsafe{raw.to_string()}.ok();
            unsafe{CoTaskMemFree(Some(raw.0.cast()));}
            result
        };
        if name(SIGDN_NORMALDISPLAY).is_some_and(|display|display.to_lowercase()==folded){
            if let Some(path)=name(SIGDN_FILESYSPATH){return Some(ShellLocation::Path(path.into()));}
        }
    }
    None
}
fn dialog_location(address:&str)->Option<zsdesk_core::model::explorer::ShellLocation>{
    initial_location(address).or_else(||known_location(address))
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
fn multiple(automation:&IUIAutomation,shell:HWND)->Option<bool>{
    // Other lists in the dialog (such as navigation) do not define file selection.
    if shell.0.is_null(){return None;}
    let root=unsafe{automation.ElementFromHandle(shell)}.ok()?;
    fn selection_mode(item:&IUIAutomationElement)->Option<bool>{
        let selection=unsafe{item.GetCurrentPatternAs::<IUIAutomationSelectionPattern>(UIA_SelectionPatternId)}.ok()?;
        unsafe{selection.CurrentCanSelectMultiple()}.ok().map(|value|value.as_bool())
    }
    if let Some(mode)=selection_mode(&root){return Some(mode);}
    let list=unsafe{automation.CreatePropertyCondition(UIA_ControlTypePropertyId,&VARIANT::from(UIA_ListControlTypeId.0))}.ok()?;
    let grid=unsafe{automation.CreatePropertyCondition(UIA_ControlTypePropertyId,&VARIANT::from(UIA_DataGridControlTypeId.0))}.ok()?;
    let condition=unsafe{automation.CreateOrCondition(&list,&grid)}.ok()?;
    let lists=unsafe{root.FindAll(TreeScope_Descendants,&condition)}.ok()?;
    for index in 0..unsafe{lists.Length()}.ok()?.min(32){let item=unsafe{lists.GetElement(index)}.ok()?;
        if let Some(mode)=selection_mode(&item){return Some(mode);}}
    None
}
#[derive(Clone,PartialEq)]
struct Contract{kind:SelectionKind,name:isize,types:isize,shell:isize,accept:String,address:String}
fn read_contract(dialog:HWND,part:&Parts,automation:Option<&IUIAutomation>)->Result<Contract,&'static str>{
    if part.shell.0.is_null()||part.name.0.is_null(){return Err("waiting-for-controls");}
    let accept=accept_label(dialog).ok_or("waiting-for-confirmation-control")?;
    let kind=selection_kind_for(dialog,part,&accept,automation).ok_or("waiting-for-selection-kind")?;
    let address=text(part.address).filter(|value|!address_value(value).is_empty()).ok_or("waiting-for-initial-location")?;
    Ok(Contract{kind,name:part.name.0 as isize,types:if kind.folders(){0}else{part.types.0 as isize},
        shell:part.shell.0 as isize,accept,address})
}
#[derive(Default)]struct StableContract(Option<(Contract,Instant)>);
impl StableContract{
    fn observe(&mut self,value:Contract,now:Instant)->bool{
        if self.0.as_ref().is_none_or(|(previous,_)|previous!=&value){self.0=Some((value,now));return false;}
        self.0.as_ref().is_some_and(|(_,since)|now.saturating_duration_since(*since)>=Duration::from_millis(150))
    }
    fn reset(&mut self){self.0=None;}
}
struct Session{source:guard::Identity,name:HWND,types:HWND,kind:SelectionKind,multiple:bool,accept:String,token:u64,picker:Option<isize>,guard:Option<guard::Guard>,early:Option<watcher::Claim>,worker:Option<JoinHandle<()>>,fallback:bool}
struct PendingReturn{outcome:Outcome,started:Instant,reported_wait:bool,folder_navigation:Option<PathBuf>}
fn same_folder(location:Option<zsdesk_core::model::explorer::ShellLocation>,path:&Path)->bool{
    location.and_then(|location|location.as_path().map(Path::to_path_buf)).is_some_and(|folder|crate::explorer_forwarder::same_path(&folder,path))
}
fn folder_navigation_pending(kind:SelectionKind,outcome:&Outcome,location:Option<zsdesk_core::model::explorer::ShellLocation>)->Option<PathBuf>{
    if !kind.folders()||location.is_none(){return None;}
    let Outcome::Selected{paths,..}=outcome else{return None;};
    (paths.len()==1&&!same_folder(location,&paths[0])).then(||paths[0].clone())
}
fn folder_navigation_ready(target:&Path,location:Option<zsdesk_core::model::explorer::ShellLocation>,name:&str)->bool{
    let name=name.trim().trim_matches('"');
    same_folder(location,target)&&(name.is_empty()||target.file_name().is_some_and(|leaf|name==leaf.to_string_lossy()))
}
fn native_choice(source:HWND,id:i32)->Result<()>{
    let button=unsafe{GetDlgItem(Some(source),id)}.context("原窗口的确认或取消按钮不可用")?;
    // A real BN_CLICKED notification carries the originating button HWND.
    // lParam=0 is a menu command and can follow a different caller path.
    unsafe{PostMessageW(Some(source),WM_COMMAND,WPARAM(id as usize),LPARAM(button.0 as isize))}?;Ok(())
}
impl Session{
    fn restore(&mut self){self.guard.take();self.early.take();}
    fn picker_alive(&self)->bool{self.picker.is_some_and(|window|unsafe{GetPropW(HWND(window as*mut _),crate::window::picker::MARKER)}.0 as usize==self.token as usize)}
    fn close_picker(&mut self){self.restore();self.fallback=true;if let Some(window)=self.picker{let _=crate::window::picker::native_reply(window,self.token,NativeReply::Fallback);}}
    fn poll(&mut self,enabled:bool){
        if !enabled||!self.source.matches(){self.close_picker();return;}
        if self.early.as_ref().is_some_and(|claim|!claim.active()){self.close_picker();return;}
        if self.guard.as_mut().is_some_and(guard::Guard::exited){
            // The picker can close before its STA has finished COM teardown.
            // Its eventual Selected result must still reach the caller.
            if self.picker_alive(){self.close_picker();}else{self.restore();}
        }
    }
    fn finish(&mut self,outcome:Outcome)->Result<()>{
        if self.fallback||!self.source.matches(){self.restore();return Ok(());}
        match outcome{
            Outcome::Cancelled=>{native_choice(self.source.hwnd(),2)?;},
            Outcome::Selected{paths,filter}=>{
                crate::window::picker::validate_paths(&paths,self.multiple,self.kind).map_err(anyhow::Error::msg)?;
                let actual=parts(self.source.hwnd());let accept=unsafe{GetDlgItem(Some(self.source.hwnd()),1)}.ok().and_then(text);
                ensure!(actual.name==self.name&&accept.as_deref()==Some(self.accept.as_str()),"文件选择窗口已改变");
                let automation:IUIAutomation=unsafe{CoCreateInstance(&CUIAutomation,None,CLSCTX_INPROC_SERVER)}.context("无法确认原窗口的选择模式")?;
                ensure!(dialog_kind(self.source.hwnd(),&actual,Some(&automation))==Some(self.kind),"原窗口的文件或文件夹选择模式已改变");
                if !self.types.0.is_null()&&self.types==actual.types{
                    send(self.types,CB_SETCURSEL,filter,0).context("文件类型切换失败")?;
                    let owner=unsafe{GetParent(self.types)}.context("文件类型控件父窗口不可用")?;
                    send(owner,WM_COMMAND,1136|((CBN_SELCHANGE as usize)<<16),self.types.0 as isize).context("文件类型通知失败")?;
                    send(owner,WM_COMMAND,1136|((CBN_SELENDOK as usize)<<16),self.types.0 as isize).context("文件类型确认失败")?;
                    send(owner,WM_COMMAND,1136|((CBN_CLOSEUP as usize)<<16),self.types.0 as isize).context("文件类型收尾失败")?;
                    #[cfg(test)]eprintln!("filter owner={} control_id={} selected={:?}",class(owner),unsafe{GetDlgCtrlID(self.types)},send(self.types,CB_GETCURSEL,0,0));
                }
                let current_folder=self.kind.folders()&&paths.len()==1&&same_folder(text(actual.address).and_then(|value|dialog_location(&value)),&paths[0]);
                let value=if current_folder{String::new()}else{paths.iter().map(|path|format!("\"{}\"",path.to_string_lossy())).collect::<Vec<_>>().join(" ")};let value=wide(&value);
                ensure!(send(self.name,WM_SETTEXT,0,value.as_ptr()as isize).is_some_and(|result|result!=0),"文件选择结果未能返回");
                // Only the caller's Open command can validate and accept paths.
                native_choice(self.source.hwnd(),1)?;
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
    run_with_watcher(profile,stop,watcher::Connection::spawn)
}
fn run_with_watcher(profile:&Path,stop:&AtomicBool,mut spawn_watcher:impl FnMut()->Result<watcher::Connection>)->Result<()>{
    unsafe{CoInitializeEx(None,COINIT_MULTITHREADED)}.ok()?;struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{CoUninitialize();}}}let _apartment=Apartment;
    struct ActiveDialogs;impl Drop for ActiveDialogs{fn drop(&mut self){DIALOG_BUSY.store(false,std::sync::atomic::Ordering::Release);}}let _busy=ActiveDialogs;
    let mut automation:Option<IUIAutomation>=None;
    let(tx,rx)=mpsc::channel();let mut sessions=Vec::<Session>::new();let mut preparing=Vec::<watcher::Claim>::new();let mut contracts=HashMap::<u64,StableContract>::new();
    let mut watcher:Option<watcher::Connection>=None;let mut enabled=false;let mut returning=HashMap::<u64,PendingReturn>::new();
    let mut profile_session=None;
    let mut refresh=Instant::now()-Duration::from_secs(3);let mut last_error=None::<String>;let mut previous=String::new();let mut completed=0u64;
    let mut last_kind=None::<&'static str>;let mut last_confirmation=None::<&'static str>;
    let mut preserved_native=0u64;let mut last_preservation=None::<&'static str>;
    // Stages contain no caller titles, file names, or paths.
    let mut last_handoff=None::<(u64,&'static str,u64)>;
    let record=|last:&mut Option<(u64,&'static str,u64)>,claim:&watcher::Claim,stage|{
        if !last.is_some_and(|(token,previous,_)|token==claim.token()&&previous==stage){
            *last=Some((claim.token(),stage,claim.age_ms()));
        }
    };
    while !stop.load(Ordering::Acquire){
        if refresh.elapsed()>=Duration::from_secs(1){
            profile_session=crate::storage::load(profile).ok().filter(|session|session.ui.file_dialog_takeover);
            enabled=profile_session.is_some();refresh=Instant::now();
            if watcher.as_mut().is_some_and(watcher::Connection::exited){watcher.take();preparing.clear();}
            if enabled&&watcher.is_none(){match spawn_watcher(){Ok(value)=>watcher=Some(value),Err(error)=>last_error=Some(error.to_string())}}
            if let Some(watcher)=&watcher{let _=watcher.enable(enabled);let _=watcher.heartbeat();}
            if enabled&&automation.is_none(){match unsafe{CoCreateInstance(&CUIAutomation,None,CLSCTX_INPROC_SERVER)}{Ok(value)=>automation=Some(value),Err(error)=>last_error=Some(error.to_string())}}
            if !enabled{automation=None;preparing.clear();contracts.clear();}
        }
        for event in rx.try_iter(){match event{
            Event::Ready{token,window,use_native,show}=>if let Some(session)=sessions.iter_mut().find(|session|session.token==token){
                session.picker=Some(window);
                if enabled&&!session.fallback&&session.early.as_ref().is_some_and(watcher::Claim::preparing){
                    let dialog=session.source.hwnd();
                    let part=parts(dialog);
                    let kind=dialog_kind(dialog,&part,automation.as_ref());
                    match capabilities::inspect(dialog,&part,kind,automation.as_ref()){
                        Err(reason)=>{
                            record(&mut last_handoff,session.early.as_ref().unwrap(),reason.key());
                            session.close_picker();preserved_native+=1;last_preservation=Some(reason.key());last_kind=Some("native");last_confirmation=None;continue;
                        }
                        // If the alternative appeared late, don't show a
                        // picker that can't get back to it.
                        Ok(true) if !use_native=>{
                            record(&mut last_handoff,session.early.as_ref().unwrap(),"late-native-alternative");
                            session.close_picker();continue;
                        }
                        Ok(_)=>{},
                    }
                    if kind==Some(session.kind)&&part.name==session.name&&(session.kind.folders()||part.types==session.types)
                        &&accept_label(dialog).as_deref()==Some(session.accept.as_str())
                        &&session.early.as_ref().is_some_and(|claim|claim.bind(window).is_ok())
                        &&show.send(()).is_ok(){record(&mut last_handoff,session.early.as_ref().unwrap(),"show-authorized");continue;}
                }
                if let Some(claim)=&session.early{record(&mut last_handoff,claim,"show-not-authorized");}
                session.close_picker();
            },
            Event::UseNative{token}=>if let Some(session)=sessions.iter_mut().find(|session|session.token==token){
                if !session.fallback&&!returning.contains_key(&token)&&session.early.as_ref().is_some_and(watcher::Claim::active){
                    record(&mut last_handoff,session.early.as_ref().unwrap(),"native-requested");
                    session.close_picker();
                    // Only a deliberate switch activates the caller. Other
                    // recovery paths must never steal foreground focus.
                    if session.source.matches(){let _=unsafe{SetForegroundWindow(session.source.hwnd())};}
                    last_kind=Some("native");last_error=None;
                }
            },
            Event::Submit{token,outcome}=>if let Some(session)=sessions.iter_mut().find(|session|session.token==token){
                if session.fallback||!session.source.matches()||session.early.as_ref().is_some_and(|claim|!claim.active()){continue;}
                if returning.contains_key(&token)&&!matches!(outcome,Outcome::Cancelled){continue;}
                let folder_navigation=if session.kind.folders()&&matches!(outcome,Outcome::Selected{..}){
                    let native=parts(session.source.hwnd());
                    folder_navigation_pending(session.kind,&outcome,text(native.address).and_then(|value|dialog_location(&value)))
                }else{None};
                match session.finish(outcome.clone()){
                    Ok(())=>{last_confirmation=Some("pending");returning.insert(token,PendingReturn{outcome,started:Instant::now(),reported_wait:false,folder_navigation});},
                    Err(error)=>{last_confirmation=Some("rejected");last_error=Some(format!("选择结果回传失败：{error:#}"));if let Some(window)=session.picker{let reply=if returning.contains_key(&token){NativeReply::Waiting}else{NativeReply::Rejected};let _=crate::window::picker::native_reply(window,token,reply);}},
                }
            },
            Event::Finished{token,result}=>if let Some(index)=sessions.iter().position(|session|session.token==token){let mut session=sessions.remove(index);
                returning.remove(&token);session.restore();if let Err(error)=result{last_error=Some(error);}
            },
        }}
        let mut index=0;
        while index<sessions.len(){
            let session=&mut sessions[index];let token=session.token;
            if let Some(pending)=returning.get_mut(&token){
                if !session.source.matches(){
                    let reply=if matches!(pending.outcome,Outcome::Cancelled){NativeReply::Cancelled}else{NativeReply::Accepted};
                    if session.picker.is_some_and(|window|crate::window::picker::native_reply(window,token,reply))||!session.picker_alive(){
                        last_confirmation=Some(if matches!(pending.outcome,Outcome::Cancelled){"cancelled"}else{"accepted"});
                        sessions.remove(index);returning.remove(&token);completed+=1;last_error=None;continue;
                    }
                    index+=1;continue;
                }
                if pending.started.elapsed()>=Duration::from_millis(150)&&unsafe{IsWindowEnabled(session.source.hwnd())}.as_bool(){
                    if let Some(target)=pending.folder_navigation.as_ref(){
                        let native=parts(session.source.hwnd());let location=text(native.address).and_then(|value|dialog_location(&value));
                        if native.name==session.name&&text(native.name).is_some_and(|name|folder_navigation_ready(target,location,&name)){
                            // Some folder pickers consume a typed absolute path
                            // as navigation first. Confirm the current folder
                            // only after both navigation and text consumption
                            // are observed; never repeat the initial submission.
                            pending.folder_navigation=None;pending.started=Instant::now();
                            let empty=wide("");
                            if send(session.name,WM_SETTEXT,0,empty.as_ptr()as isize).is_some_and(|result|result!=0){
                                if let Err(error)=native_choice(session.source.hwnd(),1){last_error=Some(format!("文件夹确认失败：{error:#}"));}
                            }else{last_error=Some("原程序的文件夹输入状态未能完成确认".into());}
                        }
                    }
                }
                if !pending.reported_wait&&pending.started.elapsed()>=Duration::from_secs(2){
                    last_confirmation=Some("awaiting_caller");
                    pending.reported_wait=true;last_error=Some("原程序尚未确认选择结果，选择窗口保持打开".into());
                    if let Some(window)=session.picker{let _=crate::window::picker::native_reply(window,token,NativeReply::Waiting);}
                }
                if matches!(pending.outcome,Outcome::Cancelled)&&pending.started.elapsed()>=Duration::from_secs(8){
                    last_error=Some("原程序未响应取消，已恢复原选择窗口".into());session.close_picker();
                }
            }
            let startup_failure=session.early.as_ref().filter(|claim|!session.fallback&&!claim.active()&&session.source.matches()
                &&last_handoff.is_some_and(|(id,stage,_)|id==token&&stage=="starting-picker"));
            if let Some(claim)=startup_failure{record(&mut last_handoff,claim,"picker-startup-timeout");}
            session.poll(enabled);if session.fallback{returning.remove(&token);}index+=1;
        }
        if let Some(watcher)=&watcher{preparing.extend(watcher.claims());}
        preparing.retain(|claim|{
            if claim.preparing(){return true;}
            if let Some((token,stage,ms))=last_handoff.as_mut(){
                if *token==claim.token(){*ms=claim.age_ms();last_error=Some(format!("原文件选择窗口已恢复：{stage}"));}
            }
            false
        });
        contracts.retain(|token,_|preparing.iter().any(|claim|claim.token()==*token));
        if let Some(profile_session)=profile_session.as_ref(){
            let mut index=0;
            while index<preparing.len()&&sessions.len()<4{
                let source=preparing[index].source();let dialog=source.hwnd();let part=parts(dialog);let token=preparing[index].token();
                let observed=contracts.entry(token).or_default();
                let contract=match read_contract(dialog,&part,automation.as_ref()){
                    Ok(contract)=>contract,
                    Err(stage)=>{record(&mut last_handoff,&preparing[index],stage);observed.reset();index+=1;continue;}
                };
                // Do not scan the accessibility tree while the caller is still
                // switching between its initial file and folder controls.
                if !observed.observe(contract.clone(),Instant::now()){record(&mut last_handoff,&preparing[index],"stabilizing-contract");index+=1;continue;}
                let Some(multiple)=automation.as_ref().and_then(|uia|multiple(uia,part.shell))else{record(&mut last_handoff,&preparing[index],"waiting-for-multi-select-mode");index+=1;continue;};
                record(&mut last_handoff,&preparing[index],"inspecting-actions");
                let use_native=match capabilities::inspect(dialog,&part,Some(contract.kind),automation.as_ref()){
                    Ok(use_native)=>use_native,
                    Err(reason)=>{
                        record(&mut last_handoff,&preparing[index],reason.key());
                        contracts.remove(&token);drop(preparing.remove(index));
                        preserved_native+=1;last_preservation=Some(reason.key());last_kind=Some("native");last_confirmation=None;continue;
                    }
                };
                let current=parts(dialog);
                if read_contract(dialog,&current,automation.as_ref()).as_ref().ok()!=Some(&contract){
                    observed.reset();index+=1;continue;
                }
                let location=dialog_location(&contract.address);
                if location.is_none()&&contract.kind.folders(){
                    record(&mut last_handoff,&preparing[index],"unsupported-folder-location");
                    drop(preparing.remove(index));contracts.remove(&token);continue;
                }
                // Absolute file results don't depend on the caller's current
                // folder. Don't copy relative input into a different location.
                let matched_location=location.is_some();
                let initial=location.unwrap_or(zsdesk_core::model::explorer::ShellLocation::ThisPc);
                let Contract{kind,accept,..}=contract;
                contracts.remove(&token);let early=preparing.remove(index);
                let (types,filter)=if kind.folders(){(Vec::new(),0)}else{types(part.types)};
                let initial_name=if kind.folders()||!matched_location{String::new()}else{text(part.name).filter(|name|!name.contains(['*','?'])).unwrap_or_default()};
                record(&mut last_handoff,&early,"starting-picker");
                if !early.preparing(){record(&mut last_handoff,&early,"preparation-timeout");continue;}
                let options=Options{token,title:kind.title().into(),initial,multiple,kind,initial_name,types,filter,use_native,events:tx.clone()};
                let config=profile.to_path_buf();let report=tx.clone();
                let profile_session=profile_session.clone();let time_to_show=early.show_check();
                let worker=thread::Builder::new().name("zsdesk-picker-window".into()).spawn(move||{
                    let result=std::panic::catch_unwind(||crate::window::picker::run(profile_session,config,options,time_to_show))
                        .map_err(|_|"文件选择窗口异常退出".to_owned()).and_then(|result|result.map_err(|error|format!("{error:#}")));
                    let _=report.send(Event::Finished{token,result});
                })?;
                last_kind=Some(if kind.folders(){"folder"}else{"file"});last_confirmation=None;
                sessions.push(Session{source,name:part.name,types:if kind.folders(){HWND::default()}else{part.types},kind,multiple,accept,token,picker:None,guard:None,early:Some(early),worker:Some(worker),fallback:false});
            }
        }
        DIALOG_BUSY.store(!sessions.is_empty()||!returning.is_empty()||!preparing.is_empty(),std::sync::atomic::Ordering::Release);
        let status=serde_json::json!({"pid":std::process::id(),"enabled":enabled,"active_pickers":sessions.len(),"awaiting_caller":returning.len(),"last_kind":last_kind,"last_confirmation":last_confirmation,"completed":completed,"last_error":last_error,"early_masking":watcher.is_some(),"preserved_native":preserved_native,"last_preservation":last_preservation,"last_handoff":last_handoff.map(|(_,stage,ms)|serde_json::json!({"stage":stage,"elapsed_ms":ms}))}).to_string();
        if status!=previous{let _=crate::quick_search::control::atomic_write(&profile.with_file_name("file-dialog-takeover-status.json"),status.as_bytes());previous=status;}
        thread::sleep(Duration::from_millis(10));
    }
    for session in &mut sessions{session.close_picker();}Ok(())
}

#[cfg(test)]mod tests{
    use super::*;
    #[test]fn profile_breadcrumb_resolves_the_localized_shell_display_name()->Result<()>{
        unsafe{CoInitializeEx(None,COINIT_MULTITHREADED)}.ok()?;
        struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{CoUninitialize();}}}
        let _apartment=Apartment;
        let label=profile_display_name().context("current user shell folder")?;
        assert_eq!(initial_location(&format!("地址: {label}")),dirs::home_dir().map(zsdesk_core::model::explorer::ShellLocation::Path));
        Ok(())
    }
    #[test]fn folder_followup_requires_confirmed_navigation_and_consumed_input(){
        use zsdesk_core::model::explorer::ShellLocation;
        let target=PathBuf::from(r"E:\selected folder");let selected=Outcome::Selected{paths:vec![target.clone()],filter:0};
        assert_eq!(folder_navigation_pending(SelectionKind::Folders,&selected,Some(ShellLocation::ThisPc)),Some(target.clone()));
        assert!(folder_navigation_pending(SelectionKind::Folders,&selected,Some(ShellLocation::Path(target.clone()))).is_none());
        assert!(folder_navigation_pending(SelectionKind::Files,&selected,Some(ShellLocation::ThisPc)).is_none());
        assert!(!folder_navigation_ready(&target,Some(ShellLocation::ThisPc),""));
        assert!(!folder_navigation_ready(&target,Some(ShellLocation::Path(target.clone())),r#""E:\selected folder""#));
        assert!(folder_navigation_ready(&target,Some(ShellLocation::Path(target.clone())),"selected folder"));
        assert!(folder_navigation_ready(&target,Some(ShellLocation::Path(target.clone())),""));
        assert!(folder_navigation_pending(SelectionKind::Folders,&Outcome::Cancelled,Some(ShellLocation::ThisPc)).is_none());
    }
    #[test]fn exact_folder_picker_titles_preserve_the_directory_contract(){
        for title in ["选择文件夹","Select folder","选择目录","Choose directory"]{
            assert_eq!(selection_kind("Open",false,folder_title(title)),Some(SelectionKind::Folders));
        }
        for title in ["选择文件","选择文件夹中的文件","Select files","Upload"]{assert!(!folder_title(title));}
        assert_eq!(selection_kind("Save",false,folder_title("Select folder")),None);
        assert_eq!(selection_kind("Open",true,folder_title("Select folder")),None);
    }
    #[test]fn folder_mode_must_settle_after_transient_file_controls(){
        let start=Instant::now();let mut observed=StableContract::default();
        let file=Contract{kind:SelectionKind::Files,name:1,types:2,shell:3,accept:"上传".into(),address:"此电脑".into()};
        assert!(!observed.observe(file.clone(),start));
        assert!(!observed.observe(file.clone(),start+Duration::from_millis(100)));
        let folder=Contract{kind:SelectionKind::Folders,types:0,..file};
        assert!(!observed.observe(folder.clone(),start+Duration::from_millis(120)));
        assert!(!observed.observe(folder.clone(),start+Duration::from_millis(269)));
        assert!(observed.observe(folder.clone(),start+Duration::from_millis(270)));
        observed.reset();assert!(!observed.observe(folder,start+Duration::from_millis(300)));
    }
    #[test]
    #[ignore="Owned hidden window hierarchy; no input or caller windows"]
    fn native_folder_mode_ignores_a_hidden_nested_dialog()->Result<()>{
        let root=unsafe{CreateWindowExW(WS_EX_TOOLWINDOW|WS_EX_NOACTIVATE,w!("#32770"),w!("Nested control visibility fixture"),WS_POPUP,-30000,-30000,200,100,None,None,None,None)}?;
        struct Window(HWND);impl Drop for Window{fn drop(&mut self){let _=unsafe{DestroyWindow(self.0)};}}let _root=Window(root);
        let group=unsafe{CreateWindowExW(WINDOW_EX_STYLE(0),w!("#32770"),None,WS_CHILD,0,0,100,50,Some(root),None,None,None)}?;
        let types=unsafe{CreateWindowExW(WINDOW_EX_STYLE(0),w!("COMBOBOX"),None,WS_CHILD|WS_VISIBLE,0,0,80,20,Some(group),None,None,None)}?;
        assert!(!control_visible(types),"hidden inner dialog exposed a file-type control");
        assert_eq!(selection_kind("上传",control_visible(types),true),Some(SelectionKind::Folders));
        unsafe{let _=ShowWindow(group,SW_SHOWNA);}
        assert!(!unsafe{IsWindowVisible(root)}.as_bool());
        assert!(control_visible(types),"only the top-level pre-show state may be ignored");
        unsafe{let _=ShowWindow(types,SW_HIDE);}
        assert!(!control_visible(types));Ok(())
    }
    #[test]fn only_open_dialogs_are_admitted(){for value in ["打开(&O)","Open","选择文件"]{assert!(open_label(value));}for value in ["保存(&S)","Save","确定","Cancel","选择文件夹","Select Folder"]{assert!(!open_label(value));}}
    #[test]fn folder_selection_requires_an_explicit_folder_contract(){
        for label in ["选择文件夹","选择此文件夹","Select Folder","Choose directory"]{
            assert_eq!(selection_kind(label,false,false),Some(SelectionKind::Folders));assert_eq!(selection_kind(label,true,false),None);
        }
        for label in ["Upload","上传","上传(&U)","Upload (&U)","打开(&O)","Open","确定","OK","添加到项目"]{
            assert_eq!(selection_kind(label,false,true),Some(SelectionKind::Folders),"{label}");
            assert_eq!(selection_kind(label,true,true),None,"conflicting folder and file controls: {label}");
        }
        assert_eq!(selection_kind("Upload",true,false),Some(SelectionKind::Files));
        assert_eq!(selection_kind("上传(&U)",true,false),Some(SelectionKind::Files));
        assert_eq!(selection_kind("打开(&O)",true,false),Some(SelectionKind::Files));
        for label in ["保存(&S)","Save","Save As","替换","Cancel",""]{for types in [false,true]{assert_eq!(selection_kind(label,types,true),None);}}
        for label in ["Upload","Open","确定","OK"]{assert_eq!(selection_kind(label,false,false),None);}
        for label in ["文件夹：","目录:","Folder:","&Folder:","文件夹(&F)："]{assert!(directory_label(label));}
        for label in ["文件名：","File name:","选择文件夹上传"]{assert!(!directory_label(label));}
    }
    #[test]fn filter_labels_preserve_masks_and_default_to_all(){assert_eq!(pattern("文本文件 (*.txt;*.log)"),"*.txt;*.log");assert_eq!(pattern("文档（*.docx）"),"*.docx");assert_eq!(pattern("Files"),"*");}
    #[test]fn initial_location_preserves_unc_and_chinese_paths(){
        use zsdesk_core::model::explorer::ShellLocation;
        assert_eq!(initial_location(r"地址: E:\中文 资料"),Some(ShellLocation::Path(r"E:\中文 资料".into())));
        assert_eq!(initial_location(r"Address: \\server\share"),Some(ShellLocation::Path(r"\\server\share".into())));
        assert_eq!(address_value("地址: "), "");
        assert_eq!(address_value("Address: Administrator"), "Administrator");
        assert_eq!(address_value(r"E:\中文 资料"), r"E:\中文 资料");
    }
    #[test]
    #[ignore="Read-only check of an explicitly identified open native dialog"]
    fn inspect_live_dialog()->Result<()>{
        let hwnd=std::env::var("ZSDESK_INSPECT_DIALOG")?.parse::<isize>()?;
        let dialog=HWND(hwnd as *mut _);
        ensure!(class(dialog)=="#32770","not a native dialog");
        unsafe{CoInitializeEx(None,COINIT_MULTITHREADED)}.ok()?;
        struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{CoUninitialize();}}}let _apartment=Apartment;
        let uia:IUIAutomation=unsafe{CoCreateInstance(&CUIAutomation,None,CLSCTX_INPROC_SERVER)}?;
        let start=Instant::now();
        let part=parts(dialog);
        let contract=read_contract(dialog,&part,Some(&uia)).map_err(anyhow::Error::msg)?;
        let resolved=dialog_location(&contract.address).is_some();
        let mode=multiple(&uia,part.shell).context("selection mode")?;
        let action=capabilities::inspect(dialog,&part,Some(contract.kind),Some(&uia)).map_err(|reason|anyhow::anyhow!("{}",reason.key()))?;
        let (filters,_)=types(part.types);
        eprintln!("kind={:?} resolved_location={resolved} multiple={mode} native_alternative={action} filters={} inspection_ms={}",contract.kind,filters.len(),start.elapsed().as_millis());
        Ok(())
    }
}
