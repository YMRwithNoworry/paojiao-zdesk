# WASM 插件包

将每个插件的 `*.json` 清单与同目录 `*.wasm` 模块放入 ZSDesk 配置目录下的 `plugins` 文件夹。桌面格子宿主启动时读取清单并预编译模块；右键路径只读取缓存的 action 描述，不执行 WASM。默认模式把 action 插入完整 Shell 菜单；可选“ZSUI 即时右键菜单”将缓存 action 显示在首屏，并把完整 Shell 菜单延迟到“完整 Windows Shell 菜单…”命令。卸载插件即删除其清单和模块并重启宿主。

清单格式：

```json
{
  "id": "sample.echo",
  "name": "Echo",
  "wasm": "echo.wasm",
  "actions": [{ "id": "show", "label": "处理所选项目" }]
}
```

Rust 插件须编译为 `wasm32-unknown-unknown`，不导入 WASI，并导出：

- `memory`：线性内存，最大 16 MiB；
- `alloc(i32) -> i32`：分配输入缓冲区，负数表示失败；
- `invoke(i32, i32) -> i32`：读取 UTF-8 JSON `{ "action": "...", "context": { "selected": [...] } }`；返回 0 表示成功。

唯一允许的导入是 `zsdesk.notify(ptr, len)`，参数是 UTF-8 消息，最多 1024 字节；调用后消息由宿主显示。Rust guest 可用 `#[link(wasm_import_module = "zsdesk")] extern "C" { fn notify(ptr: i32, len: i32); }` 声明。

每次调用有 2,000,000 fuel、16 MiB 内存及 64 KiB 输入上限。插件只获得所选项目的标识；无文件系统、网络或进程能力。插件运行失败不会阻止原生 Shell 菜单。

默认模式保留全部 Windows Shell 动词、第三方扩展及其原生菜单处理。ZSUI 即时模式首屏不查询 Shell；通过“完整 Windows Shell 菜单…”可打开同一完整 `IContextMenu` 菜单，因此不丢 Shell 功能。第三方扩展的 `QueryContextMenu` 可同步执行任意代码，打开完整 Shell 菜单时仍可能延迟；无法承诺该路径绝对零延迟。
