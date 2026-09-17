# Ops: keeping a fresh `x-codex-turn-state` injected

The operational side of [codex-rec](../README.md): a small set of scripts that keep a fresh
`x-codex-turn-state` in the proxy, so a real codex client keeps the routing state the backend
hands out per turn.

The `codex-rec` binary does the work that needs to speak TLS exactly like the client
(`tsgrab`). Everything here is bash + curl + python3, so it can be changed on a live box
without a rebuild.

## What a turn-state is, and why it needs babysitting

`x-codex-turn-state` is a token the backend issues in its **response headers** as part of a
turn's routing state. Measured behaviour on this account (see `docs/fingerprint.md` in the
parent repo):

| observation | value |
|---|---|
| token length, healthy | **292** base64 chars (217 decoded bytes) |
| token length when it goes wrong | 312 (233 bytes) |
| structure | `0x80` version byte, then u64 big-endian issue time, rest opaque |
| honoured for | ~30 min confirmed honoured; **refused** by 62 min |
| issuing | re-issued per turn; the client echoes it within a turn |

So a token has a use-by time that is shorter than an hour, and the backend issues the healthy
kind only in bursts -- minutes-long stretches hand out the other length. Polling alone cannot
cover the gaps, so a rotation loop is needed.

## Files

| file | role |
|---|---|
| `tsroll.sh` | the rotation loop: check age, hunt for a newer token, install it |
| `tsroll.service` | systemd unit so the loop survives reboots and crashes |
| `fpstatus.sh` | one command for the current state; `\| tail -1` prints the token |
| `tsgrab.py` | python reference for probing/replaying, kept for ad-hoc work |
| `tsproxy.py` | run a codex-rec instance with a chosen token injected (experiments) |
| `tsprobe.sh` | plain sampler: log what the backend hands out, no injection |
| `tsauto.sh` | earlier rotation attempt, superseded by `tsroll.sh` (kept for reference) |
| `override.env` | **not committed** - the admin API key when the override mirror is used |

`tsroll.sh` calls `codex-rec tsgrab` for both of its inputs: `--scan-only` to read tokens the
recorder already logged (free, no request) and a plain probe (reads the response head, then
aborts, so nothing is billed).

## Install

```bash
install -d /root/tsgrab/state /root/tsgrab/templates
install -m755 tsroll.sh fpstatus.sh tsprobe.sh /root/tsgrab/
install -m644 tsroll.service /etc/systemd/system/
systemctl daemon-reload && systemctl enable --now tsroll
```

The proxy side needs the per-request file read, in `codex-rec.toml`:

```toml
[headers.request]
set_from_file = { "x-codex-turn-state" = "/root/codex-rec/ts_token.txt" }
```

## How the rotation works

```
every 30 s:
  token younger than 35 min?  -> nothing to do
  otherwise -> hunt:
      scan the recorder's history   (no request)      -> install if strictly newer
      probe the backend             (not billed)      -> install if strictly newer
      neither -> retry every 90 s, up to 60 times; then idle 20 min
```

Two properties matter, and both were learned the hard way:

1. **No restart.** The token file is re-read on every request (`set_from_file`), so
   `install_token` takes effect on the next request. Verified: the proxy's pid does not change.
2. **Strictly-newer gate.** Without it, a scan that keeps returning the same already-old token
   re-installs it forever: the slot never gets younger, so the age check fires again
   immediately. That produced a 30-second loop that refreshed nothing while looking busy.

## Status

```bash
fpstatus.sh                 # full report: service, token age + verdict, scheduler, history
fpstatus.sh --short         # one line: tok=292 age=7m sched+
fpstatus.sh | tail -1       # just the token currently in use
fpstatus.sh --probe         # also send one probe (not billed)
```

```
$ fpstatus.sh
                      turn-state status
-- service ---------------------------------------------------
  codex-rec      : active  pid 189460  up 18:51
-- injected token --------------------------------------------
  len            : 292
  issued         : 23:00:42Z
  age            : 3 min
  verdict        : honoured (under the ~30 min we measured)
-- scheduler -------------------------------------------------
  tsroll         : running (pid 197614, up 00:18)
  installs/miss  : 20 / 0
-- history on disk -------------------------------------------
  by length      : 292 x1
  usable now     : len=292 age=3 min  <- could be installed without a request
-- token now in use ------------------------------------------
gAAAAABqrHEaXtWGjpKvF6l3vobFea0U98v-d-8xD7TL6wgUWFYzAoNQtLsyvVWZFsrcLDaK…
```

## Optional: mirror each install to a remote admin API

Some deployments need the same token pushed to a relay's admin API, so that side can pin the
same routing state. `install_token` calls `push_override` right after the local write.

It is **off unless configured**, and the key is deliberately not a default in the script (this
repository is public):

```bash
# /root/tsgrab/override.env, chmod 600
OVERRIDE_URL=https://example.invalid/api/admin/accounts/turn-state-override
OVERRIDE_KEY=<admin key>
OVERRIDE_ACCOUNT=acct_…
```

`tsroll.sh` sources that file when `OVERRIDE_KEY` is not already in the environment. The push
is best-effort: the local write is what the proxy uses, so a failure is logged and never rolls
it back or aborts the rotation. The request body is built by hand, so the token is checked
against `[A-Za-z0-9_=-]` first -- a token containing a quote or newline is refused and logged
rather than sent as malformed JSON.

## Operating notes

* **Never let the systemd `ExecStart` drift from the config it can parse.** Changing
  `codex-rec.toml` to use an option an older binary does not know made the service crash-loop
  11 517 times and flooded the log. `systemctl stop` first, then verify, then start.
* **Verify by sending a real request, not by reading the config.** Both silent failures here
  (a `--quiet` mode that printed an old token, and a `set_from_file` resolved twice at startup)
  looked perfectly healthy in the config and in the audit file. What caught them was comparing
  the token file with what `*.req.out.hdr` shows was actually sent.
* The audit file lists configured headers as `# from file:` comments and then the header that
  was really sent, marked `<- sent (last write wins)`. That single marked line is the answer to
  "what went upstream?".
