//! SQLCipher 密钥管理：从 OS Keychain 取/生成 32B key + 16B salt。

use anyhow::Result;

const KEY_LEN: usize = 32;
const SALT_LEN: usize = 16;
const KEYCHAIN_SERVICE: &str = "weft";
const KEYCHAIN_ACCOUNT: &str = "db-key-v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SqlCipherKey {
    key: [u8; KEY_LEN],
    salt: [u8; SALT_LEN],
}

impl SqlCipherKey {
    pub fn random() -> Self {
        use rand::RngCore;
        let mut buf = [0u8; KEY_LEN + SALT_LEN];
        rand::rngs::OsRng.fill_bytes(&mut buf);
        let mut key = [0u8; KEY_LEN];
        let mut salt = [0u8; SALT_LEN];
        key.copy_from_slice(&buf[..KEY_LEN]);
        salt.copy_from_slice(&buf[KEY_LEN..]);
        Self { key, salt }
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != KEY_LEN + SALT_LEN {
            return Err(anyhow::anyhow!(
                "sqlcipher key must be {} bytes, got {}",
                KEY_LEN + SALT_LEN,
                bytes.len()
            ));
        }
        let mut key = [0u8; KEY_LEN];
        let mut salt = [0u8; SALT_LEN];
        key.copy_from_slice(&bytes[..KEY_LEN]);
        salt.copy_from_slice(&bytes[KEY_LEN..]);
        Ok(Self { key, salt })
    }

    pub fn to_bytes(&self) -> [u8; KEY_LEN + SALT_LEN] {
        let mut out = [0u8; KEY_LEN + SALT_LEN];
        out[..KEY_LEN].copy_from_slice(&self.key);
        out[KEY_LEN..].copy_from_slice(&self.salt);
        out
    }
}

/// Format key + salt as the SQLCipher `"x'<64hex><32hex>'"` literal.
/// The outer double quotes are REQUIRED: SQLCipher's `PRAGMA key` parser treats
/// the value as a string and only then recognises the inner `x'...'` blob
/// form. Sending the bare blob literal would yield "syntax error near x'...'".
/// See https://www.zetetic.net/sqlcipher/sqlcipher-api/#example-3-raw-key-data-with-explicit-salt-without-key-derivation
pub fn format_for_pragma(k: &SqlCipherKey) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(2 + 2 + 64 + 32 + 1);
    s.push('"');
    s.push_str("x'");
    for b in k.key.iter().chain(k.salt.iter()) {
        let _ = write!(s, "{:02x}", b);
    }
    s.push('\'');
    s.push('"');
    s
}

/// 从 OS Keychain 取密钥；不存在则随机生成并写回。
///
/// 测试旁路：环境变量 `WEFT_TEST_DB_KEY_B64` 存在时直接用它（base64 编码的 48 字节），
/// 完全绕开 Keychain。集成测试用 `tempfile + WEFT_HOME + WEFT_TEST_DB_KEY_B64` 隔离环境。
pub fn get_or_create() -> Result<SqlCipherKey> {
    if let Ok(b64) = std::env::var("WEFT_TEST_DB_KEY_B64") {
        return decode_b64_key(&b64);
    }

    let entry = keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
        .map_err(|e| anyhow::anyhow!("keyring entry: {e}"))?;

    match entry.get_password() {
        Ok(b64) => decode_b64_key(&b64),
        Err(keyring::Error::NoEntry) => {
            let k = SqlCipherKey::random();
            let b64 = encode_b64_key(&k);
            entry
                .set_password(&b64)
                .map_err(|e| anyhow::anyhow!("keyring write: {e}"))?;
            Ok(k)
        }
        Err(e) => Err(anyhow::anyhow!("keyring read: {e}")),
    }
}

fn encode_b64_key(k: &SqlCipherKey) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(k.to_bytes())
}

fn decode_b64_key(s: &str) -> Result<SqlCipherKey> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(s.trim())
        .map_err(|e| anyhow::anyhow!("base64 decode: {e}"))?;
    SqlCipherKey::from_bytes(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_produces_48_bytes() {
        let k = SqlCipherKey::random();
        assert_eq!(k.to_bytes().len(), 48);
    }

    #[test]
    fn from_bytes_roundtrip() {
        let raw = [0xABu8; 48];
        let k = SqlCipherKey::from_bytes(&raw).unwrap();
        assert_eq!(k.to_bytes(), raw);
    }

    #[test]
    fn from_bytes_rejects_wrong_length() {
        assert!(SqlCipherKey::from_bytes(&[0u8; 47]).is_err());
        assert!(SqlCipherKey::from_bytes(&[0u8; 49]).is_err());
    }

    #[test]
    fn format_for_pragma_is_x_hex_literal() {
        let raw = {
            let mut v = [0u8; 48];
            for (i, b) in v.iter_mut().enumerate() {
                *b = i as u8;
            }
            v
        };
        let k = SqlCipherKey::from_bytes(&raw).unwrap();
        let s = format_for_pragma(&k);
        // SQLCipher requires the blob literal to be wrapped in double quotes
        // so its PRAGMA key parser accepts it as a string.
        assert!(s.starts_with("\"x'"));
        assert!(s.ends_with("'\""));
        // 64 hex chars (key) + 32 hex chars (salt) + "x'' = 101
        assert_eq!(s.len(), 101);
        // first key byte 00 → "00"; second 01 → "01"
        assert_eq!(&s[3..7], "0001");
    }

    #[test]
    fn b64_roundtrip() {
        let k = SqlCipherKey::random();
        let s = encode_b64_key(&k);
        let k2 = decode_b64_key(&s).unwrap();
        assert_eq!(k.to_bytes(), k2.to_bytes());
    }

    #[test]
    fn decode_rejects_garbage() {
        assert!(decode_b64_key("not-valid-base64!!!").is_err());
        assert!(decode_b64_key("aGVsbG8=").is_err());
    }

    #[test]
    fn get_or_create_uses_env_bypass() {
        use base64::Engine;
        let raw = [0x7Fu8; 48];
        let b64 = base64::engine::general_purpose::STANDARD.encode(raw);
        std::env::set_var("WEFT_TEST_DB_KEY_B64", &b64);
        let k = get_or_create().unwrap();
        std::env::remove_var("WEFT_TEST_DB_KEY_B64");
        assert_eq!(k.to_bytes(), raw);
    }
}
