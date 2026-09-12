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
# Every field is "override or inherit":
#   - omit the key, or write "inherit"  -> keep whatever the client sent
#   - any other string                  -> replace that part of the identity
[persona]
originator    = "codex-tui"        # `originator` header + User-Agent product name
codex_version = "0.145.0"          # User-Agent version segments (+ ?client_version=)
os            = "Debian 12.0.0"    # the "<os>; <arch>" part of the User-Agent
arch          = "x86_64"
terminal      = "tmux/3.3a"        # "" removes the terminal segment entirely
user_agent    = "inherit"          # escape hatch: a literal User-Agent, wins over all of the above
rewrite_client_version = true      # also rewrite ?client_version= on /models

# ---------------------------------------------------------------- header rewriting
[headers.request]
drop = ["cf-ray", "cf-connecting-ip", "cf-connecting-ipv6", "x-forwarded-for", "x-real-ip", "cdn-loop"]
set  = { }

# [headers.request.set]
# "x-openai-internal-codex-residency" = "us"

[headers.response]
drop = [ ]
set  = { }

# ---------------------------------------------------------------- TLS
[tls]
# fixed     -> native-tls / OpenSSL, no ALPN, HTTP/1.1: byte-identical ClientHello to the real
#              codex client (JA3 0b85eb0d4981e69064e40753e4f0ac5f with SNI). Default.
# randomize -> rustls backend, whose ClientHello shuffles the extension order per connection.
#              Caveat: rustls also has a different cipher/group set (10/4 instead of 30/8), so this
#              mode does NOT look like codex; it is only for experiments. Requires a build with
#              the `rustls-backend` feature (the Linux release and CI builds include it).
extension_order = "fixed"
```

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
