//! Everything Unicode QUERY2 IPC: paged results and indexed metadata.
//! Wire definitions: https://www.voidtools.com/support/everything/sdk/ipc/
use super::*;
use anyhow::ensure;
use windows::{core::PCWSTR,Win32::{Foundation::{HINSTANCE,LRESULT},System::{DataExchange::COPYDATASTRUCT,LibraryLoader::GetModuleHandleW},UI::{WindowsAndMessaging::*,Shell::{FileOpenDialog,IFileOpenDialog,Common::COMDLG_FILTERSPEC,FOS_FILEMUSTEXIST,FOS_FORCEFILESYSTEM,FOS_PATHMUSTEXIST,SIGDN_FILESYSPATH}}}};
#[path="everything_runtime.rs"]
mod runtime;

const CLASS:PCWSTR=windows::core::w!("ZSDesk.Everything.Reply");
const QUERY2_UNICODE:usize=18;
const REQUEST_FULL_PATH:u32=0x4;
const REQUEST_SIZE:u32=0x10;
const REQUEST_MODIFIED:u32=0x40;
const REQUEST_FLAGS:u32=REQUEST_FULL_PATH|REQUEST_SIZE|REQUEST_MODIFIED;
const MAX_PACKET:usize=16*1024*1024;
const PAGE_LIMIT:u32=2000;
fn wide(value:&str)->Vec<u16>{value.encode_utf16().chain(Some(0)).collect()}

#[derive(Clone,Copy,Debug,Default,PartialEq,Eq)]
pub enum SearchSort{#[default]NameAsc,NameDesc,ModifiedDesc,SizeDesc}
impl SearchSort{
    fn code(self)->u32{match self{Self::NameAsc=>1,Self::NameDesc=>2,Self::ModifiedDesc=>14,Self::SizeDesc=>6}}
}
#[derive(Clone,Debug)]
pub struct QueryOptions{pub text:String,pub offset:u32,pub limit:u32,pub sort:SearchSort}
impl Default for QueryOptions{fn default()->Self{Self{text:String::new(),offset:0,limit:200,sort:SearchSort::NameAsc}}}
#[derive(Clone,Debug,PartialEq,Eq)]
pub struct SearchItem{
    pub path:PathBuf,pub is_dir:bool,pub size:Option<u64>,
    /// UTC Windows FILETIME ticks (100 ns since 1601-01-01), from the index.
    pub modified:Option<u64>,
}
#[derive(Clone,Debug,PartialEq,Eq)]
pub struct SearchPage{pub items:Vec<SearchItem>,pub total:u64,pub offset:u32}

pub fn running()->Option<HWND>{runtime::running()}
pub fn executable(configured:Option<&Path>)->Option<PathBuf>{runtime::executable(configured)}
pub fn open_settings(profile:&Path)->Result<()>{let _activity=super::everything_activity();super::require_everything(profile)?;runtime::open_settings(profile)}
/// Must be invoked only by the explicit “启用全盘索引” user action.
pub fn enable_service(profile:&Path)->Result<()>{let _activity=super::everything_activity();super::require_everything(profile)?;runtime::enable_service(profile)}
pub fn shutdown_managed_if_unused()->Result<()>{runtime::shutdown_managed_if_unused()}

pub fn choose_executable(owner:HWND)->Result<Option<PathBuf>>{
    use windows::Win32::System::Com::{CoCreateInstance,CLSCTX_INPROC_SERVER,CoTaskMemFree};
    let dialog:IFileOpenDialog=unsafe{CoCreateInstance(&FileOpenDialog,None,CLSCTX_INPROC_SERVER)}?;
    unsafe{dialog.SetTitle(windows::core::w!("选择已安装的 Everything"))?;
        dialog.SetOptions(FOS_FILEMUSTEXIST|FOS_PATHMUSTEXIST|FOS_FORCEFILESYSTEM)?;
        dialog.SetFileTypes(&[COMDLG_FILTERSPEC{pszName:windows::core::w!("Everything 程序"),pszSpec:windows::core::w!("Everything*.exe")}])?;}
    if let Err(error)=unsafe{dialog.Show(Some(owner))}{if error.code().0 as u32==0x800704c7{return Ok(None);}return Err(error.into());}
    let item=unsafe{dialog.GetResult()?};let text=unsafe{item.GetDisplayName(SIGDN_FILESYSPATH)?};
    let path=unsafe{text.to_string()}.map(PathBuf::from);unsafe{CoTaskMemFree(Some(text.0.cast()))};let path=path?;
    runtime::ensure_executable(&path)?;Ok(Some(path))
}

struct Reply{cookie:u32,data:Option<Vec<u8>>}
unsafe extern "system" fn window_proc(hwnd:HWND,msg:u32,wp:WPARAM,lp:LPARAM)->LRESULT{
    if msg==WM_NCCREATE{let create=unsafe{&*(lp.0 as*const CREATESTRUCTW)};unsafe{SetWindowLongPtrW(hwnd,GWLP_USERDATA,create.lpCreateParams as isize);}return LRESULT(1);}
    if msg==WM_COPYDATA&&lp.0!=0{
        let pointer=unsafe{GetWindowLongPtrW(hwnd,GWLP_USERDATA)}as*mut Reply;if pointer.is_null(){return LRESULT(0);}
        let data=unsafe{&*(lp.0 as*const COPYDATASTRUCT)};let state=unsafe{&mut *pointer};
        if data.dwData==state.cookie as usize&&data.cbData as usize<=MAX_PACKET&&data.cbData>=20&&!data.lpData.is_null(){
            state.data=Some(unsafe{std::slice::from_raw_parts(data.lpData as*const u8,data.cbData as usize)}.to_vec());return LRESULT(1);
        }
    }
    unsafe{DefWindowProcW(hwnd,msg,wp,lp)}
}
fn u32_at(bytes:&[u8],offset:usize)->Result<u32>{Ok(u32::from_le_bytes(bytes.get(offset..offset.checked_add(4).context("Everything 数据偏移溢出")?).context("Everything 返回数据不完整")?.try_into()?))}
fn u64_at(bytes:&[u8],offset:usize)->Result<u64>{Ok(u64::from_le_bytes(bytes.get(offset..offset.checked_add(8).context("Everything 数据偏移溢出")?).context("Everything 返回元数据不完整")?.try_into()?))}
fn string_at(bytes:&[u8],cursor:&mut usize)->Result<OsString>{
    let length=u32_at(bytes,*cursor)? as usize;ensure!(length<=32767,"Everything 返回的路径过长");
    let start=(*cursor).checked_add(4).context("Everything 路径偏移溢出")?;
    let end=start.checked_add((length+1)*2).context("Everything 路径长度溢出")?;
    let raw=bytes.get(start..end).context("Everything 路径数据被截断")?;
    let units:Vec<u16>=raw.chunks_exact(2).map(|pair|u16::from_le_bytes([pair[0],pair[1]])).collect();
    ensure!(units.last()==Some(&0)&&!units[..length].contains(&0),"Everything 路径终止符无效");
    *cursor=end;Ok(OsString::from_wide(&units[..length]))
}
fn decode_page(bytes:&[u8],options:&QueryOptions)->Result<SearchPage>{
    ensure!(bytes.len()<=MAX_PACKET,"Everything 返回数据过大");
    let total=u32_at(bytes,0)?;let count=u32_at(bytes,4)?;let offset=u32_at(bytes,8)?;let flags=u32_at(bytes,12)?;let sort=u32_at(bytes,16)?;
    ensure!(count<=options.limit&&count<=PAGE_LIMIT,"Everything 返回数量超出页大小");
    ensure!(offset==options.offset,"Everything 返回了其他分页的数据");
    ensure!(count==0||u64::from(offset)+u64::from(count)<=u64::from(total),"Everything 返回总数与分页不一致");
    ensure!(flags&!REQUEST_FLAGS==0&&(count==0||flags&REQUEST_FULL_PATH!=0),"Everything 返回字段不兼容");
    ensure!(count==0||sort==options.sort.code(),"Everything 未应用所选排序，请检查其索引设置");
    let data_start=20usize+(count as usize)*8;ensure!(data_start<=bytes.len(),"Everything 返回项目表不完整");
    let mut items=Vec::with_capacity(count as usize);
    for index in 0..count as usize{
        let entry=20+index*8;let item_flags=u32_at(bytes,entry)?;let mut cursor=u32_at(bytes,entry+4)? as usize;
        ensure!(cursor>=data_start&&cursor<bytes.len(),"Everything 路径偏移无效");
        let path=PathBuf::from(string_at(bytes,&mut cursor)?);ensure!(path.is_absolute(),"Everything 返回了非绝对路径");
        let size=if flags&REQUEST_SIZE!=0{let value=u64_at(bytes,cursor)?;cursor+=8;(value!=u64::MAX).then_some(value)}else{None};
        let modified=if flags&REQUEST_MODIFIED!=0{let value=u64_at(bytes,cursor)?;(value!=0&&value!=u64::MAX).then_some(value)}else{None};
        items.push(SearchItem{path,is_dir:item_flags&3!=0,size,modified});
    }
    Ok(SearchPage{items,total:u64::from(total),offset})
}
fn validate(options:&QueryOptions)->Result<()>{
    ensure!(!options.text.contains('\0'),"搜索内容包含无效字符");
    ensure!(options.text.chars().count()<=512,"搜索内容过长，请缩短关键词");
    ensure!((1..=PAGE_LIMIT).contains(&options.limit),"搜索页大小无效");
    ensure!(options.offset.checked_add(options.limit).is_some(),"搜索分页超出范围");Ok(())
}
fn query_with(options:&QueryOptions,cookie:u32,configured:Option<&Path>,cancel:impl Fn()->bool)->Result<SearchPage>{
    query_with_progress(options,cookie,configured,cancel,REQUEST_FLAGS,&mut||{})
}
fn query_packet(options:&QueryOptions,hwnd:HWND,cookie:u32,flags:u32)->Vec<u8>{
    query_packet_with_search_flags(options,hwnd,cookie,flags,0)
}
fn query_packet_with_search_flags(options:&QueryOptions,hwnd:HWND,cookie:u32,flags:u32,search_flags:u32)->Vec<u8>{
    let mut payload=Vec::new();for value in [hwnd.0 as usize as u32,cookie,search_flags,options.offset,options.limit,flags,options.sort.code()]{payload.extend(value.to_le_bytes());}
    for unit in options.text.encode_utf16().chain(Some(0)){payload.extend(unit.to_le_bytes());}payload
}
fn query_with_progress(options:&QueryOptions,cookie:u32,configured:Option<&Path>,cancel:impl Fn()->bool,flags:u32,pending:&mut dyn FnMut())->Result<SearchPage>{
    validate(options)?;if cancel(){bail!("搜索已取消");}
    let server=runtime::ensure_running(configured,&cancel,false)?;runtime::wait_ready(&server,&cancel,pending)?;
    query_connection(options,cookie,&server,cancel,flags,0)
}
fn query_connection(options:&QueryOptions,cookie:u32,server:&runtime::Connection,cancel:impl Fn()->bool,flags:u32,search_flags:u32)->Result<SearchPage>{
    validate(options)?;if cancel(){bail!("搜索已取消");}
    let instance=HINSTANCE(unsafe{GetModuleHandleW(None)?}.0);
    unsafe{RegisterClassW(&WNDCLASSW{hInstance:instance,lpfnWndProc:Some(window_proc),lpszClassName:CLASS,..Default::default()});}
    let mut reply=Box::new(Reply{cookie,data:None});
    let hwnd=unsafe{CreateWindowExW(WINDOW_EX_STYLE::default(),CLASS,PCWSTR::null(),WINDOW_STYLE::default(),0,0,0,0,Some(HWND_MESSAGE),None,Some(instance),Some((&mut*reply as*mut Reply).cast()))?};
    struct Owned(HWND);impl Drop for Owned{fn drop(&mut self){let _=unsafe{DestroyWindow(self.0)};}}let _window=Owned(hwnd);
    let mut payload=query_packet_with_search_flags(options,hwnd,cookie,flags,search_flags);
    let data=COPYDATASTRUCT{dwData:QUERY2_UNICODE,cbData:payload.len()as u32,lpData:payload.as_mut_ptr().cast()};let mut accepted=0;
    if unsafe{SendMessageTimeoutW(server.hwnd,WM_COPYDATA,WPARAM(hwnd.0 as usize),LPARAM(&data as*const _ as isize),SMTO_ABORTIFHUNG,1500,Some(&mut accepted))}.0==0||accepted==0{
        bail!("Everything 未接受查询，请检查是否为 1.4.1 以上非 Lite 版本，以及两者的权限级别");
    }
    let deadline=Instant::now()+Duration::from_secs(15);let mut message=MSG::default();
    loop{
        if cancel(){bail!("搜索已取消");}
        if let Some(bytes)=reply.data.take(){return decode_page(&bytes,options);}
        ensure!(Instant::now()<deadline,"Everything 查询超时，请等待索引就绪后重试");
        // Limit each drain so a noisy thread queue cannot starve cancellation.
        for _ in 0..128{
            if !unsafe{PeekMessageW(&mut message,None,0,0,PM_REMOVE)}.as_bool(){break;}
            unsafe{let _=TranslateMessage(&message);DispatchMessageW(&message);}
        }
        thread::sleep(Duration::from_millis(5));
    }
}
pub fn query_page(profile:&Path,options:&QueryOptions,cookie:u32,cancel:impl Fn()->bool)->Result<SearchPage>{
    let _activity=super::everything_activity();super::require_everything(profile)?;
    query_with(options,cookie,runtime::configured(profile).as_deref(),cancel)
}

/// Reads only indexed folder sizes, for a bounded set of already visible paths.
/// This never requests a filesystem scan, enables an index, or treats a missing
/// index entry as a zero-byte folder. Call only from a cancellable worker.
pub(crate) fn query_folder_sizes(profile:&Path,paths:&[PathBuf],cookie:u32,cancel:impl Fn()->bool)->Result<Vec<(PathBuf,Option<u64>)>> {
    let _activity=super::everything_activity();super::require_everything(profile)?;
    ensure!(paths.len()<=64,"一次最多查询 64 个可见文件夹");
    if paths.is_empty(){return Ok(Vec::new());}
    if cancel(){bail!("文件夹大小查询已取消");}
    let started=Instant::now();
    let cancelled=||cancel()||started.elapsed()>Duration::from_secs(8);
    let server=runtime::ensure_running(runtime::configured(profile).as_deref(),&cancelled,false)?;
    if cancelled(){bail!("文件夹大小查询已取消或超时");}
    ensure!(runtime::ipc(server.hwnd,401,0)?!=0,"Everything 索引尚未就绪");
    // Official Everything SDK: WM_USER/411, FILE_INFO_FOLDER_SIZE=2.
    // Refuse unindexed sizes rather than triggering on-demand disk metadata.
    ensure!(runtime::ipc(server.hwnd,411,2)?!=0,"Everything 未启用文件夹大小索引；请在 Everything 索引设置中启用大小索引");
    let mut results=Vec::with_capacity(paths.len());
    for (index,path) in paths.iter().enumerate(){
        if cancel(){bail!("文件夹大小查询已取消");}
        ensure!(started.elapsed()<=Duration::from_secs(8),"文件夹大小查询超时");
        let Some(text)=folder_size_pattern(path)else{results.push((path.clone(),None));continue;};
        // MATCHPATH | REGEX. Anchors and escaping make the lookup literal even
        // for names containing Everything operators or regex metacharacters.
        let options=QueryOptions{text,offset:0,limit:2,sort:SearchSort::NameAsc};
        let page=query_connection(&options,cookie.wrapping_add(index as u32),&server,&cancelled,REQUEST_FULL_PATH|REQUEST_SIZE,0x4|0x8)?;
        let lookup=folder_size_literal(path).map(PathBuf::from).unwrap_or_else(||path.clone());
        let mut matches=page.items.into_iter().filter(|item|item.is_dir&&crate::explorer_forwarder::same_path(&item.path,&lookup));
        let size=matches.next().and_then(|item|item.size);
        // Case-sensitive directories can contain case variants. Do not guess.
        results.push((path.clone(),if page.total==1&&matches.next().is_none(){size}else{None}));
    }
    Ok(results)
}

fn folder_size_pattern(path:&Path)->Option<String>{
    let normalized=folder_size_literal(path)?;
    let mut pattern=String::from("^");
    for ch in normalized.chars(){
        if "\\.^$|?*+()[]{}".contains(ch){pattern.push('\\');}
        pattern.push(ch);
    }
    pattern.push('$');
    (pattern.chars().count()<=512).then_some(pattern)
}
fn folder_size_literal(path:&Path)->Option<String>{
    if !path.is_absolute(){return None;}
    let path=path.to_str()?;
    if path.contains('\0'){return None;}
    let path=path.strip_prefix(r"\\?\UNC\").map(|value|format!(r"\\{value}"))
        .unwrap_or_else(||path.strip_prefix(r"\\?\").unwrap_or(path).to_owned());
    let normalized=path.replace('/',r"\");
    let literal=if Path::new(&normalized).parent().is_none(){normalized.as_str()}else{normalized.trim_end_matches('\\')};
    Some(literal.to_owned())
}
/// Compatibility entry used by the existing main-window and native IPC probes.
#[cfg(test)]pub(super) fn query(text:&str,cookie:u32,configured:Option<&Path>,cancel:impl Fn()->bool)->Result<(Vec<PathBuf>,u64,bool)>{
    let page=query_with(&QueryOptions{text:text.into(),limit:PAGE_LIMIT,..Default::default()},cookie,configured,cancel)?;
    let truncated=u64::from(page.offset)+(page.items.len()as u64)<page.total;
    Ok((page.items.into_iter().map(|item|item.path).collect(),page.total,truncated))
}
pub(super) fn execute(shared:&Shared,sender:&SyncSender<SearchEvent>,request:&Request,profile:&Path,configured:Option<PathBuf>){
        let _activity=super::everything_activity();
        if let Err(error)=super::require_everything(profile){emit(shared,sender,failure(request.generation,BackendKind::Everything,error.to_string()));return;}
        let options=QueryOptions{text:request.query.clone(),offset:request.offset,limit:request.limit,sort:request.sort};
        let mut pending=||{
            if shared.current(request.generation){
                let event=SearchEvent{generation:request.generation,paths:Vec::new(),scanned:0,indexed:0,denied:0,skipped_reparse:0,scanning:true,
                    finished:false,truncated:false,error:None,reset:false,backend:BackendKind::Everything,phase:IndexPhase::Snapshot,volumes:Vec::new(),read_only:true};
                if sender.try_send(event).is_ok(){shared.notify();}
            }
        };
        // The Shell results view consumes paths only. Asking an external
        // Everything instance for unindexed size/date fields can force disk
        // metadata reads even though these values would be discarded here.
        let outcome=if let Some(error)=&request.invalid{Err(anyhow::anyhow!(error.clone()))}else{query_with_progress(&options,request.generation as u32,configured.as_deref(),||!shared.current(request.generation)||shared.cancelled(),REQUEST_FULL_PATH,&mut pending)};
        if !shared.current(request.generation){return;}
        let (paths,total,truncated,error)=match outcome{
            Ok(page)=>{let truncated=u64::from(page.offset)+(page.items.len()as u64)<page.total;(page.items.into_iter().map(|item|item.path).collect::<Vec<_>>(),page.total,truncated,None)},
            Err(error)=>(Vec::new(),0,false,Some(format!("{error:#}"))),
        };
        let batches=paths.len().div_ceil(64).max(1);
        for index in 0..batches{
            let begin=index*64;let end=(begin+64).min(paths.len());
            let mut event=SearchEvent{generation:request.generation,paths:paths.get(begin..end).unwrap_or_default().to_vec(),scanned:0,indexed:total,denied:0,skipped_reparse:0,scanning:false,
                finished:index+1==batches,truncated,error:error.clone(),reset:false,backend:BackendKind::Everything,phase:IndexPhase::Ready,volumes:Vec::new(),read_only:true};
            loop{if !shared.current(request.generation){break;}match sender.try_send(event){Ok(())=>{shared.notify();break;},Err(TrySendError::Full(value))=>{event=value;thread::sleep(Duration::from_millis(10));},Err(TrySendError::Disconnected(_))=>return,}}
        }
}

#[cfg(test)]mod tests{
    use super::*;
    use std::os::windows::fs::MetadataExt;
    #[test]fn folder_size_lookup_escapes_paths_and_requests_only_indexed_size(){
        assert_eq!(folder_size_pattern(Path::new(r"C:\资料\a[1]+.x")),Some(r"^C:\\资料\\a\[1\]\+\.x$".into()));
        assert_eq!(folder_size_pattern(Path::new(r"\\?\C:\a\b")),folder_size_pattern(Path::new(r"C:\a\b")));
        assert_eq!(folder_size_pattern(Path::new(r"\\?\UNC\server\share\x")),folder_size_pattern(Path::new(r"\\server\share\x")));
        assert!(folder_size_pattern(Path::new("relative")).is_none());
        let options=QueryOptions{text:folder_size_pattern(Path::new(r"C:\a" )).unwrap(),limit:2,..Default::default()};
        let packet=query_packet_with_search_flags(&options,HWND(123usize as*mut _),7,REQUEST_FULL_PATH|REQUEST_SIZE,0x4|0x8);
        assert_eq!(u32_at(&packet,8).unwrap(),12);assert_eq!(u32_at(&packet,20).unwrap(),0x14);
    }
    #[test]fn folder_size_unknown_and_zero_have_different_wire_values(){
        let bytes=packet(&[(r"C:\empty",0,0,1),(r"C:\unknown",u64::MAX,0,1),(r"C:\large",u32::MAX as u64+1,0,1)],3,0);
        let page=decode_page(&bytes,&QueryOptions::default()).unwrap();
        assert_eq!(page.items.iter().map(|item|item.size).collect::<Vec<_>>(),[Some(0),None,Some(u32::MAX as u64+1)]);
    }
    #[test]fn ui_query2_packet_requests_paths_without_unneeded_file_metadata(){
        let options=QueryOptions{text:"中文 query".into(),offset:10,limit:200,sort:SearchSort::NameDesc};
        let packet=query_packet(&options,HWND(0x1234usize as*mut _),7,REQUEST_FULL_PATH);
        let header=packet[..28].chunks_exact(4).map(|field|u32::from_le_bytes(field.try_into().unwrap())).collect::<Vec<_>>();
        assert_eq!(header,[0x1234,7,0,10,200,0x4,2]);
        let text=packet[28..].chunks_exact(2).map(|unit|u16::from_le_bytes(unit.try_into().unwrap())).collect::<Vec<_>>();
        assert_eq!(text.last(),Some(&0));assert_eq!(String::from_utf16(&text[..text.len()-1]).unwrap(),"中文 query");
    }
    fn packet(paths:&[(&str,u64,u64,u32)],total:u32,offset:u32)->Vec<u8>{
        let mut bytes=Vec::new();for n in [total,paths.len()as u32,offset,REQUEST_FLAGS,1]{bytes.extend(n.to_le_bytes());}
        bytes.resize(20+paths.len()*8,0);
        for(index,(path,size,modified,flags))in paths.iter().enumerate(){
            let entry=20+index*8;let data=bytes.len()as u32;bytes[entry..entry+4].copy_from_slice(&flags.to_le_bytes());bytes[entry+4..entry+8].copy_from_slice(&data.to_le_bytes());
            let units=wide(path);bytes.extend(((units.len()-1)as u32).to_le_bytes());for unit in units{bytes.extend(unit.to_le_bytes());}bytes.extend(size.to_le_bytes());bytes.extend(modified.to_le_bytes());
        }bytes
    }
    #[test]fn query2_preserves_unicode_metadata_and_page_offset(){
        let options=QueryOptions{offset:20,limit:2,..Default::default()};
        let bytes=packet(&[(r"E:\资料\中文 空格.txt",123,133000000000000000,0),(r"E:\文件夹",u64::MAX,0,1)],99,20);
        let page=decode_page(&bytes,&options).unwrap();assert_eq!(page.total,99);assert_eq!(page.offset,20);
        assert_eq!(page.items[0],SearchItem{path:PathBuf::from(r"E:\资料\中文 空格.txt"),size:Some(123),modified:Some(133000000000000000),is_dir:false});
        assert!(page.items[1].is_dir);assert_eq!(page.items[1].size,None);assert_eq!(page.items[1].modified,None);
    }
    #[test]fn query2_rejects_truncated_and_overlapping_or_wrong_page_packets(){
        let options=QueryOptions::default();let bytes=packet(&[(r"E:\文件.txt",5,10,0)],1,0);
        for end in 0..bytes.len(){assert!(decode_page(&bytes[..end],&options).is_err(),"accepted truncation at {end}");}
        let mut bad=bytes.clone();bad[24..28].copy_from_slice(&20u32.to_le_bytes());assert!(decode_page(&bad,&options).is_err());
        let mut bad=bytes.clone();bad[8..12].copy_from_slice(&1u32.to_le_bytes());assert!(decode_page(&bad,&options).is_err());
        let mut bad=bytes;bad[4..8].copy_from_slice(&2001u32.to_le_bytes());assert!(decode_page(&bad,&options).is_err());
    }
    #[test]fn query2_limits_validate_before_starting_a_process(){
        assert!(validate(&QueryOptions{text:"a\0b".into(),..Default::default()}).is_err());
        assert!(validate(&QueryOptions{limit:2001,..Default::default()}).is_err());
        assert!(validate(&QueryOptions{offset:u32::MAX,..Default::default()}).is_err());
        assert!(validate(&QueryOptions{text:"中文".repeat(256),..Default::default()}).is_ok());
        assert!(decode_page(&packet(&[],0,0),&QueryOptions::default()).unwrap().items.is_empty());
    }
    #[test]
    #[ignore="Requires isolated Everything with folder size indexing and ZSDESK_EVERYTHING_FOLDER_TEST_PATH/BYTES"]
    fn native_everything_folder_size_lookup_is_indexed_and_exact()->Result<()>{
        let path=PathBuf::from(std::env::var_os("ZSDESK_EVERYTHING_FOLDER_TEST_PATH").context("folder fixture")?);
        let expected=std::env::var("ZSDESK_EVERYTHING_FOLDER_TEST_BYTES")?.parse::<u64>()?;
        let output=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("native output")?);
        let missing=path.join("__zsdesk_folder_size_missing__");ensure!(!missing.exists(),"missing fixture exists");
        let profile=output.join("folder-size-test-profile.json");let mut session=crate::storage::default_session();session.ui.search_backend=BackendPreference::Everything;crate::storage::save(&profile,&session)?;
        let values=query_folder_sizes(&profile,&[path.clone(),missing.clone()],0x5a535a31,||false)?;
        ensure!(values==vec![(path,Some(expected)),(missing,None)],"indexed folder size/unknown mismatch: {values:?}");
        Ok(())
    }
    #[test]
    #[ignore="Requires an isolated Everything instance and fixture query"]
    fn native_everything_unicode_ipc()->Result<()>{
        let text=std::env::var("ZSDESK_EVERYTHING_TEST_QUERY")?;let expected=PathBuf::from(std::env::var_os("ZSDESK_EVERYTHING_TEST_PATH").context("fixture")?);
        let(paths,total,truncated)=query(&text,0x5a534455,None,||false)?;
        assert!(paths.iter().any(|p|crate::explorer_forwarder::same_path(p,&expected)));assert!(!truncated);
        println!("{}",serde_json::json!({"state":"passed","matches":paths.len(),"total":total,"fixture":expected,"backend":"Everything Unicode QUERY2 IPC"}));Ok(())
    }
    #[test]
    #[ignore="Requires isolated Everything with two or more fixtures, indexed size/date metadata"]
    fn native_everything_query2_pages_sort_and_metadata()->Result<()>{
        let text=std::env::var("ZSDESK_EVERYTHING_TEST_QUERY")?;
        let first=query_with(&QueryOptions{text:text.clone(),limit:1,..Default::default()},0x5a534461,None,||false)?;
        ensure!(first.total>=2&&first.items.len()==1,"at least two matching fixtures required");
        let second=query_with(&QueryOptions{text:text.clone(),offset:1,limit:1,sort:SearchSort::NameAsc},0x5a534462,None,||false)?;
        ensure!(second.total==first.total&&second.items.len()==1&&second.items[0].path!=first.items[0].path,"paging repeated first item");
        let descending=query_with(&QueryOptions{text,offset:0,limit:PAGE_LIMIT,sort:SearchSort::NameDesc},0x5a534463,None,||false)?;
        ensure!(descending.items.last().is_some_and(|item|same_file(item,&first.items[0])),"descending name order disagrees with first page");
        for item in first.items.iter().chain(&second.items){if item.is_dir{continue;}let metadata=fs::metadata(&item.path)?;ensure!(item.size==Some(metadata.len()),"indexed size mismatch");ensure!(item.modified==Some(metadata.last_write_time()),"indexed modification time mismatch");}
        println!("{}",serde_json::json!({"state":"passed","total":first.total,"paging":true,"name_descending":true,"metadata":true}));Ok(())
    }
    fn same_file(a:&SearchItem,b:&SearchItem)->bool{crate::explorer_forwarder::same_path(&a.path,&b.path)}
}
