//! The XOR "autokey" cipher every Kasa JSON endpoint uses: UDP/9999
//! discovery, the TCP/9999 smart-home protocol and the camera LINKIE2
//! control API.
//!
//! The key starts at `0xAB` and each **ciphertext** byte becomes the key
//! for the next byte, in both directions. It is an obfuscation, not
//! encryption.

/// Initial key byte.
const INITIAL_KEY: u8 = 0xab;

/// Encrypts `buf` in place.
pub fn encrypt_in_place(buf: &mut [u8]) {
    let mut key = INITIAL_KEY;
    for b in buf.iter_mut() {
        let c = key ^ *b;
        *b = c;
        key = c;
    }
}

/// Decrypts `buf` in place.
pub fn decrypt_in_place(buf: &mut [u8]) {
    let mut key = INITIAL_KEY;
    for b in buf.iter_mut() {
        let c = *b;
        *b = key ^ c;
        key = c;
    }
}

/// Encrypts `plain` into a new buffer.
#[must_use]
pub fn encrypt(plain: &[u8]) -> Vec<u8> {
    let mut out = plain.to_vec();
    encrypt_in_place(&mut out);
    out
}

/// Decrypts `cipher` into a new buffer.
#[must_use]
pub fn decrypt(cipher: &[u8]) -> Vec<u8> {
    let mut out = cipher.to_vec();
    decrypt_in_place(&mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write as _;

    /// Known-answer vector from `docs/PROTOCOL.md` of the Scrypted plugin.
    const SYSINFO: &str = r#"{"system":{"get_sysinfo":{}}}"#;
    const SYSINFO_HEX: &str = "d0f281f88bff9af7d5ef94b6d1b4c09fec95e68fe187e8caf08bf68bf6";

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().fold(String::new(), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
    }

    #[test]
    fn encrypts_known_vector() {
        assert_eq!(hex(&encrypt(SYSINFO.as_bytes())), SYSINFO_HEX);
    }

    #[test]
    fn decrypts_known_vector() {
        let cipher = encrypt(SYSINFO.as_bytes());
        assert_eq!(decrypt(&cipher), SYSINFO.as_bytes());
    }

    #[test]
    fn round_trips_arbitrary_bytes() {
        let plain: Vec<u8> = (0..=255).collect();
        assert_eq!(decrypt(&encrypt(&plain)), plain);
        assert_eq!(encrypt(&[]), Vec::<u8>::new());
    }

    #[test]
    fn first_byte_is_xored_with_initial_key() {
        assert_eq!(encrypt(&[0x00]), vec![0xab]);
        assert_eq!(decrypt(&[0xab]), vec![0x00]);
    }
}
