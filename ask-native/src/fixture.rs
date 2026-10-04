//! Deterministic in-process fixture ACP server. No tool or filesystem execution.
use serde_json::{Value, json};
use std::io::{self, BufRead, Write};
pub fn serve() -> io::Result<()> {
    let stdin = io::stdin();
    let mut output = io::stdout().lock();
    let mut prompt: Option<Value> = None;
    let mut serial = 0u64;
    let send = |out: &mut io::StdoutLock<'_>, v: Value| -> io::Result<()> {
        serde_json::to_writer(&mut *out, &v)?;
        out.write_all(b"\n")?;
        out.flush()
    };
    for line in stdin.lock().lines() {
        let line = line?;
        if line.len() > 1024 * 1024 {
            return Err(io::Error::other("fixture input limit"));
        }
        let m: Value = serde_json::from_str(&line)?;
        let result = match m["method"].as_str() {
            Some("initialize") => Some(
                json!({"protocolVersion":1,"agentCapabilities":{"sessionCapabilities":{"close":{}}}}),
            ),
            Some("session/new") => Some(json!({"sessionId":"native-fixture"})),
            Some("session/prompt") => {
                serial += 1;
                prompt = Some(m["id"].clone());
                let mut options = vec![json!({"optionId":"fixture-allow","kind":"allow_once"})];
                if !m["params"]["prompt"].to_string().contains("cancel-only") {
                    options.push(json!({"optionId":"fixture-deny","kind":"reject_once"}));
                }
                send(
                    &mut output,
                    json!({"jsonrpc":"2.0","id":format!("permission-{serial}"),"method":"session/request_permission","params":{"sessionId":"native-fixture","toolCall":{"title":"Read the selected fixture resource"},"options":options}}),
                )?;
                None
            }
            Some("session/cancel") => {
                if let Some(id) = prompt.take() {
                    send(
                        &mut output,
                        json!({"jsonrpc":"2.0","id":id,"result":{"stopReason":"cancelled"}}),
                    )?
                }
                None
            }
            Some("session/close") => {
                send(
                    &mut output,
                    json!({"jsonrpc":"2.0","id":m["id"],"result":{}}),
                )?;
                break;
            }
            None if m.get("result").is_some() => {
                if let Some(id) = prompt.take() {
                    let allowed = m
                        .pointer("/result/outcome/optionId")
                        .and_then(Value::as_str)
                        == Some("fixture-allow");
                    let text = if allowed {
                        "Fixture completed · Unicode 日本語 😀"
                    } else if m.pointer("/result/outcome/outcome").and_then(Value::as_str)
                        == Some("cancelled")
                    {
                        "Tool cancelled; no option selected."
                    } else {
                        "Tool declined; no resource was read."
                    };
                    send(
                        &mut output,
                        json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"native-fixture","update":{"sessionUpdate":"agent_message_chunk","messageId":format!("reply-{serial}"),"content":{"type":"text","text":text}}}}),
                    )?;
                    send(
                        &mut output,
                        json!({"jsonrpc":"2.0","id":id,"result":{"stopReason":"end_turn"}}),
                    )?
                }
                None
            }
            _ => None,
        };
        if let Some(result) = result {
            send(
                &mut output,
                json!({"jsonrpc":"2.0","id":m["id"],"result":result}),
            )?
        }
    }
    Ok(())
}
