//! Bottle Box: one task split across many small Claude agents working at
//! the same time and sharing one context, so it is done in seconds instead
//! of minutes. General-purpose; its first driver is Bottle Clip.
//!
//!   bottlebox clip <url> [--style hype] [--length 30] [--agents 20] [--candidates 12] [--model sonnet] [--single] [--compare]
//!
//! --agents N: how many agents run at once (the rest wait their turn).
//! --single: one agent does the whole task alone. --compare: the swarm, then one agent, side by side.

#[allow(dead_code)]
#[path = "../../Strike/src/lightning.rs"]
mod lightning;
#[allow(dead_code)]
#[path = "../../Strike/src/fstrike.rs"]
mod nimbus;
mod agent;
mod clip;
mod swarm;


fn print(r: &clip::Report) {
    println!("\n== {} ==", r.mode);
    println!("{:<26} {:>8} {:>7} {:>10} {:>10} {:>10}", "step", "seconds", "agents", "tokens", "cache read", "cost $");
    for s in &r.steps {
        println!("{:<26} {:>8.1} {:>7} {:>10} {:>10} {:>10.3}", s.name, s.seconds, s.agents, s.usage.tokens(), s.usage.cache_read, s.usage.cost_usd);
    }
    println!("{:<26} {:>8.1} {:>7} {:>10} {:>10} {:>10.3}", "total", r.wall, r.agents, r.usage.tokens(), r.usage.cache_read, r.usage.cost_usd);
    println!("tokens: {} in, {} out, {} written to the cache, {} read from it", r.usage.input, r.usage.output, r.usage.cache_write, r.usage.cache_read);
    println!("video: {}\nrun: {}", r.mp4, r.dir.display());
    let line = format!("{}  {}  wall {:.1} s  agents {}  tokens {}  cost ${:.3}  {}\n", r.dir.file_name().unwrap().to_string_lossy(), r.mode, r.wall, r.agents, r.usage.tokens(), r.usage.cost_usd, r.mp4);
    let log = agent::home().join("usage.log");
    let _ = std::fs::OpenOptions::new().create(true).append(true).open(&log).and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| args.iter().any(|a| a == name);
    let value = |name: &str, or: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned().unwrap_or(or.to_string());
    let usage = "bottlebox clip <url> [--style hype] [--length 30] [--agents 20] [--candidates 12] [--model sonnet] [--single] [--compare]";
    let (Some("clip"), Some(url)) = (args.first().map(String::as_str), args.get(1)) else {
        eprintln!("{usage}");
        std::process::exit(2);
    };
    let num = |name: &str, or: &str| value(name, or).parse::<f64>().unwrap_or_else(|_| {
        eprintln!("{name}: not a number");
        std::process::exit(2)
    });
    let o = clip::Options {
        url: url.clone(),
        style: value("--style", "hype"),
        length: num("--length", "30"),
        agents: num("--agents", "20") as usize,
        candidates: num("--candidates", "12") as usize,
        model: value("--model", "sonnet"),
        single: flag("--single"),
    };
    let mut done = Vec::new();
    let runs: Vec<bool> = if flag("--compare") { vec![false, true] } else { vec![o.single] };
    for single in runs {
        match clip::run(&clip::Options { single, url: o.url.clone(), style: o.style.clone(), model: o.model.clone(), ..o }) {
            Ok(r) => {
                print(&r);
                done.push(r);
            }
            Err(e) => {
                eprintln!("bottlebox: {e}");
                std::process::exit(1);
            }
        }
    }
    if let [swarm, single] = done.as_slice() {
        println!("\n== the swarm against one agent ==");
        println!("{:<22} {:>14} {:>14}", "", "swarm", "one agent");
        println!("{:<22} {:>14.1} {:>14.1}", "wall time, s", swarm.wall, single.wall);
        let agent_s = |r: &clip::Report| r.steps.iter().filter(|s| s.agents > 0).map(|s| s.seconds).sum::<f64>();
        println!("{:<22} {:>14.1} {:>14.1}", "of it the agents, s", agent_s(swarm), agent_s(single));
        println!("{:<22} {:>14} {:>14}", "agents", swarm.agents, single.agents);
        println!("{:<22} {:>14} {:>14}", "tokens", swarm.usage.tokens(), single.usage.tokens());
        println!("{:<22} {:>14} {:>14}", "  read from cache", swarm.usage.cache_read, single.usage.cache_read);
        println!("{:<22} {:>14} {:>14}", "  output", swarm.usage.output, single.usage.output);
        println!("{:<22} {:>14.3} {:>14.3}", "cost at API prices, $", swarm.usage.cost_usd, single.usage.cost_usd);
        println!("{:<22} {:>14.1}x", "faster", single.wall / swarm.wall.max(0.1));
    }
}
