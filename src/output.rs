// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Results, diagnostics and stage narration shared by both output streams.

pub(crate) mod human;
mod prompt;

use crate::args::Options;
use crate::error::Error;
use crate::progress::Update;
use crate::style::{self, Role, Theme};
use darkbio_clock::{Clock, crossbeam_channel};
use darkbio_connect::trust::Environment;
use serde_json::{Value, json};
use std::io::{self, Write};
use std::sync::{
    Arc, Mutex, MutexGuard,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Clonable output handle sharing the sole result and current narration.
#[derive(Clone)]
pub(crate) struct Output(
    /// Invocation state shared by result, event and interruption writers.
    Arc<State>,
);

/// Stream policy and synchronization for one invocation.
struct State {
    /// Whether the invocation uses JSON documents and events.
    json: bool,
    /// Independently resolved stdout capabilities.
    out: Theme,
    /// Independently resolved stderr capabilities.
    err: Theme,
    /// Whether optional narration and diagnostics are hidden.
    quiet: bool,
    /// Whether step events are shown.
    verbose: bool,
    /// Whether stdout has been claimed, including a failed write.
    printed: AtomicBool,
    /// Whether a non-release environment was already announced.
    environment_noted: AtomicBool,
    /// Result claim and write lock, taken before the ticker and terminal locks.
    result: Mutex<()>,
    /// Clock timing narration, replaced when a connection supplies one.
    clock: Mutex<Clock>,
    /// Redraw worker lock, always taken before the terminal lock.
    ticker: Mutex<Option<Ticker>>,
    /// Layout and activity shared with the redraw worker.
    terminal: Arc<Mutex<Terminal>>,
}

/// Worker stopped and joined before its display changes owner.
struct Ticker {
    /// Dropping this sender wakes the worker to stop.
    stop: Option<crossbeam_channel::Sender<()>>,
    /// Join handle preventing a redraw from outliving its display.
    worker: Option<JoinHandle<()>>,
}

impl Ticker {
    /// Draws once per clock second until the owner drops the stop sender.
    fn start(clock: Clock, mut draw: impl FnMut(Instant) + Send + 'static) -> Self {
        let (stop, stopped) = crossbeam_channel::bounded::<()>(0);
        let worker = std::thread::spawn(move || {
            loop {
                let next = clock.now() + Duration::from_secs(1);
                if clock.recv_deadline(&stopped, next)
                    != Err(crossbeam_channel::RecvTimeoutError::Timeout)
                {
                    break;
                }
                draw(clock.now());
            }
        });
        Self {
            stop: Some(stop),
            worker: Some(worker),
        }
    }
}

impl Drop for Ticker {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Page composition and the current stage, serialized with stderr writes.
#[derive(Default)]
struct Terminal {
    /// Number of screen rows occupied by the temporary live line.
    live: usize,
    /// Active stage or approval, retained until its outcome is known.
    active: Option<Activity>,
    /// Whether persistent stderr content precedes a result.
    err_printed: bool,
    /// Whether the next event needs a blank line after the result.
    out_block: bool,
}

/// A live stage or approval with its timing and most recent facts.
#[derive(Clone)]
struct Activity {
    /// Most recent label and progress facts.
    update: Update,
    /// Clock time when this stage or approval began.
    started: Instant,
    /// Timer semantics explicitly chosen by its caller.
    timer: Option<Wait>,
    /// Approval action and location, absent for machine work.
    approval: Option<(String, &'static str)>,
    /// Last emission time, used to bound live redraws and pipe intervals.
    drawn: Option<Instant>,
}

/// Presentation-only wait semantics, independent of operation enforcement.
#[derive(Clone, Copy)]
pub(crate) enum Wait {
    /// Count up from the request, including device-owned approval windows.
    Elapsed,
    /// Count down to a deadline set by the CLI itself.
    Deadline(Instant),
}

/// Temporary connection notice erased when the connection attempt returns.
pub(crate) struct Connecting(
    /// Output owning the notice, absent when it was suppressed.
    Option<Output>,
);

impl Drop for Connecting {
    fn drop(&mut self) {
        if let Some(output) = &self.0 {
            let mut terminal = output.0.terminal.lock().expect("output not poisoned");
            close_line(&mut terminal, &mut io::stderr().lock());
        }
    }
}

impl Output {
    /// Resolves both streams and starts narration on the real clock.
    pub fn new(options: &Options) -> Self {
        Self(Arc::new(State {
            json: options.json,
            out: Theme::new(options.json, false),
            err: Theme::new(options.json, true),
            quiet: options.quiet,
            verbose: options.verbose,
            printed: AtomicBool::new(false),
            environment_noted: AtomicBool::new(false),
            result: Mutex::new(()),
            clock: Mutex::new(Clock::real()),
            ticker: Mutex::new(None),
            terminal: Arc::new(Mutex::new(Terminal::default())),
        }))
    }

    /// Times subsequent stages on the connection's clock.
    pub fn connection(&self, clock: Clock) {
        *self.0.clock.lock().expect("output not poisoned") = clock;
    }
    /// Reports whether both streams use JSON.
    pub fn json(&self) -> bool {
        self.0.json
    }
    /// Reports whether stderr supports live updates.
    pub fn terminal(&self) -> bool {
        self.0.err.interactive
    }
    /// Reports whether the sole stdout result was claimed, so failure reporting
    /// must not emit another.
    pub fn printed(&self) -> bool {
        self.0.printed.load(Ordering::SeqCst)
    }
    /// Returns the current narration clock.
    fn clock(&self) -> Clock {
        self.0.clock.lock().expect("output not poisoned").clone()
    }

    /// Notes a non-release environment once per invocation, however often it
    /// reconnects.
    pub fn environment(&self, env: Environment) {
        if let Some(message) = self.environment_note(env) {
            self.event("note", message);
        }
    }

    /// Claims the non-release note unless quiet suppresses it.
    fn environment_note(&self, env: Environment) -> Option<String> {
        if self.0.quiet
            || env == Environment::Release
            || self.0.environment_noted.swap(true, Ordering::Relaxed)
        {
            return None;
        }
        Some(format!("using the {env} environment"))
    }

    /// Emits the sole result as a reading block or JSON document.
    pub fn document(&self, value: &Value) -> Result<(), Error> {
        self.document_with(value, |theme| human::document(theme, value))
    }

    /// Emits preformatted text as the sole result, including in JSON mode.
    ///
    /// A failed write claims the result too, so no error document follows text
    /// that may already have reached stdout.
    pub fn text(&self, text: &str) -> Result<(), Error> {
        let _result = self.0.result.lock().expect("output not poisoned");
        if self.0.printed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        self.write_result(text.trim_end_matches('\n'))
    }

    /// Emits a document, invoking the supplied renderer only for reading output.
    ///
    /// Later result attempts are ignored, including after an earlier write failed.
    pub fn document_with(
        &self,
        value: &Value,
        render: impl FnOnce(&Theme) -> String,
    ) -> Result<(), Error> {
        let _result = self.0.result.lock().expect("output not poisoned");
        if self.0.printed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let text = if self.json() {
            serde_json::to_string_pretty(value).expect("JSON value serializes")
        } else {
            render(&self.0.out)
        };
        self.write_result(&text)
    }

    /// Renders rows as a table while retaining the full JSON document.
    pub fn table(
        &self,
        document: &Value,
        rows: &[Value],
        columns: &[(&str, &str)],
    ) -> Result<(), Error> {
        self.document_with(document, |theme| human::table(theme, rows, columns))
    }

    /// Renders titled check groups with their summary.
    pub fn checklist(&self, document: &Value, groups: &[(&str, &[Value])]) -> Result<(), Error> {
        self.document_with(document, |theme| human::checklist(theme, groups))
    }

    /// Stops live redraws and writes a result with all page spacing on stderr.
    ///
    /// The caller holds the result lock, which comes before the ticker and
    /// terminal locks.
    fn write_result(&self, text: &str) -> Result<(), Error> {
        let mut terminal = self.stop_ticker();
        write_result(
            &mut terminal,
            &mut io::stdout().lock(),
            &mut io::stderr().lock(),
            text,
            self.json(),
        )?;
        Ok(())
    }

    /// Emits an app's report as the sole result, copying its bytes unchanged.
    ///
    /// The report goes to stdout without any layout. A nonempty app stderr
    /// follows on stderr, after a note with its size.
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

    /// Writes a best-effort stderr event under quiet and verbosity policy.
    ///
    /// A plain progress message starts a stage, keyed by its text before the
    /// first colon.
    pub fn event(&self, kind: &str, message: impl AsRef<str>) {
        if self.0.quiet && matches!(kind, "progress" | "note" | "warning" | "step") {
            return;
        }
        if kind == "step" && !self.0.verbose {
            return;
        }
        let message = message.as_ref();
        if kind == "progress" {
            self.stage(message.split(':').next().unwrap_or(message), message);
            return;
        }
        let mut terminal = self.0.terminal.lock().expect("output not poisoned");
        emit(
            &mut terminal,
            &mut io::stderr().lock(),
            self.json(),
            json!({"event":kind,"message":message}),
            &event_line(&self.0.err, kind, message),
        );
    }

    /// Starts a stage, completing its predecessor before showing the live line.
    pub fn stage(&self, stage: &str, message: &str) {
        self.start(Update::new(stage, message), None, None);
    }

    /// Starts a count-up wait or a countdown to a CLI-owned deadline.
    pub fn wait(&self, stage: &str, message: &str, timer: Wait) {
        self.start(Update::new(stage, message), Some(timer), None);
    }

    /// Starts an approval with caller-supplied instruction words and machine facts.
    pub fn approve(&self, action: &str, location: &'static str, instruction: impl AsRef<str>) {
        self.start(
            Update::new(action, instruction.as_ref()),
            Some(Wait::Elapsed),
            Some((action.into(), location)),
        );
    }

    /// Completes the current activity and starts its successor on the shared clock.
    fn start(&self, update: Update, timer: Option<Wait>, approval: Option<(String, &'static str)>) {
        // Serialize completion and replacement with the worker that owns the line
        let clock = self.clock();
        let mut ticker = self.0.ticker.lock().expect("output not poisoned");
        drop(ticker.take());
        {
            let mut terminal = self.0.terminal.lock().expect("output not poisoned");
            end_activity(
                &self.0.err,
                &mut terminal,
                &mut io::stderr().lock(),
                clock.now(),
                self.json(),
                self.0.quiet,
                None,
                None,
                false,
            );
            terminal.active = Some(Activity {
                update,
                started: clock.now(),
                timer,
                approval,
                drawn: None,
            });
            draw(
                &self.0.err,
                &mut terminal,
                &mut io::stderr().lock(),
                clock.now(),
                self.json(),
                self.0.quiet,
                true,
            );
        }
        // The worker owns only its display, so dropping Output always stops it
        let theme = self.0.err.clone();
        let terminal = self.0.terminal.clone();
        let (json, quiet) = (self.json(), self.0.quiet);
        *ticker = Some(Ticker::start(clock, move |now| {
            let mut terminal = terminal.lock().expect("output not poisoned");
            draw(
                &theme,
                &mut terminal,
                &mut io::stderr().lock(),
                now,
                json,
                quiet,
                false,
            );
        }));
    }

    /// Updates the live facts, finishing a stage when its percentage reaches 100.
    pub fn progress(&self, update: &Update) {
        // A restarted phase replaces its abandoned frame without claiming success
        if update.restarted {
            let mut terminal = self.stop_ticker();
            close_line(&mut terminal, &mut io::stderr().lock());
            terminal.active = None;
        }

        // Update one stage in place, or complete it before starting the next
        let same = self
            .0
            .terminal
            .lock()
            .expect("output not poisoned")
            .active
            .as_ref()
            .is_some_and(|active| active.approval.is_none() && active.update.same_stage(update));
        if !same {
            if update.failed {
                self.fail_stage();
            }
            self.start(update.clone(), None, None);
        } else {
            let mut terminal = self.0.terminal.lock().expect("output not poisoned");
            terminal.active.as_mut().unwrap().update = update.clone();
            draw(
                &self.0.err,
                &mut terminal,
                &mut io::stderr().lock(),
                self.clock().now(),
                self.json(),
                self.0.quiet,
                !self.terminal(),
            );
        }
        // A terminal bar never becomes the persistent completion record
        if update.failed {
            self.fail_stage();
        } else if update.facts.percent == Some(100) {
            let message = update
                .facts
                .done_bytes
                .filter(|_| update.facts.stage.as_deref() == Some("uploading"))
                .map(|bytes| format!("Uploaded {}", style::bytes(bytes)));
            self.end_stage(message.as_deref(), None);
        }
    }

    /// Ends a stage with caller-supplied words and a fact, or its elapsed duration.
    ///
    /// Absent words retain the stage label or use the approval's location outcome.
    pub fn end_stage(&self, message: Option<&str>, fact: Option<&str>) {
        self.outcome(message, fact, false);
    }

    /// Ends the active stage or approval as failed, before its diagnostic.
    pub fn fail_stage(&self) {
        self.outcome(None, None, true);
    }

    /// Stops redraws and emits exactly one outcome for the active activity.
    fn outcome(&self, message: Option<&str>, fact: Option<&str>, failed: bool) {
        let mut terminal = self.stop_ticker();
        end_activity(
            &self.0.err,
            &mut terminal,
            &mut io::stderr().lock(),
            self.clock().now(),
            self.json(),
            self.0.quiet,
            message,
            fact,
            failed,
        );
    }

    /// Shows an immediately flushed connection notice until its guard drops.
    ///
    /// A reconnect inside a narrated wait retains that wait's live line.
    pub fn connecting(&self, name: &str) -> Connecting {
        let mut terminal = self.0.terminal.lock().expect("output not poisoned");
        if !self.terminal() || self.0.quiet || terminal.active.is_some() {
            return Connecting(None);
        }
        let mut stderr = io::stderr().lock();
        connection_line(&self.0.err, &mut terminal, &mut stderr, name);
        Connecting(Some(self.clone()))
    }

    /// Presents a pairing link and QR code before its count-up instruction.
    pub fn pairing(&self, url: &str) {
        self.end_stage(None, None);
        if self.terminal() {
            let mut terminal = self.0.terminal.lock().expect("output not poisoned");
            let mut stderr = io::stderr().lock();
            separate_result(&mut terminal, &mut stderr);
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
                    &format!(
                        "  {}",
                        self.0.err.paint(Role::Accent, style::printable(url))
                    ),
                    self.0.err.width,
                    2
                )
            );
            terminal.err_printed = true;
        }
        self.approve(
            "pair",
            "phone",
            if self.terminal() {
                "scan in Ark Companion".into()
            } else {
                format!("scan with Ark Companion: {url}")
            },
        );
    }

    /// Reads a yes or no key and echoes the answer as a word on the prompt line.
    pub fn confirm(&self, message: &str, default: bool) -> Result<bool, Error> {
        let mut input = prompt::Input::new()?;
        let result = prompt::confirm(
            &mut input,
            default,
            || self.prompt(message, default),
            |answer| {
                let mut stderr = io::stderr().lock();
                writeln!(stderr, "{}", if answer { "yes" } else { "no" })?;
                stderr.flush()?;
                Ok(())
            },
        );
        if result.is_err() {
            let mut stderr = io::stderr().lock();
            let _ = writeln!(stderr);
            let _ = stderr.flush();
        }
        result
    }

    /// Prints and flushes a prompt after ending live cursor activity.
    fn prompt(&self, message: &str, default: bool) -> Result<(), Error> {
        self.finish();
        let mut terminal = self.0.terminal.lock().expect("output not poisoned");
        let mut stderr = io::stderr().lock();
        separate_result(&mut terminal, &mut stderr);
        let theme = &self.0.err;
        let line = format!(
            "{} {} {}",
            theme.paint(Role::Attention, "?"),
            theme.inline(&style::printable(message)),
            theme.paint(Role::Muted, if default { "(Y/n)" } else { "(y/N)" })
        );
        write!(
            stderr,
            "{} ",
            style::wrap(&line, theme.width.saturating_sub(1), 2)
        )?;
        stderr.flush()?;
        terminal.err_printed = true;
        Ok(())
    }

    /// Reports an active failure, then its diagnostic with hints inside JSON errors.
    pub fn error(&self, error: &Error) {
        self.fail_stage();
        if self.json() {
            self.event_value(
                json!({"event":"error", "message":error.message, "error":error.json()}),
            );
            return;
        }
        let remote = error
            .remote
            .as_ref()
            .map(|remote| format!(" (code 0x{:x})", remote.code))
            .unwrap_or_default();
        let theme = &self.0.err;
        let prefix = format!("error[{}]:", error.code);
        let line = style::wrap(
            &format!(
                "{} {}{}",
                theme.paint(Role::Failure, &prefix),
                theme.inline(&style::printable(&error.message)),
                theme.paint(Role::Muted, remote)
            ),
            theme.width,
            prefix.len() + 1,
        );
        {
            let mut terminal = self.0.terminal.lock().expect("output not poisoned");
            emit(
                &mut terminal,
                &mut io::stderr().lock(),
                false,
                Value::Null,
                &line,
            );
        }
        for hint in &error.hints {
            self.event("hint", hint);
        }
    }

    /// Emits one preassembled JSON event without stopping an active stage.
    ///
    /// The caller uses it only in JSON mode.
    pub fn event_value(&self, value: Value) {
        let mut terminal = self.0.terminal.lock().expect("output not poisoned");
        emit(&mut terminal, &mut io::stderr().lock(), true, value, "");
    }

    /// Stops redraws and erases the temporary line without inventing an
    /// outcome; safe to call repeatedly.
    pub fn finish(&self) {
        let mut terminal = self.stop_ticker();
        close_line(&mut terminal, &mut io::stderr().lock());
    }

    /// Joins the worker before locking its display, retaining the active outcome.
    ///
    /// The worker stops under the ticker lock and is joined before the terminal
    /// lock is taken, since each redraw takes that lock too.
    fn stop_ticker(&self) -> MutexGuard<'_, Terminal> {
        let mut ticker = self.0.ticker.lock().expect("output not poisoned");
        drop(ticker.take());
        self.0.terminal.lock().expect("output not poisoned")
    }
}

/// Styles an event's mark or diagnostic label identically in terminals and pipes.
fn event_line(theme: &Theme, kind: &str, message: &str) -> String {
    let printable = style::printable(message);
    let message = theme.inline(&printable);
    let (prefix, message) = match kind {
        "progress" => (
            theme.glyph("›", ">").into(),
            message.replace(" · ", &theme.separator()),
        ),
        "approve" => (
            theme.paint(Role::Attention, "!"),
            inline_role(theme, Role::Attention, &printable),
        ),
        "step" => {
            return style::wrap(
                &format!("  {}", inline_role(theme, Role::Muted, &printable)),
                theme.width,
                2,
            );
        }
        "error" => (theme.paint(Role::Failure, "error:"), message),
        "warning" => (theme.paint(Role::Attention, "warning:"), message),
        "hint" => (theme.paint(Role::Accent, "hint:"), message),
        "note" | "log" => (theme.paint(Role::Muted, format!("{kind}:")), message),
        _ => (theme.paint(Role::Muted, theme.glyph("·", "-")), message),
    };
    style::wrap(
        &format!("{prefix} {message}"),
        theme.width,
        console::measure_text_width(&prefix) + 1,
    )
}

/// Styles the words plainly and every fact after the first separator as muted.
fn outcome_line(theme: &Theme, message: &str, failed: bool) -> String {
    let message = style::printable(message);
    let text = if let Some((words, facts)) = message.split_once(" · ") {
        format!(
            "{}{}{}",
            theme.inline(words),
            theme.separator(),
            inline_role(theme, Role::Muted, facts)
        )
    } else {
        theme.inline(&message)
    };
    let line = theme.mark(if failed { Role::Failure } else { Role::Success }, &text);
    style::wrap(
        &line,
        theme.width,
        if theme.unicode || failed { 2 } else { 3 },
    )
}

/// Restores surrounding emphasis after each inline command's accent reset.
fn inline_role(theme: &Theme, role: Role, text: &str) -> String {
    let text = theme
        .inline(text)
        .replace("\x1b[0m", &format!("\x1b[0m{}", theme.style(role)));
    theme.paint(role, text)
}

/// Erases a live line without leaving a bar or timer behind.
fn close_line(terminal: &mut Terminal, output: &mut impl Write) {
    let rows = std::mem::take(&mut terminal.live);
    if rows > 0 {
        let _ = write!(output, "{}", style::CLEAR_LINE);
        for _ in 1..rows {
            let _ = write!(output, "\x1b[1A{}", style::CLEAR_LINE);
        }
        let _ = output.flush();
    }
}

/// Separates the next stderr event from a result exactly once.
fn separate_result(terminal: &mut Terminal, output: &mut impl Write) {
    if std::mem::take(&mut terminal.out_block) {
        let _ = writeln!(output);
    }
}

/// Writes a result alone to stdout, putting page separators on stderr.
fn write_result(
    terminal: &mut Terminal,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
    text: &str,
    json: bool,
) -> io::Result<()> {
    close_line(terminal, stderr);
    if !json && terminal.err_printed {
        writeln!(stderr)?;
        stderr.flush()?;
    }
    writeln!(stdout, "{text}")?;
    stdout.flush()?;
    terminal.out_block = !json;
    Ok(())
}

/// Writes and flushes a persistent event, preserving active narration for redraw.
fn emit(terminal: &mut Terminal, output: &mut impl Write, json: bool, event: Value, line: &str) {
    close_line(terminal, output);
    if json {
        let _ = writeln!(output, "{event}");
    } else {
        separate_result(terminal, output);
        let _ = writeln!(output, "{line}");
    }
    terminal.err_printed = true;
    let _ = output.flush();
}

/// Writes an ephemeral connection label before any connection work starts.
fn connection_line(theme: &Theme, terminal: &mut Terminal, output: &mut impl Write, name: &str) {
    close_line(terminal, output);
    separate_result(terminal, output);
    let line = format!(
        "{} Connecting to {}",
        theme.glyph("›", ">"),
        theme.inline(&style::printable(name))
    );
    let _ = write!(
        output,
        "{}",
        theme.truncate(&line, theme.width.saturating_sub(1))
    );
    terminal.live = 1;
    let _ = output.flush();
}

/// Draws the latest activity on a terminal or at the pipe's five-second interval.
fn draw(
    theme: &Theme,
    terminal: &mut Terminal,
    output: &mut impl Write,
    now: Instant,
    json: bool,
    quiet: bool,
    force: bool,
) {
    let Some(active) = terminal.active.as_mut() else {
        return;
    };
    if quiet && active.approval.is_none() {
        return;
    }
    // A pipe repeats machine waits but prints an approval instruction only once
    let interval = Duration::from_secs(if theme.interactive { 1 } else { 5 });
    if active.drawn.is_some_and(|last| {
        (!theme.interactive && active.approval.is_some())
            || (!force && now.saturating_duration_since(last) < interval)
    }) {
        return;
    }
    active.drawn = Some(now);
    // Timers are presentation facts; they never enforce a device's window
    let mut update = active.update.clone();
    if let Some(timer) = active.timer {
        let fact = match timer {
            Wait::Elapsed => {
                style::duration(now.saturating_duration_since(active.started).as_secs())
            }
            Wait::Deadline(until) => {
                let seconds = until.saturating_duration_since(now).as_secs();
                update.facts.remaining_seconds = Some(seconds);
                format!("{} left", style::duration(seconds))
            }
        };
        if theme.interactive || active.approval.is_none() {
            update.details.push(fact);
        }
    }
    // Keep machine messages whole while the reading view fits its available width
    let message = update.message();
    let (event, line) = if let Some((action, location)) = &active.approval {
        let instruction = format!(
            "{} {}",
            theme.paint(Role::Attention, "!"),
            inline_role(theme, Role::Attention, &style::printable(&update.label))
        );
        let line = format!(
            "{instruction}{}",
            if theme.interactive {
                format!(
                    "{}{}",
                    theme.separator(),
                    theme.paint(Role::Muted, update.details.join(" · "))
                )
            } else {
                String::new()
            }
        );
        (
            json!({"event":"approve", "message":message, "action":action, "where":location}),
            line,
        )
    } else {
        (
            update.facts.event(&message),
            if theme.interactive {
                update.render(theme)
            } else {
                event_line(theme, "progress", &message)
            },
        )
    };
    // Replace the terminal frame, or append a complete pipe or JSON event
    if theme.interactive {
        close_line(terminal, output);
        separate_result(terminal, output);
        let line = if terminal
            .active
            .as_ref()
            .is_some_and(|active| active.approval.is_some())
        {
            style::wrap(&line, theme.width.saturating_sub(1), 2)
        } else {
            theme.truncate(&line, theme.width.saturating_sub(1))
        };
        let _ = write!(output, "{line}");
        terminal.live = line.lines().count().max(1);
        let _ = output.flush();
    } else {
        emit(
            terminal,
            output,
            json,
            event,
            &style::wrap(&line, theme.width, 2),
        );
    }
}

/// Turns the active activity into one outcome, keeping its structured facts.
#[allow(clippy::too_many_arguments)] // The renderer takes stream policy and an explicit clock for tests.
fn end_activity(
    theme: &Theme,
    terminal: &mut Terminal,
    output: &mut impl Write,
    now: Instant,
    json: bool,
    quiet: bool,
    message: Option<&str>,
    fact: Option<&str>,
    failed: bool,
) {
    let Some(active) = terminal.active.take() else {
        close_line(terminal, output);
        return;
    };
    close_line(terminal, output);
    if quiet {
        return;
    }
    let words = message.map(str::to_owned).unwrap_or_else(|| {
        if let Some((action, location)) = &active.approval {
            let place = if *location == "button" {
                "with the Ark's button"
            } else {
                "on your phone"
            };
            if failed {
                format!("{action} approval failed {place}")
            } else if action == "pair" && *location == "phone" {
                "scanned in Ark Companion".into()
            } else {
                format!("Approved {place}")
            }
        } else {
            active.update.label.clone()
        }
    });
    let fact = fact.map(str::to_owned).unwrap_or_else(|| {
        style::duration(now.saturating_duration_since(active.started).as_secs())
    });
    let message = format!("{words} · {fact}");
    emit(
        terminal,
        output,
        json,
        active.update.facts.event(&message),
        &outcome_line(theme, &message, failed),
    );
}

/// Formats printable scalar values without field-specific units.
pub(crate) fn scalar(value: &Value) -> String {
    match value {
        Value::Null => "-".into(),
        Value::Bool(true) => "yes".into(),
        Value::Bool(false) => "no".into(),
        Value::String(value) => style::printable(value),
        Value::Array(values) => values.iter().map(scalar).collect::<Vec<_>>().join(", "),
        value => value.to_string(),
    }
}

/// Tests of narration and composition using explicit clock instants and streams.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::Color;
    use crate::testing::wait_deadline;
    use darkbio_clock::TestClock;

    /// Builds an activity whose start is controlled by the test.
    fn activity(started: Instant) -> Activity {
        Activity {
            update: Update::new("sorting", "Sorting"),
            started,
            timer: None,
            approval: None,
            drawn: None,
        }
    }

    /// Every event has the same mark or diagnostic label in a terminal and pipe.
    #[test]
    fn test_event_lines() {
        for interactive in [true, false] {
            let theme = Theme {
                interactive,
                ..Theme::test(80, if interactive { Color::True } else { Color::Off }, true)
            };
            for (kind, expected) in [
                ("progress", "› message"),
                ("approve", "! message"),
                ("step", "  message"),
                ("note", "note: message"),
                ("warning", "warning: message"),
                ("hint", "hint: message"),
                ("log", "log: message"),
                ("error", "error: message"),
            ] {
                assert_eq!(
                    console::strip_ansi_codes(&event_line(&theme, kind, "message")),
                    expected,
                    "{kind}"
                );
            }
            assert!(event_line(&theme, "hint", "run `ark unlock`\nnext").contains("\\nnext"));
        }
        let theme = Theme::test(80, Color::True, true);
        assert_eq!(event_line(&theme, "progress", "Connecting"), "› Connecting");
        assert_eq!(
            event_line(&theme, "step", "reading"),
            "  \x1b[38;2;124;128;152mreading\x1b[0m"
        );
        assert_eq!(
            event_line(&theme, "approve", "Approve"),
            "\x1b[1m\x1b[38;2;232;162;74m!\x1b[0m \x1b[1m\x1b[38;2;232;162;74mApprove\x1b[0m"
        );
    }

    /// Stages end with a duration, an explicit fact or a failure instead of a bar.
    #[test]
    fn test_stage_lifecycle() {
        let start = TestClock::new().clock().now();
        let theme = Theme::test(80, Color::Off, true);
        for (fact, failed, expected) in [
            (None, false, "✓ Sorted · 1m 02s\n"),
            (
                Some("high confidence"),
                false,
                "✓ Sorted · high confidence\n",
            ),
            (None, true, "✗ Sorted · 1m 02s\n"),
        ] {
            let mut terminal = Terminal {
                active: Some(activity(start)),
                ..Default::default()
            };
            let mut bytes = Vec::new();
            draw(&theme, &mut terminal, &mut bytes, start, false, false, true);
            assert_eq!(String::from_utf8(bytes.clone()).unwrap(), "› Sorting");
            bytes.clear();
            end_activity(
                &theme,
                &mut terminal,
                &mut bytes,
                start + Duration::from_secs(62),
                false,
                false,
                Some("Sorted"),
                fact,
                failed,
            );
            assert_eq!(
                String::from_utf8(bytes).unwrap(),
                format!("{}{expected}", style::CLEAR_LINE)
            );
            assert!(terminal.active.is_none());
        }
    }

    /// Approvals count up on one terminal line and print once in a pipe.
    #[test]
    fn test_approval_lifecycle() {
        let start = TestClock::new().clock().now();
        for interactive in [true, false] {
            let theme = Theme {
                interactive,
                ..Theme::test(100, Color::Off, true)
            };
            let mut active = activity(start);
            active.update = Update::new("upload", "Approve the upload on your phone");
            active.approval = Some(("upload".into(), "phone"));
            active.timer = Some(Wait::Elapsed);
            let mut terminal = Terminal {
                active: Some(active),
                ..Default::default()
            };
            let mut bytes = Vec::new();
            draw(&theme, &mut terminal, &mut bytes, start, false, false, true);
            draw(
                &theme,
                &mut terminal,
                &mut bytes,
                start + Duration::from_secs(12),
                false,
                false,
                false,
            );
            end_activity(
                &theme,
                &mut terminal,
                &mut bytes,
                start + Duration::from_secs(14),
                false,
                false,
                None,
                None,
                false,
            );
            let text = String::from_utf8(bytes).unwrap();
            if interactive {
                assert_eq!(
                    text,
                    format!(
                        "! Approve the upload on your phone · 0s{}! Approve the upload on your phone · 12s{}✓ Approved on your phone · 14s\n",
                        style::CLEAR_LINE,
                        style::CLEAR_LINE
                    )
                );
            } else {
                assert_eq!(
                    text,
                    "! Approve the upload on your phone\n✓ Approved on your phone · 14s\n"
                );
            }
        }
    }

    /// Failed approvals keep their elapsed outcome before the following error.
    #[test]
    fn test_approval_failure() {
        let start = TestClock::new().clock().now();
        let theme = Theme {
            interactive: false,
            ..Theme::test(100, Color::Off, true)
        };
        let mut terminal = Terminal {
            active: Some(Activity {
                update: Update::new("unlock", "Approve the unlock on your phone"),
                approval: Some(("unlock".into(), "phone")),
                timer: Some(Wait::Elapsed),
                ..activity(start)
            }),
            ..Default::default()
        };
        let mut bytes = Vec::new();
        draw(&theme, &mut terminal, &mut bytes, start, false, false, true);
        end_activity(
            &theme,
            &mut terminal,
            &mut bytes,
            start + Duration::from_secs(14),
            false,
            false,
            None,
            None,
            true,
        );
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "! Approve the unlock on your phone\n✗ unlock approval failed on your phone · 14s\n"
        );
        assert!(terminal.active.is_none());
    }

    /// Finished facts are muted while the stage's words remain plain.
    #[test]
    fn test_finished_line_roles() {
        let theme = Theme::test(80, Color::True, true);
        assert_eq!(
            outcome_line(&theme, "Uploaded 293.3 MiB · 22s", false),
            "\x1b[1m\x1b[38;2;148;202;110m✓\x1b[0m Uploaded 293.3 MiB\x1b[38;2;124;128;152m · \x1b[0m\x1b[38;2;124;128;152m22s\x1b[0m"
        );
    }

    /// Timers count up and only caller-owned deadlines count down.
    #[test]
    fn test_timer_and_countdown() {
        let start = TestClock::new().clock().now();
        let theme = Theme::test(80, Color::Off, true);
        for (timer, expected) in [
            (Wait::Elapsed, "› Sorting · 1m 02s"),
            (
                Wait::Deadline(start + Duration::from_secs(148)),
                "› Sorting · 1m 26s left",
            ),
        ] {
            let mut terminal = Terminal {
                active: Some(Activity {
                    timer: Some(timer),
                    ..activity(start)
                }),
                ..Default::default()
            };
            let mut bytes = Vec::new();
            draw(
                &theme,
                &mut terminal,
                &mut bytes,
                start + Duration::from_secs(62),
                false,
                false,
                true,
            );
            assert_eq!(String::from_utf8(bytes).unwrap(), expected);
        }
    }

    /// Pipe wait updates use five-second intervals without cursor control.
    #[test]
    fn test_pipe_interval_lines() {
        let start = TestClock::new().clock().now();
        let theme = Theme {
            interactive: false,
            ..Theme::test(80, Color::Off, true)
        };
        let mut terminal = Terminal {
            active: Some(Activity {
                timer: Some(Wait::Elapsed),
                ..activity(start)
            }),
            ..Default::default()
        };
        let mut bytes = Vec::new();
        for seconds in [0, 1, 4, 5, 6, 10] {
            draw(
                &theme,
                &mut terminal,
                &mut bytes,
                start + Duration::from_secs(seconds),
                false,
                false,
                false,
            );
        }
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "› Sorting · 0s\n› Sorting · 5s\n› Sorting · 10s\n"
        );
    }

    /// All result layouts put their separating blank lines on stderr in pipes.
    #[test]
    fn test_composition_separates_only_on_stderr() {
        let theme = Theme {
            interactive: false,
            ..Theme::test(80, Color::Off, true)
        };
        let value = json!({"name":"Example"});
        for result in [
            human::document(&theme, &value),
            human::table(&theme, &[value], &[("NAME", "name")]),
            human::checklist(&theme, &[("Ark", &[])]),
        ] {
            for (kind, expected) in [
                ("note", "note: after"),
                ("warning", "warning: after"),
                ("hint", "hint: after"),
                ("step", "  after"),
                ("log", "log: after"),
                ("approve", "! after"),
                ("progress", "› after"),
                ("error", "error: after"),
            ] {
                let mut terminal = Terminal::default();
                let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
                emit(
                    &mut terminal,
                    &mut stderr,
                    false,
                    Value::Null,
                    "note: before",
                );
                write_result(&mut terminal, &mut stdout, &mut stderr, &result, false).unwrap();
                emit(
                    &mut terminal,
                    &mut stderr,
                    false,
                    Value::Null,
                    &event_line(&theme, kind, "after"),
                );
                emit(&mut terminal, &mut stderr, false, Value::Null, "hint: last");
                assert_eq!(String::from_utf8(stdout).unwrap(), format!("{result}\n"));
                assert_eq!(
                    String::from_utf8(stderr).unwrap(),
                    format!("note: before\n\n\n{expected}\nhint: last\n")
                );
            }
        }
    }

    /// The connection notice is erased without becoming persistent page content.
    #[test]
    fn test_connecting_line_erased() {
        let mut terminal = Terminal::default();
        let mut bytes = Vec::new();
        connection_line(
            &Theme::test(80, Color::Off, true),
            &mut terminal,
            &mut bytes,
            "Sample Ark",
        );
        close_line(&mut terminal, &mut bytes);
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            format!("› Connecting to Sample Ark{}", style::CLEAR_LINE)
        );
        assert!(!terminal.err_printed);
    }

    /// JSON approvals and their progress outcomes retain machine facts.
    #[test]
    fn test_json_approval_and_outcome() {
        let start = TestClock::new().clock().now();
        let theme = Theme {
            interactive: false,
            ..Theme::test(80, Color::Off, false)
        };
        let mut terminal = Terminal {
            active: Some(Activity {
                approval: Some(("upload".into(), "phone")),
                timer: Some(Wait::Elapsed),
                ..activity(start)
            }),
            ..Default::default()
        };
        let mut bytes = Vec::new();
        draw(&theme, &mut terminal, &mut bytes, start, true, false, true);
        end_activity(
            &theme,
            &mut terminal,
            &mut bytes,
            start + Duration::from_secs(14),
            true,
            false,
            None,
            None,
            false,
        );
        let text = String::from_utf8(bytes).unwrap();
        let events: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            events[0],
            json!({"event":"approve","message":"Sorting","action":"upload","where":"phone"})
        );
        assert_eq!(events[1]["event"], "progress");
        assert_eq!(events[1]["message"], "Approved on your phone · 14s");
        for key in [
            "phase",
            "step",
            "steps",
            "percent",
            "done_bytes",
            "total_bytes",
            "remaining_seconds",
        ] {
            assert_eq!(events[1].get(key), Some(&Value::Null), "{key}");
        }
    }

    /// Long approval instructions wrap whole and erase every occupied row.
    #[test]
    fn test_approval_wraps_without_losing_instructions() {
        let start = TestClock::new().clock().now();
        let theme = Theme::test(38, Color::True, true);
        let mut active = activity(start);
        active.update.label =
            "Approve with `ark-emulator button press` after checking the colors".into();
        active.approval = Some(("pair".into(), "button"));
        active.timer = Some(Wait::Elapsed);
        let mut terminal = Terminal {
            active: Some(active),
            ..Default::default()
        };
        let mut bytes = Vec::new();
        draw(&theme, &mut terminal, &mut bytes, start, false, false, true);
        let rendered = String::from_utf8(bytes).unwrap();
        let plain = console::strip_ansi_codes(&rendered);
        assert_eq!(
            plain.split_whitespace().collect::<Vec<_>>().join(" "),
            "! Approve with `ark-emulator button press` after checking the colors · 0s"
        );
        assert!(
            plain
                .lines()
                .all(|line| console::measure_text_width(line) < 38)
        );
        assert!(rendered.contains("\x1b[38;2;232;162;74m` after"));
        let rows = terminal.live;
        assert!(rows > 1);
        let mut erased = Vec::new();
        close_line(&mut terminal, &mut erased);
        let erased = String::from_utf8(erased).unwrap();
        assert_eq!(erased.matches(style::CLEAR_LINE).count(), rows);
        assert_eq!(erased.matches("\x1b[1A").count(), rows - 1);
    }

    /// Public output APIs retain JSON facts, quiet policy and complete errors.
    #[test]
    fn test_output_event_contract() {
        use clap::Parser;

        // A child process captures the actual stderr and sole stdout document
        if let Ok(mode) = std::env::var("ARK_TEST_NARRATION") {
            let mut options = crate::args::Cli::parse_from(["ark", "--json", "-v"]).options;
            options.quiet = mode == "quiet";
            options.verbose = !options.quiet;
            let output = Output::new(&options);
            let tester = TestClock::new();
            output.connection(tester.clock());
            drop(output.connecting("Sample Ark"));
            output.stage("uploading", "Uploading");
            output.event("note", "note");
            output.event("warning", "warning");
            output.event("step", "step");
            output.approve("upload", "phone", "Approve the upload on your phone");
            output.event("log", "log");
            output.end_stage(None, None);
            output.event("hint", "independent hint");
            let error =
                Error::new(6, "approval-timeout", "approval expired").hint("have your phone ready");
            writeln!(io::stdout(), "<result>").unwrap();
            output.document(&json!({"error":error.json()})).unwrap();
            writeln!(io::stdout(), "</result>").unwrap();
            output.error(&error);
            return;
        }

        // Error hints belong to the identical object on both streams
        for mode in ["verbose", "quiet"] {
            let captured = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "output::tests::test_output_event_contract",
                    "--nocapture",
                ])
                .env("ARK_TEST_NARRATION", mode)
                .output()
                .unwrap();
            assert!(captured.status.success(), "{captured:?}");
            let stdout = String::from_utf8(captured.stdout).unwrap();
            let document: Value = serde_json::from_str(
                stdout
                    .split_once("<result>\n")
                    .unwrap()
                    .1
                    .split_once("</result>\n")
                    .unwrap()
                    .0,
            )
            .unwrap();
            let stderr = String::from_utf8(captured.stderr).unwrap();
            let events: Vec<Value> = stderr
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(events.last().unwrap()["error"], document["error"]);
            assert_eq!(document["error"]["hints"], json!(["have your phone ready"]));
            let hints: Vec<_> = events
                .iter()
                .filter(|event| event["event"] == "hint")
                .collect();
            assert_eq!(hints.len(), 1);
            assert_eq!(hints[0]["message"], "independent hint");
            let kinds: Vec<_> = events
                .iter()
                .map(|event| event["event"].as_str().unwrap())
                .collect();
            assert_eq!(
                kinds,
                if mode == "quiet" {
                    vec!["approve", "log", "hint", "error"]
                } else {
                    vec![
                        "progress", "note", "warning", "step", "progress", "approve", "log",
                        "progress", "hint", "error",
                    ]
                }
            );
            for event in events.iter().filter(|event| event["event"] == "progress") {
                assert!(event["stage"].is_string());
                for key in [
                    "phase",
                    "step",
                    "steps",
                    "percent",
                    "done_bytes",
                    "total_bytes",
                    "remaining_seconds",
                ] {
                    assert_eq!(event.get(key), Some(&Value::Null), "{key}");
                }
            }
            let approve = events
                .iter()
                .find(|event| event["event"] == "approve")
                .unwrap();
            assert_eq!(approve["action"], "upload");
            assert_eq!(approve["where"], "phone");
            assert!(!stderr.contains("Connecting"));
        }
    }

    /// The redraw worker follows the injected clock and stops when dropped.
    #[test]
    fn test_ticker_uses_test_clock() {
        let mut tester = TestClock::new();
        let start = tester.clock().now();
        let (send, recv) = std::sync::mpsc::channel();
        let ticker = Ticker::start(tester.clock(), move |now| send.send(now).unwrap());
        wait_deadline(&tester, start + Duration::from_secs(1));
        tester.advance_to(start + Duration::from_secs(1));
        assert_eq!(recv.recv().unwrap(), start + Duration::from_secs(1));
        drop(ticker);
        assert_eq!(tester.next_deadline(), None);
    }
}
