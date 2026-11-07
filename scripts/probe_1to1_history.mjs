// Read-only probe: does the cloud-cm service (`/api/cm/*`) serve plain 1-1 threads?
//
// Evidence step for 1-1 deep-history sync (#19 follow-up). The official PC client gates
// `getCM` client-side to groups + send2me (`apiEnable` in compact-app-pc), but that says
// nothing about the server. This probe calls the cm endpoints against real 1-1 ids and a
// group id (positive control), then walks `getoldv2` pages to prove or disprove paging.
//
// Request path replicates `Session::get` + `get_cloud_messages` (src/bin equivalence of
// the Rust standard path): AES-CBC zero-IV PKCS7 params (key = secret_key_b64), cookie
// `zpw_sek`, `zpw_ver=694&zpw_type=30`.
//
// Prints only error codes, message counts and numeric msg ids — never message text,
// never cookies or keys.
//
// Usage:
//   node scripts/probe_1to1_history.mjs [--data-dir <dir>] [--tid <1-1 id>] [--pages 3] [--dry-run]

import { createCipheriv, createDecipheriv } from "node:crypto";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { homedir } from "node:os";

const args = process.argv.slice(2);
const arg = (name) => {
  const i = args.indexOf(name);
  return i >= 0 ? args[i + 1] : undefined;
};
const PAGES = Number(arg("--pages") ?? 3);
const DRY = args.includes("--dry-run");
const DATA_DIR =
  arg("--data-dir") ?? join(homedir(), "AppData", "Local", "Programs", "ctdozlite");

const sess = JSON.parse(readFileSync(join(DATA_DIR, "session.json"), "utf-8"));
const key = Buffer.from(sess.secret_key_b64, "base64");
const AES = key.length === 16 ? "aes-128-cbc" : "aes-256-cbc";
const ZERO_IV = Buffer.alloc(16);
const CLOUD = sess.domains?.cloud_msg || "https://tt-group-cm.api.zaloapp.com";

console.log(
  `✓ session: UIN=${sess.login?.uin} name=${sess.login?.display_name} key=${AES} cloud=${CLOUD}`
);

function encodeAES(text) {
  const c = createCipheriv(AES, key, ZERO_IV);
  return c.update(text, "utf8", "base64") + c.final("base64");
}
function decodeAES(b64) {
  const d = createDecipheriv(AES, key, ZERO_IV);
  return d.update(b64, "base64", "utf8") + d.final("utf8");
}

async function cmGet(endpoint, body) {
  const params = encodeURIComponent(encodeAES(JSON.stringify(body)));
  const url = `${CLOUD.replace(/\/$/, "")}${endpoint}?zpw_ver=694&zpw_type=30&params=${params}&nretry=0`;
  if (DRY) {
    console.log(`  [dry] GET ${endpoint} groupId=${body.groupId} gmid=${body.globalMsgId} src=${body.src}`);
    return { error_code: -2, error_message: "dry-run" };
  }
  const resp = await fetch(url, {
    headers: {
      Cookie: `zpw_sek=${sess.zpw_sek}; _zlang=vi`,
      "zpw_sek": sess.zpw_sek,
      Origin: "https://chat.zalo.me",
      Referer: "https://chat.zalo.me/",
      "User-Agent":
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36",
    },
  });
  const outer = JSON.parse(await resp.text());
  // maybe_decrypt: `data` may be AES/base64 — decrypt and use the inner envelope.
  if (typeof outer.data === "string" && outer.data && !outer.data.startsWith("{")) {
    try {
      return JSON.parse(decodeAES(outer.data));
    } catch {
      try {
        return JSON.parse(outer.data);
      } catch {
        return outer; // undecryptable — surface the outer envelope as-is
      }
    }
  }
  if (typeof outer.data === "string" && outer.data.startsWith("{")) {
    try { return JSON.parse(outer.data); } catch { return outer; }
  }
  return outer;
}

function parseCm(resp) {
  const code = resp?.error_code ?? -1;
  const msg = resp?.error_message ?? "";
  const data = typeof resp?.data === "string" ? safeJson(resp.data) : (resp?.data ?? {});
  const msgs = Array.isArray(data?.groupMsgs) ? data.groupMsgs : [];
  const id = (m) => String(m?.msgId ?? m?.globalMsgId ?? "");
  return {
    code,
    msg,
    n: msgs.length,
    newest: msgs.length ? id(msgs[0]) : "",
    oldest: msgs.length ? id(msgs[msgs.length - 1]) : "",
    hasMore: typeof data?.hasMore === "boolean" ? data.hasMore : undefined,
  };
}
const safeJson = (s) => { try { return JSON.parse(s); } catch { return {}; } };

function show(tag, label, r) {
  console.log(
    `  [${tag}] ${label.padEnd(20)} code=${String(r.code).padEnd(5)} msgs=${String(r.n).padStart(3)} newest=${r.newest} oldest=${r.oldest} hasMore=${r.hasMore}${r.msg ? ` (${r.msg})` : ""}`
  );
}

async function cmCall(tag, label, endpoint, body) {
  const r = parseCm(await cmGet(endpoint, body));
  show(tag, label, r);
  return r;
}

// ---- pick targets ----------------------------------------------------------
let targets = [];
const explicitTid = arg("--tid");
if (explicitTid) {
  targets.push({ tag: "1-1", name: "(from --tid)", id: explicitTid });
} else {
  const cache = JSON.parse(readFileSync(join(DATA_DIR, "threads_cache.json"), "utf-8"));
  const list = Array.isArray(cache) ? cache : (cache.threads ?? []);
  const send2me = sess.login?.send2me_id;
  const ones = list
    .filter((t) => !t.is_group && t.id && t.id !== send2me && !(t.name ?? "").includes("Cloud của tôi"))
    .sort((a, b) => (b.messages?.length ?? 0) - (a.messages?.length ?? 0))
    .slice(0, 2);
  const groups = list
    .filter((t) => t.is_group && t.id)
    .sort((a, b) => (b.messages?.length ?? 0) - (a.messages?.length ?? 0))
    .slice(0, 1);
  for (const t of [...ones, ...groups]) {
    console.log(`✓ target: [${t.is_group ? "GROUP" : "1-1"}] ${t.name} (${t.messages?.length ?? 0} tin cục bộ) id=${t.id}`);
    targets.push({ tag: t.is_group ? "GROUP" : "1-1", name: t.name, id: t.id });
  }
  if (!targets.some((t) => t.tag === "1-1")) throw new Error("không có 1-1 target trong threads_cache.json");
}

// ---- run matrix ------------------------------------------------------------
const MAX = "9223372036854775808";
for (const t of targets) {
  console.log(`\n--- [${t.tag}] ${t.name} id=${t.id} ---`);
  const base = { groupId: t.id, count: 50, msgIds: [], imei: sess.imei };
  const rApp = await cmCall(t.tag, "recent src=1 (app)", "/api/cm/getrecentv2", { ...base, globalMsgId: MAX, src: 1 });
  const rOld = await cmCall(t.tag, "old src=3 (app)", "/api/cm/getoldv2", { ...base, globalMsgId: MAX, src: 3 });
  const rPc = await cmCall(t.tag, "recent src=-1 (PC)", "/api/cm/getrecentv2", { ...base, globalMsgId: "0", src: -1 });
  const cursor = rApp.oldest || rPc.oldest || rOld.oldest;
  if (!cursor) {
    console.log(`  [${t.tag}] không có cursor để thử phân trang`);
    continue;
  }
  console.log(`  [${t.tag}] paging proof (getoldv2 src=3) from cursor ${cursor}:`);
  let cur = cursor;
  for (let p = 0; p < PAGES; p++) {
    const r = await cmCall(t.tag, `page ${p + 1}`, "/api/cm/getoldv2", { ...base, globalMsgId: cur, src: 3 });
    if (r.code !== 0 || r.n === 0 || !r.oldest || r.oldest === cur) break;
    cur = r.oldest;
    await new Promise((res) => setTimeout(res, 400));
  }
}

console.log("\n=== PROBE DONE (read-only; không in nội dung tin nhắn) ===");
