# Configuration

Everything can be set in a TOML file, via CLI flags, or both. Precedence:
**built-in defaults < TOML file < CLI flags**.

The config file is looked up in this order:

1. `--config <path>`
2. `$CODEX_REC_CONFIG`
3. `./codex-rec.toml`
4. `/etc/codex-rec.toml`

## Full example

```toml
listen   = "127.0.0.1:18080"
upstream = "https://chatgpt.com"
log_dir  = "/root/rec"

# ---------------------------------------------------------------- device persona
# Each field has three states, decided by whether the key is present:
#   - key absent      -> inherit the client's value (nothing is rewritten)
#   - "inherit"       -> same as absent, kept as a readable alias
#   - any other value -> use that value
#   - "" is only valid for `terminal` (it removes the terminal segment); an empty
#     originator/codex_version/os/arch/user_agent is a config error instead of a broken UA
[persona]
originator    = "codex-tui"        # `originator` header + User-Agent product name
codex_version = "0.145.0"          # User-Agent version segments (+ ?client_version=)
os            = "Debian 12.0.0"    # the "<os>; <arch>" part of the User-Agent
arch          = "x86_64"
terminal      = "tmux/3.3a"        # "" removes the terminal segment entirely
user_agent    = "inherit"          # escape hatch: a literal User-Agent, wins over all of the above
rewrite_client_version = true      # also rewrite ?client_version= on /models

# ---------------------------------------------------------------- header rewriting
# `drop` entries are case-insensitive glob patterns (globset), so `cf-*` works.
[headers.request]
drop = ["cf-*", "x-forwarded-*", "x-real-ip", "cdn-loop"]
set  = { }

# [headers.request.set]
# "x-openai-internal-codex-residency" = "us"

[headers.response]
drop = [ ]
set  = { }

# ---------------------------------------------------------------- path mapping
# `upstream_prefix` defaults to /backend-api/codex ON PURPOSE: codex only treats a provider as the
# codex backend when its base_url ends with that path (model-provider-info/src/lib.rs,
# supports_codex_backend_routes). With any other path it stops sending codex-backend-only headers
# such as x-codex-routing-hint and the request shape changes. `/v1` and `/` are accepted as
# *incoming* aliases so other OpenAI-style clients can use the proxy — they are not the default,
# and codex itself should keep using /backend-api/codex.
[routes]
upstream_prefix = "/backend-api/codex"
strip_prefixes  = ["/backend-api/codex", "/v1", "/api/v1"]

# ---------------------------------------------------------------- TLS
[tls]
# fixed     -> native-tls / OpenSSL, no ALPN, HTTP/1.1: byte-identical ClientHello to the real
#              codex client (JA3 0b85eb0d4981e69064e40753e4f0ac5f with SNI). Default.
# randomize -> rustls backend, whose ClientHello shuffles the extension order per connection.
#              Caveat: rustls also has a different cipher/group set (10/4 instead of 30/8), so this
#              mode does NOT look like codex; it is only for experiments. Requires a build with
#              the `rustls-backend` feature (the Linux release and CI builds include it).
extension_order = "fixed"

# ---------------------------------------------------------------- sessions
# With session_capture = true every codex session also gets its own interaction log:
#   <session_dir>/ss-<date>-<session-uuid>-<title>.jsonl
# One JSON object per line: requests (with the decoded body) and responses (with the answer text).
session_capture = true
session_dir     = "./sessions"
```

## Persona field states

Whether a key is **present** decides what happens — there are no hidden defaults:

| config | effect |
|---|---|
| key absent | inherit the client's value |
| `field = "inherit"` | same as absent (readable alias) |
| `field = "some value"` | use that value |
| `field = ""` | only valid for `terminal`: removes the terminal segment. Anywhere else it is a config error |

So the smallest useful persona changes exactly one thing and leaves everything else untouched:

```toml
[persona]
codex_version = "0.145.0"
# client : codex-tui/0.153.4 (Debian 12.0.0; x86_64) xterm-256color (codex-tui; 0.153.4)
# upstream: codex-tui/0.145.0 (Debian 12.0.0; x86_64) xterm-256color (codex-tui; 0.145.0)
```

Incoming User-Agents that do not parse (anything that is not `product/version (os; arch) …`) are
left untouched rather than replaced with a guess.

## Path mapping

`[routes]` maps what clients call onto what the codex backend expects:

| incoming | upstream |
|---|---|
| `/backend-api/codex/responses` | `/backend-api/codex/responses` (unchanged — this is what codex sends) |
| `/v1/responses` | `/backend-api/codex/responses` |
| `/api/v1/models?client_version=…` | `/backend-api/codex/models?client_version=…` |
| `/responses` | `/backend-api/codex/responses` |

Keep `upstream_prefix = "/backend-api/codex"` for codex clients: codex only treats a provider as the
codex backend when its `base_url` ends with that path, otherwise it stops sending codex-backend-only
headers such as `x-codex-routing-hint`. `/v1` and `/` exist for other OpenAI-style clients; they are
not the default. Incoming and outgoing paths are both recorded (`req-*.hdr` vs `req-*.out.hdr`).

## Swapping credentials

`[headers.request].set` is applied after the client's headers are copied, so it can replace the
bearer token and the account id — i.e. the proxy can run a client's traffic against a different
account:

```toml
[headers.request]
set = { authorization = "Bearer sk-...", "chatgpt-account-id" = "00000000-0000-0000-0000-000000000000" }
```

Keep such a config file at mode `0600`; the recorder itself never writes tokens to the log
(`authorization`/`cookie`/`x-api-key` are redacted in every recorded header file).

## CLI flags

All flags still work and extend (not replace) the file:

```
--config <path>            explicit config file
--listen  <addr>           default 127.0.0.1:18080
--upstream <url>           default https://chatgpt.com
--log-dir <path>           default /root/rec
--drop-req-header a,b      add to headers.request.drop
--set-req-header a=b;c=d   add to headers.request.set
--drop-res-header a,b      add to headers.response.drop
--set-res-header a=b;c=d   add to headers.response.set
--drop-body-key k1,k2      remove JSON keys from the request body (zstd handled automatically)
--set-body-key k=json;…    set JSON keys in the request body
--rewrite-catalog          force use_responses_lite=false and empty service_tiers in /models
```

## Deploying with the codex client

```toml
# ~/.codex/config.toml on the client machine
model_provider = "openai-custom"

[model_providers.openai-custom]
name = "OpenAI"                  # must stay "OpenAI": it decides codex-backend routing
requires_openai_auth = true      # keeps using the ChatGPT OAuth tokens
wire_api = "responses"
supports_websockets = true
base_url = "http://127.0.0.1:18080/backend-api/codex"   # <- the recorder
```

`base_url` has to end in `/backend-api/codex`, otherwise codex does not treat the endpoint as the
codex backend and stops sending the backend-specific headers.

## What gets recorded

```
index.jsonl               one line per request/response (+ persona changes, origin pool, model/tier)
req-<stamp>.hdr           incoming request line + headers (authorization redacted)
req-<stamp>.out.hdr       exactly what went upstream after persona + header rewriting
req-<stamp>.body / .json  raw body and the zstd-decoded body
req-<stamp>.summary.json  model, service_tier, reasoning, instructions length/sha, client_metadata
resp-<stamp>.hdr          status + headers, with the Cloudflare `__oailb` origin pool decoded
resp-<stamp>.sse          raw streamed response
resp-<stamp>.summary.json every `model` value seen, service_tier echoes, response ids, errors
```

Compare `req-*.hdr` with `req-*.out.hdr` to prove that a persona only touched the fields you
configured.

## `[record]` — what to write (v0.4.0)

Every artifact has its own switch and they all default to `true` except the catalog stream:

```toml
[record]
enabled = false            # master switch: false = pure forwarder, nothing is written at all
                           # (persona, header rules and [routes] still apply)

index = true               # index.jsonl
request_headers = true     # <stem>.req.hdr        (incoming, secrets redacted)
request_headers_out = true # <stem>.req.out.hdr    (what we actually sent)
request_body_raw = true    # <stem>.req.body       (only when the client compressed the body)
request_body_json = true   # <stem>.req.json       (decoded)
request_body_out = true    # <stem>.req.out.json   (only when --drop/set-body-key rewrote it)
request_summary = true     # <stem>.req.summary.json
response_headers = true    # <stem>.resp.hdr       (with the decoded __oailb origin host)
response_stream = true     # <stem>.resp.stream.sse|json|bin
response_stream_catalog = false  # keep the whole /models payload (off: etag + sha256 only)
response_summary = true    # <stem>.resp.summary.json

# retention (the pruner runs at startup and then hourly)
retention_days = 14        # delete files older than this
gzip_after_days = 3        # gzip files older than this (`gzip -f`, skipped when missing)
max_total_bytes = 5000000000  # delete the oldest files until the tree fits
```

Turning an artifact off also skips its work: no `zstd` decode when neither `request_body_json` nor
`request_summary` is on, and the response body is passed straight through when `response_stream` and
`response_summary` are both off.

### Layout

```text
<log_dir>/<codex-session-uuid>/260912-231425-user-r0001.req.hdr
                               260912-231425-user-r0001.req.json
                               260912-231425-user-r0001.resp.stream.sse
                               ...
<log_dir>/_nosession/...       # requests without a session-id header
```

`user|system` is the turn's thread source (the TITLE/RECAP helpers run as system threads), `rNNNN` is
the per-session request number and is shared by the matching response. Empty files are never created,
and a decoded request body is only written when the request was compressed (otherwise it would be a
copy of `req.json`). Session capture events carry `body_file` / `summary_file` / `response_file`
pointers into this directory instead of duplicating the payload.

## `[limits]` — memory bounds (v0.4.0)

```toml
[limits]
request_body_bytes = 4194304       # 4 MiB in-memory cap for a request body
request_body_over_limit = "spill"  # "spill" (default) | "reject" (413) | "stream",
                                   # "stream" only applies when nothing needs the body
summary_buffer_bytes = 4194304     # in-memory copy for non-SSE response summaries (0 = none)
write_buffer_bytes = 262144        # BufWriter size for recorded files (0 = write straight through)
```

SSE responses are summarized *while they stream*, so they never need a full in-memory copy; the
`summary_buffer_bytes` cap only applies to non-SSE bodies such as the `/models` catalog. A body that
exceeds `request_body_bytes` is spilled to a file and forwarded from there — memory stays bounded and
nothing is lost, but the decoded body/JSON/summary are skipped for that request (the index row marks
`request_body_spilled = true`).

## `set_from_env` — credentials outside the file (v0.4.0)

```toml
[headers.request]
set = { "x-fixed" = "plain", authorization = "env:CODEX_TOKEN" }   # `env:NAME` is read from the env
set_from_env = { "chatgpt-account-id" = "CODEX_ACCOUNT_ID" }        # explicit form
```

A missing environment variable produces a startup warning and the header is skipped.
