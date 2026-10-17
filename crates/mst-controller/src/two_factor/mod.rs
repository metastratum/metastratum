//! Second factors for password sign-in: authenticator apps (TOTP), security
//! keys and passkeys (WebAuthn, FIDO U2F keys included), YubiKeys typing
//! Yubico OTPs, and backup codes for when those are lost.

pub mod totp;
pub mod webauthn;
pub mod yubico;

use mst_core::ids::UserId;

/// Compares secrets in time that does not depend on where they differ.
pub fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// What a typed code is, by its shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodeKind {
    Totp,
    YubicoOtp,
    Backup,
}

pub fn code_kind(code: &str) -> Option<CodeKind> {
    let code = code.trim();
    if totp::looks_like(code) {
        Some(CodeKind::Totp)
    } else if yubico::looks_like(code) {
        Some(CodeKind::YubicoOtp)
    } else if normalize_backup_code(code).is_some() {
        Some(CodeKind::Backup)
    } else {
        None
    }
}

/// Letters and digits that cannot be mistaken for one another.
const BACKUP_ALPHABET: &[u8] = b"23456789abcdefghjkmnpqrstuvwxyz";
const BACKUP_LEN: usize = 12;
pub const BACKUP_CODES: usize = 10;

/// A new set of backup codes, as shown to the user (`abcd-efgh-jkmn`).
pub fn new_backup_codes() -> Vec<String> {
    (0..BACKUP_CODES)
        .map(|_| {
            let chars: String = (0..BACKUP_LEN)
                .map(|_| {
                    let i = rand::random::<u32>() as usize % BACKUP_ALPHABET.len();
                    BACKUP_ALPHABET[i] as char
                })
                .collect();
            format!("{}-{}-{}", &chars[..4], &chars[4..8], &chars[8..])
        })
        .collect()
}

/// A backup code as typed, without dashes and spaces and in lower case;
/// none if it cannot be one.
pub fn normalize_backup_code(code: &str) -> Option<String> {
    let c: String = code
        .chars()
        .filter(|c| *c != '-' && !c.is_whitespace())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    (c.len() == BACKUP_LEN && c.bytes().all(|b| BACKUP_ALPHABET.contains(&b))).then_some(c)
}

/// How a backup code is kept: hashed with its user's ID. Codes carry about
/// 59 bits, too many to search for.
pub fn hash_backup_code(user: UserId, normalized: &str) -> String {
    mst_pki::hash_secret(&format!("{user}:{normalized}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tells_codes_apart() {
        assert_eq!(code_kind("123456"), Some(CodeKind::Totp));
        assert_eq!(code_kind(" 123 456 "), Some(CodeKind::Totp));
        assert_eq!(
            code_kind("cccjgjgkhcbbirdrfdnlnghhfgrtnnlgedjlftrbdeut"),
            Some(CodeKind::YubicoOtp)
        );
        for code in new_backup_codes() {
            assert_eq!(code.len(), 14);
            assert_eq!(code_kind(&code), Some(CodeKind::Backup));
            assert_eq!(code_kind(&code.to_uppercase()), Some(CodeKind::Backup));
        }
        assert_eq!(code_kind("12345"), None);
        assert_eq!(code_kind("abcd-efgh-ijkl"), None); // i and l are not used
    }

    #[test]
    fn backup_codes_differ_and_hash_per_user() {
        let codes = new_backup_codes();
        let unique: std::collections::HashSet<_> = codes.iter().collect();
        assert_eq!(unique.len(), BACKUP_CODES);
        let n = normalize_backup_code(&codes[0]).unwrap();
        assert_ne!(
            hash_backup_code(UserId::new(), &n),
            hash_backup_code(UserId::new(), &n)
        );
    }

    #[test]
    fn compares() {
        assert!(same(b"abc", b"abc"));
        assert!(!same(b"abc", b"abd"));
        assert!(!same(b"abc", b"ab"));
    }
}
