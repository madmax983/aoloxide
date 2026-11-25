//! Module loading (`nsd/modload.c`).
//!
//! AOLserver loads shared libraries listed in `ns/server/S/modules` (and
//! `ns/modules` for global modules) and calls their `Ns_ModuleInit`.  Rust
//! has no stable ABI for that, so modules are compiled in and registered
//! by name in a [`ModuleRegistry`]; config entries such as
//! `ns_param nssock ${bindir}/nssock.so` resolve to the module `nssock`.
//! The value `tcl` marks a Tcl-only module whose library is sourced at
//! startup, exactly as in AOLserver.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use crate::driver::Driver;
use crate::global::{Global, NsError};
use crate::server::Server;

/// Context passed to a module's init function.
pub struct ModuleContext<'a> {
    /// Process state.
    pub global: &'a Arc<Global>,
    /// The server for per-server modules, `None` for global modules.
    pub server: Option<&'a Arc<Server>>,
    /// All servers (for global drivers with virtual hosts).
    pub servers: &'a [Arc<Server>],
    /// The module name as configured (the config key).
    pub name: &'a str,
    /// Drivers created by the module are pushed here and started with the
    /// server.
    pub drivers: &'a Mutex<Vec<Arc<Driver>>>,
}

impl ModuleContext<'_> {
    /// The module's config section path (`Ns_ConfigGetPath(server, module)`).
    pub fn config_path(&self) -> String {
        ns_core::Config::make_path(self.server.map(|s| s.name.as_str()), Some(self.name), &[])
    }

    /// Register a driver for startup.
    pub fn add_driver(&self, d: Arc<Driver>) {
        self.global
            .drivers
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Arc::clone(&d.info));
        self.drivers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(d);
    }
}

/// A module's init function (`Ns_ModuleInit`).
pub type ModuleInit = Arc<dyn Fn(&ModuleContext<'_>) -> Result<(), NsError> + Send + Sync>;

/// Compiled-in modules by name.
#[derive(Clone, Default)]
pub struct ModuleRegistry {
    modules: BTreeMap<String, ModuleInit>,
}

impl std::fmt::Debug for ModuleRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModuleRegistry")
            .field("modules", &self.modules.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Derive a module name from a config value like `${bindir}/nssock.so`.
pub fn module_key(name: &str, file: &str) -> String {
    let base = file.rsplit(['/', '\\']).next().unwrap_or(file);
    let stem = base
        .strip_suffix(".so")
        .or_else(|| base.strip_suffix(".dll"))
        .or_else(|| base.strip_suffix(".dylib"))
        .unwrap_or(base);
    if stem.is_empty() {
        name.to_owned()
    } else {
        stem.to_owned()
    }
}

impl ModuleRegistry {
    /// Empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a module.
    pub fn register(&mut self, name: &str, init: ModuleInit) {
        self.modules.insert(name.to_owned(), init);
    }

    /// Look up a module.
    pub fn get(&self, name: &str) -> Option<&ModuleInit> {
        self.modules.get(name)
    }

    /// Registered module names.
    pub fn names(&self) -> Vec<String> {
        self.modules.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        assert_eq!(module_key("nssock", "${bindir}/nssock.so"), "nssock");
        assert_eq!(module_key("x", "nslog.dll"), "nslog");
        assert_eq!(module_key("perm", "nsperm"), "nsperm");
        assert_eq!(module_key("m", ""), "m");
        let mut r = ModuleRegistry::new();
        r.register("a", Arc::new(|_| Ok(())));
        assert!(r.get("a").is_some());
        assert_eq!(r.names(), vec!["a"]);
    }
}
