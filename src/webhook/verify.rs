use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    #[error("missing X-Hub-Signature-256 header")]
    MissingHeader,
    #[error("malformed signature header")]
    Malformed,
    #[error("signature mismatch")]
    Mismatch,
}

/// Verify a webhook body against the `X-Hub-Signature-256` header value.
/// `header_value` is expected to look like `sha256=<hex>`.
pub fn verify(secret: &[u8], body: &[u8], header_value: Option<&str>) -> Result<(), VerifyError> {
    let hv = header_value.ok_or(VerifyError::MissingHeader)?;
    let hex = hv.strip_prefix("sha256=").ok_or(VerifyError::Malformed)?;
    let mut sig_bytes = vec![0u8; hex.len() / 2];
    hex_decode(hex, &mut sig_bytes).map_err(|_| VerifyError::Malformed)?;

    let mut mac = <Hmac<Sha256>>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(body);
    let expected = mac.finalize().into_bytes();

    if expected.len() != sig_bytes.len() {
        return Err(VerifyError::Mismatch);
    }
    if expected.ct_eq(&sig_bytes).into() {
        Ok(())
    } else {
        Err(VerifyError::Mismatch)
    }
}

fn hex_decode(s: &str, out: &mut [u8]) -> Result<(), ()> {
    if s.len() != out.len() * 2 {
        return Err(());
    }
    for (i, byte) in out.iter_mut().enumerate() {
        let hi = u8::from_str_radix(&s[i * 2..i * 2 + 1], 16).map_err(|_| ())?;
        let lo = u8::from_str_radix(&s[i * 2 + 1..i * 2 + 2], 16).map_err(|_| ())?;
        *byte = (hi << 4) | lo;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hmac::Mac;

    fn sign(secret: &[u8], body: &[u8]) -> String {
        let mut mac = <Hmac<Sha256>>::new_from_slice(secret).unwrap();
        mac.update(body);
        let bytes = mac.finalize().into_bytes();
        let mut out = String::with_capacity(7 + bytes.len() * 2);
        out.push_str("sha256=");
        for b in bytes {
            out.push_str(&format!("{b:02x}"));
        }
        out
    }

    #[test]
    fn good_signature_passes() {
        let h = sign(b"secret", b"hello");
        verify(b"secret", b"hello", Some(&h)).unwrap();
    }

    #[test]
    fn tampered_body_rejected() {
        let h = sign(b"secret", b"hello");
        assert!(matches!(
            verify(b"secret", b"hello!", Some(&h)),
            Err(VerifyError::Mismatch)
        ));
    }

    #[test]
    fn wrong_secret_rejected() {
        let h = sign(b"secret", b"hello");
        assert!(matches!(
            verify(b"other", b"hello", Some(&h)),
            Err(VerifyError::Mismatch)
        ));
    }

    #[test]
    fn missing_header_rejected() {
        assert!(matches!(
            verify(b"secret", b"hello", None),
            Err(VerifyError::MissingHeader)
        ));
    }

    #[test]
    fn malformed_header_rejected() {
        assert!(matches!(
            verify(b"secret", b"hello", Some("nope")),
            Err(VerifyError::Malformed)
        ));
    }
}
