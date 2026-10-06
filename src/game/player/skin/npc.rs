//! Cosmetic NPC appearances. Gameplay hitboxes remain those of the regular player.
use super::PlayerAnimationState;
use crate::common::{Direction, Rect};
use crate::engine_constants::EngineConstants;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde_derive::Serialize, serde_derive::Deserialize)]
pub enum NpcAppearance {
    Sue,
    Toroko,
    King,
    Jack,
    Santa,
    Chaco,
    Kazuma,
    Booster,
}

impl NpcAppearance {
    pub const ALL: [Self; 8] =
        [Self::Sue, Self::Toroko, Self::King, Self::Jack, Self::Santa, Self::Chaco, Self::Kazuma, Self::Booster];

    pub fn name(self) -> &'static str {
        match self {
            Self::Sue => "Sue",
            Self::Toroko => "Toroko",
            Self::King => "King",
            Self::Jack => "Jack",
            Self::Santa => "Santa",
            Self::Chaco => "Chaco",
            Self::Kazuma => "Kazuma",
            Self::Booster => "Professor Booster",
        }
    }

    pub fn texture(self) -> &'static str {
        match self {
            Self::Jack | Self::Chaco => "Npc/NpcGuest",
            _ => "Npc/NpcRegu",
        }
    }

    pub fn frames(self, constants: &EngineConstants) -> [Rect<u16>; 8] {
        // Idle and three walking poses for each facing direction, from the NPC's own atlas.
        let (frames, stride, walk): (&[Rect<u16>], usize, [usize; 3]) = match self {
            Self::Sue => (&constants.npc.n042_sue.0, 13, [2, 3, 4]),
            Self::Toroko => (&constants.npc.n060_toroko.0, 8, [2, 3, 4]),
            Self::King => (&constants.npc.n061_king.0, 11, [4, 5, 6]),
            Self::Jack => (&constants.npc.n074_jack.0, 6, [2, 3, 4]),
            Self::Santa => (&constants.npc.n040_santa.0, 7, [2, 3, 4]),
            Self::Chaco => (&constants.npc.n093_chaco.0, 7, [2, 3, 4]),
            Self::Kazuma => (&constants.npc.n055_kazuma.0, 6, [1, 2, 3]),
            Self::Booster => (&constants.npc.n113_professor_booster.0, 7, [2, 3, 4]),
        };
        [
            frames[0],
            frames[walk[0]],
            frames[walk[1]],
            frames[walk[2]],
            frames[stride],
            frames[stride + walk[0]],
            frames[stride + walk[1]],
            frames[stride + walk[2]],
        ]
    }
}

pub fn animation_frame(
    frames: &[Rect<u16>; 8],
    state: PlayerAnimationState,
    direction: Direction,
    tick: u16,
) -> Rect<u16> {
    let pose = match state {
        PlayerAnimationState::Walking | PlayerAnimationState::WalkingUp => [1, 2, 3, 2][(tick as usize / 5) % 4],
        PlayerAnimationState::Jumping | PlayerAnimationState::FallingLookingUp => 1,
        PlayerAnimationState::Falling | PlayerAnimationState::FallingLookingDown => 3,
        _ => 0,
    };
    frames[pose + if direction == Direction::Left { 0 } else { 4 }]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds(rect: Rect<u16>) -> (u16, u16, u16, u16) {
        (rect.left, rect.top, rect.right, rect.bottom)
    }

    #[test]
    fn npc_animation_frames_fit_their_own_atlas_in_both_directions() {
        let constants = EngineConstants::defaults();
        for npc in NpcAppearance::ALL {
            let frames = npc.frames(&constants);
            let size = constants.tex_sizes.get(npc.texture()).unwrap();
            for frame in frames.iter() {
                assert_eq!(frame.right - frame.left, 16);
                assert!(frame.bottom > frame.top);
                assert!(frame.right <= size.0 && frame.bottom <= size.1);
            }
            assert_ne!(bounds(frames[0]), bounds(frames[4]));
            for direction in [Direction::Left, Direction::Right].iter().copied() {
                for state in [
                    PlayerAnimationState::Idle,
                    PlayerAnimationState::Walking,
                    PlayerAnimationState::WalkingUp,
                    PlayerAnimationState::LookingUp,
                    PlayerAnimationState::Examining,
                    PlayerAnimationState::Sitting,
                    PlayerAnimationState::Collapsed,
                    PlayerAnimationState::Jumping,
                    PlayerAnimationState::Falling,
                    PlayerAnimationState::FallingLookingUp,
                    PlayerAnimationState::FallingLookingDown,
                    PlayerAnimationState::FallingUpsideDown,
                    PlayerAnimationState::Drowned,
                ]
                .iter()
                .copied()
                {
                    for tick in 0..40 {
                        let frame = animation_frame(&frames, state, direction, tick);
                        assert!(frames[if direction == Direction::Left { 0..4 } else { 4..8 }]
                            .iter()
                            .any(|candidate| bounds(*candidate) == bounds(frame)));
                    }
                }
            }
        }
    }
}
