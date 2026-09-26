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
        return if seconds >= 60 {
            format!("{}m {}s", seconds / 60, seconds % 60)
        } else {
            format!("{seconds} s")
        };
    }

    // Other values read as text, styled by what their key means
    let text = super::scalar(value);
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
        "environment" => theme.paint(
            match text.as_str() {
                "release" => Role::Accent,
                "staging" => Role::Staging,
                "develop" => Role::Develop,
                _ => Role::Default,
            },
            text,
        ),
        "flags" => text
            .split(", ")
            .map(|flag| {
                if flag == "installed" {
                    theme.paint(Role::Success, theme.glyph("\u{2713}", "installed"))
                } else {
                    theme.paint(Role::Attention, flag)
                }
            })
            .collect::<Vec<_>>()
            .join(&theme.separator()),
        "slot" | "locator" | "identity" | "pubkey" | "url" | "requires" | "required_by" => {
            theme.paint(Role::Accent, text)
        }
        "damage" | "mismatch" => theme.paint(Role::Failure, text),
        "synced" | "paired" | "unlocked" | "verified" | "active" | "enrolled" | "success"
        | "returned" | "installed" => {
            if let Some(yes) = value.as_bool() {
                theme.mark(if yes { Role::Success } else { Role::Attention }, &text)
            } else {
                text
            }
        }
        _ => text,
    }
}

/// Aligns label/value rows, stacking them when labels consume the available width.
///
/// An empty pair separates groups; an empty label introduces an unlabeled row.
pub(crate) fn block(theme: &Theme, rows: &[(String, String)]) -> String {
    let labels = rows
        .iter()
        .map(|(label, _)| console::measure_text_width(label))
        .max()
        .unwrap_or(0);
    let stacked = labels + 8 > theme.width;
    rows.iter()
        .map(|(label, value)| {
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
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Renders nested fields as readable labels with units in their values.
pub(crate) fn document(theme: &Theme, value: &Value) -> String {
    let mut rows = Vec::new();
    fields(theme, "", "", value, &mut rows);
    block(theme, &rows)
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

/// Fits a table by shrinking one free-text column, then falls back to blocks.
///
/// Selectors, hashes and other actionable fields are never ellipsized by the table.
pub(super) fn table(
    theme: &Theme,
    rows: &[Value],
    columns: &[(&str, &str)],
    groups: &[String],
) -> String {
    // Style every cell and measure each column against its header
    let cells: Vec<Vec<_>> = rows
        .iter()
        .map(|row| {
            columns
                .iter()
                .map(|(_, key)| value(theme, key, &row[*key]))
                .collect()
        })
        .collect();
    let mut widths: Vec<_> = columns
        .iter()
        .enumerate()
        .map(|(i, (label, _))| {
            cells
                .iter()
                .map(|row| console::measure_text_width(&row[i]))
                .max()
                .unwrap_or(0)
                .max(label.len())
        })
        .collect();

    // Shrink the one free-text column to fit, never below its header or 8 cells
    let total =
        |widths: &[usize]| 2 + widths.iter().sum::<usize>() + columns.len().saturating_sub(1) * 2;
    let flexible = columns
        .iter()
        .position(|(_, key)| matches!(*key, "summary" | "description" | "name"));
    if let Some(index) = flexible {
        widths[index] = widths[index]
            .saturating_sub(total(&widths).saturating_sub(theme.width))
            .max(columns[index].0.len().max(8));
    }

    // A table that still overflows falls back to one block per row
    if total(&widths) > theme.width {
        return cells
            .iter()
            .map(|row| {
                block(
                    theme,
                    &columns
                        .iter()
                        .zip(row)
                        .map(|((label, _), cell)| (label.to_string(), cell.clone()))
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n");
    }

    // Lay out a line, right-aligning sizes, durations and numeric columns
    let line = |cells: &[String], header: bool| {
        let mut result = String::from("  ");
        for (i, cell) in cells.iter().enumerate() {
            let cell = if Some(i) == flexible {
                theme.truncate(cell, widths[i])
            } else {
                cell.clone()
            };
            let right = columns[i].1.ends_with("_bytes")
                || columns[i].1.ends_with("_seconds")
                || rows.iter().any(|row| row[columns[i].1].is_number());
            let padding = " ".repeat(widths[i].saturating_sub(console::measure_text_width(&cell)));
            if right {
                result.push_str(&padding);
            }
            result.push_str(&if header {
                theme.paint(Role::Muted, cell)
            } else {
                cell
            });
            if i + 1 < cells.len() {
                if !right {
                    result.push_str(&padding);
                }
                result.push_str("  ");
            }
        }
        result
    };

    // Print the header, then each row under a label whenever its group changes
    let mut lines = vec![line(
        &columns
            .iter()
            .map(|(label, _)| label.to_string())
            .collect::<Vec<_>>(),
        true,
    )];
    let mut previous = None;
    for (index, row) in cells.iter().enumerate() {
        if let Some(group) = groups.get(index)
            && previous != Some(group)
        {
            if previous.is_some() {
                lines.push(String::new());
            }
            lines.push(format!("  {}", theme.paint(Role::Accent, group)));
            previous = Some(group);
        }
        lines.push(line(row, false));
    }
    lines.join("\n")
}

/// Aligns diagnostic outcomes, keeping skipped checks explicit and hints nearby.
pub(super) fn checklist(theme: &Theme, rows: &[Value]) -> String {
    let labels = rows
        .iter()
        .map(|row| console::measure_text_width(row["name"].as_str().unwrap_or("")))
        .max()
        .unwrap_or(0);
    rows.iter()
        .map(|row| {
            let name = row["name"].as_str().unwrap_or("-");
            let role = match row["result"].as_str() {
                Some("ok") => Role::Success,
                Some("fail") => Role::Failure,
                Some("warn") => Role::Attention,
                _ => Role::Muted,
            };
            let result = row["result"].as_str().unwrap_or("-");
            let detail = row["detail"].as_str().unwrap_or("-");
            let detail = if result == "skip" {
                format!("skipped: {detail}")
            } else {
                detail.to_string()
            };
            let name = theme.mark(role, name);
            // Pad marked names to one column, which the two-letter ASCII mark
            // widens
            let width = labels + if theme.unicode { 2 } else { 3 };
            let line = format!(
                "  {}{}  {}",
                name,
                " ".repeat(width.saturating_sub(console::measure_text_width(&name))),
                theme.paint(Role::Muted, detail)
            );
            let mut line = style::wrap(&line, theme.width, 4);
            if let Some(hint) = row["hint"].as_str() {
                line.push('\n');
                line.push_str(&style::wrap(
                    &format!(
                        "    {} {}",
                        theme.paint(Role::Accent, "hint:"),
                        theme.inline(hint)
                    ),
                    theme.width,
                    6,
                ));
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Tests of the human layouts of values, documents, tables and checklists.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::Color;
    use serde_json::json;

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
            "  Firmware version    1.2.3\n  Firmware published  -\n  Paired              \x1b[1m\u{2713} yes\x1b[0m"
        );
        assert_eq!(value(&theme, "name", &json!("failed")), "failed");
        assert_eq!(value(&theme, "serial", &Value::Null), "unverified");
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
            assert!(rendered.contains("2m 0s"));
            assert!(!rendered.contains("bytes"));
            assert!(!rendered.contains("seconds"));
            assert!(!rendered.contains("download."));
            assert!(rendered.contains("Download size"));
        }
    }

    /// A table right-aligns sizes and marks states.
    #[test]
    fn table_aligns_sizes_and_marks_states() {
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
                &[]
            ),
            "  SLOT              STATE          SIZE\n  \x1b[1mreference-genome\x1b[0m  \x1b[1m\u{2713} filled\x1b[0m    3.0 GiB\n  \x1b[1mvariant-catalog\x1b[0m   \u{00b7} empty           -\n  \x1b[1mgene-annotations\x1b[0m  \x1b[1m\u{2713} filled\x1b[0m  256.0 MiB"
        );
    }

    /// Tables show sizes in the same units as blocks.
    #[test]
    fn tables_and_blocks_agree_on_byte_units() {
        let theme = Theme::test(80, Color::Off, false);
        let rows = [
            json!({"slot":"reference-genome","size_bytes":1_u64 << 28}),
            json!({"slot":"variant-catalog","size_bytes":3_u64 << 30}),
        ];
        let rendered = table(
            &theme,
            &rows,
            &[("SLOT", "slot"), ("SIZE", "size_bytes")],
            &[],
        );
        for row in &rows {
            assert!(rendered.contains(&value(&theme, "size_bytes", &row["size_bytes"])));
        }
    }

    /// A narrow table fits the width and keeps actionable values whole.
    #[test]
    fn narrow_tables_keep_actionable_values() {
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
            &[],
        );
        assert!(
            rendered
                .lines()
                .all(|line| console::measure_text_width(line) <= theme.width)
        );
        let plain = console::strip_ansi_codes(&rendered);
        let joined = plain.split_whitespace().collect::<String>();
        assert!(joined.contains(locator));
        assert!(joined.contains("ark-123456789"));
        assert!(!plain.contains('\u{2026}'));
    }

    /// A table truncates only its free-text column to fit.
    #[test]
    fn table_truncates_only_free_text() {
        let theme = Theme::test(40, Color::Off, true);
        let rows =
            [json!({"name":"A name much longer than the available space","serial":"ark-123"})];
        let rendered = table(
            &theme,
            &rows,
            &[("NAME", "name"), ("SERIAL", "serial")],
            &[],
        );
        assert!(rendered.contains('\u{2026}'));
        assert!(rendered.contains("ark-123"));
        assert!(
            rendered
                .lines()
                .all(|line| console::measure_text_width(line) <= theme.width)
        );
    }

    /// Every result carries its own mark, and each hint stays under its check.
    #[test]
    fn test_checklist_keeps_skips_explicit_and_hints_local() {
        let theme = Theme::test(80, Color::Basic, true);
        let rows = [
            json!({"name":"usb","result":"ok","detail":"1 device found","hint":null}),
            json!({"name":"relay","result":"fail","detail":"no answer","hint":"open `Ark Companion`"}),
            json!({"name":"slots","result":"skip","detail":"Ark locked","hint":null}),
            json!({"name":"tool","result":"warn","detail":"new release","hint":"run `brew upgrade ark-cli`"}),
        ];
        assert_eq!(
            checklist(&theme, &rows),
            "  \x1b[1m\u{2713} usb\x1b[0m    1 device found\n  \x1b[1m\u{2717} relay\x1b[0m  no answer\n    \x1b[1mhint:\x1b[0m open \x1b[1mArk Companion\x1b[0m\n  \u{00b7} slots  skipped: Ark locked\n  \x1b[1m! tool\x1b[0m   new release\n    \x1b[1mhint:\x1b[0m run \x1b[1mbrew upgrade ark-cli\x1b[0m"
        );
    }
}
