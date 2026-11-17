use windows::{core::{Interface,IUnknown,Error,HRESULT,Result},Win32::{Foundation::{HWND,LPARAM,POINT,RECT},Graphics::Gdi::ScreenToClient,
    System::{Com::{IDispatch,CoCreateInstance,CLSCTX_INPROC_SERVER},Variant::{VARIANT,VT_I4}},UI::{Accessibility::*,Controls::*,WindowsAndMessaging::*}}};

#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub enum ViewMenuTarget{CurrentSelection,Selection,Background,NativeControl,Unresolved}

fn role_target(role:u32)->Option<ViewMenuTarget>{
    match role{
        ROLE_SYSTEM_COLUMNHEADER|ROLE_SYSTEM_ROWHEADER|ROLE_SYSTEM_GROUPING=>Some(ViewMenuTarget::NativeControl),
        ROLE_SYSTEM_LISTITEM|ROLE_SYSTEM_OUTLINEITEM|ROLE_SYSTEM_ROW=>Some(ViewMenuTarget::Selection),
        ROLE_SYSTEM_LIST|ROLE_SYSTEM_OUTLINE=>Some(ViewMenuTarget::Background),
        _=>None,
    }
}

/// Query the source control, so occlusion and other windows cannot change the
/// target. MSAA identifies file rows separately from headers and empty space.
/// https://learn.microsoft.com/windows/win32/api/oleacc/nf-oleacc-accessibleobjectfromwindow
struct Hit{target:ViewMenuTarget,object:Option<IAccessible>,child:VARIANT,list_item:Option<(HWND,i32)>,automation:Option<IUIAutomationElement>}
impl Hit{fn plain(target:ViewMenuTarget)->Self{Self{target,object:None,child:VARIANT::from(0i32),list_item:None,automation:None}}}
fn class(window:HWND)->String{let mut name=[0u16;96];let count=unsafe{GetClassNameW(window,&mut name)}.max(0)as usize;String::from_utf16_lossy(&name[..count])}
fn child_hit_id(value:&VARIANT)->Option<i32>{
    (value.vt()==VT_I4).then(||i32::try_from(value).ok()).flatten().filter(|id|*id>=0)
}
fn list_hit(window:HWND,point:POINT)->Option<Hit>{
    if class(window)!="SysListView32"{return None;}
    let mut point=point;if !unsafe{ScreenToClient(window,&mut point)}.as_bool(){return None;}
    let mut client=RECT::default();if unsafe{GetClientRect(window,&mut client)}.is_err(){return None;}
    if point.x<client.left||point.x>=client.right||point.y<client.top||point.y>=client.bottom{return Some(Hit::plain(ViewMenuTarget::NativeControl));}
    let mut info=LVHITTESTINFO{pt:point,iItem:-1,iSubItem:-1,iGroup:-1,..Default::default()};
    let report=unsafe{GetWindowLongPtrW(window,GWL_STYLE)}as u32&LVS_TYPEMASK==LVS_REPORT;
    let message=if report{LVM_SUBITEMHITTEST}else{LVM_HITTEST};
    // Request group metadata as well, so a group heading never masquerades as
    // blank file space. Report-view subitems are part of the same file row.
    let result=unsafe{SendMessageW(window,message,Some(windows::Win32::Foundation::WPARAM(usize::MAX)),Some(LPARAM((&mut info as*mut LVHITTESTINFO)as isize)))}.0;
    let index=if result>=0{info.iItem}else{-1};
    if info.flags.0&LVHT_EX_GROUP.0!=0{return Some(Hit::plain(ViewMenuTarget::NativeControl));}
    let mut hit=Hit::plain(if index>=0{ViewMenuTarget::Selection}else{ViewMenuTarget::Background});
    if index>=0{hit.list_item=Some((window,index));}Some(hit)
}
fn automation_hit(automation:&IUIAutomation,object:&IAccessible,child:&VARIANT)->Option<Hit>{
    let element=unsafe{automation.ElementFromIAccessible(object,i32::try_from(child).ok()?)}.ok()?;
    let kind=unsafe{element.CurrentControlType()}.ok()?;
    let target=match kind{
        UIA_ListItemControlTypeId|UIA_DataItemControlTypeId|UIA_TreeItemControlTypeId=>ViewMenuTarget::Selection,
        UIA_ListControlTypeId|UIA_DataGridControlTypeId|UIA_TreeControlTypeId=>ViewMenuTarget::Background,
        UIA_HeaderControlTypeId|UIA_HeaderItemControlTypeId|UIA_EditControlTypeId|UIA_ScrollBarControlTypeId=>ViewMenuTarget::NativeControl,
        _=>return None,
    };
    let mut hit=Hit::plain(target);hit.automation=Some(element);Some(hit)
}
fn accessible_selection(object:&IAccessible,child:&VARIANT)->Result<()>{
    let state=unsafe{object.get_accState(child)}.and_then(|value|u32::try_from(&value))?;
    if state&STATE_SYSTEM_SELECTED==0{
        // The v5 ListView provider abandons the entire combined request when
        // foreground focus is unavailable (S_FALSE, with no selection change).
        // A context-menu target only needs selection; verify it independently.
        unsafe{object.accSelect(SELFLAG_TAKESELECTION as i32,child)}?;
        let selected=unsafe{object.get_accState(child)}.and_then(|value|u32::try_from(&value))?;
        if selected&STATE_SYSTEM_SELECTED==0{return Err(Error::new(HRESULT(0x800704c7u32 as i32),"点击的文件尚未选中，请重试"));}
    }
    Ok(())
}
fn list_identity(window:HWND,index:i32)->Option<u32>{
    use windows::Win32::Foundation::WPARAM;
    // Owner-data lists and comctl32 v5 do not implement these v6 messages.
    // A zero default-window-procedure answer must never retarget row N to 0.
    if unsafe{GetWindowLongPtrW(window,GWL_STYLE)}as u32&LVS_OWNERDATA!=0{return None;}
    let id=unsafe{SendMessageW(window,LVM_MAPINDEXTOID,Some(WPARAM(index as usize)),None)}.0 as u32;
    if id==u32::MAX||unsafe{SendMessageW(window,LVM_MAPIDTOINDEX,Some(WPARAM(id as usize)),None)}.0!=index as isize{return None;}
    if unsafe{SendMessageW(window,LVM_GETITEMCOUNT,None,None)}.0>1{
        let other=if index==0{1}else{0};
        if unsafe{SendMessageW(window,LVM_MAPINDEXTOID,Some(WPARAM(other)),None)}.0 as u32==id{return None;}
    }
    Some(id)
}
fn select_list_accessible(window:HWND,index:i32)->Result<()>{
    let child=index.checked_add(1).filter(|value|*value>0).ok_or_else(||Error::from_hresult(HRESULT(0x80070057u32 as i32)))?;
    let mut raw=std::ptr::null_mut();unsafe{AccessibleObjectFromWindow(window,OBJID_CLIENT.0 as u32,&IAccessible::IID,&mut raw)}?;
    if raw.is_null(){return Err(Error::from_hresult(HRESULT(0x80004003u32 as i32)));}
    let accessible=unsafe{IAccessible::from_raw(raw)};let child=VARIANT::from(child);
    let role=unsafe{accessible.get_accRole(&child)}.and_then(|value|u32::try_from(&value))?;
    if role_target(role)!=Some(ViewMenuTarget::Selection){return Err(Error::from_hresult(HRESULT(0x800704c7u32 as i32)));}
    accessible_selection(&accessible,&child)
}
fn locate(view:HWND,_source:HWND,position:LPARAM)->Hit{
    if position.0==-1{return Hit::plain(ViewMenuTarget::CurrentSelection);}
    let point=POINT{x:position.0 as i16 as i32,y:(position.0>>16)as i16 as i32};
    let mut rect=RECT::default();
    if unsafe{GetWindowRect(view,&mut rect)}.is_err()||point.x<rect.left||point.x>=rect.right||point.y<rect.top||point.y>=rect.bottom{
        return Hit::plain(ViewMenuTarget::NativeControl);
    }
    // WM_CONTEXTMENU is often forwarded with DefView itself as its source.
    // Its accessibility object can report the whole list for a point on a file.
    // Resolve the innermost native child first, without global/occluded hits.
    let mut source=view;
    for _ in 0..32{
        let mut client=point;if !unsafe{ScreenToClient(source,&mut client)}.as_bool(){break;}
        let child=unsafe{ChildWindowFromPointEx(source,client,CWP_SKIPINVISIBLE|CWP_SKIPDISABLED)};
        if child.0.is_null()||child==source||!unsafe{IsChild(view,child)}.as_bool(){break;}source=child;
    }
    if matches!(class(source).as_str(),"SysHeader32"|"Edit"|"RICHEDIT50W"|"RichEdit20W"|"ComboBox"){return Hit::plain(ViewMenuTarget::NativeControl);}
    if let Some(hit)=list_hit(source,point){return hit;}
    let mut raw=std::ptr::null_mut();
    if unsafe{AccessibleObjectFromWindow(source,OBJID_CLIENT.0 as u32,&IAccessible::IID,&mut raw)}.is_err()||raw.is_null(){return Hit::plain(ViewMenuTarget::Unresolved);}
    let mut object=unsafe{IAccessible::from_raw(raw)};
    let mut child=VARIANT::from(0i32);
    for _ in 0..8{
        let Ok(hit)=(unsafe{object.accHitTest(point.x,point.y)})else{return Hit::plain(ViewMenuTarget::Unresolved);};
        if let Ok(next)=IDispatch::try_from(&hit).and_then(|value|value.cast::<IAccessible>()){
            let unchanged=object.cast::<IUnknown>().ok()==next.cast::<IUnknown>().ok();
            if unchanged{break;}object=next;child=VARIANT::from(0i32);
        }else{
            // VT_EMPTY is a successful "no hit" answer, not CHILDID_SELF.
            // Falling through to the container's role would turn a failed hit
            // into an unrelated background command or parent navigation.
            if child_hit_id(&hit).is_none(){return Hit::plain(ViewMenuTarget::Unresolved);}
            child=hit;break;
        }
    }
    let mut automation:Option<IUIAutomation>=None;let mut automation_attempted=false;
    for _ in 0..8{
        if let Ok(role)=unsafe{object.get_accRole(&child)}.and_then(|value|u32::try_from(&value)){
            if let Some(target)=role_target(role){return Hit{target,object:Some(object),child,list_item:None,automation:None};}
        }
        if !automation_attempted{
            automation_attempted=true;automation=unsafe{CoCreateInstance(&CUIAutomation,None,CLSCTX_INPROC_SERVER)}.ok();
        }
        if let Some(hit)=automation.as_ref().and_then(|automation|automation_hit(automation,&object,&child)){return hit;}
        // An editable filename/metadata cell belongs to its item; a native
        // Edit HWND was already excluded by the view subclass.
        let Ok(parent)=unsafe{object.accParent()}.and_then(|value|value.cast::<IAccessible>())else{break;};
        if object.cast::<IUnknown>().ok()==parent.cast::<IUnknown>().ok(){break;}
        object=parent;child=VARIANT::from(0i32);
    }
    // Do not substitute an unrelated previous selection, or load a second
    // synchronous Shell menu, when accessibility cannot identify the target.
    Hit::plain(ViewMenuTarget::Unresolved)
}

pub(super) fn classify(view:HWND,source:HWND,position:LPARAM)->ViewMenuTarget{locate(view,source,position).target}

/// Mouse menus bind to the clicked item. Preserve an existing multi-selection
/// only when that item already belongs to it; keyboard menus use the selection.
pub(super) fn prepare(view:HWND,source:HWND,position:LPARAM)->Result<ViewMenuTarget>{
    let hit=locate(view,source,position);
    if hit.target!=ViewMenuTarget::Selection{return Ok(hit.target);}
    if let Some((window,index))=hit.list_item{
        let Some(identity)=list_identity(window,index)else{select_list_accessible(window,index)?;return Ok(hit.target);};
        let selected=unsafe{SendMessageW(window,LVM_GETITEMSTATE,Some(windows::Win32::Foundation::WPARAM(index as usize)),Some(LPARAM(LVIS_SELECTED.0 as isize)))}.0!=0;
        let mut current=index;
        if !selected{
            let mut item=LVITEMW{stateMask:LIST_VIEW_ITEM_STATE_FLAGS(LVIS_SELECTED.0|LVIS_FOCUSED.0),..Default::default()};
            unsafe{SendMessageW(window,LVM_SETITEMSTATE,Some(windows::Win32::Foundation::WPARAM(usize::MAX)),Some(LPARAM((&mut item as*mut LVITEMW)as isize)));}
            current=unsafe{SendMessageW(window,LVM_MAPIDTOINDEX,Some(windows::Win32::Foundation::WPARAM(identity as usize)),None)}.0 as i32;
            if current<0{return Err(Error::from_hresult(HRESULT(0x800704c7u32 as i32)));}
            item.state=LIST_VIEW_ITEM_STATE_FLAGS(LVIS_SELECTED.0|LVIS_FOCUSED.0);
            unsafe{SendMessageW(window,LVM_SETITEMSTATE,Some(windows::Win32::Foundation::WPARAM(current as usize)),Some(LPARAM((&mut item as*mut LVITEMW)as isize)));}
        }
        let same=unsafe{SendMessageW(window,LVM_MAPINDEXTOID,Some(windows::Win32::Foundation::WPARAM(current as usize)),None)}.0 as u32==identity;
        let selected=same&&unsafe{SendMessageW(window,LVM_GETITEMSTATE,Some(windows::Win32::Foundation::WPARAM(current as usize)),Some(LPARAM(LVIS_SELECTED.0 as isize)))}.0!=0;
        return if selected{Ok(hit.target)}else{Err(Error::from_hresult(HRESULT(0x800704c7u32 as i32)))};
    }
    if let Some(element)=hit.automation{
        let selection:IUIAutomationSelectionItemPattern=unsafe{element.GetCurrentPatternAs(UIA_SelectionItemPatternId)}?;
        if !unsafe{selection.CurrentIsSelected()}?.as_bool(){unsafe{selection.Select()}?;}
        return if unsafe{selection.CurrentIsSelected()}?.as_bool(){Ok(hit.target)}else{Err(Error::from_hresult(HRESULT(0x800704c7u32 as i32)))};
    }
    let object=hit.object.ok_or_else(||Error::from_hresult(HRESULT(0x80004005u32 as i32)))?;
    accessible_selection(&object,&hit.child)?;
    Ok(hit.target)
}

#[cfg(test)]#[path="menu_target_tests.rs"]mod tests;
