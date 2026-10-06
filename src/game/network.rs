//! Ordered, host-authoritative input stream with replay-based late joining and host migration.
use crate::framework::context::Context;
use crate::framework::error::{GameError, GameResult};
use crate::framework::filesystem;
use crate::game::shared_game_state::{PlayerCount, SharedGameState, TimingMode};
use crate::input::player_controller::PlayerController;
use crate::input::replay_player_controller::{KeyState, ReplayController};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

pub const MAX_PLAYERS: usize = 8;
const PROTOCOL: u32 = 8;
const MAX_PACKET: usize = 512 * 1024;
const TIMEOUT: Duration = Duration::from_secs(15);
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

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Input {
    keys: u16,
    look: u8,
    analog: [f32; 2],
}

impl Input {
    pub fn neutral() -> Self {
        Self { keys: 0, look: 0, analog: [0.0; 2] }
    }
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
        Self { keys, look, analog: [c.move_analog_x() as f32, c.move_analog_y() as f32] }
    }

    /// Predict held controls, excluding one-shot menu actions.
    pub fn held(c: &ReplayController) -> Self {
        let mut input = Self::capture(c);
        input.keys &= !((1 << 10) | (1 << 14) | (1 << 15));
        input
    }

    pub fn valid(&self) -> bool {
        self.look < 16 && self.analog.iter().all(|v| v.is_finite() && (-1.0..=1.0).contains(v))
    }

    /// Map input belongs to the local overlay and never reaches the shared VM.
    pub fn without_map(mut self) -> Self {
        self.keys &= !(1 << 4);
        self
    }
    pub fn apply(self, c: &mut ReplayController) {
        c.state = KeyState(self.keys);
        c.network_motion = Some((self.look, [self.analog[0] as f64, self.analog[1] as f64]));
        c.update_trigger();
    }
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum GameTiming {
    #[default]
    Freeware,
    CSPlus,
}
impl GameTiming {
    pub fn mode(self) -> TimingMode {
        match self {
            Self::Freeware => TimingMode::_50Hz,
            Self::CSPlus => TimingMode::_60Hz,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct GameRules {
    pub individual_cameras: bool,
    #[serde(default)]
    pub timing: GameTiming,
    pub difficulty: crate::game::shared_game_state::GameDifficulty,
}
impl Default for GameRules {
    fn default() -> Self {
        Self {
            individual_cameras: false,
            timing: GameTiming::Freeware,
            difficulty: crate::game::shared_game_state::GameDifficulty::Normal,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkinChoice {
    pub texture: u16,
    pub offset: u16,
}

pub fn available_skins(state: &SharedGameState) -> Vec<SkinChoice> {
    if !state.constants.is_cs_plus {
        return vec![SkinChoice::default()];
    }
    let mut choices = Vec::new();
    for (texture, path) in state.constants.player_skin_paths.iter().enumerate() {
        let height = state.constants.tex_sizes.get(path.as_str()).map_or(32, |size| size.1);
        for offset in (0..height / 32).step_by(2) {
            choices.push(SkinChoice { texture: texture as u16, offset });
        }
    }
    if choices.is_empty() {
        choices.push(SkinChoice::default());
    }
    choices
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Member {
    pub name: String,
    pub address: SocketAddr,
    pub generation: u32,
    pub skin: SkinChoice,
    token: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatMessage {
    pub author: String,
    pub text: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Bootstrap {
    profile: Option<Vec<u8>>,
    seed: u64,
    settings: Vec<u8>,
    assets: u64,
    initial_members: [Option<Member>; MAX_PLAYERS],
    rules: GameRules,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Frame {
    pub sequence: u64,
    pub checksum: u64,
    pub inputs: [Input; MAX_PLAYERS],
    pub members: Option<Box<[Option<Member>; MAX_PLAYERS]>>,
    pub migration_from: Option<u8>,
    pub retry: bool,
    pub rules: Option<GameRules>,
}

#[derive(Serialize, Deserialize)]
enum Message {
    Hello {
        protocol: u32,
        version: String,
        name: String,
        assets: u64,
        port: u16,
        token: Option<u64>,
        skin: SkinChoice,
    },
    Welcome {
        bootstrap: Bootstrap,
        slot: usize,
        members: [Option<Member>; MAX_PLAYERS],
        history: u64,
        chat: Vec<ChatMessage>,
        rules: GameRules,
    },
    Frames(Vec<Frame>),
    Input {
        input: Input,
        target: Option<u64>,
        sequence: u64,
        checksum: u64,
    },
    Rename(String),
    Skin(SkinChoice),
    Chat(String),
    ChatLine(ChatMessage),
    Leave,
    Ping(u64),
    Pong(u64),
    Latency([Option<u32>; MAX_PLAYERS]),
    Reject(String),
}

struct Connection {
    stream: TcpStream,
    incoming: Vec<u8>,
    outgoing: VecDeque<Vec<u8>>,
    offset: usize,
    seen: Instant,
}

impl Connection {
    fn new(stream: TcpStream) -> GameResult<Self> {
        stream.set_nonblocking(true)?;
        stream.set_nodelay(true)?;
        Ok(Self { stream, incoming: Vec::new(), outgoing: VecDeque::new(), offset: 0, seen: Instant::now() })
    }
    fn queue(&mut self, message: &Message) -> GameResult {
        let data = serde_json::to_vec(message).map_err(|e| error(e.to_string()))?;
        if data.len() > MAX_PACKET || self.outgoing.len() >= 16 {
            return Err(error("Network queue exceeded"));
        }
        let mut packet = (data.len() as u32).to_be_bytes().to_vec();
        packet.extend(data);
        self.outgoing.push_back(packet);
        Ok(())
    }
    fn flush(&mut self) -> GameResult {
        while let Some(packet) = self.outgoing.front() {
            match self.stream.write(&packet[self.offset..]) {
                Ok(0) => return Err(error("Peer disconnected")),
                Ok(n) => self.offset += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e.into()),
            }
            if self.offset == packet.len() {
                self.outgoing.pop_front();
                self.offset = 0;
            }
        }
        Ok(())
    }
    fn pump(&mut self) -> GameResult<Vec<Message>> {
        self.flush()?;
        let mut buffer = [0; 16384];
        let mut disconnected = false;
        // Bound work per update, including when replaying a long history.
        for _ in 0..32 {
            match self.stream.read(&mut buffer) {
                Ok(0) => {
                    disconnected = true;
                    break;
                }
                Ok(n) => {
                    self.incoming.extend_from_slice(&buffer[..n]);
                    self.seen = Instant::now();
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e.into()),
            }
            if self.incoming.len() > MAX_PACKET + 4 {
                break;
            }
        }
        let mut messages = Vec::new();
        let mut consumed = 0;
        while self.incoming.len() - consumed >= 4 {
            let size = u32::from_be_bytes(self.incoming[consumed..consumed + 4].try_into().unwrap()) as usize;
            if size == 0 || size > MAX_PACKET {
                return Err(error("Invalid packet size"));
            }
            if self.incoming.len() - consumed < size + 4 {
                break;
            }
            messages.push(
                serde_json::from_slice(&self.incoming[consumed + 4..consumed + 4 + size])
                    .map_err(|e| error(e.to_string()))?,
            );
            consumed += size + 4;
        }
        self.incoming.drain(..consumed);
        if self.incoming.len() > MAX_PACKET + 4 {
            return Err(error("Receive buffer exceeded"));
        }
        if disconnected && messages.is_empty() {
            return Err(error("Peer disconnected"));
        }
        if self.seen.elapsed() > TIMEOUT {
            return Err(error("Peer timed out"));
        }
        Ok(messages)
    }
}

struct Peer {
    ping: Option<(u64, Instant)>,
    connection: Connection,
    slot: Option<usize>,
    cursor: usize,
}

pub struct Session {
    pub host: bool,
    /// Round-trip time to the host in milliseconds; absent until measured.
    pub pings: [Option<u32>; MAX_PLAYERS],
    ping_nonce: u64,
    last_ping: Instant,
    pub rules: GameRules,
    pub applied_rules: GameRules,
    rules_changed: bool,
    pub skin_draft: SkinChoice,
    skin_choices: Vec<SkinChoice>,
    listener: TcpListener,
    peers: Vec<Peer>,
    server: Option<Connection>,
    connecting: Option<Receiver<io::Result<TcpStream>>>,
    server_address: SocketAddr,
    assets: Option<u64>,
    bootstrap_data: Option<Bootstrap>,
    ready: bool,
    pub profile: Option<Vec<u8>>,
    pub seed: u64,
    pub controllers: [ReplayController; MAX_PLAYERS],
    pub local_controller: Box<dyn PlayerController>,
    pub local_settings: Option<crate::game::settings::Settings>,
    pub local_slot: usize,
    pub members: [Option<Member>; MAX_PLAYERS],
    pub applied_members: [Option<Member>; MAX_PLAYERS],
    latest_inputs: [Input; MAX_PLAYERS],
    scheduled_inputs: [std::collections::BTreeMap<u64, Input>; MAX_PLAYERS],
    history: Vec<Frame>,
    incoming_frames: VecDeque<Frame>,
    welcomed_history: u64,
    token: Option<u64>,
    generation: u32,
    roster_changed: bool,
    pending_migration: Option<u8>,
    pub restart_requested: bool,
    pub retry_requested: bool,
    last_input_send: Instant,
    pub chat: VecDeque<ChatMessage>,
    pub chat_open: bool,
    pub options_open: bool,
    pub leave_requested: bool,
    pub chat_draft: String,
    pub nickname_draft: String,
    pub menu_error: String,
    joining_since: Instant,
}

impl Session {
    pub fn connect(
        host: Option<SocketAddr>,
        join: Option<SocketAddr>,
        name: &str,
        controller: Box<dyn PlayerController>,
    ) -> GameResult<Self> {
        let name = nickname(name).map_err(error)?;
        let is_host = host.is_some();
        let address = host.or(join).ok_or_else(|| error("Missing IP address"))?;
        let listener = TcpListener::bind(if is_host {
            address
        } else if address.is_ipv6() {
            "[::]:0".parse().unwrap()
        } else {
            "0.0.0.0:0".parse().unwrap()
        })?;
        listener.set_nonblocking(true)?;
        let mut members: [Option<Member>; MAX_PLAYERS] = std::array::from_fn(|_| None);
        if is_host {
            members[0] = Some(Member {
                name: name.clone(),
                address,
                generation: 1,
                skin: SkinChoice::default(),
                token: crate::common::get_timestamp() ^ 0xa5f9a233,
            });
        }
        let mut session = Self {
            host: is_host,
            pings: std::array::from_fn(|slot| if is_host && slot == 0 { Some(0) } else { None }),
            ping_nonce: 0,
            last_ping: Instant::now() - Duration::from_secs(1),
            rules: GameRules::default(),
            applied_rules: GameRules::default(),
            rules_changed: false,
            skin_draft: SkinChoice::default(),
            skin_choices: vec![SkinChoice::default()],
            listener,
            peers: Vec::new(),
            server: None,
            connecting: None,
            server_address: address,
            assets: None,
            bootstrap_data: None,
            ready: false,
            profile: None,
            seed: 1,
            controllers: [ReplayController::new(); MAX_PLAYERS],
            local_controller: controller,
            local_settings: None,
            local_slot: 0,
            applied_members: members.clone(),
            members,
            latest_inputs: [Input::neutral(); MAX_PLAYERS],
            scheduled_inputs: std::array::from_fn(|_| std::collections::BTreeMap::new()),
            history: Vec::new(),
            incoming_frames: VecDeque::new(),
            welcomed_history: 0,
            token: None,
            generation: 1,
            roster_changed: false,
            pending_migration: None,
            restart_requested: false,
            retry_requested: false,
            last_input_send: Instant::now() - Duration::from_secs(1),
            chat: VecDeque::new(),
            chat_open: false,
            options_open: false,
            leave_requested: false,
            chat_draft: String::new(),
            nickname_draft: name,
            menu_error: String::new(),
            joining_since: Instant::now(),
        };
        if !is_host {
            session.start_connection(address);
        }
        Ok(session)
    }

    fn start_connection(&mut self, address: SocketAddr) {
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(TcpStream::connect_timeout(&address, Duration::from_secs(5)));
        });
        self.connecting = Some(receiver);
        self.server_address = address;
        self.joining_since = Instant::now();
    }

    pub fn remember_settings(&mut self, settings: &crate::game::settings::Settings) -> GameResult {
        let bytes = serde_json::to_vec(settings).map_err(|e| error(e.to_string()))?;
        if !self.ready {
            self.rules = settings.network_rules;
            self.applied_rules = self.rules;
            self.skin_draft = settings.network_skin;
            if self.host {
                self.members[0].as_mut().unwrap().skin = self.skin_draft;
                self.applied_members = self.members.clone();
            }
        }
        self.local_settings = Some(serde_json::from_slice(&bytes).map_err(|e| error(e.to_string()))?);
        Ok(())
    }

    pub fn bootstrap(&mut self, state: &mut SharedGameState, ctx: &mut Context) -> GameResult<bool> {
        if self.assets.is_none() {
            self.assets = Some(asset_hash(ctx)?);
            self.skin_choices = available_skins(state);
            if self.local_settings.is_none() {
                self.remember_settings(&state.settings)?;
            }
            if !self.skin_choices.contains(&self.skin_draft) {
                self.skin_draft = SkinChoice::default();
                if self.host {
                    self.members[0].as_mut().unwrap().skin = self.skin_draft;
                    self.applied_members = self.members.clone();
                }
            }
            if self.host {
                let profile = if let Some(path) = state.get_save_filename(state.save_slot) {
                    if let Ok(file) = filesystem::user_open(ctx, path) {
                        let mut data = Vec::new();
                        file.take(65536).read_to_end(&mut data)?;
                        crate::game::profile::GameProfile::load_from_save(std::io::Cursor::new(&data))?;
                        Some(data)
                    } else {
                        None
                    }
                } else {
                    None
                };
                self.bootstrap_data = Some(Bootstrap {
                    profile,
                    seed: state.game_rng.dump_state(),
                    settings: serde_json::to_vec(&state.settings).map_err(|e| error(e.to_string()))?,
                    assets: self.assets.unwrap(),
                    initial_members: self.members.clone(),
                    rules: self.rules,
                });
                self.ready = true;
            }
        }
        if !self.ready {
            self.pump_transport(0)?;
        }
        if !self.ready {
            return Ok(false);
        }
        let bootstrap = self.bootstrap_data.as_ref().unwrap();
        let settings: crate::game::settings::Settings =
            serde_json::from_slice(&bootstrap.settings).map_err(|e| error(e.to_string()))?;
        state.settings.locale = settings.locale;
        state.settings.original_textures = settings.original_textures;
        state.settings.seasonal_textures = settings.seasonal_textures;
        state.settings.more_rust = settings.more_rust;
        state.more_rust = settings.more_rust;
        state.settings.timing_mode = bootstrap.rules.timing.mode();
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
        self.applied_rules = bootstrap.rules;
        self.profile = bootstrap.profile.clone();
        self.seed = bootstrap.seed;
        Ok(true)
    }

    fn add_chat(&mut self, line: ChatMessage) {
        self.chat.push_back(line);
        while self.chat.len() > 64 {
            self.chat.pop_front();
        }
    }
    fn broadcast_chat(&mut self, line: ChatMessage) {
        self.add_chat(line.clone());
        for peer in &mut self.peers {
            if peer.slot.is_some() {
                let _ = peer.connection.queue(&Message::ChatLine(line.clone()));
            }
        }
    }
    pub fn send_chat(&mut self, text: &str) -> GameResult {
        let text = text.trim();
        if text.is_empty() || text.chars().count() > 240 || text.chars().any(char::is_control) {
            return Err(error("Chat messages must contain 1-240 characters"));
        }
        if self.host {
            self.broadcast_chat(ChatMessage {
                author: self.members[0].as_ref().unwrap().name.clone(),
                text: text.into(),
            });
        } else if let Some(server) = &mut self.server {
            server.queue(&Message::Chat(text.into()))?;
        }
        Ok(())
    }
    pub fn rename(&mut self, name: &str) -> GameResult {
        let name = nickname(name).map_err(error)?;
        if self.host {
            self.members[0].as_mut().unwrap().name = name.clone();
            self.roster_changed = true;
        } else if let Some(server) = &mut self.server {
            server.queue(&Message::Rename(name.clone()))?;
        }
        self.nickname_draft = name;
        Ok(())
    }
    pub fn set_rules(&mut self, rules: GameRules) -> GameResult {
        if !self.host {
            return Err(error("Only the host can change game rules"));
        }
        self.rules = rules;
        self.rules_changed = true;
        Ok(())
    }
    pub fn change_skin(&mut self, skin: SkinChoice) -> GameResult {
        if !self.skin_choices.contains(&skin) {
            return Err(error("Unavailable character"));
        }
        if self.host {
            self.members[0].as_mut().unwrap().skin = skin;
            self.roster_changed = true;
        } else if let Some(server) = &mut self.server {
            server.queue(&Message::Skin(skin))?;
        }
        self.skin_draft = skin;
        Ok(())
    }
    pub fn address(&self) -> SocketAddr {
        self.members[0].as_ref().map_or(self.server_address, |m| m.address)
    }
    pub fn catching_up(&self) -> bool {
        !self.host && ((self.history.len() as u64) < self.welcomed_history || self.incoming_frames.len() > 4)
    }
    pub fn waiting(&self) -> bool {
        !self.host && (self.server.is_none() || self.catching_up())
    }
    pub fn is_pending(&self) -> bool {
        false
    }
    pub fn ready(&self) -> bool {
        self.ready
    }

    fn server_messages(&mut self, messages: Vec<Message>) -> GameResult {
        for message in messages {
            match message {
                Message::Welcome { bootstrap, slot, members, history, chat, rules } => {
                    if slot >= MAX_PLAYERS || bootstrap.assets != self.assets.unwrap() {
                        return Err(error("Incompatible game data or player slot"));
                    }
                    let was_ready = self.ready;
                    self.local_slot = slot;
                    self.rules = rules;
                    self.applied_rules = bootstrap.rules;
                    self.token = members[slot].as_ref().map(|member| member.token);
                    self.members = members;
                    self.applied_members = bootstrap.initial_members.clone();
                    self.profile = bootstrap.profile.clone();
                    self.seed = bootstrap.seed;
                    self.bootstrap_data = Some(bootstrap);
                    self.welcomed_history = history;
                    self.pings = [None; MAX_PLAYERS];
                    self.history.clear();
                    self.incoming_frames.clear();
                    self.controllers = [ReplayController::new(); MAX_PLAYERS];
                    self.chat = chat.into();
                    self.ready = true;
                    self.restart_requested = was_ready;
                }
                Message::Frames(frames) => {
                    if frames.len() > 32 {
                        return Err(error("Invalid history batch"));
                    }
                    for frame in frames {
                        if frame.sequence != (self.history.len() + self.incoming_frames.len()) as u64
                            || frame.inputs.iter().any(|i| !i.valid())
                        {
                            return Err(error("Invalid frame stream"));
                        }
                        self.incoming_frames.push_back(frame);
                    }
                }
                Message::Ping(nonce) => {
                    if let Some(server) = &mut self.server {
                        server.queue(&Message::Pong(nonce))?;
                        server.flush()?;
                    }
                }
                Message::Latency(pings) => self.pings = pings,
                Message::ChatLine(line) => self.add_chat(line),
                Message::Reject(reason) => return Err(error(reason)),
                _ => return Err(error("Unexpected server message")),
            }
        }
        Ok(())
    }

    fn migrate(&mut self) -> GameResult {
        let candidate = (1..MAX_PLAYERS)
            .find(|&slot| self.members[slot].is_some())
            .ok_or_else(|| error("Host disconnected; no remaining player"))?;
        if candidate == self.local_slot {
            self.host = true;
            self.pings = [None; MAX_PLAYERS];
            self.pings[0] = Some(0);
            self.last_ping = Instant::now() - Duration::from_secs(1);
            self.rules = self.applied_rules;
            self.rules_changed = false;
            self.server = None;
            self.members.swap(0, candidate);
            self.members[candidate] = None;
            self.local_slot = 0;
            self.pending_migration = Some(candidate as u8);
            self.incoming_frames.clear();
            self.joining_since = Instant::now();
            self.roster_changed = true;
            self.latest_inputs = [Input::neutral(); MAX_PLAYERS];
            self.scheduled_inputs.iter_mut().for_each(|inputs| inputs.clear());
            self.generation = self.members.iter().flatten().map(|m| m.generation).max().unwrap_or(0);
            self.broadcast_chat(ChatMessage {
                author: "Server".into(),
                text: "Hosting transferred to the next player.".into(),
            });
        } else {
            let address = self.members[candidate].as_ref().unwrap().address;
            self.server = None;
            self.start_connection(address);
        }
        Ok(())
    }

    fn pump_transport(&mut self, checksum: u64) -> GameResult {
        if self.host {
            for _ in 0..MAX_PLAYERS {
                match self.listener.accept() {
                    Ok((stream, _)) => {
                        if self.peers.len() >= MAX_PLAYERS * 2 {
                            continue;
                        }
                        self.peers.push(Peer {
                            connection: Connection::new(stream)?,
                            slot: None,
                            cursor: 0,
                            ping: None,
                        });
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) => return Err(e.into()),
                }
            }
            let mut index = 0;
            while index < self.peers.len() {
                let messages = self.peers[index].connection.pump();
                let mut remove = messages.is_err();
                if let Ok(messages) = messages {
                    for message in messages {
                        let slot = self.peers[index].slot;
                        match message {
                            Message::Hello { protocol, version, name, assets, port, token, skin } if slot.is_none() => {
                                let name = nickname(&name);
                                if protocol != PROTOCOL
                                    || version != env!("CARGO_PKG_VERSION")
                                    || Some(assets) != self.assets
                                    || name.is_err()
                                    || port == 0
                                    || !self.skin_choices.contains(&skin)
                                {
                                    let _ = self.peers[index]
                                        .connection
                                        .queue(&Message::Reject("Incompatible version, game data or nickname".into()));
                                    let _ = self.peers[index].connection.flush();
                                    remove = true;
                                    break;
                                }
                                let reserved = token.and_then(|token| {
                                    (1..MAX_PLAYERS)
                                        .find(|&i| self.members[i].as_ref().map_or(false, |m| m.token == token))
                                });
                                let free = reserved.or_else(|| (1..MAX_PLAYERS).find(|&i| self.members[i].is_none()));
                                let Some(slot) = free else {
                                    let _ = self.peers[index]
                                        .connection
                                        .queue(&Message::Reject("The game is full (8 players)".into()));
                                    let _ = self.peers[index].connection.flush();
                                    remove = true;
                                    break;
                                };
                                if self.peers.iter().any(|p| p.slot == Some(slot)) {
                                    remove = true;
                                    break;
                                }
                                if reserved.is_none() {
                                    self.generation += 1;
                                    let remote = self.peers[index].connection.stream.peer_addr()?;
                                    self.members[slot] = Some(Member {
                                        name: name.unwrap(),
                                        address: SocketAddr::new(remote.ip(), port),
                                        generation: self.generation,
                                        skin,
                                        token: (self.seed ^ crate::common::get_timestamp())
                                            .wrapping_mul(0x9e3779b97f4a7c15)
                                            .wrapping_add(self.generation as u64),
                                    });
                                    self.roster_changed = true;
                                    // Advertise the routable interface used by this connection.
                                    let local = self.peers[index].connection.stream.local_addr()?;
                                    let listen_port = self.listener.local_addr()?.port();
                                    self.members[0].as_mut().unwrap().address =
                                        SocketAddr::new(local.ip(), listen_port);
                                }
                                self.peers[index].slot = Some(slot);
                                let welcome = Message::Welcome {
                                    bootstrap: self.bootstrap_data.as_ref().unwrap().clone(),
                                    slot,
                                    members: self.members.clone(),
                                    history: self.history.len() as u64,
                                    chat: self.chat.iter().cloned().collect(),
                                    rules: self.rules,
                                };
                                self.peers[index].connection.queue(&welcome)?;
                            }
                            Message::Input { input, target, sequence, checksum: reported } if slot.is_some() => {
                                let expected = self
                                    .history
                                    .get(sequence as usize)
                                    .map(|f| f.checksum)
                                    .or_else(|| (sequence as usize == self.history.len()).then_some(checksum));
                                if !input.valid()
                                    || expected != Some(reported)
                                    || target.is_some_and(|t| t > self.history.len() as u64 + 64)
                                {
                                    let _ = self.peers[index]
                                        .connection
                                        .queue(&Message::Reject("Simulation diverged; please rejoin".into()));
                                    let _ = self.peers[index].connection.flush();
                                    remove = true;
                                    break;
                                }
                                let slot = slot.unwrap();
                                if let Some(target) = target {
                                    // Late inputs apply at the next host frame; future ones wait for their frame.
                                    let target = target.max(self.history.len() as u64);
                                    self.scheduled_inputs[slot].insert(target, input);
                                } else {
                                    self.scheduled_inputs[slot].clear();
                                    self.latest_inputs[slot] = input;
                                }
                            }
                            Message::Pong(nonce) if slot.is_some() => {
                                if let Some((expected, sent)) = self.peers[index].ping {
                                    if expected == nonce {
                                        self.pings[slot.unwrap()] =
                                            Some(sent.elapsed().as_millis().min(u32::MAX as u128) as u32);
                                        self.peers[index].ping = None;
                                    }
                                }
                            }
                            Message::Skin(skin) if slot.is_some() => {
                                if self.skin_choices.contains(&skin) {
                                    self.members[slot.unwrap()].as_mut().unwrap().skin = skin;
                                    self.roster_changed = true;
                                }
                            }
                            Message::Rename(name) if slot.is_some() => {
                                if let Ok(name) = nickname(&name) {
                                    self.members[slot.unwrap()].as_mut().unwrap().name = name;
                                    self.roster_changed = true;
                                }
                            }
                            Message::Chat(text) if slot.is_some() => {
                                let text = text.trim();
                                if !text.is_empty()
                                    && text.chars().count() <= 240
                                    && !text.chars().any(char::is_control)
                                {
                                    self.broadcast_chat(ChatMessage {
                                        author: self.members[slot.unwrap()].as_ref().unwrap().name.clone(),
                                        text: text.into(),
                                    });
                                }
                            }
                            Message::Leave => {
                                remove = true;
                                break;
                            }
                            _ => {
                                remove = true;
                                break;
                            }
                        }
                    }
                }
                if remove {
                    if let Some(slot) = self.peers[index].slot {
                        if let Some(member) = self.members[slot].take() {
                            self.broadcast_chat(ChatMessage {
                                author: "Server".into(),
                                text: format!("{} left the game.", member.name),
                            });
                        }
                        self.latest_inputs[slot] = Input::neutral();
                        self.pings[slot] = None;
                        self.scheduled_inputs[slot].clear();
                        self.roster_changed = true;
                    }
                    self.peers.remove(index);
                } else {
                    index += 1;
                }
            }
            // Reserved players get a short grace period to reconnect after host migration.
            if self.joining_since.elapsed() > TIMEOUT {
                for slot in 1..MAX_PLAYERS {
                    if self.members[slot].is_some() && !self.peers.iter().any(|p| p.slot == Some(slot)) {
                        self.members[slot] = None;
                        self.pings[slot] = None;
                        self.roster_changed = true;
                    }
                }
            }
            if self.last_ping.elapsed() >= Duration::from_secs(1) {
                self.last_ping = Instant::now();
                for peer in &mut self.peers {
                    if let Some(slot) = peer.slot {
                        if peer.ping.is_some_and(|(_, sent)| sent.elapsed() > Duration::from_secs(5)) {
                            peer.ping = None;
                            self.pings[slot] = None;
                        }
                        if peer.connection.outgoing.len() < 2 {
                            peer.connection.queue(&Message::Latency(self.pings))?;
                            if peer.ping.is_none() {
                                self.ping_nonce = self.ping_nonce.wrapping_add(1);
                                peer.connection.queue(&Message::Ping(self.ping_nonce))?;
                                peer.ping = Some((self.ping_nonce, Instant::now()));
                            }
                        }
                    }
                }
            }
            for peer in &mut self.peers {
                if peer.slot.is_some() && peer.connection.outgoing.len() < 2 && peer.cursor < self.history.len() {
                    let end = (peer.cursor + 32).min(self.history.len());
                    peer.connection.queue(&Message::Frames(self.history[peer.cursor..end].to_vec()))?;
                    peer.cursor = end;
                }
                let _ = peer.connection.flush();
            }
        } else {
            if let Some(receiver) = &self.connecting {
                if let Ok(result) = receiver.try_recv() {
                    self.connecting = None;
                    match result {
                        Ok(stream) => {
                            let mut server = Connection::new(stream)?;
                            if let Some(assets) = self.assets {
                                server.queue(&Message::Hello {
                                    protocol: PROTOCOL,
                                    version: env!("CARGO_PKG_VERSION").into(),
                                    name: self.nickname_draft.clone(),
                                    assets,
                                    port: self.listener.local_addr()?.port(),
                                    token: self.token,
                                    skin: self.skin_draft,
                                })?;
                                server.flush()?;
                            }
                            self.server = Some(server);
                        }
                        Err(e) => return Err(e.into()),
                    }
                }
            }
            // A connection can complete before resources have been fingerprinted.
            if let Some(server) = &mut self.server {
                match server.pump() {
                    Ok(messages) => self.server_messages(messages)?,
                    Err(e) => {
                        if self.ready {
                            self.migrate()?;
                        } else {
                            return Err(e);
                        }
                    }
                }
            }
            if self.server.is_none() && self.connecting.is_none() && self.joining_since.elapsed() > TIMEOUT {
                return Err(error("Connection timed out"));
            }
        }
        Ok(())
    }

    pub fn sequence(&self) -> u64 {
        self.history.len() as u64
    }
    pub fn poll(&mut self, input: Input, checksum: u64) -> GameResult<Option<Frame>> {
        self.poll_predicted(input, checksum, None)
    }
    pub fn poll_predicted(&mut self, input: Input, checksum: u64, target: Option<u64>) -> GameResult<Option<Frame>> {
        self.pump_transport(checksum)?;
        if self.restart_requested {
            return Ok(None);
        }
        let frame = if self.host {
            for (slot, scheduled) in self.scheduled_inputs.iter_mut().enumerate() {
                if self.members[slot].is_none() {
                    scheduled.clear();
                    self.latest_inputs[slot] = Input::neutral();
                } else {
                    while scheduled.first_key_value().is_some_and(|(&seq, _)| seq <= self.history.len() as u64) {
                        self.latest_inputs[slot] = scheduled.pop_first().unwrap().1;
                    }
                }
            }
            self.latest_inputs[0] = input;
            let frame = Frame {
                sequence: self.history.len() as u64,
                checksum,
                inputs: self.latest_inputs,
                members: if self.roster_changed {
                    self.roster_changed = false;
                    Some(Box::new(self.members.clone()))
                } else {
                    None
                },
                migration_from: self.pending_migration.take(),
                retry: std::mem::take(&mut self.retry_requested),
                rules: if std::mem::take(&mut self.rules_changed) { Some(self.rules) } else { None },
            };
            self.history.push(frame.clone());
            Some(frame)
        } else {
            if self.last_input_send.elapsed()
                >= Duration::from_millis(if self.applied_rules.timing == GameTiming::CSPlus { 14 } else { 18 })
            {
                if let Some(server) = &mut self.server {
                    let input =
                        if (self.history.len() as u64) < self.welcomed_history { Input::neutral() } else { input };
                    server.queue(&Message::Input { input, target, sequence: self.history.len() as u64, checksum })?;
                    server.flush()?;
                    self.last_input_send = Instant::now();
                }
            }
            if let Some(frame) = self.incoming_frames.pop_front() {
                if frame.checksum != checksum {
                    return Err(error(format!("Simulation diverged at frame {}", frame.sequence)));
                }
                self.history.push(frame.clone());
                Some(frame)
            } else {
                None
            }
        };
        if let Some(frame) = &frame {
            if let Some(rules) = frame.rules {
                self.applied_rules = rules;
                if self.host || frame.sequence >= self.welcomed_history {
                    self.rules = rules;
                }
            }
            if let Some(from) = frame.migration_from {
                self.controllers.swap(0, from as usize);
            }
            if let Some(members) = &frame.members {
                self.applied_members = members.as_ref().clone();
                // Keep the live roster from Welcome while replaying older membership events.
                if self.host || frame.sequence >= self.welcomed_history {
                    self.members = self.applied_members.clone();
                }
            }
            for (input, controller) in frame.inputs.iter().zip(&mut self.controllers) {
                input.apply(controller);
            }
        }
        Ok(frame)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(server) = &mut self.server {
            let _ = server.queue(&Message::Leave);
            let _ = server.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> Session {
        let mut host =
            Session::connect(Some("127.0.0.1:0".parse().unwrap()), None, "Host", Box::new(ReplayController::new()))
                .unwrap();
        host.assets = Some(42);
        host.ready = true;
        host.bootstrap_data = Some(Bootstrap {
            profile: None,
            seed: 1,
            settings: Vec::new(),
            assets: 42,
            initial_members: host.members.clone(),
            rules: host.rules,
        });
        host
    }

    fn guest(host: &Session, name: &str) -> Session {
        let mut guest =
            Session::connect(None, Some(host.listener.local_addr().unwrap()), name, Box::new(ReplayController::new()))
                .unwrap();
        guest.assets = Some(42);
        guest
    }

    fn advance(host: &mut Session, guests: &mut [&mut Session], ticks: usize) {
        for _ in 0..ticks {
            host.poll(Input::neutral(), host.history.len() as u64).unwrap();
            for guest in guests.iter_mut() {
                guest.pump_transport(guest.history.len() as u64).unwrap();
                if guest.ready {
                    guest.restart_requested = false;
                    for _ in 0..32 {
                        if guest.poll(Input::neutral(), guest.history.len() as u64).unwrap().is_none() {
                            break;
                        }
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn late_join_chat_rename_departure_and_host_migration() {
        let mut host = host();
        advance(&mut host, &mut [], 100);
        let mut first = guest(&host, "Alice");
        let mut second = guest(&host, "Bob");
        advance(&mut host, &mut [&mut first, &mut second], 100);
        assert!(first.ready && second.ready);
        assert_eq!(host.members.iter().flatten().count(), 3);
        assert!(host.history.len() - second.history.len() < 4);
        let slot = first.local_slot;
        first.rename("Alicia").unwrap();
        first.send_chat("Bonjour").unwrap();
        first.last_input_send = Instant::now() - Duration::from_secs(1);
        advance(&mut host, &mut [&mut first, &mut second], 30);
        assert_eq!(host.members[slot].as_ref().unwrap().name, "Alicia");
        assert!(second.chat.iter().any(|line| line.author == "Alicia" && line.text == "Bonjour"));
        drop(first);
        advance(&mut host, &mut [&mut second], 30);
        assert!(host.members[slot].is_none());
        let mut replacement = guest(&host, "Carol");
        advance(&mut host, &mut [&mut second, &mut replacement], 80);
        assert_eq!(replacement.local_slot, slot);
        assert_eq!(host.members.iter().flatten().count(), 3);
        // Lowest occupied slot succeeds the host; other clients retain their slot via token.
        drop(host);
        replacement.pump_transport(replacement.history.len() as u64).unwrap();
        assert!(replacement.host);
        assert_eq!(replacement.local_slot, 0);
        second.pump_transport(second.history.len() as u64).unwrap();
        advance(&mut replacement, &mut [&mut second], 100);
        assert!(!second.host);
        assert_eq!(replacement.members.iter().flatten().count(), 2);
        assert!(replacement.history.iter().any(|f| f.migration_from == Some(slot as u8)));
        assert!(replacement.history.len() - second.history.len() < 4);
    }

    #[test]
    fn older_saved_rules_default_to_freeware_timing() {
        let rules: GameRules = serde_json::from_str(r#"{"individual_cameras":true,"difficulty":"Normal"}"#).unwrap();
        assert_eq!(rules.timing, GameTiming::Freeware);
        assert!(rules.timing.mode() == TimingMode::_50Hz);
        assert!(GameTiming::CSPlus.mode() == TimingMode::_60Hz);
    }

    #[test]
    fn host_rules_and_characters_replay_for_late_joiners() {
        let mut host = host();
        let skin = SkinChoice { texture: 0, offset: 2 };
        host.skin_choices.push(skin);
        let rules = GameRules {
            individual_cameras: true,
            timing: GameTiming::CSPlus,
            difficulty: crate::game::shared_game_state::GameDifficulty::Hard,
        };
        host.set_rules(rules).unwrap();
        host.change_skin(skin).unwrap();
        advance(&mut host, &mut [], 50);
        let mut first = guest(&host, "First");
        first.skin_choices.push(skin);
        advance(&mut host, &mut [&mut first], 80);
        assert_eq!(first.applied_rules, rules);
        assert_eq!(first.applied_members[0].as_ref().unwrap().skin, skin);
        assert!(first.set_rules(GameRules::default()).is_err());
        first.change_skin(skin).unwrap();
        host.set_rules(GameRules::default()).unwrap();
        advance(&mut host, &mut [&mut first], 50);
        assert_eq!(first.applied_rules, GameRules::default());
        assert_eq!(host.members[first.local_slot].as_ref().unwrap().skin, skin);
        let mut late = guest(&host, "Late");
        advance(&mut host, &mut [&mut first, &mut late], 100);
        assert_eq!(late.applied_rules, GameRules::default());
        assert_eq!(late.applied_members[first.local_slot].as_ref().unwrap().skin, skin);
    }

    #[test]
    fn rejects_divergence_without_stopping_host() {
        let mut host = host();
        let mut guest = guest(&host, "Guest");
        advance(&mut host, &mut [&mut guest], 50);
        guest
            .server
            .as_mut()
            .unwrap()
            .queue(&Message::Input { input: Input::neutral(), target: None, sequence: 0, checksum: u64::MAX })
            .unwrap();
        guest.server.as_mut().unwrap().flush().unwrap();
        advance(&mut host, &mut [], 10);
        assert_eq!(host.members.iter().flatten().count(), 1);
        assert!(host.poll(Input::neutral(), host.history.len() as u64).unwrap().is_some());
    }

    #[test]
    fn accepts_eight_players_and_reuses_vacated_slots() {
        let mut host = host();
        let mut guests: Vec<_> = (1..MAX_PLAYERS).map(|slot| guest(&host, &format!("Player{slot}"))).collect();
        advance(&mut host, &mut guests.iter_mut().collect::<Vec<_>>(), 100);
        assert_eq!(host.members.iter().flatten().count(), MAX_PLAYERS);
        assert!(guests.iter().all(|guest| guest.ready));
        let mut excess = guest(&host, "Excess");
        for _ in 0..100 {
            advance(&mut host, &mut guests.iter_mut().collect::<Vec<_>>(), 1);
            if excess.pump_transport(0).is_err() {
                break;
            }
        }
        assert!(!excess.ready);
        let removed = guests.remove(3);
        let slot = removed.local_slot;
        let generation = host.members[slot].as_ref().unwrap().generation;
        drop(removed);
        advance(&mut host, &mut guests.iter_mut().collect::<Vec<_>>(), 20);
        guests.push(guest(&host, "Replacement"));
        advance(&mut host, &mut guests.iter_mut().collect::<Vec<_>>(), 100);
        assert_eq!(host.members.iter().flatten().count(), MAX_PLAYERS);
        assert_eq!(guests.last().unwrap().local_slot, slot);
        assert!(host.members[slot].as_ref().unwrap().generation > generation);
    }

    #[test]
    fn receives_fragmented_packets_and_rejects_oversized_headers() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut sender = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let mut receiver = Connection::new(listener.accept().unwrap().0).unwrap();
        let data = serde_json::to_vec(&Message::Chat("Hello".into())).unwrap();
        let header = (data.len() as u32).to_be_bytes();
        sender.write_all(&header[..2]).unwrap();
        assert!(receiver.pump().unwrap().is_empty());
        sender.write_all(&header[2..]).unwrap();
        sender.write_all(&data[..3]).unwrap();
        assert!(receiver.pump().unwrap().is_empty());
        sender.write_all(&data[3..]).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        assert!(matches!(receiver.pump().unwrap().as_slice(), [Message::Chat(text)] if text == "Hello"));
        sender.write_all(&(MAX_PACKET as u32 + 1).to_be_bytes()).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        assert!(receiver.pump().is_err());
    }

    #[test]
    fn measures_and_shares_round_trip_latency_without_simulation_frames() {
        let mut host = host();
        let mut guest = guest(&host, "Guest");
        advance(&mut host, &mut [&mut guest], 50);
        let slot = guest.local_slot;
        let sequence = host.sequence();
        host.last_ping = Instant::now() - Duration::from_secs(2);
        for _ in 0..20 {
            host.pump_transport(host.sequence()).unwrap();
            guest.pump_transport(guest.sequence()).unwrap();
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(host.pings[slot].is_some());
        assert_eq!(host.sequence(), sequence);
        host.last_ping = Instant::now() - Duration::from_secs(2);
        for _ in 0..20 {
            host.pump_transport(host.sequence()).unwrap();
            guest.pump_transport(guest.sequence()).unwrap();
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(guest.pings[slot].is_some());
        assert_eq!(guest.pings[0], Some(0));
        drop(guest);
        advance(&mut host, &mut [], 10);
        assert!(host.pings[slot].is_none());
    }

    #[test]
    fn schedules_predicted_inputs_at_their_frame_and_clears_departed_players() {
        let mut host = host();
        let mut guest = guest(&host, "Guest");
        advance(&mut host, &mut [&mut guest], 50);
        let slot = guest.local_slot;
        let target = host.sequence() + 5;
        let moving = Input { keys: 2, ..Input::neutral() };
        let sequence = guest.sequence();
        guest
            .server
            .as_mut()
            .unwrap()
            .queue(&Message::Input { input: moving, target: Some(target), sequence, checksum: sequence })
            .unwrap();
        guest.server.as_mut().unwrap().flush().unwrap();
        std::thread::sleep(Duration::from_millis(5));
        for _ in 0..7 {
            let frame = host.poll(Input::neutral(), host.sequence()).unwrap().unwrap();
            assert_eq!(frame.inputs[slot], if frame.sequence < target { Input::neutral() } else { moving });
        }
        host.scheduled_inputs[slot].insert(host.sequence() + 3, moving);
        drop(guest);
        advance(&mut host, &mut [], 10);
        assert!(host.members[slot].is_none());
        assert!(host.scheduled_inputs[slot].is_empty());
        assert_eq!(host.latest_inputs[slot], Input::neutral());
    }

    #[test]
    fn prediction_preserves_held_controls_without_repeating_edges() {
        let mut controller = ReplayController::new();
        let input = Input { keys: 2 | (1 << 6) | (1 << 10), look: 1, analog: [0.5, -0.25] };
        input.apply(&mut controller);
        assert!(controller.trigger_jump());
        Input::held(&controller).apply(&mut controller);
        assert!(controller.move_right() && controller.jump());
        assert!(!controller.trigger_jump() && !controller.trigger_menu_pause());
        assert_eq!(controller.move_analog_x(), 0.5);
    }

    #[test]
    fn validates_names_inputs_and_bounded_chat() {
        assert!(nickname("  Alice  ").unwrap() == "Alice");
        assert!(nickname("\n").is_err());
        assert!(nickname(&"a".repeat(25)).is_err());
        let mut input = Input::neutral();
        input.analog[0] = f32::NAN;
        assert!(!input.valid());
        let mut host = host();
        for _ in 0..100 {
            host.send_chat("hello").unwrap();
        }
        assert_eq!(host.chat.len(), 64);
        assert!(host.send_chat(&"a".repeat(241)).is_err());
    }
}
