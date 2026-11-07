use serde::Serialize;
use std::collections::VecDeque;

pub(super) const SAMPLE_INTERVAL_SECONDS: u64 = 60;
const SAMPLE_CAPACITY: usize = 60;

#[derive(Clone, Copy, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct HistorySample {
    pub(super) uptime_ms: u64,
    pub(super) resident_bytes: Option<u64>,
    pub(super) source_bytes: u64,
    pub(super) indexed_buffer_bytes: u64,
    pub(super) retained_snapshots: usize,
    pub(super) open_documents: usize,
    pub(super) indexed_documents: usize,
    pub(super) active_calls: u64,
    pub(super) queued_calls: u64,
    pub(super) active_work: u64,
    pub(super) queued_work: u64,
    pub(super) cancelled_work_still_running: u64,
}

#[derive(Default)]
pub(super) struct History {
    samples: VecDeque<HistorySample>,
    dropped_samples: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct HistorySnapshot {
    pub(super) sample_interval_seconds: u64,
    pub(super) capacity: usize,
    pub(super) dropped_samples: u64,
    pub(super) owned_buffer_bytes: usize,
    pub(super) samples: Vec<HistorySample>,
}

impl History {
    pub(super) fn push(&mut self, sample: HistorySample) {
        if self.samples.capacity() == 0 {
            self.samples.reserve_exact(SAMPLE_CAPACITY);
        }
        if self.samples.len() == SAMPLE_CAPACITY {
            self.samples.pop_front();
            self.dropped_samples = self.dropped_samples.saturating_add(1);
        }
        self.samples.push_back(sample);
    }

    pub(super) fn snapshot(&self) -> HistorySnapshot {
        HistorySnapshot {
            sample_interval_seconds: SAMPLE_INTERVAL_SECONDS,
            capacity: SAMPLE_CAPACITY,
            dropped_samples: self.dropped_samples,
            owned_buffer_bytes: self.samples.capacity() * size_of::<HistorySample>(),
            samples: self.samples.iter().copied().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{History, HistorySample, SAMPLE_CAPACITY, SAMPLE_INTERVAL_SECONDS};

    #[test]
    fn one_hour_history_retires_oldest_samples_without_reordering_recent_samples() {
        let mut history = History::default();
        for minute in 0_u16..73 {
            history.push(HistorySample {
                uptime_ms: u64::from(minute) * SAMPLE_INTERVAL_SECONDS * 1000,
                source_bytes: u64::from(minute),
                ..HistorySample::default()
            });
        }
        let captured = history.snapshot();
        assert_eq!(captured.samples.len(), SAMPLE_CAPACITY);
        assert_eq!(captured.dropped_samples, 13);
        let expected: Vec<_> = (13_u16..73)
            .map(|minute| (u64::from(minute) * SAMPLE_INTERVAL_SECONDS * 1000, u64::from(minute)))
            .collect();
        let actual: Vec<_> = captured.samples.iter()
            .map(|sample| (sample.uptime_ms, sample.source_bytes))
            .collect();
        assert_eq!(actual, expected);
        assert_eq!(history.snapshot().dropped_samples, 13, "exporting history does not add samples");
    }
}
