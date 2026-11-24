//! One agent: a headless Claude Code (`claude -p`) given the shared context
//! as its system prompt and its own piece as the message, answering with
//! one JSON object.
//!
//! The shared context comes first and is the same file for every agent, so
//! the prompt cache holds it once and every agent after the first reads it
//! from there. Each agent runs with only the Read tool (to open images),
//! no MCP servers, no user settings (so no plugins such as Shallow, no
//! hooks), no skills: it starts fast and costs little.
//!
//! A rate limit is not a failure: the agent waits and asks again (2^k x 5 s,
//! at most 5 minutes), as often as it takes.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_write: u64,
    pub cache_read: u64,
    /// what the API would have charged (the Max plan pays none of it; it is a measure)
    pub cost_usd: f64,
}

impl Usage {
    pub fn add(&mut self, o: &Usage) {
        self.input += o.input;
        self.output += o.output;
        self.cache_write += o.cache_write;
        self.cache_read += o.cache_read;
        self.cost_usd += o.cost_usd;
    }
    pub fn tokens(&self) -> u64 {
        self.input + self.output + self.cache_write + self.cache_read
    }
}

#[derive(Clone)]
pub struct Ask {
    pub id: String,
    pub prompt: String,
    pub model: String,
    /// keep the session, to be asked again (the director)
    pub keep: bool,
    pub resume: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Answer {
    pub id: String,
    pub json: Value,
    pub text: String,
    pub session: String,
    pub usage: Usage,
    pub seconds: f64,
    pub attempts: u32,
    /// seconds spent waiting out rate limits
    pub waited: f64,
}

/// What an agent run needs besides its piece.
pub struct Setup {
    pub shared: PathBuf,
    pub workdir: PathBuf,
    /// folders the agent may read (the images)
    pub read: Vec<PathBuf>,
}

/// The first JSON object in a text.
pub fn json_in(text: &str) -> Option<Value> {
    let a = text.find('{')?;
    let b = text.rfind('}')?;
    serde_json::from_str(&text[a..=b]).ok()
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
    loop {
        attempts += 1;
        status("running");
        let mut c = Command::new("claude");
        c.current_dir(&setup.workdir)
            .args(["-p", "--output-format", "json", "--model", &ask.model, "--system-prompt-file"])
            .arg(&setup.shared)
            .args(["--strict-mcp-config", "--tools", "Read", "--allowedTools", "Read", "--setting-sources", "project", "--disable-slash-commands"]);
        for d in &setup.read {
            c.arg("--add-dir").arg(d);
        }
        if !ask.keep {
            c.arg("--no-session-persistence");
        }
        if let Some(s) = &ask.resume {
            c.args(["--resume", s]);
        }
        let mut child = c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| format!("claude: {e}"))?;
        child.stdin.take().unwrap().write_all(prompt.as_bytes()).map_err(|e| e.to_string())?;
        let out = child.wait_with_output().map_err(|e| e.to_string())?;
        let v: Value = match serde_json::from_slice(&out.stdout) {
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
        if v["is_error"].as_bool() == Some(true) {
            return Err(format!("{}: {}", ask.id, v["result"].as_str().unwrap_or("an error")));
        }
        let u = &v["usage"];
        let usage = Usage {
            input: u["input_tokens"].as_u64().unwrap_or(0),
            output: u["output_tokens"].as_u64().unwrap_or(0),
            cache_write: u["cache_creation_input_tokens"].as_u64().unwrap_or(0),
            cache_read: u["cache_read_input_tokens"].as_u64().unwrap_or(0),
            cost_usd: v["total_cost_usd"].as_f64().unwrap_or(0.0),
        };
        let text = v["result"].as_str().unwrap_or("").to_string();
        match json_in(&text) {
            Some(json) => return Ok(Answer { id: ask.id.clone(), json, text, session: v["session_id"].as_str().unwrap_or("").to_string(), usage, seconds: began.elapsed().as_secs_f64(), attempts, waited }),
            None if attempts < 3 => {
                prompt = format!("{}\n\nReply with the JSON object only.", ask.prompt);
            }
            None => return Err(format!("{}: no JSON in the answer: {}", ask.id, text.chars().take(200).collect::<String>())),
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
