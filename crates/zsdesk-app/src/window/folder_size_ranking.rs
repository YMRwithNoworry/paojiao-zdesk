//! Read-only folder occupancy ranking, independent of Shell's file-size sort.
use std::{cell::RefCell,path::{Path,PathBuf},sync::{Arc,mpsc,atomic::{AtomicBool,Ordering}},thread};
use anyhow::{Context,Result,ensure};
use windows::{core::{w,PCWSTR,PWSTR},Win32::{Foundation::{HWND,HINSTANCE,LPARAM,LRESULT,WPARAM,RECT},
    Graphics::Gdi::{GetStockObject,DEFAULT_GUI_FONT,InvalidateRect,GetSysColorBrush,COLOR_WINDOW},System::LibraryLoader::GetModuleHandleW,
    UI::{Controls::*,WindowsAndMessaging::*}}};
use super::super::{pane_input,ShellLocation};

const CLASS:PCWSTR=w!("ZSDesk.FolderOccupancy");
const LIMIT:usize=10_000;
const TIMER:usize=1;
const SELECTION_STATE:LIST_VIEW_ITEM_STATE_FLAGS=LIST_VIEW_ITEM_STATE_FLAGS(LVIS_SELECTED.0|LVIS_FOCUSED.0);
#[derive(Clone,Debug)]struct Row{path:PathBuf,name:String,size:Option<u64>}
#[derive(Clone,Copy,Debug,PartialEq,Eq)]enum Column{Name,Size}
enum Update{Rows(Vec<Row>,bool),Sizes(Vec<(PathBuf,Option<u64>)>),Done,Error(String)}
struct State{
    owner:HWND,identity:pane_input::Identity,list:HWND,status:HWND,
    rows:Vec<Row>,order:Vec<usize>,column:Column,ascending:bool,read:usize,truncated:bool,
    updates:mpsc::Receiver<Update>,stop:Arc<AtomicBool>,worker:Option<thread::JoinHandle<()>>,
}
impl Drop for State{fn drop(&mut self){self.stop.store(true,Ordering::Release);
    if self.worker.as_ref().is_some_and(thread::JoinHandle::is_finished){if let Some(worker)=self.worker.take(){let _=worker.join();}}
}}
type Slot=RefCell<State>;
fn wide(value:&str)->Vec<u16>{value.encode_utf16().chain(Some(0)).collect()}
fn sorted(rows:&[Row],column:Column,ascending:bool)->Vec<usize>{
    let mut order:Vec<_>=(0..rows.len()).collect();
    order.sort_by(|&a,&b|{
        let name=||rows[a].name.to_lowercase().cmp(&rows[b].name.to_lowercase()).then_with(||rows[a].path.cmp(&rows[b].path));
        if column==Column::Name{return if ascending{name()}else{name().reverse()};}
        match (rows[a].size,rows[b].size){
            (Some(a),Some(b))=>(if ascending{a.cmp(&b)}else{b.cmp(&a)}).then_with(name),
            (Some(_),None)=>std::cmp::Ordering::Less,(None,Some(_))=>std::cmp::Ordering::Greater,(None,None)=>name(),
        }
    });order
}
fn bytes(value:u64)->String{
    let digits=value.to_string();let mut out=String::with_capacity(digits.len()+digits.len()/3+7);
    for (index,c) in digits.chars().enumerate(){if index>0&&(digits.len()-index)%3==0{out.push(',');}out.push(c);}
    out.push_str(zsdesk_core::i18n::text(" 字节"));out
}
impl State{
    fn message(&self,text:&str){let _=unsafe{SetWindowTextW(self.status,PCWSTR(wide(text).as_ptr()))};}
    fn reorder(&mut self){
        let selected=unsafe{SendMessageW(self.list,LVM_GETNEXTITEM,Some(WPARAM(usize::MAX)),Some(LPARAM(LVNI_SELECTED as isize)))}.0;
        let selected=(selected>=0).then(||self.order.get(selected as usize).copied()).flatten();
        self.order=sorted(&self.rows,self.column,self.ascending);
        unsafe{SendMessageW(self.list,LVM_SETITEMCOUNT,Some(WPARAM(self.order.len())),Some(LPARAM(0)));}
        let clear=LVITEMW{state:LIST_VIEW_ITEM_STATE_FLAGS(0),stateMask:SELECTION_STATE,..Default::default()};
        unsafe{SendMessageW(self.list,LVM_SETITEMSTATE,Some(WPARAM(usize::MAX)),Some(LPARAM((&clear as*const LVITEMW)as isize)));}
        if let Some(position)=selected.and_then(|id|self.order.iter().position(|value|*value==id)){
            let item=LVITEMW{state:SELECTION_STATE,stateMask:SELECTION_STATE,..Default::default()};
            unsafe{SendMessageW(self.list,LVM_SETITEMSTATE,Some(WPARAM(position)),Some(LPARAM((&item as*const LVITEMW)as isize)));}
        }
        let _=unsafe{InvalidateRect(Some(self.list),None,false)};
    }
    fn poll(&mut self){
        let mut changed=false;
        for _ in 0..16 {let Ok(update)=self.updates.try_recv()else{break;};match update{
            Update::Rows(rows,truncated)=>{self.rows=rows;self.truncated=truncated;self.read=0;changed=true;
                self.message(&zsdesk_core::tr_format!("正在读取 {} 个文件夹的总大小…", self.rows.len()));},
            Update::Sizes(values)=>{
                for (path,size) in values{if let Some(row)=self.rows.iter_mut().find(|row|row.path==path){row.size=size;self.read+=1;}}
                changed=true;self.message(&zsdesk_core::tr_format!("已读取 {} / {} 个文件夹；双击打开文件夹", self.read, self.rows.len()));
            },
            Update::Done=>{let known=self.rows.iter().filter(|row|row.size.is_some()).count();
                self.message(&zsdesk_core::tr_format!("{} 个文件夹，{} 项大小未知{}；点击列标题切换排序，双击打开", self.rows.len(), self.rows.len()-known, if self.truncated{zsdesk_core::i18n::text("（仅显示前 10000 项）")}else{""}));},
            Update::Error(error)=>self.message(&zsdesk_core::tr_format!("大小查询未完成：{error}", error=error)),
        }}
        if changed{self.reorder();}
    }
    fn open(&self,index:i32)->bool{
        if index<0{return false;}let Some(row)=self.order.get(index as usize).and_then(|id|self.rows.get(*id))else{return false;};
        if pane_input::identity(self.owner,self.identity.source)!=Some(self.identity){self.message(zsdesk_core::i18n::text("原标签页已切换，请重新打开文件夹总大小窗口"));return false;}
        if !pane_input::post(self.owner,self.identity,pane_input::Intent::NavigateFolder(ShellLocation::Path(row.path.clone()))){self.message(zsdesk_core::i18n::text("文件夹打开请求未送达，请重试"));return false;}
        true
    }
}
unsafe extern "system" fn procedure(hwnd:HWND,message:u32,wp:WPARAM,lp:LPARAM)->LRESULT{
    let pointer=unsafe{GetWindowLongPtrW(hwnd,GWLP_USERDATA)}as*mut Slot;
    if message==WM_NCDESTROY{
        unsafe{SetWindowLongPtrW(hwnd,GWLP_USERDATA,0);}
        if !pointer.is_null(){unsafe{drop(Box::from_raw(pointer));}}
        return unsafe{DefWindowProcW(hwnd,message,wp,lp)};
    }
    if pointer.is_null(){return unsafe{DefWindowProcW(hwnd,message,wp,lp)};}
    let Ok(mut state)=(unsafe{&*pointer}).try_borrow_mut()else{return unsafe{DefWindowProcW(hwnd,message,wp,lp)};};
    match message{
        WM_TIMER if wp.0==TIMER=>{state.poll();return LRESULT(0);},
        WM_SIZE=>{
            let mut bounds=RECT::default();let _=unsafe{GetClientRect(hwnd,&mut bounds)};
            let width=(bounds.right-16).max(20);let height=(bounds.bottom-50).max(20);
            unsafe{let _=MoveWindow(state.list,8,8,width,height,true);let _=MoveWindow(state.status,8,height+14,width,30,true);}
            return LRESULT(0);
        },
        WM_NOTIFY if lp.0!=0=>{
            let header=unsafe{&*(lp.0 as*const NMHDR)};
            if header.hwndFrom!=state.list{return LRESULT(0);}
            if header.code==LVN_GETDISPINFOW{
                let info=unsafe{&mut*(lp.0 as*mut NMLVDISPINFOW)};
                if info.item.mask.0&LVIF_TEXT.0!=0&&info.item.iItem>=0&&!info.item.pszText.is_null()&&info.item.cchTextMax>0{
                    if let Some(row)=state.order.get(info.item.iItem as usize).and_then(|id|state.rows.get(*id)){
                        let text=if info.item.iSubItem==0{row.name.clone()}else{row.size.map(bytes).unwrap_or_else(||zsdesk_core::i18n::text("大小未知").into())};
                        let units:Vec<_>=text.encode_utf16().collect();let count=units.len().min(info.item.cchTextMax as usize-1);
                        unsafe{std::ptr::copy_nonoverlapping(units.as_ptr(),info.item.pszText.0,count);*info.item.pszText.0.add(count)=0;}
                    }
                }return LRESULT(0);
            }
            if header.code==LVN_COLUMNCLICK{
                let notification=unsafe{&*(lp.0 as*const NMLISTVIEW)};
                let column=if notification.iSubItem==0{Column::Name}else{Column::Size};
                state.ascending=if state.column==column{!state.ascending}else{column==Column::Name};state.column=column;state.reorder();return LRESULT(0);
            }
            if header.code==NM_DBLCLK{
                if state.open(unsafe{(*(lp.0 as*const NMITEMACTIVATE)).iItem}){let _=unsafe{PostMessageW(Some(hwnd),WM_CLOSE,WPARAM(0),LPARAM(0))};}
                return LRESULT(0);
            }
            if header.code==LVN_KEYDOWN&&unsafe{(*(lp.0 as*const NMLVKEYDOWN)).wVKey}==13{
                let selected=unsafe{SendMessageW(state.list,LVM_GETNEXTITEM,Some(WPARAM(usize::MAX)),Some(LPARAM(LVNI_SELECTED as isize)))}.0;
                if state.open(selected as i32){let _=unsafe{PostMessageW(Some(hwnd),WM_CLOSE,WPARAM(0),LPARAM(0))};}return LRESULT(0);
            }
        },
        _=>{},
    }
    drop(state);unsafe{DefWindowProcW(hwnd,message,wp,lp)}
}
fn gather(profile:&Path,directory:&Path,stop:&AtomicBool,send:&mpsc::SyncSender<Update>)->Result<()>{
    let mut rows=Vec::new();let mut truncated=false;
    for entry in std::fs::read_dir(directory).context(zsdesk_core::i18n::text("无法读取当前文件夹"))?{
        if stop.load(Ordering::Acquire){return Ok(());}
        let entry=entry?;if !entry.file_type()?.is_dir(){continue;}
        if rows.len()==LIMIT{truncated=true;break;}
        rows.push(Row{path:entry.path(),name:entry.file_name().to_string_lossy().into_owned(),size:None});
    }
    let paths:Vec<_>=rows.iter().map(|row|row.path.clone()).collect();
    if send.send(Update::Rows(rows,truncated)).is_err(){return Ok(());}
    let items:Vec<_>=paths.iter().filter_map(|path|path.file_name().map(|name|crate::folder_size_data::Item{name:name.to_os_string()})).collect();
    crate::folder_size_data::calculate(profile,directory,&items,&||stop.load(Ordering::Acquire),|index,value|{
        !stop.load(Ordering::Acquire)&&send.send(Update::Sizes(vec![(paths[index].clone(),value.bytes)])).is_ok()
    })?;
    let _=send.send(Update::Done);Ok(())
}
pub(super) fn show(owner:HWND,identity:pane_input::Identity,profile:&Path,directory:&Path)->Result<()>{
    ensure!(directory.is_absolute(), "{}", zsdesk_core::tr_format!("文件夹路径无效"));
    create(owner,identity,profile,directory,true,true).map(|_|())
}
fn create(owner:HWND,identity:pane_input::Identity,profile:&Path,directory:&Path,query:bool,activate:bool)->Result<HWND>{
    unsafe{InitCommonControlsEx(&INITCOMMONCONTROLSEX{dwSize:std::mem::size_of::<INITCOMMONCONTROLSEX>()as u32,dwICC:ICC_LISTVIEW_CLASSES})}.ok()?;
    let instance=HINSTANCE(unsafe{GetModuleHandleW(None)?}.0);
    unsafe{RegisterClassW(&WNDCLASSW{hInstance:instance,lpfnWndProc:Some(procedure),lpszClassName:CLASS,hCursor:LoadCursorW(None,IDC_ARROW)?,hbrBackground:GetSysColorBrush(COLOR_WINDOW),..Default::default()});}
    let title=wide(&zsdesk_core::tr_format!("文件夹总大小 — {}", directory.display()));let mut bounds=RECT::default();let _=unsafe{GetWindowRect(owner,&mut bounds)};
    let hwnd=unsafe{CreateWindowExW(WS_EX_TOOLWINDOW,CLASS,PCWSTR(title.as_ptr()),WS_OVERLAPPEDWINDOW|WS_CLIPCHILDREN,
        bounds.left+60,bounds.top+60,760,580,Some(owner),None,Some(instance),None)}?;
    let setup=(||->Result<()>{
        let list=unsafe{CreateWindowExW(WS_EX_CLIENTEDGE,WC_LISTVIEWW,PCWSTR::null(),WS_CHILD|WS_VISIBLE|WS_TABSTOP|WINDOW_STYLE(LVS_REPORT|LVS_OWNERDATA|LVS_SINGLESEL|LVS_SHOWSELALWAYS),
            8,8,720,480,Some(hwnd),Some(HMENU(1usize as*mut _)),Some(instance),None)}?;
        let status=unsafe{CreateWindowExW(WINDOW_EX_STYLE::default(),w!("STATIC"),windows::core::PCWSTR(zsdesk_core::i18n::wide("正在读取文件夹…").as_ptr()),WS_CHILD|WS_VISIBLE,8,500,720,30,Some(hwnd),None,Some(instance),None)}?;
        let font=unsafe{GetStockObject(DEFAULT_GUI_FONT)};
        for control in [list,status]{unsafe{SendMessageW(control,WM_SETFONT,Some(WPARAM(font.0 as usize)),Some(LPARAM(1)));}}
        unsafe{SendMessageW(list,LVM_SETEXTENDEDLISTVIEWSTYLE,None,Some(LPARAM((LVS_EX_FULLROWSELECT|LVS_EX_DOUBLEBUFFER)as isize)));}
        for (index,label,width) in [(0,zsdesk_core::i18n::text("文件夹"),430),(1,zsdesk_core::i18n::text("总大小"),260)] {
            let mut text=wide(label);let column=LVCOLUMNW{mask:LVCF_TEXT|LVCF_WIDTH|LVCF_SUBITEM,pszText:PWSTR(text.as_mut_ptr()),cx:width,iSubItem:index,..Default::default()};
            unsafe{SendMessageW(list,LVM_INSERTCOLUMNW,Some(WPARAM(index as usize)),Some(LPARAM((&column as*const LVCOLUMNW)as isize)));}
        }
        let (send,updates)=mpsc::sync_channel(4);let stop=Arc::new(AtomicBool::new(false));let worker_stop=stop.clone();let profile=profile.to_path_buf();let directory=directory.to_path_buf();
        let worker=if query{Some(thread::Builder::new().name("zsdesk-folder-occupancy".into()).spawn(move||{
            if let Err(error)=gather(&profile,&directory,&worker_stop,&send){if !worker_stop.load(Ordering::Acquire){let _=send.send(Update::Error(format!("{error:#}")));}}
        })?)}else{None};
        let state=Box::new(RefCell::new(State{owner,identity,list,status,rows:Vec::new(),order:Vec::new(),column:Column::Size,ascending:false,read:0,truncated:false,updates,stop,worker}));
        unsafe{SetWindowLongPtrW(hwnd,GWLP_USERDATA,Box::into_raw(state)as isize);SetTimer(Some(hwnd),TIMER,100,None);SendMessageW(hwnd,WM_SIZE,None,None);let _=ShowWindow(hwnd,if activate{SW_SHOWNORMAL}else{SW_SHOWNOACTIVATE});}
        Ok(())
    })();
    if setup.is_err(){let _=unsafe{DestroyWindow(hwnd)};}setup.map(|_|hwnd)
}

#[cfg(test)]mod tests{
    use super::*;
    #[test]fn occupancy_order_uses_unsigned_sizes_and_keeps_unknown_last(){
        let row=|name:&str,size|Row{path:PathBuf::from(name),name:name.into(),size};
        let rows=vec![row("a",Some(u64::MAX)),row("b",Some(1)),row("c",None),row("d",Some(0)),row("e",Some(u32::MAX as u64+1))];
        assert_eq!(sorted(&rows,Column::Size,true),vec![3,1,4,0,2]);assert_eq!(sorted(&rows,Column::Size,false),vec![0,4,1,3,2]);
        assert_eq!(bytes(u64::MAX),"18,446,744,073,709,551,615 字节");assert_eq!(bytes(0),"0 字节");
        assert_eq!(sorted(&rows,Column::Name,true),vec![0,1,2,3,4]);
    }
    #[test]
    #[ignore="Offscreen native folder occupancy list; requires ZSDESK_NATIVE_TEST_OUTPUT; no engine, clipboard or physical input"]
    fn native_folder_occupancy_list_sorts_exact_sizes_and_preserves_selection()->Result<()>{
        let root=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("native output")?);
        ensure!(root.is_absolute(),"absolute output required");std::fs::create_dir_all(&root)?;
        struct Owned(HWND);impl Drop for Owned{fn drop(&mut self){let _=unsafe{DestroyWindow(self.0)};}}
        let before=unsafe{GetForegroundWindow()};
        let owner=Owned(unsafe{CreateWindowExW(WS_EX_NOACTIVATE|WS_EX_TOOLWINDOW,w!("STATIC"),w!("Occupancy regression"),WS_POPUP,
            -28000,-28000,100,100,None,None,None,None)}?);
        let identity=pane_input::Identity{source:1,tab:1,epoch:1};
        let window=Owned(create(owner.0,identity,&root.join("unused-profile.json"),&root,false,false)?);
        let slot=unsafe{&*(GetWindowLongPtrW(window.0,GWLP_USERDATA)as*const Slot)};
        let row=|name:&str,size|Row{path:root.join(name),name:name.into(),size};
        {let mut state=slot.borrow_mut();state.rows=vec![row("甲",Some(u64::MAX)),row("乙",Some(1)),row("未知",None),row("零",Some(0))];state.reorder();}
        let list=slot.borrow().list;
        ensure!(unsafe{SendMessageW(list,LVM_GETITEMCOUNT,None,None)}.0==4,"native list item count");
        let item=LVITEMW{state:SELECTION_STATE,stateMask:SELECTION_STATE,..Default::default()};
        unsafe{SendMessageW(list,LVM_SETITEMSTATE,Some(WPARAM(0)),Some(LPARAM((&item as*const LVITEMW)as isize)));}
        let click=NMLISTVIEW{hdr:NMHDR{hwndFrom:list,idFrom:1,code:LVN_COLUMNCLICK},iSubItem:1,..Default::default()};
        unsafe{SendMessageW(window.0,WM_NOTIFY,None,Some(LPARAM((&click as*const NMLISTVIEW)as isize)));}
        ensure!(slot.borrow().order==vec![3,1,0,2],"column header did not change to numeric ascending order");
        let selected=unsafe{SendMessageW(list,LVM_GETNEXTITEM,Some(WPARAM(usize::MAX)),Some(LPARAM(LVNI_SELECTED as isize)))}.0;
        ensure!(selected==2,"sort changed the selected folder identity");
        let mut text=[0u16;128];let mut info=NMLVDISPINFOW{hdr:NMHDR{hwndFrom:list,idFrom:1,code:LVN_GETDISPINFOW},
            item:LVITEMW{mask:LVIF_TEXT,iItem:2,iSubItem:1,pszText:PWSTR(text.as_mut_ptr()),cchTextMax:text.len()as i32,..Default::default()}};
        unsafe{SendMessageW(window.0,WM_NOTIFY,None,Some(LPARAM((&mut info as*mut NMLVDISPINFOW)as isize)));}
        let length=text.iter().position(|c|*c==0).unwrap_or(text.len());ensure!(String::from_utf16_lossy(&text[..length])==bytes(u64::MAX),"native display lost 64-bit precision");
        unsafe{SendMessageW(window.0,WM_NOTIFY,None,Some(LPARAM((&click as*const NMLISTVIEW)as isize)));}
        ensure!(slot.borrow().order==vec![0,1,3,2],"column header did not restore descending order");
        let stop=slot.borrow().stop.clone();drop(window);ensure!(stop.load(Ordering::Acquire),"closing list did not cancel background work");drop(owner);
        let unchanged=before==unsafe{GetForegroundWindow()};ensure!(unchanged,"offscreen regression changed foreground");
        let record=serde_json::json!({"state":"passed","native_header_numeric_sort":true,"unknown_last":true,"exact_u64_text":true,"selected_identity_preserved":true,"close_cancels_worker":true,"foreground_unchanged":unchanged});
        std::fs::write(root.join("folder-occupancy-list.current.json"),serde_json::to_vec_pretty(&record)?)?;println!("{record}");Ok(())
    }
}
