//! Locate and link libtcl.
//!
//! Override with `TCL_LIB_DIR` (directory holding the library) and
//! `TCL_LIB_NAME` (default `tcl8.6`).

use std::env;
use std::path::Path;

fn main() {
    println!("cargo:rerun-if-env-changed=TCL_LIB_DIR");
    println!("cargo:rerun-if-env-changed=TCL_LIB_NAME");
    let name = env::var("TCL_LIB_NAME").unwrap_or_else(|_| "tcl8.6".to_owned());
    if let Ok(dir) = env::var("TCL_LIB_DIR") {
        println!("cargo:rustc-link-search=native={dir}");
    } else {
        for dir in [
            "/usr/lib/x86_64-linux-gnu",
            "/usr/lib/aarch64-linux-gnu",
            "/usr/lib64",
            "/usr/lib",
            "/usr/local/lib",
            "/opt/homebrew/opt/tcl-tk/lib",
            "/usr/local/opt/tcl-tk/lib",
        ] {
            if Path::new(dir).join(format!("lib{name}.so")).exists()
                || Path::new(dir).join(format!("lib{name}.dylib")).exists()
            {
                println!("cargo:rustc-link-search=native={dir}");
                break;
            }
        }
    }
    println!("cargo:rustc-link-lib=dylib={name}");
}
