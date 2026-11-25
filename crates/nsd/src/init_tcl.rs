//! The AOLserver Tcl library, embedded so a server home can be created
//! with [`install_home`] and so a missing `bin/init.tcl` still boots.
//!
//! These files are AOLserver's own Tcl sources (MPL 1.1, see
//! `LICENSE-AOLSERVER`), shipped unchanged.

use std::io;
use std::path::Path;

/// The bootstrap script (`bin/init.tcl`).
pub const INIT_TCL: &str = include_str!("../tcl/init.tcl");

/// Every library file as `(path relative to home, contents)`.
pub const LIBRARY: &[(&str, &str)] = &[
    ("bin/init.tcl", INIT_TCL),
    (
        "modules/tcl/charsets.tcl",
        include_str!("../tcl/modules/charsets.tcl"),
    ),
    (
        "modules/tcl/compat.tcl",
        include_str!("../tcl/modules/compat.tcl"),
    ),
    (
        "modules/tcl/debug.tcl",
        include_str!("../tcl/modules/debug.tcl"),
    ),
    (
        "modules/tcl/fastpath.tcl",
        include_str!("../tcl/modules/fastpath.tcl"),
    ),
    (
        "modules/tcl/file.tcl",
        include_str!("../tcl/modules/file.tcl"),
    ),
    (
        "modules/tcl/form.tcl",
        include_str!("../tcl/modules/form.tcl"),
    ),
    (
        "modules/tcl/http.tcl",
        include_str!("../tcl/modules/http.tcl"),
    ),
    (
        "modules/tcl/init.tcl",
        include_str!("../tcl/modules/init.tcl"),
    ),
    (
        "modules/tcl/modlog.tcl",
        include_str!("../tcl/modules/modlog.tcl"),
    ),
    (
        "modules/tcl/nsdb.tcl",
        include_str!("../tcl/modules/nsdb.tcl"),
    ),
    (
        "modules/tcl/packages.tcl",
        include_str!("../tcl/modules/packages.tcl"),
    ),
    (
        "modules/tcl/pools.tcl",
        include_str!("../tcl/modules/pools.tcl"),
    ),
    (
        "modules/tcl/prodebug.tcl",
        include_str!("../tcl/modules/prodebug.tcl"),
    ),
    (
        "modules/tcl/return.tcl",
        include_str!("../tcl/modules/return.tcl"),
    ),
    (
        "modules/tcl/sendmail.tcl",
        include_str!("../tcl/modules/sendmail.tcl"),
    ),
    (
        "modules/tcl/stats.tcl",
        include_str!("../tcl/modules/stats.tcl"),
    ),
    (
        "modules/tcl/util.tcl",
        include_str!("../tcl/modules/util.tcl"),
    ),
];

/// Write the library under `home` (creating `bin/`, `modules/tcl/`,
/// `log/` and `servers/`).  Existing files are left untouched unless
/// `overwrite` is set.  Returns the number of files written.
pub fn install_home(home: &Path, overwrite: bool) -> io::Result<usize> {
    let mut n = 0;
    for (rel, contents) in LIBRARY {
        let p = home.join(rel);
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        if overwrite || !p.exists() {
            std::fs::write(&p, contents)?;
            n += 1;
        }
    }
    std::fs::create_dir_all(home.join("log"))?;
    std::fs::create_dir_all(home.join("servers"))?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installs_library() {
        let d = tempfile::tempdir().expect("tempdir");
        let n = install_home(d.path(), false).expect("install");
        assert_eq!(n, LIBRARY.len());
        assert!(d.path().join("bin/init.tcl").is_file());
        assert!(d.path().join("modules/tcl/util.tcl").is_file());
        assert_eq!(install_home(d.path(), false).expect("again"), 0);
        assert!(INIT_TCL.contains("_ns_savenamespaces"));
    }
}
