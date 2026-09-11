// ark: command line interface for Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.

//! Dataset slot inventory and metadata reported by the Ark.

use crate::{Error, connect, find_enclave};
use console::style;
use darkbio_connect::TrustMode;
use darkbio_connect::schema::{SlotListRequest, SlotOrigin, SlotStatus, slot_status::Meta};
use darkbio_connect::trust::Environment;
use std::time::Duration;

pub(super) fn run(
    selector: Option<&str>,
    env: Option<Environment>,
    timeout: u64,
) -> Result<(), Error> {
    let endpoint = find_enclave(selector)?;
    let (ark, _) = connect(&endpoint, &TrustMode::RootOrSelf, env)?;
    let response = ark
        .client()
        .call_timeout(SlotListRequest {}, Duration::from_secs(timeout))?;
    println!("{}", render(&response.slots));
    Ok(())
}

/// Names and dependencies come from the device. Unknown slot kinds remain
/// visible, and absent metadata does not imply an empty or damaged slot.
fn render(slots: &[SlotStatus]) -> String {
    if slots.is_empty() {
        return "The Ark reports no dataset slots.".into();
    }
    slots
        .iter()
        .map(|slot| {
            let state = if !slot.damaged.is_empty() {
                style("damaged").red()
            } else if slot.filled {
                style("filled").green()
            } else {
                style("empty").yellow()
            };
            let origin = match SlotOrigin::try_from(slot.origin) {
                Ok(SlotOrigin::OriginPersonal) => "personal".into(),
                Ok(SlotOrigin::OriginReference) => "reference".into(),
                Err(_) => format!("unknown origin {}", slot.origin),
            };
            let mut lines = vec![format!(
                "{}  {state} {}",
                style(name(slot)).bold(),
                style(format!("({origin})")).dim()
            )];
            if !slot.desc.is_empty() {
                lines.push(format!("  {}", style(&slot.desc).dim()));
            }
            field(&mut lines, "Slot ID", &slot.kind.to_string());
            if !slot.damaged.is_empty() {
                lines.push(format!(
                    "  {} {}",
                    style("Problem:").dim(),
                    style(&slot.damaged).red()
                ));
            }
            if !slot.deps.is_empty() {
                let deps = slot
                    .deps
                    .iter()
                    .map(|kind| {
                        slots
                            .iter()
                            .find(|slot| slot.kind == *kind)
                            .map(name)
                            .unwrap_or_else(|| format!("Slot {kind}"))
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                field(&mut lines, "Requires", &deps);
            }
            match &slot.meta {
                Some(Meta::ReferenceGenome(meta)) => {
                    field(&mut lines, "Build", &meta.build);
                    download_fields(
                        &mut lines,
                        &meta.download_url,
                        meta.download_bytes,
                        &meta.download_sha256,
                    );
                }
                Some(Meta::GeneAnnotations(meta)) => {
                    field(&mut lines, "Build", &meta.build);
                    download_fields(
                        &mut lines,
                        &meta.download_url,
                        meta.download_bytes,
                        &meta.download_sha256,
                    );
                }
                Some(Meta::SnpIndelCalls(meta)) => field(&mut lines, "Build", &meta.build),
                Some(Meta::VariantCatalog(meta)) => {
                    field(&mut lines, "Build", &meta.build);
                    if meta.dbsnp_build != 0 {
                        field(&mut lines, "dbSNP", &meta.dbsnp_build.to_string());
                    }
                    download_fields(
                        &mut lines,
                        &meta.download_url,
                        meta.download_bytes,
                        &meta.download_sha256,
                    );
                }
                None => {}
            }
            lines.join("\n")
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn name(slot: &SlotStatus) -> String {
    if slot.name.is_empty() {
        format!("Slot {}", slot.kind)
    } else {
        slot.name.clone()
    }
}

fn field(lines: &mut Vec<String>, label: &str, value: &str) {
    if !value.is_empty() {
        lines.push(format!("  {} {value}", style(format!("{label}:")).dim()));
    }
}

/// A filled reference slot may report its build without a download offer.
fn download_fields(lines: &mut Vec<String>, url: &str, bytes: u64, sha256: &str) {
    field(lines, "Download", url);
    if bytes != 0 {
        field(lines, "Size", &format!("{bytes} bytes"));
    }
    field(lines, "SHA-256", sha256);
}

#[cfg(test)]
mod tests {
    use super::*;
    use darkbio_connect::schema::{
        SlotMetaGeneAnnotations, SlotMetaReferenceGenome, SlotMetaSnpIndelCalls,
        SlotMetaVariantCatalog,
    };

    /// Rendering preserves each metadata variant and uses the device's names
    /// for dependencies. A populated slot need not advertise a download.
    #[test]
    fn test_metadata() {
        let slots = [
            SlotStatus {
                kind: 2,
                name: "My variants".into(),
                filled: true,
                meta: Some(Meta::SnpIndelCalls(SlotMetaSnpIndelCalls {
                    build: "GRCh38".into(),
                })),
                ..Default::default()
            },
            SlotStatus {
                kind: 0,
                name: "Reference".into(),
                origin: SlotOrigin::OriginReference.into(),
                deps: vec![2],
                meta: Some(Meta::ReferenceGenome(SlotMetaReferenceGenome {
                    build: "GRCh38.p14".into(),
                    download_url: "https://example.invalid/reference.fa.gz".into(),
                    download_bytes: u64::MAX,
                    download_sha256: "ab".repeat(32),
                })),
                ..Default::default()
            },
            SlotStatus {
                kind: 1,
                name: "Genes".into(),
                filled: true,
                meta: Some(Meta::GeneAnnotations(SlotMetaGeneAnnotations {
                    build: "GRCh37".into(),
                    ..Default::default()
                })),
                ..Default::default()
            },
            SlotStatus {
                kind: 3,
                name: "Variants".into(),
                meta: Some(Meta::VariantCatalog(SlotMetaVariantCatalog {
                    build: "GRCh38".into(),
                    dbsnp_build: 157,
                    download_url: "https://example.invalid/catalog.vcf.gz".into(),
                    download_bytes: 123,
                    download_sha256: "cd".repeat(32),
                })),
                ..Default::default()
            },
        ];
        let rendered = render(&slots);
        let text = console::strip_ansi_codes(&rendered);
        for expected in [
            "My variants  filled (personal)",
            "Reference  empty (reference)",
            "Requires: My variants",
            "Build: GRCh38.p14",
            "Build: GRCh37",
            "dbSNP: 157",
            "Download: https://example.invalid/reference.fa.gz",
            "Download: https://example.invalid/catalog.vcf.gz",
            "18446744073709551615 bytes",
            "123 bytes",
            &"ab".repeat(32),
            &"cd".repeat(32),
        ] {
            assert!(text.contains(expected), "missing {expected}");
        }
        assert!(!render(&slots[2..3]).contains("Download:"));
    }

    /// Damage takes precedence over filled, and future enum values do not get
    /// mistaken for the first known variant or hide the affected slot.
    #[test]
    fn test_states() {
        let mut slot = SlotStatus {
            kind: 99,
            origin: 123,
            filled: true,
            damaged: "Metadata is corrupt".into(),
            desc: "Future dataset".into(),
            deps: vec![87],
            ..Default::default()
        };
        let rendered = render(&[slot.clone()]);
        let text = console::strip_ansi_codes(&rendered);
        assert!(text.contains("Slot 99  damaged (unknown origin 123)"));
        assert!(text.contains("Future dataset"));
        assert!(text.contains("Problem: Metadata is corrupt"));
        assert!(text.contains("Requires: Slot 87"));
        assert!(!text.contains("filled"));
        slot.damaged.clear();
        assert!(render(&[slot]).contains("filled"));
        assert_eq!(render(&[]), "The Ark reports no dataset slots.");
    }
}
