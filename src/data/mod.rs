// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Dataset selection, planning and result presentation.

pub(crate) mod cache;
mod reference;

use crate::{
    args::{self, slot_name},
    context::{Context, open_file},
    error::Error,
    progress::{Processing, Transfer, Update},
};
use darkbio_connect::{
    Dataset, UploadProgress,
    schema::{self, SlotState, SlotStatus},
};
use serde_json::{Value, json};
use std::io::Seek;
use std::time::Instant;

/// Dispatches dataset commands after applying CLI unlock and dry-run policy.
/// Slot metadata and refusal messages come from the Ark; the CLI does not predict
/// whether a requested delete, repair or upload will be accepted.
pub(crate) fn run(context: &Context, command: args::Data) -> Result<(), Error> {
    if let args::Data::Upload {
        file,
        slot,
        dry_run,
    } = command
    {
        return upload(context, &file, slot, dry_run);
    }
    let connection = context.connect(None)?;
    let dry_run = match &command {
        args::Data::Fetch { dry_run, .. } => *dry_run,
        args::Data::Delete(args) | args::Data::Repair(args) => args.dry_run,
        _ => false,
    };
    context.require_unlocked(&connection, dry_run)?;
    if matches!(command, args::Data::Paths) {
        let paths = connection
            .client
            .call(schema::DatasetPathsRequest {}, context.timing())?;
        return if context.output.json() {
            context.output.document(&json!({"readme": paths.readme}))
        } else {
            context.output.app(paths.readme.as_bytes(), &[])
        };
    }
    let slots = connection
        .client
        .call(schema::SlotListRequest {}, context.timing())?
        .slots;
    let repair = matches!(&command, args::Data::Repair(_));
    match command {
        args::Data::List => {
            let rows: Vec<_> = slots.iter().map(metadata).collect();
            let document = json!({"slots":rows});
            let rows: Vec<_> = rows
                .into_iter()
                .zip(&slots)
                .map(|(value, slot)| dependency_view(value, slot, &slots))
                .collect();
            context.output.table(
                &document,
                &rows,
                &[
                    ("SLOT", "slot"),
                    ("STATE", "state"),
                    ("ORIGIN", "origin"),
                    ("BUILD", "build"),
                    ("VERSION", "version"),
                    ("SIZE", "size_bytes"),
                    ("REQUIRES", "requires"),
                ],
            )?;
            for slot in &slots {
                if slot.state == SlotState::StateDamaged as i32 {
                    context.output.event(
                        "hint",
                        format!("run `ark data repair {}`", slot_name(slot.kind)),
                    );
                } else if !filled(slot) && download(slot).is_some() {
                    context.output.event(
                        "hint",
                        format!("run `ark data fetch {}`", slot_name(slot.kind)),
                    );
                }
                for dependency in &slot.deps {
                    if !slots
                        .iter()
                        .any(|other| other.kind == *dependency && filled(other))
                    {
                        context.output.event(
                            "hint",
                            format!(
                                "{} requires {}; fill it first",
                                slot_name(slot.kind),
                                slot_name(*dependency)
                            ),
                        );
                    }
                }
            }
            Ok(())
        }
        args::Data::Show { slot } => {
            let slot = select(&slots, slot)?;
            let mut value = metadata(slot);
            value["description"] = json!(slot.desc);
            value["required_by"] = json!(required_by(&slots, slot.kind));
            value["cached"] = json!(
                download(slot).is_some_and(|(_, _, hash)| cache::cached(&cache::directory(), hash))
            );
            let view = dependency_view(value.clone(), slot, &slots);
            context
                .output
                .document_with(&value, |theme| crate::output::human::document(theme, &view))
        }
        args::Data::Delete(args) | args::Data::Repair(args) => {
            let slot = select(&slots, args.slot)?;
            let mut value = json!({"slot":slot_name(slot.kind),"id":slot.kind,"state":state(slot),"changed":false});
            if args.dry_run {
                value["required_by"] = json!(required_by(&slots, slot.kind));
                return context.output.document(&value);
            }
            context.output.event(
                "approve",
                format!(
                    "{} {} (Ark Companion on your phone)",
                    if repair { "repair" } else { "delete" },
                    slot_name(slot.kind)
                ),
            );
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
            value["changed"] = json!(filled(slot) || slot.state == SlotState::StateDamaged as i32);
            context.output.document(&value)
        }
        args::Data::Fetch {
            slot,
            all: _,
            dry_run,
            cache,
            no_cache,
        } => reference::fetch(
            context,
            &connection,
            &slots,
            slot,
            dry_run,
            cache.as_deref(),
            no_cache,
        ),
        args::Data::Upload { .. } | args::Data::Paths => unreachable!(),
    }
}

/// Identifies a local file, checks an optional target constraint and either plans
/// or uploads it. Rewinds the consumed prefix and preserves progress on failure.
fn upload(
    context: &Context,
    path: &std::path::Path,
    required: Option<i32>,
    dry_run: bool,
) -> Result<(), Error> {
    let (mut file, size) = open_file(path)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Error::new(1, "file-unreadable", "filename is not valid UTF-8"))?;
    let connection = context.connect(None)?;
    context.require_unlocked(&connection, dry_run)?;
    context
        .output
        .event("progress", format!("identifying {}", path.display()));
    let identified = connection
        .client
        .identify_dataset(name, size, &mut file, context.timing())?;
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
    let mut value = json!({
        "slot": slot_name(identified.kind),
        "id": identified.kind,
        "confidence": confidence(identified.conf),
        "uploaded_bytes": 0,
        "phases": [],
        "duration_seconds": 0,
    });
    context.output.event(
        "note",
        format!(
            "identified as {} ({}): {}",
            slot_name(identified.kind),
            confidence(identified.conf),
            identified.summary
        ),
    );
    if confidence(identified.conf) == "low" {
        context.output.event(
            "warning",
            "low identification confidence; the Ark will validate during processing",
        );
    }
    if dry_run {
        let slots = connection
            .client
            .call(schema::SlotListRequest {}, context.timing())?
            .slots;
        let target = select(&slots, identified.kind)?;
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
    file.rewind()?;
    let started = Instant::now();
    let mut progress = Progress::new(context, identified.kind);
    context.interrupt.partial(value.clone());
    let result = connection.client.upload_dataset(
        &Dataset {
            name: name.into(),
            size,
            slot: Some(identified.kind),
            sha256: None,
        },
        &mut file,
        context.timing(),
        |stage| {
            progress.update(stage);
            value["uploaded_bytes"] = json!(progress.uploaded);
            value["phases"] = json!(progress.phases);
            value["duration_seconds"] = json!(started.elapsed().as_secs());
            context.interrupt.partial(value.clone());
        },
    );
    context.interrupt.clear();
    value["uploaded_bytes"] = json!(progress.uploaded);
    value["phases"] = json!(progress.phases);
    value["duration_seconds"] = json!(started.elapsed().as_secs());
    context.output.document(&value)?;
    result.map_err(Into::into)
}

/// Upload presentation and partial-result facts shared by local and reference sources.
pub(crate) struct Progress<'a> {
    /// Output and interruption handles for this invocation.
    context: &'a Context,
    /// Identified or advertised target used for approval guidance.
    slot: i32,
    /// Rate history beginning at the first acknowledged upload observation.
    transfer: Transfer,
    /// Last upload row, retained to align it when processing phases arrive.
    last_upload: Option<Update>,
    /// Independent rate history for each device processing step.
    processing: Processing,
    /// Most recent byte count acknowledged by the Ark.
    pub uploaded: u64,
    /// Processing phase names from the latest report, in device order.
    pub phases: Vec<String>,
}
impl<'a> Progress<'a> {
    /// Starts a new dataset's progress without inheriting a previous transfer's estimates.
    pub fn new(context: &'a Context, slot: i32) -> Self {
        Self {
            context,
            slot,
            transfer: Transfer::new(context.output.terminal()),
            last_upload: None,
            processing: Processing::new(context.output.terminal()),
            uploaded: 0,
            phases: Vec::new(),
        }
    }
    /// Updates cancellation and result facts on every callback, throttling only presentation.
    pub fn update(&mut self, stage: UploadProgress) {
        match stage {
            UploadProgress::Identifying => {
                self.context.output.event("progress", "identifying dataset")
            }
            UploadProgress::Identified(info) => {
                self.slot = info.kind;
                self.context
                    .output
                    .event("note", format!("identified {}", slot_name(info.kind)));
            }
            UploadProgress::Preparing => {
                if upload_approval(self.slot) {
                    self.context.output.event(
                        "approve",
                        format!(
                            "upload {} (Ark Companion on your phone)",
                            slot_name(self.slot)
                        ),
                    );
                } else {
                    self.context
                        .output
                        .event("progress", "preparing dataset upload");
                }
            }
            UploadProgress::Started { session } => self
                .context
                .interrupt
                .target(crate::interrupt::Target::Upload(session)),
            UploadProgress::Uploading { uploaded, total } => {
                self.uploaded = uploaded;
                if let Some(line) = self.transfer.update(uploaded, total) {
                    self.context.output.progress(&line);
                    if self.context.output.terminal() {
                        self.last_upload = Some(line);
                    }
                }
            }
            UploadProgress::Processing(status) => {
                let first = self.phases.is_empty();
                self.phases = status
                    .phases
                    .iter()
                    .map(|phase| phase.name.clone())
                    .collect();
                for mut line in self.processing.update(&status) {
                    if let Some(upload) = &mut self.last_upload {
                        line.align(upload);
                        if first {
                            self.context.output.progress(upload);
                        }
                    }
                    self.context.output.progress(&line);
                }
            }
        }
    }
}

/// Predicts whether to show a phone approval prompt; known public references need none.
/// This is presentation only. The Ark remains responsible for authorization.
fn upload_approval(slot: i32) -> bool {
    !matches!(
        schema::SlotKind::try_from(slot),
        Ok(schema::SlotKind::SlotReferenceGenome
            | schema::SlotKind::SlotGeneAnnotations
            | schema::SlotKind::SlotVariantCatalog)
    )
}

/// Names known confidence levels while retaining future numeric values.
fn confidence(value: i32) -> String {
    schema::SlotConfidence::try_from(value)
        .map(|value| {
            value
                .as_str_name()
                .trim_start_matches("CONFIDENCE_")
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
/// Whether the Ark explicitly reports usable, filled data in this slot.
pub(crate) fn filled(slot: &SlotStatus) -> bool {
    slot.state == SlotState::StateFilled as i32
}
/// Names known slot states while retaining future state numbers.
pub(crate) fn state(slot: &SlotStatus) -> String {
    SlotState::try_from(slot.state)
        .map(|state| {
            state
                .as_str_name()
                .trim_start_matches("STATE_")
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
/// Borrows the advertised URL, length and digest without validating or fetching them.
pub(crate) fn download(slot: &SlotStatus) -> Option<(&str, u64, &str)> {
    slot.download.as_ref().map(|download| {
        (
            download.url.as_str(),
            download.bytes,
            download.sha256.as_str(),
        )
    })
}
/// Shows dependency state without changing the advertised JSON dependency names.
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

/// Builds generic slot output, preserving future IDs and optional advertised details.
pub(crate) fn metadata(slot: &SlotStatus) -> Value {
    let origin = schema::SlotOrigin::try_from(slot.origin)
        .map(|origin| {
            origin
                .as_str_name()
                .trim_start_matches("ORIGIN_")
                .to_ascii_lowercase()
        })
        .unwrap_or_else(|_| slot.origin.to_string());
    json!({
        "slot": slot_name(slot.kind), "id": slot.kind,
        "name": slot.name,
        "state": state(slot), "origin": origin,
        "damage": (!slot.damage.is_empty()).then_some(&slot.damage),
        "requires": slot.deps.iter().map(|id| slot_name(*id)).collect::<Vec<_>>(),
        "size_bytes": slot.bytes,
        "build": (!slot.build.is_empty()).then_some(&slot.build),
        "version": (!slot.version.is_empty()).then_some(&slot.version),
        "download": download(slot).map(|(url, size, hash)| json!({"url": url, "size_bytes": size, "sha256": hash})),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_uploads_do_not_request_approval() {
        use schema::SlotKind::*;
        for kind in [SlotReferenceGenome, SlotGeneAnnotations, SlotVariantCatalog] {
            assert!(!upload_approval(kind as i32));
        }
        assert!(upload_approval(SlotSnpIndelCalls as i32));
    }

    #[test]
    fn future_slots_keep_generic_metadata() {
        let slot = SlotStatus {
            kind: 42,
            name: "Future dataset".into(),
            state: SlotState::StateFilled as i32,
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
        assert!(value.get("description").is_none());
        let dependency = SlotStatus {
            kind: 1,
            state: SlotState::StateFilled as i32,
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
        let error = select(&[], 5).unwrap_err();
        assert_eq!((error.class, error.code), (1, "invalid-slot"));
        assert!(!filled(&SlotStatus {
            state: SlotState::StateDamaged as i32,
            ..slot
        }));
    }
}
