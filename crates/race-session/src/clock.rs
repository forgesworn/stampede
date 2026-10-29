//! Host clock sync (ping/pong), as Twang Thang does it: rounds of a few pings,
//! the lowest-RTT sample of a round gives `host time = local + offset`.

use crate::proto::ToHost;

/// A round of pings every this often.
pub const SYNC_EVERY_MS: u64 = 2500;
/// Pings per round.
pub const SYNC_PINGS: u32 = 5;
const SYNC_PING_GAP_MS: u64 = 40;
const SYNC_ROUND_TIMEOUT_MS: u64 = 2000;
/// Pongs implying a clock offset (or RTT) beyond this are ignored.
const MAX_CLOCK_OFFSET_MS: u64 = 86_400_000;

/// Host-clock estimate from ping/pong rounds.
#[derive(Clone, Debug, Default)]
pub struct ClockSync {
    offset: Option<i64>,
    best_rtt: Option<u64>,
    round_started: Option<u64>,
    pings_sent: u32,
    next_id: u32,
    round_first_id: u32,
    samples: Vec<(u64, i64)>,
    last_round: Option<u64>,
}

impl ClockSync {
    /// host time ~= local + offset.
    pub fn offset_ms(&self) -> Option<i64> {
        self.offset
    }

    /// The best round-trip time of the latest round.
    pub fn rtt_ms(&self) -> Option<u64> {
        self.best_rtt
    }

    /// Estimated host clock, if synced.
    pub fn host_now(&self, local: u64) -> Option<u64> {
        self.offset.map(|o| (local as i64).saturating_add(o).max(0) as u64)
    }

    /// Recovery barrier rebase: host time at `local` becomes `host_ms` (the
    /// frozen authority time C), so the outage is not counted. Any open round
    /// is aborted, since its samples would carry the outage, and late pongs
    /// for it are dropped (their ids are below the next round's first id).
    /// The next `tick` starts a fresh round. An unsynced clock stays unsynced.
    pub fn rebase(&mut self, local: u64, host_ms: u64) {
        if self.offset.is_some() {
            self.offset = Some((host_ms as i64).saturating_sub(local as i64));
        }
        self.restart();
    }

    /// Start a fresh round at the next `tick`, keeping the offset until it
    /// completes. An open round is aborted (its samples may straddle a held
    /// host clock) and its late pongs are dropped.
    pub fn restart(&mut self) {
        self.round_started = None;
        self.pings_sent = 0;
        self.samples.clear();
        self.round_first_id = self.next_id;
        self.last_round = None;
    }

    /// Keep host time at `local` within `[low, high]` by moving the offset
    /// (an open round and its samples are kept). Returns the change in ms:
    /// negative when the estimate was above `high`, positive when below
    /// `low`, 0 when already inside or unsynced.
    pub fn bound(&mut self, local: u64, low: u64, high: u64) -> i64 {
        let Some(host) = self.host_now(local) else {
            return 0;
        };
        let target = host.clamp(low, high.max(low));
        let change = target as i64 - host as i64;
        if let Some(offset) = self.offset.as_mut() {
            *offset = offset.saturating_add(change);
        }
        change
    }

    /// Whether a ping round is open (pings sent, samples pending).
    pub fn round_open(&self) -> bool {
        self.round_started.is_some()
    }

    /// The first ping id of the open round, read-only (tests, diagnostics).
    pub fn round_first_id(&self) -> Option<u32> {
        self.round_started.map(|_| self.round_first_id)
    }

    /// Pings to send now.
    pub fn tick(&mut self, local: u64) -> Vec<ToHost> {
        let mut out = Vec::new();
        match self.round_started {
            None => {
                if self.last_round.is_none_or(|t| local >= t + SYNC_EVERY_MS) {
                    self.round_started = Some(local);
                    self.pings_sent = 0;
                    self.samples.clear();
                    self.round_first_id = self.next_id;
                }
            }
            Some(start) => {
                if local >= start + SYNC_ROUND_TIMEOUT_MS || self.samples.len() as u32 >= SYNC_PINGS {
                    self.finish_round(local);
                    return out;
                }
            }
        }
        if let Some(start) = self.round_started {
            while self.pings_sent < SYNC_PINGS && local >= start + self.pings_sent as u64 * SYNC_PING_GAP_MS {
                out.push(ToHost::Ping { id: self.next_id, client_t: local });
                self.next_id += 1;
                self.pings_sent += 1;
            }
        }
        out
    }

    pub fn on_pong(&mut self, id: u32, client_t: u64, host_t: u64, local: u64) {
        if self.round_started.is_none() || id < self.round_first_id || local < client_t {
            return;
        }
        let rtt = local - client_t;
        // The host stamped host_t about half an RTT after we sent. i128: host_t
        // is the host's to choose; an offset over a day is nonsense, not a clock.
        let offset = host_t as i128 - (client_t as i128 + rtt as i128 / 2);
        if offset.abs() > MAX_CLOCK_OFFSET_MS as i128 || rtt > MAX_CLOCK_OFFSET_MS {
            return;
        }
        self.samples.push((rtt, offset as i64));
        if self.samples.len() as u32 >= SYNC_PINGS {
            self.finish_round(local);
        }
    }

    fn finish_round(&mut self, local: u64) {
        if let Some(&(rtt, offset)) = self.samples.iter().min_by_key(|(rtt, _)| *rtt) {
            self.offset = Some(offset);
            self.best_rtt = Some(rtt);
        }
        self.round_started = None;
        self.last_round = Some(local);
    }
}
