//! The nsproxy worker process (`nsproxy/nsproxy.c`).  Started by the
//! server as `nsproxy pool id`; evaluates scripts read from stdin.

fn main() {
    std::process::exit(nsproxy::worker_main(&ns_tcl::TclEngine::new()));
}
