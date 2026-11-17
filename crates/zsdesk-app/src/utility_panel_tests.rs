use super::*;
use std::{time::{Duration,Instant},fs,thread,ffi::c_void};
#[link(name="user32")]unsafe extern "system"{fn CreateDesktopW(name:*const u16,device:*const u16,mode:*const c_void,flags:u32,access:u32,attributes:*const c_void)->*mut c_void;fn SetThreadDesktop(value:*mut c_void)->i32;fn CloseDesktop(value:*mut c_void)->i32;}
#[link(name="user32")]unsafe extern "system"{fn PrintWindow(window:HWND,dc:HDC,flags:u32)->i32;fn EnumDesktopWindows(desktop:*mut c_void,callback:Option<unsafe extern "system" fn(HWND,LPARAM)->windows::core::BOOL>,data:LPARAM)->windows::core::BOOL;fn GetUserObjectInformationW(handle:*mut c_void,index:i32,info:*mut c_void,length:u32,needed:*mut u32)->windows::core::BOOL;}
#[link(name="user32")]unsafe extern "system"{
    fn CreateWindowStationW(name:*const u16,flags:u32,access:u32,security:*const c_void)->*mut c_void;
    fn OpenWindowStationW(name:*const u16,inherit:i32,access:u32)->*mut c_void;
    fn GetProcessWindowStation()->*mut c_void;
    fn SetProcessWindowStation(station:*mut c_void)->i32;
    fn CloseWindowStation(station:*mut c_void)->i32;
}
struct Station{handle:*mut c_void,original:*mut c_void}
impl Station{
    fn enter(name:&str,create:bool)->Result<Self>{
        let original=unsafe{GetProcessWindowStation()};ensure!(!original.is_null(),"original station missing");
        let handle=unsafe{if create{CreateWindowStationW(wide(name).as_ptr(),1,0x000f037f,std::ptr::null())}else{OpenWindowStationW(wide(name).as_ptr(),0,0x000f037f)}};
        ensure!(!handle.is_null(),"private station unavailable: {}",unsafe{GetLastError()}.0);
        let owned=Self{handle,original};ensure!(unsafe{SetProcessWindowStation(handle)}!=0,"private station attachment failed");Ok(owned)
    }
    fn close(&mut self)->bool{if self.handle.is_null(){return true;}let restored=unsafe{SetProcessWindowStation(self.original)}!=0;
        let closed=restored&&unsafe{CloseWindowStation(self.handle)}!=0;if closed{self.handle=std::ptr::null_mut();}closed}
}
impl Drop for Station{fn drop(&mut self){self.close();}}
fn pump(){let mut message=MSG::default();for _ in 0..128{if !unsafe{PeekMessageW(&mut message,None,0,0,PM_REMOVE)}.as_bool(){break;}if message.message!=WM_QUIT{unsafe{let _=TranslateMessage(&message);DispatchMessageW(&message);}}}}
struct Window(HWND);impl Drop for Window{fn drop(&mut self){if unsafe{IsWindow(Some(self.0))}.as_bool(){let _=unsafe{DestroyWindow(self.0)};}}}
fn capture(hwnd:HWND,path:&Path)->Result<()>{
    let mut r=RECT::default();unsafe{GetClientRect(hwnd,&mut r)?;}
    let surface=zsdesk_windows::gfx::Surface::new(r.right,r.bottom).context("screenshot DIB")?;
    // A noninteractive station has no DWM redirection surface. Render the
    // client and each actual child into its own client-coordinate origin.
    unsafe{SendMessageW(hwnd,WM_PRINTCLIENT,Some(WPARAM(surface.dc.0 as usize)),None);}
    let pointer=unsafe{GetWindowLongPtrW(hwnd,GWLP_USERDATA)}as*const RefCell<Panel>;let panel=unsafe{&*pointer};
    let children={let p=panel.borrow();vec![p.edit,p.list,p.action,p.close,unsafe{GetDlgItem(Some(hwnd),14)}.unwrap_or_default()]};
    for child in children{if child.0.is_null(){continue;}let mut bounds=RECT::default();unsafe{GetWindowRect(child,&mut bounds)?;}
        let mut origin=POINT{x:bounds.left,y:bounds.top};unsafe{ScreenToClient(hwnd,&mut origin)}.ok()?;
        let saved=unsafe{SaveDC(surface.dc)};unsafe{SetViewportOrgEx(surface.dc,origin.x,origin.y,None);
            SendMessageW(child,WM_PRINT,Some(WPARAM(surface.dc.0 as usize)),Some(LPARAM((PRF_CLIENT|PRF_NONCLIENT|PRF_ERASEBKGND|PRF_CHILDREN)as isize)));
            if saved!=0{RestoreDC(surface.dc,saved);}}
    }
    let bytes=surface.pixel_bytes()as u32;let mut bmp=Vec::new();bmp.extend(b"BM");bmp.extend((54+bytes).to_le_bytes());bmp.extend([0;4]);bmp.extend(54u32.to_le_bytes());bmp.extend(40u32.to_le_bytes());
    bmp.extend(r.right.to_le_bytes());bmp.extend((-r.bottom).to_le_bytes());bmp.extend(1u16.to_le_bytes());bmp.extend(32u16.to_le_bytes());bmp.extend([0;4]);bmp.extend(bytes.to_le_bytes());bmp.extend([0;16]);
    let pixels=unsafe{std::slice::from_raw_parts(surface.bits,bytes as usize)};
    ensure!(pixels.chunks_exact(4).any(|pixel|pixel[..3]!=pixels[..3]),"capture contains no rendered client content");
    bmp.extend(pixels);fs::write(path,bmp)?;Ok(())
}

#[derive(serde::Serialize,serde::Deserialize,Debug)]
struct RemainingWindow{class:String,pid:u32,thread:u32,visible:bool}
fn windows_on(desktop:*mut c_void)->Result<Vec<RemainingWindow>>{
    let mut windows=Vec::<RemainingWindow>::new();
    unsafe extern "system" fn each(hwnd:HWND,data:LPARAM)->windows::core::BOOL{
        let mut name=[0u16;128];let count=unsafe{GetClassNameW(hwnd,&mut name)}.max(0)as usize;
        let mut pid=0;let thread=unsafe{GetWindowThreadProcessId(hwnd,Some(&mut pid))};
        unsafe{&mut*(data.0 as*mut Vec<RemainingWindow>)}.push(RemainingWindow{
            class:String::from_utf16_lossy(&name[..count]),pid,thread,visible:unsafe{IsWindowVisible(hwnd)}.as_bool()});true.into()
    }
    let mut name=[0u16;256];let mut needed=0;
    ensure!(unsafe{GetUserObjectInformationW(desktop,2,name.as_mut_ptr().cast(),std::mem::size_of_val(&name)as u32,&mut needed)}.as_bool(),"invalid private desktop handle");
    let end=name.iter().position(|c|*c==0).unwrap_or(name.len());ensure!(String::from_utf16_lossy(&name[..end]).starts_with("ZSDesk.Utilities."),"unexpected desktop identity");
    unsafe{SetLastError(WIN32_ERROR(0));}
    let enumerated=unsafe{EnumDesktopWindows(desktop,Some(each),LPARAM((&mut windows as*mut Vec<RemainingWindow>)as isize))}.as_bool();
    let error=unsafe{GetLastError()}.0;
    // An empty desktop returns FALSE with ERROR_SUCCESS on Windows. Require
    // independently verified handle identity and no callbacks for that case.
    ensure!(enumerated||(error==0&&windows.is_empty()),"private desktop enumeration failed: {error}");Ok(windows)
}
fn owned_system_input(window:&RemainingWindow,pid:u32)->bool{
    window.pid==pid&&matches!(window.class.as_str(),"Touch Tooltip Window"|"UAC_InputIndicatorOverlayWnd"|"UAC Input Indicator"|"CicLoaderWndClass"|"CiceroUIWndFrame"|"MSCTFIME UI"|"IME")
}
#[link(name="user32")]unsafe extern "system"{fn OpenDesktopW(name:*const u16,flags:u32,inherit:i32,access:u32)->*mut c_void;}
fn run_ui_exercise(root:PathBuf,raw:usize)->Result<()>{
        ensure!(unsafe{SetThreadDesktop(raw as*mut c_void)}!=0,"private desktop attachment failed");unsafe{windows::Win32::System::Ole::OleInitialize(None)?;}
        TEST_NO_ACTIVATE.with(|value|value.set(true));
        struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{windows::Win32::System::Ole::OleUninitialize();}}}let _apartment=Apartment;
        let profile=root.join("explorer-session.json");let mut config=crate::storage::default_session();config.ui.search_backend=crate::fast_search::BackendPreference::DirectoryScan;crate::storage::save(&profile,&config)?;
        let about=Window(show(HWND::default(),&profile,Kind::About,format!("ZSDesk {} 全量版\r\n\r\n多标签文件管理器与桌面格子\r\n\r\n项目主页  github.com/qiu7824/zsdesk",env!("CARGO_PKG_VERSION")))?);pump();capture(about.0,&root.join("about.bmp"))?;
        let pointer=unsafe{GetWindowLongPtrW(about.0,GWLP_USERDATA)}as*const RefCell<Panel>;let panel=unsafe{&*pointer};
        let button=panel.borrow().action;ensure!(text(about.0)=="关于 ZSDesk","about caption missing");ensure!(text(button)=="检查更新"&&unsafe{IsWindowEnabled(button)}.as_bool(),"about update action missing");
        let short=Window(show(HWND::default(),&profile,Kind::Shortcuts,crate::window::shortcut_help_text().into())?);pump();ensure!(unsafe{IsWindow(Some(short.0))}.as_bool(),"shortcut panel missing");
        ensure!(text(short.0)=="键盘快捷键","shortcut caption missing");capture(short.0,&root.join("shortcuts.bmp"))?;
        let pointer=unsafe{GetWindowLongPtrW(short.0,GWLP_USERDATA)}as*const RefCell<Panel>;let info=unsafe{&*pointer};
        info.borrow_mut().scroll_info(7,0);pump();ensure!(info.borrow().scroll>0,"shortcut list did not scroll");capture(short.0,&root.join("shortcuts-end.bmp"))?;
        drop(short);drop(about);
        let files=root.join("files");fs::create_dir_all(&files)?;fs::write(files.join("关键词-甲.txt"),b"owned search fixture")?;
        show_search(&profile)?;let window=Window(SEARCH.with(|slot|HWND(slot.get()as*mut _)));pump();
        ensure!(text(window.0)=="快速搜索","search caption missing");capture(window.0,&root.join("quick-search-empty.bmp"))?;
        crate::window::verify_builtin_preview_without_handlers(window.0,&files.join("关键词-甲.txt"))?;
        let ptr=unsafe{GetWindowLongPtrW(window.0,GWLP_USERDATA)}as*const RefCell<Panel>;let state=unsafe{&*ptr};
        state.borrow_mut().engine=Some(FastSearch::new_with_roots_and_window_for_test(root.join("search-index.zsi"),vec![files.clone()],window.0,RESULTS)?);
        let edit=state.borrow().edit;let _=unsafe{SetActiveWindow(window.0)};let _=unsafe{SetFocus(Some(window.0))};pump();ensure!(unsafe{GetFocus()}==edit,"search activation did not focus input: actual={:?} edit={edit:?} root={:?} enabled={} visible={}",unsafe{GetFocus()},window.0,unsafe{IsWindowEnabled(edit)}.as_bool(),unsafe{IsWindowVisible(edit)}.as_bool());
        set_text(edit,"关键词");let deadline=Instant::now()+Duration::from_secs(10);
        loop{pump();if !state.borrow().paths.is_empty(){break;}ensure!(Instant::now()<deadline,"search panel produced no matching file");thread::sleep(Duration::from_millis(10));}
        ensure!(state.borrow().paths.iter().all(|path|path.file_name().unwrap().to_string_lossy().contains("关键词")),"wrong search result");
        pump();capture(window.0,&root.join("quick-search.bmp"))?;
        let retained_engine=state.borrow().engine.as_ref().unwrap()as*const FastSearch;
        unsafe{SendMessageW(window.0,WM_CLOSE,None,None);}pump();ensure!(!search_open()&&search_exists(),"closing search discarded its reusable panel");
        ensure!(state.borrow().paths.is_empty()&&text(edit).is_empty(),"closed search retained actionable results");
        config.ui.appearance.color_scheme=zsdesk_core::model::explorer::ColorScheme::Dark;crate::storage::save(&profile,&config)?;
        show_search(&profile)?;pump();ensure!(SEARCH.with(|slot|slot.get())==window.0.0 as isize,"reopening search created another panel");
        ensure!(state.borrow().palette.mode==Mode::Dark,"reused search ignored changed appearance");
        ensure!(state.borrow().engine.as_ref().unwrap()as*const FastSearch==retained_engine,"reopening search discarded its memory index");
        set_text(edit,"关键词");state.borrow_mut().search();
        {let _held=state.borrow_mut();let until=Instant::now()+Duration::from_millis(150);while Instant::now()<until{pump();thread::sleep(Duration::from_millis(5));}}
        let deadline=Instant::now()+Duration::from_secs(10);while state.borrow().paths.is_empty(){pump();ensure!(Instant::now()<deadline,"reopened search lost a result notification during a nested message pump");thread::sleep(Duration::from_millis(10));}
        ensure!(state.borrow().paths.len()==1,"reopened query did not return the fixture");
        set_text(edit,"");ensure!(state.borrow().paths.is_empty()&&state.borrow().generation==0,"old results remained actionable after clearing query");
        unsafe{SendMessageW(edit,WM_IME_STARTCOMPOSITION,None,None);SendMessageW(edit,WM_KEYDOWN,Some(WPARAM(VK_ESCAPE.0 as usize)),None);}pump();
        ensure!(search_open(),"IME Escape closed the search panel");unsafe{SendMessageW(edit,WM_IME_ENDCOMPOSITION,None,None);}
        let rect=RECT{left:10,top:10,right:1080,bottom:780};unsafe{SendMessageW(window.0,WM_DPICHANGED,Some(WPARAM(144|(144<<16))),Some(LPARAM((&rect as*const RECT)as isize)));}
        ensure!(state.borrow().dpi==144,"DPI notification ignored");
        let weak=unsafe{Rc::increment_strong_count(ptr);Rc::from_raw(ptr)};let destroyed=Rc::downgrade(&weak);
        {let _borrow=weak.borrow_mut();let _=unsafe{DestroyWindow(window.0)};}
        drop(weak);ensure!(destroyed.upgrade().is_none(),"destroy during callback leaked panel state");ensure!(!search_open(),"closed quick search remained registered");drop(window);
        config.ui.appearance.color_scheme=zsdesk_core::model::explorer::ColorScheme::Dark;crate::storage::save(&profile,&config)?;
        show_search(&profile)?;let dark=Window(SEARCH.with(|slot|HWND(slot.get()as*mut _)));pump();
        let pointer=unsafe{GetWindowLongPtrW(dark.0,GWLP_USERDATA)}as*const RefCell<Panel>;let state=unsafe{&*pointer};
        state.borrow_mut().engine=Some(FastSearch::new_with_roots_and_window_for_test(root.join("search-index.zsi"),vec![files],dark.0,RESULTS)?);
        let edit=state.borrow().edit;set_text(edit,"关键词");let deadline=Instant::now()+Duration::from_secs(10);
        loop{pump();if !state.borrow().paths.is_empty(){break;}ensure!(Instant::now()<deadline,"dark search produced no matching file");thread::sleep(Duration::from_millis(10));}
        let dc=unsafe{GetDC(Some(edit))};unsafe{SendMessageW(dark.0,WM_CTLCOLOREDIT,Some(WPARAM(dc.0 as usize)),Some(LPARAM(edit.0 as isize)));}
        let contrast=unsafe{GetTextColor(dc)}!=unsafe{GetBkColor(dc)};unsafe{ReleaseDC(Some(edit),dc);}
        ensure!(contrast,"dark search edit has invisible text");pump();capture(dark.0,&root.join("quick-search-dark.bmp"))?;drop(dark);
        config.ui.appearance.color_scheme=zsdesk_core::model::explorer::ColorScheme::Light;crate::storage::save(&profile,&config)?;
        Ok(())
}

#[test]#[ignore="Internal owned utility-panel subprocess; requires parent gate and private desktop"]
fn native_utility_panel_helper_entry()->Result<()>{
    let root=PathBuf::from(std::env::var_os("ZSDESK_UTILITY_TEST_ROOT").context("owned helper root")?);
    let name=std::env::var("ZSDESK_UTILITY_TEST_DESKTOP").context("owned desktop name")?;
    let gate=PathBuf::from(std::env::var_os("ZSDESK_UTILITY_TEST_GATE").context("owned helper gate")?);
    ensure!(root.is_absolute()&&gate.parent()==Some(root.as_path()),"invalid helper boundary");
    let deadline=Instant::now()+Duration::from_secs(5);while !gate.exists(){ensure!(Instant::now()<deadline,"parent did not establish the helper Job");thread::sleep(Duration::from_millis(5));}
    let station_name=std::env::var("ZSDESK_UTILITY_TEST_STATION").context("owned station name")?;
    let _station=Station::enter(&station_name,false)?;
    let desktop=unsafe{OpenDesktopW(wide(&name).as_ptr(),0,0,0x000f00ff)};ensure!(!desktop.is_null(),"helper desktop open failed");
    struct Desktop(*mut c_void);impl Drop for Desktop{fn drop(&mut self){let _=unsafe{CloseDesktop(self.0)};}}let _desktop=Desktop(desktop);
    let raw=desktop as usize;let output=root.clone();
    let result=thread::spawn(move||run_ui_exercise(output,raw)).join().map_err(|_|anyhow::anyhow!("utility regression panicked")).and_then(|result|result);
    let remaining=windows_on(desktop)?;let input_only=remaining.iter().all(|window|owned_system_input(window,std::process::id()));
    let report=serde_json::json!({"pid":std::process::id(),"result":result.as_ref().map_err(|error|format!("{error:#}")),
        "remaining_windows":remaining,"remaining_owned_system_input_only":input_only,"application_windows_destroyed":input_only});
    fs::write(root.join("helper.current.json"),serde_json::to_vec_pretty(&report)?)?;
    result?;ensure!(input_only,"unexpected remaining application windows: {report}");Ok(())
}

#[test]#[ignore="Native utility panels in an owned process and private desktop; no user windows changed"]
fn native_utility_panels_search_ime_and_lifecycle()->Result<()>{
    use std::{process::{Command,Stdio},os::windows::{process::CommandExt,io::AsRawHandle}};
    use windows::Win32::System::{JobObjects::*,Threading::GetProcessTimes};
    let root=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("test output")?).join("utility-panels");fs::create_dir_all(&root)?;
    let before=unsafe{GetForegroundWindow()};let name=format!("ZSDesk.Utilities.{}",std::process::id());
    let station_name=format!("ZSDesk.UtilityStation.{}",std::process::id());let mut station=Station::enter(&station_name,true)?;
    let desktop=unsafe{CreateDesktopW(wide(&name).as_ptr(),std::ptr::null(),std::ptr::null(),0,0x000f00ff,std::ptr::null())};ensure!(!desktop.is_null(),"private desktop creation failed");
    struct Desktop(*mut c_void);impl Drop for Desktop{fn drop(&mut self){if !self.0.is_null(){let _=unsafe{CloseDesktop(self.0)};}}}let mut owned_desktop=Desktop(desktop);
    let job=unsafe{CreateJobObjectW(None,PCWSTR::null())}?;struct Job(HANDLE);impl Drop for Job{fn drop(&mut self){let _=unsafe{CloseHandle(self.0)};}}let job=Job(job);
    let mut limits=JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();limits.BasicLimitInformation.LimitFlags=JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    unsafe{SetInformationJobObject(job.0,JobObjectExtendedLimitInformation,(&limits as*const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),std::mem::size_of_val(&limits)as u32)?;}
    let gate=root.join(format!("gate-{}",std::process::id()));ensure!(!gate.exists(),"helper gate already exists");
    struct Gate(PathBuf);impl Drop for Gate{fn drop(&mut self){let _=fs::remove_file(&self.0);}}let _gate=Gate(gate.clone());
    let log=fs::File::create(root.join("helper.current.log"))?;
    let child=Command::new(std::env::current_exe()?).args(["utility_panel::tests::native_utility_panel_helper_entry","--exact","--ignored","--nocapture","--test-threads=1"])
        .env("ZSDESK_UTILITY_TEST_ROOT",&root).env("ZSDESK_UTILITY_TEST_DESKTOP",&name).env("ZSDESK_UTILITY_TEST_GATE",&gate)
        .env("ZSDESK_UTILITY_TEST_STATION",&station_name)
        .creation_flags(0x08000000).stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log).spawn()?;
    struct Running(std::process::Child);impl Drop for Running{fn drop(&mut self){if self.0.try_wait().ok().flatten().is_none(){let _=self.0.kill();let _=self.0.wait();}}}let mut helper=Running(child);
    let process=HANDLE(helper.0.as_raw_handle());let pid=helper.0.id();let(mut created,mut exited,mut kernel,mut user)=(FILETIME::default(),FILETIME::default(),FILETIME::default(),FILETIME::default());
    unsafe{GetProcessTimes(process,&mut created,&mut exited,&mut kernel,&mut user)?;AssignProcessToJobObject(job.0,process)?;}
    fs::write(&gate,b"Job boundary established")?;
    let deadline=Instant::now()+Duration::from_secs(45);
    let status=loop{if let Some(status)=helper.0.try_wait()?{break status;}ensure!(Instant::now()<deadline,"owned utility helper exceeded 45 seconds");thread::sleep(Duration::from_millis(20));};
    let mut accounting=JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
    unsafe{QueryInformationJobObject(Some(job.0),JobObjectBasicAccountingInformation,(&mut accounting as*mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),std::mem::size_of_val(&accounting)as u32,None)?;}
    // Release the completed process and Job references before asking Windows
    // to retire the process's desktop object.
    drop(helper);drop(job);
    let remaining=windows_on(desktop)?;
    let closed=unsafe{CloseDesktop(desktop)}!=0;if closed{owned_desktop.0=std::ptr::null_mut();}
    let deadline=Instant::now()+Duration::from_secs(5);
    let(absent,open_error)=loop{
        unsafe{SetLastError(WIN32_ERROR(0));}
        let reopened=unsafe{OpenDesktopW(wide(&name).as_ptr(),0,0,0x000f00ff)};let error=unsafe{GetLastError()}.0;
        if reopened.is_null(){break(true,error);}
        ensure!(unsafe{CloseDesktop(reopened)}!=0,"desktop verification handle failed to close");
        if Instant::now()>=deadline{break(false,error);}
        thread::sleep(Duration::from_millis(50));
    };
    let station_closed=station.close();
    let mut foreground_pid=0;unsafe{GetWindowThreadProcessId(GetForegroundWindow(),Some(&mut foreground_pid));}
    let isolated=foreground_pid!=std::process::id()&&foreground_pid!=pid;
    let child_report:serde_json::Value=serde_json::from_slice(&fs::read(root.join("helper.current.json"))?)?;
    let report=serde_json::json!({"helper":child_report,"helper_pid":pid,"helper_created":u64::from(created.dwHighDateTime)<<32|u64::from(created.dwLowDateTime),
        "helper_exit":status.code(),"job_total_processes":accounting.TotalProcesses,"job_active_processes":accounting.ActiveProcesses,
        "remaining_after_process_exit":remaining,"private_desktop_closed":closed,"private_desktop_absent":absent,"open_after_close_error":open_error,
        "private_station_closed":station_closed,
        "foreground_unchanged":before==unsafe{GetForegroundWindow()},"test_did_not_activate":isolated});
    fs::write(root.join("verification.current.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");
    ensure!(status.success(),"utility helper failed: {}",fs::read_to_string(root.join("helper.current.log"))?);
    ensure!(child_report["pid"].as_u64()==Some(u64::from(pid))&&child_report["remaining_owned_system_input_only"]==true,"helper evidence mismatch");
    ensure!(accounting.TotalProcesses==1&&accounting.ActiveProcesses==0,"utility helper created descendants or retained a process");
    ensure!(remaining.is_empty()&&closed&&absent&&open_error==2&&station_closed&&isolated,"native utility process/desktop cleanup failed");Ok(())
}

#[test]fn reveal_results_selects_both_files_and_directories(){
    for path in [Path::new(r"C:\own\folder"),Path::new(r"C:\own\file.txt")]{
        assert_eq!(location_arguments(path,true),vec![std::ffi::OsString::from("--select"),path.as_os_str().to_owned()]);
        assert_eq!(location_arguments(path,false),vec![path.as_os_str().to_owned()]);
    }
}
