use std::cell::RefCell;
use std::ops::{ControlFlow, Deref, Range};
use std::rc::Rc;

use log::info;

use crate::common::{interpolate_fix9_scale, Color, Direction, Rect};
use crate::components::background::Background;
use crate::components::boss_life_bar::BossLifeBar;
use crate::components::credits::Credits;
use crate::components::draw_common::Alignment;
use crate::components::fade::Fade;
use crate::components::falling_island::FallingIsland;
use crate::components::flash::Flash;
use crate::components::hud::HUD;
use crate::components::inventory::InventoryUI;
use crate::components::map_system::MapSystem;
use crate::components::nikumaru::NikumaruCounter;
use crate::components::replay::Replay;
use crate::components::stage_select::StageSelect;
use crate::components::text_boxes::TextBoxes;
use crate::components::tilemap::{TileLayer, Tilemap};
use crate::components::water_renderer::{WaterLayer, WaterRenderer};
use crate::components::whimsical_star::WhimsicalStar;
use crate::entity::GameEntity;
use crate::framework::backend::SpriteBatchCommand;
use crate::framework::context::Context;
use crate::framework::error::{map_err_to_break, GameResult};
use crate::framework::graphics::{draw_rect, BlendMode, FilterMode};
use crate::framework::keyboard::ScanCode;
use crate::framework::ui::Components;
use crate::framework::{filesystem, gamepad, graphics};
use crate::game::caret::CaretType;
use crate::game::frame::{Frame, UpdateTarget};
use crate::game::inventory::{Inventory, TakeExperienceResult};
use crate::game::map::WaterParams;
use crate::game::npc::boss::{BossNPC, BossNPCContext};
use crate::game::npc::list::{NPCAccessToken, NPCList, NPCTokenProvider};
use crate::game::npc::{NPCContext, NPCLayer, NPC};
use crate::game::physics::{PhysicalEntity, OFFSETS};
use crate::game::player::{ControlMode, Player, TargetPlayer};
use crate::game::scripting::tsc::credit_script::CreditScriptVM;
use crate::game::scripting::tsc::text_script::{ScriptMode, TextScriptExecutionState, TextScriptVM};
use crate::game::settings::ControllerType;
use crate::game::shared_game_state::{CutsceneSkipMode, PlayerCount, ReplayState, SharedGameState, TileSize};
use crate::game::stage::{BackgroundType, Stage, StageTexturePaths};
use crate::game::weapon::bullet::BulletManager;
use crate::game::weapon::{Weapon, WeaponType};
use crate::graphics::font::{Font, Symbols};
use crate::graphics::texture_set::SpriteBatch;
use crate::input::touch_controls::TouchControlType;
use crate::menu::pause_menu::PauseMenu;
use crate::scene::title_scene::TitleScene;
use crate::scene::Scene;
use crate::util::rng::RNG;

mod rollback;
mod network_chat;
mod network_inventory;

pub struct GameScene {
    pub tick: u32,
    pub stage: Stage,
    pub water_params: WaterParams,
    pub water_renderer: WaterRenderer,
    pub boss_life_bar: BossLifeBar,
    pub stage_select: StageSelect,
    pub flash: Flash,
    pub credits: Credits,
    pub falling_island: FallingIsland,
    pub inventory_ui: InventoryUI,
    network_inventories: [Option<network_inventory::NetworkInventory>; crate::game::network::MAX_PLAYERS],
    pub map_system: MapSystem,
    network_map_system: MapSystem,
    pub hud_player1: HUD,
    pub hud_player2: HUD,
    pub nikumaru: NikumaruCounter,
    pub whimsical_star: WhimsicalStar,
    pub background: Background,
    pub tilemap: Tilemap,
    pub text_boxes: TextBoxes,
    pub fade: Fade,
    pub frame: Frame,
    pub network_cameras: [Frame; crate::game::network::MAX_PLAYERS],
    pub player1: Player,
    pub player2: Player,
    pub inventory_player1: Inventory,
    pub inventory_player2: Inventory,
    pub remote_players: Vec<crate::game::player::player_list::RemotePlayer>,
    pub player_generations: [u32; crate::game::network::MAX_PLAYERS],
    pub(crate) network_game_over: bool,
    prediction: rollback::Prediction,
    network_menu: crate::menu::network_menu::NetworkMenu,
    network_chat: network_chat::NetworkChat,
    // Cache the tile hash; compare bytes to detect edits, including rollback.
    checksum_tiles: (Vec<u8>, u64),
    checksum_buffer: String,
    confirmed_checksum: Option<(u64, u64)>,
    pub stage_id: usize,
    pub npc_list: NPCList,
    pub npc_token: NPCAccessToken,
    pub boss: BossNPC,
    pub bullet_manager: BulletManager,
    pub lighting_mode: LightingMode,
    pub intro_mode: bool,
    pub pause_menu: PauseMenu,
    pub stage_textures: Rc<RefCell<StageTexturePaths>>,
    pub replay: Replay,
    map_name_counter: u16,
    skip_counter: u16,
    inventory_dim: f32,
}

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub enum LightingMode {
    None,
    BackgroundOnly,
    Ambient,
}

const P2_OFFSCREEN_TEXT: &'static str = "P2";
const CUTSCENE_SKIP_WAIT: u16 = 50;

impl GameScene {
    pub fn player_at(&self, index: usize) -> &Player {
        match index {
            0 => &self.player1,
            1 => &self.player2,
            i => &self.remote_players[i - 2].player,
        }
    }
    pub fn player_at_mut(&mut self, index: usize) -> &mut Player {
        match index {
            0 => &mut self.player1,
            1 => &mut self.player2,
            i => &mut self.remote_players[i - 2].player,
        }
    }
    pub fn inventory_at(&self, index: usize) -> &Inventory {
        match index {
            0 => &self.inventory_player1,
            1 => &self.inventory_player2,
            i => &self.remote_players[i - 2].inventory,
        }
    }
    fn ensure_network_players(&mut self) {
        while self.remote_players.len() < crate::game::network::MAX_PLAYERS - 2 {
            let mut player = self.player1.clone();
            player.cond.set_alive(false);
            self.remote_players.push(crate::game::player::player_list::RemotePlayer {
                player,
                inventory: self.inventory_player1.clone(),
                hud: HUD::new(Alignment::Left),
            });
        }
    }
    fn apply_roster(&mut self, state: &mut SharedGameState, ctx: &mut Context, migration: Option<u8>) {
        self.ensure_network_players();
        if let Some(from) = migration {
            let from = from as usize;
            if from == 1 {
                std::mem::swap(&mut self.player1, &mut self.player2);
                std::mem::swap(&mut self.inventory_player1, &mut self.inventory_player2);
                std::mem::swap(&mut self.hud_player1, &mut self.hud_player2);
            } else {
                std::mem::swap(&mut self.player1, &mut self.remote_players[from - 2].player);
                std::mem::swap(&mut self.inventory_player1, &mut self.remote_players[from - 2].inventory);
                std::mem::swap(&mut self.hud_player1, &mut self.remote_players[from - 2].hud);
            }
            self.player_generations.swap(0, from);
            self.network_cameras.swap(0, from);
            self.network_inventories.swap(0, from);
            for bullet in &mut self.bullet_manager.bullets {
                let id = bullet.owner.index();
                if id == 0 {
                    bullet.owner = TargetPlayer::from_index(from);
                } else if id == from {
                    bullet.owner = TargetPlayer::Player1;
                }
            }
            let id = state.textscript_vm.executor_player.index();
            if id == 0 {
                state.textscript_vm.executor_player = TargetPlayer::from_index(from);
            } else if id == from {
                state.textscript_vm.executor_player = TargetPlayer::Player1;
            }
        }
        let members = state.network.as_ref().unwrap().applied_members.clone();
        for index in 0..crate::game::network::MAX_PLAYERS {
            if let Some(member) = &members[index] {
                if self.player_generations[index] != member.generation {
                    if index != 0 {
                        let mut player = self.player1.clone();
                        player.cond.set_alive(true);
                        player.bubble = false;
                        player.life = player.max_life.max(1);
                        let inventory = self.inventory_player1.clone();
                        *self.player_at_mut(index) = player;
                        match index {
                            1 => self.inventory_player2 = inventory,
                            i => self.remote_players[i - 2].inventory = inventory,
                        }
                    }
                    self.player_generations[index] = member.generation;
                }
                if self.player_at(index).bubble {
                    self.player_at_mut(index).cond.set_alive(false);
                }
                let skin = member.skin;
                let player = self.player_at_mut(index);
                if player.network_skin != Some(skin) {
                    player.load_network_skin(skin, state, ctx);
                    player.network_skin = Some(skin);
                }
            } else {
                self.player_at_mut(index).cond.set_alive(false);
                self.player_at_mut(index).bubble = false;
                self.player_generations[index] = 0;
            }
        }
        if members[state.textscript_vm.executor_player.index()].is_none() {
            state.textscript_vm.executor_player = TargetPlayer::Player1;
        }
        self.bullet_manager.bullets.retain(|bullet| members[bullet.owner.index()].is_some());
    }

    fn view_frame<'a>(&'a self, state: &SharedGameState) -> std::borrow::Cow<'a, Frame> {
        let Some(session) = &state.network else { return std::borrow::Cow::Borrowed(&self.frame) };
        let frame = if session.applied_rules.individual_cameras {
            &self.network_cameras[session.local_slot]
        } else {
            &self.frame
        };
        let mut source = (320.0, 240.0);
        let mut destination = state.canvas_size;
        if state.constants.is_switch && self.stage.map.width <= 54 {
            source.0 += 10.0;
            destination.0 += 10.0;
        }
        let tile_size = state.tile_size.as_int();
        let map_pixels = (
            (self.stage.map.width as i32 - 1) * tile_size,
            (self.stage.map.height as i32 - 1) * tile_size,
        );
        std::borrow::Cow::Owned(frame.for_viewport(source, destination, map_pixels))
    }

    fn update_network_cameras(&mut self, state: &mut SharedGameState, immediate: bool) {
        if state.network.is_none() {
            return;
        }
        for slot in 0..crate::game::network::MAX_PLAYERS {
            if !state.control_flags.control_enabled() || self.frame.update_target != UpdateTarget::Player {
                self.network_cameras[slot] = self.frame.clone();
                continue;
            }
            let player = self.player_at(slot);
            let (x, y) = if immediate { (player.x, player.y) } else { (player.target_x, player.target_y) };
            let camera = &mut self.network_cameras[slot];
            camera.target_x = x;
            camera.target_y = y;
            camera.wait = self.frame.wait;
            if immediate {
                camera.immediate_update(state, &self.stage);
            } else {
                camera.update_position(state, &self.stage);
            }
        }
    }

    fn check_network_game_over(&mut self, state: &mut SharedGameState) {
        if self.network_game_over || state.sound_manager.speculative {
            return;
        }
        let Some(session) = state.network.as_ref() else { return };
        let mut members = session.applied_members.iter().enumerate().filter(|(_, member)| member.is_some()).peekable();
        if members.peek().is_none() || !members.all(|(slot, _)| self.player_at(slot).bubble) {
            return;
        }

        // Start the original retry prompt once, including when the last survivor disconnects.
        self.network_game_over = true;
        state.control_flags.set_tick_world(true);
        state.control_flags.set_interactions_disabled(true);
        state.textscript_vm.set_mode(ScriptMode::Map);
        state.textscript_vm.start_script(40);
    }

    fn tick_bubbles(&mut self, state: &mut SharedGameState) {
        let living: Vec<_> = (0..crate::game::network::MAX_PLAYERS)
            .filter_map(|slot| {
                let p = self.player_at(slot);
                (p.cond.alive() && !p.bubble && !p.cond.hidden()).then_some((slot, p.x, p.y))
            })
            .collect();
        for slot in 0..crate::game::network::MAX_PLAYERS {
            let p = self.player_at_mut(slot);
            if !p.bubble {
                continue;
            }
            if let Some(&(_, x, y)) = living.iter().min_by_key(|&&(_, x, y)| {
                let dx = x as i64 - p.x as i64;
                let dy = y as i64 - p.y as i64;
                dx * dx + dy * dy
            }) {
                p.x += ((x as i64 - p.x as i64) / 32).clamp(-0x400, 0x400) as i32;
                p.y += ((y as i64 - 20 * 0x200 - p.y as i64) / 32).clamp(-0x400, 0x400) as i32;
            }
            p.target_x = p.x;
            p.target_y = p.y;
            let (x, y) = (p.x, p.y);
            let shot = self.bullet_manager.bullets.iter_mut().find(|bullet| {
                bullet.cond.alive()
                    && bullet.life > 0
                    && bullet.damage > 0
                    && living.iter().any(|&(owner, _, _)| owner == bullet.owner.index())
                    && crate::game::player::bubble::shot_hits_bubble(
                        bullet.prev_x,
                        bullet.prev_y,
                        bullet.x,
                        bullet.y,
                        x,
                        y,
                    )
            });
            if let Some(shot) = shot {
                shot.life = 0;
                shot.cond.set_alive(false);
                self.player_at_mut(slot).revive_from_bubble();
                state.sound_manager.play_sfx_at(21, x, y);
            }
        }
    }

    fn network_checksum(&mut self, state: &mut SharedGameState) -> GameResult<u64> {
        // Only called after restoring confirmed state. Rendering and speculative frames
        // do not invalidate it; advancing the authoritative sequence does.
        let sequence = state.network.as_ref().unwrap().sequence();
        if let Some((cached_sequence, checksum)) = self.confirmed_checksum {
            if cached_sequence == sequence {
                return Ok(checksum);
            }
        }
        use std::fmt::Write;
        let mut data = std::mem::take(&mut self.checksum_buffer);
        data.clear();
        write!(
            data,
            "{}:{}:{}:{}:{}:{:?}:{:?}:{:?}:{:?}:{:?}:{:?}:{:?}",
            self.stage_id,
            self.tick,
            state.game_rng.dump_state(),
            state.effect_rng.dump_state(),
            state.control_flags.0,
            state.textscript_vm.state,
            state.textscript_vm.stack,
            state.textscript_vm.flags.0,
            state.textscript_vm.numbers,
            state.teleporter_slots,
            state.fade_state,
            (self.frame.x, self.frame.y)
        )
        .unwrap();
        write!(
            data,
            "{:?};{:?};{};",
            state.difficulty,
            state.network.as_ref().map(|s| s.applied_rules),
            self.network_game_over
        )
        .unwrap();
        for flag in state.game_flags.iter().chain(state.map_flags.iter()).chain(state.skip_flags.iter()) {
            data.push(if flag { '1' } else { '0' });
        }
        if self.checksum_tiles.0 != self.stage.map.tiles {
            self.checksum_tiles.0.clone_from(&self.stage.map.tiles);
            self.checksum_tiles.1 = crate::game::network::hash(&self.stage.map.tiles);
        }
        write!(data, ":tiles:{}:{};", self.stage.map.tiles.len(), self.checksum_tiles.1).unwrap();
        write!(
            data,
            ":{:?}:{:?}:{:?}:{:?}:{:?}",
            self.inventory_player1,
            self.inventory_player2,
            (state.water_level, state.npc_super_pos, state.npc_curly_target, state.npc_curly_counter),
            (state.quake_counter, state.super_quake_counter),
            (state.textscript_vm.mode as u8, state.textscript_vm.executor_player as u8)
        )
        .unwrap();
        data.push_str(&self.player1.network_state());
        data.push_str(&self.player2.network_state());
        for remote in &self.remote_players {
            data.push_str(&remote.player.network_state());
            write!(data, "{:?}", remote.inventory).unwrap();
        }
        for inventory in &self.network_inventories {
            match inventory {
                Some(inventory) => data.push_str(&inventory.checksum()),
                None => data.push_str("closed"),
            }
        }
        let mut checksum = crate::game::network::hash(data.as_bytes());
        let npcs = self.npc_list.iter_alive(&self.npc_token);
        for npc in npcs {
            checksum = npc.network_hash(checksum);
        }
        for npc in &self.boss.parts {
            checksum = npc.network_hash(checksum);
        }
        checksum = crate::game::network::hash_words(checksum, [self.bullet_manager.bullets.len() as u64]);
        for bullet in &self.bullet_manager.bullets {
            checksum = bullet.network_hash(checksum);
        }
        self.checksum_buffer = data;
        self.confirmed_checksum = Some((sequence, checksum));
        Ok(checksum)
    }

    fn extend_prediction(
        &mut self,
        state: &mut SharedGameState,
        ctx: &mut Context,
        prediction: &mut rollback::Prediction,
        input: crate::game::network::Input,
        target: u64,
    ) -> GameResult {
        let sequence = state.network.as_ref().unwrap().sequence();
        let held = prediction.inputs.back().map_or(crate::game::network::Input::neutral(), |(_, input)| *input);
        while prediction.inputs.len() < rollback::MAX_PREDICTION {
            let frame = sequence + prediction.inputs.len() as u64;
            if frame > target {
                break;
            }
            let predicted = if frame == target { input } else { held };
            if !self.predict_network_frame(state, ctx, predicted)? {
                break;
            }
            prediction.inputs.push_back((frame, predicted));
        }
        Ok(())
    }

    fn can_predict(&self, state: &SharedGameState) -> bool {
        !self.intro_mode
            && (self.player1.cond.alive()
                || self.player2.cond.alive()
                || self.remote_players.iter().any(|r| r.player.cond.alive()))
            && !state.control_flags.credits_running()
            && state.control_flags.control_enabled()
            && state.control_flags.tick_world()
            && state.textscript_vm.mode == ScriptMode::Map
            && state.textscript_vm.state == TextScriptExecutionState::Ended
            && state.replay_state == ReplayState::None
            && state.next_scene.is_none()
    }

    fn simulate_network_frame(&mut self, state: &mut SharedGameState, ctx: &mut Context) -> GameResult {
        state.settings.timing_mode = state.network.as_ref().unwrap().applied_rules.timing.mode();
        let local_slot = state.network.as_ref().unwrap().local_slot;
        let listener = self.player_at(local_slot);
        state.sound_manager.set_listener(Some((listener.x, listener.y)));
        let controllers = state.network.as_ref().unwrap().controllers;
        for (index, controller) in controllers.into_iter().enumerate() {
            self.player_at_mut(index).controller = Box::new(if self.network_inventories[index].is_some() {
                crate::input::replay_player_controller::ReplayController::new()
            } else { controller });
        }
        self.update_interpolation(state)?;
        let canvas = state.canvas_size;
        state.canvas_size = (320.0, 240.0);
        let host = state.network.as_ref().unwrap().host && !state.sound_manager.speculative;
        let deaths: [u32; crate::game::network::MAX_PLAYERS] =
            std::array::from_fn(|slot| self.player_at(slot).network_deaths);
        let result = self.tick_simulation(state, ctx);
        state.canvas_size = canvas;
        if result.is_ok() && host {
            for (slot, before) in deaths.into_iter().enumerate() {
                let player = self.player_at(slot);
                if player.network_deaths != before {
                    state.network.as_mut().unwrap().announce_death(slot);
                }
            }
        }
        result
    }

    fn predict_network_frame(
        &mut self,
        state: &mut SharedGameState,
        ctx: &mut Context,
        input: crate::game::network::Input,
    ) -> GameResult<bool> {
        if !self.can_predict(state) {
            return Ok(false);
        }
        let session = state.network.as_mut().unwrap();
        for (slot, controller) in session.controllers.iter_mut().enumerate() {
            let predicted =
                if slot == session.local_slot { input } else { crate::game::network::Input::held(controller) };
            predicted.apply(controller);
        }
        // Re-simulation never emits sound or device vibration. They play once, on confirmation.
        state.sound_manager.speculative = true;
        let result = self.simulate_network_frame(state, ctx);
        state.sound_manager.speculative = false;
        result?;
        Ok(true)
    }

    fn tick_simulation(&mut self, state: &mut SharedGameState, ctx: &mut Context) -> GameResult {
        if !self.pause_menu.is_paused() {
            if let ReplayState::Playback(_) = state.replay_state {
                self.replay.tick(state, (ctx, &mut self.player1))?;
            }
        }

        if state.player_count_modified_in_game {
            if state.player_count == PlayerCount::Two {
                self.add_player2(state, ctx);
            } else {
                self.drop_player2();
            }

            state.player_count_modified_in_game = false;
        }

        if state.network.is_none() {
            self.player1.controller.update(state, ctx)?;
            self.player1.controller.update_trigger();
            self.player2.controller.update(state, ctx)?;
            self.player2.controller.update_trigger();
        }

        state.touch_controls.control_type = if state.control_flags.control_enabled() && !self.pause_menu.is_paused() {
            TouchControlType::Controls
        } else {
            TouchControlType::None
        };

        if state.settings.touch_controls {
            state.touch_controls.interact_icon = false;
        }

        if self.intro_mode {
            state.touch_controls.control_type = TouchControlType::Dialog;

            if let TextScriptExecutionState::WaitTicks(_, _, 9999) = state.textscript_vm.state {
                state.next_scene = Some(Box::new(TitleScene::new()));
            }

            if self.player1.controller.trigger_menu_ok() || self.player1.controller.trigger_menu_pause() {
                state.next_scene = Some(Box::new(TitleScene::new()));
            }
        }

        if state.network.is_none() && self.player1.controller.trigger_menu_pause() {
            self.pause_menu.pause(state);
        }

        if self.pause_menu.is_paused() {
            self.pause_menu.tick(state, ctx)?;
            return Ok(());
        }

        if state.replay_state == ReplayState::Recording {
            self.replay.tick(state, (ctx, &mut self.player1))?;
        }

        match state.textscript_vm.state {
            TextScriptExecutionState::Running(_, _)
            | TextScriptExecutionState::WaitTicks(_, _, _)
            | TextScriptExecutionState::WaitInput(_, _, _)
            | TextScriptExecutionState::WaitStanding(_, _)
            | TextScriptExecutionState::WaitFade(_, _)
            | TextScriptExecutionState::Msg(_, _, _, _)
            | TextScriptExecutionState::MsgNewLine(_, _, _, _, _)
            | TextScriptExecutionState::FallingIsland(_, _, _, _, _, _)
                if !state.control_flags.control_enabled() =>
            {
                state.touch_controls.control_type = TouchControlType::Dialog;
                match state.settings.cutscene_skip_mode {
                    CutsceneSkipMode::Hold if !state.textscript_vm.flags.cutscene_skip() => {
                        if self.player1.controller.skip() {
                            self.skip_counter += 1;
                            if self.skip_counter >= CUTSCENE_SKIP_WAIT {
                                state.textscript_vm.flags.set_cutscene_skip(true);
                                state.tutorial_counter = 0;
                            }
                        } else if self.skip_counter > 0 {
                            self.skip_counter -= 1;
                        }
                    }
                    CutsceneSkipMode::FastForward => {
                        if self.player1.controller.skip() {
                            state.textscript_vm.flags.set_cutscene_skip(true);
                        } else {
                            state.textscript_vm.flags.set_cutscene_skip(false);
                        }
                    }
                    CutsceneSkipMode::Auto => {
                        state.textscript_vm.flags.set_cutscene_skip(true);
                    }
                    _ => (),
                }
            }
            _ => {
                self.skip_counter = 0;
            }
        }

        self.map_system.tick(
            state,
            ctx,
            &self.stage,
            &std::iter::once(&self.player1)
                .chain(std::iter::once(&self.player2))
                .chain(self.remote_players.iter().map(|r| &r.player))
                .collect::<Vec<_>>(),
        )?;

        match state.textscript_vm.mode {
            ScriptMode::Map | ScriptMode::Debug => {
                TextScriptVM::run(state, self, ctx)?;

                match state.textscript_vm.state {
                    TextScriptExecutionState::FallingIsland(_, _, _, _, _, _) => (),
                    TextScriptExecutionState::MapSystem => (),
                    _ => {
                        if state.control_flags.tick_world() {
                            self.tick_world(state)?;
                        }
                    }
                }
            }
            ScriptMode::StageSelect => {
                self.stage_select.tick(
                    state,
                    (
                        ctx,
                        &std::iter::once(&self.player1)
                            .chain(std::iter::once(&self.player2))
                            .chain(self.remote_players.iter().map(|r| &r.player))
                            .collect::<Vec<_>>() as &[&Player],
                    ),
                )?;

                TextScriptVM::run(state, self, ctx)?;
            }
            ScriptMode::Inventory => {
                let slot = if state.network.is_some() { state.textscript_vm.executor_player.index() } else { 0 };
                match slot {
                    0 => self
                        .inventory_ui
                        .tick(state, (ctx, &mut self.player1, &mut self.inventory_player1, &mut self.hud_player1))?,
                    1 => self
                        .inventory_ui
                        .tick(state, (ctx, &mut self.player2, &mut self.inventory_player2, &mut self.hud_player2))?,
                    _ => {
                        let remote = &mut self.remote_players[slot - 2];
                        self.inventory_ui
                            .tick(state, (ctx, &mut remote.player, &mut remote.inventory, &mut remote.hud))?;
                    }
                }

                TextScriptVM::run(state, self, ctx)?;
            }
        }

        if state.control_flags.credits_running() {
            self.skip_counter = 0;
            CreditScriptVM::run(state, ctx)?;
        }

        self.fade.tick(state, ())?;
        self.flash.tick(state, ())?;
        self.text_boxes.tick(state, ())?;
        self.tick_network_inventories(state, ctx)?;

        if state.control_flags.tick_world() {
            self.tick = self.tick.wrapping_add(1);
        }

        if state.tutorial_counter > 0 {
            state.tutorial_counter = state.tutorial_counter.saturating_sub(1);
            if state.control_flags.control_enabled() {
                state.tutorial_counter = 0;
            }
        }

        if state.quake_rumble_counter > 0 && !state.sound_manager.speculative {
            gamepad::set_quake_rumble_all(ctx, state, state.quake_rumble_counter)?;
            state.quake_rumble_counter = 0;
        }

        if state.super_quake_rumble_counter > 0 && !state.sound_manager.speculative {
            gamepad::set_super_quake_rumble_all(ctx, state, state.super_quake_rumble_counter)?;
            state.super_quake_rumble_counter = 0;
        }

        Ok(())
    }

    fn update_interpolation(&mut self, state: &mut SharedGameState) -> GameResult {
        self.frame.prev_x = self.frame.x;
        self.frame.prev_y = self.frame.y;
        for camera in &mut self.network_cameras {
            camera.prev_x = camera.x;
            camera.prev_y = camera.y;
        }
        self.player1.prev_x = self.player1.x;
        self.player1.prev_y = self.player1.y;
        self.player1.damage_popup.prev_x = self.player1.damage_popup.x;
        self.player1.damage_popup.prev_y = self.player1.damage_popup.y;
        self.player1.exp_popup.prev_x = self.player1.exp_popup.x;
        self.player1.exp_popup.prev_y = self.player1.exp_popup.y;
        self.player2.prev_x = self.player2.x;
        self.player2.prev_y = self.player2.y;
        self.player2.damage_popup.prev_x = self.player2.damage_popup.x;
        self.player2.damage_popup.prev_y = self.player2.damage_popup.y;
        self.player2.exp_popup.prev_x = self.player2.exp_popup.x;
        self.player2.exp_popup.prev_y = self.player2.exp_popup.y;

        self.npc_list.for_each_alive_mut(&mut self.npc_token, |mut npc| {
            npc.prev_x = npc.x;
            npc.prev_y = npc.y;
            npc.popup.prev_x = npc.prev_x;
            npc.popup.prev_y = npc.prev_y;
        });

        for npc in self.boss.parts.iter_mut() {
            if npc.cond.alive() {
                npc.prev_x = npc.x;
                npc.prev_y = npc.y;
                npc.popup.prev_x = npc.prev_x;
                npc.popup.prev_y = npc.prev_y;
            }
        }

        for bullet in self.bullet_manager.bullets.iter_mut() {
            if bullet.cond.alive() {
                bullet.prev_x = bullet.x;
                bullet.prev_y = bullet.y;
            }
        }

        for caret in state.carets.iter_mut() {
            if caret.cond.alive() {
                caret.prev_x = caret.x;
                caret.prev_y = caret.y;
            }
        }

        for remote in &mut self.remote_players {
            remote.player.prev_x = remote.player.x;
            remote.player.prev_y = remote.player.y;
        }
        self.whimsical_star.set_prev();

        self.tilemap.set_prev()?;

        self.inventory_dim += 0.1
            * if state.textscript_vm.mode == ScriptMode::Inventory
                || state.textscript_vm.state == TextScriptExecutionState::MapSystem
                || self.pause_menu.is_paused()
            {
                state.frame_time as f32
            } else {
                -(state.frame_time as f32)
            };

        self.inventory_dim = self.inventory_dim.clamp(0.0, 1.0);
        self.background.draw_tick()?;
        self.credits.draw_tick(state);

        Ok(())
    }

    pub fn new(state: &mut SharedGameState, ctx: &mut Context, id: usize) -> GameResult<Self> {
        info!("Loading stage {} ({})", id, &state.stages[id].map);
        let stage = Stage::load(&state.constants.base_paths, &state.stages[id], ctx)?;
        info!("Loaded stage: {}", stage.data.name);

        GameScene::from_stage(state, ctx, stage, id)
    }

    pub fn from_stage(state: &mut SharedGameState, ctx: &mut Context, stage: Stage, id: usize) -> GameResult<Self> {
        let mut water_params = WaterParams::new();
        let mut water_renderer = WaterRenderer::new();
        let mut tilemap = Tilemap::new();

        if !state.settings.original_textures {
            if let Ok(water_param_file) = filesystem::open_find(
                ctx,
                &state.constants.base_paths,
                ["Stage/", &state.stages[id].tileset.name, ".pxw"].join(""),
            ) {
                water_params.load_from(water_param_file)?;
                info!("Loaded water parameters file.");

                let regions = stage.map.find_water_regions(&water_params);
                water_renderer.initialize(regions, &water_params, &stage);
                tilemap.no_water = true;
            }
        }

        let stage_textures = {
            let mut textures = StageTexturePaths::new();
            textures.update(&stage);
            Rc::new(RefCell::new(textures))
        };

        let mut player2 = Player::new(state, ctx);

        if state.player2_skin_location.texture_index != 0 {
            let skinsheet_name =
                state.constants.player_skin_paths[state.player2_skin_location.texture_index as usize].as_str();
            player2.load_skin(skinsheet_name.to_owned(), state, ctx);
        }

        let (npc_list, npc_token) = NPCList::new();

        Ok(Self {
            tick: 0,
            stage,
            water_params,
            water_renderer,
            player1: Player::new(state, ctx),
            player2: player2,
            inventory_player1: Inventory::new(),
            inventory_player2: Inventory::new(),
            remote_players: Vec::new(),
            player_generations: [0; crate::game::network::MAX_PLAYERS],
            boss_life_bar: BossLifeBar::new(),
            stage_select: StageSelect::new(),
            flash: Flash::new(),
            credits: Credits::new(),
            falling_island: FallingIsland::new(),
            inventory_ui: InventoryUI::new(),
            network_inventories: std::array::from_fn(|_| None),
            map_system: MapSystem::new(),
            network_map_system: MapSystem::new(),
            hud_player1: HUD::new(Alignment::Left),
            hud_player2: HUD::new(Alignment::Right),
            nikumaru: NikumaruCounter::new(),
            whimsical_star: WhimsicalStar::new(),
            background: Background::new(),
            tilemap,
            text_boxes: TextBoxes::new(),
            fade: Fade::new(),
            frame: Frame::new(),
            network_cameras: std::array::from_fn(|_| Frame::new()),
            stage_id: id,
            npc_list,
            npc_token,
            boss: BossNPC::new(),
            bullet_manager: BulletManager::new(),
            lighting_mode: LightingMode::None,
            intro_mode: false,
            pause_menu: PauseMenu::new(),
            stage_textures,
            map_name_counter: 0,
            skip_counter: 0,
            inventory_dim: 0.0,
            network_game_over: false,
            prediction: rollback::Prediction::default(),
            network_menu: Default::default(),
            network_chat: Default::default(),
            checksum_tiles: (Vec::new(), crate::game::network::hash(&[])),
            checksum_buffer: String::new(),
            confirmed_checksum: None,
            replay: Replay::new(),
        })
    }

    pub fn display_map_name(&mut self, ticks: u16) {
        self.map_name_counter = ticks;
    }

    pub fn add_player2(&mut self, state: &mut SharedGameState, ctx: &mut Context) {
        self.player2.cond.set_alive(true);
        self.player2.cond.set_hidden(self.player1.cond.hidden());

        let skinsheet_name =
            state.constants.player_skin_paths[state.player2_skin_location.texture_index as usize].as_str();
        self.player2.load_skin(skinsheet_name.to_owned(), state, ctx);
        self.player2.skin.set_skinsheet_offset(state.player2_skin_location.offset);

        self.player2.x = self.player1.x;
        self.player2.y = self.player1.y;
        self.player2.vel_x = self.player1.vel_x;
        self.player2.vel_y = self.player1.vel_y;
    }

    pub fn drop_player2(&mut self) {
        self.player2.cond.set_alive(false);
    }

    fn draw_npc_layer(&self, state: &mut SharedGameState, ctx: &mut Context, layer: NPCLayer) -> GameResult {
        let view = self.view_frame(state);
        let frame = view.as_ref();
        for npc in self.npc_list.iter_alive(&self.npc_token) {
            if npc.layer != layer
                || npc.x < (frame.x - 128 * 0x200 - npc.display_bounds.width() as i32 * 0x200)
                || npc.x
                    > (frame.x + 128 * 0x200 + (state.canvas_size.0 as i32 + npc.display_bounds.width() as i32) * 0x200)
                    && npc.y < (frame.y - 128 * 0x200 - npc.display_bounds.height() as i32 * 0x200)
                || npc.y
                    > (frame.y
                        + 128 * 0x200
                        + (state.canvas_size.1 as i32 + npc.display_bounds.height() as i32) * 0x200)
            {
                continue;
            }

            npc.npc_draw(state, ctx, frame)?;
        }

        Ok(())
    }

    fn draw_npc_popup(&self, state: &mut SharedGameState, ctx: &mut Context) -> GameResult {
        let view = self.view_frame(state);
        let frame = view.as_ref();
        for npc in self.npc_list.iter_alive(&self.npc_token) {
            npc.popup.draw(state, ctx, frame)?;
        }
        Ok(())
    }

    fn draw_boss_popup(&self, state: &mut SharedGameState, ctx: &mut Context) -> GameResult {
        let view = self.view_frame(state);
        let frame = view.as_ref();
        for part in self.boss.parts.iter() {
            part.popup.draw(state, ctx, frame)?;
        }
        Ok(())
    }

    fn tick_network_map(&mut self, state: &mut SharedGameState, ctx: &mut Context) {
        let session = state.network.as_ref().unwrap();
        let player = self.player_at(session.local_slot);
        if self.network_game_over
            || !player.cond.alive()
            || player.bubble
            || state.textscript_vm.mode != ScriptMode::Map
            || state.textscript_vm.state != TextScriptExecutionState::Ended
            || !state.control_flags.control_enabled()
            || self.network_inventories[session.local_slot].is_some()
        {
            self.network_map_system.hide();
            return;
        }
        let controller = &session.local_controller;
        let menu_open = session.chat_open || session.options_open;
        let open = !menu_open && controller.trigger_map() && player.equip.has_map();
        let dismiss = !menu_open
            && (controller.trigger_map()
                || controller.trigger_jump()
                || controller.trigger_shoot()
                || controller.trigger_menu_back());
        self.network_map_system.tick_local(state, ctx, &self.stage, open, dismiss);
    }

    fn visible_bullets<'a>(
        &'a self,
        network: Option<&crate::game::network::Session>,
    ) -> impl Iterator<Item = &'a crate::game::weapon::bullet::Bullet> {
        let guest = network.filter(|session| !session.host);
        let confirmed = guest.and_then(|_| self.prediction.confirmed.as_ref()).map(|snapshot| snapshot.bullets());
        rollback::visible_bullets(&self.bullet_manager, confirmed, guest.map_or(0, |session| session.local_slot))
    }

    fn draw_bullets(&self, state: &mut SharedGameState, ctx: &mut Context) -> GameResult {
        let view = self.view_frame(state);
        let frame = view.as_ref();
        let batch = state.texture_set.get_or_load_batch(ctx, &state.constants, "Bullet")?;
        let mut x: i32;
        let mut y: i32;
        let mut prev_x: i32;
        let mut prev_y: i32;

        for bullet in self.visible_bullets(state.network.as_ref()) {
            match bullet.direction {
                Direction::Left => {
                    x = bullet.x - bullet.display_bounds.left as i32;
                    y = bullet.y - bullet.display_bounds.top as i32;
                    prev_x = bullet.prev_x - bullet.display_bounds.left as i32;
                    prev_y = bullet.prev_y - bullet.display_bounds.top as i32;
                }
                Direction::Up => {
                    x = bullet.x - bullet.display_bounds.top as i32;
                    y = bullet.y - bullet.display_bounds.left as i32;
                    prev_x = bullet.prev_x - bullet.display_bounds.top as i32;
                    prev_y = bullet.prev_y - bullet.display_bounds.left as i32;
                }
                Direction::Right => {
                    x = bullet.x - bullet.display_bounds.right as i32;
                    y = bullet.y - bullet.display_bounds.top as i32;
                    prev_x = bullet.prev_x - bullet.display_bounds.right as i32;
                    prev_y = bullet.prev_y - bullet.display_bounds.top as i32;
                }
                Direction::Bottom => {
                    x = bullet.x - bullet.display_bounds.top as i32;
                    y = bullet.y - bullet.display_bounds.right as i32;
                    prev_x = bullet.prev_x - bullet.display_bounds.top as i32;
                    prev_y = bullet.prev_y - bullet.display_bounds.right as i32;
                }
                Direction::FacingPlayer => unreachable!(),
            }

            batch.add_rect(
                interpolate_fix9_scale(prev_x - frame.prev_x, x - frame.x, state.frame_time),
                interpolate_fix9_scale(prev_y - frame.prev_y, y - frame.y, state.frame_time),
                &bullet.anim_rect,
            );
        }

        batch.draw(ctx)?;
        Ok(())
    }

    fn draw_carets(&self, state: &mut SharedGameState, ctx: &mut Context) -> GameResult {
        let view = self.view_frame(state);
        let frame = view.as_ref();
        let batch = state.texture_set.get_or_load_batch(ctx, &state.constants, "Caret")?;

        for caret in state.carets.iter() {
            batch.add_rect(
                interpolate_fix9_scale(
                    caret.prev_x - caret.offset_x - frame.prev_x,
                    caret.x - caret.offset_x - frame.x,
                    state.frame_time,
                ),
                interpolate_fix9_scale(
                    caret.prev_y - caret.offset_y - frame.prev_y,
                    caret.y - caret.offset_y - frame.y,
                    state.frame_time,
                ),
                &caret.anim_rect,
            );
        }

        batch.draw(ctx)?;
        Ok(())
    }

    fn draw_black_bars(&self, state: &mut SharedGameState, ctx: &mut Context) -> GameResult {
        let view = self.view_frame(state);
        let frame = view.as_ref();
        let (x, y) = frame.xy_interpolated(state.frame_time);
        let (x, y) = (x * state.scale, y * state.scale);
        let canvas_w_scaled = state.canvas_size.0 as f32 * state.scale;
        let canvas_h_scaled = state.canvas_size.1 as f32 * state.scale;
        let half_block = self.stage.map.tile_size.as_float() * 0.5 * state.scale;
        let level_width = (self.stage.map.width as f32) * self.stage.map.tile_size.as_float();
        let level_height = (self.stage.map.height as f32) * self.stage.map.tile_size.as_float();
        let left_side = -x - half_block;
        let right_side = left_side + level_width * state.scale;
        let upper_side = -y - half_block;
        let lower_side = upper_side + level_height * state.scale;

        if left_side > 0.0 {
            let rect = Rect::new(0, 0, left_side as isize, canvas_h_scaled as isize);
            graphics::draw_rect(ctx, rect, Color::from_rgb(0, 0, 0))?;
        }

        if right_side < canvas_w_scaled {
            let rect = Rect::new(
                right_side as isize,
                0,
                (state.canvas_size.0 * state.scale) as isize,
                (state.canvas_size.1 * state.scale) as isize,
            );
            graphics::draw_rect(ctx, rect, Color::from_rgb(0, 0, 0))?;
        }

        if upper_side > 0.0 {
            let rect = Rect::new(0, 0, canvas_w_scaled as isize, upper_side as isize);
            graphics::draw_rect(ctx, rect, Color::from_rgb(0, 0, 0))?;
        }

        if lower_side < canvas_h_scaled {
            let rect = Rect::new(0, lower_side as isize, canvas_w_scaled as isize, canvas_h_scaled as isize);
            graphics::draw_rect(ctx, rect, Color::from_rgb(0, 0, 0))?;
        }

        Ok(())
    }

    fn set_ironhead_clip(&self, state: &mut SharedGameState, ctx: &mut Context) -> GameResult {
        let x_size = if !state.constants.is_switch { 320.0 } else { 426.0 };
        let clip_rect: Rect = Rect::new_size(
            (((state.canvas_size.0 - x_size) * 0.5) * state.scale) as _,
            (((state.canvas_size.1 - 240.0) * 0.5) * state.scale) as _,
            (x_size * state.scale) as _,
            (240.0 * state.scale) as _,
        );
        graphics::set_clip_rect(ctx, Some(clip_rect))?;
        Ok(())
    }

    fn draw_light(&self, x: f32, y: f32, size: f32, color: (u8, u8, u8), batch: &mut Box<dyn SpriteBatch>) {
        batch.add_rect_scaled_tinted(
            x - size * 32.0,
            y - size * 32.0,
            (color.0, color.1, color.2, 255),
            size,
            size,
            &Rect::new(0, 0, 64, 64),
        )
    }

    fn draw_light_raycast(
        &self,
        tile_size: TileSize,
        frame: &Frame,
        world_point_x: i32,
        world_point_y: i32,
        (br, bg, bb): (u8, u8, u8),
        att: f32,
        angle: Range<i32>,
        batch: &mut Box<dyn SpriteBatch>,
    ) {
        let px = world_point_x as f32 / 512.0;
        let py = world_point_y as f32 / 512.0;

        let fx2 = frame.x as f32 / 512.0;
        let fy2 = frame.y as f32 / 512.0;

        let ti = tile_size.as_int();
        let tf = tile_size.as_float();
        let tih = ti / 2;
        let tfq = tf / 4.0;
        let (br, bg, bb) = (br as f32, bg as f32, bb as f32);
        let ahalf = (angle.end - angle.start) as f32 / 2.0;

        'ray: for (i, deg) in angle.enumerate() {
            let d = deg as f32 * (std::f32::consts::PI / 180.0);
            let dx = d.cos() * -5.0;
            let dy = d.sin() * -5.0;
            let m = 1.0 - ((ahalf - i as f32).abs() / ahalf);
            let mut x = px;
            let mut y = py;
            let mut r = br;
            let mut g = bg;
            let mut b = bb;

            for i in 0..40 {
                x += dx;
                y += dy;

                const ARR: [(i32, i32); 4] = [(0, 0), (0, 1), (1, 0), (1, 1)];
                for (ox, oy) in ARR.iter() {
                    let bx = (x as i32).wrapping_div(ti).wrapping_add(*ox);
                    let by = (y as i32).wrapping_div(ti).wrapping_add(*oy);

                    let tile = self.stage.map.attrib[self.stage.tile_at(bx as usize, by as usize) as usize];
                    let bxmth = (bx * ti - tih) as f32;
                    let bxpth = (bx * ti + tih) as f32;
                    let bymth = (by * ti - tih) as f32;
                    let bypth = (by * ti + tih) as f32;

                    if ((tile == 0x62 || tile == 0x41 || tile == 0x43 || tile == 0x46)
                        && x >= bxmth
                        && x <= bxpth
                        && y >= bymth
                        && y <= bypth)
                        || ((tile == 0x50 || tile == 0x70)
                            && x >= bxmth
                            && x <= bxpth
                            && y <= ((by as f32 * tf) - (x - bx as f32 * tf) / 2.0 + tfq)
                            && y >= bymth)
                        || ((tile == 0x51 || tile == 0x71)
                            && x >= bxmth
                            && x <= bxpth
                            && y <= ((by as f32 * tf) - (x - bx as f32 * tf) / 2.0 - tfq)
                            && y >= bymth)
                        || ((tile == 0x52 || tile == 0x72)
                            && x >= bxmth
                            && x <= bxpth
                            && y <= ((by as f32 * tf) + (x - bx as f32 * tf) / 2.0 - tfq)
                            && y >= bymth)
                        || ((tile == 0x53 || tile == 0x73)
                            && x >= bxmth
                            && x <= bxpth
                            && y <= ((by as f32 * tf) + (x - bx as f32 * tf) / 2.0 + tfq)
                            && y >= bymth)
                        || ((tile == 0x54 || tile == 0x74)
                            && x >= bxmth
                            && x <= bxpth
                            && y >= ((by as f32 * tf) + (x - bx as f32 * tf) / 2.0 - tfq)
                            && y <= bypth)
                        || ((tile == 0x55 || tile == 0x75)
                            && x >= bxmth
                            && x <= bxpth
                            && y >= ((by as f32 * tf) + (x - bx as f32 * tf) / 2.0 + tfq)
                            && y <= bypth)
                        || ((tile == 0x56 || tile == 0x76)
                            && x >= bxmth
                            && x <= bxpth
                            && y >= ((by as f32 * tf) - (x - bx as f32 * tf) / 2.0 + tfq)
                            && y <= bypth)
                        || ((tile == 0x57 || tile == 0x77)
                            && x >= bxmth
                            && x <= bxpth
                            && y >= ((by as f32 * tf) - (x - bx as f32 * tf) / 2.0 - tfq)
                            && y <= bypth)
                    {
                        continue 'ray;
                    }
                }

                r *= att;
                g *= att;
                b *= att;

                if r <= 1.0 && g <= 1.0 && b <= 1.0 {
                    continue 'ray;
                }

                self.draw_light(
                    x - fx2,
                    y - fy2,
                    0.15 + i as f32 / 75.0,
                    ((r * m) as u8, (g * m) as u8, (b * m) as u8),
                    batch,
                );
            }
        }
    }

    fn draw_light_map(&self, state: &mut SharedGameState, ctx: &mut Context) -> GameResult {
        let view = self.view_frame(state);
        let frame = view.as_ref();
        {
            let maybe_canvas = state.lightmap_canvas.as_ref();

            if maybe_canvas.is_some() {
                graphics::set_render_target(ctx, maybe_canvas)?;
            } else {
                return Ok(());
            }
        }

        graphics::set_blend_mode(ctx, BlendMode::Add)?;

        graphics::clear(ctx, Color::from_rgb(100, 100, 110));

        for npc in self.npc_list.iter_alive(&self.npc_token) {
            if npc.x < (frame.x - 128 * 0x200 - npc.display_bounds.width() as i32 * 0x200)
                || npc.x
                    > (frame.x + 128 * 0x200 + (state.canvas_size.0 as i32 + npc.display_bounds.width() as i32) * 0x200)
                    && npc.y < (frame.y - 128 * 0x200 - npc.display_bounds.height() as i32 * 0x200)
                || npc.y
                    > (frame.y
                        + 128 * 0x200
                        + (state.canvas_size.1 as i32 + npc.display_bounds.height() as i32) * 0x200)
            {
                continue;
            }

            npc.draw_lightmap(state, ctx, frame)?;
        }

        {
            let batch = state.texture_set.get_or_load_batch(ctx, &state.constants, "builtin/lightmap/spot")?;

            'cc: for (player, inv) in
                [(&self.player1, &self.inventory_player1), (&self.player2, &self.inventory_player2)].iter()
            {
                if player.cond.alive() && !player.cond.hidden() && inv.get_current_weapon().is_some() {
                    if state.settings.light_cone {
                        let range = match () {
                            _ if player.up => 60..120,
                            _ if player.down => 240..300,
                            _ if player.direction == Direction::Left => -30..30,
                            _ if player.direction == Direction::Right => 150..210,
                            _ => continue 'cc,
                        };

                        let (color, att) = match inv.get_current_weapon() {
                            Some(Weapon { wtype: WeaponType::Fireball, .. }) => ((170u8, 80u8, 0u8), 0.92),
                            Some(Weapon { wtype: WeaponType::PolarStar, .. }) => ((150u8, 150u8, 160u8), 0.92),
                            Some(Weapon { wtype: WeaponType::Spur, .. }) => ((170u8, 170u8, 200u8), 0.92),
                            Some(Weapon { wtype: WeaponType::Blade, .. }) => continue 'cc,
                            _ => ((150u8, 150u8, 150u8), 0.92),
                        };

                        let (_, gun_off_y) = player.skin.get_gun_offset();

                        self.draw_light_raycast(
                            state.tile_size,
                            frame,
                            player.x + player.direction.vector_x() * 0x800,
                            player.y + gun_off_y * 0x200 + 0x400,
                            color,
                            att,
                            range,
                            batch,
                        );
                    } else {
                        self.draw_light(
                            interpolate_fix9_scale(player.prev_x - frame.prev_x, player.x - frame.x, state.frame_time),
                            interpolate_fix9_scale(player.prev_y - frame.prev_y, player.y - frame.y, state.frame_time),
                            5.0,
                            (150, 150, 150),
                            batch,
                        );
                    }
                }
            }

            for bullet in self.visible_bullets(state.network.as_ref()) {
                self.draw_light(
                    interpolate_fix9_scale(bullet.prev_x - frame.prev_x, bullet.x - frame.x, state.frame_time),
                    interpolate_fix9_scale(bullet.prev_y - frame.prev_y, bullet.y - frame.y, state.frame_time),
                    0.3,
                    (200, 200, 200),
                    batch,
                );
            }

            for caret in state.carets.iter() {
                match caret.ctype {
                    CaretType::ProjectileDissipation | CaretType::Shoot => {
                        self.draw_light(
                            interpolate_fix9_scale(caret.prev_x - frame.prev_x, caret.x - frame.x, state.frame_time),
                            interpolate_fix9_scale(caret.prev_y - frame.prev_y, caret.y - frame.y, state.frame_time),
                            0.5,
                            (150, 150, 150),
                            batch,
                        );
                    }
                    _ => {}
                }
            }

            for npc in self.npc_list.iter_alive(&self.npc_token) {
                if npc.cond.hidden()
                    || (npc.x < (frame.x - 128 * 0x200 - npc.display_bounds.width() as i32 * 0x200)
                        || npc.x
                            > (frame.x
                                + 128 * 0x200
                                + (state.canvas_size.0 as i32 + npc.display_bounds.width() as i32) * 0x200)
                            && npc.y < (frame.y - 128 * 0x200 - npc.display_bounds.height() as i32 * 0x200)
                        || npc.y
                            > (frame.y
                                + 128 * 0x200
                                + (state.canvas_size.1 as i32 + npc.display_bounds.height() as i32) * 0x200))
                {
                    continue;
                }

                // NPC lighting
                match npc.npc_type {
                    1 => {
                        self.draw_light(
                            interpolate_fix9_scale(npc.prev_x - frame.prev_x, npc.x - frame.x, state.frame_time),
                            interpolate_fix9_scale(npc.prev_y - frame.prev_y, npc.y - frame.y, state.frame_time),
                            0.33,
                            (255, 255, 50),
                            batch,
                        );
                    }
                    4 if npc.direction == Direction::Up => self.draw_light(
                        interpolate_fix9_scale(npc.prev_x - frame.prev_x, npc.x - frame.x, state.frame_time),
                        interpolate_fix9_scale(npc.prev_y - frame.prev_y, npc.y - frame.y, state.frame_time),
                        1.0,
                        (200, 100, 0),
                        batch,
                    ),
                    7 => self.draw_light(
                        interpolate_fix9_scale(npc.prev_x - frame.prev_x, npc.x - frame.x, state.frame_time),
                        interpolate_fix9_scale(npc.prev_y - frame.prev_y, npc.y - frame.y, state.frame_time),
                        1.0,
                        (100, 100, 100),
                        batch,
                    ),
                    17 if npc.anim_num == 0 => {
                        self.draw_light(
                            interpolate_fix9_scale(npc.prev_x - frame.prev_x, npc.x - frame.x, state.frame_time),
                            interpolate_fix9_scale(npc.prev_y - frame.prev_y, npc.y - frame.y, state.frame_time),
                            1.25,
                            (100, 0, 0),
                            batch,
                        );
                        self.draw_light(
                            interpolate_fix9_scale(npc.prev_x - frame.prev_x, npc.x - frame.x, state.frame_time),
                            interpolate_fix9_scale(npc.prev_y - frame.prev_y, npc.y - frame.y, state.frame_time),
                            0.5,
                            (255, 10, 10),
                            batch,
                        );
                    }
                    20 if npc.direction == Direction::Right => {
                        self.draw_light(
                            interpolate_fix9_scale(npc.prev_x - frame.prev_x, npc.x - frame.x, state.frame_time),
                            interpolate_fix9_scale(npc.prev_y - frame.prev_y, npc.y - frame.y, state.frame_time),
                            1.5,
                            (30, 30, 130),
                            batch,
                        );

                        if npc.anim_num < 2 {
                            self.draw_light(
                                interpolate_fix9_scale(npc.prev_x - frame.prev_x, npc.x - frame.x, state.frame_time),
                                interpolate_fix9_scale(npc.prev_y - frame.prev_y, npc.y - frame.y, state.frame_time),
                                1.0,
                                (0, 0, 20),
                                batch,
                            );
                        }
                    }
                    22 if npc.action_num == 1 && npc.anim_num == 1 => self.draw_light(
                        interpolate_fix9_scale(npc.prev_x - frame.prev_x, npc.x - frame.x, state.frame_time),
                        interpolate_fix9_scale(npc.prev_y - frame.prev_y, npc.y - frame.y, state.frame_time),
                        3.0,
                        (0, 0, 255),
                        batch,
                    ),
                    32 | 87 => {
                        self.draw_light(
                            interpolate_fix9_scale(npc.prev_x - frame.prev_x, npc.x - frame.x, state.frame_time),
                            interpolate_fix9_scale(npc.prev_y - frame.prev_y, npc.y - frame.y, state.frame_time),
                            0.75,
                            (255, 30, 30),
                            batch,
                        );
                    }
                    211 => {
                        self.draw_light(
                            interpolate_fix9_scale(npc.prev_x - frame.prev_x, npc.x - frame.x, state.frame_time),
                            interpolate_fix9_scale(npc.prev_y - frame.prev_y, npc.y - frame.y, state.frame_time),
                            1.0,
                            (90, 0, 0),
                            batch,
                        );
                    }
                    27 => {
                        self.draw_light(
                            interpolate_fix9_scale(npc.prev_x - frame.prev_x, npc.x - frame.x, state.frame_time) + 0.5,
                            interpolate_fix9_scale(npc.prev_y - frame.prev_y, npc.y - frame.y, state.frame_time),
                            3.0,
                            (96, 0, 0),
                            batch,
                        );
                    }
                    38 => {
                        let flicker = ((npc.anim_num.wrapping_add(npc.id) ^ 5) & 3) as u8 * 24;
                        self.draw_light(
                            interpolate_fix9_scale(npc.prev_x - frame.prev_x, npc.x - frame.x, state.frame_time),
                            interpolate_fix9_scale(npc.prev_y - frame.prev_y, npc.y - frame.y, state.frame_time),
                            3.5,
                            (150 + flicker, 60 + flicker, 0),
                            batch,
                        );
                    }
                    69 | 81 => {
                        self.draw_light(
                            interpolate_fix9_scale(npc.prev_x - frame.prev_x, npc.x - frame.x, state.frame_time),
                            interpolate_fix9_scale(npc.prev_y - frame.prev_y, npc.y - frame.y, state.frame_time),
                            if npc.npc_type == 69 { 0.5 } else { 1.0 },
                            (200, 200, 200),
                            batch,
                        );
                    }
                    70 => {
                        let flicker = 50 + npc.anim_num as u8 * 15;
                        self.draw_light(
                            interpolate_fix9_scale(npc.prev_x - frame.prev_x, npc.x - frame.x, state.frame_time),
                            interpolate_fix9_scale(npc.prev_y - frame.prev_y, npc.y - frame.y, state.frame_time),
                            2.0,
                            (flicker, flicker, flicker),
                            batch,
                        );
                    }
                    85 if npc.action_num == 1 => {
                        let (color, color2) = if npc.direction == Direction::Left {
                            if state.constants.is_cs_plus {
                                ((20, 100, 20), (20, 50, 20))
                            } else {
                                ((20, 20, 100), (20, 20, 50))
                            }
                        } else {
                            ((150, 0, 0), (50, 0, 0))
                        };

                        self.draw_light(
                            interpolate_fix9_scale(npc.prev_x - frame.prev_x, npc.x - frame.x, state.frame_time),
                            interpolate_fix9_scale(npc.prev_y - frame.prev_y, npc.y - frame.y, state.frame_time),
                            0.75,
                            color,
                            batch,
                        );

                        if npc.anim_num < 2 && npc.direction == Direction::Right {
                            self.draw_light(
                                interpolate_fix9_scale(npc.prev_x - frame.prev_x, npc.x - frame.x, state.frame_time),
                                interpolate_fix9_scale(npc.prev_y - frame.prev_y, npc.y - frame.y, state.frame_time)
                                    - 8.0,
                                2.1,
                                color2,
                                batch,
                            );
                        }
                    }
                    101 | 102 => self.draw_light(
                        interpolate_fix9_scale(npc.prev_x - frame.prev_x, npc.x - frame.x, state.frame_time),
                        interpolate_fix9_scale(npc.prev_y - frame.prev_y, npc.y - frame.y, state.frame_time),
                        1.0,
                        (100, 100, 200),
                        batch,
                    ),
                    175 if npc.action_num < 10 => {
                        self.draw_light(
                            interpolate_fix9_scale(npc.prev_x - frame.prev_x, npc.x - frame.x, state.frame_time),
                            interpolate_fix9_scale(npc.prev_y - frame.prev_y, npc.y - frame.y, state.frame_time),
                            1.0,
                            (128, 175, 200),
                            batch,
                        );
                    }
                    189 => self.draw_light(
                        interpolate_fix9_scale(npc.prev_x - frame.prev_x, npc.x - frame.x, state.frame_time),
                        interpolate_fix9_scale(npc.prev_y - frame.prev_y, npc.y - frame.y, state.frame_time),
                        1.0,
                        (10, 50, 255),
                        batch,
                    ),
                    270 => self.draw_light(
                        interpolate_fix9_scale(npc.prev_x - frame.prev_x, npc.x - frame.x, state.frame_time),
                        interpolate_fix9_scale(npc.prev_y - frame.prev_y, npc.y - frame.y, state.frame_time),
                        0.4,
                        (192, 0, 0),
                        batch,
                    ),
                    285 | 287 => self.draw_light(
                        interpolate_fix9_scale(npc.prev_x - frame.prev_x, npc.x - frame.x, state.frame_time),
                        interpolate_fix9_scale(npc.prev_y - frame.prev_y, npc.y - frame.y, state.frame_time),
                        1.0,
                        (150, 90, 0),
                        batch,
                    ),
                    293 => self.draw_light(
                        interpolate_fix9_scale(npc.prev_x - frame.prev_x, npc.x - frame.x, state.frame_time),
                        interpolate_fix9_scale(npc.prev_y - frame.prev_y, npc.y - frame.y, state.frame_time),
                        4.0,
                        (255, 255, 255),
                        batch,
                    ),
                    311 => {
                        let size = if npc.anim_num % 7 == 2 || npc.anim_num % 7 == 5 { 1.0 } else { 0.0 };

                        self.draw_light(
                            interpolate_fix9_scale(npc.prev_x - frame.prev_x, npc.x - frame.x, state.frame_time),
                            interpolate_fix9_scale(npc.prev_y - frame.prev_y, npc.y - frame.y, state.frame_time),
                            size,
                            (255, 255, 255),
                            batch,
                        )
                    }
                    312 => self.draw_light(
                        interpolate_fix9_scale(npc.prev_x - frame.prev_x, npc.x - frame.x, state.frame_time),
                        interpolate_fix9_scale(npc.prev_y - frame.prev_y, npc.y - frame.y, state.frame_time),
                        0.5,
                        (255, 255, 255),
                        batch,
                    ),
                    319 => {
                        let color = if npc.anim_num == 2 { (255, 29, 0) } else { (234, 157, 68) };

                        self.draw_light(
                            interpolate_fix9_scale(npc.prev_x - frame.prev_x, npc.x - frame.x, state.frame_time),
                            interpolate_fix9_scale(npc.prev_y - frame.prev_y, npc.y - frame.y, state.frame_time),
                            1.0,
                            color,
                            batch,
                        )
                    }
                    180 => {
                        if state.settings.light_cone {
                            // Curly's looking upward frames
                            let range = if [5, 6, 7, 8, 9].contains(&(npc.anim_num % 11)) {
                                60..120
                            } else if npc.action_num == 40 || npc.action_num == 41 {
                                0..0
                            } else if npc.direction() == Direction::Left {
                                -30..30
                            } else if npc.direction() == Direction::Right {
                                150..210
                            } else {
                                0..0
                            };

                            self.draw_light_raycast(
                                state.tile_size,
                                frame,
                                npc.x + npc.direction.opposite().vector_x() * 0x800,
                                npc.y + 2 * 0x200,
                                (19u8, 34u8, 117u8),
                                0.95,
                                range,
                                batch,
                            );
                        }
                    }
                    320 => {
                        if state.settings.light_cone {
                            let range = match npc.direction() {
                                Direction::Up => 60..120,
                                Direction::Bottom => 240..300,
                                Direction::Left => -30..30,
                                Direction::Right => 150..210,
                                _ => 0..0,
                            };

                            self.draw_light_raycast(
                                state.tile_size,
                                frame,
                                npc.x + npc.direction.opposite().vector_x() * 0x800,
                                npc.y + 2 * 0x200,
                                (19u8, 34u8, 117u8),
                                0.95,
                                range,
                                batch,
                            );
                        }
                    }
                    322 => {
                        let scale = 0.004 * (npc.action_counter as f32);

                        self.draw_light_raycast(state.tile_size, frame, npc.x, npc.y, (255, 0, 0), scale, 0..360, batch)
                    }
                    325 => {
                        let size = 0.5 * (npc.anim_num as f32 + 1.0);
                        self.draw_light(
                            interpolate_fix9_scale(npc.prev_x - frame.prev_x, npc.x - frame.x, state.frame_time),
                            interpolate_fix9_scale(npc.prev_y - frame.prev_y, npc.y - frame.y, state.frame_time),
                            size,
                            (255, 255, 255),
                            batch,
                        )
                    }
                    _ => {}
                }
            }

            batch.draw_filtered(FilterMode::Linear, ctx)?;
        }

        graphics::set_blend_mode(ctx, BlendMode::Multiply)?;
        graphics::set_render_target(ctx, None)?;

        {
            let canvas = state.lightmap_canvas.as_mut().unwrap();
            let rect = Rect { left: 0.0, top: 0.0, right: state.screen_size.0, bottom: state.screen_size.1 };

            canvas.clear();
            canvas.add(SpriteBatchCommand::DrawRect(rect, rect));
            canvas.draw()?;

            graphics::set_render_target(ctx, Some(canvas))?;
            graphics::draw_rect(
                ctx,
                Rect {
                    left: 0,
                    top: 0,
                    right: (state.screen_size.0 + 1.0) as isize,
                    bottom: (state.screen_size.1 + 1.0) as isize,
                },
                Color { r: 0.15, g: 0.12, b: 0.12, a: 1.0 },
            )?;
            graphics::set_render_target(ctx, None)?;
            graphics::set_blend_mode(ctx, BlendMode::Add)?;
            canvas.draw()?;

            graphics::set_blend_mode(ctx, BlendMode::Alpha)?;
        }

        Ok(())
    }

    fn tick_npc_splash(&mut self, state: &mut SharedGameState) {
        self.npc_list.for_each_alive_mut(&mut self.npc_token, |mut npc| {
            // Water Droplet
            if npc.npc_type == 73 {
                return;
            }

            if !npc.splash && npc.flags.in_water() {
                let vertical_splash = !npc.flags.hit_bottom_wall() && npc.vel_y > 0x100;
                let horizontal_splash = npc.vel_x > 0x200 || npc.vel_x < -0x200;

                if vertical_splash || horizontal_splash {
                    let mut droplet = NPC::create(73, &state.npc_table);
                    droplet.cond.set_alive(true);
                    droplet.y = npc.y;
                    droplet.direction = if npc.flags.bloody_droplets() { Direction::Right } else { Direction::Left };

                    for _ in 0..7 {
                        droplet.x = npc.x + (npc.rng.range(-8..8) * 0x200) as i32;

                        droplet.vel_x = npc.vel_x + npc.rng.range(-0x200..0x200);
                        droplet.vel_y = match () {
                            _ if vertical_splash => npc.rng.range(-0x200..0x80) - (npc.vel_y / 2),
                            _ if horizontal_splash => npc.rng.range(-0x200..0x80),
                            _ => 0,
                        };

                        let _ = self.npc_list.spawn(0x100, droplet.clone());
                    }

                    state.sound_manager.play_sfx_at(56, npc.x, npc.y);
                }

                npc.splash = true;
            }

            if !npc.flags.in_water() {
                npc.splash = false;
            }
        });
    }

    fn tick_npc_bullet_collissions(&mut self, state: &mut SharedGameState) {
        self.npc_list.for_each_alive_mut(&mut self.npc_token, |mut npc| {
            if npc.npc_flags.shootable() && npc.npc_flags.interactable() {
                return;
            }

            for bullet in self.bullet_manager.bullets.iter_mut() {
                if !bullet.cond.alive() || bullet.damage < 0 {
                    continue;
                }

                if !npc.collides_with_bullet(bullet) {
                    continue;
                }

                if npc.npc_flags.shootable() {
                    npc.life = (npc.life as i32).saturating_sub(bullet.damage as i32).clamp(0, u16::MAX as i32) as u16;

                    if npc.life == 0 {
                        if npc.npc_flags.show_damage() {
                            npc.popup.add_value(-bullet.damage);
                        }

                        if self.player1.cond.alive() && npc.npc_flags.event_when_killed() {
                            state.control_flags.set_tick_world(true);
                            state.control_flags.set_interactions_disabled(true);
                            state.textscript_vm.start_script(npc.event_num);
                        } else {
                            npc.cond.set_explode_die(true);
                        }
                    } else {
                        if npc.shock < 14 {
                            if let Some(table_entry) = state.npc_table.get_entry(npc.npc_type) {
                                state.sound_manager.play_sfx_at(table_entry.hurt_sound, npc.x, npc.y);
                            }

                            npc.shock = 16;

                            for _ in 0..3 {
                                state.create_caret(
                                    (bullet.x + npc.x) / 2,
                                    (bullet.y + npc.y) / 2,
                                    CaretType::HurtParticles,
                                    Direction::Left,
                                );
                            }
                        }

                        if npc.npc_flags.show_damage() {
                            npc.popup.add_value(-bullet.damage);
                        }
                    }
                } else if !bullet.weapon_flags.no_proj_dissipation()
                    && bullet.btype != 13
                    && bullet.btype != 14
                    && bullet.btype != 15
                    && bullet.btype != 28
                    && bullet.btype != 29
                    && bullet.btype != 30
                {
                    state.create_caret(
                        (bullet.x + npc.x) / 2,
                        (bullet.y + npc.y) / 2,
                        CaretType::ProjectileDissipation,
                        Direction::Right,
                    );
                    state.sound_manager.play_sfx_at(31, bullet.x, bullet.y);
                    bullet.life = 0;
                    continue;
                }

                if bullet.life > 0 {
                    bullet.life -= 1;
                }
            }

            if npc.cond.explode_die() {
                let can_drop_missile = [&self.inventory_player1, &self.inventory_player2].iter().any(|inv| {
                    inv.has_weapon(WeaponType::MissileLauncher) || inv.has_weapon(WeaponType::SuperMissileLauncher)
                });

                let npc_id = npc.id;
                let npc_cond = npc.cond;
                npc.unborrow_then(|token| {
                    self.npc_list.kill_npc(npc_id as usize, !npc_cond.drs_novanish(), can_drop_missile, state, token);
                });
            }
        });

        for i in 0..self.boss.parts.len() {
            let mut idx = i;
            let mut npc = unsafe { self.boss.parts.get_unchecked_mut(i) };
            if !npc.cond.alive() {
                continue;
            }

            for bullet in self.bullet_manager.bullets.iter_mut() {
                if !bullet.cond.alive() || bullet.damage < 0 {
                    continue;
                }

                let hit = (npc.npc_flags.shootable()
                    && (npc.x - npc.hit_bounds.right as i32) < (bullet.x + bullet.enemy_hit_width as i32)
                    && (npc.x + npc.hit_bounds.right as i32) > (bullet.x - bullet.enemy_hit_width as i32)
                    && (npc.y - npc.hit_bounds.top as i32) < (bullet.y + bullet.enemy_hit_height as i32)
                    && (npc.y + npc.hit_bounds.bottom as i32) > (bullet.y - bullet.enemy_hit_height as i32))
                    || (npc.npc_flags.invulnerable()
                        && (npc.x - npc.hit_bounds.right as i32) < (bullet.x + bullet.hit_bounds.right as i32)
                        && (npc.x + npc.hit_bounds.right as i32) > (bullet.x - bullet.hit_bounds.left as i32)
                        && (npc.y - npc.hit_bounds.top as i32) < (bullet.y + bullet.hit_bounds.bottom as i32)
                        && (npc.y + npc.hit_bounds.bottom as i32) > (bullet.y - bullet.hit_bounds.top as i32));

                if !hit {
                    continue;
                }

                if npc.npc_flags.shootable() {
                    let shock = npc.shock;
                    if npc.cond.damage_boss() {
                        idx = 0;
                        npc = unsafe { self.boss.parts.get_unchecked_mut(0) };
                    }

                    npc.life = (npc.life as i32).saturating_sub(bullet.damage as i32).clamp(0, u16::MAX as i32) as u16;

                    if npc.life == 0 {
                        npc.life = npc.id;

                        if self.player1.cond.alive() && npc.npc_flags.event_when_killed() {
                            state.control_flags.set_tick_world(true);
                            state.control_flags.set_interactions_disabled(true);
                            state.textscript_vm.start_script(npc.event_num);
                        } else {
                            state.sound_manager.play_sfx_at(self.boss.death_sound[idx], npc.x, npc.y);

                            let destroy_count = 4usize * (2usize).pow((npc.size as u32).saturating_sub(1));

                            self.npc_list.create_death_smoke(
                                npc.x,
                                npc.y,
                                npc.display_bounds.right as usize,
                                destroy_count,
                                state,
                                &npc.rng,
                            );
                            npc.cond.set_alive(false);
                        }
                    } else {
                        if shock < 14 {
                            for _ in 0..3 {
                                state.create_caret(bullet.x, bullet.y, CaretType::HurtParticles, Direction::Left);
                            }
                            state.sound_manager.play_sfx_at(self.boss.hurt_sound[idx], npc.x, npc.y);
                        }

                        npc.shock = 8;
                        if npc.npc_flags.show_damage() {
                            npc.popup.add_value(-bullet.damage);
                        }

                        npc = unsafe { self.boss.parts.get_unchecked_mut(i) };
                        npc.shock = 8;
                    }

                    bullet.life = bullet.life.saturating_sub(1);
                    if bullet.life < 1 {
                        bullet.cond.set_alive(false);
                    }
                } else if [13, 14, 15, 28, 29, 30].contains(&bullet.btype) {
                    bullet.life = bullet.life.saturating_sub(1);
                } else if !bullet.weapon_flags.no_proj_dissipation() {
                    state.create_caret(bullet.x, bullet.y, CaretType::ProjectileDissipation, Direction::Right);
                    state.sound_manager.play_sfx_at(31, bullet.x, bullet.y);
                    bullet.life = 0;
                    continue;
                }
            }
        }
    }

    fn tick_world(&mut self, state: &mut SharedGameState) -> GameResult {
        self.nikumaru.tick(state, &self.player1)?;
        self.background.tick()?;
        self.hud_player1.visible = self.player1.cond.alive();
        self.hud_player2.visible = self.player2.cond.alive();
        self.hud_player1.has_player2 = self.player2.cond.alive() && !self.player2.cond.hidden();
        self.hud_player2.has_player2 = self.player1.cond.alive() && !self.player1.cond.hidden();
        if state.network.is_some() {
            self.hud_player1.has_player2 = false;
            self.hud_player2.has_player2 = false;
            self.hud_player1.alignment = Alignment::Left;
            self.hud_player2.alignment = Alignment::Left;
            for remote in &mut self.remote_players {
                remote.hud.alignment = Alignment::Left;
            }
        }

        self.player1.current_weapon = {
            if let Some(weapon) = self.inventory_player1.get_current_weapon_mut() {
                weapon.wtype as u8
            } else {
                0
            }
        };
        self.player2.current_weapon = {
            if let Some(weapon) = self.inventory_player2.get_current_weapon_mut() {
                weapon.wtype as u8
            } else {
                0
            }
        };
        self.player1.tick(state, &self.npc_list)?;
        self.player2.tick(state, &self.npc_list)?;
        for remote in &mut self.remote_players {
            remote.player.current_weapon = remote.inventory.get_current_weapon().map_or(0, |weapon| weapon.wtype as u8);
            remote.player.tick(state, &self.npc_list)?;
            if remote.player.damage > 0 {
                let loss = remote.player.damage * if remote.player.equip.has_arms_barrier() { 1 } else { 2 };
                remote.inventory.take_xp(loss, state);
                remote.player.damage = 0;
            }
        }
        state.textscript_vm.reset_invicibility = false;

        self.whimsical_star.tick(state, (&self.player1, &mut self.bullet_manager))?;

        if self.player1.damage > 0 {
            let xp_loss = self.player1.damage * if self.player1.equip.has_arms_barrier() { 1 } else { 2 };
            match self.inventory_player1.take_xp(xp_loss, state) {
                TakeExperienceResult::LevelDown if self.player1.life > 0 => {
                    state.create_caret(self.player1.x, self.player1.y, CaretType::LevelUp, Direction::Right);
                }
                _ => {}
            }

            self.player1.damage = 0;
        }

        if self.player2.damage > 0 {
            let xp_loss = self.player2.damage * if self.player2.equip.has_arms_barrier() { 1 } else { 2 };
            match self.inventory_player2.take_xp(xp_loss, state) {
                TakeExperienceResult::LevelDown if self.player2.life > 0 => {
                    state.create_caret(self.player2.x, self.player2.y, CaretType::LevelUp, Direction::Right);
                }
                _ => {}
            }

            self.player2.damage = 0;
        }

        self.npc_list.try_for_each_alive_mut(&mut self.npc_token, |mut npc| {
            map_err_to_break(
                npc.tick(
                    state,
                    NPCContext {
                        players: std::iter::once(&mut self.player1)
                            .chain(std::iter::once(&mut self.player2))
                            .chain(self.remote_players.iter_mut().map(|remote| &mut remote.player))
                            .collect(),
                        npc_list: &self.npc_list,
                        stage: &mut self.stage,
                        bullet_manager: &mut self.bullet_manager,
                        flash: &mut self.flash,
                        boss: &mut self.boss,
                    },
                ),
            )?;

            ControlFlow::Continue(())
        })?;

        self.boss.tick(
            state,
            BossNPCContext {
                players: std::iter::once(&mut self.player1)
                    .chain(std::iter::once(&mut self.player2))
                    .chain(self.remote_players.iter_mut().map(|remote| &mut remote.player))
                    .collect(),
                npc_list: &self.npc_list,
                npc_token: &mut self.npc_token,
                stage: &mut self.stage,
                bullet_manager: &mut self.bullet_manager,
                flash: &mut self.flash,
            },
        )?;

        //decides if the player is tangible or not
        if !state.settings.noclip {
            if !self.player1.bubble {
                self.player1.tick_map_collisions(state, &self.npc_list, &mut self.stage);
            }
            if !self.player2.bubble {
                self.player2.tick_map_collisions(state, &self.npc_list, &mut self.stage);
            }

            self.player1.tick_npc_collisions(
                TargetPlayer::Player1,
                state,
                &self.npc_list,
                &mut self.npc_token,
                &mut self.boss,
                &mut self.inventory_player1,
            );
            self.player2.tick_npc_collisions(
                TargetPlayer::Player2,
                state,
                &self.npc_list,
                &mut self.npc_token,
                &mut self.boss,
                &mut self.inventory_player2,
            );
        }

        for (index, remote) in self.remote_players.iter_mut().enumerate() {
            if !state.settings.noclip && !remote.player.bubble {
                remote.player.tick_map_collisions(state, &self.npc_list, &mut self.stage);
                remote.player.tick_npc_collisions(
                    TargetPlayer::from_index(index + 2),
                    state,
                    &self.npc_list,
                    &mut self.npc_token,
                    &mut self.boss,
                    &mut remote.inventory,
                );
            }
        }
        self.npc_list.for_each_alive_mut(&mut self.npc_token, |mut npc| {
            if !npc.npc_flags.ignore_solidity() {
                npc.tick_map_collisions(state, &self.npc_list, &mut self.stage);
            }
        });

        for npc in self.boss.parts.iter_mut() {
            if npc.cond.alive() && !npc.npc_flags.ignore_solidity() {
                npc.tick_map_collisions(state, &self.npc_list, &mut self.stage);
            }
        }

        if !self.water_params.entries.is_empty() {
            self.tick_npc_splash(state);
        }

        self.bullet_manager.tick_map_collisions(state, &self.npc_list, &mut self.stage);

        self.tick_npc_bullet_collissions(state);

        if state.control_flags.control_enabled() {
            self.inventory_player1.tick_weapons(
                state,
                &mut self.player1,
                TargetPlayer::Player1,
                &mut self.bullet_manager,
            );
            self.inventory_player2.tick_weapons(
                state,
                &mut self.player2,
                TargetPlayer::Player2,
                &mut self.bullet_manager,
            );
        }

        if state.control_flags.control_enabled() {
            for (index, remote) in self.remote_players.iter_mut().enumerate() {
                remote.inventory.tick_weapons(
                    state,
                    &mut remote.player,
                    TargetPlayer::from_index(index + 2),
                    &mut self.bullet_manager,
                );
            }
        }
        let players: Vec<_> = std::iter::once(&self.player1)
            .chain(std::iter::once(&self.player2))
            .chain(self.remote_players.iter().map(|remote| &remote.player))
            .collect();
        self.bullet_manager.tick_bullets(state, &players, &self.npc_list);
        if state.network.is_some() {
            self.tick_bubbles(state);
            self.check_network_game_over(state);
        }
        state.tick_carets();

        match self.frame.update_target {
            UpdateTarget::Player => {
                if self.player2.cond.alive()
                    && !self.player2.cond.hidden()
                    && (self.player1.x - self.player2.x).abs() < 240 * 0x200
                    && (self.player1.y - self.player2.y).abs() < 200 * 0x200
                    && self.player1.control_mode != ControlMode::IronHead
                {
                    self.frame.target_x = (self.player1.target_x * 2 + self.player2.target_x) / 3;
                    self.frame.target_y = (self.player1.target_y * 2 + self.player2.target_y) / 3;

                    self.frame.target_x = self.frame.target_x.clamp(self.player1.x - 0x8000, self.player1.x + 0x8000);
                    self.frame.target_y = self.frame.target_y.clamp(self.player1.y, self.player1.y);
                } else {
                    self.frame.target_x = self.player1.target_x;
                    self.frame.target_y = self.player1.target_y;
                }

                if self.player2.cond.alive()
                    && !self.player2.cond.hidden()
                    && !state.network.as_ref().map_or(false, |s| s.applied_rules.individual_cameras)
                {
                    if self.player2.x + 0x1000 < self.frame.x
                        || self.player2.x - 0x1000 > self.frame.x + state.canvas_size.0 as i32 * 0x200
                        || self.player2.y + 0x1000 < self.frame.y
                        || self.player2.y - 0x1000 > self.frame.y + state.canvas_size.1 as i32 * 0x200
                    {
                        self.player2.update_teleport_counter(state);

                        if self.player2.teleport_counter == 0 {
                            self.player2.x = self.player1.x;
                            self.player2.y = self.player1.y;

                            let mut npc = NPC::create(4, &state.npc_table);
                            npc.x = self.player2.x;
                            npc.y = self.player2.y;
                            npc.cond.set_alive(true);

                            let _ = self.npc_list.spawn(0x100, npc);
                        }
                    } else {
                        self.player2.teleport_counter = 0;
                    }
                }
            }
            UpdateTarget::NPC(npc_id) => {
                if let Some(npc) = self.npc_list.get_npc(npc_id as usize) {
                    let npc = npc.borrow(&self.npc_token);

                    if npc.cond.alive() {
                        self.frame.target_x = npc.x;
                        self.frame.target_y = npc.y;
                    }
                }
            }
            UpdateTarget::Boss(boss_id) => {
                if let Some(boss) = self.boss.parts.get(boss_id as usize) {
                    if boss.cond.alive() {
                        self.frame.target_x = boss.x;
                        self.frame.target_y = boss.y;
                    }
                }
            }
        }

        for remote in &mut self.remote_players {
            if !state.network.as_ref().map_or(false, |s| s.applied_rules.individual_cameras)
                && remote.player.cond.alive()
                && !remote.player.cond.hidden()
                && ((remote.player.x as i64 - self.player1.x as i64).abs() > 240 * 0x200
                    || (remote.player.y as i64 - self.player1.y as i64).abs() > 200 * 0x200)
            {
                remote.player.update_teleport_counter(state);
                if remote.player.teleport_counter == 0 {
                    remote.player.x = self.player1.x;
                    remote.player.y = self.player1.y;
                }
            } else {
                remote.player.teleport_counter = 0;
            }
        }
        self.tilemap.tick()?;

        self.frame.update(state, &self.stage);
        self.update_network_cameras(state, false);

        if state.control_flags.control_enabled() {
            self.hud_player1.tick(state, (&self.player1, &mut self.inventory_player1))?;
            self.hud_player2.tick(state, (&self.player2, &mut self.inventory_player2))?;
            for remote in &mut self.remote_players {
                remote.hud.visible = remote.player.cond.alive();
                remote.hud.tick(state, (&remote.player, &mut remote.inventory))?;
            }
            self.boss_life_bar.tick(state, (&self.npc_list, &self.npc_token, &self.boss))?;

            if state.textscript_vm.state == TextScriptExecutionState::Ended {
                let slots = if state.network.is_some() { 2 + self.remote_players.len() } else { 1 };
                for slot in 0..slots {
                    let player = self.player_at(slot);
                    if !player.cond.alive() {
                        continue;
                    }
                    if player.controller.trigger_inventory() {
                        match slot {
                            0 => self.inventory_player1.current_item = 0,
                            1 => self.inventory_player2.current_item = 0,
                            _ => self.remote_players[slot - 2].inventory.current_item = 0,
                        }
                        self.player_at_mut(slot).cond.set_interacted(false);
                        if state.network.is_some() {
                            self.network_inventories[slot] = Some(network_inventory::NetworkInventory::new(state));
                        } else {
                            state.textscript_vm.executor_player = TargetPlayer::from_index(slot);
                            state.textscript_vm.set_mode(ScriptMode::Inventory);
                            break;
                        }
                    } else if state.network.is_none()
                        && player.controller.trigger_map()
                        && player.equip.has_map()
                    {
                        state.textscript_vm.state = TextScriptExecutionState::MapSystem;
                        break;
                    }
                }
            }
        }

        if state.constants.is_switch {
            self.player1.has_dog = self.inventory_player1.has_item(14);
            self.player2.has_dog = self.inventory_player2.has_item(14);
            for remote in &mut self.remote_players {
                remote.player.has_dog = remote.inventory.has_item(14);
            }
        }

        self.water_renderer.tick(
            state,
            (
                &std::iter::once(&self.player1)
                    .chain(std::iter::once(&self.player2))
                    .chain(self.remote_players.iter().map(|r| &r.player))
                    .collect::<Vec<_>>() as &[&Player],
                &self.npc_list,
                &self.npc_token,
            ),
        )?;

        if self.map_name_counter > 0 {
            self.map_name_counter -= 1;
        }

        Ok(())
    }

    fn draw_debug_object(
        &self,
        entity: &dyn PhysicalEntity,
        state: &mut SharedGameState,
        ctx: &mut Context,
    ) -> GameResult {
        let view = self.view_frame(state);
        let frame = view.as_ref();
        if entity.x() < (frame.x - 128 - entity.display_bounds().width() as i32 * 0x200)
            || entity.x()
                > (frame.x + 128 + (state.canvas_size.0 as i32 + entity.display_bounds().width() as i32) * 0x200)
                && entity.y() < (frame.y - 128 - entity.display_bounds().height() as i32 * 0x200)
            || entity.y()
                > (frame.y + 128 + (state.canvas_size.1 as i32 + entity.display_bounds().height() as i32) * 0x200)
        {
            return Ok(());
        }

        {
            let hit_rect_size = entity.hit_rect_size().clamp(1, 4);
            let hit_rect_size = if state.tile_size == TileSize::Tile8x8 {
                4 * hit_rect_size * hit_rect_size
            } else {
                hit_rect_size * hit_rect_size
            };

            let tile_size = state.tile_size.as_int() * 0x200;
            let x = (entity.x() + entity.offset_x()) / tile_size;
            let y = (entity.y() + entity.offset_y()) / tile_size;

            let batch = state.texture_set.get_or_load_batch(ctx, &state.constants, "Caret")?;

            const CARET_RECT: Rect<u16> = Rect { left: 2, top: 74, right: 6, bottom: 78 };
            const CARET2_RECT: Rect<u16> = Rect { left: 65, top: 9, right: 71, bottom: 15 };

            for (idx, &(ox, oy)) in OFFSETS.iter().enumerate() {
                if idx == hit_rect_size {
                    break;
                }

                batch.add_rect(
                    ((x + ox) * tile_size - frame.x) as f32 / 512.0 - 2.0,
                    ((y + oy) * tile_size - frame.y) as f32 / 512.0 - 2.0,
                    &CARET_RECT,
                );
            }

            batch.add_rect(
                (entity.x() - frame.x) as f32 / 512.0 - 3.0,
                (entity.y() - frame.y) as f32 / 512.0 - 3.0,
                &CARET2_RECT,
            );

            batch.draw(ctx)?;
        }

        Ok(())
    }

    fn draw_debug_npc(&self, npc: &NPC, state: &mut SharedGameState, ctx: &mut Context) -> GameResult {
        let view = self.view_frame(state);
        let frame = view.as_ref();
        self.draw_debug_object(npc, state, ctx)?;

        let text = format!("{}:{}:{}", npc.id, npc.npc_type, npc.action_num);
        state
            .font
            .builder()
            .position(((npc.x - frame.x) / 0x200) as f32, ((npc.y - frame.y) / 0x200) as f32)
            .scale(0.5)
            .shadow(true)
            .color((255, 255, 0, 255))
            .draw(&text, ctx, &state.constants, &mut state.texture_set)?;

        Ok(())
    }

    fn draw_debug_outlines(&self, state: &mut SharedGameState, ctx: &mut Context) -> GameResult {
        for npc in self.npc_list.iter_alive(&self.npc_token) {
            self.draw_debug_npc(&npc, state, ctx)?;
        }

        for boss in self.boss.parts.iter().filter(|n| n.cond.alive()) {
            self.draw_debug_npc(boss, state, ctx)?;
        }

        self.draw_debug_object(&self.player1, state, ctx)?;

        Ok(())
    }
}

impl Scene for GameScene {
    fn init(&mut self, state: &mut SharedGameState, ctx: &mut Context) -> GameResult {
        ctx.keyboard_context.native_text_input = false;
        ctx.keyboard_context.take_text_input();
        if let Some(session) = &state.network {
            state.difficulty = session.applied_rules.difficulty;
            state.settings.timing_mode = session.applied_rules.timing.mode();
        }
        if state.mod_path.is_some() && state.replay_state == ReplayState::Recording {
            self.replay.initialize_recording(state);
        }
        if state.player_count == PlayerCount::Two {
            self.add_player2(state, ctx);
            if state.network.is_some() {
                self.player2.network_skin = None;
            }
        } else {
            self.drop_player2();
        }

        if state.mod_path.is_some() {
            if let ReplayState::Playback(replay_kind) = state.replay_state {
                self.replay.initialize_playback(state, ctx, replay_kind)?;
            }
        }

        if state.network.is_some() {
            self.apply_roster(state, ctx, None);
        }
        self.npc_list.set_rng_seed(state.game_rng.next());
        self.boss.init_rng(state.game_rng.next());
        state.textscript_vm.set_scene_script(self.stage.load_text_script(
            &state.constants.base_paths,
            &state.constants,
            ctx,
        )?);
        state.textscript_vm.suspend = false;
        state.tile_size = self.stage.map.tile_size;

        self.player1.controller = state.settings.create_player1_controller();
        self.player2.controller = state.settings.create_player2_controller();

        let npcs = self.stage.load_npcs(&state.constants.base_paths, ctx)?;
        for npc_data in npcs.iter() {
            log::debug!("creating npc: {:?}", npc_data);

            let mut npc = NPC::create_from_data(npc_data, &state.npc_table, state.tile_size);
            if npc.npc_flags.appear_when_flag_set() {
                if state.get_flag(npc_data.flag_num as _) {
                    npc.cond.set_alive(true);
                }
            } else if npc.npc_flags.hide_unless_flag_set() {
                if !state.get_flag(npc_data.flag_num as _) {
                    npc.cond.set_alive(true);
                }
            } else {
                npc.cond.set_alive(true);
            }

            self.npc_list.spawn_at_slot(npc_data.id, npc)?;
        }

        state.npc_table.stage_textures = self.stage_textures.clone();

        self.boss.boss_type = self.stage.data.boss_no as u16;
        self.player1.target_x = self.player1.x;
        self.player1.target_y = self.player1.y;
        self.player1.camera_target_x = 0;
        self.player1.camera_target_y = 0;
        self.player2.target_x = self.player2.x;
        self.player2.target_y = self.player2.y;
        self.player2.camera_target_x = 0;
        self.player2.camera_target_y = 0;
        self.frame.target_x = self.player1.x;
        self.frame.target_y = self.player1.y;
        let canvas = state.canvas_size;
        if state.network.is_some() {
            state.canvas_size = (320.0, 240.0);
        }
        self.frame.immediate_update(state, &self.stage);
        self.update_network_cameras(state, true);
        state.canvas_size = canvas;

        // I'd personally set it to something higher but left it as is for accuracy.
        state.water_level = 0x1e0000;

        state.carets.clear();

        self.lighting_mode = match () {
            _ if self.intro_mode => LightingMode::None,
            _ if !state.constants.is_switch
                && (self.stage.data.background_type == BackgroundType::Black
                    || self.stage.data.background.name() == "bkBlack") =>
            {
                LightingMode::Ambient
            }
            _ if state.constants.is_switch
                && (self.stage.data.background_type == BackgroundType::Black
                    || self.stage.data.background.name() == "bkBlack") =>
            {
                LightingMode::None
            }
            _ if self.stage.data.background.name() == "bkFall" => LightingMode::None,
            _ if self.stage.data.background_type != BackgroundType::Black
                && self.stage.data.background_type != BackgroundType::Outside
                && self.stage.data.background_type != BackgroundType::OutsideWind
                && self.stage.data.background.name() != "bkBlack" =>
            {
                LightingMode::BackgroundOnly
            }
            _ => LightingMode::None,
        };

        self.pause_menu.init(state, ctx)?;
        self.whimsical_star.init(&self.player1);

        #[cfg(feature = "discord-rpc")]
        {
            if self.stage.data.map == state.stages[state.constants.game.intro_stage as usize].map {
                state.discord_rpc.set_initializing()?;
            } else {
                state.discord_rpc.update_hp(&self.player1)?;
                state.discord_rpc.update_stage(&self.stage.data)?;
                state.discord_rpc.set_in_game()?;
            }
        }

        Ok(())
    }

    fn tick(&mut self, state: &mut SharedGameState, ctx: &mut Context) -> GameResult {
        if state.network.is_none() {
            return self.tick_simulation(state, ctx);
        }
        if state.network.as_ref().unwrap().leave_requested {
            state.end_network_session();
            ctx.keyboard_context.native_text_input = false;
            ctx.keyboard_context.take_text_input();
            state.reload_resources(ctx)?;
            state.update_locale(ctx);
            state.next_scene = Some(Box::new(TitleScene::new()));
            return Ok(());
        }
        let mut session = state.network.take().unwrap();
        session.local_controller.update(state, ctx)?;
        session.local_controller.update_trigger();
        state.network = Some(session);
        let chat_was_open = state.network.as_ref().unwrap().chat_open;
        self.network_menu.tick_ingame(state, ctx)?;
        self.network_chat.tick(state, ctx)?;
        let map_was_open = self.network_map_system.is_open();
        self.tick_network_map(state, ctx);
        let session = state.network.take().unwrap();
        let input = if chat_was_open
            || session.chat_open
            || session.options_open
            || map_was_open
            || self.network_map_system.is_open()
        {
            crate::game::network::Input::neutral()
        } else {
            crate::game::network::Input::capture(&*session.local_controller).without_map()
        };
        state.network = Some(session);
        let mut prediction = std::mem::take(&mut self.prediction);
        let presentation = prediction.confirmed.as_ref().map(|_| rollback::Presentation::capture(self));
        let mut transport_error = None;
        // If no authoritative update arrived, the predicted scene is already at
        // the right point. Keep it and advance once instead of replaying its tail.
        if prediction.confirmed.is_some() {
            if let Some((sequence, checksum)) = self.confirmed_checksum {
                let can_predict = self.can_predict(state);
                let session = state.network.as_mut().unwrap();
                if !session.host && sequence == session.sequence() {
                    let target = can_predict.then(|| {
                        prediction.target(
                            sequence,
                            session.pings[session.local_slot],
                            session.applied_rules.timing == crate::game::network::GameTiming::CSPlus,
                        )
                    });
                    match session.prediction_can_continue(input, checksum, target) {
                        Ok(true) => {
                            if let Some(target) = target {
                                self.extend_prediction(state, ctx, &mut prediction, input, target)?;
                            }
                            self.prediction = prediction;
                            return Ok(());
                        }
                        Ok(false) => {}
                        Err(error) => transport_error = Some(error),
                    }
                }
            }
        }
        // Checksums and transport acknowledgements always refer to confirmed state.
        if let Some(confirmed) = &prediction.confirmed {
            confirmed.restore(self, state);
        }
        let session = state.network.as_ref().unwrap();
        let catching_up = session.catching_up();
        let target = if !session.host && !catching_up && self.can_predict(state) {
            Some(prediction.target(
                session.sequence(),
                session.pings[session.local_slot],
                session.applied_rules.timing == crate::game::network::GameTiming::CSPlus,
            ))
        } else {
            None
        };
        let limit = if session.host {
            1
        } else if catching_up {
            200
        } else {
            32
        };
        let mut confirmed_count = 0;
        for _ in 0..limit {
            let checksum = self.network_checksum(state)?;
            let mut session = state.network.take().unwrap();
            let result = match transport_error.take() {
                Some(error) => Err(error),
                None => session.poll_predicted(input, checksum, target),
            };
            let restart = session.restart_requested;
            session.restart_requested = false;
            state.network = Some(session);
            if restart {
                state.prepare_network_replay();
                state.load_or_start_game(ctx)?;
                return Ok(());
            }
            let frame = match result {
                Ok(Some(frame)) => frame,
                Ok(None) => break,
                Err(error) => {
                    log::warn!("{}", error);
                    state.end_network_session();
                    ctx.keyboard_context.native_text_input = false;
                    ctx.keyboard_context.take_text_input();
                    state.stop_noise();
                    state.next_scene =
                        Some(Box::new(crate::scene::network_error_scene::NetworkErrorScene::new(error.to_string())));
                    return Ok(());
                }
            };
            prediction.confirm(frame.sequence);
            confirmed_count += 1;
            state.difficulty = state.network.as_ref().unwrap().applied_rules.difficulty;
            self.apply_roster(state, ctx, frame.migration_from);
            self.check_network_game_over(state);
            if frame.retry {
                state.load_or_start_game(ctx)?;
                return Ok(());
            }
            self.simulate_network_frame(state, ctx)?;
            if state.next_scene.is_some() {
                return Ok(());
            }
            // A migrated host produces exactly one frame per game tick.
            if state.network.as_ref().unwrap().host {
                break;
            }
        }
        if state.network.as_ref().unwrap().host || state.network.as_ref().unwrap().catching_up() {
            return Ok(());
        }
        if confirmed_count > 0 && prediction.confirmed.is_some() && !prediction.inputs.is_empty() {
            prediction.rollbacks += 1;
            if prediction.rollbacks % 100 == 1 {
                log::debug!(
                    "Network rollback: confirmed={}, replay={}",
                    state.network.as_ref().unwrap().sequence(),
                    prediction.inputs.len()
                );
            }
        }
        prediction.confirmed = Some(Box::new(rollback::Snapshot::capture(self, state)));
        // The transport fast path must acknowledge the authoritative world,
        // never the speculative scene left after the following replay.
        self.network_checksum(state)?;
        let sequence = state.network.as_ref().unwrap().sequence();
        let mut replayed = 0;
        for &(frame, local_input) in &prediction.inputs {
            if frame != sequence + replayed as u64 || !self.predict_network_frame(state, ctx, local_input)? {
                break;
            }
            replayed += 1;
        }
        prediction.inputs.truncate(replayed);
        if let Some(target) = target {
            // Keep an input lead so fresh local inputs reach the host before their simulation frame.
            // Fill missing frames with the last held local input, without repeating edge buttons.
            self.extend_prediction(state, ctx, &mut prediction, input, target)?;
        }
        if confirmed_count == 0 && !prediction.inputs.is_empty() {
            log::debug!(
                "Network prediction: confirmed={}, ahead={}, tick={}",
                sequence,
                prediction.inputs.len(),
                self.tick
            );
        }
        if !prediction.inputs.is_empty() {
            if let Some(presentation) = presentation {
                presentation.interpolate(self);
            }
        }
        self.prediction = prediction;
        Ok(())
    }

    fn draw_tick(&mut self, state: &mut SharedGameState) -> GameResult {
        if state.network.is_none() {
            self.update_interpolation(state)?;
        }
        Ok(())
    }

    fn draw(&self, state: &mut SharedGameState, ctx: &mut Context) -> GameResult {
        let view = self.view_frame(state);
        let frame = view.as_ref();
        //graphics::set_canvas(ctx, Some(&state.game_canvas));

        if self.player1.control_mode == ControlMode::IronHead {
            self.set_ironhead_clip(state, ctx)?;
        }

        let stage_textures_ref = &*self.stage_textures.deref().borrow();
        self.background.draw(state, ctx, frame, stage_textures_ref, &self.stage)?;
        self.tilemap.draw(state, ctx, frame, TileLayer::Background, stage_textures_ref, &self.stage)?;
        self.draw_npc_layer(state, ctx, NPCLayer::Background)?;
        self.tilemap.draw(state, ctx, frame, TileLayer::Middleground, stage_textures_ref, &self.stage)?;

        if state.settings.shader_effects && self.lighting_mode == LightingMode::BackgroundOnly {
            self.draw_light_map(state, ctx)?;
        }

        self.boss.draw(state, ctx, frame)?;
        self.draw_npc_layer(state, ctx, NPCLayer::Middleground)?;
        self.draw_bullets(state, ctx)?;
        self.player2.draw(state, ctx, frame)?;
        self.player1.draw(state, ctx, frame)?;
        for remote in &self.remote_players {
            remote.player.draw(state, ctx, frame)?;
        }

        if !self.player1.cond.hidden() {
            self.whimsical_star.draw(state, ctx, frame)?;
        }

        self.water_renderer.draw(state, ctx, frame, WaterLayer::Back)?;
        self.tilemap.draw(state, ctx, frame, TileLayer::Foreground, stage_textures_ref, &self.stage)?;
        self.tilemap.draw(state, ctx, frame, TileLayer::Snack, stage_textures_ref, &self.stage)?;
        self.water_renderer.draw(state, ctx, frame, WaterLayer::Front)?;

        self.draw_carets(state, ctx)?;
        self.player1.exp_popup.draw(state, ctx, frame)?;
        self.player1.damage_popup.draw(state, ctx, frame)?;
        self.player2.exp_popup.draw(state, ctx, frame)?;
        self.player2.damage_popup.draw(state, ctx, frame)?;
        for remote in &self.remote_players {
            remote.player.exp_popup.draw(state, ctx, frame)?;
            remote.player.damage_popup.draw(state, ctx, frame)?;
        }
        self.draw_npc_popup(state, ctx)?;
        self.draw_boss_popup(state, ctx)?;

        if !state.control_flags.credits_running()
            && state.settings.shader_effects
            && self.lighting_mode == LightingMode::Ambient
        {
            self.draw_light_map(state, ctx)?;
        }
        self.flash.draw(state, ctx, frame)?;

        self.draw_black_bars(state, ctx)?;

        if self.player1.control_mode == ControlMode::IronHead {
            graphics::set_clip_rect(ctx, None)?;
        }

        if state.settings.show_player_names {
            if let Some(session) = &state.network {
                let (frame_x, frame_y) = frame.xy_interpolated(state.frame_time);
                for (index, member) in session.applied_members.iter().enumerate() {
                    let Some(member) = member else {
                        continue;
                    };
                    let player = self.player_at(index);
                    if (!player.cond.alive() && !player.bubble) || player.cond.hidden() {
                        continue;
                    }
                    let x = interpolate_fix9_scale(player.prev_x, player.x, state.frame_time) - frame_x;
                    let mut y = interpolate_fix9_scale(player.prev_y, player.y, state.frame_time) - frame_y - 24.0;
                    let stacked = (0..index)
                        .filter(|&other| {
                            session.applied_members[other].is_some()
                                && (self.player_at(other).x as i64 - player.x as i64).abs() < 32 * 0x200
                                && (self.player_at(other).y as i64 - player.y as i64).abs() < 16 * 0x200
                        })
                        .count();
                    y -= stacked as f32 * (state.font.line_height() + 2.0);
                    let width = state.font.builder().compute_width(&member.name);
                    state.font.builder().position(x - width / 2.0, y).shadow(true).draw(
                        &member.name,
                        ctx,
                        &state.constants,
                        &mut state.texture_set,
                    )?;
                }
            }
        }

        if self.inventory_dim > 0.0 {
            let rect = Rect::new(0, 0, state.screen_size.0 as isize + 1, state.screen_size.1 as isize + 1);
            let mut dim_color = state.constants.inventory_dim_color;
            dim_color.a *= self.inventory_dim;
            graphics::draw_rect(ctx, rect, dim_color)?;
        }

        match state.textscript_vm.mode {
            ScriptMode::Map | ScriptMode::Debug if state.control_flags.control_enabled() => {
                if let Some(session) = &state.network {
                    match session.local_slot {
                        0 => self.hud_player1.draw(state, ctx, frame)?,
                        1 => self.hud_player2.draw(state, ctx, frame)?,
                        slot => self.remote_players[slot - 2].hud.draw(state, ctx, frame)?,
                    }
                } else {
                    self.hud_player1.draw(state, ctx, frame)?;
                    self.hud_player2.draw(state, ctx, frame)?;
                }
                self.boss_life_bar.draw(state, ctx, frame)?;

                if self.player2.cond.alive() && !self.player2.cond.hidden() {
                    if self.player2.teleport_counter < state.settings.timing_mode.get_tps() as u16 * 3
                        || self.player2.teleport_counter % 5 != 0
                    {
                        if self.player2.y + 0x1000 < frame.y {
                            let scale = 1.0 + (frame.y as f32 / self.player2.y as f32 / 2.0 - 0.5).clamp(0.0, 2.0);

                            let x = interpolate_fix9_scale(
                                self.player2.prev_x - frame.prev_x,
                                self.player2.x - frame.x,
                                state.frame_time,
                            );

                            let x = x.clamp(8.0, state.canvas_size.0 - 8.0 * scale - state.font.line_height());

                            state
                                .font
                                .builder()
                                .position(x, 8.0)
                                .scale(scale)
                                .shadow_color((0, 0, 130, 255))
                                .color((96, 96, 255, 255))
                                .shadow(true)
                                .draw(P2_OFFSCREEN_TEXT, ctx, &state.constants, &mut state.texture_set)?;
                        } else if self.player2.y - 0x1000 > frame.y + state.canvas_size.1 as i32 * 0x200 {
                            let scale = 1.0
                                + (self.player2.y as f32 / (frame.y as f32 + state.canvas_size.1 * 0x200 as f32) - 0.5)
                                    .clamp(0.0, 2.0);

                            let x = interpolate_fix9_scale(
                                self.player2.prev_x - frame.prev_x,
                                self.player2.x - frame.x,
                                state.frame_time,
                            );

                            let x = x.clamp(8.0, state.canvas_size.0 - 8.0 * scale - state.font.line_height());

                            state
                                .font
                                .builder()
                                .position(x, state.canvas_size.1 - 8.0 * scale - state.font.line_height())
                                .scale(scale)
                                .shadow_color((0, 0, 130, 255))
                                .color((96, 96, 255, 255))
                                .shadow(true)
                                .draw(P2_OFFSCREEN_TEXT, ctx, &state.constants, &mut state.texture_set)?;
                        } else if self.player2.x + 0x1000 < frame.x {
                            let scale = 1.0 + (frame.x as f32 / self.player2.x as f32 / 2.0 - 0.5).clamp(0.0, 2.0);

                            let y = interpolate_fix9_scale(
                                self.player2.prev_y - frame.prev_y,
                                self.player2.y - frame.y,
                                state.frame_time,
                            );
                            let y = y.clamp(8.0, state.canvas_size.1 - 8.0 * scale - state.font.line_height());

                            state
                                .font
                                .builder()
                                .position(8.0, y)
                                .scale(scale)
                                .shadow_color((0, 0, 130, 255))
                                .color((96, 96, 255, 255))
                                .shadow(true)
                                .draw(P2_OFFSCREEN_TEXT, ctx, &state.constants, &mut state.texture_set)?;
                        } else if self.player2.x - 0x1000 > frame.x + state.canvas_size.0 as i32 * 0x200 {
                            let scale = 1.0
                                + (self.player2.x as f32 / (frame.x as f32 + state.canvas_size.0 * 0x200 as f32) - 0.5)
                                    .clamp(0.0, 2.0);

                            let y = interpolate_fix9_scale(
                                self.player2.prev_y - frame.prev_y,
                                self.player2.y - frame.y,
                                state.frame_time,
                            );
                            let y = y.clamp(8.0, state.canvas_size.1 - 8.0 * scale - state.font.line_height());

                            let width = state.font.builder().compute_width(P2_OFFSCREEN_TEXT);

                            state
                                .font
                                .builder()
                                .shadow_color((0, 0, 130, 255))
                                .color((96, 96, 255, 255))
                                .shadow(true)
                                .position(state.canvas_size.0 - width - 8.0 * scale, y)
                                .scale(scale)
                                .draw(P2_OFFSCREEN_TEXT, ctx, &state.constants, &mut state.texture_set)?;
                        }
                    }
                }
            }
            ScriptMode::StageSelect => self.stage_select.draw(state, ctx, frame)?,
            ScriptMode::Inventory => self.inventory_ui.draw(state, ctx, frame)?,
            _ => {}
        }

        self.map_system.draw(
            state,
            ctx,
            &self.stage,
            &std::iter::once(&self.player1)
                .chain(std::iter::once(&self.player2))
                .chain(self.remote_players.iter().map(|r| &r.player))
                .collect::<Vec<_>>(),
        )?;
        self.fade.draw(state, ctx, frame)?;

        if state.textscript_vm.mode == ScriptMode::Map || state.textscript_vm.mode == ScriptMode::Debug {
            self.nikumaru.draw(state, ctx, frame)?;
        }

        if (state.textscript_vm.mode == ScriptMode::Map || state.textscript_vm.mode == ScriptMode::Debug)
            && state.textscript_vm.state != TextScriptExecutionState::MapSystem
            && self.map_name_counter > 0
        {
            let map_name = if self.stage.data.name == "u" {
                state.constants.title.intro_text.as_str()
            } else {
                if state.constants.is_cs_plus && state.settings.locale == "jp" {
                    self.stage.data.name_jp.as_str()
                } else {
                    self.stage.data.name.as_str()
                }
            };

            state.font.builder().shadow(true).y(80.0).center(state.canvas_size.0).draw(
                map_name,
                ctx,
                &state.constants,
                &mut state.texture_set,
            )?;
        }

        if state.control_flags.credits_running() {
            self.credits.draw(state, ctx, frame)?;
        }

        self.falling_island.draw(state, ctx, frame)?;
        self.text_boxes.draw(state, ctx, frame)?;
        self.draw_network_inventory(state, ctx, frame)?;
        if state.network.is_some() {
            self.network_map_system.draw(
                state,
                ctx,
                &self.stage,
                &std::iter::once(&self.player1)
                    .chain(std::iter::once(&self.player2))
                    .chain(self.remote_players.iter().map(|remote| &remote.player))
                    .collect::<Vec<_>>(),
            )?;
        }

        if (self.skip_counter > 1 || state.tutorial_counter > 0)
            && (state.settings.cutscene_skip_mode != CutsceneSkipMode::Auto)
        {
            let key = {
                if state.settings.touch_controls {
                    ">>".to_owned()
                } else {
                    match state.settings.player1_controller_type {
                        ControllerType::Keyboard => format!("{:?}", state.settings.player1_key_map.skip),
                        ControllerType::Gamepad(_) => "=".to_owned(),
                    }
                }
            };

            let text = state.tt("game.cutscene_skip", &[("key", key.as_str())]);

            let gamepad_sprite_offset = match state.settings.player1_controller_type {
                ControllerType::Keyboard => 1,
                ControllerType::Gamepad(index) => ctx.gamepad_context.get_gamepad_sprite_offset(index as usize),
            };

            let symbols = Symbols {
                symbols: &[(
                    '=',
                    state.settings.player1_controller_button_map.skip.get_rect(gamepad_sprite_offset, &state.constants),
                )],
                texture: "buttons",
            };

            // let width = state.font.text_width_with_rects(text.chars(), &rect_map, &state.constants);
            let width = state.font.builder().with_symbols(Some(symbols)).compute_width(&text);
            let pos_x = state.canvas_size.0 - width - 20.0;
            let pos_y = 0.0;
            let line_height = state.font.line_height();
            let w = (self.skip_counter as f32 / CUTSCENE_SKIP_WAIT as f32) * (width + 20.0) / 2.0;
            let mut rect = Rect::new_size(
                (pos_x * state.scale) as isize,
                (pos_y * state.scale) as isize,
                ((20.0 + width) * state.scale) as isize,
                ((10.0 + line_height) * state.scale) as isize,
            );

            draw_rect(ctx, rect, state.constants.background_color)?;

            rect.right = rect.left + (w * state.scale) as isize;
            draw_rect(ctx, rect, Color::from_rgb(128, 128, 160))?;

            rect.left = ((state.canvas_size.0 - w) * state.scale) as isize;
            rect.right = rect.left + (w * state.scale).ceil() as isize;
            draw_rect(ctx, rect, Color::from_rgb(128, 128, 160))?;

            state.font.builder().position(pos_x + 10.0, pos_y + 5.0).shadow(true).with_symbols(Some(symbols)).draw(
                &text,
                ctx,
                &state.constants,
                &mut state.texture_set,
            )?;
        }

        if state.settings.debug_outlines {
            self.draw_debug_outlines(state, ctx)?;
        }

        if state.settings.god_mode {
            let debug_name = "GOD";
            state
                .font
                .builder()
                .x(state.canvas_size.0 - state.font.builder().compute_width(debug_name) - 10.0)
                .y(20.0)
                .shadow(true)
                .draw(debug_name, ctx, &state.constants, &mut state.texture_set)?;
        }

        if state.settings.infinite_booster {
            let debug_name = "INF.B";
            state
                .font
                .builder()
                .x(state.canvas_size.0 - state.font.builder().compute_width(debug_name) - 10.0)
                .y(32.0)
                .shadow(true)
                .draw(debug_name, ctx, &state.constants, &mut state.texture_set)?;
        }

        if state.settings.speed != 1.0 {
            let debug_name = &format!("{:.1}x SPD", state.settings.speed);
            state
                .font
                .builder()
                .x(state.canvas_size.0 - state.font.builder().compute_width(debug_name) - 10.0)
                .y(44.0)
                .shadow(true)
                .draw(debug_name, ctx, &state.constants, &mut state.texture_set)?;
        }

        if state.settings.noclip {
            let debug_name = "NOCLIP";
            state
                .font
                .builder()
                .x(state.canvas_size.0 - state.font.builder().compute_width(debug_name) - 10.0)
                .y(56.0)
                .shadow(true)
                .draw(debug_name, ctx, &state.constants, &mut state.texture_set)?;
        }

        self.replay.draw(state, ctx, frame)?;

        if state.network.as_ref().map_or(false, |session| session.waiting()) {
            state.font.builder().center(state.canvas_size.0).y(8.0).shadow(true).draw(
                "Catching up / reconnecting...",
                ctx,
                &state.constants,
                &mut state.texture_set,
            )?;
        }

        self.pause_menu.draw(state, ctx)?;
        self.network_chat.draw(state, ctx)?;
        self.network_menu.draw(state, ctx)?;

        //draw_number(state.canvas_size.0 - 8.0, 8.0, timer::fps(ctx) as usize, Alignment::Right, state, ctx)?;
        Ok(())
    }

    fn imgui_draw(
        &mut self,
        components: &mut Components,
        state: &mut SharedGameState,
        ctx: &mut Context,
        ui: &mut imgui::Ui,
    ) -> GameResult {
        if state.network.is_none() {
            components.live_debugger.run_ingame(self, state, ctx, ui)?;
        }
        Ok(())
    }

    fn process_debug_keys(&mut self, state: &mut SharedGameState, ctx: &mut Context, key_code: ScanCode) -> GameResult {
        if state.network.is_some() {
            if self.network_menu.process_key(ctx, key_code) {
                return Ok(());
            }
            self.network_chat.key(state, ctx, key_code);
            return Ok(());
        }
        #[cfg(not(debug_assertions))]
        if !state.settings.debug_mode {
            return Ok(());
        }

        if key_code == ScanCode::F3 && ctx.keyboard_context.active_mods().ctrl() {
            let _ = state.sound_manager.reload();
            return Ok(());
        }

        if key_code == ScanCode::S && ctx.keyboard_context.active_mods().ctrl() {
            let _ = state.save_game(self, ctx, None);
            state.sound_manager.play_sfx(18);
            return Ok(());
        }

        match key_code {
            ScanCode::F3 => state.settings.god_mode = !state.settings.god_mode,
            ScanCode::F4 => state.settings.infinite_booster = !state.settings.infinite_booster,
            ScanCode::F5 => state.settings.subpixel_coords = !state.settings.subpixel_coords,
            ScanCode::F6 => state.settings.motion_interpolation = !state.settings.motion_interpolation,
            ScanCode::F7 => state.set_speed(1.0),
            ScanCode::F8 => {
                if state.settings.speed > 0.2 {
                    state.set_speed(state.settings.speed - 0.1);
                }
            }
            ScanCode::F9 => {
                if state.settings.speed < 3.0 {
                    state.set_speed(state.settings.speed + 0.1);
                }
            }
            ScanCode::F10 => state.settings.debug_outlines = !state.settings.debug_outlines,
            ScanCode::F11 => state.settings.fps_counter = !state.settings.fps_counter,
            ScanCode::F12 => state.debugger = !state.debugger,
            ScanCode::Grave => {
                state.command_line = !state.command_line;

                if !state.command_line {
                    state.control_flags.set_tick_world(true);
                }
            }
            _ => {}
        };

        Ok(())
    }
}
