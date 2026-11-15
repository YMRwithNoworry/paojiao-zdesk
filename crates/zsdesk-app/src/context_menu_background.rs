//! A real namespace view owns virtual-folder background verbs in the worker.
use super::*;
use windows::core::{Interface,IUnknown,HRESULT};
use std::{cell::{Cell,RefCell},collections::HashMap,rc::Rc};
use windows::{Win32::{Foundation::RECT,System::Com::{CoCreateInstance,CLSCTX_INPROC_SERVER},UI::Shell::{
    IExplorerBrowser,IShellView,IShellItem,IShellItemArray,IFolderView2,IPersistFolder2,IShellFolder,IResultsFolder,IEnumIDList,
    ExplorerBrowser,EBO_NOBORDER,EBO_NOPERSISTVIEWSTATE,EBO_NAVIGATEONCE,EBF_NODROPTARGET,SICHINT_CANONICAL,
    SHCreateItemFromIDList,SHCreateItemWithParent,SIGDN_DESKTOPABSOLUTEPARSING,SVGIO_ALLVIEW,
    SVSI_SELECT,SVSI_DESELECTOTHERS,SVSI_NOTAKEFOCUS,SVSI_FOCUSED,SVSI_SELECTIONMARK,ILGetSize,Common::ITEMIDLIST}}};

fn view_matches(view:&IShellView,item:&IShellItem)->bool{
    let Ok(view)=view.cast::<IFolderView2>()else{return false;};
    if let Ok(current)=unsafe{view.GetFolder::<IShellItem>()}{
        if unsafe{current.Compare(item,SICHINT_CANONICAL.0 as u32)}.is_ok_and(|order|order==0){return true;}
    }
    let Ok(folder)=(unsafe{view.GetFolder::<IPersistFolder2>()})else{return false;};
    let Ok(pidl)=(unsafe{folder.GetCurFolder()})else{return false;};
    let current=unsafe{SHCreateItemFromIDList::<IShellItem>(pidl)};
    unsafe{windows::Win32::System::Com::CoTaskMemFree(Some(pidl.cast()));}
    current.is_ok_and(|current|unsafe{current.Compare(item,SICHINT_CANONICAL.0 as u32)}.is_ok_and(|order|order==0))
}

thread_local!{
    static CACHE:RefCell<Option<(IShellItem,Rc<BackgroundView>)>>=const{RefCell::new(None)};
    static SELECTION_CACHE:RefCell<Option<(SelectionNamespace,Rc<BackgroundView>)>>=const{RefCell::new(None)};
    static REUSED:Cell<bool>=const{Cell::new(false)};
}
pub(super) struct CacheLifetime;
impl Drop for CacheLifetime{fn drop(&mut self){
    let cached=CACHE.with(|cache|cache.borrow_mut().take());
    let selected=SELECTION_CACHE.with(|cache|cache.borrow_mut().take());
    drop(selected);drop(cached);
}}
pub(super) fn reused()->bool{REUSED.with(Cell::get)}

pub(super) struct BackgroundView { pub browser:IExplorerBrowser,pub view:IShellView,window:HWND }
impl BackgroundView {
    pub fn create(item:&IShellItem,request:&Request,events:&EventSet)->Result<Rc<Self>> {
        REUSED.with(|reused|reused.set(false));
        let cached=CACHE.with(|cache|cache.borrow().clone());
        if let Some((previous,host))=cached{
            if current_view(&host)&&view_matches(&host.view,item)&&unsafe{previous.Compare(item,SICHINT_CANONICAL.0 as u32)}.is_ok_and(|order|order==0){
                // Reuse the browser's actual current view. Its Shell change
                // notifications and each new menu query update folder/command
                // state without re-enumerating the folder on every right click.
                REUSED.with(|reused|reused.set(true));return Ok(host);
            }
        }
        let window=unsafe{CreateWindowExW(WS_EX_TOOLWINDOW|WS_EX_NOACTIVATE,w!("STATIC"),w!(""),WS_POPUP,0,0,640,480,None,None,None,None)}?;
        let result=(||->Result<(IExplorerBrowser,IShellView)>{
            let browser:IExplorerBrowser=unsafe{CoCreateInstance(&ExplorerBrowser,None,CLSCTX_INPROC_SERVER)}?;
            let result=(||->Result<IShellView>{
                unsafe{browser.SetOptions(EBO_NOBORDER|EBO_NOPERSISTVIEWSTATE)?;
                    browser.Initialize(window,&RECT{left:0,top:0,right:640,bottom:480},None)?;browser.BrowseToObject(item,0)?;}
                let deadline=Instant::now()+Duration::from_secs(5);
                loop {
                    // IExplorerBrowser may expose its initial view before the
                    // requested browse completes. Bind only after the actual
                    // view folder identity matches this request.
                    if let Ok(view)=unsafe{browser.GetCurrentView::<IShellView>()}{if view_matches(&view,item){return Ok(view);}}
                    ensure!(owner_alive(request)&&!EventSet::signaled(events.cancel),"背景菜单已取消");
                    ensure!(Instant::now()<deadline,"背景菜单视图加载超时");pump(10);
                }
            })();
            match result{Ok(view)=>Ok((browser,view)),Err(error)=>{let _=unsafe{browser.Destroy()};Err(error)}}
        })();
        match result{Ok((browser,view))=>{
            let host=Rc::new(Self{browser,view,window});
            let previous=CACHE.with(|cache|cache.borrow_mut().replace((item.clone(),host.clone())));drop(previous);Ok(host)
        },Err(error)=>{let _=unsafe{DestroyWindow(window)};Err(error)}}
    }
}
impl Drop for BackgroundView {fn drop(&mut self){let _=unsafe{self.browser.Destroy()};let _=unsafe{DestroyWindow(self.window)};}}

/// The source view's namespace is independent of the selected items' parents.
/// In particular, a search result must remain a ResultsFolder even when its
/// current selection happens to contain items from only one disk directory.
pub(super) enum SelectionOrigin<'a>{Folder(&'a [u8]),Results,Unspecified}

#[derive(Clone)]
enum SelectionNamespace{Folder(IShellItem),Results}

const SELECTION_DEADLINE:Duration=Duration::from_secs(8);

fn checkpoint(request:&Request,events:&EventSet,deadline:Instant)->Result<()>{
    if !owner_alive(request)||EventSet::signaled(events.cancel){
        return Err(windows::core::Error::new(HRESULT(0x800704c7u32 as i32),"菜单选区准备已取消").into());
    }
    if Instant::now()>=deadline{
        return Err(windows::core::Error::new(HRESULT(0x800705b4u32 as i32),"菜单选区准备超时，未执行命令").into());
    }
    Ok(())
}

fn same_item(first:&IShellItem,second:&IShellItem)->bool{
    unsafe{first.Compare(second,SICHINT_CANONICAL.0 as u32)}.is_ok_and(|order|order==0)
}

fn materializing(error:&windows::core::Error)->bool{
    matches!(error.code().0 as u32,0x8000000a|0x80004005|0x80070490|0x80070057)
}

fn current_view(host:&BackgroundView)->bool{
    if !unsafe{IsWindow(Some(host.window))}.as_bool(){return false;}
    let Ok(current)=(unsafe{host.browser.GetCurrentView::<IShellView>()})else{return false;};
    match (current.cast::<IUnknown>(),host.view.cast::<IUnknown>()){
        (Ok(current),Ok(expected))=>current==expected,_=>false,
    }
}

fn namespace_matches(view:&IShellView,namespace:&SelectionNamespace)->bool{
    match namespace{
        SelectionNamespace::Folder(item)=>view_matches(view,item),
        SelectionNamespace::Results=>view.cast::<IFolderView2>().and_then(|view|unsafe{view.GetFolder::<IResultsFolder>()}).is_ok(),
    }
}

/// A parsing name is only a lookup accelerator. It is never converted into a
/// filesystem path and never establishes identity without canonical comparison.
fn parsing_key(item:&IShellItem)->Option<Vec<u16>>{
    let value=unsafe{item.GetDisplayName(SIGDN_DESKTOPABSOLUTEPARSING)}.ok()?;
    if value.is_null(){return None;}
    let raw=unsafe{value.as_wide()};
    let key=(raw.len()<=32768).then(||raw.to_vec());
    unsafe{windows::Win32::System::Com::CoTaskMemFree(Some(value.0.cast()))};
    key.map(|key|match String::from_utf16(&key){
        Ok(text)=>text.to_lowercase().encode_utf16().collect(),
        Err(_)=>key.into_iter().map(|unit|if (65..=90).contains(&unit){unit+32}else{unit}).collect(),
    })
}

struct SelectionSet{
    items:Vec<IShellItem>,
    by_pidl:HashMap<Vec<u8>,Vec<usize>>,
    by_name:HashMap<Vec<u16>,Vec<usize>>,
}
impl SelectionSet{
    fn new(pidls:&[Vec<u8>],request:&Request,events:&EventSet,deadline:Instant)->Result<Self>{
        ensure!(!pidls.is_empty()&&pidls.len()<=MAX_ITEMS,"菜单选区数量无效");
        let mut result=Self{items:Vec::with_capacity(pidls.len()),by_pidl:HashMap::new(),by_name:HashMap::new()};
        let mut bytes=0usize;let mut name_bytes=0usize;
        for (index,pidl) in pidls.iter().enumerate(){
            checkpoint(request,events,deadline)?;
            bytes=bytes.saturating_add(pidl.len());ensure!(bytes<=MAX_REQUEST as usize,"菜单选区标识过大");
            let item=zsdesk_windows::shell::pidl::item_from_pidl(pidl).context("读取菜单选区标识")?;
            ensure!(!result.by_pidl.contains_key(pidl),"菜单选区包含重复项目");
            result.by_pidl.entry(pidl.clone()).or_default().push(index);
            if let Some(key)=parsing_key(&item){
                name_bytes=name_bytes.saturating_add(key.len()*2);ensure!(name_bytes<=MAX_REQUEST as usize,"菜单选区名称过大");
                result.by_name.entry(key).or_default().push(index);
            }
            result.items.push(item);
        }
        Ok(result)
    }

    fn find(&self,item:&IShellItem,used:&[bool],request:&Request,events:&EventSet,deadline:Instant)->Result<Option<usize>>{
        let matches=|index:usize|->Result<bool>{checkpoint(request,events,deadline)?;Ok(!used[index]&&same_item(item,&self.items[index]))};
        if let Some(pidl)=zsdesk_windows::shell::pidl::item_pidl(item){
            // Identical complete absolute PIDLs are already the same identity;
            // provider metadata changes take the canonical-comparison path.
            if let Some(indices)=self.by_pidl.get(&pidl){for &index in indices{if !used[index]{checkpoint(request,events,deadline)?;return Ok(Some(index));}}}
        }
        if let Some(key)=parsing_key(item){
            if let Some(indices)=self.by_name.get(&key){for &index in indices{if matches(index)?{return Ok(Some(index));}}}
        }
        // Some namespaces expose unstable or unavailable parsing names. Keep
        // those opaque providers usable; the deadline bounds this fallback.
        for index in 0..self.items.len(){if matches(index)?{return Ok(Some(index));}}
        Ok(None)
    }

    fn selected(&self,view:&IFolderView2,request:&Request,events:&EventSet,deadline:Instant)->Result<bool>{
        checkpoint(request,events,deadline)?;
        let array:IShellItemArray=match unsafe{view.GetSelection(false)}{
            Ok(array)=>array,Err(error) if materializing(&error)=>return Ok(false),Err(error)=>return Err(error).context("读取菜单视图选区"),
        };
        let count=match unsafe{array.GetCount()}{Ok(count)=>count,Err(error) if materializing(&error)=>return Ok(false),Err(error)=>return Err(error).context("读取菜单选区数量")};
        if count as usize!=self.items.len(){return Ok(false);}
        let mut used=vec![false;self.items.len()];
        for index in 0..self.items.len(){
            let item=match unsafe{array.GetItemAt(index as u32)}{
                Ok(item)=>item,Err(error) if materializing(&error)=>return Ok(false),Err(error)=>return Err(error).context("读取菜单选中项目"),
            };
            let Some(found)=self.find(&item,&used,request,events,deadline)?else{return Ok(false);};used[found]=true;
        }
        checkpoint(request,events,deadline)?;Ok(true)
    }

    fn common_parent(&self,request:&Request,events:&EventSet,deadline:Instant)->Result<Option<IShellItem>>{
        checkpoint(request,events,deadline)?;
        let Ok(first)=(unsafe{self.items[0].GetParent()})else{return Ok(None);};
        for item in self.items.iter().skip(1){
            checkpoint(request,events,deadline)?;
            let Ok(parent)=(unsafe{item.GetParent()})else{return Ok(None);};
            if !same_item(&first,&parent){return Ok(None);}
        }
        Ok(Some(first))
    }
}

fn make_selection_host(namespace:&SelectionNamespace,request:&Request,events:&EventSet,deadline:Instant)->Result<Rc<BackgroundView>>{
    let window=unsafe{CreateWindowExW(WS_EX_TOOLWINDOW|WS_EX_NOACTIVATE,w!("STATIC"),w!(""),WS_POPUP,0,0,640,480,None,None,None,None)}?;
    let result=(||->Result<(IExplorerBrowser,IShellView)>{
        let browser:IExplorerBrowser=unsafe{CoCreateInstance(&ExplorerBrowser,None,CLSCTX_INPROC_SERVER)}?;
        let result=(||->Result<IShellView>{
            let mut options=EBO_NOBORDER|EBO_NOPERSISTVIEWSTATE;
            if matches!(namespace,SelectionNamespace::Results){options|=EBO_NAVIGATEONCE;}
            unsafe{browser.SetOptions(options)?;browser.Initialize(window,&RECT{left:0,top:0,right:640,bottom:480},None)?;}
            match namespace{
                SelectionNamespace::Folder(item)=>unsafe{browser.BrowseToObject(item,0)}?,
                SelectionNamespace::Results=>unsafe{browser.FillFromObject(None::<&IUnknown>,EBF_NODROPTARGET)}?,
            }
            loop{
                checkpoint(request,events,deadline)?;
                if let Ok(view)=unsafe{browser.GetCurrentView::<IShellView>()}{
                    if namespace_matches(&view,namespace){return Ok(view);}
                }
                pump(10);
            }
        })();
        match result{Ok(view)=>Ok((browser,view)),Err(error)=>{let _=unsafe{browser.Destroy()};Err(error)}}
    })();
    match result{
        Ok((browser,view))=>Ok(Rc::new(BackgroundView{browser,view,window})),
        Err(error)=>{let _=unsafe{DestroyWindow(window)};Err(error)},
    }
}

struct ChildPidl(*mut ITEMIDLIST);
impl Drop for ChildPidl{fn drop(&mut self){if !self.0.is_null(){unsafe{windows::Win32::System::Com::CoTaskMemFree(Some(self.0.cast()))};}}}
impl ChildPidl{
    fn bytes(&self)->Result<Vec<u8>>{
        ensure!(!self.0.is_null(),"菜单视图返回空项目标识");
        let length=unsafe{ILGetSize(Some(self.0))}as usize;ensure!((2..=65536).contains(&length),"菜单视图项目标识长度无效");
        let bytes=unsafe{std::slice::from_raw_parts(self.0.cast::<u8>(),length)}.to_vec();
        ensure!(zsdesk_windows::shell::pidl::valid(&bytes),"菜单视图项目标识无效");Ok(bytes)
    }
}

fn last_child(pidl:&[u8])->Option<Vec<u8>>{
    if !zsdesk_windows::shell::pidl::valid(pidl)||pidl.len()==2{return None;}
    let mut at=0;let mut last=0;
    loop{let size=u16::from_le_bytes([pidl[at],pidl[at+1]])as usize;if size==0{break;}last=at;at+=size;}
    Some(pidl[last..].to_vec())
}

fn direct_children(pidls:&[Vec<u8>],selection:&SelectionSet,parent:&IShellItem,request:&Request,events:&EventSet,deadline:Instant)->Result<Option<Vec<Vec<u8>>>>{
    let mut children=Vec::with_capacity(pidls.len());
    for (pidl,item) in pidls.iter().zip(&selection.items){
        checkpoint(request,events,deadline)?;
        let Ok(actual_parent)=(unsafe{item.GetParent()})else{return Ok(None);};
        if !same_item(&actual_parent,parent){return Ok(None);}
        let Some(child)=last_child(pidl)else{return Ok(None);};children.push(child);
    }
    Ok(Some(children))
}

fn fill_results(host:&BackgroundView,pidls:&[Vec<u8>],request:&Request,events:&EventSet,deadline:Instant)->Result<Option<Vec<Vec<u8>>>>{
    let view:IFolderView2=host.view.cast()?;let results:IResultsFolder=unsafe{view.GetFolder()}?;
    unsafe{results.RemoveAll()}?;
    let mut empty_observations=0;
    loop{
        checkpoint(request,events,deadline)?;
        if unsafe{view.ItemCount(SVGIO_ALLVIEW)}.is_ok_and(|count|count==0){empty_observations+=1;}else{empty_observations=0;}
        if empty_observations>=2{break;}pump(10);
    }
    let mut children=Vec::with_capacity(pidls.len());let mut complete=true;
    for (index,pidl) in pidls.iter().enumerate(){
        checkpoint(request,events,deadline)?;let mut raw=std::ptr::null_mut();
        let result=unsafe{results.AddIDList(pidl.as_ptr().cast(),Some(&mut raw))};let added=ChildPidl(raw);
        result.context("填充菜单搜索结果视图")?;
        if added.0.is_null(){complete=false;}else{children.push(added.bytes()?);}
        if index%32==31{pump(0);}
    }
    Ok(complete.then_some(children))
}

fn enumerate_children(view:&IFolderView2,selection:&SelectionSet,request:&Request,events:&EventSet,deadline:Instant)->Result<Option<Vec<Vec<u8>>>>{
    let folder:IShellFolder=unsafe{view.GetFolder()}?;let enumerator:IEnumIDList=unsafe{view.Items(SVGIO_ALLVIEW)}?;
    let mut found=vec![None;selection.items.len()];let mut used=vec![false;found.len()];let mut count=0;let mut walked=0;
    loop{
        checkpoint(request,events,deadline)?;let mut raw=[std::ptr::null_mut()];let mut fetched=0;
        let status=unsafe{enumerator.Next(&mut raw,Some(&mut fetched))};let child=ChildPidl(raw[0]);status.ok()?;
        if fetched==0{break;}
        let item:IShellItem=unsafe{SHCreateItemWithParent(None,&folder,child.0)}?;
        if let Some(index)=selection.find(&item,&used,request,events,deadline)?{found[index]=Some(child.bytes()?);used[index]=true;count+=1;}
        if count==found.len(){return Ok(Some(found.into_iter().map(Option::unwrap).collect()));}
        walked+=1;if walked%64==0{pump(0);}
        if status==HRESULT(1){break;}
    }
    Ok(None)
}

fn apply_selection(host:&BackgroundView,children:&[Vec<u8>],individual:bool)->windows::core::Result<()>{
    let view:IFolderView2=host.view.cast()?;
    let pointers:Vec<*const ITEMIDLIST>=children.iter().map(|pidl|pidl.as_ptr().cast()).collect();
    let flags=(SVSI_SELECT.0|SVSI_DESELECTOTHERS.0|SVSI_NOTAKEFOCUS.0)as u32;
    let individual=if individual{true}else{match unsafe{view.SelectAndPositionItems(pointers.len()as u32,pointers.as_ptr(),None,flags)}{
        Ok(())=>false,Err(error) if matches!(error.code().0 as u32,0x80004001|0x80004002)=>true,Err(error)=>return Err(error),
    }};
    if individual{
        for (index,pidl) in pointers.iter().enumerate(){
            let flags=(SVSI_SELECT.0|SVSI_NOTAKEFOCUS.0|if index==0{SVSI_DESELECTOTHERS.0}else{0})as u32;
            unsafe{host.view.SelectItem(*pidl,flags)}?;
        }
    }
    if let Some(first)=pointers.first(){
        unsafe{host.view.SelectItem(*first,(SVSI_SELECT.0|SVSI_FOCUSED.0|SVSI_SELECTIONMARK.0|SVSI_NOTAKEFOCUS.0)as u32)}?;
    }
    Ok(())
}

/// Returns a real, populated native view with the exact immutable request
/// selection. No private object fields or filesystem-path substitutes are used.
/// The caller must keep this Rc alive through menu construction and invocation,
/// and must not refresh or replace its selection after this function returns.
pub(super) fn for_selection(pidls:&[Vec<u8>],origin:SelectionOrigin<'_>,request:&Request,events:&EventSet)->Result<Rc<BackgroundView>>{
    REUSED.with(|reused|reused.set(false));let deadline=Instant::now()+SELECTION_DEADLINE;
    let result=(||->Result<Rc<BackgroundView>>{
        checkpoint(request,events,deadline)?;let selection=SelectionSet::new(pidls,request,events,deadline)?;
        let namespace=match origin{
            SelectionOrigin::Folder(pidl)=>SelectionNamespace::Folder(zsdesk_windows::shell::pidl::item_from_pidl(pidl).context("读取源文件夹标识")?),
            SelectionOrigin::Results=>SelectionNamespace::Results,
            SelectionOrigin::Unspecified=>match selection.common_parent(request,events,deadline)?{Some(parent)=>SelectionNamespace::Folder(parent),None=>SelectionNamespace::Results},
        };
        let cached=SELECTION_CACHE.with(|cache|cache.borrow().clone());
        let host=if let Some((previous,host))=cached.filter(|(previous,host)|{
            current_view(host)&&namespace_matches(&host.view,&namespace)&&match (previous,&namespace){
                (SelectionNamespace::Folder(previous),SelectionNamespace::Folder(current))=>same_item(previous,current),
                (SelectionNamespace::Results,SelectionNamespace::Results)=>true,_=>false,
            }
        }){let _=previous;REUSED.with(|reused|reused.set(true));host}else{
            let host=make_selection_host(&namespace,request,events,deadline)?;
            let old=SELECTION_CACHE.with(|cache|cache.borrow_mut().replace((namespace.clone(),host.clone())));drop(old);host
        };
        // Layout applies before selection, so restoration cannot later clear a
        // selection which the menu has already accepted as ready.
        if let Some(layout)=&request.layout{zsdesk_windows::shell::restore_view_layout(&host.view,layout);}
        let mut children=match &namespace{
            SelectionNamespace::Folder(parent)=>direct_children(pidls,&selection,parent,request,events,deadline)?,
            SelectionNamespace::Results=>fill_results(&host,pidls,request,events,deadline)?,
        };
        let folder:IFolderView2=host.view.cast()?;let mut stable=0;let mut retry_at=Instant::now();let mut last_error=None;let mut attempts=0;let mut native_children=false;
        loop{
            if let Err(error)=checkpoint(request,events,deadline){
                eprintln!("menu-selection-unready expected={} selected={:?} items={:?} attempts={attempts} native_children={native_children}",pidls.len(),unsafe{folder.ItemCount(windows::Win32::UI::Shell::SVGIO_SELECTION)},unsafe{folder.ItemCount(SVGIO_ALLVIEW)});
                return Err(if let Some(last)=last_error{error.context(format!("最近的视图错误：{last}"))}else{error});}
            ensure!(current_view(&host)&&namespace_matches(&host.view,&namespace),"菜单源视图在准备期间发生变化");
            let complete=!matches!(namespace,SelectionNamespace::Results)||unsafe{folder.ItemCount(SVGIO_ALLVIEW)}.is_ok_and(|count|count==pidls.len()as i32);
            if complete&&selection.selected(&folder,request,events,deadline)?{
                stable+=1;
                if stable>=2{checkpoint(request,events,deadline)?;return Ok(host);}
            }else{
                stable=0;
                if Instant::now()>=retry_at{
                    // Absolute PIDLs can contain cached namespace attributes.
                    // A successful selection call does not prove that this
                    // new view accepted those relative identities. Resolve its
                    // own children before retrying individual selection.
                    if attempts>=2&&!native_children{children=None;native_children=true;}
                    if children.is_none(){match enumerate_children(&folder,&selection,request,events,deadline){
                        Ok(found)=>children=found,
                        Err(error) if error.downcast_ref::<windows::core::Error>().is_some_and(materializing)=>last_error=Some(error.to_string()),
                        Err(error)=>return Err(error),
                    }}
                    if let Some(children)=&children{if let Err(error)=apply_selection(&host,children,native_children){
                        if !materializing(&error){return Err(error).context("设置菜单视图选区");}last_error=Some(error.to_string());
                    }}
                    attempts+=1;
                    retry_at=Instant::now()+Duration::from_millis(100);
                }
            }
            pump(20);
        }
    })();
    if result.is_err(){let old=SELECTION_CACHE.with(|cache|cache.borrow_mut().take());drop(old);}
    result
}
