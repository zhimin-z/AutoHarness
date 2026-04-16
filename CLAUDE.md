# CLAUDE.md — AutoHarness

## Project overview

Single-binary self-evolving Rust agent. The agent reads its own `src/main.rs`, scores it, asks an LLM for one improvement per iteration, executes tool calls, and persists history. Goal: fewest lines of code that still compiles and runs.

## Build & run

```bash
cargo build --release
./target/release/auto-harness          # run agent loop
./target/release/auto-harness eval     # print score and exit
```

## Key constants (src/main.rs)

| Constant | Value | Meaning |
|---|---|---|
| `SELF_PATH` | `src/main.rs` | File the agent reads and rewrites |
| `HISTORY_PATH` | `.evo/history.json` | Iteration history |
| `MAX_ITERS` | `10` | Iterations per run |
| `PATIENCE` | `3` | Stop early if score doesn't improve for N consecutive iterations |

## Scoring function

```rust
score = 1000.0 / line_count + if compiles { 1.0 } else { 0.0 }
```

Lower line count → higher score. Compiling is a hard bonus.

## Tool protocol

LLM emits plain-text tags, parsed by `run_tool()`:

```
<tool name="shell">cargo build --release 2>&1</tool>
<tool name="write_self">...full file content...</tool>
```

Only one tool per LLM turn. Results are fed back as `<tool_result>...</tool_result>` user messages. Up to 8 turns per iteration.

### write_self safety (atomic write-and-verify)

`write_self` never leaves a broken file on disk:

1. Reject immediately if content is empty
2. Back up current `src/main.rs` to `src/main.rs.bak`
3. Write the new content
4. Run `cargo build --release`
5. If build fails → restore backup, report compiler error to LLM so it can retry
6. If build passes → keep new file, update `.bak`

The LLM sees the full compiler error and can self-correct without human intervention.

After each iteration, if the score drops the agent auto-reverts to `.bak`. If `PATIENCE` consecutive iterations show no improvement, the loop stops early.

## Environment variables

Loaded from `.env` at startup (no external crate), then from the process environment.

| Variable | Default | Notes |
|---|---|---|
| `OPENROUTER_API_KEY` | required | API key |
| `INFERENCE_BASE_URL` | `https://openrouter.ai/api/v1` | Any OpenAI-compatible endpoint |
| `MODEL_NAME` | `anthropic/claude-opus-4` | Model to use |

## API format

OpenAI-compatible `/chat/completions` with `Bearer` auth. Works with OpenRouter, Anthropic, Ollama, vLLM, Together AI, etc.

## Important rules for editing this codebase

- **Do not add dependencies** without a strong reason. Current deps: `ureq`, `serde`, `serde_json` only.
- **Keep `src/main.rs` as the single source file.** No modules, no lib.rs.
- **The agent rewrites its own source** — any change you make will be in scope for the agent to further modify.
- **Test compile before any structural change**: `cargo build --release`
- **History is append-only**: `.evo/history.json` grows each run. Delete it to reset the iteration counter.
- **System prompt uses `concat!`** not raw strings — avoids `r###"..."###` delimiter collisions when the LLM rewrites the file containing the prompt.

## Common tasks

### Reset history
```bash
rm -f .evo/history.json
```

### Check current score without running the agent
```bash
./target/release/auto-harness eval
```

### Use a local model (Ollama)
```env
OPENROUTER_API_KEY=unused
INFERENCE_BASE_URL=http://localhost:11434/v1
MODEL_NAME=llama3
```

### Watch a run
```bash
cargo build --release && ./target/release/auto-harness 2>&1 | tee run.log
```
