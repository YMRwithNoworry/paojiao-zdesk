# WASM 插件包

将每个插件的 `*.json` 清单与同目录 `*.wasm` 模块放入 ZSDesk 配置目录下的 `plugins` 文件夹。桌面格子宿主启动时读取清单并预编译模块；右键菜单只读取已缓存的 action 列表。卸载插件即删除其清单和模块并重启宿主。

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

所有 Windows Shell 动词和已注册扩展仍由 Windows `IContextMenu` 处理，ZSDesk 在其顶部加入 WASM action。第三方 Shell 扩展的 `QueryContextMenu` 可同步执行任意代码，因此 Windows 原生菜单本身不能保证零延迟；插件清单读取、WASM 编译和菜单项生成不在右键路径。
