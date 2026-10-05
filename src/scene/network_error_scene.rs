use crate::framework::context::Context;
use crate::framework::error::GameResult;
use crate::game::shared_game_state::SharedGameState;
use crate::graphics::font::Font;
use crate::input::player_controller::PlayerController;
use crate::scene::{title_scene::TitleScene, Scene};

/// Keep network failures visible and recoverable, rather than aborting the game.
pub struct NetworkErrorScene {
    message: String,
    controller: Option<Box<dyn PlayerController>>,
}

impl NetworkErrorScene {
    pub fn new(message: String) -> Self {
        Self { message, controller: None }
    }
}

impl Scene for NetworkErrorScene {
    fn init(&mut self, state: &mut SharedGameState, ctx: &mut Context) -> GameResult {
        ctx.keyboard_context.native_text_input = false;
        ctx.keyboard_context.take_text_input();
        state.reload_resources(ctx)?;
        state.update_locale(ctx);
        self.controller = Some(state.settings.create_player1_controller());
        Ok(())
    }

    fn tick(&mut self, state: &mut SharedGameState, ctx: &mut Context) -> GameResult {
        if let Some(controller) = &mut self.controller {
            controller.update(state, ctx)?;
            controller.update_trigger();
            if controller.trigger_menu_ok() || controller.trigger_menu_back() || controller.trigger_menu_pause() {
                state.next_scene = Some(Box::new(TitleScene::new()));
            }
        }
        Ok(())
    }

    fn draw(&self, state: &mut SharedGameState, ctx: &mut Context) -> GameResult {
        state.font.builder().center(state.canvas_size.0).y(30.0).draw(
            "Network session ended",
            ctx,
            &state.constants,
            &mut state.texture_set,
        )?;
        let mut line = String::new();
        let mut y = 60.0;
        for word in self.message.split_whitespace() {
            let next = format!("{}{} ", line, word);
            if !line.is_empty() && state.font.builder().compute_width(&next) > state.canvas_size.0 - 24.0 {
                state.font.builder().center(state.canvas_size.0).y(y).draw(
                    &line,
                    ctx,
                    &state.constants,
                    &mut state.texture_set,
                )?;
                y += 16.0;
                line.clear();
            }
            line.push_str(word);
            line.push(' ');
        }
        state.font.builder().center(state.canvas_size.0).y(y).draw(
            &line,
            ctx,
            &state.constants,
            &mut state.texture_set,
        )?;
        state.font.builder().center(state.canvas_size.0).y(y + 32.0).draw(
            "Confirm / Back: return to title",
            ctx,
            &state.constants,
            &mut state.texture_set,
        )
    }
}
