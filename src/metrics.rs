use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;

pub struct Metrics {
    pub start_time: Instant,
    pub requests: HashMap<String, AtomicU64>,
    pub failovers: AtomicU64,
    pub faults_injected: AtomicU64,
    pub last_status: HashMap<String, AtomicU64>,
}

impl Metrics {
    pub fn new(backend_names: &[String]) -> Arc<Self> {
        let mut requests = HashMap::new();
        let mut last_status = HashMap::new();
        for name in backend_names {
            requests.insert(name.clone(), AtomicU64::new(0));
            last_status.insert(name.clone(), AtomicU64::new(0));
        }
        Arc::new(Self {
            start_time: Instant::now(),
            requests,
            failovers: AtomicU64::new(0),
            faults_injected: AtomicU64::new(0),
            last_status,
        })
    }

    pub fn inc_requests(&self, backend: &str) {
        if let Some(c) = self.requests.get(backend) {
            c.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn inc_failovers(&self) {
        self.failovers.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_faults(&self) {
        self.faults_injected.fetch_add(1, Ordering::Relaxed);
    }

    pub fn set_last_status(&self, backend: &str, status: u16) {
        if let Some(c) = self.last_status.get(backend) {
            c.store(status as u64, Ordering::Relaxed);
        }
    }

    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            uptime_seconds: self.start_time.elapsed().as_secs(),
            requests: self.requests.iter()
                .map(|(k, v)| (k.clone(), v.load(Ordering::Relaxed)))
                .collect(),
            failovers: self.failovers.load(Ordering::Relaxed),
            faults_injected: self.faults_injected.load(Ordering::Relaxed),
            last_status: self.last_status.iter()
                .map(|(k, v)| (k.clone(), v.load(Ordering::Relaxed) as u16))
                .collect(),
        }
    }
}

#[derive(Serialize)]
pub struct MetricsSnapshot {
    pub uptime_seconds: u64,
    pub requests: HashMap<String, u64>,
    pub failovers: u64,
    pub faults_injected: u64,
    pub last_status: HashMap<String, u16>,
}
