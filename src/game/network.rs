//! Two-player TCP lockstep. A tick is committed only after both inputs and
//! the preceding simulation checksums agree. Rendering never advances the simulation.
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::{Duration, Instant};

use crate::framework::context::Context;
use crate::framework::error::{GameError, GameResult};
use crate::framework::filesystem;
use crate::game::shared_game_state::{PlayerCount, SharedGameState, TimingMode};
use crate::input::player_controller::PlayerController;
use crate::input::replay_player_controller::{KeyState, ReplayController};
use serde::{Deserialize, Serialize};

const PROTOCOL: u32 = 1;
const MAX_PACKET: usize = 65536;
const TIMEOUT: Duration = Duration::from_secs(60);

fn error(message: impl Into<String>) -> GameError {
    GameError::ConfigError(format!("Network: {}", message.into()))
}

pub fn nickname(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() || value.chars().count() > 24 || value.chars().any(char::is_control) {
        return Err("Nickname must contain 1 to 24 characters, without control characters".into());
    }
    Ok(value.to_owned())
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Input {
    keys: u16,
    look: u8,
    analog: [f64; 2],
}

impl Input {
    pub fn capture(c: &dyn PlayerController) -> Self {
        let buttons = [
            c.move_left(),
            c.move_right(),
            c.move_up(),
            c.move_down(),
            c.map(),
            c.inventory(),
            c.jump(),
            c.shoot(),
            c.next_weapon(),
            c.prev_weapon(),
            c.trigger_menu_pause(),
            false,
            c.skip(),
            c.strafe(),
            c.trigger_menu_ok(),
            c.trigger_menu_back(),
        ];
        let keys = buttons.iter().enumerate().fold(0, |bits, (i, down)| bits | ((*down as u16) << i));
        let look =
            c.look_up() as u8 | (c.look_left() as u8) << 1 | (c.look_down() as u8) << 2 | (c.look_right() as u8) << 3;
        Self { keys, look, analog: [c.move_analog_x(), c.move_analog_y()] }
    }

    fn valid(&self) -> bool {
        self.look < 16 && self.analog.iter().all(|v| v.is_finite() && (-1.0..=1.0).contains(v))
    }

    pub fn apply(self, c: &mut ReplayController) {
        c.state = KeyState(self.keys);
        c.network_motion = Some((self.look, self.analog));
        c.update_trigger();
    }
}

#[derive(Serialize, Deserialize)]
struct Hello {
    protocol: u32,
    version: String,
    nickname: String,
}

#[derive(Serialize, Deserialize)]
struct Bootstrap {
    profile: Option<Vec<u8>>,
    seed: u64,
    settings: Vec<u8>,
    assets: u64,
}

#[derive(Serialize, Deserialize)]
struct Tick {
    sequence: u64,
    checksum: u64,
    input: Input,
}

fn encode<T: Serialize>(value: &T) -> GameResult<Vec<u8>> {
    let data = serde_json::to_vec(value).map_err(|e| error(e.to_string()))?;
    if data.len() > MAX_PACKET {
        return Err(error("Packet too large"));
    }
    let mut packet = (data.len() as u32).to_be_bytes().to_vec();
    packet.extend(data);
    Ok(packet)
}

fn receive<T: serde::de::DeserializeOwned>(stream: &mut TcpStream) -> GameResult<T> {
    let mut size = [0; 4];
    stream.read_exact(&mut size)?;
    let size = u32::from_be_bytes(size) as usize;
    if size == 0 || size > MAX_PACKET {
        return Err(error("Invalid packet size"));
    }
    let mut data = vec![0; size];
    stream.read_exact(&mut data)?;
    serde_json::from_slice(&data).map_err(|e| error(e.to_string()))
}

/// Stable FNV-1a hash, independent of platform and Rust's randomized hashers.
pub fn hash(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |h, b| (h ^ *b as u64).wrapping_mul(0x100000001b3))
}

// Include all mounted game resources, in sorted order, excluding user saves.
fn asset_hash(ctx: &Context) -> GameResult<u64> {
    fn visit(
        ctx: &Context,
        path: &std::path::Path,
        files: &mut std::collections::BTreeSet<std::path::PathBuf>,
    ) -> GameResult {
        let entries: std::collections::BTreeSet<_> = filesystem::read_dir(ctx, path)?
            .map(|entry| if entry.is_absolute() { entry } else { path.join(entry) })
            .collect();
        for entry in entries {
            if filesystem::is_dir(ctx, &entry) {
                visit(ctx, &entry, files)?;
            } else if filesystem::is_file(ctx, &entry) {
                files.insert(entry);
            }
        }
        Ok(())
    }
    let mut files = std::collections::BTreeSet::new();
    visit(ctx, std::path::Path::new("/"), &mut files)?;
    let mut result = Vec::new();
    for path in files {
        result.extend_from_slice(path.to_string_lossy().as_bytes());
        result.push(0);
        let mut data = Vec::new();
        filesystem::open(ctx, path)?.read_to_end(&mut data)?;
        result.extend_from_slice(&hash(&data).to_le_bytes());
    }
    Ok(hash(&result))
}

pub struct Session {
    stream: TcpStream,
    pub host: bool,
    pub names: [String; 2],
    pub profile: Option<Vec<u8>>,
    pub seed: u64,
    pub controllers: [ReplayController; 2],
    pub local_controller: Box<dyn PlayerController>,
    pub local_settings: Option<crate::game::settings::Settings>,
    sequence: u64,
    pending: Option<Input>,
    outgoing: Vec<u8>,
    sent: usize,
    incoming: Vec<u8>,
    last_progress: Instant,
}

impl Session {
    pub fn connect(
        host: Option<SocketAddr>,
        join: Option<SocketAddr>,
        name: &str,
        controller: Box<dyn PlayerController>,
    ) -> GameResult<Self> {
        let is_host = host.is_some();
        let mut stream = if let Some(address) = host {
            let listener = TcpListener::bind(address)?;
            eprintln!("Waiting for player 2 on {} (60 seconds)...", listener.local_addr()?);
            listener.set_nonblocking(true)?;
            let started = Instant::now();
            loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock && started.elapsed() < TIMEOUT => {
                        std::thread::sleep(Duration::from_millis(20))
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Err(error("Connection timed out")),
                    Err(e) => return Err(e.into()),
                }
            }
        } else {
            TcpStream::connect_timeout(&join.ok_or_else(|| error("Missing server IP"))?, Duration::from_secs(10))?
        };
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(TIMEOUT))?;
        stream.set_write_timeout(Some(TIMEOUT))?;
        let hello = Hello {
            protocol: PROTOCOL,
            version: env!("CARGO_PKG_VERSION").into(),
            nickname: nickname(name).map_err(error)?,
        };
        stream.write_all(&encode(&hello)?)?;
        let peer: Hello = receive(&mut stream)?;
        if peer.protocol != PROTOCOL || peer.version != hello.version {
            return Err(error("Incompatible game/protocol version"));
        }
        let peer_name = nickname(&peer.nickname).map_err(error)?;
        let names = if is_host { [hello.nickname, peer_name] } else { [peer_name, hello.nickname] };
        Ok(Self {
            stream,
            host: is_host,
            names,
            profile: None,
            seed: 1,
            controllers: [ReplayController::new(); 2],
            local_controller: controller,
            local_settings: None,
            sequence: 0,
            pending: None,
            outgoing: Vec::new(),
            sent: 0,
            incoming: Vec::new(),
            last_progress: Instant::now(),
        })
    }

    pub fn remember_settings(&mut self, settings: &crate::game::settings::Settings) -> GameResult {
        let bytes = serde_json::to_vec(settings).map_err(|e| error(e.to_string()))?;
        self.local_settings = Some(serde_json::from_slice(&bytes).map_err(|e| error(e.to_string()))?);
        Ok(())
    }

    /// Host owns the initial save and gameplay configuration, including retries.
    pub fn bootstrap(&mut self, state: &mut SharedGameState, ctx: &mut Context) -> GameResult {
        let assets = asset_hash(ctx)?;
        let bootstrap = if self.host {
            let profile = if let Some(path) = state.get_save_filename(state.save_slot) {
                if let Ok(file) = filesystem::user_open(ctx, path) {
                    let mut data = Vec::new();
                    file.take(MAX_PACKET as u64).read_to_end(&mut data)?;
                    // Reject malformed saves before sending anything.
                    crate::game::profile::GameProfile::load_from_save(std::io::Cursor::new(&data))?;
                    Some(data)
                } else {
                    None
                }
            } else {
                None
            };
            let bootstrap = Bootstrap {
                profile,
                seed: state.game_rng.dump_state(),
                settings: serde_json::to_vec(&state.settings).map_err(|e| error(e.to_string()))?,
                assets,
            };
            self.stream.write_all(&encode(&bootstrap)?)?;
            bootstrap
        } else {
            receive::<Bootstrap>(&mut self.stream)?
        };
        // Both peers acknowledge compatibility before starting the simulation.
        self.stream.write_all(&encode(&(assets == bootstrap.assets))?)?;
        let compatible: bool = receive(&mut self.stream)?;
        if !compatible || assets != bootstrap.assets {
            return Err(error("Both players must use identical game data"));
        }
        let settings: crate::game::settings::Settings =
            serde_json::from_slice(&bootstrap.settings).map_err(|e| error(e.to_string()))?;
        if self.local_settings.is_none() {
            self.remember_settings(&state.settings)?;
        }
        state.settings.locale = settings.locale;
        state.settings.original_textures = settings.original_textures;
        state.settings.seasonal_textures = settings.seasonal_textures;
        state.settings.more_rust = settings.more_rust;
        state.more_rust = settings.more_rust;
        state.settings.timing_mode = TimingMode::_50Hz;
        state.settings.cutscene_skip_mode = settings.cutscene_skip_mode;
        state.settings.allow_strafe = settings.allow_strafe;
        state.settings.screen_shake_intensity = settings.screen_shake_intensity;
        state.settings.pause_on_focus_loss = false;
        state.settings.touch_controls = false;
        state.settings.god_mode = false;
        state.settings.infinite_booster = false;
        state.settings.noclip = false;
        state.settings.speed = 1.0;
        state.player_count = PlayerCount::Two;
        self.profile = bootstrap.profile;
        self.seed = bootstrap.seed;
        self.stream.set_nonblocking(true)?;
        self.last_progress = Instant::now();
        Ok(())
    }

    pub fn waiting(&self) -> bool {
        self.pending.is_some() && self.last_progress.elapsed() > Duration::from_secs(1)
    }

    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Never blocks the event loop. Keep the same sampled input until committed.
    pub fn poll(&mut self, input: Input, checksum: u64) -> GameResult<Option<[ReplayController; 2]>> {
        if self.pending.is_none() {
            self.pending = Some(input);
            self.outgoing = encode(&Tick { sequence: self.sequence, checksum, input })?;
            self.sent = 0;
        }
        while self.sent < self.outgoing.len() {
            match self.stream.write(&self.outgoing[self.sent..]) {
                Ok(0) => return Err(error("Peer disconnected")),
                Ok(n) => {
                    self.sent += n;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e.into()),
            }
        }
        let mut buffer = [0; 4096];
        loop {
            match self.stream.read(&mut buffer) {
                Ok(0) => return Err(error("Peer disconnected")),
                Ok(n) => {
                    self.incoming.extend_from_slice(&buffer[..n]);
                    if self.incoming.len() > MAX_PACKET + 4 {
                        return Err(error("Receive buffer exceeded"));
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e.into()),
            }
        }
        if self.incoming.len() >= 4 {
            let size = u32::from_be_bytes(self.incoming[..4].try_into().unwrap()) as usize;
            if size == 0 || size > MAX_PACKET {
                return Err(error("Invalid packet size"));
            }
            if self.incoming.len() >= size + 4 && self.sent == self.outgoing.len() {
                let peer: Tick =
                    serde_json::from_slice(&self.incoming[4..size + 4]).map_err(|e| error(e.to_string()))?;
                if peer.sequence != self.sequence || peer.checksum != checksum {
                    return Err(error(format!("Simulation diverged at tick {}; session stopped", self.sequence)));
                }
                if !peer.input.valid() {
                    return Err(error("Invalid player input"));
                }
                let local = self.pending.take().unwrap();
                let inputs = if self.host { [local, peer.input] } else { [peer.input, local] };
                for (input, controller) in inputs.into_iter().zip(&mut self.controllers) {
                    input.apply(controller);
                }
                self.incoming.drain(..size + 4);
                self.sequence += 1;
                self.last_progress = Instant::now();
                return Ok(Some(self.controllers));
            }
        }
        if self.last_progress.elapsed() > TIMEOUT {
            return Err(error("Peer timed out"));
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::dummy_player_controller::DummyPlayerController;

    fn session(stream: TcpStream, host: bool) -> Session {
        stream.set_nonblocking(true).unwrap();
        stream.set_nodelay(true).unwrap();
        Session {
            stream,
            host,
            names: ["Host".into(), "Guest".into()],
            profile: None,
            seed: 123,
            controllers: [ReplayController::new(); 2],
            local_controller: Box::new(DummyPlayerController::new()),
            local_settings: None,
            sequence: 0,
            pending: None,
            outgoing: Vec::new(),
            sent: 0,
            incoming: Vec::new(),
            last_progress: Instant::now(),
        }
    }

    fn pair() -> (Session, Session) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        (session(server, true), session(client, false))
    }

    fn input(keys: u16) -> Input {
        Input { keys, look: 3, analog: [-0.5, 0.75] }
    }

    #[test]
    fn lockstep_keeps_player_order_and_commits_inputs_once() {
        let (mut host, mut guest) = pair();
        for tick in 0..50 {
            let mut committed = [None, None];
            let deadline = Instant::now() + Duration::from_secs(2);
            while committed.iter().any(Option::is_none) {
                if committed[0].is_none() {
                    committed[0] = host.poll(input(if tick == 0 { 1 << 6 } else { 0 }), tick).unwrap();
                }
                if committed[1].is_none() {
                    committed[1] = guest.poll(input(1 << 7), tick).unwrap();
                }
                assert!(Instant::now() < deadline, "loopback exchange stalled");
                std::thread::yield_now();
            }
            for controllers in committed.into_iter().flatten() {
                assert_eq!(controllers[0].jump(), tick == 0);
                assert_eq!(controllers[0].trigger_jump(), tick == 0);
                assert!(controllers[1].shoot());
                assert_eq!(controllers[1].trigger_shoot(), tick == 0);
                assert_eq!(controllers[1].move_analog_x(), -0.5);
                assert!(controllers[1].look_left());
                assert!(!controllers[1].look_right());
            }
        }
        assert_eq!(host.sequence, 50);
        assert_eq!(guest.sequence, 50);
    }

    #[test]
    fn partial_tcp_packet_preserves_the_original_input() {
        let (mut host, mut guest) = pair();
        assert!(host.poll(input(1 << 6), 42).unwrap().is_none());
        let packet = encode(&Tick { sequence: 0, checksum: 42, input: input(1 << 7) }).unwrap();
        guest.stream.write_all(&packet[..2]).unwrap();
        assert!(host.poll(input(0), 42).unwrap().is_none());
        guest.stream.write_all(&packet[2..]).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(c) = host.poll(input(0), 42).unwrap() {
                assert!(c[0].jump());
                assert!(c[1].shoot());
                break;
            }
            assert!(Instant::now() < deadline);
        }
    }

    #[test]
    fn rejects_desync_disconnect_and_oversized_packets() {
        let (mut host, mut guest) = pair();
        host.poll(input(0), 1).unwrap();
        guest.stream.write_all(&encode(&Tick { sequence: 0, checksum: 2, input: input(0) }).unwrap()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Err(e) = host.poll(input(0), 1) {
                assert!(e.to_string().contains("diverged"));
                break;
            }
            assert!(Instant::now() < deadline);
        }
        let (mut host, guest) = pair();
        drop(guest);
        assert!(host.poll(input(0), 0).is_err());
        let (mut host, mut guest) = pair();
        guest.stream.write_all(&((MAX_PACKET + 1) as u32).to_be_bytes()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Err(e) = host.poll(input(0), 0) {
                assert!(e.to_string().contains("packet size"));
                break;
            }
            assert!(Instant::now() < deadline);
        }
    }

    #[test]
    fn validates_names_and_motion() {
        assert_eq!(nickname("  Émilie  ").unwrap(), "Émilie");
        assert!(nickname("\n").is_err());
        assert!(nickname("a\nb").is_err());
        assert!(nickname(&"a".repeat(25)).is_err());
        assert!(!Input { analog: [f64::NAN, 0.0], ..input(0) }.valid());
        assert!(!Input { analog: [2.0, 0.0], ..input(0) }.valid());
    }
}
