//! Processing-load monitor.

/// Sliding average of per-block processing time versus the block duration.
/// Fixed-size ring; with fewer samples than the window it averages what it has.
pub struct LoadMonitor {
    block_us: f64,
    threshold: f64,
    ring: Vec<f64>,
    pos: usize,
    len: usize,
    sum: f64,
}

impl LoadMonitor {
    pub fn new(block_us: f64, window_blocks: usize, threshold: f64) -> Self {
        Self {
            block_us,
            threshold,
            ring: vec![0.0; window_blocks.max(1)],
            pos: 0,
            len: 0,
            sum: 0.0,
        }
    }

    pub fn record(&mut self, elapsed_us: f64) {
        let elapsed = if elapsed_us.is_finite() {
            elapsed_us
        } else {
            0.0
        };
        if self.len == self.ring.len() {
            self.sum -= self.ring[self.pos];
        } else {
            self.len += 1;
        }
        self.ring[self.pos] = elapsed;
        self.sum += elapsed;
        self.pos += 1;
        if self.pos == self.ring.len() {
            self.pos = 0;
            // Re-sum once per lap so floating-point drift cannot accumulate.
            self.sum = self.ring[..self.len].iter().sum();
        }
    }

    /// Mean processing time as a fraction of the block duration (0 with no samples).
    pub fn ratio(&self) -> f64 {
        if self.len == 0 || self.block_us <= 0.0 {
            return 0.0;
        }
        self.sum / self.len as f64 / self.block_us
    }

    pub fn overloaded(&self) -> bool {
        self.ratio() > self.threshold
    }

    pub fn reset(&mut self) {
        self.ring.fill(0.0);
        self.pos = 0;
        self.len = 0;
        self.sum = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_monitor_trips_above_70_percent() {
        let mut m = LoadMonitor::new(2902.0, 344, 0.70);
        assert!(!m.overloaded());
        for _ in 0..344 {
            m.record(2100.0);
        }
        assert!(m.overloaded());
        assert!((m.ratio() - 2100.0 / 2902.0).abs() < 1e-9);
        m.reset();
        assert!(!m.overloaded());
        assert_eq!(m.ratio(), 0.0);
        for _ in 0..344 {
            m.record(1900.0);
        }
        assert!(!m.overloaded());
    }

    #[test]
    fn load_monitor_slides_and_averages_partial_window() {
        let mut m = LoadMonitor::new(1000.0, 4, 0.5);
        m.record(800.0);
        assert!((m.ratio() - 0.8).abs() < 1e-12); // averaged over 1 sample
        for _ in 0..4 {
            m.record(100.0);
        }
        assert!((m.ratio() - 0.1).abs() < 1e-12); // old sample slid out
        assert!(!m.overloaded());
    }
}
