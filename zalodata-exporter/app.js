// ZaloData Exporter — client-side only. Decrypts SQLCipher per-thread DBs from Zalo PC,
// decodes the XOR-obfuscated message fields, and exports SQLite/JSON.
// Algorithm evidence: ctdozlite re-notes/09 §6.
"use strict";

const PS = 4096, RESERVE = 80, ITERS = 256000, KEYLEN = 32;

let SQL = null;
let state = { threads: [], names: {} }; // threads: {id, msgs:[{ts, sender_id, sender_name, is_outgoing, msg_type, text}]}

const $ = (id) => document.getElementById(id);
const setStatus = (s) => { $("status").textContent = s; };
const setBar = (f) => { $("bar").style.width = `${Math.round(f * 100)}%`; };
const setMsg = (s, cls) => { const el = $("msg"); el.className = cls || "err"; el.textContent = s || ""; };

// ---------- SQLCipher page decryption ----------
async function deriveKey(passBytes, salt) {
  const base = await crypto.subtle.importKey("raw", passBytes, "PBKDF2", false, ["deriveBits"]);
  return new Uint8Array(await crypto.subtle.deriveBits(
    { name: "PBKDF2", hash: "SHA-512", salt, iterations: ITERS }, base, KEYLEN * 8));
}

function decryptDb(buf, keyBytes) {
  const nPages = Math.floor(buf.length / PS);
  if (nPages < 1) throw new Error("file quá nhỏ / không phải DB Zalo");
  const out = new Uint8Array(nPages * PS);
  for (let p = 0; p < nPages; p++) {
    const off = p * PS;
    const iv = buf.subarray(off + PS - RESERVE, off + PS - RESERVE + 16);
    const body = buf.subarray(off + (p === 0 ? 16 : 0), off + PS - RESERVE);
    const dec = new aesjs.ModeOfOperation.cbc(keyBytes, iv);
    const pt = dec.decrypt(body);
    if (p === 0) {
      // The plaintext at header offset 16 starts with the 2-byte page-size field —
      // a wrong passphrase decrypts it to garbage, so this is the key check.
      const pageSize = (pt[0] << 8) | pt[1];
      if (pageSize !== PS) throw new Error("KEY sai (page size không khớp)");
      out.set(new TextEncoder().encode("SQLite format 3\x00"), 0);
      out.set(pt, 16);
    } else {
      out.set(pt, off);
    }
  }
  return out;
}

// ---------- Zalo field decoding ----------
function xorDecode(s, key) {
  let out = "";
  for (let i = 0; i < s.length; i++) out += String.fromCharCode(s.charCodeAt(i) ^ key.charCodeAt(i % key.length));
  return out;
}

function decodeMessage(raw, key) {
  const inner = JSON.parse(raw);          // raw is a JSON-escaped string literal
  const pt = xorDecode(inner, key);
  try { return JSON.parse(pt); } catch { return pt; }
}

function pickText(value, originType) {
  if (typeof value === "string") return value;
  if (value && typeof value === "object") {
    for (const k of ["description", "title", "content", "msg", "text"]) {
      if (typeof value[k] === "string" && value[k]) return value[k];
    }
    if (typeof value.params === "string") {
      try {
        const p = JSON.parse(value.params);
        for (const k of ["description", "title", "msg"]) if (typeof p[k] === "string" && p[k]) return p[k];
      } catch {}
    }
    return `[${originType}]`;
  }
  return `[${originType}]`;
}

const fmtTime = (ms) => {
  if (!ms) return "";
  const d = new Date(ms);
  const p = (n) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
};

// ---------- main pipeline ----------
async function processFiles(files, pass) {
  const passBytes = new TextEncoder().encode(pass);
  const threads = new Map();
  let okFiles = 0, skipFiles = 0;
  const fileErrors = [];

  for (let i = 0; i < files.length; i++) {
    const f = files[i];
    if (!f.name.toLowerCase().endsWith(".db") || f.name === "stranger_box.db") { skipFiles++; continue; }
    const threadId = f.name.replace(/\.db$/i, "").replace(/^g(?=\d)/, "");
    setStatus(`Đang giải mã ${i + 1}/${files.length}: ${f.name}…`);
    setBar(i / files.length);
    try {
      const buf = new Uint8Array(await f.arrayBuffer());
      const salt = buf.subarray(0, 16);
      const keyBytes = await deriveKey(passBytes, salt);
      const plain = decryptDb(buf, keyBytes);
      const db = new SQL.Database(plain);
      let hasTable = false;
      try {
        hasTable = db.exec("SELECT 1 FROM sqlite_master WHERE type='table' AND name='message'").length > 0;
      } catch {}
      if (!hasTable) { db.close(); skipFiles++; continue; }
      const res = db.exec(
        "SELECT msgId, fromUid, dName, originMsgType, msgType, serverTime, sendDttm, localDttm, message FROM message WHERE message IS NOT NULL");
      const cols = res.length ? res[0].columns : [];
      const rows = res.length ? res[0].values : [];
      const msgs = [];
      for (const r of rows) {
        const row = Object.fromEntries(cols.map((c, ix) => [c, r[ix]]));
        let value;
        try { value = decodeMessage(row.message, pass); } catch { continue; }
        let ts = Number(row.serverTime || row.sendDttm || row.localDttm || 0);
        msgs.push({
          msg_id: String(row.msgId || ""),
          sender_id: row.fromUid || "",
          sender_name: row.dName || "",
          is_outgoing: (row.fromUid || "") === state.uin ? 1 : 0,
          msg_type: row.originMsgType || String(row.msgType),
          ts,
          text: pickText(value, row.originMsgType || "chat"),
        });
      }
      msgs.sort((a, b) => a.ts - b.ts);
      if (msgs.length) { threads.set(threadId, msgs); okFiles++; } else skipFiles++;
      db.close();
    } catch (e) {
      if (String(e.message).includes("KEY sai")) throw e; // wrong key: stop early
      fileErrors.push(`${f.name}: ${e.message || e}`);
      skipFiles++;
    }
    await new Promise((r) => setTimeout(r, 0)); // keep UI responsive
  }
  setBar(1);
  return { threads, okFiles, skipFiles, fileErrors };
}

async function loadExtraNames() {
  const names = {};
  // plain SQLite Storage.db → friend display names
  if ($("wantStorage").checked && $("storageFile").files[0]) {
    try {
      const buf = new Uint8Array(await $("storageFile").files[0].arrayBuffer());
      const db = new SQL.Database(buf);
      const res = db.exec("SELECT key, val FROM \"info-cache\"");
      if (res.length) {
        for (const [k, v] of res[0].values) {
          if (typeof k === "string" && k.startsWith("0_")) {
            try {
              const d = JSON.parse(v);
              if (d && d.zName) names[k.slice(2)] = { name: d.zName, is_group: 0 };
            } catch {}
          }
        }
      }
      db.close();
    } catch {}
  }
  // ctdozlite threads_cache.json → group + display names
  if ($("wantCache").checked && $("cacheFile").files[0]) {
    try {
      const cache = JSON.parse(await $("cacheFile").files[0].text());
      const lst = Array.isArray(cache) ? cache : cache.threads || [];
      for (const t of lst) {
        if (t.name && t.id) names[t.id] = { name: t.name, is_group: t.is_group ? 1 : 0 };
      }
    } catch {}
  }
  return names;
}

// ---------- exports ----------
function buildExportDb() {
  const db = new SQL.Database();
  db.exec(`
    CREATE TABLE threads (thread_id TEXT PRIMARY KEY, name TEXT, is_group INTEGER, message_count INTEGER, first_ts INTEGER, last_ts INTEGER);
    CREATE TABLE messages (msg_id TEXT, thread_id TEXT NOT NULL, sender_id TEXT, sender_name TEXT, is_outgoing INTEGER, msg_type TEXT, ts INTEGER, datetime TEXT, text TEXT, raw_json TEXT);
    CREATE INDEX idx_msg_thread ON messages(thread_id, ts);
  `);
  const insT = db.prepare("INSERT INTO threads VALUES (?,?,?,?,?,?)");
  const insM = db.prepare("INSERT INTO messages VALUES (?,?,?,?,?,?,?,?,?,?)");
  for (const t of state.threads) {
    const meta = state.names[t.id] || {};
    const name = meta.name || "";
    const isGroup = meta.is_group ?? (t.id !== t.id.replace(/^g(?=\d)/, "") ? 1 : 0);
    insT.run([t.id, name, isGroup, t.msgs.length, t.msgs.length ? t.msgs[0].ts : 0, t.msgs.length ? t.msgs[t.msgs.length - 1].ts : 0]);
    for (const m of t.msgs) {
      const sender = m.sender_name || (!m.is_outgoing ? (namesFor(m.sender_id) || `Người dùng ${m.sender_id}`) : "Tôi");
      insM.run([m.msg_id, t.id, m.sender_id, sender, m.is_outgoing, m.msg_type,
        Math.floor(m.ts / 1000), fmtTime(m.ts), m.text, ""]);
    }
  }
  insT.free(); insM.free();
  return db.export();
}

function namesFor(id) {
  const n = state.names[id];
  return n ? n.name : null;
}

function download(bytes, filename, mime) {
  const blob = new Blob([bytes], { type: mime });
  const a = document.createElement("a");
  a.href = URL.createObjectURL(blob);
  a.download = filename;
  a.click();
  setTimeout(() => URL.revokeObjectURL(a.href), 5000);
}

// ---------- UI wiring ----------
let currentThread = null;

function renderThreads() {
  const tbody = $("threadRows");
  tbody.innerHTML = "";
  const sorted = [...state.threads].sort((a, b) => (b.msgs.at(-1)?.ts || 0) - (a.msgs.at(-1)?.ts || 0));
  for (const t of sorted) {
    const meta = state.names[t.id] || {};
    const tr = document.createElement("tr");
    tr.className = "clickable";
    tr.innerHTML = `<td>${escapeHtml(meta.name || t.id)}</td><td>${meta.is_group ? "Nhóm" : "1-1"}</td>
      <td>${t.msgs.length}</td><td>${fmtTime(t.msgs.at(-1)?.ts)}</td>`;
    tr.onclick = () => openThread(t.id);
    tbody.appendChild(tr);
  }
}

function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
}

function openThread(id) {
  currentThread = state.threads.find((t) => t.id === id);
  const meta = state.names[id] || {};
  $("viewTitle").textContent = `${meta.name || id} — ${currentThread.msgs.length} tin`;
  $("threadTable").classList.add("hide");
  $("msgView").classList.remove("hide");
  $("search").value = "";
  renderMessages("");
}

function renderMessages(filter) {
  const box = $("msgs");
  box.innerHTML = "";
  const q = (filter || "").toLowerCase();
  const list = q ? currentThread.msgs.filter((m) => m.text.toLowerCase().includes(q)) : currentThread.msgs;
  const slice = q ? list.slice(-500) : list.slice(-300);
  for (const m of slice) {
    const div = document.createElement("div");
    div.className = "msg" + (m.is_outgoing ? " out" : "");
    const who = m.is_outgoing ? "Tôi" : (m.sender_name || namesFor(m.sender_id) || m.sender_id);
    div.innerHTML = `<div class="meta">${escapeHtml(fmtTime(m.ts))} · ${escapeHtml(who)} · ${escapeHtml(m.msg_type)}</div>${escapeHtml(m.text)}`;
    box.appendChild(div);
  }
  if (!slice.length) box.innerHTML = `<div class="msg muted">Không có tin nào khớp.</div>`;
  box.scrollTop = box.scrollHeight;
}

async function main() {
  SQL = await initSqlJs({ locateFile: (f) => "vendor/" + f });

  const drop = $("drop"), input = $("files");
  drop.onclick = () => input.click();
  drop.ondragover = (e) => { e.preventDefault(); drop.classList.add("hover"); };
  drop.ondragleave = () => drop.classList.remove("hover");
  drop.ondrop = (e) => { e.preventDefault(); drop.classList.remove("hover"); input.files = e.dataTransfer.files; input.onchange(); };
  input.onchange = () => {
    const fs = [...input.files];
    $("fileList").textContent = fs.length ? `${fs.length} file: ` + fs.slice(0, 6).map((f) => f.name).join(", ") + (fs.length > 6 ? "…" : "") : "";
    $("go").disabled = !($("pass").value.trim() && fs.length);
  };
  $("pass").oninput = () => { $("go").disabled = !($("pass").value.trim() && input.files.length && $("uin").value.trim()); };
  $("uin").oninput = $("pass").oninput;
  $("wantStorage").onchange = $("wantCache").onchange = () => $("extraSlots").classList.toggle("hide", !($("wantStorage").checked || $("wantCache").checked));

  $("go").onclick = async () => {
    setMsg("");
    const pass = $("pass").value.trim();
    if (!pass) { setMsg("Chưa nhập passphrase."); return; }
    state.uin = $("uin").value.trim();
    if (!state.uin) { setMsg("Chưa nhập UIN (tên thư mục trong Database\\_production\\)."); return; }
    $("go").disabled = true;
    try {
      const { threads, okFiles, skipFiles, fileErrors } = await processFiles([...input.files], pass);
      state.names = await loadExtraNames();
      state.threads = [...threads.entries()].map(([id, msgs]) => ({ id, msgs }));
      const total = state.threads.reduce((s, t) => s + t.msgs.length, 0);
      $("summary").textContent = `✅ ${total} tin nhắn · ${state.threads.length} hội thoại (đọc được ${okFiles} file, bỏ qua ${skipFiles})` +
        (fileErrors.length ? ` — lỗi: ${fileErrors.slice(0, 3).join(" | ")}` : "");
      renderThreads();
      $("resultCard").classList.remove("hide");
      $("resultCard").scrollIntoView({ behavior: "smooth" });
    } catch (e) {
      setMsg("Lỗi: " + (e.message || e));
    } finally {
      $("go").disabled = false;
    }
  };

  $("back").onclick = () => { $("msgView").classList.add("hide"); $("threadTable").classList.remove("hide"); };
  $("search").oninput = (e) => renderMessages(e.target.value);

  $("expSqlite").onclick = () => {
    const bytes = buildExportDb();
    download(bytes, `zalo_history_${state.uin || "export"}.sqlite`, "application/octet-stream");
  };
  $("expJson").onclick = () => {
    // ctdozlite-native Thread[] shape — importable directly via Cài đặt → Nhập lịch sử.
    const payload = state.threads.map((t) => {
      const meta = state.names[t.id] || {};
      return {
        id: t.id,
        name: meta.name || `Người dùng ${t.id}`,
        preview: "",
        time: "",
        avatar: null,
        is_group: !!meta.is_group,
        messages: t.msgs.map((m) => ({
          id: m.msg_id,
          sender: m.sender_id,
          text: m.text,
          time: fmtTime(m.ts).slice(11, 16),
          is_outgoing: !!m.is_outgoing,
          ts: Math.floor(m.ts / 1000),
        })),
      };
    });
    download(JSON.stringify(payload), `zalo_history_${state.uin || "export"}.json`, "application/json");
  };
}

main();
