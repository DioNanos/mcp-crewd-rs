---
name: crew
description: Use when spawning or coordinating worker AI cells through the crewd cell-fabric MCP server (cell_spawn, cell_status, cell_result, cell_list, and the cell_send/cell_ask bus). Covers the spawn→poll→result procedure, the thread-state machine, engine/profile selection, and the known runtime behaviours (no completion push, shim reconnect, empty codex final answer) so a coordinator drives worker cells correctly instead of guessing.
---

# crew — driving worker cells over the crewd fabric

`crewd` is a cell-fabric daemon: one AI session (the *coordinator*) spawns and
coordinates other AI worker sessions (*cells*) through a single MCP server over
a local Unix socket. This skill is the operational layer the tool schemas do not
carry — the procedure and the gotchas.

## Mental model

- A **cell** is a worker AI session. Its **identity is its working directory**:
  the spawned engine loads whatever `CLAUDE.md`, `.mcp.json`, memory and skills
  live in the `cwd` you pass. One daemon serves many personas by spawning in
  different project roots.
- **Engines**: `claude` (Agent SDK shim), `codex` (`codex app-server`), `pi`.
- **Profiles** (claude): `max` (host credentials) or any profile declared in
  `crewd.toml [profile.<name>]`. Passed as a free string to `cell_spawn`.
- Spawning is **fire-and-forget**: `cell_spawn` returns a `crewd_thread_id`
  immediately and the turn runs in the background.

## The core loop: spawn → poll → result

1. **Spawn**

   `cell_spawn { engine, profile?, cwd, task, idempotency_key, mode:"background" }`
   → returns `{ crewd_thread_id, replayed }` at once. Always pass a unique
   `idempotency_key` (re-spawning with the same key returns the same thread,
   `replayed:true`).

2. **Poll — there is NO completion push.** The coordinator MUST poll; a finished
   worker notifies nobody. Call `cell_status { crewd_thread_id }` on an interval
   and watch `state`:

   | state | meaning |
   |-------|---------|
   | `spawning` | accepted, engine starting |
   | `running` | turn in progress |
   | `idle` | **turn completed** (engine went down) — this is the normal "done" |
   | `timeout` | the turn exceeded the per-turn timeout (may be **partial**) |
   | `failed` / `interrupted` / `failed_unknown` | turn/engine failure |

   **Gotcha:** poll for the state *leaving* `running`/`spawning`. Do **not** wait
   for a state literally named `finished`/`succeeded` — a normal completion
   surfaces as `idle`, and a slow worker as `timeout`. A poller that only matches
   `finished` never fires.

3. **Result**

   `cell_result { crewd_thread_id }` → `{ exit_status, final_answer, event_tail, … }`.

## Known runtime behaviours (verify, don't assume)

- **Empty codex final answer.** A `codex` cell can report `exit_status:"done"`
  with an **empty `final_answer`**. The real output is in the codex rollout
  session file's `task_complete.last_agent_message`. If you need a codex worker's
  answer and `final_answer` is empty, recover it from the rollout, or have the
  worker write its result to a file in the repo and read that file instead.
- **Shim does not auto-reconnect.** If the daemon restarts, an already-mounted
  MCP shim breaks (`Broken pipe`) and does **not** reconnect for the rest of the
  session. Drive a fresh `crew mcp` shim per call instead — see
  `scripts/crew-mcp-driver.py` in this skill for a minimal JSON-RPC driver.
- **GLM / third-party-endpoint profiles are slower and can time out mid-turn**,
  leaving the worker's edits **partial and possibly uncompilable** (edits are not
  atomic). Delegate *bounded, mechanical, or research* work to them and **verify
  and finish the result yourself**; do not hand a delicate multi-file refactor to
  such a worker without a safety net (compile/test after, be ready to fix).
- **Concurrency races.** Two code-mutating cells in the *same* working tree race
  on files and the build lock. For parallel code work, spawn with worktree
  isolation (a per-cell git worktree) so their edits don't collide.

## Engine / profile selection

- Bounded research, drafting, mechanical edits → a fast profile cell; then
  review its output.
- Adversarial audit / review → a `codex` cell (recover its answer per above).
- Delicate, correctness-critical TDD → keep it on the coordinator, or delegate
  with a strict spec + a mandatory verify pass.

## Standing discipline

**Every time you use the crew fabric, observe how the spawns actually behave —
states, timing, result capture, failures — and record anything worth improving.**
The fabric is pre-1.0; its rough edges (state names, result capture, reconnect,
timeouts) are exactly where the next improvement comes from.

## Bus (cell-to-cell messaging)

Beyond spawn/poll: `cell_send` (fire-and-forget), `cell_ask` + `cell_await`
(ask ticket + long-poll reply), `cell_reply`, `cell_broadcast`, `cell_inbox`.
Use `cell_ask`/`cell_await` when you genuinely need a worker to hand a result
back through the bus rather than polling its thread.
