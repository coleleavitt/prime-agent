//! One plugin instance's store data: its WASI context (deny-by-default plus
//! the manifest's grants), its resource limits, and the `host` interface it
//! imports.

use serde_json::json;
use wasmtime::component::ResourceTable;
use wasmtime::{StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::bindings::prime_agent::plugin::host::{Host, Level};
use crate::manifest::Manifest;

/// The store data of one plugin instance in one session.
pub struct State {
    wasi: WasiCtx,
    table: ResourceTable,
    pub(crate) limits: StoreLimits,
    /// The plugin's name: the file stem until `metadata` answers.
    pub(crate) plugin: String,
    /// The session this instance serves; status for any other is refused.
    session_id: String,
}

/// The status feature name a plugin publishes under.
#[must_use]
pub fn status_feature(plugin: &str) -> String {
    format!("plugin:{plugin}")
}

impl State {
    /// The sandbox for one instance: WASI starts deny-all (no preopens, no
    /// environment, no arguments, stdin closed, no network addresses, no
    /// name lookup); the manifest may add directories and named host
    /// environment variables, never network access.
    ///
    /// # Errors
    ///
    /// A granted directory cannot be opened.
    pub fn new(plugin: &str, session_id: &str, manifest: &Manifest) -> Result<Self, String> {
        let mut wasi = WasiCtxBuilder::new();
        for name in &manifest.env {
            if let Ok(value) = std::env::var(name) {
                wasi.env(name, value);
            }
        }
        for grant in &manifest.dirs {
            let perms = if grant.write {
                FsPerms::ReadWrite
            } else {
                FsPerms::ReadOnly
            };
            wasi.preopened_dir(&grant.host, &grant.guest, perms)
                .map_err(|error| {
                    format!(
                        "cannot grant {} as {}: {error:#}",
                        grant.host.display(),
                        grant.guest
                    )
                })?;
        }
        Ok(Self {
            wasi: wasi.build(),
            table: ResourceTable::new(),
            limits: StoreLimitsBuilder::new()
                .memory_size(manifest.memory_bytes)
                .instances(64)
                .tables(64)
                .memories(16)
                .build(),
            plugin: plugin.to_string(),
            session_id: session_id.to_string(),
        })
    }
}

impl WasiView for State {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl crate::bindings::prime_agent::plugin::types::Host for State {}

impl Host for State {
    fn log(&mut self, level: Level, message: String) {
        let plugin = self.plugin.as_str();
        match level {
            Level::Debug => tracing::debug!(target: "pa_plugins", plugin, "{message}"),
            Level::Info => tracing::info!(target: "pa_plugins", plugin, "{message}"),
            Level::Warn => tracing::warn!(target: "pa_plugins", plugin, "{message}"),
            Level::Error => tracing::error!(target: "pa_plugins", plugin, "{message}"),
        }
    }

    fn set_status(&mut self, session_id: String, line: Option<String>, panel: Vec<String>) {
        if session_id != self.session_id {
            tracing::warn!(
                target: "pa_plugins",
                plugin = self.plugin.as_str(),
                "set-status for session {session_id} refused: this instance serves {}",
                self.session_id
            );
            return;
        }
        pa_core::features::publish_feature_status(
            &session_id,
            pa_core::features::FeatureStatus {
                feature: status_feature(&self.plugin),
                line,
                status: json!({ "panel": panel }),
            },
        );
    }
}
