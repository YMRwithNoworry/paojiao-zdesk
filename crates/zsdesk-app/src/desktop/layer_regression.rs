//! Reproduce screenshot/topmost-overlay transitions without using real hotkeys.
use super::*;
use anyhow::{Context,Result,ensure};
use std::{ffi::c_void,thread,fs};
use windows::{core::w,Win32::{Foundation::{HINSTANCE,LRESULT},System::{LibraryLoader::GetModuleHandleW,Ole::{OleInitialize,OleUninitialize}}}};

#[link(name="user32")]unsafe extern "system"{
    fn CreateDesktopW(name:*const u16,device:*const u16,mode:*const c_void,flags:u32,access:u32,attributes:*const c_void)->*mut c_void;
    fn SetThreadDesktop(desktop:*mut c_void)->i32;
    fn CloseDesktop(desktop:*mut c_void)->i32;
}
unsafe extern "system" fn proc(hwnd:HWND,msg:u32,wp:WPARAM,lp:LPARAM)->LRESULT{unsafe{DefWindowProcW(hwnd,msg,wp,lp)}}
struct Windows(Vec<HWND>);impl Drop for Windows{fn drop(&mut self){for hwnd in self.0.iter().rev(){let _=unsafe{DestroyWindow(*hwnd)};}}}
fn show(owner:Option<HWND>,class:PCWSTR,style:WINDOW_EX_STYLE,instance:HINSTANCE,windows:&mut Windows)->Result<HWND>{
    let hwnd=unsafe{CreateWindowExW(style,class,w!("Layer regression"),WS_POPUP,20,20,400,300,owner,None,Some(instance),None)}?;
    windows.0.push(hwnd);let _=unsafe{ShowWindow(hwnd,SW_SHOWNOACTIVATE)};Ok(hwnd)
}
fn exercise(root:&PathBuf)->Result<serde_json::Value>{
    unsafe{OleInitialize(None)?;}
    struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}let _apartment=Apartment;
    let instance=HINSTANCE(unsafe{GetModuleHandleW(None)?}.0);let mut windows=Windows(Vec::new());
    for class in [w!("WorkerW"),w!("ZSDesk.Layer.Normal"),w!("ZSDesk.Layer.Fence"),w!("ZSDesk.Layer.Overlay")] {
        unsafe{RegisterClassW(&WNDCLASSW{lpfnWndProc:Some(proc),hInstance:instance,lpszClassName:class,..Default::default()});}
    }
    let desktop=show(None,w!("WorkerW"),WS_EX_TOOLWINDOW|WS_EX_NOACTIVATE,instance,&mut windows)?;
    let first=show(None,w!("ZSDesk.Layer.Normal"),WS_EX_NOACTIVATE,instance,&mut windows)?;
    let second=show(None,w!("ZSDesk.Layer.Normal"),WS_EX_NOACTIVATE,instance,&mut windows)?;
    let fence=show(Some(desktop),w!("ZSDesk.Layer.Fence"),WS_EX_TOOLWINDOW|WS_EX_NOACTIVATE,instance,&mut windows)?;
    let mut config=DesktopConfig::default();config.settings.organize=false;config.settings.icons_hidden_by_zsdesk=false;config.settings.hide_tray=true;
    let mut host=DesktopHost::new(HWND::default(),root.join("profile/desktop-fences.json"),root.join("profile/explorer-session.json"),config,Gfx::new("Segoe UI")?);
    let mut item=FenceWin::new(1);item.hwnd=fence;host.fences.push(item);
    crate::desktop::set_z(desktop,HWND_BOTTOM);host.raise_above(desktop);host.lower_all(first);
    ensure!(window_precedes(first,fence)&&window_precedes(second,fence),"fence did not remain below both normal applications");
    // A retained desktop anchor can temporarily sit above normal windows while
    // a screenshot/task-switch overlay owns the foreground. It is not a Show
    // Desktop request and must not promote the fences into that gap.
    crate::desktop::set_z(desktop,HWND_TOP);
    let overlay=show(None,w!("ZSDesk.Layer.Overlay"),WS_EX_TOPMOST|WS_EX_TOOLWINDOW|WS_EX_NOACTIVATE,instance,&mut windows)?;
    host.lower_all(overlay);
    ensure!(!host.desktop_exposed&&window_precedes(first,fence)&&window_precedes(second,fence),"topmost overlay promoted background fences");
    ensure!(unsafe{GetWindowLongPtrW(fence,GWL_EXSTYLE)}as u32&WS_EX_TOPMOST.0==0,"fence entered the topmost band");
    let _=unsafe{DestroyWindow(overlay)};
    host.lower_all(HWND::default());host.lower_all(first);
    ensure!(window_precedes(first,fence)&&window_precedes(second,fence),"overlay cancellation left fences covering an application");
    // Explicit stable Show Desktop still works, including the subsequent
    // activation of a fence and a transient topmost capture surface.
    host.raise_above(desktop);ensure!(window_precedes(fence,desktop),"explicit desktop activation hid its fence");
    host.lower_all(fence);ensure!(host.desktop_exposed&&window_precedes(fence,desktop),"fence activation lost exposed desktop layer");
    let overlay=show(None,w!("ZSDesk.Layer.Overlay"),WS_EX_TOPMOST|WS_EX_NOACTIVATE,instance,&mut windows)?;
    host.lower_all(overlay);ensure!(host.desktop_exposed&&window_precedes(overlay,fence)&&window_precedes(fence,desktop),"capture from exposed desktop changed the desktop band");
    host.lower_all(second);ensure!(!host.desktop_exposed&&window_precedes(first,fence)&&window_precedes(second,fence),"switching back to an application retained raised fences");
    // A visible desktop-class window between applications that does not cover
    // them (a leftover Show Desktop surface, a small Shell helper) is not the
    // desktop on screen. Activating it, directly or as a retained anchor when
    // the real desktop is clicked, must not show the fences over `first`.
    let helper=show(None,w!("WorkerW"),WS_EX_TOOLWINDOW|WS_EX_NOACTIVATE,instance,&mut windows)?;
    let _=unsafe{SetWindowPos(helper,None,30,30,120,40,SWP_NOZORDER|SWP_NOACTIVATE)};
    crate::desktop::set_z(desktop,HWND_BOTTOM);crate::desktop::set_z(second,HWND_TOP);crate::desktop::set_z(helper,second);crate::desktop::set_z(first,helper);
    ensure!(window_precedes(second,helper)&&window_precedes(helper,first)&&window_precedes(first,desktop),"helper fixture order");
    host.raise_above(helper);
    ensure!(!host.desktop_exposed&&window_precedes(first,fence)&&window_precedes(second,fence),"a non-covering desktop helper raised fences over an application");
    host.raise_above(desktop);
    ensure!(window_precedes(first,fence)&&window_precedes(second,fence)&&window_precedes(fence,desktop),"a retained non-covering anchor raised fences over an application");
    host.fences.clear();host.loader.shutdown();host.gfx.release_device_resources();
    Ok(serde_json::json!({"topmost_overlay_cannot_raise_background_fences":true,"cancel_without_foreground_is_safe":true,
        "all_normal_apps_remain_above_fences":true,"show_desktop_preserved":true,"capture_on_exposed_desktop_preserved":true,
        "own_app_switch_restores_application_priority":true,"specific_hotkey_not_required":true,"real_desktop_modified":false}))
}

#[test]
#[ignore="Private desktop Z-order regression; requires ZSDESK_NATIVE_TEST_OUTPUT"]
fn native_desktop_layer_survives_overlay_cancel_and_app_switch()->Result<()>{
    let output=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("output")?).join("desktop-layer");
    ensure!(output.is_absolute(),"absolute fixture root required");fs::create_dir_all(&output)?;
    let name:Vec<u16>=format!("ZSDesk.Layer.{}",std::process::id()).encode_utf16().chain(Some(0)).collect();
    let desktop=unsafe{CreateDesktopW(name.as_ptr(),std::ptr::null(),std::ptr::null(),0,0x000f00ff,std::ptr::null())};
    ensure!(!desktop.is_null(),"private desktop creation failed");let raw=desktop as usize;let target=output.clone();
    let result=thread::spawn(move||{ensure!(unsafe{SetThreadDesktop(raw as *mut c_void)}!=0,"private desktop attach failed");exercise(&target)}).join().map_err(|_|anyhow::anyhow!("layer regression panicked")).and_then(|r|r);
    let closed=unsafe{CloseDesktop(desktop)}!=0;
    let record=serde_json::json!({"state":if result.is_ok()&&closed{"passed"}else{"failed"},"checks":result.as_ref().map_err(|e|format!("{e:#}")),"desktop_closed":closed});
    fs::write(output.join("verification.current.json"),serde_json::to_vec_pretty(&record)?)?;println!("{record}");
    result?;ensure!(closed,"private desktop retained");Ok(())
}
