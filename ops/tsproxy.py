#!/usr/bin/env python3
"""tsproxy -- run codex-rec with ONLY the `x-codex-turn-state` request header replaced.

A thin, explicit wrapper around codex-rec for testing the turn-state mechanism:

  * it copies an existing codex-rec config (persona, environment rewrite, drop globs, ...),
  * overrides just `listen` and `log_dir` so the probe instance does not clobber the main one,
  * injects `x-codex-turn-state = <token>` into `[headers.request].set`,
  * starts the instance and tells you the exact codex client flag to use.

Nothing else about the request is touched. The injected value is visible in the recorder's
`*.req.out.hdr` as `<- set by config`, so every experiment stays auditable.

Usage
  tsproxy.py start --port 18090 --token @/root/ts_new292.json
  tsproxy.py start --port 18091 --control                  # no injection (baseline)
  tsproxy.py start --port 18090 --token @/root/ts.json --base /root/codex-rec.toml
  tsproxy.py stop  --port 18090
  tsproxy.py list
  tsproxy.py show  --token @/root/ts_new292.json

  # make the *main* proxy inject, so a normal `codex` session picks it up with no client change:
  tsproxy.py start --port 18080 --token @/root/ts.json --base /root/codex-rec.toml --restart
"""

from __future__ import annotations

import argparse
import base64
import datetime as dt
import json
import os
import re
import signal
import subprocess
import sys
import time
from pathlib import Path

HDR = "x-codex-turn-state"
BIN = os.environ.get("TS_BIN", "/root/codex-rec-56")
DIR = Path(os.environ.get("TS_DIR", "/root/tsproxy"))
DEFAULT_BASE = "/root/codex-rec.toml"


# --------------------------------------------------------------------------- token
def read_token(src: str) -> str:
    """`@file` reads {"token": ...} or a bare token from a file; otherwise it IS the token."""
    if src.startswith("@"):
        src = src[1:]
    p = Path(src)
    if p.is_file():
        txt = p.read_text().strip()
        if txt.startswith("{"):
            return json.loads(txt)["token"]
        return txt
    return src


def token_info(tok: str) -> dict:
    raw = base64.urlsafe_b64decode(tok + "=" * (-len(tok) % 4))
    ts = int.from_bytes(raw[1:9], "big") if len(raw) >= 9 else 0
    when = dt.datetime.fromtimestamp(ts, dt.timezone.utc) if ts else None
    return {
        "len": len(tok),
        "raw": len(raw),
        "version": f"0x{raw[0]:02x}" if raw else "?",
        "issued": when.strftime("%F %T") if when else "-",
        "age_s": int(time.time() - ts) if ts else -1,
    }


def describe(tok: str) -> str:
    i = token_info(tok)
    return (f"len={i['len']} raw={i['raw']}B ver={i['version']} "
            f"issued={i['issued']}Z age={i['age_s']}s")


# --------------------------------------------------------------------------- config
def patch_config(base: str, port: int, token: str | None) -> Path:
    """Copy `base` to <DIR>/p<port>.toml, overriding listen/log_dir and (optionally) the header."""
    text = Path(base).read_text() if Path(base).is_file() else ""
    rec = DIR / f"rec{port}"
    lines = text.splitlines()

    out: list[str] = []
    seen_listen = seen_logdir = False
    in_req = False
    inserted = False
    for line in lines:
        stripped = line.strip()
        if stripped.startswith("[") and stripped.endswith("]"):
            # leaving the request-header table?
            if in_req and stripped != "[headers.request]":
                if token and not inserted:
                    out.append(f'set = {{ "{HDR}" = "{token}" }}')
                    inserted = True
                in_req = False
            if stripped == "[headers.request]":
                in_req = True
                out.append(line)
                if token:  # inject right after the section header
                    out.append(f'set = {{ "{HDR}" = "{token}" }}')
                    inserted = True
                continue
        if in_req and re.match(rf"^{re.escape(HDR)}\s*=", stripped):
            continue  # drop a previous injection
        if not in_req and re.match(r"^listen\s*=", stripped) and not seen_listen:
            out.append(f'listen          = "127.0.0.1:{port}"')
            seen_listen = True
            continue
        if not in_req and re.match(r"^log_dir\s*=", stripped) and not seen_logdir:
            out.append(f'log_dir         = "{rec}"')
            seen_logdir = True
            continue
        if re.match(r"^session_dir\s*=", stripped):
            out.append(f'session_dir     = "{DIR}/sessions{port}"')
            continue
        out.append(line)

    if in_req and token and not inserted:
        out.append(f'set = {{ "{HDR}" = "{token}" }}')
        inserted = True
    if not seen_listen:
        out.insert(0, f'listen          = "127.0.0.1:{port}"')
    if not seen_logdir:
        out.append(f'log_dir         = "{rec}"')

    DIR.mkdir(parents=True, exist_ok=True)
    rec.mkdir(parents=True, exist_ok=True)
    path = DIR / f"p{port}.toml"
    path.write_text("\n".join(out) + "\n")
    return path


def port_pids(port: int) -> list[int]:
    try:
        out = subprocess.run(["ss", "-lntpH"], capture_output=True, text=True).stdout
    except FileNotFoundError:
        return []
    pids = []
    for line in out.splitlines():
        if f":{port} " not in line:
            continue
        for m in re.finditer(r"pid=(\d+)", line):
            pids.append(int(m.group(1)))
    return pids


def stop(port: int, quiet: bool = False) -> bool:
    pids = port_pids(port)
    for p in pids:
        try:
            os.kill(p, signal.SIGTERM)
        except ProcessLookupError:
            pass
    if pids and not quiet:
        print(f"  stopped pid(s) {pids} on port {port}")
    if pids:
        time.sleep(0.6)
    return bool(pids)


# --------------------------------------------------------------------------- commands
def cmd_start(a) -> int:
    if a.control and a.token:
        print("--control and --token are mutually exclusive")
        return 2
    tok = read_token(a.token) if (a.token and not a.control) else None
    if a.restart:
        stop(a.port, quiet=True)
    elif port_pids(a.port):
        print(f"port {a.port} is already in use (pids {port_pids(a.port)}); "
              f"use --restart or a different --port")
        return 2
    cfg = patch_config(a.base, a.port, tok)
    log = DIR / f"p{a.port}.log"
    with open(log, "ab") as fh:
        fh.write(f"\n=== {dt.datetime.now().isoformat(timespec='seconds')} start "
                 f"(base={a.base})\n".encode())
        proc = subprocess.Popen([BIN, "--config", str(cfg)], stdout=fh, stderr=fh,
                                start_new_session=True)
    time.sleep(2.0)
    if not port_pids(a.port):
        print(f"  FAILED to bind {a.port}; log tail:")
        print("\n".join(log.read_text().splitlines()[-8:]))
        return 1
    print(f"  pid={proc.pid}  listening 127.0.0.1:{a.port}")
    print(f"  config   {cfg}")
    print(f"  record   {DIR}/rec{a.port}")
    print(f"  log      {log}")
    if tok:
        print(f"  INJECT   {HDR} = {describe(tok)}")
        print(f"           {tok[:56]}...")
    else:
        print(f"  INJECT   off (baseline: the client's own value passes through)")
    print()
    print("  use it with the codex client as:")
    print(f"    codex exec -c model_providers.openai-custom.base_url="
          f"'\"http://127.0.0.1:{a.port}/backend-api/codex\"' ...")
    return 0


def cmd_stop(a) -> int:
    if not stop(a.port):
        print(f"  nothing listening on {a.port}")
    return 0


def cmd_list(a) -> int:
    try:
        out = subprocess.run(["ss", "-lntp"], capture_output=True, text=True).stdout
    except FileNotFoundError:
        return 1
    for line in out.splitlines():
        m = re.search(r"(127\.0\.0\.1|0\.0\.0\.0):(\d+)", line)
        if not m or not m.group(2).startswith("18"):
            continue
        cfg = DIR / f"p{m.group(2)}.toml"
        tag = ""
        if cfg.is_file():
            # the injection lives inside `set = { "x-codex-turn-state" = "..." }`
            hit = re.search(rf'"{HDR}"\s*=\s*"([^"]+)"', cfg.read_text())
            if hit:
                i = token_info(hit.group(1))
                tag = f"  INJECTING {i['len']} (issued {i['issued']}Z, age {i['age_s']}s)"
            else:
                tag = "  (baseline)"
        print(f"  {m.group(0):22}{tag}")
    return 0


def cmd_show(a) -> int:
    tok = read_token(a.token)
    print(f"  {describe(tok)}")
    print(f"  {tok}")
    return 0


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__,
                                formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)

    s = sub.add_parser("start", help="start an instance")
    s.add_argument("--port", type=int, required=True)
    s.add_argument("--token", help="token, or @file holding {token} or the bare token")
    s.add_argument("--control", action="store_true", help="do not inject (baseline)")
    s.add_argument("--base", default=DEFAULT_BASE, help="codex-rec config to copy")
    s.add_argument("--restart", action="store_true", help="kill whatever holds the port first")
    s.set_defaults(func=cmd_start)

    t = sub.add_parser("stop", help="stop the instance on a port")
    t.add_argument("--port", type=int, required=True)
    t.set_defaults(func=cmd_stop)

    l = sub.add_parser("list", help="show 18xx proxies and what they inject")
    l.set_defaults(func=cmd_list)

    w = sub.add_parser("show", help="decode a token")
    w.add_argument("--token", required=True)
    w.set_defaults(func=cmd_show)

    a = p.parse_args()
    return a.func(a)


if __name__ == "__main__":
    sys.exit(main())
