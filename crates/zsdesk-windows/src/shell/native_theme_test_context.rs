//! Scoped Common-Controls v6 activation for library test executables.
//! The application executable already embeds this dependency in its manifest.
use std::{fs,io::Write,path::{Path,PathBuf},sync::atomic::{AtomicU64,Ordering},thread,time::{Duration,Instant}};
use std::os::windows::{ffi::OsStrExt,fs::MetadataExt};
use anyhow::{Context,ensure};
use windows::{core::{BOOL,PCSTR,HRESULT},Win32::{Foundation::{HANDLE,HMODULE,FreeLibrary},System::LibraryLoader::{GetProcAddress,LoadLibraryW},
    UI::Controls::{INITCOMMONCONTROLSEX,ICC_LISTVIEW_CLASSES,ICC_TREEVIEW_CLASSES,ICC_STANDARD_CLASSES}}};

const MANIFEST:&str=r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
 <assemblyIdentity type="win32" name="ZSDesk.NativeThemeTests" version="1.0.0.0" processorArchitecture="*" />
 <dependency><dependentAssembly><assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0" processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*" /></dependentAssembly></dependency>
</assembly>"#;
#[repr(C)]struct ContextSpec{size:u32,flags:u32,source:*const u16,architecture:u16,language:u16,directory:*const u16,resource:*const u16,application:*const u16,module:HMODULE}
#[repr(C)]#[derive(Default)]struct Version{size:u32,major:u32,minor:u32,build:u32,platform:u32}
#[link(name="kernel32")]unsafe extern "system"{
    fn CreateActCtxW(context:*const ContextSpec)->HANDLE;
    fn ActivateActCtx(handle:HANDLE,cookie:*mut usize)->BOOL;
    fn DeactivateActCtx(flags:u32,cookie:usize)->BOOL;
    fn ReleaseActCtx(handle:HANDLE);
}
#[link(name="user32")]unsafe extern "system"{
    fn CreateDesktopW(name:*const u16,device:*const u16,mode:*const std::ffi::c_void,flags:u32,access:u32,attributes:*const std::ffi::c_void)->*mut std::ffi::c_void;
    fn SetThreadDesktop(desktop:*mut std::ffi::c_void)->BOOL;
    fn CloseDesktop(desktop:*mut std::ffi::c_void)->BOOL;
}
pub(super) fn private_desktop<T:Send+'static>(name:&str,action:impl FnOnce()->anyhow::Result<T>+Send+'static)->anyhow::Result<T>{
    static NEXT:AtomicU64=AtomicU64::new(1);
    let name=format!("ZSDesk.Theme.{}.{}.{}",name,std::process::id(),NEXT.fetch_add(1,Ordering::Relaxed));
    let wide=name.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let desktop=unsafe{CreateDesktopW(wide.as_ptr(),std::ptr::null(),std::ptr::null(),0,0x000f00ff,std::ptr::null())};
    ensure!(!desktop.is_null(),"create owned theme desktop: {}",windows::core::Error::from_thread());
    let raw=desktop as usize;
    let result=thread::spawn(move||->anyhow::Result<T>{unsafe{SetThreadDesktop(raw as*mut _)}.ok()?;action()}).join();
    let deadline=Instant::now()+Duration::from_secs(3);let mut closed=false;
    while Instant::now()<deadline{if unsafe{CloseDesktop(desktop)}.as_bool(){closed=true;break;}thread::sleep(Duration::from_millis(20));}
    ensure!(closed,"owned theme desktop retained after fixture thread exit: {name}");
    result.map_err(|_|anyhow::anyhow!("theme fixture thread panicked"))?
}
pub(super) struct CommonControls{manifest:PathBuf,created:bool,context:HANDLE,cookie:Option<usize>,module:Option<HMODULE>,version:[u32;3]}
impl CommonControls{
    pub(super) fn activate(output:Option<&Path>)->anyhow::Result<Self>{
        static SERIAL:AtomicU64=AtomicU64::new(1);
        let root=output.map(Path::to_path_buf).unwrap_or_else(std::env::temp_dir);
        ensure!(root.is_absolute()&&root.parent().is_some_and(|parent|parent.parent().is_some()),"dedicated absolute native theme fixture directory required");
        for ancestor in root.ancestors(){if ancestor.exists(){ensure!(fs::symlink_metadata(ancestor)?.file_attributes()&0x400==0,"native theme fixture crosses a reparse point");}}
        fs::create_dir_all(&root)?;
        let manifest=root.join(format!("common-controls-{}-{}.manifest",std::process::id(),SERIAL.fetch_add(1,Ordering::Relaxed)));
        let mut owner=Self{manifest,created:false,context:HANDLE::default(),cookie:None,module:None,version:[0;3]};
        let mut file=fs::OpenOptions::new().write(true).create_new(true).open(&owner.manifest)?;owner.created=true;file.write_all(MANIFEST.as_bytes())?;drop(file);
        let path=owner.manifest.as_os_str().encode_wide().chain(Some(0)).collect::<Vec<_>>();
        let spec=ContextSpec{size:std::mem::size_of::<ContextSpec>()as u32,flags:0,source:path.as_ptr(),architecture:0,language:0,directory:std::ptr::null(),resource:std::ptr::null(),application:std::ptr::null(),module:HMODULE::default()};
        let context=unsafe{CreateActCtxW(&spec)};
        ensure!(context.0 as isize!=-1&&!context.0.is_null(),"native theme activation context: {}",windows::core::Error::from_thread());owner.context=context;
        let mut cookie=0;unsafe{ActivateActCtx(owner.context,&mut cookie)}.ok()?;owner.cookie=Some(cookie);
        let module=unsafe{LoadLibraryW(windows::core::w!("comctl32.dll"))}?;owner.module=Some(module);
        let initialize=unsafe{GetProcAddress(module,PCSTR(b"InitCommonControlsEx\0".as_ptr()))}.context("Common-Controls init entry")?;
        let initialize:unsafe extern "system" fn(*const INITCOMMONCONTROLSEX)->BOOL=unsafe{std::mem::transmute(initialize)};
        unsafe{initialize(&INITCOMMONCONTROLSEX{dwSize:std::mem::size_of::<INITCOMMONCONTROLSEX>()as u32,dwICC:ICC_LISTVIEW_CLASSES|ICC_TREEVIEW_CLASSES|ICC_STANDARD_CLASSES})}.ok()?;
        let get_version=unsafe{GetProcAddress(module,PCSTR(b"DllGetVersion\0".as_ptr()))}.context("Common-Controls version entry")?;
        let get_version:unsafe extern "system" fn(*mut Version)->HRESULT=unsafe{std::mem::transmute(get_version)};
        let mut version=Version{size:std::mem::size_of::<Version>()as u32,..Default::default()};unsafe{get_version(&mut version)}.ok()?;
        ensure!(version.major>=6,"native fixture did not load Common-Controls v6");owner.version=[version.major,version.minor,version.build];Ok(owner)
    }
    pub(super) fn version(&self)->[u32;3]{self.version}
}
impl Drop for CommonControls{fn drop(&mut self){
    if let Some(module)=self.module.take(){let _=unsafe{FreeLibrary(module)};}
    if let Some(cookie)=self.cookie.take(){let _=unsafe{DeactivateActCtx(0,cookie)};}
    if !self.context.0.is_null(){unsafe{ReleaseActCtx(self.context);}}
    if self.created&&fs::symlink_metadata(&self.manifest).is_ok_and(|meta|meta.file_attributes()&0x400==0)
        &&fs::read(&self.manifest).is_ok_and(|bytes|MANIFEST.as_bytes().starts_with(&bytes)){let _=fs::remove_file(&self.manifest);}
}}
