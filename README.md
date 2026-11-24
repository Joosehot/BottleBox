# Bottle Box

Bottle Box splits one task across many Claude Code agents ("minions") that
work at the same time in one shared workspace (the Box). It plans, starts
and coordinates them, merges what they write and tests it. Its main use is
writing code; it can also drive Bottle Clip (a separate project that
watches videos and cuts montages) to edit a video.

On the benchmark below (ten small Rust modules), the current version
finished in **44.7 s against 92.7 s** for five ordinary Claude Code agents
working in parallel (**2.07× faster**), with **7× fewer tokens** and **less
than half the cost**, all tests passing in both. Both sides ran Claude
Opus 5.5.

```
bottlebox code --task-file bench/coding10.txt --dir <empty folder> --bench
```

## Contents

- [How it works](#how-it-works)
- [The Box: shared memory, registry, board](#the-box-shared-memory-registry-board)
- [How the agents are run](#how-the-agents-are-run)
- [Prompt caching](#prompt-caching)
- [Benchmarks](#benchmarks)
- [What the benchmarks taught](#what-the-benchmarks-taught)
- [Usage](#usage)
- [Logs and what is measured](#logs-and-what-is-measured)
- [Limitations](#limitations)
- [Layout](#layout)

## How it works

The default mode for code (`load`, in `src/load.rs`) has five steps.

1. **Split and glue, without an agent.** Bottle Box reads the task's
   numbered items; each becomes a module of one crate (`src/<module>.rs`
   and its tests `tests/<module>.rs`). Bottle Box writes `Cargo.toml` and
   `src/lib.rs` itself.
2. **Think together: one director per task, all at once.** Each director
   decides its whole task in one mind: the public API (exact Rust
   signatures, registered in the Box), the conventions (names, formats,
   error values), a few test cases with exact answers written as calls of
   that API, and the size of the implementation. A big implementation is
   split into two or three sections, each a separate set of items.
3. **Minions by workload.** A small task (about 70 lines or less) gets one
   minion for both files; a usual task gets two (implementation, tests); a
   big one gets a minion for each section and one for the tests. At most 20
   minions in all, and only as many as the work needs.
4. **Streaming.** A task's minions start the moment its director finishes,
   not when every director has finished. Each task is tested on its own as
   soon as its files are written (in a small crate holding only that
   module), while the others are still being written. A failing task goes
   with its error to one minion that owns that task's files; nothing else
   waits for it.
5. **Merge and test.** Every task lives on its own git branch in its own
   worktree, so no two minions ever write the same file. The branches are
   merged in pairs, like a tournament (10 branches: 4 rounds), every pair of
   a round at once. Then the whole crate is built and tested.

Other modes kept for comparison: `--hive` (20 minions, a director for each
file), `--director` (one director plans everything, then 20 minions), and
the `clip` driver for Bottle Clip.

## The Box: shared memory, registry, board

`src/shared.rs`. One folder per run (`runs/<run>/box`), safe to write from
many processes, and copied into Strike's Nimbus store at the end.

- **Registry ("who built what")**: one file per item (a function, a type, a
  helper), created with create-new, so the first claim wins and anyone else
  is told who has it. Directors register every public item before any code
  is written.
- **Board**: short thoughts, decisions and test cases, append only.
- **Notes**: decisions made along the way.

The Box is also served to agents as MCP tools (`bottlebox box-mcp`):
`registry_lookup`, `registry_claim`, `board_post`, `board_read`. The
current mode needs no tool calls during building: each minion is given the
part of the Box it needs.

## How the agents are run

Every agent is a headless Claude Code process (`claude -p`), running on the
owner's Claude subscription. Bottle Box takes `ANTHROPIC_API_KEY` and
`ANTHROPIC_AUTH_TOKEN` out of every agent's environment, so no run is ever
billed to an API key. The dollar figures in this README are what the tokens
would cost at API prices, a measure of how much of the plan's limits a run
takes, not money charged.

A minion runs with the shared context as its system prompt
(`--system-prompt-file`), no built-in tools unless its piece needs them, no
user settings, plugins, hooks, skills or MCP servers
(`--setting-sources project --disable-slash-commands --strict-mcp-config`),
a set effort, and a cap on its output. It starts fast and reads little.

A rate limit is not a failure: an agent waits (5 s, doubling up to five
minutes) and asks again, as often as it takes.

## Prompt caching

Claude Code places the cache breakpoints itself, after the system prompt
and after the last message; `claude -p` gives no way to add one. A stock
Claude Code agent reaches its high cache-read share by reusing one
conversation over many turns: each turn reads the whole conversation so far
from the cache and writes only what is new.

A minion that is a single fresh call cannot do that: the part every minion
shares sits between the two breakpoints, so each call wrote it again
(13 % of the input read from the cache). Bottle Box therefore reuses
conversations the same way:

- one **warm session** holds the shared prefix (the rules and the whole
  task);
- every **director** goes on from it (`--resume <session> --fork-session`);
- every **builder** goes on from its task's director, so the director's plan
  is already in its conversation;
- a **fix** goes on from the task's first builder.

Every conversation only grows at its end. All agents use the same effort,
because changing the effort in the middle of a conversation empties its
cache. This raised the cache-read share from 13 % to 73 %, and cut the
tokens written to the cache from 134 000 to 47 000.

Claude Code's own `total_cost_usd` counts a resumed session's earlier turns
again (a director's figure was exactly its own tokens plus the warm
session's), so Bottle Box computes every cost from the tokens instead
(`Usage::cost`, Claude Opus 5.5 prices).

## Benchmarks

### The frozen baseline

Every benchmark compares Bottle Box with **five stock Claude Code agents
working in parallel**, the tasks split evenly between them. A stock agent is
Claude Code as it comes, with its default settings (the user's settings,
plugins and MCP servers, default tools and system prompt). It gets only what
an unattended run cannot do without: permission to edit files and to run
cargo, and an output format that carries the token counts. The setup is
frozen (`agent::STOCK`, described in [bench/BASELINE.md](bench/BASELINE.md))
and never gets any of Bottle Box's improvements. Both sides use the same
model.

The task is [bench/coding10.txt](bench/coding10.txt): ten independent Rust
modules (a CSV parser, an INI reader, a todo list, a tokenizer, string
utilities, a date formatter, a key-value store with TTL, a Markdown table of
contents, an LRU cache, a log filter), standard library only, tested with
`cargo test --release`. A result counts only if every test passes.

### The latest result

Claude Opus 5.5 on both sides, one run each (2026-11-24).

| | Bottle Box | 5 stock Claude Code agents |
|---|---|---|
| **Wall time** | **44.7 s** | 92.7 s |
| Speed-up | **2.07×** | 1.00× |
| Thinking / building | 24.0 s / 16.0 s | (one session per agent) |
| Agents | 28 (10 directors, 18 minions) | 5 |
| **Total tokens** | **204 828** | 1 368 561 |
| read from the cache | 129 896 | 1 181 185 |
| written to the cache | 46 944 | 138 451 |
| output (of it thinking) | 27 932 (1 699) | 48 877 (1 063) |
| Cache-read share | 73.4 % | 89.5 % |
| **Cost at API prices** | **$0.96** | $2.32 |
| Lines of code | 1 058 | 2 463 |
| Duplicated code | 0.0 % | 1.4 % |
| Tests | 43 pass, 0 fail | 70 pass, 0 fail |
| Fixes needed | 0 | (inside the agents) |

The stock agents' cache-read share is higher because each turn re-reads a
large system prompt (about 43 000 tokens with the user's settings, plugins
and MCP servers) over many turns; in absolute numbers they read nine times
as much from the cache and wrote three times as much to it.

### How it got there

Every run below is the same task. Until 2026-11-24 the baseline was not yet
the frozen stock one: it ran on Claude Sonnet 5.5 with Bottle Box's own
flags (no user settings), so its time and tokens are not comparable with the
last two rows.

| Design | Bottle Box | Baseline | Bottle Box tokens | Baseline tokens | What it showed |
|---|---|---|---|---|---|
| One director plans everything (high effort), 20 minions | 336.1 s | 57.3 s | 708 869 | 206 532 | The director took 306 s; the minions wrote everything in 24.5 s. Twice the code. |
| The Box: interfaces first, compact summaries | 148.2 s | 53.6 s | 255 302 | 265 973 | Same amount of code as the baseline; the director still 123 s. |
| No director: 20 minions, each directing its own file | 43.3 s | 60.0 s | 570 704 | 293 633 | First run faster than the baseline; every minion read the whole board. |
| The same, each minion reading only its slice of the Box | 36.9 s | 57.4 s | 264 449 | 248 134 | Reading each other's messages had been half of all tokens. |
| A Foreman (21st minion) assigning fixes | 45.7 s | 57.9 s | 279 848 | 243 398 | One failing task (the date formatter) cost 14 s; the Foreman's choice was on the critical path. |
| One director per task, minions by workload, streaming | 31.8 s | 48.2 s | 179 160 | 193 524 | No fixes needed; cache-read share only 13 %. |
| The same, Opus 5.5 both, frozen stock baseline | 46.5 s | 101.3 s | 189 324 | 1 483 014 | 2.18× faster; cache-read share still 13 %. |
| The same, conversations forked for the cache | 44.7 s | 92.7 s | 204 828 | 1 368 561 | Cache-read share 73 %, cost $0.96 against $2.32. |

The first driver, Bottle Clip (a 30-second montage of *Tears of Steel*, 20
minions against one agent), took 45.8 s against 51.9 s; there the final
video render, not the agents, took most of the time.

## What the benchmarks taught

- **One planner is a bottleneck.** A single director planning every file at
  high effort took 123 to 306 s. Planning per task, all at once, takes about
  as long as the slowest task's planning.
- **Messages are the hidden cost.** When every minion read the whole board
  (143 posts), reading each other's thoughts was about half of all tokens.
  Giving each minion only its own slice (about 1 400 characters instead of
  32 000) cut building tokens by two thirds.
- **One mind per task.** The date formatter failed in several runs because
  its implementation and its tests were planned by two minions apart: one
  chose the format tokens `DDDD`, the other `dddd`, and a test called a
  6-argument function with 3. Since one director decides a task's API and
  its test cases together, it has passed first time.
- **Test early, fix one task.** Testing each task alone as soon as it is
  written, and sending only a failing task back to one minion, keeps a
  failure from holding the others up. A failing test that does not compile
  can hide another failure if the whole crate is tested at once.
- **A single-call minion cannot be helped in the middle of its work.** It
  writes its file in one go, so free minions have nothing to take over; the
  Foreman experiment showed most minions idle for most of the build.
- **Caching follows conversations, not prompts.** With Claude Code's own
  breakpoints, sharing comes from going on from the same conversation, not
  from giving many fresh calls the same system prompt.

## Usage

```
bottlebox code --task-file FILE --dir DIR [--agents 20] [--model claude-opus-5-5] [--bench] [--hive] [--director] [--baseline 5]
bottlebox clip <url> [--style hype] [--length 30] [--agents 20] [--candidates 12] [--single] [--compare]
bottlebox measure <folder>        lines of code and duplicated code of a folder
bottlebox box-mcp --box DIR --as ID    the Box's MCP server for one agent
```

- `--bench`: Bottle Box, then the frozen baseline, then the two side by side.
- `--agents N`: the most agents running at once.
- The task file is plain text; its numbered lines (`1. ...`) are the tasks.

## Logs and what is measured

- On a terminal, a live board shows every agent (running, done, waiting,
  queued) with its time; written to a file, every change is a line, with
  the whole run's clock beside each step's.
- Each run is kept in `~/.bottlebox/runs/<time>-code-<mode>/`: the shared
  prefix, the Box, every agent's answer with its tokens and time
  (`pieces/`), the test outputs and `summary.json`.
- `~/.bottlebox/usage.log` gets one line per run: wall time, agents, tokens,
  cost.
- Reported for every run: time per step, tokens (uncached, written to the
  cache, read from it, output, thinking), cache-read share, cost from the
  tokens, rate-limit waits, lines of code, duplicated code (lines in a run
  of three meaningful lines that appears twice anywhere), tests passed.

## Limitations

- Each result is a single run; the baseline's tokens varied between 194 000
  and 294 000 across runs on Sonnet, so small differences mean little.
- Cache breakpoints are Claude Code's; Bottle Box can only reuse
  conversations, not place its own.
- The warm session must finish before the directors start (about 5 s).
- The task split assumes numbered, independent tasks; tasks that depend on
  each other need the registry and the board more than these benchmarks
  did.
- Bottle Box reads Strike's Nimbus store from `../Strike` (a private repo):
  clone it beside this one to build.

## Layout

| File | What it is |
|---|---|
| `src/main.rs` | the commands, the reports, the side-by-side comparison |
| `src/agent.rs` | one agent: `claude -p`, the frozen `STOCK` baseline, rate limits, token counts, costs |
| `src/swarm.rs` | many agents at once, the live board, the warm start |
| `src/shared.rs` | the Box and its MCP server |
| `src/code.rs` | the code driver's common parts: git, tests, blame, measuring, the baseline run |
| `src/load.rs` | the default mode: one director per task, minions by workload, streaming, forked sessions |
| `src/hive.rs` | a director for each file, then 20 minions |
| `src/workspace.rs` | one director with interfaces first |
| `src/clip.rs` | the Bottle Clip driver |
| `bench/` | the benchmark tasks and the frozen baseline |

## Requirements

Claude Code (`claude` on PATH, logged in to a Claude subscription), Rust,
git, and Strike beside this repo (`../Strike`). The clip driver also needs
Bottle Clip installed (`~/.bottleclip/bin/bottle-clip.exe`).
