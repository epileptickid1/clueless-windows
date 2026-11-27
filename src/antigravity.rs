//! Antigravity's ACP v1 server over newline-delimited JSON-RPC.

use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::claude::SYSTEM_PROMPT;
use crate::event::{EffortChoice, Event, ModelChoice, ModelEvent};

const CHAT_ONLY: &str = "Answer from the supplied conversation and attachments. Do not use tools, \
    run commands, read or write local files, browse, or delegate work.";

pub struct Antigravity {
    child: Child,
    prompts: Sender<Vec<Value>>,
}

impl Antigravity {
    pub fn spawn(model: &str, effort: Option<&str>, resume: Option<&str>, generation: u64, events: Sender<Event>) -> Result<Self> {
        let cwd = std::path::absolute(crate::config::data_dir().join("antigravity"))?;
        std::fs::create_dir_all(&cwd).with_context(|| format!("cannot create {}", cwd.display()))?;
        let program = std::env::var_os("CLUELESS_AGY").unwrap_or_else(|| "agy".into());
        // Session-local settings keep the signed-in CLI in chat mode without changing its global preferences.
        let settings = json!({
            "modelProvider": "",
            "disableSlashCommands": true,
            "permissions": {"deny": [
                "command(*)", "read_file(*)", "write_file(*)", "read_url(*)", "execute_url(*)", "mcp(*)"
            ]}
        });
        let mut cmd = Command::new(program);
        cmd.arg(format!("--settings={settings}"))
            .args(["acp", "--sandbox", "--include-default-customizations=false", "--disable-telemetry"])
            .current_dir(&cwd)
            .env_remove("DEEPGRAM_API_KEY")
            .env_remove("GEMINI_API_KEY")
            .env_remove("GOOGLE_API_KEY")
            .env_remove("AGY_ADC_AUTH")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = cmd
            .spawn()
            .context("cannot start `agy acp`. Install Antigravity CLI and run `agy acp --login`")?;
        let input = child.stdin.take().context("no Antigravity stdin")?;
        let output = child.stdout.take().context("no Antigravity stdout")?;

        let (prompts, incoming) = channel();
        let model = model.to_owned();
        let effort = effort.map(str::to_owned);
        let resume = resume.map(str::to_owned);
        thread::spawn(move || {
            let mut acp = Acp::new(BufReader::new(output), BufWriter::new(input), generation, events.clone());
            if let Err(error) = acp.run(&cwd, &model, effort.as_deref(), resume.as_deref(), incoming) {
                let _ = events.send(Event::ModelExited(generation, format!("{error:#}")));
            }
        });
        Ok(Self { child, prompts })
    }

    pub fn send(&self, content: Vec<Value>) -> Result<()> {
        self.prompts.send(content).ok().context("the Antigravity process is gone")
    }
}

impl Drop for Antigravity {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Acp<R, W> {
    reader: R,
    writer: W,
    next_id: u64,
    generation: u64,
    events: Sender<Event>,
}

impl<R: BufRead, W: Write> Acp<R, W> {
    fn new(reader: R, writer: W, generation: u64, events: Sender<Event>) -> Self {
        Self { reader, writer, next_id: 1, generation, events }
    }

    fn emit(&self, event: ModelEvent) {
        let _ = self.events.send(Event::Model(self.generation, event));
    }

    fn write(&mut self, message: Value) -> Result<()> {
        writeln!(self.writer, "{message}").context("could not write to Antigravity")?;
        self.writer.flush().context("could not flush Antigravity stdin")
    }

    fn request(&mut self, method: &str, params: Value, active_session: Option<&str>) -> Result<(Value, String)> {
        let id = self.next_id;
        self.next_id += 1;
        self.write(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;

        let mut line = String::new();
        let mut answer = String::new();
        loop {
            line.clear();
            if self.reader.read_line(&mut line).context("could not read from Antigravity")? == 0 {
                bail!("Antigravity closed its output while handling {method}. \
                    Check `agy acp --help` and sign in with `agy acp --login`.");
            }
            if line.trim().is_empty() {
                continue;
            }
            let message: Value = serde_json::from_str(&line).context("invalid Antigravity ACP message")?;
            if let Some(server_method) = message["method"].as_str() {
                if let Some(server_id) = message.get("id") {
                    self.write(reject_client_request(server_method, server_id, &message["params"]))?;
                } else if server_method == "session/update"
                    && active_session.is_some()
                    && message["params"]["sessionId"].as_str() == active_session
                {
                    let update = &message["params"]["update"];
                    match update["sessionUpdate"].as_str() {
                        Some("agent_message_chunk") if method == "session/prompt" => {
                            if let Some(chunk) = update["content"]["text"].as_str() {
                                answer.push_str(chunk);
                                self.emit(ModelEvent::Delta(chunk.into()));
                            }
                        }
                        Some("config_option_update") => self.publish_models(update),
                        _ => {}
                    }
                }
                continue;
            }
            if message["id"].as_u64() != Some(id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                let detail = error["message"].as_str().unwrap_or("unexpected protocol error");
                match error["code"].as_i64() {
                    Some(-32000) => bail!("Antigravity requires sign-in. Run `agy acp --login` and try again."),
                    Some(-32002) if matches!(method, "session/resume" | "session/load") => {
                        bail!("No conversation found in Antigravity. Try again to start a fresh session.")
                    }
                    _ => bail!("Antigravity {method}: {detail}"),
                }
            }
            let result = message.get("result").context("missing Antigravity ACP result")?;
            return Ok((result.clone(), answer));
        }
    }

    fn run(&mut self, cwd: &Path, model: &str, effort: Option<&str>, resume: Option<&str>, incoming: Receiver<Vec<Value>>) -> Result<()> {
        let (hello, _) = self.request(
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {},
                "clientInfo": {"name": "clueless", "title": "Clueless", "version": env!("CARGO_PKG_VERSION")}
            }),
            None,
        )?;
        if hello["protocolVersion"].as_u64() != Some(1) {
            bail!("this version of Antigravity does not speak ACP v1");
        }

        let capabilities = &hello["agentCapabilities"];
        let session_capabilities = &capabilities["sessionCapabilities"];
        let can_append = session_capabilities["systemPrompt"].is_object();
        let mut params = json!({"cwd": cwd, "mcpServers": []});
        if can_append {
            params["systemPrompt"] = json!({"mode": "append", "content": [instructions()]});
        }
        let setup_method = match resume {
            None => "session/new",
            Some(id) => {
                params["sessionId"] = json!(id);
                if session_capabilities["resume"].is_object() {
                    "session/resume"
                } else if capabilities["loadSession"].as_bool() == Some(true) {
                    "session/load"
                } else {
                    bail!("No conversation found in Antigravity. This version cannot resume a session.");
                }
            }
        };
        let (setup, _) = self.request(setup_method, params, None)?;
        let session_id = setup["sessionId"].as_str().or(resume).context("Antigravity returned no session ID")?;
        self.publish_models(&setup);
        let selected = if model.is_empty() { None } else { self.select_model(&setup, session_id, model)? };
        self.configure_effort(selected.as_ref().unwrap_or(&setup), session_id, model, effort)?;
        self.emit(ModelEvent::Session(session_id.into()));

        let prompt_capabilities = &capabilities["promptCapabilities"];
        let mut prepend_instructions = !can_append;
        for content in incoming {
            let prompt = match prompt_blocks(content, prompt_capabilities, prepend_instructions) {
                Ok(prompt) => prompt,
                Err(error) => {
                    self.emit(ModelEvent::Failed(error.to_string()));
                    continue;
                }
            };
            let (result, answer) = self.request(
                "session/prompt",
                json!({"sessionId": session_id, "prompt": prompt}),
                Some(session_id),
            )?;
            let stop_reason = result["stopReason"].as_str().unwrap_or("unknown");
            if stop_reason == "end_turn"
                || (matches!(stop_reason, "refusal" | "max_tokens" | "max_turn_requests") && !answer.trim().is_empty())
            {
                prepend_instructions = false;
                self.emit(ModelEvent::Done(answer));
            } else {
                self.emit(ModelEvent::Failed(format!("Antigravity ended the request ({stop_reason}).")));
            }
        }
        Ok(())
    }

    fn publish_models(&self, response: &Value) {
        let mut models = Vec::new();
        if let Some(config) = model_config(response) {
            collect_model_options(&config["options"], &mut models);
        } else if let Some(options) = response["models"]["availableModels"].as_array() {
            models.extend(options.iter().filter_map(|option| {
                let id = option["modelId"].as_str()?;
                let label = option["name"].as_str().unwrap_or(id);
                Some(ModelChoice { id: id.into(), label: label.into() })
            }));
        }
        if !models.is_empty() {
            self.emit(ModelEvent::Models(models));
        }
    }

    fn select_model(&mut self, response: &Value, session_id: &str, model: &str) -> Result<Option<Value>> {
        if let Some(config) = model_config(response) {
            if config["currentValue"].as_str() == Some(model) {
                return Ok(None);
            }
            let config_id = config["id"].as_str().context("Antigravity model option has no ID")?;
            let (options, _) = self.request(
                "session/set_config_option",
                json!({"sessionId": session_id, "configId": config_id, "value": model}),
                Some(session_id),
            )?;
            self.publish_models(&options);
            return Ok(Some(options));
        } else if response["models"].is_object() {
            if response["models"]["currentModelId"].as_str() != Some(model) {
                self.request("session/set_model", json!({"sessionId": session_id, "modelId": model}), None)?;
            }
        } else {
            // Resume responses may omit configOptions. Antigravity's model option ID is `model`.
            let (options, _) = self.request(
                "session/set_config_option",
                json!({"sessionId": session_id, "configId": "model", "value": model}),
                Some(session_id),
            )?;
            self.publish_models(&options);
            return Ok(Some(options));
        }
        Ok(None)
    }

    fn configure_effort(&mut self, response: &Value, session_id: &str, model: &str, effort: Option<&str>) -> Result<()> {
        let option = thought_config(response);
        let current = option.and_then(|config| config["currentValue"].as_str());
        let mut choices = Vec::new();
        if let Some(config) = option {
            collect_options(&config["options"], &mut |id, label| {
                choices.push(EffortChoice { id: id.into(), label: label.into() });
            });
        }
        if let Some(effort) = effort.filter(|&effort| Some(effort) != current) {
            if !choices.iter().any(|choice| choice.id == effort) {
                self.emit(ModelEvent::Efforts { model: model.into(), choices, current: current.map(str::to_owned) });
                bail!("Antigravity does not offer effort {effort} for this model. Choose an available level.");
            }
            let config_id = option.and_then(|config| config["id"].as_str()).context("Antigravity's thinking level option has no ID")?;
            let (updated, _) = self.request(
                "session/set_config_option",
                json!({"sessionId": session_id, "configId": config_id, "value": effort}),
                Some(session_id),
            )?;
            if thought_config(&updated).and_then(|config| config["currentValue"].as_str()) != Some(effort) {
                bail!("Antigravity did not confirm effort {effort}.");
            }
            return self.configure_effort(&updated, session_id, model, None);
        }
        if !choices.is_empty() {
            self.emit(ModelEvent::Efforts { model: model.into(), choices, current: current.map(str::to_owned) });
        }
        Ok(())
    }
}

fn reject_client_request(method: &str, id: &Value, params: &Value) -> Value {
    if method == "session/request_permission" {
        let reject = params["options"].as_array().and_then(|options| {
            options.iter()
                .filter(|option| option["kind"] == "reject_once")
                .find_map(|option| option["optionId"].as_str())
        });
        let outcome = match reject {
            Some(option_id) => json!({"outcome": "selected", "optionId": option_id}),
            None => json!({"outcome": "cancelled"}),
        };
        json!({"jsonrpc": "2.0", "id": id, "result": {"outcome": outcome}})
    } else {
        json!({"jsonrpc": "2.0", "id": id, "error": {
            "code": -32601, "message": "Clueless does not expose client tools"
        }})
    }
}

fn model_config(response: &Value) -> Option<&Value> {
    response["configOptions"].as_array()?.iter().find(|entry| {
        entry["type"] == "select" && (entry["category"] == "model" || entry["id"] == "model")
    })
}

fn thought_config(response: &Value) -> Option<&Value> {
    response["configOptions"].as_array()?.iter().find(|entry| {
        entry["type"] == "select"
            && (entry["category"] == "thought_level" || matches!(entry["id"].as_str(), Some("thought_level" | "thinking_level")))
    })
}

fn collect_model_options(options: &Value, models: &mut Vec<ModelChoice>) {
    collect_options(options, &mut |id, label| models.push(ModelChoice { id: id.into(), label: label.into() }));
}

fn collect_options(options: &Value, add: &mut impl FnMut(&str, &str)) {
    if let Some(options) = options.as_array() {
        for option in options {
            if let Some(id) = option["value"].as_str() {
                let label = option["name"].as_str().unwrap_or(id);
                add(id, label);
            } else if let Some(group) = option.get("options") {
                collect_options(group, add);
            }
        }
    }
}

fn instructions() -> Value {
    json!({"type": "text", "text": format!("{SYSTEM_PROMPT}\n{CHAT_ONLY}")})
}

fn prompt_blocks(content: Vec<Value>, capabilities: &Value, prepend_instructions: bool) -> Result<Vec<Value>> {
    let mut prompt = Vec::with_capacity(content.len() + usize::from(prepend_instructions));
    if prepend_instructions {
        prompt.push(instructions());
    }
    for (index, block) in content.into_iter().enumerate() {
        match block["type"].as_str() {
            Some("text") => prompt.push(block),
            Some("image") if capabilities["image"].as_bool() == Some(true) => prompt.push(json!({
                "type": "image", "mimeType": block["source"]["media_type"], "data": block["source"]["data"]
            })),
            Some("document") if capabilities["embeddedContext"].as_bool() == Some(true) => prompt.push(json!({
                "type": "resource", "resource": {
                    "uri": format!("clueless:///attachment/{index}.pdf"),
                    "mimeType": block["source"]["media_type"],
                    "blob": block["source"]["data"]
                }
            })),
            Some("image") => bail!("This Antigravity session does not accept images."),
            Some("document") => {
                bail!("This Antigravity session does not accept PDF attachments. Extract the text and attach that instead.");
            }
            _ => bail!("Unsupported Antigravity attachment."),
        }
    }
    Ok(prompt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_the_selected_model_thinking_level_before_prompting() {
        let transcript = br#"
{"id":1,"result":{"protocolVersion":1}}
{"id":2,"result":{"sessionId":"s1","configOptions":[{"id":"model","category":"model","type":"select","currentValue":"a","options":[{"value":"a"},{"value":"b"}]}]}}
{"id":3,"result":{"configOptions":[{"id":"thinking-level","category":"thought_level","type":"select","currentValue":"low","options":[{"value":"low"},{"value":"high"}]}]}}
{"id":4,"result":{"configOptions":[{"id":"thinking-level","category":"thought_level","type":"select","currentValue":"high","options":[{"value":"low"},{"value":"high"}]}]}}
{"id":5,"result":{"stopReason":"end_turn"}}
"#;
        let (events, received) = channel();
        let (prompts, queue) = channel();
        prompts.send(vec![json!({"type":"text", "text":"Hi"})]).unwrap();
        drop(prompts);
        let mut acp = Acp::new(&transcript[..], Vec::new(), 42, events);
        acp.run(Path::new("test"), "b", Some("high"), None, queue).unwrap();
        let requests: Vec<Value> = std::str::from_utf8(&acp.writer).unwrap().lines()
            .map(|line| serde_json::from_str(line).unwrap()).collect();
        assert_eq!(requests[2]["params"]["value"], "b");
        assert_eq!(requests[3]["method"], "session/set_config_option");
        assert_eq!(requests[3]["params"], json!({"sessionId":"s1", "configId":"thinking-level", "value":"high"}));
        assert_eq!(requests[4]["method"], "session/prompt");
        assert!(received.try_iter().any(|event| matches!(event, Event::Model(42,
            ModelEvent::Efforts { current: Some(level), .. }) if level == "high")));
    }

    #[test]
    fn rejects_an_unconfirmed_thinking_level() {
        let options = json!({"configOptions":[{
            "id":"thinking_level", "type":"select", "currentValue":"low", "options":[{"value":"low"}, {"value":"high"}]
        }]});
        let reply = json!({"id":1, "result":options}).to_string() + "\n";
        let (events, _) = channel();
        let mut acp = Acp::new(reply.as_bytes(), Vec::new(), 42, events);
        let error = acp.configure_effort(&options, "s1", "model", Some("high")).unwrap_err();
        assert!(error.to_string().contains("did not confirm effort high"));
    }

    #[test]
    fn resumes_with_selected_model_and_streams_prompt_turns() {
        let transcript = br#"
{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{"sessionCapabilities":{"resume":{},"systemPrompt":{}},"promptCapabilities":{"image":true,"embeddedContext":true}}}}
{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"old answer"}}}}
{"jsonrpc":"2.0","id":2,"result":{"configOptions":[{"id":"model-choice","category":"model","type":"select","currentValue":"a","options":[{"value":"a","name":"Default"},{"group":"other","name":"More models","options":[{"value":"b","name":"Selected"}]}]}]}}
{"jsonrpc":"2.0","id":3,"result":{}}
{"jsonrpc":"2.0","method":"session/request_permission","id":"rule","params":{"options":[{"optionId":"allow-once","kind":"allow_once"},{"optionId":"no","kind":"reject_once"}]}}
{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"thinking"}}}}
{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"first"}}}}
{"jsonrpc":"2.0","id":4,"result":{"stopReason":"end_turn"}}
{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"second"}}}}
{"jsonrpc":"2.0","id":5,"result":{"stopReason":"end_turn"}}
"#;
        let (events, received) = channel();
        let (prompts, queue) = channel();
        prompts.send(vec![
            json!({"type":"text", "text":"What do you see?"}),
            json!({"type":"image", "source":{"media_type":"image/jpeg", "data":"aW1hZ2U="}}),
        ]).unwrap();
        prompts.send(vec![json!({"type":"text", "text":"What document is this?"}),
            json!({"type":"document", "source":{"media_type":"application/pdf", "data":"cGRm"}}),
        ]).unwrap();
        drop(prompts);
        let mut acp = Acp::new(&transcript[..], Vec::new(), 42, events);
        acp.run(Path::new("test"), "b", None, Some("s1"), queue).unwrap();

        let request: Vec<Value> = acp.writer.split(|&byte| byte == b'\n')
            .filter(|line| !line.is_empty()).map(|line| serde_json::from_slice(line).unwrap()).collect();
        assert_eq!(request[1]["method"], "session/resume");
        assert_eq!(request[1]["params"]["systemPrompt"]["mode"], "append");
        assert_eq!(request[2]["method"], "session/set_config_option");
        assert_eq!(request[2]["params"]["configId"], "model-choice");
        assert_eq!(request[2]["params"]["value"], "b");
        assert_eq!(request[3]["params"]["sessionId"], "s1");
        assert_eq!(
            request[3]["params"]["prompt"][1],
            json!({"type":"image", "mimeType":"image/jpeg", "data":"aW1hZ2U="})
        );
        assert_eq!(request[4]["result"]["outcome"]["optionId"], "no");
        assert_eq!(request[5]["params"]["sessionId"], "s1");
        assert_eq!(request[5]["params"]["prompt"][1]["resource"]["blob"], "cGRm");

        assert!(matches!(received.try_recv().unwrap(), Event::Model(42, ModelEvent::Models(models))
            if models.len() == 2 && models[1].id == "b"));
        assert!(matches!(received.try_recv().unwrap(), Event::Model(42, ModelEvent::Session(id)) if id == "s1"));
        for expected in ["first", "second"] {
            assert!(matches!(received.try_recv().unwrap(), Event::Model(42, ModelEvent::Delta(text)) if text == expected));
            assert!(matches!(received.try_recv().unwrap(), Event::Model(42, ModelEvent::Done(text)) if text == expected));
        }
        assert!(received.try_recv().is_err());
    }
}
