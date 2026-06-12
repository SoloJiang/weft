//! 检测旧明文 SQLite 库；若存在则 rename 归档，不做数据迁移。

use anyhow::Result;
use std::path::{Path, PathBuf};

/// SQLite 明文库文件前 16 字节恒为 b"SQLite format 3\0"。
/// SQLCipher 加密库前 16 字节看上去是随机密文 — 不会匹配这个签名。
const SQLITE_PLAINTEXT_MAGIC: &[u8; 16] = b"SQLite format 3\0";

/// 如果 `path` 是明文 SQLite，rename 成 `<path>.legacy-plaintext.<unix_ts>` 并返回 Ok。
/// 不存在 / 已是加密 / 其它错误：见函数体语义。
pub fn archive_if_plaintext(path: &Path) -> Result<Option<PathBuf>> {
    use std::io::Read;
    if !path.exists() {
        return Ok(None);
    }

    let mut header = [0u8; 16];
    let n = std::fs::File::open(path)
        .and_then(|mut f| f.read(&mut header))
        .map_err(|e| anyhow::anyhow!("read header of {}: {e}", path.display()))?;

    if n < 16 || &header != SQLITE_PLAINTEXT_MAGIC {
        return Ok(None);
    }

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let archived = path.with_file_name(format!(
        "{}.legacy-plaintext.{}",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("weft.db"),
        ts
    ));
    std::fs::rename(path, &archived)
        .map_err(|e| anyhow::anyhow!("rename {} -> {}: {e}", path.display(), archived.display()))?;
    eprintln!(
        "[weft] archived legacy plaintext db: {} -> {}",
        path.display(),
        archived.display()
    );
    Ok(Some(archived))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn missing_file_is_noop() {
        let dir = tmp();
        let p = dir.path().join("nope.db");
        let r = archive_if_plaintext(&p).unwrap();
        assert!(r.is_none());
        assert!(!p.exists());
    }

    #[test]
    fn plaintext_file_is_renamed() {
        let dir = tmp();
        let p = dir.path().join("weft.db");
        std::fs::write(&p, b"SQLite format 3\0\x01\x02\x03\x04").unwrap();
        let archived = archive_if_plaintext(&p).unwrap().expect("renamed");
        assert!(!p.exists(), "original should be gone");
        assert!(archived.exists(), "archive should exist");
        let name = archived.file_name().unwrap().to_string_lossy().to_string();
        assert!(
            name.starts_with("weft.db.legacy-plaintext."),
            "archive name = {name}"
        );
    }

    #[test]
    fn non_magic_file_is_left_alone() {
        let dir = tmp();
        let p = dir.path().join("weft.db");
        std::fs::write(&p, b"this is not sqlite at all xxxxxx").unwrap();
        let r = archive_if_plaintext(&p).unwrap();
        assert!(r.is_none(), "non-magic file should be left alone");
        assert!(p.exists());
    }

    #[test]
    fn read_too_short_is_left_alone() {
        let dir = tmp();
        let p = dir.path().join("weft.db");
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(b"short").unwrap();
        let r = archive_if_plaintext(&p).unwrap();
        assert!(r.is_none());
        assert!(p.exists());
    }
}
