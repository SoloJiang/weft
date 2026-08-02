//! 出站渲染：语义通知 → 飞书卡片 JSON（卡片 1.0 schema）。纯函数。
//! 文案按 lang 双语内联（与 lead_chat lang_directive 同模式——后端无 i18n 框架）。
//! 当前此层直接产飞书卡片 JSON（Channel 以 Value 传卡）；第二通道引入时
//! 渲染下沉到适配器。

use serde_json::{json, Value};

/// Localized DingTalk copy is authored in the frontend i18n catalogs and
/// synchronized into the bridge at app startup. Keeping the fixed strings out
/// of Rust means the robot follows the same locale source as the desktop UI.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DingTalkCopy {
    pub permission_title: String,
    pub permission_reply_command: String,
    pub verdict_allowed: String,
    pub verdict_always_allowed: String,
    pub verdict_full_access: String,
    pub verdict_denied: String,
    pub verdict_expired: String,
    pub verdict_resolved: String,
    pub human_question_title: String,
    pub human_answer_instruction: String,
    pub human_answer_placeholder: String,
    pub human_answered: String,
    pub answer_prefix: String,
    pub human_cancelled: String,
    pub issue_not_found: String,
    pub bind_thread_prefix: String,
    pub permission_already_handled: String,
    pub human_already_answered: String,
    pub permission_command_usage: String,
    pub human_answer_usage: String,
    pub thread_required: String,
    pub free_text_unavailable: String,
    pub unbound_thread: String,
    pub concierge_dm_prefix: String,
    pub concierge_group_prefix: String,
    pub lead_prefix: String,
    pub resync_one: String,
    pub resync_many: String,
    pub resync_more: String,
    pub resync_hint: String,
}

impl DingTalkCopy {
    pub fn validate(&self) -> anyhow::Result<()> {
        const MAX_FIELD_CHARS: usize = 2_000;
        let fields = [
            ("permissionTitle", self.permission_title.as_str()),
            (
                "permissionReplyCommand",
                self.permission_reply_command.as_str(),
            ),
            ("verdictAllowed", self.verdict_allowed.as_str()),
            ("verdictAlwaysAllowed", self.verdict_always_allowed.as_str()),
            ("verdictFullAccess", self.verdict_full_access.as_str()),
            ("verdictDenied", self.verdict_denied.as_str()),
            ("verdictExpired", self.verdict_expired.as_str()),
            ("verdictResolved", self.verdict_resolved.as_str()),
            ("humanQuestionTitle", self.human_question_title.as_str()),
            (
                "humanAnswerInstruction",
                self.human_answer_instruction.as_str(),
            ),
            (
                "humanAnswerPlaceholder",
                self.human_answer_placeholder.as_str(),
            ),
            ("humanAnswered", self.human_answered.as_str()),
            ("answerPrefix", self.answer_prefix.as_str()),
            ("humanCancelled", self.human_cancelled.as_str()),
            ("issueNotFound", self.issue_not_found.as_str()),
            ("bindThreadPrefix", self.bind_thread_prefix.as_str()),
            (
                "permissionAlreadyHandled",
                self.permission_already_handled.as_str(),
            ),
            ("humanAlreadyAnswered", self.human_already_answered.as_str()),
            (
                "permissionCommandUsage",
                self.permission_command_usage.as_str(),
            ),
            ("humanAnswerUsage", self.human_answer_usage.as_str()),
            ("threadRequired", self.thread_required.as_str()),
            ("freeTextUnavailable", self.free_text_unavailable.as_str()),
            ("unboundThread", self.unbound_thread.as_str()),
            ("conciergeDmPrefix", self.concierge_dm_prefix.as_str()),
            ("conciergeGroupPrefix", self.concierge_group_prefix.as_str()),
            ("leadPrefix", self.lead_prefix.as_str()),
            ("resyncOne", self.resync_one.as_str()),
            ("resyncMany", self.resync_many.as_str()),
            ("resyncMore", self.resync_more.as_str()),
            ("resyncHint", self.resync_hint.as_str()),
        ];
        for (name, value) in fields {
            if value.trim().is_empty() {
                anyhow::bail!("DingTalk copy field {name} is empty");
            }
            if value.chars().count() > MAX_FIELD_CHARS {
                anyhow::bail!("DingTalk copy field {name} is too long");
            }
        }
        Ok(())
    }
}

/// 卡片按钮开关：基线 = 回复消息作答，
/// 按钮只是增强（飞书长连接官方仅保证事件订阅，spec §1）。
pub const CARD_BUTTONS: bool = false;

fn t(lang: &str, zh: &'static str, en: &'static str) -> &'static str {
    if lang == "zh" {
        zh
    } else {
        en
    }
}

/// 按字符截断（CJK 安全：字节切片落在多字节字符中间会 panic，生产路径
/// deny panic）。飞书卡片消息体上限 30KB——超限报 230025 且整次 send 失败，
/// 权限卡会静默丢失，故各字段出卡前先行截断。截断标记固定英文，不随 lang。
fn clamp(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max_chars).collect();
        out.push_str("…(truncated)");
        out
    }
}

/// 权限 Ask 卡。回复作答提示恒在；CARD_BUTTONS 时追加四按钮。
pub fn perm_card(ask: &crate::ask::Ask, lang: &str) -> Value {
    let title = format!(
        "{} · {}",
        t(lang, "权限请求", "Permission ask"),
        if ask.thread_title.is_empty() {
            "weft"
        } else {
            &ask.thread_title
        }
    );
    let who = if ask.dir_name.is_empty() {
        ask.tool.clone()
    } else {
        format!("{} · {}", ask.dir_name, ask.tool)
    };
    let mut elements = vec![
        json!({"tag": "div", "text": {"tag": "lark_md", "content": format!("**{}**\n{}", clamp(&ask.summary, 200), who)}}),
        // detail 必须 plain_text：lark_md 会渲染 **/~~/<a>，权限卡显示与
        // 实际命令不忠实是欺骗面（且 lark_md 不支持 ``` 代码块——那是
        // markdown 组件独有），原样直出才可信。
        json!({"tag": "div", "text": {"tag": "plain_text", "content": clamp(&ask.detail, 3000)}}),
        // 1/2/3/4 数字映射是与 inbound::parse_verdict 的共享协议，改序必须同步。
        json!({"tag": "div", "text": {"tag": "lark_md", "content": t(lang,
            "回复本条消息作答：**允许** / **拒绝** / **总是** / **放行**（或 1/2/3/4）",
            "Reply to this message to answer: **allow** / **deny** / **always** / **full** (or 1/2/3/4)")}}),
    ];
    if CARD_BUTTONS {
        let btn = |label: &str, answer: &str, style: &str| {
            json!({
                "tag": "button", "text": {"tag": "plain_text", "content": label}, "type": style,
                "value": {"kind": "perm", "ask_id": ask.id, "answer": answer}
            })
        };
        elements.push(json!({"tag": "action", "actions": [
            btn(t(lang, "允许", "Allow"), "allow", "primary"),
            btn(t(lang, "总是", "Always"), "always", "default"),
            btn(t(lang, "放行", "Full"), "full", "default"),
            btn(t(lang, "拒绝", "Deny"), "deny", "danger"),
        ]}));
    }
    json!({
        "config": {"wide_screen_mode": true},
        "header": {"template": "orange", "title": {"tag": "plain_text", "content": title}},
        "elements": elements
    })
}

/// 应答后的终态卡（双面同步：飞书/桌面任一侧答完都 patch 成这张）。
/// verdict 必须来自 `crate::ask::Answer::as_str()` 或 `"cancelled"`，
/// 不得手写字面量（其余值兜底显示「已处理」）。
pub fn resolved_card(summary: &str, verdict: &str, lang: &str) -> Value {
    let (label, color) = match verdict {
        "allow" => (t(lang, "已允许 ✓", "Allowed ✓"), "green"),
        "always" => (t(lang, "已允许（总是）✓", "Always-allowed ✓"), "green"),
        "full" => (t(lang, "已放行（任务全权）✓", "Full access ✓"), "green"),
        "deny" => (t(lang, "已拒绝 ✕", "Denied ✕"), "red"),
        "cancelled" => (
            t(lang, "已过期（回落工具自答）", "Expired (tool fallback)"),
            "grey",
        ),
        _ => (t(lang, "已处理", "Resolved"), "grey"),
    };
    let body = if summary.is_empty() {
        "—".to_string() // 空 summary 不渲染 `~~~~`
    } else {
        format!("~~{}~~", clamp(summary, 200))
    };
    json!({
        "config": {"wide_screen_mode": true},
        "header": {"template": color, "title": {"tag": "plain_text", "content": label}},
        "elements": [
            {"tag": "div", "text": {"tag": "lark_md", "content": body}}
        ]
    })
}

/// agent 提问（ask_human）卡：回复本条消息即作答。
pub fn human_card(thread_title: &str, from: &str, text: &str, lang: &str) -> Value {
    let title = format!(
        "{} · {}",
        t(lang, "agent 提问", "Agent question"),
        thread_title
    );
    json!({
        "config": {"wide_screen_mode": true},
        "header": {"template": "blue", "title": {"tag": "plain_text", "content": title}},
        "elements": [
            {"tag": "div", "text": {"tag": "lark_md", "content": format!("**{from}**\n{}", clamp(text, 3000))}},
            {"tag": "div", "text": {"tag": "lark_md", "content": t(lang,
                "回复本条消息，你的回答会送回该 agent。",
                "Reply to this message; your answer is delivered back to the agent.")}}
        ]
    })
}

/// 提问被（任一面）作答后的终态卡。answer 为人答的文本（可空）。
pub fn human_resolved_card(answer: &str, lang: &str) -> Value {
    let body = if answer.is_empty() {
        t(lang, "已回答。", "Answered.").to_string()
    } else {
        format!("{}{}", t(lang, "答：", "Answer: "), clamp(answer, 1000))
    };
    json!({
        "config": {"wide_screen_mode": true},
        "header": {"template": "green",
            "title": {"tag": "plain_text", "content": t(lang, "已回答 ✓", "Answered ✓")}},
        "elements": [{"tag": "div", "text": {"tag": "lark_md", "content": body}}]
    })
}

/// 提问因其 thread/workspace 被删除而取消后的终态卡。
pub fn human_cancelled_card(lang: &str) -> Value {
    json!({
        "config": {"wide_screen_mode": true},
        "header": {"template": "grey",
            "title": {"tag": "plain_text", "content": t(lang, "已取消", "Cancelled")}},
        "elements": [{"tag": "div", "text": {"tag": "lark_md", "content": t(lang,
            "该 workspace 已删除，提问已取消。",
            "The workspace was deleted, so this question was cancelled.")}}]
    })
}

/// 钉钉基础机器人消息没有飞书 reply-parent / 可 patch 卡片的稳定等价物。
/// 因此权限与提问都携带完整、显式、可复制的命令；detail 用纯文本拼接，避免
/// markdown 把真实命令渲染成另一种含义。
pub fn dingtalk_permission_text(ask: &crate::ask::Ask, copy: &DingTalkCopy) -> String {
    let title = if ask.thread_title.is_empty() {
        "weft"
    } else {
        &ask.thread_title
    };
    let who = if ask.dir_name.is_empty() {
        ask.tool.clone()
    } else {
        format!("{} · {}", ask.dir_name, ask.tool)
    };
    format!(
        "{} · {title}\n{}\n{who}\n\n{}\n\n{}\n/allow {}\n/deny {}\n/always {}\n/full {}",
        copy.permission_title,
        clamp(&ask.summary, 200),
        clamp(&ask.detail, 3000),
        copy.permission_reply_command,
        ask.id,
        ask.id,
        ask.id,
        ask.id,
    )
}

pub fn dingtalk_permission_resolved_text(
    summary: &str,
    verdict: &str,
    copy: &DingTalkCopy,
) -> String {
    let label = match verdict {
        "allow" => copy.verdict_allowed.as_str(),
        "always" => copy.verdict_always_allowed.as_str(),
        "full" => copy.verdict_full_access.as_str(),
        "deny" => copy.verdict_denied.as_str(),
        "cancelled" => copy.verdict_expired.as_str(),
        _ => copy.verdict_resolved.as_str(),
    };
    if summary.is_empty() {
        label.to_string()
    } else {
        format!("{label}\n{}", clamp(summary, 200))
    }
}

pub fn dingtalk_human_question_text(
    thread_id: i32,
    ask_id: u64,
    thread_title: &str,
    from: &str,
    text: &str,
    copy: &DingTalkCopy,
) -> String {
    format!(
        "{} · {}\n{}\n{}\n\n{}\n/answer {thread_id} {ask_id} {}",
        copy.human_question_title,
        thread_title,
        from,
        clamp(text, 3000),
        copy.human_answer_instruction,
        copy.human_answer_placeholder,
    )
}

pub fn dingtalk_human_resolved_text(answer: &str, copy: &DingTalkCopy) -> String {
    if answer.is_empty() {
        copy.human_answered.clone()
    } else {
        format!(
            "{}\n{}{}",
            copy.human_answered,
            copy.answer_prefix,
            clamp(answer, 1000)
        )
    }
}

pub fn dingtalk_human_cancelled_text(copy: &DingTalkCopy) -> &str {
    &copy.human_cancelled
}

pub fn dingtalk_issue_reply_text(copy: &DingTalkCopy, body: &str) -> String {
    format!("{}{}", copy.lead_prefix, clamp(body.trim(), 9000))
}

pub fn dingtalk_resync_summary(copy: &DingTalkCopy, items: &[(i32, String)]) -> String {
    if items.is_empty() {
        return String::new();
    }
    let mut body = if items.len() == 1 {
        copy.resync_one.clone()
    } else {
        copy.resync_many.replace("{n}", &items.len().to_string())
    };
    let cap = 8;
    for (thread_id, summary) in items.iter().take(cap) {
        body.push_str(&format!("\n- #{thread_id} {}", clamp(summary, 160)));
    }
    if items.len() > cap {
        body.push_str(
            &copy
                .resync_more
                .replace("{n}", &(items.len() - cap).to_string()),
        );
    }
    body.push_str(&copy.resync_hint);
    body
}

/// M2-4：lead/Concierge 回流飞书话题的纯文本渲染。前缀「Lead：/Lead: 」
/// 让人在话题里一眼区分「自己说的」vs「agent 说的」（飞书话题里不显示
/// 发送方的角色徽章——bot 名字默认折叠成应用名）。空 body 不上桥，由
/// 调用方保证（emit 处已 trim+空判）。
pub fn issue_reply_text(lang: &str, body: &str) -> String {
    // 上限留余量：飞书文本消息体上限 30KB UTF-8，CJK 取 9000 字符 ≈ 27KB。
    // 截断的逻辑与卡片侧 clamp 同源；终态打 …(truncated) 给阅读者一个信号。
    let trimmed = body.trim();
    let prefix = t(lang, "Lead：", "Lead: ");
    format!("{}{}", prefix, clamp(trimmed, 9000))
}

/// M3-4：桥（重）上线后发给 owner 的一次性摘要。items 是「待你处理」清单
/// （每项 (thread_id, summary)）。无积压则返回空串——调用方据此跳过发送。
/// 上限 8 条避免单条消息溢出；超出部分给个 "+N more" 角标。
pub fn resync_summary(lang: &str, items: &[(i32, String)]) -> String {
    if items.is_empty() {
        return String::new();
    }
    let head = if items.len() == 1 {
        t(
            lang,
            "Weft 桥已上线，当前 1 项待你处理：",
            "Weft bridge online — 1 ask waiting:",
        )
        .to_string()
    } else {
        let tmpl = t(
            lang,
            "Weft 桥已上线，当前 {n} 项待你处理：",
            "Weft bridge online — {n} asks waiting:",
        );
        tmpl.replace("{n}", &items.len().to_string())
    };
    let cap = 8;
    let mut body = String::new();
    body.push_str(&head);
    for (tid, summary) in items.iter().take(cap) {
        body.push_str(&format!("\n• #{tid} — {}", clamp(summary, 80)));
    }
    if items.len() > cap {
        let more = t(lang, " 等（+{n} 条）", " (+{n} more)");
        body.push_str(&more.replace("{n}", &(items.len() - cap).to_string()));
    }
    body.push_str(t(
        lang,
        "\n回复对应卡片消息作答；其他请在桌面端处理。",
        "\nReply to the matching card to answer; handle the rest on desktop.",
    ));
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask() -> crate::ask::Ask {
        crate::ask::Ask {
            id: 42,
            thread: 1,
            dir: "10".into(),
            tool: "claude".into(),
            summary: "Run: npm test".into(),
            detail: "npm test".into(),
            risk: crate::ask::RiskLevel::Unknown,
            ts: 0,
            thread_title: "登录超时修复".into(),
            dir_name: "backend".into(),
            workspace_id: None,
            action_key: "claude:npm test".into(),
        }
    }

    fn dingtalk_copy() -> DingTalkCopy {
        DingTalkCopy {
            permission_title: "权限请求".into(),
            permission_reply_command: "复制一条命令回复：".into(),
            verdict_allowed: "已允许 ✓".into(),
            verdict_always_allowed: "已允许（总是）✓".into(),
            verdict_full_access: "已放行（任务全权）✓".into(),
            verdict_denied: "已拒绝 ✕".into(),
            verdict_expired: "已过期".into(),
            verdict_resolved: "已处理".into(),
            human_question_title: "Agent question".into(),
            human_answer_instruction: "Replace <answer> with your reply:".into(),
            human_answer_placeholder: "<answer>".into(),
            human_answered: "Question answered ✓".into(),
            answer_prefix: "Answer: ".into(),
            human_cancelled: "提问已取消".into(),
            issue_not_found: "没有找到 issue".into(),
            bind_thread_prefix: "已绑定 thread".into(),
            permission_already_handled: "权限请求已处理".into(),
            human_already_answered: "提问已回答".into(),
            permission_command_usage: "permission usage".into(),
            human_answer_usage: "human usage".into(),
            thread_required: "thread required".into(),
            free_text_unavailable: "unavailable".into(),
            unbound_thread: "unbound".into(),
            concierge_dm_prefix: "DingTalk DM".into(),
            concierge_group_prefix: "DingTalk group".into(),
            lead_prefix: "Lead：".into(),
            resync_one: "1 项待处理".into(),
            resync_many: "{n} 项待处理".into(),
            resync_more: "（+{n} 条）".into(),
            resync_hint: "\n在桌面端处理".into(),
        }
    }

    #[test]
    fn perm_card_carries_summary_title_and_reply_hint() {
        let c = perm_card(&ask(), "zh");
        let s = c.to_string();
        assert!(s.contains("Run: npm test"));
        assert!(s.contains("登录超时修复"));
        assert!(s.contains("允许")); // 回复作答提示
        if CARD_BUTTONS {
            assert!(s.contains("\"kind\":\"perm\"") && s.contains("\"ask_id\":42"));
        }
    }

    #[test]
    fn perm_card_en_uses_english_copy() {
        let c = perm_card(&ask(), "en");
        let s = c.to_string();
        assert!(s.contains("Permission ask"));
        assert!(s.contains("**allow**"));
        assert!(!s.contains("允许"));
    }

    #[test]
    fn perm_card_detail_is_plain_text_verbatim() {
        // lark_md 注入面：detail 里的 markdown/HTML 必须原样直出，不被渲染。
        let mut a = ask();
        a.detail = "**bold** ~~x~~ <a href='e'>y</a>".into();
        let c = perm_card(&a, "zh");
        assert_eq!(c["elements"][1]["text"]["tag"], "plain_text");
        assert_eq!(
            c["elements"][1]["text"]["content"],
            "**bold** ~~x~~ <a href='e'>y</a>"
        );
    }

    #[test]
    fn perm_card_clamps_oversized_detail_under_card_limit() {
        // 飞书卡片体上限 30KB（超限报 230025）：100KB 级中文 detail 截断后必须远低于限。
        let mut a = ask();
        a.detail = "汉".repeat(33_000); // ≈ 99KB UTF-8
        let s = perm_card(&a, "zh").to_string();
        assert!(s.len() < 30_000, "card body {} bytes >= 30KB", s.len());
        assert!(s.contains("…(truncated)"));
    }

    #[test]
    fn clamp_truncates_by_chars_cjk_safe() {
        let s = "汉".repeat(10);
        assert_eq!(clamp(&s, 10), s); // 恰好不超：原样
        let c = clamp(&s, 4); // 按字节切片会落在多字节字符中间 panic；chars 安全
        assert!(c.starts_with("汉汉汉汉"));
        assert!(c.ends_with("…(truncated)"));
        assert_eq!(c.chars().count(), 4 + "…(truncated)".chars().count());
    }

    #[test]
    fn resolved_card_shows_verdict_and_drops_actions() {
        let c = resolved_card("Run: npm test", "allow", "zh");
        let s = c.to_string();
        assert!(s.contains("Run: npm test"));
        assert!(!s.contains("\"tag\":\"action\""));
    }

    #[test]
    fn resolved_card_verdict_labels_zh_en() {
        use crate::ask::Answer;
        assert!(resolved_card("x", Answer::Deny.as_str(), "zh")
            .to_string()
            .contains("已拒绝 ✕"));
        assert!(resolved_card("x", Answer::Full.as_str(), "en")
            .to_string()
            .contains("Full access ✓"));
        assert!(resolved_card("x", "???", "zh")
            .to_string()
            .contains("已处理"));
    }

    #[test]
    fn resolved_card_empty_summary_skips_strikethrough() {
        let s = resolved_card("", "allow", "zh").to_string();
        assert!(!s.contains("~~~~"));
        assert!(s.contains("—"));
    }

    #[test]
    fn human_card_carries_question_and_thread() {
        let c = human_card("登录超时修复", "backend", "major or minor?", "zh");
        let s = c.to_string();
        assert!(s.contains("major or minor?"));
        assert!(s.contains("登录超时修复"));
    }

    #[test]
    fn human_resolved_card_empty_and_nonempty_answer() {
        let s = human_resolved_card("", "zh").to_string();
        assert!(s.contains("已回答。"));
        let s = human_resolved_card("major", "zh").to_string();
        assert!(s.contains("答：major"));
        let s = human_resolved_card("major", "en").to_string();
        assert!(s.contains("Answer: major"));
    }

    #[test]
    fn dingtalk_prompts_use_explicit_deterministic_commands() {
        let copy = dingtalk_copy();
        let permission = dingtalk_permission_text(&ask(), &copy);
        assert!(permission.contains("/allow 42"));
        assert!(permission.contains("/deny 42"));
        assert!(permission.contains("/always 42"));
        assert!(permission.contains("/full 42"));

        let human =
            dingtalk_human_question_text(7, 9, "登录超时修复", "backend", "major or minor?", &copy);
        assert!(human.contains("/answer 7 9 <answer>"));
        assert!(human.contains("major or minor?"));
    }

    #[test]
    fn dingtalk_resolution_copy_matches_answer_protocol() {
        use crate::ask::Answer;
        let copy = dingtalk_copy();
        assert!(
            dingtalk_permission_resolved_text("x", Answer::Always.as_str(), &copy)
                .contains("已允许（总是）")
        );
        assert!(dingtalk_human_resolved_text("minor", &copy).contains("Answer: minor"));
        assert!(dingtalk_human_cancelled_text(&copy).contains("已取消"));
    }

    #[test]
    fn dingtalk_copy_is_validated_and_drives_issue_and_resync_text() {
        let copy = dingtalk_copy();
        assert!(copy.validate().is_ok());
        assert_eq!(
            dingtalk_issue_reply_text(&copy, " 推进了一下 "),
            "Lead：推进了一下"
        );
        let summary = dingtalk_resync_summary(&copy, &[(7, "Run tests".into())]);
        assert!(summary.contains("1 项待处理"));
        assert!(summary.contains("#7 Run tests"));

        let invalid = DingTalkCopy {
            permission_title: String::new(),
            ..copy
        };
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn issue_reply_text_prefixes_and_clamps() {
        assert_eq!(issue_reply_text("zh", "推进了一下"), "Lead：推进了一下");
        assert_eq!(issue_reply_text("en", "  pushed it  "), "Lead: pushed it");
        // 9000 字符以上必须截断（CJK 安全）
        let s = issue_reply_text("zh", &"汉".repeat(10_000));
        assert!(s.starts_with("Lead："));
        assert!(s.ends_with("…(truncated)"));
        assert!(
            s.chars().count() <= "Lead：".chars().count() + 9000 + "…(truncated)".chars().count()
        );
    }

    #[test]
    fn resync_summary_empty_returns_empty_string() {
        assert!(resync_summary("zh", &[]).is_empty());
        assert!(resync_summary("en", &[]).is_empty());
    }

    #[test]
    fn resync_summary_singular_and_plural_zh_en() {
        let one = resync_summary("zh", &[(7, "Run: npm test".into())]);
        assert!(one.contains("1 项"));
        assert!(one.contains("#7"));
        assert!(one.contains("Run: npm test"));

        let many = resync_summary("en", &[(1, "a".into()), (2, "b".into())]);
        assert!(many.contains("2 asks"));
        assert!(many.contains("#1") && many.contains("#2"));
        assert!(many.contains("Reply to the matching card"));
    }

    #[test]
    fn resync_summary_caps_at_eight_and_reports_more() {
        let items: Vec<(i32, String)> = (1..=12).map(|i| (i, format!("s{i}"))).collect();
        let s = resync_summary("zh", &items);
        // 头 8 条要出现
        for i in 1..=8 {
            assert!(s.contains(&format!("#{i}")));
        }
        // 第 9..12 不显式出现
        for i in 9..=12 {
            assert!(!s.contains(&format!("#{i}")));
        }
        assert!(s.contains("+4 条"));
    }
}
