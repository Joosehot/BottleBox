//! The swarm: many agents at once, at most N running, the rest queued; a
//! live board in the terminal (running, done, waiting, each piece's time).
//!
//! The first agent is started alone, and the others when its first token
//! has come: by then its prompt (the shared prefix) is in the prompt cache,
//! so they read it from there instead of each writing it again.

use crate::agent::{self, Answer, Ask, Setup};
use crossterm::{cursor, terminal, QueueableCommand};
use std::io::{IsTerminal, Write};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// When the run began: the log's lines give the whole run's time beside the step's.
static RUN: Mutex<Option<Instant>> = Mutex::new(None);

/// A run begins: its clock starts.
pub fn begin_run() {
    *RUN.lock().unwrap() = Some(Instant::now());
}

fn run_s() -> f64 {
    RUN.lock().unwrap().map_or(0.0, |t| t.elapsed().as_secs_f64())
}

/// The longest the others wait for the first agent's first token (seconds).
const WARM_AT_MOST: f64 = 30.0;

#[derive(Clone)]
struct Row {
    id: String,
    state: String,
    began: Option<Instant>,
    seconds: Option<f64>,
    tokens: u64,
}

struct Board {
    title: String,
    rows: Vec<Row>,
    began: Instant,
    drawn: usize,
}

impl Board {
    fn draw(&mut self, live: bool) {
        if !live {
            return;
        }
        let mut out = std::io::stdout();
        if self.drawn > 0 {
            let _ = out.queue(cursor::MoveUp(self.drawn as u16));
        }
        let (mut run, mut done, mut wait, mut queued) = (0, 0, 0, 0);
        for r in &self.rows {
            match r.state.as_str() {
                "done" | "failed" => done += 1,
                "queued" => queued += 1,
                s if s.starts_with("rate") => wait += 1,
                _ => run += 1,
            }
        }
        let width = terminal::size().map_or(100, |(w, _)| w as usize).max(40);
        let mut lines = vec![format!("{}  {:.1} s   running {run}  done {done}  waiting {wait}  queued {queued}", self.title, self.began.elapsed().as_secs_f64())];
        let cols = (width / 34).max(1);
        for chunk in self.rows.chunks(cols) {
            let line: Vec<String> = chunk
                .iter()
                .map(|r| {
                    let t = r.seconds.or(r.began.map(|b| b.elapsed().as_secs_f64())).map_or("     ".to_string(), |s| format!("{s:5.1}"));
                    let state: String = r.state.chars().take(12).collect();
                    format!("{:<12} {:<12} {t}s", r.id.chars().take(12).collect::<String>(), state)
                })
                .collect();
            lines.push(line.join("   "));
        }
        for l in &lines {
            let _ = out.queue(terminal::Clear(terminal::ClearType::CurrentLine));
            let _ = writeln!(out, "{}", l.chars().take(width - 1).collect::<String>());
        }
        let _ = out.flush();
        self.drawn = lines.len();
    }
}

/// Runs every ask, at most `n` at once. Returns the answers in the asks' order
/// (an agent that failed for good is an Err in its place).
pub fn run(title: &str, setup: &Setup, mut asks: Vec<Ask>, n: usize) -> Vec<Result<Answer, String>> {
    // the first agent warms the cache: the others start when its first token has come
    // (only where they share a prefix: ordinary Claude Code agents each start at once)
    let warm: Option<agent::Signal> = (asks.len() > 1 && setup.shared.is_some()).then(|| Arc::new((Mutex::new(false), Condvar::new())));
    if let Some(w) = &warm {
        asks[0].first_token = Some(w.clone());
    }
    let live = std::io::stdout().is_terminal();
    let rows = asks.iter().map(|a| Row { id: a.id.clone(), state: "queued".into(), began: None, seconds: None, tokens: 0 }).collect();
    let board = Arc::new(Mutex::new(Board { title: title.to_string(), rows, began: Instant::now(), drawn: 0 }));
    let slots = Arc::new((Mutex::new(n.max(1)), Condvar::new()));
    let results: Arc<Mutex<Vec<Option<Result<Answer, String>>>>> = Arc::new(Mutex::new(vec![None; asks.len()]));
    let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
    if !live {
        println!("-- [run {:6.1} s] {title}: {} agents", run_s(), asks.len());
    }
    std::thread::scope(|scope| {
        // the board, redrawn five times a second
        let (b2, f2) = (board.clone(), finished.clone());
        scope.spawn(move || {
            while !f2.load(std::sync::atomic::Ordering::Relaxed) {
                b2.lock().unwrap().draw(live);
                std::thread::sleep(Duration::from_millis(200));
            }
        });
        let mut handles = Vec::new();
        for (i, ask) in asks.iter().enumerate() {
            if let (1, Some(w)) = (i, &warm) {
                let (lock, cv) = &**w;
                let up = lock.lock().unwrap();
                let _ = cv.wait_timeout_while(up, Duration::from_secs_f64(WARM_AT_MOST), |up| !*up).unwrap();
                if !live {
                    println!("[run {:6.1} s] the first agent's prompt is in the cache: the others start", run_s());
                }
            }
            let (board, slots, results) = (board.clone(), slots.clone(), results.clone());
            handles.push(scope.spawn(move || {
                {
                    let (lock, cv) = &*slots;
                    let mut free = lock.lock().unwrap();
                    while *free == 0 {
                        free = cv.wait(free).unwrap();
                    }
                    *free -= 1;
                }
                board.lock().unwrap().rows[i].began = Some(Instant::now());
                // without a terminal (a log file), every change is a line
                let set = |s: &str| {
                    let mut b = board.lock().unwrap();
                    if !live && b.rows[i].state != s {
                        println!("[run {:6.1} s | step {:6.1} s] {} {s}", run_s(), b.began.elapsed().as_secs_f64(), b.rows[i].id);
                    }
                    b.rows[i].state = s.to_string();
                };
                let r = agent::run(setup, ask, &set);
                {
                    let mut b = board.lock().unwrap();
                    let row = &mut b.rows[i];
                    row.seconds = row.began.map(|t| t.elapsed().as_secs_f64());
                    match &r {
                        Ok(a) => {
                            row.state = "done".into();
                            row.tokens = a.usage.tokens();
                        }
                        Err(_) => row.state = "failed".into(),
                    }
                    let (id, state, secs, tokens) = (row.id.clone(), row.state.clone(), row.seconds.unwrap_or(0.0), row.tokens);
                    if !live {
                        println!("[run {:6.1} s | step {:6.1} s] {id} {state} in {secs:.1} s, {tokens} tokens", run_s(), b.began.elapsed().as_secs_f64());
                    }
                }
                results.lock().unwrap()[i] = Some(r);
                let (lock, cv) = &*slots;
                *lock.lock().unwrap() += 1;
                cv.notify_one();
            }));
        }
        for h in handles {
            let _ = h.join();
        }
        finished.store(true, std::sync::atomic::Ordering::Relaxed);
    });
    board.lock().unwrap().draw(live);
    let mut res = results.lock().unwrap();
    res.iter_mut().map(|r| r.take().unwrap_or_else(|| Err("not run".into()))).collect()
}
