"""Import message history from the official Zalo PC `ZaloData` databases into ctdozlite.

ZaloData layout (re-notes/09 §5):
  <zalo-data>/Database/_production/<uin>/Core/Message/<threadId>.db  — per-thread SQLCipher DBs
  message table: JSON-escaped string = XOR(json-escaped plaintext, cipherKey), i.e.
  value = JSON.stringify(JSON.stringify-escaped) — decode: json.loads(raw) -> XOR with
  the SQLCipher passphrase -> json.loads again.

The SQLCipher passphrase is per-install; obtain it from the running Zalo PC's memory
(see re-notes/09 §5, `scripts/` tooling) and pass it via --key. This script never
touches the originals: every DB is opened from a temp copy.

Usage:
  python scripts/import_zalodata.py --zalo-data <ZaloData dir> --out <zalo_history.db> --key <passphrase>
"""

import argparse
import json
import shutil
import sqlite3
import sys
import tempfile
from pathlib import Path

import sqlcipher3


def decode_message(raw: str, key: str):
    """Decode one `message` column value; returns the plaintext (str or dict)."""
    inner = json.loads(raw)
    pt = "".join(chr(ord(c) ^ ord(key[i % len(key)])) for i, c in enumerate(inner))
    try:
        return json.loads(pt)
    except Exception:
        return pt


def pick_text(value, origin_type: str) -> str:
    """Best-effort human text from a decoded message value."""
    if isinstance(value, str):
        return value
    if isinstance(value, dict):
        for k in ("description", "title", "content", "msg", "text"):
            v = value.get(k)
            if isinstance(v, str) and v:
                return v
        params = value.get("params")
        if isinstance(params, str) and params:
            try:
                p = json.loads(params)
                for k in ("description", "title", "msg"):
                    if isinstance(p.get(k), str) and p[k]:
                        return p[k]
            except Exception:
                pass
        return f"[{origin_type}]"
    return f"[{origin_type}]"


def import_thread_db(db: Path, thread_id: str, key: str, out: sqlite3.Connection, me: str):
    td = tempfile.mkdtemp(prefix="zalo_import_")
    try:
        dst = Path(td) / db.name
        shutil.copy2(db, dst)
        con = sqlcipher3.connect(str(dst))
        try:
            con.execute(f"PRAGMA key='{key}'")
            rows = con.execute(
                "SELECT msgId, fromUid, dName, originMsgType, msgType, serverTime, sendDttm, message "
                "FROM message WHERE message IS NOT NULL"
            ).fetchall()
        finally:
            con.close()
    finally:
        shutil.rmtree(td, ignore_errors=True)

    added = 0
    last_ts = 0
    for msg_id, from_uid, dname, origin_type, msg_type, server_time, send_dttm, raw in rows:
        try:
            value = decode_message(raw, key)
        except Exception:
            continue
        text = pick_text(value, origin_type or "chat")
        if not text:
            continue
        try:
            ts_ms = int(server_time or send_dttm or 0)
        except (TypeError, ValueError):
            ts_ms = 0
        ts = ts_ms // 1000
        last_ts = max(last_ts, ts)
        sender = from_uid or ""
        is_outgoing = 1 if sender == me else 0
        cur = out.execute(
            "INSERT OR IGNORE INTO messages (msg_id, thread_id, sender_id, sender_name, text, msg_type, ts, is_outgoing, raw_json) "
            "VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            (
                msg_id or f"{thread_id}_{ts}",
                thread_id,
                sender,
                dname or "",
                text,
                origin_type or str(msg_type),
                ts,
                is_outgoing,
                json.dumps(value, ensure_ascii=False)[:20000],
            ),
        )
        added += cur.rowcount
    out.execute(
        "INSERT INTO threads (thread_id, name, is_group, last_ts) VALUES (?1, '', ?2, ?3) "
        "ON CONFLICT(thread_id) DO UPDATE SET last_ts = MAX(COALESCE(last_ts, 0), excluded.last_ts)",
        (thread_id, 1 if thread_id != thread_id.lstrip("g") or db.stem.startswith("g") else 0, last_ts),
    )
    return added, len(rows)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--zalo-data", required=True, help="Path to the ZaloData directory")
    ap.add_argument("--out", required=True, help="Target zalo_history.db (ctdozlite schema)")
    ap.add_argument("--key", required=True, help="SQLCipher passphrase (per-install)")
    ap.add_argument("--uin", required=True, help="Zalo UIN (directory under _production)")
    args = ap.parse_args()

    msg_root = Path(args.zalo_data) / "Database" / "_production" / args.uin / "Core" / "Message"
    if not msg_root.is_dir():
        print(f"! không tìm thấy {msg_root}")
        return 1

    out = sqlite3.connect(args.out)
    out.execute(
        "CREATE TABLE IF NOT EXISTS messages ("
        "msg_id TEXT PRIMARY KEY, thread_id TEXT NOT NULL, sender_id TEXT, sender_name TEXT, "
        "text TEXT, msg_type TEXT, ts INTEGER, is_outgoing INTEGER, raw_json TEXT)"
    )
    out.execute("CREATE INDEX IF NOT EXISTS idx_thread_ts ON messages(thread_id, ts)")
    out.execute(
        "CREATE TABLE IF NOT EXISTS threads ("
        "thread_id TEXT PRIMARY KEY, name TEXT, is_group INTEGER, last_ts INTEGER)"
    )

    total_added = 0
    files = sorted(msg_root.glob("*.db"))
    print(f"{len(files)} thread DBs under {msg_root}")
    for i, db in enumerate(files):
        thread_id = db.stem.lstrip("g") if db.stem.startswith("g") and db.stem[1:2].isdigit() else db.stem
        try:
            added, total = import_thread_db(db, thread_id, args.key, out, args.uin)
        except Exception as e:
            print(f"  [{i+1}/{len(files)}] {db.name}: FAIL {type(e).__name__} {str(e)[:60]}")
            continue
        total_added += added
        if i % 25 == 0 or added:
            print(f"  [{i+1}/{len(files)}] {db.name}: +{added}/{total}")
    out.commit()
    n = out.execute("SELECT COUNT(*) FROM messages").fetchone()[0]
    t = out.execute("SELECT COUNT(DISTINCT thread_id) FROM messages").fetchone()[0]
    print(f"done: +{total_added} imported; target now has {n} messages across {t} threads")
    out.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
