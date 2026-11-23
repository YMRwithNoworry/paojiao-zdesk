//! Read the existing snapshot in place for the first name-sorted page. A prepared
//! file keeps only its layout and digest. Each query owns its mapped view.
use super::metadata::{BLOCKED, DEAD, DIRECTORY, EXCLUDED, MAX_ROWS, NONE, REPARSE};
use super::query::Query;
use super::*;
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    ops::Range,
    os::windows::{
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawHandle,
    },
};
use windows::Win32::{
    Foundation::{CloseHandle, HANDLE},
    Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_SHARE_DELETE, FILE_SHARE_READ,
    },
    System::Memory::{
        CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_READ,
        MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READONLY,
    },
};

struct Mapping(MEMORY_MAPPED_VIEW_ADDRESS, usize, #[allow(dead_code)] File);
impl Mapping {
    fn new(file: File, size: usize) -> Result<Self> {
        let section = unsafe {
            CreateFileMappingW(
                HANDLE(file.as_raw_handle()),
                None,
                PAGE_READONLY,
                0,
                0,
                None,
            )
        }?;
        let address = unsafe { MapViewOfFile(section, FILE_MAP_READ, 0, 0, size) };
        let failure = address
            .Value
            .is_null()
            .then(windows::core::Error::from_thread);
        let _ = unsafe { CloseHandle(section) };
        if let Some(error) = failure {
            return Err(error.into());
        }
        Ok(Self(address, size, file))
    }
    fn bytes(&self) -> &[u8] {
        // The file is opened without write sharing, and updates replace it atomically.
        unsafe { std::slice::from_raw_parts(self.0.Value.cast(), self.1) }
    }
}
impl Drop for Mapping {
    fn drop(&mut self) {
        let _ = unsafe { UnmapViewOfFile(self.0) };
    }
}
struct Cursor<'a> {
    data: &'a [u8],
    at: usize,
}
impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<Range<usize>> {
        let end = self.at.checked_add(n).context("快照偏移溢出")?;
        ensure!(end <= self.data.len(), "快照列不完整");
        let start = self.at;
        self.at = end;
        Ok(start..end)
    }
    fn word<const N: usize>(&mut self) -> Result<[u8; N]> {
        let r = self.take(N)?;
        Ok(self.data[r].try_into()?)
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.word()?))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.word()?))
    }
    fn column(&mut self, width: usize, max: usize) -> Result<Range<usize>> {
        let n = usize::try_from(self.u64()?)?;
        ensure!(n <= max, "快照列数量无效");
        self.take(n.checked_mul(width).context("快照列大小无效")?)
    }
}
fn number(data: &[u8], id: usize) -> usize {
    let at = id * 4;
    u32::from_le_bytes(data[at..at + 4].try_into().unwrap()) as usize
}
fn wide(data: &[u8]) -> OsString {
    OsString::from_wide(
        &data
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect::<Vec<_>>(),
    )
}
const STRIDE: usize = 32;
enum Spelling {
    Legacy {
        units: Range<usize>,
        offsets: Range<usize>,
    },
    Compact {
        stream: Range<usize>,
        blocks: Vec<u32>,
    },
}
struct Volume {
    root: PathBuf,
    serial: u32,
    root_id: u64,
    journal: u64,
    checkpoint: i64,
    complete_journal: bool,
    parent: Range<usize>,
    name: Range<usize>,
    flags: Range<usize>,
    folded: Range<usize>,
    starts: Range<usize>,
    spelling: Spelling,
    mft: bool,
}
impl Volume {
    fn parse(
        c: &mut Cursor<'_>,
        legacy: bool,
        attributes: bool,
        remaining: usize,
        cancel: &impl Fn() -> bool,
    ) -> Result<Self> {
        let root = c.column(2, 32768)?;
        let root = PathBuf::from(wide(&c.data[root]));
        ensure!(root.is_absolute(), "快照根目录无效");
        let serial = c.u32()?;
        let journal = c.u64()?;
        let checkpoint = i64::from_le_bytes(c.word()?);
        ensure!(checkpoint >= 0, "快照日志位置无效");
        let flags = c.u32()?;
        ensure!(flags & !7 == 0, "快照标记无效");
        c.take(8)?;
        let ids = c.column(8, remaining)?;
        let rows = ids.len() / 8;
        ensure!(rows > 0, "快照卷缺少根节点");
        let root_id = u64::from_le_bytes(c.data[ids.start..ids.start + 8].try_into()?);
        let parent = c.column(4, rows)?;
        let name = c.column(4, rows)?;
        let kinds = c.column(1, rows)?;
        ensure!(
            rows > 0 && parent.len() == rows * 4 && name.len() == rows * 4 && kinds.len() == rows,
            "快照索引列长度无效"
        );
        let old = if legacy {
            Some((c.column(2, c.data.len() / 2)?, c.column(4, MAX_ROWS + 1)?))
        } else {
            None
        };
        let folded = c.column(1, c.data.len())?;
        let starts = c.column(4, MAX_ROWS + 1)?;
        let lower = std::str::from_utf8(&c.data[folded.clone()])?;
        let offsets = &c.data[starts.clone()];
        ensure!(
            offsets.len() >= 8 && number(offsets, 0) == 0,
            "快照名称列无效"
        );
        let names = offsets.len() / 4 - 1;
        ensure!(number(offsets, names) == lower.len(), "快照名称长度无效");
        if let Some((units, wide_offsets)) = &old {
            let o = &c.data[wide_offsets.clone()];
            ensure!(
                o.len() == offsets.len()
                    && number(o, 0) == 0
                    && number(o, names) == units.len() / 2,
                "快照原名列无效"
            );
        }
        let mut blocks = Vec::new();
        let stream_start = c.at;
        let original_matches = |bytes: &[u8], text: &str| {
            if text.is_ascii()
                && bytes.len() == text.len() * 2
                && bytes.chunks_exact(2).all(|u| u[1] == 0 && u[0].is_ascii())
            {
                return bytes
                    .chunks_exact(2)
                    .zip(text.bytes())
                    .all(|(u, b)| u[1] == 0 && u[0].to_ascii_lowercase() == b);
            }
            wide(bytes).to_string_lossy().to_lowercase() == text
        };
        for id in 0..names {
            if id % 4096 == 0 {
                ensure!(!cancel(), "快照读取已取消");
            }
            let text = lower
                .get(number(offsets, id)..number(offsets, id + 1))
                .context("快照名称偏移无效")?;
            ensure!(
                text.len() <= 1020
                    && !text.bytes().any(|b| matches!(b, 0 | 47 | 92))
                    && text != "."
                    && text != "..",
                "快照名称无效"
            );
            if let Some((units, wide_offsets)) = &old {
                let o = &c.data[wide_offsets.clone()];
                let a = number(o, id);
                let b = number(o, id + 1);
                ensure!(
                    b >= a && b - a <= 255 && b <= units.len() / 2,
                    "快照原名偏移无效"
                );
                ensure!(
                    original_matches(&c.data[units.start + 2 * a..units.start + 2 * b], text),
                    "快照原名无效"
                );
            } else {
                if id % STRIDE == 0 {
                    blocks.push(u32::try_from(c.at - stream_start)?);
                }
                let n = u16::from_le_bytes(c.word()?) as usize;
                ensure!(n <= 255, "快照原名长度无效");
                let r = c.take(n * 2)?;
                let valid = if n > 0 {
                    original_matches(&c.data[r], text)
                } else if text.is_ascii() {
                    text.len() <= 255 && !text.bytes().any(|b| b.is_ascii_uppercase())
                } else {
                    text.encode_utf16().count() <= 255 && text.to_lowercase() == text
                };
                ensure!(valid, "快照原名无效");
            }
        }
        let spelling = match old {
            Some((units, offsets)) => Spelling::Legacy { units, offsets },
            None => Spelling::Compact {
                stream: stream_start..c.at,
                blocks,
            },
        };
        if attributes {
            for _ in 0..2 {
                ensure!(c.column(8, rows)?.len() == rows * 8, "快照属性长度无效");
            }
        }
        if flags & 4 != 0 {
            c.column(8, MAX_ROWS)?;
        }
        let ps = &c.data[parent.clone()];
        let ns = &c.data[name.clone()];
        let fs = &c.data[kinds.clone()];
        ensure!(
            number(ps, 0) == NONE as usize && fs[0] == DIRECTORY,
            "快照根节点无效"
        );
        for row in 0..rows {
            if row % 16384 == 0 {
                ensure!(!cancel(), "快照读取已取消");
            }
            let p = number(ps, row);
            let n = number(ns, row);
            ensure!(
                n < names
                    && (number(offsets, n) == number(offsets, n + 1)) == (row == 0)
                    && fs[row] & !(DIRECTORY | BLOCKED | DEAD | EXCLUDED | REPARSE) == 0
                    && (row == 0 || (p < rows && p != row && fs[p] & DIRECTORY != 0)),
                "快照行无效"
            );
        }
        let mut seen = vec![0u8; rows];
        seen[0] = 2;
        for row in 1..rows {
            if row % 16384 == 0 {
                ensure!(!cancel(), "快照读取已取消");
            }
            let mut at = row;
            while seen[at] == 0 {
                seen[at] = 1;
                at = number(ps, at);
            }
            ensure!(seen[at] == 2, "快照目录循环");
            at = row;
            while seen[at] == 1 {
                seen[at] = 2;
                at = number(ps, at);
            }
        }
        Ok(Self {
            root,
            serial,
            root_id,
            journal,
            checkpoint,
            complete_journal: flags & 5 == 1,
            parent,
            name,
            flags: kinds,
            folded,
            starts,
            spelling,
            mft: flags & 2 != 0,
        })
    }
    fn original<'a>(&self, data: &'a [u8], id: usize) -> Option<&'a [u8]> {
        match &self.spelling {
            Spelling::Legacy { units, offsets } => {
                let o = &data[offsets.clone()];
                Some(&data[units.start + 2 * number(o, id)..units.start + 2 * number(o, id + 1)])
            }
            Spelling::Compact { stream, blocks } => {
                let mut p = stream.start + blocks[id / STRIDE] as usize;
                for current in (id / STRIDE) * STRIDE..=id {
                    let n = u16::from_le_bytes([data[p], data[p + 1]]) as usize;
                    p += 2;
                    if current == id {
                        return (n > 0).then(|| &data[p..p + 2 * n]);
                    }
                    p += 2 * n;
                }
                None
            }
        }
    }
}
struct Rows<'a> {
    volume: &'a Volume,
    data: &'a [u8],
    parents: &'a [u8],
    names: &'a [u8],
    flags: &'a [u8],
    starts: &'a [u8],
    text: &'a str,
}
impl<'a> Rows<'a> {
    fn new(v: &'a Volume, d: &'a [u8]) -> Self {
        // The complete payload digest matches the validated UTF-8 and offsets. The mapping holds a write-excluding handle for its lifetime.
        Self {
            volume: v,
            data: d,
            parents: &d[v.parent.clone()],
            names: &d[v.name.clone()],
            flags: &d[v.flags.clone()],
            starts: &d[v.starts.clone()],
            text: unsafe { std::str::from_utf8_unchecked(&d[v.folded.clone()]) },
        }
    }
    fn text(&self, id: usize) -> &str {
        &self.text[number(self.starts, id)..number(self.starts, id + 1)]
    }
    fn visible(&self, row: usize, memo: &mut Option<(usize, bool)>) -> bool {
        if self.flags[row] & (DEAD | BLOCKED) != 0 {
            return false;
        }
        let parent = number(self.parents, row);
        if let Some((last, visible)) = *memo {
            if last == parent {
                return visible;
            }
        }
        let mut at = parent;
        let mut visible = false;
        for _ in 1..1024 {
            if self.flags[at] & (DEAD | BLOCKED | EXCLUDED) != 0 {
                break;
            }
            if at == 0 {
                visible = true;
                break;
            }
            at = number(self.parents, at);
        }
        *memo = Some((parent, visible));
        visible
    }
    fn path(&self, mut row: usize) -> Result<PathBuf> {
        let mut ids = Vec::new();
        while row != 0 {
            ensure!(ids.len() < 1024, "快照路径过深");
            ids.push(number(self.names, row));
            row = number(self.parents, row);
        }
        let mut path = self.volume.root.clone();
        for id in ids.into_iter().rev() {
            let lower = self.text(id);
            let spelling = self.volume.original(self.data, id).map(wide);
            let name = spelling.as_deref().unwrap_or_else(|| OsStr::new(lower));
            ensure!(
                matches!(
                    Path::new(name).components().next(),
                    Some(std::path::Component::Normal(_))
                ) && !name
                    .as_encoded_bytes()
                    .iter()
                    .any(|c| matches!(*c, 0 | 47 | 92)),
                "快照路径组件无效"
            );
            path.push(name);
        }
        Ok(path)
    }
}
#[derive(PartialEq, Eq)]
struct Identity {
    serial: u32,
    id: u64,
    size: usize,
    modified: u64,
}
struct Prepared {
    digest: [u8; 32],
    fingerprint: blake3::Hash,
    path: PathBuf,
    identity: Identity,
    options: u32,
    volumes: Vec<Volume>,
}
static VALIDATE: Mutex<()> = Mutex::new(());
static EPOCH: AtomicU64 = AtomicU64::new(0);
pub(super) fn invalidate(path: &Path) {
    let mut cache = LAST.lock().unwrap_or_else(|e| e.into_inner());
    EPOCH.fetch_add(1, Ordering::AcqRel);
    if cache.as_ref().is_some_and(|p| p.path == path) {
        cache.take();
    }
}
pub(crate) fn diagnostics() -> serde_json::Value {
    let cache = LAST.lock().unwrap_or_else(|e| e.into_inner());
    match cache.as_ref() {
        None => serde_json::Value::Null,
        Some(p) => serde_json::json!({
            "snapshot_bytes":p.identity.size,"volumes":p.volumes.len(),
            "layout_bytes":std::mem::size_of::<Prepared>()+p.volumes.capacity()*std::mem::size_of::<Volume>()
                +p.volumes.iter().map(|v|v.root.capacity()+match &v.spelling{Spelling::Compact{blocks,..}=>blocks.capacity()*4,_=>0}).sum::<usize>()
        }),
    }
}
static LAST: Mutex<Option<Arc<Prepared>>> = Mutex::new(None);
fn prepare(
    path: &Path,
    options: &SearchOptions,
    cancel: &impl Fn() -> bool,
) -> Result<(Arc<Prepared>, Mapping)> {
    let file = File::options()
        .read(true)
        .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_DELETE.0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(path)?;
    let meta = file.metadata()?;
    let size = usize::try_from(meta.len())?;
    ensure!(
        meta.is_file()
            && meta.file_attributes() & 0x400 == 0
            && (48..=2 * 1024 * 1024 * 1024 + 32).contains(&size),
        "快照文件无效"
    );
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }?;
    let identity = Identity {
        serial: info.dwVolumeSerialNumber,
        id: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        size,
        modified: meta.last_write_time(),
    };
    let policy = snapshot::Policy {
        development: options.include_development,
        metadata: options.index_metadata,
    }
    .bits();
    let mapped = Mapping::new(file, size)?;
    let bytes = mapped.bytes();
    let (payload, digest) = bytes.split_at(size - 32);
    // The on-disk SHA-256 authenticates the snapshot on preparation. A fast
    // in-memory fingerprint then proves byte equality on each new mapping,
    // without holding a file handle across atomic saves.
    let mut fast = blake3::Hasher::new();
    for chunk in payload.chunks(4 * 1024 * 1024) {
        ensure!(!cancel(), "快照读取已取消");
        fast.update(chunk);
    }
    let fingerprint = fast.finalize();
    let cached = || {
        LAST.lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .filter(|p| {
                p.path == path
                    && p.identity == identity
                    && p.options == policy
                    && p.digest.as_slice() == digest
                    && p.fingerprint == fingerprint
            })
            .cloned()
    };
    if let Some(found) = cached() {
        return Ok((found, mapped));
    }
    let _validate = loop {
        ensure!(!cancel(), "快照读取已取消");
        match VALIDATE.try_lock() {
            Ok(g) => break g,
            Err(std::sync::TryLockError::Poisoned(e)) => break e.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => thread::sleep(Duration::from_millis(5)),
        }
    };
    if let Some(found) = cached() {
        return Ok((found, mapped));
    }
    let epoch = EPOCH.load(Ordering::Acquire);
    let mut hash = Sha256::new();
    for chunk in payload.chunks(1024 * 1024) {
        ensure!(!cancel(), "快照读取已取消");
        hash.update(chunk);
    }
    ensure!(hash.finalize().as_slice() == digest, "索引快照校验失败");

    let mut c = Cursor {
        data: payload,
        at: 0,
    };
    let magic = c.word::<8>()?;
    let legacy = &magic == snapshot::LEGACY_MAGIC;
    ensure!(
        (legacy || &magic == snapshot::MAGIC) && c.u32()? == policy,
        "索引选项或格式已改变"
    );
    let count = c.u32()?;
    ensure!((1..=128).contains(&count), "快照卷数量无效");
    let mut volumes = Vec::new();
    let mut left = MAX_ROWS;
    for _ in 0..count {
        let v = Volume::parse(&mut c, legacy, options.index_metadata, left, cancel)?;
        left -= v.flags.len();
        volumes.push(v);
    }
    ensure!(c.at == payload.len(), "快照长度无效");
    let ready = Arc::new(Prepared {
        digest: digest.try_into()?,
        fingerprint,
        path: path.to_owned(),
        identity,
        options: policy,
        volumes,
    });
    let mut cache = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if epoch == EPOCH.load(Ordering::Acquire) {
        *cache = Some(ready.clone());
    }
    Ok((ready, mapped))
}
pub(super) fn prime(path: &Path, options: &SearchOptions, cancel: &impl Fn() -> bool) {
    // Speculative preparation only. Actual queries verify the opened file's digest.
    let bits = snapshot::Policy {
        development: options.include_development,
        metadata: options.index_metadata,
    }
    .bits();
    if std::fs::metadata(path).is_ok_and(|m| {
        LAST.lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|p| {
                p.path == path
                    && p.options == bits
                    && p.identity.size as u64 == m.len()
                    && p.identity.modified == m.last_write_time()
            })
    }) {
        return;
    }
    let _ = prepare(path, options, cancel);
}

pub(super) struct Page {
    catalog: Arc<Prepared>,
    pub paths: Vec<PathBuf>,
    pub total: u64,
    pub scanned: u64,
    pub backend: BackendKind,
}
impl Page {
    pub fn current(&self, roots: Option<&[PathBuf]>, stop: &AtomicBool) -> bool {
        let desired = roots.map(<[PathBuf]>::to_vec).unwrap_or_else(fixed_roots);
        let volumes = &self.catalog.volumes;
        if volumes.len() != desired.len() || !volumes.iter().all(|v| v.complete_journal) {
            return false;
        }
        desired.iter().all(|root| {
            if stop.load(Ordering::Acquire) {
                return false;
            }
            let root = path_key_native(root);
            let Some(v) = volumes.iter().find(|v| v.root == root) else {
                return false;
            };
            if !native_scan::identity(&root)
                .is_ok_and(|(serial, id, _)| serial == v.serial && id == v.root_id)
            {
                return false;
            }
            native_scan::volume_root(&root)
                .and_then(|drive| ntfs::Volume::open_changes(&drive).ok())
                .is_some_and(|mut reader| {
                    reader.serial == v.serial
                        && reader.journal(stop).is_ok_and(|j| {
                            j.contains(v.journal, v.checkpoint) && j.next == v.checkpoint
                        })
                })
        })
    }
}
pub(super) fn search(
    path: &Path,
    options: &SearchOptions,
    request: &Request,
    cancel: &impl Fn() -> bool,
) -> Result<Page> {
    use everything::SearchSort::{NameAsc, NameDesc};
    ensure!(
        matches!(request.sort, NameAsc | NameDesc),
        "此排序需要完整索引"
    );
    let query = Query::parse(&request.query, options.pinyin_initials)?;
    let (prepared, view) = prepare(path, options, cancel)?;
    let vols: Vec<_> = prepared
        .volumes
        .iter()
        .map(|v| Rows::new(v, view.bytes()))
        .collect();
    let end = request.offset as usize + request.limit as usize;
    let key = |&(v, r): &(usize, usize)| {
        let vol = &vols[v];
        (vol.text(number(vol.names, r)), v, r)
    };
    let cmp = |a: &(usize, usize), b: &(usize, usize)| {
        let order = key(a).cmp(&key(b));
        if request.sort == NameDesc {
            order.reverse()
        } else {
            order
        }
    };
    let mut hits = Vec::new();
    let mut total = 0;
    let mut scanned = 0;
    for (v, vol) in vols.iter().enumerate() {
        let mut memo = vec![0u8; (vol.starts.len() / 4 - 1).div_ceil(2)];
        let mut last = None;
        for row in 1..vol.flags.len() {
            if row % 1024 == 0 {
                ensure!(!cancel(), "搜索已取消");
            }
            if !vol.visible(row, &mut last) {
                continue;
            }
            scanned += 1;
            let id = number(vol.names, row);
            let dir = vol.flags[row] & DIRECTORY != 0;
            let shift = (id % 2) * 4 + usize::from(dir) * 2;
            let slot = &mut memo[id / 2];
            let mut probe = (*slot >> shift) & 3;
            if probe == 0 {
                probe = match query.probe(vol.text(id), dir) {
                    query::Probe::No => 1,
                    query::Probe::Yes => 2,
                    query::Probe::Path => 3,
                };
                *slot |= probe << shift;
            }
            if probe == 1
                || (probe == 3
                    && !query.matches(
                        vol.text(id),
                        dir,
                        &vol.path(row)?.to_string_lossy().to_lowercase(),
                    ))
            {
                continue;
            }
            #[cfg(test)]
            super::runtime_regression::observe_hit(v, row);
            total += 1;
            hits.push((v, row));
            if hits.len() >= end + end.max(4096) {
                hits.select_nth_unstable_by(end, cmp);
                hits.truncate(end);
            }
        }
    }
    let begin = (request.offset as usize).min(hits.len());
    let end = end.min(hits.len());
    native::select_page(&mut hits, begin, end, cmp);
    let paths = hits[begin..end]
        .iter()
        .map(|&(v, r)| vols[v].path(r))
        .collect::<Result<Vec<_>>>()?;
    let mfts = prepared.volumes.iter().filter(|v| v.mft).count();
    let backend = if mfts == 0 {
        BackendKind::DirectoryScan
    } else if mfts == vols.len() {
        BackendKind::NativeNtfs
    } else {
        BackendKind::Mixed
    };
    Ok(Page {
        catalog: prepared.clone(),
        paths,
        total,
        scanned,
        backend,
    })
}
