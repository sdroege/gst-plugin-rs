use crate::utils::ExtendedSeqnum;
use rtp_types::RtpPacket;
use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
struct Stats {
    num_late: u64,
    num_lost: u64,
    num_duplicates: u64,
    num_pushed: u64,
}

impl From<Stats> for gst::Structure {
    fn from(stats: Stats) -> gst::Structure {
        gst::Structure::builder("application/x-rtp-jitterbuffer-stats")
            .field("num-late", stats.num_late)
            .field("num-duplicates", stats.num_duplicates)
            .field("num-lost", stats.num_lost)
            .field("num-pushed", stats.num_pushed)
            .build()
    }
}

#[derive(Debug)]
pub struct JitterBuffer {
    packet_counter: usize,
    // A set of extended seqnums that we've already seen through,
    // intentionally trimmed separately from the items list so that
    // we can detect duplicates after the first copy has exited the
    // queue
    seqnums: BTreeSet<u64>,
    items: BTreeSet<Item>,
    latency: Duration,
    // Arrival time, PTS
    base_times: Option<(Instant, u64)>,
    last_output_seqnum: Option<u64>,
    extended_seqnum: ExtendedSeqnum,
    stats: Stats,
    flushing: bool,
    can_forward_packets_when_empty: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PollResult {
    Forward { id: usize, discont: bool },
    Drop(usize),
    Timeout(Instant),
    Empty,
    Flushing,
}

#[derive(Debug, PartialEq, Eq)]
pub enum QueueResult {
    Forward { id: usize, discont: bool },
    Queued(usize),
    Late,
    Duplicate,
    Flushing,
}

#[derive(Eq, Debug)]
struct Item {
    id: usize,
    // If not set, this is an event / query
    deadline: Option<Duration>,
    seqnum: u64,
}

impl Ord for Item {
    fn cmp(&self, other: &Self) -> Ordering {
        self.seqnum
            .cmp(&other.seqnum)
            .then(match (self.deadline, other.deadline) {
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                _ => Ordering::Equal,
            })
    }
}

impl PartialOrd for Item {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Item {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl JitterBuffer {
    pub fn new(latency: Duration) -> Self {
        Self {
            packet_counter: 0,
            seqnums: BTreeSet::new(),
            items: BTreeSet::new(),
            latency,
            base_times: None,
            last_output_seqnum: None,
            extended_seqnum: ExtendedSeqnum::default(),
            stats: Stats {
                num_late: 0,
                num_lost: 0,
                num_duplicates: 0,
                num_pushed: 0,
            },
            flushing: true,
            can_forward_packets_when_empty: latency.is_zero(),
        }
    }

    pub fn queue_serialized_item(&mut self, origin: &str) -> QueueResult {
        if self.items.is_empty() {
            let id = self.packet_counter;
            self.packet_counter += 1;
            return QueueResult::Forward { id, discont: false };
        }

        let id = self.packet_counter;
        self.packet_counter += 1;
        let item = Item {
            id,
            deadline: None,
            seqnum: (*self.seqnums.last().unwrap_or(&0)),
        };
        self.items.insert(item);
        trace!("{origin} Queued serialized item and assigned ID {id}");

        QueueResult::Queued(id)
    }

    pub fn set_flushing(&mut self, origin: &str, flushing: bool) {
        trace!(
            "{origin} Flush changed from {} to {flushing}",
            self.flushing
        );
        self.flushing = flushing;
        self.last_output_seqnum = None;
        self.can_forward_packets_when_empty = self.latency.is_zero();
    }

    pub fn queue_packet(
        &mut self,
        origin: &str,
        rtp: &RtpPacket,
        pts: u64,
        now: Instant,
    ) -> QueueResult {
        if self.flushing {
            return QueueResult::Flushing;
        }

        // From this point on we always work with extended sequence numbers
        let seqnum = self.extended_seqnum.next(rtp.sequence_number());

        // Maintain (and trim) our seqnum list for duplicate detection
        while self.seqnums.len() >= u16::MAX as usize {
            debug!("{origin} Trimming");
            self.seqnums.pop_first();
        }

        if self.seqnums.contains(&seqnum) {
            trace!(
                "{origin} Duplicated packet seqnum {} (extended {seqnum})",
                rtp.sequence_number(),
            );
            self.stats.num_duplicates += 1;
            return QueueResult::Duplicate;
        }

        self.seqnums.insert(seqnum);

        if let Some(last_output_seqnum) = self.last_output_seqnum
            && last_output_seqnum >= seqnum
        {
            debug!(
                "{origin} Late packet seqnum {} (extended {seqnum}), last output seqnum {last_output_seqnum}",
                rtp.sequence_number(),
            );
            self.stats.num_late += 1;
            return QueueResult::Late;
        }

        if self.items.is_empty()
            // can forward after the first packet's deadline has been reached
            && self.can_forward_packets_when_empty
        {
            if self
                .last_output_seqnum
                .is_some_and(|last_output_seqnum| seqnum == last_output_seqnum.wrapping_add(1))
            {
                // No packets enqueued & seqnum is in order => can forward it immediately
                self.last_output_seqnum = Some(seqnum);
                let id = self.packet_counter;
                self.packet_counter += 1;
                self.stats.num_pushed += 1;

                return QueueResult::Forward { id, discont: false };
            } else if self.latency.is_zero() {
                // No packets enqueued & seqnum is *not* in order or first packet but latency is 0
                //   => can forward it immediately

                if let Some(last_output_seq_ext) = self.last_output_seqnum {
                    let gap = seqnum - last_output_seq_ext;

                    assert!(gap != 1);
                    debug!(
                        "{origin} Packets before seqnum {} (extended {seqnum}) considered lost",
                        rtp.sequence_number(),
                    );
                    self.stats.num_lost += gap - 1;
                }

                self.last_output_seqnum = Some(seqnum);
                let id = self.packet_counter;
                self.packet_counter += 1;
                self.stats.num_pushed += 1;

                return QueueResult::Forward { id, discont: true };
            }
        }

        // TODO: if the segnum base is known (i.e. first seqnum in RTSP)
        //       we could also Forward the initial Packet.

        let (_, base_pts) = self.base_times.get_or_insert_with(|| {
            debug!("{origin} Selected base times {now:?} {pts}");

            (now, pts)
        });

        let deadline = (Duration::from_nanos(pts) + self.latency)
            .saturating_sub(Duration::from_nanos(*base_pts));

        let id = self.packet_counter;
        self.packet_counter += 1;
        let item = Item {
            id,
            deadline: Some(deadline),
            seqnum,
        };

        if !self.items.insert(item) {
            unreachable!()
        }

        trace!(
            "{origin} Queued RTP packet with ts {pts}, seqnum {} (extended {seqnum}), assigned ID {id}",
            rtp.sequence_number()
        );

        QueueResult::Queued(id)
    }

    pub fn poll(&mut self, origin: &str, now: Instant) -> PollResult {
        if self.flushing {
            if let Some(item) = self.items.pop_first() {
                return PollResult::Drop(item.id);
            } else {
                return PollResult::Flushing;
            }
        }

        trace!("{origin} Polling at {now:?}");

        let Some(item) = self.items.first() else {
            return PollResult::Empty;
        };

        // If an event / query is at the top of the queue, it can be forwarded immediately
        let Some(deadline) = item.deadline else {
            let item = self.items.pop_first().unwrap();
            return PollResult::Forward {
                id: item.id,
                discont: false,
            };
        };

        let Some((base_instant, _)) = self.base_times else {
            return PollResult::Empty;
        };

        let duration_since_base_instant = now - base_instant;

        trace!("{origin} Duration since base instant {duration_since_base_instant:?}");

        trace!(
            "{origin} Considering packet ID {} (seqnum {}, extended {}), deadline is {deadline:?}",
            item.id,
            item.seqnum & 0xffff,
            item.seqnum,
        );

        if deadline <= duration_since_base_instant {
            debug!(
                "{origin} Packet with id {} (seqnum {}, extended {}) is ready",
                item.id,
                item.seqnum & 0xffff,
                item.seqnum
            );

            let discont = match self.last_output_seqnum {
                None => true,
                Some(last_output_seq_ext) => {
                    let gap = item.seqnum - last_output_seq_ext;

                    if gap != 1 {
                        debug!(
                            "{origin} Packets before ID {} (seqnum {}, extended {}) considered lost",
                            item.id,
                            item.seqnum & 0xffff,
                            item.seqnum
                        );
                        self.stats.num_lost += gap - 1;
                    }

                    gap != 1
                }
            };

            self.last_output_seqnum = Some(item.seqnum);
            // Safe unwrap, we know the queue isn't empty at this point
            let packet = self.items.pop_first().unwrap();

            self.stats.num_pushed += 1;
            self.can_forward_packets_when_empty = true;

            PollResult::Forward {
                id: packet.id,
                discont,
            }
        } else {
            trace!(
                "{origin} Packet ID {} (seqnum {}, extended {}) is not ready",
                item.id,
                item.seqnum & 0xffff,
                item.seqnum
            );
            PollResult::Timeout(base_instant + deadline)
        }
    }

    pub fn stats(&self) -> gst::Structure {
        self.stats.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtpbin2::session::tests::generate_rtp_packet;

    const SSRC: u32 = 0x12345678;

    const PAYLOAD_LEN: usize = 4;
    const LATENCY_MS: u64 = 1_000;
    const LATENCY: Duration = Duration::from_millis(LATENCY_MS);
    const CLOCK_RATE: u128 = 90000;

    fn init() {
        use std::sync::Once;
        static INIT: Once = Once::new();

        INIT.call_once(|| {
            let _ = env_logger::try_init();
        });
    }

    fn ts_to_rtpts(ts: Duration) -> u32 {
        (CLOCK_RATE * 1_000)
            .checked_div(ts.as_millis())
            .unwrap_or(0) as u32
    }

    #[test]
    fn empty() {
        const ORIGIN: &str = "empty";

        let mut jb = JitterBuffer::new(Duration::from_secs(1));
        jb.set_flushing(ORIGIN, false);

        let now = Instant::now();

        assert_eq!(jb.poll(ORIGIN, now), PollResult::Empty);
    }

    #[test]
    fn receive_one_packet_no_latency() {
        const ORIGIN: &str = "receive_one_packet_no_latency";

        let mut jb = JitterBuffer::new(Duration::from_secs(0));
        jb.set_flushing(ORIGIN, false);

        let rtp_data = generate_rtp_packet(0x12345678, 0, 0, 4);
        let packet = RtpPacket::parse(&rtp_data).unwrap();

        let now = Instant::now();

        let QueueResult::Forward { id, discont } = jb.queue_packet(ORIGIN, &packet, 0, now) else {
            unreachable!()
        };

        assert_eq!(id, 0);
        assert!(discont);
    }

    #[test]
    fn receive_one_packet_with_latency() {
        const ORIGIN: &str = "receive_one_packet_with_latency";

        let mut jb = JitterBuffer::new(Duration::from_secs(1));
        jb.set_flushing(ORIGIN, false);

        let rtp_data = generate_rtp_packet(0x12345678, 0, 0, 4);
        let packet = RtpPacket::parse(&rtp_data).unwrap();

        let mut now = Instant::now();

        let QueueResult::Queued(id) = jb.queue_packet(ORIGIN, &packet, 0, now) else {
            unreachable!()
        };

        assert_eq!(
            jb.poll(ORIGIN, now),
            PollResult::Timeout(now + Duration::from_secs(1))
        );

        now += Duration::from_secs(1);
        now -= Duration::from_nanos(1);

        assert_eq!(
            jb.poll(ORIGIN, now),
            PollResult::Timeout(now + Duration::from_nanos(1))
        );

        now += Duration::from_nanos(1);

        assert_eq!(
            jb.poll(ORIGIN, now),
            PollResult::Forward { id, discont: true }
        );
    }

    #[test]
    fn ordered_packets_no_latency() {
        const ORIGIN: &str = "ordered_packets_no_latency";

        let mut jb = JitterBuffer::new(Duration::from_secs(0));
        jb.set_flushing(ORIGIN, false);

        let now = Instant::now();

        let rtp_data = generate_rtp_packet(0x12345678, 0, 0, 4);
        let packet = RtpPacket::parse(&rtp_data).unwrap();

        let QueueResult::Forward {
            id: _,
            discont: true,
        } = jb.queue_packet(ORIGIN, &packet, 0, now)
        else {
            unreachable!()
        };

        let rtp_data = generate_rtp_packet(0x12345678, 1, 0, 4);
        let packet = RtpPacket::parse(&rtp_data).unwrap();
        let QueueResult::Forward {
            id: _,
            discont: false,
        } = jb.queue_packet(ORIGIN, &packet, 0, now)
        else {
            unreachable!()
        };
    }

    #[test]
    fn ordered_packets_no_latency_with_gap() {
        const ORIGIN: &str = "ordered_packets_no_latency_with_gap";

        let mut jb = JitterBuffer::new(Duration::from_secs(0));
        jb.set_flushing("ordered_packets_no_latency_with_gap", false);

        let now = Instant::now();

        let rtp_data = generate_rtp_packet(0x12345678, 0, 0, 4);
        let packet = RtpPacket::parse(&rtp_data).unwrap();
        let QueueResult::Forward {
            id: _,
            discont: true,
        } = jb.queue_packet(ORIGIN, &packet, 0, now)
        else {
            unreachable!()
        };

        let rtp_data = generate_rtp_packet(0x12345678, 2, 0, 4);
        let packet = RtpPacket::parse(&rtp_data).unwrap();
        let QueueResult::Forward {
            id: _,
            discont: true,
        } = jb.queue_packet(ORIGIN, &packet, 0, now)
        else {
            unreachable!()
        };
    }

    #[test]
    fn misordered_packets_no_latency() {
        const ORIGIN: &str = "misordered_packets_no_latency";

        let mut jb = JitterBuffer::new(Duration::from_secs(0));
        jb.set_flushing(ORIGIN, false);

        let now = Instant::now();

        let rtp_data = generate_rtp_packet(0x12345678, 1, 0, 4);
        let packet = RtpPacket::parse(&rtp_data).unwrap();
        let QueueResult::Forward {
            id: _,
            discont: true,
        } = jb.queue_packet(ORIGIN, &packet, 0, now)
        else {
            unreachable!()
        };

        let rtp_data = generate_rtp_packet(0x12345678, 0, 0, 4);
        let packet = RtpPacket::parse(&rtp_data).unwrap();
        assert_eq!(jb.queue_packet(ORIGIN, &packet, 0, now), QueueResult::Late);

        // Try and push a duplicate
        let rtp_data = generate_rtp_packet(0x12345678, 1, 0, 4);
        let packet = RtpPacket::parse(&rtp_data).unwrap();
        assert_eq!(
            jb.queue_packet(ORIGIN, &packet, 0, now),
            QueueResult::Duplicate
        );

        // We do accept future sequence numbers up to a distance of at least i16::MAX
        let rtp_data = generate_rtp_packet(0x12345678, i16::MAX as u16 + 1, 0, 4);
        let packet = RtpPacket::parse(&rtp_data).unwrap();
        let QueueResult::Forward {
            id: _,
            discont: true,
        } = jb.queue_packet(ORIGIN, &packet, 0, now)
        else {
            unreachable!()
        };

        // But no further
        let rtp_data = generate_rtp_packet(0x12345678, 2, 0, 4);
        let packet = RtpPacket::parse(&rtp_data).unwrap();
        assert_eq!(jb.queue_packet(ORIGIN, &packet, 0, now), QueueResult::Late);
    }

    #[test]
    fn ordered_packets_with_latency() {
        const ORIGIN: &str = "ordered_packets_with_latency";

        let mut jb = JitterBuffer::new(Duration::from_secs(1));
        jb.set_flushing("ordered_packets_with_latency", false);

        let mut now = Instant::now();

        let rtp_data = generate_rtp_packet(0x12345678, 0, 0, 4);
        let packet = RtpPacket::parse(&rtp_data).unwrap();
        let QueueResult::Queued(id_first) = jb.queue_packet(ORIGIN, &packet, 0, now) else {
            unreachable!()
        };

        assert_eq!(
            jb.poll(ORIGIN, now),
            PollResult::Timeout(now + Duration::from_secs(1))
        );

        let rtp_data = generate_rtp_packet(0x12345678, 1, 180000, 4);
        let packet = RtpPacket::parse(&rtp_data).unwrap();
        let QueueResult::Queued(id_second) = jb.queue_packet(ORIGIN, &packet, 2_000_000_000, now)
        else {
            unreachable!()
        };

        assert_eq!(
            jb.poll(ORIGIN, now),
            PollResult::Timeout(now + Duration::from_secs(1))
        );

        now += Duration::from_secs(1);

        assert_eq!(
            jb.poll(ORIGIN, now),
            PollResult::Forward {
                id: id_first,
                discont: true
            }
        );

        assert_eq!(
            jb.poll(ORIGIN, now),
            PollResult::Timeout(now + Duration::from_secs(2))
        );

        now += Duration::from_secs(2);

        assert_eq!(
            jb.poll(ORIGIN, now),
            PollResult::Forward {
                id: id_second,
                discont: false
            }
        );
    }

    #[test]
    fn poll_misordered_packets() {
        const ORIGIN: &str = "poll_misordered_packets";
        const PACKET_INTERVAL: Duration = Duration::from_millis(LATENCY_MS / 2);

        init();

        let mut jb = JitterBuffer::new(LATENCY);
        jb.set_flushing(ORIGIN, false);

        let instant_ts0 = Instant::now();

        info!("{ORIGIN} Enqeueing initial packet");
        let packet_ts0 = Duration::ZERO;
        let rtp_data = generate_rtp_packet(SSRC, 0, ts_to_rtpts(packet_ts0), PAYLOAD_LEN);
        let packet = RtpPacket::parse(&rtp_data).unwrap();
        let id0 = match jb.queue_packet(ORIGIN, &packet, packet_ts0.as_nanos() as u64, instant_ts0)
        {
            QueueResult::Queued(id0) => id0,
            other => unreachable!("{other:?}"),
        };
        info!("{ORIGIN} First packet out is expected after LATENCY");
        let deadline_packet0 = match jb.poll(ORIGIN, instant_ts0) {
            PollResult::Timeout(deadline) => deadline,
            other => unreachable!("{other:?}"),
        };
        assert_eq!(deadline_packet0, instant_ts0 + LATENCY);

        info!("{ORIGIN} Skipping two packets (one of them will arrive later)");
        let instant = instant_ts0 + 2 * PACKET_INTERVAL;
        info!("{ORIGIN} First packet out is one with seqnum 0");
        let id = match jb.poll(ORIGIN, instant) {
            PollResult::Forward {
                id,
                discont: true, // first packet to be pulled out
            } => id,
            other => unreachable!("{other:?}"),
        };
        assert_eq!(id, id0);

        info!("{ORIGIN} Pushing on-time packet 3");
        let packet_ts3 = packet_ts0 + 3 * PACKET_INTERVAL;
        let instant_ts3 = instant_ts0 + 3 * PACKET_INTERVAL;
        let rtp_data = generate_rtp_packet(SSRC, 3, ts_to_rtpts(packet_ts3), PAYLOAD_LEN);
        let packet = RtpPacket::parse(&rtp_data).unwrap();
        let id3 = match jb.queue_packet(ORIGIN, &packet, packet_ts3.as_nanos() as u64, instant_ts3)
        {
            QueueResult::Queued(id3) => id3,
            other => unreachable!("{other:?}"),
        };
        info!("{ORIGIN} Next packet out will be the one with seqnum 3");
        let deadline = match jb.poll(ORIGIN, instant_ts3) {
            PollResult::Timeout(deadline) => deadline,
            other => unreachable!("{other:?}"),
        };
        assert_eq!(deadline, deadline_packet0 + 3 * PACKET_INTERVAL);

        info!("{ORIGIN} Pushing earlier packet");
        let packet_ts1 = PACKET_INTERVAL;
        let instant_ts1 = instant_ts3 + PACKET_INTERVAL / 2;
        let rtp_data = generate_rtp_packet(SSRC, 1, ts_to_rtpts(packet_ts1), PAYLOAD_LEN);
        let packet = RtpPacket::parse(&rtp_data).unwrap();
        let id1 = match jb.queue_packet(ORIGIN, &packet, packet_ts1.as_nanos() as u64, instant_ts1)
        {
            QueueResult::Queued(id1) => id1,
            other => unreachable!("{other:?}"),
        };
        info!("{ORIGIN} Packet with seqnum 1 can be forwarded");
        let id = match jb.poll(ORIGIN, instant_ts1) {
            PollResult::Forward {
                id,
                discont: false, // Next packet after packet with seqnum 0
            } => id,
            other => unreachable!("{other:?}"),
        };
        assert_eq!(id, id1);

        info!("{ORIGIN} Pushing on-time packet 4");
        let packet_ts4 = packet_ts0 + 4 * PACKET_INTERVAL;
        let instant_ts4 = instant_ts0 + 4 * PACKET_INTERVAL;
        let rtp_data = generate_rtp_packet(SSRC, 4, ts_to_rtpts(packet_ts4), PAYLOAD_LEN);
        let packet = RtpPacket::parse(&rtp_data).unwrap();
        let id4 = match jb.queue_packet(ORIGIN, &packet, packet_ts4.as_nanos() as u64, instant_ts4)
        {
            QueueResult::Queued(id4) => id4,
            other => unreachable!("{other:?}"),
        };
        info!("{ORIGIN} Next packet out is still the one with seqnum 3");
        let deadline = match jb.poll(ORIGIN, instant_ts4) {
            PollResult::Timeout(deadline) => deadline,
            other => unreachable!("{other:?}"),
        };
        assert_eq!(deadline, deadline_packet0 + 3 * PACKET_INTERVAL);

        info!("{ORIGIN} Pulling packet with seqnum 3 at its expected deadline");
        let pulled_id = match jb.poll(ORIGIN, deadline_packet0 + 3 * PACKET_INTERVAL) {
            PollResult::Forward {
                id: pulled_id,
                discont: true, // some packets are missing
            } => pulled_id,
            other => unreachable!("{other:?}"),
        };
        assert_eq!(pulled_id, id3);

        let instant_after_pulling_ts3 =
            deadline_packet0 + 3 * PACKET_INTERVAL + PACKET_INTERVAL / 4;
        info!("{ORIGIN} Next packet out is now the one with seqnum 4");
        let deadline = match jb.poll(ORIGIN, instant_after_pulling_ts3) {
            PollResult::Timeout(deadline) => deadline,
            other => unreachable!("{other:?}"),
        };
        assert_eq!(deadline, deadline_packet0 + 4 * PACKET_INTERVAL);

        info!("{ORIGIN} Pulling packet with seqnum 4 at its expected deadline");
        let pulled_id = match jb.poll(ORIGIN, deadline_packet0 + 4 * PACKET_INTERVAL) {
            PollResult::Forward {
                id: pulled_id,
                discont: false, // not first packet to be pulled out
            } => pulled_id,
            other => unreachable!("{other:?}"),
        };
        assert_eq!(pulled_id, id4);
    }

    #[test]
    fn poll_early_misordered_packets() {
        const ORIGIN: &str = "poll_early_misordered_packets";
        const PACKET_INTERVAL: Duration = Duration::from_millis(LATENCY_MS / 2);

        init();

        let mut jb = JitterBuffer::new(LATENCY);
        jb.set_flushing(ORIGIN, false);

        info!("{ORIGIN} Pushing first received & on-time packet 2");
        let instant_ts2 = Instant::now();
        let packet_ts2 = 2 * PACKET_INTERVAL;
        let rtp_data = generate_rtp_packet(SSRC, 2, ts_to_rtpts(packet_ts2), PAYLOAD_LEN);
        let packet = RtpPacket::parse(&rtp_data).unwrap();
        match jb.queue_packet(ORIGIN, &packet, packet_ts2.as_nanos() as u64, instant_ts2) {
            QueueResult::Queued(_) => (),
            other => unreachable!("{other:?}"),
        }
        info!("{ORIGIN} So far, the first packet out is expected to be the one with seqnum 2");
        let deadline_packet_ts2_out = match jb.poll(ORIGIN, instant_ts2) {
            PollResult::Timeout(deadline) => deadline,
            other => unreachable!("{other:?}"),
        };
        assert_eq!(deadline_packet_ts2_out, instant_ts2 + LATENCY);

        info!("{ORIGIN} Earlier packet with seqnum 1 (on-time, but pushed after seqnum 2)");
        let packet_ts1 = PACKET_INTERVAL;
        let instant_ts1 = instant_ts2 + PACKET_INTERVAL / 8;
        let rtp_data = generate_rtp_packet(SSRC, 1, ts_to_rtpts(packet_ts1), PAYLOAD_LEN);
        let packet = RtpPacket::parse(&rtp_data).unwrap();
        let id1 = match jb.queue_packet(ORIGIN, &packet, packet_ts1.as_nanos() as u64, instant_ts1)
        {
            QueueResult::Queued(id1) => id1,
            other => unreachable!("{other:?}"),
        };
        info!("{ORIGIN} Now, first packet out is expected to be packet with seqnum 1");
        let deadline_packet_ts1_out = match jb.poll(ORIGIN, instant_ts1) {
            PollResult::Timeout(deadline) => deadline,
            other => unreachable!("{other:?}"),
        };
        assert_eq!(
            deadline_packet_ts1_out,
            instant_ts2 + LATENCY - PACKET_INTERVAL
        );

        info!("{ORIGIN} Pulling packet with seqnum 1 at its expected deadline");
        let pulled_id = match jb.poll(ORIGIN, deadline_packet_ts1_out) {
            PollResult::Forward {
                id: pulled_id,
                discont: true, // first packet to be pulled out
            } => pulled_id,
            other => unreachable!("{other:?}"),
        };
        assert_eq!(pulled_id, id1);
        let instant_after_pulling_ts1 = deadline_packet_ts1_out + PACKET_INTERVAL / 4;
        info!("{ORIGIN} Next packet out is the one with seqnum 2");
        let deadline = match jb.poll(ORIGIN, instant_after_pulling_ts1) {
            PollResult::Timeout(deadline) => deadline,
            other => unreachable!("{other:?}"),
        };
        assert_eq!(deadline, deadline_packet_ts2_out);
    }

    fn assert_stats(
        jb: &JitterBuffer,
        num_late: u64,
        num_lost: u64,
        num_duplicates: u64,
        num_pushed: u64,
    ) {
        let stats = jb.stats();

        assert_eq!(stats.get::<u64>("num-late").unwrap(), num_late);
        assert_eq!(stats.get::<u64>("num-lost").unwrap(), num_lost);
        assert_eq!(stats.get::<u64>("num-duplicates").unwrap(), num_duplicates);
        assert_eq!(stats.get::<u64>("num-pushed").unwrap(), num_pushed);
    }

    #[test]
    fn stats() {
        const ORIGIN: &str = "stats";

        let mut jb = JitterBuffer::new(Duration::from_secs(1));
        jb.set_flushing("stats", false);

        let mut now = Instant::now();

        let rtp_data = generate_rtp_packet(0x12345678, 0, 0, 4);
        let packet = RtpPacket::parse(&rtp_data).unwrap();
        jb.queue_packet(ORIGIN, &packet, 0, now);

        assert_stats(&jb, 0, 0, 0, 0);

        // At this point pushing the same packet in before it gets output
        // results in an increment of the duplicate stat
        jb.queue_packet(ORIGIN, &packet, 0, now);
        assert_stats(&jb, 0, 0, 1, 0);

        now += Duration::from_secs(1);
        let _ = jb.poll(ORIGIN, now);

        assert_stats(&jb, 0, 0, 1, 1);

        // Pushing it after the first version got output also results in
        // an increment of the duplicate stat
        jb.queue_packet(ORIGIN, &packet, 0, now);
        assert_stats(&jb, 0, 0, 2, 1);

        // Then after a packet with seqnum 2 goes through, the lost
        // stat must be incremented by 1 (as packet with seqnum 1 went missing)
        let rtp_data = generate_rtp_packet(0x12345678, 2, 9000, 4);
        let packet = RtpPacket::parse(&rtp_data).unwrap();
        jb.queue_packet(ORIGIN, &packet, 100_000_000, now);

        now += Duration::from_millis(100);
        let _ = jb.poll(ORIGIN, now);
        assert_stats(&jb, 0, 1, 2, 2);

        // If the packet with seqnum 1 does arrive after that, it should be
        // considered both late and lost
        let rtp_data = generate_rtp_packet(0x12345678, 1, 4500, 4);
        let packet = RtpPacket::parse(&rtp_data).unwrap();
        jb.queue_packet(ORIGIN, &packet, 50_000_000, now);

        let _ = jb.poll(ORIGIN, now);
        assert_stats(&jb, 1, 1, 2, 2);

        // Finally if it arrives again it should be considered a duplicate,
        // and will have achieved the dubious honor of simultaneously being
        // lost, late and duplicated
        jb.queue_packet(ORIGIN, &packet, 50_000_000, now);

        let _ = jb.poll(ORIGIN, now);
        assert_stats(&jb, 1, 1, 3, 2);
    }

    #[test]
    fn serialized_items() {
        const ORIGIN: &str = "serialized_items";

        let mut jb = JitterBuffer::new(Duration::from_secs(1));
        jb.set_flushing(ORIGIN, false);

        let now = Instant::now();

        let rtp_data = generate_rtp_packet(0x12345678, 0, 0, 4);
        let packet = RtpPacket::parse(&rtp_data).unwrap();

        let QueueResult::Forward {
            id: id_first_serialized_item,
            discont: discont_first_serialized_item,
        } = jb.queue_serialized_item(ORIGIN)
        else {
            unreachable!()
        };
        assert_eq!(id_first_serialized_item, 0);
        assert!(!discont_first_serialized_item);

        // query has been forwarded immediately
        assert_eq!(jb.poll(ORIGIN, now), PollResult::Empty);

        let QueueResult::Queued(id_first_packet) = jb.queue_packet(ORIGIN, &packet, 0, now) else {
            unreachable!()
        };
        assert_eq!(id_first_packet, id_first_serialized_item + 1);
        let QueueResult::Queued(id_second_serialized_item) = jb.queue_serialized_item(ORIGIN)
        else {
            unreachable!()
        };
        assert_eq!(id_second_serialized_item, id_first_packet + 1);

        assert_eq!(
            jb.poll(ORIGIN, now + Duration::from_secs(1)),
            PollResult::Forward {
                id: id_first_packet,
                discont: true
            }
        );

        assert_eq!(
            jb.poll(ORIGIN, now + Duration::from_secs(1)),
            PollResult::Forward {
                id: id_second_serialized_item,
                discont: false
            }
        );

        let rtp_data = generate_rtp_packet(0x12345678, 1, 0, 4);
        let packet = RtpPacket::parse(&rtp_data).unwrap();
        let QueueResult::Forward {
            id: id_second_packet,
            discont: discont_second_packet,
        } = jb.queue_packet(ORIGIN, &packet, 0, now)
        else {
            unreachable!()
        };
        assert_eq!(id_second_packet, id_second_serialized_item + 1);
        assert!(!discont_second_packet);
        assert_eq!(jb.poll(ORIGIN, now), PollResult::Empty);
    }

    #[test]
    fn flushing_queue() {
        const ORIGIN: &str = "flushing_queue";

        let mut jb = JitterBuffer::new(Duration::from_secs(1));
        jb.set_flushing(ORIGIN, false);

        let now = Instant::now();

        let rtp_data = generate_rtp_packet(0x12345678, 0, 0, 4);
        let packet = RtpPacket::parse(&rtp_data).unwrap();

        let QueueResult::Forward { .. } = jb.queue_serialized_item("flushing_queue") else {
            unreachable!()
        };

        let QueueResult::Queued(id_first) = jb.queue_packet(ORIGIN, &packet, 0, now) else {
            unreachable!()
        };

        let QueueResult::Queued(id_second_serialized_item) =
            jb.queue_serialized_item("flushing_queue")
        else {
            unreachable!()
        };

        // Everything after this should eventually return flushing, poll() will instruct to drop
        // everything stored and then return flushing indefinitely.
        jb.set_flushing("flushing_queue", true);
        assert_eq!(
            jb.queue_packet(ORIGIN, &packet, 0, now),
            QueueResult::Flushing
        );

        assert_eq!(jb.poll(ORIGIN, now), PollResult::Drop(id_first));
        assert_eq!(
            jb.poll(ORIGIN, now),
            PollResult::Drop(id_second_serialized_item)
        );
        assert_eq!(jb.poll(ORIGIN, now), PollResult::Flushing);
        assert_eq!(jb.poll(ORIGIN, now), PollResult::Flushing);

        jb.set_flushing("flushing_queue", false);
        assert_eq!(jb.poll(ORIGIN, now), PollResult::Empty);
    }
}
