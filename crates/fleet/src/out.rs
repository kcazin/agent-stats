//! How `fleet add` and `remove` say what they are doing: one JSON object per line with `--json` (what Tokiscope
//! reads), a short line per step otherwise. Nothing printed here ever contains a pairing code.

use serde_json::{json, Value};

pub struct Out {
    pub json: bool,
}

impl Out {
    fn emit(&self, v: Value, human: String) {
        if self.json {
            println!("{v}");
        } else {
            println!("{human}");
        }
    }

    pub fn running(&self, step: &str, detail: &str) {
        self.emit(json!({"step": step, "state": "running", "detail": detail}), format!("… {}: {detail}", title(step)));
    }

    pub fn ok(&self, step: &str, detail: &str) {
        self.emit(json!({"step": step, "state": "ok", "detail": detail}), format!("✓ {}: {detail}", title(step)));
    }

    /// A failed step; the command then exits 1.
    pub fn error(&self, step: &str, kind: &str, message: &str, hint: Option<&str>) {
        let mut v = json!({"step": step, "state": "error", "error": kind, "message": message});
        if let Some(h) = hint {
            v["hint"] = json!(h);
        }
        let human = match hint {
            Some(h) => format!("✗ {}: {message}\n  {h}", title(step)),
            None => format!("✗ {}: {message}", title(step)),
        };
        if self.json {
            println!("{v}");
        } else {
            eprintln!("{human}");
        }
    }

    pub fn done(&self, machine: &str, label: &str) {
        self.emit(
            json!({"step": "done", "state": "ok", "machine": machine, "label": label}),
            format!("{machine} is reporting to this fleet."),
        );
    }
}

fn title(step: &str) -> &str {
    match step {
        "connect" => "connect",
        "route" => "way back to this machine",
        "install" => "install",
        "join" => "join",
        "report" => "first report",
        "revoke" => "revoke",
        "uninstall" => "uninstall",
        s => s,
    }
}
