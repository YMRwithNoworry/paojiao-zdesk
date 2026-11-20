//! Speculative work owns no visible menu, command authorization, or reusable selection.
use super::*;
use std::sync::atomic::AtomicBool;

pub(crate) const SETTLE_MS:u32=220;
static BUSY:AtomicBool=AtomicBool::new(false);
static EPOCH:AtomicU64=AtomicU64::new(0);
static LAST_START:Mutex<Option<Instant>>=Mutex::new(None);

pub(super) fn running()->bool { BUSY.load(Ordering::Acquire) }
pub(super) fn interrupt(){ EPOCH.fetch_add(1,Ordering::AcqRel); }
pub(crate) fn needed()->bool {
    !running() && PREPARING.load(Ordering::Acquire)==0 &&
        IDLE.try_lock().is_ok_and(|pool|!pool.iter().any(|worker|worker.within_lifetime(Duration::from_secs(2))))
}

pub(crate) fn start(owner:HWND,target:Vec<u8>,folder:Option<PathBuf>,background:bool,options:ContextMenuOptions,context:Option<MenuViewContext>){
    if options.quick_first || !needed(){return;}
    let epoch=EPOCH.load(Ordering::Acquire);
    if BUSY.compare_exchange(false,true,Ordering::AcqRel,Ordering::Acquire).is_err(){return;}
    struct Busy;
    impl Drop for Busy{fn drop(&mut self){BUSY.store(false,Ordering::Release);}}
    let busy=Busy;
    {
        let mut last=LAST_START.lock().unwrap_or_else(|e|e.into_inner());
        if last.is_some_and(|t|t.elapsed()<Duration::from_secs(3)){return;}
        *last=Some(Instant::now());
    }
    let mut origin=Origin::capture();
    origin.view_context=context;
    let hwnd=owner.0 as isize;
    let _=std::thread::Builder::new().name("zsdesk-menu-prewarm".into()).spawn(move||{
        let _busy=busy;
        let owner=HWND(hwnd as *mut _);
        let valid=||EPOCH.load(Ordering::Acquire)==epoch && unsafe{IsWindow(Some(owner))}.as_bool();
        if !valid(){return;}
        use std::path::{Component,Prefix};
        if let Some(path)=&folder {
            let Some(Component::Prefix(prefix))=path.components().next()else{return;};
            let (Prefix::Disk(letter)|Prefix::VerbatimDisk(letter))=prefix.kind()else{return;};
            let root=[u16::from(letter),58,92,0];
            if unsafe{windows::Win32::Storage::FileSystem::GetDriveTypeW(PCWSTR(root.as_ptr()))}!=3{return;}
        }
        let mode=if background{Mode::PrepareBackground}else{Mode::PrepareSelection};
        let intercept=MenuInterceptions{rename:!background,..Default::default()};
        let Ok(mut client)=Client::spawn_at(owner,POINT::default(),vec![target],folder,options,intercept,mode,false,origin) else{return;};
        let until=Instant::now()+Duration::from_secs(5);
        while valid() && Instant::now()<until{
            if client.poll(&mut |_|false).is_some(){break;}
            client.wait_for_progress(20);
        }
        if !valid(){client.complete=false;}
    });
}
