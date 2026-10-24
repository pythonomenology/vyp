use std::path::Path;
use tracing::{info, warn};
use vyp_api::plugin_abi::{PluginInitFn, VYP_ABI_VERSION};
use vyp_api::PluginRegistration;

use super::registry::{FilterRegistry, ProviderRegistry, StrategyRegistry};

/// Errors during plugin loading.
#[derive(Debug, thiserror::Error)]
pub enum PluginLoadError {
    #[error("failed to load plugin library: {0}")]
    LoadError(String),
    #[error("plugin '{name}' has ABI version {plugin_version}, expected {expected_version}")]
    AbiMismatch {
        name: String,
        plugin_version: u32,
        expected_version: u32,
    },
    #[error("plugin init function not found in {path}")]
    InitNotFound { path: String },
}

/// Loads plugins from dynamic libraries and registers them into the registries.
pub struct PluginLoader {
    pub strategies: StrategyRegistry,
    pub providers: ProviderRegistry,
    pub filters: FilterRegistry,
    loaded_plugins: Vec<PluginInfo>,
}

#[derive(Debug, Clone)]
pub struct PluginInfo {
    pub name: String,
    pub version: String,
    pub source: String,
}

impl PluginLoader {
    pub fn new() -> Self {
        Self {
            strategies: StrategyRegistry::new(),
            providers: ProviderRegistry::new(),
            filters: FilterRegistry::new(),
            loaded_plugins: Vec::new(),
        }
    }

    /// Register a plugin's components directly (for built-in strategies).
    pub fn register_builtin(&mut self, registration: PluginRegistration) {
        let info = PluginInfo {
            name: registration.name.clone(),
            version: registration.version.clone(),
            source: "builtin".to_string(),
        };
        info!(plugin = %info.name, "Registering built-in plugin");

        for strategy in registration.strategies {
            self.strategies.register(strategy);
        }
        for provider in registration.metadata_providers {
            self.providers.register(provider);
        }
        for filter in registration.filters {
            self.filters.register(filter);
        }
        self.loaded_plugins.push(info);
    }

    /// Load a plugin from a dynamic library file.
    ///
    /// # Safety
    /// Loading dynamic libraries is inherently unsafe. The library must export
    /// a `vyp_plugin_init` function with the correct signature.
    pub unsafe fn load_plugin(&mut self, path: &Path) -> Result<(), PluginLoadError> {
        let path_str = path.display().to_string();
        info!(path = %path_str, "Loading plugin");

        let lib = unsafe {
            libloading::Library::new(path).map_err(|e| PluginLoadError::LoadError(e.to_string()))?
        };

        let init_fn: libloading::Symbol<PluginInitFn> = unsafe {
            lib.get(b"vyp_plugin_init")
                .map_err(|_| PluginLoadError::InitNotFound {
                    path: path_str.clone(),
                })?
        };

        let registration = unsafe { init_fn() };

        if registration.abi_version != VYP_ABI_VERSION {
            return Err(PluginLoadError::AbiMismatch {
                name: registration.name,
                plugin_version: registration.abi_version,
                expected_version: VYP_ABI_VERSION,
            });
        }

        let info = PluginInfo {
            name: registration.name.clone(),
            version: registration.version.clone(),
            source: path_str,
        };
        info!(plugin = %info.name, version = %info.version, "Loaded plugin");

        for strategy in registration.strategies {
            self.strategies.register(strategy);
        }
        for provider in registration.metadata_providers {
            self.providers.register(provider);
        }
        for filter in registration.filters {
            self.filters.register(filter);
        }
        self.loaded_plugins.push(info);

        // Leak the library so it stays loaded for the process lifetime
        std::mem::forget(lib);

        Ok(())
    }

    /// Load all plugins from a directory.
    ///
    /// # Safety
    ///
    /// Plugin libraries are loaded with `libloading` and must satisfy the same
    /// safety requirements as `load`: correct plugin ABI, no conflicting symbols,
    /// and valid function signatures.
    pub unsafe fn load_from_directory(&mut self, dir: &Path) -> Vec<PluginLoadError> {
        let mut errors = Vec::new();
        if !dir.exists() {
            warn!(path = %dir.display(), "Plugin directory does not exist");
            return errors;
        }

        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) => {
                errors.push(PluginLoadError::LoadError(e.to_string()));
                return errors;
            }
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if matches!(ext, "so" | "dylib" | "dll") {
                if let Err(e) = unsafe { self.load_plugin(&path) } {
                    errors.push(e);
                }
            }
        }

        errors
    }

    pub fn loaded_plugins(&self) -> &[PluginInfo] {
        &self.loaded_plugins
    }
}

impl Default for PluginLoader {
    fn default() -> Self {
        Self::new()
    }
}
