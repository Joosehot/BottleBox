//! The hive: no director, every minion is one. The work is split by Bottle
//! Box itself, mechanically (each numbered task: its implementation
//! `src/<module>.rs` and its tests `tests/<module>.rs`; the glue, Cargo.toml
//! and lib.rs, is written here), and then all the minions think together:
//!
//! 1. **Direct, all at once**: every minion directs its own piece and writes
//!    its thinking into the Box. An implementation's minion designs its pub
//!    API (registered in the registry, so it is claimed and nobody else builds
//!    it) and its decisions; a tests' minion its test cases with their exact
//!    answers. Every thought goes on the board.
//! 2. **Build, all at once**: every minion gets the whole Box (the registry
//!    and the board: everyone's thinking, the same text for all, so the
//!    prompt cache holds it once) and its own slice, and writes its file: an
//!    implementation to its registered API, so that its pair's cases pass; the
//!    tests to its pair's API, with its own cases.
//! 3. The tournament merge, the tests, and only the failing tasks back to one
//!    minion each, as before.

use crate::agent::{self, Answer, Ask, Setup, Want};
use crate::clip::Report;
use crate::code::{self, Options};
use crate::shared::{Item, Shared};
use crate::{nimbus, swarm};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

const MINION_OUTPUT: u32 = 6000;

struct Piece {
    task: usize,
    module: String,
    title: String,
    spec: String,
    path: String,
    tests: bool,
}

/// "1. CSV parser: fields, ..." as (the module's name, its title, its spec).
fn module_of(item: &str) -> (String, String, String) {
    let body = item.split_once(". ").map_or(item, |(_, b)| b);
    let (title, spec) = body.split_once(':').map_or((body, ""), |(t, s)| (t, s));
    let mut m = String::new();
    for c in title.trim().chars() {
        if c.is_ascii_alphanumeric() {
            m.push(c.to_ascii_lowercase());
        } else if !m.ends_with('_') && !m.is_empty() {
            m.push('_');
        }
    }
    (m.trim_end_matches('_').to_string(), title.trim().to_string(), spec.trim().to_string())
}

fn notes(text: &str) -> (String, Vec<String>) {
    let (mut kept, mut found) = (Vec::new(), Vec::new());
    for l in text.lines() {
        match l.trim().strip_prefix("=== NOTE:") {
            Some(n) => found.push(n.trim_end_matches('=').trim().to_string()),
            None => kept.push(l),
        }
    }
    (kept.join("\n"), found)
}

pub fn run(o: &Options, mode: String, run_dir: PathBuf, wall: Instant) -> Result<Report, String> {
    code::fresh(&o.dir)?;
    let mut steps = Vec::new();
    let mut t = Instant::now();
    let shared = Shared::open(&run_dir.join("box"))?;
    let (_, items) = code::tasks(&o.task);
    if items.is_empty() {
        return Err("the hive needs a task of numbered items (\"1. ...\")".into());
    }

    // the split and the glue, without an agent
    let mut pieces = Vec::new();
    for (k, item) in items.iter().enumerate() {
        let (module, title, spec) = module_of(item);
        for tests in [false, true] {
            let path = if tests { format!("tests/{module}.rs") } else { format!("src/{module}.rs") };
            pieces.push(Piece { task: k + 1, module: module.clone(), title: title.clone(), spec: spec.clone(), path, tests });
        }
    }
    let modules: Vec<&str> = pieces.iter().filter(|p| !p.tests).map(|p| p.module.as_str()).collect();
    agent::write(&o.dir.join("Cargo.toml"), "[package]\nname = \"tasks\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n")?;
    agent::write(&o.dir.join("src/lib.rs"), &modules.iter().map(|m| format!("pub mod {m};\n")).collect::<String>())?;
    agent::write(&o.dir.join(".gitignore"), "/target\n")?;
    code::commit(&o.dir, "the split: the glue")?;
    code::step(&mut steps, "split (no agent)", &mut t, &[]);

    let rules = "You are one minion of a hive in Bottle Box: there is no director; every minion directs its own piece, and all of them think together in one shared Box (a registry of who builds what, a board of everyone's thinking). Each minion writes only its own file. Rust, edition 2021, standard library only; the crate is `tasks`, one module a task (`tasks::<module>`); tests run with `cargo test --release`. Lean code: the smallest code that does the task well.";
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mcp_for = |who: &str| -> Result<PathBuf, String> {
        let p = run_dir.join("mcp").join(format!("{}.json", who.replace(['/', '\\', ' ', '.'], "_")));
        agent::write(&p, &serde_json::json!({"mcpServers": {"box": {"command": exe, "args": ["box-mcp", "--box", shared.dir, "--as", who]}}}).to_string())?;
        Ok(p)
    };
    let ask = |id: &str, prompt: String, want: Want, effort: &str| -> Result<Ask, String> {
        Ok(Ask { tools: "".into(), mcp: Some(mcp_for(id)?), max_output: Some(MINION_OUTPUT), effort: Some(effort.into()), ..Ask::minion(id, prompt, &o.model, want) })
    };

    // 1. direct, all at once: every minion's thinking into the Box
    let think_file = run_dir.join("think.txt");
    agent::write(&think_file, &format!("{rules}\n\nTHE WHOLE TASK:\n{}\n", o.task))?;
    let setup = Setup { shared: Some(think_file), workdir: run_dir.clone(), read: Vec::new() };
    let asks: Vec<Ask> = pieces
        .iter()
        .map(|p| {
            let prompt = if p.tests {
                format!("You direct {} : the tests of task {} ({}: {}). Think of a few quick test cases with their exact expected answers (what goes in, what must come out), whatever the API will be. Reply with ONE JSON object: {{\"cases\":[\"<input -> exact expected output>\"],\"thoughts\":[\"<at most 2 short lines the others should know>\"]}}", p.path, p.task, p.title, p.spec)
            } else {
                format!("You direct {} : the implementation of task {} ({}: {}). Design its public API: every pub item with its exact Rust signature (module `tasks::{}`), small and lean. Reply with ONE JSON object: {{\"items\":[{{\"name\":\"{}::<item>\",\"kind\":\"fn|struct|enum|trait|const|type|impl\",\"signature\":\"<exact>\"}}],\"budget\":<the lines it needs, at most 130>,\"thoughts\":[\"<at most 3 short lines: its design decisions>\"]}}", p.path, p.task, p.title, p.spec, p.module, p.module)
            };
            ask(&format!("direct {}", p.path), prompt, Want::Json, "medium")
        })
        .collect::<Result<_, _>>()?;
    let directed = swarm::run(&format!("direct: {} minions think together", asks.len()), &setup, asks, o.agents);
    let mut thinkers: Vec<Answer> = Vec::new();
    let mut budget: BTreeMap<String, u64> = BTreeMap::new();
    for (r, p) in directed.into_iter().zip(&pieces) {
        let a = r?;
        for i in a.json["items"].as_array().into_iter().flatten() {
            let item = Item { name: i["name"].as_str().unwrap_or("").into(), kind: i["kind"].as_str().unwrap_or("").into(), signature: i["signature"].as_str().unwrap_or("").into(), file: p.path.clone(), owner: a.id.clone() };
            if !item.name.is_empty() {
                if let Some(other) = shared.claim(&item)? {
                    shared.post(&a.id, &format!("{} was already claimed by {} in {}", item.name, other.owner, other.file))?;
                }
            }
        }
        for c in a.json["cases"].as_array().into_iter().flatten().filter_map(|c| c.as_str()) {
            shared.post(&a.id, &format!("case for {}: {c}", p.module))?;
        }
        for th in a.json["thoughts"].as_array().into_iter().flatten().filter_map(|c| c.as_str()) {
            shared.post(&a.id, th)?;
            shared.note(&a.id, th)?;
        }
        if let Some(b) = a.json["budget"].as_u64() {
            budget.insert(p.module.clone(), b.min(130));
        }
        thinkers.push(a);
    }
    code::step(&mut steps, "direct (every minion, thinking together)", &mut t, &thinkers.iter().collect::<Vec<_>>());

    // 2. build, all at once: everyone sees everyone's thinking
    let registry: String = shared.items().iter().map(|i| format!("{} | {} | {}\n", i.file, i.name, i.signature)).collect();
    let board: String = shared.board().iter().map(|(f, x)| format!("{} : {x}\n", f.trim_start_matches("direct "))).collect();
    let build_file = run_dir.join("build.txt");
    agent::write(&build_file, &format!(
        "{rules}\n\nTHE WHOLE TASK:\n{}\n\nTHE BOX AFTER EVERYONE THOUGHT\nREGISTRY (file | item | signature): who builds what. Build your own items exactly; use the others only as registered; build nothing that is here already. A new shared helper: registry_claim it first.\n{registry}\nBOARD (who : thought or test case):\n{board}\n\
HOW YOU BUILD: write only your own file, within its budget; never read or write another file. Reply with the file only: `=== FILE: <path> ===`, its whole content, `=== END ===`; before it you may give one line `=== NOTE: <what the others should know> ===`.",
        o.task
    ))?;
    let setup = Setup { shared: Some(build_file), workdir: run_dir.clone(), read: Vec::new() };
    let warm = Ask { want: Want::Text, ..ask("warm", "Reply with OK.".into(), Want::Text, "low")? };
    let warmed = swarm::run("warm the shared cache", &setup, vec![warm], 1).pop().unwrap()?;
    code::step(&mut steps, "warm the shared cache", &mut t, &[&warmed]);
    let wt = run_dir.join("wt");
    for i in 0..pieces.len() {
        code::git(&o.dir, &["worktree", "add", "-q", "-b", &format!("piece-{i:03}"), &wt.join(format!("{i:03}")).display().to_string(), "main"])?;
    }
    let asks: Vec<Ask> = pieces
        .iter()
        .map(|p| {
            let prompt = if p.tests {
                format!("Build {} : the tests of task {} (`tasks::{}`), through the API registered for src/{}.rs only, with the cases on the board for {}. At most 50 lines.", p.path, p.task, p.module, p.module, p.module)
            } else {
                format!("Build {} : task {} ({}). Build exactly the items registered for it, so that the cases on the board for {} pass. At most {} lines; no tests in it (its pair writes them).", p.path, p.task, p.title, p.module, budget.get(&p.module).copied().unwrap_or(120))
            };
            ask(&format!("build {}", p.path), prompt, Want::Files, "low")
        })
        .collect::<Result<_, _>>()?;
    let built = swarm::run(&format!("build: {} minions", asks.len()), &setup, asks, o.agents);
    let mut builders: Vec<Answer> = Vec::new();
    for (i, (r, p)) in built.into_iter().zip(&pieces).enumerate() {
        let a = r?;
        let (text, said) = notes(&a.text);
        for n in said {
            shared.post(&a.id, &n)?;
        }
        let got = agent::files_in(&text);
        let content = got.get(&p.path).cloned().or_else(|| (got.len() == 1).then(|| got.values().next().cloned()).flatten()).ok_or(format!("{}: its minion gave another file", p.path))?;
        let at = wt.join(format!("{i:03}"));
        agent::write(&at.join(&p.path), &content)?;
        code::commit(&at, &format!("{}, by its minion", p.path))?;
        builders.push(a);
    }
    code::step(&mut steps, "build (every minion its file)", &mut t, &builders.iter().collect::<Vec<_>>());

    // 3. the tournament merge
    let mut groups: Vec<usize> = (0..pieces.len()).collect();
    let mut rounds = 0;
    while groups.len() > 1 {
        rounds += 1;
        let pairs: Vec<(usize, usize)> = groups.chunks(2).filter(|c| c.len() == 2).map(|c| (c[0], c[1])).collect();
        std::thread::scope(|s| -> Result<(), String> {
            let wt = &wt;
            let hs: Vec<_> = pairs.iter().map(|&(a, b)| s.spawn(move || code::git(&wt.join(format!("{a:03}")), &["merge", "-q", "--no-edit", &format!("piece-{b:03}")]))).collect();
            hs.into_iter().try_for_each(|h| h.join().unwrap().map(|_| ()))
        })?;
        groups = groups.chunks(2).map(|c| c[0]).collect();
    }
    code::git(&o.dir, &["merge", "-q", "--ff-only", &format!("piece-{:03}", groups[0])])?;
    for i in 0..pieces.len() {
        let _ = code::git(&o.dir, &["worktree", "remove", "--force", &wt.join(format!("{i:03}")).display().to_string()]);
    }
    let merge_s = t.elapsed().as_secs_f64();
    code::step(&mut steps, &format!("merge ({rounds} rounds, in pairs)"), &mut t, &[]);

    // 4. tests; only the failing tasks back, each to one minion with its own files
    let files: Vec<String> = pieces.iter().map(|p| p.path.clone()).collect();
    let task_of: BTreeMap<String, usize> = pieces.iter().map(|p| (p.path.clone(), p.task)).collect();
    let mut fixers = Vec::new();
    let mut outcome = code::test(&o.dir);
    agent::write(&run_dir.join("test-0.txt"), &outcome.1)?;
    let mut fix_rounds = 0;
    while !outcome.0 && fix_rounds < code::FIX_ROUNDS {
        fix_rounds += 1;
        let blamed = code::blame(&outcome.1, &files);
        if blamed.is_empty() {
            eprintln!("  test: failures no file can be blamed for (see test-{}.txt)", fix_rounds - 1);
            break;
        }
        let mut by_task: BTreeMap<usize, Vec<String>> = BTreeMap::new();
        for (f, errs) in &blamed {
            by_task.entry(task_of.get(f).copied().unwrap_or(0)).or_default().extend(errs.iter().map(|e| format!("in {f}: {e}")));
        }
        println!("  test round {fix_rounds}: tasks {:?} fail", by_task.keys().collect::<Vec<_>>());
        let owned: Vec<(usize, Vec<String>)> = by_task.keys().map(|k| (*k, files.iter().filter(|f| task_of.get(*f) == Some(k)).cloned().collect())).collect();
        let asks: Vec<Ask> = owned
            .iter()
            .map(|(k, mine)| ask(&format!("fix task {k}"), format!(
                "You own {} (task {k}) and only them. The build or the tests fail:\n{}\n\nYour files now:\n{}\nFix them, keeping the registered items. Do not change an expected answer to match the output unless you are sure it was wrong. Reply with the files you changed, each as `=== FILE: <path> ===` and its whole content, the last ended by `=== END ===`.",
                mine.join(", "), by_task[k].join("\n\n"), code::bundle(&o.dir, mine)
            ), Want::Files, "low"))
            .collect::<Result<_, _>>()?;
        let fixed = swarm::run(&format!("fix round {fix_rounds}: {} minions", asks.len()), &setup, asks, o.agents);
        for (r, (_, mine)) in fixed.into_iter().zip(&owned) {
            let a = r?;
            for (path, content) in agent::files_in(&notes(&a.text).0).iter().filter(|(p, _)| mine.contains(p)) {
                agent::write(&o.dir.join(path), content)?;
            }
            fixers.push(a);
        }
        code::commit(&o.dir, &format!("test round {fix_rounds}: the failing tasks fixed"))?;
        outcome = code::test(&o.dir);
        agent::write(&run_dir.join(format!("test-{fix_rounds}.txt")), &outcome.1)?;
    }
    code::step(&mut steps, &format!("test ({fix_rounds} fix rounds)"), &mut t, &fixers.iter().collect::<Vec<_>>());

    for e in std::fs::read_dir(&shared.dir).into_iter().flatten().flatten().filter(|e| e.path().is_file()) {
        if let Ok(b) = std::fs::read(e.path()) {
            nimbus::put_bytes(&b)?;
        }
    }
    let (ok, _, passed, failed) = outcome;
    let result = format!("{} ({passed} tests passed, {failed} failed; {} source files; registry {} items; board {} posts)", if ok { "ALL TESTS PASS" } else { "TESTS FAIL" }, code::count_files(&o.dir), shared.items().len(), shared.board().len());
    let writing: f64 = steps.iter().filter(|s| !s.name.starts_with("test")).map(|s| s.seconds).sum();
    let testing: f64 = steps.iter().filter(|s| s.name.starts_with("test")).map(|s| s.seconds).sum();
    let _ = Value::Null;
    Ok(code::finish(mode, steps, wall, result, run_dir, (writing, testing), &o.dir, vec![("merge time".into(), format!("{merge_s:.1} s"))]))
}
