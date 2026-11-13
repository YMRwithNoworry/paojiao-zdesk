//! Bounded UTF-16 folder-location requests from the per-user shell router.
use std::{ffi::OsString,path::PathBuf};
pub const COPYDATA_TAG:usize=0x3152464e;
pub const VERIFY_TAG:usize=0x3252464e;
pub const VERIFY_THIS_PC:usize=0x3352464e;
pub fn encode(folder:&std::path::Path,items:&[PathBuf])->Option<Vec<u8>>{
    use std::os::windows::ffi::OsStrExt;
    if !folder.is_absolute()||items.len()>128{return None;}
    let payload:Vec<u16>=std::iter::once(folder).chain(items.iter().map(PathBuf::as_path)).flat_map(|path|path.as_os_str().encode_wide().chain(Some(0))).collect();
    if payload.len()>((1024*1024-32)/2){return None;}
    let mut data=Vec::with_capacity(32+payload.len()*2);
    for n in [COPYDATA_TAG as u32,1,(32+payload.len()*2)as u32,items.len()as u32,std::process::id(),1,1,0]{data.extend(n.to_le_bytes());}
    for n in payload{data.extend(n.to_le_bytes());}
    decode(&data).map(|_|data)
}
pub fn decode(bytes:&[u8])->Option<(PathBuf,Vec<OsString>)>{
    if bytes.len()<34 || bytes.len()>1024*1024 || (bytes.len()-32)%2!=0{return None;}
    let u32_at=|at|u32::from_le_bytes(bytes[at..at+4].try_into().ok().unwrap());
    if u32_at(0)!=COPYDATA_TAG as u32 || u32_at(4)!=1 || u32_at(8)as usize!=bytes.len() || u32_at(12)>128 || u32_at(16)==0{return None;}
    let count=u32_at(12)as usize;
    let units:Vec<_>=bytes[32..].chunks_exact(2).map(|b|u16::from_le_bytes([b[0],b[1]])).collect();
    let mut offset=0;
    let mut next=||{
        let end=units[offset..].iter().position(|u|*u==0)?+offset;
        let value=String::from_utf16(&units[offset..end]).ok()?;offset=end+1;
        let path=PathBuf::from(value);path.is_absolute().then_some(path)
    };
    let folder=next()?;let mut names=Vec::with_capacity(count);
    for _ in 0..count{
        let item=next()?;
        if !item.parent()?.to_string_lossy().trim_end_matches(['\\','/']).eq_ignore_ascii_case(folder.to_string_lossy().trim_end_matches(['\\','/'])){return None;}
        names.push(item.file_name()?.to_owned());
    }
    if offset!=units.len(){return None;}
    Some((folder,names))
}
#[cfg(test)]mod tests{
    use super::*;
    fn packet(folder:&str,items:&[&str])->Vec<u8>{
        let payload:Vec<u16>=std::iter::once(folder).chain(items.iter().copied()).flat_map(|s|s.encode_utf16().chain(Some(0))).collect();
        let mut data=Vec::new();for n in [COPYDATA_TAG as u32,1,(32+payload.len()*2)as u32,items.len()as u32,17,1,1,0]{data.extend(n.to_le_bytes());}
        for n in payload{data.extend(n.to_le_bytes());}data
    }
    #[test]fn retains_unicode_multi_selection_and_rejects_foreign_parent(){
        let p=packet(r"E:\资料",&[r"E:\资料\图纸, 甲.pdf",r"E:\资料\表格.xlsx"]);
        let(folder,names)=decode(&p).unwrap();assert_eq!(folder,PathBuf::from(r"E:\资料"));assert_eq!(names.len(),2);assert_eq!(names[0],OsString::from("图纸, 甲.pdf"));
        assert!(decode(&packet(r"E:\资料",&[r"C:\wrong.txt"])).is_none());
        assert!(decode(&p[..p.len()-1]).is_none());
    }
}
