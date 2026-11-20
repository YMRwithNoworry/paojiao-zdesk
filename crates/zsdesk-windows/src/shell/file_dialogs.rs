//! User-initiated file dialogs belong to the window containing the selection.
use super::menu::MenuOutcome;
use std::{
    cell::RefCell,
    path::{Path, PathBuf},
};
use windows::{
    ApplicationModel::DataTransfer::{DataPackage, DataRequestedEventArgs, DataTransferManager},
    Foundation::TypedEventHandler,
    Storage::{IStorageItem, StorageFile},
    Win32::{
        Foundation::{E_INVALIDARG, HWND},
        System::WinRT::{
            RO_INIT_MULTITHREADED, RO_INIT_SINGLETHREADED, RoInitialize, RoUninitialize,
        },
        UI::{
            Shell::{IDataTransferManagerInterop, OAIF_EXEC, OPENASINFO, SHOpenWithDialog},
            WindowsAndMessaging::{GA_ROOT, GetAncestor},
        },
    },
    core::{Error, HSTRING, Interface, PCWSTR, Result},
};
use windows_collections::IIterable;

pub fn show(owner: HWND, action: MenuOutcome, pidls: &[Vec<u8>]) -> Result<()> {
    let paths = super::file_transfer::paths_from_pidls(pidls)?;
    let owner = unsafe { GetAncestor(owner, GA_ROOT) };
    if owner.is_invalid() {
        return Err(Error::from_hresult(
            windows::Win32::Foundation::ERROR_INVALID_WINDOW_HANDLE.to_hresult(),
        ));
    }
    match (action, paths.as_slice()) {
        (MenuOutcome::ChooseApplication, [file]) => choose_application(owner, file),
        (MenuOutcome::Share, [_, ..]) => share(owner, paths),
        _ => Err(Error::from_hresult(E_INVALIDARG)),
    }
}

pub(super) fn choose_application(owner: HWND, file: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    let name: Vec<_> = file.as_os_str().encode_wide().chain(Some(0)).collect();
    let info = OPENASINFO {
        pcszFile: PCWSTR(name.as_ptr()),
        pcszClass: PCWSTR::null(),
        oaifInFlags: OAIF_EXEC,
    };
    unsafe { SHOpenWithDialog(Some(owner), &info) }
}

struct ShareRequest {
    owner: HWND,
    manager: DataTransferManager,
    token: i64,
    _apartment: WindowApartment,
}
struct WindowApartment;
impl Drop for WindowApartment {
    fn drop(&mut self) {
        unsafe {
            RoUninitialize();
        }
    }
}
impl Drop for ShareRequest {
    fn drop(&mut self) {
        let _ = self.manager.RemoveDataRequested(self.token);
    }
}
thread_local! {static REQUEST:RefCell<Option<ShareRequest>>=const{RefCell::new(None)};}

pub fn release(owner: HWND) {
    let old = REQUEST.with(|slot| slot.borrow_mut().take_if(|request| request.owner == owner));
    drop(old);
}

// Resolve cloud or network-backed StorageFiles away from the window STA.
fn set_files(data: &DataPackage, paths: &[PathBuf]) -> Result<()> {
    let mut files = Vec::with_capacity(paths.len());
    for path in paths {
        let file = StorageFile::GetFileFromPathAsync(&HSTRING::from(path.as_os_str()))?.join()?;
        files.push(Some(file.cast::<IStorageItem>()?));
    }
    let items: IIterable<IStorageItem> = files.into();
    data.SetStorageItemsReadOnly(&items)
}

fn share(owner: HWND, paths: Vec<PathBuf>) -> Result<()> {
    let factory = register_share(owner, paths)?;
    if let Err(error) = unsafe { factory.ShowShareUIForWindow(owner) } {
        release(owner);
        return Err(error);
    }
    Ok(())
}

fn register_share(owner: HWND, paths: Vec<PathBuf>) -> Result<IDataTransferManagerInterop> {
    unsafe { RoInitialize(RO_INIT_SINGLETHREADED) }?;
    let apartment = WindowApartment;
    let factory = windows::core::factory::<DataTransferManager, IDataTransferManagerInterop>()?;
    let manager: DataTransferManager = unsafe { factory.GetForWindow(owner)? };
    let title = paths[0]
        .file_name()
        .map(HSTRING::from)
        .unwrap_or_else(|| HSTRING::from("ZSDesk"));
    let paths = std::sync::Arc::new(paths);
    let handler =
        TypedEventHandler::<DataTransferManager, DataRequestedEventArgs>::new(move |_, args| {
            let request = args.ok()?.Request()?;
            request.Data()?.Properties()?.SetTitle(&title)?;
            let deferral = request.GetDeferral()?;
            let paths = paths.clone();
            let work_request = request.clone();
            let work_deferral = deferral.clone();
            let started = std::thread::Builder::new()
                .name("zsdesk-share-files".into())
                .spawn(move || {
                    let apartment = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
                    let initialized = apartment.is_ok();
                    let result = apartment.and_then(|_| set_files(&work_request.Data()?, &paths));
                    if let Err(error) = result {
                        let _ = work_request.FailWithDisplayText(&HSTRING::from(error.message()));
                    }
                    let _ = work_deferral.Complete();
                    drop((work_request, work_deferral));
                    if initialized {
                        unsafe {
                            RoUninitialize();
                        }
                    }
                });
            if let Err(error) = started {
                let _ = request.FailWithDisplayText(&HSTRING::from(error.to_string()));
                deferral.Complete()?;
            }
            Ok(())
        });
    let previous = REQUEST.with(|slot| slot.borrow_mut().take());
    drop(previous);
    let token = manager.DataRequested(&handler)?;
    REQUEST.with(|slot| {
        slot.replace(Some(ShareRequest {
            owner,
            manager,
            token,
            _apartment: apartment,
        }))
    });
    Ok(factory)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, WS_OVERLAPPEDWINDOW,
    };

    #[test]
    fn share_binds_to_source_window_and_offers_the_selected_files() -> anyhow::Result<()> {
        unsafe { windows::Win32::System::Ole::OleInitialize(None)? };
        struct Apartment;
        impl Drop for Apartment {
            fn drop(&mut self) {
                unsafe { windows::Win32::System::Ole::OleUninitialize() }
            }
        }
        let _apartment = Apartment;
        struct Owner(HWND);
        impl Drop for Owner {
            fn drop(&mut self) {
                let _ = unsafe { DestroyWindow(self.0) };
            }
        }
        let owner = Owner(unsafe {
            CreateWindowExW(
                Default::default(),
                windows::core::w!("STATIC"),
                PCWSTR::null(),
                WS_OVERLAPPEDWINDOW,
                0,
                0,
                0,
                0,
                None,
                None,
                None,
                None,
            )
        }?);
        let path = std::env::temp_dir().join(format!("共享文件 {}.txt", std::process::id()));
        std::fs::File::create_new(&path)?;
        struct File(PathBuf);
        impl Drop for File {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let _file = File(path.clone());
        let factory = register_share(owner.0, vec![path.clone()])?;
        let manager: DataTransferManager = unsafe { factory.GetForWindow(owner.0)? };
        assert!(REQUEST.with(|slot| slot.borrow().as_ref().is_some_and(|r| r.manager == manager)));
        drop((manager, factory));
        release(owner.0);
        assert!(REQUEST.with(|slot| slot.borrow().is_none()));
        std::thread::spawn(move || -> Result<()> {
            unsafe { RoInitialize(RO_INIT_MULTITHREADED)? };
            let _apartment = WindowApartment;
            let data = DataPackage::new()?;
            set_files(&data, std::slice::from_ref(&path))?;
            let offered = data.GetView()?.GetStorageItemsAsync()?.join()?;
            assert_eq!(offered.Size()?, 1);
            assert_eq!(
                PathBuf::from(offered.GetAt(0)?.Path()?.to_os_string()),
                path
            );
            Ok(())
        })
        .join()
        .expect("file sharing thread")?;
        Ok(())
    }
}
