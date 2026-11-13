//! Shared native catalog with ordinary-permission USN catchup and idle eviction.
use super::*;
use super::{metadata::{Index,VolumeIndex,DIRECTORY,REPARSE,EXCLUDED,BLOCKED,MARK,NONE,UNKNOWN},query::{Query,Probe},native_scan as scan};
use std::{collections::HashSet,sync::{MutexGuard,TryLockError},os::windows::fs::MetadataExt};
use windows::Win32::UI::Shell::IsUserAnAdmin;
pub(super) type Core=Arc<Mutex<Store>>;
static CORES:OnceLock<Mutex<HashMap<PathBuf,Weak<Mutex<Store>>>>>=OnceLock::new();
pub(super) struct Store{
    path:PathBuf,volumes:Option<Vec<VolumeIndex>>,policy:Option<(bool,bool)>,force:bool,dirty:bool,
    last_use:Instant,last_check:Instant,last_save:Instant,failures:Vec<VolumeStatus>,
    // Roots whose last build failed, with the time and reason, so typing does not re-walk them.
    backoff:Vec<(PathBuf,Instant,String)>,
    #[cfg(test)]builds:usize,
}
const FAILED_BUILD_RETRY:Duration=Duration::from_secs(30);
pub(super) fn shared(path:&Path)->Core{
    let path=path.with_extension("zsi");let mut cores=CORES.get_or_init(Default::default).lock().unwrap_or_else(|e|e.into_inner());
    if let Some(core)=cores.get(&path).and_then(Weak::upgrade){return core;}
    cores.retain(|_,core|core.strong_count()>0);let core=Arc::new(Mutex::new(Store{path:path.clone(),volumes:None,policy:None,force:false,dirty:false,
        last_use:Instant::now(),last_check:Instant::now(),last_save:Instant::now(),failures:Vec::new(),backoff:Vec::new(),#[cfg(test)]builds:0}));cores.insert(path,Arc::downgrade(&core));core
}
fn lock<'a>(core:&'a Core,shared:&Shared)->Result<MutexGuard<'a,Store>>{
    loop{ensure!(!shared.cancelled(),"搜索已取消");match core.try_lock(){Ok(value)=>return Ok(value),Err(TryLockError::WouldBlock)=>thread::sleep(Duration::from_millis(10)),
        Err(TryLockError::Poisoned(error))=>{let mut value=error.into_inner();value.volumes=None;value.force=true;return Ok(value);}}}
}
pub(super) fn invalidate(core:&Core){if let Ok(mut store)=core.lock(){store.volumes=None;store.force=true;store.dirty=false;store.backoff.clear();}}
pub(super) fn refresh(core:&Core){if let Ok(mut store)=core.lock(){store.last_check=Instant::now()-Duration::from_secs(3);store.backoff.clear();}}
pub(super) fn evict_if_idle(core:&Core,seconds:u32){
    let Ok(mut store)=core.try_lock()else{return;};if store.volumes.is_none()||store.last_use.elapsed()<Duration::from_secs(u64::from(seconds)){return;}
    if store.dirty{if let Err(error)=store.save(&||false){crate::window::diagnostics::record("search","native-snapshot-save-failed",None);eprintln!("native snapshot: {error:#}");}}
    store.volumes=None;store.dirty=false;
}
fn policy(options:&SearchOptions)->snapshot::Policy{snapshot::Policy{development:options.include_development,metadata:options.index_metadata}}
impl Store{
    fn save(&mut self,cancel:&impl Fn()->bool)->Result<()>{
        let Some(volumes)=self.volumes.as_mut()else{return Ok(());};let Some((development,metadata))=self.policy else{return Ok(());};
        for volume in volumes.iter_mut(){ensure!(!cancel(),"快照保存已取消");volume.index.compact()?;}
        snapshot::save(&self.path,&snapshot::Policy{development,metadata},volumes,cancel)?;self.dirty=false;self.last_save=Instant::now();Ok(())
    }
    fn prepare(&mut self,options:&SearchOptions,roots:Option<&[PathBuf]>,shared:&Shared,sender:&SyncSender<SearchEvent>)->Result<()>{
        self.last_use=Instant::now();let signature=(options.include_development,options.index_metadata);
        if self.policy!=Some(signature){self.volumes=None;self.policy=Some(signature);}
        let cold=self.volumes.is_none();
        if cold&&!self.force{self.volumes=snapshot::load(&self.path,&policy(options),&||shared.cancelled()).ok();self.last_save=Instant::now();}
        let desired=roots.map(<[PathBuf]>::to_vec).unwrap_or_else(fixed_roots);ensure!(!desired.is_empty(),"没有可索引的本地目录");
        let mut volumes=self.volumes.take().unwrap_or_default();volumes.retain(|v|desired.iter().any(|root|same_root(root,&v.root)));
        self.failures.clear();let refresh_fallback=cold||self.last_check.elapsed()>=Duration::from_secs(2);let mut changed=false;let mut rebuilt=false;
        for root in desired {
            ensure!(!shared.cancelled(),"索引已取消");let root=path_key_native(&std::fs::canonicalize(&root).unwrap_or(root));
            let existing=volumes.iter().position(|v|same_root(&root,&v.root));
            let valid=existing.is_some_and(|at|scan::identity(&root).is_ok_and(|(serial,id,_)|serial==volumes[at].serial&&id==volumes[at].index.ids[0]));
            if let Some(at)=existing.filter(|_|valid&&!self.force){
                if volumes[at].has_journal{
                    match catch_up(&mut volumes[at],options,shared,sender){Ok(updated)=>{changed|=updated;continue;},Err(error)=>{
                        if shared.cancelled(){return Err(error);}crate::window::diagnostics::record("search","native-journal-rescan",None);
                    }}
                }else if !refresh_fallback{continue;}
            }
            let failed=|root:PathBuf,warning:String|VolumeStatus{root,backend:BackendKind::DirectoryScan,phase:IndexPhase::Paused,indexed:0,checkpoint:None,warning:Some(warning)};
            if existing.is_none()&&!self.force{
                if let Some((_,_,warning))=self.backoff.iter().find(|(path,at,_)|same_root(path,&root)&&at.elapsed()<FAILED_BUILD_RETRY){self.failures.push(failed(root,warning.clone()));continue;}
            }
            if let Some(at)=existing{volumes.swap_remove(at);}
            self.backoff.retain(|(path,..)|!same_root(path,&root));
            match build_volume(&root,options,shared,sender){Ok(volume)=>{volumes.push(volume);changed=true;rebuilt=true;#[cfg(test)]{self.builds+=1;}},Err(error)=>{
                if shared.cancelled(){return Err(error);}let warning=format!("{error:#}");self.backoff.push((root.clone(),Instant::now(),warning.clone()));self.failures.push(failed(root,warning));
            }}
        }
        ensure!(volumes.iter().map(|v|v.index.heap_bytes()).sum::<usize>()<=metadata::MAX_HEAP_BYTES,"自带索引超过内存预算，请启用开发目录排除或关闭属性索引");
        volumes.sort_by(|a,b|a.root.cmp(&b.root));self.volumes=Some(volumes);self.force=false;self.dirty|=changed;self.last_check=Instant::now();
        ensure!(self.volumes.as_ref().is_some_and(|v|!v.is_empty()),"自带索引未就绪：{}",self.failures.iter().filter_map(|v|v.warning.as_deref()).collect::<Vec<_>>().join("；"));
        if rebuilt||self.last_save.elapsed()>=Duration::from_secs(300){self.save(&||shared.cancelled())?;}
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
    let mut volume=VolumeIndex{root:root.into(),serial,journal_id:before.map_or(0,|j|j.id),checkpoint:before.map_or(0,|j|j.next),has_journal:before.is_some(),mft:false,denied:0,index:Index::new(root_id,options.index_metadata)};
    let whole=scan::volume_root(root).is_some_and(|path|same_root(root,&path));
    if whole&&options.backend!=BackendPreference::DirectoryScan&&unsafe{IsUserAnAdmin()}.as_bool(){
        if let Ok(mut reader)=ntfs::Volume::open(root){
            volume.index.begin_build();let mut parents=vec![0];let mut cursor=0;let mut last=Instant::now();
            let result=(||->Result<()>{while let Some(page)=reader.enumerate(cursor,&shared.engine_stop)?{
                ensure!(!shared.cancelled(),"索引已取消");cursor=page.next;
                for record in page.records{if record.id==root_id||(record.id&0x0000_ffff_ffff_ffff)<16{continue;}
                    volume.index.append(record.id,NONE,&record.name,flags(record.attributes,&record.name,options))?;parents.push(record.parent);}
                if last.elapsed()>=Duration::from_millis(200){progress(shared,sender,root,volume.index.len(),IndexPhase::Snapshot,true);last=Instant::now();}
            }volume.index.link_parents(&parents)?;Ok(())})();
            if shared.cancelled(){bail!("索引已取消");}
            if result.is_ok(){volume.mft=true;volume.index.compact()?;}else{volume.index=Index::new(root_id,options.index_metadata);}
        }
    }
    // MFT contains one name per FRN. A batched directory pass supplies aliases,
    // skips excluded subtrees and reconciles creates/deletes during bootstrap.
    let bulk=!volume.mft;if bulk{volume.index.begin_build();}
    walk(&mut volume,0,bulk,options,shared,sender)?;volume.index.reindex();volume.index.compact()?;
    if volume.has_journal{catch_up(&mut volume,options,shared,sender)?;}
    Ok(volume)
}
fn synthetic_id(parent:u64,name:&OsStr)->u64{use std::hash::{Hash,Hasher};let mut hash=std::collections::hash_map::DefaultHasher::new();parent.hash(&mut hash);name.hash(&mut hash);hash.finish()}
/// The outer error ends the whole pass: cancellation or an index that cannot take the entry.
/// The inner error means only this directory could not be listed; its known rows are kept.
fn scan_one(volume:&mut VolumeIndex,parent:u32,bulk:bool,options:&SearchOptions,shared:&Shared)->Result<Result<Vec<u32>>>{
    let Some(path)=volume.index.path(&volume.root,parent)else{return Ok(Err(anyhow::anyhow!("目录路径已失效")));};
    let old=if bulk{Vec::new()}else{volume.index.children(parent)};for row in &old{volume.index.flags[*row as usize]|=MARK;}
    let mut directories=Vec::new();let mut fatal=None;let result=scan::directory(&path,&||shared.cancelled(),|entry|{
        let bits=flags(entry.attributes,&entry.name,options);let id=if entry.id==0{synthetic_id(volume.index.ids[parent as usize],&entry.name)}else{entry.id};
        // A second path to an ancestor would recurse forever or be mistaken for a move into itself.
        if bits&DIRECTORY!=0&&volume.index.aliases_ancestor(parent,id){return Ok(());}
        let row=match if bulk{volume.index.append(id,parent,&entry.name,bits)}else{volume.index.upsert_child(id,parent,&entry.name,bits)}{
            Ok(row)=>row,Err(error)=>{fatal=Some(error);bail!("索引写入失败");}};
        volume.index.set_attributes(row,if bits&DIRECTORY==0{Some(entry.size)}else{None},Some(entry.modified));
        if bits&(DIRECTORY|REPARSE|EXCLUDED)==DIRECTORY{directories.push(row);}Ok(())
    });
    if fatal.is_some()||result.is_err(){for row in old{volume.index.flags[row as usize]&=!MARK;}
        if let Some(error)=fatal{return Err(error);}let error=result.unwrap_err();return if shared.cancelled(){Err(error)}else{Ok(Err(error))};}
    for row in old{if volume.index.flags[row as usize]&MARK!=0{volume.index.flags[row as usize]&=!MARK;volume.index.delete(row);}}
    Ok(Ok(directories))
}
/// The volume itself went away; skipping its directories one by one would keep an empty catalog.
fn device_lost(error:&anyhow::Error)->bool{error.downcast_ref::<windows::core::Error>().is_some_and(|e|matches!(e.code().0 as u32,0x80070015|0x80070037|0x800701B1|0x8007048F))}
fn walk(volume:&mut VolumeIndex,start:u32,bulk:bool,options:&SearchOptions,shared:&Shared,sender:&SyncSender<SearchEvent>)->Result<()>{
    let mut pending=vec![start];let mut last=Instant::now();
    while let Some(parent)=pending.pop(){ensure!(!shared.cancelled(),"索引已取消");match scan_one(volume,parent,bulk,options,shared)?{
        // One unreadable directory, whatever the reason, must not discard the rest of the volume.
        Ok(children)=>pending.extend(children),Err(error)=>{if parent==0||device_lost(&error){return Err(error);}volume.denied+=1;}
    }
        if last.elapsed()>=Duration::from_millis(200){progress(shared,sender,&volume.root,volume.index.len(),IndexPhase::Scanning,volume.mft);last=Instant::now();}
    }Ok(())
}
fn ensure_directory(volume:&mut VolumeIndex,path:&Path,options:&SearchOptions,shared:&Shared)->Result<Option<u32>>{
    let Ok(relative)=path.strip_prefix(&volume.root)else{return Ok(None);};let mut parent=0;let mut current=volume.root.clone();
    for component in relative.components(){ensure!(!shared.cancelled(),"索引已取消");let std::path::Component::Normal(name)=component else{return Ok(None);};current.push(name);
        let (_,id,attributes)=match scan::identity(&current){Ok(value)=>value,Err(_)=>return Ok(None)};let bits=flags(attributes,name,options);
        if bits&DIRECTORY==0||bits&REPARSE!=0||volume.index.aliases_ancestor(parent,id){return Ok(None);}
        parent=volume.index.upsert_child(id,parent,name,bits)?;if bits&EXCLUDED!=0{return Ok(None);}
    }Ok(Some(parent))
}
fn catch_up(volume:&mut VolumeIndex,options:&SearchOptions,shared:&Shared,sender:&SyncSender<SearchEvent>)->Result<bool>{
    let root=scan::volume_root(&volume.root).context("此目录不支持 USN")?;let mut reader=ntfs::Volume::open_changes(&root)?;
    let journal=reader.journal(&shared.engine_stop)?;ensure!(reader.serial==volume.serial&&journal.contains(volume.journal_id,volume.checkpoint),"卷或 USN 日志范围已改变，需要重建");
    let target=journal.next;let changed=volume.checkpoint<target;let mut last=Instant::now();
    while volume.checkpoint<target{
        ensure!(!shared.cancelled(),"索引已取消");let page=reader.changes(volume.checkpoint,volume.journal_id,&shared.engine_stop)?;
        ensure!(page.next as i64>volume.checkpoint,"USN 游标未前进");let mut parents=HashSet::new();
        for record in page.records.into_iter().filter(|record|record.usn<target){
            volume.index.invalidate_id(record.id);
            for row in volume.index.rows_for_id(record.id){let parent=volume.index.parent[row as usize];if parent!=NONE{parents.insert(volume.index.ids[parent as usize]);}}
            parents.insert(record.parent);
        }
        for id in parents{
            ensure!(!shared.cancelled(),"索引已取消");
            // A change that cannot be resolved or listed costs that one directory, never a volume rebuild.
            let path=match reader.path_for_id(id){Ok(path)=>path,Err(error)=>{
                if shared.cancelled()||device_lost(&error){return Err(error);}
                if error.downcast_ref::<windows::core::Error>().is_some_and(|e|matches!(e.code().0 as u32,0x80070005|0x80070020)){
                    for row in volume.index.rows_for_id(id){if row!=0&&volume.index.is_dir(row){volume.index.flags[row as usize]|=BLOCKED;}}
                }
                volume.denied+=1;continue;
            }};
            let Some(path)=path else{volume.index.delete_id(id);continue;};
            let Some(parent)=ensure_directory(volume,&path,options,shared)?else{continue;};
            let prior:HashSet<_>=volume.index.children(parent).into_iter().filter(|row|volume.index.is_dir(*row)).collect();
            let children=match scan_one(volume,parent,false,options,shared)?{Ok(children)=>children,Err(error)=>{
                if parent==0||device_lost(&error){return Err(error);}volume.denied+=1;continue;
            }};
            for child in children{if !prior.contains(&child){walk(volume,child,false,options,shared,sender)?;}}
        }
        volume.checkpoint=(page.next as i64).min(target);
        if last.elapsed()>=Duration::from_millis(200){progress(shared,sender,&volume.root,volume.index.len(),IndexPhase::CatchingUp,volume.mft);last=Instant::now();}
    }
    Ok(changed)
}
pub(super) fn prepare(core:&Core,options:&SearchOptions,roots:Option<&[PathBuf]>,shared:&Shared,sender:&SyncSender<SearchEvent>)->Result<()>{lock(core,shared)?.prepare(options,roots,shared,sender)}

pub(super) fn execute(core:&Core,options:&SearchOptions,roots:Option<&[PathBuf]>,shared:&Shared,sender:&SyncSender<SearchEvent>,request:&Request)->Result<()>{
    let query=Query::parse(&request.query,options.pinyin_initials)?;let mut store=lock(core,shared)?;store.prepare(options,roots,shared,sender)?;
    ensure!(shared.current(request.generation),"搜索已取消");let volumes=store.volumes.as_mut().unwrap();let mut hits=Vec::<u64>::new();let mut scanned=0u64;
    for (which,volume) in volumes.iter().enumerate(){
        let mut memo=vec![0u8;volume.index.names.len()*2];
        for row in 1..volume.index.len()as u32{
            if row%1024==0{ensure!(shared.current(request.generation)&&!shared.cancelled(),"搜索已取消");}
            if !volume.index.searchable(row){continue;}scanned+=1;let id=volume.index.name[row as usize];let dir=volume.index.is_dir(row);let key=id as usize*2+usize::from(dir);
            let probe=if memo[key]==0{let value=match query.probe(volume.index.names.folded(id),dir){Probe::No=>1,Probe::Yes=>2,Probe::Path=>3};memo[key]=value;value}else{memo[key]};
            let matched=probe==2||(probe==3&&volume.index.path(&volume.root,row).is_some_and(|path|query.matches(volume.index.names.folded(id),dir,&path.to_string_lossy().to_lowercase())));
            if matched{if hits.len()==hits.capacity(){hits.try_reserve(4096)?;}hits.push(((which as u64)<<32)|u64::from(row));}
        }
    }
    let total=hits.len()as u64;let begin=(request.offset as usize).min(hits.len());let end=begin.saturating_add(request.limit as usize).min(hits.len());
    let page=match request.sort{
        everything::SearchSort::NameAsc|everything::SearchSort::NameDesc=>{
            let name_cmp=|a:&u64,b:&u64|{let (av,ar)=decode(*a);let (bv,br)=decode(*b);let ai=&volumes[av].index;let bi=&volumes[bv].index;
                ai.names.folded(ai.name[ar]).cmp(bi.names.folded(bi.name[br])).then_with(||a.cmp(b))};
            let descending=request.sort==everything::SearchSort::NameDesc;
            select_page(&mut hits,begin,end,|a,b|if descending{name_cmp(b,a)}else{name_cmp(a,b)});hits[begin..end].to_vec()
        },
        sort=>{
            let mut keyed=Vec::new();keyed.try_reserve_exact(hits.len())?;let mut last=Instant::now();
            for hit in hits{ensure!(shared.current(request.generation)&&!shared.cancelled(),"读取结果属性已取消");let(v,row)=decode(hit);let volume=&mut volumes[v];
                let known=volume.index.attributes.as_ref().map(|a|if sort==everything::SearchSort::SizeDesc{a.size[row]}else{a.modified[row]}).filter(|v|*v!=UNKNOWN);
                let value=if let Some(value)=known{value}else{let data=volume.index.path(&volume.root,row as u32).and_then(|path|fs::symlink_metadata(path).ok());
                    let size=data.as_ref().filter(|m|m.is_file()).map(MetadataExt::file_size);let modified=data.as_ref().map(MetadataExt::last_write_time);
                    volume.index.set_attributes(row as u32,size,modified);if sort==everything::SearchSort::SizeDesc{size}else{modified}.unwrap_or(UNKNOWN)};
                keyed.push((hit,value));if last.elapsed()>=Duration::from_millis(250){progress(shared,sender,&volume.root,keyed.len(),IndexPhase::Attributes,volume.mft);last=Instant::now();}
            }
            select_page(&mut keyed,begin,end,|a,b|match (a.1==UNKNOWN,b.1==UNKNOWN){(true,false)=>std::cmp::Ordering::Greater,(false,true)=>std::cmp::Ordering::Less,_=>b.1.cmp(&a.1).then_with(||a.0.cmp(&b.0))});
            keyed[begin..end].iter().map(|(row,_)|*row).collect()
        }
    };
    ensure!(shared.current(request.generation),"搜索已取消");let paths:Vec<_>=page.into_iter().filter_map(|hit|{let(v,row)=decode(hit);volumes[v].index.path(&volumes[v].root,row as u32)}).collect();
    let mut statuses:Vec<_>=volumes.iter().map(|v|VolumeStatus{root:v.root.clone(),backend:if v.mft{BackendKind::NativeNtfs}else{BackendKind::DirectoryScan},phase:IndexPhase::Ready,indexed:v.index.len().saturating_sub(1)as u64,
        checkpoint:v.has_journal.then_some(v.checkpoint),warning:if v.denied>0{Some(format!("部分目录暂不可读取（{} 次）",v.denied))}else if !v.has_journal{Some("此位置无可用 USN 日志，搜索时刷新目录".into())}else{None}}).collect();
    let denied=volumes.iter().map(|v|v.denied).sum();let backend=if volumes.iter().all(|v|v.mft){BackendKind::NativeNtfs}else if volumes.iter().any(|v|v.mft){BackendKind::Mixed}else{BackendKind::DirectoryScan};
    statuses.extend(store.failures.clone());let error=(!store.failures.is_empty()).then(||"部分位置未建立索引，请查看搜索状态".to_owned());store.last_use=Instant::now();
    if options.index_metadata&&matches!(request.sort,everything::SearchSort::SizeDesc|everything::SearchSort::ModifiedDesc){store.dirty=true;}
    let batches=paths.len().div_ceil(64).max(1);for batch in 0..batches{let start=batch*64;let stop=(start+64).min(paths.len());
        emit(shared,sender,SearchEvent{generation:request.generation,paths:paths.get(start..stop).unwrap_or_default().to_vec(),scanned,indexed:total,denied,skipped_reparse:0,scanning:false,finished:batch+1==batches,
            truncated:u64::from(request.offset)+(paths.len()as u64)<total,error:error.clone(),reset:false,backend,phase:IndexPhase::Ready,volumes:statuses.clone(),read_only:false});}
    Ok(())
}
fn decode(hit:u64)->(usize,usize){((hit>>32)as usize,(hit as u32)as usize)}
fn select_page<T>(values:&mut[T],begin:usize,end:usize,mut compare:impl FnMut(&T,&T)->std::cmp::Ordering){
    if begin==end{return;}if end<values.len(){values.select_nth_unstable_by(end,|a,b|compare(a,b));}
    if begin>0{values[..end].select_nth_unstable_by(begin,|a,b|compare(a,b));}values[begin..end].sort_unstable_by(compare);
}

#[cfg(test)]pub(super) fn inspect(core:&Core)->(usize,usize,usize,bool){let store=core.lock().unwrap();let volumes=store.volumes.as_deref().unwrap_or_default();
    (volumes.iter().map(|v|v.index.len()).sum(),volumes.iter().map(|v|v.index.heap_bytes()).sum(),store.builds,volumes.iter().any(|v|v.index.attributes.is_some()))}
#[cfg(test)]pub(super) fn expire_journal_for_test(core:&Core){let mut store=core.lock().unwrap();if let Some(volume)=store.volumes.as_mut().and_then(|v|v.first_mut()){volume.journal_id^=1;}}
#[cfg(test)]pub(super) fn checkpoints(core:&Core)->Vec<(u64,i64,bool)>{core.lock().unwrap().volumes.as_deref().unwrap_or_default().iter().map(|v|(v.journal_id,v.checkpoint,v.has_journal)).collect()}
