# 09 — 1-1 deep history & mobile transfer sync (option B investigation)

Date: 2026-11-06. Target: Zalo PC 26.10.20 `app.asar` + live probe on a real session
(Node replica of the standard request path: AES-128-CBC zero-IV params from
`secret_key_b64`, cookie `zpw_sek`, `zpw_ver=694&zpw_type=30`).

Evidence tags: [O] = read from shipped bundle, [P] = live probe result, [I] = inference.

---

## 1. Verdict on cloud-cm for 1-1 (`/api/cm/getoldv2` etc.)

[O] `compact-app-pc` has `getCM` (cmd 12505/12506 → `/api/cm/getoldv2`/`getrecentv2`),
`getCloudMessageJump` (12508 → `/api/cm/rgetv2`, `msgJump`), `getCloudMessagePath`
(12504 → `/api/cm/getpath`, `reverse`). Param shape:
`{groupId, globalMsgId, count, msgIds, imei, isOA?, src}` — official default `src=-1`;
send2me adds `isOA:1` and uses `src:3`.

[O] The loader is gated **client-side** by `apiEnable`:

```js
const s = t.startsWith(GROUPID_PREFIX)
       || t === sendToMeId && cloud_send2me.enable
       || t === sendToMeId && cloud.OA_enable;
return n && s; // n = per-apiType flags (scroll_top_enable, jump_enable, …)
```

Plain 1-1 ids never reach `getCM` on the official client; 1-1 history comes from the
local SQLite filled by sync-v2.

[P] Server confirms the gate (session: web/QR flavour):

| call | 1-1 id | group id |
|---|---|---|
| getrecentv2 (src −1/1/2/3, gmid 0/MAX) | `10002` | `0` (accepted) |
| getoldv2 (any src, gmid 0/MAX) | `-69` "Stop retry" | `-69` when cursor invalid, `0` otherwise |
| rgetv2 msgJump | `10002` | — |
| getrecentv2 isOA=1 on 1-1 | `0` but empty (not an OA) | — |
| `/api/group/history` grid=1-1 | HTML 404 | — |
| `/api/group/getmg-v2` grid=1-1 | `114` | — |

→ **REST paging of old 1-1 messages via cm is not available, server-side.**

[P] On the probe account the cm service also returned `groupMsgs: []` with
`lastMsgId:"0"` for every group and for send2me (`isOA:1`) — i.e. cloud-cm has no
message store content for this account flavour at all. Groups history sync via cm
silently yields nothing here; consistent with the local DB holding zero group rows.

## 2. The real deep-history path: mobile transfer sync (`pull_mobile_msg`)

[O] Full REST family on the **file domain** (service-map `file`):

| cmd | path | params |
|---|---|---|
| 12000 | `/api/message/pull_mobile_msg` | `{pc_name, public_key, from_seq_id, is_retry, min_seq_id, temp_key, imei}` |
| 12700 | `/api/message/cancel_pull_mobile_msg` | `{pc_name, public_key, imei}` |
| 12003 | `/api/message/delete_snapshot_mobile_msg` | `{public_key, imei}` |
| 12096 | `/api/message/get_backupmsginfo` | none (common only) — 404 on web-flavour session [P] |
| 12412 | `/api/message/get_crossdb` | `{pc_name, sync_session}` |
| 12888 | `/api/transfer-sync-v2/request-sync` | `{reqId, data}` (our `request_phone_sync_v2`) |

[O] `pullMobileMsg(e,t,…){t<0&&…; t?t+=1:t=0; …from_seq_id:t…}` — **sequential paging**:
each call sends `from_seq_id = previous + 1` (start at 0).

[O] Key delivery is **push, not generated**: control events carry
`{act, data:{temp_key, imei, pc_name, public_key, …}, ts}`. Predicate module:

```js
isTransferAfterLoginControlEvent: act === "transfer_after_login"
    && !!data.temp_key && data.temp_key.length > 0 && data.imei === getZaloClientID()
isUserConfirm: act === "user_confirm" && (user_action === 1 || user_action === 3)
isUserReject: user_action === 0;  isMobileRestoring: user_action === 2
isBackupSuccess: act === "syncmsg_info";  isBackupFail: act === "transfer_error" && error_code !== 0
```

[O] Consumer (`validateTransferWhenLoginEvents` → `processTransferWhenLogin`):
take the newest valid `transfer_after_login` event from a control batch, extract
`data.temp_key`, `machine.send({type:"START_AUTO_SYNC", tempKey})`. A dev tool offers
`tempKeySource ∈ {real, "arbitrary_temp_key_12345", ""}`.

[I] Therefore the current `sync_phone_messages` cannot work as a *deep* sync: it
generates a random `public_key`, sends one 12000 call with `temp_key:""`, swallows the
error (`let _ = …`), and drains `lp_chat`. [P] The 12000 call answers
`error_code 114 "Tham số không hợp lệ"` for a random/unregistered `public_key`.
What the user receives today comes only from `lp_chat` + `get_last_msgs` (recent window).

## 3. Implementation sketch (what "deep sync" must do)

1. Surface `act` control events from the existing socket/control plane (identify the
   frame cmd that carries them; unrecognized cmds are already logged in `--dev`).
2. On `transfer_after_login`: capture `temp_key` + `public_key`, notify the UI.
3. Trigger: `12888 request-sync`; the phone may require on-screen confirm (`user_confirm`).
4. Page 12000 with `from_seq_id = 0,1,2…` (+ `temp_key`), until the page reports
   exhaustion; decrypt/merge segments (decryption key still [U] — likely `temp_key`
   for message payloads, `secret_key` for the envelope; verify on first real pull).
5. Never call 12003 (deletes the server-side snapshot) without explicit user action.

Probe tooling: `scripts/probe_1to1_history.mjs` (read-only cm matrix) and the throwaway
temp probes used during this investigation; do not commit anything that prints message
content or secrets.

---

## 4. Addendum (2026-11-07): the phone trigger is socket-only E2EE — REST 12888 is dead

Live test of the first deep-sync build: the phone never showed a confirm dialog.
Findings from the shipped bundle (`app.asar` 26.10.20) + live probes:

[P] REST `/api/transfer-sync-v2/request-sync` (cmd 12888) with the official
`{reqId, data: JSON.stringify({...})}` shape answers **HTTP 404 HTML on every host**
(`tt-files-wpa.chat.zalo.me`, `tt-files-wpa.zaloapp.com`, `wpa.chat.zalo.me`,
`tt-chat3-wpa.chat.zalo.me`) for a web-flavour session. The endpoint is gone for this
session type — the first build's wake-up could never reach the phone.

[O] The real trigger is a **socket command** (`requestAsyncV2`), not REST. Socket cmd map
(`compact-app-pc`): `SYNC_MESSAGE.{REQUEST: 590, ACK_DELETE_SYNC_SESSION: 591,
REQUEST_MOBILE_WAKE_UP: 592}`, `BACKUP_MSG.{CREATE_SESSION: 631, INIT: 630,
GET_METADATA: 632, SIGNAL_RESTORE: 633, GET_CONFIGS: 634}`.

[O] The sync session is **E2EE over the socket** (`sync-v2` worker, `sync-session` class):

```js
this.syncId = generateRandomString(32);
this.syncKey = s.genSyncKeyPair();          // libsignal-style key pair
this.toDevice = $o;                          // constant target device
// requestSync() payload (socket cmd, likely 590):
{ syncId, syncType: SyncFull,
  ek: toBase64(ek.publicKey),               // encryption keypair (E2EE)
  ik: toBase64(ik.publicKey),               // identity keypair
  toDevice, tempKey: this.oneTimeKey,
  deviceName: r,
  req: {type, priority, queries, batchSize} }
// wakeup() → cmd 592 with {syncId, toDevice}; batches arrive as socket events
// (OnMessageBatchReceived / OnMetaBatchReceived / OnMobileStatusReceived)
```

[I] Implementing phone-triggered deep sync faithfully means implementing the sync-v2
E2EE session handshake (key pairs, one-time keys, scopes/partitions/batch assembly) —
the largest unimplemented subsystem of the official client, previously deferred in
`02_chat_data.md` §5. The REST `pull_mobile_msg` paging (§2 above) remains valid but
needs the server-initiated `transfer_after_login` event, which may only fire for the
official client's session flavour or on fresh logins — to be verified with
`ZCALL_WS_TRACE=1` frames around a fresh QR login.

[P] The official Zalo PC install on this machine keeps per-UIN databases
(`%APPDATA%\ZaloData\Database\_production\<uin>\{E2ee,Media,Meta,MsgInfo,Preferences,Reaction,Res,Sync}.db`)
— all whole-file encrypted (no SQLite header). A local-import path (#9) would need a
decryptor for that scheme; community tools exist, unverified.

---

## 5. Addendum (2026-11-07b): ZaloData local-import investigation (option C)

User-visible data confirmed on disk:

- [P] `Database/_production/<uin>/Core/Message/<threadId>.db` — **per-thread message
  DBs (~190 MB)** plus `E2ee/Media/Meta/MsgInfo/Preferences/Reaction/Res/Sync.db`.
- [P] `SecureLocalstorage.db` (plain SQLite, `async-store` table) proves the phone
  transfer ran before: `0_transfer-messages-info = {lastTransferSuccessTime: …}` and
  `0_last-time-close-first-login-transfer-modal`. Also persisted sync sessions
  (`sync-session-<uin>` = `[{toDevice:0, syncId:"<32 chars>"}]` — `toDevice` is `0`).
- [O] Message DBs are **SQLCipher** (`HAS_CODEC` in `nativelibs/sqlite3/binding/…/node_sqlite3.node`;
  `PRAGMA key = "<passphrase>"` string form, applied via a pragma helper in
  `preload-sqlite`; `sqlcipher_export` used for re-encryption).
- [O] `sqlCipherKey(name, rawCipherKey, sessional)` returns the caller-supplied key
  when `isThisDBHasEncrypted` — the key itself is decided above the sqlite layer.
- [P] Key NOT found in: `config.json`, `database-config.json` (progress only),
  `Local State` (has os_crypt.encrypted_key but storage empty), Local Storage /
  Session Storage leveldbs (both root and `Partitions/zalo`), `SecureLocalstorage.db`
  (all values plain JSON), `Storage.db` (friend cache), `Trust.db` (0 rows),
  `Encrypt/DBState.db` (0 rows).
- [P] 45 derived-key candidates (uin/imei/enk/secret_key × sha256/md5/sha1 × v3/v4
  KDF grids) do not open `Meta.db` — the passphrase is generated and stored, not derived
  from session material.
- [O] A `secure()/reveal()` value-encryption manager exists (AES-256-CBC, fixed salt
  `[221,179,255,0,79,11,0,70,61,94,221,189,11,233,96,177]`, PBKDF2-SHA256 1000 iters;
  plus an XOR variant) behind `E.create(scope).init(secrectKey)` — the DB passphrase is
  most likely persisted through this manager with a per-scope secret. Unresolved: where
  `secrectKey` comes from and where the ciphertext lives (candidate: the partition
  IndexedDB `Partitions/zalo/IndexedDB/file__0.indexeddb.leveldb`).

Remaining angles (next session): trace `E.create(...).init(...)` call chain in
`main-dist/*` bundles; scan the partition IndexedDB leveldb; as a fallback, capture the
`PRAGMA key` text from the running Zalo PC utility process memory.

---

## 6. Addendum (2026-11-07c): ZaloData import SOLVED — full local history recovered

The remaining angle worked end-to-end. Chain, all [P] live-verified on this machine:

1. **Passphrase capture**: with Zalo PC running, scan its processes' memory for
   `PRAGMA key="…"` (ASCII, private pages; utility process pid holds it next to the
   `journal_size_limit=536870912` batch). Found one 32-hex-char passphrase per install.
2. **DB open**: every per-thread `Core/Message/<id>.db` + `Meta.db` open with **plain
   SQLCipher defaults** (no `cipher_compatibility`) using that passphrase — verified with
   the `sqlcipher3` Python module (the write-path JS builds `PRAGMA key="…"` + WAL pragmas
   with no cipher overrides; v4 defaults apply).
3. **Field decoding**: `message.message` is stored as a JSON-escaped string whose
   unescaped chars are `plaintext_code ^ passphrase_code[i % len]` (the XOR
   "BasicEncryption" handler; `ev=0` rows). Decode = `json.loads(raw)` → XOR with the
   passphrase → `json.loads` again (values were stored `JSON.stringify`'d).
4. `Meta.db` (`partition_key`) maps msgId → threadId (routing, not keys).

`scripts/import_zalodata.py` implements steps 2–3 (opens each DB from a temp copy;
never touches originals). Result on this machine: **+109,541 messages across 415
threads** (e.g. a 1-1 thread: 25 → 4,998; a group: 0 → 2,616), thread names filled from
ctdozlite's `threads_cache.json` + ZaloData `Storage.db` info-cache (508/510 named).

Caveats:
- The passphrase is per-install/per-account — always obtain it live from memory, never
  hardcode it. Do not commit captured keys.
- Media/sticker messages decode to structured JSON; the importer picks a best-effort
  text (description/title) and keeps the full decoded value in `raw_json`.
- `no such table: message` on some DBs = empty/thread-not-yet-created partitions.

Integration idea (pending): wire this importer into the app's existing migrate feature
(#9) behind the same UI, reusing `store::import_history_from_path`.
