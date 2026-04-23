# Agent Orchestration Patterns

## Tools available in chat mode

| Tool | Purpose |
|---|---|
| `shell` | Run any shell command; output capped at 2000 chars |
| `write_file` | Write any file |
| `spawn_agent` | Launch an async sub-agent in a background thread |
| `wait_agent` | Block until a spawned sub-agent finishes |

## spawn_agent format

```
<tool name="spawn_agent">output_filename.md
Full self-contained task description. Include goal, file paths, snippets,
constraints, and the exact success criterion.
</tool>
```

The runtime returns: `spawned agent_id=agent_<ts> output_path=<dir>/<file>`

## wait_agent format

```
<tool name="wait_agent">agent_<ts></tool>
```

Returns when the sub-agent finishes. Read the output file afterward with `shell`.

## When to spawn a sub-agent

Spawn when ALL of these hold:
- Sub-task has a clear, bounded output (a file with a known path)
- Sub-task is independent — no data dependency on another in-flight agent
- Sub-task is large enough to justify a separate context (> ~10 lines of non-trivial work)

Do NOT spawn for:
- Trivial lookups or < 5-line outputs — do inline
- Tasks whose result is needed before any other work can start — do inline
- Tasks that require access to the current conversation history — sub-agents start fresh

## Parallel fan-out pattern

When N sub-tasks are independent, spawn all N before waiting for any:

```
Turn 1: spawn_agent → agent_A ack
Turn 2: spawn_agent → agent_B ack
Turn 3: spawn_agent → agent_C ack
Turn 4: wait_agent agent_A → done; read output
Turn 5: wait_agent agent_B → done; read output
Turn 6: wait_agent agent_C → done; synthesise
```

Never spawn-and-immediately-wait in the same sub-task — that is just slow sequential.

## Context handoff discipline (sub-agent starts fresh)

Every spawn_agent body must be fully self-contained:
- State the goal in one sentence
- Include relevant file paths and key snippets (< 200 lines total)
- State hard constraints explicitly (e.g. "no new deps", "keep LOC < 500")
- State the success criterion: what command proves the task is done?
- State the exact output file path the sub-agent must write to

Never reference "the work above" or "what we discussed" — the sub-agent has no parent memory.

## Output contract (survives context compaction)

- Sub-agent MUST write results to the specified output file before finishing
- Parent reads the file with `shell cat <path>`, not the wait_agent summary
- File naming: use the `output_filename.md` first line of the spawn_agent body

## Effective task decomposition

Good decomposition: each slice has a single owner, a single output file, and a clear done state.

| Pattern | Use when |
|---|---|
| Fan-out: N parallel agents | N files to analyse, N endpoints to write, N tests — no ordering |
| Chain: A → B → C | Each step needs the previous result (plan → implement → review) |
| Planner + Executor | Task large enough that design and implementation are separable |
| Executor + Reviewer | Correctness or security matters more than speed |

## Verification discipline

- Every sub-agent task must include a verification command in the prompt
- Sub-agent runs it and records `PASS` / `FAIL` + exact output in the result file
- Parent checks for `PASS` before accepting the sub-agent's work
- If `FAIL`, parent re-briefs the sub-agent with the error — does not fix inline

## Scope discipline (WIP = 1)

- One active logical task per agent at a time
- If sub-agent discovers a second problem, it logs it in the output file and stops
- Parent decides whether to spawn a follow-up agent for the logged issue

## Session continuity

- Before context fills: write PROGRESS.md (done / in-progress / blocked / next step)
- Design decisions go in DECISIONS.md with rationale
- A fresh session must answer "what is this?", "how do I run it?", "what's next?" from repo alone
- Rebuild cost target: < 3 minutes from cold start to executable state

## Anti-patterns to avoid

- Spawning a sub-agent then immediately waiting — use inline work instead
- Spawn prompt that says "see previous context" — sub-agent starts fresh every time
- Accepting "output file written" as done without reading and verifying its contents
- Spawning more agents than there are independent sub-tasks — merging is not free
- Long spawn prompts that bury the success criterion — put it in the first 3 lines
