#!/usr/bin/env bash
# tsroll -- keep ONE fresh turn-state injected, and start hunting again after a fixed age.
#
# The schedule you asked for:
#
#   * probe until one hit -> write it into the proxy's token file -> the running proxy picks it up
#     on the very next request (no restart, via `set_from_file`);
#   * sleep, then at `--refresh-after` minutes start hunting for the NEXT one;
#   * if the hunt misses, keep retrying every `--retry` seconds until it hits, so the slot is never
#     left empty longer than necessary.
#
#   tsroll.sh start [--refresh-after 35] [--retry 120] [--budget 40]
#   tsroll.sh stop
#   tsroll.sh stat
#   tsroll.sh log [n]
#
# --refresh-after  minutes of token age at which we start looking for a replacement (default 35)
# --retry         seconds between probes while hunting (default 120)
# --budget        max probes per hunt before backing off to `--idle` (default 40)
# --idle          seconds to wait after an exhausted budget (default 1800)
#
# Every probe reads the response head and aborts, so no generation is billed. The token file is
# written atomically, so a request in flight never sees a half-written value.
set -u

DIR=${DIR:-/root/tsgrab/state}
BIN=${BIN:-/root/codex-rec/codex-rec}
TPL=${TPL:-/root/tsgrab/templates/probe.req.body}
AUTH=${AUTH:-/root/.codex/auth.json}
TOKFILE=${TOKFILE:-/root/codex-rec/ts_token.txt}
META=${META:-/root/tsgrab/state/current.json}
WANT=${WANT:-292}
SCAN_FRESH=${SCAN_FRESH:-1800}

# Optional: mirror every install to a remote admin API, exactly like the local write.
# Disabled unless BOTH are set -- the key is a secret and must never be a default in a committed
# file. Put them in the environment, a root-only file, or the systemd unit's EnvironmentFile.
OVERRIDE_URL=${OVERRIDE_URL:-}
OVERRIDE_KEY=${OVERRIDE_KEY:-}
OVERRIDE_ACCOUNT=${OVERRIDE_ACCOUNT:-}
OVERRIDE_TIMEOUT=${OVERRIDE_TIMEOUT:-25}
# Where the key and account id live when they are not already in the environment.
OVERRIDE_ENV_FILE=${OVERRIDE_ENV_FILE:-/root/tsgrab/override.env}

# Load the secret from a root-only file if the environment does not already have it.
if [ -z "$OVERRIDE_KEY" ] && [ -r "$OVERRIDE_ENV_FILE" ]; then
  # shellcheck disable=SC1090
  . "$OVERRIDE_ENV_FILE"
fi

REFRESH_AFTER=35
RETRY=120
BUDGET=40
IDLE=1800

LOG=$DIR/roll.log
EVENTS=$DIR/events.jsonl
PIDF=$DIR/roll.pid
mkdir -p "$DIR"

log() { echo "{$(date -u +%FT%TZ)} $*" >> "$LOG"; }

# Age in minutes of the token currently in $TOKFILE (9999 when missing/unreadable).
current_age_min() {
  python3 - "$TOKFILE" <<'PY'
import base64, os, sys, time
p = sys.argv[1]
if not os.path.exists(p):
    print(9999); raise SystemExit
tok = open(p).read().strip()
try:
    raw = base64.urlsafe_b64decode(tok + "=" * (-len(tok) % 4))
    issued = int.from_bytes(raw[1:9], "big")
    print(int((time.time() - issued) / 60))
except Exception:
    print(9999)
PY
}

# Freshest usable token already sitting in the recorder's logs. Free (no request at all).
scan_history() {
  "$BIN" tsgrab --body-file "$TPL" --auth "$AUTH" \
        --want-lengths "$WANT" --scan-only --quiet \
        --scan-fresh-within "$SCAN_FRESH" 2>/dev/null
}

# Issue time (unix seconds) stamped into a token; 0 when it is missing or unreadable.
token_issued() {
  python3 - "$1" <<'PY'
import base64, sys
raw = sys.argv[1].strip()
if not raw:
    print(0); raise SystemExit
try:
    dec = base64.urlsafe_b64decode(raw + "=" * (-len(raw) % 4))
    print(int.from_bytes(dec[1:9], "big"))
except Exception:
    print(0)
PY
}

# Issue time of what is installed right now.
current_issued() {
  [ -f "$TOKFILE" ] || { echo 0; return; }
  token_issued "$(cat "$TOKFILE" 2>/dev/null)"
}

# Would installing $1 be a step forward? Returns 0 (yes) when it is strictly newer than the slot.
# Without this, a scan that keeps returning the same old token re-installs it forever: the slot
# never gets younger, so the age check at the top of hunt() fires again on the next cycle.
is_improvement() {
  local new old
  new=$(token_issued "$1")
  old=$(current_issued)
  if [ "$new" -eq 0 ]; then
    log "reject: candidate has no readable issue time"
    return 1
  fi
  if [ "$new" -le "$old" ]; then
    log "reject: candidate issued $(( old - new ))s older than (or equal to) the installed one"
    return 1
  fi
  return 0
}

# One probe. Prints the token on stdout when it matched.
probe() {
  local tok
  tok=$("$BIN" tsgrab --body-file "$TPL" --auth "$AUTH" \
        --want-lengths "$WANT" --attempts 1 --timeout 30 --quiet \
        --out "$DIR/last.json" 2>>"$LOG")
  printf '%s' "${tok:-}"
}

# Atomic write: the proxy may read this file at any instant.
# Mirror a token to the remote override API. Best-effort by design: the local install is what the
# proxy actually uses, so a failure here is logged and never rolls it back or aborts the rotation.
push_override() {
  local tok=$1 resp code
  if [ -z "$OVERRIDE_URL" ] || [ -z "$OVERRIDE_KEY" ]; then
    return 0   # not configured: silently off is the documented default
  fi
  if [ -z "$OVERRIDE_ACCOUNT" ]; then
    log "override: skipped, no OVERRIDE_ACCOUNT configured"
    return 1
  fi
  # A turn-state is base64url, optionally with `=` padding: no quotes, backslashes, newlines or
  # control characters. That is what makes a hand-built JSON document safe here -- so verify the
  # assumption instead of trusting it.
  case "$tok" in
    *[!A-Za-z0-9_=-]*)
      log "override: REFUSED, token contains characters outside base64url"
      return 1
      ;;
  esac
  resp=$(curl -sS --max-time "$OVERRIDE_TIMEOUT" -w '\n%{http_code}' \
    -X POST "$OVERRIDE_URL" \
    -H "x-api-key: $OVERRIDE_KEY" \
    -H "Content-Type: application/json" \
    -d "{\"accountId\":\"$OVERRIDE_ACCOUNT\",\"turnStateOverride\":\"$tok\"}" 2>&1)
  code=$(printf '%s' "$resp" | tail -n1)
  resp=$(printf '%s' "$resp" | sed '$d' | tr -d '\n')
  if [ "$code" = "200" ]; then
    log "override: pushed http=200 $resp"
    return 0
  fi
  log "override: FAILED http=$code $resp"
  return 1
}

install_token() {
  local tok=$1 tmp
  tmp=$(mktemp "$TOKFILE.XXXXXX")
  printf '%s' "$tok" > "$tmp"
  chmod 644 "$tmp"
  mv -f "$tmp" "$TOKFILE"
  python3 - "$META" "$tok" "$DIR/last.json" <<'PY'
import json, os, sys, time
meta_path, tok, last = sys.argv[1], sys.argv[2], sys.argv[3]
meta = {}
if os.path.exists(last):
    try: meta = json.load(open(last))
    except Exception: meta = {}
meta["token"] = tok
meta["len"] = len(tok)
meta["installed_at"] = int(time.time())
try:
    meta["installed_at_human"] = time.strftime("%FT%TZ", time.gmtime(meta["installed_at"]))
except Exception:
    pass
json.dump(meta, open(meta_path, "w"), indent=2)
PY
  # Mirror to the remote API right after the local write, so "installed" means "locally active"
  # first and "pushed" second.
  push_override "$tok"
}

hunt() {
  local n=0 tok age
  while [ "$n" -lt "$BUDGET" ]; do
    n=$((n + 1))
    age=$(current_age_min)
    if [ "$age" -lt "$REFRESH_AFTER" ]; then
      log "slot already fresh (${age}m); stopping this hunt"
      return 0
    fi
    tok=$(scan_history)
    if [ -n "$tok" ] && is_improvement "$tok"; then
      install_token "$tok"
      log "INSTALLED len=${#tok} from HISTORY (slot was ${age}m old) at round ${n}"
      printf '{"at":"%s","event":"install","source":"history","len":%s,"probes":%s,"replaced_age_min":%s}\n' \
        "$(date -u +%FT%TZ)" "${#tok}" "$n" "$age" >> "$EVENTS"
      return 0
    fi
    tok=$(probe)
    if [ -n "$tok" ] && is_improvement "$tok"; then
      install_token "$tok"
      log "INSTALLED len=${#tok} from PROBE (slot was ${age}m old) after ${n} probe(s)"
      printf '{"at":"%s","event":"install","source":"probe","len":%s,"probes":%s,"replaced_age_min":%s}\n' \
        "$(date -u +%FT%TZ)" "${#tok}" "$n" "$age" >> "$EVENTS"
      return 0
    fi
    log "miss ${n}/${BUDGET} (slot ${age}m old)"
    printf '{"at":"%s","event":"miss","probes":%s,"slot_age_min":%s}\n' \
      "$(date -u +%FT%TZ)" "$n" "$age" >> "$EVENTS"
    sleep "$RETRY"
  done
  log "budget exhausted (${BUDGET} probes); idling ${IDLE}s"
  printf '{"at":"%s","event":"budget_exhausted","probes":%s}\n' \
    "$(date -u +%FT%TZ)" "$BUDGET" >> "$EVENTS"
  sleep "$IDLE"
  return 1
}

write_lib() {
  { sed -n '/^log()/,/^}/p' "$0"
    sed -n '/^current_age_min()/,/^}/p' "$0"
    sed -n '/^scan_history()/,/^}/p' "$0"
    sed -n '/^token_issued()/,/^}/p' "$0"
    sed -n '/^current_issued()/,/^}/p' "$0"
    sed -n '/^is_improvement()/,/^}/p' "$0"
    sed -n '/^push_override()/,/^}/p' "$0"
    sed -n '/^probe()/,/^}/p' "$0"
    sed -n '/^install_token()/,/^}/p' "$0"
    sed -n '/^hunt()/,/^}/p' "$0"
  } > "$DIR/lib.sh"
  cat >> "$DIR/lib.sh" <<EOF
DIR="$DIR"; BIN="$BIN"; TPL="$TPL"; AUTH="$AUTH"; TOKFILE="$TOKFILE"
META="$META"; WANT="$WANT"; LOG="$LOG"; EVENTS="$EVENTS"
REFRESH_AFTER=$REFRESH_AFTER; RETRY=$RETRY; BUDGET=$BUDGET; IDLE=$IDLE
SCAN_FRESH=$SCAN_FRESH
OVERRIDE_URL="$OVERRIDE_URL"; OVERRIDE_KEY="$OVERRIDE_KEY"
OVERRIDE_ACCOUNT="$OVERRIDE_ACCOUNT"; OVERRIDE_TIMEOUT=$OVERRIDE_TIMEOUT
OVERRIDE_ENV_FILE="$OVERRIDE_ENV_FILE"
EOF
  cat > "$DIR/loop.sh" <<EOF
#!/usr/bin/env bash
set -u
source "$DIR/lib.sh"
while true; do
  age=\$(current_age_min)
  if [ "\$age" -ge "$REFRESH_AFTER" ]; then
    log "slot is \${age}m old (>= ${REFRESH_AFTER}m): hunting"
    hunt
  fi
  sleep 30
done
EOF
  chmod +x "$DIR/loop.sh"
}

case "${1:-}" in
  start)
    shift || true
    while [ $# -gt 0 ]; do
      case "$1" in
        --refresh-after) REFRESH_AFTER=$2; shift 2 ;;
        --retry) RETRY=$2; shift 2 ;;
        --budget) BUDGET=$2; shift 2 ;;
        --idle) IDLE=$2; shift 2 ;;
        *) shift ;;
      esac
    done
    if [ -f "$PIDF" ] && kill -0 "$(cat "$PIDF")" 2>/dev/null; then
      echo "already running (pid $(cat "$PIDF"))"; exit 0
    fi
    pkill -f "$DIR/loop.sh" 2>/dev/null
    sleep 0.3
    write_lib
    echo "refresh-after=${REFRESH_AFTER}m  retry=${RETRY}s  budget=${BUDGET}  idle=${IDLE}s  scan-fresh=${SCAN_FRESH}s"
    echo "token file : $TOKFILE"
    echo "log        : $LOG"
    setsid nohup "$DIR/loop.sh" >> "$LOG" 2>&1 &
    echo $! > "$PIDF"
    sleep 2
    echo "pid $(cat "$PIDF")"
    ;;
  stop)
    [ -f "$PIDF" ] && kill "$(cat "$PIDF")" 2>/dev/null
    pkill -f "$DIR/loop.sh" 2>/dev/null
    rm -f "$PIDF"
    echo stopped
    ;;
  stat)
    python3 - "$EVENTS" "$TOKFILE" <<'PY'
import base64, collections, json, os, sys, time
events_path, tokfile = sys.argv[1], sys.argv[2]
print("=== injected token ===")
if os.path.exists(tokfile):
    tok = open(tokfile).read().strip()
    try:
        raw = base64.urlsafe_b64decode(tok + "=" * (-len(tok) % 4))
        issued = int.from_bytes(raw[1:9], "big")
        age = int((time.time() - issued) / 60)
        print(f"  len={len(tok)}  issued={time.strftime('%H:%M:%S', time.gmtime(issued))}Z  age={age} min")
        print(f"  remaining before next hunt: {max(0, 35 - age)} min")
    except Exception as e:
        print("  cannot decode:", e)
else:
    print("  (no token file yet)")
print()
print("=== events ===")
rows = []
if os.path.exists(events_path):
    for line in open(events_path):
        line = line.strip()
        if line.startswith("{"):
            try: rows.append(json.loads(line))
            except Exception: pass
if not rows:
    print("  none yet")
else:
    inst = [r for r in rows if r.get("event") == "install"]
    miss = [r for r in rows if r.get("event") == "miss"]
    print(f"  installs : {len(inst)}")
    print(f"  misses   : {len(miss)}")
    probes = sum(r.get("probes", 0) for r in inst) + len(miss)
    if probes:
        print(f"  probes   : {probes}   hit rate {100.0*len(inst)/max(1, len(inst)+len(miss)):.0f}% per hunt-round")
    for r in rows[-12:]:
        print(f"    {r['at'][11:19]}  {r.get('event'):18} probes={r.get('probes')} "
              f"replaced_age={r.get('replaced_age_min')}m")
PY
    ;;
  log) tail -n "${2:-25}" "$LOG" ;;
  probe-now)  # one shot, for cron/manual use
    tok=$(BIN="$BIN" TPL="$TPL" AUTH="$AUTH" WANT="$WANT" DIR="$DIR" bash -c '
      source '"$DIR"'/lib.sh 2>/dev/null || exit 1
      probe')
    if [ -n "${tok:-}" ]; then
      echo "$tok"
    else
      echo "(miss)" >&2
      exit 1
    fi
    ;;
  *) sed -n '2,22p' "$0" ;;
esac
