// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Human layouts of result fields.
//!
//! Machine renderings never pass through here.

use crate::style::{self, Role, Theme};
use serde_json::Value;

/// Styles a field, retaining explicit absent values.
///
/// Arbitrary strings receive no semantic status color merely because of their text.
pub(crate) fn value(theme: &Theme, key: &str, value: &Value) -> String {
    // Absent values and empty lists stay visible
    if value.is_null() {
        return theme.paint(
            Role::Muted,
            if key == "serial" { "unverified" } else { "-" },
        );
    }
    if value.as_array().is_some_and(Vec::is_empty) {
        return theme.paint(Role::Muted, "none");
    }

    // Sizes and durations read in their units, and timestamps in local time
    if key.ends_with("_bytes")
        && let Some(bytes) = value.as_u64()
    {
        return style::bytes(bytes);
    }
    if let Some(text) = value.as_str()
        && let Ok(time) = chrono::DateTime::parse_from_rfc3339(text)
    {
        let local = time.with_timezone(&chrono::Local);
        return format!(
            "{} {}",
            local.format("%Y-%m-%d"),
            theme.paint(Role::Muted, local.format("%H:%M:%S %:z").to_string())
        );
    }
    if key.ends_with("_seconds")
        && let Some(seconds) = value.as_u64()
    {
        return style::duration(seconds);
    }

    // Other values read as text, styled by what their key means
    let text = super::scalar(value);
    if value.is_boolean() {
        return text;
    }
    match key {
        "trust" | "state" | "outcome" | "result" => {
            let role = match text.as_str() {
                "attested" | "filled" | "done" | "ok" => Role::Success,
                "damaged" | "failed" | "fail" => Role::Failure,
                "self-signed" | "pinned" | "planned" | "pending" | "warn" => Role::Attention,
                _ => Role::Muted,
            };
            theme.mark(role, &text)
        }
        "slot" | "locator" | "identity" | "pubkey" | "url" | "requires" | "required_by" => {
            theme.paint(Role::Accent, text)
        }
        "damage" | "mismatch" => theme.paint(Role::Failure, text),
        _ => text,
    }
}

/// Aligns label/value rows, stacking them when labels consume the available width.
///
/// A title prints at column 0 above rows indented two spaces. An empty pair
/// separates groups, and an empty label starts an unlabeled row.
pub(crate) fn block(theme: &Theme, title: Option<&str>, rows: &[(String, String)]) -> String {
    // Print the title above the rows
    let mut lines = Vec::new();
    if let Some(title) = title {
        lines.push(style::wrap(
            &theme.paint(Role::Heading, style::printable(title)),
            theme.width,
            0,
        ));
    }

    // Align rows within the block, stacking values when the labels leave no room
    let labels = rows
        .iter()
        .map(|(label, _)| console::measure_text_width(label))
        .max()
        .unwrap_or(0);
    let stacked = labels + 8 > theme.width;
    lines.extend(rows.iter().map(|(label, value)| {
        if label.is_empty() && value.is_empty() {
            return String::new();
        }
        if label.is_empty() {
            return style::wrap(&format!("  {value}"), theme.width, 2);
        }
        if stacked {
            return [theme.paint(Role::Muted, label), value.clone()]
                .iter()
                .map(|line| style::wrap(&format!("  {line}"), theme.width, 2))
                .collect::<Vec<_>>()
                .join("\n");
        }
        let line = format!(
            "  {}{}  {value}",
            theme.paint(Role::Muted, label),
            " ".repeat(labels.saturating_sub(console::measure_text_width(label)))
        );
        style::wrap(&line, theme.width, labels + 4)
    }));
    lines.join("\n")
}

/// Renders nested fields as readable labels with units in their values.
pub(crate) fn document(theme: &Theme, value: &Value) -> String {
    let mut rows = Vec::new();
    fields(theme, "", "", value, &mut rows);
    block(theme, None, &rows)
}

/// Flattens a value into rows, joining nested keys into readable labels
/// without machine paths or unit suffixes.
fn fields(theme: &Theme, prefix: &str, key: &str, value: &Value, rows: &mut Vec<(String, String)>) {
    // Name the field for reading, dropping the unit suffix its value shows
    let name = if key == "requires" {
        "dependencies"
    } else {
        key
    };
    let name = name
        .strip_suffix("_bytes")
        .or_else(|| name.strip_suffix("_seconds"))
        .unwrap_or(name);
    let label = format!("{prefix} {}", name.replace('_', " "))
        .trim()
        .to_string();

    // Objects and lists of structures recurse, numbering list items from one,
    // and a leaf becomes one row with a capitalized label
    match value {
        Value::Object(object) if !object.is_empty() => {
            for (key, value) in object {
                fields(theme, &label, key, value, rows);
            }
        }
        Value::Array(values)
            if values
                .iter()
                .any(|value| value.is_object() || value.is_array()) =>
        {
            for (index, value) in values.iter().enumerate() {
                fields(theme, &label, &(index + 1).to_string(), value, rows);
            }
        }
        _ => {
            let mut chars = label.chars();
            let label = chars
                .next()
                .map(|first| first.to_uppercase().to_string() + chars.as_str())
                .unwrap_or_default();
            rows.push((label, self::value(theme, key, value)));
        }
    }
}

/// A table column: its header, the row field it shows and how that prints.
#[derive(Clone, Copy)]
pub(crate) struct Column<'a> {
    /// Uppercase header, which also labels the field in a fallback block.
    pub label: &'a str,
    /// Row field the column shows.
    pub key: &'a str,
    /// Whether the column holds names a reader types, printed in Accent.
    pub accent: bool,
    /// Whether the column is the free text, which shrinks with an ellipsis.
    pub flexible: bool,
}

impl Column<'_> {
    /// Styles the column's field of a row, in Accent for a column of names.
    fn value(&self, theme: &Theme, row: &Value) -> String {
        if self.accent && !row[self.key].is_null() {
            theme.paint(Role::Accent, super::scalar(&row[self.key]))
        } else {
            value(theme, self.key, &row[self.key])
        }
    }
}

impl<'a> From<(&'a str, &'a str)> for Column<'a> {
    /// Makes a column with the field's default styling, where `summary`,
    /// `description` and `name` are the free text.
    fn from((label, key): (&'a str, &'a str)) -> Self {
        Self {
            label,
            key,
            accent: false,
            flexible: matches!(key, "summary" | "description" | "name"),
        }
    }
}

/// Fits a table by shrinking one free-text column, then falls back to blocks.
///
/// Selectors, hashes and other actionable fields are never ellipsized by the table.
/// A table with no rows prints the `empty` sentence in its place.
///
/// # Panics
///
/// Panics when `rows` is empty and `empty` is omitted.
pub(crate) fn table<'a>(
    theme: &Theme,
    rows: &[Value],
    columns: &[impl Copy + Into<Column<'a>>],
    empty: Option<&str>,
) -> String {
    // With no rows, the sentence takes the table's place
    if rows.is_empty() {
        return style::wrap(
            &style::printable(empty.expect("empty tables need a sentence")),
            theme.width,
            0,
        );
    }

    // Style every cell and measure each column against its header
    let columns: Vec<Column<'a>> = columns.iter().map(|column| (*column).into()).collect();
    let cells: Vec<Vec<_>> = rows
        .iter()
        .map(|row| {
            columns
                .iter()
                .map(|column| {
                    row[column.key]
                        .as_str()
                        .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
                        .map(|time| {
                            theme.paint(
                                Role::Muted,
                                time.with_timezone(&chrono::Local)
                                    .format("%Y-%m-%d")
                                    .to_string(),
                            )
                        })
                        .unwrap_or_else(|| column.value(theme, row))
                })
                .collect()
        })
        .collect();
    let mut widths: Vec<_> = columns
        .iter()
        .enumerate()
        .map(|(i, column)| {
            cells
                .iter()
                .map(|row| console::measure_text_width(&row[i]))
                .max()
                .unwrap_or(0)
                .max(column.label.len())
        })
        .collect();

    // Shrink the one free-text column to fit, never below its header or 8 cells
    let total =
        |widths: &[usize]| widths.iter().sum::<usize>() + columns.len().saturating_sub(1) * 2;
    let flexible = columns.iter().position(|column| column.flexible);
    if let Some(index) = flexible {
        widths[index] = widths[index]
            .saturating_sub(total(&widths).saturating_sub(theme.width))
            .max(columns[index].label.len().max(8));
    }

    // A table that still overflows falls back to one block per row
    if total(&widths) > theme.width {
        return rows
            .iter()
            .map(|row| {
                block(
                    theme,
                    None,
                    &columns
                        .iter()
                        .map(|column| (column.label.to_string(), column.value(theme, row)))
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n");
    }

    // Lay out a line, right-aligning sizes, durations and numeric columns
    let right: Vec<_> = columns
        .iter()
        .map(|column| {
            column.key.ends_with("_bytes")
                || column.key.ends_with("_seconds")
                || rows.iter().any(|row| row[column.key].is_number())
        })
        .collect();
    let line = |cells: &[String], header: bool| {
        let mut result = String::new();
        for (i, cell) in cells.iter().enumerate() {
            let cell = if Some(i) == flexible {
                theme.truncate(cell, widths[i])
            } else {
                cell.clone()
            };
            let padding = " ".repeat(widths[i].saturating_sub(console::measure_text_width(&cell)));
            if right[i] {
                result.push_str(&padding);
            }
            result.push_str(&if header {
                theme.paint(Role::Muted, cell)
            } else {
                cell
            });
            if i + 1 < cells.len() {
                if !right[i] {
                    result.push_str(&padding);
                }
                result.push_str("  ");
            }
        }
        result
    };

    // Print the header and the rows from column 0
    let mut lines = vec![line(
        &columns
            .iter()
            .map(|column| column.label.to_string())
            .collect::<Vec<_>>(),
        true,
    )];
    for row in &cells {
        lines.push(line(row, false));
    }
    lines.join("\n")
}

/// Renders checks under their group titles, then a summary of problems and
/// warnings.
///
/// The skips of one cause within a group share a row.
pub(super) fn checklist(theme: &Theme, groups: &[(&str, &[Value])]) -> String {
    // Collapse skips with the same cause inside each group, in first-seen order
    let shown: Vec<_> = groups
        .iter()
        .map(|(title, rows)| {
            let mut shown: Vec<(&Value, usize)> = Vec::new();
            for row in *rows {
                if row["result"] == "skip"
                    && let Some((_, count)) = shown.iter_mut().find(|(previous, _)| {
                        previous["result"] == "skip"
                            && previous["detail"] == row["detail"]
                            && previous["hint"] == row["hint"]
                    })
                {
                    *count += 1;
                } else {
                    shown.push((row, 1));
                }
            }
            let shown: Vec<_> = shown
                .into_iter()
                .map(|(row, count)| {
                    let name = if count > 1 {
                        format!("{count} checks")
                    } else {
                        style::printable(row["name"].as_str().unwrap_or("-"))
                    };
                    (name, row)
                })
                .collect();
            (*title, shown)
        })
        .collect();

    // Align the details across groups, leaving room for the two-letter ASCII
    // ok mark
    let labels = shown
        .iter()
        .flat_map(|(_, rows)| rows)
        .map(|(name, _)| console::measure_text_width(name))
        .chain(groups.iter().flat_map(|(_, rows)| *rows).map(|row| {
            console::measure_text_width(&style::printable(row["name"].as_str().unwrap_or("-")))
        }))
        .max()
        .unwrap_or(0);
    let width = labels + if theme.unicode { 2 } else { 3 };
    let mut sections: Vec<_> = shown
        .iter()
        .map(|(title, rows)| {
            let mut lines = vec![style::wrap(
                &theme.paint(Role::Heading, style::printable(title)),
                theme.width,
                0,
            )];
            lines.extend(
                rows.iter()
                    .map(|(name, row)| checklist_row(theme, name, row, width)),
            );
            lines.join("\n")
        })
        .collect();

    // Count failures as problems and warns as warnings, leaving out a zero
    let mut counts = Vec::new();
    for (result, noun) in [("fail", "problem"), ("warn", "warning")] {
        let count = groups
            .iter()
            .flat_map(|(_, rows)| *rows)
            .filter(|row| row["result"] == result)
            .count();
        if count > 0 {
            counts.push(format!(
                "{count} {noun}{}",
                if count == 1 { "" } else { "s" }
            ));
        }
    }
    let summary = if counts.is_empty() {
        "No problems found".into()
    } else {
        counts.join(", ")
    };
    sections.push(style::wrap(&summary, theme.width, 0));
    sections.join("\n\n")
}

/// Renders one check, its detail aligned after the name and its hint below.
fn checklist_row(theme: &Theme, name: &str, row: &Value, width: usize) -> String {
    // Only the mark takes the outcome's color, with the name muted and the
    // detail plain
    let role = match row["result"].as_str() {
        Some("ok") => Role::Success,
        Some("fail") => Role::Failure,
        Some("warn") => Role::Attention,
        _ => Role::Muted,
    };
    let name = theme.mark(role, &theme.paint(Role::Muted, name));
    let line = format!(
        "  {}{}  {}",
        name,
        " ".repeat(width.saturating_sub(console::measure_text_width(&name))),
        style::printable(row["detail"].as_str().unwrap_or("-")),
    );
    let mut line = style::wrap(&line, theme.width, width + 4);

    // Put the hint under its check, indented four spaces and wrapped under its
    // own text
    if let Some(hint) = row["hint"].as_str() {
        line.push('\n');
        line.push_str(&style::wrap(
            &format!(
                "    {} {}",
                theme.paint(Role::Accent, "hint:"),
                theme.inline(&style::printable(hint))
            ),
            theme.width,
            10,
        ));
    }
    line
}

/// Tests of the human layouts of values, documents, tables and checklists.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::Color;
    use serde_json::json;

    /// Empty block lists keep their labels and print a muted `none`.
    #[test]
    fn test_empty_block_lists_are_muted() {
        for (key, label) in [
            ("requires", "Dependencies"),
            ("required_by", "Required by"),
            ("phases", "Phases"),
            ("fetched", "Fetched"),
            ("hints", "Hints"),
        ] {
            for color in [Color::Off, Color::True] {
                let theme = Theme::test(80, color, true);
                assert_eq!(
                    document(&theme, &json!({key:[]})),
                    if color == Color::Off {
                        format!("  {label}  none")
                    } else {
                        format!(
                            "  \x1b[38;2;124;128;152m{label}\x1b[0m  \x1b[38;2;124;128;152mnone\x1b[0m"
                        )
                    },
                    "{key}, {color:?}",
                );
            }
        }
    }

    /// Empty tables print their sentence at any width and color depth.
    #[test]
    fn test_empty_table_prints_a_sentence() {
        for width in [8, 80, 160] {
            for color in [Color::Off, Color::Basic, Color::Ansi256, Color::True] {
                for unicode in [false, true] {
                    assert_eq!(
                        table(
                            &Theme::test(width, color, unicode),
                            &[],
                            &[
                                ("LOCATOR", "locator"),
                                ("NAME", "name"),
                                ("SERIAL", "serial")
                            ],
                            Some("No Arks found"),
                        ),
                        if width == 8 {
                            "No Arks\nfound"
                        } else {
                            "No Arks found"
                        },
                        "{width}, {color:?}, unicode={unicode}",
                    );
                }
            }
        }
    }

    /// Nested fields become labels, absent values stay visible, and free text
    /// gets no status color.
    #[test]
    fn block_retains_nested_fields_and_absent_values() {
        let theme = Theme::test(80, Color::Basic, true);
        assert_eq!(
            document(
                &theme,
                &json!({"firmware":{"version":"1.2.3","published":null},"paired":true})
            ),
            "  Firmware version    1.2.3\n  Firmware published  -\n  Paired              yes"
        );
        assert_eq!(value(&theme, "name", &json!("failed")), "failed");
        assert_eq!(value(&theme, "serial", &Value::Null), "unverified");
    }

    /// Environment values stay plain beside their muted labels.
    #[test]
    fn test_environment_values_are_unstyled() {
        let theme = Theme::test(80, Color::True, true);
        for environment in ["release", "staging", "develop", "unknown"] {
            assert_eq!(
                document(&theme, &json!({"environment":environment})),
                format!("  \x1b[38;2;124;128;152mEnvironment\x1b[0m  {environment}"),
                "{environment}"
            );
        }
    }

    /// A block title starts at column 0 above aligned, indented groups.
    #[test]
    fn test_block_titles_start_at_column_zero() {
        let theme = Theme::test(80, Color::Basic, true);
        let rows = [
            ("Serial".into(), "ark-1".into()),
            (String::new(), String::new()),
            ("State".into(), "ready".into()),
        ];
        assert_eq!(
            block(&theme, Some("Example Ark"), &rows),
            "\x1b[1mExample Ark\x1b[0m\n  Serial  ark-1\n\n  State   ready"
        );
        assert_eq!(
            block(&theme, None, &rows),
            "  Serial  ark-1\n\n  State   ready"
        );
    }

    /// Sizes and durations show in their units, under labels without unit
    /// suffixes or machine paths.
    #[test]
    fn transformed_values_use_reading_labels() {
        for width in [32, 80, 160] {
            let theme = Theme::test(width, Color::Off, false);
            let rendered = document(
                &theme,
                &json!({
                    "size_bytes": 1022427344_u64,
                    "duration_seconds": 120,
                    "download": {"size_bytes": u64::MAX},
                    "items": [{"uploaded_bytes": 1024, "duration_seconds": 72}],
                }),
            );
            assert!(rendered.contains("975.1 MiB"));
            assert!(rendered.contains("2m 00s"));
            assert!(!rendered.contains("bytes"));
            assert!(!rendered.contains("seconds"));
            assert!(!rendered.contains("download."));
            assert!(rendered.contains("Download size"));
        }
    }

    /// Checks that device names stay plain and truncate beside Accent locators.
    #[test]
    fn test_device_columns_keep_existing_rendering() {
        let rows = [json!({
            "locator": "hardware:00:1", "name": "Example Founder Ark", "serial": "founder-1",
        })];
        let rendered = table(
            &Theme::test(40, Color::Basic, true),
            &rows,
            &[
                ("LOCATOR", "locator"),
                ("NAME", "name"),
                ("SERIAL", "serial"),
            ],
            Some("No Arks found"),
        );
        assert_eq!(
            rendered,
            "LOCATOR        NAME            SERIAL\n\x1b[1mhardware:00:1\x1b[0m  Example Found…  founder-1",
            "device names keep their flexible plain column"
        );
    }

    /// A table right-aligns sizes and marks states.
    #[test]
    fn test_table_aligns_sizes_and_marks_states() {
        let theme = Theme::test(80, Color::Basic, true);
        let rows = [
            json!({"slot":"reference-genome","state":"filled","size_bytes":3_u64 << 30}),
            json!({"slot":"variant-catalog","state":"empty","size_bytes":null}),
            json!({"slot":"gene-annotations","state":"filled","size_bytes":1_u64 << 28}),
        ];
        assert_eq!(
            table(
                &theme,
                &rows,
                &[("SLOT", "slot"), ("STATE", "state"), ("SIZE", "size_bytes")],
                None,
            ),
            "SLOT              STATE          SIZE\n\x1b[1mreference-genome\x1b[0m  \x1b[1m\u{2713}\x1b[0m filled    3.0 GiB\n\x1b[1mvariant-catalog\x1b[0m   \u{00b7} empty           -\n\x1b[1mgene-annotations\x1b[0m  \x1b[1m\u{2713}\x1b[0m filled  256.0 MiB",
            "populated slot table",
        );
    }

    /// Boolean fields stay plain yes and no in both blocks and tables.
    #[test]
    fn test_booleans_are_plain_words_in_blocks_and_tables() {
        let fields = [
            "installed",
            "returned",
            "verified",
            "synced",
            "paired",
            "unlocked",
            "active",
            "enrolled",
            "success",
            "state",
            "ready",
        ];
        for color in [Color::Off, Color::True] {
            let theme = Theme::test(80, color, true);
            for key in fields {
                let rows = [json!({key:true}), json!({key:false})];
                let rendered = table(&theme, &rows, &[("VALUE", key)], None);
                assert_eq!(
                    rendered.lines().skip(1).collect::<Vec<_>>(),
                    ["yes", "no"],
                    "{key}"
                );
                let rendered = document(&theme, &json!({key:false}));
                assert!(rendered.ends_with("  no"), "{key}: {rendered}");
                assert_eq!(
                    console::strip_ansi_codes(&rendered)
                        .split_whitespace()
                        .last(),
                    Some("no"),
                    "{key}"
                );
                assert!(!rendered.contains('!'), "{key}");
            }
        }
    }

    /// A table starts at column 0 and may fill the whole width.
    #[test]
    fn test_tables_start_at_column_zero() {
        let theme = Theme::test(17, Color::True, true);
        assert_eq!(
            table(
                &theme,
                &[json!({"name":"Examples", "size_bytes":1024})],
                &[("NAME", "name"), ("SIZE", "size_bytes")],
                None,
            ),
            "\x1b[38;2;124;128;152mNAME\x1b[0m         \x1b[38;2;124;128;152mSIZE\x1b[0m\nExamples  1.0 KiB",
            "table uses the full width",
        );
    }

    /// A table shows a timestamp as its muted local date, and a block keeps the
    /// clock and offset.
    #[cfg(unix)]
    #[test]
    fn test_table_dates_and_block_timestamps_use_local_time() {
        // Run each time zone in a child, so its settings stay out of other tests
        let Ok(zone) = std::env::var("ARK_TEST_DATE_ZONE") else {
            for zone in ["Etc/GMT+5", "Etc/GMT-3"] {
                let output = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "output::human::tests::test_table_dates_and_block_timestamps_use_local_time",
                        "--nocapture",
                    ])
                    .env("ARK_TEST_DATE_ZONE", zone)
                    .env("TZ", zone)
                    .output()
                    .unwrap();
                assert!(output.status.success(), "{zone}: {output:?}");
            }
            return;
        };

        // West of UTC the instant falls on the previous day
        let (date, clock) = if zone == "Etc/GMT+5" {
            ("2026-10-10", "20:02:03 -05:00")
        } else {
            ("2026-10-11", "04:02:03 +03:00")
        };
        let rows = [json!({"name":"Example", "published":"2026-10-11T01:02:03Z"})];
        let columns = [("NAME", "name"), ("PUBLISHED", "published")];
        let theme = Theme::test(80, Color::True, true);
        let rendered = table(&theme, &rows, &columns, None);
        assert_eq!(
            rendered.lines().last().unwrap(),
            format!("Example   \x1b[38;2;124;128;152m{date}\x1b[0m"),
            "{zone}",
        );
        let rendered = document(&theme, &rows[0]);
        assert!(
            rendered.ends_with(&format!("{date} \x1b[38;2;124;128;152m{clock}\x1b[0m")),
            "{zone}"
        );

        // A narrow table falls back to full timestamps in blocks
        let theme = Theme::test(18, Color::True, true);
        let rendered = table(&theme, &rows, &columns, None);
        let plain = console::strip_ansi_codes(&rendered);
        let joined = plain.split_whitespace().collect::<String>();
        assert!(
            joined.contains(&format!("{date}{}", clock.replace(' ', ""))),
            "{zone}"
        );
        assert!(
            rendered
                .lines()
                .all(|line| console::measure_text_width(line) <= 18),
            "{zone}",
        );
        assert!(
            !rendered.contains(&format!("\x1b[38;2;124;128;152m{date}")),
            "{zone}"
        );
    }

    /// Tables show sizes in the same units as blocks.
    #[test]
    fn test_tables_and_blocks_agree_on_byte_units() {
        let theme = Theme::test(80, Color::Off, false);
        let rows = [
            json!({"slot":"reference-genome","size_bytes":1_u64 << 28}),
            json!({"slot":"variant-catalog","size_bytes":3_u64 << 30}),
        ];
        let rendered = table(
            &theme,
            &rows,
            &[("SLOT", "slot"), ("SIZE", "size_bytes")],
            None,
        );
        for (row, expected) in rows.iter().zip(["256.0 MiB", "3.0 GiB"]) {
            assert!(rendered.contains(expected), "{expected} in the table");
            assert!(
                document(&theme, row).contains(expected),
                "{expected} in the block"
            );
        }
    }

    /// A narrow table fits the width and keeps actionable values whole.
    #[test]
    fn test_narrow_tables_keep_actionable_values() {
        let theme = Theme::test(32, Color::True, true);
        let locator = "emulator:127.0.0.1:18181";
        let rows = [
            json!({"locator":locator,"name":"An unusually long emulator name","serial":"ark-123456789"}),
        ];
        let rendered = table(
            &theme,
            &rows,
            &[
                ("LOCATOR", "locator"),
                ("NAME", "name"),
                ("SERIAL", "serial"),
            ],
            None,
        );
        assert!(
            rendered
                .lines()
                .all(|line| console::measure_text_width(line) <= theme.width),
            "narrow table fits the width",
        );
        let plain = console::strip_ansi_codes(&rendered);
        let joined = plain.split_whitespace().collect::<String>();
        assert!(joined.contains(locator), "locator stays whole");
        assert!(joined.contains("ark-123456789"), "serial stays whole");
        assert!(!plain.contains('\u{2026}'), "blocks do not truncate");
    }

    /// A table truncates only its free-text column to fit.
    #[test]
    fn test_table_truncates_only_free_text() {
        let theme = Theme::test(40, Color::Off, true);
        let rows =
            [json!({"name":"A name much longer than the available space","serial":"ark-123"})];
        let rendered = table(
            &theme,
            &rows,
            &[("NAME", "name"), ("SERIAL", "serial")],
            None,
        );
        assert!(rendered.contains('\u{2026}'), "long name is truncated");
        assert!(rendered.contains("ark-123"), "serial stays whole");
        assert!(
            rendered
                .lines()
                .all(|line| console::measure_text_width(line) <= theme.width),
            "table fits the width",
        );
    }

    /// A healthy report shows two titled groups with aligned names, and its
    /// summary finds no problems.
    #[test]
    fn test_checklist_groups_computer_and_ark() {
        let computer = [
            json!({"name":"ark","result":"ok","detail":"0.4.1, the newest release"}),
            json!({"name":"hardware","result":"ok","detail":"1 Ark"}),
            json!({"name":"emulator","result":"ok","detail":"none running"}),
            json!({"name":"cache","result":"ok","detail":"28.5 GiB in ~/Library/Caches/ark"}),
        ];
        let device = [
            json!({"name":"connection","result":"ok","detail":"authenticated"}),
            json!({"name":"compatibility","result":"ok","detail":"firmware supports this tool"}),
            json!({"name":"environment","result":"ok","detail":"release"}),
            json!({"name":"firmware","result":"ok","detail":"0.12.0-abc, the newest published"}),
            json!({"name":"sync","result":"ok","detail":"keys and clock accepted"}),
            json!({"name":"registry","result":"ok","detail":"active"}),
            json!({"name":"pairing","result":"ok","detail":"paired and unlocked"}),
            json!({"name":"relay","result":"ok","detail":"attached"}),
            json!({"name":"slots","result":"ok","detail":"4 readable"}),
        ];
        let rendered = checklist(
            &Theme::test(80, Color::Off, true),
            &[("This computer", &computer), ("Sample Ark", &device)],
        );
        assert_eq!(
            rendered,
            concat!(
                "This computer\n",
                "  ✓ ark            0.4.1, the newest release\n",
                "  ✓ hardware       1 Ark\n",
                "  ✓ emulator       none running\n",
                "  ✓ cache          28.5 GiB in ~/Library/Caches/ark\n",
                "\nSample Ark\n",
                "  ✓ connection     authenticated\n",
                "  ✓ compatibility  firmware supports this tool\n",
                "  ✓ environment    release\n",
                "  ✓ firmware       0.12.0-abc, the newest published\n",
                "  ✓ sync           keys and clock accepted\n",
                "  ✓ registry       active\n",
                "  ✓ pairing        paired and unlocked\n",
                "  ✓ relay          attached\n",
                "  ✓ slots          4 readable\n",
                "\nNo problems found",
            )
        );
    }

    /// A failed connection collapses its eight skips into one row, keeps each
    /// hint under its check and counts every outcome.
    #[test]
    fn test_checklist_collapses_connection_skips() {
        let computer = [
            json!({"name":"ark","result":"warn","detail":"0.4.1, and 0.4.2 is out","hint":"run `brew upgrade ark-cli`"}),
            json!({"name":"hardware","result":"ok","detail":"1 Ark"}),
            json!({"name":"emulator","result":"ok","detail":"1 running"}),
        ];
        let mut device = vec![
            json!({"name":"connection","result":"fail","detail":"several Arks match","hint":"select one with --device: hardware, emulator"}),
        ];
        device.extend(
            [
                "compatibility",
                "environment",
                "firmware",
                "sync",
                "registry",
                "pairing",
                "relay",
                "slots",
            ]
            .map(
                |name| json!({"name":name,"result":"skip","detail":"skipped without a connection"}),
            ),
        );
        let groups = [
            ("This computer", computer.as_slice()),
            ("Ark", device.as_slice()),
        ];
        assert_eq!(
            checklist(&Theme::test(80, Color::Off, true), &groups),
            concat!(
                "This computer\n",
                "  ! ark            0.4.1, and 0.4.2 is out\n",
                "    hint: run `brew upgrade ark-cli`\n",
                "  ✓ hardware       1 Ark\n",
                "  ✓ emulator       1 running\n",
                "\nArk\n",
                "  ✗ connection     several Arks match\n",
                "    hint: select one with --device: hardware, emulator\n",
                "  · 8 checks       skipped without a connection\n",
                "\n1 problem, 1 warning",
            )
        );

        // ASCII marks keep the groups, counts and alignment
        let ascii = checklist(&Theme::test(80, Color::Off, false), &groups);
        assert!(ascii.contains("  ok hardware       1 Ark\n"));
        assert!(ascii.contains("  x connection      several Arks match\n"));
        assert!(ascii.contains("  - 8 checks        skipped without a connection\n"));
    }

    /// Skips of one cause collapse within their group across other rows, while
    /// another cause or another group keeps its own row.
    #[test]
    fn test_checklist_keeps_distinct_skip_causes_and_groups() {
        let computer = [json!({"name":"ark","result":"skip","detail":"needs cloud sync"})];
        let device = [
            json!({"name":"firmware","result":"skip","detail":"needs cloud sync"}),
            json!({"name":"pairing","result":"ok","detail":"paired and locked"}),
            json!({"name":"relay","result":"skip","detail":"needs cloud sync"}),
            json!({"name":"slots","result":"skip","detail":"needs an unlocked Ark and cloud sync"}),
        ];
        let rendered = checklist(
            &Theme::test(80, Color::Off, true),
            &[("This computer", &computer), ("Ark", &device)],
        );
        assert!(rendered.contains("  · ark       needs cloud sync\n"));
        assert!(rendered.contains("  · 2 checks  needs cloud sync\n"));
        assert!(rendered.contains("  · slots     needs an unlocked Ark and cloud sync\n"));
        assert!(rendered.ends_with("\n\nNo problems found"));
    }

    /// The summary counts problems and warnings in singular and plural, and
    /// leaves skips out.
    #[test]
    fn test_checklist_summary_counts_only_failures_and_warnings() {
        for (results, summary) in [
            (vec!["ok"], "No problems found"),
            (vec!["skip", "skip"], "No problems found"),
            (vec!["fail", "skip"], "1 problem"),
            (vec!["warn", "skip"], "1 warning"),
            (vec!["fail", "fail"], "2 problems"),
            (vec!["warn", "warn"], "2 warnings"),
            (vec!["fail", "warn"], "1 problem, 1 warning"),
            (
                vec!["fail", "fail", "warn", "warn"],
                "2 problems, 2 warnings",
            ),
        ] {
            let rows: Vec<_> = results
                .iter()
                .map(|result| json!({"name":"check","result":result,"detail":"detail"}))
                .collect();
            let rendered = checklist(&Theme::test(80, Color::Off, true), &[("Ark", &rows)]);
            assert!(
                rendered.ends_with(&format!("\n\n{summary}")),
                "{results:?}: {rendered}"
            );
        }
    }

    /// Names are muted, titles bold, and only the marks take a state's color.
    #[test]
    fn test_checklist_styles_names_and_marks_separately() {
        for (result, mark) in [
            ("ok", "\x1b[1m\x1b[38;2;148;202;110m✓\x1b[0m"),
            ("warn", "\x1b[1m\x1b[38;2;232;162;74m!\x1b[0m"),
            ("fail", "\x1b[1m\x1b[38;2;235;96;112m✗\x1b[0m"),
            ("skip", "\x1b[38;2;124;128;152m·\x1b[0m"),
        ] {
            let rows = [json!({"name":"ark","result":result,"detail":"detail"})];
            let rendered = checklist(
                &Theme::test(80, Color::True, true),
                &[("This computer", &rows)],
            );
            assert!(rendered.starts_with(&format!(
                "\x1b[1mThis computer\x1b[0m\n  {mark} \x1b[38;2;124;128;152mark\x1b[0m  detail\n\n"
            )), "{result}: {rendered:?}");
        }
    }

    /// Long details and hints wrap under their own text, and untrusted text
    /// adds no terminal controls or rows.
    #[test]
    fn test_checklist_wraps_and_escapes_text() {
        let rows = [json!({"name":"connection","result":"fail",
            "detail":"a long connection failure with more words to wrap",
            "hint":"select another device with --device"})];
        let rendered = checklist(&Theme::test(40, Color::Off, true), &[("Ark", &rows)]);
        assert!(
            rendered
                .contains("\n                failure with more words\n                to wrap\n")
        );
        assert!(rendered.contains("\n          --device\n"));
        assert!(
            rendered
                .lines()
                .all(|line| console::measure_text_width(line) <= 40)
        );

        let rows = [
            json!({"name":"ark","result":"warn","detail":"bad\nvalue","hint":"bad\u{001b}[2Jhint"}),
        ];
        let rendered = checklist(&Theme::test(80, Color::Off, true), &[("Ark\nname", &rows)]);
        assert!(rendered.starts_with("Ark\\nname\n  ! ark  bad\\nvalue\n"));
        assert!(rendered.contains("    hint: bad\\u{1b}[2Jhint\n"));
        assert!(!rendered.contains('\x1b'));
    }
}
