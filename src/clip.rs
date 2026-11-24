//! The first driver: a montage by Bottle Clip, decided by a swarm.
//!
//! 1. Bottle Clip analyses (watch_video, find_moments; cached in Nimbus).
//! 2. The splitter makes the pieces: a scout for every candidate (open its
//!    grid, read its words, rate it, give its role and best part), caption
//!    pieces (spellings for a few candidates each), and one piece for each
//!    effect (look, transitions, punch-ins, title).
//! 3. The swarm runs them all at once; every report is kept in Nimbus.
//! 4. The merger makes one plan of the reports (the best opener, the builds,
//!    the best climax, to the asked length; conflicting spellings: the first
//!    wins, and it is said), and Bottle Clip snaps it to the beat.
//! 5. The director gets every minion's report, the plan and the preview's
//!    frame grid; it may change the order and the effects, or send pieces
//!    back to be done again with a note (once); then the final render.
//!
//! With `single`, one agent does every piece in one go and is the director
//! too: the same work, for the comparison.

use crate::agent::{self, Answer, Ask, Setup, Usage, Want};
use crate::{nimbus, swarm};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

pub struct Options {
    pub url: String,
    pub style: String,
    pub length: f64,
    pub agents: usize,
    pub candidates: usize,
    pub model: String,
    pub single: bool,
}

/// One step of a run: its name, seconds, and the tokens its agents used.
pub struct Step {
    pub name: String,
    pub seconds: f64,
    pub agents: usize,
    pub usage: Usage,
    /// of the seconds, the agents' waits for rate limits (added over the agents)
    pub waited: f64,
}

pub struct Report {
    pub mode: String,
    pub steps: Vec<Step>,
    pub wall: f64,
    pub agents: usize,
    pub usage: Usage,
    /// what came out (the video, or the tests' result)
    pub result: String,
    pub dir: PathBuf,
    /// (writing, testing) seconds on the critical path, where they can be told apart
    pub split: Option<(f64, f64)>,
}

fn bottle_clip() -> PathBuf {
    std::env::var_os("BOTTLE_CLIP_EXE").map(PathBuf::from).unwrap_or_else(|| agent::home().parent().unwrap().join(".bottleclip").join("bin").join("bottle-clip.exe"))
}

/// Runs Bottle Clip and reads its JSON answer.
fn clip(args: &[&str]) -> Result<Value, String> {
    let out = Command::new(bottle_clip()).args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).output().map_err(|e| format!("bottle-clip: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let lines: Vec<&str> = err.lines().filter(|l| !l.contains("JavaScript runtime")).collect();
        return Err(format!("bottle-clip {} failed: {}", args.first().unwrap_or(&""), lines.join(" | ")));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // the whole answer (pretty or not), else its last line that is JSON
    if let Ok(v) = serde_json::from_str(text.trim()) {
        return Ok(v);
    }
    let line = text.lines().rev().find(|l| l.trim_start().starts_with(['{', '['])).ok_or("bottle-clip gave no JSON")?;
    serde_json::from_str(line).map_err(|e| format!("bottle-clip's JSON: {e}"))
}

fn clock(t: f64) -> String {
    format!("{:02}:{:04.1}", (t / 60.0) as u64, t % 60.0)
}

/// The shared context: the task, Bottle Clip, the style, every candidate.
fn shared(o: &Options, title: &str, moments: &[Value]) -> String {
    let mut s = format!(
        "You are one agent of Bottle Box: many agents work on one task at the same time, each on one small piece, and every one of them reports to the director. \
Bottle Clip does all the heavy lifting (analysis, the beat, rendering); you only decide. Answer with ONE JSON object and nothing else. \
When your piece names an image, open it with the Read tool; open nothing else.\n\n\
TASK: a {len:.0}-second {style} montage of \"{title}\" ({url}), vertical 9:16, cut to the beat of a CC0 music track by Bottle Clip.\n\n\
WHAT BOTTLE CLIP CAN DO: clips are snapped to whole beats and are 2.5 s long at least; captions come word by word from the transcript, at most 3 words and 1 s on screen at once; \
looks: grade none|punchy|teal-orange|warm|cool|bw, vignette 0..0.6, letterbox bars 0..0.12 of the height; transitions into each clip after the first: cut|flash|fade (0..0.6 s); \
punch-in zooms on every beat|downbeat|none, 0..0.1; one title text at the start (or none).\n\n\
CANDIDATES (found by find_moments; each grid shows 16 of its frames with their times):\n",
        len = o.length,
        style = o.style,
        url = o.url
    );
    for (k, m) in moments.iter().enumerate() {
        s += &format!(
            "c{:02}: {} to {} ({:.1} s), found for: {}. Words: \"{}\". Grid: {}\n",
            k + 1,
            clock(m["from"].as_f64().unwrap_or(0.0)),
            clock(m["to"].as_f64().unwrap_or(0.0)),
            m["to"].as_f64().unwrap_or(0.0) - m["from"].as_f64().unwrap_or(0.0),
            m["why"].as_str().unwrap_or(""),
            m["words"].as_str().unwrap_or(""),
            m["grid"].as_str().unwrap_or("")
        );
    }
    s
}

/// Every piece: (its id, what it asks).
fn pieces(o: &Options, n: usize) -> Vec<(String, String)> {
    let mut p = Vec::new();
    for k in 1..=n {
        p.push((format!("scout-c{k:02}"), format!(
            "Your piece: scout candidate c{k:02}. Open its grid and read its words. Reply {{\"id\":\"c{k:02}\",\"what\":\"<what happens, one line>\",\"rating\":<1-10 for a {} montage>,\"role\":\"opener|build|climax|skip\",\"from\":<seconds>,\"to\":<seconds>}} where from and to are its best part (at least 3 s, inside the candidate).",
            o.style
        )));
    }
    for (g, chunk) in (1..=n).collect::<Vec<_>>().chunks(3).enumerate() {
        let ids: Vec<String> = chunk.iter().map(|k| format!("c{k:02}")).collect();
        p.push((format!("caption-{}", g + 1), format!(
            "Your piece: the captions of {}. Read their words. Give the words the transcript has wrong, or that should be written otherwise on screen (names, numbers, a drawn-out shout kept readable). Reply {{\"replace\":{{\"<word as heard>\":\"<word as written>\"}}}} (empty if nothing).",
            ids.join(", ")
        )));
    }
    let fx = [
        ("fx-look", "the colour look. Reply {\"grade\":\"none|punchy|teal-orange|warm|cool|bw\",\"vignette\":<0..0.6>,\"letterbox\":<0..0.12>}"),
        ("fx-cuts", "the transitions between clips. Reply {\"transition\":\"cut|flash|fade\",\"transition_s\":<0..0.6>}"),
        ("fx-punch", "the punch-in zooms. Reply {\"pulse\":\"beat|downbeat|none\",\"pulse_zoom\":<0..0.1>}"),
        ("fx-title", "the title at the start. Reply {\"title\":\"<at most 4 words, or empty>\",\"dur\":<1..3 seconds>}"),
    ];
    for (id, what) in fx {
        p.push((id.to_string(), format!("Your piece: {what}, for this {} montage. Look at the candidates' words; open at most two grids.", o.style)));
    }
    p
}

/// What the minions decided, merged.
#[derive(Default, Clone)]
struct Decisions {
    order: Vec<String>,
    trims: BTreeMap<String, (f64, f64)>,
    fx: BTreeMap<String, Value>,
    replace: BTreeMap<String, String>,
    conflicts: Vec<String>,
}

/// The merger: one plan of the reports.
fn merge(o: &Options, moments: &[Value], reports: &BTreeMap<String, Value>) -> Decisions {
    let mut d = Decisions::default();
    let mut scouts: Vec<(String, Value)> = reports.iter().filter(|(k, _)| k.starts_with("scout-")).map(|(k, v)| (k[6..].to_string(), v.clone())).collect();
    scouts.sort_by(|a, b| b.1["rating"].as_f64().unwrap_or(0.0).total_cmp(&a.1["rating"].as_f64().unwrap_or(0.0)));
    for (id, v) in &scouts {
        let k: usize = id[1..].parse().unwrap_or(1);
        let m = &moments[k - 1];
        let (mf, mt) = (m["from"].as_f64().unwrap_or(0.0), m["to"].as_f64().unwrap_or(0.0));
        let (mut f, mut t) = (v["from"].as_f64().unwrap_or(mf).clamp(mf - 0.5, mt), v["to"].as_f64().unwrap_or(mt).clamp(mf, mt + 0.5));
        if t - f < 3.0 {
            (f, t) = (mf, mt);
        }
        d.trims.insert(id.clone(), (f, t));
    }
    let good: Vec<&(String, Value)> = scouts.iter().filter(|(_, v)| v["rating"].as_f64().unwrap_or(0.0) >= 6.0 && v["role"].as_str() != Some("skip")).collect();
    let pick = |role: &str, not: &[String]| good.iter().find(|(id, v)| v["role"].as_str() == Some(role) && !not.contains(id)).map(|(id, _)| id.clone());
    let opener = pick("opener", &[]).or_else(|| good.first().map(|(id, _)| id.clone()));
    let mut taken: Vec<String> = opener.iter().cloned().collect();
    let climax = pick("climax", &taken).or_else(|| good.iter().find(|(id, _)| !taken.contains(id)).map(|(id, _)| id.clone()));
    taken.extend(climax.iter().cloned());
    let len_of = |id: &String| d.trims.get(id).map_or(0.0, |(f, t)| t - f);
    let mut total: f64 = taken.iter().map(len_of).sum();
    let mut builds = Vec::new();
    for (id, _) in &good {
        if taken.contains(id) || builds.contains(id) {
            continue;
        }
        if total + len_of(id) > o.length + 3.0 {
            continue;
        }
        total += len_of(id);
        builds.push(id.clone());
    }
    d.order = opener.into_iter().chain(builds).chain(climax).collect();
    for (k, v) in reports.iter().filter(|(k, _)| k.starts_with("caption-")) {
        for (heard, written) in v["replace"].as_object().into_iter().flatten() {
            let w = written.as_str().unwrap_or("").to_string();
            if w.is_empty() || &w == heard {
                continue;
            }
            match d.replace.get(heard) {
                Some(old) if *old != w => d.conflicts.push(format!("{k}: \"{heard}\" as \"{w}\" (kept \"{old}\")")),
                Some(_) => {}
                None => {
                    d.replace.insert(heard.clone(), w);
                }
            }
        }
    }
    for (k, v) in reports.iter().filter(|(k, _)| k.starts_with("fx-")) {
        for (field, x) in v.as_object().into_iter().flatten() {
            if d.fx.insert(field.clone(), x.clone()).is_some() {
                d.conflicts.push(format!("{k}: {field} given twice"));
            }
        }
    }
    d
}

/// The decisions as a Bottle Clip plan (snapped to the beat), kept at `path`.
fn build(o: &Options, d: &Decisions, path: &Path) -> Result<Value, String> {
    if d.order.is_empty() {
        return Err("the merger has no clips to use (no candidate was rated 6 or more)".into());
    }
    let mut args: Vec<String> = vec!["plan".into(), o.style.clone()];
    for id in &d.order {
        let (f, t) = d.trims[id];
        args.extend([o.url.clone(), format!("{f:.2}"), format!("{t:.2}")]);
    }
    args.push("--json".into());
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut plan = clip(&refs)?["plan"].clone();
    let num = |k: &str, lo: f64, hi: f64| d.fx.get(k).and_then(|v| v.as_f64()).map(|x| x.clamp(lo, hi));
    if let Some(g) = d.fx.get("grade").and_then(|v| v.as_str()) {
        plan["look"]["grade"] = json!(g);
    }
    if let Some(x) = num("vignette", 0.0, 0.6) {
        plan["look"]["vignette"] = json!(x);
    }
    if let Some(x) = num("letterbox", 0.0, 0.12) {
        plan["look"]["letterbox"] = json!(x);
    }
    if let Some(tr) = d.fx.get("transition").and_then(|v| v.as_str()).filter(|t| ["cut", "flash", "fade"].contains(t)) {
        let s = num("transition_s", 0.0, 0.6).unwrap_or(0.2);
        for c in plan["clips"].as_array_mut().into_iter().flatten().skip(1) {
            c["transition"] = json!(tr);
            c["transition_s"] = json!(if tr == "cut" { 0.0 } else { s });
        }
    }
    if let Some(p) = d.fx.get("pulse").and_then(|v| v.as_str()) {
        let pulses = match p {
            "beat" => plan["music"]["beats"].clone(),
            "downbeat" => plan["music"]["downbeats"].clone(),
            _ => json!([]),
        };
        plan["pulses"] = pulses;
    }
    if let Some(z) = num("pulse_zoom", 0.0, 0.1) {
        plan["pulse_zoom"] = json!(z);
    }
    if let Some(t) = d.fx.get("title").and_then(|v| v.as_str()).filter(|t| !t.trim().is_empty()) {
        plan["titles"] = json!([{"text": t.trim().to_uppercase(), "at": 0.0, "dur": num("dur", 0.5, 3.0).unwrap_or(1.5)}]);
    }
    for (k, x) in [("caption_size", "size"), ("caption_y", "y")] {
        if let Some(v) = d.fx.get(k).and_then(|v| v.as_f64()) {
            plan["captions"][x] = json!(v);
        }
    }
    plan["captions"]["replace"] = json!(d.replace);
    agent::write(path, &serde_json::to_string_pretty(&plan).unwrap())?;
    Ok(plan)
}

fn render(plan: &Path, final_: bool) -> Result<Value, String> {
    let p = plan.display().to_string();
    let mut args = vec!["render", p.as_str(), "--json"];
    if final_ {
        args.push("--final");
    }
    clip(&args)
}

fn summary(plan: &Value, d: &Decisions) -> String {
    let mut s = String::new();
    for (k, c) in plan["clips"].as_array().into_iter().flatten().enumerate() {
        s += &format!("{}. {} {:.1}-{:.1} s of the video, at {:.1} s, in by {}\n", k + 1, d.order.get(k).map_or("?", |x| x.as_str()), c["from"].as_f64().unwrap_or(0.0), c["to"].as_f64().unwrap_or(0.0), c["at"].as_f64().unwrap_or(0.0), c["transition"].as_str().unwrap_or("cut"));
    }
    s += &format!("length {:.1} s; look {}; punch-ins {}, zoom {}; titles {}; caption spellings {}\n", plan["duration"].as_f64().unwrap_or(0.0), plan["look"], plan["pulses"].as_array().map_or(0, |a| a.len()), plan["pulse_zoom"], plan["titles"], plan["captions"]["replace"]);
    s
}

/// The director's changes: a new order (or fewer clips), effects set.
fn direct(d: &mut Decisions, v: &Value) {
    if let Some(order) = v["order"].as_array() {
        let ids: Vec<String> = order.iter().filter_map(|x| x.as_str().map(String::from)).filter(|id| d.trims.contains_key(id)).collect();
        if !ids.is_empty() {
            d.order = ids;
        }
    }
    for (k, x) in v["set"].as_object().into_iter().flatten() {
        d.fx.insert(k.clone(), x.clone());
    }
}

pub fn run(o: &Options) -> Result<Report, String> {
    let wall = Instant::now();
    let mode = if o.single { "single agent".to_string() } else { format!("swarm of {} agents", o.agents) };
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let dir = agent::home().join("runs").join(format!("{stamp}-{}", if o.single { "single" } else { "swarm" }));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let mut steps: Vec<Step> = Vec::new();
    let lap = |steps: &mut Vec<Step>, name: &str, t: &mut Instant, agents: usize, usage: Usage| {
        steps.push(Step { name: name.to_string(), seconds: t.elapsed().as_secs_f64(), agents, usage, waited: 0.0 });
        *t = Instant::now();
    };
    let mut t = Instant::now();
    println!("Bottle Box: {mode}, {} {:.0} s of {}", o.style, o.length, o.url);

    // 1. Bottle Clip analyses (cached)
    let watched = clip(&["watch", &o.url, "--json"])?;
    let count = o.candidates.to_string();
    let found = clip(&["find", &o.url, "--style", &o.style, "--min", "4", "--max", "12", "--count", &count, "--json"])?;
    let moments: Vec<Value> = found.as_array().cloned().unwrap_or_default();
    if moments.is_empty() {
        return Err("find_moments found no candidates".into());
    }
    lap(&mut steps, "analysis (Bottle Clip)", &mut t, 0, Usage::default());

    // 2. the shared context, in Nimbus; the pieces
    let context = shared(o, watched["title"].as_str().unwrap_or(""), &moments);
    let hash = nimbus::put_bytes(context.as_bytes())?;
    let shared_file = dir.join("shared.txt");
    agent::write(&shared_file, &context)?;
    let read = vec![agent::home().parent().unwrap().join(".bottleclip")];
    let setup = Setup { shared: Some(shared_file), workdir: dir.clone(), read };
    let all = pieces(o, moments.len());
    lap(&mut steps, "split", &mut t, 0, Usage::default());

    // 3. the swarm (or one agent for everything)
    let mut reports: BTreeMap<String, Value> = BTreeMap::new();
    let mut answers: Vec<Answer> = Vec::new();
    let single_session;
    if o.single {
        let all_text: String = all.iter().map(|(id, p)| format!("[{id}] {}\n", p.trim_start_matches("Your piece: "))).collect();
        let prompt = format!("You do every piece of this task yourself, one after another. The pieces:\n{all_text}\nReply with ONE JSON object whose keys are the piece ids above and whose values are each piece's reply object.");
        let ask = Ask { id: "single".into(), prompt, model: o.model.clone(), keep: true, resume: None, effort: None, want: Want::Json, tools: "Read".into(), allow: Vec::new(), workdir: None };
        let r = swarm::run("one agent, every piece", &setup, vec![ask], 1).pop().unwrap()?;
        for (id, _) in &all {
            if let Some(v) = r.json.get(id) {
                reports.insert(id.clone(), v.clone());
            }
        }
        single_session = Some(r.session.clone());
        answers.push(r);
    } else {
        let asks: Vec<Ask> = all.iter().map(|(id, p)| Ask { id: id.clone(), prompt: p.clone(), model: o.model.clone(), keep: false, resume: None, effort: None, want: Want::Json, tools: "Read".into(), allow: Vec::new(), workdir: None }).collect();
        for (r, (id, _)) in swarm::run(&format!("swarm: {} pieces, {} at once", asks.len(), o.agents), &setup, asks, o.agents).into_iter().zip(&all) {
            match r {
                Ok(a) => {
                    reports.insert(id.clone(), a.json.clone());
                    answers.push(a);
                }
                Err(e) => eprintln!("  {id} failed: {e}"),
            }
        }
        single_session = None;
    }
    let mut used = Usage::default();
    answers.iter().for_each(|a| used.add(&a.usage));
    lap(&mut steps, "pieces", &mut t, answers.len(), used.clone());
    for a in &answers {
        let _ = nimbus::put_bytes(serde_json::to_string(a).unwrap().as_bytes());
        agent::write(&dir.join("pieces").join(format!("{}.json", a.id)), &serde_json::to_string_pretty(a).unwrap())?;
    }

    // 4. merged and snapped to the beat; a preview
    let mut d = merge(o, &moments, &reports);
    let plan_path = dir.join("plan.json");
    let mut plan = build(o, &d, &plan_path)?;
    lap(&mut steps, "merge + plan_edit", &mut t, 0, Usage::default());
    let preview = render(&plan_path, false)?;
    lap(&mut steps, "preview render", &mut t, 0, Usage::default());

    // 5. the director: every report, the plan, the preview
    let grids: Vec<String> = preview["grids"].as_array().into_iter().flatten().filter_map(|g| g["path"].as_str().map(String::from)).collect();
    let reports_text = serde_json::to_string(&reports).unwrap();
    let ask_director = |grids: &[String], plan: &Value, d: &Decisions, resume: Option<String>, last: bool| Ask {
        id: "director".into(),
        prompt: format!(
            "{}You are the director. Every minion has reported to you: {reports_text}\n\nThe merger made this edit (clips by candidate id):\n{}{}\nThe preview's frame grids (one frame a second and at every cut; open them all): {}\n\nJudge the edit honestly for a {} montage: pacing, clip choice and order, the captions, the look. Reply {{\"verdict\":\"final\"{},\"notes\":\"<one line>\",\"order\":[<candidate ids, only to change the order or drop clips>],\"set\":{{<only effects to change: grade, vignette, letterbox, transition, transition_s, pulse, pulse_zoom, title, dur, caption_size, caption_y>}}{}}}",
            if resume.is_some() { "The pieces you sent back have been done again. " } else { "" },
            summary(plan, d),
            if d.conflicts.is_empty() { String::new() } else { format!("Conflicts the merger resolved: {}\n", d.conflicts.join("; ")) },
            grids.join(", "),
            o.style,
            if last { "" } else { "|\"redo\"" },
            if last { "" } else { ",\"redo\":[{\"piece\":\"<piece id>\",\"note\":\"<what to do otherwise>\"}]" }
        ),
        model: o.model.clone(),
        keep: true,
        resume: resume.or_else(|| single_session.clone()),
        effort: None,
        want: Want::Json,
        tools: "Read".into(),
        allow: Vec::new(),
        workdir: None,
    };
    let mut director_usage = Usage::default();
    let mut agents_used = answers.len();
    let first = swarm::run("director", &setup, vec![ask_director(&grids, &plan, &d, None, false)], 1).pop().unwrap()?;
    director_usage.add(&first.usage);
    agents_used += if o.single { 0 } else { 1 };
    agent::write(&dir.join("pieces").join("director-1.json"), &serde_json::to_string_pretty(&first).unwrap())?;
    direct(&mut d, &first.json);
    println!("  director: {} ({})", first.json["verdict"].as_str().unwrap_or("?"), first.json["notes"].as_str().unwrap_or(""));
    lap(&mut steps, "director", &mut t, 1, first.usage.clone());

    let redo: Vec<(String, String)> = first.json["redo"].as_array().into_iter().flatten().filter_map(|r| Some((r["piece"].as_str()?.to_string(), r["note"].as_str().unwrap_or("").to_string()))).filter(|(id, _)| all.iter().any(|(p, _)| p == id)).collect();
    if first.json["verdict"].as_str() == Some("redo") && !redo.is_empty() {
        // the pieces sent back, done again at once, then merged, previewed and judged again
        let asks: Vec<Ask> = redo
            .iter()
            .map(|(id, note)| {
                let p = &all.iter().find(|(x, _)| x == id).unwrap().1;
                Ask { id: id.clone(), prompt: format!("{p}\nThe director sent this piece back: {note}"), model: o.model.clone(), keep: false, resume: if o.single { single_session.clone() } else { None }, effort: None, want: Want::Json, tools: "Read".into(), allow: Vec::new(), workdir: None }
            })
            .collect();
        let mut u = Usage::default();
        let n = asks.len();
        for (r, (id, _)) in swarm::run("redo", &setup, asks, o.agents).into_iter().zip(&redo) {
            if let Ok(a) = r {
                u.add(&a.usage);
                reports.insert(id.clone(), a.json.clone());
            }
        }
        agents_used += if o.single { 0 } else { n };
        let kept_order = d.order.clone();
        let kept_fx = d.fx.clone();
        d = merge(o, &moments, &reports);
        // what the director already decided stands
        if first.json["order"].as_array().is_some_and(|a| !a.is_empty()) {
            d.order = kept_order.into_iter().filter(|id| d.trims.contains_key(id)).collect();
        }
        for (k, v) in kept_fx {
            d.fx.entry(k).or_insert(v);
        }
        direct(&mut d, &first.json);
        lap(&mut steps, "redo pieces", &mut t, n, u);
        plan = build(o, &d, &plan_path)?;
        let again = render(&plan_path, false)?;
        lap(&mut steps, "merge + preview again", &mut t, 0, Usage::default());
        let grids: Vec<String> = again["grids"].as_array().into_iter().flatten().filter_map(|g| g["path"].as_str().map(String::from)).collect();
        let second = swarm::run("director again", &setup, vec![ask_director(&grids, &plan, &d, Some(first.session.clone()), true)], 1).pop().unwrap()?;
        agent::write(&dir.join("pieces").join("director-2.json"), &serde_json::to_string_pretty(&second).unwrap())?;
        director_usage.add(&second.usage);
        direct(&mut d, &second.json);
        println!("  director: {} ({})", second.json["verdict"].as_str().unwrap_or("?"), second.json["notes"].as_str().unwrap_or(""));
        lap(&mut steps, "director again", &mut t, 1, second.usage.clone());
    }

    // the final render
    plan = build(o, &d, &plan_path)?;
    let done = render(&plan_path, true)?;
    lap(&mut steps, "final render", &mut t, 0, Usage::default());
    let _ = plan;
    let mut usage = used;
    usage.add(&director_usage);
    for s in steps.iter().filter(|s| s.name.starts_with("redo")) {
        usage.add(&s.usage);
    }
    let report = Report { mode, steps, wall: wall.elapsed().as_secs_f64(), agents: if o.single { 1 } else { agents_used }, usage, result: done["mp4"].as_str().unwrap_or("").to_string(), dir: dir.clone(), split: None };
    let steps_json: Vec<Value> = report.steps.iter().map(|s| json!({"step": s.name, "seconds": s.seconds, "agents": s.agents, "usage": s.usage})).collect();
    agent::write(&dir.join("summary.json"), &serde_json::to_string_pretty(&json!({"mode": report.mode, "wall": report.wall, "agents": report.agents, "usage": report.usage, "mp4": report.result, "shared_context": hash, "steps": steps_json})).unwrap())?;
    Ok(report)
}
