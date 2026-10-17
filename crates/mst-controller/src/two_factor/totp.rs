//! Time-based one-time passwords (RFC 6238) as authenticator apps make them
//! by default: HMAC-SHA1 over 30-second steps, six digits.

use ring::hmac;

const STEP: u64 = 30;
const DIGITS: u32 = 6;

/// A new secret: 20 random bytes, as RFC 4226 recommends.
pub fn new_secret() -> Vec<u8> {
    rand::random::<[u8; 20]>().to_vec()
}

/// The code for a time step.
pub fn code_at(secret: &[u8], step: u64) -> String {
    let key = hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, secret);
    let mac = hmac::sign(&key, &step.to_be_bytes());
    let mac = mac.as_ref();
    let offset = (mac[mac.len() - 1] & 0x0f) as usize;
    let n = u32::from_be_bytes([
        mac[offset],
        mac[offset + 1],
        mac[offset + 2],
        mac[offset + 3],
    ]) & 0x7fff_ffff;
    format!("{:0width$}", n % 10u32.pow(DIGITS), width = DIGITS as usize)
}

/// Whether text looks like a TOTP code (six digits; spaces are ignored).
pub fn looks_like(code: &str) -> bool {
    let digits: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    digits.len() == DIGITS as usize && digits.chars().all(|c| c.is_ascii_digit())
}

/// Checks a code against the current time step and one either side (clocks
/// drift). Returns the step it matched, which must come after `last`, the
/// step last used: a code works once.
pub fn check(secret: &[u8], code: &str, unix_time: u64, last: Option<u64>) -> Option<u64> {
    let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    let now = unix_time / STEP;
    (now.saturating_sub(1)..=now + 1)
        .filter(|step| last.is_none_or(|l| *step > l))
        .find(|step| super::same(code_at(secret, *step).as_bytes(), code.as_bytes()))
}

/// The `otpauth://` URI authenticator apps read from a QR code.
pub fn uri(issuer: &str, account: &str, secret: &[u8]) -> String {
    let enc = |s: &str| {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                    (b as char).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect::<String>()
    };
    format!(
        "otpauth://totp/{}:{}?secret={}&issuer={}&algorithm=SHA1&digits={DIGITS}&period={STEP}",
        enc(issuer),
        enc(account),
        base32(secret),
        enc(issuer)
    )
}

const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// RFC 4648 base32 without padding, as authenticator apps take secrets.
pub fn base32(bytes: &[u8]) -> String {
    let mut out = String::new();
    let (mut buf, mut bits) = (0u32, 0);
    for &b in bytes {
        buf = (buf << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((buf >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((buf << (5 - bits)) & 31) as usize] as char);
    }
    out
}

pub fn base32_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut buf, mut bits) = (0u32, 0);
    for c in s.trim_end_matches('=').bytes() {
        let v = ALPHABET.iter().position(|&a| a == c.to_ascii_uppercase())? as u32;
        buf = (buf << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 6238 appendix B, SHA-1, last six digits.
    #[test]
    fn rfc_vectors() {
        let secret = b"12345678901234567890";
        for (time, code) in [
            (59, "287082"),
            (1111111109, "081804"),
            (1111111111, "050471"),
            (1234567890, "005924"),
            (2000000000, "279037"),
            (20000000000, "353130"),
        ] {
            assert_eq!(code_at(secret, time / STEP), code, "at {time}");
        }
    }

    #[test]
    fn checks_window_and_replay() {
        let secret = b"12345678901234567890";
        let now = 1111111111;
        assert_eq!(check(secret, "050471", now, None), Some(now / 30));
        assert_eq!(check(secret, "050 471", now, None), Some(now / 30));
        // The step before still works, once.
        let before = code_at(secret, now / 30 - 1);
        assert_eq!(check(secret, &before, now, None), Some(now / 30 - 1));
        assert_eq!(check(secret, &before, now, Some(now / 30 - 1)), None);
        // Two steps back is too old.
        assert_eq!(
            check(secret, &code_at(secret, now / 30 - 2), now, None),
            None
        );
        assert_eq!(check(secret, "000000", now, None), None);
    }

    #[test]
    fn base32_round_trip() {
        assert_eq!(base32(b"foobar"), "MZXW6YTBOI");
        assert_eq!(base32_decode("MZXW6YTBOI").unwrap(), b"foobar");
        let s = new_secret();
        assert_eq!(base32_decode(&base32(&s)).unwrap(), s);
        assert!(base32_decode("not base32!").is_none());
    }

    #[test]
    fn uri_encodes() {
        let u = uri("mst.example.com", "alice@acme/dev", b"foobar");
        assert_eq!(
            u,
            "otpauth://totp/mst.example.com:alice%40acme%2Fdev?secret=MZXW6YTBOI&issuer=mst.example.com&algorithm=SHA1&digits=6&period=30"
        );
    }
}
