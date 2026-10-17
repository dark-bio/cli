// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Dataset selection, planning and result presentation.

pub(crate) mod cache;
mod paths;
mod reference;
pub(crate) mod samples;

#[cfg(test)]
mod scenarios;

use crate::{
    args::{self, slot_name},
    context::{Context, open_file},
    error::Error,
    output::Output,
    progress::{Processing, Transfer, Update},
};
use darkbio_clock::Clock;
use darkbio_connect::{
    Dataset, UploadProgress,
    schema::{self, SlotState, SlotStatus},
};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::io::Seek;

/// Dispatches dataset commands after applying CLI unlock and dry-run policy.
///
/// Path entries, slot metadata and refusal messages come from the Ark, which
/// decides whether a delete, repair or upload goes ahead. Only an upload's
/// required slots are checked here first, so an unfilled one fails before the
/// owner is asked.
pub(crate) fn run(context: &Context, command: args::Data) -> Result<(), Error> {
    // The sample catalog is built in, so it lists without an Ark
    if let args::Data::Samples {
        population,
        sex,
        all,
    } = command
    {
        return samples::print(&context.output, population.as_deref(), sex, all);
    }

    // An upload opens its file before it connects, so it runs on its own
    if let args::Data::Upload {
        file,
        slot,
        dry_run,
    } = command
    {
        return upload(context, &file, slot, dry_run);
    }

    // Every other command needs an unlocked Ark, which a dry run never unlocks
    let connection = context.connect(None)?;
    let dry_run = match &command {
        args::Data::Fetch { dry_run, .. } => *dry_run,
        args::Data::Delete(args) | args::Data::Repair(args) => args.dry_run,
        _ => false,
    };
    context.require_unlocked(&connection, dry_run)?;

    // The path map needs no slot inventory
    if matches!(command, args::Data::Paths) {
        let paths = connection
            .client
            .call(schema::DatasetPathsRequest {}, context.timing())?;
        return paths::print(&context.output, &paths.paths);
    }

    // The rest work from the Ark's slot inventory
    let slots = connection
        .client
        .call(schema::SlotListRequest {}, context.timing())?
        .slots;
    let repair = matches!(&command, args::Data::Repair(_));
    match command {
        args::Data::List => listing(&context.output, &slots),
        args::Data::Show { slot } => show(&context.output, &slots, slot),
        args::Data::Delete(args) | args::Data::Repair(args) => {
            let slot = select(&slots, args.slot)?;
            let mut value = json!({"slot":slot_name(slot.kind),"id":slot.kind,"state":state(slot),"changed":false});

            // A dry run reports the slot and its dependents without changing it
            if args.dry_run {
                value["required_by"] = json!(required_by(&slots, slot.kind));
                return context.output.document(&value);
            }

            // Empty the slot once the owner approves, where only a filled or
            // damaged slot counts as changed
            approve_change(&context.output, slot.kind, repair);
            if repair {
                connection.client.call(
                    schema::SlotRepairRequest { slot: slot.kind },
                    context.timing(),
                )?;
            } else {
                connection.client.call(
                    schema::SlotDeleteRequest { slot: slot.kind },
                    context.timing(),
                )?;
            }
            value["state"] = json!("empty");
            value["changed"] = json!(filled(slot) || slot.state == SlotState::Damaged as i32);
            context.output.end_stage(None, None);
            context.output.document(&value)
        }
        args::Data::Fetch {
            slot,
            all: _,
            sample,
            dry_run,
            cache,
            no_cache,
        } => reference::fetch(
            context,
            &connection,
            &slots,
            match sample {
                Some(sample) => reference::Target::Sample(sample),
                None => slot.map_or(reference::Target::All, reference::Target::Slot),
            },
            dry_run,
            cache.as_deref(),
            no_cache,
        ),
        args::Data::Upload { .. } | args::Data::Paths | args::Data::Samples { .. } => {
            unreachable!()
        }
    }
}

/// Prints the compact slot inventory and hints for missing or damaged data.
fn listing(output: &crate::output::Output, slots: &[SlotStatus]) -> Result<(), Error> {
    // The table keeps the fields that compare slots, and JSON keeps the details
    let rows: Vec<_> = slots.iter().map(metadata).collect();
    let document = json!({"slots":rows});
    output.table(
        &document,
        &rows,
        &[
            ("SLOT", "slot"),
            ("STATE", "state"),
            ("BUILD", "build"),
            ("VERSION", "version"),
            ("SIZE", "size_bytes"),
        ],
        Some("No slots found"),
    )?;

    // Hint at repairing damage and fetching missing references, and name each
    // unfilled requirement once, with every slot that requires it
    let mut hinted = HashSet::new();
    for slot in slots {
        if slot.state == SlotState::Damaged as i32 {
            output.event(
                "hint",
                format!("run `ark data repair {}`", slot_name(slot.kind)),
            );
        } else if !filled(slot) && download(slot).is_some() {
            output.event(
                "hint",
                format!("run `ark data fetch {}`", slot_name(slot.kind)),
            );
        }
        for dependency in &slot.deps {
            if hinted.insert(*dependency)
                && !slots
                    .iter()
                    .any(|other| other.kind == *dependency && filled(other))
            {
                let dependents = required_by(slots, *dependency);
                let (last, preceding) = dependents.split_last().unwrap();
                let subject = if preceding.is_empty() {
                    format!("{last} requires")
                } else {
                    format!("{} and {last} require", preceding.join(", "))
                };
                output.event(
                    "hint",
                    format!("{subject} {}; fill it first", slot_name(*dependency)),
                );
            }
        }
    }
    Ok(())
}

/// Prints a slot's description, upload format, metadata and local cache state.
fn show(output: &crate::output::Output, slots: &[SlotStatus], id: i32) -> Result<(), Error> {
    let slot = select(slots, id)?;
    let mut value = metadata(slot);
    value["required_by"] = json!(required_by(slots, slot.kind));
    value["cached"] =
        json!(download(slot).is_some_and(|(_, _, hash)| cache::cached(&cache::directory(), hash)));
    let view = dependency_view(value.clone(), slot, slots);
    output.document_with(&value, |theme| crate::output::human::document(theme, &view))
}

/// Opens a local file and connects to an unlocked Ark for [`upload_file`].
fn upload(
    context: &Context,
    path: &std::path::Path,
    required: Option<i32>,
    dry_run: bool,
) -> Result<(), Error> {
    // Open the file and connect to an unlocked Ark, which a dry run never
    // unlocks
    let (mut file, size) = open_file(path)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Error::new(1, "file-unreadable", "filename is not valid UTF-8"))?;
    let connection = context.connect(None)?;
    context.require_unlocked(&connection, dry_run)?;
    upload_file(
        context,
        &connection.client,
        name,
        size,
        &mut file,
        required,
        dry_run,
    )
}

/// Identifies an open file and either plans or uploads it.
///
/// The upload refuses a slot other than `required`, and a target whose
/// required slots are not filled, before it asks for an approval. It rewinds
/// the prefix that identification consumed, and a failure still prints the
/// progress reached.
fn upload_file(
    context: &Context,
    client: &darkbio_connect::Client,
    name: &str,
    size: u64,
    file: &mut (impl std::io::Read + Seek),
    required: Option<i32>,
    dry_run: bool,
) -> Result<(), Error> {
    // Let the Ark identify the file, refusing a rejection or a slot other than
    // the required one
    context
        .output
        .stage("identifying", &format!("Identifying {name}"), None);
    let identified = client.identify_dataset(name, size, file, context.timing())?;
    if !identified.rejection.is_empty() {
        return Err(Error::new(1, "file-rejected", identified.rejection));
    }
    if required.is_some_and(|slot| slot != identified.kind) {
        return Err(Error::new(
            1,
            "invalid-slot",
            format!(
                "the Ark identified {}, expected {}",
                slot_name(identified.kind),
                slot_name(required.unwrap())
            ),
        ));
    }

    // Report the identification, warning when the Ark is unsure of it
    let mut value = json!({
        "slot": slot_name(identified.kind),
        "id": identified.kind,
        "confidence": confidence(identified.conf),
        "uploaded_bytes": 0,
        "phases": [],
        "duration_seconds": 0,
    });
    identification(&context.output, &identified);

    // Read the slots, which both the dry run and the requirement check need
    let slots = client
        .call(schema::SlotListRequest {}, context.timing())?
        .slots;
    let target = select(&slots, identified.kind)?;

    // A dry run plans against the target slot's state and dependencies
    if dry_run {
        value["state"] = json!(state(target));
        value["requires"] = json!(
            target
                .deps
                .iter()
                .map(|id| slot_name(*id))
                .collect::<Vec<_>>()
        );
        let view = dependency_view(value.clone(), target, &slots);
        return context
            .output
            .document_with(&value, |theme| crate::output::human::document(theme, &view));
    }

    // Refuse an unfilled required slot before the upload asks for an approval
    for dependency in &target.deps {
        if !slots
            .iter()
            .any(|slot| slot.kind == *dependency && filled(slot))
        {
            let dependency = slot_name(*dependency);
            return Err(Error::new(
                5,
                "dependency-missing",
                format!(
                    "{} requires {dependency}, which is not filled",
                    slot_name(target.kind)
                ),
            )
            .hint(format!("fill {dependency} first")));
        }
    }

    // Upload from the start, keeping the partial result current on every stage
    file.rewind()?;
    let clock = client.clock();
    let started = clock.now();
    let mut progress = Progress::new(
        context,
        clock.clone(),
        identified.kind,
        size,
        Operation::Upload,
    );
    context.interrupt.partial(value.clone());
    let result = client.upload_dataset(
        &Dataset {
            name: name.into(),
            size,
            slot: Some(identified.kind),
            sha256: None,
        },
        file,
        context.timing(),
        |stage| {
            progress.update(stage);
            value["uploaded_bytes"] = json!(progress.uploaded);
            value["phases"] = json!(progress.phases);
            value["duration_seconds"] = json!(clock.elapsed(started).as_secs());
            context.interrupt.partial(value.clone());
        },
    );

    // Print the result, complete or partial, before any error
    context.interrupt.clear();
    let result = result.map_err(Error::from);
    if let Err(error) = &result {
        context.output.fail_stage(Some(error));
    } else {
        context.output.end_stage(None, None);
    }
    value["uploaded_bytes"] = json!(progress.uploaded);
    value["phases"] = json!(progress.phases);
    value["duration_seconds"] = json!(clock.elapsed(started).as_secs());
    context.output.document(&value)?;
    result
}

/// Ends the identification with the slot and its confidence, keeping the
/// Ark's summary of the file as a `-v` step.
fn identification(output: &Output, identified: &schema::SlotIdentifyResponse) {
    output.end_stage(
        Some(&format!("Identified as {}", slot_name(identified.kind))),
        Some(&format!("{} confidence", confidence(identified.conf))),
    );
    output.event("step", &identified.summary);
    if confidence(identified.conf) == "low" {
        output.event(
            "warning",
            "low identification confidence; the Ark will validate during processing",
        );
    }
}

/// Starts the phone approval of deleting or repairing `slot`.
fn approve_change(output: &Output, slot: i32, repair: bool) {
    output.approve(
        if repair { "repair" } else { "delete" },
        Some("phone"),
        format!(
            "Approve {} {} on your phone, in Ark Companion",
            if repair { "repairing" } else { "deleting" },
            slot_name(slot)
        ),
    );
}

/// Command whose transfer the narration follows.
pub(super) enum Operation {
    /// Upload of a local file, which the Ark identifies first.
    Upload,
    /// Fetch of a public file, read from the cache when it holds a copy.
    Fetch {
        /// Whether the target slot's upload asks the owner, as a personal
        /// slot's does.
        approval: bool,
    },
}

/// Upload presentation and partial-result facts shared by local and reference
/// sources.
pub(crate) struct Progress<'a> {
    /// Output and interruption handles for this invocation.
    context: &'a Context,
    /// Whether the upload waits for the owner's approval on their phone.
    approval: bool,
    /// Rate history beginning at the first acknowledged upload observation.
    transfer: Transfer,
    /// Independent rate history for each device processing step.
    processing: Processing,
    /// Most recent byte count acknowledged by the Ark.
    pub uploaded: u64,
    /// Processing phase names from the latest report, in device order.
    pub phases: Vec<String>,
}

impl<'a> Progress<'a> {
    /// Starts a new dataset's progress without inheriting a previous transfer's
    /// estimates.
    pub fn new(
        context: &'a Context,
        clock: Clock,
        slot: i32,
        size: u64,
        operation: Operation,
    ) -> Self {
        let transfer = match operation {
            Operation::Upload => Update {
                completed: Some(format!("Uploaded {}", crate::style::bytes(size))),
                ..Update::new("uploading", "Uploading")
            },
            Operation::Fetch { .. } => Update {
                completed: Some(format!(
                    "Fetched {} · {}",
                    slot_name(slot),
                    crate::style::bytes(size)
                )),
                ..Update::new("fetching", &format!("Fetching {}", slot_name(slot)))
            },
        };
        Self {
            context,
            approval: match operation {
                Operation::Upload => upload_approval(slot),
                Operation::Fetch { approval } => approval,
            },
            transfer: Transfer::new(context.output.terminal(), clock.clone(), transfer),
            processing: Processing::new(context.output.terminal(), clock),
            uploaded: 0,
            phases: Vec::new(),
        }
    }

    /// Updates cancellation and result facts on every callback, throttling only
    /// presentation.
    pub fn update(&mut self, stage: UploadProgress) {
        match stage {
            UploadProgress::Identifying => {
                self.context
                    .output
                    .stage("identifying", "Identifying dataset", None)
            }
            UploadProgress::Identified(info) => {
                self.approval = upload_approval(info.kind);
                identification(&self.context.output, &info);
            }
            UploadProgress::Preparing => {
                if self.approval {
                    self.context.output.approve(
                        "upload",
                        Some("phone"),
                        "Approve the upload on your phone, in Ark Companion",
                    );
                } else {
                    self.context
                        .output
                        .stage("preparing", "Preparing the upload", None);
                }
            }
            UploadProgress::Started { session } => {
                self.context.output.end_stage(None, None);
                self.context
                    .interrupt
                    .target(crate::interrupt::Target::Upload(session));
                self.context.output.progress(self.transfer.initial());
            }
            UploadProgress::Uploading { uploaded, total } => {
                self.uploaded = uploaded;
                if let Some(line) = self.transfer.update(uploaded, total) {
                    self.context.output.progress(&line);
                }
            }
            UploadProgress::Processing(status) => {
                self.phases = status
                    .phases
                    .iter()
                    .map(|phase| phase.name.clone())
                    .collect();
                for line in self.processing.update(&status) {
                    self.context.output.progress(&line);
                }
            }
        }
    }
}

/// Predicts whether to show a phone approval prompt for an upload to `slot`.
///
/// Known public references need none. This is presentation only, and the Ark
/// remains responsible for authorization.
fn upload_approval(slot: i32) -> bool {
    !matches!(
        schema::SlotKind::try_from(slot),
        Ok(schema::SlotKind::ReferenceGenome
            | schema::SlotKind::GeneAnnotations
            | schema::SlotKind::VariantCatalog)
    )
}

/// Names known confidence levels while retaining future numeric values.
fn confidence(value: i32) -> String {
    schema::SlotConfidence::try_from(value)
        .map(|value| {
            value
                .as_str_name()
                .trim_start_matches("SLOT_CONFIDENCE_")
                .to_ascii_lowercase()
        })
        .unwrap_or_else(|_| value.to_string())
}

/// Requires the selected ID to appear in this Ark's advertised inventory.
pub(crate) fn select(slots: &[SlotStatus], id: i32) -> Result<&SlotStatus, Error> {
    slots.iter().find(|slot| slot.kind == id).ok_or_else(|| {
        Error::new(
            1,
            "invalid-slot",
            format!("the Ark has no slot {}", slot_name(id)),
        )
    })
}

/// Checks whether the Ark explicitly reports usable, filled data in this slot.
pub(crate) fn filled(slot: &SlotStatus) -> bool {
    slot.state == SlotState::Filled as i32
}

/// Names known slot states while retaining future state numbers.
pub(crate) fn state(slot: &SlotStatus) -> String {
    SlotState::try_from(slot.state)
        .map(|state| {
            state
                .as_str_name()
                .trim_start_matches("SLOT_STATE_")
                .to_ascii_lowercase()
        })
        .unwrap_or_else(|_| slot.state.to_string())
}

/// Lists advertised direct dependents, regardless of whether their slots are filled.
fn required_by(slots: &[SlotStatus], id: i32) -> Vec<String> {
    slots
        .iter()
        .filter(|slot| slot.deps.contains(&id))
        .map(|slot| slot_name(slot.kind))
        .collect()
}

/// Borrows the advertised URL, length and digest without validating or
/// fetching them.
pub(crate) fn download(slot: &SlotStatus) -> Option<(&str, u64, &str)> {
    slot.download.as_ref().map(|download| {
        (
            download.url.as_str(),
            download.bytes,
            download.sha256.as_str(),
        )
    })
}

/// Marks each dependency in `requires` as filled or not, for the human view.
///
/// The JSON result is built apart from this view, so it keeps the advertised
/// dependency names.
fn dependency_view(mut value: Value, slot: &SlotStatus, slots: &[SlotStatus]) -> Value {
    value["requires"] = json!(
        slot.deps
            .iter()
            .map(|id| {
                let state = if slots.iter().any(|other| other.kind == *id && filled(other)) {
                    "filled"
                } else {
                    "not filled"
                };
                format!("{} ({state})", slot_name(*id))
            })
            .collect::<Vec<_>>()
    );
    value
}

/// Builds generic slot output, preserving future IDs and optional advertised
/// details.
pub(crate) fn metadata(slot: &SlotStatus) -> Value {
    let origin = schema::SlotOrigin::try_from(slot.origin)
        .map(|origin| {
            origin
                .as_str_name()
                .trim_start_matches("SLOT_ORIGIN_")
                .to_ascii_lowercase()
        })
        .unwrap_or_else(|_| slot.origin.to_string());
    json!({
        "slot": slot_name(slot.kind), "id": slot.kind,
        "name": slot.name,
        "description": slot.desc, "format": slot.format,
        "state": state(slot), "origin": origin,
        "damage": (!slot.damage.is_empty()).then_some(&slot.damage),
        "requires": slot.deps.iter().map(|id| slot_name(*id)).collect::<Vec<_>>(),
        "size_bytes": slot.bytes,
        "build": (!slot.build.is_empty()).then_some(&slot.build),
        "version": (!slot.version.is_empty()).then_some(&slot.version),
        "download": download(slot).map(|(url, size, hash)| json!({"url": url, "size_bytes": size, "sha256": hash})),
    })
}

/// Tests of the approval prediction and the generic slot metadata.
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// An empty slot reply reads as its sentence, and JSON keeps the empty list.
    #[test]
    fn test_empty_slot_listing() {
        for captured in
            crate::testing::narration("data::tests::test_empty_slot_listing", |output, _| {
                let result = listing(&output, &[]);
                assert!(result.is_ok(), "empty slot listing succeeds: {result:?}");
            })
        {
            assert_eq!(
                captured.stdout,
                if captured.json {
                    "{\n  \"slots\": []\n}\n"
                } else {
                    "No slots found\n"
                },
                "json={}, terminal={}",
                captured.json,
                captured.terminal,
            );
            assert_eq!(captured.stderr, "", "empty slot listing has no hints");
        }
    }

    /// Uploads and fetches narrate in their own words, and each Ark phase ends
    /// with its time.
    #[test]
    fn test_dataset_narration() {
        use clap::Parser;

        for captured in
            crate::testing::narration("data::tests::test_dataset_narration", |output, clock| {
                let options = args::Cli::parse_from(["ark"]).options;
                let context = Context {
                    options,
                    interrupt: crate::interrupt::Interrupt::install(output.clone()).unwrap(),
                    output,
                };
                let output = &context.output;
                output.stage("identifying", "Identifying calls.vcf.gz", None);
                identification(
                    output,
                    &schema::SlotIdentifyResponse {
                        kind: schema::SlotKind::SmallVariants as i32,
                        conf: schema::SlotConfidence::High as i32,
                        summary: "Sample calls".into(),
                        ..Default::default()
                    },
                );
                for (operation, slot, size, seconds, phase, duration) in [
                    (
                        Operation::Upload,
                        schema::SlotKind::SmallVariants as i32,
                        307547341,
                        22,
                        "Compressing",
                        62,
                    ),
                    (
                        Operation::Fetch { approval: false },
                        schema::SlotKind::ReferenceGenome as i32,
                        3221225472,
                        250,
                        "Indexing",
                        151,
                    ),
                    (
                        Operation::Upload,
                        schema::SlotKind::ReferenceGenome as i32,
                        1024,
                        1,
                        "Indexing",
                        1,
                    ),
                ] {
                    let mut progress =
                        Progress::new(&context, clock.clock(), slot, size, operation);
                    progress.update(UploadProgress::Preparing);
                    output.finish();
                    clock.advance(Duration::from_secs(14));
                    progress.update(UploadProgress::Started { session: 7 });
                    progress.update(UploadProgress::Uploading {
                        uploaded: 0,
                        total: size,
                    });
                    output.finish();
                    clock.advance(Duration::from_secs(seconds));
                    progress.update(UploadProgress::Uploading {
                        uploaded: size,
                        total: size,
                    });
                    let status = schema::SlotUploadProcessResponse {
                        proc_start: 1,
                        phase_start: 1,
                        phase_in: 1,
                        phases: vec![schema::SlotPhase {
                            name: phase.into(),
                            desc: String::new(),
                        }],
                        ..Default::default()
                    };
                    progress.update(UploadProgress::Processing(status.clone()));
                    output.finish();
                    clock.advance(Duration::from_secs(duration));
                    progress.update(UploadProgress::Processing(
                        schema::SlotUploadProcessResponse {
                            phase_progress: 10000,
                            ..status
                        },
                    ));
                }
                context.interrupt.clear();
                for repair in [false, true] {
                    approve_change(output, schema::SlotKind::SmallVariants as i32, repair);
                    output.finish();
                    clock.advance(Duration::from_secs(6));
                    output.end_stage(None, None);
                }
            })
        {
            captured.contains(&[
                "Identifying calls.vcf.gz",
                "Identified as small-variants · high confidence",
                "Approve the upload on your phone, in Ark Companion",
                "Approved on your phone · 14s",
                "Uploading",
                "Uploaded 293.3 MiB · 22s",
                "Compressing · 1m 02s",
                "Preparing the upload",
                "Fetching reference-genome",
                "Fetched reference-genome · 3.0 GiB · 4m 10s",
                "Indexing · 2m 31s",
                "Preparing the upload",
                "Uploaded 1.0 KiB · 1s",
                "Approve deleting small-variants on your phone, in Ark Companion",
                "Approved on your phone · 6s",
                "Approve repairing small-variants on your phone, in Ark Companion",
                "Approved on your phone · 6s",
            ]);
            captured.approvals(&[
                ("upload", Some("phone")),
                ("delete", Some("phone")),
                ("repair", Some("phone")),
            ]);
            assert!(!captured.stderr.contains("Sample calls"));
            assert!(!captured.stderr.contains("note: Fetching"));
            assert!(!captured.stderr.contains("✓ Preparing"));
            assert!(!captured.stderr.contains("Preparing the upload ·"));
        }
    }

    /// Checks that uploads to the public reference slots predict no approval
    /// prompt, while personal data does.
    #[test]
    fn reference_uploads_do_not_request_approval() {
        use schema::SlotKind::*;
        for kind in [ReferenceGenome, GeneAnnotations, VariantCatalog] {
            assert!(!upload_approval(kind as i32));
        }
        for kind in [SmallVariants, StructuralVariants, CopyNumberVariants] {
            assert!(upload_approval(kind as i32), "{kind:?}");
        }
        assert!(upload_approval(42));
    }

    /// Checks that a slot kind this CLI does not know keeps its texts and
    /// dependency state.
    #[test]
    fn future_slots_keep_generic_metadata() {
        // An unknown kind keeps its id as its name and every advertised text
        let slot = SlotStatus {
            kind: 42,
            name: "Future dataset".into(),
            desc: "A public dataset for apps.".into(),
            format: "An unchanged file from the reference catalog.".into(),
            state: SlotState::Filled as i32,
            bytes: 123,
            build: "GRCh39".into(),
            version: "158".into(),
            deps: vec![1],
            ..Default::default()
        };
        let value = metadata(&slot);
        assert_eq!(value["slot"], "42");
        assert_eq!(value["size_bytes"], 123);
        assert_eq!(value["version"], "158");
        assert_eq!(value["requires"], json!(["reference-genome"]));
        assert!(value["damage"].is_null());
        assert!(value["download"].is_null());
        assert_eq!(value["description"], slot.desc);
        assert_eq!(value["format"], slot.format);

        // The human view marks whether the known dependency is filled
        let dependency = SlotStatus {
            kind: 1,
            state: SlotState::Filled as i32,
            ..Default::default()
        };
        assert_eq!(
            dependency_view(value.clone(), &slot, &[dependency])["requires"],
            json!(["reference-genome (filled)"])
        );
        assert_eq!(
            dependency_view(value, &slot, &[])["requires"],
            json!(["reference-genome (not filled)"])
        );

        // A missing slot is an invalid selection, and a damaged one is not filled
        let error = select(&[], 5).unwrap_err();
        assert_eq!((error.class, error.code), (1, "invalid-slot"));
        assert!(!filled(&SlotStatus {
            state: SlotState::Damaged as i32,
            ..slot
        }));
    }
}
