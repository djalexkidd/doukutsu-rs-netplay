//! Per-player inventory scripts run beside the shared world, using the original UI.
use super::*;
use crate::common::ControlFlags;
use crate::game::network::MAX_PLAYERS;
use crate::input::replay_player_controller::ReplayController;

#[derive(Clone)]
pub(super) struct NetworkInventory {
    pub ui: InventoryUI,
    pub vm: TextScriptVM,
    boxes: TextBoxes,
    flags: ControlFlags,
    fresh: bool,
}
impl NetworkInventory {
    pub fn new(state: &SharedGameState) -> Self {
        let mut vm = state.textscript_vm.clone();
        vm.set_mode(ScriptMode::Inventory);
        vm.stack.clear();
        vm.numbers = [0; 4];
        vm.suspend = false;
        vm.executor_player = TargetPlayer::Player1;
        Self { ui: InventoryUI::new(), vm, boxes: TextBoxes::new(), flags: state.control_flags, fresh: true }
    }
    pub fn checksum(&self) -> String {
        format!(
            "{:?}:{:?}:{:?}:{}:{}:{}:{:?}:{:?}:{:?}:{:?}",
            self.ui,
            self.vm.state,
            self.vm.stack,
            self.vm.flags.0,
            self.flags.0,
            self.fresh,
            self.vm.numbers,
            self.vm.line_1,
            self.vm.line_2,
            self.vm.line_3
        )
    }
    fn draw(&self, state: &mut SharedGameState, ctx: &mut Context, frame: &Frame) -> GameResult {
        let shared_vm = std::mem::replace(&mut state.textscript_vm, self.vm.clone());
        let result = (|| {
            self.ui.draw(state, ctx, frame)?;
            self.boxes.draw(state, ctx, frame)
        })();
        state.textscript_vm = shared_vm;
        result
    }
}
impl GameScene {
    fn swap_inventory_actor(&mut self, slot: usize) {
        match slot {
            0 => (),
            1 => {
                std::mem::swap(&mut self.player1, &mut self.player2);
                std::mem::swap(&mut self.inventory_player1, &mut self.inventory_player2);
                std::mem::swap(&mut self.hud_player1, &mut self.hud_player2);
            }
            _ => {
                let remote = &mut self.remote_players[slot - 2];
                std::mem::swap(&mut self.player1, &mut remote.player);
                std::mem::swap(&mut self.inventory_player1, &mut remote.inventory);
                std::mem::swap(&mut self.hud_player1, &mut remote.hud);
            }
        }
    }
    pub(super) fn tick_network_inventories(&mut self, state: &mut SharedGameState, ctx: &mut Context) -> GameResult {
        if state.network.is_none() {
            return Ok(());
        }
        if self.network_game_over
            || state.textscript_vm.mode != ScriptMode::Map
            || !state.control_flags.control_enabled()
        {
            self.network_inventories.fill(None);
            return Ok(());
        }
        for slot in 0..MAX_PLAYERS {
            let Some(mut inventory) = self.network_inventories[slot].take() else { continue };
            if state.network.as_ref().unwrap().applied_members[slot].is_none() || !self.player_at(slot).cond.alive() {
                continue;
            }
            let input = if inventory.fresh {
                ReplayController::new()
            } else {
                state.network.as_ref().unwrap().controllers[slot]
            };
            inventory.fresh = false;
            self.swap_inventory_actor(slot);
            // Legacy inventory actions target player 1 and sometimes both local players.
            // Scope those changes and dialogue input to the actor, preserving the other players.
            let partner = (self.player2.clone(), self.inventory_player2.clone(), self.hud_player2.clone());
            let remotes = self.remote_players.clone();
            for index in 1..MAX_PLAYERS {
                self.player_at_mut(index).controller = Box::new(ReplayController::new());
            }
            let actor_controller = std::mem::replace(&mut self.player1.controller, Box::new(input));
            std::mem::swap(&mut state.textscript_vm, &mut inventory.vm);
            std::mem::swap(&mut state.control_flags, &mut inventory.flags);
            std::mem::swap(&mut self.text_boxes, &mut inventory.boxes);
            let result = (|| {
                inventory
                    .ui
                    .tick(state, (ctx, &mut self.player1, &mut self.inventory_player1, &mut self.hud_player1))?;
                TextScriptVM::run(state, self, ctx)?;
                self.text_boxes.tick(state, ())
            })();
            std::mem::swap(&mut state.textscript_vm, &mut inventory.vm);
            std::mem::swap(&mut state.control_flags, &mut inventory.flags);
            std::mem::swap(&mut self.text_boxes, &mut inventory.boxes);
            self.player1.controller = actor_controller;
            (self.player2, self.inventory_player2, self.hud_player2) = partner;
            self.remote_players = remotes;
            self.swap_inventory_actor(slot);
            // An item that changes stage must carry its transition script to the new scene.
            if state.next_scene.is_some() {
                state.textscript_vm = inventory.vm;
                state.textscript_vm.executor_player = TargetPlayer::from_index(slot);
                state.control_flags = inventory.flags;
                result?;
                return Ok(());
            }
            if inventory.vm.mode == ScriptMode::Inventory {
                self.network_inventories[slot] = Some(inventory);
            }
            result?;
        }
        Ok(())
    }
    pub(super) fn draw_network_inventory(
        &self,
        state: &mut SharedGameState,
        ctx: &mut Context,
        frame: &Frame,
    ) -> GameResult {
        if let Some(session) = &state.network {
            if let Some(inventory) = &self.network_inventories[session.local_slot] {
                let mut color = state.constants.inventory_dim_color;
                color.a *= 0.8;
                graphics::draw_rect(
                    ctx,
                    Rect::new(0, 0, state.screen_size.0 as isize + 1, state.screen_size.1 as isize + 1),
                    color,
                )?;
                inventory.draw(state, ctx, frame)?;
            }
        }
        Ok(())
    }
}
