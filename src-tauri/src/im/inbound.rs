//! 入站路由（spec §4 顺序判定）：归一化事件 → Route。纯函数、无 IO、无 LLM。
//! M1 范围：绑定 / 卡片按钮 / 卡片回复作答 / 自由文本提示；群消息 M2。

use crate::im::{CardIndex, ReplyTarget};

#[derive(Clone, Debug, PartialEq)]
pub enum Inbound {
    /// 卡片按钮回调（CARD_BUTTONS 启用时才会出现）。
    Action { operator_open_id: String, message_id: String, value: serde_json::Value },
    Text {
        sender_open_id: String,
        chat_type: String, // "p2p" | "group"
        message_id: String,
        parent_id: Option<String>,
        text: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Route {
    Ignore,
    /// 白名单为空时首个私聊发送者自动绑定为 owner。
    Bind { open_id: String },
    AnswerPerm { ask_id: u64, answer: String },
    AnswerHuman { thread: i32, ask_id: u64, text: String },
    /// 回复了权限卡但动词解析不出 → 回用法提示。
    BadVerdict,
    /// 单聊自由文本：M1 回「Concierge M3 上线」提示。
    FreeText,
}

/// 中英动词/序号 → 标准答案。与 outbound 权限卡提示文案是共享协议（见
/// outbound.rs 提示行注释）：1=允许 2=拒绝 3=总是 4=放行，改序必须同步。
pub fn parse_verdict(text: &str) -> Option<&'static str> {
    match text.trim().to_lowercase().as_str() {
        "允许" | "allow" | "1" => Some("allow"),
        "拒绝" | "deny" | "2" => Some("deny"),
        "总是" | "always" | "3" => Some("always"),
        "放行" | "full" | "4" => Some("full"),
        _ => None,
    }
}

pub fn route(inb: &Inbound, allow: &[String], cards: &CardIndex) -> Route {
    match inb {
        Inbound::Action { operator_open_id, value, .. } => {
            if !allow.iter().any(|a| a == operator_open_id) {
                return Route::Ignore;
            }
            let kind = value.get("kind").and_then(|v| v.as_str());
            let ask_id = value.get("ask_id").and_then(|v| v.as_u64());
            let answer = value.get("answer").and_then(|v| v.as_str());
            match (kind, ask_id, answer) {
                (Some("perm"), Some(id), Some(ans)) => {
                    Route::AnswerPerm { ask_id: id, answer: ans.to_string() }
                }
                _ => Route::Ignore,
            }
        }
        Inbound::Text { sender_open_id, chat_type, parent_id, text, .. } => {
            if chat_type != "p2p" {
                return Route::Ignore; // 群路由是 M2（im_route 表）
            }
            if allow.is_empty() {
                return Route::Bind { open_id: sender_open_id.clone() };
            }
            if !allow.iter().any(|a| a == sender_open_id) {
                return Route::Ignore;
            }
            if let Some(pid) = parent_id {
                match cards.target_of(pid) {
                    Some(ReplyTarget::Perm { ask_id }) => {
                        return match parse_verdict(text) {
                            Some(ans) => Route::AnswerPerm { ask_id, answer: ans.into() },
                            None => Route::BadVerdict,
                        };
                    }
                    Some(ReplyTarget::Human { thread, ask_id }) => {
                        return Route::AnswerHuman { thread, ask_id, text: text.clone() };
                    }
                    None => {}
                }
            }
            Route::FreeText
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::im::{CardIndex, ReplyTarget};

    fn text(sender: &str, parent: Option<&str>, body: &str) -> Inbound {
        Inbound::Text {
            sender_open_id: sender.into(),
            chat_type: "p2p".into(),
            message_id: "om_in".into(),
            parent_id: parent.map(|s| s.to_string()),
            text: body.into(),
        }
    }

    fn cards() -> CardIndex {
        let mut c = CardIndex::default();
        c.record_perm(42, "om_perm", "Run: npm test");
        c.record_human(3, 9, "om_q");
        c
    }

    #[test]
    fn empty_allowlist_binds_first_p2p_sender() {
        assert_eq!(
            route(&text("ou_x", None, "hi"), &[], &cards()),
            Route::Bind { open_id: "ou_x".into() }
        );
    }

    #[test]
    fn unknown_sender_is_ignored() {
        let allow = vec!["ou_me".to_string()];
        assert_eq!(route(&text("ou_evil", None, "允许"), &allow, &cards()), Route::Ignore);
    }

    #[test]
    fn reply_to_perm_card_parses_verdict() {
        let allow = vec!["ou_me".to_string()];
        assert_eq!(
            route(&text("ou_me", Some("om_perm"), " 允许 "), &allow, &cards()),
            Route::AnswerPerm { ask_id: 42, answer: "allow".into() }
        );
        assert_eq!(
            route(&text("ou_me", Some("om_perm"), "2"), &allow, &cards()),
            Route::AnswerPerm { ask_id: 42, answer: "deny".into() }
        );
        assert_eq!(
            route(&text("ou_me", Some("om_perm"), "whatever"), &allow, &cards()),
            Route::BadVerdict
        );
    }

    #[test]
    fn reply_to_human_card_routes_raw_text() {
        let allow = vec!["ou_me".to_string()];
        assert_eq!(
            route(&text("ou_me", Some("om_q"), "minor 就行"), &allow, &cards()),
            Route::AnswerHuman { thread: 3, ask_id: 9, text: "minor 就行".into() }
        );
    }

    #[test]
    fn free_p2p_text_hints_and_group_is_ignored_in_m1() {
        let allow = vec!["ou_me".to_string()];
        assert_eq!(route(&text("ou_me", None, "今天进展如何"), &allow, &cards()), Route::FreeText);
        let g = Inbound::Text {
            sender_open_id: "ou_me".into(),
            chat_type: "group".into(),
            message_id: "om".into(),
            parent_id: None,
            text: "hi".into(),
        };
        assert_eq!(route(&g, &allow, &cards()), Route::Ignore);
    }

    #[test]
    fn card_action_routes_when_whitelisted() {
        let allow = vec!["ou_me".to_string()];
        let a = Inbound::Action {
            operator_open_id: "ou_me".into(),
            message_id: "om_perm".into(),
            value: serde_json::json!({"kind": "perm", "ask_id": 42, "answer": "allow"}),
        };
        assert_eq!(
            route(&a, &allow, &cards()),
            Route::AnswerPerm { ask_id: 42, answer: "allow".into() }
        );
        let evil = Inbound::Action {
            operator_open_id: "ou_evil".into(),
            message_id: "om_perm".into(),
            value: serde_json::json!({"kind": "perm", "ask_id": 42, "answer": "allow"}),
        };
        assert_eq!(route(&evil, &allow, &cards()), Route::Ignore);
    }

    #[test]
    fn verdict_protocol_locks_numeric_ordering() {
        // 与 outbound 权限卡提示「允许/拒绝/总是/放行（或 1/2/3/4）」的共享协议锚定：
        // 改任何一边的顺序都必须同步另一边（错序后果 = 想拒绝却放行）。
        assert_eq!(parse_verdict("1"), Some("allow"));
        assert_eq!(parse_verdict("2"), Some("deny"));
        assert_eq!(parse_verdict("3"), Some("always"));
        assert_eq!(parse_verdict("4"), Some("full"));
        assert_eq!(parse_verdict("允许"), Some("allow"));
        assert_eq!(parse_verdict("拒绝"), Some("deny"));
        assert_eq!(parse_verdict("总是"), Some("always"));
        assert_eq!(parse_verdict("放行"), Some("full"));
        assert_eq!(parse_verdict("ALLOW"), Some("allow")); // 大小写不敏感
        assert_eq!(parse_verdict("5"), None);
    }
}
