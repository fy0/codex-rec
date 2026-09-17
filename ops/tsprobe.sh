#!/usr/bin/env bash
# tsprobe -- low-rate background sampler of the Codex backend's turn-state length.
#
# One probe = one request; the response head is read and the connection is aborted, so no
# generation is billed (verified: x-codex-primary-used-percent stays put). The point is to answer
# "how often does the backend hand out a 292, and can we just poll instead of running an IP pool?"
#
#   tsprobe.sh start [--interval 300]   start sampling (default every 5 min)
#   tsprobe.sh stop
#   tsprobe.sh stat                     hit rate so far
#   tsprobe.sh tail [n]
#
# Results append to one JSONL line per attempt in $DIR/hits.jsonl, so the rate is always
# recomputable and nothing depends on this script still running.
set -u

DIR=${DIR:-/root/tsprobe}
BIN=${BIN:-/root/codex-rec-71}
TPL=${TPL:-/root/rec/01a0af61-7e0a-7083-bded-c035c5858e91/260917-124033-user-r0002.req.body}
AUTH=${AUTH:-/root/.codex/auth.json}
INTERVAL=${INTERVAL:-300}
WANT=${WANT:-292}
LOG=$DIR/probe.log
HITS=$DIR/hits.jsonl
PIDF=$DIR/probe.pid

mkdir -p "$DIR"

sample() {
  local stamp tok rc
  stamp=$(date -u +%FT%TZ)
  # --quiet prints only the token, so the shell can act on it; --out keeps the metadata.
  tok=$("$BIN" tsgrab \
        --body-file "$TPL" --auth "$AUTH" \
        --want-lengths "$WANT" --attempts 1 \
        --timeout 30 --quiet \
        --out "$DIR/last.json" 2>>"$LOG")
  rc=$?
  local ts="null"
  if [ -n "${tok:-}" ]; then
    printf '{"at":"%s","hit":true,"len":%s,"token":"%s"}\n' "$stamp" "${#tok}" "$tok" >> "$HITS"
    cp -f "$DIR/last.json" "$DIR/last-hit.json" 2>/dev/null
    echo "{$(date -u +%FT%TZ)} HIT len=${#tok}" >> "$LOG"
  else
    # record what we DID get, so the miss is auditable
    local got="0"
    [ -f "$DIR/last.json" ] && got=$(python3 -c "
import json,sys
try:
    d=json.load(open('$DIR/last.json'))
    print(d.get('len') or 0)
except Exception:
    print(0)
")
    printf '{"at":"%s","hit":false,"len":%s}\n' "$stamp" "$got" >> "$HITS"
    echo "{$(date -u +%FT%TZ)} MISS (got len=$got, rc=$rc)" >> "$LOG"
  fi
}

case "${1:-}" in
  start)
    shift || true
    while [ $# -gt 0 ]; do
      case "$1" in
        --interval) INTERVAL=$2; shift 2 ;;
        *) shift ;;
      esac
    done
    if [ -f "$PIDF" ] && kill -0 "$(cat "$PIDF")" 2>/dev/null; then
      echo "already running (pid $(cat "$PIDF"))"; exit 0
    fi
    echo "starting: every ${INTERVAL}s -> $HITS"
    nohup bash -c '
      DIR="'"$DIR"'"; INTERVAL="'"$INTERVAL"'"
      source /dev/stdin <<< "$(sed -n "/^sample()/,/^}/p" "'"$0"'")"
      BIN="'"$BIN"'"; TPL="'"$TPL"'"; AUTH="'"$AUTH"'"; WANT="'"$WANT"'"
      LOG="'"$LOG"'"; HITS="'"$HITS"'"
      while true; do sample; sleep "$INTERVAL"; done
    ' >> "$LOG" 2>&1 &
    echo $! > "$PIDF"
    sleep 0.5
    echo "pid $(cat "$PIDF")"
    ;;
  stop)
    if [ -f "$PIDF" ]; then
      kill "$(cat "$PIDF")" 2>/dev/null
      rm -f "$PIDF"
      echo "stopped"
    else
      echo "not running"
    fi
    ;;
  stat)
    python3 - "$HITS" <<'PY'
import json, sys, collections, os
p = sys.argv[1]
if not os.path.exists(p):
    print("no samples yet"); raise SystemExit
rows = []
for line in open(p):
    line = line.strip()
    if line.startswith("{"):
        try: rows.append(json.loads(line))
        except Exception: pass
if not rows:
    print("no samples yet"); raise SystemExit
hits = [r for r in rows if r.get("hit")]
print(f"samples : {len(rows)}")
print(f"hits    : {len(hits)}  ({100.0*len(hits)/len(rows):.1f}%)")
print(f"first   : {rows[0]['at']}")
print(f"last    : {rows[-1]['at']}")
c = collections.Counter(r.get("len") for r in rows)
print("length distribution:", dict(sorted(c.items(), key=lambda kv: str(kv[0]))))
if hits:
    print(f"last hit: {hits[-1]['at']}  len={hits[-1]['len']}")
PY
    ;;
  tail)
    n=${2:-20}
    tail -n "$n" "$HITS" 2>/dev/null || echo "(no samples)"
    ;;
  *)
    sed -n '2,14p' "$0"
    ;;
esac
