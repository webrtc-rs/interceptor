use std::collections::HashMap;
use std::fmt;
use std::hash::Hash;
use std::sync::Arc;
use std::time::SystemTime;

use super::{RTCPStats, RTCPStatsReader, RTPStats, RTPStatsReader};
use async_trait::async_trait;
use rtcp::payload_feedbacks::full_intra_request::FullIntraRequest;
use rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use rtcp::receiver_report::ReceiverReport;
use rtcp::transport_feedbacks::transport_layer_nack::TransportLayerNack;
use rtp::extension::abs_send_time_extension::unix2ntp;
use util::sync::{Mutex, RwLock};
use util::{MarshalSize, Unmarshal};

use crate::error::Result;
use crate::stream_info::StreamInfo;
use crate::{Attributes, Interceptor, RTCPReader, RTCPWriter, RTPReader, RTPWriter};

pub struct StatsInterceptor {
    // Wrapped RTP streams
    recv_streams: Mutex<HashMap<u32, Arc<RTPReadRecorder>>>,
    send_streams: Mutex<HashMap<u32, Arc<RTPWriteRecorder>>>,

    rtcp_recv_recorder: RTCPReadRecorder,
    rtcp_send_recorder: RTCPWriteRecorder,

    id: String,
    now_gen: Option<Arc<dyn Fn() -> SystemTime + Send + Sync>>,
}

impl StatsInterceptor {
    pub fn new(id: String) -> Self {
        Self {
            id,
            recv_streams: Default::default(),
            send_streams: Default::default(),
            rtcp_recv_recorder: Default::default(),
            rtcp_send_recorder: Default::default(),
            now_gen: None,
        }
    }

    fn with_time_gen<F>(id: String, now_gen: F) -> Self
    where
        F: Fn() -> SystemTime + Send + Sync + 'static,
    {
        Self {
            id,
            recv_streams: Default::default(),
            send_streams: Default::default(),
            rtcp_recv_recorder: Default::default(),
            rtcp_send_recorder: Default::default(),
            now_gen: Some(Arc::new(now_gen)),
        }
    }

    /// Create a reader that reads stats for an incoming RTP stream.
    pub fn rtp_recv_stats_reader(&self, ssrc: u32) -> Option<RTPStatsReader> {
        self.rtp_recv_stats_readers([ssrc].iter().copied())
            .into_iter()
            .next()
    }

    /// Create readers that read stats for several incoming RTP streams.
    pub fn rtp_recv_stats_readers(&self, ssrcs: impl Iterator<Item = u32>) -> Vec<RTPStatsReader> {
        let lock = self.recv_streams.lock();

        ssrcs
            .filter_map(|ssrc| lock.get(&ssrc).map(|r| r.reader()))
            .collect()
    }

    /// Create a reader that reads stats for an outgoing RTP stream.
    pub fn rtp_send_stats_reader(&self, ssrc: u32) -> Option<RTPStatsReader> {
        self.rtp_send_stats_readers([ssrc].iter().copied())
            .into_iter()
            .next()
    }

    /// Create readers that read stats for several outgoing RTP streams.
    pub fn rtp_send_stats_readers(&self, ssrcs: impl Iterator<Item = u32>) -> Vec<RTPStatsReader> {
        let lock = self.send_streams.lock();

        ssrcs
            .filter_map(|ssrc| lock.get(&ssrc).map(|r| r.reader()))
            .collect()
    }

    /// Create a reader that reads incoming RTCP stats.
    ///
    /// Note: This reader reflects incoming RTCP stats which means the resulting stats are
    /// typically relevant to an outgoing RTP stream, e.g. [`RTCPStatsReader::nack_count`] reflects
    /// the number of nacks received for the RTP stream identified by the SSRC in question.
    pub fn rtcp_recv_stats_reader(&self, ssrc: u32) -> Option<RTCPStatsReader> {
        self.rtcp_recv_stats_readers([ssrc].iter().copied())
            .into_iter()
            .next()
    }

    /// Create readers that reads incoming RTCP stats.
    pub fn rtcp_recv_stats_readers(
        &self,
        ssrcs: impl Iterator<Item = u32>,
    ) -> Vec<RTCPStatsReader> {
        ssrcs
            .map(|ssrc| self.rtcp_recv_recorder.get_reader(ssrc))
            .collect()
    }

    /// Create a reader that reads outgoing RTCP stats.
    ///
    /// Note: This reader reflects outgiong RTCP stats which means the resulting stats are
    /// typically relevant to an incoming RTP stream, e.g. [`RTCPStatsReader::nack_count`] reflects
    /// the number of nacks sent for the RTP stream identified by the SSRC in question.
    pub fn rtcp_send_stats_reader(&self, ssrc: u32) -> Option<RTCPStatsReader> {
        self.rtcp_send_stats_readers([ssrc].iter().copied())
            .into_iter()
            .next()
    }

    /// Create readers that reads outgoing RTCP stats.
    pub fn rtcp_send_stats_readers(
        &self,
        ssrcs: impl Iterator<Item = u32>,
    ) -> Vec<RTCPStatsReader> {
        ssrcs
            .map(|ssrc| self.rtcp_send_recorder.get_reader(ssrc))
            .collect()
    }
}

#[async_trait]
impl Interceptor for StatsInterceptor {
    /// bind_remote_stream lets you modify any incoming RTP packets. It is called once for per RemoteStream. The returned method
    /// will be called once per rtp packet.
    async fn bind_remote_stream(
        &self,
        info: &StreamInfo,
        reader: Arc<dyn RTPReader + Send + Sync>,
    ) -> Arc<dyn RTPReader + Send + Sync> {
        let mut lock = self.recv_streams.lock();

        let e = lock
            .entry(info.ssrc)
            .or_insert_with(|| Arc::new(RTPReadRecorder::new(reader)));

        e.clone()
    }

    /// unbind_remote_stream is called when the Stream is removed. It can be used to clean up any data related to that track.
    async fn unbind_remote_stream(&self, info: &StreamInfo) {
        let mut lock = self.recv_streams.lock();

        lock.remove(&info.ssrc);
    }

    /// bind_local_stream lets you modify any outgoing RTP packets. It is called once for per LocalStream. The returned method
    /// will be called once per rtp packet.
    async fn bind_local_stream(
        &self,
        info: &StreamInfo,
        writer: Arc<dyn RTPWriter + Send + Sync>,
    ) -> Arc<dyn RTPWriter + Send + Sync> {
        let mut lock = self.send_streams.lock();

        let e = lock
            .entry(info.ssrc)
            .or_insert_with(|| Arc::new(RTPWriteRecorder::new(writer)));

        e.clone()
    }

    /// unbind_local_stream is called when the Stream is removed. It can be used to clean up any data related to that track.
    async fn unbind_local_stream(&self, info: &StreamInfo) {
        let mut lock = self.send_streams.lock();

        lock.remove(&info.ssrc);
    }

    async fn close(&self) -> Result<()> {
        Ok(())
    }

    /// bind_rtcp_writer lets you modify any outgoing RTCP packets. It is called once per PeerConnection. The returned method
    /// will be called once per packet batch.
    async fn bind_rtcp_writer(
        &self,
        writer: Arc<dyn RTCPWriter + Send + Sync>,
    ) -> Arc<dyn RTCPWriter + Send + Sync> {
        let now = self
            .now_gen
            .clone()
            .map(|n| n.clone())
            .unwrap_or(Arc::new(|| SystemTime::now()));

        Arc::new(RTCPWriteInterceptor {
            rtcp_writer: writer,
            recorder: self.rtcp_send_recorder.clone(),
            now_gen: move || now(),
        })
    }

    /// bind_rtcp_reader lets you modify any incoming RTCP packets. It is called once per sender/receiver, however this might
    /// change in the future. The returned method will be called once per packet batch.
    async fn bind_rtcp_reader(
        &self,
        reader: Arc<dyn RTCPReader + Send + Sync>,
    ) -> Arc<dyn RTCPReader + Send + Sync> {
        let now = self
            .now_gen
            .clone()
            .map(|n| n.clone())
            .unwrap_or(Arc::new(|| SystemTime::now()));

        Arc::new(RTCPReadInterceptor {
            rtcp_reader: reader,
            recorder: self.rtcp_recv_recorder.clone(),
            now_gen: move || now(),
        })
    }
}

// Records incoming RTCP packets from the remote.
#[derive(Clone, Default)]
struct RTCPReadRecorder {
    // Observed RTCP feedback stats keyed by SSRC.
    stats: Arc<RwLock<HashMap<u32, RTCPStats>>>,
}

impl RTCPReadRecorder {
    fn get_reader(&self, ssrc: u32) -> RTCPStatsReader {
        let entry = self.stats.get_or_create(ssrc);

        entry.get_reader()
    }

    fn write_rtt_ms(&self, ssrc: u32, rtt: f64) {
        let entry = self.stats.get_or_create(ssrc);

        entry.write_rtt_ms(rtt)
    }

    fn write_loss(&self, ssrc: u32, loss: u8) {
        let entry = self.stats.get_or_create(ssrc);

        entry.write_loss(loss);
    }

    fn write_fir(&self, ssrc: u32, fir_count: u64) {
        let entry = self.stats.get_or_create(ssrc);

        entry.write_fir(fir_count);
    }

    fn write_pli(&self, ssrc: u32, pli_count: u64) {
        let entry = self.stats.get_or_create(ssrc);

        entry.write_pli(pli_count);
    }

    fn write_nack(&self, ssrc: u32, nack_count: u64) {
        let entry = self.stats.get_or_create(ssrc);

        entry.write_nack(nack_count);
    }
}

pub struct RTCPReadInterceptor<F> {
    rtcp_reader: Arc<dyn RTCPReader + Send + Sync>,
    recorder: RTCPReadRecorder,
    now_gen: F,
}

#[async_trait]
impl<F> RTCPReader for RTCPReadInterceptor<F>
where
    F: Fn() -> SystemTime + Send + Sync,
{
    /// read a batch of rtcp packets
    async fn read(&self, buf: &mut [u8], attributes: &Attributes) -> Result<(usize, Attributes)> {
        let (n, attributes) = self.rtcp_reader.read(buf, attributes).await?;

        let mut b = &buf[..n];
        let pkts = rtcp::packet::unmarshal(&mut b)?;
        // Middle 32 bits
        let now = (unix2ntp((self.now_gen)()) >> 16) as u32;

        for p in pkts {
            if let Some(rr) = p.as_any().downcast_ref::<ReceiverReport>() {
                for recp in &rr.reports {
                    let rtt_ms = calculate_rtt_ms(now, recp.delay, recp.last_sender_report);

                    self.recorder.write_rtt_ms(recp.ssrc, rtt_ms);
                    self.recorder.write_loss(recp.ssrc, recp.fraction_lost);
                }
            } else if let Some(fir) = p.as_any().downcast_ref::<FullIntraRequest>() {
                for entry in &fir.fir {
                    self.recorder.write_fir(entry.ssrc, 1);
                }
            } else if let Some(pli) = p.as_any().downcast_ref::<PictureLossIndication>() {
                self.recorder.write_pli(pli.media_ssrc, 1);
            } else if let Some(nack) = p.as_any().downcast_ref::<TransportLayerNack>() {
                // TODO: It would be nice if `NackPair` had a way to avoid allocating
                // simply to get the count.
                let count = nack.nacks.iter().flat_map(|p| p.packet_list()).count();

                self.recorder.write_nack(nack.media_ssrc, count as u64);
            }
        }

        Ok((n, attributes))
    }
}

// Records outgoing RTCP packets to the remote.
#[derive(Clone, Default)]
struct RTCPWriteRecorder {
    // Observed RTCP feedback stats keyed by SSRC.
    stats: Arc<RwLock<HashMap<u32, RTCPStats>>>,
}

impl RTCPWriteRecorder {
    fn get_reader(&self, ssrc: u32) -> RTCPStatsReader {
        let entry = self.stats.get_or_create(ssrc);

        entry.get_reader()
    }

    fn write_rtt_ms(&self, ssrc: u32, rtt: f64) {
        let entry = self.stats.get_or_create(ssrc);

        entry.write_rtt_ms(rtt)
    }

    fn write_loss(&self, ssrc: u32, loss: u8) {
        let entry = self.stats.get_or_create(ssrc);

        entry.write_loss(loss);
    }

    fn write_fir(&self, ssrc: u32, fir_count: u64) {
        let entry = self.stats.get_or_create(ssrc);

        entry.write_fir(fir_count);
    }

    fn write_pli(&self, ssrc: u32, pli_count: u64) {
        let entry = self.stats.get_or_create(ssrc);

        entry.write_pli(pli_count);
    }

    fn write_nack(&self, ssrc: u32, nack_count: u64) {
        let entry = self.stats.get_or_create(ssrc);

        entry.write_nack(nack_count);
    }
}

pub struct RTCPWriteInterceptor<F> {
    rtcp_writer: Arc<dyn RTCPWriter + Send + Sync>,
    recorder: RTCPWriteRecorder,
    now_gen: F,
}

#[async_trait]
impl<F> RTCPWriter for RTCPWriteInterceptor<F>
where
    F: Fn() -> SystemTime + Send + Sync,
{
    async fn write(
        &self,
        pkts: &[Box<dyn rtcp::packet::Packet + Send + Sync>],
        attributes: &Attributes,
    ) -> Result<usize> {
        dbg!(&pkts);
        for p in pkts {
            if let Some(rr) = p.as_any().downcast_ref::<ReceiverReport>() {
                for recp in &rr.reports {
                    // TODO
                }
            } else if let Some(fir) = p.as_any().downcast_ref::<FullIntraRequest>() {
                for entry in &fir.fir {
                    self.recorder.write_fir(entry.ssrc, 1);
                }
            } else if let Some(pli) = p.as_any().downcast_ref::<PictureLossIndication>() {
                self.recorder.write_pli(pli.media_ssrc, 1);
            } else if let Some(nack) = p.as_any().downcast_ref::<TransportLayerNack>() {
                // TODO: It would be nice if `NackPair` had a way to avoid allocating
                // simply to get the count.
                let count = nack.nacks.iter().flat_map(|p| p.packet_list()).count();

                self.recorder.write_nack(nack.media_ssrc, count as u64);
            }
        }

        self.rtcp_writer.write(pkts, attributes).await
    }
}

pub struct RTPReadRecorder {
    rtp_reader: Arc<dyn RTPReader + Send + Sync>,
    stats: RTPStats,
}

impl RTPReadRecorder {
    fn new(rtp_reader: Arc<dyn RTPReader + Send + Sync>) -> Self {
        Self {
            rtp_reader,
            stats: Default::default(),
        }
    }

    fn reader(&self) -> RTPStatsReader {
        self.stats.reader()
    }
}

#[async_trait]
impl RTPReader for RTPReadRecorder {
    async fn read(&self, buf: &mut [u8], attributes: &Attributes) -> Result<(usize, Attributes)> {
        let (bytes_read, attributes) = self.rtp_reader.read(buf, attributes).await?;
        // TODO: This parsing happens redundantly in several interceptors, would be good if we
        // could not do this.
        let mut b = &buf[..bytes_read];
        let packet = rtp::packet::Packet::unmarshal(&mut b)?;

        self.stats.update(
            (bytes_read - packet.payload.len()) as u64,
            packet.payload.len() as u64,
            1,
        );

        Ok((bytes_read, attributes))
    }
}

impl fmt::Debug for RTPReadRecorder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RTPReadRecorder")
            .field("stats", &self.stats)
            .finish()
    }
}

pub struct RTPWriteRecorder {
    rtp_writer: Arc<dyn RTPWriter + Send + Sync>,
    stats: RTPStats,
}

impl RTPWriteRecorder {
    fn new(rtp_writer: Arc<dyn RTPWriter + Send + Sync>) -> Self {
        Self {
            rtp_writer,
            stats: Default::default(),
        }
    }

    fn reader(&self) -> RTPStatsReader {
        self.stats.reader()
    }
}

#[async_trait]
impl RTPWriter for RTPWriteRecorder {
    /// write a rtp packet
    async fn write(&self, pkt: &rtp::packet::Packet, attributes: &Attributes) -> Result<usize> {
        let n = self.rtp_writer.write(pkt, attributes).await?;

        self.stats.update(
            pkt.header.marshal_size() as u64,
            pkt.payload.len() as u64,
            1,
        );

        Ok(n)
    }
}

impl fmt::Debug for RTPWriteRecorder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RTPWriteRecorder")
            .field("stats", &self.stats)
            .finish()
    }
}

trait GetOrCreateAtomic<T, U> {
    fn get_or_create(&self, key: T) -> U;
}

impl<T, U> GetOrCreateAtomic<T, U> for RwLock<HashMap<T, U>>
where
    T: Hash + Eq,
    U: Default + Clone,
{
    fn get_or_create(&self, key: T) -> U {
        let lock = self.read();

        if let Some(v) = lock.get(&key) {
            v.clone()
        } else {
            // Upgrade lock to write
            drop(lock);
            let mut lock = self.write();

            lock.entry(key).or_default().clone()
        }
    }
}

/// Calculate the round trip time for a given peer as described in
/// [RFC3550 6.4.1](https://datatracker.ietf.org/doc/html/rfc3550#section-6.4.1).
///
/// ## Params
///
/// - `now` the current middle 32 bits of an NTP timestamp for the current time.
/// - `delay` the delay(`DLSR`) since last sender report expressed as fractions of a second in 32 bits.
/// - `last_sender_report` the middle 32 bits of an NTP timestamp for the most recent sender report(LSR).
fn calculate_rtt_ms(now: u32, delay: u32, last_sender_report: u32) -> f64 {
    // [10 Nov 1995 11:33:25.125 UTC]       [10 Nov 1995 11:33:36.5 UTC]
    // n                 SR(n)              A=b710:8000 (46864.500 s)
    // ---------------------------------------------------------------->
    //                    v                 ^
    // ntp_sec =0xb44db705 v               ^ dlsr=0x0005:4000 (    5.250s)
    // ntp_frac=0x20000000  v             ^  lsr =0xb705:2000 (46853.125s)
    //   (3024992005.125 s)  v           ^
    // r                      v         ^ RR(n)
    // ---------------------------------------------------------------->
    //                        |<-DLSR->|
    //                         (5.250 s)
    //
    // A     0xb710:8000 (46864.500 s)
    // DLSR -0x0005:4000 (    5.250 s)
    // LSR  -0xb705:2000 (46853.125 s)
    // -------------------------------
    // delay 0x0006:2000 (    6.125 s)

    let rtt = now - delay - last_sender_report;
    let rtt_seconds = rtt >> 16;
    let rtt_fraction = (rtt & (u16::MAX as u32)) as f64 / (u16::MAX as u32) as f64;

    rtt_seconds as f64 * 1000.0 + (rtt_fraction as f64) * 1000.0
}

#[cfg(test)]
mod test {
    use bytes::Bytes;
    use rtcp::payload_feedbacks::full_intra_request::{FirEntry, FullIntraRequest};
    use rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
    use rtcp::receiver_report::ReceiverReport;
    use rtcp::reception_report::ReceptionReport;
    use rtcp::transport_feedbacks::transport_layer_nack::{NackPair, TransportLayerNack};

    use std::sync::Arc;
    use std::time::{Duration, SystemTime};

    use crate::error::Result;
    use crate::mock::mock_stream::MockStream;
    use crate::stream_info::StreamInfo;

    use super::StatsInterceptor;

    #[tokio::test]
    async fn test_stats_interceptor_rtp() -> Result<()> {
        let icpr: Arc<_> = Arc::new(StatsInterceptor::new("Hello".to_owned()));

        let recv_stream = MockStream::new(
            &StreamInfo {
                ssrc: 123456,
                ..Default::default()
            },
            icpr.clone(),
        )
        .await;

        let send_stream = MockStream::new(
            &StreamInfo {
                ssrc: 234567,
                ..Default::default()
            },
            icpr.clone(),
        )
        .await;

        let recv_reader = icpr
            .rtp_recv_stats_reader(123456)
            .expect("After binding recv_stats_reader should return Some");

        let send_reader = icpr
            .rtp_send_stats_reader(234567)
            .expect("After binding send_stats_reader should return Some");

        let _ = recv_stream
            .receive_rtp(rtp::packet::Packet {
                header: rtp::header::Header {
                    ..Default::default()
                },
                payload: Bytes::from_static(b"\xde\xad\xbe\xef"),
            })
            .await;

        let _ = recv_stream
            .read_rtp()
            .await
            .expect("After calling receive_rtp read_rtp should return Some")?;

        let _ = send_stream
            .write_rtp(&rtp::packet::Packet {
                header: rtp::header::Header {
                    ..Default::default()
                },
                payload: Bytes::from_static(b"\xde\xad\xbe\xef\xde\xad\xbe\xef"),
            })
            .await;

        let _ = send_stream
            .write_rtp(&rtp::packet::Packet {
                header: rtp::header::Header {
                    ..Default::default()
                },
                payload: Bytes::from_static(&[0x13, 0x37]),
            })
            .await;

        assert_eq!(recv_reader.packets(), 1);
        assert_eq!(recv_reader.header_bytes(), 12);
        assert_eq!(recv_reader.payload_bytes(), 4);

        assert_eq!(send_reader.packets(), 2);
        assert_eq!(send_reader.header_bytes(), 24);
        assert_eq!(send_reader.payload_bytes(), 10);

        Ok(())
    }

    #[tokio::test]
    async fn test_stats_interceptor_rtcp() -> Result<()> {
        let icpr: Arc<_> = Arc::new(StatsInterceptor::with_time_gen("Hello".to_owned(), || {
            // 10 Nov 1995 11:33:36.5 UTC
            SystemTime::UNIX_EPOCH + Duration::from_secs_f64(816003216.5)
        }));

        let recv_stream = MockStream::new(
            &StreamInfo {
                ssrc: 123456,
                ..Default::default()
            },
            icpr.clone(),
        )
        .await;

        let send_stream = MockStream::new(
            &StreamInfo {
                ssrc: 234567,
                ..Default::default()
            },
            icpr.clone(),
        )
        .await;

        let rtcp_recv_reader = icpr
            .rtcp_recv_stats_reader(234567)
            .expect("After binding rtcp_recv_stats_reader should return Some");

        let rtcp_send_reader = icpr
            .rtcp_send_stats_reader(123456)
            .expect("After binding rtcp_recv_stats_reader should return Some");

        send_stream
            .receive_rtcp(vec![
                Box::new(ReceiverReport {
                    reports: vec![ReceptionReport {
                        ssrc: 234567,
                        last_sender_report: 0xb705_2000,
                        delay: 0x0005_4000,
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
                Box::new(TransportLayerNack {
                    sender_ssrc: 0,
                    media_ssrc: 234567,
                    nacks: vec![NackPair {
                        packet_id: 5,
                        lost_packets: 0b0011_0110,
                    }],
                }),
                Box::new(TransportLayerNack {
                    sender_ssrc: 0,
                    // NB: Different SSRC
                    media_ssrc: 999999,
                    nacks: vec![NackPair {
                        packet_id: 5,
                        lost_packets: 0b0011_0110,
                    }],
                }),
                Box::new(PictureLossIndication {
                    sender_ssrc: 0,
                    media_ssrc: 234567,
                }),
                Box::new(PictureLossIndication {
                    sender_ssrc: 0,
                    media_ssrc: 234567,
                }),
                Box::new(FullIntraRequest {
                    sender_ssrc: 0,
                    media_ssrc: 234567,
                    fir: vec![
                        FirEntry {
                            ssrc: 234567,
                            sequence_number: 132,
                        },
                        FirEntry {
                            ssrc: 234567,
                            sequence_number: 135,
                        },
                    ],
                }),
            ])
            .await;

        let _ = send_stream
            .read_rtcp()
            .await
            .expect("After calling `receive_rtcp`, `read_rtcp` should return some packets");

        recv_stream
            .write_rtcp(&[
                Box::new(TransportLayerNack {
                    sender_ssrc: 0,
                    media_ssrc: 123456,
                    nacks: vec![NackPair {
                        packet_id: 5,
                        lost_packets: 0b0011_0111,
                    }],
                }),
                Box::new(TransportLayerNack {
                    sender_ssrc: 0,
                    // NB: Different SSRC
                    media_ssrc: 999999,
                    nacks: vec![NackPair {
                        packet_id: 5,
                        lost_packets: 0b1111_0110,
                    }],
                }),
                Box::new(PictureLossIndication {
                    sender_ssrc: 0,
                    media_ssrc: 123456,
                }),
                Box::new(PictureLossIndication {
                    sender_ssrc: 0,
                    media_ssrc: 123456,
                }),
                Box::new(PictureLossIndication {
                    sender_ssrc: 0,
                    media_ssrc: 123456,
                }),
                Box::new(FullIntraRequest {
                    sender_ssrc: 0,
                    media_ssrc: 123456,
                    fir: vec![FirEntry {
                        ssrc: 123456,
                        sequence_number: 132,
                    }],
                }),
            ])
            .await
            .expect("Failed to write RTCP packets for recv_stream");

        let rtt_ms = rtcp_recv_reader.rtt_ms();
        assert!(rtt_ms > 6124.99 && rtt_ms < 6125.01);

        assert_eq!(rtcp_recv_reader.nack_count(), 5);
        assert_eq!(rtcp_recv_reader.pli_count(), 2);
        assert_eq!(rtcp_recv_reader.fir_count(), 2);

        assert_eq!(rtcp_send_reader.fir_count(), 1);
        assert_eq!(rtcp_send_reader.nack_count(), 6);
        assert_eq!(rtcp_send_reader.pli_count(), 3);

        Ok(())
    }
}
