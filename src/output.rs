// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Three renderings of one result, with diagnostics confined to stderr.

use crate::args::{Format, Options};
use crate::error::Error;
use console::Style;
use darkbio_connect::trust::Environment;
use serde_json::{Value, json};
use std::io::{self, IsTerminal, Write};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

#[derive(Clone)]
pub(crate) struct Output(Arc<State>);
struct State {
    json: bool,
    human_out: bool,
    human_err: bool,
    color: bool,
    quiet: bool,
    verbose: u8,
    printed: AtomicBool,
    environment_noted: AtomicBool,
    result: Mutex<()>,
    progress: Mutex<bool>,
}

impl Output {
    pub fn new(options: &Options) -> Self {
        let human = |tty| match options.format {
            Format::Human => true,
            Format::Auto => tty,
            _ => false,
        };
        Self(Arc::new(State {
            json: options.format == Format::Json,
            human_out: human(io::stdout().is_terminal()),
            human_err: human(io::stderr().is_terminal()),
            color: std::env::var_os("NO_COLOR").is_none(),
            quiet: options.quiet,
            verbose: options.verbose,
            printed: AtomicBool::new(false),
            environment_noted: AtomicBool::new(false),
            result: Mutex::new(()),
            progress: Mutex::new(false),
        }))
    }
    pub fn json(&self) -> bool {
        self.0.json
    }
    pub fn human(&self) -> bool {
        self.0.human_err
    }
    pub fn printed(&self) -> bool {
        self.0.printed.load(Ordering::SeqCst)
    }

    /// Reconnects share one environment note for the command.
    pub fn environment(&self, env: Environment) {
        if let Some(message) = self.environment_note(env) {
            self.event("note", message);
        }
    }

    fn environment_note(&self, env: Environment) -> Option<String> {
        if self.0.quiet
            || env == Environment::Release
            || self.0.environment_noted.swap(true, Ordering::Relaxed)
        {
            return None;
        }
        Some(format!("using the {env} environment"))
    }

    pub fn document(&self, value: &Value) -> Result<(), Error> {
        let _result = self.0.result.lock().expect("output not poisoned");
        if self.0.printed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        self.finish();
        let text = if self.json() {
            value.to_string()
        } else {
            let mut lines = Vec::new();
            fields("", value, &mut lines, self.0.human_out);
            lines
                .into_iter()
                .map(|(key, value)| {
                    format!(
                        "{} {}",
                        self.style(&format!("{key}:"), "key", false),
                        self.style(&value, &value, false)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        self.write(text.as_bytes())?;
        self.write(b"\n")?;
        self.0.printed.store(true, Ordering::SeqCst);
        Ok(())
    }

    pub fn table(
        &self,
        document: &Value,
        rows: &[Value],
        columns: &[(&str, &str)],
    ) -> Result<(), Error> {
        if self.json() {
            return self.document(document);
        }
        let _result = self.0.result.lock().expect("output not poisoned");
        if self.0.printed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        self.finish();
        let cells: Vec<Vec<String>> = rows
            .iter()
            .map(|row| {
                columns
                    .iter()
                    .map(|(_, key)| display(key, &row[*key], self.0.human_out))
                    .collect()
            })
            .collect();
        let widths: Vec<usize> = columns
            .iter()
            .enumerate()
            .map(|(i, (name, _))| {
                cells
                    .iter()
                    .map(|row| console::measure_text_width(&row[i]))
                    .max()
                    .unwrap_or(0)
                    .max(name.len())
            })
            .collect();
        let line = |row: Vec<String>| {
            row.iter()
                .enumerate()
                .map(|(i, cell)| {
                    if i + 1 == row.len() {
                        return cell.clone();
                    }
                    format!(
                        "{cell}{}",
                        " ".repeat(widths[i].saturating_sub(console::measure_text_width(cell)) + 2)
                    )
                })
                .collect::<String>()
        };
        let header = line(columns.iter().map(|(name, _)| name.to_string()).collect());
        self.write(format!("{}\n", self.style(&header, "header", false)).as_bytes())?;
        for row in cells {
            let row: Vec<_> = row
                .into_iter()
                .map(|value| self.style(&value, &value, false))
                .collect();
            self.write(format!("{}\n", line(row)).as_bytes())?;
        }
        self.0.printed.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Reports binary app streams verbatim. JSON callers receive them as fields
    /// in their result instead of using this method.
    pub fn app(&self, stdout: &[u8], stderr: &[u8]) -> Result<(), Error> {
        let _result = self.0.result.lock().expect("output not poisoned");
        if self.0.printed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        self.finish();
        self.write(stdout)?;
        self.0.printed.store(true, Ordering::SeqCst);
        if !stderr.is_empty() {
            self.event("note", format!("app stderr, {} bytes", stderr.len()));
            let mut output = io::stderr().lock();
            output.write_all(stderr)?;
            output.flush()?;
        }
        Ok(())
    }

    pub fn event(&self, kind: &str, message: impl AsRef<str>) {
        if self.0.quiet && matches!(kind, "progress" | "note" | "warning" | "step") {
            return;
        }
        if kind == "step" && self.0.verbose == 0 {
            return;
        }
        let mut active = self.0.progress.lock().expect("output not poisoned");
        let mut stderr = io::stderr().lock();
        if *active {
            let _ = write!(stderr, "\r\x1b[2K");
            *active = false;
        }
        if self.json() {
            let _ = writeln!(
                stderr,
                "{}",
                json!({"event":kind,"message":message.as_ref()})
            );
        } else if kind == "progress" && self.0.human_err {
            let _ = write!(
                stderr,
                "{}",
                self.style(&bar(message.as_ref()), "progress", true)
            );
            *active = true;
        } else {
            let message = message.as_ref();
            let _ = writeln!(
                stderr,
                "{} {message}",
                self.style(&format!("{kind}:"), kind, true)
            );
        }
        let _ = stderr.flush();
    }

    pub fn error(&self, error: &Error) {
        self.finish();
        if self.json() {
            self.event_value(json!({"event":"error", "error":error.json()}));
        } else {
            let remote = error
                .remote
                .as_ref()
                .map(|remote| format!(" (code 0x{:x})", remote.code))
                .unwrap_or_default();
            let label = self.style(&format!("error[{}]:", error.code), "error", true);
            let _ = writeln!(io::stderr().lock(), "{label} {}{remote}", error.message);
        }
        for hint in &error.hints {
            self.event("hint", hint);
        }
    }

    pub fn event_value(&self, value: Value) {
        self.finish();
        let _ = writeln!(io::stderr().lock(), "{value}");
    }

    pub fn finish(&self) {
        let mut active = self.0.progress.lock().expect("output not poisoned");
        if *active {
            let _ = writeln!(io::stderr().lock());
            *active = false;
        }
    }

    fn write(&self, bytes: &[u8]) -> Result<(), Error> {
        let mut stdout = io::stdout().lock();
        stdout.write_all(bytes)?;
        stdout.flush()?;
        Ok(())
    }

    fn style(&self, value: &str, kind: &str, stderr: bool) -> String {
        let enabled = self.0.color
            && if stderr {
                self.0.human_err
            } else {
                self.0.human_out
            };
        let style = match kind {
            "error" | "damaged" | "develop" | "fail" => Style::new().red(),
            "warning" | "approve" | "locked" | "empty" | "staging" | "warn" => {
                Style::new().yellow()
            }
            "attested" | "filled" | "ok" | "verified" => Style::new().green(),
            "header" => Style::new().bold(),
            _ => Style::new().dim(),
        };
        style.force_styling(enabled).apply_to(value).to_string()
    }
}

fn fields(prefix: &str, value: &Value, lines: &mut Vec<(String, String)>, human: bool) {
    if let Value::Object(object) = value {
        for (key, value) in object {
            let key = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{prefix}.{key}")
            };
            fields(&key, value, lines, human);
        }
    } else if let Value::Array(values) = value
        && values.iter().any(Value::is_object)
    {
        for (index, value) in values.iter().enumerate() {
            fields(&format!("{prefix}.{index}"), value, lines, human);
        }
    } else {
        lines.push((prefix.to_string(), display(prefix, value, human)));
    }
}

fn display(key: &str, value: &Value, human: bool) -> String {
    if key == "serial" && value.is_null() {
        return "unverified".into();
    }
    if key.ends_with("_bytes")
        && let Some(bytes) = value.as_u64()
    {
        return if bytes >= 1024 * 1024 {
            format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
        } else if bytes >= 1024 {
            format!("{:.1} KiB", bytes as f64 / 1024.0)
        } else {
            format!("{bytes} B")
        };
    }
    if key.ends_with("_seconds")
        && let Some(seconds) = value.as_u64()
    {
        return if human && seconds >= 60 {
            format!("{}m {}s", seconds / 60, seconds % 60)
        } else {
            format!("{seconds} s")
        };
    }
    if human
        && let Some(value) = value.as_str()
        && let Ok(time) = chrono::DateTime::parse_from_rfc3339(value)
    {
        return time
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M:%S %:z")
            .to_string();
    }
    scalar(value)
}

fn bar(message: &str) -> String {
    let percent = message.split_whitespace().find_map(|word| {
        word.strip_suffix('%')
            .and_then(|number| number.parse::<f64>().ok())
    });
    match percent {
        Some(percent) => {
            let filled = (percent.clamp(0.0, 100.0) / 5.0) as usize;
            format!(
                "[{}{}] {message}",
                "=".repeat(filled),
                " ".repeat(20 - filled)
            )
        }
        None => message.to_string(),
    }
}

pub(crate) fn scalar(value: &Value) -> String {
    match value {
        Value::Null => "-".into(),
        Value::Bool(true) => "yes".into(),
        Value::Bool(false) => "no".into(),
        Value::String(value) => value.clone(),
        Value::Array(values) => values.iter().map(scalar).collect::<Vec<_>>().join(", "),
        value => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn nonrelease_environment_is_noted_once_unless_quiet() {
        for env in [Environment::Develop, Environment::Staging] {
            for quiet in [false, true] {
                let mut options = crate::args::Cli::try_parse_from(["ark"]).unwrap().options;
                options.quiet = quiet;
                let output = Output::new(&options);
                assert_eq!(output.environment_note(Environment::Release), None);
                assert_eq!(
                    output.environment_note(env),
                    (!quiet).then(|| format!("using the {env} environment"))
                );
                assert_eq!(output.clone().environment_note(env), None);
            }
        }
    }

    #[test]
    fn units_and_absent_fields_are_explicit() {
        assert_eq!(display("serial", &Value::Null, false), "unverified");
        assert_eq!(display("size_bytes", &json!(13002342), false), "12.4 MiB");
        assert_eq!(display("duration_seconds", &json!(72), false), "72 s");
        assert_eq!(display("duration_seconds", &json!(72), true), "1m 12s");
        assert_eq!(display("size_bytes", &Value::Null, false), "-");
        let mut lines = Vec::new();
        fields(
            "",
            &json!({"firmware":{"version":"1.0.0-1234567","published":null},"unlocked":false}),
            &mut lines,
            false,
        );
        assert_eq!(
            lines,
            [
                ("firmware.version".into(), "1.0.0-1234567".into()),
                ("firmware.published".into(), "-".into()),
                ("unlocked".into(), "no".into())
            ]
        );
    }
}
