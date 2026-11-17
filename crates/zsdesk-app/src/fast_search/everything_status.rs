//! Read-only source, IPC and SCM observations for the search settings window.
use super::*;
use std::ffi::c_void;

#[derive(Clone,Debug,PartialEq,Eq)]
enum ConnectionState{NotConnected,Indexing,Ready,Unavailable(String)}
fn observed_readiness(index_source:Result<()>,loaded:impl FnOnce()->Result<usize>)->ConnectionState{
    match index_source.and_then(|_|loaded()){
        Ok(0)=>ConnectionState::Indexing,Ok(_)=>ConnectionState::Ready,
        Err(error)=>ConnectionState::Unavailable(format!("{error:#}")),
    }
}
#[derive(Clone,Debug)]
struct Snapshot{
    builtin:bool,configured:Option<PathBuf>,candidate:Option<PathBuf>,source:String,
    connected_path:Option<PathBuf>,pid:Option<u32>,instance:Option<String>,managed:bool,
    connection:ConnectionState,fallback_builtin:bool,services:Vec<String>,note:Option<String>,
}
impl Snapshot{
    fn from_config(builtin:bool,configured:Option<PathBuf>)->Self{
        Self{builtin,configured,candidate:None,source:String::new(),connected_path:None,pid:None,instance:None,managed:false,
            connection:ConnectionState::NotConnected,fallback_builtin:false,services:Vec::new(),note:None}
    }
    fn render(&self)->String{
        let mut rows=vec![zsdesk_core::i18n::text("搜索引擎状态").to_owned(),String::new(),format!("配置搜索引擎：{}",if self.builtin{"自带搜索"}else{"Everything"}),
            format!("配置来源：{}",if self.configured.is_some(){"指定的 Everything 程序"}else{"自动选择已运行、已安装或内置 Everything"})];
        if let Some(path)=&self.configured{rows.push(format!("指定程序：{}",path.display()));}
        if self.builtin{
            rows.push("Everything 连接：未使用（当前选择自带搜索）".into());
            if self.configured.is_some(){rows.push("指定路径已保存，切换到 Everything 搜索时使用。".into());}
            return rows.join("\r\n");
        }
        if !self.source.is_empty(){rows.push(format!("有效来源：{}",self.source));}
        if self.fallback_builtin{rows.push("可用搜索后端：自带搜索（Everything 程序不可用，搜索时回退）".into());}
        if let Some(path)=&self.candidate{rows.push(format!("程序路径：{}",path.display()));}
        rows.push(String::new());
        rows.push(match &self.connection{
            ConnectionState::NotConnected=>"实际连接：尚未连接；选择程序仅保存配置，首次搜索时连接或启动。".into(),
            ConnectionState::Indexing=>"实际连接：IPC 已响应；索引仍在加载，尚未就绪。".into(),
            ConnectionState::Ready=>"实际连接：IPC 已响应；文件名索引已就绪。".into(),
            ConnectionState::Unavailable(error)=>format!("实际连接：未确认可用\r\n失败原因：{error}"),
        });
        if let Some(path)=&self.connected_path{rows.push(format!("连接程序：{}",path.display()));}
        if let Some(pid)=self.pid{rows.push(format!("连接进程：{pid}"));}
        if self.connected_path.is_some(){
            let instance=self.instance.as_deref().unwrap_or("默认实例");
            rows.push(format!("连接实例：{instance}{}",if self.managed{"（ZSDesk 独立实例）"}else{"（现有实例）"}));
        }
        if !self.services.is_empty(){rows.push(String::new());rows.push("索引服务状态".into());rows.extend(self.services.iter().cloned());
            rows.push("服务运行不等于当前实例正在使用该服务；以上 IPC 和索引状态来自实际连接的 Everything 进程。".into());}
        if let Some(note)=&self.note{rows.push(String::new());rows.push(note.clone());}
        rows.join("\r\n")
    }
}

#[repr(C)]#[derive(Default)]
struct ServiceStatus{kind:u32,state:u32,accepted:u32,exit:u32,specific_exit:u32,checkpoint:u32,wait_hint:u32,pid:u32,flags:u32}
#[link(name="advapi32")]unsafe extern "system"{
    fn OpenSCManagerW(machine:*const u16,database:*const u16,access:u32)->*mut c_void;
    fn OpenServiceW(manager:*mut c_void,name:*const u16,access:u32)->*mut c_void;
    fn QueryServiceStatusEx(service:*mut c_void,level:u32,buffer:*mut u8,bytes:u32,needed:*mut u32)->i32;
    fn CloseServiceHandle(handle:*mut c_void)->i32;
}
struct ServiceHandle(*mut c_void);impl Drop for ServiceHandle{fn drop(&mut self){if !self.0.is_null(){unsafe{CloseServiceHandle(self.0);}}}}
fn service_state(name:&str)->String{
    let manager=ServiceHandle(unsafe{OpenSCManagerW(std::ptr::null(),std::ptr::null(),1)});
    if manager.0.is_null(){return format!("{name}：无法读取服务状态（{}）",std::io::Error::last_os_error());}
    let service=ServiceHandle(unsafe{OpenServiceW(manager.0,wide(name).as_ptr(),4)});
    if service.0.is_null(){let error=std::io::Error::last_os_error();return if error.raw_os_error()==Some(1060){format!("{name}：未安装")}else{format!("{name}：无法读取（{error}）")};}
    let mut status=ServiceStatus::default();let mut needed=0;
    if unsafe{QueryServiceStatusEx(service.0,0,(&mut status as*mut ServiceStatus).cast(),std::mem::size_of::<ServiceStatus>()as u32,&mut needed)}==0{
        return format!("{name}：无法读取（{}）",std::io::Error::last_os_error());
    }
    let state=match status.state{1=>"已停止",2=>"正在启动",3=>"正在停止",4=>zsdesk_core::i18n::text("运行中"),5=>"正在继续",6=>"正在暂停",7=>zsdesk_core::i18n::text("已暂停"),_=>"状态未知"};
    if status.state==4{format!("{name}：{state}（服务进程 {}）",status.pid)}else{format!("{name}：{state}")}
}

fn observe(configured:Option<PathBuf>,builtin:bool)->Snapshot{
    let mut state=Snapshot::from_config(builtin,configured.clone());
    if builtin{return state;}
    // Same preference order as ensure_running, with no lock, startup, INI
    // changes, installation, or ownership adoption performed by this probe.
    let connection=external(configured.as_deref()).or_else(||root().ok().and_then(|root|managed_connection(&root))
        .filter(|connection|configured.as_deref().is_none_or(|path|same_path(path,&connection.executable))));
    if let Some(connection)=connection{
        state.candidate=Some(connection.executable.clone());state.connected_path=Some(connection.executable.clone());
        state.instance=connection.instance.clone();state.managed=connection.managed;
        state.source=if configured.is_some(){"指定的 Everything 程序"}else if connection.managed{"内置 Everything 引擎"}else{"已运行的 Everything（自动选择）"}.into();
        let identity_before=identity(connection.hwnd);state.pid=identity_before.as_ref().map(|value|value.pid);
        state.connection=observed_readiness(check_index_source(&connection),||ipc(connection.hwnd,401,0));
        if identity_before.is_none()||identity(connection.hwnd)!=identity_before{
            state.connection=ConnectionState::Unavailable("Everything 进程已退出或实例发生变化，请刷新状态。".into());
        }
    }else if let Some(path)=configured{
        state.source="指定的 Everything 程序".into();state.candidate=Some(path.clone());
        if let Err(error)=ensure_executable(&path){state.fallback_builtin=true;state.connection=ConnectionState::Unavailable(format!("指定程序不可用：{}（{error:#}）",path.display()));}
    }else if let Some(path)=installed_executable(None){state.source="已安装的 Everything（自动选择）".into();state.candidate=Some(path);}
    else{match bundle(){Ok(path)=>{state.source="内置 Everything 引擎".into();state.candidate=Some(path);},Err(error)=>{state.fallback_builtin=true;state.connection=ConnectionState::Unavailable(format!("未找到可用的已安装或内置引擎：{error:#}"));}}}
    let mut services=vec!["Everything".to_owned(),format!("Everything ({SERVICE_INSTANCE})")];
    if let Some(instance)=state.instance.as_ref().filter(|instance|!instance.starts_with("ZSdesk-")){services.push(format!("Everything ({instance})"));}
    services.sort();services.dedup();state.services=services.iter().map(|name|service_state(name)).collect();
    state.note=Some("状态检查不会启动 Everything 或安装服务。可先执行一次搜索，再查看连接和索引状态；窗口会自动刷新。".into());
    state
}
pub(super) fn report(profile:&Path)->String{
    let result=(||->Result<String>{
        let before=crate::storage::load(profile)?.ui.search_preferences();
        let snapshot=observe(before.everything_executable.clone(),before.backend.is_builtin());
        let after=crate::storage::load(profile)?.ui.search_preferences();
        if before!=after{return Ok("搜索设置已改变，正在等待下一次状态刷新。".into());}
        Ok(snapshot.render())
    })();
    result.unwrap_or_else(|error|format!("搜索引擎状态\r\n\r\n无法读取当前设置：{error:#}"))
}

#[cfg(test)]mod tests{
    use super::*;
    #[test]fn loaded_database_does_not_hide_a_missing_or_unknown_managed_index_source(){
        let denied=require_managed_index_source(Ok(false),||false,||false);
        let state=observed_readiness(denied,||panic!("DB-loaded must not override unavailable index source"));
        assert!(matches!(state,ConnectionState::Unavailable(ref error) if error.contains("兼容")&&error.contains("目录索引")));
        let unknown=require_managed_index_source(Err(anyhow::anyhow!("IPC403 timeout")),||true,||true);
        assert!(matches!(observed_readiness(unknown,||Ok(1)),ConnectionState::Unavailable(ref error) if error.contains("IPC403 timeout")));
        assert_eq!(observed_readiness(require_managed_index_source(Ok(false),||true,||false),||Ok(0)),ConnectionState::Indexing);
        assert_eq!(observed_readiness(require_managed_index_source(Ok(false),||true,||false),||Ok(1)),ConnectionState::Ready);
        assert_eq!(observed_readiness(require_managed_index_source(Ok(false),||false,||true),||Ok(1)),ConnectionState::Ready);
        assert!(matches!(observed_readiness(Ok(()),||Err(anyhow::anyhow!("IPC401 timeout"))),ConnectionState::Unavailable(ref error) if error.contains("IPC401 timeout")));
    }
    #[test]fn choosing_a_program_is_not_a_successful_connection(){
        let path=PathBuf::from(r"E:\Apps\Everything.exe");let mut status=Snapshot::from_config(false,Some(path.clone()));
        status.candidate=Some(path.clone());status.source="指定的 Everything 程序".into();
        let text=status.render();assert!(text.contains(&path.display().to_string())&&text.contains("尚未连接"));assert!(!text.contains("索引已就绪"));
        status.connected_path=Some(path);status.pid=Some(42);status.instance=Some("ZSdesk-1".into());status.managed=true;status.connection=ConnectionState::Indexing;
        let text=status.render();assert!(text.contains("ZSDesk 独立实例")&&text.contains("仍在加载")&&!text.contains("索引已就绪"));
        status.connection=ConnectionState::Ready;assert!(status.render().contains("文件名索引已就绪"));
        status.connection=ConnectionState::Unavailable("IPC超时".into());assert!(status.render().contains("IPC超时")&&!status.render().contains("索引已就绪"));
        assert!(!status.render().contains("搜索时回退"));status.fallback_builtin=true;assert!(status.render().contains("搜索时回退"));
    }
    #[test]fn builtin_and_auto_sources_do_not_reuse_stale_connection_claims(){
        let configured=Some(PathBuf::from(r"E:\Old\Everything.exe"));let builtin=Snapshot::from_config(true,configured);
        assert!(builtin.render().contains("路径已保存")&&builtin.render().contains("未使用"));
        let auto=Snapshot::from_config(false,None);let text=auto.render();assert!(text.contains("自动选择")&&!text.contains("Old")&&!text.contains("连接进程"));
        let mut service_only=auto;service_only.services=vec!["Everything：运行中（服务进程 9）".into()];
        assert!(service_only.render().contains("尚未连接")&&!service_only.render().contains("IPC 已响应"));
    }
}
