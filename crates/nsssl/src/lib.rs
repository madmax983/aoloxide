//! # nsssl
//!
//! The AOLserver SSL driver (`nsssl/`), reimplemented on rustls.  It is the
//! standard socket driver with a TLS [`StreamWrapper`] applied after accept,
//! so HTTPS connections share read-ahead, keep-alive and limits with
//! `nssock` (ADR 0003).
//!
//! Config (`ns/server/S/module/nsssl`): `certfile` and `keyfile` (PEM;
//! relative paths resolve in the module directory) plus every `nssock`
//! parameter.  As in AOLserver, a missing `certfile` logs a warning and the
//! driver is not started.  The default port is 443 and locations use
//! `https://`.
//!
//! As `NsSSLInitialize` does, a per-server load adds `ssl_info` to the
//! server's interps (even without a certificate).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ns_core::log::Severity;
use ns_script::command;
use nsd::driver::{ConnStream, StreamWrapper, WrapFuture};
use nsd::{ModuleContext, NsError};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::TlsAcceptor;

/// A TLS stream wrapper.
pub struct TlsWrapper {
    acceptor: TlsAcceptor,
}

impl std::fmt::Debug for TlsWrapper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsWrapper").finish_non_exhaustive()
    }
}

impl StreamWrapper for TlsWrapper {
    fn wrap(&self, tcp: tokio::net::TcpStream) -> WrapFuture {
        let acceptor = self.acceptor.clone();
        Box::pin(async move {
            let s = acceptor.accept(tcp).await?;
            Ok(Box::new(s) as Box<dyn ConnStream>)
        })
    }
}

/// Load a PEM certificate chain.
pub fn load_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>, NsError> {
    let data = std::fs::read(path)
        .map_err(|e| NsError::Config(format!("nsssl: could not read {}: {e}", path.display())))?;
    let certs: Vec<_> = rustls_pemfile::certs(&mut &data[..])
        .collect::<Result<_, _>>()
        .map_err(|e| {
            NsError::Config(format!(
                "nsssl: invalid certificate {}: {e}",
                path.display()
            ))
        })?;
    if certs.is_empty() {
        return Err(NsError::Config(format!(
            "nsssl: no certificates in {}",
            path.display()
        )));
    }
    Ok(certs)
}

/// Load a PEM private key (PKCS#8, PKCS#1 or SEC1).
pub fn load_key(path: &Path) -> Result<PrivateKeyDer<'static>, NsError> {
    let data = std::fs::read(path)
        .map_err(|e| NsError::Config(format!("nsssl: could not read {}: {e}", path.display())))?;
    rustls_pemfile::private_key(&mut &data[..])
        .map_err(|e| NsError::Config(format!("nsssl: invalid key {}: {e}", path.display())))?
        .ok_or_else(|| NsError::Config(format!("nsssl: no private key in {}", path.display())))
}

/// Build a wrapper from PEM files.
pub fn wrapper(cert: &Path, key: &Path) -> Result<TlsWrapper, NsError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| NsError::Config(format!("nsssl: {e}")))?
        .with_no_client_auth()
        .with_single_cert(load_certs(cert)?, load_key(key)?)
        .map_err(|e| NsError::Config(format!("nsssl: {e}")))?;
    Ok(TlsWrapper {
        acceptor: TlsAcceptor::from(Arc::new(config)),
    })
}

fn resolve(ctx: &ModuleContext<'_>, p: &str) -> PathBuf {
    let path = PathBuf::from(p);
    if path.is_absolute() {
        return path;
    }
    match ctx.server {
        Some(s) => s.module_path(Some(ctx.name), &[p]),
        None => ctx.global.home_path(&["modules", ctx.name, p]),
    }
}

/// `ssl_info` (`InfoCmd` in `ssltcl.c`): `{SSL_EXPORT} {SSL_PROTOCOL_VERSION} `.
/// AOLserver's non-export build reports `{0} {2} `; the constants are
/// reproduced verbatim, whatever TLS version rustls negotiates.  Arguments
/// are ignored, as in C.
pub const SSL_INFO: &str = "{0} {2} ";

/// Module init (`Ns_ModuleInit` in `nsssl.c`).
pub fn init(ctx: &ModuleContext<'_>) -> Result<(), NsError> {
    if let Some(server) = ctx.server {
        server.add_command("ssl_info", command(|_, _| Ok(SSL_INFO.to_owned())));
    }
    let path = ctx.config_path();
    let c = &ctx.global.config;
    let Some(cert) = c.get(&path, "certfile") else {
        ctx.global.log(
            Severity::Warning,
            &format!("{}: certfile not specified", ctx.name),
        );
        return Ok(());
    };
    let cert = resolve(ctx, cert);
    let key = resolve(ctx, c.get(&path, "keyfile").unwrap_or("keyfile.pem"));
    let w = wrapper(&cert, &key)?;
    nsd::driver_init(ctx, "nsssl", true, Arc::new(w))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_bad_files() {
        let d = tempfile::tempdir().expect("tmp");
        let p = d.path().join("x.pem");
        std::fs::write(&p, "not pem").expect("w");
        assert!(load_certs(&p).is_err());
        assert!(load_key(&p).is_err());
        assert!(load_certs(&d.path().join("missing")).is_err());
        assert!(wrapper(&p, &p).is_err());
    }
}
