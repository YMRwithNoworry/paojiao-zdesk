//! User-scoped, opt-in file-location routing. Packaged manifests contain no
//! machine paths; application and target bindings are generated locally.
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    fs::{self, File},
    io::{Read, Write},
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Condvar, Mutex, OnceLock},
    time::{Duration, Instant},
};
use windows::{
    core::{PCWSTR, PWSTR},
    Win32::{
        Foundation::{CloseHandle, HWND, LPARAM, WPARAM},
        System::{
            Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ},
            Threading::{
                OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
                PROCESS_QUERY_LIMITED_INFORMATION,
            },
        },
        UI::WindowsAndMessaging::{
            FindWindowExW, GetWindowThreadProcessId, PostMessageW, HWND_MESSAGE,
        },
    },
};
pub const WM_RESULT: u32 = 0x8000 + 42;
const LIMIT: u64 = 1024 * 1024;
const STARTUP: &str = "ZSDeskFileRevealRouting";
const LEGACY_STARTUP: &str = "Native360FileRevealRouting";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Application {
    pub id: u32,
    pub label: String,
    pub executable: PathBuf,
    pub caller_module: PathBuf,
    pub profile: u32,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub executable_sha256: String,
    #[serde(default)]
    pub caller_sha256: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub schema: u32,
    pub enabled: bool,
    pub start_with_windows: bool,
    pub pending_changes: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub control_path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub control_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_exe: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub startup_name: Option<String>,
    pub applications: Vec<Application>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
    #[serde(skip)]
    pub legacy: bool,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            schema: 2,
            enabled: false,
            start_with_windows: false,
            pending_changes: false,
            control_path: None,
            control_sha256: None,
            target_exe: None,
            startup_name: None,
            applications: vec![],
            extra: BTreeMap::new(),
            legacy: false,
        }
    }
}
#[derive(Clone, Deserialize)]
struct Component {
    file: String,
    sha256: String,
}
#[derive(Clone, Deserialize)]
struct Manifest {
    schema: String,
    adapter_abi: u32,
    controller: Component,
    adapter: Component,
}
struct Bundle {
    root: PathBuf,
    manifest: Manifest,
}
#[derive(Deserialize)]
struct Inspection {
    executable: PathBuf,
    caller_module: PathBuf,
    label: String,
    supported: bool,
    profile: u32,
    reason: String,
}
#[derive(Deserialize)]
struct Discovery {
    schema: String,
    applications: Vec<Inspection>,
}
enum Action {
    Discover,
    Add(PathBuf),
    AddWithCaller(PathBuf, PathBuf),
    Apply,
    Disable,
    Refresh,
}
struct Request {
    profile: PathBuf,
    action: Action,
    window: isize,
}
struct Worker {
    pending: Mutex<VecDeque<Request>>,
    wake: Condvar,
}
static WORKER: OnceLock<Arc<Worker>> = OnceLock::new();
static WORKER_INIT: Mutex<()> = Mutex::new(());

fn config_file(profile: &Path) -> PathBuf {
    profile.with_file_name("shell-routing.json")
}
fn key(path: &Path) -> String {
    path.to_string_lossy().replace('/', "\\").to_lowercase()
}
fn token(profile: &Path) -> String {
    format!("{:x}", Sha256::digest(key(profile).as_bytes()))
}
pub fn profile_property(profile: &Path) -> Vec<u16> {
    format!("ZSDesk.RoutingProfile.{}", token(profile))
        .encode_utf16()
        .chain(Some(0))
        .collect()
}
fn bounded_read(path: &Path) -> Result<Vec<u8>> {
    let m = path.metadata()?;
    ensure!(m.is_file() && m.len() <= LIMIT, "配置大小或类型无效");
    let data = fs::read(path)?;
    ensure!(data.len() as u64 == m.len(), "配置正在变化");
    Ok(data)
}
fn digest(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("无法读取 {}", path.display()))?;
    let mut hash = Sha256::new();
    let mut block = [0u8; 65536];
    loop {
        let n = file.read(&mut block)?;
        if n == 0 {
            break;
        }
        hash.update(&block[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
fn valid_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit())
}
fn atomic(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension("routing-pending");
    let mut file = File::create(&temp)?;
    file.write_all(data)?;
    file.sync_all()?;
    drop(file);
    fs::rename(temp, path)?;
    ensure!(fs::read(path)? == data, "配置回读校验失败");
    Ok(())
}
fn save(profile: &Path, settings: &Settings) -> Result<()> {
    let path = config_file(profile);
    if path.exists() {
        atomic(
            &path.with_file_name("shell-routing.previous.json"),
            &bounded_read(&path)?,
        )?;
    }
    atomic(&path, &serde_json::to_vec_pretty(settings)?)
}
pub fn save_draft(profile: &Path, settings: &Settings) -> Result<()> {
    validate_settings(settings)?;
    let mut next = settings.clone();
    next.schema = 2;
    next.pending_changes = true;
    save(profile, &next)
}
fn validate_settings(settings: &Settings) -> Result<()> {
    ensure!(
        settings.schema == 2 && settings.applications.len() <= 128,
        "路由配置版本或数量无效"
    );
    ensure!(
        settings
            .startup_name
            .as_deref()
            .is_none_or(|name| name == STARTUP || name == LEGACY_STARTUP),
        "启动项不属于此程序"
    );
    let mut ids = HashSet::new();
    let mut paths = HashSet::new();
    for app in &settings.applications {
        ensure!(
            app.id >= 2 && ids.insert(app.id) && paths.insert(key(&app.executable)),
            "应用编号或路径重复"
        );
    }
    Ok(())
}
pub fn load(profile: &Path) -> Result<Settings> {
    let path = config_file(profile);
    if !path.try_exists()? {
        return Ok(Settings::default());
    }
    let raw: serde_json::Value =
        serde_json::from_slice(&bounded_read(&path)?).context("文件位置转发配置损坏")?;
    let mut settings: Settings = serde_json::from_value(raw.clone())?;
    validate_settings(&settings)?;
    if let Some(control) = &settings.control_path {
        let ini = read_ini(&control.with_file_name("package.ini"))?;
        settings.legacy = !ini.contains_key("BindingsFile");
        if settings.legacy {
            settings
                .startup_name
                .get_or_insert_with(|| LEGACY_STARTUP.into());
            if raw.get("enabled").is_none() {
                settings.enabled = service_running(control);
            }
            if raw.get("start_with_windows").is_none() {
                settings.start_with_windows = startup_owned(control, LEGACY_STARTUP);
            }
        }
    }
    Ok(settings)
}
fn component(root: &Path, item: &Component) -> Result<PathBuf> {
    ensure!(
        item.file == "ShellRoutingControl.exe" || item.file == "ZSDeskRevealRouter.dll",
        "组件名称无效"
    );
    ensure!(valid_hash(&item.sha256), "组件内容标识无效");
    let path = root.join(&item.file);
    ensure!(
        digest(&path)?.eq_ignore_ascii_case(&item.sha256),
        "组件校验失败：{}",
        item.file
    );
    Ok(path)
}
fn bundle_at(root: &Path) -> Result<Bundle> {
    let manifest: Manifest = serde_json::from_slice(&bounded_read(&root.join("manifest.json"))?)?;
    ensure!(
        manifest.schema == "ZSDesk.RoutingPackage.v1" && manifest.adapter_abi == 2,
        "组件清单版本不兼容"
    );
    ensure!(
        manifest.controller.file == "ShellRoutingControl.exe"
            && manifest.adapter.file == "ZSDeskRevealRouter.dll",
        "组件清单无效"
    );
    component(root, &manifest.controller)?;
    component(root, &manifest.adapter)?;
    Ok(Bundle {
        root: root.into(),
        manifest,
    })
}
fn bundled() -> Result<Bundle> {
    let exe = std::env::current_exe()?;
    bundle_at(&exe.parent().context("无法确定安装目录")?.join("routing"))
        .context("此安装包缺少可用的文件位置转发组件")
}
pub fn bundle_available() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.join("routing/manifest.json")))
        .is_some_and(|p| p.is_file())
}
fn installed(settings: &Settings) -> Result<Option<PathBuf>> {
    let Some(path) = &settings.control_path else {
        return Ok(None);
    };
    let hash = settings
        .control_sha256
        .as_deref()
        .context("转发器缺少内容标识")?;
    ensure!(
        path.is_absolute()
            && path.file_name().is_some_and(|n| n
                .to_string_lossy()
                .eq_ignore_ascii_case("ShellRoutingControl.exe"))
            && valid_hash(hash),
        "转发器绑定无效"
    );
    ensure!(
        digest(path)?.eq_ignore_ascii_case(hash),
        "已安装转发器发生变化，未执行操作"
    );
    Ok(Some(path.clone()))
}
fn output(exe: &Path, args: &[&std::ffi::OsStr]) -> Result<std::process::Output> {
    Command::new(exe)
        .args(args)
        .creation_flags(0x08000000)
        .stdin(Stdio::null())
        .output()
        .context("无法启动路由组件")
}
fn control(exe: &Path, arg: &str) -> Result<()> {
    let result = output(exe, &[arg.as_ref()])?;
    ensure!(
        result.status.success(),
        "{}",
        match result.status.code() {
            Some(5) => "应用权限不匹配".to_string(),
            Some(50) => "应用接口不兼容".into(),
            Some(183) => "应用中已加载另一份转发组件，请关闭该应用、应用接管设置后再重新打开".into(),
            Some(1467) => "应用中仍有旧转发组件，原有转发保持不变；请关闭对应应用、应用接管设置后再重新打开".into(),
            Some(1306) => "程序已变化，请重新检查绑定".into(),
            _ => format!(
                "组件执行失败：{}",
                String::from_utf8_lossy(&result.stdout).trim()
            ),
        }
    );
    Ok(())
}
fn json_output<T: serde::de::DeserializeOwned>(result: &std::process::Output) -> Result<T> {
    let text = String::from_utf8_lossy(&result.stdout);
    let line = text
        .lines()
        .find(|s| s.trim_start().starts_with('{'))
        .context("组件未返回兼容信息")?;
    Ok(serde_json::from_str(line)?)
}
fn inspect(bundle: &Bundle, path: &Path, caller: Option<&Path>) -> Result<Inspection> {
    let exe = component(&bundle.root, &bundle.manifest.controller)?;
    let mut args = vec![std::ffi::OsStr::new("--inspect"), path.as_os_str()];
    if let Some(caller) = caller {
        args.push(caller.as_os_str());
    }
    let value: Inspection = json_output(&output(&exe, &args)?)?;
    ensure!(value.supported, "此应用尚不兼容：{}", value.reason);
    Ok(value)
}
fn discover(bundle: &Bundle) -> Result<Vec<Inspection>> {
    let exe = component(&bundle.root, &bundle.manifest.controller)?;
    let value: Discovery = json_output(&output(&exe, &["--discover".as_ref()])?)?;
    ensure!(
        value.schema == "ZSDesk.RoutingDiscovery.v1" && value.applications.len() <= 128,
        "发现结果无效"
    );
    Ok(value.applications)
}
fn label(value: &Inspection) -> String {
    let path = key(&value.executable);
    if path.contains("openai.codex_") {
        "Codex".into()
    } else if value
        .executable
        .file_name()
        .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case("Weixin.exe"))
    {
        "微信".into()
    } else {
        value
            .label
            .chars()
            .filter(|c| !c.is_control())
            .take(80)
            .collect()
    }
}
fn legacy_default_selection(value: &Inspection) -> bool {
    matches!(value.profile, 2 | 3) && matches!(label(value).as_str(), "Codex" | "微信")
}
fn add(settings: &mut Settings, value: Inspection, selected: bool) -> Result<()> {
    ensure!(
        value.supported && matches!(value.profile, 1..=4),
        "应用不兼容"
    );
    ensure!(
        key(&value.executable) != key(&std::env::current_exe()?),
        "不需要将 ZSDesk 设为自身的来源"
    );
    if let Some(app) = settings
        .applications
        .iter_mut()
        .find(|a| key(&a.executable) == key(&value.executable))
    {
        app.caller_module = value.caller_module;
        app.profile = value.profile;
        app.enabled |= selected;
        return Ok(());
    }
    ensure!(settings.applications.len() < 128, "应用数量超过上限");
    let id = settings
        .applications
        .iter()
        .map(|a| a.id)
        .max()
        .unwrap_or(99)
        .checked_add(1)
        .context("应用编号已用尽")?;
    settings.applications.push(Application {
        id,
        label: label(&value),
        executable: value.executable,
        caller_module: value.caller_module,
        profile: value.profile,
        enabled: selected,
        executable_sha256: String::new(),
        caller_sha256: String::new(),
    });
    Ok(())
}

pub fn request_discovery(profile: &Path, window: HWND) -> Result<()> {
    enqueue(profile, Action::Discover, window)
}
pub fn request_add(profile: &Path, path: PathBuf, window: HWND) -> Result<()> {
    enqueue(profile, Action::Add(path), window)
}
/// Pins an explicitly chosen helper DLL in the selected application's directory.
pub fn request_add_with_caller(profile: &Path, executable: PathBuf, caller_module: PathBuf, window: HWND) -> Result<()> {
    enqueue(profile, Action::AddWithCaller(executable, caller_module), window)
}
pub fn diagnostic_reason(code: u32) -> (&'static str, bool) {
    (match code {
        0 => "暂无拒绝记录", 1 => "调用来自尚未绑定的辅助模块", 2 => "此打开方式保持系统处理",
        3 => "隐藏、最小化或特殊显示方式保持系统处理", 4 => "目标不是已核验的系统 Explorer",
        5 => "参数不是受支持的文件夹或定位请求", 6 => "特殊启动标志保持原调用", 7 => "特殊启动信息保持原调用",
        8 => "ANSI 参数无法无损转换", 9 => "文件或目录当前不可用", 10 => "未找到已核验的转发服务",
        11 => "进程访问被拒绝", 12 => "访问较高权限进程被拒绝", 13 => "请求结果超时，未重复发送",
        14 => "请求参数无效", 15 => "转发器文件身份已变化", 16 => "无法创建转发进程",
        17 | 1467 => "请关闭对应应用、应用接管设置后再重新打开，原有转发保持不变", _ => "当前入口未提供具体诊断",
    }, matches!(code, 17 | 1467))
}
pub fn request_apply(profile: &Path, window: HWND) -> Result<()> {
    enqueue(profile, Action::Apply, window)
}
/// Synchronous counterpart for a background CLI. Returns only after the selected
/// bindings are applied; it does not create a window or queue work that could be
/// lost when the short-lived command process exits.
pub fn apply_owned_settings(config_path: &Path) -> Result<String> {
    apply_settings(config_path, load(config_path)?)
}
pub fn configure(profile: &Path, enabled: bool, window: HWND) -> Result<bool> {
    let s = load(profile)?;
    if s.control_path.is_none() && !s.applications.iter().any(|a| a.enabled) {
        return Ok(false);
    }
    enqueue(
        profile,
        if enabled {
            Action::Apply
        } else {
            Action::Disable
        },
        window,
    )?;
    Ok(true)
}
pub fn refresh_owned_target(profile: &Path, window: HWND) -> Result<()> {
    if config_file(profile).exists() {
        enqueue(profile, Action::Refresh, window)?;
    }
    Ok(())
}
pub fn uninstall_owned(profile: &Path, exe: &Path) -> Result<()> {
    let mut settings = load(profile)?;
    if settings
        .target_exe
        .as_ref()
        .is_none_or(|p| key(p) != key(exe))
    {
        return Ok(());
    }
    let Some(control_path) = installed(&settings)? else {
        return Ok(());
    };
    let ini = read_ini(&control_path.with_file_name("package.ini"))?;
    if ini
        .get("TargetExe")
        .is_none_or(|p| key(Path::new(p)) != key(exe))
    {
        return Ok(());
    }
    control(&control_path, "--restore")?;
    settings.enabled = false;
    settings.pending_changes = false;
    save(profile, &settings)
}
fn enqueue(profile: &Path, action: Action, window: HWND) -> Result<()> {
    let _init = WORKER_INIT
        .lock()
        .map_err(|_| anyhow::anyhow!("控制队列不可用"))?;
    let worker = match WORKER.get() {
        Some(w) => w.clone(),
        None => {
            let state = Arc::new(Worker {
                pending: Mutex::new(VecDeque::new()),
                wake: Condvar::new(),
            });
            let background = state.clone();
            std::thread::Builder::new()
                .name("zsdesk-routing-control".into())
                .stack_size(512 * 1024)
                .spawn(move || run(background))?;
            let _ = WORKER.set(state.clone());
            state
        }
    };
    let mut queue = worker
        .pending
        .lock()
        .map_err(|_| anyhow::anyhow!("控制队列不可用"))?;
    if matches!(action, Action::Refresh)
        && queue
            .iter()
            .any(|r| r.profile == profile && matches!(r.action, Action::Refresh))
    {
        return Ok(());
    }
    ensure!(queue.len() < 16, "后台设置请求较多，请稍后重试");
    queue.push_back(Request {
        profile: profile.to_path_buf(),
        action,
        window: window.0 as isize,
    });
    worker.wake.notify_one();
    Ok(())
}
fn run(worker: Arc<Worker>) {
    loop {
        let request = {
            let Ok(mut slot) = worker.pending.lock() else {
                return;
            };
            while slot.is_empty() {
                slot = match worker.wake.wait(slot) {
                    Ok(s) => s,
                    Err(_) => return,
                };
            }
            slot.pop_front().unwrap()
        };
        let silent = matches!(request.action, Action::Refresh);
        let result = apply_request(&request);
        if silent && result.is_ok() {
            continue;
        }
        let text = match result {
            Ok(text) => text,
            Err(e) => format!("文件位置设置未完成：{e}"),
        };
        let hwnd = HWND(request.window as *mut _);
        let mut pid = 0;
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
        }
        if pid != std::process::id() {
            continue;
        }
        let raw = Box::into_raw(Box::new(text));
        if unsafe { PostMessageW(Some(hwnd), WM_RESULT, WPARAM(0), LPARAM(raw as isize)) }.is_err()
        {
            unsafe {
                drop(Box::from_raw(raw));
            }
        }
    }
}
fn apply_request(request: &Request) -> Result<String> {
    let mut settings = load(&request.profile)?;
    match &request.action {
        Action::Discover => {
            let bundle = bundled()?;
            let legacy = settings.legacy;
            for found in discover(&bundle)? {
                let selected = legacy && legacy_default_selection(&found);
                let _ = add(&mut settings, found, selected);
            }
            let count = settings.applications.len();
            save_draft(&request.profile, &settings)?;
            Ok(format!(
                "已发现 {count} 个可配置应用，请勾选后点击“一键启用所选应用”"
            ))
        }
        Action::Add(path) => {
            let bundle = bundled()?;
            add(&mut settings, inspect(&bundle, path, None)?, true)?;
            save_draft(&request.profile, &settings)?;
            Ok("应用已加入选择，点击“一键启用所选应用”生效".into())
        }
        Action::AddWithCaller(path, caller) => {
            let bundle = bundled()?;
            add(&mut settings, inspect(&bundle, path, Some(caller))?, true)?;
            save_draft(&request.profile, &settings)?;
            Ok("辅助模块已加入草稿，应用后仅允许该模块和已核验的主程序转发文件导航".into())
        }
        Action::Apply => apply_settings(&request.profile, settings),
        Action::Disable => {
            if let Some(path) = installed(&settings)? {
                control(&path, "--restore")?;
            }
            settings.enabled = false;
            settings.pending_changes = false;
            save(&request.profile, &settings)?;
            Ok("文件位置转发已停用，应用选择已保留".into())
        }
        Action::Refresh => {
            refresh_target(&settings)?;
            Ok(String::new())
        }
    }
}
fn read_ini(path: &Path) -> Result<BTreeMap<String, String>> {
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let data = bounded_read(path)?;
    let text = if data.starts_with(&[255, 254]) {
        ensure!(data.len() % 2 == 0, "配置编码无效");
        String::from_utf16(
            &data[2..]
                .chunks_exact(2)
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .collect::<Vec<_>>(),
        )?
    } else {
        String::from_utf8(data)?
    };
    let mut routing = false;
    let mut map = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            routing = line.eq_ignore_ascii_case("[Routing]");
            continue;
        }
        if routing {
            if let Some((k, v)) = line.split_once('=') {
                map.insert(k.into(), v.into());
            }
        }
    }
    Ok(map)
}
fn utf16_file(path: &Path, text: &str) -> Result<()> {
    let mut data = vec![255, 254];
    for n in text.encode_utf16() {
        data.extend(n.to_le_bytes());
    }
    atomic(path, &data)
}
fn write_ini(path: &Path, map: &BTreeMap<String, String>) -> Result<()> {
    let mut text = String::from("[Routing]\r\n");
    for (k, v) in map {
        ensure!(
            !k.contains(['=', '\r', '\n']) && !v.contains(['\r', '\n']),
            "配置包含非法换行"
        );
        text.push_str(&format!("{k}={v}\r\n"));
    }
    utf16_file(path, &text)
}
fn targets_current(settings: &Settings, ini: &BTreeMap<String, String>, current: &Path) -> bool {
    settings.target_exe.as_ref().is_some_and(|path| key(path) == key(current))
        && ini.get("TargetExe").is_some_and(|path| key(Path::new(path)) == key(current))
}
fn resume_owned_watcher(settings: &Settings, ini: &BTreeMap<String, String>, current: &Path, running: bool) -> bool {
    settings.enabled && !running && targets_current(settings, ini, current)
}
fn refresh_target(settings: &Settings) -> Result<()> {
    let current = std::env::current_exe()?;
    if settings
        .target_exe
        .as_ref()
        .is_none_or(|p| key(p) != key(&current))
    {
        return Ok(());
    }
    if let Some(control) = installed(settings)? {
        let path = control.with_file_name("package.ini");
        let mut ini = read_ini(&path)?;
        if targets_current(settings, &ini, &current) {
            let hash = digest(&current)?;
            if ini.get("TargetSHA256") != Some(&hash) {
                ini.insert("TargetSHA256".into(), hash);
                write_ini(&path, &ini)?;
            }
            // Resume only the already installed binding file. A draft selection
            // and the independent login-startup preference are never applied here.
            if resume_owned_watcher(settings, &ini, &current, service_running(&control)) {
                start_watch(&control)?;
            }
        }
    }
    Ok(())
}
fn validate_application(bundle: &Bundle, app: &mut Application, target_hash: &str) -> Result<()> {
    ensure!(
        app.executable.is_absolute()
            && app.caller_module.is_absolute()
            && app.executable.to_string_lossy().encode_utf16().count() < 2048
            && app.caller_module.to_string_lossy().encode_utf16().count() < 2048,
        "应用路径无效或过长"
    );
    let actual = inspect(bundle, &app.executable, Some(&app.caller_module))?;
    ensure!(
        actual.profile == app.profile,
        "调用模块发生变化，请重新发现应用"
    );
    app.executable_sha256 = digest(&app.executable)?;
    app.caller_sha256 = digest(&app.caller_module)?;
    ensure!(
        app.executable_sha256 != target_hash,
        "ZSDesk 不能作为自身的来源"
    );
    Ok(())
}
fn write_clients(path: &Path, apps: &[Application]) -> Result<()> {
    let selected: Vec<_> = apps.iter().filter(|a| a.enabled).collect();
    let mut text = format!("[Clients]\r\nCount={}\r\n", selected.len());
    for (i, a) in selected.iter().enumerate() {
        ensure!(
            !a.executable.to_string_lossy().contains(['\r', '\n'])
                && !a.caller_module.to_string_lossy().contains(['\r', '\n']),
            "应用路径无效"
        );
        text.push_str(&format!("\r\n[Client.{i}]\r\nId={}\r\nProfile={}\r\nEnabled=1\r\nExecutable={}\r\nExecutableSHA256={}\r\nCallerModule={}\r\nCallerSHA256={}\r\n",a.id,a.profile,a.executable.display(),a.executable_sha256,a.caller_module.display(),a.caller_sha256));
    }
    utf16_file(path, &text)
}
fn copy_component(root: &Path, item: &Component, target: &Path) -> Result<()> {
    let source = component(root, item)?;
    if target.exists() && digest(target)?.eq_ignore_ascii_case(&item.sha256) {
        return Ok(());
    }
    let bytes = fs::read(&source)?;
    ensure!(
        format!("{:x}", Sha256::digest(&bytes)).eq_ignore_ascii_case(&item.sha256),
        "组件正在变化"
    );
    atomic(target, &bytes)
}
fn start_watch(controller: &Path) -> Result<()> {
    if service_running(controller) {
        return Ok(());
    }
    let mut child = Command::new(controller)
        .arg("--watch")
        .creation_flags(0x08000000)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let until = Instant::now() + Duration::from_secs(10);
    while !service_running(controller) {
        if let Some(status) = child.try_wait()? {
            ensure!(status.success(), "转发服务启动失败");
        }
        ensure!(Instant::now() < until, "转发服务尚未准备好");
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}
fn import_legacy_choices(settings: &Settings) -> bool {
    settings.legacy && settings.applications.is_empty() && !settings.pending_changes
}
fn require_selected_applications(settings: &Settings) -> Result<()> {
    ensure!(settings.applications.iter().any(|app| app.enabled) || import_legacy_choices(settings),
            "请先勾选需要接管的应用");
    Ok(())
}
#[derive(Deserialize)]
struct RestartProcess { pid: u32, application: String }
#[derive(Deserialize)]
struct UpgradeCheck { schema: String, restart_required: bool, processes: Vec<RestartProcess> }
struct UpgradeInput { path: PathBuf, bytes: Vec<u8> }
impl Drop for UpgradeInput {
    fn drop(&mut self) {
        if fs::read(&self.path).ok().as_deref() == Some(self.bytes.as_slice()) { let _ = fs::remove_file(&self.path); }
    }
}
fn preflight_component_replacement(profile: &Path, bundle: &Bundle, settings: &Settings, directory: &Path) -> Result<()> {
    // Include selected validated bindings; installed bindings are independently
    // read by the controller so a newly deselected running app is also retained.
    let applications: Vec<_> = settings.applications.iter().filter(|app| app.enabled && valid_hash(&app.executable_sha256) && valid_hash(&app.caller_sha256)).collect();
    let mut ini = format!("[Clients]\r\nCount={}\r\n", applications.len());
    for (index, app) in applications.into_iter().enumerate() {
        ini.push_str(&format!("\r\n[Client.{index}]\r\nId={}\r\nProfile={}\r\nEnabled=1\r\nExecutable={}\r\nExecutableSHA256={}\r\nCallerModule={}\r\nCallerSHA256={}\r\n",
            app.id, app.profile, app.executable.display(), app.executable_sha256, app.caller_module.display(), app.caller_sha256));
    }
    let path = profile.with_file_name(format!("routing-upgrade-{}.pending.ini", std::process::id()));
    let mut bytes = vec![255, 254];for value in ini.encode_utf16() { bytes.extend(value.to_le_bytes()); }
    let mut file = fs::OpenOptions::new().write(true).create_new(true).open(&path).context("无法创建组件预检查输入")?;
    let input = UpgradeInput { path, bytes };
    file.write_all(&input.bytes)?;file.sync_all()?;drop(file);
    let controller = component(&bundle.root, &bundle.manifest.controller)?;
    let previous = directory.join("clients.ini");
    let mut args = vec![std::ffi::OsStr::new("--upgrade-check"), input.path.as_os_str()];
    if previous.is_file() { args.push(previous.as_os_str()); }
    let result = output(&controller, &args)?;
    ensure!(result.status.success() || result.status.code() == Some(1467), "无法核验已加载的组件，当前接管未更改：{}", String::from_utf8_lossy(&result.stdout).trim());
    let check: UpgradeCheck = json_output(&result)?;
    ensure!(check.schema == "ZSDesk.RoutingUpgrade.v1", "组件预检查协议不兼容");
    let names = check.processes.iter().map(|process| format!("{}（PID {}）", process.application, process.pid)).collect::<Vec<_>>().join("、");
    ensure!(!check.restart_required, "请先关闭 {names}，应用新版接管后再重新打开；当前转发服务和绑定保持不变");
    ensure!(result.status.success(), "无法核验已加载的转发组件，未更改当前接管");
    Ok(())
}
fn apply_settings(profile: &Path, mut settings: Settings) -> Result<String> {
    validate_settings(&settings)?;
    require_selected_applications(&settings)?;
    let document_path = config_file(profile);
    let original_document = if document_path.try_exists()? { Some(bounded_read(&document_path)?) } else { None };
    let bundle = bundled()?;
    let current = std::env::current_exe()?;
    let target_hash = digest(&current)?;
    let import_legacy_choices = import_legacy_choices(&settings);
    if import_legacy_choices {
        for found in discover(&bundle)? {
            if legacy_default_selection(&found) {
                add(&mut settings, found, true)?;
            }
        }
    }
    ensure!(
        settings.applications.iter().any(|a| a.enabled),
        "请先勾选需要接管的应用"
    );
    for app in settings.applications.iter_mut().filter(|a| a.enabled) {
        validate_application(&bundle, app, &target_hash)?;
    }
    let old = installed(&settings)?;
    let directory = old
        .as_ref()
        .and_then(|p| p.parent())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| profile.parent().unwrap_or(Path::new(".")).join("routing"));
    ensure!(directory.is_absolute(), "组件目录无效");
    fs::create_dir_all(&directory)?;
    let controller = directory.join(&bundle.manifest.controller.file);
    let adapter = directory.join(&bundle.manifest.adapter.file);
    let replacing = settings.legacy
        || !adapter.is_file()
        || !digest(&adapter)?.eq_ignore_ascii_case(&bundle.manifest.adapter.sha256);
    if old.is_some() && replacing { preflight_component_replacement(profile, &bundle, &settings, &directory)?; }
    let old_ini = read_ini(&directory.join("package.ini"))?;
    let was_running = old.as_ref().is_some_and(|p| service_running(p));
    let old_start = settings
        .startup_name
        .as_deref()
        .unwrap_or(if settings.legacy {
            LEGACY_STARTUP
        } else {
            STARTUP
        });
    let was_startup = old.as_ref().is_some_and(|p| startup_owned(p, old_start));
    let files = [
        "ShellRoutingControl.exe",
        "ZSDeskRevealRouter.dll",
        "package.ini",
        "clients.ini",
        "routing-startup-backup.bin",
        "MinHook-LICENSE.txt",
    ];
    let saved: Vec<_> = files
        .iter()
        .map(|name| {
            let path = directory.join(name);
            let bytes = if path.exists() {
                ensure!(
                    path.metadata()?.len() <= 64 * 1024 * 1024,
                    "现有路由组件异常过大"
                );
                Some(fs::read(&path)?)
            } else {
                None
            };
            Ok((path, bytes))
        })
        .collect::<Result<_>>()?;
    // Keep one durable recovery set before stopping a working installation.
    let recovery = profile.with_file_name("routing-backup");
    fs::create_dir_all(&recovery)?;
    for (path, data) in &saved {
        if let Some(data) = data {
            atomic(
                &recovery.join(path.file_name().context("组件文件名无效")?),
                data,
            )?;
        }
    }
    let recovery_state = serde_json::json!({"schema":"ZSDesk.RoutingBackup.v1","control_path":old,"running":was_running,"startup":was_startup,
        "files":saved.iter().map(|(p,b)|serde_json::json!({"file":p.file_name(),"present":b.is_some(),"sha256":b.as_ref().map(|d|format!("{:x}",Sha256::digest(d)))})).collect::<Vec<_>>()});
    atomic(
        &recovery.join("manifest.json"),
        &serde_json::to_vec_pretty(&recovery_state)?,
    )?;
    let startup_name = settings
        .startup_name
        .clone()
        .or_else(|| old_ini.get("StartupName").cloned())
        .unwrap_or_else(|| {
            if settings.legacy {
                LEGACY_STARTUP.into()
            } else {
                STARTUP.into()
            }
        });
    let attempt = (|| -> Result<()> {
        if let Some(old) = &old {
            control(old, "--restore")?;
        }
        copy_component(&bundle.root, &bundle.manifest.controller, &controller)?;
        copy_component(&bundle.root, &bundle.manifest.adapter, &adapter)?;
        atomic(
            &directory.join("MinHook-LICENSE.txt"),
            &bounded_read(&bundle.root.join("MinHook-LICENSE.txt"))?,
        )?;
        let mut ini = BTreeMap::new();
        let old_router = old_ini
            .get("RouterFile")
            .map(String::as_str)
            .unwrap_or("FileRevealRouter.dll");
        if old.is_some() && old_router == "FileRevealRouter.dll" {
            if let Some(hash) = old_ini.get("RouterSHA256").filter(|s| valid_hash(s)) {
                let path = directory.join(old_router);
                if path.is_file() && digest(&path)?.eq_ignore_ascii_case(hash) {
                    ini.insert("LegacyRouterFile".into(), old_router.into());
                    ini.insert("LegacyRouterSHA256".into(), hash.clone());
                }
            }
        }
        for name in ["LegacyRouterFile", "LegacyRouterSHA256"] {
            if let Some(value) = old_ini.get(name) {
                ini.insert(name.into(), value.clone());
            }
        }
        ini.extend([
            ("StartupName".into(), startup_name.clone()),
            ("BindingsFile".into(), "clients.ini".into()),
            ("RouterFile".into(), bundle.manifest.adapter.file.clone()),
            (
                "RouterSHA256".into(),
                bundle.manifest.adapter.sha256.clone(),
            ),
            (
                "ControllerSHA256".into(),
                bundle.manifest.controller.sha256.clone(),
            ),
            ("TargetExe".into(), current.to_string_lossy().into_owned()),
            ("TargetSHA256".into(), target_hash.clone()),
            (
                "TargetProfileDir".into(),
                profile
                    .parent()
                    .context("配置目录无效")?
                    .to_string_lossy()
                    .into_owned(),
            ),
            ("TargetProfileToken".into(), token(profile)),
        ]);
        write_clients(&directory.join("clients.ini"), &settings.applications)?;
        write_ini(&directory.join("package.ini"), &ini)?;
        start_watch(&controller)?;
        control(&controller, "--apply-once")?;
        if settings.start_with_windows {
            control(&controller, "--enable-startup")?;
        }
        Ok(())
    })();
    if let Err(error) = attempt {
        let _ = control(&controller, "--restore");
        let mut restore_errors = Vec::new();
        for (path, bytes) in &saved {
            let result = match bytes {
                Some(data) => atomic(path, data),
                None => {
                    if path.exists() {
                        fs::remove_file(path).map_err(anyhow::Error::from)
                    } else {
                        Ok(())
                    }
                }
            };
            if let Err(e) = result {
                restore_errors.push(format!("{}：{e}", path.display()));
            }
        }
        if let Some(old) = &old {
            if was_running {
                if let Err(e) = start_watch(old) {
                    restore_errors.push(e.to_string());
                }
            }
            if was_startup {
                if let Err(e) = control(old, "--enable-startup") {
                    restore_errors.push(e.to_string());
                }
            }
        }
        ensure!(
            restore_errors.is_empty(),
            "启用失败：{error}；部分组件恢复需要处理：{}",
            restore_errors.join("；")
        );
        return Err(error);
    }
    let mut latest = load(profile)?;
    if import_legacy_choices && latest.applications.is_empty() && !latest.pending_changes {
        latest.applications = settings.applications.clone();
    }
    let latest_document = if document_path.try_exists()? { Some(bounded_read(&document_path)?) } else { None };
    merge_after_apply(&mut settings, latest, original_document != latest_document);
    settings.control_path = Some(controller);
    settings.control_sha256 = Some(bundle.manifest.controller.sha256);
    settings.target_exe = Some(current);
    settings.startup_name = Some(startup_name);
    settings.enabled = true;
    settings.legacy = false;
    save(profile, &settings)?;
    Ok("所选应用的文件位置转发已启用；未运行的应用将在启动后接入".into())
}
fn merge_after_apply(settings: &mut Settings, latest: Settings, document_changed: bool) {
    // Installing the new INI changes legacy runtime inference. That is not a
    // user edit, and must not overwrite the choices captured before applying.
    if !document_changed { settings.pending_changes = false; return; }
    merge_latest_choices(settings, latest);
}
fn merge_latest_choices(settings: &mut Settings, latest: Settings) {
    let choices = |apps: &[Application]| {
        apps.iter()
            .map(|a| {
                (
                    a.id,
                    key(&a.executable),
                    key(&a.caller_module),
                    a.profile,
                    a.enabled,
                )
            })
            .collect::<Vec<_>>()
    };
    let same_choices = choices(&latest.applications) == choices(&settings.applications)
        && latest.start_with_windows == settings.start_with_windows;
    if !same_choices {
        settings.applications = latest.applications;
        settings.start_with_windows = latest.start_with_windows;
        settings.pending_changes = true;
    } else {
        settings.pending_changes = false;
    }
    settings.extra = latest.extra;
}
pub fn service_running(controller: &Path) -> bool {
    for name in ["ZSDesk.FileRevealRouter.2", "Native360.FileRevealRouter.1"] {
        let name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let mut previous = None;
        while let Ok(window) =
            unsafe { FindWindowExW(Some(HWND_MESSAGE), previous, PCWSTR(name.as_ptr()), None) }
        {
            previous = Some(window);
            let mut pid = 0;
            unsafe {
                GetWindowThreadProcessId(window, Some(&mut pid));
            }
            let Ok(process) =
                (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) })
            else {
                continue;
            };
            let mut path = vec![0u16; 32768];
            let mut count = path.len() as u32;
            let result = unsafe {
                QueryFullProcessImageNameW(
                    process,
                    PROCESS_NAME_WIN32,
                    PWSTR(path.as_mut_ptr()),
                    &mut count,
                )
            };
            let _ = unsafe { CloseHandle(process) };
            if result.is_ok()
                && key(Path::new(&String::from_utf16_lossy(
                    &path[..count as usize],
                ))) == key(controller)
            {
                return true;
            }
        }
    }
    false
}
fn startup_owned(controller: &Path, name: &str) -> bool {
    let path: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Run"
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    let mut data = vec![0u16; 4096];
    let mut bytes = (data.len() * 2) as u32;
    if unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(path.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(data.as_mut_ptr().cast()),
            Some(&mut bytes),
        )
    }
    .is_err()
    {
        return false;
    }
    let end = data.iter().position(|n| *n == 0).unwrap_or(data.len());
    String::from_utf16_lossy(&data[..end])
        .eq_ignore_ascii_case(&format!("\"{}\" --watch", controller.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unchanged_legacy_document_keeps_applied_startup_choice() {
        let mut applied = Settings { legacy:true, start_with_windows:true, enabled:true, pending_changes:true, ..Default::default() };
        applied.extra.insert("custom".into(), serde_json::json!({"keep":true}));
        let inferred_after_install = Settings::default();
        merge_after_apply(&mut applied, inferred_after_install, false);
        assert!(applied.start_with_windows);
        assert!(!applied.pending_changes);
        assert_eq!(applied.extra["custom"]["keep"], true);
        let changed = Settings { start_with_windows:false, pending_changes:true, ..Default::default() };
        merge_after_apply(&mut applied, changed, true);
        assert!(!applied.start_with_windows);
        assert!(applied.pending_changes);
    }
    #[test]
    fn refresh_resumes_only_an_enabled_owned_missing_watcher() {
        let current = Path::new("C:/Apps/ZSDesk.exe");
        let mut settings = Settings { enabled: true, target_exe: Some(current.into()), ..Default::default() };
        let mut ini = BTreeMap::from([("TargetExe".into(), "c:\\apps\\zsdesk.exe".into())]);
        assert!(resume_owned_watcher(&settings, &ini, current, false));
        assert!(!resume_owned_watcher(&settings, &ini, current, true));
        settings.start_with_windows = true;
        assert!(resume_owned_watcher(&settings, &ini, current, false));
        settings.enabled = false;
        assert!(!resume_owned_watcher(&settings, &ini, current, false));
        settings.enabled = true;
        ini.insert("TargetExe".into(), "C:/Other/ZSDesk.exe".into());
        assert!(!resume_owned_watcher(&settings, &ini, current, false));
        ini.insert("TargetExe".into(), current.to_string_lossy().into_owned());
        settings.target_exe = Some("C:/Other/ZSDesk.exe".into());
        assert!(!resume_owned_watcher(&settings, &ini, current, false));
    }
    #[test]
    fn fresh_or_explicitly_empty_choices_never_auto_select_sources() {
        let mut settings = Settings::default();
        assert!(require_selected_applications(&settings).is_err());
        settings.legacy = true;
        assert!(require_selected_applications(&settings).is_ok());
        settings.pending_changes = true;
        assert!(require_selected_applications(&settings).is_err());
        settings.pending_changes = false;
        settings.applications.push(Application { id:100, label:"reader".into(), executable:"C:/Apps/reader.exe".into(),
            caller_module:"C:/Apps/reader.exe".into(), profile:1, enabled:false, executable_sha256:String::new(), caller_sha256:String::new() });
        assert!(require_selected_applications(&settings).is_err());
        settings.applications[0].enabled = true;
        assert!(require_selected_applications(&settings).is_ok());
    }
    #[test]
    fn explicitly_adding_a_discovered_application_selects_it() {
        let found = || Inspection {
            executable: "C:/apps/reader.exe".into(), caller_module: "C:/apps/reader.exe".into(),
            label: "reader.exe".into(), supported: true, profile: 1, reason: String::new(),
        };
        let mut settings = Settings::default();
        add(&mut settings, found(), false).unwrap();
        assert!(!settings.applications[0].enabled);
        add(&mut settings, found(), true).unwrap();
        assert!(settings.applications[0].enabled);
        add(&mut settings, found(), false).unwrap();
        assert!(settings.applications[0].enabled);
        assert_eq!(settings.applications.len(), 1);
    }
    #[test]
    fn public_defaults_have_no_local_bindings() {
        let value = serde_json::to_value(Settings::default()).unwrap();
        assert_eq!(value["enabled"], false);
        assert_eq!(value["start_with_windows"], false);
        assert_eq!(value["applications"], serde_json::json!([]));
        assert!(value.get("control_path").is_none());
        assert!(value.get("target_exe").is_none());
    }
    #[test]
    fn legacy_paths_and_unknown_settings_are_preserved() {
        let value = serde_json::json!({"control_path":"C:\\tools\\ShellRoutingControl.exe","control_sha256":"a".repeat(64),"target_exe":"C:\\tools\\ZSDesk.exe","custom":{"keep":true}});
        let settings: Settings = serde_json::from_value(value.clone()).unwrap();
        let output = serde_json::to_value(settings).unwrap();
        for name in ["control_path", "target_exe", "custom"] {
            assert_eq!(output[name], value[name]);
        }
    }
    #[test]
    fn package_members_are_fixed_filenames() {
        for file in [
            "../tool.exe",
            "sub/tool.exe",
            "C:/tool.exe",
            "sub\\tool.exe",
        ] {
            assert!(component(
                Path::new("C:/bundle"),
                &Component {
                    file: file.into(),
                    sha256: "a".repeat(64)
                }
            )
            .is_err());
        }
    }
    #[test]
    fn profile_labels_ignore_case_and_separators() {
        assert_eq!(
            profile_property(Path::new("C:/Profile/session.json")),
            profile_property(Path::new("c:\\profile\\session.json"))
        );
        assert_ne!(
            profile_property(Path::new("C:/Profile/session.json")),
            profile_property(Path::new("C:/Other/session.json"))
        );
    }
    #[test]
    fn legacy_defaults_do_not_opt_in_other_chromium_apps() {
        let make = |path: &str| Inspection {
            executable: path.into(),
            caller_module: "C:/apps/chrome.dll".into(),
            label: "browser.exe".into(),
            supported: true,
            profile: 2,
            reason: String::new(),
        };
        assert!(legacy_default_selection(&make(
            "C:/apps/OpenAI.Codex_test/app/ChatGPT.exe"
        )));
        assert!(!legacy_default_selection(&make(
            "C:/apps/other-browser/browser.exe"
        )));
    }
    #[test]
    fn validation_metadata_does_not_overwrite_newer_choices_or_extra_values() {
        let app = Application {
            id: 100,
            label: "app".into(),
            executable: "C:/app/app.exe".into(),
            caller_module: "C:/app/app.exe".into(),
            profile: 1,
            enabled: true,
            executable_sha256: "a".repeat(64),
            caller_sha256: "b".repeat(64),
        };
        let mut applied = Settings {
            applications: vec![app.clone()],
            ..Default::default()
        };
        let mut latest = applied.clone();
        latest.applications[0].executable_sha256.clear();
        latest.extra.insert("custom".into(), serde_json::json!(2));
        merge_latest_choices(&mut applied, latest);
        assert!(!applied.pending_changes);
        assert_eq!(applied.applications[0].executable_sha256, "a".repeat(64));
        assert_eq!(applied.extra["custom"], serde_json::json!(2));
        merge_latest_choices(
            &mut applied,
            Settings {
                pending_changes: true,
                ..Default::default()
            },
        );
        assert!(applied.applications.is_empty() && applied.pending_changes);
    }
}
