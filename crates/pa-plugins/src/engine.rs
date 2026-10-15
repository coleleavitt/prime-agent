//! The process-wide wasmtime engine, its epoch ticker, and the compiled-plugin
//! cache. Compiling a component is the expensive step (cranelift), so each
//! plugin file compiles once per content and every session instantiates the
//! cached [`PluginPre`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, SystemTime};

use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Config, Engine};

use crate::bindings::{Plugin, PluginPre};
use crate::state::State;

/// One epoch tick: the granularity of every call deadline.
pub(crate) const EPOCH_TICK: Duration = Duration::from_millis(10);

/// The engine every plugin shares, with its ticker started. `Err` when the
/// engine cannot be configured on this machine (reported once per load).
pub(crate) fn engine() -> Result<&'static Engine, String> {
    static ENGINE: OnceLock<Result<Engine, String>> = OnceLock::new();
    ENGINE
        .get_or_init(|| {
            let mut config = Config::new();
            config.wasm_component_model(true);
            // Every call runs under a deadline: a plugin that loops forever
            // traps at the next epoch check past it.
            config.epoch_interruption(true);
            let engine = Engine::new(&config).map_err(|error| format!("{error:#}"))?;
            let ticker = engine.clone();
            std::thread::Builder::new()
                .name("pa-plugins-epoch".to_string())
                .spawn(move || {
                    loop {
                        std::thread::sleep(EPOCH_TICK);
                        ticker.increment_epoch();
                    }
                })
                .map_err(|error| format!("cannot start the plugin epoch ticker: {error}"))?;
            Ok(engine)
        })
        .as_ref()
        .map_err(Clone::clone)
}

/// The linker every plugin instantiates against: WASI preview 2 (whose
/// context starts deny-all, see [`crate::state`]) and the `host` interface.
fn linker(engine: &Engine) -> Result<Linker<State>, String> {
    let mut linker = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker).map_err(|error| format!("{error:#}"))?;
    Plugin::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)
        .map_err(|error| format!("{error:#}"))?;
    Ok(linker)
}

/// What identifies one compiled plugin file: a changed file recompiles.
#[derive(Clone, PartialEq, Eq)]
struct FileStamp {
    modified: Option<SystemTime>,
    len: u64,
}

fn stamp(path: &Path) -> std::io::Result<FileStamp> {
    let metadata = std::fs::metadata(path)?;
    Ok(FileStamp {
        modified: metadata.modified().ok(),
        len: metadata.len(),
    })
}

type Cache = Mutex<HashMap<PathBuf, (FileStamp, Arc<PluginPre<State>>)>>;

fn cache() -> &'static Cache {
    static CACHE: OnceLock<Cache> = OnceLock::new();
    CACHE.get_or_init(Cache::default)
}

/// The pre-linked plugin for `path`, compiled on first use and again
/// whenever the file changes. Blocking (compiles): call it off the async
/// runtime.
pub(crate) fn prepare(path: &Path) -> Result<Arc<PluginPre<State>>, String> {
    let engine = engine()?;
    let current =
        stamp(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    if let Some((cached, pre)) = cache()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(path)
    {
        if *cached == current {
            return Ok(Arc::clone(pre));
        }
    }
    let component = Component::from_file(engine, path).map_err(|error| format!("{error:#}"))?;
    let pre = prepare_component(engine, &component)?;
    cache()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(path.to_path_buf(), (current, Arc::clone(&pre)));
    Ok(pre)
}

/// Link one compiled component against the plugin world.
pub(crate) fn prepare_component(
    engine: &Engine,
    component: &Component,
) -> Result<Arc<PluginPre<State>>, String> {
    let linker = linker(engine)?;
    let instance = linker
        .instantiate_pre(component)
        .map_err(|error| format!("{error:#}"))?;
    let pre = PluginPre::new(instance).map_err(|error| format!("{error:#}"))?;
    Ok(Arc::new(pre))
}
