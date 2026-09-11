// ark: command line interface for Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.

//! Local datasets, reference selection and upload progress for the terminal.

use crate::progress::{Processing, Transfer};
use crate::{Error, connect, find_enclave};
use console::style;
use darkbio_connect::schema::{SlotKind, SlotListRequest, SlotStatus};
use darkbio_connect::trust::Environment;
use darkbio_connect::{TrustMode, UploadProgress};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(clap::Args)]
#[command(group(clap::ArgGroup::new("source")
    .required(true)
    .args(["file", "reference_genome", "gene_annotations", "variant_catalog"])))]
pub(super) struct Args {
    /// Dataset file; the Ark identifies its format and target slot
    #[arg(value_name = "FILE")]
    file: Option<PathBuf>,

    /// Download and upload the Ark's advertised reference genome
    #[arg(long)]
    reference_genome: bool,

    /// Download and upload the Ark's advertised gene annotations
    #[arg(long)]
    gene_annotations: bool,

    /// Download and upload the Ark's advertised variant catalog
    #[arg(long)]
    variant_catalog: bool,

    /// Endpoint locator, or a unique serial, name or disk image
    #[arg(long)]
    device: Option<String>,

    /// Total budget in seconds for setup, approval, transfer and processing
    #[arg(long, default_value_t = 3600, value_parser = clap::value_parser!(u64).range(1..))]
    timeout: u64,
}

impl Args {
    /// Clap admits exactly one source, so at most one reference flag is set.
    fn reference(&self) -> Option<SlotKind> {
        if self.reference_genome {
            Some(SlotKind::SlotReferenceGenome)
        } else if self.gene_annotations {
            Some(SlotKind::SlotGeneAnnotations)
        } else if self.variant_catalog {
            Some(SlotKind::SlotVariantCatalog)
        } else {
            None
        }
    }
}

pub(super) fn run(args: Args, env: Option<Environment>) -> Result<(), Error> {
    // Open and inspect local input before selecting or contacting an Ark.
    let local = args.file.as_deref().map(open).transpose()?;
    let endpoint = find_enclave(args.device.as_deref())?;
    let (ark, _) = connect(&endpoint, &TrustMode::RootOrSelf, env)?;
    let client = ark.client();
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(args.timeout))
        .ok_or_else(|| "upload timeout is too large".to_owned())?;
    let mut progress = Progress::default();
    if let Some((mut file, name, size)) = local {
        println!(
            "Uploading {} ({:.1} MiB)",
            style(&name).bold(),
            size as f64 / (1024.0 * 1024.0)
        );
        client.upload_dataset(&name, size, &mut file, deadline, |stage| {
            progress.show(stage)
        })?;
    } else if let Some(reference) = args.reference() {
        let slots = client.call(SlotListRequest {}, deadline)?.slots;
        let slot = select(&slots, reference)?;
        println!("Uploading reference for {}", style(&slot.name).bold());
        client.upload_reference(slot, deadline, |stage| progress.show(stage))?;
    }
    println!(
        "{}",
        style("Dataset uploaded and processed successfully.").green()
    );
    Ok(())
}

fn open(path: &Path) -> Result<(File, String, u64), Error> {
    let file =
        File::open(path).map_err(|error| format!("failed to open {}: {error}", path.display()))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("failed to inspect {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("{} is not a regular file", path.display()).into());
    }
    if metadata.len() == 0 {
        return Err(format!("{} is empty", path.display()).into());
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "dataset filename must be valid UTF-8".to_owned())?;
    Ok((file, name.into(), metadata.len()))
}

/// Select by the protocol kind. Device labels and slot ordering may change.
fn select(slots: &[SlotStatus], kind: SlotKind) -> Result<&SlotStatus, Error> {
    let mut matches = slots.iter().filter(|slot| slot.kind == kind as i32);
    let name = || {
        kind.as_str_name()
            .trim_start_matches("SLOT_")
            .replace('_', "-")
            .to_ascii_lowercase()
    };
    let slot = matches
        .next()
        .ok_or_else(|| format!("Ark reports no {} slot", name()))?;
    if matches.next().is_some() {
        return Err(format!("Ark reports duplicate {} slots", name()).into());
    }
    Ok(slot)
}

/// Retains separate estimates for transferred bytes and device processing.
#[derive(Default)]
struct Progress {
    upload: Transfer,
    processing: Processing,
}

impl Progress {
    fn show(&mut self, stage: UploadProgress) {
        match stage {
            UploadProgress::Downloading => {
                eprintln!("{}", style("Downloading reference dataset…").dim())
            }
            UploadProgress::Identifying => eprintln!("{}", style("Identifying dataset…").dim()),
            UploadProgress::Identified(info) => {
                if !info.summary.is_empty() {
                    eprintln!("{}", style(info.summary).bold());
                }
                if !info.details.is_empty() {
                    eprintln!("{}", style(info.details).dim());
                }
            }
            UploadProgress::Preparing => eprintln!(
                "{}",
                style("Preparing upload. Approve in your companion app if requested.").dim()
            ),
            UploadProgress::Uploading { uploaded, total } => {
                if let Some(line) = self.upload.update(uploaded, total) {
                    eprintln!("{}", style(line).dim());
                }
            }
            UploadProgress::Processing(status) => {
                if let Some(line) = self.processing.update(&status) {
                    eprintln!("{}", style(line).dim());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn test_arguments() {
        let sources = [
            ("calls.vcf.gz", None),
            ("--reference-genome", Some(SlotKind::SlotReferenceGenome)),
            ("--gene-annotations", Some(SlotKind::SlotGeneAnnotations)),
            ("--variant-catalog", Some(SlotKind::SlotVariantCatalog)),
        ];
        for (index, &(source, expected)) in sources.iter().enumerate() {
            let crate::Command::Upload(args) =
                crate::Cli::try_parse_from(["ark", "upload", source])
                    .unwrap()
                    .command
            else {
                panic!("expected upload command");
            };
            assert_eq!(args.reference(), expected);
            assert_eq!(args.file.is_some(), expected.is_none());
            for &(other, _) in &sources[index + 1..] {
                assert!(crate::Cli::try_parse_from(["ark", "upload", source, other]).is_err());
            }
        }
        for args in [
            vec!["ark", "upload"],
            vec!["ark", "upload", "--reference", "reference-genome"],
            vec!["ark", "upload", "calls.vcf.gz", "--timeout", "0"],
        ] {
            assert!(crate::Cli::try_parse_from(args).is_err());
        }
    }

    /// Selection follows the protocol kind even with renamed or reordered slots.
    /// Whether a populated slot can be uploaded to remains the Ark's decision.
    #[test]
    fn test_selection() {
        let slots = [
            SlotStatus {
                kind: 0,
                name: "Renamed reference".into(),
                filled: true,
                ..Default::default()
            },
            SlotStatus {
                kind: 1,
                name: "Gene Annotations".into(),
                ..Default::default()
            },
            SlotStatus {
                kind: 3,
                name: "Renamed catalog".into(),
                ..Default::default()
            },
        ];
        assert_eq!(
            select(&slots, SlotKind::SlotReferenceGenome).unwrap().kind,
            0
        );
        assert_eq!(
            select(&slots, SlotKind::SlotGeneAnnotations).unwrap().kind,
            1
        );
        assert_eq!(
            select(&slots, SlotKind::SlotVariantCatalog).unwrap().kind,
            3
        );
        assert!(select(&slots[1..], SlotKind::SlotReferenceGenome).is_err());
        assert!(
            select(
                &[slots[0].clone(), slots[0].clone()],
                SlotKind::SlotReferenceGenome
            )
            .is_err()
        );
    }
}
