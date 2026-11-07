//! Deep phone-sync integration: page parsing + persistence, no network.
//!
//! The real `pull_mobile_msg` page shape is not yet live-verified ([U] in
//! `re-notes/09_mobile_transfer_sync.md`); these fixtures pin the shapes
//! `merge_mobile_page` must handle — the same message objects `lp_chat` pushes carry.

use zalo_rust::chat::{merge_mobile_page, count_mobile_page_msgs, Thread};
use zalo_rust::store::{persist_threads, MessageStore};

fn thread() -> Thread {
    Thread {
        presentation: Default::default(),
        unread: Default::default(),
        id: "2258306161344846438".into(),
        name: "Bạn thân".into(),
        preview: String::new(),
        time: String::new(),
        avatar: None,
        is_group: false,
        messages: Vec::new(),
    }
}

#[test]
fn merge_mobile_page_accepts_msgs_array_under_data() {
    let mut threads = vec![thread()];
    let page = serde_json::json!({
        "error_code": 0,
        "data": {
            "msgs": [
                {
                    "msgId": "1001", "msgType": "chat.text", "content": "tin 1",
                    "uidFrom": "3035194392954103598", "idTo": "2258306161344846438",
                    "ts": 1700000001000i64, "direction": 1
                },
                {
                    "msgId": "1002", "msgType": "chat.text", "content": "tin 2",
                    "uidFrom": "2258306161344846438", "idTo": "3035194392954103598",
                    "ts": 1700000002000i64, "direction": 2
                }
            ],
            "hasMore": 1
        }
    });
    assert_eq!(count_mobile_page_msgs(&page), 2);
    merge_mobile_page(&mut threads, &page);
    assert!(!threads[0].messages.is_empty());
}

#[test]
fn merge_mobile_page_accepts_entries_with_messages_arrays() {
    let mut threads = vec![thread()];
    let page = serde_json::json!({
        "error_code": 0,
        "data": {
            "conversations": [
                {
                    "threadId": "2258306161344846438",
                    "messages": [
                        { "msgId": "2001", "msgType": "chat.text", "content": "xin chào",
                          "ts": 1700000003000i64 }
                    ]
                }
            ]
        }
    });
    assert!(count_mobile_page_msgs(&page) >= 1);
    merge_mobile_page(&mut threads, &page);
    let t = threads.iter().find(|t| t.id == "2258306161344846438").unwrap();
    assert!(t.messages.iter().any(|m| m.id == "2001"));
}

#[test]
fn merge_mobile_page_is_idempotent_on_remerge() {
    let mut threads = vec![thread()];
    let page = serde_json::json!({
        "error_code": 0,
        "data": { "msgs": [
            { "msgId": "3001", "msgType": "chat.text", "content": "giống nhau",
              "ts": 1700000004000i64 }
        ]}
    });
    merge_mobile_page(&mut threads, &page);
    let count = threads[0].messages.len();
    merge_mobile_page(&mut threads, &page);
    assert_eq!(threads[0].messages.len(), count, "(INV-4) re-merging a page must not duplicate");
}

#[test]
fn deep_sync_pages_persist_and_hydrate_idempotently() {
    let mut threads = vec![thread()];
    let page_a = serde_json::json!({
        "error_code": 0,
        "data": { "msgs": [
            { "msgId": "4001", "msgType": "chat.text", "content": "trang 1",
              "uidFrom": "3035194392954103598", "idTo": "2258306161344846438",
              "ts": 1700000005000i64, "direction": 1 }
        ]}
    });
    let page_b = serde_json::json!({
        "error_code": 0,
        "data": { "msgs": [
            { "msgId": "4002", "msgType": "chat.text", "content": "trang 2",
              "uidFrom": "3035194392954103598", "idTo": "2258306161344846438",
              "ts": 1700000006000i64, "direction": 2 }
        ]}
    });
    merge_mobile_page(&mut threads, &page_a);
    merge_mobile_page(&mut threads, &page_b);

    let mut store = MessageStore::open(":memory:").unwrap();
    let added = persist_threads(&mut store, &threads).unwrap();
    assert_eq!(added, 2);

    // Re-persisting the same merged state (as a resume run would) must add nothing.
    let re_added = persist_threads(&mut store, &threads).unwrap();
    assert_eq!(re_added, 0, "(INV-4) checkpoint resume re-persist must be idempotent");

    let mut hydrated = vec![thread()];
    store.hydrate_threads(&mut hydrated, 100).unwrap();
    let t = hydrated.iter().find(|t| t.id == "2258306161344846438").unwrap();
    let mut ids: Vec<&str> = t.messages.iter().map(|m| m.id.as_str()).collect();
    ids.sort();
    assert_eq!(ids, vec!["4001", "4002"]);
}

#[test]
fn checkpoint_resumes_only_for_same_transfer_session() {
    let mut store = MessageStore::open(":memory:").unwrap();
    assert_eq!(store.resume_seq_if_same_session("pk-A").unwrap(), None);
    store.set_deep_sync_checkpoint(5, "pk-A", "tk-A").unwrap();
    assert_eq!(store.resume_seq_if_same_session("pk-A").unwrap(), Some(5));
    assert_eq!(store.resume_seq_if_same_session("pk-B").unwrap(), None, "a new transfer session must restart from seq 0");
}
