//! UvCraft for Python.
//!
//! The Python module (`import uvcraft`, built with `maturin` and the `python`
//! feature) wraps a [`Session`]: every call becomes a [`Command`] run through
//! [`Session::execute`], exactly like the CLI and the GUI, so undo/redo, the
//! command log and macros work the same. Nothing here calls the algorithms
//! directly.
//!
//! This file holds the Python-free part (JSON in, JSON out) so it is tested
//! by plain `cargo test`; `python.rs` only converts between Python objects
//! and JSON.

use serde_json::{Map, Value, json};
use uvcraft_commands::{Command, CommandError, Outcome, ScriptError, Session, parse_script};
use uvcraft_core::Islands;

#[cfg(feature = "python")]
mod python;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// The command is not valid JSON or not a known command.
    #[error("invalid command: {0}")]
    InvalidCommand(#[from] serde_json::Error),
    #[error(transparent)]
    Script(#[from] ScriptError),
    #[error(transparent)]
    Command(#[from] CommandError),
    #[error("{path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
}

/// Builds a command object `{"cmd": name, ...args}`; `null` arguments are
/// dropped so that the command's defaults apply.
pub fn command_value(name: &str, args: impl IntoIterator<Item = (String, Value)>) -> Value {
    let mut map = Map::new();
    map.insert("cmd".into(), Value::String(name.into()));
    map.extend(args.into_iter().filter(|(_, v)| !v.is_null()));
    Value::Object(map)
}

/// Runs one command given as JSON and describes the outcome as JSON:
/// `{"cmd": name, "status": "done"}`, `{"status": "info", "message": ..}` or
/// `{"status": "report", "report": {..}}`.
pub fn execute_value(session: &mut Session, cmd: Value) -> Result<Value, ApiError> {
    let cmd: Command = serde_json::from_value(cmd)?;
    execute(session, cmd)
}

/// [`execute_value`] for a JSON string.
pub fn execute_json(session: &mut Session, cmd: &str) -> Result<Value, ApiError> {
    let cmd: Command = serde_json::from_str(cmd)?;
    execute(session, cmd)
}

/// Runs a whole script (JSON Lines or a JSON array) and returns one result
/// per command. Stops at the first failing command.
pub fn run_script(session: &mut Session, text: &str) -> Result<Vec<Value>, ApiError> {
    parse_script(text)?
        .into_iter()
        .map(|cmd| execute(session, cmd))
        .collect()
}

/// [`run_script`] for a script file (e.g. a saved macro).
pub fn run_script_file(session: &mut Session, path: &str) -> Result<Vec<Value>, ApiError> {
    let text = std::fs::read_to_string(path).map_err(|source| ApiError::Io {
        path: path.into(),
        source,
    })?;
    run_script(session, &text)
}

fn execute(session: &mut Session, cmd: Command) -> Result<Value, ApiError> {
    let name = cmd.name();
    Ok(match session.execute(cmd)? {
        Outcome::Done => json!({"cmd": name, "status": "done"}),
        Outcome::Info(message) => json!({"cmd": name, "status": "info", "message": message}),
        Outcome::Report(report) => json!({"cmd": name, "status": "report", "report": report}),
        Outcome::PhysicalReport(report) => {
            json!({"cmd": name, "status": "physical_report", "report": report})
        }
        Outcome::Selection { islands } => {
            json!({"cmd": name, "status": "selection", "islands": islands})
        }
        Outcome::FaceSelection { faces } => {
            json!({"cmd": name, "status": "face_selection", "faces": faces})
        }
    })
}

/// Commands as JSON objects.
pub fn commands_value(commands: &[Command]) -> Vec<Value> {
    commands
        .iter()
        .map(|c| serde_json::to_value(c).expect("commands always serialize"))
        .collect()
}

/// The faces of each island, in island order (the numbering commands use).
pub fn island_faces(session: &Session) -> Result<Vec<Vec<usize>>, ApiError> {
    let doc = session.document().ok_or(CommandError::NoDocument)?;
    let islands = Islands::compute(&doc.mesh, doc.uv());
    Ok((0..islands.count())
        .map(|i| islands.faces(i).iter().map(|&f| f as usize).collect())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const QUADS: &str = "v 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\nv 2 0 0\nv 2 1 0\n\
vt 0 0\nvt 0.5 0\nvt 0.5 1\nvt 0 1\nvt 0.5 0\nvt 1 0\nvt 1 1\nvt 0.5 1\n\
f 1/1 2/2 3/3 4/4\nf 2/5 5/6 6/7 3/8\n";

    struct TempDir(PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn loaded() -> (Session, TempDir) {
        let dir = std::env::temp_dir().join(format!(
            "uvcraft-py-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("in.obj");
        std::fs::write(&path, QUADS).unwrap();
        let mut s = Session::new();
        let load = command_value("load", [("path".into(), json!(path))]);
        assert_eq!(
            execute_value(&mut s, load).unwrap(),
            json!({"cmd": "load", "status": "done"})
        );
        (s, TempDir(dir))
    }

    #[test]
    fn executes_json_and_reports() {
        let (mut s, _dir) = loaded();
        let r = execute_json(&mut s, r#"{"cmd":"analyze","resolution":512}"#).unwrap();
        assert_eq!(r["status"], "report");
        assert_eq!(r["report"]["island_count"], 2);
        assert_eq!(island_faces(&s).unwrap(), vec![vec![0], vec![1]]);

        // Null arguments fall back to the command's defaults.
        let pack = command_value(
            "pack",
            [
                ("islands".into(), Value::Null),
                ("resolution".into(), json!(256)),
            ],
        );
        assert_eq!(execute_value(&mut s, pack).unwrap()["status"], "done");
        let log = commands_value(s.log());
        assert_eq!(log.len(), 3);
        assert_eq!(log[2]["cmd"], "pack");
        assert_eq!(log[2]["padding_px"], 4.0);
    }

    #[test]
    fn errors_are_typed() {
        let mut s = Session::new();
        assert!(matches!(
            execute_json(&mut s, r#"{"cmd":"undo"}"#),
            Err(ApiError::Command(CommandError::NothingToUndo))
        ));
        assert!(matches!(
            execute_json(&mut s, r#"{"cmd":"bogus"}"#),
            Err(ApiError::InvalidCommand(_))
        ));
        assert!(matches!(
            run_script(&mut s, "{\"cmd\":\"undo\"}\nnot json"),
            Err(ApiError::Script(_))
        ));
        assert!(matches!(
            island_faces(&s),
            Err(ApiError::Command(CommandError::NoDocument))
        ));
    }

    #[test]
    fn replays_a_saved_macro() {
        let (mut s, dir) = loaded();
        let results = run_script(
            &mut s,
            r#"[{"cmd":"transform","islands":[1],"translate":[0,1]},{"cmd":"undo"},
                {"cmd":"transform","rotate_deg":90}]"#,
        )
        .unwrap();
        assert_eq!(results.len(), 3);
        let path = dir.0.join("m.jsonl");
        execute_value(
            &mut s,
            command_value("save_macro", [("path".into(), json!(path))]),
        )
        .unwrap();

        let mut other = Session::new();
        let load = command_value("load", [("path".into(), json!(dir.0.join("in.obj")))]);
        execute_value(&mut other, load).unwrap();
        let replayed = run_script_file(&mut other, path.to_str().unwrap()).unwrap();
        assert_eq!(replayed.len(), 1);
        assert_eq!(other.document().unwrap().uv(), s.document().unwrap().uv());
    }
}
