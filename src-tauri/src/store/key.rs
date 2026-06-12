//! SQLCipher 密码管理：默认明文；启用加密时由用户在 Settings 里设置密码，
//! 密码缓存在 OS Keychain，下次启动透明打开。
//!
//! 测试旁路：环境变量 `WEFT_TEST_DB_PASSWORD` 存在时直接用它作密码，完全绕开
//! Keychain。集成测试搭配 `tempfile + WEFT_HOME + WEFT_TEST_DB_PASSWORD` 隔离环境。

use anyhow::Result;

const KEYCHAIN_SERVICE: &str = "weft";
const KEYCHAIN_ACCOUNT: &str = "db-password-v1";
const ENV_BYPASS: &str = "WEFT_TEST_DB_PASSWORD";

/// 把用户密码序列化成 SQLCipher 的 `"x'<hex>'"` 字面量或带引号字符串字面量。
/// 密码走 PBKDF2，传 PRAGMA key 时用 SQL 字符串就行。注意密码里的单引号要 doubled。
pub fn pragma_literal(password: &str) -> String {
    let escaped = password.replace('\'', "''");
    format!("'{escaped}'")
}

/// 取 Keychain 里保存的密码；不存在返回 None。优先 env bypass 用于测试隔离。
pub fn get_password() -> Result<Option<String>> {
    if let Ok(pwd) = std::env::var(ENV_BYPASS) {
        return Ok(Some(pwd));
    }
    let entry = keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
        .map_err(|e| anyhow::anyhow!("keyring entry: {e}"))?;
    match entry.get_password() {
        Ok(pwd) => Ok(Some(pwd)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(anyhow::anyhow!("keyring read: {e}")),
    }
}

/// 写密码到 Keychain（覆盖已有）。env bypass 模式下只设 process env var。
pub fn set_password(password: &str) -> Result<()> {
    if std::env::var(ENV_BYPASS).is_ok() {
        std::env::set_var(ENV_BYPASS, password);
        return Ok(());
    }
    let entry = keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
        .map_err(|e| anyhow::anyhow!("keyring entry: {e}"))?;
    entry
        .set_password(password)
        .map_err(|e| anyhow::anyhow!("keyring write: {e}"))?;
    Ok(())
}

/// 删除 Keychain 里的密码条目；env bypass 模式下移除 env var。
/// 不存在不算错误（关闭加密时调用，本来就可能没设）。
pub fn delete_password() -> Result<()> {
    if std::env::var(ENV_BYPASS).is_ok() {
        std::env::remove_var(ENV_BYPASS);
        return Ok(());
    }
    let entry = keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
        .map_err(|e| anyhow::anyhow!("keyring entry: {e}"))?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(anyhow::anyhow!("keyring delete: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pragma_literal_quotes_password() {
        assert_eq!(pragma_literal("hello"), "'hello'");
    }

    #[test]
    fn pragma_literal_escapes_single_quote() {
        assert_eq!(pragma_literal("it's"), "'it''s'");
    }

    #[test]
    fn env_bypass_round_trips() {
        let prev = std::env::var(ENV_BYPASS).ok();
        std::env::set_var(ENV_BYPASS, "test-pw");
        assert_eq!(get_password().unwrap().as_deref(), Some("test-pw"));
        set_password("changed").unwrap();
        assert_eq!(get_password().unwrap().as_deref(), Some("changed"));
        delete_password().unwrap();
        // After delete in env-bypass mode the env var is gone.
        assert!(std::env::var(ENV_BYPASS).is_err());
        if let Some(v) = prev {
            std::env::set_var(ENV_BYPASS, v);
        }
    }
}
