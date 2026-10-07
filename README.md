# Ghostwritin'

The engine behind [Ghostwritin'](https://ghostwrit.in): it rewrites AI-assisted drafts into the writer's own voice and checks that the meaning stayed the same. Names, numbers, quotes and code are locked; a rewrite that changes one is retried once and then refused. Every answer carries a word diff (removed, added, locked, same) and the list of locked facts.

Rust, on the [Cratefield](https://github.com/Cratefield/harness) harness. MIT licensed.

> **Status: early, nothing is live.** No Worker is deployed and `api.ghostwrit.in` does not answer. The CLI and the MCP server are not published to crates.io or npm. The **human score is not implemented**: the open build has no detector and returns `null` for both scores. Watermark detection and My voice at scale are hosted features that do not exist yet. Everything below marked *works* runs locally and is covered by tests.

## Open core

Everything in this repository is MIT. The hosted service at ghostwrit.in will add a few things from a separate **private** repository, without forking this one: each of them plugs in through a port (a Rust trait in [`ghostwritin-core::ports`](crates/ghostwritin-core/src/ports.rs)) that has an open implementation here.

| Port | Open implementation (this repo, MIT) | Hosted implementation (private, planned) |
|---|---|---|
| `HumanScore` | `NoHumanScore`: no score, `null` in the API. Not a detector. | a detector ensemble |
| `WatermarkDetector` | `NoWatermarkDetector`: finds nothing | detection for major model families |
| `Quota` | `Unlimited`; `DailyWordLimit` (in memory, per process) | plans and word quotas, billed through Polar |
| `VoiceStore` | `NoVoiceStore` (the Worker's default); `InMemoryVoiceStore` | style summaries per account, encrypted at rest |
| `ApiKeys` | `NoApiKeys`; `StaticApiKeys` (SHA-256 digests from config) | accounts, passkeys and API keys |
| `RequestLogger` | `TracingLogger`; a console logger in the Worker | the hosted log pipeline and usage metering |

The hosted Worker composes the same `RewriteApi` and `HealthApi` modules with its own `Services` value. The engine, the meaning lock, the diff, the prompts, the CLI and the MCP server are the same code in both.

## Crates

| Crate | What it does |
|---|---|
| `ghostwritin-core` | Domain types (`Voice`, `Strength`, `RewriteRequest`, `RewriteResponse`, `Lock`, `DiffSegment`, `StyleSummary`), validation (non-empty, at most 10,000 words), errors with stable codes, and the open-core ports above. No I/O. |
| `ghostwritin-rewrite` | The engine. Splits Markdown into prose and structure, builds the prompt for a voice and strength (and a style summary for My voice), calls the harness `TextModel` port with structured output in chunks of about 600 words, checks the meaning lock per paragraph, retries once with the broken facts named, then builds the diff and the scores. |
| `ghostwritin-voice` | My voice: a style summary from 3 to 5 samples through `TextModel`. Stores the summary only, and refuses a summary that quotes eight or more words of a sample. |
| `ghostwritin-api` | The HTTP API as two Cratefield modules: `POST /v1/rewrite` and `GET /v1/health`. Bearer API keys, problem+json errors, one metadata-only log record per request. |
| `ghostwritin-worker` | The open-source Cloudflare Worker: the API modules on `cratefield-runtime-cloudflare`, with the model chosen from the Worker's secrets. |
| `ghostwritin-cli` | The `ghostwritin` binary: rewrite a file locally with your own model key, keeping code fences and Markdown structure; build a style summary. Also the native HTTP client and model wiring the MCP server shares. |
| `ghostwritin-mcp` | The `ghostwritin-mcp` binary: a stdio MCP server with one `rewrite` tool on the same engine. |

The meaning lock and the word diff are generic, so they live in the harness, not here: `cratefield-text-guard` (find names, numbers, quotes and code; check a rewrite kept them) and `cratefield-text-diff` (word diff with protected ranges kept whole), proposed in [Cratefield/harness#837](https://github.com/Cratefield/harness/pull/837). Until that is merged and released, this workspace pins them to the PR's head commit as a git dependency (see `Cargo.toml`).

## What works today, and what is planned

**Works** (locally, with tests):

- The engine end to end against a scripted model: voices, strengths, chunking, Markdown structure kept, the lock with one retry and then `meaning-changed`, the diff, the locks.
- The API through the harness router: auth, validation, `my_voice` without a summary (403), quotas (429), no model (503), a broken lock (422 with the locks).
- The Worker builds for `wasm32-unknown-unknown` and with `worker-build`, and answered `/v1/health`, `401` and `503 model-not-configured` under `wrangler dev --local` (no model key was used, so no real rewrite went through it).
- The CLI and the MCP server, against the same engine. Not run against a real provider in this repository's tests.

**Not built or not live**: a deployed Worker and `api.ghostwrit.in`; accounts, passkeys and issued API keys; billing and word quotas; a human-score detector; watermark detection; My voice in the hosted service; published CLI and MCP packages; self-hosting documentation beyond this README. Each has an issue.

## The API

```http
POST /v1/rewrite
Authorization: Bearer <key>
Content-Type: application/json

{"text": "…", "voice": "professional", "strength": "edit"}
```

- `voice`: `casual`, `professional`, `academic`, `my_voice` (needs a stored style summary; the open Worker has none and answers 403).
- `strength`: `polish` (keep the sentences, fix what reads as machine-written), `edit` (rework sentences, keep each paragraph's points in order), `rewrite` (write each paragraph again from the same facts).

```json
{
  "rewrite": "…",
  "score_before": null,
  "score_after": null,
  "diff": [{"op": "removed", "text": "Furthermore, "}, {"op": "locked", "text": "Dana Okafor "}, {"op": "same", "text": "said "}],
  "locks": [{"kind": "name", "text": "Dana Okafor"}, {"kind": "number", "text": "18%"}]
}
```

Errors are `application/problem+json` with a stable `code`: `invalid-request`, `empty-text`, `too-many-words` (over 10,000), `unauthorized`, `voice-unavailable`, `quota-exceeded`, `meaning-changed` (with `locks`: the facts the model would not keep), `model-not-configured`, `model-unavailable`, `model-rejected`, `model-output`.

### The meaning lock

A paragraph's protected spans are found by `cratefield-text-guard`: fenced and inline code; text in double quotes (`"…"`, `“…”`, `„…“`, `«…»`); numbers with their currency, percent sign and glued suffix (`$4.2M`, `18%`, `Q3`, `FY2024`); and two or more capitalised words in a row (`Dana Okafor`). Each must appear in the rewrite of that paragraph verbatim, at a word boundary; for a quote, the quoted words must (the quote marks may change style, closing punctuation may move). A number the rewrite adds is a violation too. The heuristics are simple on purpose and their limits are documented in the crate (a capitalised verb before a name, "Ask Dana", locks both words; spelled-out numbers are not locked).

## CLI

```sh
export ANTHROPIC_API_KEY=…          # or OPENAI_API_KEY (+ OPENAI_BASE_URL for a compatible endpoint)
cargo run -p ghostwritin-cli -- rewrite draft.md -o final.md --voice casual --strength edit --diff
cargo run -p ghostwritin-cli -- voice learn a.md b.md c.md -o style.txt
cargo run -p ghostwritin-cli -- rewrite draft.md --voice my_voice --style style.txt
```

Code fences, indented code, front matter, headings, tables, HTML and link definitions are copied through untouched; paragraphs, list items and block quotes are rewritten (list and quote markers kept). `--json` writes the whole answer. Text goes only to the provider whose key is set.

## MCP server

```sh
cargo build --release -p ghostwritin-mcp
claude mcp add ghostwritin -- /path/to/target/release/ghostwritin-mcp   # with a model key in the environment
```

One tool, `rewrite` (`text`, optional `voice` and `strength`), returning the rewrite as text and the full answer as structured content.

## Self-hosting the Worker

Untested end to end with a real model; the steps are what the code expects.

```sh
cd crates/ghostwritin-worker
../../tools/api-key.sh me                     # prints a key once; the line it outputs is the config entry
npx wrangler secret put GHOSTWRITIN_API_KEYS  # me=<sha256>, comma-separated for several
npx wrangler secret put ANTHROPIC_API_KEY     # or OPENAI_API_KEY (+ OPENAI_BASE_URL as a var)
npx wrangler secret put HARNESS_SECRET        # 32+ random bytes, for log pseudonyms
PATH="$HOME/.cargo/bin:$PATH" npx wrangler deploy
```

## Data policy, and where the code enforces it

Drafts, rewrites, diffs and scores are never stored or logged. Logs hold metadata only. My voice keeps the style summary, never the samples.

- **Nothing persists text.** No crate has a database or file write for drafts or rewrites; the engine (`crates/ghostwritin-rewrite/src/lib.rs`) holds text in the call's memory and returns it. The only store is `VoiceStore`, which takes a `StyleSummary` type, not samples.
- **One log record, with no field for text.** `RequestLog` (`crates/ghostwritin-core/src/ports.rs`) has account, word count, voice, strength, status and outcome code; that is all any logger can write. The API writes one per request (`crates/ghostwritin-api/src/lib.rs`), and a test checks the records (`crates/ghostwritin-api/tests/routes.rs`). The harness's own request log line is metadata too (route, status, hashed IP).
- **Errors carry no text.** `GhostwritinError`'s messages are fixed strings with counts (`crates/ghostwritin-core/src/error.rs`); a JSON parse failure answers `invalid-request` without the parser's message (which can quote input); provider errors, which can quote the prompt, are mapped to codes and their text dropped (`model_error` in the engine).
- **`Debug` hides text.** `RewriteRequest`, `RewriteResponse` and `StyleSummary` print counts, not content, so a stray `{:?}` in a log is safe (tested in `crates/ghostwritin-core/src/types.rs`).
- **Samples are dropped.** `VoiceBuilder::learn` takes the samples by value, stores the summary only, and a summary that quotes eight or more words of a sample is refused (`crates/ghostwritin-voice/src/lib.rs`).
- **Locally, text goes only to your provider.** The CLI and the MCP server call the provider whose key is set, directly.

To rewrite, text is sent to a language model provider, which may keep API requests for a limited time under its own terms. Which provider the hosted service uses will be named before launch.

## Checks

The gates, as CI runs them:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --workspace
cargo build --target wasm32-unknown-unknown -p ghostwritin-worker
```

## License

MIT, copyright Factory Zero Pte. Ltd. See [LICENSE](LICENSE).
