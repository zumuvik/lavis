use std::io::{self, BufRead, Write};

const PROTOCOL_VERSION: u32 = 6;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    for line in io::stdin().lock().lines() {
        let line = line?;
        let val: serde_json::Value = serde_json::from_str(&line)?;
        let proto = val
            .get("protocol_version")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        if proto != PROTOCOL_VERSION as u64 {
            let rid = val.get("request_id").and_then(|v| v.as_str()).unwrap_or("?");
            send_error(rid, "protocol_version_mismatch", "Expected protocol v6");
            continue;
        }
        let msg_type = val.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let rid = val.get("request_id").and_then(|v| v.as_str()).unwrap_or("?");
        match msg_type {
            "initialize" => {
                let mid = val.get("module_id").and_then(|v| v.as_str()).unwrap_or("");
                send(&serde_json::json!({
                    "type": "initialized",
                    "protocol_version": PROTOCOL_VERSION,
                    "request_id": rid,
                    "module_id": mid,
                }));
            }
            "execute" => {
                let cmd = val.get("command").and_then(|v| v.as_str()).unwrap_or("");
                let args = val.get("arguments").and_then(|v| v.as_str()).unwrap_or("");
                if cmd == "echo" {
                    send(&serde_json::json!({
                        "type": "result",
                        "protocol_version": PROTOCOL_VERSION,
                        "request_id": rid,
                        "text": args,
                    }));
                } else {
                    send_error(rid, "unknown_command", &format!("Unknown command: {cmd}"));
                }
            }
            "health" => {
                send(&serde_json::json!({
                    "type": "health",
                    "protocol_version": PROTOCOL_VERSION,
                    "request_id": rid,
                }));
            }
            "shutdown" => {
                break;
            }
            _ => {
                send_error(rid, "unknown_message", &format!("Unknown type: {msg_type}"));
            }
        }
    }
    Ok(())
}

fn send(msg: &serde_json::Value) {
    let line = serde_json::to_string(msg).unwrap();
    let mut out = io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

fn send_error(request_id: &str, code: &str, message: &str) {
    send(&serde_json::json!({
        "type": "error",
        "protocol_version": PROTOCOL_VERSION,
        "request_id": request_id,
        "code": code,
        "message": message,
    }));
}
