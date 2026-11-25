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
use crate::swarm;
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
    /// no director: every minion directs its own piece, all thinking together in the Box (hive.rs)
    Hive,
    /// one director a task thinking together, minions by workload, a streaming plan (load.rs)
    Load,
}

pub struct Options {
    pub task: String,
    pub dir: PathBuf,
    /// agents running at once at most
    pub agents: usize,
    pub model: String,
    pub how: How,
}

pub(crate) const FIX_ROUNDS: usize = 3;

pub(crate) fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git").current_dir(dir).args(args).stdin(Stdio::null()).output().map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        return Err(format!("git {} in {}: {}", args.join(" "), dir.display(), String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub(crate) fn commit(dir: &Path, message: &str) -> Result<(), String> {
    git(dir, &["add", "-A"])?;
    // nothing to commit is not an error
    let _ = git(dir, &["-c", "user.name=Bottle Box", "-c", "user.email=bottlebox@localhost", "commit", "-q", "--no-verify", "-m", message]);
    Ok(())
}

/// The task's numbered items ("1. ..."), and the text around them (common to all).
pub(crate) fn tasks(text: &str) -> (String, Vec<String>) {
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
pub(crate) fn test(dir: &Path) -> (bool, String, usize, usize) {
    test_in(dir, None)
}

/// The same, building into `target` (one target folder shared by many crates: their dependencies are built once).
pub(crate) fn test_in(dir: &Path, target: Option<&Path>) -> (bool, String, usize, usize) {
    let mut c = Command::new("cargo");
    c.current_dir(dir).args(["test", "--release", "--message-format", "short", "--color", "never"]).env("CARGO_TERM_COLOR", "never").stdin(Stdio::null());
    if let Some(t) = target {
        c.env("CARGO_TARGET_DIR", t);
    }
    let out = c.output();
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
pub(crate) fn blame(text: &str, files: &[String]) -> BTreeMap<String, Vec<String>> {
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
pub(crate) fn bundle(dir: &Path, files: &[String]) -> String {
    files.iter().map(|f| format!("=== FILE: {f} ===\n{}", std::fs::read_to_string(dir.join(f)).unwrap_or_default())).collect::<Vec<_>>().join("\n") + "\n=== END ===\n"
}

pub(crate) fn step(steps: &mut Vec<Step>, name: &str, t: &mut Instant, answers: &[&Answer]) {
    let mut usage = Usage::default();
    answers.iter().for_each(|a| usage.add(&a.usage));
    steps.push(Step { name: name.to_string(), seconds: t.elapsed().as_secs_f64(), agents: answers.len(), usage, waited: answers.iter().map(|a| a.waited).sum() });
    *t = Instant::now();
}

pub(crate) fn count_files(dir: &Path) -> usize {
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

/// The code under a folder (its .rs files, not target): every line that is
/// not blank, and how much of the code is duplicated. A line counts as code
/// when it says something (8 characters or more once trimmed, not only marks,
/// not a comment, not an attribute or a `use`); it is duplicated when it is
/// in a run of 3 such lines that comes twice or more anywhere in the code.
pub(crate) fn measure(dir: &Path) -> (usize, usize, usize) {
    fn walk(d: &Path, out: &mut Vec<PathBuf>) {
        for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() && p.file_name().is_some_and(|x| x != "target" && x != ".git") {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }
    let mut files = Vec::new();
    walk(dir, &mut files);
    let (mut lines, mut code): (usize, Vec<Vec<String>>) = (0, Vec::new());
    for f in &files {
        let text = std::fs::read_to_string(f).unwrap_or_default();
        lines += text.lines().filter(|l| !l.trim().is_empty()).count();
        code.push(
            text.lines()
                .map(|l| l.trim().to_string())
                .filter(|l| l.len() >= 8 && l.chars().any(|c| c.is_alphanumeric()) && !l.starts_with("//") && !l.starts_with("#[") && !l.starts_with("use "))
                .collect(),
        );
    }
    let mut seen: std::collections::HashMap<&[String], usize> = std::collections::HashMap::new();
    for f in &code {
        for w in f.windows(3) {
            *seen.entry(w).or_default() += 1;
        }
    }
    let (mut total, mut dup) = (0, 0);
    for f in &code {
        let mut marked = vec![false; f.len()];
        for (i, w) in f.windows(3).enumerate() {
            if seen[w] >= 2 {
                marked[i..i + 3].iter_mut().for_each(|m| *m = true);
            }
        }
        total += f.len();
        dup += marked.iter().filter(|m| **m).count();
    }
    (lines, total, dup)
}

pub(crate) fn finish(mode: String, steps: Vec<Step>, wall: Instant, result: String, dir: PathBuf, split: (f64, f64), code_dir: &Path, mut extra: Vec<(String, String)>) -> Report {
    let mut usage = Usage::default();
    steps.iter().for_each(|s| usage.add(&s.usage));
    let agents = steps.iter().map(|s| s.agents).sum();
    let (lines, code, dup) = measure(code_dir);
    extra.insert(0, ("lines of code".into(), lines.to_string()));
    extra.insert(1, ("duplicated code".into(), format!("{:.1} % ({dup} of {code} lines)", 100.0 * dup as f64 / code.max(1) as f64)));
    let steps_json: Vec<Value> = steps.iter().map(|s| json!({"step": s.name, "seconds": s.seconds, "agents": s.agents, "usage": s.usage, "rate_limit_waits": s.waited})).collect();
    let r = Report { mode, steps, wall: wall.elapsed().as_secs_f64(), agents, usage, result, dir, split: Some(split), extra };
    let extra_json: BTreeMap<&str, &str> = r.extra.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let _ = agent::write(&r.dir.join("summary.json"), &serde_json::to_string_pretty(&json!({"mode": r.mode, "wall": r.wall, "agents": r.agents, "usage": r.usage, "result": r.result, "writing_s": split.0, "testing_s": split.1, "code": extra_json, "steps": steps_json})).unwrap());
    r
}

pub(crate) fn fresh(dir: &Path) -> Result<(), String> {
    if dir.exists() && std::fs::read_dir(dir).map_or(false, |mut d| d.next().is_some()) {
        return Err(format!("{} is not empty", dir.display()));
    }
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    git(dir, &["init", "-q", "-b", "main"]).map(|_| ())
}

pub fn run(o: &Options) -> Result<Report, String> {
    let wall = Instant::now();
    swarm::begin_run();
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let (common, items) = tasks(&o.task);
    let (mode, tag) = match o.how {
        How::Baseline(k) => (format!("{k} Claude Code agents in parallel"), format!("baseline{k}")),
        How::Swarm { per_task, .. } => (format!("Bottle Box, {per_task} minion file(s) a task"), format!("swarm{per_task}")),
        How::Hive => ("Bottle Box hive, every minion a director".to_string(), "hive".to_string()),
        How::Load => ("Bottle Box, minions by workload".to_string(), "load".to_string()),
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
            // a stock Claude Code agent (agent::STOCK, frozen): its own model, settings and tools; only the prompt and the folder are ours
            asks.push(Ask {
                id: format!("agent-{}", g + 1),
                prompt: format!("{common}\n\nYour tasks:\n{}\n\nWork in this folder (it is empty; git is set up). Make it one Rust library crate with one module for each of your tasks and their tests, then run `cargo test --release` until every test passes.", mine.join("\n")),
                model: String::new(),
                keep: false,
                resume: None,
                effort: None,
                want: Want::Text,
                tools: String::new(),
                allow: Vec::new(),
                workdir: Some(d),
                mcp: None,
                max_output: None,
                first_token: None,
                stock: true,
                fork: false,
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
        return Ok(finish(mode, steps, wall, result, run_dir, split, &o.dir, Vec::new()));
    }
    if let How::Hive = o.how {
        return crate::hive::run(o, mode, run_dir, wall);
    }
    if let How::Load = o.how {
        return crate::load::run(o, mode, run_dir, wall);
    }
    let How::Swarm { per_task, merge_minions } = o.how else { unreachable!() };
    crate::workspace::run(o, per_task, merge_minions, mode, run_dir, wall)
}
