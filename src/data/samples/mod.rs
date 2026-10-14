// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! The public sample catalog and the listing of `ark data samples`.

mod adjectives;
mod codec;
mod nouns;
mod people;
mod populations;

pub(crate) use populations::POPULATIONS;

use crate::{
    error::Error,
    output::{
        Output,
        human::{self, Column},
    },
    style::Theme,
};
use clap::ValueEnum;
use people::PEOPLE;
use serde::Serialize;
use serde_json::{Value, json};

/// Sex of a person, as the pedigree records it.
///
/// Clap shows the variants' doc comments as completion help, so they have none.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Sex {
    Male,
    Female,
}

/// A person of the cohort.
struct Person {
    /// Sample ID, such as `NA12878`.
    sample: &'static str,
    /// Code of the person's population, one of [`POPULATIONS`].
    population: &'static str,
    /// Sex as the pedigree records it.
    sex: Sex,
    /// Father's sample ID, when he is in the cohort.
    father: Option<&'static str>,
    /// Mother's sample ID, when she is in the cohort.
    mother: Option<&'static str>,
}

/// A population of the cohort.
pub(crate) struct Population {
    /// Three-letter code, such as `JPT`, which the command takes.
    pub code: &'static str,
    /// Description, such as `Japanese in Tokyo, Japan`.
    pub description: &'static str,
}

/// Prints the populations, or the people of one population or of all.
///
/// It reads only the built-in tables, so it needs no Ark.
pub(super) fn print(
    output: &Output,
    population: Option<&str>,
    sex: Option<Sex>,
    all: bool,
) -> Result<(), Error> {
    if population.is_none() && !all {
        let rows = population_rows();
        output.document_with(&json!({"populations": rows}), |theme| {
            population_table(theme, &rows)
        })
    } else {
        let rows = sample_rows(population, sex);
        output.document_with(&json!({"samples": rows}), |theme| {
            sample_table(theme, &rows, all)
        })
    }
}

/// Builds a row per population in code order, counting its people.
fn population_rows() -> Vec<Value> {
    POPULATIONS.iter().map(|population| json!({
        "population": population.code,
        "count": PEOPLE.iter().filter(|person| person.population == population.code).count(),
        "description": population.description,
    })).collect()
}

/// Builds a row per person of the given population and sex, in sample ID order.
fn sample_rows(population: Option<&str>, sex: Option<Sex>) -> Vec<Value> {
    PEOPLE.iter()
        .filter(|person| population.is_none_or(|code| code == person.population))
        .filter(|person| sex.is_none_or(|sex| sex == person.sex))
        .map(|person| json!({
            "name": codec::encode(person.sample).expect("every cohort sample ID lies in the codec's domain"),
            "sample": person.sample,
            "population": person.population,
            "sex": person.sex,
            "father": person.father,
            "mother": person.mother,
        })).collect()
}

/// Lays out the populations, letting only the descriptions truncate.
fn population_table(theme: &Theme, rows: &[Value]) -> String {
    human::table(
        theme,
        rows,
        &[
            Column {
                label: "POPULATION",
                key: "population",
                accent: true,
                flexible: false,
            },
            Column::from(("SAMPLES", "count")),
            Column::from(("DESCRIPTION", "description")),
        ],
        Some("No populations found"),
    )
}

/// Lays out people, adding a population column when they span populations.
fn sample_table(theme: &Theme, rows: &[Value], all: bool) -> String {
    let mut columns = vec![
        Column {
            label: "NAME",
            key: "name",
            accent: true,
            flexible: false,
        },
        Column {
            label: "SAMPLE",
            key: "sample",
            accent: true,
            flexible: false,
        },
    ];
    if all {
        columns.push(Column {
            label: "POPULATION",
            key: "population",
            accent: true,
            flexible: false,
        });
    }
    columns.push(Column::from(("SEX", "sex")));
    human::table(theme, rows, &columns, Some("No samples found"))
}

/// Tests of the catalog's data, its selections and its tables.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::Color;

    /// Checks the catalog's size, order, population codes, counts and parents.
    #[test]
    fn test_catalog_integrity() {
        assert_eq!(PEOPLE.len(), 3_202, "cohort size");
        assert_eq!(POPULATIONS.len(), 26, "population count");
        for pair in PEOPLE.windows(2) {
            assert!(
                pair[0].sample < pair[1].sample,
                "order at {}",
                pair[0].sample
            );
        }
        for pair in POPULATIONS.windows(2) {
            assert!(pair[0].code < pair[1].code, "order at {}", pair[0].code);
        }
        for person in PEOPLE {
            assert!(
                POPULATIONS
                    .iter()
                    .any(|population| population.code == person.population),
                "{} population",
                person.sample
            );
            for (parent, sex) in [(person.father, Sex::Male), (person.mother, Sex::Female)] {
                if let Some(parent) = parent {
                    let index = PEOPLE
                        .binary_search_by_key(&parent, |person| person.sample)
                        .expect("every recorded parent belongs to the cohort");
                    assert_eq!(PEOPLE[index].sex, sex, "{} parent {parent}", person.sample);
                }
            }
        }

        // Count each population's people from the cohort itself
        let rows = population_rows();
        assert_eq!(
            rows.iter()
                .map(|row| row["count"].as_u64().unwrap())
                .sum::<u64>(),
            3_202,
            "population total"
        );
        assert_eq!(
            rows.iter().find(|row| row["population"] == "JPT").unwrap()["count"],
            104,
            "JPT size"
        );
    }

    /// Checks the population table's first rows, alignment and narrow layouts.
    #[test]
    fn test_population_table_layout() {
        let rows = population_rows();
        let rendered = population_table(&Theme::test(80, Color::Off, true), &rows);
        assert_eq!(
            rendered.lines().take(3).collect::<Vec<_>>(),
            [
                "POPULATION  SAMPLES  DESCRIPTION",
                "ACB             116  African Caribbean in Barbados",
                "ASW              74  African Ancestry in Southwest US",
            ],
            "population table first rows and number alignment"
        );
        assert_eq!(rendered.lines().count(), 27, "header and all populations");

        // Narrow terminals truncate descriptions, then fall back to blocks
        for unicode in [true, false] {
            for width in [20, 32, 80] {
                let rendered = population_table(&Theme::test(width, Color::Off, unicode), &rows);
                assert!(
                    rendered
                        .lines()
                        .all(|line| console::measure_text_width(line) <= width),
                    "width={width}, unicode={unicode}"
                );
                if width == 32 {
                    assert!(
                        rendered.contains(if unicode {
                            "African Ca…"
                        } else {
                            "African ..."
                        }),
                        "description truncation, unicode={unicode}: {rendered}"
                    );
                }
                if width == 20 {
                    assert!(
                        rendered.starts_with("  POPULATION"),
                        "narrow table falls back to blocks"
                    );
                }
            }
        }
    }

    /// Checks the population and sex filters, the row fields and both tables.
    #[test]
    fn test_sample_selection_and_layout() {
        let rows = sample_rows(Some("JPT"), Some(Sex::Female));
        assert_eq!(rows.len(), 48, "female JPT samples");
        assert_eq!(
            rows[0],
            json!({
                "name": "laced-brook", "sample": "NA18939", "population": "JPT",
                "sex": "female", "father": null, "mother": null,
            }),
            "first female JPT sample"
        );
        assert!(
            rows.iter()
                .all(|row| row["sex"] == "female" && row["population"] == "JPT"),
            "both filters apply"
        );
        let rendered = sample_table(&Theme::test(80, Color::Off, true), &rows, false);
        assert_eq!(
            rendered
                .lines()
                .next()
                .unwrap()
                .split_whitespace()
                .collect::<Vec<_>>(),
            ["NAME", "SAMPLE", "SEX"],
            "population view columns"
        );
        assert_eq!(rendered.lines().count(), 49, "header and filtered rows");

        // The whole cohort adds a population column and keeps parents as IDs
        let rows = sample_rows(None, None);
        assert_eq!(rows.len(), 3_202, "all people");
        assert!(
            rows.windows(2)
                .all(|pair| pair[0]["sample"].as_str() < pair[1]["sample"].as_str()),
            "sample ID order"
        );
        assert_eq!(
            rows.iter().find(|row| row["sample"] == "HG00405").unwrap(),
            &json!({
                "name": "lemon-grass", "sample": "HG00405", "population": "CHS",
                "sex": "female", "father": "HG00403", "mother": "HG00404",
            }),
            "in-cohort parents are sample IDs"
        );
        let rendered = sample_table(&Theme::test(80, Color::Off, true), &rows, true);
        assert_eq!(
            rendered
                .lines()
                .next()
                .unwrap()
                .split_whitespace()
                .collect::<Vec<_>>(),
            ["NAME", "SAMPLE", "POPULATION", "SEX"],
            "all-samples view columns"
        );
        assert_eq!(rendered.lines().count(), 3_203, "header and entire cohort");
        for (sex, count) in [(Sex::Male, 1_599), (Sex::Female, 1_603)] {
            let selected = sample_rows(None, Some(sex));
            assert_eq!(selected.len(), count, "{sex:?}");
        }
    }

    /// Checks that names, IDs and codes print whole and in Accent, blocks too.
    #[test]
    fn test_selectors_keep_accent_and_never_truncate() {
        let rows = sample_rows(Some("GBR"), None);
        for width in [32, 80] {
            let theme = Theme::test(width, Color::True, true);
            let rendered = sample_table(&theme, &rows, true);
            for text in ["orange-ravine", "HG00096", "GBR"] {
                assert!(
                    rendered.contains(&format!("\x1b[1m\x1b[38;2;137;180;250m{text}\x1b[0m")),
                    "width={width}, {text}"
                );
            }
            assert!(
                !rendered.contains('…'),
                "selectors never ellipsize at {width}"
            );
            assert!(
                rendered
                    .lines()
                    .all(|line| console::measure_text_width(line) <= width),
                "sample view width {width}"
            );
        }
        let rendered = population_table(&Theme::test(80, Color::True, true), &population_rows());
        assert!(
            rendered.contains("\x1b[1m\x1b[38;2;137;180;250mJPT\x1b[0m"),
            "population code Accent"
        );
        assert!(
            rendered.starts_with("\x1b[38;2;124;128;152mPOPULATION\x1b[0m"),
            "headers are Muted"
        );
    }

    /// Checks that a table without people prints its sentence instead.
    #[test]
    fn test_empty_sample_table() {
        let rows = sample_rows(Some("missing"), None);
        for all in [false, true] {
            assert_eq!(
                sample_table(&Theme::test(80, Color::Off, true), &rows, all),
                "No samples found",
                "all={all}"
            );
        }
    }
}
