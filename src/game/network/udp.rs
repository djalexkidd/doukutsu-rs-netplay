//! Reliable ordered messages and redundant, sequenced live inputs over UDP.
//! Fragment below the IPv6 minimum MTU; never rely on IP fragmentation.
use super::{error, GameResult, Message, MAX_PACKET, TIMEOUT};
use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::net::{SocketAddr, SocketAddrV6, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(super) const DATAGRAM_SIZE: usize = 1200;
const HEADER: usize = 30;
const PAYLOAD: usize = DATAGRAM_SIZE - HEADER;
const WINDOW: usize = 16;
const IN_FLIGHT: usize = 64;
const DATA: u8 = 0;
const ACK: u8 = 1;
const LIVE: u8 = 2;
const CLOSE: u8 = 3;

pub(super) fn canonical(address: SocketAddr) -> SocketAddr {
    match address {
        SocketAddr::V6(v6) => v6.ip().to_ipv4_mapped().map_or(address, |ip| SocketAddr::new(ip.into(), v6.port())),
        _ => address,
    }
}

struct Packet<'a> {
    id: u64,
    kind: u8,
    sequence: u64,
    fragment: usize,
    count: usize,
    total: usize,
    payload: &'a [u8],
}
impl<'a> Packet<'a> {
    fn parse(bytes: &'a [u8]) -> Option<Self> {
        if bytes.len() < HEADER || bytes.len() > DATAGRAM_SIZE || &bytes[..4] != b"DRSU" || bytes[13] != 1 {
            return None;
        }
        let packet = Self {
            id: u64::from_be_bytes(bytes[4..12].try_into().ok()?),
            kind: bytes[12],
            sequence: u64::from_be_bytes(bytes[14..22].try_into().ok()?),
            fragment: u16::from_be_bytes(bytes[22..24].try_into().ok()?) as usize,
            count: u16::from_be_bytes(bytes[24..26].try_into().ok()?) as usize,
            total: u32::from_be_bytes(bytes[26..30].try_into().ok()?) as usize,
            payload: &bytes[HEADER..],
        };
        match packet.kind {
            DATA | LIVE => {
                if packet.total == 0
                    || packet.total > MAX_PACKET
                    || packet.count != packet.total.div_ceil(PAYLOAD)
                    || packet.fragment >= packet.count
                    || packet.payload.len() != PAYLOAD.min(packet.total - packet.fragment * PAYLOAD)
                    || (packet.kind == LIVE && packet.count != 1)
                {
                    return None;
                }
            }
            ACK | CLOSE if bytes.len() == HEADER && packet.total == 0 && packet.count == 0 => {}
            _ => return None,
        }
        Some(packet)
    }
}

fn packet(id: u64, kind: u8, sequence: u64, fragment: usize, count: usize, total: usize, payload: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(HEADER + payload.len());
    bytes.extend_from_slice(b"DRSU");
    bytes.extend_from_slice(&id.to_be_bytes());
    bytes.extend_from_slice(&[kind, 1]);
    bytes.extend_from_slice(&sequence.to_be_bytes());
    bytes.extend_from_slice(&(fragment as u16).to_be_bytes());
    bytes.extend_from_slice(&(count as u16).to_be_bytes());
    bytes.extend_from_slice(&(total as u32).to_be_bytes());
    bytes.extend_from_slice(payload);
    bytes
}

struct Fragment {
    bytes: Vec<u8>,
    sent: Option<Instant>,
    attempts: u32,
    acknowledged: bool,
}
struct Outgoing {
    sequence: u64,
    fragments: Vec<Fragment>,
}
struct Assembly {
    total: usize,
    fragments: Vec<Option<Vec<u8>>>,
    remaining: usize,
}

pub(super) struct Connection {
    socket: Arc<UdpSocket>,
    pub remote: SocketAddr,
    wire_remote: SocketAddr,
    id: u64,
    outgoing: VecDeque<Outgoing>,
    incoming: BTreeMap<u64, Assembly>,
    next_out: u64,
    next_in: u64,
    next_live: u64,
    live_seen: [Option<u64>; 4],
    live_out: VecDeque<Vec<u8>>,
    live_dirty: bool,
    live_in: VecDeque<Message>,
    seen: Instant,
    closed: bool,
    rtt: Option<Duration>,
    retry: Duration,
}
impl Connection {
    pub fn new(socket: Arc<UdpSocket>, remote: SocketAddr, id: u64) -> Self {
        let remote = canonical(remote);
        let wire_remote = match remote {
            SocketAddr::V4(v4) if socket.local_addr().is_ok_and(|a| a.is_ipv6()) => {
                SocketAddr::V6(SocketAddrV6::new(v4.ip().to_ipv6_mapped(), v4.port(), 0, 0))
            }
            _ => remote,
        };
        Self {
            socket,
            remote,
            wire_remote,
            id,
            outgoing: VecDeque::new(),
            incoming: BTreeMap::new(),
            next_out: 0,
            next_in: 0,
            next_live: 0,
            live_seen: [None; 4],
            live_out: VecDeque::new(),
            live_dirty: false,
            live_in: VecDeque::new(),
            seen: Instant::now(),
            closed: false,
            rtt: None,
            retry: Duration::from_millis(350),
        }
    }
    pub fn client(socket: Arc<UdpSocket>, remote: SocketAddr) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let now =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos() as u64;
        let id = now ^ NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed).rotate_left(23);
        Self::new(socket, remote, id)
    }
    pub fn pending(&self) -> usize {
        self.outgoing.len()
    }
    pub fn invalidate(&mut self) {
        self.closed = true;
    }
    pub fn matches(&self, remote: SocketAddr, bytes: &[u8]) -> bool {
        remote == self.remote && Packet::parse(bytes).is_some_and(|p| p.id == self.id)
    }
    /// Only a complete Hello can allocate an unknown peer, not ACKs or arbitrary fragments.
    pub fn hello_id(bytes: &[u8]) -> Option<u64> {
        let p = Packet::parse(bytes)?;
        if p.kind != DATA || p.sequence != 0 || p.count != 1 {
            return None;
        }
        match serde_json::from_slice::<Message>(p.payload).ok()? {
            Message::Hello { .. } => Some(p.id),
            _ => None,
        }
    }
    fn live_lane(message: &Message) -> Option<usize> {
        match message {
            Message::Input { .. } => Some(0),
            Message::Ping(_) => Some(1),
            Message::Pong(_) => Some(2),
            Message::Latency(_) => Some(3),
            _ => None,
        }
    }
    pub fn queue(&mut self, message: &Message) -> GameResult {
        let data = serde_json::to_vec(message).map_err(|e| error(e.to_string()))?;
        if data.len() > MAX_PACKET {
            return Err(error("Network message exceeded"));
        }
        if Self::live_lane(message).is_some() {
            if data.len() > PAYLOAD {
                return Err(error("Live message exceeded datagram size"));
            }
            self.live_out.push_back(packet(self.id, LIVE, self.next_live, 0, 1, data.len(), &data));
            self.next_live += 1;
            while self.live_out.len() > 3 {
                self.live_out.pop_front();
            }
            self.live_dirty = true;
        } else {
            if self.outgoing.len() >= WINDOW {
                return Err(error("Network queue exceeded"));
            }
            let count = data.len().div_ceil(PAYLOAD);
            self.outgoing.push_back(Outgoing {
                sequence: self.next_out,
                fragments: data
                    .chunks(PAYLOAD)
                    .enumerate()
                    .map(|(index, chunk)| Fragment {
                        bytes: packet(self.id, DATA, self.next_out, index, count, data.len(), chunk),
                        sent: None,
                        attempts: 0,
                        acknowledged: false,
                    })
                    .collect(),
            });
            self.next_out += 1;
        }
        Ok(())
    }
    fn send(&self, bytes: &[u8]) -> GameResult<bool> {
        match self.socket.send_to(bytes, self.wire_remote) {
            Ok(n) if n == bytes.len() => Ok(true),
            Ok(_) => Err(error("Incomplete datagram")),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
    pub fn flush(&mut self) -> GameResult {
        while self.outgoing.front().is_some_and(|m| m.fragments.iter().all(|f| f.acknowledged)) {
            self.outgoing.pop_front();
        }
        if self.live_dirty {
            // Carry the last three inputs together: receiving a newer datagram
            // first must not lose a short button press in the preceding input.
            let mut data = vec![b'['];
            for (index, bytes) in self.live_out.iter().enumerate() {
                let p = Packet::parse(bytes).unwrap();
                if index != 0 {
                    data.push(b',');
                }
                data.extend_from_slice(format!("[{},", p.sequence).as_bytes());
                data.extend_from_slice(p.payload);
                data.push(b']');
            }
            data.push(b']');
            if data.len() > PAYLOAD {
                return Err(error("Live input bundle exceeded datagram size"));
            }
            let sequence = self.next_live - 1;
            if !self.send(&packet(self.id, LIVE, sequence, 0, 1, data.len(), &data))? {
                return Ok(());
            }
            self.live_dirty = false;
        }
        let mut in_flight =
            self.outgoing.iter().flat_map(|m| &m.fragments).filter(|f| f.sent.is_some() && !f.acknowledged).count();
        let now = Instant::now();
        let mut budget = IN_FLIGHT;
        for message in &mut self.outgoing {
            for fragment in &mut message.fragments {
                if fragment.acknowledged {
                    continue;
                }
                if let Some(sent) = fragment.sent {
                    let timeout =
                        (self.retry * (1 << fragment.attempts.saturating_sub(1).min(3))).min(Duration::from_secs(2));
                    if now.duration_since(sent) < timeout {
                        continue;
                    }
                } else if in_flight >= IN_FLIGHT {
                    continue;
                }
                if budget == 0 {
                    return Ok(());
                }
                match self.socket.send_to(&fragment.bytes, self.wire_remote) {
                    Ok(n) if n == fragment.bytes.len() => {}
                    Ok(_) => return Err(error("Incomplete datagram")),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                    Err(e) => return Err(e.into()),
                }
                if fragment.sent.is_none() {
                    in_flight += 1;
                }
                fragment.sent = Some(now);
                fragment.attempts += 1;
                budget -= 1;
            }
        }
        Ok(())
    }
    pub fn receive(&mut self, bytes: &[u8]) -> GameResult {
        let Some(p) = Packet::parse(bytes).filter(|p| p.id == self.id) else {
            return Ok(());
        };
        match p.kind {
            ACK => {
                if let Some(message) = self.outgoing.iter_mut().find(|m| m.sequence == p.sequence) {
                    if let Some(fragment) = message.fragments.get_mut(p.fragment) {
                        if let Some(sent) = fragment.sent {
                            if !fragment.acknowledged && fragment.attempts == 1 {
                                let sample = sent.elapsed();
                                let rtt = self.rtt.map_or(sample, |old| (old * 7 + sample) / 8);
                                self.rtt = Some(rtt);
                                self.retry = (rtt * 2 + Duration::from_millis(20))
                                    .clamp(Duration::from_millis(40), Duration::from_secs(1));
                            }
                            fragment.acknowledged = true;
                        }
                    }
                }
            }
            CLOSE => self.closed = true,
            LIVE => {
                let messages: Vec<(u64, Message)> =
                    serde_json::from_slice(p.payload).map_err(|e| error(e.to_string()))?;
                if messages.is_empty()
                    || messages.len() > 3
                    || messages.last().unwrap().0 != p.sequence
                    || messages.windows(2).any(|pair| pair[0].0 >= pair[1].0)
                {
                    return Err(error("Invalid live bundle"));
                }
                for (sequence, message) in messages {
                    let Some(lane) = Self::live_lane(&message) else {
                        return Err(error("Invalid live message"));
                    };
                    if self.live_seen[lane].is_none_or(|seq| sequence > seq) {
                        if self.live_in.len() >= 256 {
                            return Err(error("Live receive queue exceeded"));
                        }
                        self.live_seen[lane] = Some(sequence);
                        self.live_in.push_back(message);
                    }
                }
            }
            DATA => {
                if p.sequence >= self.next_in && p.sequence - self.next_in >= WINDOW as u64 {
                    return Ok(());
                }
                if p.sequence >= self.next_in {
                    let assembly = self.incoming.entry(p.sequence).or_insert_with(|| Assembly {
                        total: p.total,
                        fragments: (0..p.count).map(|_| None).collect(),
                        remaining: p.count,
                    });
                    if assembly.total != p.total {
                        return Ok(());
                    }
                    if assembly.fragments[p.fragment].is_none() {
                        assembly.fragments[p.fragment] = Some(p.payload.to_vec());
                        assembly.remaining -= 1;
                    }
                }
                // Duplicate and already delivered fragments still need acknowledgements.
                self.send(&packet(self.id, ACK, p.sequence, p.fragment, 0, 0, &[]))?;
            }
            _ => unreachable!(),
        }
        self.seen = Instant::now();
        Ok(())
    }
    pub fn pump(&mut self) -> GameResult<Vec<Message>> {
        self.flush()?;
        if self.closed {
            return Err(error("Peer disconnected"));
        }
        if self.seen.elapsed() > TIMEOUT {
            return Err(error("Peer timed out"));
        }
        let mut messages = Vec::new();
        while self.incoming.get(&self.next_in).is_some_and(|a| a.remaining == 0) {
            let assembly = self.incoming.remove(&self.next_in).unwrap();
            let mut bytes = Vec::with_capacity(assembly.total);
            for fragment in assembly.fragments {
                bytes.extend(fragment.unwrap());
            }
            messages.push(serde_json::from_slice(&bytes).map_err(|e| error(e.to_string()))?);
            self.next_in += 1;
        }
        // Live traffic cannot be blocked by a lost bootstrap/chat/history fragment.
        messages.extend(self.live_in.drain(..));
        Ok(messages)
    }
    pub fn close(&self) {
        let bytes = packet(self.id, CLOSE, 0, 0, 0, 0, &[]);
        for _ in 0..3 {
            let _ = self.send(&bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::network::Input;

    fn pair() -> (Connection, Connection) {
        let a = Arc::new(UdpSocket::bind("127.0.0.1:0").unwrap());
        let b = Arc::new(UdpSocket::bind("127.0.0.1:0").unwrap());
        a.set_nonblocking(true).unwrap();
        b.set_nonblocking(true).unwrap();
        let first = Connection::new(Arc::clone(&a), b.local_addr().unwrap(), 123);
        let second = Connection::new(b, a.local_addr().unwrap(), 123);
        (first, second)
    }
    fn datagrams(socket: &UdpSocket) -> Vec<Vec<u8>> {
        let mut packets = Vec::new();
        loop {
            let mut bytes = [0; DATAGRAM_SIZE + 1];
            match socket.recv_from(&mut bytes) {
                Ok((size, _)) => packets.push(bytes[..size].to_vec()),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("{}", e),
            }
        }
        packets
    }
    fn acknowledge(sender: &mut Connection) {
        for bytes in datagrams(&sender.socket) {
            sender.receive(&bytes).unwrap();
        }
    }
    fn expire_retries(sender: &mut Connection) {
        for message in &mut sender.outgoing {
            for fragment in &mut message.fragments {
                if fragment.sent.is_some() {
                    fragment.sent = Some(Instant::now() - Duration::from_secs(3));
                }
            }
        }
    }
    fn input(keys: u16) -> Message {
        Message::Input { input: Input { keys, ..Input::neutral() }, target: Some(100), sequence: 50, checksum: 42 }
    }

    #[test]
    fn fragment_loss_reordering_and_duplicates_preserve_order_without_blocking_live_input() {
        let (mut a, mut b) = pair();
        let text = "x".repeat(PAYLOAD * 4);
        a.queue(&Message::Chat(text.clone())).unwrap();
        a.queue(&Message::Chat("after".into())).unwrap();
        a.queue(&input(1)).unwrap();
        a.flush().unwrap();
        let mut packets = datagrams(&b.socket);
        packets.reverse();
        for bytes in packets {
            let p = Packet::parse(&bytes).unwrap();
            if p.kind == DATA && p.sequence == 0 && p.fragment == 1 {
                continue;
            }
            b.receive(&bytes).unwrap();
            b.receive(&bytes).unwrap();
        }
        let messages = b.pump().unwrap();
        assert!(matches!(messages.as_slice(), [Message::Input { input: Input { keys: 1, .. }, .. }]));
        acknowledge(&mut a);
        a.pump().unwrap();
        assert_eq!(a.pending(), 2); // Acknowledged later messages cannot outrun the receive window.
        expire_retries(&mut a);
        a.flush().unwrap();
        let retries = datagrams(&b.socket);
        assert_eq!(retries.len(), 1); // Only the missing fragment is resent.
        b.receive(&retries[0]).unwrap();
        let messages = b.pump().unwrap();
        assert!(matches!(messages.as_slice(), [Message::Chat(first), Message::Chat(second)]
            if first == &text && second == "after"));
        b.receive(&retries[0]).unwrap();
        assert!(b.pump().unwrap().is_empty());
        acknowledge(&mut a);
        a.pump().unwrap();
        assert_eq!(a.pending(), 0);
    }

    #[test]
    fn lost_ack_is_recovered_without_delivering_the_message_twice() {
        let (mut a, mut b) = pair();
        a.queue(&Message::Chat("once".into())).unwrap();
        a.flush().unwrap();
        for bytes in datagrams(&b.socket) {
            b.receive(&bytes).unwrap();
        }
        assert!(matches!(b.pump().unwrap().as_slice(), [Message::Chat(text)] if text == "once"));
        assert!(!datagrams(&a.socket).is_empty()); // Lose all ACKs.
        expire_retries(&mut a);
        a.flush().unwrap();
        for bytes in datagrams(&b.socket) {
            b.receive(&bytes).unwrap();
        }
        assert!(b.pump().unwrap().is_empty());
        acknowledge(&mut a);
        a.pump().unwrap();
        assert_eq!(a.pending(), 0);
    }

    #[test]
    fn large_messages_advance_through_a_bounded_datagram_window() {
        let (mut a, mut b) = pair();
        let text = "a".repeat(PAYLOAD * (IN_FLIGHT + 12));
        a.queue(&Message::Chat(text.clone())).unwrap();
        a.flush().unwrap();
        let first = datagrams(&b.socket);
        assert_eq!(first.len(), IN_FLIGHT);
        assert!(first.iter().all(|bytes| bytes.len() <= DATAGRAM_SIZE));
        for bytes in first {
            b.receive(&bytes).unwrap();
        }
        assert!(b.pump().unwrap().is_empty());
        acknowledge(&mut a);
        a.flush().unwrap();
        for bytes in datagrams(&b.socket) {
            b.receive(&bytes).unwrap();
        }
        assert!(matches!(b.pump().unwrap().as_slice(), [Message::Chat(received)] if received == &text));
        acknowledge(&mut a);
        a.pump().unwrap();
        assert_eq!(a.pending(), 0);
    }

    #[test]
    fn live_inputs_are_redundant_sequenced_and_independent_of_ping() {
        let (mut a, mut b) = pair();
        a.queue(&input(1)).unwrap();
        a.flush().unwrap();
        let older = datagrams(&b.socket);
        a.queue(&input(2)).unwrap();
        a.queue(&Message::Ping(9)).unwrap();
        a.flush().unwrap();
        let newer = datagrams(&b.socket);
        assert_eq!(newer.len(), 1); // One datagram carries the last three messages.
        for bytes in newer.iter().chain(older.iter()) {
            b.receive(bytes).unwrap();
            b.receive(bytes).unwrap();
        }
        let messages = b.pump().unwrap();
        assert!(matches!(
            messages.as_slice(),
            [
                Message::Input { input: Input { keys: 1, .. }, .. },
                Message::Input { input: Input { keys: 2, .. }, .. },
                Message::Ping(9)
            ]
        ));
        a.queue(&input(3)).unwrap();
        a.flush().unwrap();
        datagrams(&b.socket); // Lose this entire send.
        a.queue(&input(4)).unwrap();
        a.flush().unwrap();
        for bytes in datagrams(&b.socket) {
            b.receive(&bytes).unwrap();
        }
        assert!(matches!(
            b.pump().unwrap().as_slice(),
            [Message::Input { input: Input { keys: 3, .. }, .. }, Message::Input { input: Input { keys: 4, .. }, .. }]
        ));
        assert_eq!(a.pending(), 0); // No live packet waits for ACK or retransmission.
    }

    #[test]
    fn invalid_packets_and_old_sessions_cannot_allocate_or_keep_a_peer_alive() {
        let (_, mut b) = pair();
        let data = serde_json::to_vec(&Message::Chat("test".into())).unwrap();
        let seen = b.seen;
        let malformed = [
            vec![0; DATAGRAM_SIZE + 1],
            packet(b.id, DATA, 0, 0, 1, MAX_PACKET + 1, &data),
            packet(b.id, DATA, 0, 0, 2, data.len(), &data),
            packet(b.id + 1, CLOSE, 0, 0, 0, 0, &[]),
            packet(b.id + 1, DATA, 0, 0, 1, data.len(), &data),
            packet(b.id, DATA, WINDOW as u64, 0, 1, data.len(), &data),
        ];
        for bytes in malformed {
            assert!(Connection::hello_id(&bytes).is_none());
            b.receive(&bytes).unwrap();
        }
        assert_eq!(b.seen, seen);
        assert!(b.incoming.is_empty() && b.live_in.is_empty() && !b.closed);
        b.seen = Instant::now() - TIMEOUT - Duration::from_secs(1);
        assert!(b.pump().is_err());
    }

    #[test]
    fn only_a_complete_hello_allocates_a_peer_and_send_queues_are_bounded() {
        let (mut a, _) = pair();
        let hello = Message::Hello {
            protocol: super::super::PROTOCOL,
            version: "test".into(),
            name: "Player".into(),
            assets: 42,
            port: 28000,
            token: None,
            skin: super::super::SkinChoice::default(),
        };
        let data = serde_json::to_vec(&hello).unwrap();
        assert_eq!(Connection::hello_id(&packet(a.id, DATA, 0, 0, 1, data.len(), &data)), Some(a.id));
        assert!(Connection::hello_id(&packet(a.id, DATA, 1, 0, 1, data.len(), &data)).is_none());
        for _ in 0..WINDOW {
            a.queue(&Message::Chat("queued".into())).unwrap();
        }
        assert!(a.queue(&Message::Chat("overflow".into())).is_err());
        assert!(a.queue(&Message::Chat("x".repeat(MAX_PACKET))).is_err());
    }
}
