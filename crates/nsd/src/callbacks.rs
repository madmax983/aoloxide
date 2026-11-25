//! Process lifecycle callbacks (`nsd/callbacks.c`).
//!
//! As in AOLserver, callbacks are pushed onto the front of their list, so
//! they run in reverse registration order (LIFO).

use std::sync::{Arc, Mutex, PoisonError};

/// A lifecycle callback.
pub type Callback = Arc<dyn Fn() + Send + Sync>;

/// When a callback runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum When {
    /// Before drivers start (`Ns_RegisterAtPreStartup`).
    PreStartup,
    /// After drivers start (`Ns_RegisterAtStartup`).
    Startup,
    /// When the server is ready (`Ns_RegisterAtReady`).
    Ready,
    /// On SIGHUP (`Ns_RegisterAtSignal`).
    Signal,
    /// At shutdown, in a separate thread with a timeout
    /// (`Ns_RegisterAtShutdown`).
    Shutdown,
    /// Just before exit (`Ns_RegisterAtExit`).
    Exit,
}

/// All registered callbacks.
#[derive(Default)]
pub struct Callbacks {
    lists: Mutex<Vec<(When, Callback)>>,
}

impl std::fmt::Debug for Callbacks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Callbacks").finish_non_exhaustive()
    }
}

impl Callbacks {
    /// Register `cb` to run at `when`.
    pub fn register(&self, when: When, cb: Callback) {
        self.lists
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(0, (when, cb));
    }

    /// Callbacks registered for `when`, in run order.
    pub fn get(&self, when: When) -> Vec<Callback> {
        self.lists
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(w, _)| *w == when)
            .map(|(_, c)| Arc::clone(c))
            .collect()
    }

    /// Run every callback for `when`, catching panics.  Returns the number
    /// that ran.
    pub fn run(&self, when: When) -> usize {
        let cbs = self.get(when);
        for cb in &cbs {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cb()));
        }
        cbs.len()
    }

    /// Number of callbacks registered for `when`.
    pub fn count(&self, when: When) -> usize {
        self.get(when).len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifo_order() {
        let cbs = Callbacks::default();
        let out = Arc::new(Mutex::new(Vec::new()));
        for i in 0..3 {
            let o = Arc::clone(&out);
            cbs.register(
                When::Shutdown,
                Arc::new(move || o.lock().expect("lock").push(i)),
            );
        }
        let o = Arc::clone(&out);
        cbs.register(
            When::Exit,
            Arc::new(move || o.lock().expect("lock").push(99)),
        );
        cbs.register(When::Exit, Arc::new(|| panic!("ignored")));
        assert_eq!(cbs.run(When::Shutdown), 3);
        assert_eq!(*out.lock().expect("lock"), vec![2, 1, 0]);
        assert_eq!(cbs.run(When::Exit), 2);
        assert_eq!(cbs.count(When::Startup), 0);
    }
}
