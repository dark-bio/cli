// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Two-word names for sample IDs, derived both ways by arithmetic.

use super::{adjectives::ADJECTIVES, nouns::NOUNS};

/// Multiplier of the affine permutation, 2654435761 reduced modulo 65536.
const MULTIPLIER: u16 = 2_654_435_761u32 as u16;
/// Offset of the affine permutation, 1013904223 reduced modulo 65536.
const OFFSET: u16 = 1_013_904_223u32 as u16;
/// Inverse of [`MULTIPLIER`] modulo 65536, which undoes its multiplication.
const INVERSE: u16 = 12_113;

/// Derives the name of a sample ID, or `None` for an ID outside the domain.
///
/// The domain is `HG` or `NA` followed by five digits below 30000. IDs outside
/// the cohort have names too, so the catalog decides who exists.
pub(super) fn encode(sample: &str) -> Option<String> {
    // Split the ID into prefix and digits, refusing any ID outside the domain
    if sample.len() != 7 {
        return None;
    }
    let base = match sample.get(..2)? {
        "HG" => 0,
        "NA" => 30_000,
        _ => return None,
    };
    let digits = &sample[2..];
    if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let digits: u16 = digits.parse().ok()?;
    if digits >= 30_000 {
        return None;
    }

    // Permute the index modulo 65536 by wrapping, then pick a word per byte
    let y = (base + digits)
        .wrapping_mul(MULTIPLIER)
        .wrapping_add(OFFSET);
    Some(format!(
        "{}-{}",
        ADJECTIVES[usize::from(y / 256)],
        NOUNS[usize::from(y % 256)]
    ))
}

/// Derives the sample ID a name stands for, or `None` for a name no ID has.
///
/// Unknown words, a missing or extra hyphen and the 5,536 pairs outside the
/// domain decode to nothing. Words of a list differ in two letters or more, so
/// a one-letter slip never names another person.
pub(super) fn decode(name: &str) -> Option<String> {
    // Refuse anything longer than two eight-letter words, then look both up
    if name.len() > 17 {
        return None;
    }
    let (adjective, noun) = name.split_once('-')?;
    let high = ADJECTIVES.iter().position(|word| *word == adjective)?;
    let low = NOUNS.iter().position(|word| *word == noun)?;

    // Undo the permutation, where indexes past the domain belong to no ID
    let y = (high * 256 + low) as u16;
    let x = y.wrapping_sub(OFFSET).wrapping_mul(INVERSE);
    if x >= 60_000 {
        return None;
    }
    Some(format!(
        "{}{:05}",
        if x < 30_000 { "HG" } else { "NA" },
        x % 30_000
    ))
}

/// Tests of the codec over its whole domain and against the reference names.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::samples::people::PEOPLE;
    use sha2::{Digest, Sha256};
    use std::collections::HashSet;

    /// Checks that every ID in the domain round trips through its own name.
    #[test]
    fn test_domain_is_bijective() {
        let mut names = HashSet::new();
        for prefix in ["HG", "NA"] {
            for digits in 0..30_000 {
                let sample = format!("{prefix}{digits:05}");
                let name = encode(&sample).expect("domain IDs encode");
                assert_eq!(decode(&name).as_deref(), Some(sample.as_str()), "{sample}");
                assert!(names.insert(name), "duplicate name for {sample}");
            }
        }
        assert_eq!(names.len(), 60_000, "all domain names are distinct");

        // Decode every pair, so the unused count does not lean on the encoder
        let mut unreachable = 0;
        for adjective in ADJECTIVES {
            for noun in NOUNS {
                let name = format!("{adjective}-{noun}");
                match decode(&name) {
                    Some(sample) => assert_eq!(encode(&sample), Some(name.clone()), "{name}"),
                    None => unreachable += 1,
                }
            }
        }
        assert_eq!(unreachable, 5_536, "word pairs outside the ID domain");
    }

    /// Checks that bad IDs and names, reversed names and unused pairs all fail.
    #[test]
    fn test_invalid_inputs_are_rejected() {
        for sample in [
            "HG30000", "NA30000", "XX00001", "HG0001", "hg00096", "HG+0096", "HG000096", "HGé096",
            "", "NA99999",
        ] {
            assert_eq!(encode(sample), None, "{sample:?}");
        }
        for name in [
            "unknown-ravine",
            "orange-unknown",
            "orangeravine",
            "orange--ravine",
            "orange-ravine-",
            "-orange-ravine",
            "ravine-orange",
            "",
            "grooved-ravine",
        ] {
            assert_eq!(decode(name), None, "{name:?}");
        }
    }

    /// Checks the names against the design's examples and the reference codec.
    #[test]
    fn test_reference_names() {
        for (sample, name) in [
            ("HG00096", "orange-ravine"),
            ("HG00097", "blue-hill"),
            ("HG04239", "woven-crystal"),
            ("NA12878", "forked-knoll"),
            ("NA19238", "opaque-soil"),
            ("NA21144", "pebbled-patio"),
        ] {
            assert_eq!(encode(sample).as_deref(), Some(name), "{sample}");
            assert_eq!(decode(name).as_deref(), Some(sample), "{name}");
        }

        // Hash every name and ID in sample ID order, the way the reference was
        let mut digest = Sha256::new();
        for person in PEOPLE {
            digest.update(format!(
                "{} {}\n",
                encode(person.sample).unwrap(),
                person.sample
            ));
        }
        assert_eq!(
            hex::encode(digest.finalize()),
            "bb2148264972232e75e45c25b2d425f746b86cfed09faf58f1b1224f4b34552d",
            "reference codec cohort digest"
        );
    }

    /// Checks that the wordlists keep the design's screening rules.
    #[test]
    fn test_wordlist_screening() {
        for words in [&ADJECTIVES, &NOUNS] {
            assert_eq!(words.len(), 256, "codec word count");
            for (index, word) in words.iter().enumerate() {
                assert!((4..=8).contains(&word.len()), "length of {word}");
                assert!(word.bytes().all(|byte| byte.is_ascii_lowercase()), "{word}");
                for other in &words[index + 1..] {
                    assert_ne!(word, other, "repeated word {word}");
                    assert_ne!(&word[..4], &other[..4], "{word}, {other}");
                    assert!(distance(word, other) >= 2, "{word}, {other}");
                }
            }
        }
        for word in ADJECTIVES {
            assert!(!NOUNS.contains(&word), "word in both lists: {word}");
        }
    }

    /// Counts the single-letter edits between two words, as Levenshtein does.
    fn distance(left: &str, right: &str) -> usize {
        let mut previous: Vec<_> = (0..=right.len()).collect();
        for (i, a) in left.bytes().enumerate() {
            let mut next = vec![i + 1];
            for (j, b) in right.bytes().enumerate() {
                next.push(
                    (previous[j] + usize::from(a != b))
                        .min(previous[j + 1] + 1)
                        .min(next[j] + 1),
                );
            }
            previous = next;
        }
        previous[right.len()]
    }
}
