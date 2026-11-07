# ZSDesk

Windows 原生多标签文件管理器与桌面格子，使用 Rust、Win32/COM、Direct2D 和 DirectWrite。

界面的语义配色、图标和菜单模型来自作者的 Rust 原生桌面 UI 框架 [ZSUI](https://github.com/qiu7824/zsui)；
窗口外壳、绘制、输入和 Shell 集成由 ZSDesk 直接调用 Win32 与 Direct2D 实现。

[下载](https://github.com/qiu7824/zsdesk/releases) · [问题反馈](https://github.com/qiu7824/zsdesk/issues)

## 版本

| 版本 | Cargo features | 功能 |
|---|---|---|
| 标准版 | 无 | 文件管理器、搜索、预览、界面设置和系统集成 |
| 网络版 | `network` | 标准版 + FTP、SFTP/SSH 地址、网络驱动器和网络位置 |
| 桌面版 | `desktop` | 标准版 + 桌面格子与腾讯布局导入 |
| 全量版 | `network,desktop` | 全部功能，默认构建版本 |

每个版本提供 Windows x64 便携 ZIP 和安装器，文件名为
`ZSDesk-<版本>-<standard|network|desktop|full>-x64`。
安装器默认按当前用户安装，保留个人数据，文件夹接管和登录启动均由软件内设置。
每次向任意分支推送都会自动测试、打包四个版本，并创建 GitHub Release；Release notes 由 GitHub 根据提交自动生成。
自动构建标签格式为 `v<版本>-build.<运行号>.<重跑次数>`。Pull request 运行测试和构建检查，不发布 Release。

## 文件管理

- 多标签、收藏栏、地址面包屑和系统快速访问。
- 左侧使用原生 Shell 导航树，支持展开、收起、固定位置和系统虚拟文件夹。
- 文件列表使用 `IExplorerBrowser`，文件操作、拖放、冲突处理和进度由 Windows Shell 执行。
- 文件夹、回收站、此电脑、网络及控制面板使用系统显示名称。
- 界面遵循系统缩放；Windows 11 22H2 及以上支持 Mica 和系统窗口按钮。
- 地址栏也可输入任意终端命令及参数（如 `pwsh`、`nu`、`wt`、`cmd`）启动终端；在文件夹中启动时以该文件夹为工作目录，目录路径仍用于导航，参数遵循 Windows 命令行引号规则。

“≡ → 界面设置”可控制整排命令栏以及新建、剪切、复制、粘贴、重命名、共享、删除、
排序、查看、更多和预览的显示。隐藏项目后自动重排，窄窗口使用紧凑布局。

| 快捷键 | 功能 |
|---|---|
| Ctrl+T / Ctrl+W | 新建 / 关闭标签 |
| Ctrl+L / Ctrl+F | 地址输入 / 快速搜索 |
| Ctrl+C / Ctrl+X / Ctrl+V | 复制 / 剪切 / 粘贴文件 |
| Ctrl+Z / Ctrl+Y | 撤销 / 重做 Windows Shell 支持的文件操作 |
| Ctrl+A | 全选文件 |
| F2 / Delete / Shift+Delete | 重命名 / 删除 / 永久删除 |
| F5 | 刷新当前目录或搜索 |
| Alt+P / 空格 | 预览窗格 / 快速预览 |

输入框和文件重命名编辑框保留文本编辑快捷键；隐藏右键菜单项目不会禁用文件快捷键。
普通删除遵循系统回收站规则；永久删除、不可回收位置及系统未记录的操作不能撤销。

## 搜索与预览

快速搜索默认查找本地固定磁盘中的文件名，也可在界面设置中切换到当前目录及子目录。
全盘搜索使用 Rust 后台扫描与持久化 SQLite 索引；首次建立索引，后续查询复用数据。
F5 更新变动，主菜单可重建索引。

扫描不读取文件内容，不跟随目录链接；网络位置、移动盘和无读取权限的目录不进入该索引。
单次最多展示 2000 项并提示细化关键词。索引数据库与结果队列有容量限制。
当前实现不使用 Everything，也未实现 MFT/USN 实时索引或拼音匹配。

右侧预览窗格使用系统 Shell 预览能力；空格预览优先使用进程外预览处理器，
支持已注册的 PDF/Office 处理器，并提供文本和图片回退。
文本、图片尺寸及保留图像有边界，关闭预览释放文件流和显示资源。

## 右键菜单

“≡ → 右键菜单设置”可选择原版或 Windows 11 样式，并分别控制常用命令、7-Zip 与 WPS。
Win11 菜单及“显示更多选项”使用同一套功能开关；格子菜单也读取这些设置。
7-Zip/WPS 菜单依赖本机已注册的扩展。

关闭的 Win11 扩展在加载前被跳过。原生菜单兼容层校验系统组件版本，
未支持的系统使用原版菜单；过滤失败时保留隐藏设置并显示错误状态。
这些设置仅作用于 ZSDesk 内的菜单。

## 桌面格子

桌面版和全量版可从主菜单启动格子，或执行 `ZSDesk.exe --desktop`。

- 文件夹映射、桌面项目分组、手动排序、大小和屏幕位置。
- 悬停临时展开，移开收起；双击固定展开，支持锁定和拖动。
- 图标/列表视图、主题、透明度、托盘及桌面空白双击开关。
- 标题文件数量默认隐藏，可在格子“外观”设置中启用。
- 一键整理使用项目引用；删除格子不会删除文件。
- 整理模式配有恢复进程，异常退出时恢复系统桌面图标。
- 图标请求、冷缓存与绘图资源按可见状态回收。

腾讯桌面布局迁移只读取本机布局文件，保留分组、成员顺序和屏幕布局，
不移动原文件、不修改腾讯配置，也不分发腾讯程序或图形资源。

```powershell
# 预览迁移报告；“-”表示自动查找本机腾讯布局。
ZSDesk.exe --import-tencent - --import-report "C:\Reports\layout.json"

# 退出 ZSDesk 格子后应用迁移；原配置保留为 desktop-fences.before-tencent.json。
ZSDesk.exe --import-tencent - --import-report "C:\Reports\layout.json" --apply-import
```

## 文件夹与应用接入

“≡ → 系统集成”提供不接管、增加右键入口、默认打开文件夹和磁盘，以及按备份恢复。
操作仅涉及当前用户注册表；系统虚拟文件夹保持原有默认处理方式。

“系统集成 → 文件位置转发”可以发现和选择 Codex、微信及其他兼容的 x64 应用，
也可手动添加程序，然后一键启用所选应用。登录启动单独设置，默认关闭。
仅调用受支持 Windows 文件定位接口的应用可接入；程序身份和调用模块在本地校验。
应用升级或转发组件被占用时会提示重新发现或重启相应应用。

组件由源码构建，随安装包放在 `routing/`。默认不带应用绑定或个人路径；
启用时才在当前配置目录生成绑定。已有旧版配置可保留兼容转发。
多个配置目录使用独立窗口标识，避免将文件定位请求送到另一实例。
组件和配置说明见 [native/routing/README.md](native/routing/README.md)。

网络版和全量版支持系统 FTP、SMB/UNC，以及映射/断开网络驱动器、添加网络位置。
`sftp://` 和 `ssh://` 地址需要另行安装 SSHFS-Win 与 WinFsp。

## 数据

默认目录为 `%APPDATA%\ZSDesk`。程序旁的 `portable.flag` 使用相邻 `data/` 目录，
`--profile-dir` 可指定独立配置目录。

桌面布局、会话、搜索索引、应用绑定和系统接管备份仅保存于本机，
不包含在源代码、默认配置或发行包中。卸载保留个人数据，并仅恢复仍由当前安装拥有的系统集成。

## 功能边界

- 文件列表、文件操作和虚拟目录复用 Windows Shell；本程序不替换系统任务栏、Shell 服务或其他应用的打开/保存对话框。
- 标签页快照保存视图类型、图标大小、焦点、滚动位置及最多 256 项选择（PIDL 总量 128 KiB）；列宽、排序列和分组规则未纳入完整的标签恢复。
- 全盘搜索是文件名索引，不包含内容检索、MFT/USN 实时更新、移动盘或网络盘索引。
- 应用接入覆盖可识别的文件定位接口，不承诺接管每个软件的全部打开方式；SFTP/SSH 依赖 SSHFS-Win 与 WinFsp。
- Windows 11 原生菜单及预览能力受系统版本和已安装扩展影响，未支持的菜单组件使用原版表现形式。

## 构建

需要 Windows x64、Rust MSVC 工具链、Visual Studio C++ 工具与 Windows SDK；
验证工具链为 Rust 1.99.0。打包使用 PowerShell 7，安装器需要 Inno Setup 6.3 或更新版本。
VC Runtime 从 Visual Studio x64 Redist 目录获取。

ZSUI 关闭默认特性，仅提供主题、图标和菜单模型。其固定源码版本记录于
`packaging/zsui-revision.txt`，位于本仓库的同级 `zsui/` 目录。

```powershell
git clone https://github.com/qiu7824/zsdesk.git
git clone https://github.com/qiu7824/zsui.git
git -C zsui checkout (Get-Content zsdesk/packaging/zsui-revision.txt -Raw).Trim()
Set-Location zsdesk

$env:CARGO_TARGET_DIR = 'C:\Build\ZSDesk-target'
$env:CARGO_PROFILE_DEV_DEBUG = '0'
$env:CARGO_PROFILE_TEST_DEBUG = '0'
cargo test --workspace --release --locked
cargo build --release --locked -p zsdesk-app

# 可选功能版本
cargo build --release --locked -p zsdesk-app --no-default-features
cargo build --release --locked -p zsdesk-app --no-default-features --features network
cargo build --release --locked -p zsdesk-app --no-default-features --features desktop

# 转发组件与四个发行版本
./native/routing/build.ps1 -OutputDir 'C:\Build\ZSDesk-routing' -BuildDir 'C:\Build\ZSDesk-routing-build'
./scripts/package.ps1 -WorkDir 'C:\Build\ZSDesk-package' -TargetDir $env:CARGO_TARGET_DIR `
    -RoutingDir 'C:\Build\ZSDesk-routing' -InnoCompiler 'C:\Program Files (x86)\Inno Setup 6\ISCC.exe'
```

打包器验证程序版本与 feature 身份，生成文件哈希清单，移除本机编译路径信息；
已有发行目录不会被静默覆盖。`-Editions full,desktop` 可选择部分版本，
`-RuntimeDir` 可指定 Visual Studio Redist 下的 CRT 目录。

项目代码使用 [MIT](LICENSE)；ZSUI 使用 MPL-2.0，MinHook 使用 BSD-2-Clause。
其余组件声明见 [packaging/licenses/NOTICE.txt](packaging/licenses/NOTICE.txt)。
