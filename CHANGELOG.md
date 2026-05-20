# Changelog

All notable changes to Circuit Scope are documented here. The format is
loosely based on [Keep a Changelog](https://keepachangelog.com/) and the
project adheres to [Semantic Versioning](https://semver.org/).

## [0.3.0] - 2026-05-20

A meaty release: substantial expansion of the IEEE 1364 subset
`verilog-core` accepts, a brand-new MCP server for agent automation, and
a temporary trim of the in-progress breakpoint debugger so the rest can
ship cleanly.

### Compiler — new language features

- **Concat-LHS in procedural assignments** — `{a, b, cin} = 3'b001;`
  inside `always`/`initial` blocks now lowers to per-component blocking
  (or non-blocking) assignments with the correct bit-slice extraction.
  Nested concats and concats containing bit/part-selects are supported.
- **`generate for` of continuous assigns** — bodies like
  `for (i=0; i<W; i=i+1) begin assign y[i] = a[i] ^ b[i]; end` unroll
  into N continuous assigns with the loop variable substituted.
- **`generate if (cond) … else …`** — condition is const-evaluated against
  module parameters at IR-build time; the chosen branch elaborates
  normally.
- **`generate case (scrutinee) …`** — scrutinee const-evaluated, matching
  arm (or `default`) elaborates.
- **Nested `generate for`** — arbitrary depth of generate-for blocks
  (e.g. `xor_grid #(.R(R), .C(C))`) with shadowing-aware loop-variable
  substitution.
- **`function` / `endfunction`** — bodies are a sequence of blocking
  assignments. Each non-function-name assignment acts as a let-binding
  for subsequent statements; the last assignment to the function name
  supplies the return value. Local `reg`/`integer` declarations are
  accepted (skipped at parse time — let-bindings replace the need for
  storage). Control flow inside function bodies (`if`/`case`/`for`) is
  rejected with a diagnostic — express it as ternaries.
- **`task` / `endtask`** — bodies can be arbitrary `begin … end` blocks
  containing any statements (conditionals, loops, system tasks, nested
  task/function calls). Bodies are substituted with the actual call
  arguments and spliced flat at the call site.
- **`casez` / `casex` with `?`/`x`/`z` wildcards** — lexer accepts
  wildcards inside sized binary/hex literals; each case arm carries an
  optional care-mask; matcher uses `(scrutinee & mask) == (value & mask)`
  for wildcard arms (exact equality preserved for plain `case`).

### New crate: `verilog-mcp`

A Model Context Protocol server that exposes the compiler, VCD viewer,
and stepping debugger over MCP stdio. Links `verilog-core` directly (no
subprocess overhead).

19 tools total, grouped:

- **Read-only / one-shot**: `compiler_info`, `parse_file`,
  `index_project`, `analyze_project`, `find_top_module`, `simulate_vcd`.
- **VCD inspection** (stateless): `vcd_info`, `vcd_query`,
  `vcd_find_edge`.
- **Stepping debugger** (stateful session): `sim_start`,
  `sim_list_sessions`, `sim_state`, `sim_step`, `sim_query_active`,
  `sim_eval`, `sim_eval_many`, `sim_driver_at`, `sim_source_excerpt`,
  `sim_end`.

Install with `claude mcp add verilog-mcp /path/to/verilog-mcp` after
`cargo build --release --manifest-path src-tauri/verilog-mcp/Cargo.toml`.
See `src-tauri/verilog-mcp/README.md` for per-tool documentation.

### Debugger

- **Stepping debugger session** is exposed via both Tauri commands and
  MCP tools: open a session, step by statement / tick / cycle / run,
  query active source spans, evaluate signals at the current or past
  time, jump to the driving statement of a signal.
- **Breakpoints temporarily removed** for this release. The underlying
  feature (line + conditional breakpoints with a small DSL) was midway
  through being wired to the React UI when other compiler work took
  priority; rather than ship a half-finished feature, the breakpoint
  code was lifted out cleanly and the rest of the debugger shipped.
  Search the codebase for `BREAKPOINTS DISABLED FOR 0.3.0` (in
  `src/state/debuggerStore.ts` and
  `src/components/editor/debuggerExtension.ts`) and revert the
  deletions in `verilog-core/src/sim_session.rs`,
  `src-tauri/src/sim_commands.rs`, and `verilog-mcp/src/main.rs` to
  restore. Expected back in a near-term release.

### Tests

- **232 → 237 tests passing** (8 new regression tests for the new
  compiler features and debugger paths; 6 breakpoint tests removed
  with the feature).

### Known limitations

- **Functions with control flow** — `if`/`case`/`for` inside function
  bodies still requires statement-lifting at every call site and is
  not yet supported. Workaround: use ternaries.
- **Combinational sub-module inlining** — the optimiser inlines purely
  combinational instances into their parent, which removes them from
  the VCD hierarchy (output values stay correct). Cosmetic.
- **MCP concurrency** — pipelined `sim_step` requests issued before
  `sim_start`'s response can race with session insertion. Standard MCP
  clients (Claude Code, Claude Desktop) await each response so this
  doesn't bite in practice; documented in the server instructions.

### Project / packaging

- Homebrew cask + formula bumped to 0.3.0; SHA placeholders filled by
  the new `homebrew/bump-shas.sh` helper after the release artifacts
  publish.

## [0.2.2] - 2026-04-21

Homebrew packaging fix; full release notes at the GitHub Release page.

## [0.2.1] and earlier

See git history for incremental changes prior to 0.2.2.

[0.3.0]: https://github.com/um-mepel/circuit-scope-verilog/releases/tag/v0.3.0
[0.2.2]: https://github.com/um-mepel/circuit-scope-verilog/releases/tag/v0.2.2
