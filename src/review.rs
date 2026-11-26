//! `bottlebox review`: code that exists, read and mended by many at once.
//!
//! 1. **One minion a file reviews**, all at once, each a fresh mind: none of
//!    them wrote the file it reads (a writer reads what it meant, not what is
//!    there). Each is given the checks and its one file with line numbers, and
//!    reports only real issues, each with its line.
//! 2. **One director decides**: for every reported issue, real or not, and
//!    for a real one how to mend it. It writes no code: a short answer.
//! 3. **One minion a file mends**, all at once: each file with a real issue
//!    goes to a minion with the director's decisions, and comes back whole,
//!    with its tests (`tests/<name>.rs`) if they must change with it.
//! 4. The crate's tests run. A failure goes to a minion for each file under
//!    review that the output names, whether it was mended or not (twice at
//!    most): a mend in one file can break another that uses it (a trait method
//!    given a result broke the tests of the file that implements the trait,
//!    and in the first version nobody was there to mend those).
//! 5. **The director gives the verdict**, yes or no, from what was mended and
//!    what the tests say. A yes with failing tests is a no.
//!
//! The first version had the director write every mended file itself, one after
//! another: 211 s of 244 s for nine files. Deciding is one mind's work; mending
//! a file is not.
//!
//! No agent has tools. Only the files under review and their tests are written.

use crate::agent::{self, Answer, Ask, Setup, Usage, Want};
use crate::code;
use crate::hive::notes;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::{Duration, Instant};

pub struct Options {
    /// the crate (where `cargo test` runs)
    pub dir: PathBuf,
    /// the files to review, from `dir`
    pub files: Vec<String>,
    /// files the reviewed ones build on: only their public items are shown
    pub context: Vec<String>,
    /// reference material the code was written from (files, anywhere): every agent gets their
    /// text, so that the code can be held against its source and not only against itself
    pub reference: Vec<String>,
    /// what to look for
    pub checks: String,
    pub model: String,
}

pub struct Issue {
    pub file: String,
    pub line: u64,
    pub what: String,
    pub why: String,
    /// the director's decision: real or not, and how to mend it (or why it is not real)
    pub real: bool,
    pub how: String,
}

const FIX_ROUNDS: usize = 2;

/// A file with its line numbers, as the reviewers see it.
fn numbered(text: &str) -> String {
    text.lines().enumerate().map(|(i, l)| format!("{:>5}| {l}\n", i + 1)).collect()
}

/// A file's public items, a line each: what another file's reader needs of it.
fn public_items(text: &str) -> String {
    let body = text.find("\n#[cfg(test)]\nmod ").map_or(text, |at| &text[..at]);
    body.lines().map(str::trim).filter(|l| l.starts_with("pub ")).map(|l| format!("  {}\n", l.trim_end_matches('{').trim_end())).collect()
}

/// The tests that go with a file: `tests/<its name>.rs`.
fn tests_of(file: &str) -> String {
    let stem = std::path::Path::new(file).file_stem().and_then(|s| s.to_str()).unwrap_or("");
    format!("tests/{stem}.rs")
}

/// The verdict line in the director's answer (the last one said).
fn verdict(text: &str) -> Option<bool> {
    let at = text.rfind("=== VERDICT:")?;
    let word = text[at + 12..].trim_start().to_lowercase();
    if word.starts_with("yes") {
        Some(true)
    } else if word.starts_with("no") {
        Some(false)
    } else {
        None
    }
}

/// The files, of those given, that a failing build or test run names (by `src/x.rs` or `tests/x.rs`).
fn blamed(output: &str, files: &[String]) -> Vec<String> {
    let output = output.replace('\\', "/");
    files.iter().filter(|f| output.contains(f.as_str()) || output.contains(&tests_of(f))).cloned().collect()
}

/// An output cap for writing a file of so many lines back whole (with its tests).
fn room(lines: usize) -> u32 {
    (lines as u64 * 45).clamp(8000, 48000) as u32
}

/// Runs the review; true when the verdict is yes and the tests pass.
pub fn run(o: &Options) -> Result<bool, String> {
    let wall = Instant::now();
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let run_dir = agent::home().join("runs").join(format!("{stamp}-review"));
    std::fs::create_dir_all(&run_dir).map_err(|e| e.to_string())?;
    let read = |f: &String| std::fs::read_to_string(o.dir.join(f)).map_err(|e| format!("{f}: {e}"));
    let mut sources: BTreeMap<String, String> = BTreeMap::new();
    for f in &o.files {
        sources.insert(f.replace('\\', "/"), read(f)?);
    }
    let files: Vec<String> = sources.keys().cloned().collect();
    println!("Bottle Box (review): {} files in {}: a minion each reviews, one director decides, a minion each mends", files.len(), o.dir.display());

    // the shared prefix: the rules, the checks, and what every file offers the others
    let mut around = String::new();
    for f in o.context.iter().chain(&o.files) {
        around += &format!("{f}:\n{}", public_items(&read(f)?));
    }
    let mut reference = String::new();
    for f in &o.reference {
        let text = std::fs::read_to_string(f).map_err(|e| format!("--reference {f}: {e}"))?;
        let name = f.rsplit(['/', '\\']).next().unwrap_or(f);
        reference += &format!("=== REFERENCE: {name} ===\n{text}\n=== END ===\n");
    }
    if !reference.is_empty() {
        reference = format!("\n\nTHE REFERENCE the code was written from. Where the code differs from it in a layout, a size, an offset, a constant, a number or the order of steps, that is a real issue: name the reference's function or structure.\n{reference}");
    }
    let rules = format!("You are part of a code review in Bottle Box: many agents work on one crate at once, a file each, and one director decides. Reviewers did not write the code they read. Only real issues count: something that is wrong, missing or dangerous when the code runs. No style, no naming, no wishes. Every issue names its line.\n\nWHAT TO LOOK FOR:\n{}\n\nTHE PUBLIC ITEMS of the files under review and of the files they build on (for what a call does, not for review):\n{around}{reference}", o.checks.trim());
    let prefix = run_dir.join("prefix.txt");
    agent::write(&prefix, &rules)?;
    let setup = Setup { shared: Some(prefix), workdir: run_dir.clone(), read: Vec::new() };
    let say = |line: String| println!("  [{:6.1} s] {line}", wall.elapsed().as_secs_f64());
    // a line every 20 s while one agent works alone, so that silence is not taken for a stall
    let alone = |what: &str, ask: &Ask| -> Result<Answer, String> {
        let done = AtomicBool::new(false);
        std::thread::scope(|s| {
            s.spawn(|| {
                let began = Instant::now();
                while !done.load(Relaxed) {
                    std::thread::sleep(Duration::from_millis(250));
                    if !done.load(Relaxed) && began.elapsed().as_millis() % 20_000 < 250 && began.elapsed().as_secs() >= 20 {
                        say(format!("{what} ({:.0} s)", began.elapsed().as_secs_f64()));
                    }
                }
            });
            let r = agent::run(&setup, ask, &|_| {});
            done.store(true, Relaxed);
            r
        })
    };
    let mut usage = Usage::default();
    let mut marks: Vec<(String, f64)> = Vec::new();

    // 1. one minion a file reviews, all at once
    let reports: Vec<(String, Result<Answer, String>)> = std::thread::scope(|s| {
        let hs: Vec<_> = files.iter().map(|f| {
            let prompt = format!("Review `{f}`. Its lines are numbered. Reply with ONE JSON object: {{\"issues\":[{{\"line\":<number>,\"what\":\"<what is wrong, one sentence>\",\"why\":\"<what happens because of it, one sentence>\"}}]}}; an empty list when the file has no real issue.\n\n=== {f} ===\n{}", numbered(&sources[f]));
            let ask = Ask { tools: String::new(), effort: Some("medium".into()), max_output: Some(8000), ..Ask::minion(&format!("review {f}"), prompt, &o.model, Want::Json) };
            let (setup, say) = (&setup, &say);
            s.spawn(move || {
                let r = agent::run(setup, &ask, &|_| {});
                match &r {
                    Ok(a) => say(format!("{f} reviewed: {} issue(s)", a.json["issues"].as_array().map_or(0, |i| i.len()))),
                    Err(e) => say(format!("{f}: its reviewer failed: {e}")),
                }
                (f.clone(), r)
            })
        }).collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut issues: Vec<Issue> = Vec::new();
    for (f, r) in &reports {
        let a = r.as_ref().map_err(|e| e.clone())?;
        usage.add(&a.usage);
        for i in a.json["issues"].as_array().into_iter().flatten() {
            issues.push(Issue { file: f.clone(), line: i["line"].as_u64().unwrap_or(0), what: i["what"].as_str().unwrap_or("").into(), why: i["why"].as_str().unwrap_or("").into(), real: false, how: String::new() });
        }
    }
    marks.push(("review".into(), wall.elapsed().as_secs_f64()));

    // every file with its tests, as the director and the menders see them
    let with_tests = |f: &String| -> String {
        let mut text = format!("=== FILE: {f} ===\n{}=== END ===\n", std::fs::read_to_string(o.dir.join(f)).unwrap_or_default());
        if let Ok(tests) = std::fs::read_to_string(o.dir.join(tests_of(f))) {
            text += &format!("=== FILE: {} ===\n{tests}=== END ===\n", tests_of(f));
        }
        text
    };
    let line_of = |i: &Issue| sources[&i.file].lines().nth((i.line as usize).saturating_sub(1)).unwrap_or("").trim().to_string();

    // 2. the director decides: no code, a decision for each issue
    let mut director_session = None;
    if !issues.is_empty() {
        let listed: String = issues.iter().enumerate().map(|(n, i)| format!("{n}. {}:{} `{}`: {} ({})\n", i.file, i.line, line_of(i), i.what, i.why)).collect();
        let all: String = files.iter().map(with_tests).collect();
        let prompt = format!("You are the director of this review. The reviewers reported {} issue(s), numbered:\n{listed}\nThe files and their tests:\n{all}\nDecide for each reported issue whether it is real: look at the code yourself (a reviewer saw one file only, and what it feared may be handled elsewhere). Write no code: a minion for each file will mend it from your decisions. Reply with ONE JSON object: {{\"decisions\":[{{\"issue\":<its number>,\"real\":true|false,\"how\":\"<for a real one: exactly what to change, in one or two sentences, with the names to use; for one that is not real: why not, in a few words>\"}}]}}, one decision for every issue.", issues.len());
        let ask = Ask { tools: String::new(), effort: Some("high".into()), max_output: Some(16000), keep: true, ..Ask::minion("director decides", prompt, &o.model, Want::Json) };
        let a = alone("the director is deciding", &ask)?;
        usage.add(&a.usage);
        for d in a.json["decisions"].as_array().into_iter().flatten() {
            if let Some(i) = d["issue"].as_u64().and_then(|n| issues.get_mut(n as usize)) {
                i.real = d["real"].as_bool().unwrap_or(false);
                i.how = d["how"].as_str().unwrap_or("").into();
            }
        }
        say(format!("the director has decided: {} of {} issues are real", issues.iter().filter(|i| i.real).count(), issues.len()));
        director_session = Some(a.session);
    }
    marks.push(("decide".into(), wall.elapsed().as_secs_f64()));

    // 3. one minion a file mends, all at once; 4. the tests, and what fails goes back to its minion
    let to_mend: Vec<String> = files.iter().filter(|f| issues.iter().any(|i| i.real && &i.file == *f)).cloned().collect();
    let mut changed: Vec<String> = Vec::new();
    let mut sessions: BTreeMap<String, String> = BTreeMap::new();
    let mut outcome = (true, String::new(), 0usize, 0usize);
    // every file a minion was given to mend, for its issues or for a failure
    let mut mended: Vec<String> = to_mend.clone();
    let decided: String = issues.iter().filter(|i| i.real).map(|i| format!("- {}:{}: {}\n", i.file, i.line, i.how)).collect();
    let mut work: Vec<(String, String)> = to_mend.iter().map(|f| {
        let mine: String = issues.iter().filter(|i| i.real && &i.file == f).map(|i| format!("- line {} `{}`: {}. Mend: {}\n", i.line, line_of(i), i.what, i.how)).collect();
        (f.clone(), format!("Mend `{f}`: the director found these issues real.\n{mine}\nChange only what they need; keep every public signature unless a mend needs otherwise; the tests must still pass, and a mend that changes behaviour gets its test changed or added with it. Reply with the file whole, and its tests whole if you changed them, each as a line `=== FILE: <path> ===` and its content, the last ended by `=== END ===`.\n\n{}", with_tests(f)))
    }).collect();
    for round in 0..=FIX_ROUNDS {
        if work.is_empty() && round > 0 {
            break;
        }
        let answers: Vec<(String, Result<Answer, String>)> = std::thread::scope(|s| {
            let hs: Vec<_> = work.iter().map(|(f, prompt)| {
                let lines = with_tests(f).lines().count();
                let ask = Ask { tools: String::new(), effort: Some("medium".into()), max_output: Some(room(lines)), keep: true, resume: sessions.get(f).cloned(), want: Want::Files, ..Ask::minion(&format!("mend {f}"), prompt.clone(), &o.model, Want::Files) };
                let (setup, say) = (&setup, &say);
                s.spawn(move || {
                    let r = agent::run(setup, &ask, &|_| {});
                    say(match &r {
                        Ok(_) => format!("{f} mended"),
                        Err(e) => format!("{f}: its mender failed: {e}"),
                    });
                    (f.clone(), r)
                })
            }).collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        for (f, r) in answers {
            let a = r?;
            usage.add(&a.usage);
            for (path, content) in agent::files_in(&notes(&a.text).0) {
                if path != f && path != tests_of(&f) {
                    say(format!("the mender of {f} wrote {path}, which is not its file: not written"));
                    continue;
                }
                agent::write(&o.dir.join(&path), &content)?;
                if !changed.contains(&path) {
                    changed.push(path);
                }
            }
            sessions.insert(f, a.session);
        }
        say("the tests are running".into());
        outcome = code::test_in(&o.dir, None);
        if outcome.0 {
            say(format!("the tests pass ({} passed)", outcome.2));
            break;
        }
        let errors: String = outcome.1.lines().filter(|l| l.contains("error") || l.contains("FAILED") || l.contains("panicked") || l.contains("-->")).take(40).collect::<Vec<_>>().join("\n");
        // to a minion for each file under review that the failure names, mended before or not;
        // to every mender when it names none of them
        let mut back = blamed(&outcome.1, &files);
        if back.is_empty() {
            back = to_mend.clone();
        }
        say(format!("the tests fail ({} failed): back to {}", outcome.3, if round < FIX_ROUNDS { back.join(", ") } else { "nobody: the rounds are used up".into() }));
        work = back.into_iter().map(|f| {
            let prompt = format!("With the mended files in, the build or the tests fail:\n{errors}\n\nWhat was mended in this review (another file's mend may be what your file must now follow):\n{decided}\nYour file is `{f}`. If it or its tests are at fault or must follow a mend, fix them. Reply with what you change now, each as `=== FILE: <path> ===` and its whole content, the last ended by `=== END ===`; your file as it is if nothing in it needs to change.\n\n{}", with_tests(&f));
            if !mended.contains(&f) {
                mended.push(f.clone());
            }
            (f, prompt)
        }).collect();
    }
    marks.push(("mend and test".into(), wall.elapsed().as_secs_f64()));
    agent::write(&run_dir.join("tests.txt"), &outcome.1)?;

    // 5. the verdict: the director goes on from its own conversation
    let listed: String = issues.iter().map(|i| format!("- {}:{} `{}`: {} ({})\n    {}: {}\n", i.file, i.line, line_of(i), i.what, i.why, if i.real { "REAL, mended" } else { "not real" }, i.how)).collect();
    let (yes, why) = if issues.is_empty() {
        (true, "the reviewers found no real issue".to_string())
    } else {
        let now: String = mended.iter().map(with_tests).collect();
        let prompt = format!("The minions have mended the files, and the crate's tests {} ({} passed, {} failed). The mended files as they are now:\n{now}\nCheck that each real issue is mended as you decided. Reply with the verdict on its own line, `=== VERDICT: yes ===` or `=== VERDICT: no ===` (yes only when nothing found in this review would make running this code unsafe as it stands now), then one line `=== NOTE: <one sentence why> ===`, and a line `=== NOTE: still open: <file>:<what> ===` for each real issue that is not mended.", if outcome.0 { "pass" } else { "FAIL" }, outcome.2, outcome.3);
        let ask = Ask { tools: String::new(), effort: Some("high".into()), max_output: Some(8000), resume: director_session, ..Ask::minion("director's verdict", prompt, &o.model, Want::Text) };
        let a = alone("the director is checking the mended files", &ask)?;
        usage.add(&a.usage);
        (verdict(&a.text).unwrap_or(false), notes(&a.text).1.join("; "))
    };
    let safe = yes && outcome.0;
    marks.push(("verdict".into(), wall.elapsed().as_secs_f64()));

    let mut report = format!("Bottle Box review of {} files in {}\n\n", files.len(), o.dir.display());
    report += &format!("Reported by the reviewers: {}; real, by the director: {}\n{}\n", issues.len(), issues.iter().filter(|i| i.real).count(), if listed.is_empty() { "(none)\n".into() } else { listed });
    report += &format!("Files changed: {}\n", if changed.is_empty() { "none".into() } else { changed.join(", ") });
    report += &format!("Tests: {} passed, {} failed\n", outcome.2, outcome.3);
    report += &format!("VERDICT: {}{}\n  {why}\n", if safe { "yes" } else { "no" }, if yes && !outcome.0 { " (the director said yes, but the tests fail)" } else { "" });
    let mut before = 0.0;
    let times: Vec<String> = marks.iter().map(|(name, at)| {
        let s = format!("{name} {:.1} s", at - before);
        before = *at;
        s
    }).collect();
    report += &format!("\n{} reviewers, a director, {} menders; {}; {:.1} s in all; {} tokens ({} read from the cache), ${:.2} at API prices\n", files.len(), mended.len(), times.join(", "), wall.elapsed().as_secs_f64(), usage.tokens(), usage.cache_read, usage.cost());
    agent::write(&run_dir.join("review.txt"), &report)?;
    println!("\n{report}run: {}", run_dir.display());
    Ok(safe)
}

#[cfg(test)]
mod tests {
    use super::{blamed, numbered, public_items, room, tests_of, verdict};

    #[test]
    fn a_file_is_numbered_and_its_public_items_listed() {
        assert_eq!(numbered("a\nb\n"), "    1| a\n    2| b\n");
        let text = "use x;\npub fn a(b: u32) -> u32 {\n    b\n}\nfn hidden() {}\n    pub const C: u32 = 1;\n\n#[cfg(test)]\nmod tests {\n    pub fn t() {}\n}\n";
        assert_eq!(public_items(text), "  pub fn a(b: u32) -> u32\n  pub const C: u32 = 1;\n");
        assert_eq!(tests_of("src/msgq.rs"), "tests/msgq.rs");
    }

    #[test]
    fn the_verdict_is_the_last_one_said() {
        assert_eq!(verdict("x\n=== VERDICT: yes ===\n"), Some(true));
        assert_eq!(verdict("=== VERDICT: yes ===\nlater\n=== VERDICT: No ===\n"), Some(false));
        assert_eq!(verdict("=== VERDICT: maybe ==="), None);
        assert_eq!(verdict("no verdict"), None);
    }

    #[test]
    fn a_failure_goes_back_to_the_files_it_names() {
        let files = vec!["src/msgq.rs".to_string(), "src/rpc.rs".to_string(), "src/boot.rs".to_string()];
        let output = "error[E0308]: mismatched types\n  --> src\\rpc.rs:12:5\ntest result: FAILED\nerror: test failed, to rerun pass `--test boot`\n  --> tests\\boot.rs:3:1\n";
        assert_eq!(blamed(output, &files), ["src/rpc.rs", "src/boot.rs"]);
        assert!(blamed("error: linker failed", &files).is_empty());
        assert_eq!((room(10), room(400), room(5000)), (8000, 18000, 48000));
    }
}
