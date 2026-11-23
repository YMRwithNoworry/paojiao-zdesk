//! Metadata for the visible page only. Network and file metadata lookups never run on the panel thread.
use std::{
    collections::VecDeque,
    fs, io,
    os::windows::fs::MetadataExt,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Condvar, Mutex,
    },
};
use windows::Win32::{
    Foundation::{FILETIME, HWND, LPARAM, SYSTEMTIME, WPARAM},
    UI::WindowsAndMessaging::PostMessageW,
};
pub(crate) const READY: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 107;
#[link(name = "kernel32")]
extern "system" {
    fn FileTimeToLocalFileTime(input: *const FILETIME, output: *mut FILETIME) -> i32;
    fn FileTimeToSystemTime(input: *const FILETIME, output: *mut SYSTEMTIME) -> i32;
}
fn date(t: u64) -> String {
    let utc = FILETIME {
        dwLowDateTime: t as u32,
        dwHighDateTime: (t >> 32) as u32,
    };
    let mut local = FILETIME::default();
    let mut s = SYSTEMTIME::default();
    if unsafe { FileTimeToLocalFileTime(&utc, &mut local) } == 0
        || unsafe { FileTimeToSystemTime(&local, &mut s) } == 0
    {
        return String::new();
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        s.wYear, s.wMonth, s.wDay, s.wHour, s.wMinute
    )
}
pub(crate) struct Detail {
    pub generation: u64,
    pub row: usize,
    pub path: PathBuf,
    pub size: String,
    pub modified: String,
    pub folder: bool,
}
#[derive(Default)]
struct Work {
    generation: u64,
    pending: VecDeque<(usize, PathBuf)>,
    closed: bool,
}
pub(crate) struct Reader {
    work: Arc<(Mutex<Work>, Condvar)>,
    ready: Arc<AtomicBool>,
    results: mpsc::Receiver<Detail>,
}
impl Reader {
    pub fn new(owner: HWND) -> io::Result<Self> {
        let (tx, results) = mpsc::channel();
        let work = Arc::new((Mutex::new(Work::default()), Condvar::new()));
        let shared = work.clone();
        let ready = Arc::new(AtomicBool::new(false));
        let notify = ready.clone();
        let window = owner.0 as isize;
        std::thread::Builder::new()
            .name("zsdesk-search-details".into())
            .spawn(move || loop {
                let (generation, row, path) = {
                    let state = shared.0.lock().unwrap_or_else(|e| e.into_inner());
                    let mut state = shared
                        .1
                        .wait_while(state, |s| !s.closed && s.pending.is_empty())
                        .unwrap_or_else(|e| e.into_inner());
                    if state.closed {
                        return;
                    }
                    let (row, path) = state.pending.pop_front().unwrap();
                    (state.generation, row, path)
                };
                let Ok(meta) = fs::metadata(&path) else {
                    continue;
                };
                let size = if meta.is_dir() {
                    String::new()
                } else {
                    zsdesk_ui::chrome::format_size(meta.len())
                };
                let state = shared.0.lock().unwrap_or_else(|e| e.into_inner());
                if state.closed {
                    return;
                }
                if state.generation == generation {
                    if tx
                        .send(Detail {
                            generation,
                            row,
                            path,
                            size,
                            modified: date(meta.last_write_time()),folder:meta.is_dir(),
                        })
                        .is_err()
                    {
                        return;
                    }
                    if !notify.swap(true, Ordering::AcqRel) {
                        let _ = unsafe {
                            PostMessageW(Some(HWND(window as *mut _)), READY, WPARAM(0), LPARAM(0))
                        };
                    }
                }
            })?;
        Ok(Self {
            work,
            results,
            ready,
        })
    }
    pub fn submit(&self, generation: u64, row: usize, path: PathBuf) {
        let mut work = self.work.0.lock().unwrap_or_else(|e| e.into_inner());
        if work.generation == generation {
            work.pending.push_back((row, path));
            self.work.1.notify_one();
        }
    }
    pub fn poll(&self) -> impl Iterator<Item = Detail> + '_ {
        self.ready.store(false, Ordering::Release);
        self.results.try_iter()
    }
    pub fn reset(&self, generation: u64) {
        let mut work = self.work.0.lock().unwrap_or_else(|e| e.into_inner());
        work.generation = generation;
        work.pending.clear();
        let _ = self.poll().count();
    }
}
impl Drop for Reader {
    fn drop(&mut self) {
        let mut work = self.work.0.lock().unwrap_or_else(|e| e.into_inner());
        work.closed = true;
        work.pending.clear();
        self.work.1.notify_one();
    }
}
