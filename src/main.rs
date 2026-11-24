//! Bottle Box: one task split across many small Claude agents working at
//! the same time and sharing one context, so it is done in seconds instead
//! of minutes. General-purpose; its first drivers are Bottle Clip and code.
//!
//!   bottlebox clip <url> [--style hype] [--length 30] [--agents 20] [--candidates 12] [--model sonnet] [--single] [--compare]
//!   bottlebox code --task-file FILE --dir DIR [--agents 20] [--per-task 2] [--merge-minions] [--model sonnet] [--baseline 5] [--bench]
//!
//! --agents N: how many agents run at once (the rest wait their turn).
//! code --baseline K: K ordinary Claude Code agents at once instead of the swarm.
//! code --bench: the swarm (2 files a task: 20 minions for 10 tasks) against 5 Claude Code agents, side by side;
//! --third adds 10 minions (1 file a task).

#[allow(dead_code)]
#[path = "../../Strike/src/lightning.rs"]
mod lightning;
#[allow(dead_code)]
#[path = "../../Strike/src/fstrike.rs"]
mod nimbus;
mod agent;
mod clip;
mod code;
mod swarm;

fn print(r: &clip::Report) {
    println!("\n== {} ==", r.mode);
    println!("{:<40} {:>8} {:>6} {:>10} {:>9} {:>10} {:>10} {:>8} {:>8}", "step", "seconds", "agents", "tokens", "output", "thinking", "cache read", "waits s", "cost $");
    let row = |name: &str, secs: f64, agents: usize, u: &agent::Usage, waited: f64| println!("{:<40} {:>8.1} {:>6} {:>10} {:>9} {:>10} {:>10} {:>8.1} {:>8.3}", name, secs, agents, u.tokens(), u.output, u.thinking, u.cache_read, waited, u.cost_usd);
    for s in &r.steps {
        row(&s.name, s.seconds, s.agents, &s.usage, s.waited);
    }
    row("total", r.wall, r.agents, &r.usage, r.steps.iter().map(|s| s.waited).sum());
    println!("tokens: {} in, {} out ({} of it thinking), {} written to the cache, {} read from it", r.usage.input, r.usage.output, r.usage.thinking, r.usage.cache_write, r.usage.cache_read);
    println!("result: {}\nrun: {}", r.result, r.dir.display());
    let line = format!("{}  {}  wall {:.1} s  agents {}  tokens {}  cost ${:.3}  {}\n", r.dir.file_name().unwrap().to_string_lossy(), r.mode, r.wall, r.agents, r.usage.tokens(), r.usage.cost_usd, r.result);
    let log = agent::home().join("usage.log");
    let _ = std::fs::OpenOptions::new().create(true).append(true).open(&log).and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
}

/// The runs side by side: time, speedup against the first baseline, tokens.
fn compare(done: &[clip::Report]) {
    if done.len() < 2 {
        return;
    }
    println!("\n== side by side ==");
    let base = done.iter().find(|r| r.mode.contains("Claude Code") || r.mode.contains("single")).unwrap_or(&done[done.len() - 1]);
    print!("{:<28}", "");
    for r in done {
        print!(" {:>24}", r.mode.chars().take(24).collect::<String>());
    }
    println!();
    let line = |name: &str, f: &dyn Fn(&clip::Report) -> String| {
        print!("{name:<28}");
        for r in done {
            print!(" {:>24}", f(r));
        }
        println!();
    };
    line("wall time, s", &|r| format!("{:.1}", r.wall));
    line("speedup", &|r| format!("{:.2}x", base.wall / r.wall.max(0.1)));
    line("writing, s", &|r| r.split.map_or("-".into(), |s| format!("{:.1}", s.0)));
    line("testing, s", &|r| r.split.map_or("-".into(), |s| format!("{:.1}", s.1)));
    line("slowest step", &|r| r.steps.iter().max_by(|a, b| a.seconds.total_cmp(&b.seconds)).map_or(String::new(), |s| format!("{} {:.0}s", s.name.split(' ').next().unwrap_or(""), s.seconds)));
    line("agents", &|r| r.agents.to_string());
    line("tokens", &|r| r.usage.tokens().to_string());
    line("  output (thinking)", &|r| format!("{} ({})", r.usage.output, r.usage.thinking));
    line("  read from cache", &|r| r.usage.cache_read.to_string());
    line("rate-limit waits, s", &|r| format!("{:.1}", r.steps.iter().map(|s| s.waited).sum::<f64>()));
    line("cost at API prices, $", &|r| format!("{:.3}", r.usage.cost_usd));
    line("result", &|r| r.result.split(" (").next().unwrap_or("").to_string());
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| args.iter().any(|a| a == name);
    let value = |name: &str, or: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned().unwrap_or(or.to_string());
    let num = |name: &str, or: &str| value(name, or).parse::<f64>().unwrap_or_else(|_| {
        eprintln!("{name}: not a number");
        std::process::exit(2)
    });
    let usage = "bottlebox clip <url> [--style hype] [--length 30] [--agents 20] [--candidates 12] [--model sonnet] [--single] [--compare]\nbottlebox code --task-file FILE --dir DIR [--agents 20] [--per-task 2] [--merge-minions] [--baseline K] [--bench] [--model sonnet]";
    let fail = |e: String| -> ! {
        eprintln!("bottlebox: {e}");
        std::process::exit(1)
    };
    let mut done = Vec::new();
    match args.first().map(String::as_str) {
        Some("clip") => {
            let Some(url) = args.get(1) else { fail(usage.into()) };
            let runs: Vec<bool> = if flag("--compare") { vec![false, true] } else { vec![flag("--single")] };
            for single in runs {
                let o = clip::Options { url: url.clone(), style: value("--style", "hype"), length: num("--length", "30"), agents: num("--agents", "20") as usize, candidates: num("--candidates", "12") as usize, model: value("--model", "sonnet"), single };
                let r = clip::run(&o).unwrap_or_else(|e| fail(e));
                print(&r);
                done.push(r);
            }
        }
        Some("code") => {
            let task = std::fs::read_to_string(value("--task-file", "")).unwrap_or_else(|e| fail(format!("--task-file: {e}")));
            let dir = std::path::PathBuf::from(value("--dir", ""));
            if dir.as_os_str().is_empty() {
                fail(usage.into());
            }
            let model = value("--model", "sonnet");
            let agents = num("--agents", "20") as usize;
            let runs: Vec<(code::How, &str)> = if flag("--bench") {
                // the ratio rule: 20 minions against 5 Claude Code agents (and with --third, 10 minions, one a task)
                let mut v = vec![(code::How::Swarm { per_task: 2, merge_minions: false }, "swarm20"), (code::How::Baseline(5), "baseline5")];
                if flag("--third") {
                    v.push((code::How::Swarm { per_task: 1, merge_minions: false }, "swarm10"));
                }
                v
            } else if args.iter().any(|a| a == "--baseline") {
                vec![(code::How::Baseline(num("--baseline", "5") as usize), "")]
            } else {
                vec![(code::How::Swarm { per_task: num("--per-task", "2") as usize, merge_minions: flag("--merge-minions") }, "")]
            };
            let many = runs.len() > 1;
            for (how, name) in runs {
                let d = if many { dir.join(name) } else { dir.clone() };
                let r = code::run(&code::Options { task: task.clone(), dir: d, agents, model: model.clone(), how }).unwrap_or_else(|e| fail(e));
                print(&r);
                done.push(r);
            }
        }
        _ => fail(usage.into()),
    }
    compare(&done);
}
