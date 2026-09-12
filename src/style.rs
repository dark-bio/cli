// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Role colors and terminal capabilities, kept separate from content.

use crate::args::Format;
use clap::builder::styling::{Ansi256Color, RgbColor, Style, Styles};
use std::io::{self, IsTerminal};

#[derive(Clone, Copy, Debug)]
pub(crate) enum Role {
    Default,
    Heading,
    Success,
    Attention,
    Failure,
    Muted,
    Accent,
    Staging,
    Develop,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Color {
    Off,
    Basic,
    Ansi256,
    True,
}

#[derive(Clone, Debug)]
pub(crate) struct Theme {
    pub human: bool,
    pub interactive: bool,
    pub unicode: bool,
    pub color: Color,
    pub width: usize,
}

impl Theme {
    pub fn new(format: Format, stderr: bool) -> Self {
        let attended = if stderr {
            io::stderr().is_terminal()
        } else {
            io::stdout().is_terminal()
        };
        let human = format == Format::Human || (format == Format::Auto && attended);
        let term = std::env::var("TERM").unwrap_or_default();
        let interactive = human && attended && term != "dumb";
        let locale = ["LC_ALL", "LC_CTYPE", "LANG"]
            .into_iter()
            .filter_map(|key| std::env::var(key).ok())
            .find(|value| !value.is_empty());
        let unicode = interactive
            && locale.is_none_or(|locale| {
                let locale = locale.to_ascii_uppercase().replace('-', "");
                locale.contains("UTF8") || cfg!(windows)
            });
        let color = if !interactive
            || std::env::var_os("NO_COLOR").is_some()
            || std::env::var("CLICOLOR").is_ok_and(|value| value == "0")
        {
            Color::Off
        } else if std::env::var("COLORTERM")
            .is_ok_and(|value| matches!(value.as_str(), "truecolor" | "24bit"))
        {
            Color::True
        } else if term.contains("256color") {
            Color::Ansi256
        } else {
            Color::Basic
        };
        let terminal = if stderr {
            console::Term::stderr()
        } else {
            console::Term::stdout()
        };
        let width = terminal
            .size_checked()
            .map_or(80, |(_, width)| usize::from(width).max(1));
        Self {
            human,
            interactive,
            unicode,
            color,
            width,
        }
    }

    pub fn style(&self, role: Role) -> Style {
        if self.color == Color::Off || matches!(role, Role::Default) {
            return Style::new();
        }
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
        let style = if matches!(role, Role::Muted | Role::Staging | Role::Develop) {
            Style::new()
        } else {
            Style::new().bold()
        };
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

    pub fn paint(&self, role: Role, text: impl AsRef<str>) -> String {
        let style = self.style(role);
        format!("{style}{}{style:#}", text.as_ref())
    }

    pub fn glyph<'a>(&self, unicode: &'a str, ascii: &'a str) -> &'a str {
        if self.unicode { unicode } else { ascii }
    }

    pub fn mark(&self, role: Role, text: &str) -> String {
        let icon = match role {
            Role::Success => self.glyph("\u{2713}", "ok"),
            Role::Failure => self.glyph("\u{2717}", "x"),
            Role::Attention => "!",
            _ => self.glyph("\u{00b7}", "-"),
        };
        self.paint(role, format!("{icon} {text}"))
    }

    pub fn separator(&self) -> String {
        self.paint(Role::Muted, format!(" {} ", self.glyph("\u{00b7}", "-")))
    }

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

/// Terminal control sequences belong to the writer, never to result content.
pub(crate) const CLEAR_LINE: &str = "\r\x1b[2K";

pub(crate) fn bytes(bytes: u64) -> String {
    for (unit, divisor) in [("GiB", 1_u64 << 30), ("MiB", 1 << 20), ("KiB", 1 << 10)] {
        if bytes >= divisor {
            return format!("{:.1} {unit}", bytes as f64 / divisor as f64);
        }
    }
    format!("{bytes} B")
}

/// Wraps styled text at words, splitting long tokens without dropping bytes.
pub(crate) fn wrap(text: &str, width: usize, indent: usize) -> String {
    let width = width.max(1);
    let indent = indent.min(width.saturating_sub(1));
    let mut result = String::new();
    let mut column = 0;
    let mut append = |word: &str| {
        let size = console::measure_text_width(word.trim_end());
        if column > indent && column + size > width && size <= width - indent {
            result.push('\n');
            result.push_str(&" ".repeat(indent));
            column = indent;
        }
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
    pub fn test(width: usize, color: Color, unicode: bool) -> Self {
        Self {
            human: true,
            interactive: true,
            unicode,
            color,
            width,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn wrapping_preserves_links_and_measures_terminal_cells() {
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

    #[test]
    fn inline_code_keeps_unmatched_backticks() {
        let theme = Theme::test(80, Color::Basic, false);
        assert_eq!(
            theme.inline("run `ark unlock` first"),
            "run \x1b[1mark unlock\x1b[0m first"
        );
        assert_eq!(theme.inline("an unmatched ` stays"), "an unmatched ` stays");
    }

    #[test]
    fn wrapping_ignores_style_boundaries_inside_words() {
        let theme = Theme::test(40, Color::True, true);
        let text = "Update hardware with `ark firmware update`;";
        let colored = wrap(&theme.inline(text), 40, 0);
        let plain = wrap(&text.replace('`', ""), 40, 0);
        assert_eq!(console::strip_ansi_codes(&colored), plain);
    }
}
