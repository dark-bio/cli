// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Role colors and terminal capabilities, kept separate from content.

use clap::builder::styling::{Ansi256Color, RgbColor, Style, Styles};
use std::io::{self, IsTerminal};

/// Semantic emphasis shared by help, diagnostics and human result layouts.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Role {
    /// Unstyled content whose meaning needs no emphasis.
    Default,
    /// Section titles and help headings.
    Heading,
    /// Completed or verified states.
    Success,
    /// Pending approval, warnings and states needing action.
    Attention,
    /// Errors, damage and failed checks.
    Failure,
    /// Labels and secondary context.
    Muted,
    /// Commands, identifiers and links the user may act on.
    Accent,
    /// Staging environment label.
    Staging,
    /// Develop environment label.
    Develop,
}

/// Terminal color capability, separate from Unicode and interactive cursor control.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Color {
    /// No ANSI styling, including bold emphasis.
    Off,
    /// Bold emphasis without palette colors.
    Basic,
    /// Palette colors approximated on the terminal's 256-color cube.
    Ansi256,
    /// Exact RGB palette colors.
    True,
}

/// Presentation capabilities resolved once for one output stream.
#[derive(Clone, Debug)]
pub(crate) struct Theme {
    /// Whether this stream permits cursor control and live line updates.
    pub interactive: bool,
    /// Whether terminal and locale permit decorative Unicode glyphs.
    pub unicode: bool,
    /// Available style depth after honoring color opt-out settings.
    pub color: Color,
    /// Terminal width in display cells, with an 80-column fallback.
    pub width: usize,
}

impl Theme {
    /// Resolves the capabilities of stdout, or of stderr when `stderr` is set.
    ///
    /// Only an attended terminal outside JSON mode gets color and cursor
    /// control, so pipes keep the reading layouts.
    pub fn new(json: bool, stderr: bool) -> Self {
        // Cursor control needs a human layout on an attended terminal
        let terminal = if stderr {
            console::Term::stderr()
        } else {
            console::Term::stdout()
        };
        let attended = if stderr {
            io::stderr().is_terminal()
        } else {
            io::stdout().is_terminal()
        };
        let human = !json;
        let term = std::env::var("TERM").unwrap_or_default();
        let interactive = human && attended && term != "dumb";
        // Windows needs ANSI processing enabled for colors and live progress
        #[cfg(windows)]
        let interactive = interactive && terminal.features().colors_supported();

        // A native Windows console gets exact colors without COLORTERM
        let native_console = {
            #[cfg(windows)]
            {
                use std::os::windows::io::AsRawHandle;
                use windows_sys::Win32::System::Console::GetConsoleMode;
                let mut mode = 0;
                // Sound, since the call takes the stream's own handle and
                // writes only the local mode
                unsafe { GetConsoleMode(terminal.as_raw_handle(), &mut mode) != 0 }
            }
            #[cfg(not(windows))]
            {
                false
            }
        };

        // Glyphs need a UTF-8 locale when one is set, except on Windows
        let locale = ["LC_ALL", "LC_CTYPE", "LANG"]
            .into_iter()
            .filter_map(|key| std::env::var(key).ok())
            .find(|value| !value.is_empty());
        let unicode = interactive
            && locale.is_none_or(|locale| {
                let locale = locale.to_ascii_uppercase().replace('-', "");
                locale.contains("UTF8") || cfg!(windows)
            });

        // Color honors the opt-outs, then takes the deepest palette on offer
        let color = if !interactive
            || std::env::var_os("NO_COLOR").is_some()
            || std::env::var("CLICOLOR").is_ok_and(|value| value == "0")
        {
            Color::Off
        } else if native_console
            || std::env::var("COLORTERM")
                .is_ok_and(|value| matches!(value.as_str(), "truecolor" | "24bit"))
        {
            Color::True
        } else if term.contains("256color") {
            Color::Ansi256
        } else {
            Color::Basic
        };

        // An unknown width falls back to 80 columns
        let width = terminal
            .size_checked()
            .map_or(80, |(_, width)| usize::from(width).max(1));
        Self {
            interactive,
            unicode,
            color,
            width,
        }
    }

    /// Maps semantic emphasis to supported styling, degrading to bold or plain text.
    pub fn style(&self, role: Role) -> Style {
        if self.color == Color::Off || matches!(role, Role::Default) {
            return Style::new();
        }

        // Each role has one RGB color, except headings, which are only bold
        let rgb = match role {
            Role::Success => (148, 202, 110),
            Role::Attention => (232, 162, 74),
            Role::Failure => (235, 96, 112),
            Role::Muted => (124, 128, 152),
            Role::Accent => (137, 180, 250),
            Role::Staging => (147, 153, 178),
            Role::Develop => (108, 112, 134),
            Role::Heading => return Style::new().bold(),
            Role::Default => unreachable!(),
        };

        // Muted text and environment labels keep regular weight, and the other
        // roles are bold
        let style = if matches!(role, Role::Muted | Role::Staging | Role::Develop) {
            Style::new()
        } else {
            Style::new().bold()
        };

        // The 256-color cube rounds each channel to one of its six levels, and
        // basic color keeps only the weight
        match self.color {
            Color::True => style.fg_color(Some(RgbColor(rgb.0, rgb.1, rgb.2).into())),
            Color::Ansi256 => {
                let cell = |channel| ((u16::from(channel) * 5 + 127) / 255) as u8;
                style.fg_color(Some(
                    Ansi256Color(16 + 36 * cell(rgb.0) + 6 * cell(rgb.1) + cell(rgb.2)).into(),
                ))
            }
            _ => style,
        }
    }

    /// Wraps text in a role's style and its reset, or leaves it plain when disabled.
    pub fn paint(&self, role: Role, text: impl AsRef<str>) -> String {
        let style = self.style(role);
        format!("{style}{}{style:#}", text.as_ref())
    }

    /// Picks the decorative Unicode glyph when the stream allows it, or its
    /// ASCII stand-in otherwise.
    pub fn glyph<'a>(&self, unicode: &'a str, ascii: &'a str) -> &'a str {
        if self.unicode { unicode } else { ascii }
    }

    /// Prefixes a state marker so meaning survives without color.
    pub fn mark(&self, role: Role, text: &str) -> String {
        let icon = match role {
            Role::Success => self.glyph("\u{2713}", "ok"),
            Role::Failure => self.glyph("\u{2717}", "x"),
            Role::Attention => "!",
            _ => self.glyph("\u{00b7}", "-"),
        };
        self.paint(role, format!("{icon} {text}"))
    }

    /// Returns the muted separator used between related human facts.
    pub fn separator(&self) -> String {
        self.paint(Role::Muted, format!(" {} ", self.glyph("\u{00b7}", "-")))
    }

    /// Truncates by terminal cells, preserving ANSI sequences and fitting the tail.
    pub fn truncate(&self, text: &str, width: usize) -> String {
        if console::measure_text_width(text) <= width {
            return text.to_string();
        }
        let tail = self.glyph("\u{2026}", "...");
        let tail = if width < console::measure_text_width(tail) {
            ""
        } else {
            tail
        };
        console::truncate_str(text, width, tail).into_owned()
    }

    /// Styles paired backtick spans as commands; unmatched backticks remain literal.
    pub fn inline(&self, text: &str) -> String {
        let mut result = String::new();
        let mut remaining = text;
        while let Some((before, after)) = remaining.split_once('`') {
            let Some((code, rest)) = after.split_once('`') else {
                break;
            };
            result.push_str(before);
            result.push_str(&self.paint(Role::Accent, code));
            remaining = rest;
        }
        result.push_str(remaining);
        result
    }

    /// Applies the same semantic palette to clap's generated help and errors.
    pub fn clap(&self) -> Styles {
        Styles::plain()
            .header(self.style(Role::Heading))
            .usage(self.style(Role::Heading))
            .literal(self.style(Role::Accent))
            .placeholder(self.style(Role::Muted))
            .error(self.style(Role::Failure))
            .valid(self.style(Role::Success))
            .invalid(self.style(Role::Attention))
    }
}

/// Control sequence that returns the cursor to the line start and clears the
/// line.
///
/// Terminal control sequences belong to the writer, never to result content.
pub(crate) const CLEAR_LINE: &str = "\r\x1b[2K";

/// Escapes control characters in text that came from a device, a file or a
/// remote, so it can neither drive the terminal nor start a line of its own.
pub(crate) fn printable(text: &str) -> String {
    if !text.chars().any(char::is_control) {
        return text.to_string();
    }
    text.chars()
        .flat_map(|ch| {
            if ch.is_control() {
                ch.escape_default().collect::<Vec<_>>()
            } else {
                vec![ch]
            }
        })
        .collect()
}

/// Formats a byte count in KiB, MiB or GiB with one decimal place, or in whole
/// bytes below 1 KiB.
pub(crate) fn bytes(bytes: u64) -> String {
    for (unit, divisor) in [("GiB", 1_u64 << 30), ("MiB", 1 << 20), ("KiB", 1 << 10)] {
        if bytes >= divisor {
            return format!("{:.1} {unit}", bytes as f64 / divisor as f64);
        }
    }
    format!("{bytes} B")
}

/// Wraps styled text at words to `width` terminal cells, indenting continued
/// lines by `indent` cells.
///
/// A token too long for a line splits across lines. Only the whitespace at a
/// break is dropped, and ANSI sequences stay in place.
pub(crate) fn wrap(text: &str, width: usize, indent: usize) -> String {
    // Keep at least one cell of text beside the indent
    let width = width.max(1);
    let indent = indent.min(width.saturating_sub(1));
    let mut result = String::new();
    let mut column = 0;

    // Append one word, moving it to a new line when it fits there but not here
    let mut append = |word: &str| {
        let size = console::measure_text_width(word.trim_end());
        if column > indent && column + size > width && size <= width - indent {
            // The space that ended the previous word is not part of the line
            while result.ends_with(' ') {
                result.pop();
            }
            result.push('\n');
            result.push_str(&" ".repeat(indent));
            column = indent;
        }

        // Copy the word, breaking wherever it still overflows and indenting
        // after every newline
        for (part, ansi) in console::AnsiCodeIterator::new(word) {
            if ansi {
                result.push_str(part);
                continue;
            }
            for ch in part.chars() {
                if ch == '\n' {
                    result.push(ch);
                    result.push_str(&" ".repeat(indent));
                    column = indent;
                    continue;
                }
                let size = console::measure_text_width(ch.encode_utf8(&mut [0; 4]));
                if column + size > width {
                    if ch.is_whitespace() {
                        continue;
                    }
                    result.push('\n');
                    result.push_str(&" ".repeat(indent));
                    column = indent;
                }
                result.push(ch);
                column += size;
            }
        }
    };

    // Split the text into words that end at whitespace, keeping ANSI sequences
    // inside the word they touch
    let mut word = String::new();
    for (part, ansi) in console::AnsiCodeIterator::new(text) {
        if ansi {
            word.push_str(part);
            continue;
        }
        for ch in part.chars() {
            word.push(ch);
            if ch.is_whitespace() {
                append(&word);
                word.clear();
            }
        }
    }
    append(&word);
    result
}

#[cfg(test)]
impl Theme {
    /// Builds an interactive theme with fixed capabilities, independent of the
    /// test's own terminal.
    pub fn test(width: usize, color: Color, unicode: bool) -> Self {
        Self {
            interactive: true,
            unicode,
            color,
            width,
        }
    }
}

/// Tests of the capability detection, the palette, wrapping and escaping.
#[cfg(test)]
mod tests {
    use super::*;

    /// Checks that a native Windows console gets exact colors without any
    /// color variable, while the opt-outs still turn them off.
    #[cfg(windows)]
    #[test]
    fn windows_console_enables_color_without_environment() {
        use std::{fs::OpenOptions, os::windows::io::AsRawHandle, process::Command};
        use windows_sys::Win32::System::Console::{
            AllocConsole, ENABLE_VIRTUAL_TERMINAL_PROCESSING, FreeConsole, GetConsoleMode,
            GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE, SetConsoleMode, SetStdHandle,
        };

        /// Environment variable that marks a child run and names its case.
        const CHILD: &str = "ARK_TEST_WINDOWS_CONSOLE";

        // Run each case in a child process, isolating console handles and
        // environment from the other tests
        let Ok(case) = std::env::var(CHILD) else {
            for (name, value) in [
                ("", ""),
                ("NO_COLOR", "1"),
                ("CLICOLOR", "0"),
                ("TERM", "dumb"),
            ] {
                let mut command = Command::new(std::env::current_exe().unwrap());
                command
                    .args([
                        "--exact",
                        "style::tests::windows_console_enables_color_without_environment",
                        "--nocapture",
                    ])
                    .env(CHILD, if name.is_empty() { "color" } else { name })
                    .env_remove("TERM")
                    .env_remove("COLORTERM")
                    .env_remove("NO_COLOR")
                    .env_remove("CLICOLOR")
                    .env_remove("CLICOLOR_FORCE")
                    .env_remove("FORCE_COLOR");
                if !name.is_empty() {
                    command.env(name, value);
                }
                let output = command.output().unwrap();
                assert!(output.status.success(), "{name}={value}: {output:?}");
            }
            return;
        };

        // Swap in a fresh console, saving the standard handles to restore. The
        // console calls take no pointers, so they are sound.
        let stdout = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
        let stderr = unsafe { GetStdHandle(STD_ERROR_HANDLE) };
        unsafe { FreeConsole() };
        assert_ne!(unsafe { AllocConsole() }, 0);

        // Check both streams, catching a failure so the handles come back first
        let result = std::panic::catch_unwind(|| {
            let console = OpenOptions::new()
                .read(true)
                .write(true)
                .open("CONOUT$")
                .unwrap();
            let handle = console.as_raw_handle();
            for (stream, stderr) in [(STD_OUTPUT_HANDLE, false), (STD_ERROR_HANDLE, true)] {
                // Point the stream at the new console with ANSI processing off.
                // The calls use the handle `console` keeps open and write only
                // the local mode, so they are sound.
                assert_ne!(unsafe { SetStdHandle(stream, handle) }, 0);
                let mut mode = 0;
                assert_ne!(unsafe { GetConsoleMode(handle, &mut mode) }, 0);
                assert_ne!(
                    unsafe { SetConsoleMode(handle, mode & !ENABLE_VIRTUAL_TERMINAL_PROCESSING) },
                    0
                );

                // The theme gets exact colors unless an opt-out or a dumb TERM
                // is set
                let theme = Theme::new(false, stderr);
                assert_eq!(theme.interactive, case != "TERM");
                assert_eq!(
                    theme.color,
                    if case == "color" {
                        Color::True
                    } else {
                        Color::Off
                    }
                );
                assert_eq!(
                    theme.paint(Role::Muted, "label").contains("\x1b[38;2;"),
                    case == "color"
                );

                // Resolving the theme turns ANSI processing back on, except
                // under a dumb TERM. The call writes only the local mode, so it
                // is sound.
                assert_ne!(unsafe { GetConsoleMode(handle, &mut mode) }, 0);
                assert_eq!(
                    mode & ENABLE_VIRTUAL_TERMINAL_PROCESSING != 0,
                    case != "TERM"
                );

                // JSON stays plain, and a second theme resolves the same way
                let json = Theme::new(true, stderr);
                assert!(!json.interactive);
                assert_eq!(json.paint(Role::Muted, "label"), "label");
                let repeated = Theme::new(false, stderr);
                assert_eq!(repeated.color, theme.color);
                assert_eq!(repeated.interactive, theme.interactive);
            }
        });

        // Restore the saved standard handles, then rethrow any failure. The
        // console calls take no pointers, so they are sound.
        unsafe {
            FreeConsole();
            SetStdHandle(STD_OUTPUT_HANDLE, stdout);
            SetStdHandle(STD_ERROR_HANDLE, stderr);
        }
        if let Err(error) = result {
            std::panic::resume_unwind(error);
        }
    }

    /// Checks that styling degrades from exact colors to the 256-color cube to
    /// plain marks, keeping the words.
    #[test]
    fn palette_degrades_without_losing_words() {
        let theme = Theme::test(80, Color::True, true);
        assert_eq!(
            theme.paint(Role::Success, "done"),
            "\x1b[1m\x1b[38;2;148;202;110mdone\x1b[0m"
        );
        assert_eq!(
            Theme {
                color: Color::Ansi256,
                ..theme.clone()
            }
            .paint(Role::Success, "done"),
            "\x1b[1m\x1b[38;5;150mdone\x1b[0m"
        );
        assert_eq!(
            Theme {
                color: Color::Off,
                ..theme
            }
            .mark(Role::Success, "done"),
            "\u{2713} done"
        );
    }

    /// Checks that wrapping and truncation measure terminal cells, and wrapping
    /// keeps every character of a styled link.
    #[test]
    fn wrapping_preserves_links_and_measures_terminal_cells() {
        // A styled link splits across lines but keeps every character
        let theme = Theme::test(24, Color::True, true);
        let url = "https://app.dark.bio/pair/0123456789abcdef";
        let wrapped = wrap(&theme.paint(Role::Accent, url), 24, 2);
        assert!(
            wrapped
                .lines()
                .all(|line| console::measure_text_width(line) <= 24)
        );
        assert_eq!(
            console::strip_ansi_codes(&wrapped)
                .split_whitespace()
                .collect::<String>(),
            url
        );

        // Wide characters count two cells each, and truncation fits its tail
        let wide = "\u{754c}".repeat(12);
        let wrapped = wrap(&wide, 10, 2);
        assert!(
            wrapped
                .lines()
                .all(|line| console::measure_text_width(line) <= 10)
        );
        assert_eq!(wrapped.split_whitespace().collect::<String>(), wide);
        assert_eq!(theme.truncate("abcdef", 2), "a\u{2026}");
        assert_eq!(theme.truncate("abcdef", 6), "abcdef");
    }

    /// Checks that control characters are escaped, while plain text passes
    /// unchanged.
    #[test]
    fn control_characters_cannot_reach_the_terminal() {
        assert_eq!(printable("plain name"), "plain name");
        assert_eq!(
            printable("evil\x1b[2Jname\nsecond line\t"),
            "evil\\u{1b}[2Jname\\nsecond line\\t"
        );
    }

    /// Checks that paired backticks become styled spans, while an unmatched one
    /// stays literal.
    #[test]
    fn inline_code_keeps_unmatched_backticks() {
        let theme = Theme::test(80, Color::Basic, false);
        assert_eq!(
            theme.inline("run `ark unlock` first"),
            "run \x1b[1mark unlock\x1b[0m first"
        );
        assert_eq!(theme.inline("an unmatched ` stays"), "an unmatched ` stays");
    }

    /// Checks that inline styling does not move where text wraps.
    #[test]
    fn wrapping_ignores_style_boundaries_inside_words() {
        let theme = Theme::test(40, Color::True, true);
        let text = "Update hardware with `ark firmware update`;";
        let colored = wrap(&theme.inline(text), 40, 0);
        let plain = wrap(&text.replace('`', ""), 40, 0);
        assert_eq!(console::strip_ansi_codes(&colored), plain);
    }
}
