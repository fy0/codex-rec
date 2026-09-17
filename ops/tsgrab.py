#!/usr/bin/env python3
"""tsgrab -- fetch a *fresh* `x-codex-turn-state` from the Codex backend by
reading only the response header block and closing the socket immediately.

Why: the turn-state token is server-issued and time-bounded (community: ~1h).
To test "replay a good token" you need one that is still young, and you want to
get it without paying for a full generation.

Subcommands
  grab   probe until a token of the wanted length shows up; save it
  use    replay a real codex request with a stored token injected; report
         whether upstream ECHOES it back (accepted / sticky) or issues a new one
  info   decode a stored/raw token: version byte, issued-at, byte size

The probe is byte-faithful: it replays an actual captured request body
(zstd, straight off the wire) with only the session/thread/turn ids regenerated,
so the routing class the server sees is the same as a real client's.
"""

from __future__ import annotations

import argparse
import base64
import glob
import json
import os
import random
import re
import socket
import ssl
import subprocess
import sys
import time
from datetime import datetime, timezone

HOST = "chatgpt.com"
PORT = 443
ROUTE = "/backend-api/codex/responses"
AUTH = "/root/.codex/auth.json"
INSTALL_ID = "52e15863-9a4c-4e2d-a9a9-949e497dc41f"
REC = "/root/rec"
ZSTD_MAGIC = b"\x28\xb5\x2f\xfd"


# --------------------------------------------------------------------------- helpers
def now() -> str:
    return datetime.now(timezone.utc).strftime("%H:%M:%S")


def log(*a):
    print(*a, flush=True)


def load_auth(path: str = AUTH) -> tuple[str, str]:
    d = json.load(open(path))
    t = d["tokens"]
    return t["access_token"], t["account_id"]


def uuid7() -> str:
    """Timezone-ordered UUID, same shape the codex client uses (01a0...)."""
    ms = int(time.time() * 1000) & ((1 << 48) - 1)
    ra = random.getrandbits(12)
    rb = random.getrandbits(62)
    v = (ms << 80) | (0x7 << 76) | (ra << 64) | (0b10 << 62) | rb
    h = f"{v:032x}"
    return f"{h[0:8]}-{h[8:12]}-{h[12:16]}-{h[16:20]}-{h[20:32]}"


def user_agent(version: str) -> str:
    return (f"codex-tui/{version} (Debian 12.0.0; x86_64) "
            f"xterm-256color (codex-tui; {version})")


def zstd_dec(data: bytes) -> bytes:
    return subprocess.run(["zstd", "-d", "-q", "-c"], input=data,
                          capture_output=True, check=True).stdout


def zstd_enc(data: bytes) -> bytes:
    return subprocess.run(["zstd", "-q", "-c"], input=data,
                          capture_output=True, check=True).stdout


def latest_template() -> str:
    cands = glob.glob(f"{REC}/*/*.req.body")
    cands.sort(key=os.path.getmtime, reverse=True)
    if not cands:
        raise SystemExit("no captured req.body found under " + REC)
    return cands[0]


def turn_metadata(sid: str, turn_id: str) -> str:
    return json.dumps({
        "installation_id": INSTALL_ID,
        "session_id": sid,
        "thread_id": sid,
        "agent_name": "/root",
        "turn_id": turn_id,
        "window_id": f"{sid}:0",
        "window_number": 0,
        "request_kind": "turn",
        "root_turn_id": turn_id,
        "thread_source": "user",
        "sandbox": "seccomp",
        "sandbox_mode": "workspace-write",
        "turn_started_at_unix_ms": int(time.time() * 1000),
    }, separators=(",", ":"))


def fresh_body(template: str, sid: str, turn_id: str) -> bytes:
    """Replay the captured body with regenerated identity fields."""
    raw = open(template, "rb").read()
    plain = zstd_dec(raw) if raw[:4] == ZSTD_MAGIC else raw
    j = json.loads(plain)
    j["prompt_cache_key"] = sid
    cm = j.get("client_metadata")
    if isinstance(cm, dict):
        for k in list(cm):
            kl = k.lower()
            if kl in ("session_id", "thread_id"):
                cm[k] = sid
            elif kl in ("turn_id", "root_turn_id"):
                cm[k] = turn_id
            elif kl in ("window_id", "x-codex-window-id"):
                cm[k] = f"{sid}:0"
            elif kl == "x-codex-turn-metadata":
                try:
                    inner = json.loads(cm[k])
                    inner.update({"session_id": sid, "thread_id": sid,
                                  "turn_id": turn_id, "root_turn_id": turn_id,
                                  "window_id": f"{sid}:0"})
                    for ik in ("x-codex-turn-metadata",):
                        pass
                    cm[k] = json.dumps(inner, separators=(",", ":"))
                except Exception:
                    cm[k] = turn_metadata(sid, turn_id)
    return zstd_enc(json.dumps(j, separators=(",", ":")).encode())


def build_request(access: str, acct: str, body: bytes, sid: str, turn_id: str,
                  meta: str, version: str, inject: str | None,
                  model_hint: str, drop: tuple = ()) -> bytes:
    h = [
        ("Host", HOST),
        ("Content-Type", "application/json"),
        ("Content-Encoding", "zstd"),
        ("Accept", "text/event-stream"),
        ("Authorization", "Bearer " + access),
        ("chatgpt-account-id", acct),
        ("originator", "codex-tui"),
        ("user-agent", user_agent(version)),
        ("session-id", sid),
        ("thread-id", sid),
        ("x-client-request-id", sid),
        ("x-codex-window-id", f"{sid}:0"),
        ("x-codex-beta-features", "remote_compaction_v2"),
        ("x-codex-turn-metadata", meta),
        ("x-codex-routing-hint", f"model={model_hint}"),
        ("x-openai-internal-codex-responses-lite", "true"),
    ]
    if inject:
        h.append(("x-codex-turn-state", inject))
    d = {x.lower() for x in drop}
    h = [(k, v) for k, v in h if k.lower() not in d]
    h += [("Content-Length", str(len(body))), ("Connection", "close")]
    head = f"POST {ROUTE} HTTP/1.1\r\n" + "".join(f"{k}: {v}\r\n" for k, v in h) + "\r\n"
    return head.encode() + body


def parse_head(block: bytes) -> tuple[int, dict]:
    lines = block.decode("iso-8859-1", "replace").split("\r\n")
    status = 0
    m = re.match(r"HTTP/[\d.]+ (\d+)", lines[0] if lines else "")
    if m:
        status = int(m.group(1))
    hdrs: dict = {}
    for line in lines[1:]:
        if ":" in line:
            k, v = line.split(":", 1)
            hdrs.setdefault(k.strip().lower(), v.strip())
    return status, hdrs


def probe(req: bytes, timeout: float, read_body: bool = False,
          max_body: int = 4 << 20) -> tuple[int, dict, bytes]:
    """Send, read the header block, then abort (unless read_body)."""
    ctx = ssl.create_default_context()
    with socket.create_connection((HOST, PORT), timeout=timeout) as s:
        with ctx.wrap_socket(s, server_hostname=HOST) as ss:
            ss.sendall(req)
            buf = b""
            while b"\r\n\r\n" not in buf:
                chunk = ss.recv(65536)
                if not chunk:
                    break
                buf += chunk
            head, _, tail = buf.partition(b"\r\n\r\n")
            status, hdrs = parse_head(head)
            if not read_body:
                return status, hdrs, b""          # <-- abort here
            body = bytearray(tail)
            try:
                while len(body) < max_body:
                    chunk = ss.recv(65536)
                    if not chunk:
                        break
                    body += chunk
            except Exception:
                pass
            return status, hdrs, bytes(body)


def origin_of(hdrs: dict) -> str:
    for k in ("set-cookie", "x-set-cookie"):
        pass
    m = re.search(r"__oailb=([A-Za-z0-9._-]+)", json.dumps(hdrs))
    if not m:
        return "-"
    p = m.group(1).split(".")
    if len(p) < 2:
        return "-"
    try:
        payload = json.loads(base64.urlsafe_b64decode(p[1] + "=" * (-len(p[1]) % 4)))
        return payload.get("host", "-").replace("chat.gateway.", "").replace(".api.openai.com", "")
    except Exception:
        return "-"


def token_info(tok: str) -> dict:
    raw = base64.urlsafe_b64decode(tok + "=" * (-len(tok) % 4))
    ts = int.from_bytes(raw[1:9], "big") if len(raw) >= 9 else 0
    return {
        "b64_len": len(tok),
        "raw_len": len(raw),
        "version": f"0x{raw[0]:02x}" if raw else "?",
        "issued_at": ts,
        "issued_utc": (datetime.fromtimestamp(ts, timezone.utc).strftime("%F %T")
                       if ts else "-"),
        "age_s": int(time.time() - ts) if ts else -1,
    }


def sse_summary(raw: bytes) -> dict:
    txt = raw.decode("utf-8", "replace")
    parts, comp = [], {}
    for line in txt.splitlines():
        line = line.strip()
        if not line.startswith("data:"):
            continue
        try:
            o = json.loads(line[5:].strip())
        except Exception:
            continue
        t = o.get("type", "")
        if t == "response.output_text.delta":
            parts.append(o.get("delta", ""))
        elif t == "response.completed":
            r = o.get("response", {}) or {}
            u = r.get("usage") or {}
            comp = {
                "model": r.get("model"),
                "tier": r.get("service_tier"),
                "status": r.get("status"),
                "in": u.get("input_tokens"),
                "out": u.get("output_tokens"),
                "reasoning": (u.get("output_tokens_details") or {}).get("reasoning_tokens"),
            }
    return {"answer": "".join(parts), **comp}


# --------------------------------------------------------------------------- cmds
def cmd_info(args):
    tok = args.token
    if os.path.exists(tok):
        d = json.load(open(tok))
        tok = d.get("token", "")
        log("stored:", json.dumps({k: v for k, v in d.items() if k != "token"},
                                  ensure_ascii=False))
    i = token_info(tok)
    log(f"  b64_len={i['b64_len']}  raw={i['raw_len']}B  version={i['version']}")
    log(f"  issued={i['issued_utc']} UTC   age={i['age_s']}s")


def cmd_token(args):
    """Print the raw token only -- meant for $(...) substitution."""
    tok = args.src
    if os.path.exists(tok):
        tok = json.load(open(tok))["token"]
    sys.stdout.write(tok)
    return 0


def cmd_grab(args):
    if args.template == "latest":
        args.template = latest_template()
    log(f"# template : {args.template}")
    access, acct = load_auth(args.auth)
    log(f"# account  : {acct}")
    log(f"# want     : len={args.want}   attempts={args.attempts}  gap={args.gap}s")
    log(f"# mode     : read headers then ABORT")
    log("")
    found = None
    for i in range(1, args.attempts + 1):
        sid = args.sid or uuid7()
        turn = uuid7()
        meta = turn_metadata(sid, turn)
        body = fresh_body(args.template, sid, turn)
        req = build_request(access, acct, body, sid, turn, meta,
                            args.version, args.inject, args.model, tuple(args.drop))
        try:
            st, h, _ = probe(req, args.timeout)
        except Exception as e:
            log(f"[{now()} {i:02}] ERROR {type(e).__name__}: {e}")
            time.sleep(args.gap)
            continue
        ts = h.get("x-codex-turn-state", "")
        origin = origin_of(h)
        used = h.get("x-codex-primary-used-percent", "?")
        log(f"[{now()} {i:02}] HTTP {st}  turn-state={len(ts):3}  "
            f"origin={origin:22} used%={used}  sb={h.get('x-codex-safety-buffering-enabled','-')}")
        rec = {
            "token": ts, "len": len(ts), "grabbed_at": int(time.time()),
            "origin": origin, "cf_ray": h.get("cf-ray"),
            "primary_used_percent": used, "attempt": i,
            "template": args.template, "sid": sid, "turn": turn,
        }
        if ts:
            rec.update({k: v for k, v in token_info(ts).items() if k != "token"})
        if ts and (args.want == "any" or len(ts) == int(args.want)):
            found = rec
            break
        time.sleep(args.gap)

    if not found:
        log("\n# no token of the wanted length in this window")
        return 2
    json.dump(found, open(args.out, "w"), indent=2)
    log("")
    log(f"# GOT len={found['len']}  issued={found.get('issued_utc')}  origin={found['origin']}")
    log(f"# saved -> {args.out}")
    log(f"# token  : {found['token'][:60]}...")
    return 0


def cmd_use(args):
    if args.control:
        d = {"token": None}
    elif not args.inject:
        d = json.load(open(args.inject_file))
    else:
        d = {"token": args.inject}
    tok = d["token"]
    if args.control:
        log("# CONTROL: no x-codex-turn-state sent")
    else:
        info = token_info(tok)
        log(f"# injected token: len={info['b64_len']} issued={info['issued_utc']} age={info['age_s']}s")
    if args.template == "latest":
        args.template = latest_template()
    access, acct = load_auth(args.auth)
    sid = args.sid or uuid7()
    turn = uuid7()
    meta = turn_metadata(sid, turn)
    body = fresh_body(args.template, sid, turn)
    req = build_request(access, acct, body, sid, turn, meta,
                        args.version, tok, args.model, tuple(args.drop))
    st, h, raw = probe(req, args.timeout, read_body=args.full)
    got = h.get("x-codex-turn-state", "")
    echo = "NO"
    if got:
        echo = "YES" if got == tok else f"NO (new len={len(got)})"
    log(f"# HTTP {st}   origin={origin_of(h)}   used%={h.get('x-codex-primary-used-percent')}")
    log(f"# upstream echoed our token back?  {echo}")
    log(f"# sb={h.get('x-codex-safety-buffering-enabled','-')} "
        f"faster={h.get('x-codex-safety-buffering-faster-model','-')}")
    if got:
        gi = token_info(got)
        log(f"# returned token: len={gi['b64_len']} issued={gi['issued_utc']} age={gi['age_s']}s")
    if args.full:
        s = sse_summary(raw)
        log(f"# served model={s.get('model')} tier={s.get('tier')} "
            f"tok={s.get('in')}/{s.get('out')} reasoning={s.get('reasoning')}")
        log(f"# answer: {s.get('answer','')[:400]!r}")
    else:
        log("# (aborted after headers -- no generation consumed)")
    return 0


def main():
    p = argparse.ArgumentParser(description=__doc__,
                                formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)

    def common(sp):
        sp.add_argument("--template", default="latest",
                        help="captured req.body to replay ('latest' = newest)")
        sp.add_argument("--auth", default=AUTH)
        sp.add_argument("--model", default="gpt-6-astra")
        sp.add_argument("--version", default="0.145.0")
        sp.add_argument("--timeout", type=float, default=30.0)
        sp.add_argument("--sid", default=None,
                        help="reuse a fixed session id (keeps grab/use in one turn)")
        sp.add_argument("--drop", action="append", default=[],
                        metavar="HEADER", help="omit this request header (repeatable)")

    g = sub.add_parser("grab", help="probe until a fresh token appears")
    common(g)
    g.add_argument("--want", default="292", help="292 | 312 | any")
    g.add_argument("--attempts", type=int, default=30)
    g.add_argument("--gap", type=float, default=4.0)
    g.add_argument("--inject", default=None, help="also send this x-codex-turn-state")
    g.add_argument("--out", default="/root/ts_good.json")
    g.set_defaults(func=cmd_grab)

    u = sub.add_parser("use", help="replay with a stored token injected")
    common(u)
    u.add_argument("--inject-file", default="/root/ts_good.json")
    u.add_argument("--inject", default=None, help="raw token string")
    u.add_argument("--control", action="store_true",
                   help="send NO x-codex-turn-state (baseline)")
    u.add_argument("--full", action="store_true", help="read the whole response")
    u.set_defaults(func=cmd_use)

    i = sub.add_parser("info", help="decode a token or a saved json")
    i.add_argument("token")
    i.set_defaults(func=cmd_info)

    t = sub.add_parser("token", help="print the raw token (for $(...))")
    t.add_argument("src", help="saved json file, or the raw token itself")
    t.set_defaults(func=cmd_token)

    args = p.parse_args()
    sys.exit(args.func(args) or 0)


if __name__ == "__main__":
    main()
