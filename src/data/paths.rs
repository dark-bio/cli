// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! The map of data paths an app can read, as a compact tree or complete JSON.

use crate::{
    error::Error,
    output::Output,
    style::{self, Role, Theme},
};
use darkbio_connect::schema::DatasetPath;
use serde_json::{Value, json};

/// Prints every path entry, hinting at the slot list when any data is missing.
pub(super) fn print(output: &Output, paths: &[DatasetPath]) -> Result<(), Error> {
    let entries: Vec<_> = paths.iter().map(metadata).collect();
    output.document_with(&json!({"paths": entries}), |theme| render(theme, paths))?;
    if paths.iter().any(|path| !path.available) {
        output.event("hint", "some paths are unavailable; run `ark data list`");
    }
    Ok(())
}

/// Converts one entry to JSON, naming its `desc` field `description` as slot
/// output does.
fn metadata(path: &DatasetPath) -> Value {
    json!({
        "path": path.path,
        "directory": path.directory,
        "grantable": path.grantable,
        "available": path.available,
        "description": path.desc,
        "format": path.format,
        "examples": path.examples,
    })
}

/// Renders the entries as a tree, nesting each under its closest listed parent.
///
/// Only the topmost unavailable entry of a subtree carries the mark. Examples
/// line up in one column right of the widest row and wrap within it.
fn render(theme: &Theme, paths: &[DatasetPath]) -> String {
    if paths.is_empty() {
        return format!("  {}", theme.paint(Role::Muted, "No dataset paths"));
    }

    // Lay out one row per entry, indented under its closest listed parent
    let mut rows = Vec::new();
    let mut parents: Vec<&DatasetPath> = Vec::new();
    for path in paths {
        // Names shorten only under a listed parent, so a root keeps its full path
        while parents.last().is_some_and(|parent| {
            !path
                .path
                .strip_prefix(&parent.path)
                .is_some_and(|suffix| suffix.starts_with('/'))
        }) {
            parents.pop();
        }
        let name = parents.last().map_or(path.path.as_str(), |parent| {
            &path.path[parent.path.len() + 1..]
        });
        let indent = 2 + 2 * parents.len();
        let name = format!("{name}{}", if path.directory { "/" } else { "" });
        let mut line = format!(
            "{}{}",
            " ".repeat(indent),
            theme.paint(
                if path.directory {
                    Role::Accent
                } else {
                    Role::Default
                },
                name
            )
        );
        if path.grantable {
            line.push_str(&theme.paint(Role::Muted, " +"));
        }
        if !path.available && parents.last().is_none_or(|parent| parent.available) {
            line.push_str(&theme.paint(Role::Attention, " !"));
        }
        rows.push((line, indent, &path.examples));
        if path.directory {
            parents.push(path);
        }
    }

    // Examples start 2 cells right of the widest row, below a legend line
    let column = rows
        .iter()
        .map(|(line, _, _)| console::measure_text_width(line))
        .max()
        .unwrap_or(0)
        + 2;
    let mut lines = vec![style::wrap(
        &format!(
            "  {}",
            theme.paint(
                Role::Muted,
                "/ directory, + grantable, ! unavailable; details with --json"
            )
        ),
        theme.width,
        2,
    )];

    // Rows without examples wrap under their own indent, and examples wrap
    // within their column
    for (line, indent, examples) in rows {
        if examples.is_empty() {
            lines.push(style::wrap(&line, theme.width, indent + 2));
            continue;
        }
        let padding = " ".repeat(column - console::measure_text_width(&line));
        let examples = theme.paint(Role::Muted, examples.join(", "));
        lines.push(style::wrap(
            &format!("{line}{padding}{examples}"),
            theme.width,
            column,
        ));
    }
    lines.join("\n")
}

/// Tests of the path tree layout.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::Color;

    /// Checks that unknown roots, missing parents and long names keep every
    /// path component in tree order.
    ///
    /// Unavailable ancestors hide repeated marks, and examples share one column
    /// that wraps within itself.
    #[test]
    fn future_paths_keep_their_hierarchy() {
        // The layout is exact at 80 columns without color
        let paths = [
            DatasetPath {
                path: "v1/sample".into(),
                directory: true,
                available: true,
                grantable: true,
                ..Default::default()
            },
            DatasetPath {
                path: "v1/sample/groups/<item>".into(),
                directory: true,
                examples: ["alpha", "beta"].map(String::from).to_vec(),
                ..Default::default()
            },
            DatasetPath {
                path: "v1/sample/groups/<item>/value".into(),
                examples: [
                    "A/G", "T|T", "A", "./.", "A/.", ".", "AT/A", "T/*", "A/<DEL>",
                ]
                .map(String::from)
                .to_vec(),
                ..Default::default()
            },
            DatasetPath {
                path: "v1/sample/summary".into(),
                available: true,
                ..Default::default()
            },
            DatasetPath {
                path: "v2/sample/a-long-example-directory-name".into(),
                directory: true,
                ..Default::default()
            },
        ];
        let text = render(&Theme::test(80, Color::Off, false), &paths);
        assert_eq!(
            text,
            format!(
                "  / directory, + grantable, ! unavailable; details with --json\n  v1/sample/ +\n    groups/<item>/ !{}alpha, beta\n      value{}A/G, T|T, A, ./., A/., ., AT/A,\n{}T/*, A/<DEL>\n    summary\n  v2/sample/a-long-example-directory-name/ !",
                " ".repeat(26),
                " ".repeat(35),
                " ".repeat(46)
            )
        );

        // Every width and color depth fits the lines and keeps every component
        for width in [20, 40, 80] {
            for color in [Color::Off, Color::Basic, Color::True] {
                let text = render(&Theme::test(width, color, true), &paths);
                assert!(
                    text.lines()
                        .all(|line| console::measure_text_width(line) <= width)
                );
                let plain = console::strip_ansi_codes(&text);
                let joined = plain.split_whitespace().collect::<String>();
                assert!(joined.contains("v2/sample/a-long-example-directory-name/!"));
                assert!(joined.contains(
                    "groups/<item>/!alpha,betavalueA/G,T|T,A,./.,A/.,.,AT/A,T/*,A/<DEL>summary"
                ));
            }
        }

        // An empty map prints a placeholder
        assert_eq!(
            render(&Theme::test(80, Color::Off, false), &[]),
            "  No dataset paths"
        );
    }
}
