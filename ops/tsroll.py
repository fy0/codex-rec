#!/usr/bin/env python3
"""tsroll -- keep ONE fresh `x-codex-turn-state` (length 292) installed into codex-rec.

The backend issues per-turn routing state in response headers and stops honouring a
token after roughly an hour (measured: ~30 min fine, refused by 62 min). A healthy
token is 292 chars; 312 is what it hands out when the routing state is not what it
wants, and 292s arrive only in bursts -- so a loop keeps watch: whenever the installed
token is older than --refresh-after, hunt for a fresh 292 and install it.

Both ways to get a token go through the `codex-rec` binary, so the whole fingerprint
story (TLS ClientHello, persona headers, auth.json) is exactly the proxy's own:

  scan   `tsgrab --scan-only`  the freshest 292 already sitting in the recorder's
                               logs. Free: no request at all.
  probe  `tsgrab --attempts 1` with a randomized request body (tsgen.py), asking
                               specifically for --want-lengths 292.

Installing is an atomic write to a file codex-rec re-reads on every request
(`set_from_file`), so the proxy never restarts.

State files (same formats the earlier bash version wrote; fpstatus.sh reads them):
  state/current.json   the token in the slot plus probe metadata
  state/events.jsonl   one JSON line per miss/install
  state/roll.log       human-readable log
  state/last.status    rc/http/err/len of the last probe (len = the turn-state length seen)
  state/roll.pid       the loop's pid, so fpstatus can tell alive from dead
"""

import argparse
import base64
import json
import os
import random
import re
import subprocess
import sys
import tempfile
import time
import urllib.request
from pathlib import Path

BIN = "/root/codex-rec/codex-rec"
TPL = Path("/root/tsgrab/templates/probe.req.body")
TPLDIR = Path("/root/tsgrab/templates/probes")
AUTH = "/root/.codex/auth.json"
TOKFILE = Path("/root/codex-rec/ts_token.txt")
TSGEN = "/root/tsgrab/tsgen.py"
STATE = Path("/root/tsgrab/state")
WANT = "292"
PROBE_KEEP = 40
OVERRIDE_ENV = Path("/root/tsgrab/override.env")


def now_iso() -> str:
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())


def log(msg: str) -> None:
    line = f"{{{now_iso()}}} {msg}"
    print(line, flush=True)
    with open(STATE / "roll.log", "a") as f:
        f.write(line + "\n")


def event(**kw) -> None:
    kw.setdefault("at", now_iso())
    with open(STATE / "events.jsonl", "a") as f:
        f.write(json.dumps(kw) + "\n")


def last_status(rc: int, http: str, err: str, ln=None) -> None:
    with open(STATE / "last.status", "w") as f:
        f.write(f"rc={rc}\nhttp={http}\nerr={err}\nlen={ln}\n")


def token_issued(tok: str):
    """Issue time embedded in a token: `0x80` version byte, then u64 big-endian unix time."""
    try:
        raw = base64.urlsafe_b64decode(tok + "=" * (-len(tok) % 4))
        return int.from_bytes(raw[1:9], "big") or None
    except Exception:
        return None


def read_slot() -> str:
    try:
        return TOKFILE.read_text().strip()
    except OSError:
        return ""


def token_age() -> float:
    """Age of the installed token, from its embedded issue time -- not the file mtime.
    The mtime only says when the slot was last written; a scan can install a token that
    was already minutes old, and the use-by clock keeps running regardless.
    No slot at all -> inf, so the very first loop pass goes hunting immediately."""
    issued = token_issued(read_slot())
    if issued:
        return max(0.0, time.time() - issued)
    try:
        return max(0.0, time.time() - TOKFILE.stat().st_mtime)
    except OSError:
        return float("inf")


def run(cmd: list, timeout: int = 90) -> subprocess.CompletedProcess:
    return subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)


def load_override() -> dict:
    cfg = {}
    if not OVERRIDE_ENV.is_file():
        return cfg
    for line in OVERRIDE_ENV.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        k, v = line.split("=", 1)
        cfg[k.strip()] = v.strip()
    return cfg


def push_override(tok: str) -> None:
    """Mirror the token to the remote admin API. Best-effort: the local write is what
    the proxy uses, so a failure here is logged and never rolls the install back."""
    o = load_override()
    if not o.get("OVERRIDE_URL") or not o.get("OVERRIDE_KEY"):
        return
    if not o.get("OVERRIDE_ACCOUNT"):
        log("override: skipped, no OVERRIDE_ACCOUNT configured")
        return
    # A turn-state is base64url with optional `=` padding. Check that assumption instead
    # of trusting it -- the JSON body below is hand-built around it.
    if not re.fullmatch(r"[A-Za-z0-9_=-]+", tok):
        log("override: REFUSED, token contains characters outside base64url")
        return
    body = json.dumps({"accountId": o["OVERRIDE_ACCOUNT"], "turnStateOverride": tok}).encode()
    req = urllib.request.Request(
        o["OVERRIDE_URL"], data=body, method="POST",
        headers={"x-api-key": o["OVERRIDE_KEY"], "Content-Type": "application/json"})
    timeout = int(o.get("OVERRIDE_TIMEOUT", "25"))
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            log(f"override: pushed http={r.status} {r.read()[:120].decode('utf-8', 'replace')}")
    except Exception as e:
        log(f"override: FAILED {e}")


def scan_fresh(within: int) -> str:
    """Freshest usable 292 already in the recorder's logs. Free: no request at all."""
    r = run([BIN, "tsgrab", "--body-file", str(TPL), "--want-lengths", WANT,
             "--scan-only", "--quiet", "--scan-fresh-within", str(within)])
    return r.stdout.strip()


def build_probe_body() -> tuple:
    """One varied probe body via tsgen.py; returns (path, {shape, effort}).

    tsgen rewrites the prompt text and reasoning effort. The header side must be a copy
    of the template's dump under the same base name, because tsgrab looks for a sibling
    `*.req.out.hdr` -- a body without one goes out with no headers at all. Returns
    (None, {}) to mean "use the fixed template".
    """
    src_hdr = TPL.with_name(TPL.name.replace(".req.body", ".req.out.hdr"))
    if not src_hdr.is_file():
        log("vary: no header dump next to the template; using the fixed template")
        return None, {}
    TPLDIR.mkdir(parents=True, exist_ok=True)
    body = TPLDIR / f"p-{time.strftime('%Y%m%d-%H%M%S')}-{random.randint(0, 32768)}.req.body"
    try:
        r = run(["python3", TSGEN, "--template", str(TPL), "--out", str(body), "--show"])
        if r.returncode != 0 or not body.is_file():
            log(f"vary: tsgen failed rc={r.returncode}; using the fixed template")
            body.unlink(missing_ok=True)
            return None, {}
        shape = re.search(r"(?m)^shape\s*:\s*(.+)$", r.stdout)
        effort = re.search(r"(?m)^effort\s*:\s*(\S+)", r.stdout)
        info = {"shape": shape.group(1).strip() if shape else "?",
                "effort": effort.group(1) if effort else "?"}
    except Exception as e:
        log(f"vary: tsgen error {e}; using the fixed template")
        body.unlink(missing_ok=True)
        return None, {}
    hdr = body.with_name(body.name.replace(".req.body", ".req.out.hdr"))
    hdr.write_text(src_hdr.read_text())
    # Keep the directory bounded: the newest PROBE_KEEP bodies survive.
    probes = sorted(TPLDIR.glob("p-*.req.body"), key=lambda p: p.stat().st_mtime)
    for old in probes[:-PROBE_KEEP]:
        old.unlink(missing_ok=True)
        old.with_name(old.name.replace(".req.body", ".req.out.hdr")).unlink(missing_ok=True)
    return body, info


def random_v6(subnet: str) -> str:
    """One random address from the subnet. A fresh source per probe keeps any single
    address from accumulating a history with the backend."""
    import ipaddress
    net = ipaddress.IPv6Network(subnet.strip(), strict=False)
    off = random.getrandbits(128 - net.prefixlen) or 1
    return str(ipaddress.IPv6Address(int(net.network_address) + off))


def probe(body, args) -> tuple:
    """One probe. Returns (token, {rc, http, err, len, src}).

    Deliberately NOT --quiet: the verbose `[NN] HTTP ... turn-state=N` line is the only place
    a missed probe reports the length it actually saw. A miss with len=312 is a different
    problem from len=0 (no header at all), and recording N means a new unexpected length can
    never look identical to a known one. The token is taken only from the `token: ...` line
    tsgrab prints on a hit, so nothing else on stdout can ever be installed by accident.
    """
    out = STATE / "last.json"
    out.unlink(missing_ok=True)
    cmd = [BIN, "tsgrab", "--body-file", str(body), "--auth", AUTH,
           "--want-lengths", WANT, "--attempts", "1", "--timeout", "30",
           "--out", str(out)]
    src = ""
    if args.v6_subnet:
        src = random_v6(args.v6_subnet)
        cmd += ["--source-ip", src]
    r = run(cmd, timeout=60)
    tok = ""
    ln, http, err = None, "", ""
    if m := re.search(r"(?m)^token:\s*(\S+)", r.stdout):
        tok = m.group(1)
    if m := re.search(r"turn-state=\s*(\d+)", r.stdout):
        ln = int(m.group(1))
    if m := re.search(r"(?m)^\[\d+\]\s+HTTP\s+(\d+)", r.stdout):
        http = m.group(1)
    if out.is_file():
        try:
            meta = json.loads(out.read_text())
            http = http or str(meta.get("http_status") or "")
            if ln is None:
                ln = meta.get("len")
        except Exception:
            pass
    if r.stderr.strip():
        err = r.stderr.strip().splitlines()[0][:200]
    return tok, {"rc": r.returncode, "http": http, "err": err, "len": ln, "src": src}


def install(tok: str, source: str, probes: int, replaced_age_min, info: dict) -> None:
    fd, tmp = tempfile.mkstemp(dir=TOKFILE.parent, prefix=".ts_token.")
    with os.fdopen(fd, "w") as f:
        f.write(tok)
    os.chmod(tmp, 0o644)
    os.replace(tmp, TOKFILE)  # atomic: the proxy may read the file at any instant

    rag = round(replaced_age_min) if replaced_age_min is not None else None
    meta = {}
    try:
        meta = json.loads((STATE / "last.json").read_text())
    except Exception:
        pass
    meta.update(token=tok, len=len(tok), installed_at=int(time.time()),
                installed_at_human=now_iso(), source=source, probes=probes,
                replaced_age_min=rag)
    (STATE / "current.json").write_text(json.dumps(meta, indent=2) + "\n")
    event(event="install", source=source, len=len(tok), probes=probes,
          replaced_age_min=rag)
    rag_s = f"{rag}min" if rag is not None else "new"
    log(f"install len={len(tok)} source={source} probe={probes} "
        f"replaced_age={rag_s} shape={info.get('shape', '-')} "
        f"effort={info.get('effort', '-')}")
    push_override(tok)


def hunt(args) -> bool:
    """One hunt cycle: scan first, then probe up to --budget times. True when installed."""
    age = token_age()
    age_min = age / 60 if age != float("inf") else None
    cur_issued = token_issued(read_slot())
    tok = scan_fresh(args.scan_fresh_within)
    # Strictly-newer gate: a scan that keeps returning the token already in the slot must
    # not re-install it -- the slot never gets younger and the loop looks busy while doing
    # nothing. Fall through to probing instead.
    if tok and (issued := token_issued(tok)) and cur_issued and issued <= cur_issued:
        log(f"scan: freshest on disk ({age_min:.0f}min) is not newer than the slot; probing")
        tok = ""
    if tok:
        install(tok, "scan", 0, age_min, {})
        return True
    for n in range(1, args.budget + 1):
        body, info = build_probe_body()
        tok, status = probe(body or TPL, args)
        last_status(status["rc"], status["http"], status["err"], status["len"])
        if tok:
            install(tok, "probe", n, age_min, info)
            return True
        event(event="miss", probes=n, slot_age_min=round(age_min) if age_min is not None else None,
              rc=status["rc"], http=status["http"], err=status["err"], src=status["src"],
              len=status["len"], **{k: v for k, v in info.items()})
        slot = f"{age_min:.0f}min" if age_min is not None else "new"
        got = status["len"] if status["len"] is not None else "?"
        log(f"miss {n}/{args.budget} rc={status['rc']} http={status['http'] or '-'} "
            f"got_len={got} err={status['err'] or '-'} slot_age={slot} "
            f"shape={info.get('shape', '-')} src={status['src'] or '-'}")
        if n < args.budget:
            time.sleep(args.retry + random.randint(0, args.retry_jitter))
    return False


def run_loop(args) -> None:
    (STATE / "roll.pid").write_text(str(os.getpid()))
    log(f"loop started pid={os.getpid()} refresh-after={args.refresh_after}m "
        f"retry={args.retry}+0..{args.retry_jitter}s budget={args.budget} idle={args.idle}s")
    while True:
        age = token_age()
        if age >= args.refresh_after * 60:
            try:
                hunt(args)
            except Exception as e:
                log(f"hunt error: {e!r}")
                event(event="error", err=repr(e)[:200])
            time.sleep(args.retry + random.randint(0, args.retry_jitter))
        else:
            time.sleep(min(args.idle, args.refresh_after * 60 - age))


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--refresh-after", type=int, default=35, help="hunt when the token is older, minutes")
    p.add_argument("--retry", type=int, default=30, help="gap between probes, seconds")
    p.add_argument("--retry-jitter", type=int, default=5, help="extra random 0..N seconds per gap")
    p.add_argument("--budget", type=int, default=40, help="probes per hunt")
    p.add_argument("--idle", type=int, default=1800, help="max sleep while the slot is fresh, seconds")
    p.add_argument("--scan-fresh-within", type=int, default=1800, help="scan window for reusable tokens, seconds")
    p.add_argument("--v6-subnet", default=os.environ.get("TSROLL_V6_SUBNET", ""),
                   help="pick a random source address from this subnet for every probe; "
                        "the prefix must be local (ip_nonlocal_bind or a local route)")
    p.add_argument("--once", action="store_true", help="run one hunt cycle and exit")
    args = p.parse_args()
    STATE.mkdir(parents=True, exist_ok=True)
    if args.once:
        (STATE / "roll.pid").write_text(str(os.getpid()))
        ok = hunt(args)
        sys.exit(0 if ok else 1)
    run_loop(args)


if __name__ == "__main__":
    main()
