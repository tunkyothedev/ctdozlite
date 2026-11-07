"""ZaloData Exporter helper — capture the SQLCipher passphrase of the local Zalo PC.

Read-only: scans the memory of running Zalo processes for the `PRAGMA key="…"`
statement that SQLCipher executes when opening the message databases. Nothing is
sent anywhere; the passphrase only appears in this console (and an optional file).

Usage: keep Zalo PC open and logged in, then:  python zalo_key_capture.py
Requires: Windows + Python 3.8+ (no external packages).
"""

import ctypes
import ctypes.wintypes as wt
import re
import sys

PROCESS_VM_READ = 0x0010
PROCESS_QUERY_INFORMATION = 0x0400
MEM_COMMIT = 0x1000
PAGE_READABLE = {0x02, 0x04, 0x06, 0x20, 0x40}
MAX_REGION = 512 * 1024 * 1024

kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)


class MBI(ctypes.Structure):
    _fields_ = [
        ("BaseAddress", ctypes.c_void_p),
        ("AllocationBase", ctypes.c_void_p),
        ("AllocationProtect", wt.DWORD),
        ("RegionSize", ctypes.c_size_t),
        ("State", wt.DWORD),
        ("Protect", wt.DWORD),
        ("Type", wt.DWORD),
    ]


def zalo_pids():
    out = subprocess.run(["tasklist", "/FI", "IMAGENAME eq Zalo.exe", "/FO", "CSV"],
                         capture_output=True, text=True).stdout
    pids = []
    for line in out.splitlines()[1:]:
        parts = [p.strip('"') for p in line.split('","')]
        if len(parts) > 1 and parts[0].lower() == "zalo.exe":
            try:
                pids.append(int(parts[1]))
            except ValueError:
                pass
    return pids


def scan(pid, patterns):
    h = kernel32.OpenProcess(PROCESS_VM_READ | PROCESS_QUERY_INFORMATION, False, pid)
    if not h:
        return []
    found = []
    addr = 0
    mbi = MBI()
    size = ctypes.sizeof(mbi)
    try:
        while addr < 0x7FFFFFFF:
            if not kernel32.VirtualQueryEx(h, ctypes.c_void_p(addr), ctypes.byref(mbi), size):
                break
            region = mbi.RegionSize
            base = mbi.BaseAddress or addr
            if mbi.State == MEM_COMMIT and mbi.Protect in PAGE_READABLE and region <= MAX_REGION:
                buf = (ctypes.c_char * region)()
                got = ctypes.c_size_t(0)
                if kernel32.ReadProcessMemory(h, ctypes.c_void_p(base), buf, region, ctypes.byref(got)):
                    data = buf.raw[: got.value]
                    for pat in patterns:
                        start = 0
                        while True:
                            i = data.find(pat, start)
                            if i < 0:
                                break
                            found.append(data[i: i + 140])
                            start = i + 1
                            if len(found) > 200:
                                return found
            addr = base + region
            if addr <= base:
                break
    finally:
        kernel32.CloseHandle(h)
    return found


def main():
    import subprocess
    pids = zalo_pids()
    if not pids:
        print("! Không thấy Zalo PC nào đang chạy. Hãy mở Zalo PC, đăng nhập, rồi chạy lại.")
        return 1
    print(f"Đang quét {len(pids)} tiến trình Zalo (chỉ đọc)…")
    pat = re.compile(rb'PRAGMA\s+key\s*=\s*["\']([0-9a-fA-F]{32})["\']')
    keys = set()
    for pid in pids:
        for blob in scan(pid, [b"PRAGMA key"]):
            m = pat.search(blob)
            if m:
                keys.add(m.group(1).decode())
    if not keys:
        print('! Không tìm thấy passphrase. Hãy chắc chắn Zalo PC đã mở và bạn đã mở vài cuộc trò chuyện, rồi chạy lại.')
        return 1
    print("\nPassphrase tìm thấy (dán vào ZaloData Exporter):")
    for k in sorted(keys):
        print(f'  PRAGMA key="{k}"')
    return 0


if __name__ == "__main__":
    sys.exit(main())
