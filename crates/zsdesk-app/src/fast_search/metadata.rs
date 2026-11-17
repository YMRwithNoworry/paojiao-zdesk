//! Compact filename columns; only result rows materialize full paths.
use std::{collections::HashMap,hash::{Hash,Hasher},ffi::{OsStr,OsString},os::windows::ffi::{OsStrExt,OsStringExt},path::{Path,PathBuf}};
use anyhow::{Result,ensure};
pub const NONE:u32=u32::MAX;
pub const DIRECTORY:u8=1;
pub const REPARSE:u8=2;
pub const DEAD:u8=4;
pub const EXCLUDED:u8=8;
pub const MARK:u8=16;
pub const BLOCKED:u8=32;
pub const UNKNOWN:u64=u64::MAX;
pub const MAX_ROWS:usize=32_000_000;
pub const MAX_HEAP_BYTES:usize=512*1024*1024;
// Compacted and restored columns have no spare capacity. Vec's default growth
// doubles every column on the first journal append, even for one changed file.
fn reserve_column<T>(column:&mut Vec<T>,additional:usize)->Result<()>{
    let required=column.len().checked_add(additional).ok_or_else(||anyhow::anyhow!("索引列超出容量"))?;
    if required>column.capacity(){
        let element=std::mem::size_of::<T>().max(1);
        let growth=(column.capacity()/8).clamp((4096/element).max(1),(1024*1024/element).max(1));
        let capacity=required.max(column.capacity().saturating_add(growth));
        column.try_reserve_exact(capacity-column.len())?;
    }
    Ok(())
}
pub struct VolumeIndex {
    pub root:PathBuf,pub serial:u32,pub journal_id:u64,pub checkpoint:i64,pub has_journal:bool,
    pub mft:bool,pub denied:u64,pub index:Index,
    /// Directory identities still needing enumeration; identities survive compaction.
    pub pending_dirs:Vec<u64>,
}

#[derive(Default)]
pub struct Names {
    pub units:Vec<u16>,pub starts:Vec<u32>,pub folded:Vec<u8>,pub folded_starts:Vec<u32>,
    slots:HashMap<u64,u32>,
}
impl Names {
    pub fn new()->Self{Self{starts:vec![0],folded_starts:vec![0],..Default::default()}}
    pub fn len(&self)->usize{self.starts.len().saturating_sub(1)}
    pub fn get(&self,id:u32)->&[u16]{let id=id as usize;&self.units[self.starts[id]as usize..self.starts[id+1]as usize]}
    pub fn folded(&self,id:u32)->&str{let id=id as usize;std::str::from_utf8(&self.folded[self.folded_starts[id]as usize..self.folded_starts[id+1]as usize]).unwrap()}
    fn hash(units:&[u16])->u64{let mut hash=std::collections::hash_map::DefaultHasher::new();units.hash(&mut hash);hash.finish()}
    pub fn intern(&mut self,name:&OsStr)->Result<u32>{let units:Vec<_>=name.encode_wide().collect();self.intern_units(&units)}
    pub fn intern_units(&mut self,units:&[u16])->Result<u32>{
        ensure!(units.len()<=255&&!units.iter().any(|u|matches!(*u,0|47|92))&&units!=[46]&&units!=[46,46],"索引名称无效");let mut key=Self::hash(units);
        loop{match self.slots.get(&key).copied(){Some(id)if self.get(id)==units=>return Ok(id),Some(_)=>key=key.wrapping_add(1),None=>break}}
        ensure!(self.len()<MAX_ROWS&&self.units.len()+units.len()<u32::MAX as usize,"文件名索引超出容量");
        let id=self.len()as u32;let folded=String::from_utf16_lossy(units).to_lowercase();
        ensure!(self.folded.len()+folded.len()<u32::MAX as usize,"文件名索引超出容量");
        reserve_column(&mut self.units,units.len())?;reserve_column(&mut self.starts,1)?;reserve_column(&mut self.folded,folded.len())?;reserve_column(&mut self.folded_starts,1)?;self.slots.try_reserve(1)?;
        self.units.extend_from_slice(units);self.starts.push(self.units.len()as u32);
        self.folded.extend_from_slice(folded.as_bytes());self.folded_starts.push(self.folded.len()as u32);self.slots.insert(key,id);Ok(id)
    }
    pub fn validate(&mut self)->Result<()>{
        ensure!(self.starts.first()==Some(&0)&&self.folded_starts.first()==Some(&0)&&self.starts.len()==self.folded_starts.len()
            &&self.starts.last().copied()==Some(self.units.len()as u32)&&self.folded_starts.last().copied()==Some(self.folded.len()as u32),"快照名称列长度不一致");
        ensure!(self.starts.windows(2).all(|w|w[0]<=w[1])&&self.folded_starts.windows(2).all(|w|w[0]<=w[1]),"快照名称偏移无效");
        std::str::from_utf8(&self.folded)?;self.slots.clear();
        for id in 0..self.len()as u32{
            let units=self.get(id);ensure!(units.len()<=255&&!units.iter().any(|u|matches!(*u,0|47|92))&&units!=[46]&&units!=[46,46],"快照名称无效");
            let expected=String::from_utf16_lossy(units).to_lowercase();
            let a=self.folded_starts[id as usize]as usize;let b=self.folded_starts[id as usize+1]as usize;
            ensure!(self.folded.get(a..b)==Some(expected.as_bytes()),"快照名称规范化不一致");
            let mut key=Self::hash(units);while self.slots.contains_key(&key){key=key.wrapping_add(1);}self.slots.insert(key,id);
        }Ok(())
    }
    pub fn bytes(&self)->usize{self.units.capacity()*2+self.starts.capacity()*4+self.folded.capacity()+self.folded_starts.capacity()*4+self.slots.capacity()*24}
    pub fn shrink(&mut self){self.units.shrink_to_fit();self.starts.shrink_to_fit();self.folded.shrink_to_fit();self.folded_starts.shrink_to_fit();self.slots.shrink_to_fit();}
}
pub struct Attributes {pub size:Vec<u64>,pub modified:Vec<u64>}
pub struct Index {
    pub ids:Vec<u64>,pub parent:Vec<u32>,pub name:Vec<u32>,pub flags:Vec<u8>,pub names:Names,
    pub attributes:Option<Attributes>,order:Vec<u32>,delta:HashMap<u64,Vec<u32>>,building:bool,first:Vec<u32>,next:Vec<u32>,
}
impl Index {
    pub fn new(root_id:u64,metadata:bool)->Self{
        let mut value=Self{ids:Vec::new(),parent:Vec::new(),name:Vec::new(),flags:Vec::new(),names:Names::new(),
            attributes:metadata.then(||Attributes{size:Vec::new(),modified:Vec::new()}),order:Vec::new(),delta:HashMap::new(),building:true,first:Vec::new(),next:Vec::new()};
        value.append(root_id,NONE,OsStr::new(""),DIRECTORY).unwrap();value.reindex();value
    }
    pub fn len(&self)->usize{self.ids.len()}
    pub fn live(&self,row:u32)->bool{self.flags.get(row as usize).is_some_and(|f|f&DEAD==0)}
    pub fn searchable(&self,row:u32)->bool{
        let mut at=row;for _ in 0..1024{let Some(&flags)=self.flags.get(at as usize)else{return false;};
            if flags&(DEAD|BLOCKED)!=0||(flags&EXCLUDED!=0&&at!=row){return false;}if at==0{return row!=0;}at=self.parent[at as usize];}false
    }
    pub fn is_dir(&self,row:u32)->bool{self.flags[row as usize]&DIRECTORY!=0}
    pub fn rows_for_id(&self,id:u64)->Vec<u32>{
        let start=self.order.partition_point(|row|self.ids[*row as usize]<id);
        let mut rows=self.order[start..].iter().take_while(|row|self.ids[**row as usize]==id).copied().filter(|row|self.live(*row)).collect::<Vec<_>>();
        rows.extend(self.delta.get(&id).into_iter().flatten().copied().filter(|row|self.live(*row)));rows
    }
    pub fn directory_for_id(&self,id:u64)->Option<u32>{
        let start=self.order.partition_point(|row|self.ids[*row as usize]<id);
        self.order[start..].iter().take_while(|row|self.ids[**row as usize]==id).chain(self.delta.get(&id).into_iter().flatten())
            .copied().find(|row|self.live(*row)&&self.is_dir(*row))
    }
    /// True when `id` already names `parent` or one of its ancestors. A bind or virtualized
    /// view can list a directory again below itself; that entry is an alias, never a child.
    pub fn aliases_ancestor(&self,parent:u32,id:u64)->bool{
        let mut at=parent;for _ in 0..1024{let Some(&value)=self.ids.get(at as usize)else{return false;};if value==id{return true;}if at==0{return false;}at=self.parent[at as usize];}false
    }
    pub fn begin_build(&mut self){self.building=true;}
    pub fn children(&self,parent:u32)->Vec<u32>{
        let mut rows=Vec::new();let mut row=self.first.get(parent as usize).copied().unwrap_or(NONE);
        while row!=NONE&&rows.len()<=self.len(){if self.live(row){rows.push(row);}row=self.next[row as usize];}rows
    }
    pub fn append(&mut self,id:u64,parent:u32,name:&OsStr,flags:u8)->Result<u32>{
        ensure!(self.len()<MAX_ROWS,"索引项目数量超过上限");
        if self.len()%4096==0{ensure!(self.heap_bytes()<MAX_HEAP_BYTES,"自带索引超过内存预算，请启用开发目录排除或关闭属性索引");}
        reserve_column(&mut self.ids,1)?;reserve_column(&mut self.parent,1)?;reserve_column(&mut self.name,1)?;reserve_column(&mut self.flags,1)?;reserve_column(&mut self.first,1)?;reserve_column(&mut self.next,1)?;
        if let Some(attrs)=&mut self.attributes{reserve_column(&mut attrs.size,1)?;reserve_column(&mut attrs.modified,1)?;}
        if !self.building{self.delta.try_reserve(1)?;self.delta.entry(id).or_default().try_reserve(1)?;}
        let name=self.names.intern(name)?;let row=self.len()as u32;
        self.ids.push(id);self.parent.push(parent);self.name.push(name);self.flags.push(flags);self.first.push(NONE);self.next.push(NONE);
        if let Some(attrs)=&mut self.attributes{attrs.size.push(UNKNOWN);attrs.modified.push(UNKNOWN);}
        if (parent as usize)<self.first.len(){self.next[row as usize]=self.first[parent as usize];self.first[parent as usize]=row;}
        if !self.building{self.delta.entry(id).or_default().push(row);}Ok(row)
    }
    pub fn upsert_child(&mut self,id:u64,parent:u32,name:&OsStr,flags:u8)->Result<u32>{
        let units:Vec<_>=name.encode_wide().collect();
        if flags&DIRECTORY!=0{
            let start=self.order.partition_point(|row|self.ids[*row as usize]<id);
            let existing=self.order[start..].iter().take_while(|row|self.ids[**row as usize]==id).chain(self.delta.get(&id).into_iter().flatten()).copied().find(|row|self.is_dir(*row));
            if let Some(row)=existing{
                ensure!(row!=0,"不能移动索引根目录");let mut ancestor=parent;
                for _ in 0..1024{ensure!(ancestor!=row,"目录父节点形成循环");if ancestor==NONE||ancestor==0{break;}ancestor=self.parent[ancestor as usize];}
                let old=self.parent[row as usize];
                if old!=parent{
                    if old!=NONE{let mut current=self.first[old as usize];let mut previous=NONE;
                        while current!=NONE{if current==row{if previous==NONE{self.first[old as usize]=self.next[row as usize];}else{self.next[previous as usize]=self.next[row as usize];}break;}previous=current;current=self.next[current as usize];}}
                    self.parent[row as usize]=parent;self.next[row as usize]=self.first[parent as usize];self.first[parent as usize]=row;
                }
                self.name[row as usize]=self.names.intern_units(&units)?;self.flags[row as usize]=flags;return Ok(row);
            }
        }
        if let Some(row)=self.rows_for_id(id).into_iter().find(|row|self.parent[*row as usize]==parent&&self.names.get(self.name[*row as usize])==units){self.flags[row as usize]=flags;return Ok(row);}
        self.append(id,parent,name,flags)
    }
    pub fn delete(&mut self,row:u32){if row!=0{self.flags[row as usize]|=DEAD;}}
    pub fn delete_id(&mut self,id:u64){for row in self.rows_for_id(id){self.delete(row);}}
    pub fn reindex(&mut self){
        self.order=(0..self.len()as u32).filter(|row|self.live(*row)).collect();let ids=&self.ids;
        self.order.sort_unstable_by_key(|row|ids[*row as usize]);self.order.shrink_to_fit();self.delta.clear();self.delta.shrink_to_fit();self.building=false;
    }
    pub fn link_parents(&mut self,parents:&[u64])->Result<()>{
        ensure!(parents.len()==self.len(),"父目录列长度无效");self.reindex();
        for (row,id) in parents.iter().enumerate().skip(1){self.parent[row]=self.directory_for_id(*id).filter(|p|*p as usize!=row).unwrap_or(NONE);}
        self.rebuild_children();Ok(())
    }
    pub fn rebuild_children(&mut self){
        self.first=vec![NONE;self.len()];self.next=vec![NONE;self.len()];
        for row in 1..self.len(){let parent=self.parent[row]as usize;if parent<self.len(){self.next[row]=self.first[parent];self.first[parent]=row as u32;}}
    }
    pub fn path(&self,root:&Path,row:u32)->Option<PathBuf>{
        if row==0{return Some(root.into());}let mut names=Vec::new();let mut at=row;
        for _ in 0..1024{if at==0{let mut path=root.to_path_buf();for name in names.into_iter().rev(){path.push(OsString::from_wide(self.names.get(name)));}return Some(path);}
            if !self.live(at){return None;}names.push(self.name[at as usize]);at=self.parent[at as usize];if at==NONE{return None;}}None
    }
    pub fn set_attributes(&mut self,row:u32,size:Option<u64>,modified:Option<u64>){if let Some(attrs)=&mut self.attributes{attrs.size[row as usize]=size.unwrap_or(UNKNOWN);attrs.modified[row as usize]=modified.unwrap_or(UNKNOWN);}}
    pub fn invalidate_id(&mut self,id:u64){for row in self.rows_for_id(id){self.set_attributes(row,None,None);}}
    pub fn apply_exclusions(&mut self,enabled:bool){for row in 1..self.len(){self.flags[row]&=!EXCLUDED;if enabled&&self.flags[row]&DIRECTORY!=0&&excluded_name(self.names.folded(self.name[row])){self.flags[row]|=EXCLUDED;}}}
    pub fn compact(&mut self)->Result<()>{
        let keep:Vec<_>=(0..self.len()as u32).filter(|row|*row==0||self.searchable(*row)).collect();
        let mut remap=vec![NONE;self.len()];for (next,old) in keep.iter().enumerate(){remap[*old as usize]=next as u32;}
        let mut names=Names::new();let mut name_ids=Vec::with_capacity(keep.len());
        for row in &keep{name_ids.push(names.intern_units(self.names.get(self.name[*row as usize]))?);}
        for (next,old) in keep.iter().enumerate(){let old=*old as usize;self.ids[next]=self.ids[old];self.flags[next]=self.flags[old];
            self.parent[next]=self.parent.get(old).filter(|p|**p!=NONE).map_or(NONE,|p|remap[*p as usize]);
            if let Some(attrs)=&mut self.attributes{attrs.size[next]=attrs.size[old];attrs.modified[next]=attrs.modified[old];}}
        self.ids.truncate(keep.len());self.flags.truncate(keep.len());self.parent.truncate(keep.len());self.name=name_ids;self.names=names;
        if let Some(attrs)=&mut self.attributes{attrs.size.truncate(keep.len());attrs.modified.truncate(keep.len());attrs.size.shrink_to_fit();attrs.modified.shrink_to_fit();}
        self.ids.shrink_to_fit();self.flags.shrink_to_fit();self.parent.shrink_to_fit();self.names.shrink();self.reindex();self.rebuild_children();Ok(())
    }
    pub fn validate(&mut self)->Result<()>{
        let n=self.len();ensure!(n>0&&n<=MAX_ROWS&&self.parent.len()==n&&self.name.len()==n&&self.flags.len()==n,"快照索引列长度无效");
        ensure!(self.parent[0]==NONE&&self.flags[0]==DIRECTORY,"快照根节点无效");self.names.validate()?;
        ensure!(self.name.iter().all(|id|(*id as usize)<self.names.len()),"快照名称引用无效");
        ensure!(self.names.get(self.name[0]).is_empty()&&self.name.iter().skip(1).all(|id|!self.names.get(*id).is_empty()),"快照空名称无效");
        ensure!(self.flags.iter().all(|f|f&!(DIRECTORY|REPARSE|DEAD|EXCLUDED|BLOCKED)==0),"快照属性无效");
        ensure!(self.parent.iter().enumerate().skip(1).all(|(i,p)|(*p as usize)<n&&*p as usize!=i&&self.flags[*p as usize]&DIRECTORY!=0),"快照父节点无效");
        let mut seen=vec![0u8;n];seen[0]=2;
        for start in 1..n{let mut at=start;let mut trail=Vec::new();while seen[at]==0{seen[at]=1;trail.push(at);at=self.parent[at]as usize;}
            ensure!(seen[at]==2,"快照目录循环");for row in trail{seen[row]=2;}}
        if let Some(attrs)=&self.attributes{ensure!(attrs.size.len()==n&&attrs.modified.len()==n,"快照属性列长度无效");}
        self.reindex();self.rebuild_children();Ok(())
    }
    pub fn heap_bytes(&self)->usize{self.ids.capacity()*8+self.parent.capacity()*4+self.name.capacity()*4+self.flags.capacity()+
        (self.order.capacity()+self.first.capacity()+self.next.capacity())*4+self.delta.capacity()*40+self.delta.values().map(|v|v.capacity()*4).sum::<usize>()+
        self.names.bytes()+self.attributes.as_ref().map_or(0,|a|(a.size.capacity()+a.modified.capacity())*8)}
}
pub fn excluded_name(name:&str)->bool{matches!(name,".git"|".hg"|".svn"|"node_modules"|"target"|".cache"|"cache"|"caches"|"__pycache__"|".venv"|"venv"|".next"|".nuxt"|".gradle"|".cargo"|".rustup"|".npm"|".pnpm-store"|".yarn"|".m2"|".ivy2"|".tox"|".turbo"|".vs"|"cmakefiles"|"temp"|"tmp")}

#[cfg(test)]mod tests{
    use super::*;
    #[test]fn incremental_changes_keep_compacted_memory_bounded(){
        for metadata in [false,true]{
            let mut index=Index::new(5,metadata);index.begin_build();
            for id in 10..60_010{index.append(id,0,OsStr::new(&format!("document-{id:08}.txt")),0).unwrap();}
            index.compact().unwrap();let compact=index.heap_bytes();
            for id in 70_000..71_000{index.append(id,0,OsStr::new(&format!("new-document-{id:08}.txt")),0).unwrap();}
            assert!(index.heap_bytes()<compact+compact/4,"incremental columns grew from {compact} to {}",index.heap_bytes());
            assert_eq!(index.rows_for_id(70_999).len(),1);index.validate().unwrap();
            assert!(index.path(Path::new(r"C:\"),index.rows_for_id(70_999)[0]).unwrap().ends_with("new-document-00070999.txt"));
        }
    }
    #[test]fn columns_preserve_aliases_and_parent_renames(){
        let mut index=Index::new(5,false);let dir=index.append(8,0,OsStr::new("中文"),DIRECTORY).unwrap();
        let a=index.append(9,dir,OsStr::new("甲.txt"),0).unwrap();let b=index.append(9,0,OsStr::new("乙.txt"),0).unwrap();index.reindex();
        assert_eq!(index.rows_for_id(9).len(),2);index.name[dir as usize]=index.names.intern(OsStr::new("移动后")).unwrap();
        assert_eq!(index.path(Path::new(r"C:\"),a).unwrap(),PathBuf::from(r"C:\移动后\甲.txt"));
        index.delete(dir);assert!(!index.searchable(a));assert!(index.searchable(b));index.compact().unwrap();assert_eq!(index.len(),2);index.validate().unwrap();
    }
    #[test]fn ancestor_aliases_are_recognized_by_identity(){
        let mut index=Index::new(5,false);let a=index.append(8,0,OsStr::new("a"),DIRECTORY).unwrap();let b=index.append(9,a,OsStr::new("b"),DIRECTORY).unwrap();index.reindex();
        for id in [5,8,9]{assert!(index.aliases_ancestor(b,id),"{id}");}
        assert!(!index.aliases_ancestor(a,9));assert!(!index.aliases_ancestor(b,10));assert!(!index.aliases_ancestor(NONE,5));
    }
    #[test]fn exclusion_prunes_descendants_and_metadata_cost_is_optional(){
        let mut index=Index::new(5,false);let dir=index.append(6,0,OsStr::new("node_modules"),DIRECTORY).unwrap();index.append(7,dir,OsStr::new("x"),0).unwrap();
        index.apply_exclusions(true);assert!(!index.searchable(2));assert!(index.searchable(1));index.compact().unwrap();assert_eq!(index.len(),2);assert!(index.attributes.is_none());
        let mut with=Index::new(5,true);with.append(8,0,OsStr::new("empty"),0).unwrap();with.set_attributes(1,Some(0),Some(0));assert_eq!(with.attributes.unwrap().size[1],0);
    }
    #[test]fn snapshot_graph_validation_rejects_cycles_and_bad_names(){
        let mut index=Index::new(5,false);index.append(6,0,OsStr::new("a"),DIRECTORY).unwrap();index.append(7,1,OsStr::new("b"),DIRECTORY).unwrap();
        index.parent[1]=2;assert!(index.validate().is_err());assert!(index.names.intern(OsStr::new("../bad")).is_err());
    }
}
