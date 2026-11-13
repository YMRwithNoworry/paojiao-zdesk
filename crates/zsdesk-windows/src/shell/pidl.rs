//! Owned PIDL bytes. ZSDesk keeps Shell identities as plain byte vectors so no
//! COM allocation outlives the call that produced it.

use windows::{
    core::Result,
    Win32::{
        System::Com::CoTaskMemFree,
        UI::Shell::{Common::ITEMIDLIST, ILGetSize, IShellItem, SHCreateItemFromIDList, SHGetIDListFromObject},
    },
};
use zsdesk_core::model::explorer::ShellLocation;

/// Copies an ITEMIDLIST into owned bytes (including the terminator).
pub fn pidl_bytes(pidl: *const ITEMIDLIST) -> Vec<u8> {
    if pidl.is_null() { return vec![0, 0]; }
    let len = unsafe { ILGetSize(Some(pidl)) } as usize;
    if len < 2 { return vec![0, 0]; }
    unsafe { std::slice::from_raw_parts(pidl.cast::<u8>(), len) }.to_vec()
}

pub fn item_pidl(item: &IShellItem) -> Option<Vec<u8>> {
    let pidl = unsafe { SHGetIDListFromObject(item).ok()? };
    let bytes = checked_pidl_bytes(pidl);
    unsafe { CoTaskMemFree(Some(pidl.cast())) };
    bytes
}

pub(super) fn checked_pidl_bytes(pidl:*const ITEMIDLIST)->Option<Vec<u8>>{
    if pidl.is_null(){return None;}
    let length=unsafe{ILGetSize(Some(pidl))}as usize;
    if !(2..=64*1024).contains(&length){return None;}
    let bytes=unsafe{std::slice::from_raw_parts(pidl.cast::<u8>(),length)};
    valid(bytes).then(||bytes.to_vec())
}

pub fn item_from_pidl(pidl: &[u8]) -> Result<IShellItem> {
    if !valid(pidl){return Err(windows::core::Error::from_hresult(windows::core::HRESULT(0x80070057u32 as i32)));}
    unsafe { SHCreateItemFromIDList(pidl.as_ptr().cast()) }
}

pub fn valid(bytes:&[u8])->bool{
    if bytes.len()<2||bytes.len()>64*1024{return false;}
    let mut at=0usize;
    while at+2<=bytes.len(){
        let length=u16::from_le_bytes([bytes[at],bytes[at+1]])as usize;
        if length==0{return at+2==bytes.len();}
        if length<2||length>bytes.len()-at{return false;}
        at+=length;
    }
    false
}

#[cfg(test)]mod tests{
    use super::*;
    #[test]fn malformed_namespace_identities_never_reach_shell(){
        for invalid in [vec![],vec![0],vec![1,0,0,0],vec![8,0,1,0,0,0],vec![0,0,1,2],vec![2,0]]{
            assert!(!valid(&invalid));assert!(item_from_pidl(&invalid).is_err());
        }
        assert!(valid(&[0,0]));assert!(valid(&[4,0,42,7,0,0]));
    }
}

pub fn location_pidl(location: &ShellLocation) -> Option<Vec<u8>> {
    let item = unsafe { super::shell_item(location).ok()? };
    item_pidl(&item)
}

