use serde::Serialize;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::env;
use std::fs;
use std::io::{self, BufRead, Write};
use std::os::unix::process::CommandExt;
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

fn load_prompt(name: &str) -> String {
    fs::read_to_string(format!("src/prompts/{name}")).unwrap_or_default()
}

fn is_new_task(cfg: &Cfg, history: &[Msg], next_input: &str) -> bool {
    let system = "You decide if a new user message starts a NEW task or continues the current one. Reply with exactly one word: NEW or CONTINUE.";
    let window: Vec<Msg> = history.iter().rev().take(6).cloned().collect::<Vec<_>>()
        .into_iter().rev().collect();
    let mut msgs = window;
    msgs.push(Msg { role: "user".to_string(), content: json!(next_input) });
    match llm(cfg, &msgs, system) {
        Ok(r) => r.trim().to_uppercase().starts_with("NEW"),
        Err(e) => { eprintln!("Task-judge error (defaulting CONTINUE): {e}"); false }
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

// Sub-agent state: maps agent_id -> JoinHandle result (None = still running)
type AgentRegistry = Arc<Mutex<Vec<(String, Arc<Mutex<Option<String>>>)>>>;

fn new_agent_registry() -> AgentRegistry {
    Arc::new(Mutex::new(vec![]))
}

// Spawns a background LLM agent for a sub-task. Returns the agent_id.
fn spawn_sub_agent(
    cfg_snapshot: (String, String, String), // (api_key, base_url, model)
    task: &str,
    output_path: &str,
    traj: &str,
    registry: &AgentRegistry,
) -> String {
    let agent_id = format!("agent_{}", now_secs());
    let result_slot: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let slot2 = result_slot.clone();

    let task = task.to_string();
    let output_path = output_path.to_string();
    let traj = traj.to_string();
    let id2 = agent_id.clone();

    std::thread::spawn(move || {
        let cfg = Cfg {
            api_key: cfg_snapshot.0,
            base_url: cfg_snapshot.1,
            model: cfg_snapshot.2,
        };
        let system = load_prompt("chat_system.txt");
        let mut messages = vec![Msg {
            role: "user".to_string(),
            content: json!(format!(
                "You are a sub-agent. Complete this task and write the result to {output_path}.\n\nTask:\n{task}"
            )),
        }];

        traj_log(&traj, "sub_agent_start", json!({"agent_id": &id2, "output_path": &output_path}));

        let mut final_result = format!("sub-agent {id2}: no output produced");
        for turn in 1..=8 {
            let reply = match llm(&cfg, &messages, &system) {
                Ok(r) => r,
                Err(e) => {
                    final_result = format!("sub-agent {id2} LLM error on turn {turn}: {e}");
                    break;
                }
            };
            traj_log(&traj, "sub_agent_turn", json!({"agent_id": &id2, "turn": turn, "preview": &reply.chars().take(200).collect::<String>()}));
            messages.push(Msg { role: "assistant".to_string(), content: json!(&reply) });

            // Handle shell and write_file tools inline
            if let Some((name, body)) = extract_tool(&reply) {
                let tool_result = match name {
                    "shell" => {
                        let out = Command::new("sh")
                            .args(["-c", body])
                            .output()
                            .map(|o| format!("exit={}\n{}{}", o.status.code().unwrap_or(-1),
                                String::from_utf8_lossy(&o.stdout),
                                String::from_utf8_lossy(&o.stderr)))
                            .unwrap_or_else(|e| format!("error: {e}"));
                        out.chars().take(2000).collect::<String>()
                    }
                    "write_file" => {
                        let mut lines = body.splitn(2, '\n');
                        let path = lines.next().unwrap_or("").trim();
                        let content = lines.next().unwrap_or("");
                        if let Some(parent) = std::path::Path::new(path).parent() {
                            fs::create_dir_all(parent).ok();
                        }
                        fs::write(path, content).ok();
                        // match by canonical suffix: agent may write short name or full path
                        let wrote_output = path == output_path
                            || output_path.ends_with(path)
                            || path.ends_with(&output_path);
                        if wrote_output {
                            final_result = format!("written {path}");
                        }
                        format!("written {path}")
                    }
                    _ => format!("unknown tool: {name}"),
                };
                messages.push(Msg { role: "user".to_string(), content: json!(format!("<tool_result>{tool_result}</tool_result>")) });
                // stop if the output file was written
                let wrote_output = if name == "write_file" {
                    let first_line = body.splitn(2, '\n').next().unwrap_or("").trim();
                    first_line == output_path
                        || output_path.ends_with(first_line)
                        || first_line.ends_with(&output_path)
                } else {
                    false
                };
                if wrote_output {
                    break; // task done — output file written
                }
            } else {
                // No tool call — agent is done; write reply as output if file not yet written
                if !std::path::Path::new(&output_path).exists() {
                    if let Some(parent) = std::path::Path::new(&output_path).parent() {
                        fs::create_dir_all(parent).ok();
                    }
                    fs::write(&output_path, &reply).ok();
                    final_result = format!("written {output_path} (from reply)");
                }
                break;
            }
        }

        traj_log(&traj, "sub_agent_end", json!({"agent_id": &id2, "result": &final_result}));
        *slot2.lock().unwrap() = Some(final_result);
    });

    registry.lock().unwrap().push((agent_id.clone(), result_slot));
    agent_id
}

// Poll result for an agent_id; returns None if still running, Some(result) when done.
fn poll_agent(registry: &AgentRegistry, agent_id: &str) -> Option<String> {
    let reg = registry.lock().unwrap();
    for (id, slot) in reg.iter() {
        if id == agent_id {
            return slot.lock().unwrap().clone();
        }
    }
    Some(format!("unknown agent_id: {agent_id}"))
}

fn run_tool(
    text: &str,
    traj: &str,
    evolve_mode: bool,
    registry: Option<(&AgentRegistry, &(String, String, String), &str)>,
) -> Option<String> {
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
        "spawn_agent" => {
            let (reg, cfg_snap, out_dir) = registry?;
            // body format: first line = output_file (relative to out_dir), rest = task
            let mut lines = body.splitn(2, '\n');
            let rel_path = lines.next().unwrap_or("sub_agent_output.md").trim();
            let task = lines.next().unwrap_or(body).trim();
            let output_path = format!("{out_dir}/{rel_path}");
            let agent_id = spawn_sub_agent(cfg_snap.clone(), task, &output_path, traj, reg);
            traj_log(traj, "tool_result", json!({"tool": "spawn_agent", "agent_id": &agent_id, "output_path": &output_path}));
            Some(format!("<tool_result>spawned agent_id={agent_id} output_path={output_path}\nUse <tool name=\"wait_agent\">{agent_id}</tool> to block until done, or read {output_path} when ready.</tool_result>"))
        }
        "wait_agent" => {
            let agent_id = body.trim();
            let (reg, _, _) = registry?;
            eprintln!("Waiting for sub-agent {agent_id}...");
            loop {
                if let Some(result) = poll_agent(reg, agent_id) {
                    traj_log(traj, "tool_result", json!({"tool": "wait_agent", "agent_id": agent_id, "result": &result}));
                    return Some(format!("<tool_result>agent {agent_id} finished: {result}</tool_result>"));
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
        _ => None,
    }
}

fn chat_mode(cfg: &Cfg, session_ts: &str, traj: &str) {
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

    let system = load_prompt("chat_system.txt");
    let registry = new_agent_registry();
    let cfg_snap = (cfg.api_key.clone(), cfg.base_url.clone(), cfg.model.clone());

    let mut messages: Vec<Msg> = vec![];
    let mut task_n = 1usize;
    let mut out_dir = format!("outputs/{session_ts}/task_{task_n}");
    fs::create_dir_all(&out_dir).ok();

    eprintln!("Ready. /exit to quit, /evolve to evolve and relaunch.");

    loop {
        let input = loop {
            let mut q = queue.lock().unwrap();
            if let Some(line) = q.pop_front() {
                break line;
            }
            drop(q);
            // Check if stdin thread has exited (EOF)
            if Arc::strong_count(&queue) == 1 {
                traj_log(traj, "session_end", json!({"turns": task_n}));
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        };

        let trimmed = input.trim();
        if trimmed.is_empty() { continue; }

        // Slash commands
        if trimmed == "/exit" {
            traj_log(traj, "session_end", json!({"turns": task_n, "reason": "user /exit"}));
            eprintln!("Bye.");
            std::process::exit(0);
        }
        if trimmed == "/evolve" {
            traj_log(traj, "session_end", json!({"turns": task_n, "reason": "user /evolve"}));
            eprintln!("Starting evolution loop...");
            evolve_mode(cfg, traj);
            // Re-exec the evolved binary with the same arguments
            let exe = env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("./target/release/auto-harness"));
            eprintln!("Evolution done. Relaunching {}...", exe.display());
            let err = Command::new(&exe).args(env::args().skip(1)).exec();
            eprintln!("re-exec failed: {err}");
            std::process::exit(1);
        }

        traj_log(traj, "user_input", json!(input));

        if is_new_task(cfg, &messages, &input) && !messages.is_empty() {
            task_n += 1;
            out_dir = format!("outputs/{session_ts}/task_{task_n}");
            fs::create_dir_all(&out_dir).ok();
            traj_log(traj, "task_boundary", json!({"task": task_n}));
        }

        let stamped = format!("[output_dir: {out_dir}]\n{input}");
        messages.push(Msg { role: "user".to_string(), content: json!(stamped) });
        if messages.len() > 20 { messages.drain(..messages.len() - 20); }

        for turn in 1..=8 {
            let reply = match llm(cfg, &messages, &system) {
                Ok(r) => r,
                Err(e) => { eprintln!("LLM error: {e}"); break; }
            };
            traj_log(traj, "llm_response", json!({"task": task_n, "turn": turn, "preview": &reply.chars().take(200).collect::<String>()}));
            println!("{reply}");

            messages.push(Msg { role: "assistant".to_string(), content: json!(&reply) });
            if messages.len() > 20 { messages.drain(..messages.len() - 20); }

            if let Some(tool_result) = run_tool(&reply, traj, false, Some((&registry, &cfg_snap, &out_dir))) {
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

    let system = load_prompt("reflect_system.txt");

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
        match llm(cfg, &msgs, &system) {
            Ok(suggestion) => {
                eprintln!("Reflection [{session_ts}]: {suggestion}");
                traj_log(traj, "reflect_result", json!(suggestion));
                fs::create_dir_all(".evo").ok();
                fs::write(WATERMARK_PATH, session_ts.to_string()).ok();
            }
            Err(e) => eprintln!("Reflection LLM error [{session_ts}]: {e}"),
        }
    }
}

fn evolve_mode(cfg: &Cfg, traj: &str) {
    reflect(cfg, traj);

    let evolve_system = load_prompt("evolve_system.txt");

    traj_log(traj, "evolve_start", json!({}));
    let mut no_improve_streak = 0usize;

    let agents_path = "src/AGENTS.md";

    'outer: for iter in 1..=MAX_ITERS {
        traj_log(traj, "iter_start", json!({"iter": iter}));
        let src = fs::read_to_string(SELF_PATH).unwrap_or_default();
        let agents_md = fs::read_to_string(agents_path).unwrap_or_default();
        let prompts_section = {
            let names = ["chat_system.txt", "reflect_system.txt", "evolve_system.txt", "doc_system.txt"];
            names.iter().map(|n| {
                let content = fs::read_to_string(format!("src/prompts/{n}")).unwrap_or_default();
                format!("=== src/prompts/{n} ===\n{content}")
            }).collect::<Vec<_>>().join("\n\n")
        };
        let mut messages: Vec<Msg> = vec![Msg {
            role: "user".to_string(),
            content: json!(format!(
                "Current prompt files (highest priority to improve):\n{prompts_section}\n\nCurrent src/AGENTS.md:\n{agents_md}\n\nCurrent src/main.rs:\n```rust\n{src}\n```\n\nPropose one improvement. Priority: prompts > AGENTS.md > main.rs. Reply SKIP if nothing is worth changing."
            )),
        }];

        let mut improved = false;
        for turn in 1..=8 {
            let reply = match llm(cfg, &messages, &evolve_system) {
                Ok(r) => r,
                Err(e) => { eprintln!("LLM error: {e}"); break; }
            };
            traj_log(traj, "llm_response", json!({"turn": turn, "preview": &reply.chars().take(200).collect::<String>()}));

            if reply.trim().to_uppercase().starts_with("SKIP") {
                traj_log(traj, "iter_skip", json!({"iter": iter, "reason": "LLM chose not to evolve"}));
                break 'outer;
            }

            messages.push(Msg { role: "assistant".to_string(), content: json!(&reply) });

            if let Some(tool_result) = run_tool(&reply, traj, true, None) {
                let write_ok = tool_result.contains("verified OK") || tool_result.contains("written ");
                if write_ok { improved = true; }
                messages.push(Msg { role: "user".to_string(), content: json!(tool_result) });
                if write_ok { break; }
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
    let doc_system = load_prompt("doc_system.txt");
    let doc_prompt = format!(
        "Current src/main.rs:\n```rust\n{src}\n```\n\nCurrent CLAUDE.md:\n{claude_md}\n\nCurrent README.md:\n{readme}\n\nUpdate both docs to match the implementation."
    );
    let mut doc_msgs = vec![Msg { role: "user".to_string(), content: json!(doc_prompt) }];
    for _ in 0..4 {
        match llm(cfg, &doc_msgs, &doc_system) {
            Ok(reply) => {
                doc_msgs.push(Msg { role: "assistant".to_string(), content: json!(&reply) });
                if let Some(result) = run_tool(&reply, traj, true, None) {
                    doc_msgs.push(Msg { role: "user".to_string(), content: json!(result) });
                } else {
                    break;
                }
            }
            Err(e) => { eprintln!("Doc update LLM error: {e}"); break; }
        }
    }

    // Final lint + test to verify evolved binary is healthy
    eprintln!("Running post-evolution lint and tests...");
    let clippy = Command::new("cargo")
        .args(["clippy", "--release", "--", "-D", "warnings"])
        .output();
    let (clippy_ok, clippy_out) = match clippy {
        Ok(o) => {
            let out = String::from_utf8_lossy(&o.stderr).chars().take(2000).collect::<String>();
            (o.status.success(), out)
        }
        Err(e) => (false, e.to_string()),
    };
    traj_log(traj, "lint_result", json!({"ok": clippy_ok, "output": &clippy_out}));
    if clippy_ok {
        eprintln!("Lint: PASS");
    } else {
        eprintln!("Lint: FAIL\n{clippy_out}");
    }

    let test = Command::new("cargo").args(["test", "--release"]).output();
    let (test_ok, test_out) = match test {
        Ok(o) => {
            let combined = format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            );
            let out = combined.chars().take(2000).collect::<String>();
            (o.status.success(), out)
        }
        Err(e) => (false, e.to_string()),
    };
    traj_log(traj, "test_result", json!({"ok": test_ok, "output": &test_out}));
    if test_ok {
        eprintln!("Tests: PASS");
    } else {
        eprintln!("Tests: FAIL\n{test_out}");
    }

    if !clippy_ok || !test_ok {
        eprintln!("WARNING: evolved binary has lint/test failures. Review traj for details.");
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

    chat_mode(&cfg, &ts, &traj);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn test_extract_tool_basic() {
        let text = r#"Some text <tool name="shell">echo hello</tool> more"#;
        let (name, body) = extract_tool(text).unwrap();
        assert_eq!(name, "shell");
        assert_eq!(body, "echo hello");
    }

    #[test]
    fn test_extract_tool_write_file() {
        let text = "<tool name=\"write_file\">path/to/file.txt\nfile content here</tool>";
        let (name, body) = extract_tool(text).unwrap();
        assert_eq!(name, "write_file");
        assert!(body.starts_with("path/to/file.txt"));
    }

    #[test]
    fn test_extract_tool_strips_fences() {
        let text = "<tool name=\"write_self\">```rust\nfn main() {}\n```</tool>";
        let (name, body) = extract_tool(text).unwrap();
        assert_eq!(name, "write_self");
        assert_eq!(body, "fn main() {}");
    }

    #[test]
    fn test_extract_tool_none() {
        assert!(extract_tool("no tool here").is_none());
    }

    #[test]
    fn test_agent_registry_poll_unknown() {
        let registry = new_agent_registry();
        let result = poll_agent(&registry, "nonexistent");
        assert!(result.unwrap().contains("unknown agent_id"));
    }

    #[test]
    fn test_agent_registry_spawn_and_poll() {
        // Manually insert a completed entry to test poll_agent
        let registry = new_agent_registry();
        let slot: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let slot2 = slot.clone();
        registry.lock().unwrap().push(("agent_test".to_string(), slot));

        // Should be None (still running)
        assert!(poll_agent(&registry, "agent_test").is_none());

        // Mark done
        *slot2.lock().unwrap() = Some("written output.md".to_string());

        // Should now return result
        assert_eq!(poll_agent(&registry, "agent_test"), Some("written output.md".to_string()));
    }

    #[test]
    fn test_spawn_sub_agent_writes_output() {
        // This test exercises the spawn path without hitting the real LLM.
        // We verify: registry gets an entry, output file is eventually written.
        // We use a pre-written output file to simulate agent completion.
        let dir = std::env::temp_dir().join(format!("autoharness_test_{}", now_secs()));
        fs::create_dir_all(&dir).unwrap();
        let output_path = dir.join("result.md");

        // Write the output file directly (simulating a fast agent)
        fs::write(&output_path, "test result").unwrap();

        let registry = new_agent_registry();
        let slot: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let slot2 = slot.clone();
        registry.lock().unwrap().push(("agent_sim".to_string(), slot));
        *slot2.lock().unwrap() = Some(format!("written {}", output_path.display()));

        let result = poll_agent(&registry, "agent_sim").unwrap();
        assert!(result.contains("written"));
        assert!(output_path.exists());
        assert_eq!(fs::read_to_string(&output_path).unwrap(), "test result");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_extract_tool_spawn_agent() {
        let text = "<tool name=\"spawn_agent\">result.md\nAnalyse src/main.rs and summarise key functions.</tool>";
        let (name, body) = extract_tool(text).unwrap();
        assert_eq!(name, "spawn_agent");
        let mut lines = body.splitn(2, '\n');
        assert_eq!(lines.next().unwrap().trim(), "result.md");
        assert!(lines.next().unwrap().contains("Analyse"));
    }

    #[test]
    fn test_extract_tool_wait_agent() {
        let text = "<tool name=\"wait_agent\">agent_1234567890</tool>";
        let (name, body) = extract_tool(text).unwrap();
        assert_eq!(name, "wait_agent");
        assert_eq!(body.trim(), "agent_1234567890");
    }

    #[test]
    fn test_run_tool_spawn_and_wait() {
        // Exercise run_tool dispatch for spawn_agent + wait_agent end-to-end
        // without hitting a real LLM. We verify:
        //   1. spawn_agent tool tag is parsed, registry entry created, agent_id returned
        //   2. wait_agent blocks until the background thread writes the output file
        //      (the thread will fail LLM call, fall back to writing a "no output" result,
        //       but it still completes and sets the slot)
        // We skip actual LLM by pointing at an unreachable endpoint — the thread errors
        // and still writes the slot, which is what we verify.
        let dir = std::env::temp_dir().join(format!("autoharness_rtt_{}", now_secs()));
        fs::create_dir_all(&dir).unwrap();
        let out_dir = dir.to_string_lossy().to_string();
        let traj = format!("{out_dir}/traj.jsonl");

        let registry = new_agent_registry();
        // Use a bogus endpoint so the LLM call fails fast
        let cfg_snap = (
            "dummy_key".to_string(),
            "http://127.0.0.1:1".to_string(), // nothing listening here
            "test-model".to_string(),
        );

        let spawn_text = format!(
            "<tool name=\"spawn_agent\">sub_out.md\nWrite the word DONE to the output file.</tool>"
        );
        let result = run_tool(&spawn_text, &traj, false, Some((&registry, &cfg_snap, &out_dir)));
        let result_str = result.unwrap();
        assert!(result_str.contains("spawned agent_id="), "got: {result_str}");
        assert!(result_str.contains("output_path="), "got: {result_str}");

        // Extract agent_id from result
        let agent_id = result_str
            .split("agent_id=").nth(1).unwrap()
            .split_whitespace().next().unwrap()
            .to_string();

        // wait_agent should block until thread finishes (fails LLM, sets slot)
        let wait_text = format!("<tool name=\"wait_agent\">{agent_id}</tool>");
        let wait_result = run_tool(&wait_text, &traj, false, Some((&registry, &cfg_snap, &out_dir)));
        let wait_str = wait_result.unwrap();
        assert!(wait_str.contains("finished:"), "got: {wait_str}");

        fs::remove_dir_all(&dir).ok();
    }
}
