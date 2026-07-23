//! Minimal "look but don't re-encode" message inspection.
//!
//! Passthrough must be byte-transparent: parse to `serde_json::Value` only
//! to inspect, forward the original bytes. See `docs/design/dap-proxy.md` §3.2.

use anyhow::{Context, Result};
use serde_json::{json, Value};

/// Proxy-generated requests use a private seq namespace starting here.
/// Client (Zed) seqs stay below this; responses from lldb-dap with
/// `request_seq >= SEQ_BASE` answer proxy-internal requests.
pub const SEQ_BASE: i64 = 1_000_000;

/// Classification of a message arriving from the client (Zed).
pub enum ClientMsg<'a> {
    Initialize {
        raw: &'a [u8],
    },
    /// `launch` is intercepted — its `arguments` is OUR scenario config
    /// (the raw frame is never forwarded).
    Launch {
        seq: i64,
        args: Value,
    },
    /// `seq` is kept so the proxy can answer the disconnect itself when it
    /// cancels a mid-build pipeline (no lldb-dap roundtrip).
    Disconnect {
        seq: i64,
        raw: &'a [u8],
    },
    Other {
        raw: &'a [u8],
    },
}

/// Classification of a message arriving from the lldb-dap child.
pub enum ChildMsg<'a> {
    /// Response to a proxy-internal request (`request_seq >= SEQ_BASE`).
    /// The attach response gets rewritten onto the client's launch seq
    /// ([`rewrite_attach_response`]); all others are dropped.
    InternalResponse {
        request_seq: i64,
        raw: &'a [u8],
    },
    Other {
        raw: &'a [u8],
    },
}

/// Classify one raw client frame.
pub fn classify_client(raw: &[u8]) -> Result<ClientMsg<'_>> {
    let v: Value = serde_json::from_slice(raw).context("client frame is not valid JSON")?;
    if v.get("type").and_then(Value::as_str) == Some("request") {
        match v.get("command").and_then(Value::as_str) {
            Some("initialize") => return Ok(ClientMsg::Initialize { raw }),
            Some("launch") => {
                let seq = v.get("seq").and_then(Value::as_i64).unwrap_or(0);
                let args = v.get("arguments").cloned().unwrap_or(Value::Null);
                return Ok(ClientMsg::Launch { seq, args });
            }
            Some("disconnect") => {
                let seq = v.get("seq").and_then(Value::as_i64).unwrap_or(0);
                return Ok(ClientMsg::Disconnect { seq, raw });
            }
            _ => {}
        }
    }
    Ok(ClientMsg::Other { raw })
}

/// Classify one raw lldb-dap frame.
pub fn classify_child(raw: &[u8]) -> Result<ChildMsg<'_>> {
    let v: Value = serde_json::from_slice(raw).context("child frame is not valid JSON")?;
    if v.get("type").and_then(Value::as_str) == Some("response") {
        if let Some(request_seq) = v.get("request_seq").and_then(Value::as_i64) {
            if request_seq >= SEQ_BASE {
                return Ok(ChildMsg::InternalResponse { request_seq, raw });
            }
        }
    }
    Ok(ChildMsg::Other { raw })
}

/// Compact one-line summary of a DAP frame for DEBUG logging: type,
/// command/event, seq, request_seq, success. Never includes the body
/// (full bodies are TRACE-only, truncated — see `util::logging`).
pub fn summarize(raw: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(raw) else {
        return format!("unparseable frame ({} bytes)", raw.len());
    };
    let mut s = v
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("?")
        .to_string();
    if let Some(name) = v
        .get("command")
        .and_then(Value::as_str)
        .or_else(|| v.get("event").and_then(Value::as_str))
    {
        s.push(' ');
        s.push_str(name);
    }
    if let Some(seq) = v.get("seq").and_then(Value::as_i64) {
        s.push_str(&format!(" seq={seq}"));
    }
    if let Some(request_seq) = v.get("request_seq").and_then(Value::as_i64) {
        s.push_str(&format!(" request_seq={request_seq}"));
    }
    if let Some(success) = v.get("success").and_then(Value::as_bool) {
        s.push_str(&format!(" success={success}"));
    }
    s
}

// --- proxy-built message constructors (all return serde_json::Value) ---

/// `output` event with `seq: 0` (proven fine with Zed).
pub fn output_event(category: &str, text: &str) -> Value {
    json!({
        "type": "event",
        "seq": 0,
        "event": "output",
        "body": { "category": category, "output": text }
    })
}

/// `evaluate` request with `context: "repl"` (e.g. `platform select ios-simulator`).
pub fn evaluate_repl(expr: &str, seq: i64) -> Value {
    json!({
        "type": "request",
        "seq": seq,
        "command": "evaluate",
        "arguments": { "expression": expr, "context": "repl" }
    })
}

/// Simulator attach is a plain `{"pid": N}` attach request.
pub fn attach_pid(pid: i64, seq: i64) -> Value {
    json!({
        "type": "request",
        "seq": seq,
        "command": "attach",
        "arguments": { "pid": pid }
    })
}

/// Failure response for an intercepted request.
pub fn error_response(request_seq: i64, command: &str, msg: &str) -> Value {
    json!({
        "type": "response",
        "seq": 0,
        "request_seq": request_seq,
        "command": command,
        "success": false,
        "message": msg
    })
}

/// Bare success response for an intercepted request (used to answer a
/// `disconnect` that cancelled a mid-build pipeline).
pub fn success_response(request_seq: i64, command: &str) -> Value {
    json!({
        "type": "response",
        "seq": 0,
        "request_seq": request_seq,
        "command": command,
        "success": true
    })
}

/// `terminated` event.
pub fn terminated_event() -> Value {
    json!({ "type": "event", "seq": 0, "event": "terminated" })
}

/// Classify a DAP end-of-session event from lldb-dap:
/// - `Some(true)`  — `exited`: the debuggee **process** has gone.
/// - `Some(false)` — `terminated`: the debug **session** ended, but the
///   process may still be alive (lldb-dap detaches an attach-by-pid session
///   on disconnect, emitting `terminated` only, leaving the app running).
/// - `None`        — any other frame.
///
/// The proxy needs both: either event means it must own a following
/// `disconnect` (lldb-dap can wedge on it once its debuggee is gone), but
/// only `exited` proves the process is gone — the teardown terminate-skip is
/// gated on that, so a plain `terminated` detach still lets the belt-and-braces
/// `simctl terminate` honor `terminateOnStop`.
pub fn terminal_event(raw: &[u8]) -> Option<bool> {
    let v: Value = serde_json::from_slice(raw).ok()?;
    if v.get("type").and_then(Value::as_str) != Some("event") {
        return None;
    }
    match v.get("event").and_then(Value::as_str) {
        Some("exited") => Some(true),
        Some("terminated") => Some(false),
        _ => None,
    }
}

/// Rewrite lldb-dap's attach response into the client's launch response:
/// `request_seq` -> the client's launch seq and `command` -> `"launch"`
/// (the command rewrite isn't strictly required, but it is spec-correct).
/// Every other field passes through untouched. Returns the rewritten
/// message and whether the attach succeeded.
pub fn rewrite_attach_response(raw: &[u8], launch_seq: i64) -> Result<(Value, bool)> {
    let mut v: Value = serde_json::from_slice(raw).context("attach response is not valid JSON")?;
    let success = v.get("success").and_then(Value::as_bool).unwrap_or(false);
    v["request_seq"] = json!(launch_seq);
    v["command"] = json!("launch");
    Ok((v, success))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_initialize() {
        let raw = br#"{"seq":1,"type":"request","command":"initialize","arguments":{"adapterID":"xcode"}}"#;
        match classify_client(raw).unwrap() {
            ClientMsg::Initialize { raw: r } => assert_eq!(r, raw, "raw must be byte-identical"),
            _ => panic!("expected Initialize"),
        }
    }

    #[test]
    fn classifies_launch_with_seq_and_args() {
        let raw =
            br#"{"seq":3,"type":"request","command":"launch","arguments":{"scheme":"MyApp (staging)"}}"#;
        match classify_client(raw).unwrap() {
            ClientMsg::Launch { seq, args } => {
                assert_eq!(seq, 3);
                assert_eq!(args["scheme"], "MyApp (staging)");
            }
            _ => panic!("expected Launch"),
        }
    }

    #[test]
    fn classifies_disconnect() {
        let raw = br#"{"seq":9,"type":"request","command":"disconnect","arguments":{}}"#;
        match classify_client(raw).unwrap() {
            ClientMsg::Disconnect { seq, raw: r } => {
                assert_eq!(seq, 9);
                assert_eq!(r, raw);
            }
            _ => panic!("expected Disconnect"),
        }
    }

    #[test]
    fn other_request_and_non_request_are_other() {
        let bp = br#"{"seq":4,"type":"request","command":"setBreakpoints","arguments":{}}"#;
        assert!(matches!(
            classify_client(bp).unwrap(),
            ClientMsg::Other { .. }
        ));
        // An event named "launch" must NOT classify as Launch.
        let ev = br#"{"seq":0,"type":"event","command":"launch","event":"output"}"#;
        assert!(matches!(
            classify_client(ev).unwrap(),
            ClientMsg::Other { .. }
        ));
    }

    #[test]
    fn invalid_json_is_error() {
        assert!(classify_client(b"not json").is_err());
        assert!(classify_child(b"not json").is_err());
    }

    #[test]
    fn child_response_below_seq_base_passes_through() {
        // Boundary: 999_999 is still client traffic.
        let raw =
            br#"{"seq":2,"type":"response","request_seq":999999,"command":"next","success":true}"#;
        assert!(matches!(
            classify_child(raw).unwrap(),
            ChildMsg::Other { .. }
        ));
    }

    #[test]
    fn child_response_at_seq_base_is_internal() {
        let raw = br#"{"seq":2,"type":"response","request_seq":1000000,"command":"attach","success":true}"#;
        match classify_child(raw).unwrap() {
            ChildMsg::InternalResponse {
                request_seq,
                raw: r,
            } => {
                assert_eq!(request_seq, SEQ_BASE);
                assert_eq!(r, raw);
            }
            _ => panic!("expected InternalResponse"),
        }
    }

    #[test]
    fn child_event_with_big_seq_is_not_internal() {
        // Only *responses* with request_seq >= SEQ_BASE are internal.
        let raw = br#"{"seq":1000005,"type":"event","event":"output","body":{}}"#;
        assert!(matches!(
            classify_child(raw).unwrap(),
            ChildMsg::Other { .. }
        ));
    }

    #[test]
    fn builders_shape() {
        let out = output_event("stdout", "hello\n");
        assert_eq!(out["seq"], 0);
        assert_eq!(out["event"], "output");
        assert_eq!(out["body"]["category"], "stdout");
        assert_eq!(out["body"]["output"], "hello\n");

        let ev = evaluate_repl("platform select ios-simulator", SEQ_BASE);
        assert_eq!(ev["command"], "evaluate");
        assert_eq!(ev["seq"], SEQ_BASE);
        assert_eq!(ev["arguments"]["context"], "repl");

        let at = attach_pid(4242, SEQ_BASE + 1);
        assert_eq!(at["command"], "attach");
        assert_eq!(at["arguments"]["pid"], 4242);
        assert_eq!(at["seq"], SEQ_BASE + 1);

        let err = error_response(7, "launch", "build failed");
        assert_eq!(err["type"], "response");
        assert_eq!(err["request_seq"], 7);
        assert_eq!(err["command"], "launch");
        assert_eq!(err["success"], false);
        assert_eq!(err["message"], "build failed");

        let ok = success_response(11, "disconnect");
        assert_eq!(ok["type"], "response");
        assert_eq!(ok["request_seq"], 11);
        assert_eq!(ok["command"], "disconnect");
        assert_eq!(ok["success"], true);

        let term = terminated_event();
        assert_eq!(term["event"], "terminated");
    }

    #[test]
    fn summarize_extracts_routing_fields_only() {
        assert_eq!(
            summarize(
                r#"{"seq":3,"type":"request","command":"launch","arguments":{"scheme":"S"}}"#
            ),
            "request launch seq=3"
        );
        assert_eq!(
            summarize(
                r#"{"seq":9,"type":"response","request_seq":3,"command":"launch","success":true,"body":{"x":1}}"#
            ),
            "response launch seq=9 request_seq=3 success=true"
        );
        assert_eq!(
            summarize(r#"{"seq":0,"type":"event","event":"output","body":{}}"#),
            "event output seq=0"
        );
        assert_eq!(summarize("not json"), "unparseable frame (8 bytes)");
    }

    #[test]
    fn terminal_event_distinguishes_exited_from_terminated() {
        // `exited` => process gone (Some(true)); `terminated` => session
        // ended but process may be alive (Some(false)). The distinction is
        // load-bearing: lldb-dap detaches an attach session on disconnect,
        // emitting `terminated` only, and the app must still be terminated.
        assert_eq!(
            terminal_event(br#"{"seq":5,"type":"event","event":"exited","body":{"exitCode":0}}"#),
            Some(true)
        );
        assert_eq!(
            terminal_event(br#"{"seq":6,"type":"event","event":"terminated"}"#),
            Some(false)
        );
    }

    #[test]
    fn terminal_event_rejects_other_frames() {
        // Other events (a killed *host* process reports `stopped`, not
        // `terminated`) are not terminal.
        assert_eq!(
            terminal_event(
                br#"{"seq":1,"type":"event","event":"stopped","body":{"reason":"signal"}}"#
            ),
            None
        );
        assert_eq!(
            terminal_event(br#"{"seq":0,"type":"event","event":"output","body":{}}"#),
            None
        );
        // A *response* named terminate is not an event.
        assert_eq!(
            terminal_event(
                br#"{"seq":2,"type":"response","request_seq":1,"command":"terminate","success":true}"#
            ),
            None
        );
        // A request that merely mentions the word is not an event either.
        assert_eq!(
            terminal_event(br#"{"seq":3,"type":"request","command":"terminate"}"#),
            None
        );
        assert_eq!(terminal_event(b"not json"), None);
    }

    #[test]
    fn rewrites_attach_response_onto_launch_seq() {
        let raw = br#"{"seq":42,"type":"response","request_seq":1000001,"command":"attach","success":true,"body":{"extra":"kept"}}"#;
        let (msg, success) = rewrite_attach_response(raw, 3).unwrap();
        assert!(success);
        assert_eq!(msg["request_seq"], 3);
        assert_eq!(msg["command"], "launch");
        // Everything else is untouched.
        assert_eq!(msg["seq"], 42);
        assert_eq!(msg["type"], "response");
        assert_eq!(msg["success"], true);
        assert_eq!(msg["body"]["extra"], "kept");
    }

    #[test]
    fn rewrites_failed_attach_response() {
        let raw = br#"{"seq":42,"type":"response","request_seq":1000001,"command":"attach","success":false,"message":"attach failed"}"#;
        let (msg, success) = rewrite_attach_response(raw, 7).unwrap();
        assert!(!success);
        assert_eq!(msg["request_seq"], 7);
        assert_eq!(msg["command"], "launch");
        assert_eq!(msg["message"], "attach failed");
    }
}
