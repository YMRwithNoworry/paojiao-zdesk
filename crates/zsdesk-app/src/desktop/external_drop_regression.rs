use super::*;
use anyhow::{Context,Result,ensure};
use std::{ffi::c_void,fs,thread};
use zsdesk_core::model::desktop::{DesktopConfig,Fence,FenceSource,Placement};
use zsdesk_windows::gfx::Gfx;
use super::super::{HOST,HOST_HWND};
#[link(name="user32")]unsafe extern "system"{
    fn CreateDesktopW(name:*const u16,device:*const u16,mode:*const c_void,flags:u32,access:u32,attributes:*const c_void)->*mut c_void;
    fn SetThreadDesktop(desktop:*mut c_void)->i32;
    fn CloseDesktop(desktop:*mut c_void)->i32;
}
struct Fixture(HWND);impl Drop for Fixture{fn drop(&mut self){
    TEST_DESKTOP.with(|slot|slot.borrow_mut().take());
    if let Some(mut host)=HOST.with(|slot|slot.borrow_mut().take()){host.config.settings.icons_hidden_by_zsdesk=false;host.shutdown();}
    HOST_HWND.with(|slot|slot.set(0));let _=unsafe{DestroyWindow(self.0)};
}}
fn exercise(root:&Path)->Result<serde_json::Value>{
    unsafe{OleInitialize(None)}?;struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}let _apartment=Apartment;
    let source=root.join("source");let target=root.join("desktop");fs::create_dir_all(&source)?;fs::create_dir_all(&target)?;
    let instance=windows::Win32::Foundation::HINSTANCE(unsafe{windows::Win32::System::LibraryLoader::GetModuleHandleW(None)}?.0);
    unsafe{RegisterClassW(&WNDCLASSW{lpfnWndProc:Some(super::super::host_proc),hInstance:instance,lpszClassName:windows::core::w!("ZSDesk.ExternalDrop.Fixture"),..Default::default()});
        RegisterClassW(&WNDCLASSW{lpfnWndProc:Some(super::super::fence_proc),hInstance:instance,lpszClassName:windows::core::w!("ZSDesk.Native.Fence"),..Default::default()});}
    let hwnd=unsafe{CreateWindowExW(WS_EX_TOOLWINDOW|WS_EX_NOACTIVATE,windows::core::w!("ZSDesk.ExternalDrop.Fixture"),None,WS_POPUP,0,0,1,1,None,None,Some(instance),None)}?;
    let _fixture=Fixture(hwnd);
    let mut config=DesktopConfig::default();config.settings.organize=true;config.settings.icons_hidden_by_zsdesk=false;config.settings.hide_tray=true;
    let mut catch_all=Fence::new(1,"Collection",FenceSource::Desktop,Placement{x:10,y:10,width:200,height:180,..Default::default()});catch_all.catch_all=true;
    config.fences.push(catch_all);config.next_id=2;
    let mut host=host::DesktopHost::new(hwnd,root.join("profile/desktop-fences.json"),root.join("profile/explorer-session.json"),config,Gfx::new("Segoe UI")?);
    host.desktop_test_entries=Some(Vec::new());host.sync_windows();host.refresh_desktop();
    HOST.with(|slot|*slot.borrow_mut()=Some(host));HOST_HWND.with(|slot|slot.set(hwnd.0 as isize));TEST_DESKTOP.with(|slot|*slot.borrow_mut()=Some(target.clone()));
    let drop=ExternalDrop{owner:hwnd,delegate:RefCell::new(None),on_desktop:std::cell::Cell::new(false)};
    let make_data=|path:&Path|->Result<IDataObject>{let entry=desktop::entry_from_parsing_name(&path.to_string_lossy()).context("source entry")?;Ok(desktop::data_object(hwnd,&[entry.pidl])?)};
    for (name,copy,point) in [("copy 中文.txt",true,POINT{x:600,y:400}),("move 中文.txt",false,POINT{x:1000,y:600})]{
        let path=source.join(name);fs::write(&path,b"external drop fixture")?;let data=make_data(&path)?;
        let requested=if copy{DROPEFFECT_COPY}else{DROPEFFECT_MOVE};let keys=if copy{8}else{4};
        ensure!(drop.enter(&data,keys|1,point,requested)==requested,"desktop effect rejected {name}");
        let returned=drop.dropped(&data,keys,point,requested);
        ensure!(returned==if copy{DROPEFFECT_COPY}else{DROPEFFECT_NONE},"optimized move effect protocol");
        ensure!(path.exists()==copy&&fs::read(target.join(name))?==b"external drop fixture","transfer result mismatch");
        with_host(|host|->Result<()>{
            ensure!(host.config.fences[0].members.is_empty(),"external drop joined automatic collection");
            let item=host.config.released_items.iter().find(|item|item.members.iter().any(|key|Path::new(key).file_name()==Some(std::ffi::OsStr::new(name)))).context("loose icon missing")?;
            let monitor=host::monitors().into_iter().find(|monitor|monitor.name==item.placement.monitor).context("destination monitor")?;
            let x=super::super::drag_placement::signed_scale(point.x-monitor.work.left,96,monitor.dpi)-48;
            let y=super::super::drag_placement::signed_scale(point.y-monitor.work.top,96,monitor.dpi)-24;
            ensure!((item.placement.x-x).abs()<=1&&(item.placement.y-y).abs()<=1,"external drop lost screen location");Ok(())
        }).context("desktop host")??;
    }
    let path=target.join("copy 中文.txt");let data=make_data(&path)?;let point=POINT{x:700,y:750};
    ensure!(drop.enter(&data,0,point,DROPEFFECT_MOVE)==DROPEFFECT_MOVE,"same-directory visual move was rejected");
    ensure!(drop.dropped(&data,0,point,DROPEFFECT_MOVE)==DROPEFFECT_NONE,"same-directory move protocol");
    ensure!(with_host(|host|host.config.released_items.len())==Some(2)&&fs::read(&path)?==b"external drop fixture","reposition duplicated or changed a file");
    let link=source.join("shortcut source.txt");fs::write(&link,b"shortcut fixture")?;let data=make_data(&link)?;
    let effect=windows::Win32::System::Ole::DROPEFFECT_LINK;
    unsafe extern "system" fn cancel_menu(_:HWND,_:u32,_:usize,_:u32){let _=unsafe{EndMenu()};}
    let timer=unsafe{SetTimer(None,0,3000,Some(cancel_menu))};
    struct Timer(usize);impl Drop for Timer{fn drop(&mut self){let _=unsafe{KillTimer(None,self.0)};}}let _timer=Timer(timer);
    ensure!(drop.enter(&data,33,point,effect)==effect,"shortcut drag lost native negotiation");
    ensure!(drop.dropped(&data,32,point,effect)==effect,"shortcut drop was rejected");
    ensure!(link.exists()&&fs::read_dir(&target)?.filter_map(|entry|entry.ok()).any(|entry|entry.path().extension().is_some_and(|extension|extension.eq_ignore_ascii_case("lnk"))),"native shortcut was not created");
    Ok(serde_json::json!({"copy_and_move":true,"catch_all_bypassed":true,"screen_position_preserved":true,"same_directory_reposition":true,"native_shortcut_drop":true,"real_desktop_modified":false}))
}
#[test]
#[ignore="Real OLE drop and Shell transfer on a private desktop with isolated files"]
fn native_external_drop_keeps_files_outside_fences_at_pointer()->Result<()>{
    let root=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("output")?).join("external-drop");
    ensure!(root.is_absolute()&&root.starts_with(r"E:\codex操作目录"),"isolated operation root required");fs::create_dir_all(&root)?;
    let name=wide(Path::new(&format!("ZSDesk.ExternalDrop.{}",std::process::id())));
    let desktop=unsafe{CreateDesktopW(name.as_ptr(),std::ptr::null(),std::ptr::null(),0,0x000f00ff,std::ptr::null())};ensure!(!desktop.is_null(),"private desktop creation");
    let raw=desktop as usize;let output=root.clone();
    let result=thread::spawn(move||->Result<serde_json::Value>{ensure!(unsafe{SetThreadDesktop(raw as*mut _)}!=0,"private desktop attach");exercise(&output)}).join().map_err(|_|anyhow::anyhow!("external drop test panicked")).and_then(|result|result);
    let closed=unsafe{CloseDesktop(desktop)}!=0;let report=serde_json::json!({"checks":result.as_ref().map_err(|error|format!("{error:#}")),"desktop_closed":closed});
    fs::write(root.join("verification.current.json"),serde_json::to_vec_pretty(&report)?)?;result?;ensure!(closed,"private desktop close");Ok(())
}
