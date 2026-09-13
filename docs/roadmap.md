# codex-rec roadmap / change backlog

Status legend: **DONE** · **AGREED** (decided, not implemented) · **OPEN** (needs a decision) · **DECLINED**

> **All batches below landed in v0.4.0** (A1–A7, B1–B6, C1–C3, D1–D5 as far as they apply).
> What is left is only operations: deploying v0.4.0 on the landing box and the cleanup.

## Context

What the recorder writes today (flat, `<log_dir>/`): `index.jsonl`, `req-<stamp>.hdr`,
`req-<stamp>.out.hdr`, `req-<stamp>.body` (raw bytes as received, zstd for POSTs),
`req-<stamp>.json` (decoded), `req-<stamp>.summary.json`, `resp-<stamp>.hdr`, `resp-<stamp>.sse`
(verbatim response body, streamed), `resp-<stamp>.summary.json`, and — with `--rewrite-catalog` —
`resp-<stamp>.catalog.json`. Session capture (opt-in) writes `sessions/ss-<date>-<uuid>-<title>.jsonl`.

Already shipped: **v0.3.0** (session capture, glob header matching, Windows release as zip, HTTP/SSE
`data:` frame parsing) and **v0.3.1** (persona keys inherit when absent, `""` only clears `terminal`,
`[routes]` path mapping, config validation errors). WebSocket pass-through is **DECLINED** by the
user (HTTP/SSE is enough for the current traffic).

## Fingerprint finding (2026-09-12)

The external tool **ModelTrace** (https://github.com/xqy2006/ModelTrace — its GPT fingerprints were
sampled from the *official Codex subscription*, i.e. the same channel) attributed our recorded probe
answers as follows: session `01a097d1` = **gpt-6-astra** for its first three probes and
**gpt-5.6-luna** for its *fourth* probe; session `01a09739` = astra for the first answer and luna for
the second; most other sessions = luna. The substitution is **silent**: both the request body and the
response `model` field always said `gpt-6-astra`, and `x-models-etag` never changed. Follow-up
tooling inside the recorder (auto-attribution, channel A/B) was offered and **declined**, so the plan
below concerns codex-rec itself only.

## A. Recording layer (naming and size)

* **A1 — file naming (AGREED: one directory per session).**
  `<log_dir>/<codex-session-uuid>/<YYMMDD-HHMMSS>-<user|system>-rNNNN.<req|resp>.<role>[.ext]`,
  e.g. `…/01a097d1-19b4-79d2-a2a2-fb5dc004dbae/260912-231425-user-r0001.req.out.hdr`.
  The directory carries the **full codex session uuid** so it lines up with
  `sessions/ss-<date>-<uuid>-<title>.jsonl`; `user|system` is the thread source (TITLE/RECAP run as
  system threads); `rNNNN` is the per-session request sequence, reused by the matching response;
  requests without a `session-id` header go to `_nosession/`.
* **A2 (AGREED)** — do not create empty files: GETs currently produce 0-byte `req-*.body`/`.json`.
* **A3 (AGREED)** — when the request body was *not* compressed, `.body` is a byte-identical copy of
  `.json`; keep only `.json`.
* **A4 (AGREED)** — store the rewritten body (`req.out.json`) when `--drop-body-key` /
  `--set-body-key` changed it; today only the outgoing *headers* are recorded.
* **A5 (AGREED)** — for `/models` store only the etag/hash (or the payload only when it changed):
  every poll currently writes ~392 KB.
* **A6 (AGREED)** — retention: `retention_days`, `max_total_bytes`, gzip of finished files.
* **A7 (AGREED)** — every artifact gets its own config switch, see section D.

## B. Memory and buffering

* **B1 — request body buffer (the 64 MiB one).** Today `axum::body::to_bytes(body, MAX_BODY)` buffers
  a whole request in memory (64 MiB cap). Make the cap **configurable, default 4 MiB**, and define
  what happens above it — this is the case the user is worried about ("a request slightly over the
  limit and our implementation has a bug"):
  1. *spill to a temp file* (recommended: memory stays bounded, nothing is lost, the request is
     still forwarded normally),
  2. reject with `413` (explicit, but breaks the client),
  3. stream the body through and record only a prefix/hash (needed for the pure-forwarder mode).
  Tests to write: body exactly at the limit, limit+1, limit+1 chunk, empty body, `limit = 0`.
* **B2 — response summary buffer.** The in-memory copy of a response stream is capped at
  `MAX_ACC = 32 MiB` and the current code **overshoots the cap by up to one read chunk**
  (`if acc.len() < MAX { extend(chunk) }`). Either truncate strictly
  (`let n = (max - len).min(chunk.len())`) or remove the buffer entirely via B3. Configurable
  (`limits.summary_buffer_bytes`, default 4 MiB, `0` = keep nothing). Truncation can cut the last
  SSE frame; the parser already skips unparsable lines, but the summary then loses the tail.
* **B3 — streaming summary parser (recommended).** Parse SSE/JSON frames incrementally while
  forwarding, so no full-response copy is needed at all; also fixes "summary truncated for very
  large responses". Makes B2 an optional fallback.
* **B4 — file write buffer.** Optional `BufWriter` around the response file
  (`limits.write_buffer_bytes`, default 256 KiB, `0` = write every chunk straight through). This is
  a different knob from B1/B2.
* **B5 — session title map.** `Sessions.titles` keeps one entry per session forever; add a cap/eviction.
* **B6 — pure forwarder.** With recording disabled (D1), stop buffering request bodies: stream them.

## C. Summary / content

* **C1 (OPEN)** — make session capture self-contained (`session_full`) or add `body_file` pointers to
  the full request/response files (pointer preferred: keeps session files small).
* **C2 (AGREED)** — add `instructions_source: "item" | "field"` to summaries; responses-lite turns
  carry the system prompt as an input item, which is why `instructions_len` reads `0`.
* **C3 (AGREED)** — `index.jsonl` should log the incoming path as well as the outgoing one.

## D. Configuration switches (router / "整流器" mode)

* **D1 (AGREED)** — `[record] enabled = false`: nothing is written at all (persona, header and path
  rewriting still apply). This is the "pure forwarder" mode.
* **D2 (AGREED)** — per-artifact booleans, all defaulting to `true`: `index`, `request_headers`,
  `request_headers_out`, `request_body_raw`, `request_body_json`, `request_summary`,
  `response_headers`, `response_stream`, `response_summary`.
* **D3 (AGREED)** — disabling an artifact must also skip its work (e.g. no `zstd` decode when
  `request_body_json` is off; never touch the response body when `response_stream` and
  `response_summary` are off).
* **D4** — `session_capture` keeps its own switch (already off by default).
* **D5 (OPEN)** — `set_from_env`: let `[headers.request].set` pull values from environment variables
  so swapped credentials never sit in plain text in the TOML file.

## E. Operations

* Deploy **v0.3.1** on the landing box (linux-musl, sha256 `51f823b3…`); the artifact has to be
  downloaded by hand (private repo, no token available here).
* Clean up `/root/sesstest-*`, `/root/tokenswap*`, `/root/codex-rec-2`, `/root/codex-rec-0145.toml`.
* Optional: `auto_recap = false` in the client config (saves quota and recording noise; unrelated to
  the persona).

## F. Observability — corrections from the fingerprint investigation

* **F1 (correction: do not rely on it)** — logging the *served* model would **not** have caught the
  swap: the response `model` field said `gpt-6-astra` even while the fingerprint said `gpt-5.6-luna`.
  Only output-fingerprint attribution detects a silent substitution, and that lives outside this
  recorder. Recording `x-codex-safety-buffering-*` keeps some audit value, but a matching model name
  is not proof.
* **F2 (cancelled)** — the "quality probe harness" idea is superseded by the external ModelTrace
  attribution, and further tooling here was declined by the user.

## G. Suggested implementation batches

1. **Batch 1 (small, recording layer only):** A1, A2, A3, C2, C3 + the `[record]` master switch (D1)
   and enough of D2 to make A2/A3 meaningful.
2. **Batch 2 (memory):** B1 (configurable request cap + spill), B2 (strict truncation), B4, B5.
3. **Batch 3 (size and lifetime):** A4, A5, A6, B3, B6, D5, C1.
