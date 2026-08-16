use std::{env, process::Stdio, time::Duration};

use lavis::external_modules::protocol::{
    MessageEvent, MessageEventKind, V6CallError, V6InboundFrame, V6OutboundCoreFrame,
    parse_v6_inbound_frame,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
};

const CONTRACT: &str = include_str!("../../protocol/v6/alpha-contract.json");
const RESPONSE_DEADLINE: Duration = Duration::from_secs(5);
const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(5);

#[derive(Default)]
struct ObservedCalls {
    curated: bool,
    raw: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Profile {
    /// Protocol and lifecycle conformance only. A correct v6 module passes
    /// regardless of which Telegram methods it calls; `raw.invoke` is not
    /// required because `telegram.raw` is an opt-in high-risk capability.
    Base,
    /// Full RPC capability profile: additionally requires the module to
    /// exercise at least one curated helper and at least one `raw.invoke`
    /// call during the transcript.
    Full,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut arguments = env::args_os().skip(1);
    let mut profile = Profile::Base;
    let program = loop {
        let Some(argument) = arguments.next() else {
            anyhow::bail!(
                "usage: lavis-v6-conformance [--profile base|full] <executable> [arguments...]"
            );
        };
        if argument == "--profile" {
            let value = arguments
                .next()
                .ok_or_else(|| anyhow::anyhow!("--profile requires a value"))?;
            profile = match value.to_str() {
                Some("base") => Profile::Base,
                Some("full") => Profile::Full,
                _ => anyhow::bail!("--profile must be base or full"),
            };
        } else {
            break argument;
        }
    };
    let contract: serde_json::Value = serde_json::from_str(CONTRACT)?;
    if contract["schema_version"] != 1 || contract["protocol_version"] != 6 {
        anyhow::bail!("embedded v6 contract is unsupported");
    }

    let mut child = Command::new(program)
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("missing child stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("missing child stdout"))?;
    let mut writer = stdin;
    let mut reader = BufReader::new(stdout).lines();
    let mut observed = ObservedCalls::default();

    // The mandatory lifecycle transcript is initialize, execute, event,
    // health, then shutdown (docs/module-api-v6.md).
    drive_lifecycle(
        &mut writer,
        &mut reader,
        V6OutboundCoreFrame::Initialize {
            request_id: "1".to_owned(),
            module_id: "conformance".to_owned(),
        },
        "initialized",
        "1",
        &mut observed,
    )
    .await?;
    drive_lifecycle(
        &mut writer,
        &mut reader,
        V6OutboundCoreFrame::Execute {
            request_id: "2".to_owned(),
            command: "conformance".to_owned(),
            arguments: String::new(),
            argument_entities: vec![],
        },
        "result",
        "2",
        &mut observed,
    )
    .await?;
    drive_lifecycle(
        &mut writer,
        &mut reader,
        V6OutboundCoreFrame::Event {
            request_id: "3".to_owned(),
            event: MessageEventKind::Created,
            payload: MessageEvent {
                event_id: "event-1".to_owned(),
                message_ref: "message-1".to_owned(),
                message_key: "message-1".to_owned(),
                peer_id: None,
                text: String::new(),
                outgoing: false,
                entities: vec![],
            },
        },
        "event_result",
        "3",
        &mut observed,
    )
    .await?;
    drive_lifecycle(
        &mut writer,
        &mut reader,
        V6OutboundCoreFrame::Health {
            request_id: "4".to_owned(),
        },
        "health",
        "4",
        &mut observed,
    )
    .await?;
    if profile == Profile::Full && (!observed.curated || !observed.raw) {
        anyhow::bail!("full RPC capability profile requires curated and raw.invoke calls");
    }
    let shutdown = V6OutboundCoreFrame::Shutdown {
        request_id: "5".to_owned(),
    }
    .serialize()?;
    writer.write_all(shutdown.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    let status = match tokio::time::timeout(SHUTDOWN_DEADLINE, child.wait()).await {
        Ok(status) => status?,
        Err(_) => {
            // `kill()` is async: it sends SIGKILL and reaps the child. The
            // child must be killed before bailing so the runner never hangs
            // on a module that ignores the shutdown frame.
            let _ = child.kill().await;
            anyhow::bail!("module did not exit within the shutdown deadline");
        }
    };
    if !status.success() {
        anyhow::bail!("module exited unsuccessfully during shutdown");
    }
    println!("v6 alpha conformance: passed");
    Ok(())
}

async fn drive_lifecycle(
    writer: &mut tokio::process::ChildStdin,
    reader: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    frame: V6OutboundCoreFrame,
    expected: &str,
    request_id: &str,
    observed: &mut ObservedCalls,
) -> anyhow::Result<()> {
    let line = frame.serialize()?;
    writer.write_all(line.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    loop {
        let line = tokio::time::timeout(RESPONSE_DEADLINE, reader.next_line())
            .await
            .map_err(|_| anyhow::anyhow!("module response deadline exceeded"))??
            .ok_or_else(|| anyhow::anyhow!("module closed stdout"))?;
        match parse_v6_inbound_frame(&line)? {
            V6InboundFrame::TelegramInvoke(call) => {
                let (call_id, method) = match call {
                    lavis::external_modules::protocol::V6ModuleFrame::TelegramInvoke {
                        call_id,
                        method,
                        ..
                    } => (call_id, method),
                };
                let result: Result<serde_json::Value, V6CallError> = if method == "raw.invoke" {
                    observed.raw = true;
                    Ok(
                        serde_json::json!({"kind":"raw_tl","dc_id":1,"body_base64_chunks":["eFY0Eg=="]}),
                    )
                } else {
                    observed.curated = true;
                    Ok(serde_json::json!({}))
                };
                let response =
                    V6OutboundCoreFrame::TelegramResult { call_id, result }.serialize()?;
                writer.write_all(response.as_bytes()).await?;
                writer.write_all(b"\n").await?;
                writer.flush().await?;
            }
            V6InboundFrame::Initialized {
                request_id: actual,
                module_id,
            } if expected == "initialized"
                && actual == request_id
                && module_id == "conformance" =>
            {
                return Ok(());
            }
            V6InboundFrame::Health { request_id: actual }
                if expected == "health" && actual == request_id =>
            {
                return Ok(());
            }
            V6InboundFrame::Log {
                request_id: actual, ..
            } if actual == request_id => continue,
            V6InboundFrame::Result {
                request_id: actual, ..
            } if expected == "result" && actual == request_id => return Ok(()),
            V6InboundFrame::EventResult {
                request_id: actual, ..
            } if expected == "event_result" && actual == request_id => return Ok(()),
            _ => anyhow::bail!("unexpected v6 lifecycle transcript frame"),
        }
    }
}