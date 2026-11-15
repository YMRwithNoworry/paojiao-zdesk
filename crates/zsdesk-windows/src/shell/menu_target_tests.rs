use super::*;
use anyhow::ensure;
use windows::{core::{w,PWSTR},Win32::{Foundation::{WPARAM,LRESULT},Graphics::Gdi::ClientToScreen,System::Ole::{OleInitialize,OleUninitialize}}};

struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}
struct Fixture{owner:HWND,list:HWND,_apartment:Apartment}
impl Drop for Fixture{fn drop(&mut self){let _=unsafe{DestroyWindow(self.owner)};}}
fn fixture()->anyhow::Result<Fixture>{fixture_kind(false)}
fn fixture_kind(owner_data:bool)->anyhow::Result<Fixture>{
    unsafe{OleInitialize(None)}?;let apartment=Apartment;
    unsafe{InitCommonControlsEx(&INITCOMMONCONTROLSEX{dwSize:std::mem::size_of::<INITCOMMONCONTROLSEX>()as u32,dwICC:ICC_LISTVIEW_CLASSES})}.ok()?;
    let owner=unsafe{CreateWindowExW(WS_EX_TOOLWINDOW|WS_EX_NOACTIVATE,w!("STATIC"),w!("owned list-view hit fixture"),WS_POPUP,
        -30000,-30000,460,300,None,None,None,None)}?;
    let mut value=Fixture{owner,list:HWND::default(),_apartment:apartment};
    value.list=unsafe{CreateWindowExW(WINDOW_EX_STYLE::default(),w!("SysListView32"),None,
        WS_CHILD|WS_VISIBLE|WINDOW_STYLE(LVS_REPORT|LVS_SHOWSELALWAYS|if owner_data{LVS_OWNERDATA}else{0}),10,10,420,240,Some(owner),None,None,None)}?;
    unsafe{SendMessageW(value.list,LVM_SETEXTENDEDLISTVIEWSTYLE,Some(WPARAM(LVS_EX_FULLROWSELECT as usize)),Some(LPARAM(LVS_EX_FULLROWSELECT as isize)));}
    for (index,label) in ["Name","Kind"].into_iter().enumerate(){
        let mut text=label.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
        let column=LVCOLUMNW{mask:LVCF_TEXT|LVCF_WIDTH,cx:190,pszText:PWSTR(text.as_mut_ptr()),..Default::default()};
        ensure!(unsafe{SendMessageW(value.list,LVM_INSERTCOLUMNW,Some(WPARAM(index)),Some(LPARAM((&column as*const LVCOLUMNW)as isize)))}.0>=0,"column insertion failed");
    }
    if owner_data{unsafe{SendMessageW(value.list,LVM_SETITEMCOUNT,Some(WPARAM(3)),None);}}
    else{for (index,label) in ["one.txt","two.txt","three.txt"].into_iter().enumerate(){
        let mut text=label.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
        let item=LVITEMW{mask:LVIF_TEXT,iItem:index as i32,pszText:PWSTR(text.as_mut_ptr()),..Default::default()};
        ensure!(unsafe{SendMessageW(value.list,LVM_INSERTITEMW,None,Some(LPARAM((&item as*const LVITEMW)as isize)))}.0==index as isize,"item insertion failed");
        let mut detail="Text document".encode_utf16().chain(Some(0)).collect::<Vec<_>>();
        let subitem=LVITEMW{iSubItem:1,pszText:PWSTR(detail.as_mut_ptr()),..Default::default()};
        ensure!(unsafe{SendMessageW(value.list,LVM_SETITEMTEXTW,Some(WPARAM(index)),Some(LPARAM((&subitem as*const LVITEMW)as isize)))}.0!=0,"subitem insertion failed");
    }}
    unsafe{let _=ShowWindow(owner,SW_SHOWNOACTIVATE);}
    Ok(value)
}
fn packed(window:HWND,mut point:POINT)->anyhow::Result<LPARAM>{
    unsafe{ClientToScreen(window,&mut point)}.ok()?;
    Ok(LPARAM(((point.x as u16 as u32)|((point.y as u16 as u32)<<16))as isize))
}
fn row_point(list:HWND,index:usize,x:i32)->anyhow::Result<LPARAM>{
    let mut bounds=RECT{left:LVIR_BOUNDS as i32,..Default::default()};
    ensure!(unsafe{SendMessageW(list,LVM_GETITEMRECT,Some(WPARAM(index)),Some(LPARAM((&mut bounds as*mut RECT)as isize)))}.0!=0,"item bounds unavailable");
    packed(list,POINT{x,y:(bounds.top+bounds.bottom)/2})
}
fn select(list:HWND,index:usize){
    let item=LVITEMW{state:LVIS_SELECTED,stateMask:LVIS_SELECTED,..Default::default()};
    assert_ne!(unsafe{SendMessageW(list,LVM_SETITEMSTATE,Some(WPARAM(index)),Some(LPARAM((&item as*const LVITEMW)as isize)))}.0,0);
}
fn selected(list:HWND)->Vec<usize>{
    (0..3).filter(|index|unsafe{SendMessageW(list,LVM_GETITEMSTATE,Some(WPARAM(*index)),Some(LPARAM(LVIS_SELECTED.0 as isize)))}.0!=0).collect()
}

#[test]
fn msaa_no_hit_is_not_coerced_to_childid_self(){
    assert_eq!(child_hit_id(&VARIANT::from(0i32)),Some(0));
    assert_eq!(child_hit_id(&VARIANT::from(8i32)),Some(8));
    assert_eq!(child_hit_id(&VARIANT::default()),None);
    assert_eq!(child_hit_id(&VARIANT::from(-1i32)),None);
    assert_eq!(child_hit_id(&VARIANT::from(windows::core::BSTR::from("0"))),None);
}

unsafe extern "system" fn unsupported_ids(window:HWND,message:u32,wp:WPARAM,lp:LPARAM,id:usize,_:usize)->LRESULT{
    if matches!(message,LVM_MAPINDEXTOID|LVM_MAPIDTOINDEX){return LRESULT(0);}
    if message==WM_NCDESTROY{let _=unsafe{windows::Win32::UI::Shell::RemoveWindowSubclass(window,Some(unsupported_ids),id)};}
    unsafe{windows::Win32::UI::Shell::DefSubclassProc(window,message,wp,lp)}
}
#[test]
fn unsupported_stable_ids_and_owner_data_keep_the_exact_msaa_row()->anyhow::Result<()>{
    for owner_data in [false,true]{
        let fixture=fixture_kind(owner_data)?;
        ensure!(unsafe{windows::Win32::UI::Shell::SetWindowSubclass(fixture.list,Some(unsupported_ids),1,0)}.as_bool(),"ID compatibility fixture hook");
        select(fixture.list,0);
        ensure!(prepare(fixture.owner,fixture.owner,row_point(fixture.list,2,220)?)?==ViewMenuTarget::Selection,"fallback item hit missing");
        ensure!(selected(fixture.list)==[2],"unsupported stable IDs retargeted the clicked file");
        select(fixture.list,0);
        ensure!(prepare(fixture.owner,fixture.owner,row_point(fixture.list,0,35)?)?==ViewMenuTarget::Selection,"fallback selected hit missing");
        ensure!(selected(fixture.list)==[0,2],"fallback lost existing multi-selection");
    }
    Ok(())
}

#[test]
fn classic_list_view_context_hit_selects_exact_row_and_preserves_selected_group()->anyhow::Result<()>{
    let fixture=fixture()?;select(fixture.list,0);select(fixture.list,1);
    ensure!(prepare(fixture.owner,fixture.owner,row_point(fixture.list,0,35)?)?==ViewMenuTarget::Selection,"name hit was not an item");
    ensure!(selected(fixture.list)==[0,1],"right click on a selected row lost multiple selection");
    ensure!(prepare(fixture.owner,fixture.owner,row_point(fixture.list,2,35)?)?==ViewMenuTarget::Selection,"new item was not hit");
    ensure!(selected(fixture.list)==[2],"clicked file did not replace an unrelated selection");
    ensure!(prepare(fixture.owner,fixture.owner,row_point(fixture.list,0,220)?)?==ViewMenuTarget::Selection,"report subitem was not part of its file row");
    ensure!(selected(fixture.list)==[0],"subitem hit selected the wrong row");
    ensure!(prepare(fixture.owner,fixture.list,LPARAM(-1))?==ViewMenuTarget::CurrentSelection,"keyboard menu did not preserve its selection contract");
    ensure!(selected(fixture.list)==[0],"keyboard menu changed the row");
    Ok(())
}

#[test]
fn classic_list_view_background_header_and_edit_controls_are_distinct()->anyhow::Result<()>{
    let fixture=fixture()?;select(fixture.list,1);
    let background=packed(fixture.list,POINT{x:400,y:220})?;
    ensure!(prepare(fixture.owner,fixture.owner,background)?==ViewMenuTarget::Background,"empty client area was not background");
    ensure!(selected(fixture.list)==[1],"background classification changed the item selection");
    let header=HWND(unsafe{SendMessageW(fixture.list,LVM_GETHEADER,None,None)}.0 as*mut _);
    ensure!(!header.0.is_null(),"header missing");
    ensure!(classify(fixture.owner,fixture.owner,packed(header,POINT{x:30,y:5})?)==ViewMenuTarget::NativeControl,"header was treated as blank file space");
    let edit=unsafe{CreateWindowExW(WINDOW_EX_STYLE::default(),w!("EDIT"),w!("rename field"),WS_CHILD|WS_VISIBLE,270,100,100,25,Some(fixture.list),None,None,None)}?;
    ensure!(classify(fixture.owner,fixture.owner,packed(edit,POINT{x:10,y:10})?)==ViewMenuTarget::NativeControl,"filename edit was intercepted");
    let outside=packed(fixture.owner,POINT{x:-5,y:-5})?;
    ensure!(classify(fixture.owner,fixture.owner,outside)==ViewMenuTarget::NativeControl,"non-client/outside point became background");
    let mut pid=0;unsafe{GetWindowThreadProcessId(GetForegroundWindow(),Some(&mut pid));}
    ensure!(pid!=std::process::id(),"owned offscreen controls took foreground focus");
    Ok(())
}
