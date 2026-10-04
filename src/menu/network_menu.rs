use crate::framework::context::Context;
use crate::framework::error::{GameError, GameResult};
use crate::game::network::{nickname, Session};
use crate::game::shared_game_state::SharedGameState;
use crate::scene::loading_scene::LoadingScene;

#[derive(Default)]
pub struct NetworkMenu {
    pub open: bool,
    name: String,
    listen: String,
    address: String,
    error: String,
    action: Option<bool>,
    initialized: bool,
    rules: crate::game::network::GameRules,
    skin: crate::game::network::SkinChoice,
}

impl NetworkMenu {
    pub fn tick(&mut self, state: &mut SharedGameState, ctx: &mut Context) -> GameResult {
        if !self.open {
            self.initialized = false;
            return Ok(());
        }
        if !self.initialized {
            self.name = state.settings.network_nickname.clone();
            self.rules = state.settings.network_rules;
            self.skin = state.settings.network_skin;
            self.listen = state.settings.network_listen.clone();
            self.address = state.settings.network_address.clone();
            self.initialized = true;
        }
        if let Some(host) = self.action.take() {
            let result = (|| -> GameResult {
                let name = nickname(&self.name).map_err(GameError::ConfigError)?;
                let address = if host { &self.listen } else { &self.address };
                let address = address.parse().map_err(|_| {
                    GameError::ConfigError("Enter an IP address and port, for example 192.168.1.10:28000".into())
                })?;
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
                state.network = Some(session);
                state.mod_path = None;
                state.next_scene = Some(Box::new(LoadingScene::new()));
                Ok(())
            })();
            if let Err(error) = result {
                self.error = error.to_string();
            }
        }
        Ok(())
    }

    pub fn draw_ui(&mut self, state: &mut SharedGameState, ctx: &mut Context, ui: &imgui::Ui) {
        if !self.open {
            return;
        }
        let size = [state.screen_size.0.min(500.0) - 24.0, state.screen_size.1.min(544.0) - 24.0];
        let position = [(state.screen_size.0 - size[0]) / 2.0, (state.screen_size.1 - size[1]) / 2.0];
        ui.window("Network multiplayer")
            .position(position, imgui::Condition::Always)
            .size(size, imgui::Condition::Always)
            .collapsible(false)
            .resizable(false)
            .movable(false)
            .build(|| {
                ui.text("Up to 8 players. You can join a running game.");
                ui.input_text("Nickname", &mut self.name).build();
                skin_picker(ui, state, ctx, &mut self.skin);
                rules_picker(ui, &mut self.rules);
                ui.input_text("Listen IP:port", &mut self.listen).build();
                if ui.button("Host game") {
                    self.action = Some(true);
                    self.error.clear();
                }
                ui.separator();
                ui.input_text("Server IP:port", &mut self.address).build();
                if ui.button("Join game") {
                    self.action = Some(false);
                    self.error.clear();
                }
                ui.separator();
                if ui.checkbox("Show player names", &mut state.settings.show_player_names) {
                    let _ = state.settings.save(ctx);
                }
                if !self.error.is_empty() {
                    ui.text_wrapped(&self.error);
                }
                if ui.button("Back") || ui.is_key_pressed(imgui::Key::Escape) {
                    self.open = false;
                }
            });
    }
}

fn rules_picker(ui: &imgui::Ui, rules: &mut crate::game::network::GameRules) -> bool {
    use crate::game::shared_game_state::GameDifficulty;
    let mut changed = ui.checkbox("Individual cameras", &mut rules.individual_cameras);
    let mut difficulty = match rules.difficulty {
        GameDifficulty::Easy => 0,
        GameDifficulty::Normal => 1,
        GameDifficulty::Hard => 2,
    };
    if ui.combo_simple_string("Difficulty", &mut difficulty, &["Easy", "Normal", "Hard"]) {
        rules.difficulty = [GameDifficulty::Easy, GameDifficulty::Normal, GameDifficulty::Hard][difficulty];
        changed = true;
    }
    changed
}

fn skin_picker(
    ui: &imgui::Ui,
    state: &mut SharedGameState,
    ctx: &mut Context,
    skin: &mut crate::game::network::SkinChoice,
) -> bool {
    let choices = crate::game::network::available_skins(state);
    let labels: Vec<_> = choices
        .iter()
        .map(|choice| {
            format!("{} #{}", state.constants.player_skin_paths[choice.texture as usize], choice.offset / 2 + 1)
        })
        .collect();
    let mut selected = choices.iter().position(|choice| choice == skin).unwrap_or(0);
    let changed = ui.combo_simple_string("Character", &mut selected, &labels);
    *skin = choices[selected];
    let path = &state.constants.player_skin_paths[skin.texture as usize];
    if let Ok(batch) = state.texture_set.get_or_load_batch(ctx, &state.constants, path) {
        if let Some(texture) = batch.get_texture() {
            if let Ok(id) = crate::framework::graphics::imgui_texture_id(ctx, texture) {
                let (width, height) = (batch.width() as f32, batch.height() as f32);
                let top = skin.offset as f32 * 32.0;
                imgui::Image::new(id, [32.0, 32.0])
                    .uv0([0.0, top / height])
                    .uv1([16.0 / width, (top + 16.0) / height])
                    .build(ui);
            }
        }
    }
    changed
}

/// Per-client controls never pause or disconnect the other participants.
pub fn draw_ingame(state: &mut SharedGameState, ctx: &mut Context, ui: &imgui::Ui) {
    let Some(mut session) = state.network.take() else {
        return;
    };
    if session.options_open {
        ui.window("Network options")
            .position([24.0, 24.0], imgui::Condition::FirstUseEver)
            .size(
                [420.0f32.min(state.screen_size.0 - 48.0), (state.screen_size.1 - 48.0).min(520.0)],
                imgui::Condition::FirstUseEver,
            )
            .collapsible(false)
            .build(|| {
                ui.text(format!("Server: {}", session.address()));
                ui.text(format!("{} / 8 players", session.members.iter().flatten().count()));
                for (index, member) in session.members.iter().enumerate() {
                    if let Some(member) = member {
                        ui.text(format!("{}{}", member.name, if index == session.local_slot { " (you)" } else { "" }));
                    }
                }
                ui.separator();
                if skin_picker(ui, state, ctx, &mut session.skin_draft) {
                    let skin = session.skin_draft;
                    if let Err(error) = session.change_skin(skin) {
                        session.menu_error = error.to_string();
                    } else {
                        state.settings.network_skin = skin;
                        if let Some(settings) = &mut session.local_settings {
                            settings.network_skin = skin;
                            let _ = settings.save(ctx);
                        }
                    }
                }
                if session.host {
                    let mut rules = session.rules;
                    if rules_picker(ui, &mut rules) {
                        session.set_rules(rules).ok();
                        state.settings.network_rules = rules;
                        if let Some(settings) = &mut session.local_settings {
                            settings.network_rules = rules;
                            let _ = settings.save(ctx);
                        }
                    }
                } else {
                    ui.text(format!(
                        "Camera: {} | Difficulty: {:?}",
                        if session.rules.individual_cameras { "Individual" } else { "Shared" },
                        session.rules.difficulty
                    ));
                }
                ui.input_text("Nickname", &mut session.nickname_draft).build();
                if ui.button("Apply nickname") {
                    let name = session.nickname_draft.clone();
                    match session.rename(&name) {
                        Ok(()) => {
                            session.menu_error.clear();
                            state.settings.network_nickname = name.clone();
                            if let Some(settings) = &mut session.local_settings {
                                settings.network_nickname = name;
                                let _ = settings.save(ctx);
                            }
                        }
                        Err(error) => {
                            session.menu_error = error.to_string();
                        }
                    }
                }
                if !session.menu_error.is_empty() {
                    ui.text_wrapped(&session.menu_error);
                }
                if ui.checkbox("Show player names", &mut state.settings.show_player_names) {
                    if let Some(settings) = &mut session.local_settings {
                        settings.show_player_names = state.settings.show_player_names;
                        let _ = settings.save(ctx);
                    }
                }
                if session.host && ui.button("Retry shared checkpoint") {
                    session.retry_requested = true;
                    session.options_open = false;
                }
                if ui.button("Chat") {
                    session.chat_open = true;
                    session.options_open = false;
                }
                if ui.button("Leave game") {
                    session.leave_requested = true;
                }
                if ui.button("Resume") {
                    session.options_open = false;
                }
            });
    }
    if session.chat_open {
        ui.window("Chat")
            .position([16.0, state.screen_size.1 - 290.0], imgui::Condition::FirstUseEver)
            .size([state.screen_size.0 - 32.0, 270.0], imgui::Condition::FirstUseEver)
            .collapsible(false)
            .build(|| {
                ui.child_window("Messages").size([0.0, 180.0]).build(|| {
                    let follow = ui.is_window_appearing() || ui.scroll_y() >= ui.scroll_max_y() - 2.0;
                    for line in &session.chat {
                        ui.text_wrapped(format!("{}: {}", line.author, line.text));
                    }
                    if follow {
                        ui.set_scroll_here_y_with_ratio(1.0);
                    }
                });
                ui.set_keyboard_focus_here();
                let send = ui.input_text("##Message", &mut session.chat_draft).enter_returns_true(true).build();
                if send || ui.button("Send") {
                    let text = session.chat_draft.clone();
                    if session.send_chat(&text).is_ok() {
                        session.chat_draft.clear();
                    }
                }
                ui.same_line();
                if ui.button("Close") {
                    session.chat_open = false;
                }
            });
    }
    state.network = Some(session);
}
