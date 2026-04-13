use anyhow::Result;
use rcgen::CertifiedKey;

/// Generate a self-signed certificate for QUIC/TLS.
///
/// Uses rcgen with default parameters. The certificate is intended for LAN
/// benchmarking only -- not production use.
pub fn generate_self_signed_cert() -> Result<CertifiedKey> {
    let certified_key = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])?;
    Ok(certified_key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_self_signed_cert() {
        let ck = generate_self_signed_cert().expect("cert generation should succeed");
        // Verify we can extract DER-encoded cert and key.
        let _cert_der = ck.cert.der();
        let _key_der = ck.key_pair.serialize_der();
    }
}
