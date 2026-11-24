//! The swarm as a shared workspace: the agents work in one Box (`shared.rs`).
//!
//! 1. **Think once, interfaces first**: the director (thinking hard) writes
//!    the architecture and its reasons, the conventions and the decisions into
//!    the Box, and registers every shared type and every pub signature of
//!    every file before anyone writes code; it writes the small glue
//!    (Cargo.toml, lib.rs) itself.
//! 2. **Write**: one minion a file, all at once, each in its own git worktree.
//!    A minion gets a compact summary (the same for all, so the prompt cache
//!    holds it once: it is warmed by one small call before the minions start)
//!    and only its own slice: its file's spec, budget and items. It never
//!    reads another file; it uses other files only through the registry. A
//!    new helper is claimed in the registry (through the Box's MCP tools)
//!    before it is built, and one already claimed is used instead. Thoughts
//!    and blockers go to the board. Its output is capped.
//! 3. **Merge** in pairs like a tournament; the pieces are independent, so git
//!    joins them alone.
//! 4. **Test** after writing; only the failing tasks go back, each to one
//!    minion with its errors and only its own files.

use crate::agent::{self, Answer, Ask, Setup, Want};
use crate::clip::Report;
use crate::code::{self, Options};
use crate::shared::{Item, Shared};
use crate::{nimbus, swarm};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

/// the most a minion may write in one turn (tokens)
const MINION_OUTPUT: u32 = 6000;

/// An answer's notes (`=== NOTE: ... ===`) taken out of its text.
fn notes(text: &str) -> (String, Vec<String>) {
    let mut kept = Vec::new();
    let mut found = Vec::new();
    for l in text.lines() {
        match l.trim().strip_prefix("=== NOTE:") {
            Some(n) => found.push(n.trim_end_matches('=').trim().to_string()),
            None => kept.push(l),
        }
    }
    (kept.join("\n"), found)
}

pub fn run(o: &Options, per_task: usize, _merge_minions: bool, mode: String, run_dir: PathBuf, wall: Instant) -> Result<Report, String> {
    code::fresh(&o.dir)?;
    let mut steps = Vec::new();
    let mut t = Instant::now();
    let shared = Shared::open(&run_dir.join("box"))?;

    // 1. think once: the director writes the Box
    let rules = "You are part of Bottle Box, a shared workspace: many Claude agents work on one task at once, in one Box (the plan, the decisions, the registry of who built what, a board). The director plans once; each minion then writes only its own file. Rust, edition 2021, standard library only (no dependencies); tests run with `cargo test --release`.";
    let task_file = run_dir.join("task.txt");
    agent::write(&task_file, &format!("{rules}\n\nTASK:\n{}\n", o.task))?;
    let setup = Setup { shared: Some(task_file), workdir: run_dir.clone(), read: Vec::new() };
    let split = match per_task {
        1 => "one file for each task: src/<module>.rs with the implementation and a few unit tests".to_string(),
        _ => "two files for each task: the implementation (src/<module>.rs: the pub API, no tests) and its tests (tests/<module>.rs: a few quick integration tests through the pub API only)".to_string(),
    };
    let think = Ask {
        id: "director".into(),
        prompt: format!(
            "You are the director. Think it through once, then write it into the Box, compactly: signatures and one-line specs, no prose beyond what is asked. Split the work into {split}. \
Interfaces first: register every pub item of every file (its exact Rust signature) and every shared type; each minion builds only its own file and sees only the registry and its own spec, never another file. \
No duplication: anything two files need lives once, registered, in one file. Lean code: the smallest code that does each task well; give every file a line budget (an implementation about 80 to 130 lines, its tests about 25 to 50). \
You write the glue yourself: Cargo.toml and src/lib.rs (declaring every module). Reply with ONE JSON object: \
{{\"architecture\":\"<the reasoning, at most 8 short lines>\",\"conventions\":\"<at most 5 lines>\",\"decisions\":[\"<one line each>\"],\"glue\":[{{\"path\":\"Cargo.toml\",\"content\":\"...\"}},{{\"path\":\"src/lib.rs\",\"content\":\"...\"}}],\
\"registry\":[{{\"name\":\"module::item\",\"kind\":\"fn|struct|enum|trait|const|type|impl\",\"signature\":\"<exact>\",\"file\":\"<path>\"}}],\
\"files\":[{{\"path\":\"...\",\"task\":<number>,\"role\":\"impl|tests\",\"budget\":<lines>,\"spec\":\"<at most 3 lines>\",\"tests\":\"<its tests and their exact answers, for a tests file>\"}}]}}"
        ),
        model: o.model.clone(),
        keep: false,
        resume: None,
        effort: Some("high".into()),
        want: Want::Json,
        tools: "".into(),
        allow: Vec::new(),
        workdir: None,
        mcp: None,
        max_output: None, first_token: None,
        stock: false,
        fork: false,
    };
    let plan_answer = swarm::run("think: the director writes the Box", &setup, vec![think], 1).pop().unwrap()?;
    let plan = plan_answer.json.clone();
    agent::write(&shared.dir.join("plan.json"), &serde_json::to_string_pretty(&plan).unwrap())?;
    for d in plan["decisions"].as_array().into_iter().flatten().filter_map(|d| d.as_str()) {
        shared.note("director", d)?;
    }
    // interfaces first: every item registered, owned by its file
    for i in plan["registry"].as_array().into_iter().flatten() {
        let item = Item { name: i["name"].as_str().unwrap_or("").into(), kind: i["kind"].as_str().unwrap_or("").into(), signature: i["signature"].as_str().unwrap_or("").into(), file: i["file"].as_str().unwrap_or("").replace('\\', "/"), owner: "director".into() };
        if !item.name.is_empty() {
            shared.claim(&item)?;
        }
    }
    let specs: Vec<Value> = plan["files"].as_array().cloned().unwrap_or_default();
    let files: Vec<String> = specs.iter().filter_map(|f| f["path"].as_str().map(|p| p.replace('\\', "/"))).collect();
    if files.len() < 2 {
        return Err("the director's plan has fewer than 2 files".into());
    }
    for g in plan["glue"].as_array().into_iter().flatten() {
        if let (Some(p), Some(c)) = (g["path"].as_str(), g["content"].as_str()) {
            agent::write(&o.dir.join(p), c)?;
        }
    }
    agent::write(&o.dir.join(".gitignore"), "/target\n")?;
    code::commit(&o.dir, "the director's glue")?;
    println!("  the Box: {} files, {} registered items", files.len(), shared.items().len());
    code::step(&mut steps, "think (director, interfaces first)", &mut t, &[&plan_answer]);

    // the compact summary every minion shares (the same text for all: cached once)
    let registry: String = shared.items().iter().map(|i| format!("{} | {} | {}\n", i.file, i.name, i.signature)).collect();
    let summary = format!(
        "{rules}\n\nTASK (summary): {}\n\nARCHITECTURE:\n{}\n\nCONVENTIONS:\n{}\n\nDECISIONS:\n{}\n\nREGISTRY (who builds what: file | item | signature). Use these; never build again what is here. A new helper that another file might need: registry_claim it first.\n{registry}\n\
HOW A MINION WORKS: you get your file's slice below; write only your file, within its budget; do not read or write any other file. If something blocks you, board_post it (one line). \
Reply with the file only: a line `=== FILE: <path> ===`, its whole content, then `=== END ===`; before it you may give one line `=== NOTE: <a decision the others should know> ===`.",
        o.task.lines().next().unwrap_or(""),
        plan["architecture"].as_str().unwrap_or(""),
        plan["conventions"].as_str().unwrap_or(""),
        plan["decisions"].as_array().into_iter().flatten().filter_map(|d| d.as_str()).collect::<Vec<_>>().join("\n")
    );
    let summary_file = run_dir.join("summary-for-minions.txt");
    agent::write(&summary_file, &summary)?;
    let setup = Setup { shared: Some(summary_file), workdir: run_dir.clone(), read: Vec::new() };
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mcp_for = |who: &str| -> Result<PathBuf, String> {
        let p = run_dir.join("mcp").join(format!("{}.json", who.replace(['/', '\\', ' ', '.'], "_")));
        let cfg = serde_json::json!({"mcpServers": {"box": {"command": exe, "args": ["box-mcp", "--box", shared.dir, "--as", who]}}});
        agent::write(&p, &cfg.to_string())?;
        Ok(p)
    };
    let minion = |id: &str, prompt: String| -> Result<Ask, String> { Ok(Ask { tools: "".into(), mcp: Some(mcp_for(id)?), max_output: Some(MINION_OUTPUT), ..Ask::minion(id, prompt, &o.model, Want::Files) }) };

    // the cache warmed once, so that every minion reads the summary from it
    let warm = Ask { want: Want::Text, ..minion("warm", "Reply with OK.".into())? };
    let warmed = swarm::run("warm the cache", &setup, vec![warm], 1).pop().unwrap()?;
    code::step(&mut steps, "warm the shared cache", &mut t, &[&warmed]);

    // 2. write: one minion a file, each with only its slice
    let wt = run_dir.join("wt");
    for i in 0..files.len() {
        code::git(&o.dir, &["worktree", "add", "-q", "-b", &format!("piece-{i:03}"), &wt.join(format!("{i:03}")).display().to_string(), "main"])?;
    }
    let items = shared.items();
    let slice = |s: &Value| -> String {
        let path = s["path"].as_str().unwrap_or("").replace('\\', "/");
        let mine: String = items.iter().filter(|i| i.file == path).map(|i| format!("  {} {}\n", i.kind, i.signature)).collect();
        format!(
            "Your piece: you own {path} ({} of task {}) and only it. Budget: {} lines at most.\nSpec: {}\n{}{}",
            s["role"].as_str().unwrap_or("impl"),
            s["task"],
            s["budget"],
            s["spec"].as_str().unwrap_or(""),
            if mine.is_empty() { String::new() } else { format!("Build exactly these registered items:\n{mine}") },
            s["tests"].as_str().filter(|x| !x.is_empty()).map_or(String::new(), |x| format!("Tests: {x}\n"))
        )
    };
    let asks: Vec<Ask> = specs.iter().zip(&files).map(|(s, f)| minion(&format!("write {f}"), slice(s))).collect::<Result<_, _>>()?;
    let written = swarm::run(&format!("write: {} minions", asks.len()), &setup, asks, o.agents);
    let mut writers: Vec<Answer> = Vec::new();
    for (i, (r, f)) in written.into_iter().zip(&files).enumerate() {
        let a = r?;
        let (text, said) = notes(&a.text);
        for n in said {
            shared.note(&a.id, &n)?;
            shared.post(&a.id, &n)?;
        }
        let got = agent::files_in(&text);
        let content = got.get(f).cloned().or_else(|| (got.len() == 1).then(|| got.values().next().cloned()).flatten()).ok_or(format!("{f}: its minion gave another file ({:?})", got.keys().collect::<Vec<_>>()))?;
        let at = wt.join(format!("{i:03}"));
        agent::write(&at.join(f), &content)?;
        code::commit(&at, &format!("{f}, by its minion"))?;
        writers.push(a);
    }
    code::step(&mut steps, "write (one minion a file)", &mut t, &writers.iter().collect::<Vec<_>>());

    // 3. merge in pairs, like a tournament, every pair of a round at once
    let mut groups: Vec<usize> = (0..files.len()).collect();
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
    for i in 0..files.len() {
        let _ = code::git(&o.dir, &["worktree", "remove", "--force", &wt.join(format!("{i:03}")).display().to_string()]);
    }
    let merge_s = t.elapsed().as_secs_f64();
    code::step(&mut steps, &format!("merge ({rounds} rounds, in pairs)"), &mut t, &[]);

    // 4. test after writing; only the failing tasks go back, each to one minion with its own files
    let task_of: BTreeMap<String, u64> = specs.iter().zip(&files).map(|(s, f)| (f.clone(), s["task"].as_u64().unwrap_or(0))).collect();
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
        let mut by_task: BTreeMap<u64, Vec<String>> = BTreeMap::new();
        for (f, errs) in &blamed {
            by_task.entry(task_of.get(f).copied().unwrap_or(0)).or_default().extend(errs.iter().map(|e| format!("in {f}: {e}")));
        }
        println!("  test round {fix_rounds}: tasks {:?} fail", by_task.keys().collect::<Vec<_>>());
        let owned: Vec<(u64, Vec<String>)> = by_task.keys().map(|k| (*k, files.iter().filter(|f| task_of.get(*f) == Some(k) || (*k == 0 && blamed.contains_key(*f))).cloned().collect())).collect();
        let asks: Vec<Ask> = owned
            .iter()
            .map(|(k, mine)| {
                minion(&format!("fix task {k}"), format!(
                    "Your piece: you own {} (task {k}) and only them. The build or the tests fail:\n{}\n\nYour files now:\n{}\nFix them within their budgets, keeping the registered items. Do not change an expected answer to match the output unless you are sure the expected answer was wrong. Reply with the files you changed, each as `=== FILE: <path> ===` and its whole content, the last ended by `=== END ===`.",
                    mine.join(", "), by_task[k].join("\n\n"), code::bundle(&o.dir, mine)
                ))
            })
            .collect::<Result<_, _>>()?;
        let fixed = swarm::run(&format!("fix round {fix_rounds}: {} minions", asks.len()), &setup, asks, o.agents);
        for (r, (_, mine)) in fixed.into_iter().zip(&owned) {
            let a = r?;
            for (path, content) in agent::files_in(&notes(&a.text).0).iter().filter(|(p, _)| mine.contains(p)) {
                agent::write(&o.dir.join(path), content)?;
            }
            fixers.push(a);
        }
        code::commit(&o.dir, &format!("test round {fix_rounds}: the failing tasks fixed by their minions"))?;
        outcome = code::test(&o.dir);
        agent::write(&run_dir.join(format!("test-{fix_rounds}.txt")), &outcome.1)?;
    }
    code::step(&mut steps, &format!("test ({fix_rounds} fix rounds)"), &mut t, &fixers.iter().collect::<Vec<_>>());

    // the Box kept in Nimbus too
    let mut kept = 0;
    for e in walkdir(&shared.dir) {
        if let Ok(b) = std::fs::read(&e) {
            nimbus::put_bytes(&b)?;
            kept += 1;
        }
    }
    let (ok, _, passed, failed) = outcome;
    let board = shared.board();
    let claimed_by_minions = shared.items().iter().filter(|i| i.owner != "director").count();
    let result = format!(
        "{} ({passed} tests passed, {failed} failed; {} source files; registry {} items, {claimed_by_minions} claimed by minions; board {} posts; {kept} Box files kept in Nimbus)",
        if ok { "ALL TESTS PASS" } else { "TESTS FAIL" },
        code::count_files(&o.dir),
        shared.items().len(),
        board.len()
    );
    let writing: f64 = steps.iter().filter(|s| !s.name.starts_with("test")).map(|s| s.seconds).sum();
    let testing: f64 = steps.iter().filter(|s| s.name.starts_with("test")).map(|s| s.seconds).sum();
    Ok(code::finish(mode, steps, wall, result, run_dir, (writing, testing), &o.dir, vec![("merge time".into(), format!("{merge_s:.1} s"))]))
}

fn walkdir(d: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walkdir(&p));
        } else {
            out.push(p);
        }
    }
    out
}
