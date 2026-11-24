//! The Box: the shared workspace of one run, which every agent reads and
//! writes. One folder (`runs/<run>/box`), its writes safe across processes:
//!
//! - `plan.json`: the director's plan, written once: the architecture and
//!   its reasons, the conventions, the decisions, every file's spec;
//! - `registry/`: who built what. One file for each item (a helper, a type,
//!   a function), named by the item; it is made with create-new, so the first
//!   claim wins and a second is told who has it. The director registers every
//!   shared type and signature before anyone writes code;
//! - `board.jsonl`: thoughts and blockers, one line each, that every agent
//!   can read;
//! - `notes.jsonl`: decisions and notes made along the way.
//!
//! When the run ends the Box is kept in Nimbus too.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Clone, Serialize, Deserialize)]
pub struct Item {
    pub name: String,
    /// fn, struct, enum, trait, const, type
    pub kind: String,
    pub signature: String,
    /// the file it is built in
    pub file: String,
    /// who claimed it (the director, or a minion)
    pub owner: String,
}

pub struct Shared {
    pub dir: PathBuf,
}

fn file_name(name: &str) -> String {
    name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '-' }).collect::<String>() + ".json"
}

impl Shared {
    pub fn open(dir: &Path) -> Result<Shared, String> {
        std::fs::create_dir_all(dir.join("registry")).map_err(|e| e.to_string())?;
        Ok(Shared { dir: dir.to_path_buf() })
    }

    /// Claims an item: Ok(None) if it is now this owner's, Ok(Some(item)) if it was claimed before.
    pub fn claim(&self, item: &Item) -> Result<Option<Item>, String> {
        let path = self.dir.join("registry").join(file_name(&item.name));
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => {
                f.write_all(serde_json::to_string(item).unwrap().as_bytes()).map_err(|e| e.to_string())?;
                Ok(None)
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(self.lookup(&item.name)),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    pub fn lookup(&self, name: &str) -> Option<Item> {
        // a claim being written is read again a moment later
        for _ in 0..20 {
            match std::fs::read_to_string(self.dir.join("registry").join(file_name(name))) {
                Ok(t) => match serde_json::from_str(&t) {
                    Ok(i) => return Some(i),
                    Err(_) => std::thread::sleep(std::time::Duration::from_millis(10)),
                },
                Err(_) => return None,
            }
        }
        None
    }

    pub fn items(&self) -> Vec<Item> {
        let mut v: Vec<Item> = std::fs::read_dir(self.dir.join("registry")).into_iter().flatten().flatten().filter_map(|e| serde_json::from_str(&std::fs::read_to_string(e.path()).ok()?).ok()).collect();
        v.sort_by(|a, b| a.file.cmp(&b.file).then(a.name.cmp(&b.name)));
        v
    }

    /// Appends one line to a shared log (the board, the notes), under a lock file.
    fn append(&self, log: &str, line: &serde_json::Value) -> Result<(), String> {
        let lock = self.dir.join(format!("{log}.lock"));
        let mut tries = 0;
        while std::fs::OpenOptions::new().write(true).create_new(true).open(&lock).is_err() {
            tries += 1;
            if tries > 500 {
                let _ = std::fs::remove_file(&lock);
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let r = std::fs::OpenOptions::new().create(true).append(true).open(self.dir.join(format!("{log}.jsonl"))).and_then(|mut f| writeln!(f, "{line}"));
        let _ = std::fs::remove_file(&lock);
        r.map_err(|e| e.to_string())
    }

    pub fn post(&self, from: &str, text: &str) -> Result<(), String> {
        self.append("board", &serde_json::json!({"from": from, "text": text}))
    }

    pub fn note(&self, from: &str, text: &str) -> Result<(), String> {
        self.append("notes", &serde_json::json!({"from": from, "text": text}))
    }

    pub fn board(&self) -> Vec<(String, String)> {
        std::fs::read_to_string(self.dir.join("board.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .map(|v| (v["from"].as_str().unwrap_or("").to_string(), v["text"].as_str().unwrap_or("").to_string()))
            .collect()
    }
}

/// The Box as an MCP server for one agent (`bottlebox box-mcp --box DIR --as ID`):
/// registry_lookup, registry_claim, board_post, board_read.
pub fn serve(dir: &Path, me: &str) -> Result<(), String> {
    use std::io::BufRead;
    let shared = Shared::open(dir)?;
    let tools = serde_json::json!([
        {"name": "registry_lookup", "description": "Is this helper, type or function already built (or claimed) by someone? Gives its file, signature and owner.", "inputSchema": {"type": "object", "properties": {"name": {"type": "string", "description": "module::item"}}, "required": ["name"]}},
        {"name": "registry_claim", "description": "Claim a new helper, type or function before you build it. If someone has it already you are told whose it is: use theirs instead.", "inputSchema": {"type": "object", "properties": {"name": {"type": "string", "description": "module::item"}, "kind": {"type": "string"}, "signature": {"type": "string"}, "file": {"type": "string"}}, "required": ["name", "kind", "signature", "file"]}},
        {"name": "board_post", "description": "Post a short thought or blocker to the shared board (one line).", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}},
        {"name": "board_read", "description": "Read the shared board.", "inputSchema": {"type": "object", "properties": {}}}
    ]);
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line.map_err(|e| e.to_string())?;
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(line.trim_start_matches('\u{feff}')) else { continue };
        let Some(id) = msg.get("id").cloned() else { continue };
        let a = &msg["params"]["arguments"];
        let s = |k: &str| a[k].as_str().unwrap_or("").to_string();
        let result = match msg["method"].as_str().unwrap_or("") {
            "initialize" => serde_json::json!({"protocolVersion": msg["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18"), "capabilities": {"tools": {}}, "serverInfo": {"name": "box", "version": "0.1"}}),
            "tools/list" => serde_json::json!({"tools": tools}),
            "tools/call" => {
                let text = match msg["params"]["name"].as_str().unwrap_or("") {
                    "registry_lookup" => shared.lookup(&s("name")).map_or("not built and not claimed".to_string(), |i| serde_json::to_string(&i).unwrap()),
                    "registry_claim" => match shared.claim(&Item { name: s("name"), kind: s("kind"), signature: s("signature"), file: s("file"), owner: me.to_string() }) {
                        Ok(None) => "claimed: it is yours to build".to_string(),
                        Ok(Some(i)) => format!("already claimed, use it: {}", serde_json::to_string(&i).unwrap()),
                        Err(e) => format!("error: {e}"),
                    },
                    "board_post" => shared.post(me, &s("text")).map_or_else(|e| e, |_| "posted".into()),
                    "board_read" => shared.board().iter().map(|(f, t)| format!("{f}: {t}")).collect::<Vec<_>>().join("\n"),
                    other => format!("no tool {other}"),
                };
                serde_json::json!({"content": [{"type": "text", "text": text}]})
            }
            "ping" => serde_json::json!({}),
            _ => serde_json::json!({}),
        };
        writeln!(out, "{}", serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result})).map_err(|e| e.to_string())?;
        out.flush().map_err(|e| e.to_string())?;
    }
    Ok(())
}
