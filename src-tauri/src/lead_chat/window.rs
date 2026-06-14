//! 模型 → 上下文窗口(token)静态表。用作会话信息面板 Context 段的「% 分母」。
//! 真实 claude headless `system/init.model` 形如 `claude-opus-4-8[1m]`,`[1m]`
//! 后缀表示 1M 窗口;无后缀按各家族默认。拿不到就返回 None(面板只显示 token、
//! 不显示 %)。codex/opencode 的窗口走各自路径(后续里程碑),不在此表。

/// 解析模型字符串得到上下文窗口大小(token)。大小写不敏感;识别 `[1m]`/`-1m` 后缀。
pub fn context_window(model: &str) -> Option<u64> {
    let m = model.to_ascii_lowercase();
    if m.contains("[1m]") || m.contains("-1m") {
        return Some(1_000_000);
    }
    if m.contains("claude") {
        return Some(200_000); // opus / sonnet / haiku 默认 200k
    }
    None
}

#[cfg(test)]
mod tests {
    use super::context_window;

    #[test]
    fn maps_claude_models_and_1m_suffix() {
        assert_eq!(context_window("claude-opus-4-8[1m]"), Some(1_000_000));
        assert_eq!(context_window("claude-opus-4-8"), Some(200_000));
        assert_eq!(context_window("claude-sonnet-4-6"), Some(200_000));
        assert_eq!(context_window("gpt-5.5"), None); // 非 claude → 交给后续 codex/opencode 路径
        assert_eq!(context_window(""), None);
    }
}
