// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Output layer that prints one result as a reading view or JSON, with
//! diagnostics confined to stderr.

pub(crate) mod human;

use crate::args::Options;
use crate::error::Error;
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

/// Clonable output handle for one invocation.
///
/// The first result claims stdout, and later ones are ignored. Events and live
/// stderr lines share terminal state across clones.
#[derive(Clone)]
pub(crate) struct Output(
    /// Output state shared by every clone.
    Arc<State>,
);

/// Immutable stream policy and synchronization shared by output handles.
struct State {
    /// Whether stdout uses a JSON document and stderr uses JSON events.
    json: bool,
    /// Capabilities of stdout, resolved independently of stderr.
    out: Theme,
    /// Capabilities of stderr, including live progress support.
    err: Theme,
    /// Whether optional events are hidden, keeping approvals, errors, hints
    /// and logs.
    quiet: bool,
    /// Whether step narration is enabled.
    verbose: bool,
    /// Whether a caller already claimed the sole result, even if its write failed.
    printed: AtomicBool,
    /// Whether a non-release route was announced during this command.
    environment_noted: AtomicBool,
    /// Lock that serializes result claims and writes, taken before the ticker
    /// and terminal locks.
    result: Mutex<()>,
    /// Clock of the latest connection, which times every wait display.
    clock: Mutex<Option<Clock>>,
    /// Worker redrawing the active wait.
    ///
    /// Its lock orders every change of the wait display with the worker's
    /// replacement, and comes before the terminal lock.
    ticker: Mutex<Option<Ticker>>,
    /// Terminal layout, whose lock serializes stderr line changes and spacing
    /// around human result blocks.
    ///
    /// The redraw worker shares it, but never the rest of the output.
    terminal: Arc<Mutex<Terminal>>,
}

/// Owner of a worker that redraws a wait display once a second on a clock.
///
/// Dropping it stops the worker and waits for it to exit. The worker holds
/// only what it draws with, never its owner, so the owner is never dropped on
/// the worker's own thread.
struct Ticker {
    /// Sender of the stop channel, whose drop wakes the worker to exit.
    stop: Option<crossbeam_channel::Sender<()>>,
    /// Worker thread, joined on drop so no redraw outlives the display.
    worker: Option<JoinHandle<()>>,
}

impl Ticker {
    /// Starts a worker that draws a second after its start and then a second
    /// after each draw, until the owner drops it.
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
    /// Wakes the worker and waits for it to exit.
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Current terminal layout, guarded independently of the result claim.
#[derive(Default)]
struct Terminal {
    /// Stage key and whether the unterminated stderr line is temporary.
    live: Option<(String, bool)>,
    /// Active elapsed-time or countdown display, if any.
    waiting: Option<Waiting>,
    /// Whether stderr has printed content that needs spacing before a result.
    err_printed: bool,
    /// Whether a human stdout block needs separation from the next stderr event.
    out_block: bool,
}

/// Presentation-only wait state; it never controls an operation's deadline.
struct Waiting {
    /// Short activity name displayed beside the elapsed or remaining time.
    label: String,
    /// Clock time when this wait display began.
    started: Instant,
    /// Fixed countdown bound, absent for an elapsed-time display.
    until: Option<Instant>,
}

impl Output {
    /// Resolves each stream's capabilities and starts with no claimed result.
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
            clock: Mutex::new(None),
            ticker: Mutex::new(None),
            terminal: Arc::new(Mutex::new(Terminal::default())),
        }))
    }
    /// Times later wait displays on the clock of a newly opened connection.
    pub fn connection(&self, clock: Clock) {
        *self.0.clock.lock().expect("output not poisoned") = Some(clock);
    }
    /// Reports whether the caller requested JSON for both output streams.
    pub fn json(&self) -> bool {
        self.0.json
    }
    /// Reports whether stderr supports live progress.
    pub fn terminal(&self) -> bool {
        self.0.err.interactive
    }
    /// Reports whether a result was claimed, so failure reporting must not
    /// emit another.
    pub fn printed(&self) -> bool {
        self.0.printed.load(Ordering::SeqCst)
    }

    /// Notes a non-release environment once per command, however often it
    /// reconnects.
    pub fn environment(&self, env: Environment) {
        if let Some(message) = self.environment_note(env) {
            self.event("note", message);
        }
    }

    /// Claims the invocation's first non-release note unless quiet suppresses it.
    fn environment_note(&self, env: Environment) -> Option<String> {
        if self.0.quiet
            || env == Environment::Release
            || self.0.environment_noted.swap(true, Ordering::Relaxed)
        {
            return None;
        }
        Some(format!("using the {env} environment"))
    }

    /// Emits the sole result as a reading view or JSON.
    pub fn document(&self, value: &Value) -> Result<(), Error> {
        self.document_with(value, |theme| human::document(theme, value))
    }

    /// Emits the sole result, invoking the custom renderer only for human stdout.
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

    /// Renders rows as a table; JSON retains the complete document.
    pub fn table(
        &self,
        document: &Value,
        rows: &[Value],
        columns: &[(&str, &str)],
    ) -> Result<(), Error> {
        self.grouped_table(document, rows, columns, &[])
    }

    /// Renders rows as a table under group headings, given one group per row;
    /// JSON keeps the complete document.
    pub fn grouped_table(
        &self,
        document: &Value,
        rows: &[Value],
        columns: &[(&str, &str)],
        groups: &[String],
    ) -> Result<(), Error> {
        self.document_with(document, |theme| human::table(theme, rows, columns, groups))
    }

    /// Renders diagnostic rows with local hints, or the equivalent machine result.
    pub fn checklist(&self, document: &Value, rows: &[Value]) -> Result<(), Error> {
        self.document_with(document, |theme| human::checklist(theme, rows))
    }

    /// Ends live stderr activity and writes a complete stdout result block.
    ///
    /// The caller holds the result lock, which comes before the ticker and
    /// terminal locks.
    fn write_result(&self, text: &str) -> Result<(), Error> {
        let mut terminal = self.end_wait();
        close_line(&mut terminal, &mut io::stderr().lock());
        let mut stdout = io::stdout().lock();
        if self.0.out.interactive && self.0.err.interactive && terminal.err_printed {
            writeln!(stdout)?;
        }
        writeln!(stdout, "{text}")?;
        stdout.flush()?;
        terminal.out_block = self.0.out.interactive;
        Ok(())
    }

    /// Emits an app's report as the sole result, copying its bytes unchanged.
    ///
    /// The report goes to stdout without any layout. A nonempty app stderr
    /// follows on stderr, after a note with its size.
    pub fn app(&self, stdout: &[u8], stderr: &[u8]) -> Result<(), Error> {
        // Claim the sole result, ignoring every later attempt
        let _result = self.0.result.lock().expect("output not poisoned");
        if self.0.printed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }

        // End live terminal activity and copy the report to stdout
        self.finish();
        {
            let mut output = io::stdout().lock();
            output.write_all(stdout)?;
            output.flush()?;
        }

        // Copy a nonempty app stderr after a note with its size
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
    /// A single-line approval starts a wait display on an interactive terminal,
    /// unless quiet hides it.
    pub fn event(&self, kind: &str, message: impl AsRef<str>) {
        // Quiet hides the optional events, and steps need verbose
        if self.0.quiet && matches!(kind, "progress" | "note" | "warning" | "step") {
            return;
        }
        if kind == "step" && !self.0.verbose {
            return;
        }

        // JSON escapes on its own; the reading streams get a printable copy,
        // since a device name or verdict must not drive the terminal
        let message = message.as_ref();
        let shown = style::printable(message);

        // Progress on a terminal takes the live line of its stage, which the
        // text before the first colon names
        if kind == "progress" && self.terminal() {
            let theme = &self.0.err;
            let line = format!(
                "{} {}",
                theme.paint(Role::Muted, "progress:"),
                theme.inline(&shown)
            );
            self.progress_line(shown.split(':').next().unwrap_or(&shown), &line);
            return;
        }

        // Notes, warnings, steps and logs keep a wait display running, and
        // other events end it before their line
        {
            let mut terminal = if matches!(kind, "note" | "warning" | "step" | "log") {
                self.0.terminal.lock().expect("output not poisoned")
            } else {
                self.end_wait()
            };
            let mut stderr = io::stderr().lock();
            close_line(&mut terminal, &mut stderr);
            if self.json() {
                let _ = writeln!(stderr, "{}", json!({"event":kind,"message":message}));
            } else if self.terminal() {
                separate_result(&mut terminal, &mut stderr);
                let _ = writeln!(stderr, "{}", event_line(&self.0.err, kind, &shown));
            } else {
                let _ = writeln!(stderr, "{kind}: {shown}");
            }
            terminal.err_printed = true;
            let _ = stderr.flush();
        }

        // A single-line approval starts a wait display for its answer
        if kind == "approve" && !message.contains('\n') {
            self.wait("waiting", None);
        }
    }

    /// Shows an update as a live terminal line, or elsewhere as a progress
    /// event with its stable text.
    pub fn progress(&self, update: &crate::progress::Update) {
        if self.terminal() {
            self.progress_line(&update.stage, &update.render(&self.0.err));
        } else {
            self.event("progress", &update.text);
        }
    }

    /// Replaces the same live stage in place, preserving a completed previous stage.
    ///
    /// Redirected human output appends lines without terminal control sequences.
    fn progress_line(&self, key: &str, line: &str) {
        if self.0.quiet {
            return;
        }

        // Erase the same stage's line for a redraw, or close an earlier one
        let mut terminal = self.end_wait();
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

        // Fit the line to the width, leaving it open on a terminal for the next
        // update
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

    /// Emits an event only on a terminal, for human detail that has no
    /// counterpart in the machine event stream.
    pub fn human_event(&self, kind: &str, message: impl AsRef<str>) {
        if self.terminal() {
            self.event(kind, message);
        }
    }

    /// Starts an optional human stderr section after finishing prior live activity.
    pub fn title(&self, title: &str) {
        if !self.terminal() || self.0.quiet {
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

    /// Shows a pairing stage, retaining its final line when the stage completes.
    pub fn stage(&self, name: &str, done: bool) {
        if !self.terminal() {
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

    /// Presents a pairing URL to scan, with a QR code when Unicode and the
    /// terminal width permit.
    ///
    /// It announces the approval first and then counts down to `deadline`. The
    /// caller uses it only on a terminal; elsewhere the URL goes out in an
    /// approval event.
    pub fn pairing(&self, url: &str, deadline: Instant) {
        // Announce the approval and close any live line
        self.event("approve", "scan in Ark Companion");
        self.finish();

        // Draw the QR code when Unicode is on and every line fits, then the URL
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

        // Count down to the scan deadline
        self.wait("scan", Some(deadline));
    }

    /// Shows a wait display counting the elapsed time, or down to `until`.
    ///
    /// The display is presentation only and never bounds the call. It needs an
    /// interactive stderr outside quiet mode, runs on the latest connection's
    /// clock and stays hidden before a connection exists.
    pub fn wait(&self, label: &str, until: Option<Instant>) {
        if !self.0.err.interactive || self.0.quiet {
            return;
        }
        self.show_wait(label, until, || io::stderr().lock());
    }

    /// Replaces any earlier wait display with one drawn to `screen`, which the
    /// worker then redraws every second.
    ///
    /// The replacement happens under the ticker lock, so a concurrent change of
    /// the display lands before or after it as a whole.
    fn show_wait<W: Write>(
        &self,
        label: &str,
        until: Option<Instant>,
        screen: impl Fn() -> W + Send + 'static,
    ) {
        // Without a connection's clock, no wait display shows
        let Some(clock) = self.0.clock.lock().expect("output not poisoned").clone() else {
            return;
        };

        // Stop the earlier worker before touching the terminal, which it draws on
        let mut ticker = self.0.ticker.lock().expect("output not poisoned");
        drop(ticker.take());

        // Publish the new display and draw its first frame at once
        {
            let mut terminal = self.0.terminal.lock().expect("output not poisoned");
            let now = clock.now();
            terminal.waiting = Some(Waiting {
                label: label.into(),
                started: now,
                until,
            });
            tick(&self.0.err, &mut terminal, &mut screen(), now);
        }

        // Install the worker before another change can take the ticker lock
        let theme = self.0.err.clone();
        let terminal = self.0.terminal.clone();
        *ticker = Some(Ticker::start(clock, move |now| {
            let mut terminal = terminal.lock().expect("output not poisoned");
            tick(&theme, &mut terminal, &mut screen(), now);
        }));
    }

    /// Prints and flushes a prompt without reading stdin or deciding whether to ask.
    pub fn prompt(&self, message: &str, default: bool) -> Result<(), Error> {
        self.finish();
        let choices = if default { "Y/n" } else { "y/N" };
        let mut stderr = io::stderr().lock();
        if self.terminal() {
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

    /// Reports a failure and its hints on stderr without claiming a stdout result.
    pub fn error(&self, error: &Error) {
        // End live activity, then write the error as a JSON event or a
        // printable line
        self.finish();
        if self.json() {
            self.event_value(json!({"event":"error", "error":error.json()}));
        } else {
            let remote = error
                .remote
                .as_ref()
                .map(|remote| format!(" (code 0x{:x})", remote.code))
                .unwrap_or_default();
            let message = style::printable(&error.message);
            let mut terminal = self.0.terminal.lock().expect("output not poisoned");
            let mut stderr = io::stderr().lock();
            if self.terminal() {
                separate_result(&mut terminal, &mut stderr);
                let theme = &self.0.err;
                let line = format!(
                    "{} {}{}",
                    theme.paint(Role::Failure, format!("error[{}]:", error.code)),
                    theme.inline(&message),
                    theme.paint(Role::Muted, &remote)
                );
                let _ = writeln!(stderr, "{}", style::wrap(&line, theme.width, 2));
            } else {
                let _ = writeln!(stderr, "error[{}]: {message}{remote}", error.code);
            }
            terminal.err_printed = true;
        }

        // Hints follow as events of their own
        for hint in &error.hints {
            self.event("hint", hint);
        }
    }

    /// Writes one preassembled JSON event after ending live terminal activity.
    ///
    /// The caller uses it only in JSON mode.
    pub fn event_value(&self, value: Value) {
        self.finish();
        let _ = writeln!(io::stderr().lock(), "{value}");
    }

    /// Stops the wait display and closes the live line; safe to call repeatedly.
    pub fn finish(&self) {
        let mut terminal = self.end_wait();
        let mut stderr = io::stderr().lock();
        close_line(&mut terminal, &mut stderr);
        let _ = stderr.flush();
    }

    /// Ends the wait display and returns the terminal for the caller's next
    /// write.
    ///
    /// The worker is stopped under the ticker lock and joined before the
    /// terminal lock is taken, since each redraw takes that lock too.
    fn end_wait(&self) -> MutexGuard<'_, Terminal> {
        let mut ticker = self.0.ticker.lock().expect("output not poisoned");
        drop(ticker.take());
        let mut terminal = self.0.terminal.lock().expect("output not poisoned");
        terminal.waiting = None;
        terminal
    }
}

/// Styles and wraps one human event while preserving its recognizable prefix.
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

/// Erases a temporary timer or terminates persistent progress with a newline.
fn close_line(terminal: &mut Terminal, output: &mut impl Write) {
    if let Some((_, temporary)) = terminal.live.take() {
        if temporary {
            let _ = write!(output, "{}", style::CLEAR_LINE);
        } else {
            let _ = writeln!(output);
        }
    }
}

/// Separates the first stderr event after a human stdout block, once.
fn separate_result(terminal: &mut Terminal, output: &mut impl Write) {
    if terminal.out_block {
        let _ = writeln!(output);
        terminal.out_block = false;
    }
}

/// Redraws the active wait within terminal width without changing its deadline.
fn tick(theme: &Theme, terminal: &mut Terminal, output: &mut impl Write, now: Instant) {
    let Some(wait) = &terminal.waiting else {
        return;
    };

    // Count down to the bound, or up from the start
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

    // Replace the previous frame with a temporary line that the next write
    // erases
    close_line(terminal, output);
    let _ = write!(
        output,
        "{}",
        theme.truncate(&line, theme.width.saturating_sub(1))
    );
    terminal.live = Some(("waiting".into(), true));
    let _ = output.flush();
}

/// Formats scalar values and lists without terminal styling or field-specific units.
///
/// Text is made printable here, since the document and table layouts pass
/// through it.
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

/// Tests of wait displays, event lines and the environment note.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::Color;
    use crate::testing::wait_deadline;
    use clap::Parser;
    use darkbio_clock::TestClock;
    use std::sync::{TryLockError, mpsc};
    use std::thread;

    /// In-memory screen holding the frames that wait displays draw in place of
    /// stderr.
    #[derive(Clone, Default)]
    struct Screen(
        /// Bytes written so far, shared by every clone.
        Arc<Mutex<Vec<u8>>>,
    );

    impl Screen {
        /// Returns the frames drawn so far, oldest first.
        fn frames(&self) -> Vec<String> {
            String::from_utf8(self.0.lock().unwrap().clone())
                .unwrap()
                .split(style::CLEAR_LINE)
                .filter(|frame| !frame.is_empty())
                .map(String::from)
                .collect()
        }
    }

    impl Write for Screen {
        /// Appends every byte to the shared screen buffer.
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        /// Returns at once, since the screen holds no deferred output.
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Builds an output whose waits run on the clock.
    fn output(clock: Clock) -> Output {
        let options = crate::args::Cli::try_parse_from(["ark"]).unwrap().options;
        let output = Output::new(&options);
        output.connection(clock);
        output
    }

    /// Shows a wait on a thread of its own, drawing it on the screen.
    fn spawn_wait(output: &Output, label: &'static str, screen: &Screen) -> thread::JoinHandle<()> {
        let (output, screen) = (output.clone(), screen.clone());
        thread::spawn(move || output.show_wait(label, None, move || screen.clone()))
    }

    /// Waits until another thread holds the output's ticker lock.
    fn ticker_held(output: &Output) {
        loop {
            match output.0.ticker.try_lock() {
                Err(TryLockError::WouldBlock) => return,
                free => drop(free),
            }
            thread::yield_now();
        }
    }

    /// A wait arriving while another replaces the display lands after it, and
    /// its sole worker stops when the output drops.
    #[test]
    fn test_concurrent_waits_keep_one_worker() {
        // Hold the terminal, so the first wait stops inside its replacement
        let mut tester = TestClock::new();
        let start = tester.clock().now();
        let output = output(tester.clock());
        let screen = Screen::default();
        let terminal = output.0.terminal.lock().unwrap();
        let first = spawn_wait(&output, "first", &screen);
        ticker_held(&output);

        // The second wait queues behind the first one's replacement
        let second = spawn_wait(&output, "second", &screen);
        drop(terminal);
        first.join().unwrap();
        second.join().unwrap();
        let frames = screen.frames();
        assert_eq!(frames.len(), 2);
        assert!(frames[0].contains("first") && frames[0].contains("0 s elapsed"));
        assert!(frames[1].contains("second") && frames[1].contains("0 s elapsed"));

        // Its worker alone redraws it a second later, then waits another second
        wait_deadline(&tester, start + Duration::from_secs(1));
        tester.advance_to(start + Duration::from_secs(1));
        wait_deadline(&tester, start + Duration::from_secs(2));
        let frames = screen.frames();
        assert_eq!(frames.len(), 3);
        assert!(frames[2].contains("second") && frames[2].contains("1 s elapsed"));

        // Dropping the output stops the waiting worker
        drop(output);
        assert_eq!(tester.next_deadline(), None);
        assert_eq!(screen.frames().len(), 3);
    }

    /// Ending the display while a wait replaces it lands after the replacement,
    /// so no worker is left once the end returns.
    #[test]
    fn test_end_stops_a_pending_worker() {
        // Hold the terminal, so the wait stops inside its replacement
        let tester = TestClock::new();
        let output = output(tester.clock());
        let screen = Screen::default();
        let terminal = output.0.terminal.lock().unwrap();
        let waiting = spawn_wait(&output, "waiting", &screen);
        ticker_held(&output);

        // The end queues behind the replacement and stops its worker
        let ending = thread::spawn({
            let output = output.clone();
            move || drop(output.end_wait())
        });
        drop(terminal);
        waiting.join().unwrap();
        ending.join().unwrap();
        assert!(output.0.ticker.lock().unwrap().is_none());
        assert!(output.0.terminal.lock().unwrap().waiting.is_none());
        assert_eq!(tester.next_deadline(), None);
    }

    /// Dropping a ticker during a redraw returns only after the redraw finished
    /// and the worker exited, without another redraw.
    #[test]
    fn test_ticker_drop_waits_for_a_redraw() {
        // The worker redraws a second after its start and holds the redraw open
        let mut tester = TestClock::new();
        let clock = tester.clock();
        let (entered, redraws) = mpsc::channel();
        let (release, gate) = mpsc::channel::<()>();
        let mut ticker = Ticker::start(clock.clone(), move |now| {
            entered.send(now).unwrap();
            gate.recv().unwrap();
        });
        let first = clock.now() + Duration::from_secs(1);
        wait_deadline(&tester, first);
        tester.advance_to(first);
        assert_eq!(redraws.recv().unwrap(), first);

        // Pass the stop signal through a helper, which lets the redraw finish
        // only once the drop has started canceling the worker
        let (tap, tapped) = crossbeam_channel::bounded::<()>(0);
        let stop = ticker.stop.replace(tap);
        let releasing = thread::spawn(move || {
            assert_eq!(tapped.recv(), Err(crossbeam_channel::RecvError));
            drop(stop);
            release.send(()).unwrap();
        });

        // The drop itself waits for the worker, so its closure is gone the
        // moment the drop returns
        drop(ticker);
        assert_eq!(redraws.try_recv(), Err(mpsc::TryRecvError::Disconnected));
        releasing.join().unwrap();
        assert_eq!(tester.next_deadline(), None);
    }

    /// Event lines keep their kind prefix, style inline commands and mark steps.
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

    /// A wait line is erased, while a finished progress line stays, ended by one
    /// newline.
    #[test]
    fn waiting_line_is_erased_but_completed_progress_stays() {
        // The first frame counts up from the start of the wait
        let theme = Theme::test(80, Color::Off, true);
        let started = TestClock::new().clock().now();
        let mut terminal = Terminal {
            waiting: Some(Waiting {
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

        // A countdown frame erases the previous frame first
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

        // Closing erases the wait line, and a tick without a wait draws nothing
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

        // A finished progress line ends with one newline, however often it
        // closes
        terminal.live = Some(("uploading".into(), false));
        close_line(&mut terminal, &mut output);
        close_line(&mut terminal, &mut output);
        assert_eq!(output, b"\n");
    }

    /// A non-release environment is noted once per output, and never under
    /// quiet.
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
}
