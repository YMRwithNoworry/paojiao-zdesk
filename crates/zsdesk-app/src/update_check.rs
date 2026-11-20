//! Official release discovery and verified, edition-matched update downloads.
use anyhow::{Context,Result,ensure};
use serde::{Deserialize,Serialize};
use sha2::{Digest,Sha256};
use std::{collections::HashMap,fs,io::{Read,Write},path::{Path,PathBuf},sync::{Mutex,OnceLock},time::{Duration,Instant,SystemTime,UNIX_EPOCH}};
use std::os::windows::fs::OpenOptionsExt;
use windows::{core::{w,PCWSTR},Win32::{Foundation::{HWND,LPARAM,WPARAM},Networking::WinHttp::*,UI::WindowsAndMessaging::PostMessageW}};

const REPOSITORY:&str="https://github.com/qiu7824/zsdesk/releases/download/";
pub(crate) const NOTICE:u32=0x8000+180;
const MAX_PACKAGE:u64=512*1024*1024;
#[path="update_lanzou.rs"] mod lanzou;
fn public_folder()->Result<String>{
    let manifest=std::env::current_exe()?.with_file_name("manifest.json");
    let mut data=Vec::new();
    fs::File::open(manifest)?.take(1024*1024+1).read_to_end(&mut data)?;
    ensure!(data.len()<=1024*1024,"安装清单超过大小限制");
    let value:serde_json::Value=serde_json::from_slice(data.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&data))?;
    ensure!(value["product"]=="ZSDesk","安装清单产品不匹配");
    value["update_folder"].as_str().map(str::to_owned).context("安装清单缺少蓝奏更新目录")
}
#[derive(Clone,Debug,Deserialize)]struct FileRecord{name:String,size:u64,sha256:String}
#[derive(Deserialize)]struct FolderRelease{format:String,version:String,files:Vec<FileRecord>}
#[derive(Clone,Debug)]pub(crate) struct Release{pub tag:String,pub name:String,url:String,checksums:String,pub portable:bool,mirror:Option<FileRecord>}
pub(crate) enum Outcome{Current(String),Available(Release),Downloaded(PathBuf,bool,String)}
struct Internet(*mut core::ffi::c_void);
impl Drop for Internet{fn drop(&mut self){let _=unsafe{WinHttpCloseHandle(self.0)};}}
fn owned(handle:*mut core::ffi::c_void)->Result<Internet>{if handle.is_null(){Err(windows::core::Error::from_thread().into())}else{Ok(Internet(handle))}}
fn wide(value:&str)->Vec<u16>{value.encode_utf16().chain(Some(0)).collect()}
fn version(tag:&str)->Option<[u64;3]>{let values=tag.strip_prefix('v').unwrap_or(tag).split('.').map(str::parse::<u64>).collect::<std::result::Result<Vec<_>,_>>().ok()?;values.try_into().ok()}
fn request(url:&str,post:bool,limit:u64,mut write:impl FnMut(&[u8])->Result<()>)->Result<()>{
    let address=url.strip_prefix("https://").context(zsdesk_core::i18n::text("更新地址必须使用 HTTPS"))?;
    let (host,path)=address.split_once('/').context(zsdesk_core::i18n::text("更新地址缺少路径"))?;
    let public=host=="apix.ilanzou.com";
    let (path,form)=if post {
        let (endpoint,form)=path.split_once('?').context("蓝奏目录请求缺少参数")?;
        ensure!(public && matches!(endpoint,"unproved/recommend/list"|"unproved/share/list"),"蓝奏目录请求地址无效");
        (endpoint,Some(form.as_bytes()))
    }else{
        ensure!(matches!(host,"api.github.com"|"github.com") || (public && path.starts_with("unproved/file/redirect?")), "{}", zsdesk_core::tr_format!("更新地址不属于官方仓库"));
        (path,None)
    };
    let session=owned(unsafe{WinHttpOpen(w!("ZSDesk updater"),WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,None,None,0)})?;
    unsafe{WinHttpSetTimeouts(session.0,5000,5000,15000,15000)?;}
    let host=wide(host);let connection=owned(unsafe{WinHttpConnect(session.0,PCWSTR(host.as_ptr()),443,0)})?;
    let path=wide(&format!("/{path}"));
    let request=owned(unsafe{WinHttpOpenRequest(connection.0,if post{w!("POST")}else{w!("GET")},PCWSTR(path.as_ptr()),None,None,std::ptr::null(),WINHTTP_FLAG_SECURE)})?;
    if public {
        let encodings=WINHTTP_DECOMPRESSION_FLAG_GZIP|WINHTTP_DECOMPRESSION_FLAG_DEFLATE;
        unsafe{WinHttpSetOption(Some(request.0),WINHTTP_OPTION_DECOMPRESSION,Some(&encodings.to_ne_bytes()))?;}
    }
    let headers=wide("Accept: application/json\r\nContent-Type: application/x-www-form-urlencoded\r\nOrigin: https://www.ilanzou.com\r\nReferer: https://www.ilanzou.com/\r\n");
    let length=form.map_or(0,|body|body.len() as u32);
    unsafe{WinHttpSendRequest(request.0,post.then_some(&headers[..headers.len()-1]),form.map(|body|body.as_ptr().cast()),length,length,0)?;WinHttpReceiveResponse(request.0,std::ptr::null_mut())?;}
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
fn metadata(url:&str,post:bool)->Result<Vec<u8>>{let mut data=Vec::new();request(url,post,1024*1024,|part|{data.extend_from_slice(part);Ok(())})?;Ok(data)}
fn bytes(url:&str)->Result<Vec<u8>>{metadata(url,false)}
fn package_name(tag:&str,edition:&str,portable:bool)->Result<String>{
    ensure!(matches!(edition,"standard"|"network"|"desktop"|"full"|"themed"|"desktop-box"), "{}", zsdesk_core::tr_format!("未知的程序版本类型"));
    let suffix=if portable{".zip"}else{"-setup.exe"};
    Ok(format!("ZSDesk-{}-{edition}-x64{suffix}",tag.trim_start_matches('v')))
}
fn current_status(remote:[u64;3],current:[u64;3],tag:&str)->Option<Outcome>{
    (remote<=current).then(||Outcome::Current(if remote==current{zsdesk_core::i18n::text("当前已是最新正式版本").into()}else{zsdesk_core::tr_format!("当前本地版本较新；最新正式版为 {tag}" , tag=tag)}))
}
fn select(value:&serde_json::Value,current:[u64;3],edition:&str,portable:bool)->Result<Outcome>{
    ensure!(value["draft"]==false&&value["prerelease"]==false, "{}", zsdesk_core::tr_format!("服务未返回正式发布版本"));
    let tag=value["tag_name"].as_str().context(zsdesk_core::i18n::text("发布版本号缺失"))?;
    let remote=version(tag).context(zsdesk_core::i18n::text("发布版本号格式无效"))?;
    if let Some(outcome)=current_status(remote,current,tag){return Ok(outcome);}
    let name=package_name(tag,edition,portable)?;
    let assets=value["assets"].as_array().context(zsdesk_core::i18n::text("发布文件缺失"))?;
    let asset=|name:&str|->Result<String>{
        let found:Vec<_>=assets.iter().filter(|asset|asset["name"].as_str()==Some(name)).collect();
        ensure!(found.len()==1, "{}", zsdesk_core::tr_format!("发布文件缺失或重复：{name}" , name=name));
        let url=found[0]["browser_download_url"].as_str().context(zsdesk_core::i18n::text("下载地址缺失"))?;
        ensure!(url==format!("{REPOSITORY}{tag}/{name}"), "{}", zsdesk_core::tr_format!("发布文件地址不属于当前版本"));Ok(url.to_owned())
    };
    Ok(Outcome::Available(Release{tag:tag.into(),url:asset(&name)?,checksums:asset("SHA256SUMS.txt")?,name,portable,mirror:None}))
}
fn select_folder(body:&[u8],current:[u64;3],edition:&str,portable:bool)->Result<Outcome>{
    let manifest:FolderRelease=serde_json::from_slice(body)?;
    ensure!(manifest.format=="zsdesk-update-v1","蓝奏更新清单格式无效");
    let remote=version(&manifest.version).context("蓝奏更新版本号无效")?;
    let [a,b,c]=remote;
    ensure!(manifest.version==format!("{a}.{b}.{c}"),"蓝奏更新版本号无效");
    let tag=format!("v{}",manifest.version);
    let name=package_name(&tag,edition,portable)?;
    let mut matching=manifest.files.into_iter().filter(|file|file.name==name);
    let mut file=matching.next().context("蓝奏清单缺少当前版本类型的更新包")?;
    ensure!(matching.next().is_none()&&(1..=MAX_PACKAGE).contains(&file.size)&&file.sha256.len()==64&&file.sha256.bytes().all(|b|b.is_ascii_hexdigit()),"蓝奏清单中的更新包信息无效");
    if let Some(outcome)=current_status(remote,current,&tag){return Ok(outcome);}
    file.sha256.make_ascii_lowercase();
    Ok(Outcome::Available(Release{url:format!("{REPOSITORY}{tag}/{name}"),checksums:format!("{REPOSITORY}{tag}/SHA256SUMS.txt"),tag,name,portable,mirror:Some(file)}))
}
fn mirror_url(name:&str)->Result<String>{lanzou::resolve(&public_folder()?,name,|url|metadata(url,true))}
fn check_with(current:[u64;3],edition:&str,portable:bool,folder:impl FnOnce()->Result<Vec<u8>>,github:impl FnOnce()->Result<Vec<u8>>)->Result<Outcome>{
    match folder().and_then(|body|select_folder(&body,current,edition,portable)){
        Ok(result)=>Ok(result),
        Err(mirror_error)=>{let result=(||select(&serde_json::from_slice(&github()?)?,current,edition,portable))();
            result.with_context(||format!("蓝奏更新源不可用（{mirror_error}），GitHub 检查也失败"))}
    }
}
fn portable()->bool{
    let Ok(exe)=std::env::current_exe() else{return false};
    let Some(dir)=exe.parent() else{return false};
    if dir.join("portable.flag").is_file(){return true;}
    if !dir.join("manifest.json").is_file(){return false;}
    // ZIP distributions have no Inno uninstall record. Their profile may still use AppData.
    let Ok(entries)=fs::read_dir(dir) else{return false};
    !entries.filter_map(Result::ok).any(|entry|{
        let name=entry.file_name();let name=name.to_string_lossy().to_ascii_lowercase();
        name.strip_prefix("unins").and_then(|s|s.strip_suffix(".dat")).is_some_and(|n|n.len()==3&&n.bytes().all(|b|b.is_ascii_digit()))
            && entry.path().with_extension("exe").is_file()
    })
}
pub(crate) fn check()->Result<Outcome>{
    check_with(version(env!("CARGO_PKG_VERSION")).context(zsdesk_core::i18n::text("当前版本号无效"))?,crate::edition::key(),portable(),||bytes(&mirror_url(lanzou::MANIFEST)?),||bytes("https://api.github.com/repos/qiu7824/zsdesk/releases/latest"))
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
fn fetch_package(url:&str,expected:&str,size:Option<u64>,temporary:&Path,destination:&Path,alive:&std::sync::atomic::AtomicBool)->Result<()>{
    let active=||alive.load(std::sync::atomic::Ordering::Acquire);
    ensure!(active(),"更新已取消");
    if destination.is_file()&&size.is_none_or(|bytes|fs::metadata(destination).is_ok_and(|m|m.len()==bytes))&&hash(destination).is_ok_and(|s|s==expected){return Ok(());}
    struct Partial<'a>(&'a Path);impl Drop for Partial<'_>{fn drop(&mut self){let _=fs::remove_file(self.0);}}
    let _partial=Partial(temporary);
    let mut file=fs::File::create(temporary)?;
    let mut digest=Sha256::new();
    let mut downloaded=0u64;
    request(url,false,size.unwrap_or(MAX_PACKAGE),|part|{
        ensure!(active(),"更新已取消");file.write_all(part)?;digest.update(part);downloaded+=part.len() as u64;Ok(())
    })?;
    ensure!(size.is_none_or(|bytes|bytes==downloaded)&&format!("{:x}",digest.finalize())==expected,"更新包大小或 SHA-256 校验失败");
    file.sync_all()?;drop(file);
    ensure!(active(),"更新已取消");
    use windows::Win32::Storage::FileSystem::{MoveFileExW,MOVEFILE_REPLACE_EXISTING,MOVEFILE_WRITE_THROUGH};
    let from=wide(&temporary.to_string_lossy());let to=wide(&destination.to_string_lossy());
    unsafe{MoveFileExW(PCWSTR(from.as_ptr()),PCWSTR(to.as_ptr()),MOVEFILE_REPLACE_EXISTING|MOVEFILE_WRITE_THROUGH)}?;
    Ok(())
}
fn download_from_sources(mirror:Option<&FileRecord>,alive:&std::sync::atomic::AtomicBool,public:impl FnOnce(&FileRecord)->Result<()>,github:impl FnOnce()->Result<String>)->Result<String>{
    let error=if let Some(file)=mirror{
        match public(file){Ok(())=>return Ok(file.sha256.clone()),Err(error)=>Some(error)}
    }else{None};
    ensure!(alive.load(std::sync::atomic::Ordering::Acquire),"更新已取消");
    match error{Some(cause)=>github().with_context(||format!("蓝奏下载失败（{cause}），GitHub 下载也失败")),None=>github()}
}
pub(crate) fn download(release:&Release,alive:&std::sync::atomic::AtomicBool)->Result<Outcome>{
    ensure!(alive.load(std::sync::atomic::Ordering::Acquire),"更新已取消");
    let root=dirs::cache_dir().context(zsdesk_core::i18n::text("无法确定更新缓存目录"))?.join(crate::edition::profile_name()).join("updates");
    fs::create_dir_all(&root)?;
    let _lock=fs::OpenOptions::new().write(true).create(true).truncate(false).share_mode(0).open(root.join("download.lock")).context(zsdesk_core::i18n::text("另一个更新下载正在进行"))?;
    let destination=root.join(if release.portable{"ZSDesk-update.zip"}else{"ZSDesk-update.exe"});
    let temporary=root.join("download.part");
    let expected=download_from_sources(release.mirror.as_ref(),alive,|file|{
        fetch_package(&mirror_url(&release.name)?,&file.sha256,Some(file.size),&temporary,&destination,alive)
    },||{
        let expected=checksum(std::str::from_utf8(&bytes(&release.checksums)?)?,&release.name)?;
        fetch_package(&release.url,&expected,None,&temporary,&destination,alive)?;
        Ok(expected)
    })?;
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
    fn feed()->serde_json::Value {
        let files=["standard","network","desktop","full","themed","desktop-box"].into_iter()
            .flat_map(|edition|[false,true].map(|portable|serde_json::json!({"name":package_name("v0.4.1",edition,portable).unwrap(),"size":27,"sha256":"AB".repeat(32)}))).collect::<Vec<_>>();
        serde_json::json!({"format":"zsdesk-update-v1","version":"0.4.1","files":files})
    }
    #[test]fn public_folder_is_first_and_preserves_all_editions(){
        for edition in ["standard","network","desktop","full","themed","desktop-box"]{
            for portable in [true,false]{
                let result=check_with([0,4,0],edition,portable,||Ok(serde_json::to_vec(&feed()).unwrap()),||panic!("unexpected GitHub request")).unwrap();
                let Outcome::Available(found)=result else{panic!()};
                assert_eq!(found.name,package_name("v0.4.1",edition,portable).unwrap());
                assert_eq!(found.mirror.unwrap().sha256,"ab".repeat(32));
            }
        }
        assert!(matches!(select_folder(&serde_json::to_vec(&feed()).unwrap(),[0,4,1],"full",false).unwrap(),Outcome::Current(_)));
    }
    #[test]fn invalid_or_missing_public_file_falls_back(){
        let mut duplicate=feed();
        let entries=duplicate["files"].as_array_mut().unwrap();
        let full=entries.iter().find(|f|f["name"]=="ZSDesk-0.4.1-full-x64-setup.exe").unwrap().clone();
        entries.push(full);
        for data in [b"<html>login</html>".to_vec(),serde_json::to_vec(&duplicate).unwrap(),b"{}".to_vec()]{
            let found=check_with([0,3,15],"full",false,||Ok(data),||Ok(serde_json::to_vec(&release()).unwrap())).unwrap();
            assert!(matches!(found,Outcome::Available(r) if r.mirror.is_none()));
        }
    }
    #[test]fn folder_resolution_uses_exact_unique_names_and_public_access(){
        let directory=||serde_json::json!({"code":200,"list":[{"fileList":[{"folderId":"123"}]}]});
        let mut calls=0;
        let url=lanzou::resolve("https://www.ilanzou.com/s/demo",lanzou::MANIFEST,|_|{
            calls+=1;Ok(serde_json::to_vec(&if calls==1{directory()}else{serde_json::json!({"code":200,"list":[{"fileId":42,"fileName":"zsdesk-update.json"}]})})?)
        }).unwrap();
        assert_eq!(calls,2);
        assert!(url.starts_with("https://apix.ilanzou.com/unproved/file/redirect?"));
        assert!(url.ends_with("&shareId=demo"));
        assert!(lanzou::resolve("https://www.ilanzou.com.evil.invalid/s/demo",lanzou::MANIFEST,|_|panic!()).is_err());
        assert!(lanzou::resolve("https://www.ilanzou.com/s/demo",lanzou::MANIFEST,|_|Ok(serde_json::to_vec(&serde_json::json!({"code":200,"list":[{"amt":1,"fileList":[]}]}))?)).is_err());
    }
    #[test]fn download_retries_the_official_asset_after_mirror_failure_but_honors_cancel(){
        use std::sync::atomic::AtomicBool;
        let alive=AtomicBool::new(true);
        let file=FileRecord{name:"fixture.exe".into(),size:123,sha256:"ab".repeat(32)};
        assert_eq!(download_from_sources(Some(&file),&alive,|_|Ok(()),||panic!()).unwrap(),file.sha256);
        assert_eq!(download_from_sources(Some(&file),&alive,|_|anyhow::bail!("wrong digest"),||Ok("cd".repeat(32))).unwrap(),"cd".repeat(32));
        let stopped=download_from_sources(Some(&file),&alive,|_|{alive.store(false,std::sync::atomic::Ordering::Release);anyhow::bail!("cancelled")},||panic!());
        assert!(stopped.is_err());
    }
    #[test]#[ignore="Validates a published anonymous file without installing anything"]
    fn live_public_file(){
        let folder=std::env::var("ZSDESK_TEST_PUBLIC_FOLDER").unwrap_or_else(|_|public_folder().unwrap());
        let name=std::env::var("ZSDESK_TEST_PUBLIC_NAME").unwrap_or_else(|_|lanzou::MANIFEST.into());
        let url=lanzou::resolve(&folder,&name,|url|metadata(url,true)).unwrap();
        let body=bytes(&url).unwrap();
        let value:serde_json::Value=serde_json::from_slice(&body).unwrap();
        assert!(value["version"].as_str().is_some());
        if name==lanzou::MANIFEST{assert!(matches!(select_folder(&body,[0,0,0],"full",false).unwrap(),Outcome::Available(_)));}
        eprintln!("verified public JSON: {} bytes, SHA-256 {:x}",body.len(),Sha256::digest(&body));
    }
    #[test]#[ignore="Downloads a release from its actual anonymous folder and compares all package hashes"]
    fn live_installed_folder_all_editions() -> Result<()> {
        let installed=PathBuf::from(std::env::var_os("ZSDESK_TEST_INSTALLED_MANIFEST").context("installed manifest")?);
        let local=PathBuf::from(std::env::var_os("ZSDESK_TEST_RELEASE_DIR").context("release directory")?);
        let spec:serde_json::Value=serde_json::from_slice(&fs::read(installed)?)?;
        ensure!(spec["product"]=="ZSDesk" && spec["edition"]=="full");
        let folder=spec["update_folder"].as_str().context("installed public folder")?;
        let resolve=|name:&str|lanzou::resolve(folder,name,|url|metadata(url,true));
        let body=bytes(&resolve(lanzou::MANIFEST)?)?;
        ensure!(body==fs::read(local.join(lanzou::MANIFEST))?,"published manifest differs");
        let alive=std::sync::atomic::AtomicBool::new(true);
        for edition in ["full","standard","network","desktop","themed","desktop-box"]{
            for portable in [false,true]{
                let outcome=check_with([0,4,0],edition,portable,||Ok(body.clone()),||anyhow::bail!("unexpected GitHub fallback"))?;
                let Outcome::Available(release)=outcome else{anyhow::bail!("missing update")};
                let sample=std::env::temp_dir().join(format!("zsdesk-live-{}-{}",std::process::id(),release.name));
                let partial=sample.with_extension("part");
                ensure!(!sample.exists()&&!partial.exists());
                struct Sample(PathBuf);impl Drop for Sample{fn drop(&mut self){let _=fs::remove_file(&self.0);}}
                let _sample=Sample(sample.clone());
                let sum=download_from_sources(release.mirror.as_ref(),&alive,|file|{
                    fetch_package(&resolve(&file.name)?,&file.sha256,Some(file.size),&partial,&sample,&alive)
                },||anyhow::bail!("blue line did not deliver {}",release.name))?;
                ensure!(sum==hash(&local.join(&release.name))? && sum==hash(&sample)?);
                eprintln!("Verified Lanzou: {} {} {sum}",release.name,fs::metadata(&sample)?.len());
            }
        }
        Ok(())
    }
    #[test]
    #[ignore="Read-only network verification of GitHub fallback and the official installer digest"]
    fn live_github_fallback_download(){
        let actual=(||->Result<()>{
            let current=[0,0,0];
            let result=check_with(current,"full",false,||anyhow::bail!("test: folder unavailable"),||bytes("https://api.github.com/repos/qiu7824/zsdesk/releases/latest"))?;
            let Outcome::Available(release)=result else{anyhow::bail!("official release missing")};
            ensure!(release.mirror.is_none(),"expected official source");
            let root=std::env::temp_dir();
            let destination=root.join(format!("zsdesk-update-qa-{}.exe",std::process::id()));
            let partial=destination.with_extension("part");
            ensure!(!destination.exists()&&!partial.exists(),"test path already exists");
            struct Cleanup(PathBuf);impl Drop for Cleanup{fn drop(&mut self){let _=fs::remove_file(&self.0);}}
            let _cleanup=Cleanup(destination.clone());
            let alive=std::sync::atomic::AtomicBool::new(true);
            let expected=checksum(std::str::from_utf8(&bytes(&release.checksums)?)?,&release.name)?;
            let mirror=FileRecord{name:release.name.clone(),size:1,sha256:"00".repeat(32)};
            let digest=download_from_sources(Some(&mirror),&alive,|_|anyhow::bail!("test: mirror bytes failed verification"),||{
                fetch_package(&release.url,&expected,None,&partial,&destination,&alive)?;
                Ok(expected.clone())
            })?;
            ensure!(hash(&destination)?==digest);
            eprintln!("official fallback verified: {} {} bytes {digest}",release.name,fs::metadata(&destination)?.len());
            Ok(())
        })();actual.unwrap();
    }
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
