// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! The live line's facts, from transfer rates and the Ark's processing reports.
//!
//! An [`Update`] carries the words a stage shows and the [`Facts`] its JSON
//! progress event holds.

use crate::style::{self, Role, Theme};
use darkbio_clock::Clock;
use darkbio_connect::schema::SlotUploadProcessResponse;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Span of the rolling rate history, including the sample before it.
const RATE_WINDOW: Duration = Duration::from_secs(10);
/// How long a rate is observed before it yields an estimate.
const WARMUP: Duration = Duration::from_secs(1);

/// Fields of a progress event, each present in JSON and null when unknown.
#[derive(Clone, Default, Serialize)]
pub(crate) struct Facts {
    /// Stage of `ark`'s own work, such as `uploading`.
    pub stage: Option<String>,
    /// Processing phase spelled as the Ark reports it.
    pub phase: Option<String>,
    /// One-based processing step reported by the Ark.
    pub step: Option<u64>,
    /// Number of processing steps reported by the Ark.
    pub steps: Option<u64>,
    /// How far the stage is, in percent.
    pub percent: Option<u64>,
    /// Bytes acknowledged by the Ark.
    pub done_bytes: Option<u64>,
    /// Bytes the transfer sends in all.
    pub total_bytes: Option<u64>,
    /// Seconds left, estimated or counted down to a deadline `ark` set.
    pub remaining_seconds: Option<u64>,
}

impl Facts {
    /// Builds the progress event for `message`, with every field present.
    pub fn event(&self, message: &str) -> Value {
        let mut event = json!({"event":"progress", "message":message});
        event.as_object_mut().unwrap().extend(
            serde_json::to_value(self)
                .unwrap()
                .as_object()
                .unwrap()
                .clone(),
        );
        event
    }
}

/// One observation of a stage, as the reading line and JSON both show it.
#[derive(Clone, Default)]
pub(crate) struct Update {
    /// Words of the live line, such as `Uploading`.
    pub label: String,
    /// Words the outcome line shows on success, where an Ark phase keeps its
    /// own name instead.
    pub completed: Option<String>,
    /// Fact the outcome line shows on success in place of the time.
    pub completed_fact: Option<String>,
    /// Whether the stage leaves no line on success.
    pub transient: bool,
    /// Time the work has run by its own report, which the timer continues.
    pub elapsed: Option<Duration>,
    /// Fields of the JSON progress event.
    pub facts: Facts,
    /// Facts after the percent, which a narrow line drops from the right.
    pub details: Vec<String>,
    /// Whether the Ark reported a failed phase.
    pub failed: bool,
    /// Whether the Ark restarted processing without completing the prior phase.
    pub restarted: bool,
}

impl Update {
    /// Builds the first update of a stage, without amounts or Ark phases.
    pub fn new(stage: &str, label: &str) -> Self {
        Self {
            label: label.into(),
            facts: Facts {
                stage: Some(stage.into()),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// Reports whether `other` belongs to the same stage and Ark step, and so
    /// to the same live line.
    pub fn same_stage(&self, other: &Self) -> bool {
        self.facts.stage == other.facts.stage
            && self.facts.phase == other.facts.phase
            && self.facts.step == other.facts.step
    }

    /// Returns the line's words without mark, bar or style, as JSON carries
    /// them.
    pub fn message(&self) -> String {
        let mut message = self.label.clone();
        if let Some(percent) = self.facts.percent {
            message.push_str(&format!(" {percent}%"));
        }
        for detail in &self.details {
            message.push_str(&format!(" · {detail}"));
        }
        message
    }

    /// Renders the live line, shrinking the bar from 40 to 8 cells before
    /// facts drop from the right.
    pub fn render(&self, theme: &Theme) -> String {
        // Leave a cell free so an unterminated live line cannot wrap itself
        let width = theme.width.saturating_sub(1);
        let label = theme.inline(&style::printable(&self.label));
        let mark = theme.glyph("›", ">");
        let Some(percent) = self.facts.percent else {
            return theme.truncate(
                &format!(
                    "{mark} {}",
                    theme
                        .inline(&style::printable(&self.message()))
                        .replace(" · ", &theme.separator())
                ),
                width,
            );
        };
        let percent = format!("{percent}%");
        let fixed = console::measure_text_width(&format!("{mark} {label}  {percent}"));

        // Drop facts from the right once the bar is down to 8 cells
        let mut details: Vec<_> = self
            .details
            .iter()
            .map(|text| theme.inline(&style::printable(text)))
            .collect();
        let (facts, size) = loop {
            let facts = if details.is_empty() {
                String::new()
            } else {
                format!("  {}", details.join(&theme.separator()))
            };
            let size = width
                .saturating_sub(fixed + console::measure_text_width(&facts))
                .min(40);
            if size >= 8 || details.is_empty() {
                break (facts, size);
            }
            details.pop();
        };
        let bar = if size >= 8 {
            let filled = (size as u64 * self.facts.percent.unwrap().min(100) / 100) as usize;
            format!(
                "{}{} ",
                theme.paint(Role::Success, theme.glyph("━", "=").repeat(filled)),
                theme.paint(Role::Muted, theme.glyph("─", "-").repeat(size - filled))
            )
        } else {
            String::new()
        };

        // On a very narrow terminal, shorten the words before the percent
        let used = console::measure_text_width(&format!("{mark}  {bar}{percent}{facts}"));
        let label = theme.truncate(&label, width.saturating_sub(used));
        theme.truncate(&format!("{mark} {label} {bar}{percent}{facts}"), width)
    }
}

/// Transfer rate and time left, from the bytes the Ark acknowledges.
pub(super) struct Transfer {
    /// Caller's update naming the stage, its words and its outcome.
    initial: Update,
    /// Connection's clock, which times the samples.
    clock: Clock,
    /// Recent acknowledged byte counts.
    rate: Rate,
    /// How often an update goes out, apart from how often bytes are sampled.
    report: Report,
}

impl Transfer {
    /// Starts without rate history so setup never skews estimates.
    pub(super) fn new(human: bool, clock: Clock, initial: Update) -> Self {
        Self {
            initial,
            clock,
            rate: Rate::default(),
            report: Report::new(human),
        }
    }

    /// Returns the stage's first update, shown before any byte is acknowledged.
    pub(super) fn initial(&self) -> &Update {
        &self.initial
    }

    /// Samples an acknowledgment, returning an update when one is due.
    pub(super) fn update(&mut self, uploaded: u64, total: u64) -> Option<Update> {
        self.update_at(uploaded, total, self.clock.now())
    }

    /// Samples an acknowledgment at `now`, so tests set the time.
    fn update_at(&mut self, uploaded: u64, total: u64, now: Instant) -> Option<Update> {
        let rate = self.rate.sample(uploaded, now);
        let percent = percent(uploaded, total);
        if !self.report.due(percent, now) {
            return None;
        }
        let remaining = eta(total.saturating_sub(uploaded), rate);
        let mut details = Vec::new();
        if let Some(rate) = rate {
            details.push(format!("{}/s", style::bytes(rate.max(0.0) as u64)));
        }
        if let Some(seconds) = remaining {
            details.push(format!("{} left", style::duration(seconds)));
        }
        Some(Update {
            facts: Facts {
                stage: self.initial.facts.stage.clone(),
                percent: Some(percent),
                done_bytes: Some(uploaded),
                total_bytes: Some(total),
                remaining_seconds: remaining,
                ..Default::default()
            },
            details,
            ..self.initial.clone()
        })
    }
}

/// Progress and time left of the Ark's processing, phase by phase, under the
/// Ark's names.
pub(super) struct Processing {
    /// Connection's clock, which times the samples.
    clock: Clock,
    /// Previous report, whose phase ends once the Ark moves on.
    previous: Option<SlotUploadProcessResponse>,
    /// Progress samples of the current phase, in hundredths of a percent.
    rate: Rate,
    /// How often an update goes out, reset when a phase starts or restarts.
    report: Report,
}

impl Processing {
    /// Starts before any phase is known.
    pub(super) fn new(human: bool, clock: Clock) -> Self {
        Self {
            clock,
            previous: None,
            rate: Rate::default(),
            report: Report::new(human),
        }
    }

    /// Returns the updates a report yields, ending the previous phase first
    /// when the Ark moved on.
    pub(super) fn update(
        &mut self,
        status: &SlotUploadProcessResponse,
    ) -> impl Iterator<Item = Update> {
        let completed = self
            .previous
            .as_ref()
            .filter(|previous| {
                previous.proc_start == status.proc_start
                    && previous.phase_in < status.phase_in
                    && previous.phase_progress < 10_000
                    && status.failure.is_empty()
            })
            .map(|previous| {
                let mut completed = previous.clone();
                completed.phase_progress = 10_000;
                Self::observation(&completed, None)
            });
        let now = self.clock.now();
        completed.into_iter().chain(self.update_at(status, now))
    }

    /// Samples a report at `now`, starting the estimate afresh for a new phase.
    fn update_at(&mut self, status: &SlotUploadProcessResponse, now: Instant) -> Option<Update> {
        // A restart starts timing afresh, without ending the old phase as done
        let restarted = self.previous.as_ref().is_some_and(|previous| {
            previous.proc_start != status.proc_start
                || previous.phase_in > status.phase_in
                || (previous.phase_in == status.phase_in
                    && previous.phase_start != status.phase_start)
        });
        let phase = (status.proc_start, status.phase_in, status.phase_start);
        if self.previous.as_ref().is_none_or(|previous| {
            (previous.proc_start, previous.phase_in, previous.phase_start) != phase
        }) {
            self.rate = Rate::default();
            self.report.last = None;
        }
        // Sample every report, including those no update goes out for
        self.previous = Some(status.clone());
        let rate = self.rate.sample(status.phase_progress, now);
        if !self.report.due(percent(status.phase_progress, 10_000), now)
            && status.failure.is_empty()
        {
            return None;
        }
        Some(Update {
            restarted,
            ..Self::observation(status, rate)
        })
    }

    /// Builds the update for a report, under the Ark's phase name, or
    /// `Processing` when it gives none.
    fn observation(status: &SlotUploadProcessResponse, rate: Option<f64>) -> Update {
        let name = status
            .phase_in
            .checked_sub(1)
            .and_then(|index| usize::try_from(index).ok())
            .and_then(|index| status.phases.get(index))
            .map(|phase| phase.name.clone());
        let remaining = eta(10_000_u64.saturating_sub(status.phase_progress), rate);
        Update {
            label: name.clone().unwrap_or_else(|| "Processing".into()),
            facts: Facts {
                stage: Some("processing".into()),
                phase: name,
                step: Some(status.phase_in),
                steps: Some(status.phases.len() as u64),
                percent: Some(percent(status.phase_progress, 10_000)),
                remaining_seconds: remaining,
                ..Default::default()
            },
            details: remaining
                .map(|seconds| format!("{} left", style::duration(seconds)))
                .into_iter()
                .collect(),
            failed: !status.failure.is_empty(),
            restarted: false,
            ..Default::default()
        }
    }
}

/// Rate of a counter over the last 10s.
#[derive(Default)]
struct Rate {
    /// Time and value samples of the window, and the one before it.
    samples: VecDeque<(Instant, u64)>,
}

impl Rate {
    /// Samples `value` and returns its rate per second once warmed up, starting
    /// over when the value goes back.
    fn sample(&mut self, value: u64, now: Instant) -> Option<f64> {
        if self
            .samples
            .back()
            .is_some_and(|&(time, previous)| now < time || value < previous)
        {
            self.samples.clear();
        }
        self.samples.push_back((now, value));
        while self.samples.len() > 2 && now.duration_since(self.samples[1].0) >= RATE_WINDOW {
            self.samples.pop_front();
        }
        let &(start, initial) = self.samples.front()?;
        let elapsed = now.duration_since(start);
        (elapsed >= WARMUP).then(|| (value - initial) as f64 / elapsed.as_secs_f64())
    }
}

/// How often updates go out, every second on a terminal, and every ten percent
/// or 5s in a pipe.
struct Report {
    /// Whether the terminal's pace of once a second applies.
    human: bool,
    /// Time and percent of the last update.
    last: Option<(Instant, u64)>,
}

impl Report {
    /// Starts a pace that lets the first update out at once.
    fn new(human: bool) -> Self {
        Self { human, last: None }
    }

    /// Reports whether an update at `percent` is due now, and records it if so.
    fn due(&mut self, percent: u64, now: Instant) -> bool {
        if percent == 100 && self.last.is_some_and(|(_, previous)| previous == 100) {
            return false;
        }
        if self.last.is_none_or(|(time, previous)| {
            if self.human {
                (percent == 100 && previous != 100)
                    || now.saturating_duration_since(time) >= Duration::from_secs(1)
            } else {
                percent / 10 != previous / 10
                    || now.saturating_duration_since(time) >= Duration::from_secs(5)
            }
        }) {
            self.last = Some((now, percent));
            true
        } else {
            false
        }
    }
}

/// Computes a clamped percentage without overflowing byte counters.
fn percent(done: u64, total: u64) -> u64 {
    if total == 0 {
        return 0;
    }
    (u128::from(done.min(total)) * 100 / u128::from(total)) as u64
}

/// Estimates the seconds left, rounded up, or none without a usable rate.
fn eta(remaining: u64, rate: Option<f64>) -> Option<u64> {
    if remaining == 0 {
        return Some(0);
    }
    rate.filter(|rate| rate.is_finite() && *rate > 0.0)
        .and_then(|rate| Duration::try_from_secs_f64((remaining as f64 / rate).ceil()).ok())
        .map(|duration| duration.as_secs())
}

/// Tests of the live line, rates, Ark phases and the update pace.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::Color;
    use darkbio_clock::TestClock;

    /// The bar shrinks to 8 cells before facts drop from the right.
    #[test]
    fn test_bar_and_fact_priority() {
        let mut update = Update::new("uploading", "Uploading");
        update.facts.percent = Some(50);
        update.details = vec!["10.0 MiB/s".into(), "5s left".into()];
        let wide = update.render(&Theme::test(120, Color::Basic, false));
        assert_eq!(
            wide,
            "> Uploading \x1b[1m====================\x1b[0m-------------------- 50%  10.0 MiB/s - 5s left"
        );
        for (width, bar, rate, left) in [
            (80, 40, true, true),
            (48, 9, true, true),
            (47, 8, true, true),
            (46, 17, true, false),
            (30, 13, false, false),
        ] {
            let rendered = update.render(&Theme::test(width, Color::True, true));
            let plain = console::strip_ansi_codes(&rendered);
            assert!(
                console::measure_text_width(&rendered) < width,
                "{width}: {rendered}"
            );
            assert_eq!(
                plain.chars().filter(|ch| matches!(ch, '━' | '─')).count(),
                bar,
                "{width}"
            );
            assert_eq!(plain.contains("10.0 MiB/s"), rate, "{width}");
            assert_eq!(plain.contains("5s left"), left, "{width}");
            assert!(plain.contains("50%"));
        }
    }

    /// Upload estimates count only acknowledged bytes since sampling began.
    #[test]
    fn test_transfer_estimate_and_unknown_facts() {
        let clock = TestClock::new().clock();
        let start = clock.now();
        let mut transfer = Transfer::new(false, clock, Update::new("uploading", "Uploading"));
        let mib = 1024 * 1024;
        let first = transfer.update_at(25 * mib, 100 * mib, start).unwrap();
        assert_eq!(first.message(), "Uploading 25%");
        let next = transfer
            .update_at(50 * mib, 100 * mib, start + Duration::from_secs(5))
            .unwrap();
        assert_eq!(next.message(), "Uploading 50% · 5.0 MiB/s · 10s left");
        assert_eq!(next.facts.done_bytes, Some(50 * mib));
        assert_eq!(next.facts.remaining_seconds, Some(10));
        assert_eq!(
            Facts::default().event("waiting"),
            json!({"event":"progress","message":"waiting","stage":null,"phase":null,"step":null,"steps":null,"percent":null,"done_bytes":null,"total_bytes":null,"remaining_seconds":null})
        );
    }

    /// Builds an Ark processing report with two phases.
    fn status(phase: u64, progress: u64) -> SlotUploadProcessResponse {
        SlotUploadProcessResponse {
            proc_start: 100,
            phase_start: 100 + phase,
            phases: ["Compressing", "Sorting"]
                .map(|name| darkbio_connect::schema::SlotPhase {
                    name: name.into(),
                    desc: String::new(),
                })
                .into(),
            phase_in: phase,
            phase_progress: progress,
            ..Default::default()
        }
    }

    /// A phase ends when the Ark moves on, and each phase starts its estimate
    /// afresh.
    #[test]
    fn test_processing_lifecycle() {
        let clock = TestClock::new().clock();
        let start = clock.now();
        let mut processing = Processing::new(false, clock);
        assert_eq!(
            processing
                .update_at(&status(1, 1000), start)
                .unwrap()
                .message(),
            "Compressing 10%"
        );
        let next = processing
            .update_at(&status(1, 3000), start + Duration::from_secs(5))
            .unwrap();
        assert_eq!(next.message(), "Compressing 30% · 18s left");
        assert_eq!(next.facts.phase.as_deref(), Some("Compressing"));
        assert_eq!(next.facts.step, Some(1));
        assert_eq!(next.facts.steps, Some(2));
        let next: Vec<_> = processing.update(&status(2, 4000)).collect();
        assert_eq!(next.len(), 2);
        assert_eq!(next[0].facts.percent, Some(100));
        assert_eq!(next[1].message(), "Sorting 40%");
    }

    /// A failed report never ends the previous phase as done.
    #[test]
    fn test_processing_failure() {
        let mut processing = Processing::new(true, TestClock::new().clock());
        assert_eq!(processing.update(&status(1, 8200)).count(), 1);
        let updates: Vec<_> = processing
            .update(&SlotUploadProcessResponse {
                failure: "failed".into(),
                ..status(2, 2000)
            })
            .collect();
        assert_eq!(updates.len(), 1);
        assert!(updates[0].failed);
    }

    /// A restart keeps neither an outcome nor an estimate of the old phase.
    #[test]
    fn test_processing_restarts() {
        for next in [
            SlotUploadProcessResponse {
                phase_start: 999,
                ..status(1, 2000)
            },
            SlotUploadProcessResponse {
                proc_start: 999,
                ..status(2, 2000)
            },
        ] {
            let mut processing = Processing::new(true, TestClock::new().clock());
            assert_eq!(processing.update(&status(1, 8200)).count(), 1);
            let updates: Vec<_> = processing.update(&next).collect();
            assert_eq!(updates.len(), 1);
            assert!(updates[0].restarted);
            assert_eq!(updates[0].facts.percent, Some(20));
            assert_eq!(updates[0].facts.remaining_seconds, None);
        }
    }

    /// A stall ages out a fast rate, and a counter going back starts over.
    #[test]
    fn test_rate_stall_and_reset() {
        let start = TestClock::new().clock().now();
        let mut rate = Rate::default();
        assert_eq!(rate.sample(0, start), None);
        assert_eq!(
            rate.sample(100, start + Duration::from_secs(1)),
            Some(100.0)
        );
        for seconds in 2..=11 {
            rate.sample(100, start + Duration::from_secs(seconds));
        }
        let stopped = rate.sample(100, start + Duration::from_secs(12));
        assert_eq!(stopped, Some(0.0));
        assert_eq!(eta(100, stopped), None);
        assert_eq!(rate.sample(50, start + Duration::from_secs(13)), None);
        assert_eq!(rate.sample(75, start + Duration::from_secs(14)), Some(25.0));
    }

    /// A pipe gets an update every ten percent or 5s, and a terminal every
    /// second.
    #[test]
    fn test_report_cadence() {
        let start = TestClock::new().clock().now();
        for human in [false, true] {
            let mut report = Report::new(human);
            assert!(report.due(1, start));
            assert!(!report.due(2, start + Duration::from_millis(100)));
            assert_eq!(report.due(10, start + Duration::from_millis(200)), !human);
            assert_eq!(report.due(11, start + Duration::from_secs(1)), human);
            assert!(report.due(11, start + Duration::from_secs(6)));
            assert!(report.due(100, start + Duration::from_millis(6100)));
            assert!(!report.due(100, start + Duration::from_secs(20)));
        }
    }
}
