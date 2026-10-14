// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Sample names, an adjective and a given name, derived from each person's
//! sample ID and sex.

use super::{Person, Sex, adjectives::ADJECTIVES, female::FEMALE, male::MALE, people::PEOPLE};

/// Multiplier of the affine permutation, 2654435761 reduced modulo 65536.
const MULTIPLIER: u16 = 2_654_435_761u32 as u16;
/// Offset of the affine permutation, 1013904223 reduced modulo 65536.
const OFFSET: u16 = 1_013_904_223u32 as u16;
/// Inverse of [`MULTIPLIER`] modulo 65536, which undoes its multiplication.
const INVERSE: u16 = 12_113;

/// Derives a person's name, taking the given name from the list of their sex.
pub(super) fn encode(person: &Person) -> String {
    // Split the sample ID into its prefix and digits
    let base = match &person.sample[..2] {
        "HG" => 0,
        "NA" => 30_000,
        _ => unreachable!("catalog sample IDs start with HG or NA"),
    };
    let digits: u16 = person.sample[2..]
        .parse()
        .expect("catalog sample IDs end in five digits");

    // Permute the index modulo 65536 by wrapping, then pick the adjective by
    // the high byte and the given name by the low one
    let y = (base + digits)
        .wrapping_mul(MULTIPLIER)
        .wrapping_add(OFFSET);
    let given = match person.sex {
        Sex::Female => &FEMALE,
        Sex::Male => &MALE,
    };
    format!(
        "{}-{}",
        ADJECTIVES[usize::from(y / 256)],
        given[usize::from(y % 256)]
    )
}

/// Finds the person a name stands for, or `None` for a name nobody has.
///
/// Unknown words, malformed names, IDs outside the catalog and a given name of
/// the other sex all decode to nothing. No word is in both name lists, so the
/// list that holds the given name gives the sex.
pub(super) fn decode(name: &str) -> Option<&'static Person> {
    // Refuse anything longer than the longest name, then look up both words
    if name.len() > 18 {
        return None;
    }
    let (adjective, given) = name.split_once('-')?;
    let high = ADJECTIVES.binary_search(&adjective).ok()?;
    let (sex, low) = match FEMALE.binary_search(&given) {
        Ok(index) => (Sex::Female, index),
        Err(_) => (Sex::Male, MALE.binary_search(&given).ok()?),
    };

    // Undo the permutation, then find the person and check their sex
    let y = (high * 256 + low) as u16;
    let x = y.wrapping_sub(OFFSET).wrapping_mul(INVERSE);
    if x >= 60_000 {
        return None;
    }
    let sample = format!("{}{:05}", if x < 30_000 { "HG" } else { "NA" }, x % 30_000);
    let index = PEOPLE
        .binary_search_by_key(&sample.as_str(), |person| person.sample)
        .ok()?;
    let person = &PEOPLE[index];
    (person.sex == sex).then_some(person)
}

/// Tests of the names, every word pair and the screening of the lists.
#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::collections::HashSet;

    /// Checks that every person round trips through a name of their own.
    #[test]
    fn test_catalog_names_are_bijective() {
        let mut names = HashSet::new();
        for person in PEOPLE {
            let name = encode(person);
            assert_eq!(
                decode(&name).map(|person| person.sample),
                Some(person.sample),
                "{name}"
            );
            assert!(names.insert(name), "duplicate name for {}", person.sample);
        }
        assert_eq!(names.len(), 3_202, "all catalog names are distinct");

        // Decode every word pair, which names its own person or nobody
        let mut accepted = 0;
        for adjective in ADJECTIVES {
            for given in FEMALE.into_iter().chain(MALE) {
                let name = format!("{adjective}-{given}");
                if let Some(person) = decode(&name) {
                    assert_eq!(
                        encode(person),
                        name,
                        "{} with the matching sex",
                        person.sample
                    );
                    accepted += 1;
                }
            }
        }
        assert_eq!(accepted, 3_202, "only catalog names decode");
    }

    /// Checks that malformed names, unknown words, given names of the other
    /// sex and pairs outside the catalog decode to nothing.
    #[test]
    fn test_invalid_names_are_rejected() {
        for name in [
            "unknown-owen",
            "notable-unknown",
            "notableowen",
            "notable--owen",
            "notable-owen-",
            "-notable-owen",
            "owen-notable",
            "notable-Owen",
            "",
            "upstanding-stephenx",
            "notable-rachel",
            "affable-giorgio",
            "vibrant-jane",
            "gentle-rachel",
        ] {
            assert!(decode(name).is_none(), "{name:?}");
        }
    }

    /// Checks names against the reviewed preview, by example and by a digest
    /// over the whole cohort.
    #[test]
    fn test_reference_names() {
        for (sample, name) in [
            ("HG00096", "notable-owen"),
            ("HG00097", "assured-lata"),
            ("HG00183", "upstanding-stephen"),
            ("HG03730", "affable-hazel"),
            ("HG04239", "wonderful-enzo"),
            ("NA12878", "excellent-lola"),
            ("NA19238", "noble-sayuri"),
            ("NA21144", "perceptive-naoko"),
        ] {
            let person = &PEOPLE[PEOPLE
                .binary_search_by_key(&sample, |person| person.sample)
                .unwrap()];
            assert_eq!(encode(person), name, "{sample}");
            assert_eq!(
                decode(name).map(|person| person.sample),
                Some(sample),
                "{name}"
            );
        }

        // Hash the cohort as name and sample ID lines in sample ID order, as the
        // preview lists them
        let mut digest = Sha256::new();
        for person in PEOPLE {
            digest.update(format!("{} {}\n", encode(person), person.sample));
        }
        assert_eq!(
            hex::encode(digest.finalize()),
            "445a166775045fe823c468bf078b9b1c986944adde2a14ce2a2bcf2f5377961a",
            "reference preview cohort digest"
        );
    }

    /// Checks the lists' sizes, letters, lengths and order, and that any two
    /// words a typo could confuse are at least two edits apart.
    #[test]
    fn test_wordlist_screening() {
        for (words, limit) in [(&ADJECTIVES, 10), (&FEMALE, 7), (&MALE, 7)] {
            assert_eq!(words.len(), 256, "codec word count");
            for word in words {
                assert!((1..=limit).contains(&word.len()), "length of {word}");
                assert!(word.bytes().all(|byte| byte.is_ascii_lowercase()), "{word}");
            }
            for pair in words.windows(2) {
                assert!(
                    pair[0] < pair[1],
                    "sorted distinct words: {}, {}",
                    pair[0],
                    pair[1]
                );
            }
        }

        // Screen the given names as one set, so a single typo cannot cross the
        // sexes
        for word in FEMALE {
            assert!(!MALE.contains(&word), "name in both lists: {word}");
        }
        let given: Vec<_> = FEMALE.into_iter().chain(MALE).collect();
        for words in [ADJECTIVES.as_slice(), given.as_slice()] {
            for (index, word) in words.iter().enumerate() {
                for other in &words[index + 1..] {
                    assert!(distance(word, other) >= 2, "{word}, {other}");
                }
            }
        }
    }

    /// Checks that the distance counts a swap of neighboring letters as one
    /// edit, and never edits a substring twice.
    #[test]
    fn test_distance_counts_adjacent_swaps() {
        for (left, right, expected) in [
            ("", "abc", 3),
            ("same", "same", 0),
            ("form", "from", 1),
            ("abc", "acb", 1),
            ("ca", "abc", 3),
            ("ab", "ba", 1),
        ] {
            assert_eq!(distance(left, right), expected, "{left}, {right}");
            assert_eq!(distance(right, left), expected, "{right}, {left}");
        }
    }

    /// Counts the edits between two words, a swap of neighboring letters
    /// being one, as optimal string alignment does.
    fn distance(left: &str, right: &str) -> usize {
        // Seed the cost of turning each prefix into an empty word
        let left = left.as_bytes();
        let right = right.as_bytes();
        let mut costs = vec![vec![0; right.len() + 1]; left.len() + 1];
        for (index, row) in costs.iter_mut().enumerate() {
            row[0] = index;
        }
        for (index, cost) in costs[0].iter_mut().enumerate() {
            *cost = index;
        }

        // Take the cheapest of an insertion, a deletion, a substitution or a swap
        for i in 1..=left.len() {
            for j in 1..=right.len() {
                costs[i][j] = (costs[i - 1][j] + 1)
                    .min(costs[i][j - 1] + 1)
                    .min(costs[i - 1][j - 1] + usize::from(left[i - 1] != right[j - 1]));
                if i > 1 && j > 1 && left[i - 1] == right[j - 2] && left[i - 2] == right[j - 1] {
                    costs[i][j] = costs[i][j].min(costs[i - 2][j - 2] + 1);
                }
            }
        }
        costs[left.len()][right.len()]
    }
}
