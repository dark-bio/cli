// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Transfer rates and per-step estimates for terminal progress.

use darkbio_connect::schema::SlotUploadProcessResponse;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

const MIB: f64 = 1024.0 * 1024.0;
const RATE_WINDOW: Duration = Duration::from_secs(10);
const WARMUP: Duration = Duration::from_secs(1);
const HUMAN_REPORT_INTERVAL: Duration = Duration::from_secs(1);
const REPORT_INTERVAL: Duration = Duration::from_secs(5);

/// Samples only acknowledged bytes, starting with the first upload report so
/// cloud setup and approval do not enter the rate estimate.
pub(super) struct Transfer {
    rate: Rate,
    report: Report,
}

impl Transfer {
    pub(super) fn new(human: bool) -> Self {
        Self {
            rate: Rate::default(),
            report: Report::new(human),
        }
    }

    pub(super) fn update(&mut self, uploaded: u64, total: u64) -> Option<String> {
        self.update_at(uploaded, total, Instant::now())
    }

    fn update_at(&mut self, uploaded: u64, total: u64, now: Instant) -> Option<String> {
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
        Some(format!(
            "Uploading: {percent}% ({:.1}/{:.1} MiB) | {speed} | ETA {}",
            uploaded as f64 / MIB,
            total as f64 / MIB,
            eta(total.saturating_sub(uploaded), rate),
        ))
    }
}

/// Progress percentages belong to individual steps. Device timestamps identify
/// a restarted step; elapsed time is measured by the host's monotonic clock.
pub(super) struct Processing {
    phase: Option<(u64, u64, u64)>, // Processing start, step number and step start
    rate: Rate,
    report: Report,
}

impl Processing {
    pub(super) fn new(human: bool) -> Self {
        Self {
            phase: None,
            rate: Rate::default(),
            report: Report::new(human),
        }
    }

    pub(super) fn update(&mut self, status: &SlotUploadProcessResponse) -> Option<String> {
        self.update_at(status, Instant::now())
    }

    fn update_at(&mut self, status: &SlotUploadProcessResponse, now: Instant) -> Option<String> {
        let phase = (status.proc_start, status.phase_in, status.phase_start);
        if self.phase != Some(phase) {
            self.phase = Some(phase);
            self.rate = Rate::default();
            self.report.last = None;
        }
        let rate = self.rate.sample(status.phase_progress, now);
        let percent = percent(status.phase_progress, 10_000);
        if !self.report.due(percent, now) {
            return None;
        }
        let name = status
            .phase_in
            .checked_sub(1)
            .and_then(|index| usize::try_from(index).ok())
            .and_then(|index| status.phases.get(index).map(|phase| &phase.name))
            .map(String::as_str)
            .unwrap_or("Processing");
        Some(format!(
            "Processing [{}/{}] {name}: {percent}% | step ETA {}",
            status.phase_in,
            status.phases.len(),
            eta(10_000_u64.saturating_sub(status.phase_progress), rate),
        ))
    }
}

/// Keep the sample immediately before the rolling window's boundary so a
/// stalled or sparse counter does not retain an old, optimistic speed.
#[derive(Default)]
struct Rate {
    samples: VecDeque<(Instant, u64)>,
}

impl Rate {
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
    human: bool,
    last: Option<(Instant, u64)>,
}

impl Report {
    fn new(human: bool) -> Self {
        Self { human, last: None }
    }

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

fn percent(done: u64, total: u64) -> u64 {
    if total == 0 {
        return 0;
    }
    (u128::from(done.min(total)) * 100 / u128::from(total)) as u64
}

fn speed(bytes: f64) -> String {
    if bytes >= MIB {
        format!("{:.1} MiB/s", bytes / MIB)
    } else if bytes >= 1024.0 {
        format!("{:.1} KiB/s", bytes / 1024.0)
    } else {
        format!("{bytes:.0} B/s")
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    /// The initial bytes were accepted before sampling began. Counting them as
    /// newly transferred would inflate speed and shorten the ETA.
    #[test]
    fn test_transfer_estimate() {
        let start = Instant::now();
        let mut transfer = Transfer::new(false);
        let mib = 1024 * 1024;
        let first = transfer.update_at(25 * mib, 100 * mib, start).unwrap();
        assert!(first.contains("speed estimating... | ETA estimating..."));
        let next = transfer
            .update_at(50 * mib, 100 * mib, start + Duration::from_secs(5))
            .unwrap();
        assert_eq!(
            next,
            "Uploading: 50% (50.0/100.0 MiB) | 5.0 MiB/s | ETA ~10s"
        );
        let done = transfer
            .update_at(100 * mib, 100 * mib, start + Duration::from_secs(10))
            .unwrap();
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

    /// Every step gets its own estimate, even if the first observation arrives
    /// partway through it. A restarted step must not inherit its previous rate.
    #[test]
    fn test_step_estimate() {
        let start = Instant::now();
        let mut processing = Processing::new(false);
        assert!(
            processing
                .update_at(&status(1, 1000), start)
                .unwrap()
                .ends_with("step ETA estimating...")
        );
        assert_eq!(
            processing
                .update_at(&status(1, 3000), start + Duration::from_secs(5))
                .unwrap(),
            "Processing [1/2] Validate: 30% | step ETA ~18s"
        );
        assert_eq!(
            processing
                .update_at(&status(2, 4000), start + Duration::from_secs(6))
                .unwrap(),
            "Processing [2/2] Index: 40% | step ETA estimating..."
        );
        assert_eq!(
            processing
                .update_at(&status(2, 5000), start + Duration::from_secs(11))
                .unwrap(),
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
                .ends_with("step ETA 0s")
        );
    }

    /// Recent stalls age out a formerly fast rate. A regressing counter starts
    /// over instead of underflowing or producing an estimate from another run.
    #[test]
    fn test_rate_stall_and_reset() {
        let start = Instant::now();
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
        let start = Instant::now();
        let mut report = Report::new(false);
        assert!(report.due(91, start));
        assert!(!report.due(92, start + Duration::from_secs(1)));
        assert!(report.due(92, start + REPORT_INTERVAL));
        assert!(report.due(100, start + REPORT_INTERVAL + Duration::from_millis(1)));
        assert!(!report.due(100, start + REPORT_INTERVAL + Duration::from_millis(2)));
    }

    #[test]
    fn test_human_transfer_cadence() {
        let start = Instant::now();
        let mut transfer = Transfer::new(true);
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
        let start = Instant::now();
        let mut processing = Processing::new(true);
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
