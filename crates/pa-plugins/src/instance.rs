//! One plugin in one session: its store and instance, and the deadline every
//! call runs under. A trap (including a missed deadline or the memory cap)
//! drops the instance; the next call instantiates a fresh one, which sees
//! `on-session-start` again.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use wasmtime::Store;

use crate::bindings::prime_agent::plugin::types::SessionInfo;
use crate::bindings::{Plugin, PluginPre};
use crate::engine::{EPOCH_TICK, engine};
use crate::manifest::Manifest;
use crate::state::State;

struct Live {
    store: Store<State>,
    plugin: Plugin,
}

/// A loaded plugin serving one session.
pub struct SessionPlugin {
    name: String,
    description: String,
    path: PathBuf,
    manifest: Manifest,
    pre: Arc<PluginPre<State>>,
    session: SessionInfo,
    live: Mutex<Option<Live>>,
}

impl SessionPlugin {
    /// Instantiate the plugin for `session`, read its metadata, and deliver
    /// `on-session-start`. Blocking: run it off the async runtime.
    ///
    /// # Errors
    ///
    /// The manifest, sandbox, instantiation, `metadata`, or `on-session-start`
    /// failed.
    pub fn load(
        path: &Path,
        pre: Arc<PluginPre<State>>,
        session: SessionInfo,
    ) -> Result<Self, String> {
        let manifest = Manifest::for_plugin(path)?;
        let stem = path.file_stem().map_or_else(
            || "plugin".to_string(),
            |stem| stem.to_string_lossy().into_owned(),
        );
        let mut plugin = Self {
            name: stem,
            description: String::new(),
            path: path.to_path_buf(),
            manifest,
            pre,
            session,
            live: Mutex::new(None),
        };
        let mut live = plugin.instantiate()?;
        let (name, description) =
            plugin.within_deadline(&mut live, |plugin, store| plugin.call_metadata(store))?;
        if !name.trim().is_empty() {
            plugin.name = name.trim().to_string();
            live.store.data_mut().plugin.clone_from(&plugin.name);
        }
        plugin.description = description;
        plugin.start(&mut live)?;
        *plugin.live.lock().unwrap_or_else(PoisonError::into_inner) = Some(live);
        Ok(plugin)
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session.session_id
    }

    fn instantiate(&self) -> Result<Live, String> {
        let engine = engine()?;
        let state = State::new(&self.name, &self.session.session_id, &self.manifest)?;
        let mut store = Store::new(engine, state);
        store.limiter(|state| &mut state.limits);
        store.set_epoch_deadline(self.deadline_ticks());
        let plugin = self
            .pre
            .instantiate(&mut store)
            .map_err(|error| format!("cannot instantiate {}: {error:#}", self.path.display()))?;
        Ok(Live { store, plugin })
    }

    fn start(&self, live: &mut Live) -> Result<(), String> {
        let session = self.session.clone();
        self.within_deadline(live, |plugin, store| {
            plugin.call_on_session_start(store, &session)
        })
    }

    fn deadline_ticks(&self) -> u64 {
        let tick = u64::try_from(EPOCH_TICK.as_millis()).unwrap_or(1).max(1);
        self.manifest.timeout_ms.div_ceil(tick).max(1)
    }

    fn within_deadline<R>(
        &self,
        live: &mut Live,
        call: impl FnOnce(&Plugin, &mut Store<State>) -> wasmtime::Result<R>,
    ) -> Result<R, String> {
        live.store.set_epoch_deadline(self.deadline_ticks());
        call(&live.plugin, &mut live.store).map_err(|error| format!("{error:#}"))
    }

    /// Run one export under the deadline. Blocking: run it off the async
    /// runtime.
    ///
    /// # Errors
    ///
    /// The plugin trapped, ran past its deadline, hit its memory cap, or
    /// could not be re-instantiated; the instance is dropped.
    pub fn call<R>(
        &self,
        call: impl FnOnce(&Plugin, &mut Store<State>) -> wasmtime::Result<R>,
    ) -> Result<R, String> {
        let mut slot = self.live.lock().unwrap_or_else(PoisonError::into_inner);
        let mut live = if let Some(live) = slot.take() {
            live
        } else {
            let mut live = self.instantiate()?;
            self.start(&mut live)?;
            live
        };
        let result = self.within_deadline(&mut live, call);
        if result.is_ok() {
            *slot = Some(live);
        }
        result.map_err(|error| format!("plugin {} failed: {error}", self.name))
    }
}
