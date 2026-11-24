//! The Foreman: a 21st minion that only orchestrates the building, and
//! writes no code. The building runs as events, not as one wave:
//!
//! - every builder is started (the first alone, until its first token: the
//!   shared prefix is then in the cache), each the owner of one file;
//! - as soon as both files of a task are written, the task is tested at
//!   once, on its own (a small crate holding only that module and its tests),
//!   while the others are still writing;
//! - a failing task goes to the Foreman with its error and everyone's status
//!   (working, done, free); it names one free minion for it, the assignment
//!   is posted on the board before the minion starts, and the minion fixes
//!   only that task's files; nothing else waits for it;
//! - every assignment is logged with its time.
//!
//! The Foreman's messages are tiny (low effort, a short cap on its output).
//! A minion still writing cannot be helped in the middle of its file (it
//! writes it in one go), so help goes where work is waiting: the failures.

use crate::agent::{self, Answer, Ask, Setup, Want};
use crate::code;
use crate::hive::notes;
use crate::shared::Shared;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Instant;

const FOREMAN_OUTPUT: u32 = 300;
const TRIES: usize = 3;

pub struct Piece {
    pub task: usize,
    pub module: String,
    pub path: String,
}

pub struct Outcome {
    pub builders: Vec<Answer>,
    pub fixers: Vec<Answer>,
    pub foreman: Vec<Answer>,
    pub reassignments: usize,
    /// each minion's idle seconds in the building phase
    pub idle: Vec<(String, f64)>,
    pub first_start: f64,
    pub failed: Vec<usize>,
    pub log: Vec<String>,
}

enum Event {
    Built(usize, Result<Answer, String>, f64, f64),
    Tested(usize, bool, String),
    Assigned(usize, String, Result<Answer, String>),
    Fixed(usize, usize, Result<Answer, String>, f64, f64),
}

/// The building phase. `asks` are the builders, in the order of `pieces`;
/// `make` makes an ask (for a fix or the Foreman) on the shared prefix.
#[allow(clippy::too_many_arguments)]
pub fn build(setup: &Setup, mut asks: Vec<Ask>, pieces: &[Piece], run_dir: &Path, wt: &Path, cargo_toml: &str, shared: &Shared, make: &(dyn Fn(&str, String, Want, Option<u32>) -> Result<Ask, String> + Sync)) -> Result<Outcome, String> {
    let began = Instant::now();
    let now = || began.elapsed().as_secs_f64();
    let n = pieces.len();
    let minion = |i: usize| format!("m{:02}", i + 1);
    let tasks: BTreeMap<usize, Vec<usize>> = pieces.iter().enumerate().fold(BTreeMap::new(), |mut m, (i, p)| {
        m.entry(p.task).or_insert_with(Vec::new).push(i);
        m
    });
    // each task's own small crate, to be tested alone
    let check = |task: usize| -> PathBuf { run_dir.join("check").join(&pieces[tasks[&task][0]].module) };
    for (&task, idx) in &tasks {
        let d = check(task);
        agent::write(&d.join("Cargo.toml"), cargo_toml)?;
        agent::write(&d.join("src/lib.rs"), &format!("pub mod {};\n", pieces[idx[0]].module))?;
    }
    let mut out = Outcome { builders: Vec::new(), fixers: Vec::new(), foreman: Vec::new(), reassignments: 0, idle: Vec::new(), first_start: 0.0, failed: Vec::new(), log: Vec::new() };
    let mut busy = vec![0.0f64; n];
    let mut working: Vec<Option<(String, f64)>> = vec![None; n];
    let mut written = vec![false; n];
    let mut tries: BTreeMap<usize, usize> = BTreeMap::new();
    let mut done: BTreeMap<usize, bool> = BTreeMap::new();
    let mut pending = 0usize;
    let (tx, rx) = mpsc::channel::<Event>();
    let warm = agent::Signal::default();
    asks[0].first_token = Some(warm.clone());
    let log = |line: String, out: &mut Outcome| {
        let l = format!("[{:6.1} s] {line}", now());
        println!("  foreman {l}");
        out.log.push(l);
    };
    std::thread::scope(|s| -> Result<(), String> {
        let spawn_build = |i: usize, ask: Ask, tx: mpsc::Sender<Event>| {
            s.spawn(move || {
                let t0 = now();
                let r = agent::run(setup, &ask, &|_| {});
                let _ = tx.send(Event::Built(i, r, t0, now()));
            });
        };
        // the first builder alone, until its first token; then the others
        let rest: Vec<Ask> = asks.drain(1..).collect();
        out.first_start = now();
        spawn_build(0, asks.remove(0), tx.clone());
        working[0] = Some((pieces[0].path.clone(), now()));
        pending += 1;
        {
            let (lock, cv) = &*warm;
            let up = lock.lock().unwrap();
            let _ = cv.wait_timeout_while(up, std::time::Duration::from_secs(30), |up| !*up).unwrap();
        }
        log(format!("the shared prefix is cached; {} more builders start", rest.len()), &mut out);
        for (k, ask) in rest.into_iter().enumerate() {
            working[k + 1] = Some((pieces[k + 1].path.clone(), now()));
            spawn_build(k + 1, ask, tx.clone());
            pending += 1;
        }
        let test_task = |task: usize, tx: mpsc::Sender<Event>| {
            let d = check(task);
            s.spawn(move || {
                let (ok, text, _, _) = code::test(&d);
                let _ = tx.send(Event::Tested(task, ok, text));
            });
        };
        let place = |i: usize, content: &str| -> Result<(), String> {
            let p = &pieces[i];
            agent::write(&wt.join(format!("{i:03}")).join(&p.path), content)?;
            agent::write(&check(p.task).join(&p.path), content)
        };
        while pending > 0 {
            let ev = rx.recv().map_err(|e| e.to_string())?;
            pending -= 1;
            match ev {
                Event::Built(i, r, t0, t1) => {
                    busy[i] += t1 - t0;
                    working[i] = None;
                    let a = r?;
                    let got = agent::files_in(&notes(&a.text).0);
                    let content = got.get(&pieces[i].path).cloned().or_else(|| (got.len() == 1).then(|| got.values().next().cloned()).flatten()).ok_or(format!("{}: its minion gave another file", pieces[i].path))?;
                    place(i, &content)?;
                    written[i] = true;
                    shared.post(&minion(i), &format!("done {}", pieces[i].path))?;
                    out.builders.push(a);
                    let task = pieces[i].task;
                    if tasks[&task].iter().all(|&j| written[j]) {
                        test_task(task, tx.clone());
                        pending += 1;
                    }
                }
                Event::Tested(task, ok, text) => {
                    if ok {
                        done.insert(task, true);
                        log(format!("task {task} passes"), &mut out);
                        continue;
                    }
                    let t = tries.entry(task).or_insert(0);
                    *t += 1;
                    if *t > TRIES {
                        done.insert(task, false);
                        log(format!("task {task} still fails after {TRIES} fixes"), &mut out);
                        continue;
                    }
                    let files: Vec<String> = tasks[&task].iter().map(|&j| pieces[j].path.clone()).collect();
                    let errs: Vec<String> = code::blame(&text, &files).into_iter().flat_map(|(f, e)| e.into_iter().map(move |x| format!("{f}: {x}"))).collect();
                    let error = if errs.is_empty() { text.lines().filter(|l| l.contains("error") || l.contains("FAILED") || l.contains("panicked")).take(6).collect::<Vec<_>>().join("\n") } else { errs.join("\n") };
                    // the Foreman picks a free minion: a tiny status, a tiny reply
                    let status: Vec<String> = (0..n).map(|k| match &working[k] {
                        Some((what, since)) => format!("{} working on {what} ({:.0} s)", minion(k), now() - since),
                        None => format!("{} free", minion(k)),
                    }).collect();
                    let first_line = error.lines().next().unwrap_or("").chars().take(160).collect::<String>();
                    log(format!("task {task} fails: {first_line}"), &mut out);
                    let prompt = format!("You are the Foreman: you only assign work, you write no code. Task {task} fails ({first_line}). Minions:\n{}\nPick one free minion to fix task {task} (its files: {}). Reply with ONE JSON object: {{\"minion\":\"<id>\",\"note\":\"<at most 10 words>\"}}", status.join("\n"), files.join(", "));
                    let fa = Ask { effort: Some("low".into()), ..make("foreman", prompt, Want::Json, Some(FOREMAN_OUTPUT))? };
                    let tx = tx.clone();
                    s.spawn(move || {
                        let r = agent::run(setup, &fa, &|_| {});
                        let _ = tx.send(Event::Assigned(task, error, r));
                    });
                    pending += 1;
                }
                Event::Assigned(task, error, r) => {
                    let a = r?;
                    let named = a.json["minion"].as_str().unwrap_or("").to_string();
                    let note = a.json["note"].as_str().unwrap_or("").to_string();
                    out.foreman.push(a);
                    // the named minion if it is free, else the first free one, else its own builder
                    let k = (0..n).find(|&k| minion(k) == named && working[k].is_none()).or_else(|| (0..n).find(|&k| working[k].is_none())).unwrap_or(tasks[&task][0]);
                    let files: Vec<String> = tasks[&task].iter().map(|&j| pieces[j].path.clone()).collect();
                    // posted before the minion starts: its piece is these files and nothing else
                    shared.post("foreman", &format!("assign {} -> fix task {task} ({}): {note}", minion(k), files.join(", ")))?;
                    log(format!("assign {} -> fix task {task} ({note})", minion(k)), &mut out);
                    out.reassignments += 1;
                    working[k] = Some((format!("fix task {task}"), now()));
                    let current = code::bundle(&check(task), &files);
                    let prompt = format!("You own {} (task {task}) and only them. The build or its tests fail:\n{error}\n\nThe files now:\n{current}\nFix them, keeping their pub items. Do not change an expected answer to match the output unless you are sure it was wrong. Reply with the files you changed, each as `=== FILE: <path> ===` and its whole content, the last ended by `=== END ===`.", files.join(", "));
                    let ask = make(&format!("fix task {task} by {}", minion(k)), prompt, Want::Files, None)?;
                    let tx = tx.clone();
                    s.spawn(move || {
                        let t0 = now();
                        let r = agent::run(setup, &ask, &|_| {});
                        let _ = tx.send(Event::Fixed(task, k, r, t0, now()));
                    });
                    pending += 1;
                }
                Event::Fixed(task, k, r, t0, t1) => {
                    busy[k] += t1 - t0;
                    working[k] = None;
                    let a = r?;
                    for (path, content) in agent::files_in(&notes(&a.text).0) {
                        if let Some(&j) = tasks[&task].iter().find(|&&j| pieces[j].path == path) {
                            place(j, &content)?;
                        }
                    }
                    shared.post(&minion(k), &format!("fixed task {task}"))?;
                    out.fixers.push(a);
                    test_task(task, tx.clone());
                    pending += 1;
                }
            }
        }
        Ok(())
    })?;
    let total = now();
    out.idle = (0..n).map(|k| (minion(k), (total - busy[k]).max(0.0))).collect();
    out.failed = done.iter().filter(|(_, ok)| !**ok).map(|(t, _)| *t).collect();
    let _ = std::fs::write(run_dir.join("foreman.log"), out.log.join("\n"));
    Ok(out)
}
