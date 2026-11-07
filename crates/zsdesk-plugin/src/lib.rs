//! Sandboxed WebAssembly extensions. Manifests are read and modules compiled at
//! startup; the menu hot path only reads the immutable action list.
use serde::Deserialize;
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};
use wasmi::{
    core::Trap, Caller, Config, Engine, Extern, Func, Linker, Module, Store, StoreLimits,
    StoreLimitsBuilder,
};

const MAX_MANIFEST: u64 = 64 * 1024;
const MAX_MODULE: u64 = 8 * 1024 * 1024;
const MAX_CONTEXT: usize = 64 * 1024;
const MAX_MEMORY: usize = 16 * 1024 * 1024;
const FUEL: u64 = 2_000_000;
const MAX_PLUGINS: usize = 16;
const MAX_ACTIONS_PER_PLUGIN: usize = 8;
const MAX_TOTAL_ACTIONS: usize = 64;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    id: String,
    name: String,
    wasm: String,
    actions: Vec<Action>,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Action {
    id: String,
    label: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginAction {
    pub plugin_id: String,
    pub id: String,
    pub label: String,
}
struct Loaded {
    manifest: Manifest,
    module: Module,
}
pub struct PluginManager {
    engine: Engine,
    loaded: Vec<Loaded>,
    actions: Vec<PluginAction>,
}
struct HostState {
    limits: StoreLimits,
    message: Option<String>,
}

impl PluginManager {
    /// Loads valid `*.json` manifests and sibling modules. Bad packages are skipped.
    pub fn load(dir: &Path) -> Self {
        let mut config = Config::default();
        config.consume_fuel(true);
        let engine = Engine::new(&config);
        let mut loaded = Vec::new();
        let mut actions = Vec::new();
        let Ok(entries) = fs::read_dir(dir) else {
            return Self {
                engine,
                loaded,
                actions,
            };
        };
        let mut files: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("json"))
            })
            .take(MAX_PLUGINS * 4)
            .collect();
        files.sort();
        for file in files {
            if loaded.len() >= MAX_PLUGINS || actions.len() >= MAX_TOTAL_ACTIONS {
                break;
            }
            let Ok(meta) = fs::metadata(&file) else {
                continue;
            };
            if !meta.is_file() || meta.len() > MAX_MANIFEST {
                continue;
            }
            let Some(bytes) = read_limited(&file, MAX_MANIFEST) else {
                continue;
            };
            let Ok(manifest) = serde_json::from_slice::<Manifest>(&bytes) else {
                continue;
            };
            if !valid_id(&manifest.id)
                || manifest.name.trim().is_empty()
                || manifest.name.len() > 80
                || manifest.actions.is_empty()
                || manifest.actions.len() > MAX_ACTIONS_PER_PLUGIN
            {
                continue;
            }
            let wasm_name = Path::new(&manifest.wasm);
            if wasm_name.components().count() != 1
                || wasm_name.file_name().is_none()
                || wasm_name
                    .extension()
                    .is_none_or(|e| !e.eq_ignore_ascii_case("wasm"))
            {
                continue;
            }
            let wasm_path = file.with_file_name(wasm_name);
            let Ok(meta) = fs::metadata(&wasm_path) else {
                continue;
            };
            if !meta.is_file() || meta.len() == 0 || meta.len() > MAX_MODULE {
                continue;
            }
            let Some(wasm) = read_limited(&wasm_path, MAX_MODULE) else {
                continue;
            };
            let Ok(module) = Module::new(&engine, &mut &wasm[..]) else {
                continue;
            };
            if module
                .imports()
                .any(|i| i.module() != "zsdesk" || i.name() != "notify")
            {
                continue;
            }
            let mut ids = std::collections::HashSet::new();
            if manifest.actions.iter().any(|a| {
                !valid_id(&a.id)
                    || a.label.trim().is_empty()
                    || a.label.len() > 80
                    || !ids.insert(a.id.clone())
            }) {
                continue;
            }
            if loaded.iter().any(|p: &Loaded| p.manifest.id == manifest.id) {
                continue;
            }
            actions.extend(
                manifest
                    .actions
                    .iter()
                    .take(MAX_TOTAL_ACTIONS - actions.len())
                    .map(|a| PluginAction {
                        plugin_id: manifest.id.clone(),
                        id: a.id.clone(),
                        label: a.label.clone(),
                    }),
            );
            loaded.push(Loaded { manifest, module });
        }
        Self {
            engine,
            loaded,
            actions,
        }
    }

    pub fn actions(&self) -> &[PluginAction] {
        &self.actions
    }

    /// Runs a declared action with bounded fuel/memory. Only `zsdesk.notify` is imported.
    pub fn invoke(
        &self,
        plugin_id: &str,
        action_id: &str,
        context_json: &str,
    ) -> Result<Option<String>, String> {
        if context_json.len() > MAX_CONTEXT {
            return Err("plugin context too large".into());
        }
        let plugin = self
            .loaded
            .iter()
            .find(|p| p.manifest.id == plugin_id)
            .ok_or("plugin not found")?;
        if !plugin.manifest.actions.iter().any(|a| a.id == action_id) {
            return Err("action not declared".into());
        }
        let payload = serde_json::to_vec(&serde_json::json!({"action": action_id, "context": serde_json::from_str::<serde_json::Value>(context_json).map_err(|e| e.to_string())?})).map_err(|e| e.to_string())?;
        let limits = StoreLimitsBuilder::new()
            .memory_size(MAX_MEMORY)
            .instances(1)
            .memories(1)
            .tables(1)
            .build();
        let mut store = Store::new(
            &self.engine,
            HostState {
                limits,
                message: None,
            },
        );
        store.limiter(|state| &mut state.limits);
        store.add_fuel(FUEL).map_err(|e| e.to_string())?;
        let notify = Func::wrap(
            &mut store,
            |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| -> Result<(), Trap> {
                if ptr < 0 || len < 0 || len > 1024 {
                    return Err(Trap::new("invalid notification range"));
                }
                let Some(Extern::Memory(memory)) = caller.get_export("memory") else {
                    return Err(Trap::new("plugin memory export missing"));
                };
                let mut bytes = vec![0; len as usize];
                memory
                    .read(&caller, ptr as usize, &mut bytes)
                    .map_err(|_| Trap::new("invalid notification range"))?;
                let text =
                    String::from_utf8(bytes).map_err(|_| Trap::new("notification is not UTF-8"))?;
                caller.data_mut().message = Some(text);
                Ok(())
            },
        );
        let mut linker = Linker::<HostState>::new(&self.engine);
        linker
            .define("zsdesk", "notify", notify)
            .map_err(|e| e.to_string())?;
        let instance = linker
            .instantiate(&mut store, &plugin.module)
            .map_err(|e| e.to_string())?
            .start(&mut store)
            .map_err(|e| e.to_string())?;
        let memory = instance
            .get_memory(&store, "memory")
            .ok_or("plugin memory export missing")?;
        let alloc = instance
            .get_typed_func::<i32, i32>(&store, "alloc")
            .map_err(|e| e.to_string())?;
        let invoke = instance
            .get_typed_func::<(i32, i32), i32>(&store, "invoke")
            .map_err(|e| e.to_string())?;
        let ptr = alloc
            .call(
                &mut store,
                i32::try_from(payload.len()).map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?;
        if ptr < 0 {
            return Err("plugin returned invalid buffer".into());
        }
        memory
            .write(&mut store, ptr as usize, &payload)
            .map_err(|e| e.to_string())?;
        let status = invoke
            .call(&mut store, (ptr, payload.len() as i32))
            .map_err(|e| e.to_string())?;
        if status == 0 {
            Ok(store.data().message.clone())
        } else {
            Err(format!("plugin action failed ({status})"))
        }
    }
}

fn read_limited(path: &Path, limit: u64) -> Option<Vec<u8>> {
    let mut file = fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.by_ref().take(limit + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() as u64 <= limit).then_some(bytes)
}

fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invokes_a_wasm_action_with_json_context() {
        let dir = std::env::temp_dir().join(format!("zsdesk-plugin-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let wasm = wat::parse_str(r#"(module (import "zsdesk" "notify" (func $notify (param i32 i32))) (memory (export "memory") 1 1) (data (i32.const 64) "hello") (global $p (mut i32) (i32.const 1024)) (func (export "alloc") (param i32) (result i32) (global.get $p)) (func (export "invoke") (param i32 i32) (result i32) (call $notify (i32.const 64) (i32.const 5)) (i32.const 0)))"#).unwrap();
        fs::write(dir.join("demo.wasm"), wasm).unwrap();
        fs::write(dir.join("demo.json"), r#"{"id":"demo","name":"Demo","wasm":"demo.wasm","actions":[{"id":"run","label":"Run"}]}"#).unwrap();
        let manager = PluginManager::load(&dir);
        assert_eq!(
            manager.invoke("demo", "run", r#"{"paths":[]}"#).unwrap(),
            Some("hello".into())
        );
        assert!(manager.invoke("demo", "other", "{}").is_err());
        drop(manager);
        let loop_wasm = wat::parse_str(r#"(module (import "zsdesk" "notify" (func $notify (param i32 i32))) (memory (export "memory") 1 1) (func (export "alloc") (param i32) (result i32) (i32.const 1024)) (func (export "invoke") (param i32 i32) (result i32) (loop $again (br $again)) (i32.const 0)))"#).unwrap();
        fs::write(dir.join("demo.wasm"), loop_wasm).unwrap();
        let manager = PluginManager::load(&dir);
        assert!(manager.invoke("demo", "run", "{}").is_err());
        drop(manager);
        let imported =
            wat::parse_str(r#"(module (import "wasi_snapshot_preview1" "fd_write" (func)))"#)
                .unwrap();
        fs::write(dir.join("demo.wasm"), imported).unwrap();
        assert!(PluginManager::load(&dir).actions().is_empty());
        let _ = fs::remove_dir_all(dir);
    }
}
