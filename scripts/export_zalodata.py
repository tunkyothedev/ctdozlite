"""Export the official Zalo PC `ZaloData` message history to plain SQLite + JSON.

Decodes every per-thread `Core/Message/<id>.db` (SQLCipher; passphrase via --key,
obtained live per re-notes/09 §6) and writes:
  <out-dir>/zalo_history_<uin>.sqlite — friendly schema for DB Browser / SQL tools
  <out-dir>/zalo_history_<uin>.json   — full structured export (threads + messages)

Read-only on the originals: each DB is opened from a temp copy.

Usage:
  python scripts/export_zalodata.py --zalo-data <ZaloData> --uin <uin> --key <passphrase> \
      [--out-dir <dir>] [--threads-cache <threads_cache.json>] [--format both|sqlite|json]
"""

import argparse
import json
import shutil
import sqlite3
import sys
import tempfile
from datetime import datetime
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


def load_names(zalo_data: Path, uin: str, threads_cache: Path | None) -> dict[str, tuple[str, int]]:
    """thread_id -> (name, is_group). Friends from Storage.db, groups from ctdozlite cache."""
    names: dict[str, tuple[str, int]] = {}
    try:
        st = sqlite3.connect(zalo_data / "Database" / "_production" / "Storage.db")
        for k, v in st.execute('SELECT key, val FROM "info-cache"'):
            if k.startswith("0_"):
                try:
                    d = json.loads(v)
                    if d.get("zName"):
                        names[k[2:]] = (d["zName"], 0)
                except Exception:
                    pass
        st.close()
    except Exception as e:
        print(f"! info-cache: {e}")
    if threads_cache and threads_cache.exists():
        cache = json.loads(threads_cache.read_text(encoding="utf-8"))
        lst = cache if isinstance(cache, list) else cache.get("threads", [])
        for t in lst:
            if t.get("name"):
                names[t["id"]] = (t["name"], 1 if t.get("is_group") else 0)
    return names


def collect(zalo_data: Path, uin: str, key: str) -> dict[str, list[dict]]:
    """thread_id -> sorted list of message dicts, decoded from every per-thread DB."""
    msg_root = zalo_data / "Database" / "_production" / uin / "Core" / "Message"
    if not msg_root.is_dir():
        raise SystemExit(f"! không tìm thấy {msg_root}")
    threads: dict[str, list[dict]] = {}
    files = sorted(msg_root.glob("*.db"))
    print(f"{len(files)} thread DBs")
    for i, db in enumerate(files):
        if db.stem == "stranger_box":
            continue
        thread_id = db.stem[1:] if db.stem.startswith("g") and db.stem[1:2].isdigit() else db.stem
        td = tempfile.mkdtemp(prefix="zalo_export_")
        try:
            dst = Path(td) / db.name
            shutil.copy2(db, dst)
            con = sqlcipher3.connect(str(dst))
            con.execute(f"PRAGMA key='{key}'")
            rows = con.execute(
                "SELECT msgId, cliMsgId, fromUid, toUid, dName, originMsgType, msgType, "
                "serverTime, sendDttm, localDttm, message FROM message WHERE message IS NOT NULL"
            ).fetchall()
            con.close()
        except Exception as e:
            print(f"  [{i+1}/{len(files)}] {db.name}: skip ({type(e).__name__} {str(e)[:50]})")
            continue
        finally:
            shutil.rmtree(td, ignore_errors=True)

        msgs = threads.setdefault(thread_id, [])
        for (msg_id, cli_id, from_uid, to_uid, dname, origin_type, msg_type,
             server_time, send_dttm, local_dttm, raw) in rows:
            try:
                value = decode_message(raw, key)
            except Exception:
                continue
            try:
                ts = int(server_time or send_dttm or local_dttm or 0)
            except (TypeError, ValueError):
                ts = 0
            msgs.append({
                "msg_id": str(msg_id or ""),
                "cli_msg_id": str(cli_id or ""),
                "sender_id": from_uid or "",
                "sender_name": dname or "",
                "is_outgoing": 1 if (from_uid or "") == uin else 0,
                "msg_type": origin_type or str(msg_type),
                "ts": ts,
                "text": pick_text(value, origin_type or "chat"),
                "raw": value,
            })
        if i % 50 == 0:
            print(f"  [{i+1}/{len(files)}] {db.name}: {len(msgs)} msgs")
    for msgs in threads.values():
        msgs.sort(key=lambda m: (m["ts"], m["msg_id"]))
    return threads


def write_sqlite(out_path: Path, uin: str, threads: dict[str, list[dict]], names):
    if out_path.exists():
        out_path.unlink()
    con = sqlite3.connect(str(out_path))
    con.executescript(
        """
        CREATE TABLE threads (
            thread_id TEXT PRIMARY KEY,
            name TEXT,
            is_group INTEGER,
            message_count INTEGER,
            first_ts INTEGER,
            last_ts INTEGER
        );
        CREATE TABLE messages (
            msg_id TEXT,
            thread_id TEXT NOT NULL,
            sender_id TEXT,
            sender_name TEXT,
            is_outgoing INTEGER,
            msg_type TEXT,
            ts INTEGER,
            datetime TEXT,
            text TEXT,
            raw_json TEXT
        );
        CREATE INDEX idx_msg_thread ON messages(thread_id, ts);
        CREATE INDEX idx_msg_sender ON messages(sender_id);
        CREATE TABLE export_info (key TEXT PRIMARY KEY, value TEXT);
        """
    )
    con.execute("INSERT INTO export_info VALUES ('uin', ?)", (uin,))
    con.execute("INSERT INTO export_info VALUES ('exported_at', ?)", (datetime.now().isoformat(),))
    con.execute("INSERT INTO export_info VALUES ('source', 'ZaloData Core/Message per-thread DBs (SQLCipher)')")
    total = 0
    for thread_id, msgs in threads.items():
        name, is_group = names.get(thread_id, ("", 0))
        con.execute(
            "INSERT INTO threads VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            (thread_id, name, is_group, len(msgs),
             msgs[0]["ts"] if msgs else 0, msgs[-1]["ts"] if msgs else 0),
        )
        for m in msgs:
            sender_name = m["sender_name"]
            if not sender_name and not m["is_outgoing"]:
                sender_name = names.get(m["sender_id"], ("", 0))[0] or f"Người dùng {m['sender_id']}"
            dt = datetime.fromtimestamp(m["ts"] / 1000).strftime("%Y-%m-%d %H:%M:%S") if m["ts"] else ""
            con.execute(
                "INSERT INTO messages VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                (m["msg_id"], thread_id, m["sender_id"], sender_name, m["is_outgoing"],
                 m["msg_type"], m["ts"] // 1000 if m["ts"] else 0, dt, m["text"],
                 json.dumps(m["raw"], ensure_ascii=False)[:20000]),
            )
            total += 1
    con.commit()
    con.close()
    return total


def write_json(out_path: Path, uin: str, threads: dict[str, list[dict]], names):
    payload = {
        "exported_at": datetime.now().isoformat(),
        "uin": uin,
        "source": "ZaloData Core/Message per-thread DBs (SQLCipher)",
        "threads": [
            {
                "id": thread_id,
                "name": names.get(thread_id, ("", 0))[0],
                "is_group": names.get(thread_id, ("", 0))[1],
                "message_count": len(msgs),
                "messages": [
                    {**m, "raw": m["raw"] if isinstance(m["raw"], (str, int, float)) else m["raw"]}
                    for m in msgs
                ],
            }
            for thread_id, msgs in sorted(threads.items(), key=lambda kv: -(kv[1][-1]["ts"] if kv[1] else 0))
        ],
    }
    out_path.write_text(json.dumps(payload, ensure_ascii=False), encoding="utf-8")
    return sum(len(m["messages"]) for m in payload["threads"])


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--zalo-data", required=True)
    ap.add_argument("--uin", required=True)
    ap.add_argument("--key", required=True)
    ap.add_argument("--out-dir", default=".")
    ap.add_argument("--threads-cache", default=None, help="ctdozlite threads_cache.json (group names)")
    ap.add_argument("--format", choices=["both", "sqlite", "json"], default="both")
    args = ap.parse_args()

    zalo_data = Path(args.zalo_data)
    out_dir = Path(args.out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)
    names = load_names(zalo_data, args.uin, Path(args.threads_cache) if args.threads_cache else None)
    print(f"names loaded: {len(names)}")

    threads = collect(zalo_data, args.uin, args.key)
    n_threads = sum(1 for m in threads.values() if m)
    n_msgs = sum(len(m) for m in threads.values())
    print(f"decoded: {n_msgs} messages across {n_threads} threads")

    if args.format in ("both", "sqlite"):
        p = out_dir / f"zalo_history_{args.uin}.sqlite"
        n = write_sqlite(p, args.uin, threads, names)
        print(f"sqlite → {p} ({p.stat().st_size/1e6:.1f} MB, {n} rows)")
    if args.format in ("both", "json"):
        p = out_dir / f"zalo_history_{args.uin}.json"
        n = write_json(p, args.uin, threads, names)
        print(f"json   → {p} ({p.stat().st_size/1e6:.1f} MB, {n} messages)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
