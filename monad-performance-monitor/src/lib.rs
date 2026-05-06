use std::{
    sync::{Mutex, OnceLock},
    time::Instant,
};

static GLOBAL_BLOCK_MONITOR: OnceLock<Mutex<BlockPerformanceMonitor>> = OnceLock::new();

fn global_monitor() -> &'static Mutex<BlockPerformanceMonitor> {
    GLOBAL_BLOCK_MONITOR.get_or_init(|| Mutex::new(BlockPerformanceMonitor::default()))
}

#[derive(Debug, Default)]
pub struct BlockPerformanceMonitor {
    current: Option<TrackedBlock>,
}

#[derive(Clone, Debug)]
struct TrackedBlock {
    height: u64,
    started_at: Instant,
    stage_timestamps: Vec<StageTimestamp>,
    committed_at: Option<Instant>,
}

impl TrackedBlock {
    fn new(height: u64, started_at: Instant) -> Self {
        Self {
            height,
            started_at,
            stage_timestamps: Vec::new(),
            committed_at: None,
        }
    }

    fn into_report(self, finished_at: Instant) -> BlockPerformanceReport {
        let mut stage_latencies_ms = Vec::with_capacity(self.stage_timestamps.len());
        let mut cursor = self.started_at;
        for stage in self.stage_timestamps {
            let latency_ms = stage
                .at
                .saturating_duration_since(cursor)
                .as_secs_f64()
                * 1000.0;
            stage_latencies_ms.push(StageLatency {
                name: stage.name,
                latency_ms,
            });
            cursor = stage.at;
        }

        BlockPerformanceReport {
            height: self.height,
            total_ms: finished_at
                .saturating_duration_since(self.started_at)
                .as_secs_f64()
                * 1000.0,
            stage_latencies_ms,
            committed_after_start_ms: self.committed_at.map(|committed_at| {
                committed_at
                    .saturating_duration_since(self.started_at)
                    .as_secs_f64()
                    * 1000.0
            }),
        }
    }
}

#[derive(Clone, Debug)]
struct StageTimestamp {
    name: String,
    at: Instant,
}

#[derive(Clone, Debug)]
pub struct StageLatency {
    pub name: String,
    pub latency_ms: f64,
}

#[derive(Clone, Debug)]
pub struct BlockPerformanceReport {
    pub height: u64,
    pub total_ms: f64,
    pub stage_latencies_ms: Vec<StageLatency>,
    pub committed_after_start_ms: Option<f64>,
}

impl BlockPerformanceReport {
    pub fn stage_summary(&self) -> String {
        if self.stage_latencies_ms.is_empty() {
            return "none".to_owned();
        }

        self.stage_latencies_ms
            .iter()
            .map(|stage| format!("{}:{:.3}", stage.name, stage.latency_ms))
            .collect::<Vec<_>>()
            .join(",")
    }

    pub fn print_summary(&self) {
        println!(
            "msg=block_total_duration height={} total_ms={:.3} committed_ms={} stage_durations={}",
            self.height,
            self.total_ms,
            self.committed_after_start_ms
                .map(|ms| format!("{ms:.3}"))
                .unwrap_or_else(|| "none".to_owned()),
            self.stage_summary()
        );
    }
}

impl BlockPerformanceMonitor {
    pub fn begin_block(&mut self, height: u64, started_at: Instant) -> Option<BlockPerformanceReport> {
        if self.current.as_ref().is_some_and(|current| current.height == height) {
            return None;
        }

        let previous = self.current.take().map(|current| current.into_report(started_at));
        self.current = Some(TrackedBlock::new(height, started_at));
        previous
    }

    pub fn record_stage_timestamp(
        &mut self,
        height: u64,
        stage: impl Into<String>,
        at: Instant,
    ) -> bool {
        let Some(current) = self.current.as_mut() else {
            return false;
        };
        if current.height != height {
            return false;
        }

        current.stage_timestamps.push(StageTimestamp {
            name: stage.into(),
            at,
        });
        true
    }

    pub fn mark_block_committed(&mut self, height: u64, at: Instant) -> bool {
        let Some(mut current) = self.current.take() else {
            self.current = Some(TrackedBlock::new(height.saturating_add(1), at));
            return false;
        };
        if current.height != height {
            self.current = Some(TrackedBlock::new(height.saturating_add(1), at));
            return false;
        }

        current.committed_at = Some(at);
        current.into_report(at).print_summary();
        self.current = Some(TrackedBlock::new(height.saturating_add(1), at));
        true
    }

    pub fn current_block_height(&self) -> Option<u64> {
        self.current.as_ref().map(|current| current.height)
    }
}

pub fn begin_block(height: u64) -> Option<BlockPerformanceReport> {
    global_monitor()
        .lock()
        .unwrap()
        .begin_block(height, Instant::now())
}

pub fn record_stage_timestamp(height: u64, stage: impl Into<String>) -> bool {
    global_monitor()
        .lock()
        .unwrap()
        .record_stage_timestamp(height, stage, Instant::now())
}

pub fn mark_block_committed(height: u64) -> bool {
    global_monitor()
        .lock()
        .unwrap()
        .mark_block_committed(height, Instant::now())
}

pub fn current_block_height() -> Option<u64> {
    global_monitor().lock().unwrap().current_block_height()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::BlockPerformanceMonitor;

    #[test]
    fn anchors_next_block_on_first_commit() {
        let base = std::time::Instant::now();
        let mut monitor = BlockPerformanceMonitor::default();

        assert!(!monitor.mark_block_committed(10, base));
        assert_eq!(monitor.current_block_height(), Some(11));
    }

    #[test]
    fn measures_block_from_previous_commit_to_current_commit() {
        let base = std::time::Instant::now();
        let mut monitor = BlockPerformanceMonitor::default();

        assert!(!monitor.mark_block_committed(10, base));
        assert!(monitor.record_stage_timestamp(
            11,
            "finalize_commit_done",
            base + Duration::from_millis(24)
        ));
        assert!(monitor.mark_block_committed(11, base + Duration::from_millis(25)));
        assert_eq!(monitor.current_block_height(), Some(12));
    }

    #[test]
    fn ignores_stage_updates_for_other_heights() {
        let base = std::time::Instant::now();
        let mut monitor = BlockPerformanceMonitor::default();

        assert!(!monitor.mark_block_committed(3, base));
        assert_eq!(monitor.current_block_height(), Some(4));
        assert!(!monitor.record_stage_timestamp(5, "vote", base + Duration::from_millis(1)));
        assert!(monitor.mark_block_committed(4, base + Duration::from_millis(2)));
        assert_eq!(monitor.current_block_height(), Some(5));
    }

    #[test]
    fn resyncs_tracker_when_commit_height_jumps() {
        let base = std::time::Instant::now();
        let mut monitor = BlockPerformanceMonitor::default();

        assert!(!monitor.mark_block_committed(10, base));
        assert!(!monitor.mark_block_committed(15, base + Duration::from_millis(5)));
        assert_eq!(monitor.current_block_height(), Some(16));
    }
}
