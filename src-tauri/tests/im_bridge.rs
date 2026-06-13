//! IM 桥集成测试：FakeChannel + 真 registry + 内存 Db。不打真飞书。

use std::sync::{Arc, Mutex};
use weft::ask::{Answer, AskRegistry, Decision};
use weft::bus::BusRegistry;
use weft::im::{self, inbound::Route, Channel};
use weft::store::repo;
use weft::store::Db;

#[derive(Default)]
struct FakeChannel {
    texts: Arc<Mutex<Vec<(String, String)>>>, // (open_id, text)
    chat_texts: Arc<Mutex<Vec<(String, String)>>>, // (chat_id, text)
    topics: Arc<Mutex<Vec<(String, String, String)>>>, // (chat_id, seed_message_id, text)
    replies: Arc<Mutex<Vec<(String, String)>>>, // (reply_to, body)
    reactions: Arc<Mutex<Vec<(String, String)>>>, // (message_id, emoji) — adds
    deletions: Arc<Mutex<Vec<(String, String)>>>, // (message_id, reaction_id) — deletes
}

struct SlowReactionChannel {
    inner: FakeChannel,
    delay: std::time::Duration,
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
        self.texts
            .lock()
            .unwrap()
            .push((open_id.into(), text.into()));
        Ok(())
    }
    async fn send_chat_text(&self, chat_id: &str, text: &str) -> anyhow::Result<String> {
        self.chat_texts
            .lock()
            .unwrap()
            .push((chat_id.into(), text.into()));
        Ok(format!("om_chat_{}", self.chat_texts.lock().unwrap().len()))
    }
    async fn create_chat_topic(
        &self,
        chat_id: &str,
        seed_message_id: &str,
        text: &str,
    ) -> anyhow::Result<String> {
        self.topics
            .lock()
            .unwrap()
            .push((chat_id.into(), seed_message_id.into(), text.into()));
        Ok(format!("omt_topic_{}", self.topics.lock().unwrap().len()))
    }

    async fn reply_text(&self, reply_to: &str, text: &str) -> anyhow::Result<String> {
        self.replies
            .lock()
            .unwrap()
            .push((reply_to.into(), text.into()));
        Ok(format!("om_reply_{}", self.replies.lock().unwrap().len()))
    }
    async fn add_reaction(&self, message_id: &str, emoji: &str) -> anyhow::Result<String> {
        self.reactions
            .lock()
            .unwrap()
            .push((message_id.into(), emoji.into()));
        Ok(format!("re_{}", self.reactions.lock().unwrap().len()))
    }
    async fn delete_reaction(&self, message_id: &str, reaction_id: &str) -> anyhow::Result<()> {
        self.deletions
            .lock()
            .unwrap()
            .push((message_id.into(), reaction_id.into()));
        Ok(())
    }
}

#[async_trait::async_trait]
impl Channel for SlowReactionChannel {
    async fn send_card(&self, open_id: &str, card: serde_json::Value) -> anyhow::Result<String> {
        self.inner.send_card(open_id, card).await
    }
    async fn patch_card(&self, message_id: &str, card: serde_json::Value) -> anyhow::Result<()> {
        self.inner.patch_card(message_id, card).await
    }
    async fn send_text(&self, open_id: &str, text: &str) -> anyhow::Result<()> {
        self.inner.send_text(open_id, text).await
    }
    async fn send_chat_text(&self, chat_id: &str, text: &str) -> anyhow::Result<String> {
        self.inner.send_chat_text(chat_id, text).await
    }
    async fn create_chat_topic(
        &self,
        chat_id: &str,
        seed_message_id: &str,
        text: &str,
    ) -> anyhow::Result<String> {
        self.inner
            .create_chat_topic(chat_id, seed_message_id, text)
            .await
    }

    async fn reply_text(&self, reply_to: &str, text: &str) -> anyhow::Result<String> {
        self.inner.reply_text(reply_to, text).await
    }
    async fn add_reaction(&self, message_id: &str, emoji: &str) -> anyhow::Result<String> {
        tokio::time::sleep(self.delay).await;
        self.inner.add_reaction(message_id, emoji).await
    }
    async fn delete_reaction(&self, message_id: &str, reaction_id: &str) -> anyhow::Result<()> {
        self.inner.delete_reaction(message_id, reaction_id).await
    }
}

async fn mem_db() -> Db {
    Db::connect("sqlite::memory:").await.unwrap()
}

#[tokio::test]
async fn answer_perm_route_resolves_the_blocked_ask() {
    let db = mem_db().await;
    let (asks, bus, ch) = (
        AskRegistry::new(),
        BusRegistry::new(),
        FakeChannel::default(),
    );
    let (id, rx) = asks.request(1, "10", "claude", "Run: npm test", "npm test");
    let r = Route::AnswerPerm {
        ask_id: id,
        answer: Answer::Allow,
    };
    im::execute(r, &db, &asks, &bus, &ch, "ou_me", "zh", None, None)
        .await
        .unwrap();
    assert_eq!(rx.await.unwrap(), Decision::Allow);
    assert!(asks.open().is_empty());
    assert!(ch.texts.lock().unwrap().is_empty()); // 成功路径不发提示
}

#[tokio::test]
async fn answer_human_route_lands_in_asker_inbox() {
    let db = mem_db().await;
    let (asks, bus, ch) = (
        AskRegistry::new(),
        BusRegistry::new(),
        FakeChannel::default(),
    );
    bus.join(1, "10");
    let qid = bus.ask_human(1, "10", "major or minor?");
    let r = Route::AnswerHuman {
        thread: 1,
        ask_id: qid,
        text: "minor".into(),
    };
    im::execute(r, &db, &asks, &bus, &ch, "ou_me", "zh", None, None)
        .await
        .unwrap();
    let inbox = bus.inbox(1, "10");
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].text, "minor");
    assert_eq!(inbox[0].from, "you");
}

#[tokio::test]
async fn bind_route_appends_allowlist_without_legacy_confirmation() {
    let db = mem_db().await;
    let (asks, bus, ch) = (
        AskRegistry::new(),
        BusRegistry::new(),
        FakeChannel::default(),
    );
    let r = Route::Bind {
        open_id: "ou_me".into(),
        chat_id: "oc_dm".into(),
        text: "hello".into(),
    };
    im::execute(r, &db, &asks, &bus, &ch, "ou_me", "zh", None, None)
        .await
        .unwrap();
    let saved = repo::get_setting(&db, im::K_ALLOW).await.unwrap();
    assert_eq!(saved.as_deref(), Some("ou_me"));
    let texts = ch.texts.lock().unwrap();
    assert!(texts.is_empty()); // 首绑不再单发确认；有 app 时首句直接进 Concierge。
}

#[tokio::test]
async fn bind_route_rechecks_allowlist_still_empty() {
    // Bind 竞态防线：route 判定后、execute 落库前白名单已被并发写入 → 放弃绑定。
    let db = mem_db().await;
    let (asks, bus, ch) = (
        AskRegistry::new(),
        BusRegistry::new(),
        FakeChannel::default(),
    );
    repo::set_setting(&db, im::K_ALLOW, "ou_first")
        .await
        .unwrap();
    let r = Route::Bind {
        open_id: "ou_second".into(),
        chat_id: "oc_dm".into(),
        text: "hello".into(),
    };
    im::execute(r, &db, &asks, &bus, &ch, "ou_second", "zh", None, None)
        .await
        .unwrap();
    let saved = repo::get_setting(&db, im::K_ALLOW).await.unwrap();
    assert_eq!(saved.as_deref(), Some("ou_first")); // 未被覆盖/追加
    assert!(ch.texts.lock().unwrap().is_empty()); // 不发确认
}

#[tokio::test]
async fn stale_perm_answer_replies_already_handled() {
    let db = mem_db().await;
    let (asks, bus, ch) = (
        AskRegistry::new(),
        BusRegistry::new(),
        FakeChannel::default(),
    );
    let r = Route::AnswerPerm {
        ask_id: 999,
        answer: Answer::Allow,
    };
    im::execute(r, &db, &asks, &bus, &ch, "ou_me", "zh", None, None)
        .await
        .unwrap();
    let texts = ch.texts.lock().unwrap();
    assert_eq!(texts.len(), 1); // 「已过期/已处理」提示发给 sender
    assert_eq!(texts[0].0, "ou_me");
}

#[tokio::test]
async fn stale_human_answer_replies_already_answered() {
    let db = mem_db().await;
    let (asks, bus, ch) = (
        AskRegistry::new(),
        BusRegistry::new(),
        FakeChannel::default(),
    );
    bus.join(1, "10");
    let qid = bus.ask_human(1, "10", "q?");
    assert!(bus.answer_ask(1, qid, "first"));
    let r = Route::AnswerHuman {
        thread: 1,
        ask_id: qid,
        text: "second".into(),
    };
    im::execute(r, &db, &asks, &bus, &ch, "ou_me", "zh", None, None)
        .await
        .unwrap();
    assert_eq!(ch.texts.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn hint_routes_send_usage_text() {
    let db = mem_db().await;
    let (asks, bus, ch) = (
        AskRegistry::new(),
        BusRegistry::new(),
        FakeChannel::default(),
    );
    im::execute(
        Route::BadVerdict,
        &db,
        &asks,
        &bus,
        &ch,
        "ou_me",
        "zh",
        None,
        None,
    )
    .await
    .unwrap();
    im::execute(
        Route::FreeText {
            sender_open_id: "ou_me".into(),
            chat_id: "oc_dm".into(),
            im_thread_ref: "dm:ou_me".into(),
            reply_to: None,
            text: "今天进展如何".into(),
        },
        &db,
        &asks,
        &bus,
        &ch,
        "ou_me",
        "zh",
        None,
        None,
    )
    .await
    .unwrap();
    im::execute(
        Route::Ignore,
        &db,
        &asks,
        &bus,
        &ch,
        "ou_me",
        "zh",
        None,
        None,
    )
    .await
    .unwrap();
    let texts = ch.texts.lock().unwrap();
    assert_eq!(texts.len(), 2); // BadVerdict + FreeText 各一条；Ignore 零动作
    assert!(texts[0].1.contains("允许")); // 用法提示含协议词
}

#[tokio::test]
async fn issue_message_route_resolves_via_im_route() {
    // M2-3：bind 一条 im_route 后，IssueMessage 路由应能用 (chat_id, thread_ref)
    // 反查到 issue thread_id。这里跑 execute(..., None) 即无 app 句柄路径——
    // 不进 engine，但反查 + early-return 不能报错。完整 issue → lead engine 投递由
    // 桥运行时（spawn 路径）覆盖。
    let db = mem_db().await;
    let (asks, bus, ch) = (
        AskRegistry::new(),
        BusRegistry::new(),
        FakeChannel::default(),
    );
    let ws = repo::create_workspace(&db, "ws").await.unwrap();
    let t = repo::create_thread(&db, ws.id, "issue-x", "feature", "claude")
        .await
        .unwrap();
    repo::bind_im_route(&db, t.id, "feishu", "oc_g", "omt_42")
        .await
        .unwrap();
    let r = Route::IssueMessage {
        chat_id: "oc_g".into(),
        im_thread_ref: "omt_42".into(),
        sender_open_id: "ou_x".into(),
        text: "推一下".into(),
    };
    im::execute(r, &db, &asks, &bus, &ch, "ou_x", "zh", None, None)
        .await
        .unwrap();
    assert!(ch.texts.lock().unwrap().is_empty()); // 未走 engine 也不发其他文字
}

#[tokio::test]
async fn bind_issue_thread_route_persists_im_route_and_confirms() {
    let db = mem_db().await;
    let (asks, bus, ch) = (
        AskRegistry::new(),
        BusRegistry::new(),
        FakeChannel::default(),
    );
    let ws = repo::create_workspace(&db, "ws").await.unwrap();
    let t = repo::create_thread(&db, ws.id, "issue-bind", "feature", "claude")
        .await
        .unwrap();
    let r = Route::BindIssueThread {
        thread_id: t.id,
        chat_id: "oc_g".into(),
        im_thread_ref: "omt_42".into(),
    };
    im::execute(r, &db, &asks, &bus, &ch, "ou_me", "zh", None, None)
        .await
        .unwrap();
    let route = repo::im_route_of_thread(&db, t.id).await.unwrap().unwrap();
    assert_eq!(route.chat_id, "oc_g");
    assert_eq!(route.im_thread_ref, "omt_42");
    let texts = ch.texts.lock().unwrap();
    assert_eq!(texts.len(), 1);
    assert_eq!(texts[0].0, "ou_me");
    assert!(texts[0].1.contains("已绑定飞书话题"));
}

#[tokio::test]
async fn bind_issue_thread_missing_issue_is_polite_noop() {
    let db = mem_db().await;
    let (asks, bus, ch) = (
        AskRegistry::new(),
        BusRegistry::new(),
        FakeChannel::default(),
    );
    let r = Route::BindIssueThread {
        thread_id: 999,
        chat_id: "oc_g".into(),
        im_thread_ref: "omt_42".into(),
    };
    im::execute(r, &db, &asks, &bus, &ch, "ou_me", "zh", None, None)
        .await
        .unwrap();
    assert!(repo::list_im_routes(&db).await.unwrap().is_empty());
    let texts = ch.texts.lock().unwrap();
    assert_eq!(texts.len(), 1);
    assert!(texts[0].1.contains("没有找到"));
}

#[tokio::test]
async fn issue_message_route_unbound_thread_hints_without_creating_issue() {
    // 未 bind 的话题消息：不反向创建 issue，只提示如何为已有 issue 绑定 topic。
    use std::collections::HashMap;
    let db = mem_db().await;
    let (asks, bus, ch) = (
        AskRegistry::new(),
        BusRegistry::new(),
        FakeChannel::default(),
    );
    let ctx = im::ExecuteCtx {
        inbound_message_id: Some("om_in".into()),
        acks: Some(std::sync::Arc::new(tokio::sync::Mutex::new(HashMap::new()))),
        reaction_tx: None,
    };
    let r = Route::IssueMessage {
        chat_id: "oc_g".into(),
        im_thread_ref: "omt_never_bound".into(),
        sender_open_id: "ou_x".into(),
        text: "无主消息".into(),
    };
    im::execute(r, &db, &asks, &bus, &ch, "ou_x", "zh", None, Some(&ctx))
        .await
        .unwrap();
    assert!(repo::list_im_routes(&db).await.unwrap().is_empty());
    let replies = ch.replies.lock().unwrap();
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].0, "om_in");
    assert!(replies[0].1.contains("/bind"));
}

#[tokio::test]
async fn issue_message_with_ctx_adds_eyes_reaction() {
    // M2-6: bind 后的话题消息且 ctx 带 inbound_message_id + acks → 加 👀。
    use std::collections::HashMap;
    let db = mem_db().await;
    let (asks, bus, ch) = (
        AskRegistry::new(),
        BusRegistry::new(),
        FakeChannel::default(),
    );
    let ws = repo::create_workspace(&db, "ws").await.unwrap();
    let t = repo::create_thread(&db, ws.id, "issue-x", "feature", "claude")
        .await
        .unwrap();
    repo::bind_im_route(&db, t.id, "feishu", "oc_g", "omt_42")
        .await
        .unwrap();
    let acks = std::sync::Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let ctx = im::ExecuteCtx {
        inbound_message_id: Some("om_in_1".into()),
        acks: Some(acks.clone()),
        reaction_tx: None,
    };
    let r = Route::IssueMessage {
        chat_id: "oc_g".into(),
        im_thread_ref: "omt_42".into(),
        sender_open_id: "ou_x".into(),
        text: "看下进展".into(),
    };
    im::execute(r, &db, &asks, &bus, &ch, "ou_x", "zh", None, Some(&ctx))
        .await
        .unwrap();
    let rxns = ch.reactions.lock().unwrap();
    assert_eq!(rxns.len(), 1);
    assert_eq!(rxns[0], ("om_in_1".into(), "MeMeMe".into()));
    let acks_g = acks.lock().await;
    assert_eq!(acks_g.get(&t.id).map(|v| v.len()), Some(1));
}

#[tokio::test]
async fn issue_message_with_reaction_queue_does_not_wait_for_feishu_reaction_api() {
    use std::collections::HashMap;
    let db = mem_db().await;
    let (asks, bus) = (AskRegistry::new(), BusRegistry::new());
    let ch = SlowReactionChannel {
        inner: FakeChannel::default(),
        delay: std::time::Duration::from_millis(200),
    };
    let ws = repo::create_workspace(&db, "ws").await.unwrap();
    let t = repo::create_thread(&db, ws.id, "issue-x", "feature", "claude")
        .await
        .unwrap();
    repo::bind_im_route(&db, t.id, "feishu", "oc_g", "omt_42")
        .await
        .unwrap();
    let (reaction_tx, _reaction_rx) = tokio::sync::mpsc::unbounded_channel();
    let ctx = im::ExecuteCtx {
        inbound_message_id: Some("om_in_slow".into()),
        acks: Some(std::sync::Arc::new(tokio::sync::Mutex::new(HashMap::new()))),
        reaction_tx: Some(reaction_tx),
    };
    let r = Route::IssueMessage {
        chat_id: "oc_g".into(),
        im_thread_ref: "omt_42".into(),
        sender_open_id: "ou_x".into(),
        text: "看下进展".into(),
    };

    let result = tokio::time::timeout(
        std::time::Duration::from_millis(50),
        im::execute(r, &db, &asks, &bus, &ch, "ou_x", "zh", None, Some(&ctx)),
    )
    .await;

    assert!(result.is_ok(), "inbound execution waited for add_reaction");
}

#[tokio::test]
async fn consume_lead_out_replies_and_drains_acks() {
    // 端到端 M2-4 + M2-6：bind im_route → 模拟入站累计两条 👀 →
    // consume_lead_out 一次回流 + 清空两条 reaction。
    use std::collections::HashMap;
    use weft::lead_chat::out_hub::LeadOut;
    let db = mem_db().await;
    let ch = FakeChannel::default();
    let ws = repo::create_workspace(&db, "ws").await.unwrap();
    let t = repo::create_thread(&db, ws.id, "issue-y", "feature", "claude")
        .await
        .unwrap();
    repo::bind_im_route(&db, t.id, "feishu", "oc_g", "omt_99")
        .await
        .unwrap();
    // 预挂两条 👀（模拟入站消费已加过）
    let acks = std::sync::Arc::new(tokio::sync::Mutex::new(
        HashMap::<i32, Vec<(String, String)>>::new(),
    ));
    {
        let mut g = acks.lock().await;
        g.entry(t.id)
            .or_default()
            .push(("om_a".into(), "re_a".into()));
        g.entry(t.id)
            .or_default()
            .push(("om_b".into(), "re_b".into()));
    }
    let out = LeadOut {
        thread_id: t.id,
        message_id: 7,
        text: "搞定了一半".into(),
        origin_tag: None,
    };
    im::consume_lead_out(out, &db, &ch, &acks, false).await;
    // reply 一次：目标是最新入站 message_id，不是持久 topic id；body 带 Lead 前缀
    let replies = ch.replies.lock().unwrap();
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].0, "om_b");
    assert!(replies[0].1.starts_with("Lead："));
    // 两条 👀 一次性被 delete
    let dels = ch.deletions.lock().unwrap();
    assert_eq!(dels.len(), 2);
    assert!(acks.lock().await.get(&t.id).is_none());
}

#[tokio::test]
async fn consume_lead_out_unbound_thread_is_noop() {
    // thread 没绑 im_route → 桥不 reply、不动 reactions。
    use std::collections::HashMap;
    use weft::lead_chat::out_hub::LeadOut;
    let db = mem_db().await;
    let ch = FakeChannel::default();
    let acks = std::sync::Arc::new(tokio::sync::Mutex::new(
        HashMap::<i32, Vec<(String, String)>>::new(),
    ));
    let out = LeadOut {
        thread_id: 999,
        message_id: 1,
        text: "nope".into(),
        origin_tag: None,
    };
    im::consume_lead_out(out, &db, &ch, &acks, false).await;
    assert!(ch.replies.lock().unwrap().is_empty());
    assert!(ch.deletions.lock().unwrap().is_empty());
}

#[tokio::test]
async fn ensure_issue_topic_creates_feishu_root_and_binds_issue() {
    let db = mem_db().await;
    let ch = FakeChannel::default();
    let ws = repo::create_workspace(&db, "ws").await.unwrap();
    let t = repo::create_thread(&db, ws.id, "issue-topic", "feature", "claude")
        .await
        .unwrap();

    im::ensure_issue_topic(&db, &ch, t.id, "oc_g", Some("om_cmd"), "zh")
        .await
        .unwrap();

    let route = repo::im_route_of_thread(&db, t.id).await.unwrap().unwrap();
    assert_eq!(route.channel, "feishu");
    assert_eq!(route.chat_id, "oc_g");
    assert_eq!(route.im_thread_ref, "omt_topic_1");
    assert!(ch.chat_texts.lock().unwrap().is_empty());
    let topics = ch.topics.lock().unwrap();
    assert_eq!(topics.len(), 1);
    assert_eq!(topics[0].0, "oc_g");
    assert_eq!(topics[0].1, "om_cmd");
    assert!(topics[0].2.contains("Weft issue"));
    let replies = ch.replies.lock().unwrap();
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].0, "om_cmd");
    assert!(replies[0].1.contains("已创建"));
}

#[tokio::test]
async fn consume_lead_out_concierge_replies_to_bound_dm_route() {
    use std::collections::HashMap;
    use weft::lead_chat::out_hub::LeadOut;

    let db = mem_db().await;
    let ch = FakeChannel::default();
    let ws = repo::create_workspace(&db, "Concierge").await.unwrap();
    let concierge = repo::create_thread(&db, ws.id, "飞书私聊 · ou_owner", "concierge", "claude")
        .await
        .unwrap();
    repo::bind_im_route(
        &db,
        concierge.id,
        "feishu_concierge",
        "oc_dm",
        "dm:ou_owner",
    )
    .await
    .unwrap();
    let acks = std::sync::Arc::new(tokio::sync::Mutex::new(
        HashMap::<i32, Vec<(String, String)>>::new(),
    ));
    {
        let mut g = acks.lock().await;
        g.entry(concierge.id)
            .or_default()
            .push(("om_in_dm".into(), "re_dm".into()));
    }
    let out = LeadOut {
        thread_id: concierge.id,
        message_id: 1,
        text: "我查到了。".into(),
        origin_tag: None,
    };

    im::consume_lead_out(out, &db, &ch, &acks, false).await;

    let texts = ch.texts.lock().unwrap();
    assert_eq!(texts.len(), 1);
    assert_eq!(texts[0], ("ou_owner".into(), "我查到了。".into()));
    assert!(ch.replies.lock().unwrap().is_empty());
    assert_eq!(
        ch.deletions.lock().unwrap().as_slice(),
        [("om_in_dm".into(), "re_dm".into())]
    );
    assert!(acks.lock().await.get(&concierge.id).is_none());
}

#[tokio::test]
async fn consume_lead_out_concierge_replies_to_bound_group_route() {
    use std::collections::HashMap;
    use weft::lead_chat::out_hub::LeadOut;

    let db = mem_db().await;
    let ch = FakeChannel::default();
    let ws = repo::create_workspace(&db, "Concierge").await.unwrap();
    let concierge = repo::create_thread(&db, ws.id, "飞书群聊 · oc_g", "concierge", "claude")
        .await
        .unwrap();
    repo::bind_im_route(&db, concierge.id, "feishu_concierge", "oc_g", "chat:oc_g")
        .await
        .unwrap();
    let acks = std::sync::Arc::new(tokio::sync::Mutex::new(
        HashMap::<i32, Vec<(String, String)>>::new(),
    ));
    let out = LeadOut {
        thread_id: concierge.id,
        message_id: 1,
        text: "收到，我看一下。".into(),
        origin_tag: None,
    };

    im::consume_lead_out(out, &db, &ch, &acks, false).await;

    assert!(ch.texts.lock().unwrap().is_empty());
    let chat_texts = ch.chat_texts.lock().unwrap();
    assert_eq!(chat_texts.len(), 1);
    assert_eq!(chat_texts[0], ("oc_g".into(), "收到，我看一下。".into()));
}

#[tokio::test]
async fn consume_lead_out_concierge_replies_to_latest_group_message() {
    use std::collections::HashMap;
    use weft::lead_chat::out_hub::LeadOut;

    let db = mem_db().await;
    let ch = FakeChannel::default();
    let ws = repo::create_workspace(&db, "Concierge").await.unwrap();
    let concierge = repo::create_thread(&db, ws.id, "飞书群聊 · oc_g", "concierge", "claude")
        .await
        .unwrap();
    repo::bind_im_route(
        &db,
        concierge.id,
        "feishu_concierge",
        "oc_g",
        "chat:oc_g;reply:om_latest",
    )
    .await
    .unwrap();
    let acks = std::sync::Arc::new(tokio::sync::Mutex::new(
        HashMap::<i32, Vec<(String, String)>>::new(),
    ));
    let out = LeadOut {
        thread_id: concierge.id,
        message_id: 1,
        text: "收到，我看一下。".into(),
        origin_tag: None,
    };

    im::consume_lead_out(out, &db, &ch, &acks, false).await;

    assert!(ch.texts.lock().unwrap().is_empty());
    assert!(ch.chat_texts.lock().unwrap().is_empty());
    let replies = ch.replies.lock().unwrap();
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0], ("om_latest".into(), "收到，我看一下。".into()));
}
