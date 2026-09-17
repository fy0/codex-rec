#!/usr/bin/env bash
# tsauto -- keep a fresh `x-codex-turn-state` injected into the main proxy, by polling cheaply.
#
# Idea: if the backend hands out a 292 often enough, there is no need for an IP pool -- just sample
# it on a timer and swap the value in when the injected one goes stale.
#
#   tsauto.sh start [--interval 600] [--refresh-after 20] [--no-restart]
#   tsauto.sh stop
#   tsauto.sh stat          hit rate + injection age
#   tsauto.sh log [n]
#
# --interval        seconds between probes (default 600 = 10 min, ~144 requests/day)
# --refresh-after   re-inject when the currently injected token is older than this many minutes
# --no-restart      update /root/ts_inject.json only; do not touch the proxy
#
# A probe reads the response head and aborts, so the generation is never billed; the proxy restart is
# a sub-second gap and only happens when the injected token is actually stale.
set -u

DIR=${DIR:-/root/tsprobe}
BIN=${BIN:-/root/codex-rec-71}
TPL=${TPL:-/root/rec/01a0af61-7e0a-7083-bded-c035c5858e91/260917-124033-user-r0002.req.body}
AUTH=${AUTH:-/root/.codex/auth.json}
CFG=${CFG:-/root/codex-rec.toml}
INJECT=${INJECT:-/root/ts_inject.json}
PORT=${PORT:-18080}
WANT=${WANT:-292}

INTERVAL=600
REFRESH_AFTER=20
DO_RESTART=1

LOG=$DIR/auto.log
HITS=$DIR/hits.jsonl
PIDF=$DIR/auto.pid
mkdir -p "$DIR"

log() { echo "{$(date -u +%FT%TZ)} $*" >> "$LOG"; }

# minutes since the token currently written into the proxy config was issued
injected_age_min() {
  python3 - "$INJECT" <<'PY'
import base64, json, os, sys, time
p = sys.argv[1]
if not os.path.exists(p):
    print(9999); raise SystemExit
try:
    tok = json.load(open(p))["token"]
    raw = base64.urlsafe_b64decode(tok + "=" * (-len(tok) % 4))
    issued = int.from_bytes(raw[1:9], "big")
    print(int((time.time() - issued) / 60))
except Exception:
    print(9999)
PY
}

reload_proxy() {
  local tok=$1
  python3 - "$CFG" "$tok" <<'PY'
import re, sys
cfg, tok = sys.argv[1], sys.argv[2]
s = open(cfg).read()
# replace the existing injection line, keeping its position
new, n = re.subn(r'"x-codex-turn-state"\s*=\s*"[^"]*"',
                 '"x-codex-turn-state" = "%s"' % tok, s)
if n == 0:
    # no line yet -> add one under [headers.request]
    new = s.replace("[headers.request]\n",
                    '[headers.request]\nset = { "x-codex-turn-state" = "%s" }\n' % tok, 1)
open(cfg, "w").write(new)
print("config injection lines:", n if n else 1)
PY
  local pid
  pid=$(ss -lntpH 2>/dev/null | grep ":${PORT} " | grep -oE 'pid=[0-9]+' | head -1 | cut -d= -f2)
  if [ -n "$pid" ]; then
    kill "$pid" 2>/dev/null
    sleep 1
  fi
  nohup "$BIN" --config "$CFG" >> "$DIR/proxy.log" 2>&1 &
  sleep 2
  if ss -lntpH 2>/dev/null | grep -q ":${PORT} "; then
    log "proxy $PORT restarted with fresh token"
    return 0
  fi
  log "ERROR: proxy $PORT did not come back up"
  return 1
}

sample() {
  local stamp tok rc age
  stamp=$(date -u +%FT%TZ)
  tok=$("$BIN" tsgrab --body-file "$TPL" --auth "$AUTH" \
        --want-lengths "$WANT" --attempts 1 --timeout 30 --quiet \
        --out "$DIR/last.json" 2>>"$LOG")
  rc=$?
  age=$(injected_age_min)

  if [ -n "${tok:-}" ]; then
    printf '{"at":"%s","hit":true,"len":%s,"injected_age_min":%s,"token":"%s"}\n' \
      "$stamp" "${#tok}" "$age" "$tok" >> "$HITS"
    python3 - "$INJECT" "$tok" "$DIR/last.json" <<'PY'
import json, sys
inj, tok, last = sys.argv[1], sys.argv[2], sys.argv[3]
meta = {}
try:
    meta = json.load(open(last))
except Exception:
    pass
meta["token"] = tok
meta["len"] = len(tok)
json.dump(meta, open(inj, "w"), indent=2)
PY
    log "HIT len=${#tok} (injected was ${age}m old)"
    if [ "$DO_RESTART" = "1" ] && [ "$age" -ge "$REFRESH_AFTER" ]; then
      reload_proxy "$tok"
    fi
  else
    local got=0
    [ -f "$DIR/last.json" ] && got=$(python3 -c "
import json
try: print(json.load(open('$DIR/last.json')).get('len') or 0)
except Exception: print(0)")
    printf '{"at":"%s","hit":false,"len":%s,"injected_age_min":%s}\n' "$stamp" "$got" "$age" >> "$HITS"
    log "MISS (got len=$got rc=$rc, injected ${age}m old)"
  fi
}

case "${1:-}" in
  start)
    shift || true
    while [ $# -gt 0 ]; do
      case "$1" in
        --interval) INTERVAL=$2; shift 2 ;;
        --refresh-after) REFRESH_AFTER=$2; shift 2 ;;
        --no-restart) DO_RESTART=0; shift ;;
        *) shift ;;
      esac
    done
    if [ -f "$PIDF" ] && kill -0 "$(cat "$PIDF")" 2>/dev/null; then
      echo "already running (pid $(cat "$PIDF"))"; exit 0
    fi
    cat > "$DIR/loop.sh" <<EOF
#!/usr/bin/env bash
set -u
source "$DIR/lib.sh"
while true; do sample; sleep $INTERVAL; done
EOF
    chmod +x "$DIR/loop.sh"
    # lib.sh holds the functions so the loop can source them
    { sed -n '/^log()/,/^}/p' "$0"; sed -n '/^injected_age_min()/,/^}/p' "$0";
      sed -n '/^reload_proxy()/,/^}/p' "$0"; sed -n '/^sample()/,/^}/p' "$0"; } > "$DIR/lib.sh"
    cat >> "$DIR/lib.sh" <<EOF
DIR="$DIR"; BIN="$BIN"; TPL="$TPL"; AUTH="$AUTH"; CFG="$CFG"
INJECT="$INJECT"; PORT="$PORT"; WANT="$WANT"
LOG="$LOG"; HITS="$HITS"; DO_RESTART="$DO_RESTART"; REFRESH_AFTER="$REFRESH_AFTER"
EOF
    echo "interval=${INTERVAL}s  refresh-after=${REFRESH_AFTER}min  restart=$DO_RESTART"
    echo "log=$LOG  samples=$HITS"
    setsid nohup "$DIR/loop.sh" >> "$LOG" 2>&1 &
    echo $! > "$PIDF"
    sleep 3
    echo "pid $(cat "$PIDF")"
    ;;
  stop)
    [ -f "$PIDF" ] && { kill "$(cat "$PIDF")" 2>/dev/null; rm -f "$PIDF"; echo stopped; } || echo "not running"
    pkill -f "$DIR/loop.sh" 2>/dev/null
    ;;
  stat)
    python3 - "$HITS" "$INJECT" <<'PY'
import base64, collections, json, os, sys, time
hits, inj = sys.argv[1], sys.argv[2]
rows = []
if os.path.exists(hits):
    for line in open(hits):
        line = line.strip()
        if line.startswith("{"):
            try: rows.append(json.loads(line))
            except Exception: pass
print("=== sampling ===")
if rows:
    h = [r for r in rows if r.get("hit")]
    print(f"samples : {len(rows)}   hits: {len(h)}  ({100.0*len(h)/len(rows):.1f}%)")
    print(f"window  : {rows[0]['at']} -> {rows[-1]['at']}")
    print("length dist:", dict(sorted(collections.Counter(r.get('len') for r in rows).items(),
                                       key=lambda kv: str(kv[0]))))
    # hourly rate
    byh = collections.defaultdict(lambda: [0, 0])
    for r in rows:
        k = r["at"][:13]
        byh[k][1] += 1
        if r.get("hit"): byh[k][0] += 1
    print("per hour:")
    for k in sorted(byh):
        a, b = byh[k]
        print(f"  {k}:00  {a}/{b}  {100.0*a/b:.0f}%")
else:
    print("no samples yet")
print()
print("=== injected token ===")
if os.path.exists(inj):
    d = json.load(open(inj))
    tok = d.get("token", "")
    try:
        raw = base64.urlsafe_b64decode(tok + "=" * (-len(tok) % 4))
        issued = int.from_bytes(raw[1:9], "big")
        print(f"len={len(tok)}  issued={time.strftime('%H:%M:%S', time.gmtime(issued))}Z  "
              f"age={int((time.time()-issued)/60)} min")
        print(f"origin={d.get('origin')}  plan={d.get('plan_type')}")
    except Exception as e:
        print("cannot decode:", e)
else:
    print("no /root/ts_inject.json")
PY
    ;;
  log) tail -n "${2:-25}" "$LOG" ;;
  *) sed -n '2,16p' "$0" ;;
esac
