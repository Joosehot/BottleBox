# Bottle Box

**2.07× faster than five Claude Code agents, with 7× fewer tokens and less
than half the cost.** Same task, same model (Claude Opus 5.5), all tests
passing on both sides.

| | Bottle Box | 5 stock Claude Code agents |
|---|---|---|
| **Wall time** | **44.7 s** | 92.7 s |
| Speed-up | **2.07×** | 1.00× |
| **Total tokens** | **204 828** | 1 368 561 |
| read from the cache | 129 896 | 1 181 185 |
| written to the cache | 46 944 | 138 451 |
| output (of it thinking) | 27 932 (1 699) | 48 877 (1 063) |
| Cache-read share | 73.4 % | 89.5 % |
| **Cost at API prices** | **$0.96** | $2.32 |
| Thinking / building | 24.0 s / 16.0 s | one session per agent |
| Agents | 28 (10 directors, 18 minions) | 5 |
| Lines of code | 1 058 | 2 463 |
| Duplicated code | 0.0 % | 1.4 % |
| Tests | 43 pass, 0 fail | 70 pass, 0 fail |

The task: [bench/coding10.txt](bench/coding10.txt), ten independent Rust
modules (a CSV parser, an INI reader, a todo list, a math tokenizer, string
utilities, a date formatter, a key-value store with TTL, a Markdown table of
contents, an LRU cache, a log filter), standard library only, tested with
`cargo test --release`. The baseline is five stock Claude Code agents in
parallel, the tasks split evenly, set up as in
[bench/BASELINE.md](bench/BASELINE.md). One run each, 2026-11-24.

```
bottlebox code --task-file bench/coding10.txt --dir <empty folder> --bench
```

## What it is

Bottle Box splits one task across many Claude Code agents ("minions") that
work at the same time in one shared workspace (the Box). It plans, starts
and coordinates them, merges what they write and tests it. Its main use is
writing code; it can also drive Bottle Clip (a separate project that
watches videos and cuts montages) to edit a video.

## How it works

The default mode for code (`src/load.rs`) has five steps.

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
3. **Minions by workload.** A small task gets one minion for both files; a
   usual task gets two (implementation, tests); a big one gets a minion for
   each section and one for the tests. At most 20 minions in all, and only
   as many as the work needs.
4. **Start at once, test at once.** A task's minions start the moment its
   director finishes, not when every director has finished. Each task is
   tested on its own as soon as its files are written (in a small crate
   holding only that module), while the others are still being written. A
   failing task goes with its error to one minion that owns that task's
   files; nothing else waits for it.
5. **Merge and test.** Every task lives on its own git branch in its own
   worktree, so no two minions ever write the same file. The branches are
   merged in pairs, like a tournament, every pair of a round at once. Then
   the whole crate is built and tested.

Other modes kept for comparison: `--hive` (a director for each file, then
20 minions), `--director` (one director plans everything, then 20
minions), and the `clip` driver for Bottle Clip.

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
`registry_lookup`, `registry_claim`, `board_post`, `board_read`. Each
minion is given the part of the Box it needs, so the default mode needs no
tool calls while building.

## How the agents are run

Every agent is a headless Claude Code process (`claude -p`), running on the
user's Claude subscription: Bottle Box takes `ANTHROPIC_API_KEY` and
`ANTHROPIC_AUTH_TOKEN` out of every agent's environment, so no run is
billed to an API key. The dollar figures above are what the tokens would
cost at API prices (a measure of how much of a plan's limits a run takes),
computed from the tokens (`Usage::cost`).

A minion runs with the shared context as its system prompt
(`--system-prompt-file`), no built-in tools unless its piece needs them, no
user settings, plugins, hooks, skills or MCP servers
(`--setting-sources project --disable-slash-commands --strict-mcp-config`),
a set effort, and a cap on its output.

A rate limit is not a failure: an agent waits (5 s, doubling up to five
minutes) and asks again, as often as it takes.

## Prompt caching

Claude Code places the cache breakpoints itself, after the system prompt
and after the last message; `claude -p` gives no way to add one. A stock
agent gets its cache reads by going on in one conversation: each turn reads
the whole conversation so far from the cache and writes only what is new.
A fresh single call cannot share that way, because the part every minion
has in common sits between the two breakpoints.

So Bottle Box reuses conversations the same way:

- one **warm session** holds the shared prefix (the rules and the whole
  task);
- every **director** goes on from it (`--resume <session> --fork-session`);
- every **builder** goes on from its task's director, so the plan is
  already in its conversation;
- a **fix** goes on from the task's first builder.

Every conversation only grows at its end, and all agents use the same
effort, because changing the effort in the middle of a conversation empties
its cache.

Claude Code's own `total_cost_usd` counts a resumed session's earlier turns
again, so Bottle Box computes every cost from the tokens instead.

## The baseline

Bottle Box is always compared with **five stock Claude Code agents working
in parallel**: Claude Code as it comes, with its default settings (the
user's settings, plugins and MCP servers, default tools and system prompt).
It gets only what an unattended run cannot do without: permission to edit
files and to run cargo, and an output format that carries the token counts.
The setup is frozen (`agent::STOCK`, [bench/BASELINE.md](bench/BASELINE.md))
and never gets any of Bottle Box's improvements. Both sides use the same
model.

The stock agents' cache-read share is higher because each turn re-reads a
large system prompt (the user's settings, plugins and MCP servers) over many
turns; in absolute numbers they read nine times as much from the cache and
wrote three times as much to it.

## What makes it fast

- **No single planner.** Planning is split per task and runs all at once,
  so it takes as long as the slowest task's planning, not the sum.
- **Small messages.** Each minion reads only its own slice of the Box, not
  everyone's thoughts.
- **One mind per task.** A task's API and its tests are decided together by
  one director, so implementation and tests agree; when two minds planned
  them apart, they chose different conventions and the tests failed.
- **Test early, fix one task.** A failing task goes back to one minion and
  holds nobody else up.
- **Caching follows conversations.** Sharing comes from going on from the
  same conversation, not from giving many fresh calls the same prompt.

## Usage

```
bottlebox code --task-file FILE --dir DIR [--agents 20] [--model claude-opus-5-5] [--bench] [--hive] [--director] [--baseline 5]
bottlebox clip <url> [--style hype] [--length 30] [--agents 20] [--candidates 12] [--single] [--compare]
bottlebox measure <folder>             lines of code and duplicated code of a folder
bottlebox box-mcp --box DIR --as ID    the Box's MCP server for one agent
```

- `bottlebox review --dir CRATE --files src/a.rs,src/b.rs --checks-file FILE`:
  code that exists, read and mended by many at once. One minion a file, each
  a fresh mind that did not write it, reports only real issues with their
  lines, looking for what the checks file says. One director decides which are
  real and how to mend each (it writes no code). One minion a file then mends,
  all at once; the crate's tests run, and a failure goes back to the minions
  whose files it names. The director ends with a verdict, yes or no (a yes
  with failing tests is a no). `--context` names files the reviewed ones build
  on: the reviewers see their public items. `--reference` names the material
  the code was written from (a specification, another implementation): every
  agent gets its text, and a difference from it is an issue.
- `--bench`: Bottle Box, then the frozen baseline, then the two side by side.
- `--agents N`: the most agents running at once.
- The task file is plain text; its numbered lines (`1. ...`) are the tasks.
- Its head may name the crate, its dependencies and a binary:
  `Crate: my_crate`, `Dependencies: cpal = "0.15"; crossterm = "0.28"`,
  `Binary: ui::run` (a `fn run() -> Result<(), String>` the binary calls).
  A task that uses other modules is tested with them beside it, as soon as
  they are written; the dependencies are built once, while the directors think.
- The minions have no tools, so what they need from outside is brought to
  them, named in the head (each may be given on several lines):
  - `Sources: <url or file>; <url or file> as <name>`: reference material,
    fetched once before anyone thinks (kept in the run's `sources/`). A task
    names the ones it needs in its text, `[reads: fwsec.c, hs.h]`; its
    director gets their text, and its minions read it from the cache.
  - `Uses: path/to/existing.rs; ...`: modules that exist already. They are
    put in the crate beside the new ones (`crate::existing`), unchanged, and
    every agent sees their source (without their unit tests).
  - `Attributes: #![no_std]`: a line for the top of the crate's `lib.rs`
    (one `Attributes:` line for each).

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

- The result above is a single run of each side.
- Cache breakpoints are Claude Code's; Bottle Box can only reuse
  conversations, not place its own.
- The warm session must finish before the directors start.
- The task split assumes numbered, independent tasks; tasks that depend on
  each other lean more on the registry and the board.
- Bottle Box reads Strike's Nimbus store from `../Strike`
  (`src/main.rs`, `#[path]`): clone [Strike](https://github.com/Joosehot/Strike)
  beside this repo to build.

## Layout

| File | What it is |
|---|---|
| `src/main.rs` | the commands, the reports, the side-by-side comparison |
| `src/agent.rs` | one agent: `claude -p`, the frozen `STOCK` baseline, rate limits, token counts, costs |
| `src/swarm.rs` | many agents at once, the live board, the warm start |
| `src/shared.rs` | the Box and its MCP server |
| `src/code.rs` | the code driver's common parts: git, tests, blame, measuring, the baseline run |
| `src/load.rs` | the default mode: one director per task, minions by workload, forked sessions |
| `src/hive.rs` | a director for each file, then 20 minions |
| `src/workspace.rs` | one director with interfaces first |
| `src/clip.rs` | the Bottle Clip driver |
| `bench/` | the benchmark tasks and the frozen baseline |

## Requirements

Claude Code (`claude` on PATH, logged in to a Claude subscription), Rust,
git, and [Strike](https://github.com/Joosehot/Strike) cloned beside this repo
(`../Strike`). The clip driver also needs
Bottle Clip installed (`~/.bottleclip/bin/bottle-clip.exe`).

## License

MIT, see [LICENSE](LICENSE).
