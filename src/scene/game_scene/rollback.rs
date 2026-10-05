//! In-memory simulation snapshots. GPU resources, sockets and persistent preferences stay live.
use super::*;
use crate::game::network::{Input, MAX_PLAYERS};
use crate::input::replay_player_controller::ReplayController;
use std::collections::VecDeque;

pub const INPUT_LEAD: u64 = 8; // 160 ms at 50 Hz or about 133 ms at 60 Hz.
pub const MAX_PREDICTION: usize = 20; // Bound replay work and memory during a stalled connection.

macro_rules! snapshot {
    ($name:ident, $owner:ty, {$($field:ident: $ty:ty),* $(,)?}) => {
        pub struct $name { $($field: $ty),* }
        impl $name {
            fn capture(owner: &$owner) -> Self { Self { $($field: owner.$field.clone()),* } }
            fn restore(&self, owner: &mut $owner) { $(owner.$field = self.$field.clone();)* }
        }
    };
}

snapshot!(SceneSnapshot, GameScene, {
    tick: u32, stage: Stage, water_renderer: WaterRenderer, boss_life_bar: BossLifeBar,
    stage_select: StageSelect, flash: Flash, inventory_ui: InventoryUI,
    hud_player1: HUD, hud_player2: HUD, nikumaru: NikumaruCounter,
    whimsical_star: WhimsicalStar, background: Background, tilemap: Tilemap, text_boxes: TextBoxes,
    network_inventories: [Option<network_inventory::NetworkInventory>; MAX_PLAYERS],
    frame: Frame, network_cameras: [Frame; MAX_PLAYERS], player1: Player, player2: Player,
    inventory_player1: Inventory, inventory_player2: Inventory,
    remote_players: Vec<crate::game::player::player_list::RemotePlayer>,
    player_generations: [u32; MAX_PLAYERS], network_game_over: bool,
    npc_list: NPCList, boss: BossNPC, bullet_manager: BulletManager,
    map_name_counter: u16, skip_counter: u16, inventory_dim: f32,
});

snapshot!(SharedSnapshot, SharedGameState, {
    control_flags: crate::common::ControlFlags,
    game_flags: crate::util::bitvec::BitVec, skip_flags: crate::util::bitvec::BitVec,
    map_flags: crate::util::bitvec::BitVec, fade_state: crate::common::FadeState,
    game_rng: crate::util::rng::XorShift, effect_rng: crate::util::rng::XorShift,
    quake_counter: u16, super_quake_counter: u16, quake_rumble_counter: u32, super_quake_rumble_counter: u32,
    teleporter_slots: Vec<(u16, u16)>, carets: Vec<crate::game::caret::Caret>,
    npc_super_pos: (i32, i32), npc_curly_target: (i32, i32), npc_curly_counter: u16, water_level: i32,
    textscript_vm: TextScriptVM, difficulty: crate::game::shared_game_state::GameDifficulty,
    player_count: PlayerCount, player_count_modified_in_game: bool, tutorial_counter: u16,
});

pub struct Snapshot {
    scene: SceneSnapshot,
    shared: SharedSnapshot,
    controllers: [ReplayController; MAX_PLAYERS],
    map: ((u16, u16), u16, crate::components::map_system::MapSystemState),
}
impl Snapshot {
    pub fn capture(scene: &GameScene, state: &SharedGameState) -> Self {
        Self {
            scene: SceneSnapshot::capture(scene),
            shared: SharedSnapshot::capture(state),
            controllers: state.network.as_ref().unwrap().controllers,
            map: scene.map_system.rollback_state(),
        }
    }
    pub fn bullets(&self) -> &BulletManager {
        &self.scene.bullet_manager
    }
    pub fn restore(&self, scene: &mut GameScene, state: &mut SharedGameState) {
        self.scene.restore(scene);
        self.shared.restore(state);
        scene.map_system.restore_rollback_state(self.map);
        state.network.as_mut().unwrap().controllers = self.controllers;
    }
}

#[derive(Default)]
pub struct Prediction {
    pub confirmed: Option<Box<Snapshot>>,
    pub inputs: VecDeque<(u64, Input)>,
    pub rollbacks: u64,
}
impl Prediction {
    pub fn confirm(&mut self, sequence: u64) {
        while self.inputs.front().is_some_and(|(frame, _)| *frame <= sequence) {
            self.inputs.pop_front();
        }
    }
    pub fn target(&self, sequence: u64) -> u64 {
        self.inputs.back().map_or(sequence + INPUT_LEAD, |(frame, _)| frame + 1).max(sequence + INPUT_LEAD)
    }
}

/// Local shots stay responsive on the predicted timeline. Remote shots use the
/// confirmed timeline: short-lived shots may have already expired by the time
/// the guest finishes replaying its prediction lead. This only affects drawing,
/// including bullet lighting; collisions and damage still use the predicted world.
pub fn visible_bullets<'a>(
    predicted: &'a BulletManager,
    confirmed: Option<&'a BulletManager>,
    local_slot: usize,
) -> impl Iterator<Item = &'a crate::game::weapon::bullet::Bullet> {
    predicted.bullets.iter().filter(move |bullet| confirmed.is_none() || bullet.owner.index() == local_slot).chain(
        confirmed
            .into_iter()
            .flat_map(|manager| manager.bullets.iter())
            .filter(move |bullet| bullet.owner.index() != local_slot),
    )
}

/// Interpolate a correction from the image the player actually saw, without changing simulation positions.
pub struct Presentation {
    players: [(i32, i32, bool, bool, u32); MAX_PLAYERS],
    cameras: [(i32, i32); MAX_PLAYERS],
    frame: (i32, i32),
}
impl Presentation {
    pub fn capture(scene: &GameScene) -> Self {
        Self {
            players: std::array::from_fn(|slot| {
                let p = scene.player_at(slot);
                (p.x, p.y, p.cond.alive(), p.bubble, scene.player_generations[slot])
            }),
            cameras: std::array::from_fn(|slot| (scene.network_cameras[slot].x, scene.network_cameras[slot].y)),
            frame: (scene.frame.x, scene.frame.y),
        }
    }
    pub fn interpolate(&self, scene: &mut GameScene) {
        fn nearby(a: (i32, i32), b: (i32, i32)) -> bool {
            (a.0 as i64 - b.0 as i64).abs() < 32 * 0x200 && (a.1 as i64 - b.1 as i64).abs() < 32 * 0x200
        }
        for slot in 0..MAX_PLAYERS {
            let (x, y, alive, bubble, generation) = self.players[slot];
            let same_player = scene.player_generations[slot] == generation;
            let p = scene.player_at_mut(slot);
            if same_player && p.cond.alive() == alive && p.bubble == bubble && nearby((x, y), (p.x, p.y)) {
                p.prev_x = x;
                p.prev_y = y;
            }
            let camera = &mut scene.network_cameras[slot];
            if nearby(self.cameras[slot], (camera.x, camera.y)) {
                (camera.prev_x, camera.prev_y) = self.cameras[slot];
            }
        }
        if nearby(self.frame, (scene.frame.x, scene.frame.y)) {
            (scene.frame.prev_x, scene.frame.prev_y) = self.frame;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirmations_remove_only_authoritative_inputs_and_bound_the_lead() {
        let mut prediction = Prediction::default();
        assert_eq!(prediction.target(100), 108);
        for sequence in 100..110 {
            prediction.inputs.push_back((sequence, Input::neutral()));
        }
        prediction.confirm(103);
        assert_eq!(prediction.inputs.front().unwrap().0, 104);
        assert_eq!(prediction.target(104), 112);
        prediction.confirm(109);
        assert!(prediction.inputs.is_empty());
        assert_eq!(prediction.target(110), 118);
    }

    #[test]
    fn remote_short_lived_shots_remain_visible_without_duplicating_local_shots() {
        use crate::engine_constants::EngineConstants;
        use crate::game::weapon::bullet::Bullet;
        let constants = EngineConstants::defaults();
        let mut confirmed = BulletManager::new();
        let mut predicted = BulletManager::new();
        let remote = Bullet::new(0, 0, 4, TargetPlayer::Player1, Direction::Right, &constants);
        assert!(remote.lifetime <= INPUT_LEAD as u16);
        confirmed.bullets.push(remote);
        confirmed.bullets.push(Bullet::new(100, 0, 4, TargetPlayer::Player2, Direction::Left, &constants));
        // The remote shot expired during replay; the local shot has moved ahead.
        predicted.bullets.push(Bullet::new(200, 0, 4, TargetPlayer::Player2, Direction::Left, &constants));
        let visible: Vec<_> = visible_bullets(&predicted, Some(&confirmed), 1).collect();
        assert_eq!(visible.len(), 2);
        assert_eq!(visible[0].x, 200);
        assert_eq!(visible[1].owner, TargetPlayer::Player1);
        assert_eq!(predicted.bullets.len(), 1); // Drawing never resurrects gameplay bullets.
        assert_eq!(visible_bullets(&predicted, None, 1).count(), 1); // Solo/host/no snapshot.

        // A remote shot present in both worlds is drawn just once.
        predicted.bullets.push(Bullet::new(777, 0, 4, TargetPlayer::Player1, Direction::Right, &constants));
        let visible: Vec<_> = visible_bullets(&predicted, Some(&confirmed), 1).collect();
        assert_eq!(visible.len(), 2);
        assert_eq!(visible[1].x, 0);
        assert_eq!(visible_bullets(&predicted, None, 0).count(), 2);
    }

    #[test]
    fn snapshots_copy_dead_npc_slots_and_random_generators_independently() {
        let (mut list, mut token) = NPCList::new();
        list.set_rng_seed(42);
        let mut npc = NPC::empty();
        npc.cond.set_alive(true);
        npc.x = 123;
        list.spawn(0, npc).unwrap();
        let saved = list.clone();
        let original_rng = saved.get_npc(0).unwrap().borrow_unmanaged().rng.dump_state();
        {
            let mut npc = list.get_npc(0).unwrap().borrow_mut(&mut token);
            npc.cond.set_alive(false);
            npc.x = 999;
            npc.rng.next();
        }
        let restored = saved.clone();
        let npc = restored.get_npc(0).unwrap().borrow_unmanaged();
        assert_eq!(npc.x, 123);
        assert!(npc.cond.alive());
        assert_eq!(npc.rng.dump_state(), original_rng);
        assert!(!restored.get_npc(511).unwrap().borrow_unmanaged().cond.alive());
    }
}
