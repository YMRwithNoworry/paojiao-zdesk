//! File selection uses the ordinary explorer window and its existing tabs.
use super::*;
use std::{path::{Path,PathBuf},sync::mpsc::Sender};
use zsdesk_windows::gfx::{Canvas,RectF};
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
#[cfg(test)]#[path="picker_regression.rs"]mod native_tests;

pub(crate) const MARKER:PCWSTR=windows::core::w!("ZSDesk.FilePicker.v1");
pub(super) const ACCEPT:usize=7301;
pub(super) const CANCEL:usize=7302;
const NAME:usize=7303;
const FILTER:usize=7304;
const USE_ORIGINAL:usize=7305;
const FOOTER_H:i32=76;
pub(crate) const WM_NATIVE_REPLY:u32=WM_APP+147;
#[derive(Clone,Copy)]pub(crate) enum NativeReply{Accepted=1,Cancelled=2,Rejected=3,Waiting=4,Fallback=5,MobileUpload=6}
pub(crate) fn native_reply(window:isize,token:u64,reply:NativeReply)->bool{
    let window=HWND(window as*mut _);
    if unsafe{GetPropW(window,MARKER)}.0 as usize!=token as usize{return false;}
    unsafe{PostMessageW(Some(window),WM_NATIVE_REPLY,WPARAM(token as usize),LPARAM(reply as isize))}.is_ok()
}
#[derive(Clone,Debug,serde::Serialize,serde::Deserialize)]
pub(crate) struct FileType{pub label:String,pub pattern:String}
#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub(crate) enum SelectionKind{Files,Folders}
impl SelectionKind{
    pub fn folders(self)->bool{self==Self::Folders}
    pub fn title(self)->&'static str{if self.folders(){zsdesk_core::i18n::text("选择文件夹")}else{zsdesk_core::i18n::text("选择文件")}}
}
#[derive(Clone)]pub(crate) struct Options{
    pub token:u64,pub title:String,pub initial:ShellLocation,pub multiple:bool,pub kind:SelectionKind,
    pub initial_name:String,pub types:Vec<FileType>,pub filter:usize,pub events:Sender<Event>,
}
#[derive(Clone,Debug,Default)]pub(crate) enum Outcome{
    Selected{paths:Vec<PathBuf>,filter:usize},
    #[default] Cancelled,
}
pub(crate) enum Event{Ready{token:u64,window:isize},UseOriginal{token:u64},Submit{token:u64,outcome:Outcome},Finished{token:u64,result:Result<Outcome,String>}}
#[derive(Default)]struct Submission{waiting:bool,cancel_requested:bool,confirmed:bool,selected:Option<Outcome>}
impl Submission{
    fn begin(&mut self,outcome:&Outcome)->bool{
        let cancelled=matches!(outcome,Outcome::Cancelled);
        if self.confirmed||(self.waiting&&!cancelled)||(cancelled&&self.cancel_requested){return false;}
        if !cancelled{self.selected=Some(outcome.clone());}
        self.waiting=true;self.cancel_requested=cancelled;true
    }
    fn reply(&mut self,reply:NativeReply)->Option<Outcome>{
        match reply{
            NativeReply::Accepted=>{self.confirmed=true;self.waiting=false;self.selected.take()},
            NativeReply::Cancelled|NativeReply::Fallback=>{self.confirmed=true;self.waiting=false;Some(Outcome::Cancelled)},
            NativeReply::Rejected=>{self.waiting=false;self.cancel_requested=false;self.selected=None;None},
            NativeReply::Waiting|NativeReply::MobileUpload=>None,
        }
    }
}
pub(super) struct State{
    pub options:Options,pub result:Rc<RefCell<Outcome>>,name:HWND,filter:HWND,accept:HWND,cancel:HWND,original:HWND,edited:bool,submission:Submission,
}
impl State{
    pub fn new(options:Options,result:Rc<RefCell<Outcome>>)->Self{let edited=!options.initial_name.is_empty();Self{options,result,name:HWND::default(),filter:HWND::default(),accept:HWND::default(),cancel:HWND::default(),original:HWND::default(),edited,submission:Submission::default()}}
    pub fn pattern(&self)->&str{if self.options.kind.folders(){"*"}else{self.options.types.get(self.options.filter).map_or("*",|value|value.pattern.as_str())}}
    fn controls(&self)->[HWND;5]{[self.name,self.filter,self.accept,self.cancel,self.original]}
}
fn prepare_session(mut session:ExplorerSession,initial:&ShellLocation)->ExplorerSession{
    for tab in &mut session.tabs{tab.locked=false;if let Some(view)=&mut tab.view{view.selected.clear();view.focused=None;}}
    session.secondary_pane=Default::default();session.ui.dual_pane=false;
    if let Some(index)=session.tabs.iter().position(|tab|shell_registration::same_location(tab.current(),initial)){session.active_tab=index;}
    else{let id=session.take_id();session.tabs.push(ExplorerTab::new(id,initial.clone()));session.active_tab=session.tabs.len()-1;}
    session
}
pub(crate) fn run(session:ExplorerSession,profile:PathBuf,options:Options)->Result<Outcome>{
    let session=prepare_session(session,&options.initial);
    let result=Rc::new(RefCell::new(Outcome::Cancelled));
    unsafe{OleInitialize(None)}?;
    struct Apartment;impl Drop for Apartment{fn drop(&mut self){zsdesk_windows::flyout::release_thread_resources();unsafe{OleUninitialize();}}}let _apartment=Apartment;
    unsafe{run_window(session,profile,None,true,true,None,Some(State::new(options,result.clone())))}?;
    let outcome=result.borrow().clone();Ok(outcome)
}
impl WindowState{
    pub(super) fn picker_close_pending(&mut self)->bool{
        if self.picker.as_ref().is_none_or(|picker|picker.submission.confirmed){return false;}
        self.picker_submit(Outcome::Cancelled);true
    }
    fn picker_submit(&mut self,outcome:Outcome){
        let Some(picker)=self.picker.as_mut()else{return;};
        if !picker.submission.begin(&outcome){return;}
        if picker.options.events.send(Event::Submit{token:picker.options.token,outcome}).is_err(){
            picker.submission.confirmed=true;*picker.result.borrow_mut()=Outcome::Cancelled;post(self.hwnd,WM_CLOSE,0,0);return;
        }
        unsafe{let _=EnableWindow(picker.accept,false);let _=EnableWindow(picker.original,false);}
        self.show_message(zsdesk_core::i18n::text("正在等待原程序确认选择…").into());
    }
    pub(super) fn picker_native_reply(&mut self,token:u64,code:isize){
        let reply=match code{1=>NativeReply::Accepted,2=>NativeReply::Cancelled,3=>NativeReply::Rejected,4=>NativeReply::Waiting,5=>NativeReply::Fallback,6=>NativeReply::MobileUpload,_=>return};
        let Some(picker)=self.picker.as_mut().filter(|picker|picker.options.token==token)else{return;};
        if matches!(reply,NativeReply::MobileUpload){
            unsafe{let _=ShowWindow(picker.original,SW_SHOWNA);}return;
        }
        if let Some(outcome)=picker.submission.reply(reply){*picker.result.borrow_mut()=outcome;post(self.hwnd,WM_CLOSE,0,0);return;}
        unsafe{let _=EnableWindow(picker.accept,!picker.submission.waiting);let _=EnableWindow(picker.original,!picker.submission.waiting);}
        self.show_message(match reply{NativeReply::Rejected=>zsdesk_core::i18n::text("原程序未接受选择，请检查后重试"),_=>zsdesk_core::i18n::text("原程序尚未完成选择，可取消后重新打开")}.into());
    }
    pub(super) fn picker_height(&self)->i32{if self.picker.is_some(){self.s(FOOTER_H)}else{0}}
    pub(super) fn initialize_picker(&mut self)->Result<()>{
        let Some(picker)=self.picker.as_mut()else{return Ok(());};
        unsafe{
            SetPropW(self.hwnd,MARKER,Some(windows::Win32::Foundation::HANDLE(picker.options.token as usize as*mut _)))?;
            picker.name=child(self.hwnd,"EDIT",&picker.options.initial_name,NAME,ES_AUTOHSCROLL as u32,true)?;
            picker.filter=child(self.hwnd,"COMBOBOX","",FILTER,(CBS_DROPDOWNLIST|WS_VSCROLL.0 as i32)as u32,!picker.options.kind.folders())?;
            picker.accept=child(self.hwnd,"BUTTON",picker.options.kind.title(),ACCEPT,BS_DEFPUSHBUTTON as u32,true)?;
            picker.cancel=child(self.hwnd,"BUTTON",zsdesk_core::i18n::text("取消"),CANCEL,BS_PUSHBUTTON as u32,true)?;
            picker.original=child(self.hwnd,"BUTTON",zsdesk_core::i18n::text("其他上传方式…"),USE_ORIGINAL,BS_PUSHBUTTON as u32,false)?;
            for value in &picker.options.types{SendMessageW(picker.filter,CB_ADDSTRING,Some(WPARAM(0)),Some(LPARAM(wide(&value.label).as_ptr()as isize)));}
            SendMessageW(picker.filter,CB_SETCURSEL,Some(WPARAM(picker.options.filter)),Some(LPARAM(0)));
        }Ok(())
    }
    pub(super) fn picker_fonts(&self){if let Some(picker)=&self.picker{for window in picker.controls(){if !window.0.is_null(){unsafe{SendMessageW(window,WM_SETFONT,Some(WPARAM(self.font.0 as usize)),Some(LPARAM(1)));}}}}}
    pub(super) fn position_picker(&self){
        let Some(picker)=&self.picker else{return;};let (width,height)=self.client_size();let top=height-self.s(FOOTER_H);
        let pad=self.s(12);let gap=self.s(10);let control=self.s(28);let button=self.s(100);let filter=if picker.options.kind.folders(){0}else{self.s(260).min((width/3).max(self.s(120)))};
        unsafe{
            move_control(picker.name,LayoutRect::new(self.s(76),top+self.s(8),(width-self.s(76)-filter-pad-gap).max(20),control));
            move_control(picker.filter,LayoutRect::new(width-filter-pad,top+self.s(8),filter,self.s(240)));
            move_control(picker.accept,LayoutRect::new(width-pad-button*2-gap,top+self.s(42),button,control));
            move_control(picker.cancel,LayoutRect::new(width-pad-button,top+self.s(42),button,control));
            move_control(picker.original,LayoutRect::new(pad,top+self.s(42),self.s(140),control));
            // Moving a child never changes WS_VISIBLE. Keep the footer usable
            // after creation, resize, theme changes and navigation.
            for window in [picker.name,picker.accept,picker.cancel]{let _=ShowWindow(window,SW_SHOWNA);}
            let _=ShowWindow(picker.filter,if picker.options.kind.folders(){SW_HIDE}else{SW_SHOWNA});
        }
    }
    pub(super) fn paint_picker(&self,canvas:&Canvas){
        let Some(picker)=&self.picker else{return;};let(w,h)=self.client_size();let top=h-self.s(FOOTER_H);
        canvas.fill(RectF::new(0.0,top as f32,w as f32,self.s(FOOTER_H)as f32),paint::c(self.palette.layer));
        canvas.line(0.0,top as f32,w as f32,top as f32,paint::c(self.palette.divider),1.0);
        canvas.text(if picker.options.kind.folders(){zsdesk_core::i18n::text("文件夹：")}else{zsdesk_core::i18n::text("文件名：")},RectF::new(self.sf(12.0),(top+self.s(8))as f32,self.sf(64.0),self.sf(28.0)),&self.ui_style(),paint::c(self.palette.text));
    }
    pub(super) fn picker_selection_changed(&mut self){
        let Some(picker)=self.picker.as_ref()else{return;};if picker.edited&&unsafe{GetFocus()}==picker.name{return;}
        let paths=self.browser.as_ref().map(|browser|browser.selection_paths_limit(257)).unwrap_or_default();
        if picker.edited&&paths.is_empty(){return;}
        let value=paths.iter().filter_map(|path|Path::new(path).file_name()).map(|name|format!("\"{}\"",name.to_string_lossy())).collect::<Vec<_>>().join(" ");
        let name=picker.name;if let Some(picker)=self.picker.as_mut(){picker.edited=false;}
        let _=unsafe{SetWindowTextW(name,PCWSTR(wide(&value).as_ptr()))};
    }
    pub(super) fn picker_command(&mut self,id:usize,code:u32)->bool{
        if self.picker.is_none(){return false;}
        match id{
            ACCEPT=>{self.picker_accept();true},CANCEL=>{post(self.hwnd,WM_CLOSE,0,0);true},
            USE_ORIGINAL=>{
                if let Some(p)=self.picker.as_mut(){
                    if !p.submission.waiting && unsafe{IsWindowVisible(p.original)}.as_bool(){
                        if p.options.events.send(Event::UseOriginal{token:p.options.token}).is_ok(){
                            p.submission.waiting=true;
                            unsafe{let _=EnableWindow(p.original,false);let _=EnableWindow(p.accept,false);}
                        }
                    }
                }
                true
            },
            NAME=>{if code==EN_CHANGE{if let Some(picker)=self.picker.as_mut(){if unsafe{GetFocus()}==picker.name{picker.edited=true;}}}true},
            FILTER=>{if code==CBN_SELCHANGE as u32{
                let index=unsafe{SendMessageW(self.picker.as_ref().unwrap().filter,CB_GETCURSEL,None,None)}.0;
                if index>=0{let picker=self.picker.as_mut().unwrap();picker.options.filter=index as usize;
                    if let Some(browser)=&self.browser{browser.set_picker(picker.pattern(),picker.options.multiple,picker.options.kind.folders());}
                    post(self.hwnd,WM_ACTION,Command::Refresh.id()as usize,0);
                }
            }true},_=>false,
        }
    }
    pub(super) fn picker_accept_view(&mut self){if let Some(picker)=self.picker.as_mut(){picker.edited=false;}self.picker_accept();}
    pub(super) fn picker_accept(&mut self){
        let Some(picker)=self.picker.as_ref()else{return;};
        if picker.submission.waiting{return;}
        if self.pending.is_some()||self.queued.is_some(){self.show_message(zsdesk_core::i18n::text("文件夹正在加载，请稍后选择").into());return;}
        let kind=picker.options.kind;
        let mut paths=if picker.edited{let text=control_text(picker.name);if kind.folders()&&text.trim().is_empty(){Vec::new()}else{match parse_names(&text,self.current_location().as_path()){Ok(paths)=>paths,Err(error)=>{self.show_message(error);return;}}}}
            else{self.browser.as_ref().map(|browser|browser.selection_paths_limit(257).into_iter().map(PathBuf::from).collect()).unwrap_or_default()};
        // Double clicking a folder still navigates through the ordinary Shell
        // view. Only the explicit footer action selects it in folder mode.
        if !kind.folders()&&paths.len()==1&&paths[0].is_dir(){if let Some(picker)=self.picker.as_mut(){picker.edited=false;}unsafe{self.navigate_to(ShellLocation::Path(paths[0].clone()));}return;}
        if kind.folders()&&paths.is_empty(){if let Some(path)=self.current_location().as_path(){paths.push(path.to_path_buf());}}
        let Some(picker)=self.picker.as_ref()else{return;};
        if let Err(error)=validate_paths(&paths,picker.options.multiple,kind){self.show_message(error);return;}
        self.picker_submit(Outcome::Selected{paths,filter:picker.options.filter});
    }
}
fn control_text(window:HWND)->String{let mut value=vec![0u16;32768];let len=unsafe{GetWindowTextW(window,&mut value)}.max(0)as usize;String::from_utf16_lossy(&value[..len])}
fn parse_names(text:&str,folder:Option<&Path>)->Result<Vec<PathBuf>,String>{
    let text=text.trim();if text.is_empty(){return Err(zsdesk_core::i18n::text("请选择文件").into());}
    let resolve=|value:&str|->Result<PathBuf,String>{let path=PathBuf::from(value);if path.is_absolute(){Ok(path)}else{folder.map(|folder|folder.join(path)).ok_or_else(||zsdesk_core::i18n::text("请打开文件夹后选择文件").into())}};
    if !text.contains('"'){let whole=resolve(text)?;if whole.exists(){return Ok(vec![whole]);}}
    let mut quoted=false;let mut part=String::new();let mut names=Vec::new();
    for c in text.chars(){match c{'"'=>quoted=!quoted,c if c.is_whitespace()&&!quoted=>{if !part.is_empty(){names.push(resolve(&part)?);part.clear();}},_=>part.push(c)}}
    if quoted{return Err(zsdesk_core::i18n::text("文件名的引号不完整").into());}if !part.is_empty(){names.push(resolve(&part)?);}Ok(names)
}
pub(crate) fn validate_paths(paths:&[PathBuf],multiple:bool,kind:SelectionKind)->Result<(),String>{
    if paths.is_empty(){return Err(kind.title().into());}
    if !multiple&&paths.len()!=1{return Err(if kind.folders(){zsdesk_core::i18n::text("此窗口每次只能选择一个文件夹")}else{zsdesk_core::i18n::text("此窗口每次只能选择一个文件")}.into());}
    if paths.len()>256{return Err(zsdesk_core::i18n::text("一次最多选择 256 个项目").into());}
    if paths.iter().any(|path|!path.is_absolute()||if kind.folders(){!path.is_dir()}else{!path.is_file()}){return Err(if kind.folders(){zsdesk_core::i18n::text("所选文件夹已移动、不可用或不是文件夹")}else{zsdesk_core::i18n::text("所选文件已移动、不可用或不是普通文件")}.into());}
    if paths.iter().map(|path|path.as_os_str().len()+3).sum::<usize>()>30000{return Err(zsdesk_core::i18n::text("所选文件路径过长，请分批选择").into());}
    Ok(())
}
pub(super) fn pre_translate(slot:&Slot,message:&MSG)->bool{
    if message.message!=WM_KEYDOWN{return false;}
    let Ok(slot)=slot.try_borrow()else{return false;};let Some(state)=slot.as_ref()else{return false;};let Some(picker)=&state.picker else{return false;};
    if message.wParam.0==VK_RETURN.0 as usize&&message.hwnd==picker.name{post(state.hwnd,WM_COMMAND,ACCEPT,0);return true;}
    if message.wParam.0==VK_RETURN.0 as usize&&[picker.accept,picker.cancel,picker.original].contains(&message.hwnd){
        let command=if message.hwnd==picker.original{USE_ORIGINAL}else if message.hwnd==picker.cancel{CANCEL}else{ACCEPT};
        post(state.hwnd,WM_COMMAND,command,0);return true;
    }
    if message.wParam.0==VK_TAB.0 as usize&&picker.controls().contains(&message.hwnd){return unsafe{IsDialogMessageW(state.hwnd,message)}.as_bool();}
    if message.wParam.0==VK_ESCAPE.0 as usize&&!state.address_editing{post(state.hwnd,WM_CLOSE,0,0);return true;}false
}
#[cfg(test)]mod tests{
    use super::*;
    #[test]fn selection_waits_for_native_confirmation_and_does_not_resubmit(){
        let selected=Outcome::Selected{paths:vec![PathBuf::from(r"E:\folder")],filter:0};
        let mut submission=Submission::default();
        assert!(submission.begin(&selected));assert!(!submission.confirmed);
        assert!(!submission.begin(&selected),"pending selection must remain single-shot");
        assert!(submission.reply(NativeReply::Waiting).is_none());assert!(!submission.confirmed&&submission.waiting);
        assert!(matches!(submission.reply(NativeReply::Accepted),Some(Outcome::Selected{..})));
        assert!(submission.confirmed);assert!(!submission.begin(&selected));
    }
    #[test]fn cancel_and_rejection_do_not_commit_a_tentative_selection(){
        let selected=Outcome::Selected{paths:vec![PathBuf::from(r"E:\folder")],filter:0};
        let mut submission=Submission::default();assert!(submission.begin(&selected));
        assert!(submission.reply(NativeReply::Rejected).is_none());assert!(!submission.waiting&&!submission.confirmed);
        assert!(submission.begin(&selected));assert!(submission.begin(&Outcome::Cancelled));
        assert!(!submission.begin(&Outcome::Cancelled));assert!(matches!(submission.reply(NativeReply::Cancelled),Some(Outcome::Cancelled)));
        assert!(submission.confirmed);
    }
    #[test]fn filenames_keep_spaces_and_unicode_without_executing_text(){
        let paths=parse_names("\"甲 乙.txt\" \"丙.txt\"",Some(Path::new(r"E:\目录"))).unwrap();
        assert_eq!(paths,vec![PathBuf::from(r"E:\目录\甲 乙.txt"),PathBuf::from(r"E:\目录\丙.txt")]);
        assert!(parse_names("\"unfinished",Some(Path::new(r"E:\目录"))).is_err());
        assert!(validate_paths(&[],true,SelectionKind::Files).is_err());
    }
    #[test]fn picker_inherits_folders_but_never_invents_a_previous_file_selection(){
        let mut saved=ExplorerSession::default();saved.tabs[0].locked=true;
        saved.tabs[0].view=Some(zsdesk_core::model::explorer::ShellViewState{selected:vec![vec![1,2]],focused:Some(vec![1,2]),icon_size:48,..Default::default()});
        let selected=prepare_session(saved.clone(),saved.tabs[0].current());
        assert_eq!(selected.tabs[0].current(),saved.tabs[0].current());assert!(!selected.tabs[0].locked);
        let view=selected.tabs[0].view.as_ref().unwrap();assert!(view.selected.is_empty()&&view.focused.is_none());assert_eq!(view.icon_size,48);
        assert!(!saved.tabs[0].view.as_ref().unwrap().selected.is_empty());
    }
}
