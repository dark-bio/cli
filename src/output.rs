// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Three renderings of one result, with diagnostics confined to stderr.

pub(crate) mod human;

use crate::args::{Format, Options};
use crate::error::Error;
use crate::style::{self, Role, Theme};
use darkbio_connect::trust::Environment;
use serde_json::{Value, json};
use std::io::{self, Write};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant, SystemTime};

#[derive(Clone)]
pub(crate) struct Output(Arc<State>);
struct State {
    json: bool,
    out: Theme,
    err: Theme,
    quiet: bool,
    verbose: u8,
    printed: AtomicBool,
    environment_noted: AtomicBool,
    result: Mutex<()>,
    terminal: Mutex<Terminal>,
}

#[derive(Default)]
struct Terminal {
    live: Option<(String, bool)>,
    waiting: Option<Waiting>,
    generation: u64,
    err_printed: bool,
    out_block: bool,
}

struct Waiting {
    generation: u64,
    label: String,
    started: Instant,
    until: Option<Instant>,
}

impl Output {
    pub fn new(options: &Options) -> Self {
        Self(Arc::new(State {
            json: options.format == Format::Json,
            out: Theme::new(options.format, false),
            err: Theme::new(options.format, true),
            quiet: options.quiet,
            verbose: options.verbose,
            printed: AtomicBool::new(false),
            environment_noted: AtomicBool::new(false),
            result: Mutex::new(()),
            terminal: Mutex::new(Terminal::default()),
        }))
    }
    pub fn json(&self) -> bool {
        self.0.json
    }
    pub fn human(&self) -> bool {
        self.0.err.human
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
        self.document_with(value, |theme, _| human::document(theme, value))
    }

    pub fn document_with(
        &self,
        value: &Value,
        render: impl FnOnce(&Theme, u8) -> String,
    ) -> Result<(), Error> {
        let _result = self.0.result.lock().expect("output not poisoned");
        if self.0.printed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let text = if self.json() {
            value.to_string()
        } else if self.0.out.human {
            render(&self.0.out, self.0.verbose)
        } else {
            let mut lines = Vec::new();
            fields("", value, &mut lines, false);
            lines
                .into_iter()
                .map(|(key, value)| format!("{key}: {value}"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        self.write_result(&text)
    }

    pub fn table(
        &self,
        document: &Value,
        rows: &[Value],
        columns: &[(&str, &str)],
    ) -> Result<(), Error> {
        self.grouped_table(document, rows, columns, &[])
    }

    pub fn grouped_table(
        &self,
        document: &Value,
        rows: &[Value],
        columns: &[(&str, &str)],
        groups: &[String],
    ) -> Result<(), Error> {
        self.list(document, rows, columns, |theme| {
            human::table(theme, rows, columns, groups)
        })
    }

    pub fn checklist(&self, document: &Value, rows: &[Value]) -> Result<(), Error> {
        self.list(
            document,
            rows,
            &[
                ("CHECK", "name"),
                ("RESULT", "result"),
                ("DETAIL", "detail"),
                ("HINT", "hint"),
            ],
            |theme| human::checklist(theme, rows),
        )
    }

    fn list(
        &self,
        document: &Value,
        rows: &[Value],
        columns: &[(&str, &str)],
        render: impl FnOnce(&Theme) -> String,
    ) -> Result<(), Error> {
        if self.json() {
            return self.document(document);
        }
        let _result = self.0.result.lock().expect("output not poisoned");
        if self.0.printed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        self.write_result(&if self.0.out.human {
            render(&self.0.out)
        } else {
            text_table(rows, columns)
        })
    }

    fn write_result(&self, text: &str) -> Result<(), Error> {
        let mut terminal = self.0.terminal.lock().expect("output not poisoned");
        terminal.waiting = None;
        close_line(&mut terminal, &mut io::stderr().lock());
        let mut stdout = io::stdout().lock();
        if self.0.out.human && self.0.err.human && terminal.err_printed {
            writeln!(stdout)?;
        }
        writeln!(stdout, "{text}")?;
        stdout.flush()?;
        terminal.out_block = self.0.out.human;
        Ok(())
    }

    /// App reports and dataset READMEs bypass every layout rule.
    pub fn app(&self, stdout: &[u8], stderr: &[u8]) -> Result<(), Error> {
        let _result = self.0.result.lock().expect("output not poisoned");
        if self.0.printed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        self.finish();
        {
            let mut output = io::stdout().lock();
            output.write_all(stdout)?;
            output.flush()?;
        }
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
        let message = message.as_ref();
        if kind == "progress" && self.human() {
            let theme = &self.0.err;
            let line = format!(
                "{} {}",
                theme.paint(Role::Muted, "progress:"),
                theme.inline(message)
            );
            self.progress_line(message.split(':').next().unwrap_or(message), &line);
            return;
        }
        {
            let mut terminal = self.0.terminal.lock().expect("output not poisoned");
            if !matches!(kind, "note" | "warning" | "step" | "log") {
                terminal.waiting = None;
            }
            let mut stderr = io::stderr().lock();
            close_line(&mut terminal, &mut stderr);
            if self.json() {
                let _ = writeln!(stderr, "{}", json!({"event":kind,"message":message}));
            } else if self.human() {
                separate_result(&mut terminal, &mut stderr);
                let _ = writeln!(stderr, "{}", event_line(&self.0.err, kind, message));
            } else {
                let _ = writeln!(stderr, "{kind}: {message}");
            }
            terminal.err_printed = true;
            let _ = stderr.flush();
        }
        if kind == "approve" && !message.contains('\n') {
            self.wait("waiting", None);
        }
    }

    pub fn progress(&self, update: &crate::progress::Update) {
        if self.human() {
            self.progress_line(&update.stage, &update.render(&self.0.err));
        } else {
            self.event("progress", &update.text);
        }
    }

    fn progress_line(&self, key: &str, line: &str) {
        if self.0.quiet {
            return;
        }
        let mut terminal = self.0.terminal.lock().expect("output not poisoned");
        terminal.waiting = None;
        let mut stderr = io::stderr().lock();
        if terminal
            .live
            .as_ref()
            .is_some_and(|(previous, _)| previous == key)
        {
            let _ = write!(stderr, "{}", style::CLEAR_LINE);
            terminal.live = None;
        } else {
            close_line(&mut terminal, &mut stderr);
        }
        separate_result(&mut terminal, &mut stderr);
        let line = self
            .0
            .err
            .truncate(line, self.0.err.width.saturating_sub(1));
        if self.0.err.interactive {
            let _ = write!(stderr, "{line}");
            terminal.live = Some((key.to_string(), false));
        } else {
            let _ = writeln!(stderr, "{line}");
        }
        terminal.err_printed = true;
        let _ = stderr.flush();
    }

    /// Optional human detail has no counterpart in the machine event stream.
    pub fn human_event(&self, kind: &str, message: impl AsRef<str>) {
        if self.human() {
            self.event(kind, message);
        }
    }

    pub fn title(&self, title: &str) {
        if !self.human() || self.0.quiet {
            return;
        }
        self.finish();
        let mut terminal = self.0.terminal.lock().expect("output not poisoned");
        let mut stderr = io::stderr().lock();
        if terminal.err_printed {
            let _ = writeln!(stderr);
        }
        let _ = writeln!(
            stderr,
            "{}",
            style::wrap(
                &format!("  {}", self.0.err.paint(Role::Muted, title)),
                self.0.err.width,
                2
            )
        );
        terminal.err_printed = true;
    }

    pub fn stage(&self, name: &str, done: bool) {
        if !self.human() {
            return;
        }
        let theme = &self.0.err;
        let value = if done {
            theme.mark(Role::Success, name)
        } else {
            theme.paint(
                Role::Muted,
                format!("{} {name}", theme.glyph("\u{203a}", ">")),
            )
        };
        self.progress_line(
            &format!("pair:{name}"),
            &format!("{} {value}", theme.paint(Role::Muted, "progress:")),
        );
        if done {
            self.finish();
        }
    }

    pub fn pairing(&self, url: &str, deadline: SystemTime) {
        self.event("approve", "scan in Ark Companion");
        self.finish();
        {
            let mut terminal = self.0.terminal.lock().expect("output not poisoned");
            let mut stderr = io::stderr().lock();
            if self.0.err.unicode
                && let Ok(qr) = qrcode::QrCode::new(url.as_bytes())
            {
                let qr = qr
                    .render::<qrcode::render::unicode::Dense1x2>()
                    .quiet_zone(true)
                    .build();
                if qr
                    .lines()
                    .all(|line| console::measure_text_width(line) + 2 <= self.0.err.width)
                {
                    let _ = writeln!(
                        stderr,
                        "\n{}",
                        qr.lines()
                            .map(|line| format!("  {line}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    );
                }
            }
            let _ = writeln!(
                stderr,
                "\n{}",
                style::wrap(
                    &format!("  {}", self.0.err.paint(Role::Accent, url)),
                    self.0.err.width,
                    2
                )
            );
            terminal.err_printed = true;
        }
        self.wait("scan", Some(deadline));
    }

    /// The timer only draws while a wait is active. It never bounds the call.
    pub fn wait(&self, label: &str, until: Option<SystemTime>) {
        if !self.0.err.interactive || self.0.quiet {
            return;
        }
        let generation;
        {
            let mut terminal = self.0.terminal.lock().expect("output not poisoned");
            terminal.generation += 1;
            generation = terminal.generation;
            terminal.waiting = Some(Waiting {
                generation,
                label: label.into(),
                started: Instant::now(),
                until: until.and_then(|end| {
                    Instant::now()
                        .checked_add(end.duration_since(SystemTime::now()).unwrap_or_default())
                }),
            });
            tick(
                &self.0.err,
                &mut terminal,
                &mut io::stderr().lock(),
                Instant::now(),
            );
        }
        let state = Arc::downgrade(&self.0);
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(1));
                let Some(state) = state.upgrade() else { break };
                let mut terminal = state.terminal.lock().expect("output not poisoned");
                if !terminal
                    .waiting
                    .as_ref()
                    .is_some_and(|wait| wait.generation == generation)
                {
                    break;
                }
                tick(
                    &state.err,
                    &mut terminal,
                    &mut io::stderr().lock(),
                    Instant::now(),
                );
            }
        });
    }

    pub fn prompt(&self, message: &str, default: bool) -> Result<(), Error> {
        self.finish();
        let choices = if default { "Y/n" } else { "y/N" };
        let mut stderr = io::stderr().lock();
        if self.human() {
            let line = format!(
                "{} {} {}",
                self.0.err.paint(Role::Attention, "?"),
                self.0.err.inline(message),
                self.0.err.paint(Role::Muted, format!("({choices})"))
            );
            write!(
                stderr,
                "{} ",
                style::wrap(&line, self.0.err.width.saturating_sub(1), 2)
            )?;
        } else {
            write!(stderr, "{message} [{choices}] ")?;
        }
        stderr.flush()?;
        Ok(())
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
            let mut terminal = self.0.terminal.lock().expect("output not poisoned");
            let mut stderr = io::stderr().lock();
            if self.human() {
                separate_result(&mut terminal, &mut stderr);
                let theme = &self.0.err;
                let line = format!(
                    "{} {}{}",
                    theme.paint(Role::Failure, format!("error[{}]:", error.code)),
                    theme.inline(&error.message),
                    theme.paint(Role::Muted, &remote)
                );
                let _ = writeln!(stderr, "{}", style::wrap(&line, theme.width, 2));
            } else {
                let _ = writeln!(stderr, "error[{}]: {}{remote}", error.code, error.message);
            }
            terminal.err_printed = true;
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
        let mut terminal = self.0.terminal.lock().expect("output not poisoned");
        terminal.waiting = None;
        let mut stderr = io::stderr().lock();
        close_line(&mut terminal, &mut stderr);
        let _ = stderr.flush();
    }
}

fn event_line(theme: &Theme, kind: &str, message: &str) -> String {
    let role = match kind {
        "error" => Role::Failure,
        "warning" | "approve" => Role::Attention,
        "hint" => Role::Accent,
        _ => Role::Muted,
    };
    let prefix = theme.paint(role, format!("{kind}:"));
    let message = if kind == "step" {
        theme.paint(
            Role::Muted,
            format!("{} {message}", theme.glyph("\u{203a}", ">")),
        )
    } else {
        theme.inline(message)
    };
    style::wrap(&format!("{prefix} {message}"), theme.width, kind.len() + 2)
}

fn close_line(terminal: &mut Terminal, output: &mut impl Write) {
    if let Some((_, temporary)) = terminal.live.take() {
        if temporary {
            let _ = write!(output, "{}", style::CLEAR_LINE);
        } else {
            let _ = writeln!(output);
        }
    }
}

fn separate_result(terminal: &mut Terminal, output: &mut impl Write) {
    if terminal.out_block {
        let _ = writeln!(output);
        terminal.out_block = false;
    }
}

fn tick(theme: &Theme, terminal: &mut Terminal, output: &mut impl Write, now: Instant) {
    let Some(wait) = &terminal.waiting else {
        return;
    };
    let time = match wait.until {
        Some(until) => format!("{} s left", until.saturating_duration_since(now).as_secs()),
        None => format!(
            "{} s elapsed",
            now.saturating_duration_since(wait.started).as_secs()
        ),
    };
    let line = theme.paint(
        Role::Muted,
        format!(
            "         {} {} {time}",
            wait.label,
            theme.glyph("\u{00b7}", "-")
        ),
    );
    close_line(terminal, output);
    let _ = write!(
        output,
        "{}",
        theme.truncate(&line, theme.width.saturating_sub(1))
    );
    terminal.live = Some(("waiting".into(), true));
    let _ = output.flush();
}

fn text_table(rows: &[Value], columns: &[(&str, &str)]) -> String {
    let cells: Vec<Vec<_>> = rows
        .iter()
        .map(|row| {
            columns
                .iter()
                .map(|(_, key)| display(key, &row[*key], false))
                .collect()
        })
        .collect();
    let widths: Vec<_> = columns
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
    let line = |row: &[String]| {
        row.iter()
            .enumerate()
            .map(|(i, cell)| {
                if i + 1 == row.len() {
                    cell.clone()
                } else {
                    format!(
                        "{cell}{}",
                        " ".repeat(widths[i].saturating_sub(console::measure_text_width(cell)) + 2)
                    )
                }
            })
            .collect::<String>()
    };
    let mut lines = vec![line(
        &columns
            .iter()
            .map(|(name, _)| name.to_string())
            .collect::<Vec<_>>(),
    )];
    lines.extend(cells.iter().map(|row| line(row)));
    lines.join("\n")
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

pub(crate) fn display(key: &str, value: &Value, human: bool) -> String {
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
    use crate::style::Color;
    use clap::Parser;

    #[test]
    fn events_keep_prefixes_and_style_inline_commands() {
        let theme = Theme::test(80, Color::Basic, true);
        assert_eq!(
            event_line(&theme, "hint", "run `ark unlock` first"),
            "\x1b[1mhint:\x1b[0m run \x1b[1mark unlock\x1b[0m first"
        );
        assert_eq!(
            event_line(&theme, "approve", "confirm on your phone"),
            "\x1b[1mapprove:\x1b[0m confirm on your phone"
        );
        assert_eq!(
            event_line(&theme, "step", "reading attestation"),
            "step: \u{203a} reading attestation"
        );
    }

    #[test]
    fn waiting_line_is_erased_but_completed_progress_stays() {
        let theme = Theme::test(80, Color::Off, true);
        let started = Instant::now();
        let mut terminal = Terminal {
            waiting: Some(Waiting {
                generation: 1,
                label: "waiting".into(),
                started,
                until: None,
            }),
            ..Default::default()
        };
        let mut output = Vec::new();
        tick(
            &theme,
            &mut terminal,
            &mut output,
            started + Duration::from_secs(2),
        );
        assert_eq!(
            String::from_utf8(output.clone()).unwrap(),
            "         waiting \u{00b7} 2 s elapsed"
        );
        output.clear();
        terminal.waiting.as_mut().unwrap().until = Some(started + Duration::from_secs(20));
        tick(
            &theme,
            &mut terminal,
            &mut output,
            started + Duration::from_secs(5),
        );
        assert_eq!(
            String::from_utf8(output.clone()).unwrap(),
            format!("{}         waiting \u{00b7} 15 s left", style::CLEAR_LINE)
        );
        output.clear();
        terminal.waiting = None;
        close_line(&mut terminal, &mut output);
        tick(
            &theme,
            &mut terminal,
            &mut output,
            started + Duration::from_secs(6),
        );
        assert_eq!(output, style::CLEAR_LINE.as_bytes());
        output.clear();
        terminal.live = Some(("uploading".into(), false));
        close_line(&mut terminal, &mut output);
        close_line(&mut terminal, &mut output);
        assert_eq!(output, b"\n");
    }

    #[test]
    fn text_table_keeps_machine_layout() {
        let rows = [
            json!({"slot":"reference-genome","state":"filled","size_bytes":3_u64 << 30}),
            json!({"slot":"variant-catalog","state":"empty","size_bytes":null}),
        ];
        assert_eq!(
            text_table(
                &rows,
                &[("SLOT", "slot"), ("STATE", "state"), ("SIZE", "size_bytes")]
            ),
            "SLOT              STATE   SIZE\nreference-genome  filled  3072.0 MiB\nvariant-catalog   empty   -"
        );
    }

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
