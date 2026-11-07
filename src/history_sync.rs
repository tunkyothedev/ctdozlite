//! Paged history sync for groups, "Cloud của tôi", and mobile message sync (`Đồng bộ tin nhắn từ điện thoại`, #19, #23, #68).
//!
//! - Group & Cloud history walks `/api/cm/getoldv2` backwards 50 messages at a time from the oldest
//!   message already in `zalo_history.db`, so a second run resumes where the first stopped.
//! - Phone message sync (`sync_phone_messages`, #68) triggers Zalo's mobile pull endpoints
//!   (`/api/message/pull_mobile_msg` cmd `12000` & `/api/transfer-sync-v2/request-sync` cmd `12888`),
//!   drains `lp_chat` multi-batch queues (`getChatFromPolling`), batch-queries `/api/preloadconvers/get-last-msgs`
//!   across all 1-1 and group threads in 25-thread chunks, and syncs recent cloud/group history into `zalo_history.db`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;

use crate::chat::{self, Thread};
use crate::session::Session;
use crate::store::{self, MessageStore};

const DB_PATH: &str = "zalo_history.db";
const PAGE: u32 = 50;
/// Pages per thread per run (1 000 messages); run again to go further back.
const MAX_PAGES: usize = 20;
const PAUSE: Duration = Duration::from_millis(400);

/// Set by the GUI's "Dừng" button; checked between pages.
pub static CANCEL: AtomicBool = AtomicBool::new(false);
/// True while a Cloud/Group sync runs, so a second click doesn't start a parallel one.
pub static RUNNING: AtomicBool = AtomicBool::new(false);

/// Set by the GUI's "Dừng đồng bộ điện thoại" button (#68).
pub static PHONE_SYNC_CANCEL: AtomicBool = AtomicBool::new(false);
/// True while a phone message sync (`sync_phone_messages`) is active (#68).
pub static PHONE_SYNC_RUNNING: AtomicBool = AtomicBool::new(false);

/// Set by the GUI's "Dừng" button of the deep-sync card.
pub static DEEP_SYNC_CANCEL: AtomicBool = AtomicBool::new(false);
/// True while the deep phone sync (`sync_deep_from_phone`) is active.
pub static DEEP_SYNC_RUNNING: AtomicBool = AtomicBool::new(false);

/// Transfer session delivered by the phone via the `transfer_after_login` control event
/// (`{act, data:{temp_key, imei, pc_name, public_key}}`, re-notes/09 §2). The phone/server
/// issues these — a locally generated key answers `114` ([P] in re-notes/09).
#[derive(Debug, Clone)]
pub struct TransferKeys {
    pub public_key: String,
    pub temp_key: String,
}

/// Latest transfer keys observed by the worker's socket loop, for the deep sync to consume.
pub static TRANSFER_KEYS: std::sync::Mutex<Option<TransferKeys>> = std::sync::Mutex::new(None);

/// Extract transfer-related control acts from a socket payload node.
///
/// Mirrors `extract_mute_controls`: control events ride `data.controls[]` entries with an
/// `act` field. Returns the keys for `transfer_after_login` (imei must match ours, like
/// the official `isTransferAfterLoginControlEvent` predicate); logs the other acts.
pub fn extract_transfer_act(
    data_node: &serde_json::Value,
    expected_imei: &str,
) -> Option<TransferKeys> {
    let acts: Vec<(String, &serde_json::Value)> = match data_node.get("controls").and_then(|c| c.as_array()) {
        Some(arr) => arr
            .iter()
            .map(|c| {
                (
                    c.get("act").and_then(|a| a.as_str()).unwrap_or("").to_string(),
                    c,
                )
            })
            .collect(),
        None => data_node
            .get("act")
            .and_then(|a| a.as_str())
            .map(|top| vec![(top.to_string(), data_node)])?,
    };
    for (act, node) in acts {
        let data = node.get("data").unwrap_or(node);
        match act.as_str() {
            "transfer_after_login" => {
                let temp_key = data.get("temp_key").and_then(|t| t.as_str()).unwrap_or("");
                let public_key = data
                    .get("public_key")
                    .or_else(|| data.get("publicKey"))
                    .and_then(|k| k.as_str())
                    .unwrap_or("");
                let event_imei = data.get("imei").and_then(|i| i.as_str()).unwrap_or("");
                if temp_key.is_empty() || public_key.is_empty() {
                    continue;
                }
                if !event_imei.is_empty() && event_imei != expected_imei {
                    continue;
                }
                return Some(TransferKeys {
                    public_key: public_key.to_string(),
                    temp_key: temp_key.to_string(),
                });
            }
            "user_confirm" | "syncmsg_info" | "transfer_error" => {
                crate::logger::log_error(
                    "history_sync::transfer_act",
                    &format!("{act} event received"),
                );
            }
            _ => {}
        }
    }
    None
}

/// Pull the phone's full message backup page-by-page into `zalo_history.db`
/// (deep history for 1-1 AND groups; the official PC client's own fill path, re-notes/09).
///
/// Flow: wake the phone (cmd 12888) with the delivered transfer keys, retry page 0 until
/// the phone confirms (or timeout), then page `from_seq_id = 0,1,2…` committing each page
/// to SQLite so a cancel or crash resumes from the checkpoint, never from scratch. Never
/// calls `delete_snapshot_mobile_msg` (cmd 12003) — that destroys the server copy.
///
/// Returns `(added_messages, last_seq_id)`.
///
/// # Errors
/// Only transport/setup failures bail; per-page business errors end the loop and are
/// reported through the final progress status (Vietnamese).
pub async fn sync_deep_from_phone<F>(
    s: &Session,
    threads: &mut Vec<Thread>,
    keys: Option<TransferKeys>,
    mut on_progress: F,
) -> Result<(usize, i64)>
where
    F: FnMut(f32, String),
{
    if s.login.is_none() {
        anyhow::bail!("chưa đăng nhập");
    }
    let delivered = keys.or_else(|| TRANSFER_KEYS.lock().ok().and_then(|g| g.clone()));
    let pub_key = delivered
        .as_ref()
        .map(|k| k.public_key.clone())
        .unwrap_or_else(generate_sync_public_key);
    let temp_key = delivered
        .as_ref()
        .map(|k| k.temp_key.clone())
        .unwrap_or_default();

    let before_total: usize = threads.iter().map(|t| t.messages.len()).sum();

    // Resume from the checkpoint only if it belongs to the same transfer session.
    let mut seq = {
        let pub_key = pub_key.clone();
        tokio::task::spawn_blocking(move || {
            MessageStore::open(DB_PATH)
                .ok()?
                .resume_seq_if_same_session(&pub_key)
                .ok()
                .flatten()
        })
        .await?
        .unwrap_or(0)
    };

    // Phase 1: wake the phone with the real keys. Failures are logged, not fatal — the
    // phone may still push once the user confirms on screen.
    on_progress(0.05, "📲 Đang gửi yêu cầu đồng bộ sâu tới điện thoại…".to_string());
    if let Err(e) = chat::request_phone_sync_v2(s, &pub_key, &temp_key).await {
        crate::logger::log_error("sync_deep_from_phone::wake_v2", &e.to_string());
    }

    // Phase 2: wait for the phone to confirm by probing page 0 (answer 114 = not yet).
    let wait_deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    let mut page = None;
    while tokio::time::Instant::now() < wait_deadline {
        if DEEP_SYNC_CANCEL.load(Ordering::Relaxed) {
            let _ = chat::cancel_phone_sync(s, &pub_key).await; // best-effort cleanup (INV-3)
            return Ok((0, 0));
        }
        match chat::pull_mobile_page(s, &pub_key, 0, &temp_key).await {
            Ok(resp) if !chat::mobile_page_rejected(&resp) => {
                page = Some(resp);
                break;
            }
            Ok(_) => { /* 114/-69: phone has not prepared the snapshot yet */ }
            Err(e) => crate::logger::log_error("sync_deep_from_phone::probe", &e.to_string()),
        }
        on_progress(
            0.10,
            "⏳ Đang chờ điện thoại xác nhận đồng bộ (mở Zalo trên điện thoại nếu được hỏi)…"
                .to_string(),
        );
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    let Some(mut page) = page else {
        anyhow::bail!("điện thoại không xác nhận đồng bộ trong thời gian chờ");
    };

    // Phase 3: page through the backup. `from_seq_id` advances by 1 per pull (official
    // client: `t ? t+=1 : t=0`); exhaustion = a page that merges nothing new, or a
    // rejected cursor (114/-69). Progress is heuristic — the total page count is unknown.
    let mut added_total = 0usize;
    let mut stale_pages = 0usize;
    const MAX_PAGES: usize = 20_000;
    const MAX_STALE: usize = 3;
    for page_idx in 0..MAX_PAGES {
        if DEEP_SYNC_CANCEL.load(Ordering::Relaxed) {
            break;
        }
        let frac = 0.15 + 0.8 * (page_idx as f32 / 50.0).tanh().min(0.8);
        on_progress(
            frac.max(0.15),
            format!("📥 Đang tải trang {seq} từ điện thoại… (+{added_total} tin)"),
        );
        let before = threads.iter().map(|t| t.messages.len()).sum::<usize>();
        chat::merge_mobile_page(threads, &page);
        let page_msgs = chat::count_mobile_page_msgs(&page);
        let after = threads.iter().map(|t| t.messages.len()).sum::<usize>();
        let added = after.saturating_sub(before);
        added_total += added;

        let snapshot = threads.clone();
        let (pk, tk, s_seq) = (pub_key.clone(), temp_key.clone(), seq);
        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut st = MessageStore::open(DB_PATH)?;
            store::persist_threads(&mut st, &snapshot)?;
            st.set_deep_sync_checkpoint(s_seq + 1, &pk, &tk)?;
            Ok(())
        })
        .await??;

        if page_msgs == 0 {
            break; // nothing at this seq: snapshot exhausted
        }
        stale_pages = if added == 0 { stale_pages + 1 } else { 0 };
        if stale_pages >= MAX_STALE {
            break;
        }
        seq += 1;
        tokio::time::sleep(PAUSE).await;
        match chat::pull_mobile_page(s, &pub_key, seq, &temp_key).await {
            Ok(next) if !chat::mobile_page_rejected(&next) => page = next,
            Ok(_) => break, // 114/-69: snapshot exhausted or session lost
            Err(e) => {
                crate::logger::log_error("sync_deep_from_phone::page", &e.to_string());
                break;
            }
        }
    }

    // Phase 4: resolve names, dedup, persist the merged state (same tail as phone sync).
    on_progress(0.97, "💾 Đang lưu lịch sử đã tải…".to_string());
    chat::resolve_thread_names(s, threads).await;
    chat::deduplicate_threads(threads, None);
    let snapshot = threads.clone();
    let db_added = tokio::task::spawn_blocking(move || -> Result<usize> {
        let mut st = MessageStore::open(DB_PATH)?;
        let n = store::persist_threads(&mut st, &snapshot)?;
        chat::save_threads_cache(&snapshot);
        Ok(n)
    })
    .await??;

    let after_total: usize = threads.iter().map(|t| t.messages.len()).sum();
    let added = db_added
        .max(after_total.saturating_sub(before_total))
        .max(added_total);
    on_progress(
        1.0,
        format!("✅ Đồng bộ sâu hoàn tất: +{added} tin nhắn (đến trang {seq})"),
    );
    Ok((added, seq))
}


/// Fetch older pages for `t` until the server has nothing older, `MAX_PAGES` is hit, or the user
/// cancels. Each page is committed immediately so a crash or cancel loses nothing. SQLite work
/// runs on the blocking pool; no connection is held across an await.
/// Returns how many messages were new to the database.
pub async fn sync_thread(s: &Session, t: &mut Thread) -> Result<usize> {
    let id = t.id.clone();
    let mut cursor =
        tokio::task::spawn_blocking(move || MessageStore::open(DB_PATH).ok()?.oldest_msg_id(&id))
            .await?
            .unwrap_or_default();
    let mut added = 0;
    for _ in 0..MAX_PAGES {
        if CANCEL.load(Ordering::Relaxed) {
            break;
        }
        let resp = chat::get_cloud_messages(s, &t.id, &cursor, true, PAGE).await?;
        let before = t.messages.len();
        chat::merge_group_history(t, &resp);
        if t.messages.len() == before {
            break; // nothing older on the server
        }
        let snapshot = t.clone();
        added += tokio::task::spawn_blocking(move || {
            store::persist_threads(
                &mut MessageStore::open(DB_PATH)?,
                std::slice::from_ref(&snapshot),
            )
        })
        .await??;
        match next_cursor(t, &cursor) {
            Some(c) => cursor = c,
            None => break,
        }
        tokio::time::sleep(PAUSE).await;
    }
    Ok(added)
}

/// The oldest server message id after a merge, or `None` when the page didn't move the cursor
/// back (the server returned only messages we already had); continuing would loop forever.
/// Local echoes (`out_*`) are not valid cursors, hence the digits-only filter.
fn next_cursor(t: &Thread, current: &str) -> Option<String> {
    let oldest = t
        .messages
        .iter()
        .map(|m| m.id.as_str())
        .find(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))?;
    (oldest != current).then(|| oldest.to_string())
}

/// Generate a 32-byte base64 key token for `pullMobileMsg` (`cmd 12000`) / `requestSyncMessage` (`cmd 12888`).
pub fn generate_sync_public_key() -> String {
    use base64::Engine;
    let hex = crate::param_cipher::random_hex(64);
    let mut raw = Vec::with_capacity(32);
    let bytes = hex.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        let hi = (bytes[i] as char).to_digit(16).unwrap_or(0) as u8;
        let lo = (bytes[i + 1] as char).to_digit(16).unwrap_or(0) as u8;
        raw.push((hi << 4) | lo);
        i += 2;
    }
    base64::engine::general_purpose::STANDARD.encode(raw)
}

/// Synchronize 1-1 and group messages from the user's phone and Zalo cloud (`Đồng bộ tin nhắn từ điện thoại`, #68).
///
/// Workflow:
/// 1. Sends mobile pull signal (`request_phone_sync` `cmd 12000` + `request_phone_sync_v2` `cmd 12888`)
///    so the phone Zalo app is notified to push recent 1-1 & group messages.
/// 2. Drains `lp_chat` (`get_chat_from_polling`) across multiple batches to collect queued 1-1 & group messages.
/// 3. Queries `/api/preloadconvers/get-last-msgs` (`get_last_msgs`) in 25-thread batches across all
///    1-1 and group conversations and merges messages into `threads`.
/// 4. Fetches recent & older pages via `/api/cm/getrecentv2` and `/api/cm/getoldv2` for Cloud & groups.
/// 5. Persists all merged messages to `zalo_history.db` and `threads_cache.json`.
///
/// Returns `(added_messages, synced_threads_count)`.
pub async fn sync_phone_messages<F>(
    s: &Session,
    poll_state: &tokio::sync::Mutex<chat::PollState>,
    threads: &mut Vec<Thread>,
    mut on_progress: F,
) -> Result<(usize, usize)>
where
    F: FnMut(f32, String),
{
    if s.login.is_none() {
        anyhow::bail!("chưa đăng nhập");
    }
    let pub_key = generate_sync_public_key();
    let before_total: usize = threads.iter().map(|t| t.messages.len()).sum();

    // Phase 1: Send mobile wake-up / pull request to phone (`cmd 12000` + `cmd 12888`)
    on_progress(
        0.08,
        "📲 Đang gửi tín hiệu đồng bộ tới Zalo trên điện thoại…".to_string(),
    );
    let _ = chat::request_phone_sync(s, &pub_key).await;
    let _ = chat::request_phone_sync_v2(s, &pub_key, "").await;

    if PHONE_SYNC_CANCEL.load(Ordering::Relaxed) {
        let _ = chat::cancel_phone_sync(s, &pub_key).await;
        return Ok((0, 0));
    }

    // Brief pause to allow the phone/server queue to populate after wake-up signal
    tokio::time::sleep(Duration::from_millis(600)).await;

    // Phase 2: Drain `lp_chat` multi-batch action queues (`getChatFromPolling`)
    on_progress(
        0.22,
        "📥 Đang nhận hàng đợi tin nhắn 1-1 & nhóm từ điện thoại…".to_string(),
    );
    for batch_idx in 0..5 {
        if PHONE_SYNC_CANCEL.load(Ordering::Relaxed) {
            let _ = chat::cancel_phone_sync(s, &pub_key).await;
            return Ok((0, 0));
        }
        let lp_res = {
            let mut ps = poll_state.lock().await;
            chat::get_chat_from_polling(s, &mut ps).await
        };
        let Ok(lp_resp) = lp_res else {
            break;
        };
        chat::merge_previews(threads, &lp_resp);
        if let Some(data_node) = lp_resp.get("data") {
            crate::unread::merge_read_points(threads, data_node);
        }
        let data_node = lp_resp.get("data").unwrap_or(&lp_resp);
        let more = data_node
            .get("more")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        let has_reset = data_node
            .get("resetLastActionId")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|r| !r.is_empty() && r != "0");
        on_progress(
            0.22 + (batch_idx as f32 + 1.0) * 0.04,
            format!("📥 Đã nhận lô hàng đợi #{} từ điện thoại…", batch_idx + 1),
        );
        if more == 0 && !has_reset {
            break;
        }
    }

    // Phase 3: Batch-query `/api/preloadconvers/get-last-msgs` across all 1-1 and group conversations
    on_progress(
        0.45,
        "🔄 Đang đồng bộ tin nhắn các cuộc trò chuyện 1-1 & nhóm…".to_string(),
    );
    if let Ok(preview_resp) = chat::get_last_msgs(s, &[]).await {
        chat::merge_previews(threads, &preview_resp);
    }
    let known_ids: Vec<String> = threads
        .iter()
        .filter(|t| !t.id.is_empty())
        .map(|t| {
            if t.is_group && !t.id.starts_with('g') && !t.id.starts_with("group_") {
                format!("g{}", t.id)
            } else {
                t.id.clone()
            }
        })
        .collect();
    let chunks: Vec<&[String]> = known_ids.chunks(25).collect();
    let total_chunks = chunks.len().max(1);
    for (idx, chunk) in chunks.into_iter().enumerate() {
        if PHONE_SYNC_CANCEL.load(Ordering::Relaxed) {
            let _ = chat::cancel_phone_sync(s, &pub_key).await;
            break;
        }
        if let Ok(preview_resp) = chat::get_last_msgs(s, chunk).await {
            chat::merge_previews(threads, &preview_resp);
        }
        let frac = 0.45 + 0.25 * ((idx + 1) as f32 / total_chunks as f32);
        on_progress(
            frac,
            format!(
                "🔄 Đang đồng bộ hội thoại 1-1 & nhóm ({}/{total_chunks} nhóm lô)…",
                idx + 1
            ),
        );
        tokio::time::sleep(Duration::from_millis(120)).await;
    }

    // Phase 4: Sync Cloud & group conversations via `/api/cm/getrecentv2` + `/api/cm/getoldv2`
    let send2me_id = s
        .login
        .as_ref()
        .map(|l| l.send2me_id.clone())
        .unwrap_or_default();
    let cm_targets: Vec<usize> = threads
        .iter()
        .enumerate()
        .filter(|(_, t)| {
            !t.id.is_empty()
                && (t.is_group
                    || t.name.contains("Cloud của tôi")
                    || (!send2me_id.is_empty() && t.id == send2me_id))
        })
        .map(|(i, _)| i)
        .collect();
    let total_cm = cm_targets.len().max(1);
    for (pos, tidx) in cm_targets.into_iter().enumerate() {
        if PHONE_SYNC_CANCEL.load(Ordering::Relaxed) {
            let _ = chat::cancel_phone_sync(s, &pub_key).await;
            break;
        }
        let (tid, tname) = {
            let t = &threads[tidx];
            (t.id.clone(), t.name.clone())
        };
        let frac = 0.70 + 0.25 * ((pos + 1) as f32 / total_cm as f32);
        on_progress(
            frac,
            format!("☁️ Đang đồng bộ lịch sử: {tname} ({}/{total_cm})…", pos + 1),
        );
        if let Ok(cm_resp) = chat::get_cloud_messages(s, &tid, "0", false, PAGE).await {
            chat::merge_group_history(&mut threads[tidx], &cm_resp);
        }
        if let Some(oldest) = next_cursor(&threads[tidx], "") {
            if let Ok(old_resp) = chat::get_cloud_messages(s, &tid, &oldest, true, PAGE).await {
                chat::merge_group_history(&mut threads[tidx], &old_resp);
            }
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }

    // Phase 5: Resolve contact/group display names, deduplicate, and persist to SQLite (`zalo_history.db`)
    on_progress(
        0.96,
        "💾 Đang lưu tin nhắn đã đồng bộ vào cơ sở dữ liệu zalo_history.db…".to_string(),
    );
    chat::resolve_thread_names(s, threads).await;
    chat::deduplicate_threads(threads, None);

    let snapshot = threads.clone();
    let db_added = tokio::task::spawn_blocking(move || -> Result<usize> {
        let mut st = MessageStore::open(DB_PATH)?;
        let n = store::persist_threads(&mut st, &snapshot)?;
        chat::save_threads_cache(&snapshot);
        Ok(n)
    })
    .await??;

    let after_total: usize = threads.iter().map(|t| t.messages.len()).sum();
    let added = db_added.max(after_total.saturating_sub(before_total));
    let active_threads = threads.iter().filter(|t| !t.messages.is_empty()).count();
    on_progress(
        1.0,
        format!("✅ Đồng bộ hoàn tất: +{added} tin nhắn trên {active_threads} hội thoại"),
    );
    Ok((added, active_threads))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::ChatMessage;

    fn thread_with(ids: &[&str]) -> Thread {
        Thread {
            id: "g1".into(),
            name: "Nhóm".into(),
            preview: String::new(),
            time: String::new(),
            avatar: None,
            is_group: true,
            messages: ids
                .iter()
                .map(|id| ChatMessage {
                    id: (*id).into(),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn cursor_moves_to_the_oldest_server_message() {
        assert_eq!(
            next_cursor(&thread_with(&["5", "9"]), "9").as_deref(),
            Some("5")
        );
        assert_eq!(
            next_cursor(&thread_with(&["out_1", "7", "9"]), "9").as_deref(),
            Some("7")
        );
    }

    #[test]
    fn cursor_stops_when_page_brought_nothing_older() {
        assert_eq!(next_cursor(&thread_with(&["5", "9"]), "5"), None);
        assert_eq!(next_cursor(&thread_with(&[]), "5"), None);
        assert_eq!(next_cursor(&thread_with(&["", "out_2"]), "5"), None);
    }

    #[test]
    fn generate_sync_public_key_produces_valid_32_byte_base64() {
        use base64::Engine;
        let k1 = generate_sync_public_key();
        let k2 = generate_sync_public_key();
        assert_ne!(k1, k2);
        let raw = base64::engine::general_purpose::STANDARD
            .decode(&k1)
            .expect("valid base64");
        assert_eq!(raw.len(), 32);
    }
}
