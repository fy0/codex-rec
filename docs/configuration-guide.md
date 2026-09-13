# codex-rec configuration guide

Everything lives in one TOML file. Precedence: **built-in defaults < TOML file < CLI flags**.
The file is found in this order: `--config <path>`, `$CODEX_REC_CONFIG`, `./codex-rec.toml`,
`/etc/codex-rec.toml`. Unknown keys are a hard error (`deny_unknown_fields`), so typos never fail
silently.

```toml
listen   = "127.0.0.1:18080"   # address the recorder listens on
upstream = "https://chatgpt.com"   # scheme + host only; the path comes from [routes]
log_dir  = "/root/rec"

session_capture = true          # also write one JSONL per session (default false)
session_dir     = "/root/sessions"
```

---

## 1. `[persona]` — the device identity (most important section)

The persona decides what **identity the upstream sees**. Every field has two states, decided by
whether the key is present at all:

| Written as | Effect |
|---|---|
| key **absent** (or commented out) | **inherit** the value the client sent — nothing is rewritten |
| key present with a value | replace that part |
| `terminal = ""` | remove the terminal segment entirely (**only** `terminal` may be empty) |

**Recommended style: keep the fields you want to inherit commented out.** That way the file states
explicitly which parts are pinned and which follow the client, instead of repeating a magic word:

```toml
[persona]
originator    = "codex-tui"    # pinned
codex_version = "0.145.0"      # pinned
# os          = ...            # commented out -> inherit the client's OS string
# arch        = ...            # commented out -> inherit the client's architecture
# terminal    = ...            # commented out -> inherit the client's terminal
# user_agent  = ...            # commented out -> inherit the whole User-Agent
```

`"inherit"` is still accepted as an explicit value (useful when a value is generated or when the key
must be present), but it is no longer the recommended way to express the default.

```toml
[persona]
originator    = "codex-tui"     # `originator` header + product name in the User-Agent
codex_version = "0.145.0"       # both version segments of the User-Agent
# os          = ...            # commented out = inherit "<OS> <version>", e.g. "Debian 12.0.0"
# arch        = ...            # commented out = inherit "x86_64" / "arm64" / ...
# terminal    = ...            # commented out = inherit the terminal segment (see §2)
# user_agent  = ...            # commented out = inherit the whole User-Agent
rewrite_client_version = false  # also rewrite `?client_version=` (needs codex_version)
```

The User-Agent codex builds is:

```
<originator>/<version> (<os> <os-version>; <arch>) <terminal> (<user_agent_suffix>)
```

Measured example (Linux client in tmux):

```
codex-tui/0.153.4 (Debian 12.0.0; x86_64) tmux/3.3a (codex-tui; 0.153.4)
```

Notes that matter in practice:

* `originator` also feeds the `originator` header, so both stay consistent.
* `rewrite_client_version` is **off by default on purpose**: the server picks the model catalog by
  `?client_version=`, not by the User-Agent. Rewriting it to an older version downloads an older
  catalog (e.g. before `0.153.0` there is no `gpt-6-astra`), which turns into
  "Model metadata … not found" at startup. Keep the flag off and the mask only changes the header.
* Empty values for `originator` / `codex_version` / `os` / `arch` / `user_agent` are rejected at
  startup (they would produce an invalid User-Agent); leave the key out (commented) instead.
* Nothing is rewritten when every field is absent — the section is inert. A `[persona]` block whose
  fields are all commented out is therefore a no-op that documents intent.

---

## 2. `terminal` values — what the client can send (`codex-rs/terminal-detection`)

The terminal segment is detected from the environment, never executed. Detection order:

| Probe (first match wins) | Resulting segment |
|---|---|
| `TERM_PROGRAM` (+ `TERM_PROGRAM_VERSION`) | the raw value, e.g. `iTerm.app/3.5.0`, `WarpTerminal`, `vscode/1.90.0` |
| `TERM_PROGRAM=tmux` | treated as a multiplexer, not the terminal (keeps probing) |
| `GHOSTTY_RESOURCES_DIR` | `Ghostty` |
| `WEZTERM_VERSION` | `WezTerm` or `WezTerm/<version>` |
| `ITERM_SESSION_ID` / `ITERM_PROFILE` / `ITERM_PROFILE_NAME` | `iTerm.app` |
| `TERM_SESSION_ID` | `Apple_Terminal` |
| `KITTY_WINDOW_ID` or `TERM` contains `kitty` | `kitty` |
| `ALACRITTY_SOCKET` or `TERM=alacritty` | `Alacritty` |
| `KONSOLE_VERSION` | `Konsole` / `Konsole/<version>` |
| `GNOME_TERMINAL_SCREEN` | `gnome-terminal` |
| `VTE_VERSION` | `VTE` / `VTE/<version>` |
| `WT_SESSION` | `WindowsTerminal` |
| `TERM` (any other value) | that raw value, e.g. `xterm-256color`, `screen`, `linux` |
| nothing set | `unknown` |
| `TERM=dumb` | `dumb` |

Multiplexers are tracked separately and appear as a second token in practice (`tmux/3.3a` above);
`TMUX`/`TMUX_PANE` → tmux and `ZELLIJ*` → zellij.

**Consequences for the persona.** `terminal` is a free-form string here, so any of the values above
is valid, plus:
* a literal (`"WindowsTerminal"`, `"iTerm.app/3.5.0"`, `"xterm-256color"`) to pin a fixed look;
* the key commented out (or absent) to keep whatever the real client sent;
* `""` to drop the segment entirely (`codex-tui/0.145.0 (Debian 12.0.0; x86_64) (codex-tui; 0.145.0)`).

Remember the shell is *not* the terminal: PowerShell inside Windows Terminal yields
`WindowsTerminal`, while a bare `powershell.exe` console yields `unknown`.

---

## 3. `[headers.request]` / `[headers.response]` — header rewriting

```toml
[headers.request]
drop = ["cf-*", "x-forwarded-*", "x-real-ip", "cdn-loop"]   # glob patterns (globset crate)
set  = { "x-openai-internal-codex-residency" = "eu" }        # literals, or "env:NAME"
set_from_env = { "chatgpt-account-id" = "CODEX_ACCOUNT_ID" } # value read from the environment

[headers.response]
drop = []
set  = { "x-served-by" = "codex-rec" }
```

* `drop` matches header **names** case-insensitively and accepts globs (`cf-*`).
* `set` wins over passthrough; `env:VAR` and `set_from_env` read a value from the environment so
  swapped credentials never sit in the file. A missing variable logs a warning and skips that header.
* Hop-by-hop headers (`host`, `connection`, `content-length`, `transfer-encoding`, `keep-alive`,
  `upgrade`, `proxy-connection`) are always dropped in both directions.
* `authorization`, `cookie` and `x-api-key` are redacted in every recorded header dump.

---

## 4. `[routes]` — path mapping

```toml
[routes]
upstream_prefix = "/backend-api/codex"                        # default, keep it for codex
strip_prefixes  = ["/backend-api/codex", "/v1", "/api/v1"]    # default
```

Incoming `/v1/responses` → upstream `/backend-api/codex/responses`. An empty `upstream_prefix`
forwards the stripped path unchanged. **Keep the default when the client is codex**: only a
`base_url` ending in `/backend-api/codex` makes codex use its backend routes
(`x-codex-routing-hint`, guardian endpoints, lite request shape).

---

## 5. `[tls]`

```toml
[tls]
extension_order = "fixed"   # or "randomize"
```

`fixed` (default) = native-tls/OpenSSL, byte-identical ClientHello to the real codex client
(measured JA3 match: `23211f2b48104c7030b93680a2efcfd0` without SNI-family extensions,
`0b85eb0d4981e69064e40753e4f0ac5f` with a different parser). `randomize` switches to the rustls
backend, whose extension order shuffles per connection (OpenSSL has no such option); it needs the
`rustls-backend` cargo feature and logs a warning when it is missing.

---

## 6. `[record]` — what to write (all default `true` except the catalog stream)

```toml
[record]
enabled = false                  # master switch: false = pure forwarder, nothing written
index = true                     # index.jsonl
request_headers = true           # <stem>.req.hdr        (incoming, secrets redacted)
request_headers_out = true       # <stem>.req.out.hdr    (what was actually sent)
request_body_raw = true          # <stem>.req.body       (only when the client compressed it)
request_body_json = true         # <stem>.req.json       (decoded)
request_body_out = true          # <stem>.req.out.json   (only when a rewrite changed the body)
request_summary = true           # <stem>.req.summary.json
response_headers = true          # <stem>.resp.hdr       (with the decoded __oailb origin host)
response_stream = true           # <stem>.resp.stream.sse|json|bin
response_stream_catalog = false  # keep the whole /models payload (off: etag + sha256 only)
response_summary = true          # <stem>.resp.summary.json

retention_days  = 14             # delete recorded files older than N days
gzip_after_days = 3              # gzip older files (`gzip -f`; skipped when unavailable)
max_total_bytes = 5000000000     # delete the oldest files until the tree fits
```

Turning an artifact off also skips its work (no `zstd` decode, no response buffering).

Layout (one directory per codex session, empty files never created):

```text
<log_dir>/<session-uuid>/260912-231425-user-r0001.req.hdr
                         260912-231425-user-r0001.req.json
                         260912-231425-user-r0001.resp.stream.sse
<log_dir>/_nosession/…   # requests without a session-id header
```

`user|system` is the turn's thread source (TITLE/RECAP run as system threads) and `rNNNN` is the
per-session request number, shared by the matching response.

---

## 7. `[limits]` — memory bounds

```toml
[limits]
request_body_bytes      = 4194304   # 4 MiB in-memory cap for a request body
request_body_over_limit = "spill"   # "spill" (default) | "reject" (413) | "stream"
summary_buffer_bytes    = 4194304   # non-SSE summary buffer (0 = keep nothing)
write_buffer_bytes      = 262144    # BufWriter size for recorded files (0 = write through)
```

SSE responses are summarized while they stream, so they need no full copy; the cap applies to
non-SSE bodies such as `/models`. A body above the cap is spilled to a file and forwarded from there
(nothing is lost, memory stays bounded; the decoded body/summary are skipped and `index.jsonl` marks
`request_body_spilled = true`).

---

## 8. `[rewrite.environment]` — editing the `<environment_context>` block

codex injects `<environment_context>` as a `role: "user"` item in `input`, once per turn when it
changes. Every key below is optional; **an absent key changes nothing**.

```toml
[rewrite.environment]
timezone      = "+08:00"        # offset, "UTC", or an IANA name such as "Asia/Shanghai"
current_date  = "auto"          # "auto" | "YYYY-MM-DD" | "shift+1d" | "shift-7d"
cwd           = "C:/proj/demo"  # <cwd>
shell         = "zsh"           # <shell>
workspace_roots = ["/srv/app"]  # replaces the <root> entries inside <workspace_roots>
drop          = ["subagents"]   # delete these elements; "environment_context" drops the block
fill_missing  = false           # create elements the client did not send (default false)
```

* `timezone` accepts both forms a real client sends: a UTC offset (`+08:00`, `-05:30`, `UTC`) or an
  IANA name (`Asia/Taipei`, `Europe/Berlin`, `America/Los_Angeles`).
  * On Windows, both the probe and the rewriter read the real system zone (`Get-TimeZone`, e.g.
    `China Standard Time`) and map it to its IANA equivalent, so a Windows host reports
    `Asia/Shanghai` rather than a bare offset.
  * Windows also has no `/usr/share/zoneinfo`, so a *configured* IANA name is resolved through the
    platform instead (`[System.TimeZoneInfo]::GetUtcOffset` at the requested instant) — DST included:
    `America/Los_Angeles` gives `-07:00` in summer and `-08:00` in winter. `CODEX_REC_TZ_DEBUG=1`
    prints which path produced the offset. IANA names are resolved through
  the host's tzdata (`/usr/share/zoneinfo`, …) so **DST is applied**; if tzdata is missing the
  recorder falls back to a fixed offset and says so in the log (`[timezone] no tzdata entry usable
  for …`). v0.5.1 fixed the TZif v2/v3 parser, which previously always took that fallback.
* `Asia/Taipei` is worth knowing about: its tzdata carries the historical +08:06 LMT and the 1937–1979
  +09:00 summer time, but every offset used today resolves to **+08:00** — the same as the simpler
  `"+08:00"` offset form.
* `current_date = "auto"` is **converted**, not copied: the request is stamped in UTC, so a zone east
  or west of UTC can be a day ahead or behind (20:30Z on the 12th is already the 13th at `+08:00`).
* `shift+Nd` / `shift-Nd` moves the converted date by whole days; a literal `YYYY-MM-DD` is used as-is.
* Only items whose text contains `<environment_context>` are touched; the rest of the body, the local
  session files and other tags such as `<permissions>` are left alone.
* Every change is reported in the log (`[rewrite] …`) and stored as `env_rewrite` in `index.jsonl`;
  the rewritten body is saved as `<stem>.req.out.json`.

---

## 9. Completed example

```toml
listen   = "127.0.0.1:18080"
upstream = "https://chatgpt.com"
log_dir  = "/root/rec"
session_capture = true
session_dir     = "/root/sessions"

[persona]
originator    = "codex-tui"
codex_version = "0.145.0"
# os          = ...            # inherit
# arch        = ...            # inherit
# terminal    = ...            # inherit
rewrite_client_version = false

[headers.request]
drop = ["cf-*", "x-forwarded-*", "x-real-ip", "cdn-loop"]
[headers.response]
drop = []

[rewrite.environment]
timezone     = "+08:00"
current_date = "auto"

[tls]
extension_order = "fixed"

[record]
response_stream_catalog = false

[limits]
request_body_bytes = 4194304
```

---

## 10. Generating a config for a machine: `codex-rec probe`

```bash
codex-rec probe                                  # report this machine + print a config for it
codex-rec probe --cwd /root/code --shell bash     # with a pinned working directory / shell
codex-rec probe --client-ua "<a user-agent>"      # adopt another machine's codex-native identity
```

The first block mirrors the real client's local probe: the User-Agent it would send, `originator`,
`?client_version=`, the detected terminal (plus the raw `TERM` / `TERM_PROGRAM` / `TMUX` /
`WT_SESSION` values behind that decision), shell, cwd, timezone, `current_date`, the
`<environment_context>` block, and `codex --version` if the CLI is installed. The terminal order is
the one documented in §2.

The second block is that same machine expressed as configuration: anything already equal to the host
is printed commented out (inherit), anything that differs is pinned — so on the machine it ran on,
pasting the output changes nothing, while pasting it on a different machine reproduces this one.
