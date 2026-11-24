# Bottle Box

A black box where one task is split across many small Claude agents working
at the same time, sharing one context, so the job is done in seconds instead
of minutes. The same work as one Claude doing it alone, in many small pieces
run in parallel. General-purpose; the first project it drives is
[Bottle Clip](../BottleClip) (montages of videos).

    bottlebox clip <url> [--style hype] [--length 30] [--agents 20] [--candidates 12] [--model sonnet] [--single] [--compare]

- `--agents N`: how many agents run at once; the rest wait their turn. It can
  be hundreds, but each agent is a `claude` process (about 200 MB).
- `--single`: one agent does the whole task alone (the comparison).
- `--compare`: the swarm, then one agent, and the two side by side.

## How it works

- **One shared context**: the task, what Bottle Clip can do, the style and
  every candidate (times, words, the path of its frame grid). It is kept in
  Strike's Nimbus (content hash in the run's summary) and given to every agent
  as its system prompt, so it comes first and is the same for all: the prompt
  cache holds it once. The first agent is started 3 s before the others, so
  they read the context from the cache instead of each writing it again.
- **Splitter**: the task in independent pieces. For Bottle Clip: a scout for
  each candidate (open its grid, read its words, rate it 1 to 10, its role:
  opener, build, climax or skip, and its best part), caption pieces (the
  spellings for three candidates each), one piece for each effect (the look,
  the transitions, the punch-ins, the title).
- **Swarm**: every piece at once, at most N running. Each agent is a headless
  Claude Code (`claude -p`) with only the Read tool (to open images), no MCP
  servers, no user settings (so no plugins such as Shallow, no hooks) and no
  skills, so it starts fast and costs little. A rate limit (the Max plan's
  limits included) is not a failure: the agent waits (5 s, then twice as long
  each time, at most 5 minutes) and asks again.
- **Merger**: one plan from the reports: the best opener, the builds, the
  best climax, to the asked length; for conflicting spellings the first wins,
  and the conflict is told to the director. Bottle Clip snaps it to the beat.
- **Director**: every minion reports to it. It gets all their reports, the
  merged plan and the preview's frame grid (Bottle Clip renders it); it may
  change the order and the effects, or send pieces back to be done again
  with a note (once: they are done again at once, merged, previewed and
  judged again). Then Bottle Clip renders the final video.

Bottle Clip does all the heavy lifting (analysis, the beat with Bottle EQ,
rendering with NVENC); the agents only decide.

## What is logged

- A live board in the terminal: running, done, waiting (rate limited),
  queued, and each piece's time.
- Each run in `~/.bottlebox/runs/<time>-swarm|single/`: the shared context,
  every agent's answer with its tokens and time (`pieces/`), the plan, and
  `summary.json` (each step's time, agents and tokens).
- One line a run in `~/.bottlebox/usage.log`: wall time, agents, tokens,
  and what it would cost at API prices (the Max plan pays none of it; it is
  the measure of how much of the limits a run takes).

## Needs

Claude Code (`claude` on PATH, logged in), Bottle Clip built and installed
(`~/.bottleclip/bin/bottle-clip.exe`, or `BOTTLE_CLIP_EXE`), and Strike beside
this repo (`../Strike`, for Nimbus).
