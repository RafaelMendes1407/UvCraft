//! Macro recording: the session's edit history as a replayable script.
//!
//! The raw command log ([`Session::log`]) records everything, including
//! loads, saves, reports, failed-then-undone edits and the undo/redo commands
//! themselves. A *macro* is the clean version: the edit commands behind the
//! current undo stack, in order. Undone edits are dropped (and come back on
//! redo), loads reset it, and commands that change nothing (save, analyze,
//! no-op edits) are left out. Replaying a macro after loading a mesh
//! reproduces the current UVs, so it can be applied to other meshes too.

use crate::{Command, Session};

impl Session {
    /// Keeps `history`/`undone` aligned with the undo/redo stacks after `cmd`
    /// succeeded. `undo_depth` is the undo stack size before it ran.
    pub(crate) fn record_history(&mut self, cmd: &Command, undo_depth: usize) {
        match cmd {
            Command::Load { .. } => {
                self.history.clear();
                self.undone.clear();
            }
            Command::Undo => self.undone.extend(self.history.pop()),
            Command::Redo => self.history.extend(self.undone.pop()),
            _ if self.undo.len() > undo_depth => {
                self.history.push(cmd.clone());
                self.undone.clear();
            }
            _ => {}
        }
        debug_assert_eq!(self.history.len(), self.undo.len());
        debug_assert_eq!(self.undone.len(), self.redo.len());
    }

    /// The edit commands that produced the current UVs since the last load.
    pub fn macro_commands(&self) -> &[Command] {
        &self.history
    }

    /// [`Self::macro_commands`] as a JSON Lines script (runs with `uvcraft run`
    /// once a `load` line is prepended, or with `run` after a load in the GUI/Python).
    pub fn macro_jsonl(&self) -> String {
        let mut out = String::from("# UvCraft macro: load a mesh first, then replay\n");
        for c in &self.history {
            out += &serde_json::to_string(c).expect("commands always serialize");
            out.push('\n');
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::{Command, Session, parse_script};

    const QUADS: &str = "v 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\nv 2 0 0\nv 2 1 0\n\
vt 0 0\nvt 0.5 0\nvt 0.5 1\nvt 0 1\nvt 0.5 0\nvt 1 0\nvt 1 1\nvt 0.5 1\n\
f 1/1 2/2 3/3 4/4\nf 2/5 5/6 6/7 3/8\n";

    struct TempDir(PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp_dir() -> TempDir {
        let dir = std::env::temp_dir().join(format!(
            "uvcraft-macro-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn run(s: &mut Session, script: &str) {
        for cmd in parse_script(script).unwrap() {
            s.execute(cmd).unwrap();
        }
    }

    #[test]
    fn macro_drops_undone_and_non_edits() {
        let dir = temp_dir();
        let mesh = dir.0.join("in.obj");
        std::fs::write(&mesh, QUADS).unwrap();
        let mut s = Session::new();
        s.execute(Command::Load { path: mesh.clone() }).unwrap();
        run(
            &mut s,
            r#"
            {"cmd":"transform","islands":[1],"translate":[0,1]}
            {"cmd":"analyze"}
            {"cmd":"cut","edges":[]}
            {"cmd":"transform","rotate_deg":90}
            {"cmd":"pin","vertices":[0]}
            {"cmd":"undo"}
            {"cmd":"undo"}
            {"cmd":"redo"}
            "#,
        );
        let names: Vec<String> = s.macro_commands().iter().map(Command::name).collect();
        assert_eq!(names, ["transform", "transform"]);

        // A new edit drops the redo branch.
        run(&mut s, r#"{"cmd":"set_texel_density","density":10}"#);
        assert!(!s.can_redo());
        assert_eq!(s.macro_commands().len(), 3);

        // The saved macro replays to the same UVs on a fresh load.
        let path = dir.0.join("m.jsonl");
        s.execute(Command::SaveMacro { path: path.clone() })
            .unwrap();
        assert_eq!(s.macro_commands().len(), 3, "save_macro is not an edit");
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(parse_script(&text).unwrap(), s.macro_commands());

        let mut replay = Session::new();
        replay.execute(Command::Load { path: mesh }).unwrap();
        run(&mut replay, &text);
        assert_eq!(replay.document().unwrap().uv(), s.document().unwrap().uv());
        assert_eq!(replay.macro_commands(), s.macro_commands());

        // Loading starts a new macro.
        let again = s.document().unwrap().path.clone();
        s.execute(Command::Load { path: again }).unwrap();
        assert!(s.macro_commands().is_empty());
    }

    #[test]
    fn save_macro_needs_a_document() {
        let mut s = Session::new();
        assert!(
            s.execute(Command::SaveMacro {
                path: "unused.jsonl".into()
            })
            .is_err()
        );
    }
}
