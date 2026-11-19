//! Generic OLE drop target forwarding to an application handler, with the
//! system drag image (`IDropTargetHelper`) so drops look like File Explorer.

use std::{path::PathBuf, rc::Rc};

use windows::{
    core::{implement, Ref, Result},
    Win32::{
        Foundation::{HWND, POINT, POINTL},
        System::{
            Com::{CoCreateInstance, IDataObject, CLSCTX_INPROC_SERVER, DVASPECT_CONTENT, FORMATETC, STGMEDIUM, TYMED_HGLOBAL},
            Ole::{IDropTarget, IDropTarget_Impl, RegisterDragDrop, ReleaseStgMedium, RevokeDragDrop, CF_HDROP, DROPEFFECT, DROPEFFECT_NONE},
            SystemServices::MODIFIERKEYS_FLAGS,
        },
        UI::Shell::{CLSID_DragDropHelper, IDropTargetHelper},
    },
};

/// Receives drag-and-drop events in screen coordinates.
pub trait DropHandler {
    fn enter(&self, data: &IDataObject, keys: u32, point: POINT, allowed: DROPEFFECT) -> DROPEFFECT;
    fn over(&self, keys: u32, point: POINT, allowed: DROPEFFECT) -> DROPEFFECT;
    fn leave(&self);
    fn dropped(&self, data: &IDataObject, keys: u32, point: POINT, allowed: DROPEFFECT) -> DROPEFFECT;
}

#[implement(IDropTarget)]
struct Target {
    hwnd: isize,
    handler: Rc<dyn DropHandler>,
    helper: Option<IDropTargetHelper>,
}

pub struct DropRegistration { hwnd: HWND, _target: IDropTarget }

impl DropRegistration {
    /// Registers `handler` for `hwnd`.
    ///
    /// # Safety
    ///
    /// OLE must be initialized on this thread; `hwnd` must stay valid until the
    /// registration is dropped.
    pub unsafe fn register(hwnd: HWND, handler: Rc<dyn DropHandler>) -> Result<Self> {
        let helper = unsafe { CoCreateInstance::<_, IDropTargetHelper>(&CLSID_DragDropHelper, None, CLSCTX_INPROC_SERVER) }.ok();
        let target: IDropTarget = Target { hwnd: hwnd.0 as isize, handler, helper }.into();
        unsafe { RegisterDragDrop(hwnd, &target)? };
        Ok(Self { hwnd, _target: target })
    }
}

impl Drop for DropRegistration {
    fn drop(&mut self) { let _ = unsafe { RevokeDragDrop(self.hwnd) }; }
}

fn point(pt: &POINTL) -> POINT { POINT { x: pt.x, y: pt.y } }

#[allow(non_snake_case)]
impl IDropTarget_Impl for Target_Impl {
    fn DragEnter(&self, data: Ref<IDataObject>, keys: MODIFIERKEYS_FLAGS, pt: &POINTL, effect: *mut DROPEFFECT) -> Result<()> {
        let allowed = unsafe { *effect };
        let result = match data.as_ref() { Some(data) => self.handler.enter(data, keys.0, point(pt), allowed), None => DROPEFFECT_NONE };
        unsafe { *effect = result; }
        if let (Some(helper), Some(data)) = (&self.helper, data.as_ref()) {
            let mut p = point(pt);
            let _ = unsafe { helper.DragEnter(HWND(self.hwnd as *mut _), data, &mut p, result) };
        }
        Ok(())
    }
    fn DragOver(&self, keys: MODIFIERKEYS_FLAGS, pt: &POINTL, effect: *mut DROPEFFECT) -> Result<()> {
        let allowed = unsafe { *effect };
        let result = self.handler.over(keys.0, point(pt), allowed);
        unsafe { *effect = result; }
        if let Some(helper) = &self.helper { let mut p = point(pt); let _ = unsafe { helper.DragOver(&mut p, result) }; }
        Ok(())
    }
    fn DragLeave(&self) -> Result<()> {
        self.handler.leave();
        if let Some(helper) = &self.helper { let _ = unsafe { helper.DragLeave() }; }
        Ok(())
    }
    fn Drop(&self, data: Ref<IDataObject>, keys: MODIFIERKEYS_FLAGS, pt: &POINTL, effect: *mut DROPEFFECT) -> Result<()> {
        let allowed = unsafe { *effect };
        if let (Some(helper), Some(data)) = (&self.helper, data.as_ref()) {
            let mut p = point(pt);
            let _ = unsafe { helper.Drop(data, &mut p, allowed) };
        }
        let result = match data.as_ref() { Some(data) => self.handler.dropped(data, keys.0, point(pt), allowed), None => DROPEFFECT_NONE };
        unsafe { *effect = result; }
        Ok(())
    }
}

pub fn has_hdrop(data: &IDataObject) -> bool { unsafe { data.QueryGetData(&hdrop_format()) }.is_ok() }

/// File-system paths carried by a data object (`CF_HDROP`).
pub fn hdrop_paths(data: &IDataObject) -> Vec<PathBuf> {
    let Ok(mut medium) = (unsafe { data.GetData(&hdrop_format()) }) else { return Vec::new() };
    use windows::Win32::System::Memory::{GlobalLock,GlobalSize,GlobalUnlock};
    // Reject an invalid or oversize list as a whole. Truncating it before
    // reporting a successful move could make a source discard unhandled files.
    let paths=if medium.tymed==TYMED_HGLOBAL.0 as u32{
        let memory=unsafe{medium.u.hGlobal};let size=unsafe{GlobalSize(memory)};
        let pointer=unsafe{GlobalLock(memory)};
        if pointer.is_null(){Vec::new()}else{
            let value=if size<=isize::MAX as usize{
                crate::shell::file_transfer::parse_hdrop(unsafe{std::slice::from_raw_parts(pointer.cast::<u8>(),size)}).unwrap_or_default()
            }else{Vec::new()};
            let _=unsafe{GlobalUnlock(memory)};value
        }
    }else{Vec::new()};
    unsafe{ReleaseStgMedium(&mut medium as *mut STGMEDIUM)};
    paths
}

fn hdrop_format() -> FORMATETC {
    FORMATETC { cfFormat: CF_HDROP.0, ptd: std::ptr::null_mut(), dwAspect: DVASPECT_CONTENT.0, lindex: -1, tymed: TYMED_HGLOBAL.0 as u32 }
}
