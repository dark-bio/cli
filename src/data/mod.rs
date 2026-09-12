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
    progress::{Processing, Transfer},
};
use darkbio_connect::{
    Dataset, UploadProgress,
    schema::{self, SlotState, SlotStatus},
};
use serde_json::{Value, json};
use std::io::Seek;
use std::time::Instant;

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
            context.output.table(
                &json!({"slots":rows}),
                &rows,
                &[
                    ("SLOT", "slot"),
                    ("STATE", "state"),
                    ("ORIGIN", "origin"),
                    ("BUILD", "build"),
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
            value["required_by"] = json!(required_by(&slots, slot.kind));
            value["cached"] = json!(
                download(slot).is_some_and(|(_, _, hash)| cache::cached(&cache::directory(), hash))
            );
            context.output.document(&value)
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
        return context.output.document(&value);
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

pub(crate) struct Progress<'a> {
    context: &'a Context,
    slot: i32,
    transfer: Transfer,
    processing: Processing,
    pub uploaded: u64,
    pub phases: Vec<String>,
}
impl<'a> Progress<'a> {
    pub fn new(context: &'a Context, slot: i32) -> Self {
        Self {
            context,
            slot,
            transfer: Transfer::new(context.output.human()),
            processing: Processing::new(context.output.human()),
            uploaded: 0,
            phases: Vec::new(),
        }
    }
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
                    self.context.output.event("progress", line);
                }
            }
            UploadProgress::Processing(status) => {
                self.phases = status
                    .phases
                    .iter()
                    .map(|phase| phase.name.clone())
                    .collect();
                if let Some(line) = self.processing.update(&status) {
                    self.context.output.event("progress", line);
                }
            }
        }
    }
}

fn upload_approval(slot: i32) -> bool {
    !matches!(
        schema::SlotKind::try_from(slot),
        Ok(schema::SlotKind::SlotReferenceGenome
            | schema::SlotKind::SlotGeneAnnotations
            | schema::SlotKind::SlotVariantCatalog)
    )
}

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

pub(crate) fn select(slots: &[SlotStatus], id: i32) -> Result<&SlotStatus, Error> {
    slots.iter().find(|slot| slot.kind == id).ok_or_else(|| {
        Error::new(
            1,
            "invalid-slot",
            format!("the Ark has no slot {}", slot_name(id)),
        )
    })
}
pub(crate) fn filled(slot: &SlotStatus) -> bool {
    slot.state == SlotState::StateFilled as i32
}
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
fn required_by(slots: &[SlotStatus], id: i32) -> Vec<String> {
    slots
        .iter()
        .filter(|slot| slot.deps.contains(&id))
        .map(|slot| slot_name(slot.kind))
        .collect()
}
pub(crate) fn download(slot: &SlotStatus) -> Option<(&str, u64, &str)> {
    slot.download.as_ref().map(|download| {
        (
            download.url.as_str(),
            download.bytes,
            download.sha256.as_str(),
        )
    })
}
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
        "name": slot.name, "description": slot.desc,
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
        assert!(!filled(&SlotStatus {
            state: SlotState::StateDamaged as i32,
            ..slot
        }));
    }
}
