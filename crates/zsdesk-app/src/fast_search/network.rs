//! Scoped share search: no local-volume index, MFT, file IDs or mapped-drive creation.
use super::*;
use std::{cmp::Ordering as Cmp,collections::BinaryHeap,os::windows::fs::MetadataExt};
use windows::Win32::{Foundation::{CloseHandle,HANDLE},
    System::{IO::CancelSynchronousIo,Threading::{GetCurrentThreadId,OpenThread,THREAD_TERMINATE}}};
const MAX_CANDIDATES:usize=100_000;
const MAX_PATH_MEMORY:usize=32*1024*1024;
const MAX_DEPTH:usize=256;
const DRIVE_REMOTE:u32=4;

pub(super) struct IoThread(usize);
impl Drop for IoThread{fn drop(&mut self){let _=unsafe{CloseHandle(HANDLE(self.0 as*mut _))};}}
struct IoScope<'a>{shared:&'a Shared,thread:Arc<IoThread>}
impl<'a> IoScope<'a>{fn enter(shared:&'a Shared)->Result<Self>{
    let handle=unsafe{OpenThread(THREAD_TERMINATE,false,GetCurrentThreadId())}?;
    let thread=Arc::new(IoThread(handle.0 as usize));*shared.network_io.lock().unwrap_or_else(|e|e.into_inner())=Some(thread.clone());
    Ok(Self{shared,thread})
}}
impl Drop for IoScope<'_>{fn drop(&mut self){let mut slot=self.shared.network_io.lock().unwrap_or_else(|e|e.into_inner());
    if slot.as_ref().is_some_and(|thread|Arc::ptr_eq(thread,&self.thread)){slot.take();}
}}
pub(super) fn cancel(shared:&Shared){
    // Hold only this short lifetime lock so the thread cannot finish its share
    // operation and start unrelated local I/O before cancellation is submitted.
    if let Some(thread)=shared.network_io.lock().unwrap_or_else(|e|e.into_inner()).as_ref(){
        let _=unsafe{CancelSynchronousIo(HANDLE(thread.0 as*mut _))};
    }
}
fn remote_kind(path:&Path,drive_type:u32)->bool{
    use std::path::{Component,Prefix};
    match path.components().next(){
        Some(Component::Prefix(prefix))=>match prefix.kind(){
            Prefix::UNC(..)|Prefix::VerbatimUNC(..)=>true,
            Prefix::Disk(_)|Prefix::VerbatimDisk(_)=>path.is_absolute()&&drive_type==DRIVE_REMOTE,
            _=>false,
        },_=>false,
    }
}
pub(crate) fn is_remote(path:&Path)->bool{
    if remote_kind(path,0){return true;}
    let Some(root)=super::native_scan::volume_root(path)else{return false;};
    let name:Vec<_>=root.as_os_str().encode_wide().chain(Some(0)).collect();
    remote_kind(path,unsafe{GetDriveTypeW(PCWSTR(name.as_ptr()))})
}
#[derive(Eq)]struct Hit{path:PathBuf,name:String,size:u64,modified:u64,sort:everything::SearchSort}
impl Hit{fn bytes(&self)->usize{self.path.as_os_str().len()*2+self.name.len()}}
impl Ord for Hit{fn cmp(&self,other:&Self)->Cmp{
    let name=||self.name.cmp(&other.name).then_with(||self.path.cmp(&other.path));
    match self.sort{everything::SearchSort::NameAsc=>name(),everything::SearchSort::NameDesc=>name().reverse(),
        everything::SearchSort::ModifiedDesc=>other.modified.cmp(&self.modified).then_with(name),
        everything::SearchSort::SizeDesc=>other.size.cmp(&self.size).then_with(name)}
}}
impl PartialOrd for Hit{fn partial_cmp(&self,other:&Self)->Option<Cmp>{Some(self.cmp(other))}}
impl PartialEq for Hit{fn eq(&self,other:&Self)->bool{self.cmp(other)==Cmp::Equal}}

pub(super) fn execute(shared:&Shared,sender:&SyncSender<SearchEvent>,request:&Request,root:&Path,pinyin:bool)->Result<()>{
    ensure!(root.is_absolute(), "{}", zsdesk_core::tr_format!("网络搜索需要完整的文件夹路径"));
    let wanted=(request.offset as usize).checked_add(request.limit as usize).context(zsdesk_core::i18n::text("搜索页码无效"))?;
    ensure!(wanted>0&&wanted<=MAX_CANDIDATES, "{}", zsdesk_core::tr_format!("结果页超出范围，请缩小搜索条件"));
    let query=query::Query::parse(&request.query,pinyin)?;let _io=IoScope::enter(shared)?;
    ensure!(shared.current(request.generation)&&!shared.cancelled(),"搜索已取消");
    let metadata=fs::symlink_metadata(root).context(zsdesk_core::i18n::text("无法读取网络文件夹，请检查连接和访问权限"))?;
    ensure!(metadata.is_dir()&&metadata.file_attributes()&0x400==0, "{}", zsdesk_core::tr_format!("搜索位置不是可直接读取的文件夹"));
    let first=fs::read_dir(root).context(zsdesk_core::i18n::text("无法读取网络文件夹，请检查连接和访问权限"))?;
    let mut stack=vec![first];let mut heap=BinaryHeap::<Hit>::new();let(mut memory,mut scanned,mut total,mut denied,mut skipped)=(0usize,0u64,0u64,0u64,0u64);
    let mut last=Instant::now();
    while !stack.is_empty(){
        ensure!(shared.current(request.generation)&&!shared.cancelled(),"搜索已取消");
        let Some(entry)=stack.last_mut().unwrap().next()else{stack.pop();continue;};
        let entry=match entry{Ok(entry)=>entry,Err(_)=>{denied+=1;stack.pop();continue;}};
        let meta=match entry.metadata(){Ok(meta)=>meta,Err(_)=>{denied+=1;continue;}};
        if meta.file_attributes()&0x400!=0{skipped+=1;continue;}
        let directory=meta.is_dir();let path=entry.path();let name=entry.file_name().to_string_lossy().to_lowercase();scanned+=1;
        if query.matches(&name,directory,&path.to_string_lossy().replace('/',"\\").to_lowercase()){
            total+=1;let hit=Hit{path:path.clone(),name,size:meta.file_size(),modified:meta.last_write_time(),sort:request.sort};
            if heap.len()<wanted||heap.peek().is_some_and(|worst|hit<*worst){
                if heap.len()==wanted{memory=memory.saturating_sub(heap.pop().unwrap().bytes());}
                memory=memory.saturating_add(hit.bytes());ensure!(memory<=MAX_PATH_MEMORY, "{}", zsdesk_core::tr_format!("结果路径较多，请缩小搜索范围"));heap.push(hit);
            }
        }
        if directory{
            if stack.len()>=MAX_DEPTH{denied+=1;}else{match fs::read_dir(path){Ok(children)=>stack.push(children),Err(_)=>denied+=1}}
        }
        if last.elapsed()>=Duration::from_millis(250){
            emit(shared,sender,SearchEvent{generation:request.generation,paths:Vec::new(),scanned,indexed:total,denied,skipped_reparse:skipped,
                scanning:true,finished:false,truncated:false,error:None,reset:false,backend:BackendKind::DirectoryScan,phase:IndexPhase::Scanning,volumes:Vec::new(),read_only:false});
            last=Instant::now();
        }
    }
    let paths=heap.into_sorted_vec().into_iter().skip(request.offset as usize).map(|hit|hit.path).collect::<Vec<_>>();
    let warning=(denied>0).then(||zsdesk_core::tr_format!("有 {denied} 个目录或项目无法读取，其余结果已显示" , denied=denied));
    let chunks=paths.len().max(1).div_ceil(64);
    for batch in 0..chunks{let from=(batch*64).min(paths.len());let to=(from+64).min(paths.len());
        emit(shared,sender,SearchEvent{generation:request.generation,paths:paths[from..to].to_vec(),scanned,indexed:total,denied,skipped_reparse:skipped,
            scanning:false,finished:batch+1==chunks,truncated:u64::from(request.offset)+(paths.len()as u64)<total,error:warning.clone(),reset:batch==0,
            backend:BackendKind::DirectoryScan,phase:IndexPhase::Ready,volumes:Vec::new(),read_only:false});
    }
    Ok(())
}

#[cfg(test)]mod tests{
    use super::*;
    #[test]fn unc_and_remote_drive_scopes_do_not_depend_on_local_fixed_roots(){
        for path in [r"\\server\share\folder",r"\\?\UNC\server\share\folder"]{assert!(remote_kind(Path::new(path),0));}
        assert!(remote_kind(Path::new(r"Z:\folder"),DRIVE_REMOTE));
        assert!(!remote_kind(Path::new(r"C:\folder"),3));assert!(!remote_kind(Path::new(r"Z:relative"),DRIVE_REMOTE));
    }
    #[test]fn bounded_heap_returns_the_requested_sorted_page(){
        for sort in [everything::SearchSort::NameAsc,everything::SearchSort::NameDesc,everything::SearchSort::SizeDesc,everything::SearchSort::ModifiedDesc]{
            let mut heap=BinaryHeap::new();for index in (0..100).rev(){let hit=Hit{path:format!(r"Z:\{index:03}.txt").into(),name:format!("{index:03}"),size:index,modified:index,sort};
                if heap.len()<7{heap.push(hit);}else if hit<*heap.peek().unwrap(){heap.pop();heap.push(hit);}}
            let rows=heap.into_sorted_vec().into_iter().skip(5).map(|hit|hit.size).collect::<Vec<_>>();
            assert_eq!(rows,if sort==everything::SearchSort::NameAsc{vec![5,6]}else{vec![94,93]});
        }
    }
}
