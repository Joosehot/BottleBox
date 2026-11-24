//! The coding driver: a program written by a swarm, or by ordinary Claude
//! Code agents for the comparison.
//!
//! The swarm:
//! 1. **Think** once: the director (thinking hard, `--effort high`) writes the
//!    plan, file by file and function by function: each minion file's task,
//!    purpose, exact public items, what it uses and its tests; the small glue
//!    (Cargo.toml, lib.rs) it writes itself. The task and the plan are the
//!    shared context (in Nimbus), the system prompt of every minion.
//! 2. **Write**: one minion a file (`--effort low`: it executes, it does not
//!    plan), all at once. Each owns its file, in its own git worktree on its
//!    own branch: two minions never write the same file.
//! 3. **Merge** in pairs, like a tournament (n files: about log2 n rounds),
//!    every pair of a round at once. With merge minions, each pair is checked
//!    by a minion (do the halves fit?) that fixes only its pair's files;
//!    without (independent pieces), git joins the branches alone.
//! 4. **Test**: when everything is written and merged, the build and the
//!    tests run. Each error is put on its file and so its task; only the
//!    failing tasks go back, each to one minion with its errors (it owns that
//!    task's files), all at once; then the tests again (3 rounds at most).
//!
//! The baseline: K Claude Code agents at once, the tasks split evenly, each
//! in its own crate, writing, building and testing by itself with its own
//! tools; then every crate's tests are run again here.

use crate::agent::{self, Answer, Ask, Setup, Usage, Want};
use crate::clip::{Report, Step};
use crate::{nimbus, swarm};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

pub enum How {
    /// this many ordinary Claude Code agents at once
    Baseline(usize),
    /// minion files per task (1: implementation with its tests; 2: implementation, tests), and merge minions or not
    Swarm { per_task: usize, merge_minions: bool },
}

pub struct Options {
    pub task: String,
    pub dir: PathBuf,
    /// agents running at once at most
    pub agents: usize,
    pub model: String,
    pub how: How,
}

const FIX_ROUNDS: usize = 3;

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git").current_dir(dir).args(args).stdin(Stdio::null()).output().map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        return Err(format!("git {} in {}: {}", args.join(" "), dir.display(), String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn commit(dir: &Path, message: &str) -> Result<(), String> {
    git(dir, &["add", "-A"])?;
    // nothing to commit is not an error
    let _ = git(dir, &["-c", "user.name=Bottle Box", "-c", "user.email=bottlebox@localhost", "commit", "-q", "--no-verify", "-m", message]);
    Ok(())
}

/// The task's numbered items ("1. ..."), and the text around them (common to all).
fn tasks(text: &str) -> (String, Vec<String>) {
    let (mut common, mut items) = (Vec::new(), Vec::new());
    for l in text.lines() {
        let t = l.trim_start();
        let numbered = t.split_once(". ").is_some_and(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
        if numbered {
            items.push(t.to_string());
        } else if !t.is_empty() {
            common.push(l.to_string());
        }
    }
    (common.join("\n"), items)
}

/// The build and the tests (release): (passed, the output, tests passed, tests failed).
fn test(dir: &Path) -> (bool, String, usize, usize) {
    let out = Command::new("cargo").current_dir(dir).args(["test", "--release", "--message-format", "short", "--color", "never"]).env("CARGO_TERM_COLOR", "never").stdin(Stdio::null()).output();
    let Ok(out) = out else { return (false, "cargo could not be run".into(), 0, 0) };
    let text = format!("{}{}", String::from_utf8_lossy(&out.stderr), String::from_utf8_lossy(&out.stdout));
    // "test result: ok. 12 passed; 0 failed; ...": the number before each word, over every test binary
    let count = |word: &str| -> usize {
        text.lines()
            .filter(|l| l.starts_with("test result:"))
            .filter_map(|l| {
                let w: Vec<&str> = l.split(|c: char| c.is_whitespace() || c == ';' || c == '.').filter(|x| !x.is_empty()).collect();
                w.iter().position(|x| *x == word).and_then(|i| i.checked_sub(1)).and_then(|i| w[i].parse::<usize>().ok())
            })
            .sum()
    };
    let (passed, failed) = (count("passed"), count("failed"));
    (out.status.success() && passed > 0, text, passed, failed)
}

/// The failures of a test run, put on their files.
fn blame(text: &str, files: &[String]) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let lines: Vec<&str> = text.lines().collect();
    // build errors: "src/x.rs:12:5: error[E0425]: ..." (and the lines under them)
    for (i, l) in lines.iter().enumerate() {
        let path = l.split(':').next().unwrap_or("").replace('\\', "/");
        if l.contains(": error") && files.contains(&path) {
            let more: Vec<&str> = lines[i + 1..].iter().take(6).take_while(|x| !x.contains(": error") && !x.contains(": warning")).copied().collect();
            out.entry(path).or_default().push(format!("{l}\n{}", more.join("\n")));
        }
    }
    // failing tests: "test a::b::c ... FAILED" in the binary "Running tests/x.rs" or of a module
    let mut binary = String::new();
    for l in &lines {
        if let Some(rest) = l.trim().strip_prefix("Running ") {
            binary = rest.split_whitespace().next().unwrap_or("").replace('\\', "/");
        }
        if let Some(name) = l.strip_prefix("test ").and_then(|r| r.strip_suffix(" ... FAILED")) {
            let module: Vec<&str> = name.split("::").filter(|p| *p != "tests").collect();
            let in_module = (1..module.len().max(1)).rev().flat_map(|n| {
                let m = module[..n].join("/");
                vec![format!("src/{m}.rs"), format!("src/{m}/mod.rs")]
            });
            let file = files.iter().find(|f| binary.ends_with(f.as_str()) && f.starts_with("tests/")).cloned().or_else(|| in_module.into_iter().find(|f| files.contains(f)));
            let why: String = lines.iter().skip_while(|x| !x.contains(&format!("---- {name} stdout ----"))).skip(1).take(8).copied().collect::<Vec<_>>().join("\n");
            if let Some(f) = file {
                out.entry(f).or_default().push(format!("test {name} FAILED\n{why}"));
            }
        }
    }
    out
}

/// The files of a set in one text, each as `=== FILE: path ===`.
fn bundle(dir: &Path, files: &[String]) -> String {
    files.iter().map(|f| format!("=== FILE: {f} ===\n{}", std::fs::read_to_string(dir.join(f)).unwrap_or_default())).collect::<Vec<_>>().join("\n") + "\n=== END ===\n"
}

fn step(steps: &mut Vec<Step>, name: &str, t: &mut Instant, answers: &[&Answer]) {
    let mut usage = Usage::default();
    answers.iter().for_each(|a| usage.add(&a.usage));
    steps.push(Step { name: name.to_string(), seconds: t.elapsed().as_secs_f64(), agents: answers.len(), usage, waited: answers.iter().map(|a| a.waited).sum() });
    *t = Instant::now();
}

fn count_files(dir: &Path) -> usize {
    fn walk(d: &Path, n: &mut usize) {
        for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() && p.file_name().is_some_and(|x| x != "target" && x != ".git") {
                walk(&p, n);
            } else if p.extension().is_some_and(|x| x == "rs") {
                *n += 1;
            }
        }
    }
    let mut n = 0;
    walk(dir, &mut n);
    n
}

fn finish(mode: String, steps: Vec<Step>, wall: Instant, result: String, dir: PathBuf, split: (f64, f64)) -> Report {
    let mut usage = Usage::default();
    steps.iter().for_each(|s| usage.add(&s.usage));
    let agents = steps.iter().map(|s| s.agents).sum();
    let steps_json: Vec<Value> = steps.iter().map(|s| json!({"step": s.name, "seconds": s.seconds, "agents": s.agents, "usage": s.usage, "rate_limit_waits": s.waited})).collect();
    let r = Report { mode, steps, wall: wall.elapsed().as_secs_f64(), agents, usage, result, dir, split: Some(split) };
    let _ = agent::write(&r.dir.join("summary.json"), &serde_json::to_string_pretty(&json!({"mode": r.mode, "wall": r.wall, "agents": r.agents, "usage": r.usage, "result": r.result, "writing_s": split.0, "testing_s": split.1, "steps": steps_json})).unwrap());
    r
}

fn fresh(dir: &Path) -> Result<(), String> {
    if dir.exists() && std::fs::read_dir(dir).map_or(false, |mut d| d.next().is_some()) {
        return Err(format!("{} is not empty", dir.display()));
    }
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    git(dir, &["init", "-q", "-b", "main"]).map(|_| ())
}

pub fn run(o: &Options) -> Result<Report, String> {
    let wall = Instant::now();
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let (common, items) = tasks(&o.task);
    let (mode, tag) = match o.how {
        How::Baseline(k) => (format!("{k} Claude Code agents in parallel"), format!("baseline{k}")),
        How::Swarm { per_task, .. } => (format!("Bottle Box, {per_task} minion file(s) a task"), format!("swarm{per_task}")),
    };
    let run_dir = agent::home().join("runs").join(format!("{stamp}-code-{tag}"));
    std::fs::create_dir_all(&run_dir).map_err(|e| e.to_string())?;
    println!("Bottle Box (code): {mode}, into {}", o.dir.display());
    let mut steps = Vec::new();
    let mut t = Instant::now();

    if let How::Baseline(k) = o.how {
        // K ordinary agents at once, the tasks split evenly, each in its own crate
        std::fs::create_dir_all(&o.dir).map_err(|e| e.to_string())?;
        let groups: Vec<Vec<String>> = if items.is_empty() { vec![vec![o.task.clone()]] } else { (0..k).map(|g| items.iter().skip(g).step_by(k).cloned().collect()).filter(|g: &Vec<String>| !g.is_empty()).collect() };
        let mut asks = Vec::new();
        for (g, mine) in groups.iter().enumerate() {
            let d = o.dir.join(format!("agent-{}", g + 1));
            fresh(&d)?;
            asks.push(Ask {
                id: format!("agent-{}", g + 1),
                prompt: format!("{common}\n\nYour tasks:\n{}\n\nWork in this folder (it is empty; git is set up). Make it one Rust library crate with one module for each of your tasks and their tests, then run `cargo test --release` until every test passes.", mine.join("\n")),
                model: o.model.clone(),
                keep: false,
                resume: None,
                effort: None,
                want: Want::Text,
                tools: "Read,Write,Edit,Bash,Glob,Grep".into(),
                allow: vec!["Bash(cargo:*)".into(), "Bash(git:*)".into(), "Bash(ls:*)".into(), "Bash(mkdir:*)".into()],
                workdir: Some(d),
            });
        }
        let setup = Setup { shared: None, workdir: o.dir.clone(), read: Vec::new() };
        let done: Vec<Answer> = swarm::run(&format!("{} agents at once", asks.len()), &setup, asks, o.agents).into_iter().collect::<Result<_, _>>()?;
        for a in &done {
            agent::write(&run_dir.join(format!("{}.json", a.id)), &serde_json::to_string_pretty(a).unwrap())?;
        }
        step(&mut steps, "agents write and test (parallel)", &mut t, &done.iter().collect::<Vec<_>>());
        // every crate's tests run again here, at once
        let dirs: Vec<PathBuf> = (1..=done.len()).map(|g| o.dir.join(format!("agent-{g}"))).collect();
        let results: Vec<(bool, String, usize, usize)> = std::thread::scope(|s| dirs.iter().map(|d| s.spawn(move || test(d))).collect::<Vec<_>>().into_iter().map(|h| h.join().unwrap()).collect());
        for (g, r) in results.iter().enumerate() {
            agent::write(&run_dir.join(format!("test-agent-{}.txt", g + 1)), &r.1)?;
        }
        step(&mut steps, "check: cargo test --release", &mut t, &[]);
        let ok = results.iter().all(|r| r.0);
        let (passed, failed): (usize, usize) = (results.iter().map(|r| r.2).sum(), results.iter().map(|r| r.3).sum());
        let n: usize = dirs.iter().map(|d| count_files(d)).sum();
        // writing against testing on the critical path: the slowest agent's own cargo commands are testing, the rest of its time writing; the check here is testing too
        let slowest = done.iter().max_by(|a, b| a.seconds.total_cmp(&b.seconds)).unwrap();
        let check = steps.last().map_or(0.0, |s| s.seconds);
        let split = (slowest.seconds - slowest.cargo_seconds, slowest.cargo_seconds + check);
        let cargo_all: f64 = done.iter().map(|a| a.cargo_seconds).sum();
        let result = format!("{} ({passed} tests passed, {failed} failed; {n} source files in {} crates; the agents spent {cargo_all:.0} s in cargo in all)", if ok { "ALL TESTS PASS" } else { "TESTS FAIL" }, dirs.len());
        return Ok(finish(mode, steps, wall, result, run_dir, split));
    }
    let How::Swarm { per_task, merge_minions } = o.how else { unreachable!() };
    fresh(&o.dir)?;

    // 1. think: the director plans
    let rules = "You are part of Bottle Box: one task split across many agents at once. The director plans once, thinking hard; each minion then writes or fixes one piece and only that piece. Two minions never write the same file. Rust, edition 2021, standard library only (no dependencies), so it builds offline; tests run with `cargo test --release`.";
    let task_file = run_dir.join("task.txt");
    agent::write(&task_file, &format!("{rules}\n\nTASK:\n{}\n", o.task))?;
    let setup = Setup { shared: Some(task_file.clone()), workdir: run_dir.clone(), read: Vec::new() };
    let split = match per_task {
        1 => "one minion file for each task: src/<module>.rs with the implementation and its unit tests".to_string(),
        2 => "two minion files for each task: its implementation (src/<module>.rs, the pub API, no tests) and its tests (tests/<module>.rs, integration tests that check exact known answers through the pub API only)".to_string(),
        n => format!("{n} minion files for each task, each with a clear part of it"),
    };
    let think = Ask {
        id: "director".into(),
        prompt: format!(
            "You are the director. Think it through once, carefully, then write the plan the minions will execute. Split the work into {split}. Each file goes to one minion that sees only this plan and writes only its file, so the plan must leave nothing to guess: for every file its exact public items (full Rust signatures of every pub fn, struct with its fields, enum, const), what it uses from which file (by module path), its error handling, and its tests with the exact known answers they check (only answers you are sure of; where you are not, a test that checks one method against another). You write the small glue yourself: Cargo.toml and src/lib.rs (declaring every module). Reply with ONE JSON object: {{\"crate\":\"<name>\",\"glue\":[{{\"path\":\"Cargo.toml\",\"content\":\"...\"}},{{\"path\":\"src/lib.rs\",\"content\":\"...\"}}],\"conventions\":\"<rules every file follows>\",\"files\":[{{\"path\":\"...\",\"task\":<its task's number>,\"purpose\":\"...\",\"items\":\"<the exact pub signatures>\",\"uses\":\"<what it uses from which module>\",\"tests\":\"<its tests and the answers they check>\"}}]}}"
        ),
        model: o.model.clone(),
        keep: false,
        resume: None,
        effort: Some("high".into()),
        want: Want::Json,
        tools: "".into(),
        allow: Vec::new(),
        workdir: None,
    };
    let plan_answer = swarm::run("think: the director plans", &setup, vec![think], 1).pop().unwrap()?;
    let plan = plan_answer.json.clone();
    let specs: Vec<(String, u64)> = plan["files"].as_array().into_iter().flatten().filter_map(|f| Some((f["path"].as_str()?.replace('\\', "/"), f["task"].as_u64().unwrap_or(0)))).collect();
    let files: Vec<String> = specs.iter().map(|s| s.0.clone()).collect();
    if files.len() < 2 {
        return Err("the director's plan has fewer than 2 files".into());
    }
    for g in plan["glue"].as_array().into_iter().flatten() {
        if let (Some(p), Some(c)) = (g["path"].as_str(), g["content"].as_str()) {
            agent::write(&o.dir.join(p), c)?;
        }
    }
    agent::write(&o.dir.join(".gitignore"), "/target\n")?;
    agent::write(&run_dir.join("plan.json"), &serde_json::to_string_pretty(&plan).unwrap())?;
    commit(&o.dir, "the director's plan: the glue")?;
    println!("  plan: {} minion files", files.len());
    step(&mut steps, "think (director)", &mut t, &[&plan_answer]);

    // the shared context of the minions: the task and the whole plan
    let shared = format!("{rules}\n\nTASK:\n{}\n\nTHE DIRECTOR'S PLAN (follow it exactly; every name and signature in it is binding):\n{}\n", o.task, serde_json::to_string_pretty(&plan).unwrap());
    let hash = nimbus::put_bytes(shared.as_bytes())?;
    let shared_file = run_dir.join("shared.txt");
    agent::write(&shared_file, &shared)?;
    let setup = Setup { shared: Some(shared_file), workdir: run_dir.clone(), read: Vec::new() };
    let form = "Reply with the file only: a line `=== FILE: <its path> ===`, then its whole content, then `=== END ===`.";

    // 2. write: one minion a file, each in its own worktree and branch
    let wt = run_dir.join("wt");
    for i in 0..files.len() {
        git(&o.dir, &["worktree", "add", "-q", "-b", &format!("piece-{i:03}"), &wt.join(format!("{i:03}")).display().to_string(), "main"])?;
    }
    let asks: Vec<Ask> = files.iter().map(|f| Ask { tools: "".into(), ..Ask::minion(&format!("write {f}"), format!("Your piece: you own {f} and only it. Write it exactly to the plan: the same pub items with the same signatures; use the other files only through their planned items. {form}"), &o.model, Want::Files) }).collect();
    let written = swarm::run(&format!("write: {} minions", asks.len()), &setup, asks, o.agents);
    let mut writers = Vec::new();
    for (i, (r, f)) in written.into_iter().zip(&files).enumerate() {
        let a = r?;
        let content = a.files.get(f).cloned().or_else(|| (a.files.len() == 1).then(|| a.files.values().next().cloned()).flatten()).ok_or(format!("{f}: its minion gave another file ({:?})", a.files.keys().collect::<Vec<_>>()))?;
        let at = wt.join(format!("{i:03}"));
        agent::write(&at.join(f), &content)?;
        commit(&at, &format!("{f}, by its minion"))?;
        let _ = nimbus::put_bytes(serde_json::to_string(&a).unwrap().as_bytes());
        writers.push(a);
    }
    step(&mut steps, "write (one minion a file)", &mut t, &writers.iter().collect::<Vec<_>>());

    // 3. merge: pairs, like a tournament, every pair of a round at once
    let mut groups: Vec<(usize, Vec<String>)> = (0..files.len()).map(|i| (i, vec![files[i].clone()])).collect();
    let (mut mergers, mut round) = (Vec::new(), 0usize);
    while groups.len() > 1 {
        round += 1;
        let (mut next, mut pairs) = (Vec::new(), Vec::new());
        let mut it = groups.into_iter();
        while let Some(a) = it.next() {
            match it.next() {
                Some(b) => pairs.push((a, b)),
                None => next.push(a),
            }
        }
        // the branches joined (each file has one owner, so git never meets a conflict)
        std::thread::scope(|s| -> Result<(), String> {
            let wt = &wt;
            let hs: Vec<_> = pairs.iter().map(|((a, _), (b, _))| {
                let (a, b) = (*a, *b);
                s.spawn(move || git(&wt.join(format!("{a:03}")), &["merge", "-q", "--no-edit", &format!("piece-{b:03}")]))
            }).collect();
            hs.into_iter().try_for_each(|h| h.join().unwrap().map(|_| ()))
        })?;
        if merge_minions {
            let asks: Vec<Ask> = pairs
                .iter()
                .map(|((a, fa), (_, fb))| Ask { tools: "".into(), ..Ask::minion(&format!("merge r{round} p{a}"), format!(
                    "Your piece: merge two halves. Half A: {}. Half B: {}. Check that they fit together: every use in one half of an item of the other half (its name, module path, signature, types) against the item as written and as planned. Fix only what does not fit, and only in these files. Reply with only the files you changed, each as `=== FILE: <path> ===` and its whole content, the last ended by `=== END ===`; if everything fits, reply `=== END ===` alone.\n\nThe files:\n{}",
                    fa.join(", "), fb.join(", "), bundle(&wt.join(format!("{a:03}")), &[fa.clone(), fb.clone()].concat())
                ), &o.model, Want::Text) })
                .collect();
            let results = swarm::run(&format!("merge round {round}: {} pairs at once", pairs.len()), &setup, asks, o.agents);
            for (r, ((a, fa), (_, fb))) in results.into_iter().zip(&pairs) {
                let ans = r?;
                let mine: Vec<String> = [fa.clone(), fb.clone()].concat();
                let at = wt.join(format!("{a:03}"));
                for (path, content) in agent::files_in(&ans.text).into_iter().filter(|(p, _)| mine.contains(p)) {
                    agent::write(&at.join(&path), &content)?;
                }
                commit(&at, &format!("merge round {round}"))?;
                mergers.push(ans);
            }
        }
        for ((a, fa), (_, fb)) in pairs {
            next.push((a, [fa, fb].concat()));
        }
        next.sort_by_key(|g| g.0);
        groups = next;
    }
    git(&o.dir, &["merge", "-q", "--ff-only", &format!("piece-{:03}", groups[0].0)])?;
    for i in 0..files.len() {
        let _ = git(&o.dir, &["worktree", "remove", "--force", &wt.join(format!("{i:03}")).display().to_string()]);
    }
    step(&mut steps, &format!("merge ({round} rounds, {})", if merge_minions { "minions" } else { "git only: independent pieces" }), &mut t, &mergers.iter().collect::<Vec<_>>());

    // 4. test: everything at once; only the failing tasks go back, each to one minion
    let task_of: BTreeMap<String, u64> = specs.iter().cloned().collect();
    let mut fixers = Vec::new();
    let mut outcome = test(&o.dir);
    agent::write(&run_dir.join("test-0.txt"), &outcome.1)?;
    let mut rounds = 0;
    while !outcome.0 && rounds < FIX_ROUNDS {
        rounds += 1;
        let blamed = blame(&outcome.1, &files);
        if blamed.is_empty() {
            eprintln!("  test: failures that no file can be blamed for (see test-{}.txt)", rounds - 1);
            break;
        }
        // the failing files by task: one minion a task, owning all that task's files
        let mut by_task: BTreeMap<u64, Vec<String>> = BTreeMap::new();
        for (f, errs) in &blamed {
            by_task.entry(task_of.get(f).copied().unwrap_or(0)).or_default().extend(errs.iter().map(|e| format!("in {f}: {e}")));
        }
        println!("  test round {rounds}: tasks {:?} fail", by_task.keys().collect::<Vec<_>>());
        let owned: Vec<(u64, Vec<String>)> = by_task.keys().map(|k| (*k, specs.iter().filter(|s| s.1 == *k || (*k == 0 && blamed.contains_key(&s.0))).map(|s| s.0.clone()).collect())).collect();
        let asks: Vec<Ask> = owned
            .iter()
            .map(|(k, mine)| Ask { tools: "".into(), ..Ask::minion(&format!("fix task {k}"), format!(
                "Your piece: you own {} (task {k}) and only them. The build or the tests fail:\n{}\n\nThe files now:\n{}\nFix them, keeping the plan's pub items and signatures. Do not change an expected answer to match the output unless you are sure the expected answer was wrong. Reply with the files you changed, each as `=== FILE: <path> ===` and its whole content, the last ended by `=== END ===`.",
                mine.join(", "), by_task[k].join("\n\n"), bundle(&o.dir, mine)
            ), &o.model, Want::Files) })
            .collect();
        let fixed = swarm::run(&format!("fix round {rounds}: {} minions", asks.len()), &setup, asks, o.agents);
        for (r, (_, mine)) in fixed.into_iter().zip(&owned) {
            let a = r?;
            for (path, content) in a.files.iter().filter(|(p, _)| mine.contains(p)) {
                agent::write(&o.dir.join(path), content)?;
            }
            fixers.push(a);
        }
        commit(&o.dir, &format!("test round {rounds}: the failing tasks fixed by their minions"))?;
        outcome = test(&o.dir);
        agent::write(&run_dir.join(format!("test-{rounds}.txt")), &outcome.1)?;
    }
    step(&mut steps, &format!("test ({rounds} fix rounds)"), &mut t, &fixers.iter().collect::<Vec<_>>());

    let (ok, _, passed, failed) = outcome;
    let result = format!("{} ({passed} tests passed, {failed} failed; {} source files; shared context {})", if ok { "ALL TESTS PASS" } else { "TESTS FAIL" }, count_files(&o.dir), &hash[..12]);
    // writing: the plan, the minions' writing and the merge; testing: the tests and the fixes
    let writing: f64 = steps.iter().filter(|s| !s.name.starts_with("test")).map(|s| s.seconds).sum();
    let testing: f64 = steps.iter().filter(|s| s.name.starts_with("test")).map(|s| s.seconds).sum();
    Ok(finish(mode, steps, wall, result, run_dir, (writing, testing)))
}
