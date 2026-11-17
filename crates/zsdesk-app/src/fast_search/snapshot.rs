//! Versioned column snapshots: bounded streaming I/O, checksum and atomic replace.
use std::{fs::{self,File,OpenOptions},io::{self,Read,Write,BufReader,BufWriter},os::windows::ffi::{OsStrExt,OsStringExt},ffi::OsString,path::Path};
use anyhow::{Result,ensure,Context};
use sha2::{Digest,Sha256};
use super::metadata::{Index,Names,Attributes,VolumeIndex,MAX_ROWS};
const MAGIC:&[u8;8]=b"ZSDIDX01";
// Increment when the built-in exclusion policy changes without a format change.
const FILTER_REVISION:u32=1;
const MAX_BYTES:u64=2*1024*1024*1024;
pub struct Policy{pub development:bool,pub metadata:bool}
impl Policy{fn bits(&self)->u32{u32::from(self.development)|(u32::from(self.metadata)<<1)|(FILTER_REVISION<<8)}}
struct HashWriter{file:File,hash:Sha256,bytes:u64}
impl Write for HashWriter{
    fn write(&mut self,bytes:&[u8])->io::Result<usize>{if self.bytes+bytes.len()as u64>MAX_BYTES{return Err(io::Error::other("索引快照超过 2 GiB 容量"));}
        let n=self.file.write(bytes)?;self.hash.update(&bytes[..n]);self.bytes+=n as u64;Ok(n)}
    fn flush(&mut self)->io::Result<()>{self.file.flush()}
}
struct HashReader<R>{input:R,hash:Sha256}
impl<R:Read> Read for HashReader<R>{fn read(&mut self,bytes:&mut[u8])->io::Result<usize>{let n=self.input.read(bytes)?;self.hash.update(&bytes[..n]);Ok(n)}}
trait Number:Copy{const BYTES:usize;fn encode(self,out:&mut Vec<u8>);fn decode(bytes:&[u8])->Self;}
macro_rules! number{($t:ty,$n:expr)=>{impl Number for $t{const BYTES:usize=$n;fn encode(self,out:&mut Vec<u8>){out.extend_from_slice(&self.to_le_bytes());}fn decode(bytes:&[u8])->Self{Self::from_le_bytes(bytes.try_into().unwrap())}}};}
number!(u16,2);number!(u32,4);number!(u64,8);number!(i64,8);
fn scalar<T:Number>(writer:&mut impl Write,value:T)->Result<()>{let mut bytes=Vec::with_capacity(T::BYTES);value.encode(&mut bytes);writer.write_all(&bytes)?;Ok(())}
fn read_scalar<T:Number>(reader:&mut impl Read)->Result<T>{let mut bytes=vec![0;T::BYTES];reader.read_exact(&mut bytes)?;Ok(T::decode(&bytes))}
fn column<T:Number>(writer:&mut impl Write,values:&[T],cancel:&impl Fn()->bool)->Result<()>{
    scalar(writer,values.len()as u64)?;let mut bytes=Vec::with_capacity(65536);
    for chunk in values.chunks(65536/T::BYTES){ensure!(!cancel(),"快照保存已取消");bytes.clear();for value in chunk{value.encode(&mut bytes);}writer.write_all(&bytes)?;}Ok(())
}
fn read_column<T:Number>(reader:&mut impl Read,remaining:&mut u64,max:usize,cancel:&impl Fn()->bool)->Result<Vec<T>>{
    let count=read_scalar::<u64>(reader)?;let bytes=count.checked_mul(T::BYTES as u64).context("快照长度溢出")?;
    ensure!(count<=max as u64&&bytes<=*remaining,"快照列长度无效");*remaining-=bytes;
    let mut values=Vec::new();values.try_reserve_exact(count as usize)?;let mut buffer=vec![0u8;65536];let mut left=bytes as usize;
    while left>0{ensure!(!cancel(),"快照读取已取消");let n=left.min(buffer.len());reader.read_exact(&mut buffer[..n])?;
        values.extend(buffer[..n].chunks_exact(T::BYTES).map(T::decode));left-=n;}Ok(values)
}
fn bytes(writer:&mut impl Write,values:&[u8])->Result<()>{scalar(writer,values.len()as u64)?;writer.write_all(values)?;Ok(())}
fn read_bytes(reader:&mut impl Read,remaining:&mut u64,max:usize)->Result<Vec<u8>>{
    let count=read_scalar::<u64>(reader)?;ensure!(count<=max as u64&&count<=*remaining,"快照字节列无效");*remaining-=count;
    let mut values=Vec::new();values.try_reserve_exact(count as usize)?;values.resize(count as usize,0);reader.read_exact(&mut values)?;Ok(values)
}
pub fn save(path:&Path,policy:&Policy,volumes:&[VolumeIndex],cancel:&impl Fn()->bool)->Result<()>{
    ensure!(path.is_absolute()&&volumes.len()<=128,"快照路径或卷数量无效");let parent=path.parent().context("快照目录缺失")?;fs::create_dir_all(parent)?;
    let temporary=path.with_extension(format!("{}.tmp",std::process::id()));
    struct Temporary<'a>(&'a Path);impl Drop for Temporary<'_>{fn drop(&mut self){let _=fs::remove_file(self.0);}}
    let _temporary=Temporary(&temporary);
    use std::os::windows::fs::OpenOptionsExt;
    let file=OpenOptions::new().create(true).truncate(true).write(true).share_mode(0).open(&temporary)?;
    let mut writer=BufWriter::with_capacity(1024*1024,HashWriter{file,hash:Sha256::new(),bytes:0});
    writer.write_all(MAGIC)?;scalar(&mut writer,policy.bits())?;scalar(&mut writer,volumes.len()as u32)?;
    for volume in volumes{
        ensure!(!cancel(),"快照保存已取消");column(&mut writer,&volume.root.as_os_str().encode_wide().collect::<Vec<_>>(),cancel)?;
        scalar(&mut writer,volume.serial)?;scalar(&mut writer,volume.journal_id)?;scalar(&mut writer,volume.checkpoint)?;
        scalar(&mut writer,u32::from(volume.has_journal)|(u32::from(volume.mft)<<1)|if volume.pending_dirs.is_empty(){0}else{4})?;scalar(&mut writer,volume.denied)?;
        let index=&volume.index;
        column(&mut writer,&index.ids,cancel)?;column(&mut writer,&index.parent,cancel)?;column(&mut writer,&index.name,cancel)?;bytes(&mut writer,&index.flags)?;
        column(&mut writer,&index.names.units,cancel)?;column(&mut writer,&index.names.starts,cancel)?;bytes(&mut writer,&index.names.folded)?;column(&mut writer,&index.names.folded_starts,cancel)?;
        ensure!(index.attributes.is_some()==policy.metadata,"快照属性选项不一致");
        if let Some(attrs)=&index.attributes{column(&mut writer,&attrs.size,cancel)?;column(&mut writer,&attrs.modified,cancel)?;}
        if !volume.pending_dirs.is_empty(){column(&mut writer,&volume.pending_dirs,cancel)?;}
    }
    writer.flush()?;let mut writer=writer.into_inner().map_err(|e|e.into_error())?;let digest=writer.hash.finalize();writer.file.write_all(&digest)?;writer.file.sync_all()?;drop(writer.file);
    ensure!(!cancel(),"快照保存已取消");
    use windows::{core::PCWSTR,Win32::Storage::FileSystem::{MoveFileExW,MOVEFILE_REPLACE_EXISTING}};
    let from:Vec<_>=temporary.as_os_str().encode_wide().chain(Some(0)).collect();let to:Vec<_>=path.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe{MoveFileExW(PCWSTR(from.as_ptr()),PCWSTR(to.as_ptr()),MOVEFILE_REPLACE_EXISTING)}?;Ok(())
}
pub fn load(path:&Path,policy:&Policy,cancel:&impl Fn()->bool)->Result<Vec<VolumeIndex>>{
    use std::os::windows::fs::MetadataExt;
    let metadata=fs::symlink_metadata(path)?;ensure!(metadata.is_file()&&metadata.file_attributes()&0x400==0,"快照必须为普通文件");
    let size=metadata.len();ensure!((48..=MAX_BYTES+32).contains(&size),"快照大小无效");let file=File::open(path)?;
    let mut reader=BufReader::with_capacity(1024*1024,HashReader{input:file.take(size-32),hash:Sha256::new()});
    let mut magic=[0u8;8];reader.read_exact(&mut magic)?;ensure!(&magic==MAGIC,"快照版本不兼容");ensure!(read_scalar::<u32>(&mut reader)?==policy.bits(),"索引选项已改变，需要重新建立");
    let count=read_scalar::<u32>(&mut reader)?;ensure!(count<=128,"快照卷数量无效");let mut remaining=size-32;let mut volumes=Vec::new();let mut total=0;
    for _ in 0..count{
        let root=std::path::PathBuf::from(OsString::from_wide(&read_column::<u16>(&mut reader,&mut remaining,32768,cancel)?));ensure!(root.is_absolute(),"快照根目录必须为绝对路径");
        let serial=read_scalar(&mut reader)?;let journal_id=read_scalar(&mut reader)?;let checkpoint=read_scalar::<i64>(&mut reader)?;
        let flags=read_scalar::<u32>(&mut reader)?;ensure!(flags&!7==0&&checkpoint>=0,"快照日志状态无效");let denied=read_scalar(&mut reader)?;
        let mut index=Index::new(0,policy.metadata);
        index.ids=read_column(&mut reader,&mut remaining,MAX_ROWS,cancel)?;total+=index.ids.len();ensure!(total<=MAX_ROWS,"快照总项目数超出容量");
        index.parent=read_column(&mut reader,&mut remaining,index.ids.len(),cancel)?;index.name=read_column(&mut reader,&mut remaining,index.ids.len(),cancel)?;
        index.flags=read_bytes(&mut reader,&mut remaining,index.ids.len())?;
        let mut names=Names::new();names.units=read_column(&mut reader,&mut remaining,MAX_BYTES as usize/2,cancel)?;
        names.starts=read_column(&mut reader,&mut remaining,MAX_ROWS+1,cancel)?;names.folded=read_bytes(&mut reader,&mut remaining,MAX_BYTES as usize)?;
        names.folded_starts=read_column(&mut reader,&mut remaining,MAX_ROWS+1,cancel)?;index.names=names;
        if policy.metadata{index.attributes=Some(Attributes{size:read_column(&mut reader,&mut remaining,index.ids.len(),cancel)?,modified:read_column(&mut reader,&mut remaining,index.ids.len(),cancel)?});}
        let pending_dirs=if flags&4!=0{read_column::<u64>(&mut reader,&mut remaining,MAX_ROWS,cancel)?}else{Vec::new()};
        index.validate()?;volumes.push(VolumeIndex{root,serial,journal_id,checkpoint,has_journal:flags&1!=0,mft:flags&2!=0,denied,index,pending_dirs});
    }
    let mut extra=[0];ensure!(reader.read(&mut extra)?==0,"快照存在未识别数据");let reader=reader.into_inner();let digest=reader.hash.finalize();let mut file=reader.input.into_inner();
    let mut expected=[0u8;32];file.read_exact(&mut expected)?;ensure!(digest.as_slice()==expected,"快照校验失败，需要重新建立");Ok(volumes)
}

#[cfg(test)]mod tests{
    use super::*;
    #[test]fn snapshot_roundtrip_checksum_and_policy()->Result<()>{
        let root=std::env::temp_dir().join(format!("zsdesk-column-snapshot-{}",std::process::id()));fs::create_dir_all(&root)?;
        let path=root.join("index.zsi");let mut index=Index::new(5,true);index.append(7,0,std::ffi::OsStr::new("中文.txt"),0)?;index.reindex();index.set_attributes(1,Some(0),Some(42));
        let volumes=vec![VolumeIndex{root:Path::new(r"C:\").into(),serial:1,journal_id:7,checkpoint:99,has_journal:true,mft:true,denied:0,index,pending_dirs:vec![5]}];let policy=Policy{development:false,metadata:true};
        save(&path,&policy,&volumes,&||false)?;let restored=load(&path,&policy,&||false)?;assert_eq!(restored[0].checkpoint,99);assert_eq!(restored[0].index.attributes.as_ref().unwrap().size[1],0);assert_eq!(restored[0].pending_dirs,[5]);
        assert!(load(&path,&Policy{development:true,metadata:true},&||false).is_err());
        let mut data=fs::read(&path)?;let end=data.len()-1;data[end]^=1;fs::write(&path,&data)?;assert!(load(&path,&policy,&||false).is_err());
        fs::remove_file(&path)?;fs::remove_dir(&root)?;Ok(())
    }
}
