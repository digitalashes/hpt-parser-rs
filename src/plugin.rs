//! WASM plugins run by extism: no WASI, no network (this build has no HTTP), no files. Each
//! function takes JSON and answers JSON.

use crate::Error;
use crate::config::{Config, Source, Transform, plugin_error};
use extism::{Manifest, Plugin, Wasm};
use serde_json::value::RawValue;
use std::collections::HashMap;
use std::time::Duration;

/// ponytail: fixed limits for every plugin; a `[[plugin]]` key each once one needs more.
const TIMEOUT: Duration = Duration::from_secs(60);
/// 64 KiB pages: 256 MiB.
const MEMORY_PAGES: u32 = 4096;

/// The config's plugins, loaded, by name.
pub(crate) struct Plugins(HashMap<String, Plugin>);

impl Plugins {
    /// Loads every `[[plugin]]`, its `config` table as extism config (values other than
    /// strings as their TOML text), and checks that each function the config names exists.
    pub(crate) fn load(config: &Config) -> Result<Self, Error> {
        let mut plugins = HashMap::new();
        for p in &config.plugins {
            let error = |message| plugin_error(&p.name, message);
            let wasm =
                std::fs::read(&p.path).map_err(|e| error(format!("{}: {e}", p.path.display())))?;
            let values = (p.config.iter()).map(|(key, value)| match value {
                toml::Value::String(s) => (key, s.clone()),
                value => (key, value.to_string()),
            });
            let manifest = Manifest::new([Wasm::data(wasm)])
                .with_config(values)
                .with_timeout(TIMEOUT)
                .with_memory_max(MEMORY_PAGES);
            let plugin = Plugin::new(&manifest, [], false).map_err(|e| error(format!("{e:#}")))?;
            plugins.insert(p.name.clone(), plugin);
        }
        let mut plugins = Plugins(plugins);
        for function in functions(config) {
            let (plugin, name) = plugins.get(function);
            if !plugin.function_exists(name) {
                return Err(plugin_error(function, "the plugin has no such function".into()));
            }
        }
        Ok(plugins)
    }

    /// Calls `<plugin>.<function>` with JSON.
    pub(crate) fn call(&mut self, function: &str, input: &[u8]) -> Result<Box<RawValue>, Error> {
        let (plugin, name) = self.get(function);
        let output: &[u8] =
            plugin.call(name, input).map_err(|e| plugin_error(function, format!("{e:#}")))?;
        serde_json::from_slice(output)
            .map_err(|e| plugin_error(function, format!("the answer is not JSON: {e}")))
    }

    fn get<'a>(&mut self, function: &'a str) -> (&mut Plugin, &'a str) {
        let (plugin, name) = function.split_once('.').expect("checked when the config loaded");
        let plugin = self.0.get_mut(plugin).expect("declared, checked when the config loaded");
        (plugin, name)
    }
}

/// Every plugin function the config names.
fn functions(config: &Config) -> impl Iterator<Item = &str> {
    let columns = config.columns.iter().flat_map(|c| {
        let source = match &c.source {
            Source::Plugin(h) => Some(h.plugin.as_str()),
            Source::Fields(_) => None,
        };
        let transforms = c.transforms.iter().filter_map(|t| match t {
            Transform::Plugin(f) => Some(f.as_str()),
            _ => None,
        });
        source.into_iter().chain(transforms)
    });
    let filters = config.filters.iter().map(|h| h.plugin.as_str());
    columns.chain(filters).chain(config.metadata.iter().map(String::as_str))
}
