//! Native, controller-operated network menus. Opening a menu only releases the local player's inputs.
mod text_editor;
use crate::framework::{
    context::Context,
    error::{GameError, GameResult},
    filesystem,
    keyboard::ScanCode,
};
use crate::game::network::{available_skins, nickname, GameRules, GameTiming, Session, SkinChoice};
use crate::game::profile::GameProfile;
use crate::game::shared_game_state::{GameDifficulty, SharedGameState};
use crate::graphics::font::Font;
use crate::input::combined_menu_controller::CombinedMenuController;
use crate::input::player_controller::PlayerController;
use crate::menu::save_select_menu::MenuSaveInfo;
use crate::menu::{Menu, MenuEntry, MenuSelectionResult};
use crate::scene::loading_scene::LoadingScene;
use text_editor::TextEditor;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Screen {
    #[default]
    Main,
    Host,
    Saves,
    Join,
    Pause,
    Rules,
    Skin,
    Players,
    Player(usize),
    Leave,
    Retry,
    Error,
}
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Entry {
    #[default]
    Name,
    Listen,
    Address,
    Host,
    Join,
    Rules,
    Skin,
    Names,
    Start,
    Save,
    SaveSlot(usize),
    Camera,
    Timing,
    Difficulty,
    Players,
    Page,
    Player(usize),
    Back,
    Retry,
    Leave,
    Resume,
    Yes,
    No,
    Info(u8),
}
#[derive(Clone, Copy)]
enum Field {
    Name,
    Listen,
    Address,
}

pub struct NetworkMenu {
    pub open: bool,
    initialized: bool,
    screen: Screen,
    parent: Screen,
    menu: Menu<Entry>,
    controller: CombinedMenuController,
    pause_controller: crate::input::gamepad_player_controller::GamepadController,
    editor: Option<(Field, TextEditor)>,
    name: String,
    listen: String,
    address: String,
    rules: GameRules,
    skin: SkinChoice,
    save_slot: usize,
    saves: [Option<MenuSaveInfo>; 3],
    invalid_saves: [bool; 3],
    page: usize,
    error: String,
}
impl Default for NetworkMenu {
    fn default() -> Self {
        Self {
            open: false,
            initialized: false,
            screen: Screen::Main,
            parent: Screen::Main,
            menu: Menu::new(0, 0, 200, 0),
            controller: CombinedMenuController::new(),
            pause_controller: crate::input::gamepad_player_controller::GamepadController::new(
                0,
                crate::game::player::TargetPlayer::Player1,
            ),
            editor: None,
            name: String::new(),
            listen: String::new(),
            address: String::new(),
            rules: GameRules::default(),
            skin: SkinChoice::default(),
            save_slot: 1,
            saves: [None; 3],
            invalid_saves: [false; 3],
            page: 0,
            error: String::new(),
        }
    }
}
fn text(state: &SharedGameState, key: &str, fallback: &str) -> String {
    let localized = state.loc.t(key);
    if localized == key {
        fallback.to_owned()
    } else {
        localized.to_owned()
    }
}
fn short(state: &SharedGameState, value: &str, width: f32) -> String {
    let mut result = value.to_owned();
    if state.font.builder().compute_width(&result) <= width {
        return result;
    }
    while !result.is_empty() && state.font.builder().compute_width(&(result.clone() + "...")) > width {
        result.pop();
    }
    result + "..."
}
fn message_lines(state: &SharedGameState, value: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for ch in value.chars() {
        if state.font.builder().compute_width(&(line.clone() + &ch.to_string())) > 240.0 {
            lines.push(std::mem::take(&mut line));
        }
        line.push(ch);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}
impl NetworkMenu {
    fn initialize(&mut self, state: &mut SharedGameState, ctx: &mut Context) -> GameResult {
        self.name = state.settings.network_nickname.clone();
        self.listen = state.settings.network_listen.clone();
        self.address = state.settings.network_address.clone();
        self.rules = state.settings.network_rules;
        self.skin = state.settings.network_skin;
        self.save_slot = state.save_slot.clamp(1, 3);
        self.controller.replace(state.settings.create_player1_controller());
        self.controller.add(Box::new(crate::input::gamepad_player_controller::GamepadController::new(
            0,
            crate::game::player::TargetPlayer::Player1,
        )));
        if let Some(session) = &state.network {
            self.name = session.nickname_draft.clone();
            self.skin = session.skin_draft;
            self.rules = session.rules;
        }
        self.controller.update(state, ctx)?;
        self.controller.update_trigger();
        self.initialized = true;
        Ok(())
    }
    fn switch(&mut self, screen: Screen, selected: Entry) {
        self.screen = screen;
        self.menu.selected = selected;
    }
    fn close(&mut self, state: &mut SharedGameState, ctx: &mut Context) {
        self.open = false;
        self.initialized = false;
        self.editor = None;
        ctx.keyboard_context.native_text_input = false;
        ctx.keyboard_context.take_text_input();
        if let Some(session) = &mut state.network {
            session.options_open = false;
            session.chat_open = false;
        }
    }
    pub fn process_key(&mut self, ctx: &mut Context, key: ScanCode) -> bool {
        if let Some((_, editor)) = &mut self.editor {
            editor.key(key, ctx.keyboard_context.active_mods().ctrl());
            return true;
        }
        false
    }
    pub fn tick_ingame(&mut self, state: &mut SharedGameState, ctx: &mut Context) -> GameResult {
        self.pause_controller.update(state, ctx)?;
        self.pause_controller.update_trigger();
        let session = state.network.as_ref().unwrap();
        if session.chat_open {
            return Ok(());
        }
        let pause = session.local_controller.trigger_menu_pause() || self.pause_controller.trigger_menu_pause();
        if pause && self.editor.is_none() {
            if self.open && self.screen == Screen::Pause {
                self.close(state, ctx);
                return Ok(());
            }
            self.open = true;
            self.switch(Screen::Pause, Entry::Resume);
            state.network.as_mut().unwrap().options_open = true;
            state.network.as_mut().unwrap().chat_open = false;
            self.initialize(state, ctx)?;
            self.rebuild(state);
            return Ok(());
        }
        let session = state.network.as_ref().unwrap();
        if !self.open && session.options_open {
            self.open = true;
            self.switch(Screen::Pause, Entry::Resume);
        }
        self.tick(state, ctx)?;
        if let Some(session) = &mut state.network {
            session.options_open = self.open;
        }
        Ok(())
    }
    pub fn tick(&mut self, state: &mut SharedGameState, ctx: &mut Context) -> GameResult {
        if !self.open {
            self.initialized = false;
            return Ok(());
        }
        if !self.initialized {
            self.initialize(state, ctx)?;
            self.rebuild(state);
            return Ok(());
        }
        self.controller.update(state, ctx)?;
        self.controller.update_trigger();
        if let Some((field, editor)) = &mut self.editor {
            if let Some(accept) = editor.tick(&self.controller, state, ctx) {
                let field = *field;
                let value = editor.value.clone();
                self.editor = None;
                ctx.keyboard_context.native_text_input = false;
                if accept {
                    if let Err(error) = self.apply_text(field, value, state, ctx) {
                        self.fail(error);
                    }
                }
            }
            self.rebuild(state);
            return Ok(());
        }
        if let Some(session) = &state.network {
            self.rules = session.rules;
            self.skin = session.skin_draft;
        }
        self.rebuild(state);
        let event = match self.menu.tick(&mut self.controller, state) {
            MenuSelectionResult::Selected(entry, _) => Some((entry, 1)),
            MenuSelectionResult::Left(entry, _, _) => Some((entry, -1)),
            MenuSelectionResult::Right(entry, _, _) => Some((entry, 1)),
            MenuSelectionResult::Canceled => Some((Entry::Back, 1)),
            _ => None,
        };
        if let Some((entry, direction)) = event {
            if let Err(error) = self.activate(entry, direction, state, ctx) {
                self.fail(error);
            }
        }
        self.rebuild(state);
        Ok(())
    }
    fn fail(&mut self, error: GameError) {
        self.error = error.to_string();
        self.parent = self.screen;
        self.switch(Screen::Error, Entry::Back);
    }
    fn edit(&mut self, field: Field, title: String, value: String, limit: usize, ctx: &mut Context) {
        self.editor = Some((field, TextEditor::new(title, value, limit)));
        ctx.keyboard_context.native_text_input = true;
        ctx.keyboard_context.take_text_input();
    }
    fn persist(&self, state: &mut SharedGameState, ctx: &mut Context) -> GameResult {
        if let Some(session) = &mut state.network {
            if let Some(settings) = &mut session.local_settings {
                settings.network_nickname = state.settings.network_nickname.clone();
                settings.network_skin = state.settings.network_skin;
                settings.network_rules = state.settings.network_rules;
                settings.show_player_names = state.settings.show_player_names;
                settings.save(ctx)?;
            }
        } else {
            state.settings.save(ctx)?;
        }
        Ok(())
    }
    fn apply_text(
        &mut self,
        field: Field,
        value: String,
        state: &mut SharedGameState,
        ctx: &mut Context,
    ) -> GameResult {
        match field {
            Field::Name => {
                let name = nickname(&value).map_err(GameError::ConfigError)?;
                if let Some(session) = &mut state.network {
                    session.rename(&name)?;
                }
                state.settings.network_nickname = name.clone();
                self.name = name;
            }
            Field::Listen | Field::Address => {
                let value = value.trim().to_owned();
                value
                    .parse::<std::net::SocketAddr>()
                    .map_err(|_| GameError::ConfigError("Enter IP:port (for example 192.168.1.10:28000)".into()))?;
                match field {
                    Field::Listen => {
                        self.listen = value.clone();
                        state.settings.network_listen = value;
                    }
                    _ => {
                        self.address = value.clone();
                        state.settings.network_address = value;
                    }
                }
            }
        }
        self.persist(state, ctx)
    }
    fn activate(
        &mut self,
        entry: Entry,
        direction: isize,
        state: &mut SharedGameState,
        ctx: &mut Context,
    ) -> GameResult {
        match entry {
            Entry::Name => self.edit(
                Field::Name,
                text(state, "menus.network_menu.nickname", "Nickname"),
                self.name.clone(),
                24,
                ctx,
            ),
            Entry::Listen => self.edit(Field::Listen, "Listen IP:port".into(), self.listen.clone(), 64, ctx),
            Entry::Address => self.edit(Field::Address, "Server IP:port".into(), self.address.clone(), 64, ctx),
            Entry::Host => self.switch(Screen::Host, Entry::Start),
            Entry::Save => {
                for i in 0..3 {
                    self.saves[i] = None;
                    self.invalid_saves[i] = false;
                    if let Some(path) = state.get_save_filename(i + 1) {
                        if let Ok(data) = filesystem::user_open(ctx, path) {
                            match GameProfile::load_from_save(data) {
                                Ok(profile) if (profile.current_map as usize) < state.stages.len() => {
                                    self.saves[i] = Some(MenuSaveInfo {
                                        current_map: profile.current_map,
                                        life: profile.life,
                                        max_life: profile.max_life,
                                        weapon_count: profile.weapon_data.iter().filter(|w| w.weapon_id != 0).count(),
                                        weapon_id: profile.weapon_data.map(|w| w.weapon_id),
                                        difficulty: profile.difficulty,
                                    });
                                }
                                _ => self.invalid_saves[i] = true,
                            }
                        }
                    }
                }
                self.switch(Screen::Saves, Entry::SaveSlot(self.save_slot));
            }
            Entry::SaveSlot(slot) => {
                self.save_slot = slot;
                self.switch(Screen::Host, Entry::Start);
            }
            Entry::Join => self.switch(Screen::Join, Entry::Address),
            Entry::Rules => {
                self.parent = self.screen;
                self.switch(Screen::Rules, Entry::Camera);
            }
            Entry::Skin if self.screen != Screen::Skin => {
                self.parent = self.screen;
                self.switch(Screen::Skin, Entry::Skin);
            }
            Entry::Skin => {
                let skins = available_skins(state);
                let i = skins.iter().position(|skin| *skin == self.skin).unwrap_or(0);
                self.skin = skins[(i as isize + direction).rem_euclid(skins.len() as isize) as usize];
                if let Some(session) = &mut state.network {
                    session.change_skin(self.skin)?;
                }
                state.settings.network_skin = self.skin;
                self.persist(state, ctx)?;
            }
            Entry::Camera | Entry::Timing | Entry::Difficulty => {
                if entry == Entry::Camera {
                    self.rules.individual_cameras = !self.rules.individual_cameras;
                } else if entry == Entry::Timing {
                    self.rules.timing = if self.rules.timing == GameTiming::Freeware {
                        GameTiming::CSPlus
                    } else {
                        GameTiming::Freeware
                    };
                } else {
                    let difficulties = [GameDifficulty::Easy, GameDifficulty::Normal, GameDifficulty::Hard];
                    let i = difficulties.iter().position(|d| *d == self.rules.difficulty).unwrap_or(1);
                    self.rules.difficulty = difficulties[(i as isize + direction).rem_euclid(3) as usize];
                }
                if let Some(session) = &mut state.network {
                    session.set_rules(self.rules)?;
                }
                state.settings.network_rules = self.rules;
                self.persist(state, ctx)?;
            }
            Entry::Names => {
                state.settings.show_player_names = !state.settings.show_player_names;
                self.persist(state, ctx)?;
            }
            Entry::Players => {
                self.page = 0;
                self.switch(Screen::Players, Entry::Page);
            }
            Entry::Page => {
                let count = state.network.as_ref().unwrap().members.iter().flatten().count();
                self.page = (self.page as isize + direction).rem_euclid(((count + 3) / 4).max(1) as isize) as usize;
            }
            Entry::Player(slot) => self.switch(Screen::Player(slot), Entry::Back),
            Entry::Resume => self.close(state, ctx),
            Entry::Leave | Entry::Retry => {
                self.switch(if entry == Entry::Leave { Screen::Leave } else { Screen::Retry }, Entry::No)
            }
            Entry::No => self.switch(Screen::Pause, Entry::Resume),
            Entry::Yes => {
                if self.screen == Screen::Leave {
                    state.network.as_mut().unwrap().leave_requested = true;
                } else {
                    state.network.as_mut().unwrap().retry_requested = true;
                }
                self.close(state, ctx);
            }
            Entry::Back => match self.screen {
                Screen::Main | Screen::Pause => self.close(state, ctx),
                Screen::Host | Screen::Join => self.switch(Screen::Main, Entry::Host),
                Screen::Saves => self.switch(Screen::Host, Entry::Save),
                Screen::Rules | Screen::Skin | Screen::Error => self.switch(self.parent, Entry::Back),
                Screen::Player(_) => self.switch(Screen::Players, Entry::Page),
                _ => self.switch(Screen::Pause, Entry::Resume),
            },
            Entry::Start => {
                let host = self.screen == Screen::Host;
                let name = nickname(&self.name).map_err(GameError::ConfigError)?;
                let address = if host { &self.listen } else { &self.address }
                    .parse()
                    .map_err(|_| GameError::ConfigError("Enter IP:port".into()))?;
                let mut session = Session::connect(
                    if host { Some(address) } else { None },
                    if host { None } else { Some(address) },
                    &name,
                    state.settings.create_player1_controller(),
                )?;
                state.settings.network_nickname = name;
                state.settings.network_rules = self.rules;
                state.settings.network_skin = self.skin;
                state.settings.network_listen = self.listen.clone();
                state.settings.network_address = self.address.clone();
                state.settings.save(ctx)?;
                session.remember_settings(&state.settings)?;
                state.settings.pause_on_focus_loss = false;
                if host {
                    state.save_slot = self.save_slot;
                }
                state.network = Some(session);
                ctx.keyboard_context.native_text_input = false;
                state.mod_path = None;
                state.next_scene = Some(Box::new(LoadingScene::new()));
            }
            _ => (),
        }
        Ok(())
    }
    fn rebuild(&mut self, state: &SharedGameState) {
        self.menu.entries.clear();
        self.menu.height_overrides.clear();
        self.menu.width = 200;
        let back = text(state, "common.back", "Back");
        match self.screen {
            Screen::Main => {
                self.menu.push_entry(
                    Entry::Info(0),
                    MenuEntry::Title(text(state, "menus.network_menu.title", "Network multiplayer"), true, true),
                );
                self.menu.push_entry(
                    Entry::Name,
                    MenuEntry::Active(format!(
                        "{}: {}",
                        text(state, "menus.network_menu.nickname", "Nickname"),
                        short(state, &self.name, 130.0)
                    )),
                );
                self.menu.push_entry(Entry::Skin, MenuEntry::Active(text(state, "menus.skin_menu.label", "Character")));
                self.menu
                    .push_entry(Entry::Rules, MenuEntry::Active(text(state, "menus.network_menu.rules", "Game rules")));
                self.menu
                    .push_entry(Entry::Host, MenuEntry::Active(text(state, "menus.network_menu.host", "Host game")));
                self.menu
                    .push_entry(Entry::Join, MenuEntry::Active(text(state, "menus.network_menu.join", "Join game")));
            }
            Screen::Saves => {
                self.menu.width = 230;
                let label = text(state, "menus.network_menu.save", "Save file");
                self.menu.push_entry(Entry::Info(0), MenuEntry::Title(label.clone(), true, true));
                for i in 0..3 {
                    self.menu.push_entry(Entry::Info(i as u8 + 1), MenuEntry::Disabled(format!("{label} {}", i + 1)));
                    self.menu.push_entry(
                        Entry::SaveSlot(i + 1),
                        if self.invalid_saves[i] {
                            MenuEntry::Disabled(state.loc.t("menus.save_menu.invalid_save").to_owned())
                        } else if let Some(save) = self.saves[i] {
                            MenuEntry::SaveData(save)
                        } else {
                            MenuEntry::NewSave
                        },
                    );
                }
            }
            Screen::Host | Screen::Join => {
                let host = self.screen == Screen::Host;
                self.menu.push_entry(
                    Entry::Info(0),
                    MenuEntry::Title(
                        text(
                            state,
                            if host { "menus.network_menu.host" } else { "menus.network_menu.join" },
                            if host { "Host game" } else { "Join game" },
                        ),
                        true,
                        true,
                    ),
                );
                self.menu.push_entry(
                    if host { Entry::Listen } else { Entry::Address },
                    MenuEntry::Active(short(state, if host { &self.listen } else { &self.address }, 240.0)),
                );
                if host {
                    self.menu.push_entry(
                        Entry::Save,
                        MenuEntry::Active(format!(
                            "{}: {}",
                            text(state, "menus.network_menu.save", "Save file"),
                            self.save_slot
                        )),
                    );
                    self.menu.push_entry(
                        Entry::Rules,
                        MenuEntry::Active(text(state, "menus.network_menu.rules", "Game rules")),
                    );
                }
                self.menu.push_entry(
                    Entry::Start,
                    MenuEntry::Active(text(
                        state,
                        if host { "menus.network_menu.host" } else { "menus.network_menu.join" },
                        if host { "Host game" } else { "Join game" },
                    )),
                );
            }
            Screen::Pause => {
                self.menu
                    .push_entry(Entry::Resume, MenuEntry::Active(text(state, "menus.pause_menu.resume", "Resume")));
                self.menu.push_entry(
                    Entry::Players,
                    MenuEntry::Active(text(state, "menus.network_menu.players", "Players")),
                );
                self.menu
                    .push_entry(Entry::Name, MenuEntry::Active(text(state, "menus.network_menu.nickname", "Nickname")));
                self.menu.push_entry(Entry::Skin, MenuEntry::Active(text(state, "menus.skin_menu.label", "Character")));
                self.menu
                    .push_entry(Entry::Rules, MenuEntry::Active(text(state, "menus.network_menu.rules", "Game rules")));
                self.menu.push_entry(
                    Entry::Names,
                    MenuEntry::Toggle(
                        text(state, "menus.options_menu.behavior_menu.show_player_names", "Show player names"),
                        state.settings.show_player_names,
                    ),
                );
                if state.network.as_ref().unwrap().host {
                    self.menu.push_entry(
                        Entry::Retry,
                        MenuEntry::Active(text(state, "menus.pause_menu.retry", "Retry checkpoint")),
                    );
                }
                self.menu
                    .push_entry(Entry::Leave, MenuEntry::Active(text(state, "menus.network_menu.leave", "Leave game")));
            }
            Screen::Rules => {
                let host = state.network.as_ref().map_or(true, |s| s.host);
                let camera = text(state, "menus.network_menu.cameras", "Individual cameras");
                self.menu.push_entry(
                    Entry::Camera,
                    if host {
                        MenuEntry::Toggle(camera, self.rules.individual_cameras)
                    } else {
                        MenuEntry::Disabled(format!(
                            "{camera}: {}",
                            if self.rules.individual_cameras { "On" } else { "Off" }
                        ))
                    },
                );
                let timing = text(state, "menus.options_menu.behavior_menu.game_timing.entry", "Game timing");
                let modes = vec!["50 Hz (Freeware)".into(), "60 Hz (CS+)".into()];
                let index = if self.rules.timing == GameTiming::Freeware { 0 } else { 1 };
                self.menu.push_entry(
                    Entry::Timing,
                    if host {
                        MenuEntry::Options(timing, index, modes)
                    } else {
                        MenuEntry::Disabled(format!("{timing}: {}", modes[index]))
                    },
                );
                let difficulty = text(state, "menus.network_menu.difficulty", "Difficulty");
                self.menu.push_entry(
                    Entry::Difficulty,
                    if host {
                        MenuEntry::Options(
                            difficulty,
                            match self.rules.difficulty {
                                GameDifficulty::Easy => 0,
                                GameDifficulty::Normal => 1,
                                GameDifficulty::Hard => 2,
                            },
                            vec!["Easy".into(), "Normal".into(), "Hard".into()],
                        )
                    } else {
                        MenuEntry::Disabled(format!("{difficulty}: {:?}", self.rules.difficulty))
                    },
                );
            }
            Screen::Skin => {
                self.menu.push_entry(
                    Entry::Info(0),
                    MenuEntry::Title(text(state, "menus.skin_menu.label", "Character"), true, true),
                );
                self.menu.push_entry(
                    Entry::Skin,
                    MenuEntry::PlayerPreview(text(state, "menus.skin_menu.label", "Character"), self.skin, None, true),
                );
                self.menu.push_entry(Entry::Info(1), MenuEntry::Disabled(self.skin.label(state)));
            }
            Screen::Players => {
                let session = state.network.as_ref().unwrap();
                let members: Vec<_> =
                    session.members.iter().enumerate().filter_map(|(i, m)| m.as_ref().map(|m| (i, m))).collect();
                let pages = ((members.len() + 3) / 4).max(1);
                self.page = self.page.min(pages - 1);
                self.menu.width = 270;
                self.menu.push_entry(
                    Entry::Info(0),
                    MenuEntry::Title(
                        format!("{} ({}/8)", text(state, "menus.network_menu.players", "Players"), members.len()),
                        true,
                        true,
                    ),
                );
                self.menu.push_entry(Entry::Info(1), MenuEntry::Disabled("Ping: RTT to host".into()));
                for &(slot, member) in members.iter().skip(self.page * 4).take(4) {
                    self.menu.push_entry(
                        Entry::Player(slot),
                        MenuEntry::PlayerPreview(
                            short(state, &member.name, 140.0),
                            member.skin,
                            session.pings[slot],
                            true,
                        ),
                    );
                }
                self.menu.push_entry(
                    Entry::Page,
                    MenuEntry::Options("Page".into(), self.page, (1..=pages).map(|n| format!("{n}/{pages}")).collect()),
                );
            }
            Screen::Player(slot) => {
                let session = state.network.as_ref().unwrap();
                if let Some(member) = &session.members[slot] {
                    for (i, line) in message_lines(state, &member.name).into_iter().enumerate() {
                        self.menu.push_entry(Entry::Info(10 + i as u8), MenuEntry::Disabled(line));
                    }
                    self.menu.push_entry(
                        Entry::Info(1),
                        MenuEntry::PlayerPreview(
                            if slot == session.local_slot {
                                "You"
                            } else if slot == 0 {
                                "Host"
                            } else {
                                "Player"
                            }
                            .into(),
                            member.skin,
                            session.pings[slot],
                            false,
                        ),
                    );
                    if session.host {
                        if let Some(address) = member.address {
                            for (i, line) in message_lines(state, &address.to_string()).into_iter().enumerate() {
                                self.menu.push_entry(Entry::Info(20 + i as u8), MenuEntry::Disabled(line));
                            }
                        }
                    }
                }
            }
            Screen::Leave | Screen::Retry => {
                let prompt = text(
                    state,
                    if self.screen == Screen::Leave {
                        "menus.network_menu.leave_confirm"
                    } else {
                        "menus.network_menu.retry_confirm"
                    },
                    if self.screen == Screen::Leave { "Leave this game?" } else { "Restart for all players?" },
                );
                for (i, line) in message_lines(state, &prompt).into_iter().enumerate() {
                    self.menu.push_entry(Entry::Info(i as u8), MenuEntry::Disabled(line));
                }
                self.menu.push_entry(Entry::Yes, MenuEntry::Active(text(state, "common.yes", "Yes")));
                self.menu.push_entry(Entry::No, MenuEntry::Active(text(state, "common.no", "No")));
            }
            Screen::Error => {
                for (i, line) in message_lines(state, &self.error).into_iter().enumerate() {
                    self.menu.push_entry(Entry::Info(i as u8), MenuEntry::Disabled(line));
                }
            }
        }
        if self.screen != Screen::Pause {
            self.menu.push_entry(Entry::Back, MenuEntry::Active(back));
        }
        if !self.menu.entries.iter().any(|(id, entry)| *id == self.menu.selected && entry.selectable()) {
            self.menu.selected =
                self.menu.entries.iter().find(|(_, entry)| entry.selectable()).map_or(Entry::Back, |(id, _)| *id);
        }
        self.menu.update_width(state);
        self.menu.update_height(state);
        self.menu.x = ((state.canvas_size.0 - self.menu.width as f32) / 2.0).floor() as isize;
        self.menu.y = ((state.canvas_size.1 - self.menu.height as f32) / 2.0).floor() as isize;
    }
    pub fn draw(&self, state: &mut SharedGameState, ctx: &mut Context) -> GameResult {
        if !self.open {
            return Ok(());
        }
        if let Some((_, editor)) = &self.editor {
            editor.draw(state, ctx)
        } else {
            self.menu.draw(state, ctx)
        }
    }
}
