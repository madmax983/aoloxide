//! # ns-core
//!
//! Engine-independent building blocks of the AOLserver port: the data
//! structures and HTTP utilities that `libnsd` provided to the rest of the
//! server and to modules.
//!
//! | Module | AOLserver source |
//! |---|---|
//! | [`set`] | `nsd/set.c` (`Ns_Set`) |
//! | [`urlspace`] | `nsd/urlspace.c` (`Ns_UrlSpecific*`) |
//! | [`glob`] | `Tcl_StringMatch` |
//! | [`request`] | `nsd/request.c` |
//! | [`pathname`] | `nsd/pathname.c` |
//! | [`urlencode`] | `nsd/urlencode.c` |
//! | [`httptime`] | `nsd/httptime.c` |
//! | [`text`] | `nsd/quotehtml.c`, `nsd/uuencode.c` |
//! | [`crypt`] | `nsd/crypt.c` |
//! | [`mimetypes`] | `nsd/mimetypes.c` |
//! | [`config`] | `nsd/config.c` |
//! | [`cache`] | `nsd/cache.c` |
//! | [`log`] | `nsd/log.c`, `nsd/rollfile.c` |
//! | [`form`] | `nsd/form.c` |

pub mod cache;
pub mod config;
pub mod crypt;
pub mod form;
pub mod glob;
pub mod httptime;
pub mod localtime;
pub mod log;
pub mod mimetypes;
pub mod pathname;
pub mod request;
pub mod set;
pub mod text;
pub mod urlencode;
pub mod urlspace;

pub use config::Config;
pub use set::NsSet;
pub use urlspace::{UrlFlags, UrlSpace};

/// AOLserver version this port tracks.
pub const NS_VERSION: &str = "4.5";
/// AOLserver patch level this port tracks.
pub const NS_PATCH_LEVEL: &str = "4.5.1";
