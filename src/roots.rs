//! Root certificates for machines without a system trust store, such as
//! minimal container images without the `ca-certificates` package. There the
//! platform verifier refuses to start, which would stop even plain `http://`
//! runs; loadman then verifies `https://` with the Mozilla roots it bundles.

use std::error::Error;
use std::sync::{Arc, Once, OnceLock};

use rustls::crypto::CryptoProvider;
use rustls::pki_types::CertificateDer;

/// The platform verifier's message when the system has no CA certificates.
const NO_SYSTEM_ROOTS: &str = "No CA certificates were loaded from the system";

/// Whether `err`, or any error it wraps, says the system has no CA certificates.
pub fn is_missing(err: &(dyn Error + 'static)) -> bool {
    let mut cur = Some(err);
    while let Some(e) = cur {
        if e.to_string().contains(NO_SYSTEM_ROOTS) {
            return true;
        }
        cur = e.source();
    }
    false
}

/// Whether this machine has no system trust store (checked once per process).
pub fn system_missing() -> bool {
    static MISSING: OnceLock<bool> = OnceLock::new();
    *MISSING.get_or_init(|| {
        let provider = CryptoProvider::get_default()
            .cloned()
            .unwrap_or_else(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider()));
        matches!(rustls_platform_verifier::Verifier::new(provider), Err(e) if is_missing(&e))
    })
}

/// Mozilla's root certificates, as shipped by the `webpki-root-certs` crate.
pub fn bundled() -> &'static [CertificateDer<'static>] {
    webpki_root_certs::TLS_SERVER_ROOT_CERTS
}

pub fn warn_once() {
    static WARNED: Once = Once::new();
    WARNED.call_once(|| {
        eprintln!(
            "warning: no system CA certificates found; https:// is verified with the Mozilla roots bundled into loadman"
        )
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Outer(rustls::Error);

    impl std::fmt::Display for Outer {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("builder error")
        }
    }

    impl Error for Outer {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            Some(&self.0)
        }
    }

    #[test]
    fn detects_the_error_anywhere_in_the_chain() {
        let inner = rustls::Error::General(NO_SYSTEM_ROOTS.to_string());
        assert!(is_missing(&Outer(inner)));
        assert!(!is_missing(&Outer(rustls::Error::General("other".into()))));
    }

    #[test]
    fn bundle_is_usable() {
        let mut store = rustls::RootCertStore::empty();
        let (added, _) = store.add_parsable_certificates(bundled().iter().cloned());
        assert!(added > 100, "only {added} roots");
    }
}
