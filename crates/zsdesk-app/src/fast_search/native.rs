//! Shared native catalog with ordinary-permission USN catchup and idle eviction.
use super::*;
use super::{metadata::{Index,VolumeIndex,DIRECTORY,REPARSE,EXCLUDED,BLOCKED,MARK,NONE,UNKNOWN},query::{Query,Probe},native_scan as scan};
use std::{collections::HashSet,sync::{MutexGuard,TryLockError},os::windows::fs::MetadataExt};
use windows::Win32::UI::Shell::IsUserAnAdmin;
pub(super) type Core=Arc<Mutex<Store>>;
static CORES:OnceLock<Mutex<HashMap<PathBuf,Weak<Mutex<Store>>>>>=OnceLock::new();
pub(super) struct Store{
    path:PathBuf,volumes:Option<Vec<VolumeIndex>>,policy:Option<(bool,bool)>,force:bool,dirty:bool,
    last_use:Instant,last_check:Instant,last_save:Instant,failures:Vec<VolumeStatus>,pending:bool,next_root:usize,save_retry:Option<Instant>,snapshot_warning:Option<String>,
    // Roots whose last build failed, with the time and reason, so typing does not re-walk them.
    backoff:Vec<(PathBuf,Instant,String)>,
    #[cfg(test)]builds:usize,
}
const FAILED_BUILD_RETRY:Duration=Duration::from_secs(30);
const DIRECTORY_REFRESH:Duration=Duration::from_secs(30);
const SCAN_SLICE:Duration=Duration::from_millis(200);
pub(super) fn shared(path:&Path)->Core{
    let path=path.with_extension("zsi");let mut cores=CORES.get_or_init(Default::default).lock().unwrap_or_else(|e|e.into_inner());
    if let Some(core)=cores.get(&path).and_then(Weak::upgrade){return core;}
    cores.retain(|_,core|core.strong_count()>0);let core=Arc::new(Mutex::new(Store{path:path.clone(),volumes:None,policy:None,force:false,dirty:false,
        last_use:Instant::now(),last_check:Instant::now(),last_save:Instant::now(),failures:Vec::new(),pending:false,next_root:0,save_retry:None,snapshot_warning:None,backoff:Vec::new(),#[cfg(test)]builds:0}));cores.insert(path,Arc::downgrade(&core));core
}
fn lock<'a>(core:&'a Core,shared:&Shared)->Result<MutexGuard<'a,Store>>{
    loop{ensure!(!shared.cancelled(),"搜索已取消");match core.try_lock(){Ok(value)=>return Ok(value),Err(TryLockError::WouldBlock)=>thread::sleep(Duration::from_millis(10)),
        Err(TryLockError::Poisoned(error))=>{let mut value=error.into_inner();value.volumes=None;value.force=true;return Ok(value);}}}
}
pub(super) fn invalidate(core:&Core){if let Ok(mut store)=core.lock(){store.volumes=None;store.force=true;store.dirty=false;store.backoff.clear();store.last_use=Instant::now();}}
pub(super) fn refresh(core:&Core){if let Ok(mut store)=core.lock(){store.last_check=Instant::now()-DIRECTORY_REFRESH;store.backoff.clear();store.last_use=Instant::now();}}
pub(super) fn can_read_snapshot(core:&Core)->bool{
    core.try_lock().is_ok_and(|store|store.volumes.is_none()&&!store.force)
}
pub(super) fn pending(core:&Core)->bool{core.try_lock().is_ok_and(|store|store.pending)}
pub(super) fn persist(core:&Core){if let Ok(mut store)=core.try_lock(){if store.dirty{if let Err(error)=store.save(&||false){crate::window::diagnostics::record("search",&format!("native-snapshot-save-failed: {error:#}"),None);}}}}
pub(super) fn evict_if_idle(core:&Core,seconds:u32){evict(core,seconds,&||false);}
pub(super) fn evict(core:&Core,seconds:u32,cancel:&impl Fn()->bool){
    let Ok(mut store)=core.try_lock()else{return;};if store.volumes.is_none()||store.last_use.elapsed()<Duration::from_secs(u64::from(seconds)){return;}
    if store.save_retry.is_some_and(|at|Instant::now()<at){return;}
    if store.dirty{if let Err(error)=store.save(cancel){if cancel(){store.snapshot_warning=None;return;}store.save_retry=Some(Instant::now()+FAILED_BUILD_RETRY);crate::window::diagnostics::record("search",&format!("native-snapshot-save-failed: {error:#}"),None);return;}}
    store.volumes=None;store.dirty=false;store.pending=false;
}
#[cfg(test)]pub(super) fn set_idle_for_test(core:&Core,age:Duration)->Instant{
    let mut store=core.lock().unwrap();store.last_use=Instant::now()-age;store.last_use
}
#[cfg(test)]pub(super) fn last_use_for_test(core:&Core)->Instant{core.lock().unwrap().last_use}
pub(super) fn diagnostics()->serde_json::Value{
    let cores=CORES.get().map(|cores|cores.lock().unwrap_or_else(|e|e.into_inner()).values().filter_map(Weak::upgrade).collect::<Vec<_>>()).unwrap_or_default();
    serde_json::Value::Array(cores.into_iter().map(|core|match core.try_lock(){
        Ok(store)=>serde_json::json!({"busy":false,"resident":store.volumes.is_some(),"idle_milliseconds":store.last_use.elapsed().as_millis()as u64,"scan_pending":store.pending,
            "heap_bytes":store.volumes.as_ref().map_or(0,|volumes|volumes.iter().map(|v|v.index.heap_bytes()+v.pending_dirs.capacity()*8).sum::<usize>()),
            "volumes":store.volumes.as_ref().map(|volumes|volumes.iter().map(|v|serde_json::json!({"root":v.root,"historical_read_skips":v.denied,"last_pass_read_issue":v.read_issue})).collect::<Vec<_>>()),
            "rows":store.volumes.as_ref().map_or(0,|volumes|volumes.iter().map(|v|v.index.len()).sum::<usize>()),"snapshot_warning":store.snapshot_warning}),
        Err(_)=>serde_json::json!({"busy":true}),
    }).collect())
}
#[cfg(test)]
pub(super) fn warm_existing(core:&Core,options:&SearchOptions,shared:&Shared) {
    let Ok(mut store)=lock(core,shared) else {return;};
    if store.volumes.is_some(){store.last_use=Instant::now();return;}
    if store.force || !store.path.is_file(){return;}
    if let Ok(volumes)=snapshot::load(&store.path,&policy(options),&||shared.cancelled()){
        if shared.cancelled() || volumes.is_empty(){return;}
        store.pending=volumes.iter().any(|volume|!volume.pending_dirs.is_empty());
        store.volumes=Some(volumes);
        store.policy=Some((options.include_development,options.index_metadata));
        store.last_use=Instant::now();
        store.last_check=Instant::now()-DIRECTORY_REFRESH;
    }
}
fn policy(options:&SearchOptions)->snapshot::Policy{snapshot::Policy{development:options.include_development,metadata:options.index_metadata}}
impl Store{
    fn save(&mut self,cancel:&impl Fn()->bool)->Result<()>{
        let result=self.save_inner(cancel);
        self.snapshot_warning=result.as_ref().err().map(|error|zsdesk_core::tr_format!("索引快照未能保存：{error:#}；当前结果仍可使用",error=error));result
    }
    fn save_inner(&mut self,cancel:&impl Fn()->bool)->Result<()>{
        let Some(volumes)=self.volumes.as_mut()else{return Ok(());};let Some((development,metadata))=self.policy else{return Ok(());};
        for volume in volumes.iter_mut(){ensure!(!cancel(),"快照保存已取消");volume.index.compact()?;}
        snapshot::save(&self.path,&snapshot::Policy{development,metadata},volumes,cancel)?;super::mapped::invalidate(&self.path);self.dirty=false;self.save_retry=None;self.last_save=Instant::now();Ok(())
    }
    fn prepare(&mut self,options:&SearchOptions,roots:Option<&[PathBuf]>,shared:&Shared,sender:&SyncSender<SearchEvent>,load_only:bool)->Result<()>{
        let generation=shared.generation.load(Ordering::Acquire);let signature=(options.include_development,options.index_metadata);
        if self.policy!=Some(signature){self.volumes=None;self.pending=false;self.policy=Some(signature);}
        let cold=self.volumes.is_none();
        if cold&&!self.force{
            if self.path.is_file(){progress(shared,sender,&self.path,0,IndexPhase::Snapshot,false);}
            match snapshot::load(&self.path,&policy(options),&||shared.cancelled()||!shared.current(generation)){
                Ok(volumes)=>self.volumes=Some(volumes),
                Err(error)=>{if shared.cancelled()||!shared.current(generation){return Err(error);}if self.path.is_file(){crate::window::diagnostics::record("search",&format!("native-snapshot-load-failed: {error:#}"),None);}}
            }self.last_save=Instant::now();
        }
        if load_only&&!self.force&&self.volumes.as_ref().is_some_and(|volumes|!volumes.is_empty()){
            self.pending=self.volumes.as_ref().unwrap().iter().any(|volume|!volume.pending_dirs.is_empty());return Ok(());
        }
        let desired=roots.map(<[PathBuf]>::to_vec).unwrap_or_else(fixed_roots);ensure!(!desired.is_empty(),"没有可索引的本地目录");
        let mut volumes=self.volumes.take().unwrap_or_default();volumes.retain(|v|desired.iter().any(|root|same_root(root,&v.root)));
        self.failures.clear();let refresh_fallback=cold||self.last_check.elapsed()>=DIRECTORY_REFRESH;let mut changed=false;let mut rebuilt=false;
        let was_pending=self.pending||volumes.iter().any(|v|!v.pending_dirs.is_empty());let begin=Instant::now();let count=desired.len();let first=self.next_root%count;
        let outcome=(||->Result<()>{for step in 0..count {
            ensure!(!shared.cancelled()&&shared.current(generation),"索引已让出给新查询");let at=(first+step)%count;let root=desired[at].clone();
            self.next_root=(at+1)%count;
            let root=path_key_native(&std::fs::canonicalize(&root).unwrap_or(root));
            let existing=volumes.iter().position(|v|same_root(&root,&v.root));
            let valid=existing.is_some_and(|at|scan::identity(&root).is_ok_and(|(serial,id,_)|serial==volumes[at].serial&&id==volumes[at].index.ids[0]));
            if let Some(at)=existing.filter(|_|valid&&!self.force){
                if let Some((_,_,warning))=self.backoff.iter().find(|(path,at,_)|same_root(path,&root)&&at.elapsed()<FAILED_BUILD_RETRY){
                    self.failures.push(VolumeStatus{root:root.clone(),backend:BackendKind::DirectoryScan,phase:IndexPhase::Paused,indexed:volumes[at].index.len()as u64,checkpoint:None,warning:Some(warning.clone())});continue;
                }
                let volume=&mut volumes[at];volume.read_issue=None;
                if volume.pending_dirs.is_empty(){
                    if volume.has_journal{
                        match catch_up(volume,options,shared,sender,generation){Ok(updated)=>changed|=updated,Err(error)=>{
                            changed=true;if shared.cancelled()||!shared.current(generation){return Err(error);}
                            crate::window::diagnostics::record("search",&format!("native-journal-rescan: {error:#}"),None);
                            queue_full_refresh(volume,shared);
                        }}
                    }else if refresh_fallback{queue_full_refresh(volume,shared);changed=true;}
                }
                if !volume.pending_dirs.is_empty(){changed=true;if let Err(error)=resume_walk(volume,options,shared,sender,SCAN_SLICE,generation){
                    if shared.cancelled()||!shared.current(generation){return Err(error);}let warning=format!("{error:#}");self.backoff.retain(|(path,..)|!same_root(path,&root));
                    self.backoff.push((root.clone(),Instant::now(),warning.clone()));self.failures.push(VolumeStatus{root:root.clone(),backend:BackendKind::DirectoryScan,phase:IndexPhase::Paused,indexed:volume.index.len()as u64,checkpoint:None,warning:Some(warning)});
                }}
                if begin.elapsed()>=SCAN_SLICE{break;}continue;
            }
            let failed=|root:PathBuf,warning:String|VolumeStatus{root,backend:BackendKind::DirectoryScan,phase:IndexPhase::Paused,indexed:0,checkpoint:None,warning:Some(warning)};
            if existing.is_none()&&!self.force{
                if let Some((_,_,warning))=self.backoff.iter().find(|(path,at,_)|same_root(path,&root)&&at.elapsed()<FAILED_BUILD_RETRY){self.failures.push(failed(root,warning.clone()));continue;}
            }
            self.backoff.retain(|(path,..)|!same_root(path,&root));
            if let Some(at)=existing{volumes.swap_remove(at);changed=true;}
            match build_volume(&root,options,shared,sender){Ok(mut volume)=>{
                changed=true;rebuilt=true;#[cfg(test)]{self.builds+=1;}
                let resumed=resume_walk(&mut volume,options,shared,sender,SCAN_SLICE,generation);volumes.push(volume);
                if let Err(error)=resumed{if shared.cancelled()||!shared.current(generation){return Err(error);}let warning=format!("{error:#}");self.backoff.push((root.clone(),Instant::now(),warning.clone()));self.failures.push(failed(root.clone(),warning));}
            },Err(error)=>{
                if shared.cancelled(){return Err(error);}let warning=format!("{error:#}");self.backoff.push((root.clone(),Instant::now(),warning.clone()));self.failures.push(failed(root,warning));
            }}
            if begin.elapsed()>=SCAN_SLICE{break;}
        }Ok(())})();
        self.pending=volumes.iter().any(|v|!v.pending_dirs.is_empty()&&!self.backoff.iter().any(|(path,at,_)|same_root(path,&v.root)&&at.elapsed()<FAILED_BUILD_RETRY))||desired.iter().any(|root|!volumes.iter().any(|v|same_root(root,&v.root))
            &&!self.backoff.iter().any(|(path,at,_)|same_root(path,root)&&at.elapsed()<FAILED_BUILD_RETRY));
        for root in &desired{if !volumes.iter().any(|v|same_root(root,&v.root))&&!self.failures.iter().any(|v|same_root(root,&v.root)){
            if let Some((_,_,warning))=self.backoff.iter().find(|(path,at,_)|same_root(path,root)&&at.elapsed()<FAILED_BUILD_RETRY){
                self.failures.push(VolumeStatus{root:root.clone(),backend:BackendKind::DirectoryScan,phase:IndexPhase::Paused,indexed:0,checkpoint:None,warning:Some(warning.clone())});
            }
        }}
        volumes.sort_by(|a,b|a.root.cmp(&b.root));self.volumes=Some(volumes);self.force=false;self.dirty|=changed;
        if refresh_fallback{self.last_check=Instant::now();}
        // Always restore owned catalogs, including cancellation and I/O errors.
        outcome?;
        ensure!(self.volumes.as_ref().unwrap().iter().map(|v|v.index.heap_bytes()+v.pending_dirs.capacity()*8).sum::<usize>()<=metadata::MAX_HEAP_BYTES,"自带索引超过内存预算，请启用开发目录排除或关闭属性索引");
        ensure!(self.volumes.as_ref().is_some_and(|v|!v.is_empty()),"自带索引未就绪：{}",self.failures.iter().filter_map(|v|v.warning.as_deref()).collect::<Vec<_>>().join("；"));
        if self.dirty&&self.save_retry.is_none_or(|at|Instant::now()>=at)&&(rebuilt||(was_pending&&!self.pending)||self.last_save.elapsed()>=Duration::from_secs(if self.pending{30}else{300})){
            if let Err(error)=self.save(&||shared.cancelled()){if shared.cancelled(){return Err(error);}self.save_retry=Some(Instant::now()+FAILED_BUILD_RETRY);crate::window::diagnostics::record("search",&format!("native-snapshot-save-failed: {error:#}"),None);}
        }
        Ok(())
    }
}
fn same_root(a:&Path,b:&Path)->bool{path_key_native(a).as_os_str().to_string_lossy().eq_ignore_ascii_case(&path_key_native(b).to_string_lossy())}
fn progress(shared:&Shared,sender:&SyncSender<SearchEvent>,root:&Path,count:usize,phase:IndexPhase,mft:bool){
    let generation=shared.generation.load(Ordering::Acquire);let backend=if mft{BackendKind::NativeNtfs}else{BackendKind::DirectoryScan};
    let event=SearchEvent{generation,paths:Vec::new(),scanned:count as u64,indexed:count as u64,denied:0,skipped_reparse:0,scanning:true,finished:false,truncated:false,error:None,reset:false,backend,phase,
        volumes:vec![VolumeStatus{root:root.into(),backend,phase,indexed:count as u64,checkpoint:None,warning:None}],read_only:false};
    if sender.try_send(event).is_ok(){shared.notify();}
}
fn flags(attributes:u32,name:&OsStr,options:&SearchOptions)->u8{
    let directory=attributes&0x10!=0;u8::from(directory)*DIRECTORY|u8::from(attributes&0x400!=0)*REPARSE|
        u8::from(directory&&!options.include_development&&metadata::excluded_name(&name.to_string_lossy().to_lowercase()))*EXCLUDED
}
fn build_volume(root:&Path,options:&SearchOptions,shared:&Shared,sender:&SyncSender<SearchEvent>)->Result<VolumeIndex>{
    let(serial,root_id,attributes)=scan::identity(root)?;ensure!(attributes&0x400==0,"索引根目录不能是重解析链接");
    let mut journal=scan::volume_root(root).and_then(|path|ntfs::Volume::open_changes(&path).ok());
    let before=journal.as_mut().and_then(|volume|{let journal=volume.journal(&shared.engine_stop).ok()?;
        volume.changes(journal.next,journal.id,&shared.engine_stop).ok()?;Some(journal)});
    let mut volume=VolumeIndex{root:root.into(),serial,journal_id:before.map_or(0,|j|j.id),checkpoint:before.map_or(0,|j|j.next),has_journal:before.is_some(),mft:false,denied:0,read_issue:None,index:Index::new(root_id,options.index_metadata),pending_dirs:vec![root_id]};
    let whole=scan::volume_root(root).is_some_and(|path|same_root(root,&path));
    if whole&&options.backend!=BackendPreference::DirectoryScan&&unsafe{IsUserAnAdmin()}.as_bool(){
        if let Ok(mut reader)=ntfs::Volume::open(root){
            volume.index.begin_build();let mut parents=vec![0];let mut cursor=0;let mut last=Instant::now();
            let result=(||->Result<()>{while let Some(page)=reader.enumerate(cursor,&shared.engine_stop)?{
                ensure!(!shared.cancelled(),"索引已取消");cursor=page.next;
                for record in page.records{if record.id==root_id||(record.id&0x0000_ffff_ffff_ffff)<16{continue;}
                    volume.index.append(record.id,NONE,&record.name,flags(record.attributes,&record.name,options))?;parents.push(record.parent);}
                if last.elapsed()>=Duration::from_millis(200){progress(shared,sender,root,volume.index.len(),IndexPhase::Scanning,true);last=Instant::now();}
            }volume.index.link_parents(&parents)?;Ok(())})();
            if shared.cancelled(){bail!("索引已取消");}
            if result.is_ok(){volume.mft=true;volume.index.compact()?;}else{volume.index=Index::new(root_id,options.index_metadata);}
        }
    }
    // Publish the MFT catalog before supplementing aliases. Directory traversal
    // is resumable work, never an all-volume barrier before the first result.
    Ok(volume)
}
fn queue_full_refresh(volume:&mut VolumeIndex,shared:&Shared){
    if volume.pending_dirs.is_empty(){volume.pending_dirs.push(volume.index.ids[0]);}
    let journal=scan::volume_root(&volume.root).and_then(|root|ntfs::Volume::open_changes(&root).ok())
        .and_then(|mut reader|reader.journal(&shared.engine_stop).ok());
    volume.has_journal=journal.is_some();
    if let Some(journal)=journal{volume.journal_id=journal.id;volume.checkpoint=journal.next;}
}
fn resume_walk(volume:&mut VolumeIndex,options:&SearchOptions,shared:&Shared,sender:&SyncSender<SearchEvent>,budget:Duration,generation:u64)->Result<()>{
    let begin=Instant::now();let mut queued:HashSet<_>=volume.pending_dirs.iter().copied().collect();let mut visited=0usize;
    while let Some(id)=volume.pending_dirs.pop(){
        queued.remove(&id);
        if shared.cancelled()||!shared.current(generation){volume.pending_dirs.push(id);bail!("索引已让出给新查询");}
        let Some(parent)=volume.index.rows_for_id(id).into_iter().find(|row|volume.index.is_dir(*row)
            &&volume.index.flags[*row as usize]&(REPARSE|EXCLUDED|metadata::DEAD)==0)else{continue;};
        match scan_one(volume,parent,false,options,shared,generation){
            Ok(Ok(children))=>for row in children{let child=volume.index.ids[row as usize];if queued.insert(child){volume.pending_dirs.push(child);}},
            Ok(Err(error))=>{if parent==0||device_lost(&error){volume.pending_dirs.push(id);return Err(error);}volume.note_read_error(id,&error);},
            Err(error)=>{volume.pending_dirs.push(id);return Err(error);},
        }
        #[cfg(test)]CANCEL_AFTER_DIRECTORY.with(|count|{let left=count.get();if left!=usize::MAX{if left<=1{count.set(usize::MAX);if QUERY_AFTER_DIRECTORY.with(|value|value.replace(false)){shared.generation.fetch_add(1,Ordering::AcqRel);}else{shared.engine_stop.store(true,Ordering::Release);}}else{count.set(left-1);}}});
        visited+=1;if visited>=64||begin.elapsed()>=budget{break;}
    }
    if !volume.pending_dirs.is_empty(){progress(shared,sender,&volume.root,volume.index.len(),IndexPhase::Scanning,volume.mft);}
    else if volume.has_journal{catch_up(volume,options,shared,sender,generation)?;}
    Ok(())
}
#[cfg(test)]thread_local!{static CANCEL_AFTER_DIRECTORY:std::cell::Cell<usize>=const{std::cell::Cell::new(usize::MAX)};}
#[cfg(test)]thread_local!{static QUERY_AFTER_DIRECTORY:std::cell::Cell<bool>=const{std::cell::Cell::new(false)};}
#[cfg(test)]pub(super) fn cancel_after_directory_for_test(){CANCEL_AFTER_DIRECTORY.with(|count|count.set(1));}
#[cfg(test)]pub(super) fn change_query_after_directory_for_test(){QUERY_AFTER_DIRECTORY.with(|value|value.set(true));cancel_after_directory_for_test();}
fn synthetic_id(parent:u64,name:&OsStr)->u64{use std::hash::{Hash,Hasher};let mut hash=std::collections::hash_map::DefaultHasher::new();parent.hash(&mut hash);name.hash(&mut hash);hash.finish()}
// Windows reserves this directory only directly under an NTFS volume root.
fn extend_path(root:&Path,path:&Path)->bool {
    path.strip_prefix(root).ok().and_then(|relative|relative.components().next())
        .is_some_and(|part|matches!(part,std::path::Component::Normal(name) if name.eq_ignore_ascii_case("$Extend")))
}
fn reserved_walk_path(path:&Path)->bool{
    scan::volume_root(path).is_some_and(|root|extend_path(&root,path)&&ntfs::is_ntfs(&root))
}
/// The outer error ends the whole pass: cancellation or an index that cannot take the entry.
/// The inner error means only this directory could not be listed; its known rows are kept.
fn scan_one(volume:&mut VolumeIndex,parent:u32,bulk:bool,options:&SearchOptions,shared:&Shared,generation:u64)->Result<Result<Vec<u32>>>{
    let Some(path)=volume.index.path(&volume.root,parent)else{return Ok(Err(anyhow::anyhow!("目录路径已失效")));};
    if reserved_walk_path(&path){if parent!=0{volume.index.delete(parent);}return Ok(Ok(Vec::new()));}
    let old=if bulk{Vec::new()}else{volume.index.children(parent)};for row in &old{volume.index.flags[*row as usize]|=MARK;}
    let mut directories=Vec::new();let mut fatal=None;let result=scan::directory(&path,&||shared.cancelled()||!shared.current(generation),|entry|{
        if entry.name.eq_ignore_ascii_case("$Extend") && reserved_walk_path(&path.join(&entry.name)){return Ok(());}
        let bits=flags(entry.attributes,&entry.name,options);let id=if entry.id==0{synthetic_id(volume.index.ids[parent as usize],&entry.name)}else{entry.id};
        // A second path to an ancestor would recurse forever or be mistaken for a move into itself.
        if bits&DIRECTORY!=0&&volume.index.aliases_ancestor(parent,id){return Ok(());}
        let row=match if bulk{volume.index.append(id,parent,&entry.name,bits)}else{volume.index.upsert_child(id,parent,&entry.name,bits)}{
            Ok(row)=>row,Err(error)=>{fatal=Some(error);bail!("索引写入失败");}};
        volume.index.set_attributes(row,if bits&DIRECTORY==0{Some(entry.size)}else{None},Some(entry.modified));
        if bits&(DIRECTORY|REPARSE|EXCLUDED)==DIRECTORY{directories.push(row);}Ok(())
    });
    if fatal.is_some()||result.is_err(){for row in old{volume.index.flags[row as usize]&=!MARK;}
        if let Some(error)=fatal{return Err(error);}let error=result.unwrap_err();return if shared.cancelled()||!shared.current(generation){Err(error)}else{Ok(Err(error))};}
    for row in old{if volume.index.flags[row as usize]&MARK!=0{volume.index.flags[row as usize]&=!MARK;volume.index.delete(row);}}
    Ok(Ok(directories))
}
/// The volume itself went away; skipping its directories one by one would keep an empty catalog.
fn device_lost(error:&anyhow::Error)->bool{error.downcast_ref::<windows::core::Error>().is_some_and(|e|matches!(e.code().0 as u32,0x80070015|0x80070037|0x800701B1|0x8007048F))}
fn ensure_directory(volume:&mut VolumeIndex,path:&Path,options:&SearchOptions,shared:&Shared)->Result<Option<u32>>{
    let Ok(relative)=path.strip_prefix(&volume.root)else{return Ok(None);};let mut parent=0;let mut current=volume.root.clone();
    for component in relative.components(){ensure!(!shared.cancelled(),"索引已取消");let std::path::Component::Normal(name)=component else{return Ok(None);};current.push(name);
        let (_,id,attributes)=match scan::identity(&current){Ok(value)=>value,Err(_)=>return Ok(None)};let bits=flags(attributes,name,options);
        if bits&DIRECTORY==0||bits&REPARSE!=0||volume.index.aliases_ancestor(parent,id){return Ok(None);}
        parent=volume.index.upsert_child(id,parent,name,bits)?;if bits&EXCLUDED!=0{return Ok(None);}
    }Ok(Some(parent))
}
fn catch_up(volume:&mut VolumeIndex,options:&SearchOptions,shared:&Shared,sender:&SyncSender<SearchEvent>,generation:u64)->Result<bool>{
    let root=scan::volume_root(&volume.root).context("此目录不支持 USN")?;let mut reader=ntfs::Volume::open_changes(&root)?;
    let journal=reader.journal(&shared.engine_stop)?;ensure!(reader.serial==volume.serial&&journal.contains(volume.journal_id,volume.checkpoint),"卷或 USN 日志范围已改变，需要重建");
    let target=journal.next;let changed=volume.checkpoint<target;let mut last=Instant::now();
    while volume.checkpoint<target{
        ensure!(!shared.cancelled()&&shared.current(generation),"索引已让出给新查询");let page=reader.changes(volume.checkpoint,volume.journal_id,&shared.engine_stop)?;
        ensure!(page.next as i64>volume.checkpoint,"USN 游标未前进");let mut parents=HashSet::new();
        for record in page.records.into_iter().filter(|record|record.usn<target){
            volume.index.invalidate_id(record.id);
            for row in volume.index.rows_for_id(record.id){let parent=volume.index.parent[row as usize];if parent!=NONE{parents.insert(volume.index.ids[parent as usize]);}}
            parents.insert(record.parent);
        }
        for id in parents{
            ensure!(!shared.cancelled()&&shared.current(generation),"索引已让出给新查询");
            if id & 0x0000_ffff_ffff_ffff == 11 {continue;} // NTFS $Extend's reserved MFT record
            // A change that cannot be resolved or listed costs that one directory, never a volume rebuild.
            let path=match reader.path_for_id(id){Ok(path)=>path,Err(error)=>{
                if shared.cancelled()||device_lost(&error){return Err(error);}
                let known=volume.index.rows_for_id(id);
                if !known.is_empty() && known.iter().all(|row|volume.index.path(&volume.root,*row).is_some_and(|p|extend_path(&root,&p))){continue;}
                if error.downcast_ref::<windows::core::Error>().is_some_and(|e|matches!(e.code().0 as u32,0x80070005|0x80070020)){
                    for row in volume.index.rows_for_id(id){if row!=0&&volume.index.is_dir(row){volume.index.flags[row as usize]|=BLOCKED;}}
                }
                volume.note_read_error(id,&error);continue;
            }};
            let Some(path)=path else{volume.index.delete_id(id);continue;};
            if extend_path(&root,&path){continue;}
            let Some(parent)=ensure_directory(volume,&path,options,shared)?else{continue;};
            let prior:HashSet<_>=volume.index.children(parent).into_iter().filter(|row|volume.index.is_dir(*row)).collect();
            let children=match scan_one(volume,parent,false,options,shared,generation)?{Ok(children)=>children,Err(error)=>{
                if parent==0||device_lost(&error){return Err(error);}volume.note_read_error(id,&error);continue;
            }};
            for child in children{if !prior.contains(&child){let id=volume.index.ids[child as usize];if !volume.pending_dirs.contains(&id){volume.pending_dirs.push(id);}}}
        }
        volume.checkpoint=(page.next as i64).min(target);
        if last.elapsed()>=Duration::from_millis(200){progress(shared,sender,&volume.root,volume.index.len(),IndexPhase::CatchingUp,volume.mft);last=Instant::now();}
    }
    Ok(changed)
}
pub(super) fn prepare(core:&Core,options:&SearchOptions,roots:Option<&[PathBuf]>,shared:&Shared,sender:&SyncSender<SearchEvent>)->Result<()>{lock(core,shared)?.prepare(options,roots,shared,sender,false)}

pub(super) fn execute(core:&Core,options:&SearchOptions,roots:Option<&[PathBuf]>,shared:&Shared,sender:&SyncSender<SearchEvent>,request:&Request)->Result<()>{
    execute_inner(core,options,roots,shared,sender,request,false)
}
pub(super) fn execute_cached(core:&Core,options:&SearchOptions,roots:Option<&[PathBuf]>,shared:&Shared,sender:&SyncSender<SearchEvent>,request:&Request)->Result<()>{
    execute_inner(core,options,roots,shared,sender,request,true)
}
fn execute_inner(core:&Core,options:&SearchOptions,roots:Option<&[PathBuf]>,shared:&Shared,sender:&SyncSender<SearchEvent>,request:&Request,cached:bool)->Result<()>{
    let query=Query::parse(&request.query,options.pinyin_initials)?;let mut store=lock(core,shared)?;
    if !cached||store.volumes.is_none()||store.force||store.policy!=Some((options.include_development,options.index_metadata)){store.prepare(options,roots,shared,sender,cached)?;}
    ensure!(shared.current(request.generation),"搜索已取消");let scanning=store.pending;let volumes=store.volumes.as_mut().unwrap();let mut candidates=Vec::new();
    let mut scanned=0;
    let mut total=0;
    let mut last_progress=Instant::now();
    let prefix=(request.offset as usize).saturating_add(request.limit as usize);
    let trim_at=prefix.saturating_add(prefix.max(4096));
    for which in 0..volumes.len(){
        let index=&volumes[which].index;
        // Two bits encode a probe for each (name, file-or-directory) pair.
        let mut memo=vec![0u8;index.names.len().div_ceil(2)];
        let mut last_parent=None;
        for row in 1..index.len() as u32{
            if row%1024==0{ensure!(shared.current(request.generation)&&!shared.cancelled(),"搜索已取消");}
            let volume=&mut volumes[which];
            let index=&volume.index;
            if !index.searchable_with_parent(row,&mut last_parent){continue;}
            scanned+=1;
            let id=index.name[row as usize];
            let dir=index.is_dir(row);
            let slot=&mut memo[id as usize/2];
            let shift=(id%2)*4+u32::from(dir)*2;
            let mut probe=(*slot>>shift)&3;
            if probe==0{
                probe=match query.probe(index.names.folded(id),dir){Probe::No=>1,Probe::Yes=>2,Probe::Path=>3};
                *slot|=probe<<shift;
            }
            if probe==1 || (probe==3&&!index.path(&volume.root,row).is_some_and(|path|
                query.matches(index.names.folded(id),dir,&path.to_string_lossy().to_lowercase()))){continue;}
            #[cfg(test)]super::runtime_regression::observe_hit(which,row as usize);
            total+=1;
            let value=match request.sort{
                everything::SearchSort::NameAsc|everything::SearchSort::NameDesc=>0,
                sort=>{
                    ensure!(shared.current(request.generation)&&!shared.cancelled(),"读取结果属性已取消");
                    let value=sort_attribute(volume,row,sort);
                    if last_progress.elapsed()>=Duration::from_millis(250){
                        progress(shared,sender,&volume.root,total as usize,IndexPhase::Attributes,volume.mft);
                        last_progress=Instant::now();
                    }
                    value
                }
            };
            candidates.try_reserve(1)?;
            candidates.push(RankedRow{hit:((which as u64)<<32)|u64::from(row),value});
            if candidates.len()>=trim_at{
                candidates.select_nth_unstable_by(prefix,|a,b|compare_rows(volumes,request.sort,a,b));
                candidates.truncate(prefix);
            }
        }
    }
    let begin=(request.offset as usize).min(candidates.len());
    let end=prefix.min(candidates.len());
    select_page(&mut candidates,begin,end,|a,b|compare_rows(volumes,request.sort,a,b));
    ensure!(shared.current(request.generation),"搜索已取消");
    let paths:Vec<_>=candidates[begin..end].iter().filter_map(|item|{
        let(v,row)=decode(item.hit);volumes[v].index.path(&volumes[v].root,row as u32)
    }).collect();
    drop(candidates);
    let mut statuses:Vec<_>=volumes.iter().map(|v|VolumeStatus{root:v.root.clone(),backend:if v.mft{BackendKind::NativeNtfs}else{BackendKind::DirectoryScan},phase:if v.pending_dirs.is_empty(){IndexPhase::Ready}else{IndexPhase::Scanning},indexed:v.index.len().saturating_sub(1)as u64,
        checkpoint:v.has_journal.then_some(v.checkpoint),warning:if let Some((count,last))=&v.read_issue{Some(zsdesk_core::tr_format!("{} · 最近一次索引维护跳过 {} 次读取：{}",v.root.display(),count,last))}else if !v.has_journal{Some(zsdesk_core::i18n::text("此位置无可用 USN 日志，搜索时刷新目录").into())}else{None}}).collect();
    let denied=volumes.iter().filter_map(|v|v.read_issue.as_ref().map(|(count,_)|*count)).sum();let backend=if volumes.iter().all(|v|v.mft){BackendKind::NativeNtfs}else if volumes.iter().any(|v|v.mft){BackendKind::Mixed}else{BackendKind::DirectoryScan};
    for failure in &store.failures{if let Some(status)=statuses.iter_mut().find(|status|same_root(&status.root,&failure.root)){status.phase=IndexPhase::Paused;status.warning=failure.warning.clone();}else{statuses.push(failure.clone());}}
    if let Some(warning)=&store.snapshot_warning{if let Some(status)=statuses.first_mut(){status.warning=Some(warning.clone());}}
    let error=(!store.failures.is_empty()).then(||zsdesk_core::i18n::text("部分位置暂时不可更新，请查看搜索状态").to_owned());store.last_use=Instant::now();
    if options.index_metadata&&!matches!(request.sort,everything::SearchSort::NameAsc|everything::SearchSort::NameDesc){store.dirty=true;}
    drop(store);
    let changed={let mut previous=shared.native_page.lock().unwrap_or_else(|e|e.into_inner());
        ensure!(shared.current(request.generation),"搜索已取消");
        let changed=previous.as_ref().is_none_or(|(generation,old)|*generation!=request.generation||old!=&paths);
        if changed{*previous=Some((request.generation,paths.clone()));}changed};
    let batches=if changed{paths.len().div_ceil(64).max(1)}else{1};for batch in 0..batches{let start=batch*64;let stop=(start+64).min(paths.len());
        emit(shared,sender,SearchEvent{generation:request.generation,paths:if changed{paths.get(start..stop).unwrap_or_default().to_vec()}else{Vec::new()},scanned,indexed:total,denied,skipped_reparse:0,scanning,finished:!scanning&&batch+1==batches,
            truncated:u64::from(request.offset)+(paths.len()as u64)<total,error:error.clone(),reset:changed&&batch==0,backend,phase:if scanning{IndexPhase::Scanning}else{IndexPhase::Ready},volumes:statuses.clone(),read_only:false});}
    Ok(())
}
struct RankedRow{hit:u64,value:u64}

fn sort_attribute(volume:&mut VolumeIndex,row:u32,sort:everything::SearchSort)->u64{
    let size_sort=matches!(sort,everything::SearchSort::SizeDesc|everything::SearchSort::SizeAsc);
    let known=volume.index.attributes.as_ref().map(|a|if size_sort{a.size[row as usize]}else{a.modified[row as usize]});
    if let Some(value)=known.filter(|v|*v!=UNKNOWN){return value;}
    let data=volume.index.path(&volume.root,row).and_then(|path|fs::symlink_metadata(path).ok());
    let size=data.as_ref().filter(|m|m.is_file()).map(MetadataExt::file_size);
    let modified=data.as_ref().map(MetadataExt::last_write_time);
    volume.index.set_attributes(row,size,modified);
    if size_sort{size}else{modified}.unwrap_or(UNKNOWN)
}

fn compare_rows(volumes:&[VolumeIndex],sort:everything::SearchSort,a:&RankedRow,b:&RankedRow)->std::cmp::Ordering{
    use everything::SearchSort::*;
    match sort{
        NameAsc|NameDesc=>{
            let name=|hit|{let(v,r)=decode(hit);let index=&volumes[v].index;index.names.folded(index.name[r])};
            let order=name(a.hit).cmp(name(b.hit)).then_with(||a.hit.cmp(&b.hit));
            if sort==NameDesc{order.reverse()}else{order}
        }
        SizeAsc|ModifiedAsc=>(a.value==UNKNOWN).cmp(&(b.value==UNKNOWN)).then_with(||a.value.cmp(&b.value)).then_with(||a.hit.cmp(&b.hit)),
        SizeDesc|ModifiedDesc=>(a.value==UNKNOWN).cmp(&(b.value==UNKNOWN))
            .then_with(||b.value.cmp(&a.value)).then_with(||a.hit.cmp(&b.hit)),
    }
}
fn decode(hit:u64)->(usize,usize){((hit>>32)as usize,(hit as u32)as usize)}
pub(super) fn select_page<T>(values:&mut[T],begin:usize,end:usize,mut compare:impl FnMut(&T,&T)->std::cmp::Ordering){
    if begin==end{return;}if end<values.len(){values.select_nth_unstable_by(end,|a,b|compare(a,b));}
    if begin>0{values[..end].select_nth_unstable_by(begin,|a,b|compare(a,b));}values[begin..end].sort_unstable_by(compare);
}

#[cfg(test)]pub(super) fn inspect(core:&Core)->(usize,usize,usize,bool){let store=core.lock().unwrap();let volumes=store.volumes.as_deref().unwrap_or_default();
    (volumes.iter().map(|v|v.index.len()).sum(),volumes.iter().map(|v|v.index.heap_bytes()).sum(),store.builds,volumes.iter().any(|v|v.index.attributes.is_some()))}
#[cfg(test)]pub(super) fn expire_journal_for_test(core:&Core){let mut store=core.lock().unwrap();if let Some(volume)=store.volumes.as_mut().and_then(|v|v.first_mut()){volume.journal_id^=1;}}
#[cfg(test)]pub(super) fn disable_journal_for_test(core:&Core){let mut store=core.lock().unwrap();if let Some(volumes)=store.volumes.as_mut(){for volume in volumes{volume.has_journal=false;}}}
#[cfg(test)]pub(super) fn dirty_for_test(core:&Core){core.lock().unwrap().dirty=true;}
#[cfg(test)]pub(super) fn checkpoints(core:&Core)->Vec<(u64,i64,bool)>{core.lock().unwrap().volumes.as_deref().unwrap_or_default().iter().map(|v|(v.journal_id,v.checkpoint,v.has_journal)).collect()}

#[cfg(test)]
mod metadata_scope_tests{
    use super::*;
    #[test]
    fn reserved_metadata_is_limited_to_the_volume_root_component(){
        let root=Path::new(r"E:\");
        for p in [r"E:\$Extend",r"E:\$extend\$Deleted"]{assert!(extend_path(root,Path::new(p)));}
        for p in [r"E:\docs\$Extend\$Deleted",r"E:\$Extended\private",r"D:\$Extend",r"E:\资料\拒绝访问"]{assert!(!extend_path(root,Path::new(p)));}
    }
}
