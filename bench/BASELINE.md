# The benchmark baseline (frozen)

Every Bottle Box benchmark is compared with **5 stock Claude Code agents**
working in parallel, the tasks split evenly between them (the ratio rule: 20
minions against 5 agents).

A stock agent is Claude Code as it comes, with its default settings: the
user's own settings, plugins and MCP servers, its default tools, system
prompt and effort. No custom infrastructure, no tweaks, and none of Bottle
Box's improvements are ever applied to it.

The command (`agent::STOCK` in `src/agent.rs`), run in its own empty folder
with the task on stdin:

    claude -p --model claude-opus-5-5 --output-format stream-json --verbose --permission-mode acceptEdits --allowedTools=Bash(cargo:*)

- `--model claude-opus-5-5`: the model of the comparison (the minions use
  the same one).
- `--permission-mode acceptEdits` and `--allowedTools=Bash(cargo:*)`: an
  unattended `claude -p` has nobody to answer a permission prompt; without
  these it could not write files or run its tests.
- `--output-format stream-json --verbose`: only the form of the output, so
  that its tokens and the time of its cargo commands can be read.

Do not change this setup.
