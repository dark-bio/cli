// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Transfer rates and per-step estimates for terminal progress.

use crate::style::{Role, Theme};
use darkbio_clock::Clock;
use darkbio_connect::schema::SlotUploadProcessResponse;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Binary megabyte divisor used for byte-rate formatting.
const MIB: f64 = 1024.0 * 1024.0;
/// Rolling rate history, retaining one sample before the boundary.
const RATE_WINDOW: Duration = Duration::from_secs(10);
/// Minimum observation span before publishing an estimate.
const WARMUP: Duration = Duration::from_secs(1);
/// Minimum interval between ordinary human progress observations.
const HUMAN_REPORT_INTERVAL: Duration = Duration::from_secs(1);
/// Maximum silence between machine progress lines when reports keep arriving.
const REPORT_INTERVAL: Duration = Duration::from_secs(5);

/// One observation, with the established log line and facts for the terminal.
pub(crate) struct Update {
    /// Stable text observation used by plain text and JSON events.
    pub text: String,
    /// Human stage label and live-line identity.
    pub stage: String,
    /// Label column shared by the known phases, measured in terminal cells.
    pub stage_width: usize,
    /// Completion percentage for this stage, not for the entire workflow.
    pub percent: u64,
    /// Human facts in discard order; the leftmost is dropped first on narrow terminals.
    pub details: Vec<String>,
    /// Shared fact column widths, allowing narrower layouts without moving the bars.
    pub detail_widths: Vec<usize>,
}

impl Update {
    /// Shares label and fact columns with another stage of the same operation.
    pub(super) fn align(&mut self, other: &mut Self) {
        let stage_width = self.stage_width.max(other.stage_width);
        let mut detail_widths = Vec::new();
        for update in [&*self, &*other] {
            if update.detail_widths.is_empty() {
                for index in 0..update.details.len() {
                    detail_widths.push(console::measure_text_width(
                        &update.details[index..].join(" - "),
                    ));
                }
            } else {
                detail_widths.extend_from_slice(&update.detail_widths);
            }
        }
        detail_widths.sort_unstable();
        detail_widths.dedup();
        self.stage_width = stage_width;
        other.stage_width = stage_width;
        self.detail_widths = detail_widths.clone();
        other.detail_widths = detail_widths;
    }

    /// Fits a stage, progress bar and optional facts into one terminal line.
    pub fn render(&self, theme: &Theme) -> String {
        let width = theme.width.saturating_sub(1);
        let stage_width = self.stage_width.min((width / 2).max(8));
        let stage = theme.truncate(&self.stage, stage_width);
        let stage = format!(
            "{stage}{}",
            " ".repeat(stage_width.saturating_sub(console::measure_text_width(&stage)))
        );
        let prefix = theme.paint(Role::Muted, "progress:");
        let percent = format!("{:3} %", self.percent);
        let fixed = console::measure_text_width(&format!("progress: {stage} {percent}"));
        let reserve = if theme.width >= 60 { 9 } else { 0 };
        let available = width.saturating_sub(fixed + reserve);
        let shared = (!self.detail_widths.is_empty()).then(|| {
            self.detail_widths
                .iter()
                .map(|width| width + console::measure_text_width(&theme.separator()))
                .filter(|width| *width <= available)
                .max()
                .unwrap_or(0)
        });
        let mut details = self.details.clone();
        let (facts, facts_width) = loop {
            let facts = if details.is_empty() {
                String::new()
            } else {
                format!("{}{}", theme.separator(), details.join(&theme.separator()))
            };
            let facts_width = console::measure_text_width(&facts);
            if facts_width <= shared.unwrap_or(available) || details.is_empty() {
                break (facts, shared.unwrap_or(facts_width));
            }
            details.remove(0);
        };
        let used = fixed + facts_width;
        let size = width.saturating_sub(used + 1).min(40);
        let bar = if size >= 8 {
            let filled = (size as u64 * self.percent.min(100) / 100) as usize;
            format!(
                "{}{} ",
                theme.paint(Role::Success, theme.glyph("\u{2501}", "=").repeat(filled)),
                theme.paint(
                    Role::Muted,
                    theme.glyph("\u{2500}", "-").repeat(size - filled)
                )
            )
        } else {
            String::new()
        };
        theme.truncate(&format!("{prefix} {stage} {bar}{percent}{facts}"), width)
    }
}

/// Samples only acknowledged bytes, starting with the first upload report so
/// cloud setup and approval do not enter the rate estimate.
pub(super) struct Transfer {
    /// Connection's clock, which times the samples.
    clock: Clock,
    /// Rolling counter samples, in bytes for uploads and basis points for processing.
    rate: Rate,
    /// Emission cadence, independent of the sampling cadence.
    report: Report,
}

impl Transfer {
    /// Starts without rate history so setup and approval cannot skew the first estimate.
    pub(super) fn new(human: bool, clock: Clock) -> Self {
        Self {
            clock,
            rate: Rate::default(),
            report: Report::new(human),
        }
    }

    /// Samples acknowledged bytes and emits an observation only when reporting is due.
    pub(super) fn update(&mut self, uploaded: u64, total: u64) -> Option<Update> {
        self.update_at(uploaded, total, self.clock.now())
    }

    /// Updates byte-rate history at the supplied clock time, even if output is throttled.
    fn update_at(&mut self, uploaded: u64, total: u64, now: Instant) -> Option<Update> {
        let rate = self.rate.sample(uploaded, now);
        let percent = percent(uploaded, total);
        if !self.report.due(percent, now) {
            return None;
        }
        let speed = rate.map_or_else(
            || {
                if uploaded >= total {
                    "speed unavailable".into()
                } else {
                    "speed estimating...".into()
                }
            },
            speed,
        );
        let text = format!(
            "Uploading: {percent}% ({:.1}/{:.1} MiB) | {speed} | ETA {}",
            uploaded as f64 / MIB,
            total as f64 / MIB,
            eta(total.saturating_sub(uploaded), rate),
        );
        let (divisor, unit) = if total >= 1 << 30 {
            ((1_u64 << 30) as f64, "GiB")
        } else {
            (MIB, "MiB")
        };
        Some(Update {
            text,
            stage: "uploading".into(),
            stage_width: "uploading".len(),
            percent,
            details: vec![
                format!(
                    "{:.1}/{:.1} {unit}",
                    uploaded as f64 / divisor,
                    total as f64 / divisor
                ),
                speed,
                human_eta(total.saturating_sub(uploaded), rate),
            ],
            detail_widths: Vec::new(),
        })
    }
}

/// Progress percentages belong to individual steps. Device timestamps identify
/// a restarted step; elapsed time is measured on the connection's monotonic clock.
pub(super) struct Processing {
    /// Connection's clock, which times the samples.
    clock: Clock,
    /// Last report, retained to finish a phase when the next report advances past it.
    previous: Option<SlotUploadProcessResponse>,
    /// Basis-point progress samples for the current processing step only.
    rate: Rate,
    /// Reporting cadence reset whenever a step starts or restarts.
    report: Report,
}

impl Processing {
    /// Starts without a step identity or estimate; the first report establishes both.
    pub(super) fn new(human: bool, clock: Clock) -> Self {
        Self {
            clock,
            previous: None,
            rate: Rate::default(),
            report: Report::new(human),
        }
    }

    /// Finishes an observed phase before starting a later one in the same run.
    /// Restarts and failures never imply successful completion of the previous phase.
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

    /// Builds a step-specific estimate from basis points and monotonic clock time.
    fn update_at(&mut self, status: &SlotUploadProcessResponse, now: Instant) -> Option<Update> {
        let phase = (status.proc_start, status.phase_in, status.phase_start);
        if self.previous.as_ref().is_none_or(|previous| {
            (previous.proc_start, previous.phase_in, previous.phase_start) != phase
        }) {
            self.rate = Rate::default();
            self.report.last = None;
        }
        self.previous = Some(status.clone());
        let rate = self.rate.sample(status.phase_progress, now);
        let percent = percent(status.phase_progress, 10_000);
        if !self.report.due(percent, now) {
            return None;
        }
        Some(Self::observation(status, rate))
    }

    /// Reserves the widest phase label and the initial ETA before rendering any step.
    fn observation(status: &SlotUploadProcessResponse, rate: Option<f64>) -> Update {
        let percent = percent(status.phase_progress, 10_000);
        let name = status
            .phase_in
            .checked_sub(1)
            .and_then(|index| usize::try_from(index).ok())
            .and_then(|index| status.phases.get(index).map(|phase| &phase.name))
            .map(String::as_str)
            .unwrap_or("Processing");
        let text = format!(
            "Processing [{}/{}] {name}: {percent}% | step ETA {}",
            status.phase_in,
            status.phases.len(),
            eta(10_000_u64.saturating_sub(status.phase_progress), rate),
        );
        let stage = format!(
            "processing [{}/{}] {name}",
            status.phase_in,
            status.phases.len()
        );
        let stage_width = status
            .phases
            .iter()
            .enumerate()
            .map(|(index, phase)| {
                console::measure_text_width(&format!(
                    "processing [{}/{}] {}",
                    index + 1,
                    status.phases.len(),
                    phase.name
                ))
            })
            .max()
            .unwrap_or_else(|| console::measure_text_width(&stage));
        Update {
            text,
            stage,
            stage_width,
            percent,
            details: vec![human_eta(
                10_000_u64.saturating_sub(status.phase_progress),
                rate,
            )],
            detail_widths: vec!["eta estimating...".len(), "eta 0 s".len()],
        }
    }
}

/// Keep the sample immediately before the rolling window's boundary so a
/// stalled or sparse counter does not retain an old, optimistic speed.
#[derive(Default)]
struct Rate {
    /// Time and monotonically increasing counter observations around the rolling window.
    samples: VecDeque<(Instant, u64)>,
}

impl Rate {
    /// Returns units per second after warmup; counter or clock regression resets history.
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

/// Refresh terminal progress once a second as reports arrive. Line output uses
/// ten-percent boundaries or five seconds to keep logs readable.
struct Report {
    /// Selects one-second human cadence instead of sparse log boundaries.
    human: bool,
    /// Time and percentage of the last emitted observation.
    last: Option<(Instant, u64)>,
}

impl Report {
    /// Starts a cadence that always emits the first observation.
    fn new(human: bool) -> Self {
        Self { human, last: None }
    }

    /// Claims an emission slot for elapsed cadence or a required completion boundary.
    fn due(&mut self, percent: u64, now: Instant) -> bool {
        if self.last.is_none_or(|(time, previous)| {
            if self.human {
                (percent == 100 && previous != 100)
                    || now.saturating_duration_since(time) >= HUMAN_REPORT_INTERVAL
            } else {
                percent / 10 != previous / 10
                    || now.saturating_duration_since(time) >= REPORT_INTERVAL
            }
        }) {
            self.last = Some((now, percent));
            true
        } else {
            false
        }
    }
}

/// Computes a clamped integer percentage without overflowing 64-bit counters.
fn percent(done: u64, total: u64) -> u64 {
    if total == 0 {
        return 0;
    }
    (u128::from(done.min(total)) * 100 / u128::from(total)) as u64
}

/// Formats bytes per second in the smallest useful binary unit.
fn speed(bytes: f64) -> String {
    if bytes >= MIB {
        format!("{:.1} MiB/s", bytes / MIB)
    } else if bytes >= 1024.0 {
        format!("{:.1} KiB/s", bytes / 1024.0)
    } else {
        format!("{bytes:.0} B/s")
    }
}

/// Estimates rounded-up remaining time, withholding nonpositive or nonfinite rates.
fn eta(remaining: u64, rate: Option<f64>) -> String {
    if remaining == 0 {
        return "0s".into();
    }
    let Some(seconds) = rate
        .filter(|rate| rate.is_finite() && *rate > 0.0)
        .and_then(|rate| Duration::try_from_secs_f64((remaining as f64 / rate).ceil()).ok())
        .map(|duration| duration.as_secs())
    else {
        return "estimating...".into();
    };
    if seconds >= 3600 {
        format!("~{}h {:02}m", seconds / 3600, seconds % 3600 / 60)
    } else if seconds >= 60 {
        format!("~{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!("~{seconds}s")
    }
}

/// Adds human spacing and an ETA label to the shared remaining-time estimate.
fn human_eta(remaining: u64, rate: Option<f64>) -> String {
    let eta = eta(remaining, rate);
    if eta == "estimating..." {
        return format!("eta {eta}");
    }
    format!(
        "eta {}",
        eta.replace('h', " h").replace('m', " m").replace('s', " s")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use darkbio_clock::TestClock;

    #[test]
    fn bar_keeps_speed_and_eta_in_the_available_width() {
        use crate::style::Color;
        let update = Update {
            text: String::new(),
            stage: "uploading".into(),
            stage_width: "uploading".len(),
            percent: 50,
            details: vec![
                "50.0/100.0 MiB".into(),
                "10.0 MiB/s".into(),
                "eta 5 s".into(),
            ],
            detail_widths: Vec::new(),
        };
        let theme = Theme::test(120, Color::Basic, false);
        assert_eq!(
            update.render(&theme),
            "progress: uploading \x1b[1m====================\x1b[0m--------------------  50 % - 50.0/100.0 MiB - 10.0 MiB/s - eta 5 s"
        );
        for width in [32, 60, 80, 120] {
            let theme = Theme::test(width, Color::True, true);
            let rendered = update.render(&theme);
            assert!(
                console::measure_text_width(&rendered) < width,
                "{width}: {rendered}"
            );
            assert!(rendered.contains("50 %"));
            if width >= 60 {
                assert!(rendered.contains('\u{2501}'));
                assert!(rendered.contains("10.0 MiB/s"));
                assert!(rendered.contains("eta 5 s"), "{width}: {rendered:?}");
            }
        }
    }

    /// The initial bytes were accepted before sampling began. Counting them as
    /// newly transferred would inflate speed and shorten the ETA.
    #[test]
    fn test_transfer_estimate() {
        let clock = TestClock::new().clock();
        let start = clock.now();
        let mut transfer = Transfer::new(false, clock);
        let mib = 1024 * 1024;
        let first = transfer.update_at(25 * mib, 100 * mib, start).unwrap().text;
        assert!(first.contains("speed estimating... | ETA estimating..."));
        let next = transfer
            .update_at(50 * mib, 100 * mib, start + Duration::from_secs(5))
            .unwrap()
            .text;
        assert_eq!(
            next,
            "Uploading: 50% (50.0/100.0 MiB) | 5.0 MiB/s | ETA ~10s"
        );
        let done = transfer
            .update_at(100 * mib, 100 * mib, start + Duration::from_secs(10))
            .unwrap()
            .text;
        assert!(done.ends_with("ETA 0s"));
    }

    fn status(phase: u64, progress: u64) -> SlotUploadProcessResponse {
        SlotUploadProcessResponse {
            proc_start: 100,
            phase_start: 100 + phase,
            phases: ["Validate", "Index"]
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

    #[test]
    fn processing_bars_align_across_labels_and_estimates() {
        use crate::style::Color;
        let mut report = status(1, 8200);
        report.phases = ["Compressing", "Indexing", "\u{68c0}\u{9a8c}"]
            .into_iter()
            .cycle()
            .take(12)
            .map(|name| darkbio_connect::schema::SlotPhase {
                name: name.into(),
                desc: String::new(),
            })
            .collect();
        for width in [60, 80, 100, 140] {
            let theme = Theme::test(width, Color::True, true);
            let mut expected = None;
            for phase in 1..=12 {
                report.phase_in = phase;
                for (progress, rate) in [(8200, None), (9200, Some(500.0)), (10_000, None)] {
                    report.phase_progress = progress;
                    let rendered = Processing::observation(&report, rate).render(&theme);
                    let line = console::strip_ansi_codes(&rendered);
                    let position = bar_position(&line);
                    assert_eq!(position, *expected.get_or_insert(position), "{line}");
                    assert!(console::measure_text_width(&line) < width);
                }
            }
        }
    }

    #[test]
    fn upload_and_processing_share_columns_without_losing_transfer_facts() {
        use crate::style::Color;
        for width in [60, 80, 100, 140] {
            let theme = Theme::test(width, Color::True, true);
            let clock = TestClock::new().clock();
            let start = clock.now();
            let total = 290 * 1024 * 1024;
            let mut transfer = Transfer::new(true, clock.clone());
            transfer.update_at(0, total, start);
            let mut upload = transfer
                .update_at(total, total, start + Duration::from_secs(8))
                .unwrap();
            let mut report = status(1, 8200);
            report.phases[0].name = "Compressing".into();
            report.phases[1].name = "Indexing".into();
            Processing::observation(&report, None).align(&mut upload);
            let rendered = upload.render(&theme);
            let expected = bar_position(&rendered);
            assert!(console::measure_text_width(&rendered) < width);
            if width >= 100 {
                assert!(rendered.contains("MiB/s"));
                assert!(rendered.contains("eta 0 s"));
            }
            if width >= 140 {
                assert!(rendered.contains("290.0/290.0 MiB"));
            }
            let mut processing = Processing::new(true, clock);
            for (phase, progress) in [(1, 8200), (2, 9200), (2, 10_000)] {
                report.phase_in = phase;
                report.phase_progress = progress;
                for mut update in processing.update(&report) {
                    update.align(&mut upload);
                    let rendered = update.render(&theme);
                    assert_eq!(bar_position(&rendered), expected, "{rendered}");
                    assert!(console::measure_text_width(&rendered) < width);
                }
            }
        }
    }

    fn bar_position(line: &str) -> (usize, usize) {
        let line = console::strip_ansi_codes(line);
        let bar = line
            .find(['\u{2501}', '\u{2500}'])
            .unwrap_or_else(|| panic!("missing progress bar: {line}"));
        (
            console::measure_text_width(&line[..bar]),
            line[bar..]
                .chars()
                .take_while(|ch| matches!(ch, '\u{2501}' | '\u{2500}'))
                .count(),
        )
    }

    #[test]
    fn advancing_completes_the_previous_phase_before_rendering_the_next() {
        let mut processing = Processing::new(true, TestClock::new().clock());
        let first: Vec<_> = processing.update(&status(1, 8200)).collect();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].percent, 82);
        let next: Vec<_> = processing.update(&status(2, 9200)).collect();
        assert_eq!(next.len(), 2);
        assert_eq!(next[0].stage, first[0].stage);
        assert_eq!(next[0].percent, 100);
        assert!(next[0].text.ends_with("100% | step ETA 0s"));
        assert_eq!(next[0].details, ["eta 0 s"]);
        assert_eq!(next[1].percent, 92);
        let done: Vec<_> = processing.update(&status(2, 10_000)).collect();
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].stage, next[1].stage);
        assert_eq!(done[0].percent, 100);
        assert_eq!(done[0].details, ["eta 0 s"]);
    }

    #[test]
    fn restarts_and_failures_do_not_complete_the_previous_phase() {
        for next in [
            SlotUploadProcessResponse {
                phase_start: 999,
                ..status(1, 2000)
            },
            SlotUploadProcessResponse {
                proc_start: 999,
                ..status(2, 2000)
            },
            SlotUploadProcessResponse {
                failure: "failed".into(),
                ..status(2, 2000)
            },
        ] {
            let mut processing = Processing::new(true, TestClock::new().clock());
            assert_eq!(processing.update(&status(1, 8200)).count(), 1);
            assert!(processing.update(&next).all(|update| update.percent != 100));
        }
    }

    /// Every step gets its own estimate, even if the first observation arrives
    /// partway through it. A restarted step must not inherit its previous rate.
    #[test]
    fn test_step_estimate() {
        let clock = TestClock::new().clock();
        let start = clock.now();
        let mut processing = Processing::new(false, clock);
        assert!(
            processing
                .update_at(&status(1, 1000), start)
                .unwrap()
                .text
                .ends_with("step ETA estimating...")
        );
        assert_eq!(
            processing
                .update_at(&status(1, 3000), start + Duration::from_secs(5))
                .unwrap()
                .text,
            "Processing [1/2] Validate: 30% | step ETA ~18s"
        );
        assert_eq!(
            processing
                .update_at(&status(2, 4000), start + Duration::from_secs(6))
                .unwrap()
                .text,
            "Processing [2/2] Index: 40% | step ETA estimating..."
        );
        assert_eq!(
            processing
                .update_at(&status(2, 5000), start + Duration::from_secs(11))
                .unwrap()
                .text,
            "Processing [2/2] Index: 50% | step ETA ~25s"
        );
        let restarted = SlotUploadProcessResponse {
            phase_start: 999,
            ..status(2, 6000)
        };
        assert!(
            processing
                .update_at(&restarted, start + Duration::from_secs(12))
                .unwrap()
                .text
                .ends_with("step ETA estimating...")
        );
        let done = SlotUploadProcessResponse {
            phase_progress: 10_000,
            ..restarted
        };
        assert!(
            processing
                .update_at(&done, start + Duration::from_secs(13))
                .unwrap()
                .text
                .ends_with("step ETA 0s")
        );
    }

    /// Recent stalls age out a formerly fast rate. A regressing counter starts
    /// over instead of underflowing or producing an estimate from another run.
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
        assert_eq!(eta(100, stopped), "estimating...");
        assert_eq!(rate.sample(50, start + Duration::from_secs(13)), None);
        assert_eq!(rate.sample(75, start + Duration::from_secs(14)), Some(25.0));
    }

    /// Time-based reporting refreshes a stuck percentage, while short bursts
    /// stay quiet. Completion is printed even inside the normal interval.
    #[test]
    fn test_report_cadence() {
        let start = TestClock::new().clock().now();
        let mut report = Report::new(false);
        assert!(report.due(91, start));
        assert!(!report.due(92, start + Duration::from_secs(1)));
        assert!(report.due(92, start + REPORT_INTERVAL));
        assert!(report.due(100, start + REPORT_INTERVAL + Duration::from_millis(1)));
        assert!(!report.due(100, start + REPORT_INTERVAL + Duration::from_millis(2)));
    }

    #[test]
    fn test_human_transfer_cadence() {
        let clock = TestClock::new().clock();
        let start = clock.now();
        let mut transfer = Transfer::new(true, clock);
        assert!(transfer.update_at(0, 100, start).is_some());
        assert!(
            transfer
                .update_at(90, 100, start + Duration::from_millis(500))
                .is_none()
        );
        let next = start + Duration::from_secs(1);
        assert!(transfer.update_at(90, 100, next).is_some());
        assert!(
            transfer
                .update_at(100, 100, next + Duration::from_millis(1))
                .is_some()
        );
        assert!(
            transfer
                .update_at(100, 100, next + Duration::from_millis(2))
                .is_none()
        );
    }

    #[test]
    fn test_human_step_cadence() {
        let clock = TestClock::new().clock();
        let start = clock.now();
        let mut processing = Processing::new(true, clock);
        assert!(processing.update_at(&status(1, 1000), start).is_some());
        let next = start + Duration::from_millis(500);
        assert!(processing.update_at(&status(2, 1000), next).is_some());
        assert!(
            processing
                .update_at(&status(2, 9000), next + Duration::from_millis(500))
                .is_none()
        );
        assert!(
            processing
                .update_at(&status(2, 9000), next + Duration::from_secs(1))
                .is_some()
        );
    }
}
