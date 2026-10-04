use crate::game::inventory::Inventory;
use crate::game::player::Player;

#[derive(Clone)]
pub struct RemotePlayer {
    pub player: Player,
    pub inventory: Inventory,
    pub hud: crate::components::hud::HUD,
}
