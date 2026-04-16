# AutoHarness

A self-evolving coding agent in Rust — the smallest possible implementation that actually works.

<img width="1200" height="800" alt="AutoResearch" src="https://github.com/user-attachments/assets/805635cc-88d4-4f26-9467-07ef8ca99b7b" />

The agent reads its own source code, asks an LLM to improve it, executes the proposed changes, scores the result, and repeats. Over time it tries to make itself shorter, more correct, and more capable.

## How it works

```mermaid
flowchart TD
    A[read src/main.rs] --> B[score: 1000/lines + compile_bonus]
    B --> C[send source + score to LLM]
    C --> D{LLM tool call}
    D -->|shell| E[run shell command]
    D -->|write_self| F[backup → write → cargo build]
    F -->|build fails| G[restore backup\nreport error to LLM]
    G --> C
    F -->|build passes| H[keep new file]
    E --> I[feed result back to LLM]
    H --> I
    I --> J[rescore + persist to .evo/history.json]
    J --> A
```

### Scoring

```
score = 1000 / line_count          (fewer lines = higher score)
      + 1.0   if cargo build passes (compile bonus)
```

The LLM is incentivised to compress the source while keeping it compiling.

### Tool dispatch

The LLM emits plain-text XML-like tags — no framework, no function-calling schema:

```
<tool name="shell">cargo test 2>&1</tool>
<tool name="write_self">...full new src/main.rs...</tool>
```

The agent parses these with string search and feeds the result back as the next user message.

`write_self` is atomic: the agent backs up the current file, writes the new code, runs `cargo build --release`, and automatically restores the backup if the build fails — reporting the compiler error back to the LLM so it can self-correct.

### History

Each iteration's score is saved to `.evo/history.json`. On restart the agent resumes where it left off (iteration counter continues from the last saved entry).

## Installation

```bash
# Install Rust (if not already installed)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source $HOME/.cargo/env

# Clone the repository
git clone https://github.com/Engineering4AI/AutoHarness
cd AutoHarness

# Set API key
echo "OPENROUTER_API_KEY=sk-or-..." > .env

# Build and run
cargo build --release
./target/release/auto-harness          # run the agent loop (10 iterations)
./target/release/auto-harness eval     # print current score and exit
```

Any OpenAI-compatible endpoint works (Ollama, vLLM, Together, etc.):

```bash
export OPENROUTER_API_KEY=anything
export INFERENCE_BASE_URL=http://localhost:11434/v1
export MODEL_NAME=llama3
```

## File layout

```
.
├── Cargo.toml              # ureq + serde + serde_json
├── src/
│   ├── main.rs             # the entire agent (~270 lines)
│   └── main.rs.bak         # last known-good version (auto-created)
├── .env                    # API keys (not committed)
└── .evo/
    └── history.json        # iteration scores (auto-created)
```

## Configuration

| Variable | Default | Description |
|---|---|---|
| `OPENROUTER_API_KEY` | — | OpenRouter key (required) |
| `INFERENCE_BASE_URL` | `https://openrouter.ai/api/v1` | Any OpenAI-compat base URL |
| `MODEL_NAME` | `anthropic/claude-opus-4` | Model identifier |

`MAX_ITERS` (default `10`) and `PATIENCE` (default `3`) are compile-time constants in `src/main.rs`.

## What happens on each run

```
[evo] starting at iteration 1 / 10
[iter 1] I'll refactor the tool parser to reduce line count.
<tool name="write_self">...</tool>
  -> write_self: written and verified OK
[evo] iter=1 score 4.846->5.102
[evo] done. 1 total iterations.
```

If a rewrite lowers the score, the agent automatically restores `src/main.rs.bak`. If patience (3 consecutive non-improving iterations) is exhausted, the loop stops early.

## Citation

If you use AutoHarness in your research, please cite:

```bibtex
@software{autoharness2026,
  title  = {AutoHarness: A Self-Evolving Coding Agent in Rust},
  author = {Zhao, Zhimin},
  year   = {2026},
  url    = {https://github.com/Engineering4AI/AutoHarness}
}
```
