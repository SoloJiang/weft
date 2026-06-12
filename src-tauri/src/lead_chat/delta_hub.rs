//! Lead-engine 增量 tap。区别于 [`super::out_hub`]（只在整段文本「完成」时广播一次），
//! 本 hub 在 engine 的 `TextDelta` 节流点广播「到目前为止的累积文本」，让 IM 桥能把
//! lead 的回复以「打字机」式流式渲染到飞书卡片（CardKit streaming）。
//!
//! - `accumulated` 是**到当前为止的完整文本**（非单个 delta），故渲染端无状态、幂等：
//!   每次拿最新全文覆盖卡片即可，丢包/lag 只丢中间帧不影响终态。
//! - `done=true` 是该消息的最后一帧（finalize），携带权威全文——渲染端据此定稿。
//! - 广播容量比 out_hub 大（streaming 帧更密）；无订阅者静默丢弃。
//! - 节流在 engine 侧（与 DB 写同一个 500ms 闸），本 hub 不再二次节流。

use tokio::sync::broadcast;

/// 一帧 lead 流式增量。`thread_id` 供桥侧反查 `im_route`；`message_id` 是
/// lead_message.id，桥侧用它把同一条消息的帧归并到同一张卡。
#[derive(Clone, Debug)]
pub struct LeadDelta {
    pub thread_id: i32,
    pub message_id: i32,
    /// 到当前为止的累积全文（已清洗，sentinel 不在内）。
    pub accumulated: String,
    /// 该消息的最后一帧（finalize）。
    pub done: bool,
}

/// Tauri-managed 单例。
pub struct LeadDeltaHub {
    tx: broadcast::Sender<LeadDelta>,
}

impl Default for LeadDeltaHub {
    fn default() -> Self {
        // streaming 帧密度高于 out_hub 的「每段一次」，容量给大一档；正常消费下 lag 不到。
        let (tx, _rx) = broadcast::channel(256);
        Self { tx }
    }
}

impl LeadDeltaHub {
    /// engine 侧调用：节流点 / finalize 时发布。无订阅者静默丢弃。
    pub fn emit(&self, d: LeadDelta) {
        let _ = self.tx.send(d);
    }

    /// IM 桥/测试侧调用：拿一个独立 receiver。
    pub fn subscribe(&self) -> broadcast::Receiver<LeadDelta> {
        self.tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn broadcasts_accumulated_and_done_flag() {
        let hub = LeadDeltaHub::default();
        let mut rx = hub.subscribe();
        hub.emit(LeadDelta {
            thread_id: 7,
            message_id: 1,
            accumulated: "he".into(),
            done: false,
        });
        let got = rx.recv().await.unwrap();
        assert_eq!(got.thread_id, 7);
        assert_eq!(got.message_id, 1);
        assert_eq!(got.accumulated, "he");
        assert!(!got.done);
    }

    #[tokio::test]
    async fn emit_with_no_subscribers_is_silent() {
        let hub = LeadDeltaHub::default();
        hub.emit(LeadDelta {
            thread_id: 1,
            message_id: 1,
            accumulated: "x".into(),
            done: true,
        }); // 不应 panic
    }
}
