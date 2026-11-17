//! Stop only the legacy Win+E agent belonging to the installation being updated.
//! The legacy stop event is global to the user session, so signalling it could
//! stop a different installation. Verify image and command line instead.
use std::{ffi::c_void, mem::size_of, path::Path};
use windows::{core::{PCWSTR, PWSTR}, Win32::{
    Foundation::{CloseHandle, HANDLE, HLOCAL, LPARAM, LocalFree, WPARAM},
    System::{Diagnostics::ToolHelp::*, Threading::{OpenProcess, QueryFullProcessImageNameW,
        PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION}},
    UI::{Shell::CommandLineToArgvW, WindowsAndMessaging::{PostThreadMessageW, WM_QUIT}},
}};

#[repr(C)]
struct UnicodeString { length: u16, maximum_length: u16, buffer: *const u16 }

#[link(name="ntdll")]
unsafe extern "system" {
    fn NtQueryInformationProcess(process: HANDLE, class: u32, info: *mut c_void,
        size: u32, returned: *mut u32) -> i32;
}

struct Owned(HANDLE);
impl Drop for Owned { fn drop(&mut self) { let _ = unsafe { CloseHandle(self.0) }; } }

fn command_line(process: HANDLE) -> Option<Vec<u16>> {
    // ProcessCommandLineInformation (60) returns a UNICODE_STRING and its
    // contents in the caller's buffer on supported Windows 10/11 systems.
    let mut needed = 0u32;
    unsafe { NtQueryInformationProcess(process, 60, std::ptr::null_mut(), 0, &mut needed); }
    if needed < size_of::<UnicodeString>() as u32 || needed > 131072 { return None; }
    let mut buffer = vec![0usize; (needed as usize).div_ceil(size_of::<usize>())];
    let status = unsafe { NtQueryInformationProcess(process, 60, buffer.as_mut_ptr().cast(),
        needed, &mut needed) };
    if status < 0 { return None; }
    let value = unsafe { &*(buffer.as_ptr().cast::<UnicodeString>()) };
    let base = buffer.as_ptr() as usize;
    let start = value.buffer as usize;
    let end = start.checked_add(value.length as usize)?;
    let limit = base.checked_add(buffer.len() * size_of::<usize>())?;
    if value.length % 2 != 0 || start % 2 != 0 || start < base || end > limit { return None; }
    Some(unsafe { std::slice::from_raw_parts(value.buffer, value.length as usize / 2) }.to_vec())
}

fn is_agent_command_line(line: &[u16]) -> bool {
    if line.contains(&0) { return false; }
    let terminated: Vec<_> = line.iter().copied().chain(Some(0)).collect();
    let mut count = 0;
    let args = unsafe { CommandLineToArgvW(PCWSTR(terminated.as_ptr()), &mut count) };
    if args.is_null() { return false; }
    let result = count == 2 && unsafe { (*args.add(1)).to_string() }
        .is_ok_and(|arg| arg == "--win-e-agent");
    unsafe { let _ = LocalFree(Some(HLOCAL(args.cast()))); }
    result
}

fn same_image(process: HANDLE, target: &Path) -> bool {
    let mut text = vec![0u16; 32768];
    let mut length = text.len() as u32;
    if unsafe { QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32,
        PWSTR(text.as_mut_ptr()), &mut length) }.is_err() { return false; }
    let path = std::path::PathBuf::from(String::from_utf16_lossy(&text[..length as usize]));
    crate::explorer_forwarder::same_path(&path, target)
}

pub(super) fn request_agent_shutdown(target: &Path) {
    let Ok(snapshot) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS | TH32CS_SNAPTHREAD, 0) }) else { return; };
    let snapshot = Owned(snapshot);
    let mut process = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
    if unsafe { Process32FirstW(snapshot.0, &mut process) }.is_err() { return; }
    loop {
        let end = process.szExeFile.iter().position(|c| *c == 0).unwrap_or(process.szExeFile.len());
        let name = String::from_utf16_lossy(&process.szExeFile[..end]);
        if name.eq_ignore_ascii_case("ZSDesk.exe") {
            if let Ok(handle) = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process.th32ProcessID) } {
                let handle = Owned(handle);
                if same_image(handle.0, target) && command_line(handle.0).is_some_and(|line| is_agent_command_line(&line)) {
                    // Only this single-purpose agent receives WM_QUIT. Other
                    // instances and operation hosts are deliberately untouched.
                    let mut thread = THREADENTRY32 { dwSize: size_of::<THREADENTRY32>() as u32, ..Default::default() };
                    if unsafe { Thread32First(snapshot.0, &mut thread) }.is_ok() {
                        loop {
                            if thread.th32OwnerProcessID == process.th32ProcessID {
                                let _ = unsafe { PostThreadMessageW(thread.th32ThreadID, WM_QUIT, WPARAM(0), LPARAM(0)) };
                            }
                            if unsafe { Thread32Next(snapshot.0, &mut thread) }.is_err() { break; }
                        }
                    }
                }
            }
        }
        if unsafe { Process32NextW(snapshot.0, &mut process) }.is_err() { break; }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn line(value: &str) -> Vec<u16> { value.encode_utf16().collect() }

    #[test]
    fn identifies_only_the_standalone_agent_command() {
        assert!(is_agent_command_line(&line(r#""C:\Program Files\ZSDesk\ZSDesk.exe" --win-e-agent"#)));
        for text in [
            r#""C:\ZSDesk.exe" --win-e-agent-stop"#,
            r#""C:\ZSDesk.exe" --win-e-agent --other"#,
            r#""C:\ZSDesk.exe" --desktop"#,
        ] { assert!(!is_agent_command_line(&line(text))); }
    }

    #[test]
    fn reads_a_real_process_command_line() {
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, std::process::id()) }.unwrap();
        let process = Owned(process);
        let line = command_line(process.0).expect("current process command line");
        assert!(!line.is_empty());
        assert!(!is_agent_command_line(&line));
        let current = std::env::current_exe().unwrap();
        assert!(same_image(process.0, &current));
        let other_installation = current.parent().unwrap().join("other-install").join(current.file_name().unwrap());
        assert!(!same_image(process.0, &other_installation));
    }
}
