//! Line-delimited JSON-RPC 2.0. Each TCP client owns one editing session.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};

use serde_json::{Value, json};
use uvcraft_commands::{Command, Outcome, Session};

pub fn serve(bind: &str) -> Result<(), String> {
    let address = uvcraft_bridge::loopback_addr(bind)?;
    let listener = TcpListener::bind(address).map_err(|e| format!("bind {bind}: {e}"))?;
    eprintln!(
        "UvCraft JSON-RPC listening on {}",
        listener.local_addr().map_err(|e| e.to_string())?
    );
    for connection in listener.incoming() {
        match connection {
            Ok(stream) => {
                std::thread::spawn(move || {
                    if let Err(e) = serve_client(stream) {
                        eprintln!("client error: {e}");
                    }
                });
            }
            Err(e) => eprintln!("accept error: {e}"),
        }
    }
    Ok(())
}

/// Largest accepted request line. Large edge/face lists fit comfortably; an
/// endless line without a newline can no longer exhaust the server's memory.
const MAX_REQUEST_BYTES: u64 = 64 * 1024 * 1024;

fn serve_client(stream: TcpStream) -> Result<(), std::io::Error> {
    let reader = BufReader::new(stream.try_clone()?);
    serve_lines(reader, stream, MAX_REQUEST_BYTES)
}

fn serve_lines(
    mut reader: impl BufRead,
    mut writer: impl Write,
    max_bytes: u64,
) -> Result<(), std::io::Error> {
    let mut session = Session::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = std::io::Read::take(&mut reader, max_bytes + 1).read_until(b'\n', &mut line)?;
        if read == 0 {
            return Ok(());
        }
        if read as u64 > max_bytes {
            let response = error(
                Value::Null,
                -32600,
                format!("request exceeds {max_bytes} bytes"),
            );
            writeln!(writer, "{response}")?;
            return Ok(());
        }
        let response = match std::str::from_utf8(&line) {
            Ok(text) => handle_line(&mut session, text),
            Err(_) => Some(error(Value::Null, -32700, "request is not valid UTF-8")),
        };
        if let Some(response) = response {
            writeln!(writer, "{response}")?;
        }
    }
}

fn error(id: Value, code: i32, message: impl AsRef<str>) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message.as_ref()}})
}

fn handle_line(session: &mut Session, line: &str) -> Option<Value> {
    let req: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return Some(error(Value::Null, -32700, e.to_string())),
    };
    if let Value::Array(batch) = req {
        if batch.is_empty() {
            return Some(error(Value::Null, -32600, "empty batch"));
        }
        let responses: Vec<_> = batch
            .into_iter()
            .filter_map(|request| handle_request(session, request))
            .collect();
        return (!responses.is_empty()).then_some(Value::Array(responses));
    }
    handle_request(session, req)
}

fn handle_request(session: &mut Session, req: Value) -> Option<Value> {
    let Some(obj) = req.as_object() else {
        return Some(error(Value::Null, -32600, "request must be an object"));
    };
    let id = obj.get("id").cloned().unwrap_or(Value::Null);
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
        || !obj.get("method").is_some_and(Value::is_string)
        || obj
            .get("id")
            .is_some_and(|v| !v.is_null() && !v.is_string() && !v.is_number())
    {
        return Some(error(id, -32600, "invalid JSON-RPC request"));
    }
    let method = obj["method"].as_str().expect("checked above");
    let notification = !obj.contains_key("id");
    let request_error =
        |code, message: String| (!notification).then(|| error(id.clone(), code, message));
    let result = match method {
        "execute" => {
            let Some(command) = obj.get("params").and_then(|p| p.get("command")) else {
                return request_error(-32602, "params.command is required".into());
            };
            let cmd: Command = match serde_json::from_value(command.clone()) {
                Ok(c) => c,
                Err(e) => return request_error(-32602, e.to_string()),
            };
            match session.execute(cmd) {
                Ok(Outcome::Done) => json!({"status":"done"}),
                Ok(Outcome::Info(message)) => json!({"status":"info","message":message}),
                Ok(Outcome::Selection { islands }) => {
                    json!({"status":"selection","islands":islands})
                }
                Ok(Outcome::FaceSelection { faces }) => {
                    json!({"status":"face_selection","faces":faces})
                }
                Ok(Outcome::Report(report)) => json!({"status":"report","report":report}),
                Ok(Outcome::PhysicalReport(report)) => {
                    json!({"status":"physical_report","report":report})
                }
                Err(e) => return request_error(-32000, e.to_string()),
            }
        }
        "status" => {
            json!({"loaded":session.document().is_some(),"can_undo":session.can_undo(),"can_redo":session.can_redo()})
        }
        "log" => json!(session.log()),
        _ => return request_error(-32601, "method not found".into()),
    };
    if obj.contains_key("id") {
        Some(json!({"jsonrpc":"2.0","id":id,"result":result}))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn non_loopback_addresses_are_rejected() {
        for bind in ["0.0.0.0:0", "192.0.2.1:0", "[::]:0"] {
            let error = serve(bind).unwrap_err();
            assert!(error.contains("loopback"), "{bind}: {error}");
        }
    }

    #[test]
    fn oversized_request_is_rejected_and_closes_the_connection() {
        let input = format!(
            "{}\n{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"status\"}}\n",
            "x".repeat(100)
        );
        let mut out = Vec::new();
        serve_lines(input.as_bytes(), &mut out, 64).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.lines().count(), 1, "{text}");
        assert!(text.contains("exceeds 64 bytes"), "{text}");

        let mut out = Vec::new();
        serve_lines(&b"\xff\xfe\n"[..], &mut out, 64).unwrap();
        assert!(String::from_utf8(out).unwrap().contains("-32700"));
    }

    #[test]
    fn rpc_protocol_and_session_errors() {
        let mut session = Session::new();
        assert_eq!(
            handle_line(&mut session, "not json").unwrap()["error"]["code"],
            -32700
        );
        assert_eq!(
            handle_line(&mut session, r#"{"jsonrpc":"2.0","id":1,"method":"bogus"}"#).unwrap()["error"]
                ["code"],
            -32601
        );
        assert_eq!(handle_line(&mut session, r#"{"jsonrpc":"2.0","id":2,"method":"execute","params":{"command":{"cmd":"bogus"}}}"#).unwrap()["error"]["code"], -32602);
        assert_eq!(
            handle_line(
                &mut session,
                r#"{"jsonrpc":"2.0","id":3,"method":"execute","params":{"command":{"cmd":"undo"}}}"#
            )
            .unwrap()["error"]["code"],
            -32000
        );
        assert_eq!(
            handle_line(
                &mut session,
                r#"{"jsonrpc":"2.0","id":4,"method":"status"}"#
            )
            .unwrap()["result"]["loaded"],
            false
        );
        assert!(handle_line(&mut session, r#"{"jsonrpc":"2.0","method":"status"}"#).is_none());
        assert!(handle_line(&mut session, r#"{"jsonrpc":"2.0","method":"bogus"}"#).is_none());
        let batch = r#"[{"jsonrpc":"2.0","id":6,"method":"status"},{"jsonrpc":"2.0","method":"status"},{"jsonrpc":"2.0","id":7,"method":"log"}]"#;
        let responses = handle_line(&mut session, batch).unwrap();
        assert_eq!(responses.as_array().unwrap().len(), 2);
        assert_eq!(responses[0]["id"], 6);
        assert_eq!(responses[1]["id"], 7);

        let mesh = std::env::temp_dir().join(format!(
            "uvcraft-rpc-{}-{:?}.obj",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&mesh, "v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3\n").unwrap();
        let request = json!({"jsonrpc":"2.0","id":5,"method":"execute","params":{"command":{"cmd":"load","path":mesh}}});
        let response = handle_line(&mut session, &request.to_string()).unwrap();
        assert_eq!(response["result"]["status"], "done");
        assert!(session.document().is_some());
        std::fs::remove_file(mesh).unwrap();
    }

    #[test]
    fn tcp_connection_keeps_session_between_requests() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let worker = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            serve_client(stream).unwrap();
        });
        let mut stream = TcpStream::connect(address).unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        writeln!(
            stream,
            "{}",
            json!({"jsonrpc":"2.0","id":1,"method":"status"})
        )
        .unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let first: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(first["result"]["loaded"], false);
        line.clear();
        writeln!(stream, "{}", json!({"jsonrpc":"2.0","id":2,"method":"log"})).unwrap();
        reader.read_line(&mut line).unwrap();
        let second: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(second["result"], json!([]));
        drop(reader);
        drop(stream);
        worker.join().unwrap();
    }
}
