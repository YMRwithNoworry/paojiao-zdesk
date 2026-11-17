//! Cancellable logical file/folder totals, independent of a search index.
use std::{collections::VecDeque,ffi::OsString,fs::{self,ReadDir},path::{Path,PathBuf},
    sync::atomic::{AtomicUsize,Ordering},thread,time::{Duration,Instant}};
use anyhow::{Result,Context,ensure};
use std::os::windows::{ffi::OsStrExt,fs::MetadataExt};
use windows::{core::PCWSTR,Win32::{Foundation::{CloseHandle,HANDLE},Storage::FileSystem::{
    CreateFileW,GetFileInformationByHandle,GetFileInformationByHandleEx,FileAttributeTagInfo,FILE_ATTRIBUTE_TAG_INFO,BY_HANDLE_FILE_INFORMATION,
    FILE_READ_ATTRIBUTES,FILE_SHARE_READ,FILE_SHARE_WRITE,FILE_SHARE_DELETE,OPEN_EXISTING,
    FILE_FLAG_BACKUP_SEMANTICS,FILE_FLAG_OPEN_REPARSE_POINT}}};

#[derive(Clone,Debug)]pub(crate) struct Item{pub name:OsString}
#[derive(Clone,Debug,Default)]pub(crate) struct Value{pub bytes:Option<u64>,pub skipped_links:u64,pub error:Option<String>}
impl Value{
    fn known(bytes:u64)->Self{Self{bytes:Some(bytes),..Default::default()}}
    fn error(error:impl std::fmt::Display)->Self{Self{error:Some(error.to_string().chars().take(512).collect()),..Default::default()}}
}
static SCANNERS:AtomicUsize=AtomicUsize::new(0);
struct Permit;
impl Permit{fn acquire(cancel:&impl Fn()->bool)->Option<Self>{loop{if cancel(){return None;}let count=SCANNERS.load(Ordering::Acquire);
    if count<2&&SCANNERS.compare_exchange(count,count+1,Ordering::AcqRel,Ordering::Acquire).is_ok(){return Some(Self);}
    thread::sleep(Duration::from_millis(20));}}}
impl Drop for Permit{fn drop(&mut self){SCANNERS.fetch_sub(1,Ordering::AcqRel);}}
fn name_surrogate(path:&Path,attributes:u32)->Result<bool>{
    if attributes&0x400==0{return Ok(false);}
    let name:Vec<_>=path.as_os_str().encode_wide().chain(Some(0)).collect();
    let handle=unsafe{CreateFileW(PCWSTR(name.as_ptr()),FILE_READ_ATTRIBUTES.0,FILE_SHARE_READ|FILE_SHARE_WRITE|FILE_SHARE_DELETE,
        None,OPEN_EXISTING,FILE_FLAG_BACKUP_SEMANTICS|FILE_FLAG_OPEN_REPARSE_POINT,None)}?;
    struct File(HANDLE);impl Drop for File{fn drop(&mut self){let _=unsafe{CloseHandle(self.0)};}}let file=File(handle);
    let mut info=FILE_ATTRIBUTE_TAG_INFO::default();unsafe{GetFileInformationByHandleEx(file.0,FileAttributeTagInfo,
        (&mut info as*mut FILE_ATTRIBUTE_TAG_INFO).cast(),std::mem::size_of_val(&info)as u32)}?;
    // Name-surrogate reparse points redirect to another tree. Cloud directory
    // tags are not name surrogates and can be enumerated without reading data.
    Ok(info.ReparseTag&0x2000_0000!=0)
}
fn identity(path:&Path)->Result<(u32,u64)>{
    let name:Vec<_>=path.as_os_str().encode_wide().chain(Some(0)).collect();
    let handle=unsafe{CreateFileW(PCWSTR(name.as_ptr()),FILE_READ_ATTRIBUTES.0,FILE_SHARE_READ|FILE_SHARE_WRITE|FILE_SHARE_DELETE,
        None,OPEN_EXISTING,FILE_FLAG_BACKUP_SEMANTICS|FILE_FLAG_OPEN_REPARSE_POINT,None)}?;
    struct File(HANDLE);impl Drop for File{fn drop(&mut self){let _=unsafe{CloseHandle(self.0)};}}let file=File(handle);
    let mut info=BY_HANDLE_FILE_INFORMATION::default();unsafe{GetFileInformationByHandle(file.0,&mut info)}?;
    Ok((info.dwVolumeSerialNumber,(u64::from(info.nFileIndexHigh)<<32)|u64::from(info.nFileIndexLow)))
}
struct Directory{path:PathBuf,entries:ReadDir,identity:(u32,u64)}
impl Directory{fn open(path:PathBuf)->Result<Self>{let identity=identity(&path)?;let entries=fs::read_dir(&path).with_context(||zsdesk_core::tr_format!("无法读取 {}" , path.display()))?;Ok(Self{path,entries,identity})}}
struct Scan{root:PathBuf,stack:Vec<Directory>,bytes:u64,skipped:u64}
impl Scan{
    fn new(path:PathBuf)->Result<Self>{Ok(Self{stack:vec![Directory::open(path.clone())?],root:path,bytes:0,skipped:0})}
    fn step(&mut self,cancel:&impl Fn()->bool)->Result<Option<Value>>{
        let start=Instant::now();let mut visited=0;
        loop{
            ensure!(!cancel(),"大小计算已取消");
            let Some(directory)=self.stack.last_mut()else{return Ok(Some(Value{bytes:Some(self.bytes),skipped_links:self.skipped,error:None}));};
            let entry=match directory.entries.next(){Some(entry)=>entry.with_context(||format!("无法完整读取 {}",self.root.display()))?,None=>{
                ensure!(identity(&directory.path)?==directory.identity,"目录在计算期间被替换：{}",directory.path.display());self.stack.pop();continue;
            }};
            let path=entry.path();let metadata=entry.metadata().with_context(||format!("无法读取 {} 的大小",path.display()))?;
            if metadata.file_attributes()&0x10!=0{
                if name_surrogate(&path,metadata.file_attributes())?{self.skipped+=1;}
                else{ensure!(self.stack.len()<1024,"目录层级过深：{}",path.display());self.stack.push(Directory::open(path)?);}
            }else if metadata.is_file(){self.bytes=self.bytes.checked_add(metadata.len()).context("文件夹总大小超出 64 位范围")?;}
            visited+=1;if visited>=512||start.elapsed()>=Duration::from_millis(20){return Ok(None);}
        }
    }
}
fn valid_name(name:&OsString)->bool{let mut components=Path::new(name).components();matches!(components.next(),Some(std::path::Component::Normal(_)))&&components.next().is_none()&&!name.encode_wide().any(|unit|unit==0)}

/// Emits completed totals only; a cancelled or unreadable subtree never becomes
/// a zero or a partial numeric value. Directory links are not followed.
pub(crate) fn calculate(profile:&Path,directory:&Path,items:&[Item],cancel:&impl Fn()->bool,
    mut emit:impl FnMut(usize,Value)->bool)->Result<()>{
    ensure!(directory.is_absolute()&&items.iter().all(|item|valid_name(&item.name)),"大小查询路径无效");
    let ui=crate::storage::load(profile)?.ui;
    let external=!ui.search_backend.is_builtin()&&crate::fast_search::everything::available(ui.everything_executable.as_deref());
    let root=fs::canonicalize(directory).context("无法访问文件夹大小查询目录")?;
    let mut directories=Vec::new();
    for (index,item) in items.iter().enumerate(){
        if cancel(){return Ok(());}
        let path=root.join(&item.name);
        let metadata=match fs::symlink_metadata(&path){Ok(value)=>value,Err(error)=>{if !emit(index,Value::error(format!("{}：{error}",path.display()))){return Ok(());}continue;}};
        if metadata.file_attributes()&0x10!=0{match name_surrogate(&path,metadata.file_attributes()){
            Ok(true)=>{if !emit(index,Value{skipped_links:1,..Default::default()}){return Ok(());}},
            Ok(false)=>directories.push(index),Err(error)=>if !emit(index,Value::error(error)){return Ok(());},
        }}else if metadata.is_file(){if !emit(index,Value::known(metadata.len())){return Ok(());}}
        else if !emit(index,Value::default()){return Ok(());}
    }
    let mut pending=VecDeque::new();
    if external{
        let mut unavailable=false;
        for (batch,chunk) in directories.chunks(64).enumerate(){
            if cancel(){return Ok(());}
            let paths:Vec<_>=chunk.iter().map(|index|root.join(&items[*index].name)).collect();
            let queried=if unavailable{None}else{crate::fast_search::everything::query_folder_sizes(profile,&paths,
                0x5a53b000u32.wrapping_add(batch as u32*64),cancel).ok()};
            if queried.is_none(){unavailable=true;}
            for (index,path) in chunk.iter().zip(&paths){
                let value=queried.as_ref().and_then(|values|values.iter().find(|(found,_)|crate::explorer_forwarder::same_path(found,path))).and_then(|(_,value)|*value);
                if let Some(value)=value{if !emit(*index,Value::known(value)){return Ok(());}}
                else{pending.push_back(*index);}
            }
        }
    }else{pending.extend(directories);}
    if pending.is_empty(){return Ok(());}
    let Some(_permit)=Permit::acquire(cancel)else{return Ok(());};
    let mut active=VecDeque::new();
    loop{
        if cancel(){return Ok(());}
        while active.len()<4{let Some(index)=pending.pop_front()else{break;};match Scan::new(root.join(&items[index].name)){
            Ok(scan)=>active.push_back((index,scan)),Err(error)=>if !emit(index,Value::error(error)){return Ok(());},
        }}
        let Some((index,mut scan))=active.pop_front()else{break;};
        match scan.step(cancel){
            Ok(Some(value))=>if !emit(index,value){return Ok(());},
            Ok(None)=>active.push_back((index,scan)),
            Err(error)=>{if cancel(){return Ok(());}if !emit(index,Value::error(error)){return Ok(());}},
        }
    }
    Ok(())
}

#[cfg(test)]mod tests{
    use super::*;use std::sync::atomic::AtomicU64;
    static NEXT:AtomicU64=AtomicU64::new(0);
    struct Fixture(PathBuf,String);impl Fixture{fn new()->Result<Self>{let marker=format!("{}-{}",std::process::id(),NEXT.fetch_add(1,Ordering::Relaxed));let root=std::env::temp_dir().join(format!("zsdesk-folder-totals-{marker}"));fs::create_dir(&root)?;fs::write(root.join("owner"),&marker)?;Ok(Self(root,marker))}}
    impl Drop for Fixture{fn drop(&mut self){if self.0.is_absolute()&&fs::symlink_metadata(&self.0).is_ok_and(|m|m.file_attributes()&0x400==0)&&fs::read(self.0.join("owner")).is_ok_and(|v|v==self.1.as_bytes()){let _=fs::remove_dir_all(&self.0);}}}
    #[test]fn builtin_totals_are_exact_and_unknown_is_not_empty()->Result<()>{
        let fixture=Fixture::new()?;let root=fixture.0.join("files");fs::create_dir(&root)?;fs::create_dir(root.join("空目录"))?;fs::create_dir_all(root.join("中文 空格/子目录"))?;
        fs::write(root.join("中文 空格/a.txt"),b"12345")?;fs::write(root.join("中文 空格/子目录/b.txt"),b"1234567")?;fs::write(root.join("file.bin"),b"123")?;
        let profile=fixture.0.join("explorer-session.json");crate::storage::save(&profile,&crate::storage::default_session())?;
        let items:Vec<_>=["空目录","中文 空格","file.bin","missing"].into_iter().map(|name|Item{name:name.into()}).collect();
        let mut values=vec![Value::default();items.len()];calculate(&profile,&root,&items,&||false,|index,value|{values[index]=value;true})?;
        assert_eq!(values.iter().map(|value|value.bytes).collect::<Vec<_>>(),[Some(0),Some(12),Some(3),None]);assert!(values[3].error.is_some());
        assert!(!fixture.0.join("everything-data").exists());assert!(!valid_name(&OsString::from("..")));assert!(!valid_name(&OsString::from(r"a\b")));Ok(())
    }
    #[test]fn cancellation_never_emits_partial_directory_total()->Result<()>{
        let fixture=Fixture::new()?;for index in 0..20{let dir=fixture.0.join(format!("folder/{index}"));fs::create_dir_all(&dir)?;fs::write(dir.join("data"),[0u8;128])?;}
        let mut scan=Scan::new(fixture.0.join("folder"))?;let calls=std::cell::Cell::new(0);
        let result=scan.step(&||{let n=calls.get()+1;calls.set(n);n>5});assert!(result.is_err());assert!(scan.bytes<20*128);Ok(())
    }
    #[test]fn native_total_preserves_sizes_above_four_gib_without_allocating_contents()->Result<()>{
        use std::os::windows::io::AsRawHandle;use windows::Win32::System::{IO::DeviceIoControl,Ioctl::FSCTL_SET_SPARSE};
        let fixture=Fixture::new()?;let root=fixture.0.join("files");fs::create_dir(&root)?;let file=fs::File::create(root.join("large.bin"))?;
        let mut returned=0;unsafe{DeviceIoControl(HANDLE(file.as_raw_handle()),FSCTL_SET_SPARSE,None,0,None,0,Some(&mut returned),None)}?;
        let bytes=5u64*1024*1024*1024+17;file.set_len(bytes)?;assert!(file.metadata()?.file_attributes()&0x200!=0);drop(file);
        let mut scan=Scan::new(root)?;loop{if let Some(value)=scan.step(&||false)?{assert_eq!(value.bytes,Some(bytes));break;}}Ok(())
    }
    #[test]fn replacing_a_scanned_directory_invalidates_its_total()->Result<()>{
        let fixture=Fixture::new()?;let root=fixture.0.join("folder");fs::create_dir(&root)?;fs::write(root.join("file"),b"old")?;let mut scan=Scan::new(root.clone())?;
        fs::rename(&root,fixture.0.join("previous"))?;fs::create_dir(&root)?;fs::write(root.join("new"),b"new data")?;
        assert!(scan.step(&||false).is_err());Ok(())
    }
}
