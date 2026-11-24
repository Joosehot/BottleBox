//! One agent: a headless Claude Code (`claude -p`) given the shared context
//! as its system prompt and its own piece as the message.
//!
//! The shared context comes first and is the same file for every agent, so
//! the prompt cache holds it once and every agent after the first reads it
//! from there. A minion runs with only the tools its piece needs (Read, or
//! none), no MCP servers, no user settings (so no plugins such as Shallow, no
//! hooks), no skills, and the effort its role is given (the director thinks
//! hard once; minions think little and execute).
//!
//! An answer is one JSON object, or files: each as a line
//! `=== FILE: <path> ===` and then its whole content, the last one ended by
//! `=== END ===`.
//!
//! A rate limit is not a failure: the agent waits and asks again (2^k x 5 s,
//! at most 5 minutes), as often as it takes.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    /// of the output, the extended thinking
    #[serde(default)]
    pub thinking: u64,
    pub cache_write: u64,
    pub cache_read: u64,
    /// what the API would have charged (the Max plan pays none of it; it is a measure)
    pub cost_usd: f64,
}

impl Usage {
    pub fn add(&mut self, o: &Usage) {
        self.input += o.input;
        self.output += o.output;
        self.thinking += o.thinking;
        self.cache_write += o.cache_write;
        self.cache_read += o.cache_read;
        self.cost_usd += o.cost_usd;
    }
    /// What the tokens cost at Claude Opus 5.5's API prices (per million: input $4, output $20,
    /// cache read $0.20, cache write $8 for the 1-hour cache Claude Code writes). Claude Code's own
    /// total_cost_usd counts a resumed session's earlier turns again, so it is not used for the total.
    pub fn cost(&self) -> f64 {
        (self.input as f64 * 4.0 + self.output as f64 * 20.0 + self.cache_read as f64 * 0.2 + self.cache_write as f64 * 8.0) / 1e6
    }
    pub fn tokens(&self) -> u64 {
        self.input + self.output + self.cache_write + self.cache_read
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum Want {
    Json,
    Files,
    /// anything (an agent that does its work with tools)
    Text,
}

#[derive(Clone)]
pub struct Ask {
    pub id: String,
    pub prompt: String,
    pub model: String,
    /// keep the session, to be asked again (the director)
    pub keep: bool,
    pub resume: Option<String>,
    /// low, medium, high, xhigh, max (None: Claude Code's own)
    pub effort: Option<String>,
    pub want: Want,
    /// the built-in tools it may use ("" for none)
    pub tools: String,
    /// more permissions (e.g. "Bash(cargo:*)")
    pub allow: Vec<String>,
    /// where it runs (None: the setup's folder)
    pub workdir: Option<PathBuf>,
    /// an MCP config file (the Box's tools), and the most output tokens a turn may give
    pub mcp: Option<PathBuf>,
    pub max_output: Option<u32>,
    /// told when the model's first token comes (its prompt is in the cache by then): the swarm
    /// starts the other agents only after the first one's prompt has written the cache
    pub first_token: Option<Signal>,
    /// a stock Claude Code agent (the benchmark's baseline): see `STOCK`
    pub stock: bool,
    /// with `resume`: go on from that session in a new one (its conversation is read from the cache)
    pub fork: bool,
}

/// The baseline of every benchmark, FROZEN: a stock Claude Code agent on Claude Opus 5.5 with its
/// default settings (the user's own settings, plugins and MCP servers, its default tools and system
/// prompt, its default effort). Only what an unattended `claude -p` cannot do without is given:
/// permission to edit files and to run cargo (nobody is there to answer a permission prompt), and an
/// output format that carries the token counts. Do not change this, and never give it any of Bottle
/// Box's improvements.
pub const STOCK: [&str; 9] = ["-p", "--model", "claude-opus-5-5", "--output-format", "stream-json", "--verbose", "--permission-mode", "acceptEdits", "--allowedTools=Bash(cargo:*)"];

/// A flag one thread raises and others wait for.
pub type Signal = std::sync::Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>;

pub fn raise(s: &Signal) {
    *s.0.lock().unwrap() = true;
    s.1.notify_all();
}

impl Ask {
    pub fn minion(id: &str, prompt: String, model: &str, want: Want) -> Ask {
        Ask { id: id.to_string(), prompt, model: model.to_string(), keep: false, resume: None, effort: Some("low".into()), want, tools: "Read".into(), allow: Vec::new(), workdir: None, mcp: None, max_output: None, first_token: None, stock: false, fork: false }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Answer {
    pub id: String,
    pub json: Value,
    #[serde(default)]
    pub files: BTreeMap<String, String>,
    pub text: String,
    pub session: String,
    pub usage: Usage,
    pub seconds: f64,
    pub attempts: u32,
    /// seconds spent waiting out rate limits
    pub waited: f64,
    /// of the seconds, in its own cargo commands (an agent with tools)
    #[serde(default)]
    pub cargo_seconds: f64,
}

/// What an agent run needs besides its piece.
pub struct Setup {
    /// the shared context, given as the system prompt (None: Claude Code's own)
    pub shared: Option<PathBuf>,
    pub workdir: PathBuf,
    /// folders the agent may read
    pub read: Vec<PathBuf>,
}

/// The first JSON object in a text.
pub fn json_in(text: &str) -> Option<Value> {
    let a = text.find('{')?;
    let b = text.rfind('}')?;
    serde_json::from_str(&text[a..=b]).ok()
}

/// The files in an answer (`=== FILE: path ===` ... `=== END ===`); a code
/// fence around a file's content is taken off.
pub fn files_in(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut current: Option<(String, Vec<&str>)> = None;
    let close = |c: Option<(String, Vec<&str>)>, out: &mut BTreeMap<String, String>| {
        if let Some((path, mut lines)) = c {
            while lines.first().is_some_and(|l| l.trim().is_empty()) {
                lines.remove(0);
            }
            while lines.last().is_some_and(|l| l.trim().is_empty()) {
                lines.pop();
            }
            if lines.first().is_some_and(|l| l.trim_start().starts_with("```")) && lines.last().is_some_and(|l| l.trim() == "```") {
                lines.remove(0);
                lines.pop();
            }
            out.insert(path, lines.join("\n") + "\n");
        }
    };
    for line in text.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("=== FILE:") {
            close(current.take(), &mut out);
            current = Some((rest.trim_end_matches('=').trim().replace('\\', "/"), Vec::new()));
        } else if t == "=== END ===" {
            close(current.take(), &mut out);
        } else if let Some((_, lines)) = current.as_mut() {
            lines.push(line);
        }
    }
    close(current, &mut out);
    out
}

fn limited(v: &Value) -> bool {
    let text = format!("{} {}", v["result"].as_str().unwrap_or(""), v["api_error_status"]);
    let t = text.to_lowercase();
    v["api_error_status"].as_u64().is_some_and(|s| s == 429 || s == 529) || ["rate limit", "rate_limit", "usage limit", "overloaded", "too many requests", "429"].iter().any(|k| t.contains(k))
}

/// Runs one agent to its answer. `status` is told what it is doing.
pub fn run(setup: &Setup, ask: &Ask, status: &dyn Fn(&str)) -> Result<Answer, String> {
    let began = Instant::now();
    let (mut attempts, mut waited, mut backoff) = (0u32, 0.0f64, 5.0f64);
    let mut prompt = ask.prompt.clone();
    let mut usage = Usage::default();
    // seconds its cargo commands (builds, tests) took, for an agent with tools
    let mut cargo_total = 0.0f64;
    loop {
        attempts += 1;
        status("running");
        let mut c = Command::new("claude");
        c.current_dir(ask.workdir.as_ref().unwrap_or(&setup.workdir));
        // every agent (the baseline's too) runs on the owner's Claude subscription, never on an API key
        // that the environment may hold: that changes only who is billed, not how the agent works
        c.env_remove("ANTHROPIC_API_KEY").env_remove("ANTHROPIC_AUTH_TOKEN");
        if ask.stock {
            // the frozen baseline: STOCK and nothing else
            c.args(STOCK);
        } else {
            c.args(["-p", "--model", &ask.model]);
            // no user settings (so no plugins such as Shallow, no hooks), no skills, no MCP servers, for every agent of the box
            c.args(["--setting-sources", "project", "--disable-slash-commands", "--strict-mcp-config"]);
        }
        let streamed = ask.stock || (ask.want == Want::Text && !ask.tools.is_empty()) || ask.first_token.is_some();
        if !ask.stock {
        if let Some(s) = &setup.shared {
            c.arg("--system-prompt-file").arg(s);
        }
        c.args(["--tools", &ask.tools]);
        let mut allowed: Vec<String> = ask.tools.split(',').filter(|t| !t.is_empty()).map(String::from).collect();
        allowed.extend(ask.allow.iter().cloned());
        if let Some(m) = &ask.mcp {
            c.arg("--mcp-config").arg(m);
            allowed.push("mcp__box".into());
        }
        if let Some(n) = ask.max_output {
            c.env("CLAUDE_CODE_MAX_OUTPUT_TOKENS", n.to_string());
        }
        if !allowed.is_empty() {
            c.arg("--allowedTools").arg(allowed.join(","));
        }
        if let Some(e) = &ask.effort {
            c.args(["--effort", e]);
        }
        for d in &setup.read {
            c.arg("--add-dir").arg(d);
        }
        if !ask.keep {
            c.arg("--no-session-persistence");
        }
        if let Some(s) = &ask.resume {
            c.args(["--resume", s]);
            if ask.fork {
                c.arg("--fork-session");
            }
        }
        // an agent that works with its tools is read as a stream, so the time its cargo commands take can be told apart;
        // so is one whose first token is waited for (with the partial messages, which come as the model writes)
        c.args(["--output-format", if streamed { "stream-json" } else { "json" }]);
        if streamed {
            c.arg("--verbose");
        }
        if ask.first_token.is_some() {
            c.arg("--include-partial-messages");
        }
        }
        let mut child = c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| format!("claude: {e}"))?;
        child.stdin.take().unwrap().write_all(prompt.as_bytes()).map_err(|e| e.to_string())?;
        let stdout_bytes = if streamed {
            use std::io::BufRead;
            let reader = std::io::BufReader::new(child.stdout.take().unwrap());
            let mut started: BTreeMap<String, Instant> = BTreeMap::new();
            let mut last = Vec::new();
            for line in reader.lines().map_while(Result::ok) {
                let Ok(e) = serde_json::from_str::<Value>(&line) else { continue };
                if e["type"] == "stream_event" {
                    if let Some(s) = &ask.first_token {
                        raise(s);
                    }
                }
                for part in e["message"]["content"].as_array().into_iter().flatten() {
                    if part["type"] == "tool_use" && part["input"]["command"].as_str().is_some_and(|c| c.contains("cargo")) {
                        started.insert(part["id"].as_str().unwrap_or("").to_string(), Instant::now());
                    }
                    if part["type"] == "tool_result" {
                        if let Some(t) = started.remove(part["tool_use_id"].as_str().unwrap_or("")) {
                            cargo_total += t.elapsed().as_secs_f64();
                        }
                    }
                }
                if e["type"] == "result" {
                    last = line.into_bytes();
                }
            }
            last
        } else {
            Vec::new()
        };
        let out = child.wait_with_output().map_err(|e| e.to_string())?;
        // whatever came, nobody waits for this agent's first token any longer
        if let Some(s) = &ask.first_token {
            raise(s);
        }
        let stdout = if streamed { stdout_bytes } else { out.stdout.clone() };
        let v: Value = match serde_json::from_slice(&stdout) {
            Ok(v) => v,
            Err(_) => {
                let err = String::from_utf8_lossy(&out.stderr).to_string();
                if limited(&serde_json::json!({"result": err})) {
                    Value::Null
                } else {
                    return Err(format!("{}: claude gave no answer: {}", ask.id, err.trim()));
                }
            }
        };
        if v.is_null() || (v["is_error"].as_bool() == Some(true) && limited(&v)) {
            // a rate limit: wait, then ask again
            status(&format!("rate limited, waiting {backoff:.0} s"));
            std::thread::sleep(std::time::Duration::from_secs_f64(backoff));
            waited += backoff;
            backoff = (backoff * 2.0).min(300.0);
            continue;
        }
        let u = &v["usage"];
        usage.add(&Usage {
            input: u["input_tokens"].as_u64().unwrap_or(0),
            output: u["output_tokens"].as_u64().unwrap_or(0),
            thinking: u["output_tokens_details"]["thinking_tokens"].as_u64().unwrap_or(0),
            cache_write: u["cache_creation_input_tokens"].as_u64().unwrap_or(0),
            cache_read: u["cache_read_input_tokens"].as_u64().unwrap_or(0),
            cost_usd: v["total_cost_usd"].as_f64().unwrap_or(0.0),
        });
        if v["is_error"].as_bool() == Some(true) {
            return Err(format!("{}: {}", ask.id, v["result"].as_str().unwrap_or("an error")));
        }
        let text = v["result"].as_str().unwrap_or("").to_string();
        let session = v["session_id"].as_str().unwrap_or("").to_string();
        let answer = |json: Value, files: BTreeMap<String, String>| Answer { id: ask.id.clone(), json, files, text: text.clone(), session: session.clone(), usage: usage.clone(), seconds: began.elapsed().as_secs_f64(), attempts, waited, cargo_seconds: cargo_total };
        let got = match ask.want {
            Want::Text => Some(answer(Value::Null, BTreeMap::new())),
            Want::Json => json_in(&text).map(|j| answer(j, BTreeMap::new())),
            Want::Files => Some(files_in(&text)).filter(|f| !f.is_empty()).map(|f| answer(Value::Null, f)),
        };
        match got {
            Some(a) => return Ok(a),
            None if attempts < 3 => {
                let form = if ask.want == Want::Json { "the JSON object only" } else { "the files only, each as a line `=== FILE: <path> ===` and then its whole content, the last ended by `=== END ===`" };
                prompt = format!("{}\n\nReply with {form}.", ask.prompt);
            }
            None => return Err(format!("{}: no answer in the asked form: {}", ask.id, text.chars().take(200).collect::<String>())),
        }
    }
}

/// Where Bottle Box keeps its runs (`BOTTLEBOX_HOME`, or ~/.bottlebox).
pub fn home() -> PathBuf {
    if let Some(h) = std::env::var_os("BOTTLEBOX_HOME") {
        return PathBuf::from(h);
    }
    std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from).unwrap_or_else(|| PathBuf::from(".")).join(".bottlebox")
}

pub fn write(path: &Path, text: &str) -> Result<(), String> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
    }
    std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
}
