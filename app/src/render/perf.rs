//! Frame-time logger: every [`REPORT_SECS`] seconds one `info` line with the
//! frame rate and the average and 95th-percentile frame time, both the
//! interval between presented frames and the CPU time spent in the frame
//! (logcat tag `stampede`, desktop: stderr).

use std::time::Instant;

/// Seconds between report lines.
pub const REPORT_SECS: f32 = 5.0;

#[derive(Default)]
pub struct FrameStats {
    window_start: Option<Instant>,
    last_frame: Option<Instant>,
    interval_ms: Vec<f32>,
    cpu_ms: Vec<f32>,
    /// Average fps of every finished window (for the exit summary).
    pub fps_windows: Vec<f32>,
}

/// avg and p95 (nearest rank) of `v`, in the same unit; sorts `v`.
fn avg_p95(v: &mut [f32]) -> (f32, f32) {
    if v.is_empty() {
        return (0.0, 0.0);
    }
    let avg = v.iter().sum::<f32>() / v.len() as f32;
    v.sort_by(|a, b| a.total_cmp(b));
    let rank = ((v.len() as f32 * 0.95).ceil() as usize).clamp(1, v.len());
    (avg, v[rank - 1])
}

impl FrameStats {
    /// A frame was presented; `cpu_ms` is the time from the start of the
    /// redraw to the present call returning. Returns the report line when a
    /// window closed (and logs it).
    pub fn frame(&mut self, now: Instant, cpu_ms: f32) -> Option<String> {
        if let Some(prev) = self.last_frame {
            // A gap longer than a second is a pause (background), not a frame.
            let gap = now.duration_since(prev).as_secs_f32();
            if gap < 1.0 {
                self.interval_ms.push(gap * 1000.0);
            }
        }
        self.last_frame = Some(now);
        self.cpu_ms.push(cpu_ms);
        let start = *self.window_start.get_or_insert(now);
        let el = now.duration_since(start).as_secs_f32();
        if el < REPORT_SECS {
            return None;
        }
        let frames = self.cpu_ms.len();
        let fps = frames as f32 / el;
        let (ia, ip) = avg_p95(&mut self.interval_ms);
        let (ca, cp) = avg_p95(&mut self.cpu_ms);
        let line = format!("frames: {frames} in {el:.1}s = {fps:.1} fps | interval avg {ia:.2} ms p95 {ip:.2} ms | cpu avg {ca:.2} ms p95 {cp:.2} ms");
        log::info!("{line}");
        self.fps_windows.push(fps);
        self.interval_ms.clear();
        self.cpu_ms.clear();
        self.window_start = Some(now);
        Some(line)
    }

    /// Start a fresh window (after a pause, so it isn't averaged in).
    pub fn restart(&mut self) {
        self.window_start = None;
        self.last_frame = None;
        self.interval_ms.clear();
        self.cpu_ms.clear();
    }

    /// Mean fps over the finished windows.
    pub fn average_fps(&self) -> Option<f32> {
        (!self.fps_windows.is_empty()).then(|| self.fps_windows.iter().sum::<f32>() / self.fps_windows.len() as f32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn percentile_and_window() {
        let mut v: Vec<f32> = (1..=100).map(|x| x as f32).collect();
        let (a, p) = avg_p95(&mut v);
        assert!((a - 50.5).abs() < 1e-3);
        assert_eq!(p, 95.0);
        assert_eq!(avg_p95(&mut []), (0.0, 0.0));
        let mut s = FrameStats::default();
        let t0 = Instant::now();
        let mut line = None;
        for i in 0..400u64 {
            line = s.frame(t0 + Duration::from_millis(i * 16), 2.0).or(line);
        }
        let line = line.expect("a window closed after 5 s");
        assert!(line.contains("interval avg 16.00 ms"), "{line}");
        assert!(line.contains("cpu avg 2.00 ms"), "{line}");
    }
}
