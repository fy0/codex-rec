# codex-rec

A small recording + forwarding HTTP/1.1 reverse proxy for the **Codex backend** (`chatgpt.com/backend-api/codex`).
It sits between a real codex client and OpenAI, records every request/response byte, and can present a
**configurable device persona** so the upstream sees the identity you choose.

Two things make it different from a normal debug proxy:

* **TLS parity with the real client.** The Linux codex client uses native-tls → a bundled OpenSSL,
  sends **no ALPN** and therefore speaks HTTP/1.1. `codex-rec` builds its own vendored OpenSSL and
  reproduces that ClientHello field-for-field — measured **JA3 `0b85eb0d4981e69064e40753e4f0ac5f`**
  (with SNI) / **`23211f2b48104c7030b93680a2efcfd0`** (without), the same 30 cipher suites in the same
  order, the same 8 groups and the same extension list. See [docs/fingerprint.md](docs/fingerprint.md).
* **Persona with override-or-inherit semantics.** Version, originator, OS/arch and terminal can each
  be inherited from the client or replaced from the config file — so you can, for example, make a
  0.153.4 client look like 0.145.0 (or like a completely different device) without touching it.

## Quick start

```bash
codex-rec --listen 127.0.0.1:18080 --upstream https://chatgpt.com --log-dir /root/rec
```

Point the client at it (see [docs/config.md](docs/config.md)):

```toml
[model_providers.openai-custom]
name = "OpenAI"
requires_openai_auth = true
wire_api = "responses"
base_url = "http://127.0.0.1:18080/backend-api/codex"
```

## Persona example: pretend to be an older client

```toml
[persona]
originator    = "codex-tui"
codex_version = "0.145.0"
# os          = ...            # commented out = inherit the client's OS string
# arch        = ...            # commented out = inherit the client's architecture
# terminal    = ...            # commented out = inherit the client's terminal
```

Only the fields you name are touched; everything else passes through untouched — leaving a key out
(commented out) is the recommended way to say "inherit the client's value". A request that arrives as

```
user-agent: codex-tui/0.153.4 (Debian 12.0.0; x86_64) xterm-256color (codex-tui; 0.153.4)
```

leaves as

```
user-agent: codex-tui/0.145.0 (Debian 12.0.0; x86_64) xterm-256color (codex-tui; 0.145.0)
```

`req-*.hdr` and `req-*.out.hdr` (incoming vs. outgoing headers) make this auditable for every request.

## Features

| | |
|---|---|
| recording | incoming/outgoing headers, raw + zstd-decoded bodies, streamed responses, per-request summaries (`model`, `service_tier`, reasoning effort, `client_metadata`), and the Cloudflare `__oailb` origin pool decoded from every response |
| persona | fields decided by whether the key is present: absent/commented out = inherit the client's value, any value = replace it (`"inherit"` is still accepted as an alias, `terminal = ""` removes the segment) |
| header rewriting | drop/set both directions, from the config file or CLI flags; `drop` patterns are globs (`cf-*`) |
| path mapping | `[routes]` maps `/v1` or `/` onto the codex path `/backend-api/codex` for non-codex clients; codex itself keeps using `/backend-api/codex` |
| body rewriting | remove/replace JSON keys (zstd is decoded and re-encoded automatically) |
| catalog rewriting | `--rewrite-catalog` forces `use_responses_lite=false` and clears `service_tiers` in `/models` |
| session capture | one interaction log per codex session: `sessions/ss-<date>-<uuid>-<title>.jsonl` (requests with decoded bodies, responses with the answer text) |
| credential swapping | `[headers.request].set` can replace `authorization` / `chatgpt-account-id`, so a client's traffic can run against another account |
| TLS modes | `fixed` (OpenSSL, codex-identical, default) or `randomize` (rustls backend, per-connection extension shuffling) |

## Build

```bash
cargo build --release                      # Linux (vendored OpenSSL)
cargo build --release --no-default-features --features native-tls-backend,rustls-backend
```

CI builds and attaches two artifacts per release:

* `codex-rec-x86_64-unknown-linux-musl` — static, no glibc dependency, includes both TLS modes
* `codex-rec-x86_64-pc-windows-msvc` — Windows (SChannel via native-tls; the `randomize` TLS mode is
  not included in this target)

## Documentation

* [docs/fingerprint.md](docs/fingerprint.md) — what the real codex client looks like on the wire
  (TLS, HTTP headers, request bodies), measured; plus why a widely circulated fingerprint card is wrong.
* [docs/config.md](docs/config.md) — full configuration reference.



## v0.6.0

* **IANA timezones work on Windows.** A Windows host has no `/usr/share/zoneinfo`, so
  `America/Los_Angeles` (and every other IANA name) used to fall back to a fixed offset. The rewriter
  and the probe now ask the OS (`[System.TimeZoneInfo]::GetUtcOffset`) for the offset **at the
  requested instant**, so DST is applied: `America/Los_Angeles` resolves to `-07:00` in September and
  `-08:00` in winter, matching the platform. `CODEX_REC_TZ_DEBUG=1` prints which path was used
  (`windows api`, `tzdata file`, or the fixed-offset fallback).

## v0.5.7

* **Timezone handling, fixed and verified.** IANA names (`Asia/Taipei`, `America/Los_Angeles`) are now
  resolved through the host's tzdata: the TZif v2/v3 reader was locating the 64-bit block with a
  hand-computed header length and silently fell back to a fixed offset. It now finds the second `TZif`
  magic, keeps the transition width from the version byte, and ships a synthetic-tzfile test that runs
  with no tzdata installed. `Asia/Taipei` resolves as +08:00 (matches the system `date`) and
  `America/Los_Angeles` gives -08:00 in winter / -07:00 in summer (matches zoneinfo).
* **Debug hooks:** `CODEX_REC_TZ_DEBUG=1` logs the offset a zone resolved to, and
  `CODEX_REC_NOW_TS=<epoch>` pins the instant used to derive `current_date` (both absent in normal
  runs) — that is how a deployed binary's DST behaviour is verified against the system clock.
* **`[rewrite.environment]`** — edit the `<environment_context>` block the client sends: `timezone`
  (offset or IANA name, DST via the system tzdata), `current_date` (`auto` converts the UTC stamp into
  the configured zone, `shift±Nd`, or a literal date), `cwd`, `shell`, `workspace_roots`, `drop`, and
  `fill_missing`. Absent keys change nothing; every edit is logged, stored as `env_rewrite` in
  `index.jsonl`, and the rewritten body is saved as `.req.out.json`.
* **`docs/configuration-guide.md`** — annotated guide to every key, including the full list of
  terminal values `codex-rs/terminal-detection` can produce and what the persona does with them.

## v0.5.0

* **`[rewrite.environment]`** — edit the `<environment_context>` block the client sends: `timezone`
  (offset or IANA name, with DST via the system tzdata), `current_date` (`auto` converts the UTC
  stamp into the configured zone, `shift±Nd`, or a literal date), `cwd`, `shell`, `workspace_roots`,
  `drop`, and `fill_missing`. Absent keys change nothing; every edit is logged and recorded in
  `index.jsonl` as `env_rewrite`, with the rewritten body saved as `.req.out.json`.
* **`docs/configuration-guide.md`** — annotated guide to every key, including the full list of
  terminal values `codex-rs/terminal-detection` can produce and what the persona does with them.

## v0.4.0

* **One directory per codex session** — `<log_dir>/<session-uuid>/<YYMMDD-HHMMSS>-<user|system>-rNNNN.<req|resp>.<role>`,
  so recordings line up with the session-capture files and the thread source (user vs system
  TITLE/RECAP turns) is visible at a glance. Empty artifacts are no longer created.
* **Streaming summaries** — response SSE frames are parsed as they arrive, so no full-response buffer
  is needed; non-SSE bodies are summarized by etag + sha256 (the `/models` payload is skipped by default).
* **`[record]` switches** — every artifact individually, plus `enabled = false` for a pure forwarder,
  and retention (`retention_days`, `gzip_after_days`, `max_total_bytes`).
* **`[limits]`** — configurable request-body cap with spill/reject/stream behaviour, summary buffer
  cap and file write-buffer size.
* **`set_from_env`** — keep swapped credentials out of the TOML file.
* Session-capture events now carry `body_file` / `summary_file` / `response_file` pointers.

See `docs/configuration-guide.md` for the annotated reference.

The annotated reference for every key (including the terminal values codex can produce) is
`docs/configuration-guide.md`.

## `codex-rec probe` — report (and reuse) a machine's codex environment

`probe` is a subcommand; it does not start the proxy:

```bash
codex-rec probe                                        # this machine
codex-rec probe --cwd /root/code --shell bash          # pretend a different cwd / shell
codex-rec probe --client-ua "codex-tui/0.145.0 (Debian 12.0.0; x86_64) tmux/3.3a (codex-tui; 0.145.0)"
```

It prints three blocks:

1. **codex's own local view** — the User-Agent the client builds, `originator`, `?client_version=`,
   the detected terminal (with the raw probes that produced it), shell, cwd, timezone, `current_date`,
   the `<environment_context>` block, and `codex --version` when the CLI is on PATH. Terminal
   detection reproduces `codex-rs/terminal-detection` exactly (`TERM_PROGRAM` first, then
   Ghostty/WezTerm/iTerm2/Apple_Terminal/kitty/Alacritty/Konsole/gnome-terminal/VTE/WindowsTerminal,
   then `TERM`, with tmux/zellij tracked as multiplexers).
2. **a `[persona]` + `[rewrite.environment]` config for this machine** — values that already match the
   host are printed **commented out** (= inherit), values that differ are pinned. Pasting the section
   into another machine's `codex-rec.toml` makes it look like this one.
3. **the same data as JSON**, for scripting.

Typical use: run `codex-rec probe` on the machine whose environment you want, copy the printed config
into the target machine; or pass the printed User-Agent with `--client-ua` when probing a host you
want to report on rather than copy.
