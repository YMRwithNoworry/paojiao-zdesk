//! Local QuickLook bridge for users who disable the built-in Space preview.
use super::*;
use std::{fs::OpenOptions,os::windows::{fs::OpenOptionsExt,io::AsRawHandle},path::Path,sync::OnceLock};
use windows::Win32::{Foundation::{CloseHandle,HANDLE,ERROR_IO_PENDING},
    Security::{GetLengthSid,GetTokenInformation,TokenUser,TOKEN_QUERY,TOKEN_USER},
    Storage::FileSystem::{FILE_FLAG_OVERLAPPED,SECURITY_IDENTIFICATION,SECURITY_SQOS_PRESENT,WriteFile},
    System::{IO::{CancelIoEx,GetOverlappedResult,GetOverlappedResultEx,OVERLAPPED},
        Threading::{CreateEventW,GetCurrentProcess,OpenProcessToken}}};

thread_local!{static SPACE:Cell<bool>=const{Cell::new(false)};}
struct Handle(HANDLE);
impl Drop for Handle{fn drop(&mut self){let _=unsafe{CloseHandle(self.0)};}}

fn sid_text(bytes:&[u8])->Option<String>{
    if bytes.len()<8||bytes[0]!=1||bytes.len()!=8+usize::from(bytes[1])*4{return None;}
    let authority=bytes[2..8].iter().fold(0u64,|value,byte|(value<<8)|u64::from(*byte));
    let mut text=format!("S-1-{authority}");
    for value in bytes[8..].chunks_exact(4){text.push_str(&format!("-{}",u32::from_le_bytes(value.try_into().ok()?)));}
    Some(text)
}
fn pipe_name()->Option<&'static str>{
    static NAME:OnceLock<Option<String>>=OnceLock::new();
    NAME.get_or_init(||{
        let mut token=HANDLE::default();unsafe{OpenProcessToken(GetCurrentProcess(),TOKEN_QUERY,&mut token)}.ok()?;let token=Handle(token);
        let mut size=0;let _=unsafe{GetTokenInformation(token.0,TokenUser,None,0,&mut size)};
        if !(1..=65536).contains(&size){return None;}
        let mut buffer=vec![0u64;(size as usize+7)/8];
        unsafe{GetTokenInformation(token.0,TokenUser,Some(buffer.as_mut_ptr().cast()),size,&mut size)}.ok()?;
        let user=unsafe{&*buffer.as_ptr().cast::<TOKEN_USER>()};let length=unsafe{GetLengthSid(user.User.Sid)}as usize;
        if !(8..=1024).contains(&length){return None;}
        let sid=sid_text(unsafe{std::slice::from_raw_parts(user.User.Sid.0.cast::<u8>(),length)})?;
        Some(format!(r"\\.\pipe\QuickLook.App.Pipe.{sid}"))
    }).as_deref()
}
fn toggle_message(path:&Path)->Option<Vec<u8>>{
    let path=path.to_str()?;
    if !Path::new(path).is_absolute()||path.contains(['\0','\r','\n','|'])||path.len()>32768{return None;}
    Some(format!("QuickLook.App.PipeMessages.Toggle|{path}\n").into_bytes())
}
fn send(pipe:&str,bytes:&[u8])->bool{
    let Ok(file)=OpenOptions::new().write(true).custom_flags(FILE_FLAG_OVERLAPPED.0|SECURITY_SQOS_PRESENT.0|SECURITY_IDENTIFICATION.0).open(pipe)else{return false;};
    send_file(&file,bytes)
}
fn send_file(file:&std::fs::File,bytes:&[u8])->bool{
    let handle=HANDLE(file.as_raw_handle());
    let Ok(event)=(unsafe{CreateEventW(None,true,false,None)})else{return false;};let event=Handle(event);
    let mut operation=OVERLAPPED{hEvent:event.0,..Default::default()};let mut written=0;
    match unsafe{WriteFile(handle,Some(bytes),Some(&mut written),Some(&mut operation))}{
        Ok(())=>written as usize==bytes.len(),
        Err(error)if error.code()==windows::core::HRESULT::from_win32(ERROR_IO_PENDING.0)=>{
            if unsafe{GetOverlappedResultEx(handle,&operation,&mut written,60,false)}.is_ok(){return written as usize==bytes.len();}
            let _=unsafe{CancelIoEx(handle,Some(&operation))};
            // The OVERLAPPED and its buffer remain alive until cancellation completes.
            let _=unsafe{GetOverlappedResult(handle,&operation,&mut written,true)};false
        },Err(_)=>false,
    }
}

#[cfg(test)]#[path="quicklook_process_tests.rs"]mod process_tests;
pub(super) fn key(message:&MSG,slot:&Slot)->bool{
    if message.wParam.0!=VK_SPACE.0 as usize{return false;}
    if message.message==WM_KEYUP{return SPACE.with(|space|space.replace(false));}
    if message.message!=WM_KEYDOWN||text_input_window(message.hwnd)||text_input_window(unsafe{GetFocus()})
        ||[VK_CONTROL,VK_SHIFT,VK_MENU].iter().any(|key|unsafe{GetKeyState(key.0 as i32)}<0){return false;}
    if message.lParam.0 as usize&(1usize<<30)!=0{return SPACE.with(Cell::get);}
    let handled=preview_process::selected_preview_request(slot).and_then(|(path,_,_)|toggle_message(&path))
        .zip(pipe_name()).is_some_and(|(bytes,name)|send(name,&bytes));
    SPACE.with(|space|space.set(handled));handled
}

#[cfg(test)]mod tests{
    use super::*;
    #[test]fn quicklook_bridge_writes_to_an_isolated_local_pipe()->windows::core::Result<()>{
        use windows::Win32::{Foundation::{ERROR_PIPE_CONNECTED,INVALID_HANDLE_VALUE},
            Storage::FileSystem::{PIPE_ACCESS_INBOUND,ReadFile},
            System::{Pipes::{CreateNamedPipeW,ConnectNamedPipe,DisconnectNamedPipe,PIPE_TYPE_BYTE,PIPE_READMODE_BYTE,PIPE_WAIT},
                Threading::ResetEvent}};
        let name=format!(r"\\.\pipe\ZSDesk.QuickLook.Test.{}",std::process::id());let wide=wide(&name);
        let server=unsafe{CreateNamedPipeW(PCWSTR(wide.as_ptr()),PIPE_ACCESS_INBOUND|FILE_FLAG_OVERLAPPED,
            PIPE_TYPE_BYTE|PIPE_READMODE_BYTE|PIPE_WAIT,1,65536,65536,1000,None)};
        assert_ne!(server,INVALID_HANDLE_VALUE);let server=Handle(server);
        let event=Handle(unsafe{CreateEventW(None,true,false,None)}?);
        let mut operation=OVERLAPPED{hEvent:event.0,..Default::default()};
        let connection=unsafe{ConnectNamedPipe(server.0,Some(&mut operation))};
        assert!(connection.is_ok()||connection.as_ref().is_err_and(|error|[ERROR_IO_PENDING,ERROR_PIPE_CONNECTED].iter()
            .any(|code|error.code()==windows::core::HRESULT::from_win32(code.0))));
        let bytes=toggle_message(Path::new(r"C:\中文 文件\下载 2.exe")).unwrap();
        assert!(send(&name,&bytes));let mut count=0;
        if connection.as_ref().is_err_and(|error|error.code()==windows::core::HRESULT::from_win32(ERROR_IO_PENDING.0)){
            unsafe{GetOverlappedResultEx(server.0,&operation,&mut count,1000,false)}?;
        }
        unsafe{ResetEvent(event.0)}?;let mut buffer=vec![0u8;65536];
        let read=unsafe{ReadFile(server.0,Some(&mut buffer),Some(&mut count),Some(&mut operation))};
        if read.as_ref().is_err_and(|error|error.code()==windows::core::HRESULT::from_win32(ERROR_IO_PENDING.0)){
            unsafe{GetOverlappedResultEx(server.0,&operation,&mut count,1000,false)}?;
        }else{read?;}
        assert_eq!(&buffer[..count as usize],bytes);
        unsafe{DisconnectNamedPipe(server.0)}?;
        assert!(!send(&format!("{name}.absent"),&bytes));
        Ok(())
    }
    #[test]fn quicklook_protocol_preserves_unicode_and_rejects_injected_lines(){
        assert_eq!(toggle_message(Path::new(r"C:\中文 文件\图纸.pdf")).unwrap(),"QuickLook.App.PipeMessages.Toggle|C:\\中文 文件\\图纸.pdf\n".as_bytes());
        for invalid in ["relative.txt","C:\\a\nQuit","C:\\a|b","C:\\a\0b"]{assert!(toggle_message(Path::new(invalid)).is_none());}
        assert_eq!(sid_text(&[1,2,0,0,0,0,0,5,21,0,0,0,42,0,0,0]).as_deref(),Some("S-1-5-21-42"));
        assert!(sid_text(&[1,2,0,0,0,0,0,5]).is_none());
    }
}
