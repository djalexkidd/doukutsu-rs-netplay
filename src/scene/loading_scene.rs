use crate::framework::context::Context;
use crate::framework::error::GameResult;
use crate::framework::graphics;
use crate::game::shared_game_state::SharedGameState;
use crate::scene::no_data_scene::NoDataScene;
use crate::scene::Scene;

pub struct LoadingScene {
    tick: usize,
    loaded: bool,
}

impl LoadingScene {
    pub fn new() -> Self {
        Self { tick: 0, loaded: false }
    }

    fn load_stuff(&mut self, state: &mut SharedGameState, ctx: &mut Context) -> GameResult {
        if !self.loaded {
            state.reload_resources(ctx)?;
            self.loaded = true;
        }

        if let Some(mut session) = state.network.take() {
            let result = session.bootstrap(state, ctx);
            state.network = Some(session);
            if !result? {
                return Ok(());
            }
            state.reload_resources(ctx)?;
            state.update_locale(ctx);
            state.prepare_network_replay();
            state.load_or_start_game(ctx)?;
        } else if ctx.headless {
            log::info!("Headless mode detected, skipping intro and loading last saved game.");
            state.load_or_start_game(ctx)?;
        } else {
            state.start_intro(ctx)?;
        }

        Ok(())
    }
}

impl Scene for LoadingScene {
    fn tick(&mut self, state: &mut SharedGameState, ctx: &mut Context) -> GameResult {
        // deferred to let the loading image draw
        if self.tick >= 1 && (self.tick == 1 || state.network.is_some()) {
            let network = state.network.is_some();
            if let Err(err) = self.load_stuff(state, ctx) {
                log::error!("Failed to load game data: {}", err);

                state.end_network_session();
                state.next_scene = if network {
                    Some(Box::new(crate::scene::network_error_scene::NetworkErrorScene::new(err.to_string())))
                } else {
                    Some(Box::new(NoDataScene::new(err)))
                };
            }
        }

        self.tick += 1;
        Ok(())
    }

    fn draw(&self, state: &mut SharedGameState, ctx: &mut Context) -> GameResult {
        graphics::set_vsync_mode(ctx, state.settings.vsync_mode)?;

        match state.texture_set.get_or_load_batch(ctx, &state.constants, "Loading") {
            Ok(batch) => {
                batch.add(
                    ((state.canvas_size.0 - batch.width() as f32) / 2.0).floor(),
                    ((state.canvas_size.1 - batch.height() as f32) / 2.0).floor(),
                );
                batch.draw(ctx)?;
            }
            Err(err) => {
                log::error!("Failed to load game data: {}", err);

                state.next_scene = Some(Box::new(NoDataScene::new(err)));
            }
        }

        Ok(())
    }
}
