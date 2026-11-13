//! Batched directory records include file identities without opening every file.
use std::{ffi::OsString,os::windows::{ffi::{OsStrExt,OsStringExt},fs::MetadataExt},path::{Path,PathBuf}};
use anyhow::{Result,Context,ensure};
use windows::{core::PCWSTR,Win32::{Foundation::{CloseHandle,HANDLE},Storage::FileSystem::*}};
pub struct Entry{pub id:u64,pub name:OsString,pub attributes:u32,pub size:u64,pub modified:u64}
struct Handle(HANDLE);impl Drop for Handle{fn drop(&mut self){let _=unsafe{CloseHandle(self.0)};}}
fn open(path:&Path,access:u32)->Result<Handle>{
    let text:Vec<_>=path.as_os_str().encode_wide().chain(Some(0)).collect();
    Ok(Handle(unsafe{CreateFileW(PCWSTR(text.as_ptr()),access,FILE_SHARE_READ|FILE_SHARE_WRITE|FILE_SHARE_DELETE,None,
        OPEN_EXISTING,FILE_FLAG_BACKUP_SEMANTICS|FILE_FLAG_OPEN_REPARSE_POINT,None)}?))
}
pub fn identity(path:&Path)->Result<(u32,u64,u32)>{
    let handle=open(path,FILE_READ_ATTRIBUTES.0)?;let mut info=BY_HANDLE_FILE_INFORMATION::default();unsafe{GetFileInformationByHandle(handle.0,&mut info)}?;
    Ok((info.dwVolumeSerialNumber,(u64::from(info.nFileIndexHigh)<<32)|u64::from(info.nFileIndexLow),info.dwFileAttributes))
}
pub fn volume_root(path:&Path)->Option<PathBuf>{let mut components=path.components();let prefix=components.next()?;
    let std::path::Component::Prefix(prefix)=prefix else{return None;};match prefix.kind(){std::path::Prefix::Disk(letter)|std::path::Prefix::VerbatimDisk(letter)=>Some(PathBuf::from(format!("{}:\\",letter as char))),_=>None}}
pub fn directory(path:&Path,cancel:&impl Fn()->bool,mut entry:impl FnMut(Entry)->Result<()>)->Result<()> {
    let handle=open(path,FILE_LIST_DIRECTORY.0)?;let mut info=BY_HANDLE_FILE_INFORMATION::default();unsafe{GetFileInformationByHandle(handle.0,&mut info)}?;
    ensure!(info.dwFileAttributes&0x400==0,"不递归跟随目录链接");
    // u64 storage guarantees the alignment required by FILE_ID_BOTH_DIR_INFO.
    let mut buffer=vec![0u64;8192];let mut first=true;
    loop {
        ensure!(!cancel(),"索引已取消");
        let result=unsafe{GetFileInformationByHandleEx(handle.0,if first{FileIdBothDirectoryRestartInfo}else{FileIdBothDirectoryInfo},buffer.as_mut_ptr().cast(),(buffer.len()*8)as u32)};
        if let Err(error)=result {
            if error.code().0 as u32==0x80070012{return Ok(());}
            if first&&matches!(error.code().0 as u32,0x80070001|0x80070032|0x80070057){return portable_directory(path,cancel,entry);}
            return Err(error).context("无法枚举目录记录");
        }
        first=false;let bytes=unsafe{std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(),buffer.len()*8)};let mut at=0;
        loop {
            ensure!(!cancel(),"索引已取消");
            let fixed=std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO,FileName);ensure!(at+fixed<=bytes.len(),"目录记录头越界");
            let read32=|offset|u32::from_le_bytes(bytes[at+offset..at+offset+4].try_into().unwrap());
            let read64=|offset|u64::from_le_bytes(bytes[at+offset..at+offset+8].try_into().unwrap());
            let next=read32(0)as usize;let length=read32(std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO,FileNameLength))as usize;
            let end=if next==0{bytes.len()}else{ensure!(next>=fixed&&next%8==0&&at+next<=bytes.len(),"目录记录偏移无效");at+next};
            ensure!(length>0&&length<=510&&length%2==0&&at+fixed+length<=end,"目录名称边界无效");
            let units:Vec<_>=bytes[at+fixed..at+fixed+length].chunks_exact(2).map(|v|u16::from_le_bytes([v[0],v[1]])).collect();
            ensure!(!units.iter().any(|n|matches!(*n,0|47|92)),"目录名称无效");let name=OsString::from_wide(&units);
            if name!="."&&name!=".."{entry(Entry{id:read64(std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO,FileId)),name,
                attributes:read32(std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO,FileAttributes)),
                size:read64(std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO,EndOfFile)),modified:read64(std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO,LastWriteTime))})?;}
            if next==0{break;}at+=next;
        }
    }
}
fn portable_directory(path:&Path,cancel:&impl Fn()->bool,mut entry:impl FnMut(Entry)->Result<()>)->Result<()> {
    for child in std::fs::read_dir(path)?{ensure!(!cancel(),"索引已取消");let child=child?;let metadata=child.metadata()?;
        let (_,id,attributes)=identity(&child.path())?;
        entry(Entry{id,name:child.file_name(),attributes,size:metadata.file_size(),modified:metadata.last_write_time()})?;
    }Ok(())
}
