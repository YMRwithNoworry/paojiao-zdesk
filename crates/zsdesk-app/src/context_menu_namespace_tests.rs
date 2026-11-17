//! Read-only context construction across filesystem and Shell namespaces.
use super::*;
use std::{ffi::c_void,thread};
use windows::core::HRESULT;
#[link(name="user32")]unsafe extern "system"{
    fn CreateDesktopW(name:*const u16,device:*const u16,mode:*const c_void,flags:u32,access:u32,security:*const c_void)->*mut c_void;
    fn SetThreadDesktop(desktop:*mut c_void)->i32;
    fn CloseDesktop(desktop:*mut c_void)->i32;
}
fn exercise(root:&Path)->Result<Vec<serde_json::Value>>{
    unsafe{OleInitialize(None)?;}struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}let _apartment=Apartment;
    let _cache=background_view::CacheLifetime;
    let owner=unsafe{CreateWindowExW(WS_EX_NOACTIVATE|WS_EX_TOOLWINDOW,w!("STATIC"),PCWSTR::null(),WS_POPUP,0,0,400,300,None,None,None,None)}?;
    struct Window(HWND);impl Drop for Window{fn drop(&mut self){let _=unsafe{DestroyWindow(self.0)};}}let _owner=Window(owner);
    let first=root.join("first 中文");let second=root.join("second 中文");fs::create_dir_all(&first)?;fs::create_dir_all(&second)?;
    let a=first.join("一.txt");let b=first.join("二.txt");let c=second.join("三.txt");for path in [&a,&b,&c]{fs::write(path,b"readonly context fixture")?;}
    let identity=|path:&Path|zsdesk_windows::shell::pidl::location_pidl(&zsdesk_core::model::explorer::ShellLocation::Path(path.into())).context("filesystem identity");
    let drive=root.ancestors().last().context("fixture drive root")?;
    let mut rows=Vec::new();
    for (name,paths) in [("file",vec![a.as_path()]),("folder",vec![first.as_path()]),("same-parent-multiselect",vec![a.as_path(),b.as_path()]),("mixed-parent-multiselect",vec![a.as_path(),c.as_path()]),("drive-root",vec![drive])]{
        let pidls=paths.iter().map(|path|identity(path)).collect::<Result<Vec<_>>>()?;
        let menu=ShellMenu::for_items(owner,&pidls)?;menu.populate(true,false)?;
        let count=unsafe{GetMenuItemCount(Some(menu.hmenu()))};ensure!(count>0,"{name} has no commands");rows.push(serde_json::json!({"case":name,"commands":count,"invoke":false}));
    }
    let token=format!("namespace-{}",std::process::id());let events=EventSet::create(&token)?;
    for (name,location) in [
        ("folder-background",zsdesk_core::model::explorer::ShellLocation::Path(first.clone())),
        ("drive-root-background",zsdesk_core::model::explorer::ShellLocation::Path(drive.into())),
        ("this-pc-background",zsdesk_core::model::explorer::ShellLocation::ThisPc),
        ("fonts-background",zsdesk_core::model::explorer::ShellLocation::Shell{parsing_name:"shell:Fonts".into(),title:"Fonts".into(),pidl:None}),
        ("recycle-bin-background",zsdesk_core::model::explorer::ShellLocation::Shell{parsing_name:"shell:RecycleBinFolder".into(),title:"Recycle Bin".into(),pidl:None}),
    ]{
        let pidl=zsdesk_windows::shell::pidl::location_pidl(&location).with_context(||format!("{name} PIDL"))?;
        let item=zsdesk_windows::shell::pidl::item_from_pidl(&pidl)?;
        let request=Request{token:token.clone(),session:token.clone(),ordinal:1,parent:std::process::id(),parent_created:created(std::process::id()).unwrap(),owner:owner.0 as isize,
            point:[0,0],pidls:vec![pidl],folder:location.as_path().map(PathBuf::from),options:Default::default(),rename:false,open:false,file_actions:false,mode:Mode::Background,extended:false,layout:None,view_context:None,probe:None};
        let view=background_view::BackgroundView::create(&item,&request,&events).with_context(||format!("{name} view"))?;
        let menu=ShellMenu::for_view(&view.view,false).with_context(||format!("{name} menu"))?;menu.populate(false,false)?;
        let count=unsafe{GetMenuItemCount(Some(menu.hmenu()))};ensure!(count>0,"{name} has no commands");
        let reused=background_view::BackgroundView::create(&item,&request,&events)?;ensure!(background_view::reused(),"{name} was not reused");
        rows.push(serde_json::json!({"case":name,"commands":count,"verified_view_identity":true,"warm_view_reused":true,"invoke":false}));drop(reused);
    }
    for path in [&a,&b,&c]{ensure!(fs::read(path)?==b"readonly context fixture","fixture contents changed");fs::remove_file(path)?;}fs::remove_dir(first)?;fs::remove_dir(second)?;
    Ok(rows)
}
#[test]#[ignore="Private desktop read-only context construction matrix; no menu commands invoked"]
fn native_context_menu_namespace_matrix()->Result<()>{
    let root=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("output")?).join("context-namespace-matrix");fs::create_dir_all(&root)?;
    let name=wide(&format!("ZSDesk.NamespaceMatrix.{}",std::process::id()));
    let desktop=unsafe{CreateDesktopW(name.as_ptr(),std::ptr::null(),std::ptr::null(),0,0x000f00ff,std::ptr::null())};ensure!(!desktop.is_null(),"private desktop unavailable");
    let raw=desktop as usize;let output=root.clone();let result=thread::spawn(move||{ensure!(unsafe{SetThreadDesktop(raw as*mut _)}!=0,"private desktop selection failed");exercise(&output)}).join().map_err(|_|anyhow::anyhow!("namespace matrix thread panicked")).and_then(|value|value);
    let closed=unsafe{CloseDesktop(desktop)}!=0;
    let report=serde_json::json!({"checks":result.as_ref().map_err(|error|format!("{error:#}")),"private_desktop_closed":closed,"commands_invoked":false});
    fs::write(root.join("verification.current.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");result?;ensure!(closed,"desktop cleanup failed");Ok(())
}
