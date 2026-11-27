//! Codex's App Server owns the ChatGPT login and conversation history.

use std::collections::{HashSet, VecDeque};
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::claude::SYSTEM_PROMPT;
use crate::event::{EffortChoice, Event, ModelChoice, ModelEvent};

const RPC_TIMEOUT: Duration = Duration::from_secs(30);
const TURN_TIMEOUT: Duration = Duration::from_secs(180);

enum Input {
    Turn(Vec<Value>),
    Server(Value),
    Closed,
}

pub struct Codex {
    child: Child,
    input: Sender<Input>,
}

impl Codex {
    pub fn spawn(model: &str, effort: Option<&str>, resume: Option<&str>, fast: bool, generation: u64, events: Sender<Event>) -> Result<Self> {
        let cwd = crate::config::data_dir().join("codex");
        std::fs::create_dir_all(&cwd).with_context(|| format!("cannot create {}", cwd.display()))?;
        let cwd = std::path::absolute(cwd)?;
        let mut command = Command::new(codex_program());
        command
            .args(["app-server", "--stdio"])
            .args(["-c", "model_provider=\"openai\"", "-c", "web_search=\"disabled\""])
            .args([if fast { "--enable" } else { "--disable" }, "fast_mode"])
            .args(["--disable", "ultrafast_mode", "--disable", "shell_tool", "--disable", "view_image"])
            .args(["--disable", "apps", "--disable", "plugins", "--disable", "hooks", "--disable", "memories"])
            .env_remove("DEEPGRAM_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .env_remove("OPENAI_BASE_URL")
            .env_remove("CODEX_ACCESS_TOKEN")
            .current_dir(&cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = command
            .spawn()
            .context("cannot start `codex`. Install Codex CLI and run `codex login`")?;
        let stdin = child.stdin.take().expect("Codex stdin was piped");
        let stdout = child.stdout.take().expect("Codex stdout was piped");
        let stderr = child.stderr.take().expect("Codex stderr was piped");

        let (input, incoming) = channel::<Input>();
        let output = input.clone();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(message) = serde_json::from_str(&line)
                    && output.send(Input::Server(message)).is_err()
                {
                    return;
                }
            }
            let _ = output.send(Input::Closed);
        });

        let tail = Arc::new(Mutex::new(VecDeque::new()));
        let stderr_tail = Arc::clone(&tail);
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let mut lines = stderr_tail.lock().unwrap_or_else(|e| e.into_inner());
                lines.push_back(line);
                if lines.len() > 8 {
                    lines.pop_front();
                }
            }
        });

        let model = model.to_owned();
        let effort = effort.map(str::to_owned);
        let resume = resume.map(str::to_owned);
        thread::spawn(move || {
            let emit = |event| {
                let _ = events.send(Event::Model(generation, event));
            };
            let rpc = Rpc { stdin, incoming, next_id: 0, pending: VecDeque::new() };
            if let Err(error) = run(rpc, &cwd, &model, effort.as_deref(), resume.as_deref(), fast, &emit) {
                let reason = if error.is::<RpcClosed>() {
                    let lines = tail.lock().unwrap_or_else(|e| e.into_inner());
                    let details = lines.iter().map(String::as_str).collect::<Vec<_>>().join("\n");
                    if details.trim().is_empty() { format!("{error:#}") } else { format!("{error:#}\n{details}") }
                } else {
                    format!("{error:#}")
                };
                let _ = events.send(Event::ModelExited(generation, reason));
            }
        });

        Ok(Self { child, input })
    }

    pub fn send(&self, content: Vec<Value>) -> Result<()> {
        self.input.send(Input::Turn(content)).ok().context("the Codex process is gone")
    }
}

impl Drop for Codex {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn codex_program() -> OsString {
    if let Some(program) = std::env::var_os("CLUELESS_CODEX").filter(|program| !program.is_empty()) {
        return program;
    }
    #[cfg(windows)]
    if let Some(path) = std::env::var_os("PATH") {
        let directories: Vec<_> = std::env::split_paths(&path).collect();
        for dir in &directories {
            let exe = dir.join("codex.exe");
            if exe.is_file() {
                return exe.into_os_string();
            }
        }
        // npm exposes a script on PATH. Launch the binary so Stop can end the app-server itself.
        #[cfg(target_arch = "aarch64")]
        let (package, triple) = ("codex-win32-arm64", "aarch64-pc-windows-msvc");
        #[cfg(not(target_arch = "aarch64"))]
        let (package, triple) = ("codex-win32-x64", "x86_64-pc-windows-msvc");
        for dir in directories {
            if !dir.join("codex.cmd").is_file() {
                continue;
            }
            let scope = dir.join("node_modules").join("@openai");
            for packages in [&scope, &scope.join("codex").join("node_modules").join("@openai")] {
                let exe = packages.join(package).join("vendor").join(triple).join("bin").join("codex.exe");
                if exe.is_file() {
                    return exe.into_os_string();
                }
            }
        }
    }
    "codex".into()
}

#[derive(Debug)]
struct RpcClosed;

impl std::fmt::Display for RpcClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("The Codex app-server closed its output")
    }
}

impl std::error::Error for RpcClosed {}

struct Rpc {
    stdin: ChildStdin,
    incoming: Receiver<Input>,
    next_id: u64,
    pending: VecDeque<Vec<Value>>,
}

impl Rpc {
    fn write(&mut self, message: Value) -> Result<()> {
        writeln!(self.stdin, "{message}").context("cannot write to Codex")?;
        self.stdin.flush().context("cannot flush Codex input")
    }

    fn call(&mut self, method: &str, params: Value, mut notify: impl FnMut(Value)) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        self.write(json!({"id": id, "method": method, "params": params}))?;
        let deadline = Instant::now() + RPC_TIMEOUT;
        loop {
            let message = self.receive(deadline).with_context(|| format!("Codex {method}"))?;
            if message["id"].as_u64() == Some(id) {
                if let Some(error) = message["error"]["message"].as_str() {
                    bail!("Codex {method}: {error}");
                }
                return message.get("result").cloned().with_context(|| format!("missing result for Codex {method}"));
            }
            if message.get("method").is_some() {
                notify(message);
            }
        }
    }

    fn receive(&mut self, deadline: Instant) -> Result<Value> {
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!("timed out waiting for Codex");
            }
            match self.incoming.recv_timeout(remaining).context("waiting for Codex output")? {
                Input::Server(message) => {
                    if !self.deny_request(&message)? {
                        return Ok(message);
                    }
                }
                Input::Turn(content) => self.pending.push_back(content),
                Input::Closed => bail!(RpcClosed),
            }
        }
    }

    fn next_turn(&mut self) -> Result<Vec<Value>> {
        if let Some(content) = self.pending.pop_front() {
            return Ok(content);
        }
        loop {
            match self.incoming.recv()? {
                Input::Turn(content) => return Ok(content),
                Input::Server(message) => {
                    self.deny_request(&message)?;
                }
                Input::Closed => bail!(RpcClosed),
            }
        }
    }

    fn deny_request(&mut self, message: &Value) -> Result<bool> {
        let (Some(id), Some(method)) = (message.get("id"), message["method"].as_str()) else {
            return Ok(false);
        };
        let response = match method {
            "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
                json!({"id": id, "result": {"decision": "decline"}})
            }
            "item/permissions/requestApproval" => {
                json!({"id": id, "result": {"permissions": {}, "scope": "turn"}})
            }
            _ => json!({"id": id, "error": {"code": -32601, "message": "Unavailable in Clueless"}}),
        };
        self.write(response)?;
        Ok(true)
    }
}

fn run(
    mut rpc: Rpc,
    cwd: &Path,
    model: &str,
    effort: Option<&str>,
    resume: Option<&str>,
    fast: bool,
    emit: &impl Fn(ModelEvent),
) -> Result<()> {
    rpc.call(
        "initialize",
        json!({"clientInfo": {"name": "clueless", "title": "Clueless", "version": env!("CARGO_PKG_VERSION")}}),
        |_| {},
    )?;
    rpc.write(json!({"method": "initialized"}))?;

    let account = rpc.call("account/read", json!({"refreshToken": false}), |_| {})?;
    if account["account"]["type"] != "chatgpt" {
        bail!("Sign in to Codex with your ChatGPT account using `codex login`, then retry.");
    }

    // An empty mcp_servers table merges with inherited configuration. Disable each configured server for our thread.
    let config = rpc.call("config/read", json!({"cwd": cwd, "includeLayers": false}), |_| {})?;
    let disabled_mcp = disabled_mcp_servers(&config);
    let mut overrides = serde_json::Map::new();
    if !disabled_mcp.is_empty() {
        overrides.insert("mcp_servers".into(), json!(disabled_mcp));
    }
    if let Some(effort) = effort {
        overrides.insert("model_reasoning_effort".into(), json!(effort));
    }

    if let Ok(models) = list_models(&mut rpc) {
        let configured_model = config["config"]["model"].as_str().unwrap_or_default();
        let selected = if model.is_empty() { configured_model } else { model };
        let entry = models.iter().find(|entry| {
            if selected.is_empty() {
                entry["isDefault"].as_bool() == Some(true)
            } else {
                entry["id"].as_str() == Some(selected) || entry["model"].as_str() == Some(selected)
            }
        });
        let choices = entry.map(effort_choices).unwrap_or_default();
        emit(ModelEvent::Efforts { model: model.into(), choices, current: None });
        emit(ModelEvent::Models(models.iter().filter_map(|entry| {
            let id = entry["id"].as_str()?;
            let label = entry["displayName"].as_str().unwrap_or(id);
            Some(ModelChoice { id: id.into(), label: label.into() })
        }).collect()));
    } else {
        emit(ModelEvent::Efforts { model: model.into(), choices: Vec::new(), current: None });
    }

    let mut current_thread = None;
    loop {
        let input = rpc.next_turn()?;
        let input = match codex_input(input) {
            Ok(input) => input,
            Err(error) => {
                emit(ModelEvent::Failed(format!("{error:#}")));
                continue;
            }
        };
        if current_thread.is_none() {
            let (method, mut params) = thread_request(cwd, model, resume, fast);
            if !overrides.is_empty() {
                params["config"] = json!(overrides);
            }
            let state = rpc.call(method, params, |_| {}).map_err(|e| {
                if resume.is_some() && e.to_string().contains("no rollout found") {
                    e.context("No conversation found")
                } else {
                    e
                }
            })?;
            verify_tier(&state, fast)?;
            if let Some(effort) = effort
                && state["reasoningEffort"].as_str() != Some(effort)
            {
                let applied = state["reasoningEffort"].as_str().unwrap_or("unset");
                bail!("Codex returned effort {applied} instead of {effort}. Choose a supported effort or Default.");
            }
            let id = state["thread"]["id"].as_str().context("Codex did not return a thread ID")?;
            emit(ModelEvent::Session(id.to_owned()));
            current_thread = Some(id.to_owned());
        }
        let thread = current_thread.as_deref().expect("Codex thread was initialized");
        run_turn(&mut rpc, thread, input, fast, effort, emit)?;
    }
}

fn disabled_mcp_servers(config: &Value) -> serde_json::Map<String, Value> {
    config["config"]["mcp_servers"]
        .as_object()
        .into_iter()
        .flat_map(|servers| servers.keys())
        .map(|name| (name.to_owned(), json!({"enabled": false})))
        .collect()
}

fn list_models(rpc: &mut Rpc) -> Result<Vec<Value>> {
    let mut models = Vec::new();
    let mut cursor = None;
    loop {
        let result = rpc.call("model/list", json!({"limit": 100, "cursor": cursor, "includeHidden": false}), |_| {})?;
        if let Some(data) = result["data"].as_array() {
            models.extend(data.iter().cloned());
        }
        let next = result["nextCursor"].as_str().map(str::to_owned);
        if next.is_none() || next == cursor {
            return Ok(models);
        }
        cursor = next;
    }
}

fn effort_choices(model: &Value) -> Vec<EffortChoice> {
    model["supportedReasoningEfforts"].as_array().into_iter().flatten().filter_map(|entry| {
        let id = entry["reasoningEffort"].as_str().filter(|id| !id.is_empty())?;
        Some(EffortChoice { id: id.into(), label: id.into() })
    }).collect()
}

fn service_tier(fast: bool) -> &'static str {
    if fast { "fast" } else { "default" }
}

fn thread_request(cwd: &Path, model: &str, resume: Option<&str>, fast: bool) -> (&'static str, Value) {
    let mut params = json!({
        "cwd": cwd,
        "sandbox": "read-only",
        "approvalPolicy": "never",
        "serviceTier": service_tier(fast),
        "baseInstructions": SYSTEM_PROMPT,
        "developerInstructions": "Answer from the conversation and attached input. Do not call tools, inspect the computer, run commands, or write progress updates.",
    });
    if !model.is_empty() {
        params["model"] = json!(model);
    }
    if let Some(id) = resume {
        params["threadId"] = json!(id);
        params["excludeTurns"] = json!(true);
        ("thread/resume", params)
    } else {
        ("thread/start", params)
    }
}

fn verify_tier(thread_state: &Value, fast: bool) -> Result<()> {
    let applied = thread_state["serviceTier"].as_str();
    if fast && !matches!(applied, Some("priority" | "fast")) {
        bail!("Codex did not enable Fast mode. Check that the Codex CLI and selected model support it.");
    }
    if !fast && !matches!(applied, None | Some("default")) {
        bail!("Codex did not accept standard speed. Update Codex CLI and try again.");
    }
    Ok(())
}

fn codex_input(content: Vec<Value>) -> Result<Vec<Value>> {
    content
        .into_iter()
        .map(|block| match block["type"].as_str() {
            Some("text") => {
                let text = block["text"].as_str().context("missing text attachment content")?;
                Ok(json!({"type": "text", "text": text}))
            }
            Some("image") => {
                let source = &block["source"];
                let mime = source["media_type"].as_str().context("missing image media type")?;
                let data = source["data"].as_str().context("missing image content")?;
                Ok(json!({"type": "image", "url": format!("data:{mime};base64,{data}")}))
            }
            Some("document") if block["source"]["media_type"] == "application/pdf" => {
                let data = block["source"]["data"].as_str().context("missing PDF attachment content")?;
                let text = crate::files::pdf_text_for_model(data).context("cannot extract text from the PDF for Codex")?;
                Ok(json!({"type": "text", "text": text}))
            }
            _ => bail!("Codex received an unsupported attachment"),
        })
        .collect()
}

fn turn_params(thread: &str, input: Vec<Value>, fast: bool, effort: Option<&str>) -> Value {
    let mut params = json!({"threadId": thread, "input": input, "serviceTier": service_tier(fast)});
    if let Some(effort) = effort {
        params["effort"] = json!(effort);
    }
    params
}

fn run_turn(rpc: &mut Rpc, thread: &str, input: Vec<Value>, fast: bool, effort: Option<&str>, emit: &impl Fn(ModelEvent)) -> Result<()> {
    let mut queued = VecDeque::new();
    let result = rpc.call("turn/start", turn_params(thread, input, fast, effort), |message| queued.push_back(message))?;
    let turn_id = result["turn"]["id"].as_str().context("Codex did not return a turn ID")?;
    let mut reply = CodexReply::default();
    let deadline = Instant::now() + TURN_TIMEOUT;
    loop {
        let message = match queued.pop_front() {
            Some(message) => message,
            None => rpc.receive(deadline)?,
        };
        if let Some(event) = reply.receive(&message, thread, turn_id) {
            let done = matches!(&event, ModelEvent::Done(_) | ModelEvent::Failed(_));
            emit(event);
            if done {
                return Ok(());
            }
        }
    }
}

#[derive(Default)]
struct CodexReply {
    commentary: HashSet<String>,
    text: String,
}

impl CodexReply {
    fn receive(&mut self, message: &Value, thread: &str, turn: &str) -> Option<ModelEvent> {
        let params = &message["params"];
        if params["threadId"].as_str() != Some(thread) {
            return None;
        }
        let item = &params["item"];
        let method = message["method"].as_str()?;
        let event_turn = if method == "turn/completed" { &params["turn"]["id"] } else { &params["turnId"] };
        if event_turn.as_str() != Some(turn) {
            return None;
        }
        match method {
            "item/started" if item["type"] == "agentMessage" && item["phase"] == "commentary" => {
                self.commentary.insert(item["id"].as_str()?.to_owned());
                None
            }
            "item/agentMessage/delta" if !self.commentary.contains(params["itemId"].as_str()?) => {
                let delta = params["delta"].as_str()?;
                Some(ModelEvent::Delta(delta.to_owned()))
            }
            "item/completed" if item["type"] == "agentMessage" && item["phase"] != "commentary" => {
                self.text.push_str(item["text"].as_str()?);
                None
            }
            "turn/completed" if params["turn"]["status"] == "completed" => {
                Some(ModelEvent::Done(std::mem::take(&mut self.text)))
            }
            "turn/completed" => {
                let error = params["turn"]["error"]["message"].as_str().unwrap_or("The Codex turn did not complete.");
                Some(ModelEvent::Failed(error.to_owned()))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_setting_reaches_new_resumed_and_subsequent_turns() {
        for (fast, expected, acknowledged) in [(true, "fast", "priority"), (false, "default", "default")] {
            let (start, start_params) = thread_request(Path::new("."), "", None, fast);
            let (resume, resume_params) = thread_request(Path::new("."), "", Some("thread"), fast);
            assert_eq!(start, "thread/start");
            assert_eq!(resume, "thread/resume");
            assert_eq!(start_params["serviceTier"], expected);
            assert_eq!(resume_params["serviceTier"], expected);
            for effort in [None, Some("low"), Some("high")] {
                let params = turn_params("thread", vec![], fast, effort);
                assert_eq!(params["serviceTier"], expected);
                assert_eq!(params.get("effort").and_then(Value::as_str), effort);
            }
            assert!(verify_tier(&json!({"serviceTier": acknowledged}), fast).is_ok());
        }
        assert!(verify_tier(&json!({"serviceTier": "default"}), true).is_err());
        assert!(verify_tier(&json!({"serviceTier": "priority"}), false).is_err());
        assert!(verify_tier(&json!({"serviceTier": null}), false).is_ok());
    }

    #[test]
    fn disables_inherited_mcp_servers_for_the_thread() {
        let config = json!({"config": {"mcp_servers": {
            "user-tool": {"command": "existing-command"},
            "disabled-tool": {"enabled": false}
        }}});
        let overrides = disabled_mcp_servers(&config);
        assert_eq!(json!(overrides), json!({
            "user-tool": {"enabled": false},
            "disabled-tool": {"enabled": false}
        }));
    }

    #[test]
    fn attaches_images_with_data_urls() {
        let input = codex_input(vec![
            json!({"type": "image", "source": {"media_type": "image/jpeg", "data": "AA=="}}),
            json!({"type": "text", "text": "Describe the image."}),
        ])
        .unwrap();
        assert_eq!(input[0], json!({"type": "image", "url": "data:image/jpeg;base64,AA=="}));
        assert_eq!(input[1]["text"], "Describe the image.");
    }

    #[test]
    fn excludes_commentary_and_other_turns_from_the_reply() {
        let mut reply = CodexReply::default();
        let start = json!({"method": "item/started", "params": {
            "threadId": "thread", "turnId": "turn", "item": {"type": "agentMessage", "phase": "commentary", "id": "progress"},
        }});
        let delta = |id, turn| json!({"method": "item/agentMessage/delta", "params": {
            "threadId": "thread", "turnId": turn, "itemId": id, "delta": "Hello"
        }});
        assert!(reply.receive(&start, "thread", "turn").is_none());
        assert!(reply.receive(&delta("progress", "turn"), "thread", "turn").is_none());
        assert!(reply.receive(&delta("answer", "old"), "thread", "turn").is_none());
        assert!(matches!(reply.receive(&delta("answer", "turn"), "thread", "turn"), Some(ModelEvent::Delta(text)) if text == "Hello"));
        let item = json!({"method": "item/completed", "params": {
            "threadId": "thread", "turnId": "turn", "item": {"type": "agentMessage", "phase": "final_answer", "id": "answer", "text": "Hello!"},
        }});
        let end = json!({"method": "turn/completed", "params": {
            "threadId": "thread", "turn": {"id": "turn", "status": "completed"}
        }});
        assert!(reply.receive(&item, "thread", "turn").is_none());
        assert!(matches!(reply.receive(&end, "thread", "turn"), Some(ModelEvent::Done(text)) if text == "Hello!"));
    }
}
