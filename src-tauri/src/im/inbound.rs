//! 入站路由（spec §4 顺序判定）：归一化事件 → Route。纯函数、无 IO、无 LLM。
//! M1 范围：绑定 / 卡片按钮 / 卡片回复作答 / 自由文本提示；群消息 M2。

use crate::ask::Answer;
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
    ///
    /// 契约：route 读的是 allow 的内存快照；执行侧落库前必须重查白名单
    /// 仍为空（防并发首绑竞态——两条消息同时拿到空快照会各自 Bind）。
    /// 桥运行时单循环串行消费是第一道防线，但不应是唯一防线。
    Bind { open_id: String },
    AnswerPerm { ask_id: u64, answer: Answer },
    AnswerHuman { thread: i32, ask_id: u64, text: String },
    /// 回复了权限卡但动词解析不出 → 回用法提示。
    BadVerdict,
    /// 单聊自由文本：M1 回「Concierge M3 上线」提示。
    FreeText,
}

/// 中英动词/序号 → `ask::Answer`。与 outbound 权限卡提示文案是共享协议（见
/// outbound.rs 提示行注释）：1=允许 2=拒绝 3=总是 4=放行，改序必须同步。
pub fn parse_verdict(text: &str) -> Option<Answer> {
    match text.trim().to_lowercase().as_str() {
        "允许" | "allow" | "1" => Some(Answer::Allow),
        "拒绝" | "deny" | "2" => Some(Answer::Deny),
        "总是" | "always" | "3" => Some(Answer::Always),
        "放行" | "full" | "4" => Some(Answer::Full),
        _ => None,
    }
}

/// 按钮 value 里的 ask_id 双形态解析：outbound 写入的是数字（见 outbound.rs
/// CARD_BUTTONS 的按钮 value），但飞书回调 JSON 往返后可能变字符串——实际
/// 形态待 Task 2 spike 验证，先两头都兜住。
fn as_ask_id(v: &serde_json::Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

pub fn route(inb: &Inbound, allow: &[String], cards: &CardIndex) -> Route {
    match inb {
        Inbound::Action { operator_open_id, value, .. } => {
            // 空白名单下 Action 也是 Ignore：Bind 唯一入口是 p2p Text。
            if !allow.iter().any(|a| a == operator_open_id) {
                return Route::Ignore;
            }
            let kind = value.get("kind").and_then(|v| v.as_str());
            let ask_id = value.get("ask_id").and_then(as_ask_id);
            let answer = value.get("answer").and_then(|v| v.as_str()).and_then(Answer::parse);
            match (kind, ask_id, answer) {
                (Some("perm"), Some(id), Some(ans)) => {
                    Route::AnswerPerm { ask_id: id, answer: ans }
                }
                // fail-closed：kind/ask_id/answer 任一非法即丢弃，与文本
                // 回复路径的 parse_verdict 同强度不变式。
                _ => Route::Ignore,
            }
        }
        Inbound::Text { sender_open_id, chat_type, parent_id, text, .. } => {
            // 群判定必须在 Bind 之前：空白名单 + 群消息不得触发绑定。
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
                            Some(ans) => Route::AnswerPerm { ask_id, answer: ans },
                            None => Route::BadVerdict,
                        };
                    }
                    Some(ReplyTarget::Human { thread, ask_id }) => {
                        return Route::AnswerHuman { thread, ask_id, text: text.clone() };
                    }
                    // parent_id 不命中索引（卡已终态/重启丢索引/回复无关消息）
                    // → fall through 当自由文本，不猜测语义。
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
    use crate::im::CardIndex;

    fn text(sender: &str, parent: Option<&str>, body: &str) -> Inbound {
        Inbound::Text {
            sender_open_id: sender.into(),
            chat_type: "p2p".into(),
            message_id: "om_in".into(),
            parent_id: parent.map(|s| s.to_string()),
            text: body.into(),
        }
    }

    fn action(operator: &str, value: serde_json::Value) -> Inbound {
        Inbound::Action {
            operator_open_id: operator.into(),
            message_id: "om_perm".into(),
            value,
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
            Route::AnswerPerm { ask_id: 42, answer: Answer::Allow }
        );
        assert_eq!(
            route(&text("ou_me", Some("om_perm"), "2"), &allow, &cards()),
            Route::AnswerPerm { ask_id: 42, answer: Answer::Deny }
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
    fn reply_to_unknown_card_falls_through_to_free_text() {
        // parent_id 不命中索引时即使文本是合法 verdict 也不作答——锁定
        // fall-through 语义（不许猜「大概是回最近那张卡」）。
        let allow = vec!["ou_me".to_string()];
        assert_eq!(
            route(&text("ou_me", Some("om_gone"), "允许"), &allow, &cards()),
            Route::FreeText
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
    fn group_with_empty_allowlist_never_binds() {
        // 顺序锁：group 判定必须在 Bind 之前。调换 route 里这两个 if 的
        // 顺序（先查空白名单）时此测试必红——群成员不得成为首绑 owner。
        let g = Inbound::Text {
            sender_open_id: "ou_stranger".into(),
            chat_type: "group".into(),
            message_id: "om".into(),
            parent_id: None,
            text: "hi".into(),
        };
        assert_eq!(route(&g, &[], &cards()), Route::Ignore);
    }

    #[test]
    fn card_action_routes_when_whitelisted() {
        let allow = vec!["ou_me".to_string()];
        let ok = serde_json::json!({"kind": "perm", "ask_id": 42, "answer": "allow"});
        assert_eq!(
            route(&action("ou_me", ok.clone()), &allow, &cards()),
            Route::AnswerPerm { ask_id: 42, answer: Answer::Allow }
        );
        assert_eq!(route(&action("ou_evil", ok), &allow, &cards()), Route::Ignore);
    }

    #[test]
    fn card_action_with_empty_allowlist_is_ignored() {
        // Bind 唯一入口是 p2p Text；Action 不触发绑定（M1 默认无按钮，
        // 且未绑定时不该有任何已发卡片可点）。
        let a = action("ou_x", serde_json::json!({"kind": "perm", "ask_id": 42, "answer": "allow"}));
        assert_eq!(route(&a, &[], &cards()), Route::Ignore);
    }

    #[test]
    fn card_action_parses_string_ask_id_and_rejects_garbage() {
        let allow = vec!["ou_me".to_string()];
        // 飞书回调 JSON 往返后 ask_id 可能变字符串——双形态都路由成功。
        assert_eq!(
            route(
                &action("ou_me", serde_json::json!({"kind": "perm", "ask_id": "42", "answer": "allow"})),
                &allow,
                &cards()
            ),
            Route::AnswerPerm { ask_id: 42, answer: Answer::Allow }
        );
        assert_eq!(
            route(
                &action("ou_me", serde_json::json!({"kind": "perm", "ask_id": "abc", "answer": "allow"})),
                &allow,
                &cards()
            ),
            Route::Ignore
        );
    }

    #[test]
    fn card_action_fail_closed_on_bad_kind_or_answer() {
        let allow = vec!["ou_me".to_string()];
        // kind != "perm" → Ignore（未来新增 kind 前先在这里解锁）。
        assert_eq!(
            route(
                &action("ou_me", serde_json::json!({"kind": "human", "ask_id": 42, "answer": "allow"})),
                &allow,
                &cards()
            ),
            Route::Ignore
        );
        // answer 不是 Answer::parse 认可的四个字面量 → Ignore（fail-closed，
        // 与文本回复路径 parse_verdict 同强度不变式）。
        assert_eq!(
            route(
                &action("ou_me", serde_json::json!({"kind": "perm", "ask_id": 42, "answer": "yolo"})),
                &allow,
                &cards()
            ),
            Route::Ignore
        );
    }

    #[test]
    fn verdict_protocol_locks_numeric_ordering() {
        // 与 outbound 权限卡提示「允许/拒绝/总是/放行（或 1/2/3/4）」的共享协议锚定：
        // 改任何一边的顺序都必须同步另一边（错序后果 = 想拒绝却放行）。
        assert_eq!(parse_verdict("1"), Some(Answer::Allow));
        assert_eq!(parse_verdict("2"), Some(Answer::Deny));
        assert_eq!(parse_verdict("3"), Some(Answer::Always));
        assert_eq!(parse_verdict("4"), Some(Answer::Full));
        assert_eq!(parse_verdict("允许"), Some(Answer::Allow));
        assert_eq!(parse_verdict("拒绝"), Some(Answer::Deny));
        assert_eq!(parse_verdict("总是"), Some(Answer::Always));
        assert_eq!(parse_verdict("放行"), Some(Answer::Full));
        assert_eq!(parse_verdict("ALLOW"), Some(Answer::Allow)); // 大小写不敏感
        assert_eq!(parse_verdict("5"), None);
    }
}
