//! Minions by workload, and a streaming plan.
//!
//! 1. **Think together, one director a task**: every task's director (all at
//!    once) decides its whole task in one mind: the public API (registered in
//!    the Box), its conventions, the test cases with their exact answers in
//!    that very API, and the size of the work; a big implementation it splits
//!    into separate sections (separate items). One mind for a task's API and
//!    its tests: two minds thinking apart chose different conventions (task 6
//!    of the benchmark: `DDDD` against `dddd`, a 6-argument call made with 3).
//! 2. **Minions by workload**: a small task gets one minion (both its files),
//!    a usual one two (the implementation, the tests), a big one a minion for
//!    each section and one for the tests; at most 20 in all, only as many as
//!    the work needs.
//! 3. **Streaming**: a task's minions start as soon as its director is done,
//!    not when every director is; the shared prefix is the cached one (the
//!    first agent starts alone, the others at its first token).
//! 4. Each task is tested on its own as soon as its files are written; a
//!    failing task goes with its error to one minion (it owns that task's
//!    files), and blocks nothing else. Then the tournament merge and the
//!    tests of the whole crate.

use crate::agent::{self, Answer, Ask, Setup, Usage, Want};
use crate::clip::{Report, Step};
use crate::code::{self, Options};
use crate::hive::{module_of, notes};
use crate::shared::{Item, Shared};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Instant;

const MINION_OUTPUT: u32 = 6000;
const MAX_MINIONS: usize = 20;
/// an implementation of at most this many lines and its tests go to one minion
const SMALL: u64 = 70;
/// above this many lines an implementation is split into sections
const BIG: u64 = 150;
const TRIES: usize = 3;

struct Task {
    n: usize,
    module: String,
    title: String,
    spec: String,
}

/// One minion's piece: a section of a task's implementation, its tests, or both files of a small task.
#[derive(Clone)]
struct Piece {
    /// "impl k of m", "tests", "whole"
    kind: String,
    section: usize,
    sections: usize,
    items: Vec<String>,
    lines: u64,
}

/// Joins a file's sections: their `use` lines once at the top, then the sections in order.
fn join(parts: &[String]) -> String {
    let (mut uses, mut body): (Vec<String>, Vec<String>) = (Vec::new(), Vec::new());
    for p in parts {
        let mut rest = Vec::new();
        for l in p.lines() {
            if l.starts_with("use ") {
                if !uses.contains(&l.to_string()) {
                    uses.push(l.to_string());
                }
            } else {
                rest.push(l);
            }
        }
        body.push(rest.join("\n").trim().to_string());
    }
    let head = if uses.is_empty() { String::new() } else { format!("{}\n\n", uses.join("\n")) };
    format!("{head}{}\n", body.into_iter().filter(|b| !b.is_empty()).collect::<Vec<_>>().join("\n\n"))
}

enum Event {
    Directed(usize, Result<Answer, String>, f64),
    Built(usize, usize, Result<Answer, String>, f64, f64),
    Tested(usize, bool, String),
    Fixed(usize, Result<Answer, String>, f64, f64),
}

pub fn run(o: &Options, mode: String, run_dir: PathBuf, wall: Instant) -> Result<Report, String> {
    code::fresh(&o.dir)?;
    let mut steps = Vec::new();
    let mut t = Instant::now();
    let shared = Shared::open(&run_dir.join("box"))?;
    let (_, items) = code::tasks(&o.task);
    if items.is_empty() {
        return Err("needs a task of numbered items (\"1. ...\")".into());
    }
    let tasks: Vec<Task> = items.iter().enumerate().map(|(k, i)| {
        let (module, title, spec) = module_of(i);
        Task { n: k + 1, module, title, spec }
    }).collect();
    let cargo_toml = "[package]\nname = \"tasks\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n";
    agent::write(&o.dir.join("Cargo.toml"), cargo_toml)?;
    agent::write(&o.dir.join("src/lib.rs"), &tasks.iter().map(|t| format!("pub mod {};\n", t.module)).collect::<String>())?;
    agent::write(&o.dir.join(".gitignore"), "/target\n")?;
    code::commit(&o.dir, "the split: the glue")?;
    // a worktree and a small crate (to test it alone) for each task
    let wt = run_dir.join("wt");
    for (k, task) in tasks.iter().enumerate() {
        code::git(&o.dir, &["worktree", "add", "-q", "-b", &format!("task-{k:03}"), &wt.join(format!("{k:03}")).display().to_string(), "main"])?;
        let d = run_dir.join("check").join(&task.module);
        agent::write(&d.join("Cargo.toml"), cargo_toml)?;
        agent::write(&d.join("src/lib.rs"), &format!("pub mod {};\n", task.module))?;
    }
    code::step(&mut steps, "split and glue (no agent)", &mut t, &[]);

    // the shared prefix: the rules and the whole task, the same text for every agent (cached once)
    let rules = "You are one minion of a hive in Bottle Box: many minions build one crate together at the same time, sharing one Box (a registry of who builds what, a board). Each minion writes only its own piece. Rust, edition 2021, standard library only; the crate is `tasks`, one module a task (`tasks::<module>`); tests run with `cargo test --release`. Lean code: the smallest code that does the task well. Rust has no default arguments: every call gives every argument of the registered signature.";
    let prefix = run_dir.join("prefix.txt");
    agent::write(&prefix, &format!("{rules}\n\nTHE WHOLE TASK:\n{}\n", o.task))?;
    let setup = Setup { shared: Some(prefix), workdir: run_dir.clone(), read: Vec::new() };
    let ask = |id: &str, prompt: String, want: Want, effort: &str| -> Ask { Ask { tools: "".into(), max_output: Some(MINION_OUTPUT), effort: Some(effort.into()), ..Ask::minion(id, prompt, &o.model, want) } };
    let src = |m: &str| format!("src/{m}.rs");
    let tst = |m: &str| format!("tests/{m}.rs");

    let began = Instant::now();
    let now = || began.elapsed().as_secs_f64();
    let mut log: Vec<String> = Vec::new();
    let say = |line: String, log: &mut Vec<String>| {
        let l = format!("[{:6.1} s] {line}", now());
        println!("  {l}");
        log.push(l);
    };
    let (tx, rx) = mpsc::channel::<Event>();
    let mut thinkers: Vec<Answer> = Vec::new();
    let mut builders: Vec<Answer> = Vec::new();
    let mut fixers: Vec<Answer> = Vec::new();
    let mut plans: BTreeMap<usize, Value> = BTreeMap::new();
    let mut pieces: BTreeMap<usize, Vec<Piece>> = BTreeMap::new();
    let mut parts: BTreeMap<usize, BTreeMap<usize, String>> = BTreeMap::new();
    let mut files: BTreeMap<usize, BTreeMap<String, String>> = BTreeMap::new();
    let mut tries: BTreeMap<usize, usize> = BTreeMap::new();
    let mut passed: BTreeMap<usize, bool> = BTreeMap::new();
    let mut minions = 0usize;
    let mut waits: Vec<f64> = Vec::new();
    let mut thinking_s = 0.0;
    let mut pending = 0usize;
    let warm = agent::Signal::default();

    std::thread::scope(|s| -> Result<(), String> {
        // 1. think together: one director a task, all at once (the first alone until its first token)
        for (k, task) in tasks.iter().enumerate() {
            let prompt = format!(
                "You direct task {} ({}: {}), module `tasks::{}`. Think it through once and decide it whole, for the minions who will build it: its public API (every pub item with its exact Rust signature), its conventions (names, formats, error values), a few test cases with exact answers written as calls of exactly that API, and the size of its implementation. If the implementation needs more than about {BIG} lines, split it into 2 or 3 sections, each a separate set of its items (the types and constants in the first). Reply with ONE JSON object: \
{{\"items\":[{{\"name\":\"{}::<item>\",\"kind\":\"fn|struct|enum|trait|const|type|impl\",\"signature\":\"<exact>\"}}],\"conventions\":\"<at most 3 short lines>\",\"cases\":[\"<a call of the API -> its exact result: at most 5>\"],\"lines\":<the implementation's lines>,\"sections\":[{{\"items\":[\"<item names>\"],\"lines\":<lines>}}]}}",
                task.n, task.title, task.spec, task.module, task.module
            );
            let mut a = ask(&format!("direct task {}", task.n), prompt, Want::Json, "medium");
            if k == 0 {
                a.first_token = Some(warm.clone());
            }
            let tx = tx.clone();
            let setup = &setup;
            let t0 = now();
            s.spawn(move || {
                let r = agent::run(setup, &a, &|_| {});
                let _ = tx.send(Event::Directed(k, r, t0));
            });
            pending += 1;
            if k == 0 {
                let (lock, cv) = &*warm;
                let up = lock.lock().unwrap();
                let _ = cv.wait_timeout_while(up, std::time::Duration::from_secs(30), |up| !*up).unwrap();
                say("the shared prefix is cached: the other directors start".into(), &mut log);
            }
        }

        let test_task = |k: usize, tx: mpsc::Sender<Event>| {
            let d = run_dir.join("check").join(&tasks[k].module);
            s.spawn(move || {
                let (ok, text, _, _) = code::test(&d);
                let _ = tx.send(Event::Tested(k, ok, text));
            });
        };
        let place = |k: usize, path: &str, content: &str| -> Result<(), String> {
            agent::write(&wt.join(format!("{k:03}")).join(path), content)?;
            agent::write(&run_dir.join("check").join(&tasks[k].module).join(path), content)
        };

        while pending > 0 {
            let ev = rx.recv().map_err(|e| e.to_string())?;
            pending -= 1;
            match ev {
                Event::Directed(k, r, _t0) => {
                    let a = r?;
                    let task = &tasks[k];
                    let plan = a.json.clone();
                    // the task's API registered, its cases and conventions on the board (append only)
                    for i in plan["items"].as_array().into_iter().flatten() {
                        let item = Item { name: i["name"].as_str().unwrap_or("").into(), kind: i["kind"].as_str().unwrap_or("").into(), signature: i["signature"].as_str().unwrap_or("").into(), file: src(&task.module), owner: a.id.clone() };
                        if !item.name.is_empty() {
                            shared.claim(&item)?;
                        }
                    }
                    shared.post(&a.id, &format!("conventions of {}: {}", task.module, plan["conventions"].as_str().unwrap_or("")))?;
                    for c in plan["cases"].as_array().into_iter().flatten().filter_map(|c| c.as_str()) {
                        shared.post(&a.id, &format!("case for {}: {c}", task.module))?;
                    }
                    // 2. the minions this task needs, by its size
                    let lines = plan["lines"].as_u64().unwrap_or(100);
                    let secs: Vec<(Vec<String>, u64)> = plan["sections"].as_array().into_iter().flatten().map(|x| (x["items"].as_array().into_iter().flatten().filter_map(|i| i.as_str().map(String::from)).collect(), x["lines"].as_u64().unwrap_or(60))).filter(|x: &(Vec<String>, u64)| !x.0.is_empty()).collect();
                    let room = MAX_MINIONS.saturating_sub(minions).max(1);
                    let mine: Vec<Piece> = if lines <= SMALL || room == 1 {
                        vec![Piece { kind: "whole".into(), section: 0, sections: 1, items: Vec::new(), lines }]
                    } else if lines > BIG && secs.len() >= 2 && room >= secs.len() + 1 {
                        let n = secs.len();
                        let mut v: Vec<Piece> = secs.iter().enumerate().map(|(j, (its, l))| Piece { kind: format!("impl {} of {n}", j + 1), section: j, sections: n, items: its.clone(), lines: *l }).collect();
                        v.push(Piece { kind: "tests".into(), section: 0, sections: n, items: Vec::new(), lines: 50 });
                        v
                    } else {
                        vec![
                            Piece { kind: "impl 1 of 1".into(), section: 0, sections: 1, items: Vec::new(), lines },
                            Piece { kind: "tests".into(), section: 0, sections: 1, items: Vec::new(), lines: 50 },
                        ]
                    };
                    minions += mine.len();
                    say(format!("task {} directed: about {lines} lines -> {} minion(s): {}", task.n, mine.len(), mine.iter().map(|p| p.kind.clone()).collect::<Vec<_>>().join(", ")), &mut log);
                    plans.insert(k, plan.clone());
                    pieces.insert(k, mine.clone());
                    thinkers.push(a);
                    thinking_s = now();
                    // 3. streaming: its minions start now
                    let reg: String = shared.items().iter().filter(|i| i.file == src(&task.module)).map(|i| format!("  {}\n", i.signature)).collect();
                    let slice = format!(
                        "THE BOX, task {} ({}): module `tasks::{}`\nregistered API:\n{reg}conventions: {}\ncases:\n{}",
                        task.n, task.title, task.module, plan["conventions"].as_str().unwrap_or(""),
                        plan["cases"].as_array().into_iter().flatten().filter_map(|c| c.as_str()).map(|c| format!("  {c}\n")).collect::<String>()
                    );
                    for (j, p) in mine.iter().enumerate() {
                        let prompt = match p.kind.as_str() {
                            "whole" => format!("{slice}\nBuild both files of this task: {} (exactly the registered API; no tests in it) and {} (a few quick tests with the cases, `use tasks::{}::*;`). At most {} lines and 50 lines. Reply with the two files: each a line `=== FILE: <path> ===` and its whole content, the last ended by `=== END ===`.", src(&task.module), tst(&task.module), task.module, p.lines.max(60), ),
                            "tests" => format!("{slice}\nBuild {} : a few quick tests with the cases above, calling the registered API exactly (`use tasks::{}::*;`). At most 50 lines. Reply with the file only: `=== FILE: {} ===`, its whole content, `=== END ===`.", tst(&task.module), task.module, tst(&task.module)),
                            _ if p.sections == 1 => format!("{slice}\nBuild {} : exactly the registered API, so that the cases pass; no tests in it (another minion writes them). At most {} lines. Reply with the file only: `=== FILE: {} ===`, its whole content, `=== END ===`.", src(&task.module), p.lines.max(60), src(&task.module)),
                            _ => format!("{slice}\nBuild section {} of {} of {} : the code of only these items: {}. The other sections are written at the same time by other minions; use their items by name, do not write them. Put the `use` lines it needs at its top. At most {} lines. Reply with your section only: `=== FILE: {}#{} ===`, its content, `=== END ===`.", p.section + 1, p.sections, src(&task.module), p.items.join(", "), p.lines.max(40), src(&task.module), p.section + 1),
                        };
                        let a = ask(&format!("build task {} {}", task.n, p.kind), prompt, Want::Files, "low");
                        let tx = tx.clone();
                        let setup = &setup;
                        let ready = now();
                        s.spawn(move || {
                            let t0 = now();
                            let r = agent::run(setup, &a, &|_| {});
                            let _ = tx.send(Event::Built(k, j, r, t0 - ready, now()));
                        });
                        pending += 1;
                    }
                }
                Event::Built(k, j, r, waited, _t1) => {
                    waits.push(waited);
                    let a = r?;
                    let task = &tasks[k];
                    let p = pieces[&k][j].clone();
                    let got = agent::files_in(&notes(&a.text).0);
                    let first = got.values().next().cloned().unwrap_or_default();
                    match p.kind.as_str() {
                        "whole" => {
                            for path in [src(&task.module), tst(&task.module)] {
                                if let Some(c) = got.get(&path) {
                                    files.entry(k).or_default().insert(path.clone(), c.clone());
                                    place(k, &path, c)?;
                                }
                            }
                        }
                        "tests" => {
                            let c = got.get(&tst(&task.module)).cloned().unwrap_or(first);
                            files.entry(k).or_default().insert(tst(&task.module), c.clone());
                            place(k, &tst(&task.module), &c)?;
                        }
                        _ => {
                            parts.entry(k).or_default().insert(p.section, first);
                            if parts[&k].len() == p.sections {
                                let c = join(&parts[&k].values().cloned().collect::<Vec<_>>());
                                files.entry(k).or_default().insert(src(&task.module), c.clone());
                                place(k, &src(&task.module), &c)?;
                            }
                        }
                    }
                    builders.push(a);
                    if files.get(&k).is_some_and(|f| f.contains_key(&src(&task.module)) && f.contains_key(&tst(&task.module))) && !passed.contains_key(&k) && tries.get(&k).is_none() {
                        tries.insert(k, 0);
                        test_task(k, tx.clone());
                        pending += 1;
                    }
                }
                Event::Tested(k, ok, text) => {
                    let task = &tasks[k];
                    if ok {
                        passed.insert(k, true);
                        say(format!("task {} passes", task.n), &mut log);
                        continue;
                    }
                    let t = tries.entry(k).or_insert(0);
                    *t += 1;
                    let owned = vec![src(&task.module), tst(&task.module)];
                    if *t > TRIES {
                        passed.insert(k, false);
                        say(format!("task {} still fails after {TRIES} fixes", task.n), &mut log);
                        continue;
                    }
                    let errs: Vec<String> = code::blame(&text, &owned).into_iter().flat_map(|(f, e)| e.into_iter().map(move |x| format!("{f}: {x}"))).collect();
                    let error = if errs.is_empty() { text.lines().filter(|l| l.contains("error") || l.contains("FAILED") || l.contains("panicked")).take(8).collect::<Vec<_>>().join("\n") } else { errs.join("\n") };
                    say(format!("task {} fails: {}", task.n, error.lines().next().unwrap_or("").chars().take(150).collect::<String>()), &mut log);
                    // back to one minion, with only its task's files: nothing else waits
                    let current = code::bundle(&run_dir.join("check").join(&task.module), &owned);
                    let prompt = format!("You own {} (task {}) and only them. The build or its tests fail:\n{error}\n\nThe files now:\n{current}\nFix them, keeping the registered API. Do not change an expected answer to match the output unless you are sure it was wrong. Reply with the files you changed, each as `=== FILE: <path> ===` and its whole content, the last ended by `=== END ===`.", owned.join(", "), task.n);
                    let a = ask(&format!("fix task {}", task.n), prompt, Want::Files, "low");
                    shared.post("hive", &format!("task {} back to one minion to fix", task.n))?;
                    let tx = tx.clone();
                    let setup = &setup;
                    s.spawn(move || {
                        let t0 = now();
                        let r = agent::run(setup, &a, &|_| {});
                        let _ = tx.send(Event::Fixed(k, r, t0, now()));
                    });
                    pending += 1;
                }
                Event::Fixed(k, r, _t0, _t1) => {
                    let a = r?;
                    let task = &tasks[k];
                    for (path, c) in agent::files_in(&notes(&a.text).0) {
                        if path == src(&task.module) || path == tst(&task.module) {
                            place(k, &path, &c)?;
                        }
                    }
                    fixers.push(a);
                    test_task(k, tx.clone());
                    pending += 1;
                }
            }
        }
        Ok(())
    })?;
    let build_s = now() - thinking_s;
    for (k, task) in tasks.iter().enumerate() {
        code::commit(&wt.join(format!("{k:03}")), &format!("task {}: {}", task.n, task.module))?;
    }
    let mut tu = Usage::default();
    thinkers.iter().for_each(|a| tu.add(&a.usage));
    steps.push(Step { name: format!("think ({} directors, one a task)", thinkers.len()), seconds: thinking_s, agents: thinkers.len(), usage: tu, waited: 0.0 });
    let all: Vec<&Answer> = builders.iter().chain(&fixers).collect();
    t = Instant::now();
    let mut bu = Usage::default();
    all.iter().for_each(|a| bu.add(&a.usage));
    steps.push(Step { name: format!("build ({} minions, {} fixes; streaming)", builders.len(), fixers.len()), seconds: build_s, agents: all.len(), usage: bu, waited: 0.0 });
    for a in thinkers.iter().chain(all.iter().copied()) {
        agent::write(&run_dir.join("pieces").join(format!("{}.json", a.id.replace(['/', ' ', '.'], "_"))), &serde_json::to_string_pretty(a).unwrap())?;
    }
    let _ = std::fs::write(run_dir.join("hive.log"), log.join("\n"));

    // the tournament merge of the tasks' branches
    let mut groups: Vec<usize> = (0..tasks.len()).collect();
    let mut rounds = 0;
    while groups.len() > 1 {
        rounds += 1;
        let pairs: Vec<(usize, usize)> = groups.chunks(2).filter(|c| c.len() == 2).map(|c| (c[0], c[1])).collect();
        std::thread::scope(|s| -> Result<(), String> {
            let wt = &wt;
            let hs: Vec<_> = pairs.iter().map(|&(a, b)| s.spawn(move || code::git(&wt.join(format!("{a:03}")), &["merge", "-q", "--no-edit", &format!("task-{b:03}")]))).collect();
            hs.into_iter().try_for_each(|h| h.join().unwrap().map(|_| ()))
        })?;
        groups = groups.chunks(2).map(|c| c[0]).collect();
    }
    code::git(&o.dir, &["merge", "-q", "--ff-only", &format!("task-{:03}", groups[0])])?;
    for k in 0..tasks.len() {
        let _ = code::git(&o.dir, &["worktree", "remove", "--force", &wt.join(format!("{k:03}")).display().to_string()]);
    }
    let merge_s = t.elapsed().as_secs_f64();
    code::step(&mut steps, &format!("merge ({rounds} rounds, in pairs)"), &mut t, &[]);
    let outcome = code::test(&o.dir);
    agent::write(&run_dir.join("test-whole.txt"), &outcome.1)?;
    code::step(&mut steps, "test the whole crate", &mut t, &[]);

    let (ok, _, n_passed, n_failed) = outcome;
    let busy: f64 = builders.iter().chain(&fixers).map(|a| a.seconds).sum();
    let waited_most = waits.iter().cloned().fold(0.0, f64::max);
    let result = format!("{} ({n_passed} tests passed, {n_failed} failed; {} source files; {} fixes; registry {} items; board {} posts)", if ok { "ALL TESTS PASS" } else { "TESTS FAIL" }, code::count_files(&o.dir), fixers.len(), shared.items().len(), shared.board().len());
    let writing: f64 = steps.iter().filter(|s| !s.name.starts_with("test")).map(|s| s.seconds).sum();
    let testing: f64 = steps.iter().filter(|s| s.name.starts_with("test")).map(|s| s.seconds).sum();
    let per_task: String = tasks.iter().enumerate().map(|(k, t)| format!("{}: {}", t.module, pieces.get(&k).map_or(0, |p| p.len()))).collect::<Vec<_>>().join(", ");
    Ok(code::finish(mode, steps, wall, result, run_dir, (writing, testing), &o.dir, vec![
        ("thinking time".into(), format!("{thinking_s:.1} s")),
        ("build time".into(), format!("{build_s:.1} s")),
        ("minions used".into(), format!("{minions} ({per_task})")),
        ("idle".into(), format!("{waited_most:.1} s at most from a task's plan to its minion starting; {busy:.0} minion-seconds of work")),
        ("merge time".into(), format!("{merge_s:.1} s")),
    ]))
}
