//! Sidecar: retained `NormEvent` type and OpenCode session-usage helpers.
//! The transcript-reading path (`read_transcript` / `read_claude` / `read_codex` /
//! `read_opencode`) was removed with the dead `Transcript` UI surface.

use sea_orm::{ConnectOptions, ConnectionTrait, Database, DbBackend, Statement};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// A normalized, tool-agnostic transcript event for the chat view.
#[derive(serde::Serialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum NormEvent {
    /// A conversation turn (human or agent prose).
    Message {
        role: String,
        text: String,
        ts: String,
    },
    /// An agent tool call, summarized to one line.
    Tool {
        name: String,
        summary: String,
        ts: String,
    },
}

// ---- OpenCode (read-only from its SQLite db, by session.directory) ----

/// `opencode*.db` 活跃库文件名判定。不同发布渠道把库写成 `opencode.db` /
/// `opencode-prod.db` / `opencode-stable.db` 等;排除 `-wal` / `-shm` 边车(它们不以 `.db` 结尾)。
fn is_opencode_db(name: &str) -> bool {
    name.starts_with("opencode") && name.ends_with(".db")
}

/// opencode 数据目录的候选位置(按出现顺序扫描,跨平台 + 跨渠道)。opencode 在
/// Linux **和** macOS 都用 XDG `~/.local/share/opencode`(实测 macOS 即此,**不是**
/// `~/Library/Application Support`,故不能只用 `dirs::data_dir()`——那会漏掉 mac);
/// Windows 用 `%APPDATA%\opencode`(= `dirs::data_dir()`)。`XDG_DATA_HOME` 若设则优先。
fn opencode_data_dirs() -> Vec<PathBuf> {
    let mut dirs_out: Vec<PathBuf> = Vec::new();
    let mut push = |p: PathBuf| {
        if !dirs_out.contains(&p) {
            dirs_out.push(p);
        }
    };
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME").filter(|s| !s.is_empty()) {
        push(PathBuf::from(xdg).join("opencode"));
    }
    if let Some(home) = dirs::home_dir() {
        push(home.join(".local/share/opencode"));
    }
    if let Some(data) = dirs::data_dir() {
        push(data.join("opencode")); // %APPDATA%\opencode on Windows
    }
    dirs_out
}

/// 所有候选 opencode DB 路径,按 mtime **新→旧**排序(活跃库优先)。覆盖渠道变体
/// (`opencode.db` / `opencode-prod.db` / `opencode-stable.db`)与跨平台数据目录。
/// **不返回单个"全局最新"** —— 那个库可能属于别的渠道、不含目标 session;调用方应逐个
/// 尝试直到找到拥有该 session 的库(见 [`opencode_session_usage`]).
fn opencode_db_candidates() -> Vec<PathBuf> {
    let mut found: Vec<(SystemTime, PathBuf)> = Vec::new();
    for dir in opencode_data_dirs() {
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in read.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !is_opencode_db(name) {
                continue;
            }
            let Ok(mtime) = entry.metadata().and_then(|m| m.modified()) else {
                continue;
            };
            found.push((mtime, entry.path()));
        }
    }
    found.sort_by(|a, b| b.0.cmp(&a.0)); // 新→旧
    found.into_iter().map(|(_, p)| p).collect()
}

/// 会话信息面板(M2):opencode 会话的「当前上下文」usage + model,按 session id(= 我们的
/// native_id)取**最近一条 assistant 消息**:`tokens.input + tokens.cache.read` 是当前上下文
/// (不用 `session.tokens_*` 列——那是整会话**累计**值,会随轮次无界增长、超出 window),
/// model 取 message 行的 `providerID`/`modelID`(不用 `session.model` 列——当前 opencode 对
/// 最新会话该列常为空,model 身份只在 message 行可靠)。ro 连接,不扰动 live(WAL)db。
pub async fn opencode_session_usage(native_id: &str) -> Option<(u64, Option<String>)> {
    // 多渠道库并存时逐个尝试,返回**拥有该 session** 的库给出的结果——光取"全局最新库"
    // 可能落在别的渠道、查不到该 session(见 opencode_db_candidates)。
    for db in opencode_db_candidates() {
        if let Some(found) = usage_from_db(&db, native_id).await {
            return Some(found);
        }
    }
    None
}

/// 单个 DB 里查该 session 最近一条 assistant 消息的 usage + model;该库不含此 session
/// (查不到行)→ `None`,调用方接着试下一个候选库。
async fn usage_from_db(db: &Path, native_id: &str) -> Option<(u64, Option<String>)> {
    let url = format!("sqlite://{}?mode=ro", db.to_string_lossy());
    let mut opt = ConnectOptions::new(url);
    opt.max_connections(1).sqlx_logging(false);
    let conn = Database::connect(opt).await.ok()?;
    // ctx + model 都从同一条「最近 assistant 消息」行取。
    let rows = conn
        .query_all(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT json_extract(data, '$.tokens.input') AS ti, \
             json_extract(data, '$.tokens.cache.read') AS tcr, \
             json_extract(data, '$.providerID') AS pid, \
             json_extract(data, '$.modelID') AS mid \
             FROM message \
             WHERE session_id = ? AND json_extract(data, '$.role') = 'assistant' \
             ORDER BY time_created DESC LIMIT 1",
            vec![native_id.into()],
        ))
        .await
        .ok()?;
    let r = rows.first()?; // 该库无此 session 的 assistant 行 → None,试下一个候选库
    let ti: i64 = r.try_get("", "ti").unwrap_or(0);
    let tcr: i64 = r.try_get("", "tcr").unwrap_or(0);
    let model = crate::session_meta::opencode_model_json(
        r.try_get::<String>("", "pid").ok(),
        r.try_get::<String>("", "mid").ok(),
    );
    Some(((ti.max(0) as u64) + (tcr.max(0) as u64), model))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opencode_db_filename_match() {
        // 渠道变体命中,WAL/SHM 边车与无关库不命中。
        assert!(is_opencode_db("opencode.db"));
        assert!(is_opencode_db("opencode-prod.db"));
        assert!(is_opencode_db("opencode-stable.db"));
        assert!(!is_opencode_db("opencode.db-wal"));
        assert!(!is_opencode_db("opencode.db-shm"));
        assert!(!is_opencode_db("other.db"));
        assert!(!is_opencode_db("opencode.sqlite"));
    }

    #[test]
    fn norm_event_serializes() {
        let ev = NormEvent::Message {
            role: "user".into(),
            text: "hello".into(),
            ts: "t0".into(),
        };
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(json.get("kind").and_then(|v| v.as_str()), Some("message"));
    }
}
