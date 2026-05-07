use std::{
    sync::{Mutex, OnceLock},
    time::Instant,
};

static GLOBAL_BLOCK_MONITOR: OnceLock<Mutex<BlockPerformanceMonitor>> = OnceLock::new();

fn global_monitor() -> &'static Mutex<BlockPerformanceMonitor> {
    GLOBAL_BLOCK_MONITOR.get_or_init(|| Mutex::new(BlockPerformanceMonitor::default()))
}

#[derive(Debug)]
pub struct BlockPerformanceMonitor {
    block_started_at: Option<Instant>,
    current: Option<TrackedBlock>,
}

impl Default for BlockPerformanceMonitor {
    fn default() -> Self {
        Self {
            block_started_at: Some(Instant::now()),
            current: None,
        }
    }
}

#[derive(Clone, Debug)]
struct TrackedBlock {
    height: u64,
    started_at: Instant,
    last_step_at: Instant,
    step_durations_ms: StepDurations,
}

impl TrackedBlock {
    fn new(height: u64, started_at: Instant) -> Self {
        Self {
            height,
            started_at,
            last_step_at: started_at,
            step_durations_ms: StepDurations::default(),
        }
    }

    fn into_report(self, total_started_at: Instant, finished_at: Instant) -> BlockPerformanceReport {
        BlockPerformanceReport {
            height: self.height,
            total_ms: finished_at
                .saturating_duration_since(total_started_at)
                .as_secs_f64()
                * 1000.0,
            step_durations_ms: self.step_durations_ms,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct StepDurations {
    pub new_height_ms: f64,
    pub new_round_ms: f64,
    pub propose_ms: f64,
    pub prevote_ms: f64,
    pub precommit_ms: f64,
    pub commit_ms: f64,
}

#[derive(Clone, Debug)]
pub struct BlockPerformanceReport {
    pub height: u64,
    pub total_ms: f64,
    pub step_durations_ms: StepDurations,
}

impl BlockPerformanceReport {
    pub fn stage_latency_ms(&self, stage_name: &str) -> f64 {
        match normalize_step_name(stage_name).as_deref() {
            Some("new_height") => self.step_durations_ms.new_height_ms,
            Some("new_round") => self.step_durations_ms.new_round_ms,
            Some("propose") => self.step_durations_ms.propose_ms,
            Some("prevote") => self.step_durations_ms.prevote_ms,
            Some("precommit") => self.step_durations_ms.precommit_ms,
            Some("commit") => self.step_durations_ms.commit_ms,
            _ => 0.0,
        }
    }

    pub fn stage_summary(&self) -> String {
        [
            ("new_height", self.step_durations_ms.new_height_ms),
            ("new_round", self.step_durations_ms.new_round_ms),
            ("propose", self.step_durations_ms.propose_ms),
            ("prevote", self.step_durations_ms.prevote_ms),
            ("precommit", self.step_durations_ms.precommit_ms),
            ("commit", self.step_durations_ms.commit_ms),
        ]
        .into_iter()
        .filter(|(_, latency_ms)| *latency_ms > 0.0)
        .map(|(name, latency_ms)| format!("{}:{:.3}", name, latency_ms))
        .collect::<Vec<_>>()
        .join(",")
    }

    pub fn print_summary(&self) {
        let new_height_ms = self.stage_latency_ms("new_height");
        let new_round_ms = self.stage_latency_ms("new_round");
        let propose_ms = self.stage_latency_ms("propose");
        let prevote_ms = self.stage_latency_ms("prevote");
        let precommit_ms = self.stage_latency_ms("precommit");
        let commit_ms = self.stage_latency_ms("commit");
        println!(
            "msg=consensus height={} new_height={:.3} new_round={:.3} propose={:.3} prevote={:.3} precommit={:.3} commit={:.3} total={:.3}",
            self.height,
            new_height_ms,
            new_round_ms,
            propose_ms,
            prevote_ms,
            precommit_ms,
            commit_ms,
            self.total_ms,
        );
    }
}

fn normalize_step_name(step_name: &str) -> Option<&'static str> {
    let normalized = step_name.trim().to_ascii_lowercase().replace([' ', '-'], "_");
    match normalized.as_str() {
        "newheight" | "new_height" => Some("new_height"),
        "newround" | "new_round" => Some("new_round"),
        "propose" => Some("propose"),
        "prevote" => Some("prevote"),
        "precommit" => Some("precommit"),
        "commit" => Some("commit"),
        _ => None,
    }
}

impl StepDurations {
    fn add_step(&mut self, step_name: &str, duration_ms: f64) -> bool {
        match normalize_step_name(step_name) {
            Some("new_height") => self.new_height_ms += duration_ms,
            Some("new_round") => self.new_round_ms += duration_ms,
            Some("propose") => self.propose_ms += duration_ms,
            Some("prevote") => self.prevote_ms += duration_ms,
            Some("precommit") => self.precommit_ms += duration_ms,
            Some("commit") => self.commit_ms += duration_ms,
            _ => return false,
        }
        true
    }
}

impl BlockPerformanceMonitor {
    pub fn record_step(
        &mut self,
        height: u64,
        step_name: impl AsRef<str>,
        at: Instant,
    ) -> bool {
        self.block_started_at.get_or_insert(at);
        let Some(current) = self.current.as_mut() else {
            self.current = Some(TrackedBlock::new(height, at));
            return false;
        };
        if current.height != height {
            return false;
        }
        let elapsed_ms = at
            .saturating_duration_since(current.last_step_at)
            .as_secs_f64()
            * 1000.0;
        current.last_step_at = at;
        current
            .step_durations_ms
            .add_step(step_name.as_ref(), elapsed_ms)
    }

    pub fn flush_block(&mut self, height: u64, finished_at: Instant) {
        // block total time
        let started_at = self.block_started_at.unwrap_or(finished_at);
        let total_ms = finished_at
            .saturating_duration_since(started_at)
            .as_secs_f64()
            * 1000.0;
        
        // step time
        let step_durations_ms = self
            .current
            .take()
            .filter(|current| current.height == height)
            .map(|current| current.step_durations_ms)
            .unwrap_or_default();

        BlockPerformanceReport {
            height,
            total_ms,
            step_durations_ms,
        }
        .print_summary();

        self.block_started_at = Some(finished_at);
        self.current = Some(TrackedBlock::new(height.saturating_add(1), finished_at));
    }

    pub fn current_block_height(&self) -> Option<u64> {
        self.current.as_ref().map(|current| current.height)
    }
}

pub fn record_step(height: u64, step_name: impl AsRef<str>) -> bool {
    global_monitor()
        .lock()
        .unwrap()
        .record_step(height, step_name, Instant::now())
}

pub fn flush_block(height: u64) {
    global_monitor()
        .lock()
        .unwrap()
        .flush_block(height, Instant::now())
}

pub fn current_block_height() -> Option<u64> {
    global_monitor().lock().unwrap().current_block_height()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::BlockPerformanceMonitor;

    fn monitor_with_start(started_at: Option<std::time::Instant>) -> BlockPerformanceMonitor {
        BlockPerformanceMonitor {
            block_started_at: started_at,
            current: None,
        }
    }

    #[test]
    fn anchors_next_block_on_first_commit() {
        let base = std::time::Instant::now();
        let mut monitor = monitor_with_start(None);

        monitor.flush_block(10, base);
        assert_eq!(monitor.current_block_height(), Some(11));
    }

    #[test]
    fn measures_block_from_previous_commit_to_current_commit() {
        let base = std::time::Instant::now();
        let mut monitor = monitor_with_start(Some(base));

        monitor.flush_block(10, base + Duration::from_millis(1));
        assert!(monitor.record_step(
            11,
            "new_height",
            base + Duration::from_millis(24)
        ));
        monitor.flush_block(11, base + Duration::from_millis(25));
        assert_eq!(monitor.current_block_height(), Some(12));
    }

    #[test]
    fn ignores_stage_updates_for_other_heights() {
        let base = std::time::Instant::now();
        let mut monitor = monitor_with_start(None);

        monitor.flush_block(3, base);
        assert_eq!(monitor.current_block_height(), Some(4));
        assert!(!monitor.record_step(5, "vote", base + Duration::from_millis(1)));
        monitor.flush_block(4, base + Duration::from_millis(2));
        assert_eq!(monitor.current_block_height(), Some(5));
    }

    #[test]
    fn total_duration_survives_missing_step_alignment() {
        let base = std::time::Instant::now();
        let mut monitor = monitor_with_start(Some(base));

        monitor.flush_block(10, base + Duration::from_millis(1));
        assert!(!monitor.record_step(99, "new_round", base + Duration::from_millis(5)));
        monitor.flush_block(11, base + Duration::from_millis(25));
        assert_eq!(monitor.current_block_height(), Some(12));
    }

    #[test]
    fn resyncs_tracker_when_commit_height_jumps() {
        let base = std::time::Instant::now();
        let mut monitor = monitor_with_start(None);

        monitor.flush_block(10, base);
        monitor.flush_block(15, base + Duration::from_millis(5));
        assert_eq!(monitor.current_block_height(), Some(16));
    }

    #[test]
    fn aggregates_same_stage_name_in_summary() {
        let base = std::time::Instant::now();
        let mut monitor = monitor_with_start(None);

        monitor.flush_block(10, base);
        assert!(monitor.record_step(11, "new_round", base + Duration::from_millis(5)));
        assert!(monitor.record_step(11, "propose", base + Duration::from_millis(8)));
        assert!(monitor.record_step(11, "new_round", base + Duration::from_millis(12)));

        let report = monitor
            .current
            .take()
            .expect("tracked block")
            .into_report(base, base + Duration::from_millis(15));

        assert_eq!(report.stage_summary(), "new_round:9.000,propose:3.000");
    }
}
