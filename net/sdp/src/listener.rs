// Copyright (C) 2026 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! Process-wide SAP listener shared between all elements of this plugin.
//!
//! Sessions announced on the SAP multicast addresses are cached, and all
//! changes are passed to the callbacks registered via
//! [`ListenerHandle::listen`].
//!
//! In addition to the standard SAP multicast groups, the comma-separated
//! `GST_SAP_ADDRESSES` environment variable can list further multicast groups
//! to listen on.

use std::{
    collections::HashMap,
    fmt, io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{
        Arc, LazyLock, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context, anyhow, bail};
use mio::{Events, Interest, Poll, Token, Waker, net::UdpSocket};
use rand::RngExt as _;
use slab::Slab;
use socket2::{Domain, Protocol, Socket as RawSocket, Type};

use crate::message::{MessageType, SapMessage};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "saplistener",
        gst::DebugColorFlags::empty(),
        Some("SAP Listener"),
    )
});

pub(crate) const SAP_PORT: u16 = 9875;
pub(crate) const GLOBAL_V4_MCAST_GROUP: IpAddr = IpAddr::V4(Ipv4Addr::new(224, 2, 127, 254));
pub(crate) const ORG_LOCAL_V4_MCAST_GROUP: IpAddr = IpAddr::V4(Ipv4Addr::new(239, 195, 255, 255));
pub(crate) const SITE_LOCAL_V4_MCAST_GROUP: IpAddr = IpAddr::V4(Ipv4Addr::new(239, 255, 255, 255));
pub(crate) const LINK_LOCAL_V4_MCAST_GROUP: IpAddr = IpAddr::V4(Ipv4Addr::new(224, 0, 0, 255));
/// Global-scope IPv6 SAP group, scope 0xe of `scoped_v6_mcast_group`
pub(crate) const GLOBAL_V6_MCAST_GROUP: IpAddr = IpAddr::V6(Ipv6Addr::from_segments([
    0xff0e, 0, 0, 0, 0, 0, 0x0002, 0x7ffe,
]));

/// The IPv6 SAP multicast group for a scope (RFC 3513 §2.4), `None` for
/// invalid scopes
pub(crate) fn scoped_v6_mcast_group(scope: u8) -> Option<IpAddr> {
    match scope {
        1..=0x0f => Some(IpAddr::V6(Ipv6Addr::from_segments([
            0xff00 | u16::from(scope),
            0x0000,
            0x0000,
            0x0000,
            0x0000,
            0x0000,
            0x0002,
            0x7ffe,
        ]))),
        _ => None,
    }
}

/// Scoped IPv6 SAP multicast groups, one per multicast scope
fn scoped_v6_mcast_groups() -> impl Iterator<Item = IpAddr> {
    (1..=0x0f).filter_map(scoped_v6_mcast_group)
}

const WAKER: Token = Token(0);

/// Advisory size limit for senders: announcements should fit in a single
/// datagram to avoid fragmentation, so this is the default Ethernet MTU
const MAX_ANNOUNCEMENT_SIZE: usize = 1500;

/// Largest possible UDP datagram payload: other implementations send
/// announcements larger than an Ethernet MTU, receive buffers must not
/// truncate them
const MAX_PACKET_SIZE: usize = 65507;

/// Also the granularity of the session expiry timeout
const POLL_TIMEOUT: Duration = Duration::from_secs(5);

/// Interval between periodic announcements, with the RFC 2974 §3.1 jitter
/// of 2/3-4/3x. Receivers keep sessions alive for at least 10x this interval.
///
/// FIXME: The interval should be derived from the number of announced
/// sessions, the announcement size and the bandwidth limit, 4000 bits per
/// second by default, per SAP group (RFC 2974 §3.1)
const ANNOUNCE_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Time after which observed sessions expire if not announced anymore.
/// Sessions are deleted after ten times the announcement period, or one
/// hour, whichever is greater (RFC 2974 §4).
const SESSION_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// Total number of sends for a session deletion before giving up
const DELETION_SENDS: u32 = 3;

/// Spacing between session deletion re-sends
const DELETION_RETRY_INTERVAL: Duration = Duration::from_secs(1);

pub(crate) fn parse_multicast_groups(addresses: &str) -> Vec<IpAddr> {
    let mut groups = Vec::new();
    for address in addresses.split(',') {
        let address = address.trim();
        if address.is_empty() {
            continue;
        }

        match address.parse::<IpAddr>() {
            Ok(group) if group.is_multicast() => groups.push(group),
            Ok(_) => {
                gst::warning!(CAT, "Ignoring non-multicast address {address}")
            }
            Err(err) => {
                gst::warning!(CAT, "Ignoring invalid address {address}: {err}")
            }
        }
    }

    groups
}

pub type ListenerCallback = Arc<dyn Fn(&Session, SessionEvent) + Send + Sync>;

/// Session identity per RFC 2974 §5: the originating source address and the
/// session ID of the SDP origin line. The message ID hash must change
/// whenever the session is modified, so it cannot be part of the identity.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SessionKey {
    pub sess_id: String,
    pub source: IpAddr,
}

impl SessionKey {
    fn new(sdp: &sdp_types_gst::Session, addr: IpAddr) -> SessionKey {
        SessionKey {
            sess_id: sdp.origin().sess_id.clone(),
            source: addr,
        }
    }
}

/// Whether this is a session announced by this process or one observed on the network.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionSource {
    /// Announced by this process
    Announced,
    /// Observed on the network
    Observed,
}

/// Session cache change passed to listener callbacks
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionEvent {
    Added,
    Updated,
    /// Explicitly removed by unregistering a session announced by this
    /// process
    Removed,
    /// Not announced anymore for long enough
    Expired,
}

/// A session announced via SAP
#[derive(Clone, Debug)]
pub struct Session {
    pub sdp: sdp_types_gst::Session,
    pub originating_source: IpAddr,
    pub msg_id_hash: u16,
    pub source: SessionSource,
    pub last_update: Instant,
}

impl Session {
    pub fn key(&self) -> SessionKey {
        SessionKey::new(&self.sdp, self.originating_source)
    }
}

/// ID of a session registered via [`ListenerHandle::register_session`]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionId(usize);

/// Singleton listener instances.
///
/// Shut down once the last user disappeared.
static LISTENER: Mutex<Weak<Listener>> = Mutex::new(Weak::new());

/// A session announced by this process
struct RegisteredSession {
    /// Session identity, used to recognize our own received announcements
    /// via multicast loopback and to remove the session from the cache
    key: SessionKey,
    sdp: sdp_types_gst::Session,
    addr: IpAddr,
    /// Addresses to announce on
    groups: Vec<IpAddr>,
    /// Names of the interfaces the session is sent on, to announce on a
    /// single socket per group and interface. `None` announces on all
    /// interfaces
    interfaces: Option<Vec<String>>,
    /// Whether to compress the announcement payload with zlib
    compress: bool,
    /// When the next announcement is due
    next_announce: Instant,
}

/// Queued session deletion announcement to be sent by the poller
struct PendingDeletion {
    packet: Vec<u8>,
    groups: Vec<IpAddr>,
    interfaces: Option<Vec<String>>,
    sends_left: u32,
    next_send: Instant,
}

/// Timing configuration for the poller
#[derive(Clone, Copy, Debug)]
struct Timeouts {
    /// Maximum time spent in one poll, granularity of the session expiry
    poll: Duration,
    /// Base interval between periodic announcements, actual intervals are
    /// jittered to 2/3-4/3x of this (RFC 2974 §3.1)
    announce: Duration,
    /// Time after which observed sessions expire if not announced anymore
    session_timeout: Duration,
    /// Spacing between session deletion re-sends
    deletion_retry: Duration,
    /// Total number of sends for one session deletion
    deletion_sends: u32,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            poll: POLL_TIMEOUT,
            announce: ANNOUNCE_INTERVAL,
            session_timeout: SESSION_TIMEOUT,
            deletion_retry: DELETION_RETRY_INTERVAL,
            deletion_sends: DELETION_SENDS,
        }
    }
}

/// State shared between the poller thread and the listener handles
struct PollerState {
    waker: Waker,
    shutdown: AtomicBool,
    timeouts: Timeouts,
    sessions: Mutex<HashMap<SessionKey, Session>>,
    listeners: Mutex<Slab<ListenerCallback>>,
    registered: Mutex<Slab<RegisteredSession>>,
    deletions: Mutex<Vec<PendingDeletion>>,
}

/// An announcement that became due during a poll iteration, to be sent
/// outside of the locks
struct DueAnnouncement {
    packet: Vec<u8>,
    groups: Vec<IpAddr>,
    interfaces: Option<Vec<String>>,
    sess_id: String,
}

impl PollerState {
    /// Notifies all registered callbacks about a session change.
    fn notify_listeners(&self, session: &Session, event: SessionEvent) {
        let listeners = self
            .listeners
            .lock()
            .unwrap()
            .iter()
            .map(|(_, listener)| listener.clone())
            .collect::<Vec<_>>();

        for listener in listeners {
            listener(session, event);
        }
    }

    /// Inserts or updates a session in the cache, returning the session and
    /// the change event. Sessions that did not change return `None`.
    fn cache_session(
        &self,
        key: SessionKey,
        sdp: &sdp_types_gst::Session,
        msg_id_hash: u16,
        source: SessionSource,
        now: Instant,
    ) -> Option<(Session, SessionEvent)> {
        let mut sessions = self.sessions.lock().unwrap();

        match sessions.get_mut(&key) {
            Some(session) => {
                let sdp_changed = session.sdp != *sdp;
                let event =
                    (sdp_changed || session.source != source).then_some(SessionEvent::Updated);
                if sdp_changed {
                    session.sdp = sdp.clone();
                }
                session.msg_id_hash = msg_id_hash;
                session.source = source;
                session.last_update = now;
                event.map(|event| (session.clone(), event))
            }
            None => {
                let session = Session {
                    sdp: sdp.clone(),
                    originating_source: key.source,
                    msg_id_hash,
                    source,
                    last_update: now,
                };
                sessions.insert(key, session.clone());
                Some((session, SessionEvent::Added))
            }
        }
    }

    /// Handles a received SAP packet, updating the session cache and
    /// notifying listeners.
    fn handle_packet(&self, packet: &[u8], now: Instant) {
        match SapMessage::parse(packet) {
            Err(err) => gst::debug!(CAT, "Ignoring invalid SAP packet: {err}"),
            Ok(message) => {
                let key = SessionKey {
                    sess_id: message.session.origin().sess_id.clone(),
                    source: message.originating_source,
                };

                if self
                    .registered
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(_, session)| session.key == key)
                {
                    // One of our own announcements received via multicast
                    // loopback: never overwrite sessions announced by this
                    // process with observed ones
                } else if message.message_type == MessageType::Deletion {
                    // RFC 2974: sessions MUST NOT be removed from the cache
                    // based on deletion packets: without the (unsupported)
                    // authentication header, and with a spoofable source
                    // address, any host could delete foreign sessions.
                    // Unauthenticated deletions are ignored, relying on the
                    // implicit expiry instead.
                    gst::debug!(
                        CAT,
                        "Ignoring SAP session deletion from {} (hash {:04x})",
                        message.originating_source,
                        message.msg_id_hash
                    );
                } else if let Some((session, event)) = self.cache_session(
                    key,
                    &message.session,
                    message.msg_id_hash,
                    SessionSource::Observed,
                    now,
                ) {
                    self.notify_listeners(&session, event);
                }
            }
        }
    }

    /// Expire observed sessions, announced ones are only removed by
    /// unregistering them.
    fn expire_sessions(&self, now: Instant) {
        // TODO: RFC 2974 §4 explicit timeout: also delete observed sessions
        // whose SDP t= stop time is in the past (0 = permanent), e.g. parsed
        // in handle_packet() and stored alongside last_update
        let expired = {
            let mut sessions = self.sessions.lock().unwrap();
            let mut expired = Vec::new();
            sessions.retain(|_, session| {
                if session.source == SessionSource::Observed
                    && now.saturating_duration_since(session.last_update)
                        > self.timeouts.session_timeout
                {
                    expired.push(session.clone());
                    false
                } else {
                    true
                }
            });

            expired
        };

        for session in &expired {
            gst::debug!(
                CAT,
                "Expiring SAP session {} ({})",
                session.sdp.origin().sess_id,
                session.sdp.name()
            );
            self.notify_listeners(session, SessionEvent::Expired);
        }
    }

    /// Collects all announcements that are due at `now` and reschedules
    /// them. `jitter` returns the next interval until an announcement is due.
    fn due_announcements(
        &self,
        now: Instant,
        jitter: &mut dyn FnMut() -> Duration,
    ) -> Vec<DueAnnouncement> {
        let mut registered = self.registered.lock().unwrap();
        let mut due = Vec::new();
        for (_, session) in registered.iter_mut() {
            if session.next_announce > now {
                continue;
            }

            session.next_announce = now + jitter();

            match SapMessage::announcement(&session.sdp, session.addr).serialize(session.compress) {
                Err(err) => gst::warning!(CAT, "Failed to serialize SAP announcement: {err}"),
                Ok(packet) => due.push(DueAnnouncement {
                    packet,
                    groups: session.groups.clone(),
                    interfaces: session.interfaces.clone(),
                    sess_id: session.sdp.origin().sess_id.clone(),
                }),
            }
        }
        due
    }

    /// Sends all queued deletions that are due at `now` via `send`, removing
    /// them once sent and retrying up to `Timeouts::deletion_sends` times.
    ///
    /// The sends happen without the deletions mutex held: they can block on
    /// the sockets, and new deletions queued in between are appended
    /// afterwards.
    fn process_deletions(
        &self,
        now: Instant,
        mut send: impl FnMut(&[u8], &[IpAddr], Option<&[String]>) -> bool,
    ) {
        let mut deletions = std::mem::take(&mut *self.deletions.lock().unwrap());

        deletions.retain_mut(|deletion| {
            if deletion.next_send > now {
                return true;
            }
            if send(
                &deletion.packet,
                &deletion.groups,
                deletion.interfaces.as_deref(),
            ) {
                return false;
            }
            if deletion.sends_left <= 1 {
                gst::warning!(
                    CAT,
                    "Giving up on sending SAP session deletion after {} attempts",
                    self.timeouts.deletion_sends
                );
                return false;
            }
            deletion.sends_left -= 1;
            deletion.next_send = now + self.timeouts.deletion_retry;
            true
        });

        self.deletions.lock().unwrap().extend(deletions);
    }

    /// Poll timeout that wakes up in time for the next announcement or
    /// deletion send.
    fn next_poll_timeout(&self, now: Instant) -> Duration {
        let mut timeout = self.timeouts.poll;
        for (_, session) in self.registered.lock().unwrap().iter() {
            timeout = timeout.min(session.next_announce.saturating_duration_since(now));
        }
        for deletion in self.deletions.lock().unwrap().iter() {
            timeout = timeout.min(deletion.next_send.saturating_duration_since(now));
        }
        timeout
    }

    /// Registers a new session to be announced.
    fn register_session(
        &self,
        sdp: &sdp_types_gst::Session,
        addr: IpAddr,
        groups: Vec<IpAddr>,
        interfaces: Option<Vec<String>>,
        compress: bool,
        now: Instant,
    ) -> SessionId {
        let key = SessionKey::new(sdp, addr);

        // RFC 2974 §6: announcements should fit into a single packet to avoid
        // fragmentation. Larger ones are truncated by the receive buffer of
        // listeners and cannot be parsed at all.
        if let Ok(packet) = SapMessage::announcement(sdp, addr).serialize(compress)
            && packet.len() > MAX_ANNOUNCEMENT_SIZE
        {
            gst::warning!(
                CAT,
                "SAP announcement of session {} is {} bytes, larger than the recommended \
                 maximum size of {MAX_ANNOUNCEMENT_SIZE} bytes, listeners might not be able to parse it",
                sdp.origin().sess_id,
                packet.len()
            );
        }

        let session_id = SessionId(self.registered.lock().unwrap().insert(RegisteredSession {
            key: key.clone(),
            sdp: sdp.clone(),
            addr,
            groups,
            interfaces,
            compress,
            // Announce as soon as possible
            next_announce: now,
        }));

        // FIXME: Should we fail here if trying to announce a session that already exists to avoid
        // conflicts with other sessions on the network?
        if let Some((session, event)) = self.cache_session(
            key,
            sdp,
            crate::message::msg_id_hash(sdp),
            SessionSource::Announced,
            now,
        ) {
            if event == SessionEvent::Updated {
                gst::warning!(
                    CAT,
                    "Replacing already known session {} announced from {addr}",
                    session.sdp.origin().sess_id
                );
            }

            self.notify_listeners(&session, event);
        }

        let _ = self.waker.wake();

        session_id
    }

    /// Removes a registered session and notifies the listeners.
    fn remove_registered_session(&self, session_id: SessionId, now: Instant) {
        let Some(session) = self.registered.lock().unwrap().try_remove(session_id.0) else {
            return;
        };

        // Send a SAP deletion packet for this session to inform other hosts.
        // Receivers are supposed to ignore unauthenticated deletions, so this
        // is best-effort for stacks that honor them. It is sent multiple
        // times because a single lost deletion would keep the session in
        // foreign caches until it expires there.
        match SapMessage::deletion(&session.sdp, session.addr).serialize(session.compress) {
            Err(err) => gst::warning!(CAT, "Failed to serialize SAP session deletion: {err}"),
            Ok(packet) => {
                self.deletions.lock().unwrap().push(PendingDeletion {
                    packet,
                    groups: session.groups,
                    interfaces: session.interfaces,
                    sends_left: self.timeouts.deletion_sends,
                    next_send: now,
                });
            }
        }

        // The mutex must not be held while notifying: the callbacks queue
        // events and post bus messages
        let removed = self.sessions.lock().unwrap().remove(&session.key);
        if let Some(removed) = removed {
            self.notify_listeners(&removed, SessionEvent::Removed);
        }

        let _ = self.waker.wake();
    }
}

struct Socket {
    socket: UdpSocket,
    group: IpAddr,
    interface: String,
}

struct Listener {
    state: Arc<PollerState>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

pub struct ListenerHandle {
    listener: Arc<Listener>,
    session_ids: Vec<SessionId>,
    listener_ids: Mutex<Vec<usize>>,
}

impl fmt::Debug for ListenerHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ListenerHandle").finish_non_exhaustive()
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.state.shutdown.store(true, Ordering::SeqCst);
        let _ = self.state.waker.wake();

        if let Some(thread) = self.thread.lock().unwrap().take() {
            let _ = thread.join();
        }

        gst::debug!(CAT, "Stopped SAP listener");
    }
}

impl Drop for ListenerHandle {
    fn drop(&mut self) {
        for session_id in std::mem::take(&mut self.session_ids) {
            self.listener
                .state
                .remove_registered_session(session_id, Instant::now());
        }

        let listener_ids = std::mem::take(&mut *self.listener_ids.lock().unwrap());
        let mut listeners = self.listener.state.listeners.lock().unwrap();
        for listener_id in listener_ids {
            listeners.remove(listener_id);
        }
    }
}

impl Listener {
    fn create() -> Result<(PollerState, Poll, Slab<Socket>), anyhow::Error> {
        let poll = Poll::new().context("Failed to create poll")?;
        let waker = Waker::new(poll.registry(), WAKER).context("Failed to create waker")?;

        let mut groups = [
            GLOBAL_V4_MCAST_GROUP,
            ORG_LOCAL_V4_MCAST_GROUP,
            SITE_LOCAL_V4_MCAST_GROUP,
            LINK_LOCAL_V4_MCAST_GROUP,
        ]
        .into_iter()
        .chain(scoped_v6_mcast_groups())
        .chain(if let Ok(addresses) = std::env::var("GST_SAP_ADDRESSES") {
            parse_multicast_groups(&addresses)
        } else {
            Vec::new()
        })
        .collect::<Vec<IpAddr>>();
        groups.sort();
        groups.dedup();

        let ifaces = getifaddrs::getifaddrs()
            .context("Failed to enumerate network interfaces")?
            .collect::<getifaddrs::Interfaces>();

        let mut sockets = Slab::new();

        for iface in ifaces.values() {
            if !iface.flags.contains(getifaddrs::InterfaceFlags::UP)
                || !iface.flags.contains(getifaddrs::InterfaceFlags::MULTICAST)
            {
                gst::trace!(
                    CAT,
                    "Skipping interface {}: not up or no multicast support",
                    iface.name
                );
                continue;
            }

            let Some(ifindex) = iface.index else {
                continue;
            };

            let v4_addr = iface
                .address
                .get_all(getifaddrs::AddressFamily::V4)
                .and_then(|addresses| {
                    addresses.iter().find_map(|address| match address {
                        getifaddrs::Address::V4(address) => Some(address.address),
                        _ => None,
                    })
                });
            let has_v6 = iface.address.has(getifaddrs::AddressFamily::V6);

            for group in &groups {
                if (group.is_ipv4() && v4_addr.is_none()) || (group.is_ipv6() && !has_v6) {
                    continue;
                }

                let mut socket = match create_socket(*group, v4_addr, ifindex) {
                    Ok(socket) => socket,
                    Err(err) => {
                        gst::warning!(
                            CAT,
                            "Failed listening on SAP multicast group {group} on interface {}: {err}",
                            iface.name
                        );
                        continue;
                    }
                };

                let entry = sockets.vacant_entry();
                let key = entry.key();
                if let Err(err) =
                    poll.registry()
                        .register(&mut socket, Token(key + 1), Interest::READABLE)
                {
                    gst::warning!(
                        CAT,
                        "Failed registering socket for SAP multicast group {group} on interface {}: {err}",
                        iface.name
                    );
                    continue;
                }

                gst::debug!(
                    CAT,
                    "Listening on SAP multicast group {group} on interface {}",
                    iface.name
                );
                entry.insert(Socket {
                    socket,
                    group: *group,
                    interface: iface.name.clone(),
                });
            }
        }

        if sockets.is_empty() {
            return Err(anyhow!(
                "Failed to join any SAP multicast group on any interface"
            ));
        }

        gst::info!(
            CAT,
            "Listening for SAP packets on {} sockets",
            sockets.len()
        );

        let state = PollerState {
            waker,
            shutdown: AtomicBool::new(false),
            timeouts: Timeouts::default(),
            sessions: Mutex::new(HashMap::new()),
            listeners: Mutex::new(Slab::new()),
            registered: Mutex::new(Slab::new()),
            deletions: Mutex::new(Vec::new()),
        };

        Ok((state, poll, sockets))
    }
}

/// Creates a UDP socket listening on the SAP port, joined to `group` on the
/// interface with the given index. `v4_addr` is a local address of that
/// interface and required for IPv4 groups.
fn create_socket(
    group: IpAddr,
    v4_addr: Option<Ipv4Addr>,
    ifindex: u32,
) -> Result<UdpSocket, anyhow::Error> {
    let bind_addr = if group.is_ipv4() {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), SAP_PORT)
    } else {
        SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), SAP_PORT)
    };
    let socket = RawSocket::new(
        Domain::for_address(bind_addr),
        Type::DGRAM,
        Some(Protocol::UDP),
    )?;
    socket.set_reuse_address(true)?;
    #[cfg(not(any(
        windows,
        target_os = "solaris",
        target_os = "illumos",
        target_os = "cygwin"
    )))]
    socket.set_reuse_port(true)?;
    if group.is_ipv6() {
        socket.set_only_v6(true)?;
    }

    socket.bind(&bind_addr.into())?;

    // Multiple SAP listeners can run on the same host, e.g. in different
    // processes, and have to receive the announcements sent by each other.
    match group {
        IpAddr::V4(group) => {
            let v4_addr = v4_addr.expect("checked by caller");
            // TODO: Should use join_multicast_v4_n() on platforms where that
            // is available
            socket.join_multicast_v4(&group, &v4_addr)?;
            socket.set_multicast_if_v4(&v4_addr)?;
            // RFC 2974 §3: announcements SHOULD be sent with an IP
            // time-to-live of 255
            socket.set_multicast_ttl_v4(255)?;
            socket.set_multicast_loop_v4(true)?;
        }
        IpAddr::V6(group) => {
            socket.join_multicast_v6(&group, ifindex)?;
            socket.set_multicast_if_v6(ifindex)?;
            socket.set_multicast_hops_v6(255)?;
            socket.set_multicast_loop_v6(true)?;
        }
    }

    let _ = socket.set_recv_buffer_size(64 * 1024);
    let _ = socket.set_send_buffer_size(64 * 1024);

    socket.set_nonblocking(true)?;

    Ok(UdpSocket::from_std(socket.into()))
}

/// Sends a packet to the SAP port of each group, on the sockets of the given
/// interfaces if the listener has them for that group, otherwise on all
/// sockets of the group. Returns `false` if any send failed or if a group has
/// no socket at all.
fn send_packet(
    sockets: &Slab<Socket>,
    groups: &[IpAddr],
    interfaces: Option<&[String]>,
    packet: &[u8],
    what: &str,
) -> bool {
    let mut sent = true;
    for group in groups {
        let matching = sockets
            .iter()
            .filter(|(_, socket)| socket.group == *group)
            .map(|(_, socket)| socket)
            .collect::<Vec<_>>();

        let selected = match interfaces {
            Some(interfaces) => {
                let preferred = matching
                    .iter()
                    .copied()
                    .filter(|socket| interfaces.contains(&socket.interface))
                    .collect::<Vec<_>>();
                if preferred.is_empty() {
                    matching
                } else {
                    preferred
                }
            }
            None => matching,
        };

        if selected.is_empty() {
            gst::warning!(
                CAT,
                "No socket for SAP group {group} on interface(s) {}, not sending SAP {what}",
                interfaces
                    .map(|interfaces| interfaces.join(","))
                    .unwrap_or_else(|| "all".to_string())
            );
            sent = false;
            continue;
        }

        for socket in selected {
            let dest = SocketAddr::new(*group, SAP_PORT);
            match socket.socket.send_to(packet, dest) {
                Ok(_) => (),
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    // Non-blocking sends only: skip on a full send buffer and
                    // wait for the next periodic announcement, a lost refresh
                    // is not a problem. Never block or register
                    // Interest::WRITABLE.
                    gst::warning!(
                        CAT,
                        "Failed to send SAP {what} on group {} interface {}: send buffer full",
                        socket.group,
                        socket.interface
                    );
                    sent = false;
                }
                Err(err) => {
                    gst::warning!(
                        CAT,
                        "Failed to send SAP {what} on group {} interface {}: {err}",
                        socket.group,
                        socket.interface
                    );
                    sent = false;
                }
            }
        }
    }
    sent
}

fn poller_thread(state: Arc<PollerState>, mut poll: Poll, sockets: Slab<Socket>) {
    gst::debug!(CAT, "Starting SAP listener thread");

    let mut events = Events::with_capacity(256);
    let mut buffer = [0u8; MAX_PACKET_SIZE];

    loop {
        if state.shutdown.load(Ordering::SeqCst) {
            break;
        }

        // Wake up in time for the next announcement or deletion send
        let timeout = state.next_poll_timeout(Instant::now());

        match poll.poll(&mut events, Some(timeout)) {
            Ok(()) => (),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => {
                gst::error!(CAT, "Failed polling SAP sockets: {err}");

                // Try again later
                thread::sleep(Duration::from_millis(100));
                continue;
            }
        }

        for event in &events {
            match event.token() {
                // Shutdown flag is checked at the top of the loop
                WAKER => (),
                token => {
                    let Some(socket) = sockets.get(token.0 - 1) else {
                        continue;
                    };

                    loop {
                        match socket.socket.recv_from(&mut buffer) {
                            Ok((size, from)) => {
                                gst::debug!(
                                    CAT,
                                    "Received {size} bytes SAP packet on group {} interface {} from {from}",
                                    socket.group,
                                    socket.interface
                                );
                                if size == buffer.len() {
                                    // Datagrams bigger than the buffer are
                                    // silently truncated, the incomplete SDP
                                    // would then fail to parse
                                    gst::warning!(
                                        CAT,
                                        "Received SAP packet of maximum size on group {} \
                                         interface {}, it might have been truncated",
                                        socket.group,
                                        socket.interface
                                    );
                                }

                                state.handle_packet(&buffer[..size], Instant::now());
                            }
                            Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                            Err(err) => {
                                gst::warning!(
                                    CAT,
                                    "Failed receiving SAP packet on group {} interface {}: {err}",
                                    socket.group,
                                    socket.interface
                                );
                                break;
                            }
                        }
                    }
                }
            }
        }

        let now = Instant::now();
        state.expire_sessions(now);

        // Announce/refresh all sessions that are due now
        let due = state.due_announcements(now, &mut || {
            // RFC 2974 §3.1: interval + rand(interval * 2/3) - interval/3,
            // which avoids synchronizing with other senders
            let interval = state.timeouts.announce;
            let range = interval * 2 / 3;
            interval - interval / 3 + range * rand::rng().random_range(0u32..1000) / 1000
        });

        for announcement in &due {
            gst::debug!(CAT, "Announcing SAP session {}", announcement.sess_id);
            send_packet(
                &sockets,
                &announcement.groups,
                announcement.interfaces.as_deref(),
                &announcement.packet,
                "announcement",
            );
        }

        // Send queued session deletions, repeatedly to not lose them
        state.process_deletions(now, |packet, groups, interfaces| {
            send_packet(&sockets, groups, interfaces, packet, "deletion")
        });
    }

    gst::debug!(CAT, "Stopped SAP listener thread");
}

impl ListenerHandle {
    /// Acquire a handle to the listener.
    pub fn acquire() -> Result<ListenerHandle, anyhow::Error> {
        let mut listener_guard = LISTENER.lock().unwrap();
        let listener = match listener_guard.upgrade() {
            Some(listener) => listener,
            None => {
                let (state, poll, sockets) = Listener::create()?;
                let state = Arc::new(state);
                let poller_state = state.clone();
                let thread = thread::Builder::new()
                    .name("saplistener".into())
                    .spawn(move || poller_thread(poller_state, poll, sockets))
                    .context("Failed to spawn SAP listener thread")?;
                let listener = Arc::new(Listener {
                    state,
                    thread: Mutex::new(Some(thread)),
                });
                *listener_guard = Arc::downgrade(&listener);
                listener
            }
        };

        Ok(ListenerHandle {
            listener,
            session_ids: Vec::new(),
            listener_ids: Mutex::new(Vec::new()),
        })
    }

    /// Registers a session.
    pub fn register_session(
        &mut self,
        sdp: &sdp_types_gst::Session,
        addr: IpAddr,
        groups: &[IpAddr],
        interfaces: Option<&[String]>,
        compress: bool,
    ) -> Result<SessionId, anyhow::Error> {
        if groups.is_empty() {
            bail!("No SAP multicast groups to announce on");
        }

        // FIXME: Do we need to check if sdp.origin.unicast_address matches addr and maybe remove
        // addr?
        // TODO: Support re-announcing updated sessions, e.g. when streams are
        // added to sapsink after the initial announcement.
        let session_id = self.listener.state.register_session(
            sdp,
            addr,
            groups.to_vec(),
            interfaces.map(<[String]>::to_vec),
            compress,
            Instant::now(),
        );
        self.session_ids.push(session_id);

        Ok(session_id)
    }

    /// Registers a callback for session changes.
    pub fn listen(&self, func: impl Fn(&Session, SessionEvent) + Send + Sync + 'static) {
        let func = Arc::new(func);
        let listener_id = {
            let mut listeners = self.listener.state.listeners.lock().unwrap();
            listeners.insert(func.clone())
        };
        self.listener_ids.lock().unwrap().push(listener_id);

        // Send all currently known sessions to the new callback
        let sessions = self
            .listener
            .state
            .sessions
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for session in &sessions {
            func(session, SessionEvent::Added);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, sync::mpsc};

    const REMOTE: IpAddr = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
    const LOCAL: IpAddr = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2));
    const TEST_HASH: u16 = 0x1234;

    // The waker needs a poll instance, which is never polled here
    fn test_state(timeouts: Timeouts) -> (Poll, PollerState) {
        let poll = Poll::new().unwrap();
        let waker = Waker::new(poll.registry(), WAKER).unwrap();

        (
            poll,
            PollerState {
                waker,
                shutdown: AtomicBool::new(false),
                timeouts,
                sessions: Mutex::new(HashMap::new()),
                listeners: Mutex::new(Slab::new()),
                registered: Mutex::new(Slab::new()),
                deletions: Mutex::new(Vec::new()),
            },
        )
    }

    fn add_test_listener(state: &PollerState) -> mpsc::Receiver<(u16, SessionEvent)> {
        let (sender, receiver) = mpsc::channel();
        state.listeners.lock().unwrap().insert(Arc::new(
            move |session: &Session, event: SessionEvent| {
                let _ = sender.send((session.msg_id_hash, event));
            },
        ));
        receiver
    }

    fn assert_no_event(receiver: &mpsc::Receiver<(u16, SessionEvent)>) {
        assert!(
            matches!(receiver.try_recv(), Err(mpsc::TryRecvError::Empty)),
            "expected no event"
        );
    }

    fn test_sdp(name: &str) -> sdp_types_gst::Session {
        sdp_types_gst::Session::builder()
            .origin(sdp_types::Origin::with_ip_addr(
                "test-session",
                1,
                [192, 168, 1, 2],
            ))
            .name(name.to_string())
            .build()
    }

    fn announcement(sdp: &sdp_types_gst::Session, source: IpAddr, msg_id_hash: u16) -> Vec<u8> {
        let mut message = SapMessage::announcement(sdp, source);
        message.msg_id_hash = msg_id_hash;
        message.serialize(false).unwrap()
    }

    // Deterministic base time far in the past
    fn t0() -> Instant {
        Instant::now()
            .checked_sub(Duration::from_secs(3600))
            .unwrap()
    }

    #[test]
    fn observed_add_update_dedupe() {
        let (_poll, state) = test_state(Timeouts::default());
        let events = add_test_listener(&state);
        let now = t0();

        state.handle_packet(&announcement(&test_sdp("a"), REMOTE, TEST_HASH), now);
        assert_eq!(events.try_recv(), Ok((TEST_HASH, SessionEvent::Added)));

        // Unchanged sessions do not cause any event
        state.handle_packet(&announcement(&test_sdp("a"), REMOTE, TEST_HASH), now);
        assert_no_event(&events);

        // Sessions are identified by the SDP origin session ID and the
        // originating source per RFC 2974 §5, not by the message ID hash:
        // a modified session with a changed hash updates the same session
        state.handle_packet(&announcement(&test_sdp("b"), REMOTE, TEST_HASH + 1), now);
        assert_eq!(
            events.try_recv(),
            Ok((TEST_HASH + 1, SessionEvent::Updated))
        );
        assert_no_event(&events);

        // The same origin session ID from another source is a separate session
        let other = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 3));
        state.handle_packet(&announcement(&test_sdp("c"), other, TEST_HASH), now);
        assert_eq!(events.try_recv(), Ok((TEST_HASH, SessionEvent::Added)));
        assert_no_event(&events);

        let sessions = state.sessions.lock().unwrap();
        assert_eq!(sessions.len(), 2);
        let key = SessionKey {
            sess_id: "test-session".to_string(),
            source: REMOTE,
        };
        assert_eq!(sessions[&key].sdp.name(), "b");
        assert_eq!(sessions[&key].source, SessionSource::Observed);
    }

    #[test]
    fn own_announcements_ignored() {
        let (_poll, state) = test_state(Timeouts::default());
        let events = add_test_listener(&state);
        let now = t0();

        state.register_session(
            &test_sdp("a"),
            LOCAL,
            vec![GLOBAL_V4_MCAST_GROUP],
            None,
            false,
            now,
        );
        let our_hash = crate::message::msg_id_hash(&test_sdp("a"));
        assert_eq!(events.try_recv(), Ok((our_hash, SessionEvent::Added)));

        // Own announcement received via multicast loopback
        state.handle_packet(&announcement(&test_sdp("a"), LOCAL, our_hash), now);
        // Foreign announcement with a colliding hash must not overwrite it
        state.handle_packet(&announcement(&test_sdp("evil"), LOCAL, our_hash), now);
        assert_no_event(&events);

        let sessions = state.sessions.lock().unwrap();
        assert_eq!(sessions.len(), 1);
        let session = sessions.values().next().unwrap();
        assert_eq!(session.source, SessionSource::Announced);
        assert_eq!(session.sdp.name(), "a");
    }

    #[test]
    fn deletions_ignored() {
        let (_poll, state) = test_state(Timeouts::default());
        let events = add_test_listener(&state);
        let now = t0();

        state.handle_packet(&announcement(&test_sdp("a"), REMOTE, TEST_HASH), now);
        assert_eq!(events.try_recv(), Ok((TEST_HASH, SessionEvent::Added)));

        // Unauthenticated deletions are ignored, sessions expire instead
        let deletion = SapMessage::deletion(&test_sdp("a"), REMOTE);
        state.handle_packet(&deletion.serialize(false).unwrap(), now);
        assert_no_event(&events);
        assert_eq!(state.sessions.lock().unwrap().len(), 1);
    }

    #[test]
    fn invalid_packets_ignored() {
        let (_poll, state) = test_state(Timeouts::default());
        let events = add_test_listener(&state);
        let now = t0();

        // Truncated SAP header, non-SAP data and an SDP without version line
        for packet in [&[0x04, 0x00][..], b"garbage", b"no version line\r\n"] {
            state.handle_packet(packet, now);
        }
        assert_no_event(&events);
        assert!(state.sessions.lock().unwrap().is_empty());
    }

    #[test]
    fn expiry_boundary() {
        let timeouts = Timeouts {
            session_timeout: Duration::from_secs(10),
            ..Timeouts::default()
        };
        let (_poll, state) = test_state(timeouts);
        let events = add_test_listener(&state);
        let now = t0();

        state.handle_packet(&announcement(&test_sdp("a"), REMOTE, TEST_HASH), now);
        assert_eq!(events.try_recv(), Ok((TEST_HASH, SessionEvent::Added)));

        // Exactly at the timeout nothing has expired yet
        state.expire_sessions(now + Duration::from_secs(10));
        assert_no_event(&events);

        state.expire_sessions(now + Duration::from_secs(10) + Duration::from_nanos(1));
        assert_eq!(events.try_recv(), Ok((TEST_HASH, SessionEvent::Expired)));
        assert_no_event(&events);
        assert!(state.sessions.lock().unwrap().is_empty());

        // Announced sessions never expire
        state.register_session(
            &test_sdp("b"),
            LOCAL,
            vec![GLOBAL_V4_MCAST_GROUP],
            None,
            false,
            now,
        );
        assert!(events.try_recv().is_ok());
        state.expire_sessions(now + Duration::from_secs(3600));
        assert_no_event(&events);
        assert!(
            state
                .sessions
                .lock()
                .unwrap()
                .contains_key(&SessionKey::new(&test_sdp("b"), LOCAL))
        );
    }

    #[test]
    fn announcement_schedule() {
        let timeouts = Timeouts {
            announce: Duration::from_secs(1),
            ..Timeouts::default()
        };
        let (_poll, state) = test_state(timeouts);
        let events = add_test_listener(&state);
        let now = t0();

        state.register_session(
            &test_sdp("a"),
            LOCAL,
            vec![GLOBAL_V4_MCAST_GROUP],
            None,
            false,
            now,
        );
        assert!(events.try_recv().is_ok());

        // Due immediately after registration
        let mut jitter = || Duration::from_millis(800);
        let due = state.due_announcements(now, &mut jitter);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].sess_id, "test-session");
        assert_eq!(due[0].groups, vec![GLOBAL_V4_MCAST_GROUP]);
        assert_eq!(due[0].interfaces, None);
        let message = SapMessage::parse(&due[0].packet).unwrap();
        assert_eq!(message.message_type, MessageType::Announcement);
        assert_eq!(
            message.msg_id_hash,
            crate::message::msg_id_hash(&test_sdp("a"))
        );
        assert_eq!(message.originating_source, LOCAL);

        // Next announcement due after the jittered interval
        let next = now + Duration::from_millis(800);
        assert!(
            state
                .due_announcements(next - Duration::from_nanos(1), &mut jitter)
                .is_empty()
        );
        assert_eq!(state.due_announcements(next, &mut jitter).len(), 1);
        assert_eq!(
            state
                .registered
                .lock()
                .unwrap()
                .iter()
                .next()
                .unwrap()
                .1
                .next_announce,
            next + Duration::from_millis(800)
        );
    }

    #[test]
    fn deletion_retries() {
        let timeouts = Timeouts {
            deletion_retry: Duration::from_secs(1),
            deletion_sends: 3,
            ..Timeouts::default()
        };
        let (_poll, state) = test_state(timeouts);
        let now = t0();

        let session_id = state.register_session(
            &test_sdp("a"),
            LOCAL,
            vec![GLOBAL_V4_MCAST_GROUP],
            None,
            false,
            now,
        );
        state.remove_registered_session(session_id, now);

        let calls = Cell::new(0u32);
        let mut fail = |_: &[u8], _: &[IpAddr], _: Option<&[String]>| {
            calls.set(calls.get() + 1);
            false
        };

        state.process_deletions(now, &mut fail);
        assert_eq!(calls.get(), 1);

        // Not due before the retry interval
        state.process_deletions(now + Duration::from_millis(500), &mut fail);
        assert_eq!(calls.get(), 1);

        // Given up after deletion_sends total attempts
        state.process_deletions(now + Duration::from_secs(1), &mut fail);
        state.process_deletions(now + Duration::from_secs(2), &mut fail);
        assert_eq!(calls.get(), 3);
        assert!(state.deletions.lock().unwrap().is_empty());
        state.process_deletions(now + Duration::from_secs(3), &mut fail);
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn deletion_sent_once() {
        let (_poll, state) = test_state(Timeouts::default());
        let now = t0();

        let session_id = state.register_session(
            &test_sdp("a"),
            LOCAL,
            vec![GLOBAL_V4_MCAST_GROUP],
            None,
            false,
            now,
        );
        state.remove_registered_session(session_id, now);

        let calls = Cell::new(0u32);
        let mut send = |_: &[u8], _: &[IpAddr], _: Option<&[String]>| {
            calls.set(calls.get() + 1);
            true
        };

        state.process_deletions(now, &mut send);
        assert_eq!(calls.get(), 1);
        assert!(state.deletions.lock().unwrap().is_empty());
    }

    #[test]
    fn next_poll_timeout() {
        let (_poll, state) = test_state(Timeouts::default());
        let now = t0();

        // Nothing pending: full poll timeout
        assert_eq!(state.next_poll_timeout(now), state.timeouts.poll);

        state.registered.lock().unwrap().insert(RegisteredSession {
            key: SessionKey::new(&test_sdp("a"), LOCAL),
            sdp: test_sdp("a"),
            addr: LOCAL,
            groups: vec![GLOBAL_V4_MCAST_GROUP],
            interfaces: None,
            compress: false,
            next_announce: now + Duration::from_secs(2),
        });
        assert_eq!(state.next_poll_timeout(now), Duration::from_secs(2));

        // Overdue announcements do not underflow
        assert_eq!(
            state.next_poll_timeout(now + Duration::from_secs(3)),
            Duration::ZERO
        );

        // Deletions can be due earlier than announcements
        state.deletions.lock().unwrap().push(PendingDeletion {
            packet: Vec::new(),
            groups: vec![LINK_LOCAL_V4_MCAST_GROUP],
            interfaces: None,
            sends_left: 1,
            next_send: now + Duration::from_millis(100),
        });
        assert_eq!(state.next_poll_timeout(now), Duration::from_millis(100));
    }

    #[test]
    fn unregister_removes() {
        let (_poll, state) = test_state(Timeouts::default());
        let events = add_test_listener(&state);
        let now = t0();

        let session_id =
            state.register_session(&test_sdp("a"), LOCAL, vec![LOCAL], None, true, now);
        let hash = crate::message::msg_id_hash(&test_sdp("a"));
        assert_eq!(events.try_recv(), Ok((hash, SessionEvent::Added)));

        state.remove_registered_session(session_id, now);
        assert_eq!(events.try_recv(), Ok((hash, SessionEvent::Removed)));
        assert_no_event(&events);

        assert!(state.sessions.lock().unwrap().is_empty());
        assert!(state.registered.lock().unwrap().is_empty());

        // A deletion is queued for repeated sending
        let deletions = state.deletions.lock().unwrap();
        assert_eq!(deletions.len(), 1);
        assert_eq!(deletions[0].sends_left, state.timeouts.deletion_sends);
        assert_eq!(deletions[0].next_send, now);
        assert_eq!(deletions[0].groups, vec![LOCAL]);
        drop(deletions);

        // Unknown sessions are ignored
        state.remove_registered_session(session_id, now);
        assert_no_event(&events);
        assert_eq!(state.deletions.lock().unwrap().len(), 1);
    }
}
