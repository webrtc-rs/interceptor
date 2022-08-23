use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

mod interceptor;

pub use self::interceptor::StatsInterceptor;

pub fn make_stats_interceptor(id: &str) -> Arc<StatsInterceptor> {
    Arc::new(StatsInterceptor::new(id.to_owned()))
}

#[derive(Debug, Default)]
/// Records stats about a given RTP stream.
pub struct RTPStats {
    /// Packets sent or received
    packets: Arc<AtomicU64>,

    /// Payload bytes sent or received
    payload_bytes: Arc<AtomicU64>,

    /// Header bytes sent or received
    header_bytes: Arc<AtomicU64>,

    /// A wall clock timestamp for when the last packet was sent or recieved encoded as milliseconds since
    /// [`SystemTime::UNIX_EPOCH`].
    last_packet_timestamp: Arc<AtomicU64>,
}

impl RTPStats {
    pub fn update(&self, header_bytes: u64, payload_bytes: u64, packets: u64) {
        let now = SystemTime::now();

        self.header_bytes.fetch_add(header_bytes, Ordering::SeqCst);
        self.payload_bytes
            .fetch_add(payload_bytes, Ordering::SeqCst);
        self.packets.fetch_add(packets, Ordering::SeqCst);

        if let Ok(duration) = now.duration_since(SystemTime::UNIX_EPOCH) {
            let millis = duration.as_millis();
            // NB: We truncate 128bits to 64 bits here, but even at 64 bits we have ~500k years
            // before this becomes a problem, then it can be someone else's problem.
            self.last_packet_timestamp
                .store(millis as u64, Ordering::SeqCst);
        } else {
            log::warn!("SystemTime::now was before SystemTime::UNIX_EPOCH");
        }
    }

    pub fn reader(&self) -> RTPStatsReader {
        RTPStatsReader {
            packets: self.packets.clone(),
            payload_bytes: self.payload_bytes.clone(),
            header_bytes: self.header_bytes.clone(),
            last_packet_timestamp: self.last_packet_timestamp.clone(),
        }
    }
}

#[derive(Clone, Debug, Default)]
/// Reader half of RTPStats.
pub struct RTPStatsReader {
    packets: Arc<AtomicU64>,
    payload_bytes: Arc<AtomicU64>,
    header_bytes: Arc<AtomicU64>,

    last_packet_timestamp: Arc<AtomicU64>,
}

impl RTPStatsReader {
    /// Get packets sent or received.
    pub fn packets(&self) -> u64 {
        self.packets.load(Ordering::SeqCst)
    }

    /// Get payload bytes sent or received.
    pub fn header_bytes(&self) -> u64 {
        self.header_bytes.load(Ordering::SeqCst)
    }

    /// Get header bytes sent or received.
    pub fn payload_bytes(&self) -> u64 {
        self.payload_bytes.load(Ordering::SeqCst)
    }

    pub fn last_packet_timestamp(&self) -> SystemTime {
        let millis = self.last_packet_timestamp.load(Ordering::SeqCst);

        SystemTime::UNIX_EPOCH + Duration::from_millis(millis)
    }
}

#[derive(Debug, Default, Clone)]
pub struct RTCPStats {
    rtt_ms: Arc<AtomicU64>,
    loss: Arc<AtomicU8>,
    fir_count: Arc<AtomicU64>,
    pli_count: Arc<AtomicU64>,
    nack_count: Arc<AtomicU64>,
}

impl RTCPStats {
    fn get_reader(&self) -> RTCPStatsReader {
        RTCPStatsReader {
            rtt_ms: self.rtt_ms.clone(),
            loss: self.loss.clone(),
            fir_count: self.fir_count.clone(),
            pli_count: self.pli_count.clone(),
            nack_count: self.nack_count.clone(),
        }
    }

    fn write_rtt_ms(&self, rtt: f64) {
        store_f64_in_u64(&self.rtt_ms, rtt);
    }

    fn write_loss(&self, loss: u8) {
        self.loss.store(loss, Ordering::SeqCst);
    }

    fn write_fir(&self, fir_count: u64) {
        self.fir_count.fetch_add(fir_count, Ordering::SeqCst);
    }

    fn write_pli(&self, pli_count: u64) {
        self.pli_count.fetch_add(pli_count, Ordering::SeqCst);
    }

    fn write_nack(&self, nack_count: u64) {
        self.nack_count.fetch_add(nack_count, Ordering::SeqCst);
    }
}
#[derive(Clone, Debug, Default)]
/// Reader half of RTCPStats.
pub struct RTCPStatsReader {
    rtt_ms: Arc<AtomicU64>,
    loss: Arc<AtomicU8>,
    fir_count: Arc<AtomicU64>,
    pli_count: Arc<AtomicU64>,
    nack_count: Arc<AtomicU64>,
}

impl RTCPStatsReader {
    pub fn rtt_ms(&self) -> f64 {
        read_f64_stored_as_u64(&self.rtt_ms)
    }

    pub fn fir_count(&self) -> u64 {
        self.fir_count.load(Ordering::SeqCst)
    }

    pub fn pli_count(&self) -> u64 {
        self.pli_count.load(Ordering::SeqCst)
    }

    pub fn nack_count(&self) -> u64 {
        self.nack_count.load(Ordering::SeqCst)
    }
}

// Safety guarantee used to store f64 values in AtomicU64.
const _: [(); core::mem::size_of::<u64>()] = [(); core::mem::size_of::<f64>()];

/// Stores an f64 in an atomic u64.
#[inline(always)]
pub fn store_f64_in_u64(container: &AtomicU64, value: f64) {
    let as_u64: u64 = value.to_bits();

    container.store(as_u64, Ordering::SeqCst);
}

/// Read an f64 stored in an atomic u64.
#[inline(always)]
pub fn read_f64_stored_as_u64(container: &AtomicU64) -> f64 {
    let value = container.load(Ordering::SeqCst);

    f64::from_bits(value)
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_rtp_stats() {
        let stats: RTPStats = Default::default();
        let reader = stats.reader();
        assert_eq!(
            (
                reader.header_bytes(),
                reader.payload_bytes(),
                reader.packets()
            ),
            (0, 0, 0),
        );

        stats.update(24, 960, 1);

        assert_eq!(
            (
                reader.header_bytes(),
                reader.payload_bytes(),
                reader.packets()
            ),
            (24, 960, 1),
        );
    }

    #[test]
    fn test_rtp_stats_send_sync() {
        fn test_send_sync<T: Send + Sync>() {}
        test_send_sync::<RTPStats>();
    }
}
