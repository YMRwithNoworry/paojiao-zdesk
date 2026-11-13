//! Isolated fixture regressions; no user index or live search engine is changed.
use super::*;
use std::{collections::HashSet,os::windows::fs::MetadataExt};
use serde_json::json;
static SERIAL:AtomicU64=AtomicU64::new(1);
struct Fixture{root:PathBuf,files:PathBuf,index:PathBuf,marker:String}
impl Fixture{
    fn new(base:&Path)->Result<Self>{
        ensure!(base.is_absolute(),"absolute fixture root required");fs::create_dir_all(base)?;
        let marker=format!("{}-{}",std::process::id(),SERIAL.fetch_add(1,Ordering::Relaxed));let root=base.join(format!("zsdesk-native-search-{marker}"));fs::create_dir(&root)?;
        fs::write(root.join("owner"),marker.as_bytes())?;let root=path_key_native(&fs::canonicalize(root)?);let files=root.join("files");fs::create_dir(&files)?;
        Ok(Self{index:root.join("index.zsi"),root,files,marker})
    }
    fn write(&self,name:&str,data:&[u8])->Result<PathBuf>{let path=self.files.join(name);fs::create_dir_all(path.parent().unwrap())?;fs::write(&path,data)?;Ok(path)}
}
impl Drop for Fixture{fn drop(&mut self){
    if self.root.is_absolute()&&fs::symlink_metadata(&self.root).is_ok_and(|m|m.file_attributes()&0x400==0)
        &&fs::read(self.root.join("owner")).is_ok_and(|bytes|bytes==self.marker.as_bytes()) {let _=fs::remove_dir_all(&self.root);}
}}
fn shared()->Shared{Shared{control:Mutex::new(Control::default()),wake:Condvar::new(),stop:AtomicBool::new(false),engine_stop:AtomicBool::new(false),generation:AtomicU64::new(1),
    notified:AtomicBool::new(false),hwnd:0,message:0,notifier:None,maintenance_role:AtomicU64::new(0)}}
fn direct(core:&native::Core,fixture:&Fixture,options:SearchOptions,text:&str,offset:u32,limit:u32,sort:everything::SearchSort)->Result<(Vec<PathBuf>,SearchEvent)>{
    let shared=shared();let(sender,receiver)=mpsc::sync_channel(128);let request=Request{generation:1,query:text.into(),preferred:None,invalid:None,offset,limit,sort};
    native::execute(core,&options,Some(std::slice::from_ref(&fixture.files)),&shared,&sender,&request)?;
    let mut paths=Vec::new();let mut last=None;for event in receiver.try_iter(){paths.extend(event.paths.clone());if event.finished{last=Some(event);}}
    let last=last.context("missing final page")?;ensure!(last.error.is_none(),"{:?}",last.error);Ok((paths,last))
}
fn stop(mut service:FastSearch){service.shared.stop.store(true,Ordering::Release);service.shared.engine_stop.store(true,Ordering::Release);service.shared.wake.notify_all();
    if let Some(worker)=service.worker.take(){worker.join().expect("search worker panicked");}}
fn collect(service:&FastSearch,generation:u64)->Result<(Vec<PathBuf>,SearchEvent)>{
    let deadline=Instant::now()+Duration::from_secs(30);let mut paths=Vec::new();
    loop{for event in service.poll(){if event.generation!=generation{continue;}paths.extend(event.paths.clone());if event.finished{ensure!(event.error.is_none(),"{:?}",event.error);return Ok((paths,event));}}
        ensure!(Instant::now()<deadline,"native search timeout");thread::sleep(Duration::from_millis(5));}
}

#[test]fn native_directory_pages_filters_metadata_and_pinyin()->Result<()>{
    let fixture=Fixture::new(&std::env::temp_dir())?;fixture.write("alpha.txt",b"a")?;let beta=fixture.write("beta.pdf",b"bbbb")?;
    fixture.write("中国人.txt",b"cc")?;let child=fixture.write("sub/child.pdf",b"123456789")?;fixture.write("node_modules/hidden.pdf",b"ignored")?;
    let core=native::shared(&fixture.index);let options=SearchOptions::default();
    let (paths,page)=direct(&core,&fixture,options,"ext:pdf",0,1,everything::SearchSort::NameAsc)?;assert_eq!(paths,vec![beta.clone()]);assert_eq!(page.indexed,2);assert!(page.truncated);
    let (paths,page)=direct(&core,&fixture,options,"ext:pdf",1,1,everything::SearchSort::NameAsc)?;assert_eq!(paths,vec![child.clone()]);assert!(!page.truncated);
    let (paths,_)=direct(&core,&fixture,options,"ext:pdf",0,200,everything::SearchSort::SizeDesc)?;assert_eq!(paths,vec![child.clone(),beta]);
    assert!(!native::inspect(&core).3,"disabled metadata allocated full columns");
    let scope=format!("\"{}\\\" ext:pdf",fixture.files.join("sub").display());assert_eq!(direct(&core,&fixture,options,&scope,0,200,everything::SearchSort::NameAsc)?.0,vec![child]);
    assert!(direct(&core,&fixture,options,"zgr",0,200,everything::SearchSort::NameAsc)?.0.is_empty());
    let pinyin=SearchOptions{pinyin_initials:true,..options};assert_eq!(direct(&core,&fixture,pinyin,"zgr",0,200,everything::SearchSort::NameAsc)?.0.len(),1);
    let all=SearchOptions{include_development:true,index_metadata:true,..options};assert_eq!(direct(&core,&fixture,all,"ext:pdf",0,200,everything::SearchSort::NameAsc)?.1.indexed,3);assert!(native::inspect(&core).3);
    Ok(())
}

#[test]fn native_async_latest_request_and_shared_catalog()->Result<()>{
    let fixture=Fixture::new(&std::env::temp_dir())?;let alpha=fixture.write("alpha.txt",b"a")?;let beta=fixture.write("beta.txt",b"b")?;
    let first=FastSearch::new_with_roots_for_test(fixture.index.clone(),vec![fixture.files.clone()])?;
    let second=FastSearch::new_with_roots_for_test(fixture.index.clone(),vec![fixture.files.clone()])?;
    let stale=first.start("alpha",None);let current=first.start("beta",None);assert!(current>stale);
    assert_eq!(collect(&first,current)?.0,vec![beta]);let other=second.start("alpha",None);assert_eq!(collect(&second,other)?.0,vec![alpha]);
    let a=native::shared(&fixture.index);let b=native::shared(&fixture.index);assert!(Arc::ptr_eq(&a,&b));
    // Live USN catchup may legitimately rebuild after unrelated filesystem changes.
    // An explicit rebuild by the second client must update the shared catalog once.
    let builds=native::inspect(&a).2;assert!(builds>=1);let rebuilt=second.rebuild();collect(&second,rebuilt)?;
    assert_eq!(native::inspect(&a).2,builds+1);assert_eq!(native::inspect(&a).0,3);
    first.cancel();assert!(first.poll().is_empty());stop(first);stop(second);Ok(())
}

#[test]fn native_incremental_survives_deleted_and_unreadable_directories()->Result<()>{
    let fixture=Fixture::new(&std::env::temp_dir())?;fixture.write("keep.txt",b"k")?;
    let core=native::shared(&fixture.index);let options=SearchOptions::default();
    let all=|core:&native::Core|direct(core,&fixture,options,"*",0,200,everything::SearchSort::NameAsc).map(|(paths,_)|paths);
    all(&core)?;
    // Without a readable journal every search rescans; there is no incremental pass to verify.
    if !native::checkpoints(&core).iter().all(|c|c.2){return Ok(());}
    let builds=native::inspect(&core).2;
    // The journal names a parent that no longer exists; OpenFileById reports it as an invalid parameter.
    let gone=fixture.files.join("gone");fs::create_dir(&gone)?;fs::write(gone.join("inner.txt"),b"x")?;fs::remove_file(gone.join("inner.txt"))?;fs::remove_dir(&gone)?;
    let added=fixture.write("added.txt",b"a")?;let paths=all(&core)?;
    assert!(paths.contains(&added)&&!paths.iter().any(|path|path.starts_with(&gone)),"incremental result incorrect: {paths:?}");
    assert_eq!(native::inspect(&core).2,builds,"a deleted directory rebuilt the catalog");
    // A changed directory whose attributes are readable but whose entries are not.
    struct Unlock(PathBuf);impl Drop for Unlock{fn drop(&mut self){let _=std::process::Command::new("icacls").arg(&self.0).args(["/remove:d","*S-1-1-0"]).output();}}
    let locked=fixture.files.join("locked");fs::create_dir(&locked)?;fs::write(locked.join("hidden.txt"),b"h")?;let _unlock=Unlock(locked.clone());
    let denied=std::process::Command::new("icacls").arg(&locked).args(["/deny","*S-1-1-0:(RD)"]).output().is_ok_and(|output|output.status.success())&&fs::read_dir(&locked).is_err();
    if denied{
        let later=fixture.write("later.txt",b"l")?;let paths=all(&core)?;
        assert!(paths.contains(&later)&&paths.contains(&added),"incremental result incorrect: {paths:?}");
        assert_eq!(native::inspect(&core).2,builds,"an unreadable directory rebuilt the catalog");
    }
    Ok(())
}

#[test]fn native_failed_build_is_not_retried_by_every_search()->Result<()>{
    let fixture=Fixture::new(&std::env::temp_dir())?;let late=fixture.root.join("late");let core=native::shared(&fixture.index);
    let search=||{let shared=shared();let(sender,_receiver)=mpsc::sync_channel(128);
        let request=Request{generation:1,query:"*".into(),preferred:None,invalid:None,offset:0,limit:10,sort:everything::SearchSort::NameAsc};
        native::execute(&core,&SearchOptions::default(),Some(std::slice::from_ref(&late)),&shared,&sender,&request)};
    assert!(search().is_err());fs::create_dir(&late)?;fs::write(late.join("a.txt"),b"a")?;
    assert!(search().is_err(),"a failed location was walked again by the next search");
    // An explicit refresh is the user's way to retry at once.
    native::refresh(&core);search()?;assert_eq!(native::inspect(&core).2,1);Ok(())
}

#[test]fn builtin_preference_blocks_everything_entry_points()->Result<()>{
    let fixture=Fixture::new(&std::env::temp_dir())?;let profile=fixture.root.join("explorer-session.json");let session=crate::storage::default_session();crate::storage::save(&profile,&session)?;
    assert!(require_everything(&profile).is_err());assert!(everything::open_settings(&profile).is_err());assert!(everything::enable_service(&profile).is_err());
    assert!(everything::query_folder_sizes(&profile,&[],1,||false).is_err());assert!(everything::query_page(&profile,&everything::QueryOptions::default(),1,||false).is_err());
    assert!(!fixture.index.exists());Ok(())
}

struct TestToken(windows::Win32::Foundation::HANDLE);
impl Drop for TestToken{fn drop(&mut self){let _=unsafe{windows::Win32::Foundation::CloseHandle(self.0)};}}
struct OrdinaryToken{
    active:bool,
    previous:Option<TestToken>,
    mode:&'static str,
    // Impersonation belongs to this thread, including its restoration guard.
    _thread:std::marker::PhantomData<std::rc::Rc<()>>,
}
impl OrdinaryToken{
    fn enter()->Result<Self>{
        use windows::Win32::{Foundation::{HANDLE,ERROR_NO_TOKEN,ERROR_NO_SUCH_LOGON_SESSION},
            Security::{GetTokenInformation,TokenLinkedToken,TOKEN_LINKED_TOKEN,TOKEN_QUERY,TOKEN_DUPLICATE,TOKEN_IMPERSONATE,
                ImpersonateLoggedOnUser,CreateRestrictedToken,CreateWellKnownSid,WinBuiltinAdministratorsSid,
                SID_AND_ATTRIBUTES,PSID,SECURITY_MAX_SID_SIZE,DISABLE_MAX_PRIVILEGE},
            System::Threading::{GetCurrentProcess,GetCurrentThread,OpenProcessToken,OpenThreadToken},UI::Shell::IsUserAnAdmin};
        if !unsafe{IsUserAnAdmin()}.as_bool(){return Ok(Self{active:false,previous:None,mode:"existing-non-admin-token",_thread:Default::default()});}
        let access=TOKEN_QUERY|TOKEN_DUPLICATE|TOKEN_IMPERSONATE;
        let mut prior=HANDLE::default();
        let previous=match unsafe{OpenThreadToken(GetCurrentThread(),access,true,&mut prior)}{
            Ok(())=>Some(TestToken(prior)),
            Err(error) if error.code()==windows::core::HRESULT::from_win32(ERROR_NO_TOKEN.0)=>None,
            Err(error)=>return Err(error).context("Cannot preserve the test thread token"),
        };
        let process=if previous.is_none(){let mut handle=HANDLE::default();unsafe{OpenProcessToken(GetCurrentProcess(),access,&mut handle)}?;Some(TestToken(handle))}else{None};
        let source=previous.as_ref().or(process.as_ref()).unwrap().0;
        let mut linked=TOKEN_LINKED_TOKEN::default();let mut length=0;
        let (ordinary,mode)=match unsafe{GetTokenInformation(source,TokenLinkedToken,Some((&mut linked as*mut TOKEN_LINKED_TOKEN).cast()),std::mem::size_of_val(&linked)as u32,&mut length)}{
            Ok(())=>(TestToken(linked.LinkedToken),"linked-standard-token"),
            Err(error) if [ERROR_NO_SUCH_LOGON_SESSION,ERROR_NO_TOKEN].iter().any(|code|error.code()==windows::core::HRESULT::from_win32(code.0))=>{
                // Some administrative test hosts have no UAC-linked logon token.
                // Restrict this token copy only; it is not a real standard-user logon.
                let mut sid=[0u32;(SECURITY_MAX_SID_SIZE as usize).div_ceil(4)];let mut size=std::mem::size_of_val(&sid)as u32;
                let admin=PSID(sid.as_mut_ptr().cast());unsafe{CreateWellKnownSid(WinBuiltinAdministratorsSid,None,Some(admin),&mut size)}?;
                let disabled=[SID_AND_ATTRIBUTES{Sid:admin,Attributes:0}];let mut restricted=HANDLE::default();
                unsafe{CreateRestrictedToken(source,DISABLE_MAX_PRIVILEGE,Some(&disabled),None,None,&mut restricted)}
                    .context("Cannot create the non-admin test token")?;
                (TestToken(restricted),"restricted-token-without-admin-or-privileges")
            },
            Err(error)=>return Err(error).context("Cannot query the linked ordinary token"),
        };
        unsafe{ImpersonateLoggedOnUser(ordinary.0)}?;
        let guard=Self{active:true,previous,mode,_thread:Default::default()};
        ensure!(!unsafe{IsUserAnAdmin()}.as_bool(),"test did not enter an ordinary token");Ok(guard)
    }
}
impl Drop for OrdinaryToken{fn drop(&mut self){if self.active{
    if let Err(error)=unsafe{windows::Win32::System::Threading::SetThreadToken(None,self.previous.as_ref().map(|token|token.0))}{
        eprintln!("Cannot restore the test thread token: {error}");std::process::abort();
    }
}}}

#[test]
#[ignore="Ordinary-token USN create/rename/delete/hard-link and idle snapshot resume; isolated ZSDESK_NATIVE_TEST_OUTPUT only"]
fn native_builtin_unprivileged_usn_snapshot_resume()->Result<()>{
    let output=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("isolated output required")?).join("builtin-search");fs::create_dir_all(&output)?;
    let fixture=Fixture::new(&output)?;let token=OrdinaryToken::enter()?;let source=fixture.write("docs/甲.txt",b"original")?;fixture.write("removed.txt",b"remove")?;
    let root=native_scan::volume_root(&fixture.files).context("NTFS volume")?;let stop=AtomicBool::new(false);let mut journal=ntfs::Volume::open_changes(&root)?;
    let start=journal.journal(&stop)?;journal.changes(start.next,start.id,&stop)?;
    let raw_error=match ntfs::Volume::open(&root){Ok(_)=>anyhow::bail!("non-admin test token unexpectedly opened the raw volume"),Err(error)=>error};
    let raw_denied=raw_error.downcast_ref::<windows::core::Error>().is_some_and(|error|
        error.code()==windows::core::HRESULT::from_win32(windows::Win32::Foundation::ERROR_ACCESS_DENIED.0));
    ensure!(raw_denied,"raw volume failed for a reason other than access denied: {raw_error:#}");
    let core=native::shared(&fixture.index);let options=SearchOptions::default();
    direct(&core,&fixture,options,"*",0,200,everything::SearchSort::NameAsc)?;ensure!(native::checkpoints(&core).iter().all(|c|c.2),"USN unavailable; directory refresh is not an incremental pass");
    let builds=native::inspect(&core).2;let before=native::checkpoints(&core);
    fs::rename(fixture.files.join("docs"),fixture.files.join("renamed"))?;fs::remove_file(fixture.files.join("removed.txt"))?;
    let renamed=fixture.files.join("renamed/甲.txt");fs::hard_link(&renamed,fixture.files.join("alias.txt"))?;
    let (paths,_)=direct(&core,&fixture,options,"*",0,200,everything::SearchSort::NameAsc)?;let paths:HashSet<_>=paths.into_iter().collect();
    ensure!(paths.contains(&renamed)&&paths.contains(&fixture.files.join("alias.txt"))&&!paths.contains(&source)&&!paths.contains(&fixture.files.join("removed.txt")),"incremental identities/aliases incorrect");
    ensure!(native::inspect(&core).2==builds,"ordinary USN update rebuilt the catalog");ensure!(native::checkpoints(&core)!=before,"checkpoint did not advance");
    native::evict_if_idle(&core,0);ensure!(native::inspect(&core).0==0,"idle retained the index");
    let after_idle=fixture.write("after-idle.txt",b"new")?;let (paths,_)=direct(&core,&fixture,options,"after-idle",0,200,everything::SearchSort::NameAsc)?;
    ensure!(paths==vec![after_idle]&&native::inspect(&core).2==builds,"snapshot resume did not use ordinary USN catchup");
    native::expire_journal_for_test(&core);direct(&core,&fixture,options,"*",0,200,everything::SearchSort::NameAsc)?;ensure!(native::inspect(&core).2==builds+1,"journal identity mismatch did not rebuild");
    let report=json!({"state":"passed","ordinary_token":true,"token_mode":token.mode,"standard_user_logon_tested":token.mode!="restricted-token-without-admin-or-privileges",
        "raw_volume_open_denied":raw_denied,"initial_build":"directory traversal","incremental":"FSCTL_READ_UNPRIVILEGED_USN_JOURNAL",
        "rename_delete_hardlink":true,"idle_index_dropped":true,"snapshot_and_usn_resume":true,"journal_identity_rebuild":true,"full_volume_enumeration":false,"everything_started":false});
    drop(token);fs::write(output.join("verification.current.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");Ok(())
}

#[test]
#[ignore="One million synthetic rows and one snapshot, not a production full-volume benchmark; isolated output required"]
fn native_compact_million_rows_snapshot_and_eviction()->Result<()>{
    use metadata::{Index,VolumeIndex,DIRECTORY};
    let output=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("isolated output")?).join("builtin-scale");let fixture=Fixture::new(&output)?;
    let started=Instant::now();let mut index=Index::new(5,false);index.begin_build();
    let dirs:Vec<_>=(0..1000).map(|n|index.append(100+n,0,OsStr::new(&format!("目录{n}")),DIRECTORY)).collect::<Result<_>>()?;
    for row in 0..1_000_000u64{index.append(10_000+row,dirs[(row/1000)as usize],OsStr::new(&format!("文件-{:06}.txt",row%300_000)),0)?;}
    index.reindex();index.compact()?;index.validate()?;let heap=index.heap_bytes();ensure!(heap<160*1024*1024,"compact synthetic catalog exceeded reviewed memory budget");
    let count=index.len();let volumes=vec![VolumeIndex{root:fixture.files.clone(),serial:1,journal_id:7,checkpoint:99,has_journal:true,mft:true,denied:0,index}];
    let policy=snapshot::Policy{development:false,metadata:false};snapshot::save(&fixture.index,&policy,&volumes,&||false)?;let bytes=fs::metadata(&fixture.index)?.len();drop(volumes);
    let loaded=snapshot::load(&fixture.index,&policy,&||false)?;ensure!(loaded[0].index.len()==count&&loaded[0].index.attributes.is_none(),"scale roundtrip mismatch");drop(loaded);
    let report=json!({"state":"passed","synthetic_files":1000000,"directories":1000,"resident_column_estimate_bytes":heap,"snapshot_bytes":bytes,
        "metadata_columns_allocated":false,"elapsed_ms":started.elapsed().as_millis(),"real_full_volume_benchmark":false,"everything_started":false});
    fs::write(output.join("verification.current.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");Ok(())
}

#[test]
#[ignore="Read-only administrative MFT transport, at most two pages; requires isolated output on NTFS"]
fn native_mft_transport_bounded_read()->Result<()>{
    let output=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("isolated output")?);let root=native_scan::volume_root(&output).context("NTFS root")?;
    ensure!(unsafe{windows::Win32::UI::Shell::IsUserAnAdmin()}.as_bool(),"administrative token required for this transport check");
    let stop=AtomicBool::new(false);let mut reader=ntfs::Volume::open(&root)?;let journal=reader.journal(&stop)?;let mut cursor=0;let mut records=0;let mut pages=0;
    for _ in 0..2{let Some(page)=reader.enumerate(cursor,&stop)?else{break;};ensure!(page.next>cursor,"MFT cursor did not advance");cursor=page.next;records+=page.records.len();pages+=1;}
    ensure!(pages>0&&records>0,"MFT transport returned no records");reader.changes(journal.next,journal.id,&stop)?;
    stop.store(true,Ordering::Release);ensure!(reader.enumerate(cursor,&stop).is_err(),"pre-cancelled enumeration was accepted");
    let report=json!({"state":"passed","pages":pages,"records":records,"buffer_bytes":256*1024,"complete_volume_index":false,"journal_written":false,"cancelled":true});
    fs::create_dir_all(output.join("builtin-mft"))?;fs::write(output.join("builtin-mft/verification.current.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");Ok(())
}

#[test]
#[ignore="Backend isolation with a deliberately unavailable private Everything instance; no engine is launched"]
fn native_backend_choice_does_not_cross_start()->Result<()>{
    struct Environment(&'static str,Option<OsString>);
    impl Environment{fn set(key:&'static str,value:&OsStr)->Self{let old=std::env::var_os(key);std::env::set_var(key,value);Self(key,old)}}
    impl Drop for Environment{fn drop(&mut self){if let Some(value)=&self.1{std::env::set_var(self.0,value);}else{std::env::remove_var(self.0);}}}
    fn failed(service:&FastSearch,generation:u64)->Result<String>{let deadline=Instant::now()+Duration::from_secs(10);
        loop{for event in service.poll(){if event.generation==generation&&event.finished{return event.error.context("query unexpectedly succeeded");}}
            ensure!(Instant::now()<deadline,"backend decision timeout");thread::sleep(Duration::from_millis(5));}}
    let output=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("isolated output")?).join("backend-choice");let fixture=Fixture::new(&output)?;
    let instance=OsString::from(format!("ZSDesk.NoEngine.{}-{}",std::process::id(),SERIAL.fetch_add(1,Ordering::Relaxed)));
    let _instance=Environment::set("ZSDESK_EVERYTHING_INSTANCE",&instance);let _bundle=Environment::set("ZSDESK_EVERYTHING_BUNDLE",fixture.root.join("missing/Everything.exe").as_os_str());
    let _data=Environment::set("ZSDESK_EVERYTHING_DATA_ROOT",fixture.root.join("everything-data").as_os_str());
    let profile=fixture.root.join("explorer-session.json");let mut session=crate::storage::default_session();session.ui.search_backend=BackendPreference::Everything;crate::storage::save(&profile,&session)?;
    let service=FastSearch::new_with_options(fixture.index.clone(),HWND::default(),0,SearchOptions::default())?;
    let first=service.start("regex:^ZSDeskBackendIsolation$",None);let error=failed(&service,first)?;
    ensure!(!error.contains("自带搜索支持"),"Everything failure fell back to the native parser");ensure!(!fixture.index.exists(),"Everything created a native snapshot");
    session.ui.search_backend=BackendPreference::NativeNtfs;crate::storage::save(&profile,&session)?;
    let second=service.start("regex:^ZSDeskBackendIsolation$",None);let error=failed(&service,second)?;
    ensure!(error.contains("自带搜索支持"),"persisted native choice was overwritten by Everything");
    fs::remove_file(&profile)?;let third=service.start("ZSDeskBackendIsolation",None);let error=failed(&service,third)?;
    ensure!(error.contains("无法读取搜索后端设置"),"missing preferences silently selected another engine");stop(service);
    let report=json!({"state":"passed","persisted_selection_respected":true,"everything_failure_no_native_fallback":true,"native_syntax_no_everything_fallback":true,
        "missing_preferences_fail_closed":true,"native_snapshot_created":false,"engine_process_started":false});
    fs::write(output.join("verification.current.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");Ok(())
}
