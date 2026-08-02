//! 入站路由（spec §4 顺序判定）：归一化事件 → Route。纯函数、无 IO、无 LLM。
//! 当前覆盖 owner 绑定、卡片回复/按钮路由、provider thread 绑定、thread 消息与 Concierge 私聊。

use crate::ask::Answer;
use crate::im::{CardIndex, ImProvider, ReplyTarget};

#[derive(Clone, Debug, PartialEq)]
pub enum Inbound {
    /// 卡片按钮回调（CARD_BUTTONS 启用时才会出现）。
    Action {
        operator_open_id: String,
        message_id: String,
        value: serde_json::Value,
    },
    Text {
        sender_open_id: String,
        chat_type: String,         // "p2p" | "group"
        chat_id: String,           // provider 群/单聊 id（群路由用，M2-3）
        thread_id: Option<String>, // provider 原生 thread/topic id
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
    Bind {
        open_id: String,
        chat_id: String,
        reply_to: String,
        text: String,
    },
    /// 已绑定 owner 在 provider thread 里发送 `/bind <thread_id>`，把当前
    /// thread 绑定到指定 Weft issue。群消息仍不能绑定 owner；只有 allowlist
    /// 中的 sender 可以改 issue↔thread 路由。
    BindIssueThread {
        thread_id: i32,
        chat_id: String,
        im_thread_ref: String,
        /// The `/bind` message id (a member of this topic) — recorded as a
        /// replyable seed so desktop-driven / no-ack lead replies have a valid
        /// `om_*` target instead of the non-replyable `omt_*` topic id.
        seed_message_id: String,
    },
    /// 已绑定 owner 在飞书群普通消息里发送 `/topic <thread_id>`，为已有
    /// Weft issue 创建或复用一个飞书 topic。
    EnsureIssueTopic {
        thread_id: i32,
        chat_id: String,
        reply_to: String,
    },
    AnswerPerm {
        ask_id: u64,
        answer: Answer,
    },
    AnswerHuman {
        thread: i32,
        ask_id: u64,
        text: String,
    },
    /// 回复了权限卡但动词解析不出 → 回用法提示。
    BadVerdict,
    /// 显式 `/answer <issue-id> <ask-id> <text>` 缺参数或 id 非法。
    BadHumanAnswer,
    /// 当前消息不在可绑定的 provider thread 中，或 provider 的机器人 API
    /// 不能可靠创建并返回新 thread id；引导用户先进入原生 thread 再绑定。
    IssueThreadRequired,
    /// provider thread 里给已绑定 issue 的自由文本 → 灌进 lead engine。
    /// 解析路径在 inbound 之外：执行侧用 (chat_id, im_thread_ref) 查 im_route。
    IssueMessage {
        chat_id: String,
        im_thread_ref: String,
        sender_open_id: String,
        text: String,
    },
    /// 私聊或普通群聊自由文本：接入该 IM 会话独立的 Concierge engine。
    FreeText {
        sender_open_id: String,
        chat_id: String,
        im_thread_ref: String,
        reply_to: Option<String>,
        text: String,
    },
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
/// CARD_BUTTONS 的按钮 value），但飞书回调 JSON 往返后可能变字符串——两头都兜住。
fn as_ask_id(v: &serde_json::Value) -> Option<u64> {
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

fn parse_bind_issue(text: &str) -> Option<i32> {
    let mut parts = text.split_whitespace();
    let command = parts.next()?.to_ascii_lowercase();
    match (command.as_str(), parts.next(), parts.next()) {
        ("/bind", Some(id), None) => id.parse::<i32>().ok().filter(|n| *n > 0),
        _ => None,
    }
}

fn parse_topic_issue(text: &str) -> Option<i32> {
    let mut parts = text.split_whitespace();
    let command = parts.next()?.to_ascii_lowercase();
    match (command.as_str(), parts.next(), parts.next()) {
        ("/topic" | "/issue", Some(id), None) => id.parse::<i32>().ok().filter(|n| *n > 0),
        _ => None,
    }
}

fn first_command(text: &str) -> Option<String> {
    text.split_whitespace()
        .next()
        .map(|part| part.to_ascii_lowercase())
}

fn parse_permission_command(text: &str) -> Option<(u64, Answer)> {
    let mut parts = text.split_whitespace();
    let answer = match parts.next()?.to_ascii_lowercase().as_str() {
        "/allow" => Answer::Allow,
        "/deny" => Answer::Deny,
        "/always" => Answer::Always,
        "/full" => Answer::Full,
        _ => return None,
    };
    let ask_id = parts.next()?.parse::<u64>().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((ask_id, answer))
}

fn is_permission_command(text: &str) -> bool {
    matches!(
        text.split_whitespace()
            .next()
            .map(|part| part.to_ascii_lowercase()),
        Some(command)
            if matches!(command.as_str(), "/allow" | "/deny" | "/always" | "/full")
    )
}

fn parse_human_answer_command(text: &str) -> Option<(i32, u64, String)> {
    fn next_token<'a>(text: &'a str, cursor: &mut usize) -> Option<&'a str> {
        let remaining = &text[*cursor..];
        let start = remaining
            .char_indices()
            .find(|(_, ch)| !ch.is_whitespace())?
            .0;
        let start = *cursor + start;
        let end = text[start..]
            .char_indices()
            .find(|(_, ch)| ch.is_whitespace())
            .map(|(offset, _)| start + offset)
            .unwrap_or(text.len());
        *cursor = end;
        Some(&text[start..end])
    }

    let mut cursor = 0;
    if next_token(text, &mut cursor)?.to_ascii_lowercase() != "/answer" {
        return None;
    }
    let thread = next_token(text, &mut cursor)?
        .parse::<i32>()
        .ok()
        .filter(|id| *id > 0)?;
    let ask_id = next_token(text, &mut cursor)?.parse::<u64>().ok()?;
    let remainder = &text[cursor..];
    let separator = remainder.chars().next()?;
    if !separator.is_whitespace() {
        return None;
    }
    let mut answer_start = cursor + separator.len_utf8();
    if separator == '\r' && text[answer_start..].starts_with('\n') {
        answer_start += '\n'.len_utf8();
    }
    let answer = &text[answer_start..];
    if answer.trim().is_empty() {
        return None;
    }
    Some((thread, ask_id, answer.to_string()))
}

fn is_human_answer_command(text: &str) -> bool {
    first_command(text).as_deref() == Some("/answer")
}

pub fn route(inb: &Inbound, allow: &[String], cards: &CardIndex) -> Route {
    route_for_provider(inb, allow, cards, ImProvider::Feishu)
}

pub fn route_for_provider(
    inb: &Inbound,
    allow: &[String],
    cards: &CardIndex,
    provider: ImProvider,
) -> Route {
    match inb {
        Inbound::Action {
            operator_open_id,
            value,
            ..
        } => {
            // 空白名单下 Action 也是 Ignore：Bind 唯一入口是 p2p Text。
            if !allow.iter().any(|a| a == operator_open_id) {
                return Route::Ignore;
            }
            let kind = value.get("kind").and_then(|v| v.as_str());
            let ask_id = value.get("ask_id").and_then(as_ask_id);
            let answer = value
                .get("answer")
                .and_then(|v| v.as_str())
                .and_then(Answer::parse);
            match (kind, ask_id, answer) {
                (Some("perm"), Some(id), Some(ans)) => Route::AnswerPerm {
                    ask_id: id,
                    answer: ans,
                },
                // fail-closed：kind/ask_id/answer 任一非法即丢弃，与文本
                // 回复路径的 parse_verdict 同强度不变式。
                _ => Route::Ignore,
            }
        }
        Inbound::Text {
            sender_open_id,
            chat_type,
            chat_id,
            thread_id,
            message_id,
            parent_id,
            text,
        } => {
            let sender_allowed = allow.iter().any(|a| a == sender_open_id);
            // Explicit-ID slash commands are the deterministic fallback for
            // DingTalk, which has no stable Feishu-style reply-parent card
            // contract. Feishu must resolve its indexed parent first so an
            // otherwise command-shaped human answer is delivered verbatim.
            if provider == ImProvider::DingTalk && sender_allowed {
                if let Some((ask_id, answer)) = parse_permission_command(text) {
                    return Route::AnswerPerm { ask_id, answer };
                }
                if is_permission_command(text) {
                    return Route::BadVerdict;
                }
                if let Some((thread, ask_id, text)) = parse_human_answer_command(text) {
                    return Route::AnswerHuman {
                        thread,
                        ask_id,
                        text,
                    };
                }
                if is_human_answer_command(text) {
                    return Route::BadHumanAnswer;
                }
            }
            // 群消息：不能绑定 owner。原生 thread 内 `/bind <thread_id>` 只有
            // 已绑定 owner 可用；其余 thread 消息由执行侧用 (chat_id,
            // thread_id) 查 im_route 决定路由。
            if chat_type != "p2p" {
                let command = first_command(text);
                // 钉钉支持话题圈/串聊，但企业机器人的发送接口不能可靠返回
                // 新建 thread 的 openConvThreadId。不要把普通 conversationId
                // 伪装成 thread；要求用户先在钉钉进入真实 thread 再 `/bind`。
                if provider == ImProvider::DingTalk
                    && sender_allowed
                    && matches!(command.as_deref(), Some("/topic" | "/issue"))
                {
                    return Route::IssueThreadRequired;
                }
                return match thread_id {
                    Some(tref) => {
                        if command.as_deref() == Some("/bind") {
                            if allow.iter().any(|a| a == sender_open_id) {
                                if let Some(thread_id) = parse_bind_issue(text) {
                                    return Route::BindIssueThread {
                                        thread_id,
                                        chat_id: chat_id.clone(),
                                        im_thread_ref: tref.clone(),
                                        seed_message_id: message_id.clone(),
                                    };
                                }
                            }
                            return Route::Ignore;
                        }
                        Route::IssueMessage {
                            chat_id: chat_id.clone(),
                            im_thread_ref: tref.clone(),
                            sender_open_id: sender_open_id.clone(),
                            text: text.clone(),
                        }
                    }
                    None => {
                        // 普通群聊没有飞书 thread_id；只有已绑定 owner 可以为已有
                        // issue 创建 topic，避免机器人被拉进群后被任意群成员刷 route。
                        if allow.iter().any(|a| a == sender_open_id) {
                            if let Some(thread_id) = parse_topic_issue(text) {
                                return Route::EnsureIssueTopic {
                                    thread_id,
                                    chat_id: chat_id.clone(),
                                    reply_to: message_id.clone(),
                                };
                            }
                            if command.as_deref() == Some("/bind") {
                                return if provider == ImProvider::DingTalk {
                                    Route::IssueThreadRequired
                                } else {
                                    Route::Ignore
                                };
                            }
                            Route::FreeText {
                                sender_open_id: sender_open_id.clone(),
                                chat_id: chat_id.clone(),
                                im_thread_ref: format!("chat:{chat_id}"),
                                reply_to: Some(message_id.clone()),
                                text: text.clone(),
                            }
                        } else {
                            Route::Ignore
                        }
                    }
                };
            }
            if allow.is_empty() {
                return Route::Bind {
                    open_id: sender_open_id.clone(),
                    chat_id: chat_id.clone(),
                    reply_to: message_id.clone(),
                    text: text.clone(),
                };
            }
            if !sender_allowed {
                return Route::Ignore;
            }
            if let Some(pid) = parent_id {
                match cards.target_of(pid) {
                    Some(ReplyTarget::Perm { ask_id }) => {
                        return match parse_verdict(text) {
                            Some(ans) => Route::AnswerPerm {
                                ask_id,
                                answer: ans,
                            },
                            None => Route::BadVerdict,
                        };
                    }
                    Some(ReplyTarget::Human { thread, ask_id }) => {
                        return Route::AnswerHuman {
                            thread,
                            ask_id,
                            text: text.clone(),
                        };
                    }
                    // parent_id 不命中索引（卡已终态/重启丢索引/回复无关消息）
                    // → fall through 当自由文本，不猜测语义。
                    None => {}
                }
            }
            Route::FreeText {
                sender_open_id: sender_open_id.clone(),
                chat_id: chat_id.clone(),
                im_thread_ref: format!("dm:{sender_open_id}"),
                reply_to: Some(message_id.clone()),
                text: text.clone(),
            }
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
            chat_id: "oc_dm".into(),
            thread_id: None,
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
    fn dingtalk_empty_allowlist_binds_first_dm_with_reply_target() {
        assert_eq!(
            route_for_provider(
                &text("staff_x", None, "继续 issue"),
                &[],
                &cards(),
                ImProvider::DingTalk,
            ),
            Route::Bind {
                open_id: "staff_x".into(),
                chat_id: "oc_dm".into(),
                reply_to: "om_in".into(),
                text: "继续 issue".into(),
            }
        );
    }

    #[test]
    fn unknown_sender_is_ignored() {
        let allow = vec!["ou_me".to_string()];
        assert_eq!(
            route(&text("ou_evil", None, "允许"), &allow, &cards()),
            Route::Ignore
        );
    }

    #[test]
    fn reply_to_perm_card_parses_verdict() {
        let allow = vec!["ou_me".to_string()];
        assert_eq!(
            route(&text("ou_me", Some("om_perm"), " 允许 "), &allow, &cards()),
            Route::AnswerPerm {
                ask_id: 42,
                answer: Answer::Allow
            }
        );
        assert_eq!(
            route(&text("ou_me", Some("om_perm"), "2"), &allow, &cards()),
            Route::AnswerPerm {
                ask_id: 42,
                answer: Answer::Deny
            }
        );
        assert_eq!(
            route(
                &text("ou_me", Some("om_perm"), "whatever"),
                &allow,
                &cards()
            ),
            Route::BadVerdict
        );
    }

    #[test]
    fn explicit_permission_commands_work_without_reply_parent() {
        let allow = vec!["ou_me".to_string()];
        assert_eq!(
            route_for_provider(
                &text("ou_me", None, "/allow 42"),
                &allow,
                &cards(),
                ImProvider::DingTalk,
            ),
            Route::AnswerPerm {
                ask_id: 42,
                answer: Answer::Allow,
            }
        );
        assert_eq!(
            route_for_provider(
                &text("ou_me", None, "/full nope"),
                &allow,
                &cards(),
                ImProvider::DingTalk,
            ),
            Route::BadVerdict
        );
    }

    #[test]
    fn explicit_human_answer_command_carries_thread_and_text() {
        let allow = vec!["ou_me".to_string()];
        assert_eq!(
            route_for_provider(
                &text("ou_me", None, "/answer 7 9 ship minor"),
                &allow,
                &cards(),
                ImProvider::DingTalk,
            ),
            Route::AnswerHuman {
                thread: 7,
                ask_id: 9,
                text: "ship minor".into(),
            }
        );
        assert_eq!(
            route_for_provider(
                &text("ou_me", None, "/answer 7 9"),
                &allow,
                &cards(),
                ImProvider::DingTalk,
            ),
            Route::BadHumanAnswer
        );
        assert_eq!(
            route_for_provider(
                &text(
                    "ou_me",
                    None,
                    "/answer 7 9\n    let  value = 1;\n\n  keep indent",
                ),
                &allow,
                &cards(),
                ImProvider::DingTalk,
            ),
            Route::AnswerHuman {
                thread: 7,
                ask_id: 9,
                text: "    let  value = 1;\n\n  keep indent".into(),
            }
        );
    }

    #[test]
    fn reply_to_human_card_routes_raw_text() {
        let allow = vec!["ou_me".to_string()];
        assert_eq!(
            route(&text("ou_me", Some("om_q"), "minor 就行"), &allow, &cards()),
            Route::AnswerHuman {
                thread: 3,
                ask_id: 9,
                text: "minor 就行".into()
            }
        );
    }

    #[test]
    fn feishu_human_card_reply_wins_over_dingtalk_slash_commands() {
        let allow = vec!["ou_me".to_string()];
        assert_eq!(
            route(&text("ou_me", Some("om_q"), "/allow 42"), &allow, &cards(),),
            Route::AnswerHuman {
                thread: 3,
                ask_id: 9,
                text: "/allow 42".into(),
            }
        );
    }

    #[test]
    fn reply_to_unknown_card_falls_through_to_free_text() {
        // parent_id 不命中索引时即使文本是合法 verdict 也不作答——锁定
        // fall-through 语义（不许猜「大概是回最近那张卡」）。
        let allow = vec!["ou_me".to_string()];
        assert_eq!(
            route(&text("ou_me", Some("om_gone"), "允许"), &allow, &cards()),
            Route::FreeText {
                sender_open_id: "ou_me".into(),
                chat_id: "oc_dm".into(),
                im_thread_ref: "dm:ou_me".into(),
                reply_to: Some("om_in".into()),
                text: "允许".into(),
            }
        );
    }

    #[test]
    fn free_p2p_text_hints_and_group_routes_or_ignores() {
        let allow = vec!["ou_me".to_string()];
        assert_eq!(
            route(&text("ou_me", None, "今天进展如何"), &allow, &cards()),
            Route::FreeText {
                sender_open_id: "ou_me".into(),
                chat_id: "oc_dm".into(),
                im_thread_ref: "dm:ou_me".into(),
                reply_to: Some("om_in".into()),
                text: "今天进展如何".into(),
            }
        );
        let g_no_thread = Inbound::Text {
            sender_open_id: "ou_me".into(),
            chat_type: "group".into(),
            chat_id: "oc_g".into(),
            thread_id: None,
            message_id: "om".into(),
            parent_id: None,
            text: "hi".into(),
        };
        // 普通群消息来自 owner：交给 Concierge 语义判断；chat_id 注入上下文，
        // 让它只在用户语义明确时调用 ensure_issue_topic。
        assert_eq!(
            route(&g_no_thread, &allow, &cards()),
            Route::FreeText {
                sender_open_id: "ou_me".into(),
                chat_id: "oc_g".into(),
                im_thread_ref: "chat:oc_g".into(),
                reply_to: Some("om".into()),
                text: "hi".into(),
            }
        );

        let g_in_thread = Inbound::Text {
            sender_open_id: "ou_x".into(),
            chat_type: "group".into(),
            chat_id: "oc_g".into(),
            thread_id: Some("omt_42".into()),
            message_id: "om".into(),
            parent_id: None,
            text: "推一下".into(),
        };
        // 话题内普通消息无视白名单（绑定语义在 im_route 上）；执行侧拿 (chat_id, thread_id) 反查
        assert_eq!(
            route(&g_in_thread, &allow, &cards()),
            Route::IssueMessage {
                chat_id: "oc_g".into(),
                im_thread_ref: "omt_42".into(),
                sender_open_id: "ou_x".into(),
                text: "推一下".into(),
            }
        );
    }

    #[test]
    fn group_thread_bind_command_requires_owner() {
        let allow = vec!["ou_me".to_string()];
        let bind = Inbound::Text {
            sender_open_id: "ou_me".into(),
            chat_type: "group".into(),
            chat_id: "oc_g".into(),
            thread_id: Some("omt_42".into()),
            message_id: "om".into(),
            parent_id: None,
            text: "/bind 7".into(),
        };
        assert_eq!(
            route(&bind, &allow, &cards()),
            Route::BindIssueThread {
                thread_id: 7,
                chat_id: "oc_g".into(),
                im_thread_ref: "omt_42".into(),
                seed_message_id: "om".into(),
            }
        );

        let stranger = Inbound::Text {
            sender_open_id: "ou_x".into(),
            chat_type: "group".into(),
            chat_id: "oc_g".into(),
            thread_id: Some("omt_42".into()),
            message_id: "om".into(),
            parent_id: None,
            text: "/bind 7".into(),
        };
        assert_eq!(route(&stranger, &allow, &cards()), Route::Ignore);
    }

    #[test]
    fn dingtalk_real_thread_can_bind_but_plain_group_cannot_fake_one() {
        let allow = vec!["staff_owner".to_string()];
        let bind = Inbound::Text {
            sender_open_id: "staff_owner".into(),
            chat_type: "group".into(),
            chat_id: "cid_group".into(),
            thread_id: Some("convThreadEncrypted".into()),
            message_id: "msg_bind".into(),
            parent_id: None,
            text: "/bind 7".into(),
        };
        assert_eq!(
            route_for_provider(&bind, &allow, &cards(), ImProvider::DingTalk),
            Route::BindIssueThread {
                thread_id: 7,
                chat_id: "cid_group".into(),
                im_thread_ref: "convThreadEncrypted".into(),
                seed_message_id: "msg_bind".into(),
            }
        );

        let topic = Inbound::Text {
            sender_open_id: "staff_owner".into(),
            chat_type: "group".into(),
            chat_id: "cid_group".into(),
            thread_id: None,
            message_id: "msg_topic".into(),
            parent_id: None,
            text: "/topic 7".into(),
        };
        assert_eq!(
            route_for_provider(&topic, &allow, &cards(), ImProvider::DingTalk),
            Route::IssueThreadRequired
        );

        let plain_group_bind = Inbound::Text {
            sender_open_id: "staff_owner".into(),
            chat_type: "group".into(),
            chat_id: "cid_group".into(),
            thread_id: None,
            message_id: "msg_bind".into(),
            parent_id: None,
            text: "/bind 7".into(),
        };
        assert_eq!(
            route_for_provider(&plain_group_bind, &allow, &cards(), ImProvider::DingTalk),
            Route::IssueThreadRequired
        );
    }

    #[test]
    fn group_topic_command_requires_owner() {
        let allow = vec!["ou_me".to_string()];
        let topic = Inbound::Text {
            sender_open_id: "ou_me".into(),
            chat_type: "group".into(),
            chat_id: "oc_g".into(),
            thread_id: None,
            message_id: "om_cmd".into(),
            parent_id: None,
            text: "/topic 7".into(),
        };
        assert_eq!(
            route(&topic, &allow, &cards()),
            Route::EnsureIssueTopic {
                thread_id: 7,
                chat_id: "oc_g".into(),
                reply_to: "om_cmd".into(),
            }
        );

        let stranger = Inbound::Text {
            sender_open_id: "ou_x".into(),
            chat_type: "group".into(),
            chat_id: "oc_g".into(),
            thread_id: None,
            message_id: "om_cmd".into(),
            parent_id: None,
            text: "/topic 7".into(),
        };
        assert_eq!(route(&stranger, &allow, &cards()), Route::Ignore);
    }

    #[test]
    fn group_with_empty_allowlist_never_binds() {
        // 顺序锁：群消息不得通过 Bind 路径。空白名单 + 普通群消息忽略；
        // 话题内消息仍可走 IssueMessage，让执行侧按已存在的 im_route 反查 issue。
        let no_thread = Inbound::Text {
            sender_open_id: "ou_stranger".into(),
            chat_type: "group".into(),
            chat_id: "oc_g".into(),
            thread_id: None,
            message_id: "om".into(),
            parent_id: None,
            text: "hi".into(),
        };
        assert_eq!(route(&no_thread, &[], &cards()), Route::Ignore);
        let in_thread = Inbound::Text {
            sender_open_id: "ou_stranger".into(),
            chat_type: "group".into(),
            chat_id: "oc_g".into(),
            thread_id: Some("omt_1".into()),
            message_id: "om".into(),
            parent_id: None,
            text: "hi".into(),
        };
        assert_eq!(
            route(&in_thread, &[], &cards()),
            Route::IssueMessage {
                chat_id: "oc_g".into(),
                im_thread_ref: "omt_1".into(),
                sender_open_id: "ou_stranger".into(),
                text: "hi".into(),
            }
        );
    }

    #[test]
    fn card_action_routes_when_whitelisted() {
        let allow = vec!["ou_me".to_string()];
        let ok = serde_json::json!({"kind": "perm", "ask_id": 42, "answer": "allow"});
        assert_eq!(
            route(&action("ou_me", ok.clone()), &allow, &cards()),
            Route::AnswerPerm {
                ask_id: 42,
                answer: Answer::Allow
            }
        );
        assert_eq!(
            route(&action("ou_evil", ok), &allow, &cards()),
            Route::Ignore
        );
    }

    #[test]
    fn card_action_with_empty_allowlist_is_ignored() {
        // Bind 唯一入口是 p2p Text；Action 不触发绑定（默认无按钮，
        // 且未绑定时不该有任何已发卡片可点）。
        let a = action(
            "ou_x",
            serde_json::json!({"kind": "perm", "ask_id": 42, "answer": "allow"}),
        );
        assert_eq!(route(&a, &[], &cards()), Route::Ignore);
    }

    #[test]
    fn card_action_parses_string_ask_id_and_rejects_garbage() {
        let allow = vec!["ou_me".to_string()];
        // 飞书回调 JSON 往返后 ask_id 可能变字符串——双形态都路由成功。
        assert_eq!(
            route(
                &action(
                    "ou_me",
                    serde_json::json!({"kind": "perm", "ask_id": "42", "answer": "allow"})
                ),
                &allow,
                &cards()
            ),
            Route::AnswerPerm {
                ask_id: 42,
                answer: Answer::Allow
            }
        );
        assert_eq!(
            route(
                &action(
                    "ou_me",
                    serde_json::json!({"kind": "perm", "ask_id": "abc", "answer": "allow"})
                ),
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
                &action(
                    "ou_me",
                    serde_json::json!({"kind": "human", "ask_id": 42, "answer": "allow"})
                ),
                &allow,
                &cards()
            ),
            Route::Ignore
        );
        // answer 不是 Answer::parse 认可的四个字面量 → Ignore（fail-closed，
        // 与文本回复路径 parse_verdict 同强度不变式）。
        assert_eq!(
            route(
                &action(
                    "ou_me",
                    serde_json::json!({"kind": "perm", "ask_id": 42, "answer": "yolo"})
                ),
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
