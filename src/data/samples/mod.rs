// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! The public sample catalog, the pins of its files and `ark data samples`.

mod adjectives;
mod codec;
mod female;
mod male;
mod people;
mod pins;
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
use clap::{ValueEnum, builder::TypedValueParser};
use darkbio_connect::schema::SlotDownload;
use people::PEOPLE;
use pins::PINS;
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

/// A person of the catalog, as `--sample` selects them by name or ID.
#[derive(Clone, Copy)]
pub(crate) struct Sample(
    /// The selected person.
    &'static Person,
);

impl Sample {
    /// Finds the catalog person a sample ID or a name stands for.
    fn parse(value: &str) -> Option<Self> {
        codec::decode(value)
            .or_else(|| {
                PEOPLE
                    .binary_search_by_key(&value, |person| person.sample)
                    .ok()
                    .map(|index| &PEOPLE[index])
            })
            .map(Self)
    }

    /// Returns the sample ID, such as `NA12878`.
    pub(super) fn id(self) -> &'static str {
        self.0.sample
    }

    /// Derives the person's name, such as `affable-hazel`.
    pub(super) fn name(self) -> String {
        codec::encode(self.0)
    }

    /// Builds an offer for the person's DRAGEN 4.4.7 calls from their pin.
    pub(super) fn download(self) -> SlotDownload {
        let index = PINS
            .binary_search_by_key(&self.id(), |pin| pin.sample)
            .expect("every catalog person has exactly one pin");
        let pin = &PINS[index];
        let sample = self.id();
        SlotDownload {
            url: format!(
                "https://1000genomes-dragen-v4-4-7.s3.amazonaws.com/data/individuals/hg38-alt_masked.cnv.graph.hla.methyl_cg.rna-11-r5.0-2/{sample}/{sample}.hard-filtered.vcf.gz"
            ),
            bytes: pin.size,
            sha256: pin.sha256.into(),
        }
    }
}

/// Pinned size and SHA-256 of one person's variant call file.
struct Pin {
    /// Sample ID of the file's person.
    sample: &'static str,
    /// Length in bytes.
    size: u64,
    /// SHA-256 of the file, in lowercase hex.
    sha256: &'static str,
}

/// Parser of `--sample`, which takes names and IDs and completes each name
/// with its sample ID, population and sex.
#[derive(Clone)]
pub(crate) struct Parser;

impl TypedValueParser for Parser {
    type Value = Sample;

    fn parse_ref(
        &self,
        command: &clap::Command,
        argument: Option<&clap::Arg>,
        value: &std::ffi::OsStr,
    ) -> Result<Sample, clap::Error> {
        let value = clap::builder::StringValueParser::new().parse_ref(command, argument, value)?;
        Sample::parse(&value).ok_or_else(|| {
            let mut error =
                clap::Error::new(clap::error::ErrorKind::ValueValidation).with_cmd(command);
            error.insert(
                clap::error::ContextKind::InvalidArg,
                clap::error::ContextValue::String("--sample <NAME>".into()),
            );
            error.insert(
                clap::error::ContextKind::InvalidValue,
                clap::error::ContextValue::String(value),
            );
            error
        })
    }

    fn possible_values(
        &self,
    ) -> Option<Box<dyn Iterator<Item = clap::builder::PossibleValue> + '_>> {
        Some(Box::new(PEOPLE.iter().map(|person| {
            clap::builder::PossibleValue::new(Sample(person).name()).help(format!(
                "{} · {} · {}",
                person.sample,
                person.population,
                match person.sex {
                    Sex::Male => "male",
                    Sex::Female => "female",
                }
            ))
        })))
    }
}

/// Lists every name and sample ID, for suggesting a near match to a typo.
pub(crate) fn selectors() -> impl Iterator<Item = String> {
    PEOPLE
        .iter()
        .flat_map(|person| [Sample(person).name(), person.sample.to_owned()])
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
    PEOPLE
        .iter()
        .filter(|person| population.is_none_or(|code| code == person.population))
        .filter(|person| sex.is_none_or(|sex| sex == person.sex))
        .map(|person| {
            json!({
                "name": codec::encode(person),
                "sample": person.sample,
                "population": person.population,
                "sex": person.sex,
                "father": person.father,
                "mother": person.mother,
            })
        })
        .collect()
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
    let mut columns = vec![Column {
        label: "SAMPLE",
        key: "sample",
        accent: true,
        flexible: false,
    }];
    if all {
        columns.push(Column {
            label: "POPULATION",
            key: "population",
            accent: true,
            flexible: false,
        });
    }
    columns.push(Column::from(("SEX", "sex")));
    columns.push(Column {
        label: "NAME",
        key: "name",
        accent: true,
        flexible: false,
    });
    human::table(theme, rows, &columns, Some("No samples found"))
}

/// Tests of the catalog's data, its selections and its tables.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::Color;

    /// Checks that every person has one nonempty pin, in sample ID order.
    #[test]
    fn test_every_person_has_one_pin() {
        for pair in PINS.windows(2) {
            assert!(
                pair[0].sample < pair[1].sample,
                "pin order at {}",
                pair[0].sample
            );
        }
        for pin in PINS {
            assert!(pin.size > 0, "nonempty file for {}", pin.sample);
            assert!(
                PEOPLE
                    .binary_search_by_key(&pin.sample, |person| person.sample)
                    .is_ok(),
                "pin belongs to the catalog: {}",
                pin.sample
            );
        }
        assert_eq!(PINS.len(), PEOPLE.len(), "one pin for every catalog person");
        for (pin, person) in PINS.iter().zip(PEOPLE) {
            assert_eq!(pin.sample, person.sample, "pin for {}", person.sample);
        }
    }

    /// Checks that names and IDs select only people in the catalog.
    #[test]
    fn test_sample_selection_requires_a_catalog_person() {
        for person in PEOPLE {
            let name = codec::encode(person);
            for value in [person.sample, name.as_str()] {
                assert_eq!(Sample::parse(value).unwrap().id(), person.sample, "{value}");
            }
        }
        for value in [
            "HG00000",
            "hg00096",
            "HG30000",
            "NA30000",
            "XX00001",
            "HG0001",
            "HG+0096",
            "HG000096",
            "HGé096",
            "NA99999",
            "notable-owe",
            "owen-notable",
            "notable-rachel",
            "vibrant-jane",
            "",
        ] {
            assert!(Sample::parse(value).is_none(), "unknown sample {value:?}");
        }
    }

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
                "name": "humble-barbara", "sample": "NA18939", "population": "JPT",
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
            ["SAMPLE", "SEX", "NAME"],
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
                "name": "innovative-josefa", "sample": "HG00405", "population": "CHS",
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
            ["SAMPLE", "POPULATION", "SEX", "NAME"],
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
            for text in ["notable-owen", "HG00096", "GBR"] {
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
