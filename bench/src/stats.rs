use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, RwLock,
    },
    time::Instant,
};

use serde::Serialize;

/// Live counters for one validator shard during a load run.
#[derive(Default)]
pub struct ShardStats {
    pub submitted: AtomicU64,
    pub accepted: AtomicU64,
    pub errors: AtomicU64,
    /// Executed count scraped from the validator's Prometheus endpoint.
    pub executed: AtomicU64,
    /// End-to-end latency samples (ms) from the probe transactions.
    pub latency_ms: Mutex<Vec<f64>>,
}

impl ShardStats {
    pub fn accepted(&self) -> u64 {
        self.accepted.load(Ordering::Relaxed)
    }
    pub fn submitted(&self) -> u64 {
        self.submitted.load(Ordering::Relaxed)
    }
    pub fn errors(&self) -> u64 {
        self.errors.load(Ordering::Relaxed)
    }
    pub fn executed(&self) -> u64 {
        self.executed.load(Ordering::Relaxed)
    }
    pub fn record_latency(&self, ms: f64) {
        if let Ok(mut samples) = self.latency_ms.lock() {
            samples.push(ms);
        }
    }
    pub fn latency_samples(&self) -> Vec<f64> {
        self.latency_ms.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

/// (avg, p99) over a set of latency samples, if any.
pub fn latency_summary(samples: &[f64]) -> Option<(f64, f64)> {
    if samples.is_empty() {
        return None;
    }
    let avg = samples.iter().sum::<f64>() / samples.len() as f64;
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let p99 = sorted[((sorted.len() - 1) as f64 * 0.99) as usize];
    Some((avg, p99))
}

#[derive(Clone, Serialize)]
pub struct HistoryPoint {
    pub t: f64,
    pub aggregate_tps: f64,
    pub per_shard: Vec<f64>,
}

#[derive(Clone, Serialize)]
pub struct SweepPoint {
    pub validators: usize,
    pub tps: f64,
    pub per_node_tps: f64,
    pub accepted: u64,
    pub executed: Option<u64>,
    pub verified_pairs: usize,
    pub failed_pairs: usize,
    pub changed_pairs: usize,
    pub latency_avg_ms: Option<f64>,
    pub latency_p99_ms: Option<f64>,
}

#[derive(Clone, Serialize, Default)]
pub struct RunInfo {
    pub phase: String,
    pub detail: String,
    pub validators: usize,
    pub users_per_shard: usize,
    pub total_delegated_accounts: usize,
}

/// Shared state between the benchmark driver and the dashboard.
pub struct AppState {
    pub info: RwLock<RunInfo>,
    pub shards: RwLock<Vec<Arc<ShardStats>>>,
    pub history: Mutex<Vec<HistoryPoint>>,
    pub sweep: Mutex<Vec<SweepPoint>>,
    pub started: Instant,
}

impl AppState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            info: RwLock::new(RunInfo::default()),
            shards: RwLock::new(Vec::new()),
            history: Mutex::new(Vec::new()),
            sweep: Mutex::new(Vec::new()),
            started: Instant::now(),
        })
    }

    pub fn set_phase(&self, phase: &str, detail: &str) {
        if let Ok(mut info) = self.info.write() {
            info.phase = phase.to_string();
            info.detail = detail.to_string();
        }
        println!("[phase] {phase} {detail}");
    }

    pub fn set_run(&self, validators: usize, users_per_shard: usize, total_accounts: usize) {
        if let Ok(mut info) = self.info.write() {
            info.validators = validators;
            info.users_per_shard = users_per_shard;
            info.total_delegated_accounts = total_accounts;
        }
    }

    pub fn reset_shards(&self, n: usize) -> Vec<Arc<ShardStats>> {
        let shards: Vec<Arc<ShardStats>> =
            (0..n).map(|_| Arc::new(ShardStats::default())).collect();
        if let Ok(mut s) = self.shards.write() {
            *s = shards.clone();
        }
        if let Ok(mut h) = self.history.lock() {
            h.clear();
        }
        shards
    }

    pub fn push_sweep(&self, point: SweepPoint) {
        if let Ok(mut s) = self.sweep.lock() {
            s.push(point);
        }
    }
}
