//! Official release discovery and verified, edition-matched update downloads.
use anyhow::{Context,Result,ensure};
use serde::{Deserialize,Serialize};
use sha2::{Digest,Sha256};
use std::{collections::HashMap,fs,io::{Read,Write},path::{Path,PathBuf},sync::{Mutex,OnceLock},time::{Duration,Instant,SystemTime,UNIX_EPOCH}};
use std::os::windows::fs::OpenOptionsExt;
use windows::{core::{w,PCWSTR},Win32::{Foundation::{HWND,LPARAM,WPARAM},Networking::WinHttp::*,UI::WindowsAndMessaging::PostMessageW}};

const REPOSITORY:&str="https://github.com/YMRwithNoworry/paojiao-zdesk/releases/download/";
pub(crate) const NOTICE:u32=0x8000+180;
const MAX_PACKAGE:u64=512*1024*1024;
#[derive(Clone,Debug)]pub(crate) struct Release{pub tag:String,pub name:String,url:String,checksums:String,pub portable:bool}
pub(crate) enum Outcome{Current(String),Available(Release),Downloaded(PathBuf,bool,String)}
struct Internet(*mut core::ffi::c_void);
impl Drop for Internet{fn drop(&mut self){let _=unsafe{WinHttpCloseHandle(self.0)};}}
fn owned(handle:*mut core::ffi::c_void)->Result<Internet>{if handle.is_null(){Err(windows::core::Error::from_thread().into())}else{Ok(Internet(handle))}}
fn wide(value:&str)->Vec<u16>{value.encode_utf16().chain(Some(0)).collect()}
fn version(tag:&str)->Option<[u64;3]>{let values=tag.strip_prefix('v').unwrap_or(tag).split('.').map(str::parse::<u64>).collect::<std::result::Result<Vec<_>,_>>().ok()?;values.try_into().ok()}
fn get(url:&str,limit:u64,mut write:impl FnMut(&[u8])->Result<()>)->Result<()>{
    let address=url.strip_prefix("https://").context(zsdesk_core::i18n::text("更新地址必须使用 HTTPS"))?;
    let (host,path)=address.split_once('/').context(zsdesk_core::i18n::text("更新地址缺少路径"))?;
    ensure!(matches!(host,"api.github.com"|"github.com"), "{}", zsdesk_core::tr_format!("更新地址不属于官方仓库"));
    let session=owned(unsafe{WinHttpOpen(w!("ZSDesk updater"),WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,None,None,0)})?;
    unsafe{WinHttpSetTimeouts(session.0,5000,5000,15000,15000)?;}
    let host=wide(host);let connection=owned(unsafe{WinHttpConnect(session.0,PCWSTR(host.as_ptr()),443,0)})?;
    let path=wide(&format!("/{path}"));
    let request=owned(unsafe{WinHttpOpenRequest(connection.0,w!("GET"),PCWSTR(path.as_ptr()),None,None,std::ptr::null(),WINHTTP_FLAG_SECURE)})?;
    unsafe{WinHttpSendRequest(request.0,None,None,0,0,0)?;WinHttpReceiveResponse(request.0,std::ptr::null_mut())?;}
    let mut status=0u32;let mut size=4;
    unsafe{WinHttpQueryHeaders(request.0,WINHTTP_QUERY_STATUS_CODE|WINHTTP_QUERY_FLAG_NUMBER,None,Some((&mut status as*mut u32).cast()),&mut size,std::ptr::null_mut())?;}
    ensure!(status==200, "{}", zsdesk_core::tr_format!("更新服务返回 HTTP {status}" , status=status));
    let deadline=Instant::now()+Duration::from_secs(if limit>2*1024*1024{600}else{30});let mut total=0u64;
    loop{ensure!(Instant::now()<deadline, "{}", zsdesk_core::tr_format!("更新下载超时"));let mut buffer=[0u8;65536];let mut count=0;
        unsafe{WinHttpReadData(request.0,buffer.as_mut_ptr().cast(),buffer.len()as u32,&mut count)?;}
        if count==0{break;}total+=u64::from(count);ensure!(total<=limit, "{}", zsdesk_core::tr_format!("更新文件超过大小限制"));write(&buffer[..count as usize])?;
    }
    ensure!(total>0, "{}", zsdesk_core::tr_format!("更新服务返回空文件"));Ok(())
}
fn bytes(url:&str)->Result<Vec<u8>>{let mut data=Vec::new();get(url,1024*1024,|part|{data.extend_from_slice(part);Ok(())})?;Ok(data)}
fn select(value:&serde_json::Value,current:[u64;3],edition:&str,portable:bool)->Result<Outcome>{
    ensure!(value["draft"]==false&&value["prerelease"]==false, "{}", zsdesk_core::tr_format!("服务未返回正式发布版本"));
    let tag=value["tag_name"].as_str().context(zsdesk_core::i18n::text("发布版本号缺失"))?;
    let remote=version(tag).context(zsdesk_core::i18n::text("发布版本号格式无效"))?;
    if remote<=current{return Ok(Outcome::Current(if remote==current{zsdesk_core::i18n::text("当前已是最新正式版本").into()}else{zsdesk_core::tr_format!("当前本地版本较新；最新正式版为 {tag}" , tag=tag)}));}
    ensure!(matches!(edition,"standard"|"network"|"desktop"|"full"|"themed"|"desktop-box"), "{}", zsdesk_core::tr_format!("未知的程序版本类型"));
    let suffix=if portable{".zip"}else{"-setup.exe"};
    let name=format!("ZSDesk-{}-{edition}-x64{suffix}",tag.trim_start_matches('v'));
    let assets=value["assets"].as_array().context(zsdesk_core::i18n::text("发布文件缺失"))?;
    let asset=|name:&str|->Result<String>{
        let found:Vec<_>=assets.iter().filter(|asset|asset["name"].as_str()==Some(name)).collect();
        ensure!(found.len()==1, "{}", zsdesk_core::tr_format!("发布文件缺失或重复：{name}" , name=name));
        let url=found[0]["browser_download_url"].as_str().context(zsdesk_core::i18n::text("下载地址缺失"))?;
        ensure!(url==format!("{REPOSITORY}{tag}/{name}"), "{}", zsdesk_core::tr_format!("发布文件地址不属于当前版本"));Ok(url.to_owned())
    };
    Ok(Outcome::Available(Release{tag:tag.into(),url:asset(&name)?,checksums:asset("SHA256SUMS.txt")?,name,portable}))
}
fn portable()->bool{std::env::current_exe().ok().and_then(|path|path.parent().map(|dir|dir.join("portable.flag").is_file())).unwrap_or(false)}
pub(crate) fn check()->Result<Outcome>{
    let value=serde_json::from_slice(&bytes("https://api.github.com/repos/YMRwithNoworry/paojiao-zdesk/releases/latest")?)?;
    select(&value,version(env!("CARGO_PKG_VERSION")).context(zsdesk_core::i18n::text("当前版本号无效"))?,crate::edition::key(),portable())
}
fn checksum(manifest:&str,name:&str)->Result<String>{
    let values:Vec<_>=manifest.lines().filter_map(|line|{
        let line=line.trim_start_matches('\u{feff}');let split=line.find(char::is_whitespace)?;
        let (digest,file)=line.split_at(split);let file=file.trim().trim_start_matches('*');
        (file==name).then_some(digest)
    }).collect();
    ensure!(values.len()==1&&values[0].len()==64&&values[0].bytes().all(|byte|byte.is_ascii_hexdigit()), "{}", zsdesk_core::tr_format!("更新文件的 SHA-256 校验值缺失或无效"));
    Ok(values[0].to_ascii_lowercase())
}
fn hash(path:&Path)->Result<String>{let mut file=fs::File::open(path)?;let mut hash=Sha256::new();let mut buffer=[0u8;65536];loop{let count=file.read(&mut buffer)?;if count==0{break;}hash.update(&buffer[..count]);}Ok(format!("{:x}",hash.finalize()))}
pub(crate) fn download(release:&Release,alive:&std::sync::atomic::AtomicBool)->Result<Outcome>{
    ensure!(alive.load(std::sync::atomic::Ordering::Acquire), "{}", zsdesk_core::tr_format!("更新已取消"));
    let expected=checksum(std::str::from_utf8(&bytes(&release.checksums)?)?,&release.name)?;
    let root=dirs::cache_dir().context(zsdesk_core::i18n::text("无法确定更新缓存目录"))?.join(crate::edition::profile_name()).join("updates");
    fs::create_dir_all(&root)?;
    let _lock=fs::OpenOptions::new().write(true).create(true).truncate(false).share_mode(0).open(root.join("download.lock")).context(zsdesk_core::i18n::text("另一个更新下载正在进行"))?;
    let destination=root.join(if release.portable{"ZSDesk-update.zip"}else{"ZSDesk-update.exe"});
    if destination.is_file()&&hash(&destination).is_ok_and(|digest|digest==expected){return Ok(Outcome::Downloaded(destination,release.portable,expected));}
    let temporary=root.join("download.part");
    struct Partial(PathBuf);impl Drop for Partial{fn drop(&mut self){let _=fs::remove_file(&self.0);}}
    let _partial=Partial(temporary.clone());let mut file=fs::File::create(&temporary)?;let mut digest=Sha256::new();
    get(&release.url,MAX_PACKAGE,|part|{ensure!(alive.load(std::sync::atomic::Ordering::Acquire), "{}", zsdesk_core::tr_format!("更新已取消"));file.write_all(part)?;digest.update(part);Ok(())})?;
    file.sync_all()?;drop(file);
    ensure!(format!("{:x}",digest.finalize())==expected, "{}", zsdesk_core::tr_format!("更新文件校验失败，请重新下载"));
    use windows::Win32::Storage::FileSystem::{MoveFileExW,MOVEFILE_REPLACE_EXISTING,MOVEFILE_WRITE_THROUGH};
    let from=wide(&temporary.to_string_lossy());let to=wide(&destination.to_string_lossy());
    unsafe{MoveFileExW(PCWSTR(from.as_ptr()),PCWSTR(to.as_ptr()),MOVEFILE_REPLACE_EXISTING|MOVEFILE_WRITE_THROUGH)}?;
    Ok(Outcome::Downloaded(destination,release.portable,expected))
}
pub(crate) fn open_download(owner:HWND,path:&Path,expected:&str)->Result<()>{
    let root=path.parent().context(zsdesk_core::i18n::text("更新目录无效"))?;
    let _lock=fs::OpenOptions::new().write(true).create(true).truncate(false).share_mode(0).open(root.join("download.lock"))?;
    ensure!(hash(path)?==expected, "{}", zsdesk_core::tr_format!("下载包已改变，请重新下载"));
    let path=wide(&path.to_string_lossy());
    let result=unsafe{windows::Win32::UI::Shell::ShellExecuteW(Some(owner),w!("open"),PCWSTR(path.as_ptr()),None,None,windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL)};
    ensure!(result.0 as isize>32, "{}", zsdesk_core::tr_format!("无法打开更新包，请稍后重试"));Ok(())
}

#[derive(Serialize,Deserialize)]#[serde(default)]struct Preferences{automatic:bool,last_check:u64}
impl Default for Preferences{fn default()->Self{Self{automatic:true,last_check:0}}}
fn preferences(profile:&Path)->Preferences{fs::read(profile.with_file_name("updates.json")).ok().and_then(|data|serde_json::from_slice(&data).ok()).unwrap_or_default()}
pub(crate) fn automatic(profile:&Path)->bool{preferences(profile).automatic}
static PREFERENCES_LOCK:Mutex<()>=Mutex::new(());
pub(crate) fn set_automatic(profile:&Path,enabled:bool)->Result<()>{let _lock=PREFERENCES_LOCK.lock().map_err(|_|anyhow::anyhow!("{}", zsdesk_core::tr_format!("更新设置锁异常")))?;let mut prefs=preferences(profile);prefs.automatic=enabled;crate::quick_search::control::atomic_write(&profile.with_file_name("updates.json"),&serde_json::to_vec(&prefs)?)?;Ok(())}
fn notices()->&'static Mutex<HashMap<PathBuf,Release>>{static NOTICES:OnceLock<Mutex<HashMap<PathBuf,Release>>>=OnceLock::new();NOTICES.get_or_init(Default::default)}
pub(crate) fn take_notice(profile:&Path)->Option<Release>{notices().lock().ok()?.remove(profile)}
pub(crate) fn start_daily(profile:&Path,window:HWND){
    static STARTED:std::sync::atomic::AtomicBool=std::sync::atomic::AtomicBool::new(false);
    if STARTED.swap(true,std::sync::atomic::Ordering::AcqRel){return;}
    let profile=profile.to_owned();let owner=window.0 as isize;
    let _=std::thread::Builder::new().name("zsdesk-update-notice".into()).spawn(move||{
        let Ok(lock)=PREFERENCES_LOCK.lock()else{return;};
        let mut prefs=preferences(&profile);let now=SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        if !prefs.automatic||(now>=prefs.last_check&&now-prefs.last_check<24*60*60){return;}
        prefs.last_check=now;
        if let Ok(bytes)=serde_json::to_vec(&prefs){let _=crate::quick_search::control::atomic_write(&profile.with_file_name("updates.json"),&bytes);}
        drop(lock);
        if let Ok(Outcome::Available(release))=check(){
            if !automatic(&profile){return;}
            if let Ok(mut notices)=notices().lock(){notices.insert(profile,release);}
            let _=unsafe{PostMessageW(Some(HWND(owner as*mut _)),NOTICE,WPARAM(0),LPARAM(0))};
        }
    });
}

#[cfg(test)]mod tests{
    use super::*;
    fn release()->serde_json::Value{serde_json::json!({"tag_name":"v0.3.16","draft":false,"prerelease":false,"assets":[
        {"name":"ZSDesk-0.3.16-full-x64-setup.exe","browser_download_url":format!("{REPOSITORY}v0.3.16/ZSDesk-0.3.16-full-x64-setup.exe")},
        {"name":"ZSDesk-0.3.16-full-x64.zip","browser_download_url":format!("{REPOSITORY}v0.3.16/ZSDesk-0.3.16-full-x64.zip")},
        {"name":"SHA256SUMS.txt","browser_download_url":format!("{REPOSITORY}v0.3.16/SHA256SUMS.txt")} ]})}
    #[test]fn selects_the_exact_edition_and_installation_type(){
        for portable in [true,false]{let Outcome::Available(selected)=select(&release(),[0,3,15],"full",portable).unwrap()else{panic!()};assert_eq!(selected.portable,portable);assert_eq!(selected.name.ends_with(".zip"),portable);}
        assert!(select(&release(),[0,3,15],"themed",false).is_err());
        assert!(matches!(select(&release(),[0,3,16],"full",false).unwrap(),Outcome::Current(_)));
    }
    #[test]fn rejects_foreign_urls_drafts_and_ambiguous_hashes(){
        let mut value=release();value["assets"][0]["browser_download_url"]="https://github.com/other/app.exe".into();assert!(select(&value,[0,3,15],"full",false).is_err());
        value=release();value["draft"]=true.into();assert!(select(&value,[0,3,15],"full",false).is_err());
        let sum="ab".repeat(32);assert_eq!(checksum(&format!("{sum}  app.exe\r\n"),"app.exe").unwrap(),sum);
        assert!(checksum(&format!("{sum}  app.exe\n{sum}  app.exe"),"app.exe").is_err());assert!(checksum("bad  app.exe","app.exe").is_err());
        assert!(version("1.0.0-beta").is_none());assert!(version("vv1.0.0").is_none());
    }
}
