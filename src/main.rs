use serde::Serialize;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::env;
use std::fs;
use std::io::{self, BufRead, Write};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SELF_PATH: &str = "src/main.rs";
const MAX_ITERS: usize = 10;
const PATIENCE: usize = 3;
const WATERMARK_PATH: &str = ".evo/learned_until.txt";

#[derive(Serialize, Clone)]
struct Msg {
    role: String,
    content: Value,
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn traj_log(path: &str, kind: &str, data: Value) {
    let line = format!("{}\n", json!({"ts": now_secs(), "kind": kind, "data": data}));
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(path) {
        f.write_all(line.as_bytes()).ok();
    }
}

struct Cfg {
    api_key: String,
    base_url: String,
    model: String,
}

impl Cfg {
    fn from_env() -> Self {
        let api_key = env::var("OPENROUTER_API_KEY").unwrap_or_else(|_| {
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
    let body = json!({"model": cfg.model, "max_tokens": 4096, "messages": msgs});
    let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
    let resp = ureq::post(&url)
        .timeout(Duration::from_secs(120))
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

fn is_new_task(cfg: &Cfg, history: &[Msg], next_input: &str) -> bool {
    let system = "You decide if a new user message starts a NEW task or continues the current one. Reply with exactly one word: NEW or CONTINUE.";
    let window: Vec<Msg> = history.iter().rev().take(6).cloned().collect::<Vec<_>>()
        .into_iter().rev().collect();
    let mut msgs = window;
    msgs.push(Msg { role: "user".to_string(), content: json!(next_input) });
    match llm(cfg, &msgs, system) {
        Ok(r) => r.trim().to_uppercase().starts_with("NEW"),
        Err(_) => false,
    }
}

fn extract_tool(text: &str) -> Option<(&str, &str)> {
    let open = text.find("<tool name=\"")?;
    let name_start = open + 12;
    let name_end = text[name_start..].find('"')? + name_start;
    let body_start = text[name_end..].find('>')? + name_end + 1;
    let body_end = text[body_start..].find("</tool>")? + body_start;
    // strip optional leading/trailing ```rust or ``` fences from body
    let raw = text[body_start..body_end].trim();
    let body = if raw.starts_with("```") {
        let after = raw.find('\n').map(|i| &raw[i+1..]).unwrap_or(raw);
        after.trim_end_matches("```").trim()
    } else {
        raw
    };
    Some((&text[name_start..name_end], body))
}

fn run_tool(text: &str, traj: &str, evolve_mode: bool) -> Option<String> {
    let (name, body) = extract_tool(text)?;
    match name {
        "shell" => {
            let out = Command::new("sh")
                .args(["-c", body])
                .output()
                .map(|o| format!("exit={}\n{}{}", o.status.code().unwrap_or(-1),
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr)))
                .unwrap_or_else(|e| format!("error: {e}"));
            let out = out.chars().take(2000).collect::<String>();
            traj_log(traj, "tool_result", json!({"tool": "shell", "result": &out}));
            Some(format!("<tool_result>{out}</tool_result>"))
        }
        "write_self" => {
            if body.is_empty() {
                return Some("<tool_result>REJECTED (empty content)</tool_result>".to_string());
            }
            let bak = if evolve_mode {
                let bak_path = format!("src/main.{}.rs.bak", now_secs());
                fs::copy(SELF_PATH, &bak_path).ok();
                Some(bak_path)
            } else {
                None
            };
            fs::write(SELF_PATH, body).ok();
            let build_out = Command::new("cargo")
                .args(["build", "--release"])
                .output()
                .map(|o| format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)))
                .unwrap_or_else(|e| e.to_string());
            if build_out.contains("error") {
                let err_snippet = build_out.chars().take(400).collect::<String>();
                if let Some(bak_path) = &bak {
                    fs::copy(bak_path, SELF_PATH).ok();
                }
                let result = format!("REJECTED (build failed, reverted):\n{err_snippet}");
                traj_log(traj, "tool_result", json!({"tool": "write_self", "result": &result}));
                Some(format!("<tool_result>{result}</tool_result>"))
            } else {
                traj_log(traj, "tool_result", json!({"tool": "write_self", "result": "written and verified OK"}));
                Some("<tool_result>written and verified OK</tool_result>".to_string())
            }
        }
        "write_file" => {
            let mut lines = body.splitn(2, '\n');
            let path = lines.next()?.trim();
            let content = lines.next().unwrap_or("");
            if let Some(parent) = std::path::Path::new(path).parent() {
                fs::create_dir_all(parent).ok();
            }
            fs::write(path, content).ok();
            traj_log(traj, "tool_result", json!({"tool": "write_file", "path": path}));
            Some(format!("<tool_result>written {path}</tool_result>"))
        }
        _ => None,
    }
}

fn chat_mode(cfg: &Cfg, session_ts: &str, traj: &str) {
    let traj_dir = format!(".evo/sessions/{session_ts}");
    fs::create_dir_all(&traj_dir).ok();
    traj_log(traj, "session_start", json!({}));

    let queue: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));
    let q2 = queue.clone();
    std::thread::spawn(move || {
        let stdin = io::stdin();
        for line in stdin.lock().lines() {
            match line {
                Ok(l) => { q2.lock().unwrap().push_back(l); }
                Err(_) => break,
            }
        }
    });

    let system = concat!(
        "You are a helpful coding assistant. When you need to run a command use:\n",
        "<tool name=\"shell\">command</tool>\n",
        "When you need to write a file use:\n",
        "<tool name=\"write_file\">path/to/file\ncontent</tool>\n",
        "Only one tool per reply."
    );

    let mut messages: Vec<Msg> = vec![];
    let mut task_n = 1usize;
    let mut out_dir = format!("outputs/{session_ts}/task_{task_n}");
    fs::create_dir_all(&out_dir).ok();

    eprintln!("Chat mode. Ctrl+C or Ctrl+D to quit.");

    loop {
        let input = loop {
            let mut q = queue.lock().unwrap();
            if let Some(line) = q.pop_front() {
                break line;
            }
            drop(q);
            std::thread::sleep(Duration::from_millis(50));
        };

        if input.trim().is_empty() { continue; }

        traj_log(traj, "user_input", json!(input));

        if is_new_task(cfg, &messages, &input) && !messages.is_empty() {
            task_n += 1;
            out_dir = format!("outputs/{session_ts}/task_{task_n}");
            fs::create_dir_all(&out_dir).ok();
            traj_log(traj, "task_boundary", json!({"task": task_n}));
        }

        messages.push(Msg { role: "user".to_string(), content: json!(input) });
        if messages.len() > 20 { messages.drain(..messages.len() - 20); }

        for turn in 1..=8 {
            let reply = match llm(cfg, &messages, system) {
                Ok(r) => r,
                Err(e) => { eprintln!("LLM error: {e}"); break; }
            };
            traj_log(traj, "llm_response", json!({"task": task_n, "turn": turn, "preview": &reply.chars().take(200).collect::<String>()}));
            println!("{reply}");

            messages.push(Msg { role: "assistant".to_string(), content: json!(&reply) });
            if messages.len() > 20 { messages.drain(..messages.len() - 20); }

            if let Some(tool_result) = run_tool(&reply, traj, false) {
                messages.push(Msg { role: "user".to_string(), content: json!(tool_result) });
            } else {
                break;
            }
        }
    }
}

fn reflect(cfg: &Cfg, traj: &str) {
    let watermark: u64 = fs::read_to_string(WATERMARK_PATH)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);

    let sessions: Vec<_> = if let Ok(entries) = fs::read_dir(".evo/sessions") {
        let mut v: Vec<_> = entries.flatten()
            .filter(|e| {
                e.path().is_dir() &&
                e.file_name().to_string_lossy().parse::<u64>().map(|ts| ts > watermark).unwrap_or(false)
            })
            .collect();
        v.sort_by_key(|e| e.file_name());
        v
    } else {
        vec![]
    };

    if sessions.is_empty() {
        eprintln!("No new sessions to reflect on.");
        fs::create_dir_all(".evo").ok();
        fs::write(WATERMARK_PATH, now_secs().to_string()).ok();
        return;
    }

    let system = concat!(
        "You are analyzing trajectory logs from an AI coding agent. ",
        "Identify ONE concrete, actionable improvement to the agent's src/main.rs. ",
        "Be specific. One sentence."
    );

    for entry in &sessions {
        let session_ts: u64 = entry.file_name().to_string_lossy().parse().unwrap_or(0);
        let traj_path = entry.path().join("traj.jsonl");
        let lines: Vec<String> = fs::read_to_string(&traj_path)
            .unwrap_or_default()
            .lines()
            .map(|l| l.to_string())
            .collect();

        // Progressive disclosure: strip large fields, cap strings, limit to 8000 chars
        let sample: String = {
            let stripped: Vec<String> = lines.iter().rev()
                .filter_map(|l| {
                    let mut v: Value = serde_json::from_str(l).ok()?;
                    if let Some(obj) = v.get_mut("data") {
                        if let Some(map) = obj.as_object_mut() {
                            map.remove("content");
                            map.remove("preview");
                        } else if obj.is_string() && obj.as_str().map(|s| s.len()).unwrap_or(0) > 120 {
                            *obj = json!(obj.as_str().unwrap_or("").chars().take(120).collect::<String>());
                        }
                    }
                    Some(v.to_string())
                })
                .collect::<Vec<_>>()
                .into_iter().rev().collect();
            let joined = stripped.join("\n");
            if joined.len() > 8000 { joined[joined.len() - 8000..].to_string() } else { joined }
        };

        let msgs = vec![Msg {
            role: "user".to_string(),
            content: json!(format!("Session {session_ts} events:\n{sample}\n\nWhat is the single most important improvement?")),
        }];
        match llm(cfg, &msgs, system) {
            Ok(suggestion) => {
                eprintln!("Reflection [{session_ts}]: {suggestion}");
                traj_log(traj, "reflect_result", json!(suggestion));
            }
            Err(e) => eprintln!("Reflection LLM error [{session_ts}]: {e}"),
        }

        fs::create_dir_all(".evo").ok();
        fs::write(WATERMARK_PATH, session_ts.to_string()).ok();
    }
}

fn evolve_mode(cfg: &Cfg, traj: &str) {
    reflect(cfg, traj);

    let evolve_system = concat!(
        "You are improving a Rust self-evolving agent. You will see the full src/main.rs.\n",
        "Rules:\n",
        "- Propose ONE small, clearly beneficial change.\n",
        "- Only rewrite when the improvement is unambiguously worth the complexity cost.\n",
        "- Prefer deletion and simplification over addition.\n",
        "- If nothing is worth changing, reply with exactly: SKIP\n",
        "Tools available (one per reply):\n",
        "<tool name=\"write_self\">...full new src/main.rs...</tool>\n",
        "<tool name=\"shell\">command</tool>\n",
        "After write_self succeeds, you may verify with shell, then stop."
    );

    traj_log(traj, "evolve_start", json!({}));
    let mut no_improve_streak = 0usize;

    'outer: for iter in 1..=MAX_ITERS {
        traj_log(traj, "iter_start", json!({"iter": iter}));
        let src = fs::read_to_string(SELF_PATH).unwrap_or_default();
        let mut messages: Vec<Msg> = vec![Msg {
            role: "user".to_string(),
            content: json!(format!("Current src/main.rs:\n```rust\n{src}\n```\n\nPropose one improvement or reply SKIP.")),
        }];

        let mut improved = false;
        for turn in 1..=8 {
            let reply = match llm(cfg, &messages, evolve_system) {
                Ok(r) => r,
                Err(e) => { eprintln!("LLM error: {e}"); break; }
            };
            traj_log(traj, "llm_response", json!({"turn": turn, "preview": &reply.chars().take(200).collect::<String>()}));

            if reply.trim().to_uppercase().starts_with("SKIP") {
                traj_log(traj, "iter_skip", json!({"iter": iter, "reason": "LLM chose not to evolve"}));
                break 'outer;
            }

            messages.push(Msg { role: "assistant".to_string(), content: json!(&reply) });

            if let Some(tool_result) = run_tool(&reply, traj, true) {
                let ok = tool_result.contains("verified OK");
                if ok { improved = true; }
                messages.push(Msg { role: "user".to_string(), content: json!(tool_result) });
                if ok { break; }
            } else {
                break;
            }
        }

        traj_log(traj, "iter_end", json!({"iter": iter, "improved": improved}));
        if improved {
            no_improve_streak = 0;
        } else {
            no_improve_streak += 1;
            if no_improve_streak >= PATIENCE {
                eprintln!("Patience exhausted ({PATIENCE} consecutive non-improving iters). Stopping.");
                break;
            }
        }
    }

    traj_log(traj, "evolve_end", json!({}));

    // Doc update step
    let src = fs::read_to_string(SELF_PATH).unwrap_or_default();
    let claude_md = fs::read_to_string("CLAUDE.md").unwrap_or_default();
    let readme = fs::read_to_string("README.md").unwrap_or_default();
    let doc_system = concat!(
        "You are updating documentation to match the current implementation.\n",
        "Use write_file tools to update CLAUDE.md and README.md.\n",
        "<tool name=\"write_file\">path\ncontent</tool>\n",
        "Reflect the actual current code. Be concise."
    );
    let doc_prompt = format!(
        "Current src/main.rs:\n```rust\n{src}\n```\n\nCurrent CLAUDE.md:\n{claude_md}\n\nCurrent README.md:\n{readme}\n\nUpdate both docs to match the implementation."
    );
    let mut doc_msgs = vec![Msg { role: "user".to_string(), content: json!(doc_prompt) }];
    for _ in 0..4 {
        match llm(cfg, &doc_msgs, doc_system) {
            Ok(reply) => {
                doc_msgs.push(Msg { role: "assistant".to_string(), content: json!(&reply) });
                if let Some(result) = run_tool(&reply, traj, false) {
                    doc_msgs.push(Msg { role: "user".to_string(), content: json!(result) });
                } else {
                    break;
                }
            }
            Err(e) => { eprintln!("Doc update LLM error: {e}"); break; }
        }
    }
}

fn main() {
    let ts = now_secs().to_string();
    let traj_dir = format!(".evo/sessions/{ts}");
    fs::create_dir_all(&traj_dir).ok();
    let traj = format!("{traj_dir}/traj.jsonl");
    // load .env
    if let Ok(content) = fs::read_to_string(".env") {
        for line in content.lines() {
            if let Some((k, v)) = line.split_once('=') {
                let k = k.trim();
                let v = v.trim();
                if !k.starts_with('#') && env::var(k).is_err() {
                    env::set_var(k, v);
                }
            }
        }
    }

    let cfg = Cfg::from_env();

    match env::args().nth(1).as_deref() {
        Some("evolve") => evolve_mode(&cfg, &traj),
        None           => chat_mode(&cfg, &ts, &traj),
        Some(cmd)      => { eprintln!("unknown subcommand: {cmd}"); std::process::exit(1); }
    }
}
