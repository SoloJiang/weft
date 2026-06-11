//! 出站渲染：语义通知 → 飞书卡片 JSON（卡片 1.0 schema）。纯函数。
//! 文案按 lang 双语内联（与 lead_chat lang_directive 同模式——后端无 i18n 框架）。

use serde_json::{json, Value};

/// 卡片按钮开关：Task 2 spike 结论 A 时翻 true。基线 = 回复消息作答，
/// 按钮只是增强（飞书长连接官方仅保证事件订阅，spec §1）。
pub const CARD_BUTTONS: bool = false;

fn t(lang: &str, zh: &'static str, en: &'static str) -> &'static str {
    if lang == "zh" { zh } else { en }
}

/// 权限 Ask 卡。回复作答提示恒在；CARD_BUTTONS 时追加四按钮。
pub fn perm_card(ask: &crate::ask::Ask, lang: &str) -> Value {
    let title = format!(
        "{} · {}",
        t(lang, "权限请求", "Permission ask"),
        if ask.thread_title.is_empty() { "weft" } else { &ask.thread_title }
    );
    let who = if ask.dir_name.is_empty() {
        ask.tool.clone()
    } else {
        format!("{} · {}", ask.dir_name, ask.tool)
    };
    let mut elements = vec![
        json!({"tag": "div", "text": {"tag": "lark_md", "content": format!("**{}**\n{}", ask.summary, who)}}),
        json!({"tag": "div", "text": {"tag": "lark_md", "content": format!("```\n{}\n```", ask.detail)}}),
        json!({"tag": "div", "text": {"tag": "lark_md", "content": t(lang,
            "回复本条消息作答：**允许** / **拒绝** / **总是** / **放行**（或 1/2/3/4）",
            "Reply to this message to answer: **allow** / **deny** / **always** / **full** (or 1/2/3/4)")}}),
    ];
    if CARD_BUTTONS {
        let btn = |label: &str, answer: &str, style: &str| json!({
            "tag": "button", "text": {"tag": "plain_text", "content": label}, "type": style,
            "value": {"kind": "perm", "ask_id": ask.id, "answer": answer}
        });
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
/// verdict: allow | always | full | deny | cancelled（其余显示「已处理」）。
pub fn resolved_card(summary: &str, verdict: &str, lang: &str) -> Value {
    let (label, color) = match verdict {
        "allow" => (t(lang, "已允许 ✓", "Allowed ✓"), "green"),
        "always" => (t(lang, "已允许（总是）✓", "Always-allowed ✓"), "green"),
        "full" => (t(lang, "已放行（任务全权）✓", "Full access ✓"), "green"),
        "deny" => (t(lang, "已拒绝 ✕", "Denied ✕"), "red"),
        "cancelled" => (t(lang, "已过期（回落工具自答）", "Expired (tool fallback)"), "grey"),
        _ => (t(lang, "已处理", "Resolved"), "grey"),
    };
    json!({
        "config": {"wide_screen_mode": true},
        "header": {"template": color, "title": {"tag": "plain_text", "content": label}},
        "elements": [
            {"tag": "div", "text": {"tag": "lark_md", "content": format!("~~{summary}~~")}}
        ]
    })
}

/// agent 提问（ask_human）卡：回复本条消息即作答。
pub fn human_card(thread_title: &str, from: &str, text: &str, lang: &str) -> Value {
    let title = format!("{} · {}", t(lang, "agent 提问", "Agent question"), thread_title);
    json!({
        "config": {"wide_screen_mode": true},
        "header": {"template": "blue", "title": {"tag": "plain_text", "content": title}},
        "elements": [
            {"tag": "div", "text": {"tag": "lark_md", "content": format!("**{from}**\n{text}")}},
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
        format!("{} {answer}", t(lang, "答：", "Answer:"))
    };
    json!({
        "config": {"wide_screen_mode": true},
        "header": {"template": "green",
            "title": {"tag": "plain_text", "content": t(lang, "已回答 ✓", "Answered ✓")}},
        "elements": [{"tag": "div", "text": {"tag": "lark_md", "content": body}}]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask() -> crate::ask::Ask {
        crate::ask::Ask {
            id: 42, thread: 1, dir: "10".into(), tool: "claude".into(),
            summary: "Run: npm test".into(), detail: "npm test".into(), ts: 0,
            thread_title: "登录超时修复".into(), dir_name: "backend".into(),
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
    fn resolved_card_shows_verdict_and_drops_actions() {
        let c = resolved_card("Run: npm test", "allow", "zh");
        let s = c.to_string();
        assert!(s.contains("Run: npm test"));
        assert!(!s.contains("\"tag\":\"action\""));
    }

    #[test]
    fn resolved_card_verdict_labels_zh_en() {
        assert!(resolved_card("x", "deny", "zh").to_string().contains("已拒绝 ✕"));
        assert!(resolved_card("x", "full", "en").to_string().contains("Full access ✓"));
        assert!(resolved_card("x", "???", "zh").to_string().contains("已处理"));
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
        assert!(s.contains("答： major"));
        let s = human_resolved_card("major", "en").to_string();
        assert!(s.contains("Answer: major"));
    }
}
