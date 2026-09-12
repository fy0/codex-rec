# Codex client fingerprint — measured, not guessed

All numbers below were measured on a real `codex-cli` install (Debian 12, npm build) with a
purpose-built ClientHello sniffer plus an HTTP recording proxy. Nothing here is inferred from
source code alone.

## 1. TLS layer

The Linux codex client speaks **native-tls → a bundled OpenSSL** (static). It only switches to
rustls when `CODEX_CA_CERTIFICATE` / `SSL_CERT_FILE` is set (source comment:
*"forces rustls when a custom CA is configured"*).

Measured ClientHello (`codex-cli 0.145.0` and `0.153.4` are **identical**):

| property | value |
|---|---|
| cipher suites | **30**, OpenSSL `DEFAULT` list, in this order: `1302 1303 1301 c02c c030 009f cca9 cca8 ccaa c02b c02f 009e c024 c028 006b c023 c027 0067 c00a c014 0039 c009 c013 0033 009d 009c 003d 003c 0035 002f` |
| groups | **8**: `x25519mlkem768, x25519, p256, p384, x448, p521, ffdhe2048, ffdhe3072` |
| extensions (fixed order) | `renegotiation_info, server_name, ec_point_formats, supported_groups, session_ticket, encrypt_then_mac, extended_master_secret, signature_algorithms, supported_versions, psk_key_exchange_modes, key_share` |
| ALPN | **none** → HTTP/1.1 (no HTTP/2 is ever negotiated) |
| supported_versions / ec_point_formats | `TLS1.3, TLS1.2` / `[0]` |
| ClientHello size | **1543 B** with SNI, **1522 B** without |
| extension order | **fixed** → JA3 is stable and reproducible |
| **JA3 (with SNI)** | `0b85eb0d4981e69064e40753e4f0ac5f` |
| **JA3 (no SNI, IP)** | `23211f2b48104c7030b93680a2efcfd0` |

`codex-rec` reproduces this ClientHello field-for-field (same JA3, same 30 ciphers in the same
order, same 8 groups, same extension list and order, no ALPN, same hello size) by using
native-tls with a vendored OpenSSL and `http1_only()`.

### Two traps

* **rustls randomizes the ClientHello extension order** (`extension_order_seed`, rustls PR #1730).
  The same rustls binary produces a different JA3 on every process start (measured:
  `ab74e105aa9337a691d571a7f2890a7a` and `3d75208fffa…` for one binary, with identical cipher and
  group *sets*). A rustls client therefore cannot present a stable JA3.
* **OpenSSL has no option to shuffle extension order** (upstream issue #19220 is still open), so
  `tls.extension_order = "randomize"` is implemented by switching to the rustls backend — which
  also changes the cipher/group set to rustls' 10/4. It is an escape hatch, not codex-shaped.

### A widely circulated "codex fingerprint" card is wrong

A Go-simulated profile claiming to be `codex-cli 0.145.0` lists 10 cipher suites
(`1302 1301 1303 c02c c02b cca9 c030 c02f cca8 00ff`), 4 groups (`x25519mlkem768 x25519 p256 p384`),
HTTP/2 settings and "extension order random per connection (rustls behaviour)". Every one of those
points is a **rustls** profile. The real 0.145.0 sends the 30-cipher OpenSSL profile above with no
ALPN, so a simulator built on that card is distinguishable from codex by JA3, cipher set, extension
set, ALPN and extension order.

## 2. HTTP layer (56 recorded requests from real TUI sessions)

Headers present on real codex backend requests, with observed counts:

| header | count | notes |
|---|---|---|
| `user-agent` | 56 | `codex-tui/0.153.4 (Debian 12.0.0; x86_64) tmux/3.3a (codex-tui; 0.153.4)` |
| `originator` | 51 | `codex-tui` ×46, `codex_cli_rs` ×5 (the `/models` fallback and `codex exec`) |
| `session-id`, `thread-id`, `x-client-request-id`, `x-codex-window-id`, `x-codex-turn-metadata`, `x-codex-beta-features` | 36–37 | present on every session request |
| `chatgpt-account-id` | 39 | |
| `x-codex-routing-hint` | 24 | `model=gpt-6-astra` ×18, `model=gpt-6-astra;tier=priority` ×6 |
| `content-encoding: zstd` | 23 | request bodies are zstd-compressed |
| `x-openai-internal-codex-responses-lite` | 20 | present when the catalog advertises lite mode |
| `x-codex-turn-state` | 5 | server-issued sticky token, echoed back |
| `openai-beta` | **0** | WebSocket handshakes only, never plain HTTP |
| `x-codex-installation-id` | **0** | the installation id lives in the JSON body's `client_metadata` |
| `accept-encoding` | 1 (our own curl replay) | codex never sends it |

`x-codex-beta-features: remote_compaction_v2` was constant across all 36 requests.

`x-codex-turn-metadata` only ever carried:
`(request_kind=turn, thread_source=user, sandbox_mode=workspace-write)` ×22 and
`(request_kind=turn, thread_source=system, sandbox_mode=read-only)` ×14.
`request_kind=prewarm` appears only on the WebSocket prewarm handshake.

## 3. Request body layer (42 recorded bodies)

| field | observed values |
|---|---|
| `model` | `gpt-6-astra` in **42/42** bodies (never luna/terra/sol from the client) |
| `service_tier` | absent ×30, `priority` ×8 (never `flex`/`ultrafast`: the catalog filters them) |
| `reasoning.effort` | `high` ×22, `medium` ×16, `low` ×2, absent ×2 |
| `instructions` | only 6 bodies carry a top-level `instructions` (all 21261 B); the rest use responses-lite (system prompt as an input item) |
| `client_metadata` keys | `session_id, thread_id, turn_id, x-codex-window-id, x-codex-turn-metadata, x-codex-installation-id` (+ `root_turn_id` on 27) |
| `prompt_cache_key` | the session UUID (cache affinity) |

## 4. What a proxy must preserve (and must not add)

Preserve verbatim: the session headers listed above, `chatgpt-account-id`, the User-Agent shape,
`x-codex-turn-metadata`, the body's `client_metadata` / `prompt_cache_key` / `model` /
`service_tier`, and the TLS ClientHello.

Never add: `accept-encoding`, `cf-*` / `x-forwarded-*` (only relevant when the proxy itself sits
behind Cloudflare), or `x-openai-internal-codex-residency` (unless the account really is managed).

## 5. Reproducing the measurement

* TLS: a ~120-line plain-TCP sniffer parses the first ClientHello of each connection (JA3 +
  ciphers + groups + extensions + ALPN). Point a client at `127.0.0.1:8443` (no SNI) or at a
  `/etc/hosts` name (with SNI) and record the handshake.
* HTTP: run `codex-rec` with `base_url = "http://127.0.0.1:18080/backend-api/codex"` in the codex
  provider config; every request/response is written to the log directory.
