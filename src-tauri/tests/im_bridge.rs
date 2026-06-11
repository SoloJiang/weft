//! IM 桥集成测试：FakeChannel + 真 registry + 内存 Db。不打真飞书。

use std::sync::{Arc, Mutex};
use weft_app_lib::ask::{Answer, AskRegistry, Decision};
use weft_app_lib::bus::BusRegistry;
use weft_app_lib::im::{self, inbound::Route, Channel};
use weft_app_lib::store::repo;
use weft_app_lib::store::Db;

#[derive(Default)]
struct FakeChannel {
    texts: Arc<Mutex<Vec<(String, String)>>>, // (open_id, text)
}

#[async_trait::async_trait]
impl Channel for FakeChannel {
    async fn send_card(&self, _open_id: &str, _card: serde_json::Value) -> anyhow::Result<String> {
        Ok("om_fake".into())
    }
    async fn patch_card(&self, _message_id: &str, _card: serde_json::Value) -> anyhow::Result<()> {
        Ok(())
    }
    async fn send_text(&self, open_id: &str, text: &str) -> anyhow::Result<()> {
        self.texts.lock().unwrap().push((open_id.into(), text.into()));
        Ok(())
    }
}

async fn mem_db() -> Db {
    Db::connect("sqlite::memory:").await.unwrap()
}

#[tokio::test]
async fn answer_perm_route_resolves_the_blocked_ask() {
    let db = mem_db().await;
    let (asks, bus, ch) = (AskRegistry::new(), BusRegistry::new(), FakeChannel::default());
    let (id, rx) = asks.request(1, "10", "claude", "Run: npm test", "npm test");
    let r = Route::AnswerPerm { ask_id: id, answer: Answer::Allow };
    im::execute(r, &db, &asks, &bus, &ch, "ou_me", "zh").await.unwrap();
    assert_eq!(rx.await.unwrap(), Decision::Allow);
    assert!(asks.open().is_empty());
    assert!(ch.texts.lock().unwrap().is_empty()); // 成功路径不发提示
}

#[tokio::test]
async fn answer_human_route_lands_in_asker_inbox() {
    let db = mem_db().await;
    let (asks, bus, ch) = (AskRegistry::new(), BusRegistry::new(), FakeChannel::default());
    bus.join(1, "10");
    let qid = bus.ask_human(1, "10", "major or minor?");
    let r = Route::AnswerHuman { thread: 1, ask_id: qid, text: "minor".into() };
    im::execute(r, &db, &asks, &bus, &ch, "ou_me", "zh").await.unwrap();
    let inbox = bus.inbox(1, "10");
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].text, "minor");
    assert_eq!(inbox[0].from, "you");
}

#[tokio::test]
async fn bind_route_appends_allowlist_and_confirms() {
    let db = mem_db().await;
    let (asks, bus, ch) = (AskRegistry::new(), BusRegistry::new(), FakeChannel::default());
    let r = Route::Bind { open_id: "ou_me".into() };
    im::execute(r, &db, &asks, &bus, &ch, "ou_me", "zh").await.unwrap();
    let saved = repo::get_setting(&db, im::K_ALLOW).await.unwrap();
    assert_eq!(saved.as_deref(), Some("ou_me"));
    let texts = ch.texts.lock().unwrap();
    assert_eq!(texts.len(), 1); // 绑定确认
    assert_eq!(texts[0].0, "ou_me");
}

#[tokio::test]
async fn bind_route_rechecks_allowlist_still_empty() {
    // Bind 竞态防线：route 判定后、execute 落库前白名单已被并发写入 → 放弃绑定。
    let db = mem_db().await;
    let (asks, bus, ch) = (AskRegistry::new(), BusRegistry::new(), FakeChannel::default());
    repo::set_setting(&db, im::K_ALLOW, "ou_first").await.unwrap();
    let r = Route::Bind { open_id: "ou_second".into() };
    im::execute(r, &db, &asks, &bus, &ch, "ou_second", "zh").await.unwrap();
    let saved = repo::get_setting(&db, im::K_ALLOW).await.unwrap();
    assert_eq!(saved.as_deref(), Some("ou_first")); // 未被覆盖/追加
    assert!(ch.texts.lock().unwrap().is_empty()); // 不发确认
}

#[tokio::test]
async fn stale_perm_answer_replies_already_handled() {
    let db = mem_db().await;
    let (asks, bus, ch) = (AskRegistry::new(), BusRegistry::new(), FakeChannel::default());
    let r = Route::AnswerPerm { ask_id: 999, answer: Answer::Allow };
    im::execute(r, &db, &asks, &bus, &ch, "ou_me", "zh").await.unwrap();
    let texts = ch.texts.lock().unwrap();
    assert_eq!(texts.len(), 1); // 「已过期/已处理」提示发给 sender
    assert_eq!(texts[0].0, "ou_me");
}

#[tokio::test]
async fn stale_human_answer_replies_already_answered() {
    let db = mem_db().await;
    let (asks, bus, ch) = (AskRegistry::new(), BusRegistry::new(), FakeChannel::default());
    bus.join(1, "10");
    let qid = bus.ask_human(1, "10", "q?");
    assert!(bus.answer_ask(1, qid, "first"));
    let r = Route::AnswerHuman { thread: 1, ask_id: qid, text: "second".into() };
    im::execute(r, &db, &asks, &bus, &ch, "ou_me", "zh").await.unwrap();
    assert_eq!(ch.texts.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn hint_routes_send_usage_text() {
    let db = mem_db().await;
    let (asks, bus, ch) = (AskRegistry::new(), BusRegistry::new(), FakeChannel::default());
    im::execute(Route::BadVerdict, &db, &asks, &bus, &ch, "ou_me", "zh").await.unwrap();
    im::execute(Route::FreeText, &db, &asks, &bus, &ch, "ou_me", "zh").await.unwrap();
    im::execute(Route::Ignore, &db, &asks, &bus, &ch, "ou_me", "zh").await.unwrap();
    let texts = ch.texts.lock().unwrap();
    assert_eq!(texts.len(), 2); // BadVerdict + FreeText 各一条；Ignore 零动作
    assert!(texts[0].1.contains("允许")); // 用法提示含协议词
}
