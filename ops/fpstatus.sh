#!/usr/bin/env bash
# fpstatus -- one command to see the turn-state / fingerprint situation.
#
#   fpstatus            human summary
#   fpstatus --short    one line, for a prompt or a watch loop
#   fpstatus --json     machine-readable
#   fpstatus --probe    also send one probe to see what the backend gives out right now
#
# Reads only local state (service, token file, scheduler, history) -- except with --probe,
# which costs one request that is aborted after the headers, so it is not billed.
set -u

DIR=${DIR:-/root/tsgrab/state}
TOKFILE=${TOKFILE:-/root/codex-rec/ts_token.txt}
BIN=${BIN:-/root/codex-rec/codex-rec}
TPL=${TPL:-/root/tsgrab/templates/probe.req.body}
AUTH=${AUTH:-/root/.codex/auth.json}
WANT=${WANT:-292}
SERVICE=${SERVICE:-codex-rec}

mode=human
probe=0
for a in "$@"; do
  case "$a" in
    --short) mode=short ;;
    --json)  mode=json ;;
    --probe) probe=1 ;;
  esac
done

exec python3 - "$mode" "$probe" "$DIR" "$TOKFILE" "$BIN" "$TPL" "$AUTH" "$WANT" "$SERVICE" <<'PY'
import base64, json, os, subprocess, sys, time

mode, probe, state_dir, tokfile, binary, tpl, auth, want, service = sys.argv[1:10]
probe = probe == '1'
now = time.time()

def sh(cmd):
    try:
        return subprocess.run(cmd, capture_output=True, text=True, timeout=25).stdout.strip()
    except Exception:
        return ''

def token_info(path):
    if not os.path.exists(path):
        return None
    tok = open(path, encoding='utf-8', errors='replace').read().strip()
    if not tok:
        return {'empty': True}
    try:
        raw = base64.urlsafe_b64decode(tok + '=' * (-len(tok) % 4))
        issued = int.from_bytes(raw[1:9], 'big')
        return {
            'len': len(tok), 'raw': len(raw), 'version': '0x%02x' % raw[0],
            'issued': issued, 'age_min': int((now - issued) / 60),
            'head': tok[:24],
        }
    except Exception as e:
        return {'bad': str(e), 'len': len(tok)}

# ---- service
active = sh(['systemctl', 'is-active', service]) or 'unknown'
pid = ''
ls = sh(['ss', '-lntpH'])
for line in ls.splitlines():
    if ':18080 ' in line and 'pid=' in line:
        pid = line.split('pid=')[1].split(',')[0]
        break
uptime = ''
if pid:
    uptime = sh(['ps', '-p', pid, '-o', 'etime=']).strip()

# ---- scheduler
roll_pid_file = os.path.join(state_dir, 'roll.pid')
roll_pid = open(roll_pid_file).read().strip() if os.path.exists(roll_pid_file) else ''
roll_alive = bool(roll_pid) and os.path.exists('/proc/' + roll_pid)
roll_uptime = sh(['ps', '-p', roll_pid, '-o', 'etime=']).strip() if roll_alive else ''

# ---- events
events = []
ev_path = os.path.join(state_dir, 'events.jsonl')
if os.path.exists(ev_path):
    for line in open(ev_path, encoding='utf-8', errors='replace'):
        line = line.strip()
        if line.startswith('{'):
            try:
                events.append(json.loads(line))
            except Exception:
                pass
installs = [e for e in events if e.get('event') == 'install']
misses = [e for e in events if e.get('event') == 'miss']
last_install = installs[-1] if installs else None
# a hunt that re-installs the same token every 30 s is a stuck loop, not progress
stuck = False
if len(installs) >= 4:
    tail = installs[-4:]
    ages = [e.get('replaced_age_min') for e in tail]
    tss = [e.get('at', '') for e in tail]
    if len(set(ages)) <= 2 and all(a is None or a >= 30 for a in ages):
        stuck = True

# ---- history: what lengths are on disk
hist = {}
try:
    out = subprocess.run(
        [binary, 'tsgrab', '--body-file', tpl, '--want-lengths', want,
         '--scan-only', '--scan-fresh-within', '1800'],
        capture_output=True, text=True, timeout=60).stdout
    for line in out.splitlines():
        if 'by length' in line:
            for part in line.split(':', 1)[1].split():
                if ':' in part:
                    k, v = part.split(':')
                    hist[int(k)] = int(v)
except Exception:
    pass

usable = None
try:
    out = subprocess.run(
        [binary, 'tsgrab', '--body-file', tpl, '--want-lengths', want,
         '--scan-only', '--quiet', '--scan-fresh-within', '1800'],
        capture_output=True, text=True, timeout=60).stdout.strip()
    if out:
        raw = base64.urlsafe_b64decode(out + '=' * (-len(out) % 4))
        usable = {'len': len(out), 'age_min': int((now - int.from_bytes(raw[1:9], 'big')) / 60)}
except Exception:
    pass

# ---- optional live probe
live = None
if probe:
    try:
        out = subprocess.run(
            [binary, 'tsgrab', '--body-file', tpl, '--auth', auth,
             '--want-lengths', want, '--attempts', '1', '--timeout', '30',
             '--out', os.path.join(state_dir, 'last.json')],
            capture_output=True, text=True, timeout=90).stdout
        for line in out.splitlines():
            if line.startswith('[01]'):
                live = line.strip()
                break
    except Exception as e:
        live = 'probe failed: %s' % e

tok = token_info(tokfile)

if mode == 'json':
    print(json.dumps({
        'service': {'active': active, 'pid': pid, 'uptime': uptime},
        'token': tok,
        'scheduler': {'running': roll_alive, 'pid': roll_pid, 'uptime': roll_uptime,
                      'installs': len(installs), 'misses': len(misses), 'stuck': stuck},
        'history': hist,
        'usable_from_history': usable,
        'live_probe': live,
    }, indent=2))
    sys.exit(0)

if mode == 'short':
    age = tok.get('age_min') if tok and 'age_min' in tok else '?'
    ln = tok.get('len') if tok and 'len' in tok else '-'
    sched = 'sched+' if roll_alive else 'sched-'
    flag = ' STUCK' if stuck else ''
    print('tok=%s age=%sm %s%s' % (ln, age, sched, flag))
    sys.exit(0)

# ---- human
W = 62
def rule(t=''):
    print(('-- ' + t + ' ').ljust(W, '-') if t else '-' * W)

print('turn-state status'.center(W))
rule('service')
print('  codex-rec      : %s%s' % (active, ('  pid %s  up %s' % (pid, uptime)) if pid else ''))
rule('injected token')
if tok is None:
    print('  (no token file: nothing is injected)')
elif tok.get('empty'):
    print('  (file is empty: injection is switched off)')
elif tok.get('bad'):
    print('  BROKEN: %s' % tok['bad'])
else:
    print('  len            : %d' % tok['len'])
    print('  issued         : %sZ' % time.strftime('%H:%M:%S', time.gmtime(tok['issued'])))
    print('  age            : %d min' % tok['age_min'])
    verdict = ('honoured (under the ~30 min we measured)' if tok['age_min'] < 30
               else 'probably refused by now' if tok['age_min'] > 62
               else 'borderline (refusal was measured between 34 and 62 min)')
    print('  verdict        : %s' % verdict)
rule('scheduler')
if roll_alive:
    print('  tsroll         : running (pid %s, up %s)' % (roll_pid, roll_uptime))
    print('  installs/miss  : %d / %d' % (len(installs), len(misses)))
    if last_install:
        print('  last install   : %s (from %s)' % (last_install.get('at', '?'), last_install.get('source', '?')))
    if stuck:
        print('  ** STUCK **    : re-installing a token that is already 30+ min old every cycle.')
        print('                   Its window is over, so the hunt repeats forever. Fix: kill')
        print('                   tsroll and pick up a fresh %s, or widen --scan-fresh-within.' % want)
else:
    print('  tsroll         : not running')
rule('history on disk')
if hist:
    print('  by length      : %s' % '  '.join('%d x%d' % (k, v) for k, v in sorted(hist.items())))
else:
    print('  (scan produced nothing)')
if usable:
    print('  usable now     : len=%d age=%d min  <- could be installed without a request' % (usable['len'], usable['age_min']))
else:
    print('  usable now     : none within 30 min')
if live is not None:
    rule('live probe')
    print('  %s' % live)
    print('  (the connection is aborted after the headers, so this is not billed)')

# ---- the value itself, last and on its own line so `| tail -1` is enough
rule('token now in use')
if tok is None:
    print('  (no token file: nothing is injected)')
elif tok.get('empty'):
    print('  (file is empty: injection is switched off)')
elif tok.get('bad'):
    print('  BROKEN: %s' % tok['bad'])
else:
    # Read the file again rather than reusing the parsed fields: this is the byte string the proxy
    # will actually send, and the whole point of printing it is to paste it somewhere.
    raw = open(tokfile, encoding='utf-8', errors='replace').read().strip()
    print(raw)
PY
