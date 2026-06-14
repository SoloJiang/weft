//! Lead-engine outbound tap. Engine 在每一段 assistant 文本「完成」时通过
//! [`LeadOutHub::emit`] 广播一条 [`LeadOut`]，订阅者（M2 IM 桥）据此把内容
//! 回流到 IM。设计要点：
//! - 广播 `tokio::sync::broadcast`：多消费者、迟到订阅者只会丢历史不会拖慢
//!   engine（lagged → 跳过，不阻塞）。容量 64 比单轮可能的 finalize 数高 1 个
//!   数量级，IM 端正常消费下永远 lag 不到。
//! - 没有订阅者时 `send` 返回 `Err`——engine 忽略即可（出站口未挂上时本就不
//!   想阻塞 lead）。
//! - 只广播 _完整_ 文本：deltas/activity/tool-call 等不上桥（IM 想要的是「lead
//!   回复完成了，转给我看」的语义）。emit 调用点都在 engine.rs 写完 DB +
//!   发完 Push::Finalize 之后。

use tokio::sync::broadcast;

/// 一条 lead 已落库、已 finalize 的 assistant 文本。`thread_id` 用于桥侧
/// 反查 `im_route`；`text` 已是清洗后的内容（sentinel 早已剥离）。
#[derive(Clone, Debug)]
pub struct LeadOut {
    pub thread_id: i32,
    /// lead_message.id — 桥侧可用于去重/记账。
    pub message_id: i32,
    pub text: String,
    /// 不透明的「这一轮回复对应哪条入站消息」标记（per-turn 透传，非 IM 调用恒为
    /// None）。Concierge 桥据此把回复 reply 到正确的原始消息下；engine 只携带不解析。
    pub origin_tag: Option<String>,
}

/// Tauri-managed 单例。
pub struct LeadOutHub {
    tx: broadcast::Sender<LeadOut>,
}

impl Default for LeadOutHub {
    fn default() -> Self {
        let (tx, _rx) = broadcast::channel(64);
        Self { tx }
    }
}

impl LeadOutHub {
    /// engine 侧调用：assistant 段落 finalize 后发布。无订阅者时静默丢弃。
    pub fn emit(&self, out: LeadOut) {
        let _ = self.tx.send(out);
    }

    /// IM 桥/测试侧调用：拿一个独立的 receiver。
    pub fn subscribe(&self) -> broadcast::Receiver<LeadOut> {
        self.tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn emits_to_all_subscribers() {
        let hub = LeadOutHub::default();
        let mut a = hub.subscribe();
        let mut b = hub.subscribe();
        hub.emit(LeadOut {
            thread_id: 1,
            message_id: 10,
            text: "hi".into(),
            origin_tag: None,
        });
        let ra = a.recv().await.unwrap();
        let rb = b.recv().await.unwrap();
        assert_eq!(ra.message_id, 10);
        assert_eq!(rb.text, "hi");
    }

    #[tokio::test]
    async fn emit_with_no_subscribers_is_silent() {
        let hub = LeadOutHub::default();
        hub.emit(LeadOut {
            thread_id: 1,
            message_id: 1,
            text: "x".into(),
            origin_tag: None,
        }); // 不应 panic
    }
}
