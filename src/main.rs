use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::env;
use std::fs;
use std::io::{self, Write};
use std::process::Command;

const SELF_PATH: &str = "src/main.rs";
const HISTORY_PATH: &str = ".evo/history.json";
const MAX_ITERS: usize = 10;
const PATIENCE: usize = 3;

// ── Types ─────────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone)]
struct Msg {
    role: String,
    content: Value,
}

#[derive(Serialize, Deserialize)]
struct Iteration {
    n: usize,
    score: f64,
    summary: String,
}

// ── Shell / Build ─────────────────────────────────────────────────────────────

fn shell(cmd: &str) -> (i32, String) {
    let out = Command::new("sh").args(["-c", cmd]).output().expect("sh failed");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.code().unwrap_or(-1), text.trim().to_string())
}

fn build() -> Result<(), String> {
    let (code, out) = shell("cargo build --release 2>&1");
    if code == 0 { Ok(()) } else { Err(out) }
}

fn eval_self() -> f64 {
    let src = fs::read_to_string(SELF_PATH).unwrap_or_default();
    let lines = src.lines().count() as f64;
    if lines == 0.0 { return 0.0; }
    match build() {
        Ok(_) => 1000.0 / lines + 1.0,
        Err(_) => 1000.0 / lines,
    }
}

// ── LLM ───────────────────────────────────────────────────────────────────────

struct Cfg {
    api_key: String,
    base_url: String,
    model: String,
}

impl Cfg {
    fn from_env() -> Self {
        let api_key = env::var("OPENROUTER_API_KEY")
            .unwrap_or_else(|_| {
                eprintln!("Set OPENROUTER_API_KEY");
                std::process::exit(1);
            });
        Self {
            api_key,
            base_url: env::var("INFERENCE_BASE_URL")
                .unwrap_or_else(|_| "https://openrouter.ai/api/v1".to_string()),
            model: env::var("MODEL_NAME")
                .unwrap_or_else(|_| "anthropic/claude-opus-4".to_string()),
        }
    }
}

fn llm(cfg: &Cfg, messages: &[Msg], system: &str) -> Result<String, String> {
    let mut msgs = vec![Msg { role: "system".to_string(), content: json!(system) }];
    msgs.extend_from_slice(messages);
    let body = json!({ "model": cfg.model, "max_tokens": 4096, "messages": msgs });
    let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
    let resp = ureq::post(&url)
        .set("Authorization", &format!("Bearer {}", cfg.api_key))
        .set("Content-Type", "application/json")
        .send_json(body)
        .map_err(|e| e.to_string())?;
    let v: Value = resp.into_json().map_err(|e| e.to_string())?;
    v["choices"][0]["message"]["content"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| format!("bad response: {v}"))
}

// ── Tool dispatch ─────────────────────────────────────────────────────────────

fn run_tool(text: &str) -> Option<(String, String)> {
    // Parse: <tool name="...">content</tool>
    let start = text.find("<tool ")?;
    let end = text[start..].find("</tool>")? + start;
    let tag = &text[start..end + 7];

    let name_start = tag.find("name=\"")? + 6;
    let name_end = tag[name_start..].find('"')? + name_start;
    let name = &tag[name_start..name_end];

    let content_start = tag.find('>')? + 1;
    let content_end = tag.rfind("</tool>").unwrap_or(tag.len());
    let content = tag[content_start..content_end].trim();

    match name {
        "shell" => {
            let (code, out) = shell(content);
            Some(("shell".to_string(), format!("exit={code}\n{out}")))
        }
        "write_self" => {
            // Strip markdown fences if the LLM wrapped the code
            let code = {
                let s = content.trim();
                let s = s.strip_prefix("```rust").or_else(|| s.strip_prefix("```")).unwrap_or(s);
                let s = s.strip_suffix("```").unwrap_or(s);
                s.trim()
            };
            if code.is_empty() { return Some(("write_self".to_string(), "REJECTED (empty content)".to_string())); }
            // Safety: backup → write → build-verify → restore on failure
            let backup = fs::read_to_string(SELF_PATH).unwrap_or_default();
            fs::write(SELF_PATH, code).ok()?;
            match build() {
                Ok(_) => {
                    // Keep a .bak of the last known-good version
                    let bak = format!("{SELF_PATH}.bak");
                    fs::write(&bak, &backup).ok();
                    Some(("write_self".to_string(), "written and verified OK".to_string()))
                }
                Err(e) => {
                    // Restore previous file — never leave a broken binary
                    fs::write(SELF_PATH, &backup).ok();
                    let msg = format!(
                        "REJECTED (build failed, reverted):\n{}",
                        e.chars().take(400).collect::<String>()
                    );
                    Some(("write_self".to_string(), msg))
                }
            }
        }
        _ => None,
    }
}

// ── History ───────────────────────────────────────────────────────────────────

fn load_history() -> Vec<Iteration> {
    fs::read_to_string(HISTORY_PATH)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_history(h: &[Iteration]) {
    fs::create_dir_all(".evo").ok();
    fs::write(HISTORY_PATH, serde_json::to_string_pretty(h).unwrap()).ok();
}

// ── Agent loop ────────────────────────────────────────────────────────────────

// System prompt stored as a const using concat! to avoid raw-string delimiter
// collisions when the LLM rewrites this file.
const SYSTEM: &str = concat!(
    "You are a self-evolving Rust coding agent.\n",
    "Goal: make src/main.rs shorter, more correct, more capable — keep it compiling.\n\n",
    "Tools (emit exactly one per turn):\n",
    "  <tool name=\"shell\">cargo test 2>&1</tool>\n",
    "  <tool name=\"write_self\">...FULL new src/main.rs content...</tool>\n\n",
    "Rules:\n",
    "- write_self auto-verifies the build; on failure the old file is restored and you get the error.\n",
    "- Always emit the COMPLETE file in write_self — never truncate.\n",
    "- In write_self, emit RAW Rust source only — NO ```rust fences, NO markdown.\n",
    "- After each action emit a SUMMARY: line.\n",
    "- Prefer write_self over shell when you have a ready improvement."
);

fn agent_loop(cfg: &Cfg) {
    let mut history = load_history();
    let n_done = history.len();
    println!("[evo] starting at iteration {} / {MAX_ITERS}", n_done + 1);
    let mut no_improve = 0usize;

    'outer: for iter in (n_done + 1)..=(n_done + MAX_ITERS) {
        let src = fs::read_to_string(SELF_PATH).unwrap_or_default();
        let score_before = eval_self();

        let prompt = format!(
            "Iteration {iter}. Score: {score_before:.4}. History: {} done.\n\
             Best score so far: {:.4}\n\n\
             Current src/main.rs:\n```rust\n{src}\n```\n\nPropose one improvement.",
            history.len(),
            history.iter().map(|h| h.score).fold(0.0_f64, f64::max),
        );

        let mut messages = vec![Msg { role: "user".to_string(), content: json!(prompt) }];
        let mut turns = 0usize;

        loop {
            turns += 1;
            if turns > 8 { break; }

            let reply = match llm(cfg, &messages, SYSTEM) {
                Ok(r) => r,
                Err(e) => { eprintln!("[llm] {e}"); break; }
            };

            print!("[iter {iter}] {}", reply.chars().take(200).collect::<String>());
            io::stdout().flush().ok();

            messages.push(Msg { role: "assistant".to_string(), content: json!(&reply) });

            if let Some((tool, result)) = run_tool(&reply) {
                println!("\n  -> {tool}: {}", result.chars().take(200).collect::<String>());
                messages.push(Msg {
                    role: "user".to_string(),
                    content: json!(format!("<tool_result>{result}</tool_result>")),
                });
                // Stop after a successful write
                if tool == "write_self" && result.contains("verified OK") { break; }
            } else {
                break;
            }
        }

        let score_after = eval_self();
        let summary = format!("iter={iter} score {score_before:.3}->{score_after:.3}");
        println!("\n[evo] {summary}");
        history.push(Iteration { n: iter, score: score_after, summary });
        save_history(&history);

        if score_after < score_before {
            fs::copy(format!("{SELF_PATH}.bak"), SELF_PATH).ok();
            println!("[evo] score dropped — auto-reverted");
        }
        if score_after <= score_before { no_improve += 1; } else { no_improve = 0; }
        if no_improve >= PATIENCE { println!("[evo] patience exhausted"); break 'outer; }
    }
    println!("[evo] done. {} total iterations.", history.len());
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn load_env() {
    if let Ok(s) = fs::read_to_string(".env") {
        for line in s.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') { continue; }
            if let Some((k, v)) = line.split_once('=') {
                env::set_var(k.trim(), v.trim().trim_matches('"').trim_matches('\''));
            }
        }
    }
}

fn main() {
    load_env();
    let cfg = Cfg::from_env();
    match env::args().nth(1).as_deref() {
        Some("eval") => println!("score={:.4}", eval_self()),
        _ => agent_loop(&cfg),
    }
}
