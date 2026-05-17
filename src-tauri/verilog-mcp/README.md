# verilog-mcp

A **Model Context Protocol** server that exposes Circuit Scope's Verilog
compiler, VCD viewer, and stepping debugger over MCP stdio. Any MCP client
— Claude Code, Claude Desktop, Cursor, etc. — can use it to drive the
compiler programmatically: parse, simulate, query waveforms, set
breakpoints, step source-level.

This crate links `verilog-core` directly (no subprocess overhead) and
mirrors the same APIs that Circuit Scope's Tauri shell uses
(`src-tauri/src/sim_commands.rs`, `src-tauri/src/vcd_viewer.rs`).

## Build

```bash
cargo build --release --manifest-path src-tauri/verilog-mcp/Cargo.toml
```

The binary lands at `src-tauri/verilog-mcp/target/release/verilog-mcp`.

## Install in Claude Code

```bash
claude mcp add verilog-mcp /absolute/path/to/src-tauri/verilog-mcp/target/release/verilog-mcp
```

Or hand-edit `~/.claude/settings.json`:

```json
{
  "mcpServers": {
    "verilog-mcp": {
      "command": "/absolute/path/to/src-tauri/verilog-mcp/target/release/verilog-mcp",
      "args": [],
      "env": {}
    }
  }
}
```

Restart Claude Code. The 15 tools below appear under the `verilog-mcp:` prefix.

## Tools

All paths must be absolute. Stdout is reserved for JSON-RPC — set
`RUST_LOG=verilog_mcp=debug` for stderr diagnostics.

### Read-only / one-shot

| Tool | Purpose |
| --- | --- |
| `compiler_info` | Linked `verilog-core` version. |
| `parse_file { path }` | Parse one file → `{ modules, diagnostics }`. |
| `index_project { root }` | Walk `root`, return every module + path. |
| `analyze_project { root }` | Per-module ports/nets/instances/assigns + auto-detected top modules. |
| `find_top_module { root }` | Preview the top module `sim_start` / `simulate_vcd` would auto-pick. |
| `simulate_vcd { root, cycles?, output_file? }` | One-shot compile + simulate → writes `circuit_scope.vcd` (or chosen name), returns absolute path. |

### VCD inspection (stateless — reopens the file each call)

| Tool | Purpose |
| --- | --- |
| `vcd_info { path }` | Time range, timescale, full scope/signal hierarchy. Leaf signals carry `signalId`. |
| `vcd_query { path, signal_ids, t_start, t_end, max_points_per_signal? }` | Transitions in `[t_start, t_end]`. Decimated to `max_points_per_signal` (default 65536). |
| `vcd_find_edge { path, signal_id, from_time, next, edge_kind, bit_lsb? }` | Next/previous `any | rising | falling` edge, returns matching time or `null`. |

### Stepping debugger (stateful — session lifecycle)

```
sim_start  →  sim_step  ⇄  sim_query_active / sim_eval / sim_eval_many / sim_driver_at / sim_state / sim_source_excerpt  →  sim_end
```

| Tool | Purpose |
| --- | --- |
| `sim_start { project_root, top_module?, num_cycles?, vcd_filename? }` | Elaborate, optimise, start the simulator. Returns `{ sessionId, vcdPath, sourceFiles, timeFs }`. |
| `sim_list_sessions {}` | Every active session id + top module + current time + vcd path. Use after a context loss to recover ids. |
| `sim_state { session_id }` | Metadata for one session without stepping. Returns the same shape as `sim_start` plus `numCycles`. |
| `sim_step { session_id, mode, clock_signal? }` | Advance. `mode` ∈ `statement | tick | cycle | run`. Returns `{ timeFs, activeSpans, ranStatements, done }`. *Breakpoints are temporarily disabled in 0.3.0; restore from git history when re-enabled.* |
| `sim_query_active { session_id, time_fs }` | Source spans active at `time_fs` without stepping. |
| `sim_eval { session_id, identifier, file_path?, time_fs? }` | Live signal value. Tries identifier as-typed, then `top.identifier`, then `module.identifier` for every module in `file_path`. Returns decimal/hex/binary + transitioning flag. |
| `sim_eval_many { session_id, identifiers, file_path?, time_fs? }` | Batch eval — returns an object map of `identifier -> result or null`. Cuts round-trips for watch lists. |
| `sim_driver_at { session_id, signal, time_fs }` | Which statement is driving `signal` at `time_fs`. Returns source file + byte span. |
| `sim_source_excerpt { path, start, end, context_lines? }` | Read source text at a byte span (returned by `sim_query_active` / `sim_driver_at`). Optional surrounding context lines for editor-style display. |
| `sim_end { session_id, keep_vcd? }` | Drop the session. `keep_vcd` defaults to `true` (VCD preserved for further query). |

#### Concurrency note

Tool dispatches run on independent async tasks. Clients **must await each
response before issuing the next request** — pipelining `sim_step` before
`sim_start`'s response can race with session insertion. Claude Code and other
well-behaved MCP clients already do this; only matters if you write a custom
client.

## Notes

- `verilog-mcp` is **not** a workspace member — build with `--manifest-path`
  like `verilog-core`. Path dep on `../verilog-core`; rebuild from scratch
  if the core crate changes.
- The stepping debugger is *interactive*: it owns a stateful
  `verilog_core::SimSession`, so a `sim_start` on a 50k-line project will
  take a few seconds (elaborate + optimise). Subsequent `sim_step` calls
  are cheap.
- VCD query tools accept any absolute path the binary has read access to.
  No project-root check is enforced — the model is "agent has shell
  access already; don't add friction".
- Latency: an `index_project` call on the Circuit Scope test fixtures
  (~10 modules) returns in <50 ms; `sim_start` on the same fixtures is
  ~200 ms.

## License

MIT (same as Circuit Scope and `verilog-core`).
