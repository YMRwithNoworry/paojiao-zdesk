//! A bounded, breakaway WPS renderer keeps document cleanup alive after closing a preview.
use super::{RenderedDocument,render_owned};
use std::{cell::Cell,fs,io::Write,path::{Path,PathBuf},process::{Command,Stdio},time::{Duration,Instant,SystemTime,UNIX_EPOCH}};
use std::os::windows::{fs::OpenOptionsExt,process::CommandExt};
use anyhow::{bail,Context,Result};
use serde::{Deserialize,Serialize};
use windows::{core::{w,PCWSTR},Win32::{Foundation::{CloseHandle,GetLastError,HANDLE,HWND,ERROR_ALREADY_EXISTS},
    Storage::FileSystem::{FILE_FLAG_DELETE_ON_CLOSE,MoveFileExW,MOVEFILE_REPLACE_EXISTING},
    System::{Ole::{OleInitialize,OleUninitialize},Threading::{CreateMutexW,ReleaseMutex,CREATE_BREAKAWAY_FROM_JOB,CREATE_NO_WINDOW}},
    UI::WindowsAndMessaging::{GetWindowThreadProcessId,IsWindow}}};

const LIMIT:Duration=Duration::from_secs(30);
thread_local!{static ACTIVE:Cell<Option<u128>>=const{Cell::new(None)};}
#[derive(Serialize,Deserialize)]
struct Request{id:u128,source:PathBuf,preview_generation:u64}
#[derive(Serialize,Deserialize)]
struct Outcome{id:u128,pdf:Option<PathBuf>,error:Option<String>}

fn request_path(workspace:&Path,id:u128)->PathBuf{workspace.join(format!("wps-request-{id}.json"))}
fn result_path(workspace:&Path,id:u128)->PathBuf{workspace.join(format!("wps-result-{id}.json"))}
fn cancel_path(workspace:&Path,id:u128)->PathBuf{workspace.join(format!("wps-cancel-{id}"))}
fn read<T:serde::de::DeserializeOwned>(path:&Path)->Result<T>{
    if fs::metadata(path)?.len()>65536{bail!("文档预览状态过长");}
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
fn write(path:&Path,value:&impl Serialize)->Result<()>{
    use std::os::windows::ffi::OsStrExt;
    let temporary=path.with_extension("tmp");fs::write(&temporary,serde_json::to_vec(value)?)?;
    let from:Vec<_>=temporary.as_os_str().encode_wide().chain(Some(0)).collect();
    let to:Vec<_>=path.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe{MoveFileExW(PCWSTR(from.as_ptr()),PCWSTR(to.as_ptr()),MOVEFILE_REPLACE_EXISTING)}?;Ok(())
}
fn cancelled_id(workspace:&Path,id:u128)->bool{
    if cancel_path(workspace,id).exists(){return true;}
    let Ok(request)=read::<Request>(&request_path(workspace,id))else{return true;};
    let Ok(preview)=read::<serde_json::Value>(&workspace.join("request.json"))else{return true;};
    if preview["close"].as_bool()!=Some(false)||preview["generation"].as_u64()!=Some(request.preview_generation)
        ||preview["path"].as_str().is_none_or(|path|Path::new(path)!=request.source){return true;}
    let Some(owner)=preview["owner"].as_i64()else{return true;};
    let Some(parent)=preview["parent"].as_u64()else{return true;};
    let hwnd=HWND(owner as*mut _);let mut pid=0;
    unsafe{GetWindowThreadProcessId(hwnd,Some(&mut pid));}
    pid as u64!=parent||!unsafe{IsWindow(Some(hwnd))}.as_bool()
}
fn disposed(workspace:&Path)->bool{
    let Ok(preview)=read::<serde_json::Value>(&workspace.join("request.json"))else{return true;};
    if preview["close"].as_bool()==Some(true){return true;}
    let (Some(owner),Some(parent))=(preview["owner"].as_i64(),preview["parent"].as_u64())else{return true;};
    let hwnd=HWND(owner as*mut _);let mut pid=0;unsafe{GetWindowThreadProcessId(hwnd,Some(&mut pid));}
    pid as u64!=parent||!unsafe{IsWindow(Some(hwnd))}.as_bool()
}
pub(super) fn cancelled(workspace:&Path)->bool{ACTIVE.with(|active|active.get().is_some_and(|id|cancelled_id(workspace,id)))}
pub(super) fn guarded()->bool{ACTIVE.with(|active|active.get().is_some())}

pub(super) fn render(source:&Path,workspace:&Path)->Result<RenderedDocument>{
    if workspace.join("wps-active.lock").exists(){bail!("WPS 正在完成上一次文档预览，请稍后重试");}
    let preview:serde_json::Value=read(&workspace.join("request.json"))?;
    let generation=preview["generation"].as_u64().context("文档预览请求无效")?;
    let id=SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let request=Request{id,source:source.into(),preview_generation:generation};
    let input=request_path(workspace,id);write(&input,&request)?;
    // The visible preview helper belongs to a kill-on-close job. This helper
    // explicitly breaks away so terminating that preview cannot skip WPS Drop.
    let mut child=Command::new(std::env::current_exe()?).arg("--wps-render-worker").arg(&input)
        .creation_flags(CREATE_NO_WINDOW.0|CREATE_BREAKAWAY_FROM_JOB.0)
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()
        .context("无法启动文档预览清理进程")?;
    let started=Instant::now();let output=result_path(workspace,id);
    loop{
        if let Ok(outcome)=read::<Outcome>(&output){
            if outcome.id==id{
                let _=fs::remove_file(&output);let _=fs::remove_file(&input);
                std::thread::spawn(move||{let _=child.wait();});
                if let Some(error)=outcome.error{bail!("{error}");}
                let pdf=outcome.pdf.context("文档预览未返回内容")?;
                if pdf.parent()!=Some(workspace)||pdf.file_name().is_none_or(|name|!name.to_string_lossy().starts_with("wps-")){
                    bail!("文档预览路径无效");
                }
                // Result publication happens only after the owned document has
                // closed. Reap the short-lived process away from the UI helper.
                return Ok(RenderedDocument{pdf});
            }
        }
        if cancelled_id(workspace,id)||started.elapsed()>LIMIT{
            let _=fs::write(cancel_path(workspace,id),b"cancel");
            // Never kill the process holding WPS's only cleanup guard. One
            // cross-window mutex bounds pending WPS work to one document.
            std::thread::spawn(move||{let _=child.wait();});
            bail!("文档预览已取消，正在关闭本次预览文档");
        }
        if let Some(exit)=child.try_wait()?{
            // A successful writer publishes before exiting; re-read once to
            // avoid racing the exit notification with the final file rename.
            if output.exists(){continue;}
            bail!("文档预览进程已退出（{exit}）");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

struct MutexOwner(HANDLE);
impl Drop for MutexOwner{fn drop(&mut self){let _=unsafe{ReleaseMutex(self.0)};let _=unsafe{CloseHandle(self.0)};}}
fn single_renderer()->Result<MutexOwner>{
    let mutex=unsafe{CreateMutexW(None,true,w!("Local\\ZSDesk.WpsPreview.DocumentCleanup"))}?;
    if unsafe{GetLastError()}==ERROR_ALREADY_EXISTS{let _=unsafe{CloseHandle(mutex)};bail!("WPS 正在完成另一个文档预览，请稍后重试");}
    Ok(MutexOwner(mutex))
}

pub(in super::super) fn run_worker_if_requested()->Option<Result<()>>{
    if std::env::args_os().nth(1).as_deref()!=Some(std::ffi::OsStr::new("--wps-render-worker")){return None;}
    Some((||{
        let input=PathBuf::from(std::env::args_os().nth(2).context("缺少文档预览请求")?);
        if std::env::args_os().count()!=3||!input.is_absolute(){bail!("文档预览请求无效");}
        let workspace=input.parent().context("缺少文档预览目录")?;
        if workspace.file_name().is_none_or(|name|!name.to_string_lossy().starts_with("preview-")){bail!("文档预览目录无效");}
        let request:Request=read(&input)?;
        if input!=request_path(workspace,request.id)||!request.source.is_absolute(){bail!("文档预览请求路径无效");}
        let _mutex=match single_renderer(){Ok(mutex)=>mutex,Err(error)=>{
            write(&result_path(workspace,request.id),&Outcome{id:request.id,pdf:None,error:Some(error.to_string())})?;return Ok(());
        }};
        let mut lease=fs::OpenOptions::new().read(true).write(true).create_new(true).share_mode(0)
            .custom_flags(FILE_FLAG_DELETE_ON_CLOSE.0).open(workspace.join("wps-active.lock"))?;
        let started=SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let identity=serde_json::json!({"pid":std::process::id(),"started_unix_nanos":started.to_string(),"request":request.id.to_string(),"purpose":"WPS read-only preview and owned document cleanup"});
        lease.write_all(&serde_json::to_vec(&identity)?)?;
        write(&workspace.join("wps-worker.json"),&identity)?;
        ACTIVE.with(|active|active.set(Some(request.id)));
        unsafe{OleInitialize(None)}?;
        struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}let apartment=Apartment;
        let outcome=if cancelled_id(workspace,request.id){Err(anyhow::anyhow!("文档预览已取消"))}else{render_owned(&request.source,workspace)};
        drop(apartment);
        let cancelled=cancelled_id(workspace,request.id);
        let mut retained=None;
        let result=match outcome{
            Ok(rendered) if !cancelled=>{let pdf=rendered.pdf.clone();retained=Some(rendered);Outcome{id:request.id,pdf:Some(pdf),error:None}},
            Ok(rendered)=>{drop(rendered);Outcome{id:request.id,pdf:None,error:Some("文档预览已取消".into())}},
            Err(error)=>{let _=fs::remove_file(workspace.join(format!("wps-{}.pdf",std::process::id())));Outcome{id:request.id,pdf:None,error:Some(format!("{error:#}"))}},
        };
        if !cancelled{
            write(&result_path(workspace,request.id),&result)?;
            if let Some(rendered)=retained.take(){std::mem::forget(rendered);}
        }
        drop(lease);
        if cancelled{
            let _=fs::remove_file(&input);let _=fs::remove_file(cancel_path(workspace,request.id));
            if disposed(workspace){super::super::preview_process::cleanup(workspace);}
        }
        Ok(())
    })())
}
