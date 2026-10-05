use crate::common::{Color, Rect};
use crate::framework::{context::Context, error::GameResult, graphics, keyboard::ScanCode};
use crate::game::shared_game_state::SharedGameState;
use crate::graphics::font::Font;
use crate::input::combined_menu_controller::CombinedMenuController;
use crate::menu::{Menu, MenuEntry};

const KEYS: &str = "1234567890abcdefghijklmnopqrstuvwxyz-_.:[]@!?+/=éè";

pub struct TextEditor {
    pub value: String,
    title: String,
    limit: usize,
    selected: usize,
    upper: bool,
    outcome: Option<bool>,
    keyboard_used: bool,
    panel: Menu<usize>,
}
impl TextEditor {
    pub fn new(title: String, value: String, limit: usize) -> Self {
        let mut panel = Menu::new(0, 0, 272, 194);
        panel.draw_cursor = false;
        panel.push_entry(0, MenuEntry::Spacer(194.0));
        Self { value, title, limit, selected: 0, upper: false, outcome: None, keyboard_used: false, panel }
    }
    fn append(&mut self, text: &str) {
        for c in text.chars().filter(|c| !c.is_control()) {
            if self.value.chars().count() == self.limit {
                break;
            }
            self.value.push(c);
        }
    }
    pub fn key(&mut self, key: ScanCode, ctrl: bool) {
        match key {
            ScanCode::Backspace => {
                self.value.pop();
                self.keyboard_used = true;
            }
            ScanCode::A if ctrl => {
                self.value.clear();
                self.keyboard_used = true;
            }
            ScanCode::Return => self.outcome = Some(true),
            ScanCode::Escape => self.outcome = Some(false),
            _ => (),
        }
    }
    pub fn tick(
        &mut self,
        controller: &CombinedMenuController,
        state: &mut SharedGameState,
        ctx: &mut Context,
    ) -> Option<bool> {
        self.panel.x = ((state.canvas_size.0 - 272.0) / 2.0) as isize;
        self.panel.y = ((state.canvas_size.1 - 194.0) / 2.0) as isize;
        let text = ctx.keyboard_context.take_text_input();
        let typed = self.keyboard_used || !text.is_empty();
        self.keyboard_used = false;
        self.append(&text);
        if let Some(outcome) = self.outcome.take() {
            return Some(outcome);
        }
        if typed {
            return None;
        }
        if controller.trigger_back() {
            return Some(false);
        }
        let count = KEYS.chars().count();
        let rows = (count + 9) / 10;
        let row = self.selected / 10;
        let col = self.selected % 10;
        if controller.trigger_up() {
            self.selected = if row == 0 { rows * 10 + (col / 2) * 2 } else { (row - 1) * 10 + col };
        }
        if controller.trigger_down() {
            self.selected = if row == rows {
                col
            } else if row + 1 == rows {
                rows * 10 + (col / 2) * 2
            } else {
                (row + 1) * 10 + col
            };
        }
        if controller.trigger_left() {
            self.selected = row * 10 + (col + if row == rows { 8 } else { 9 }) % 10;
        }
        if controller.trigger_right() {
            self.selected = row * 10 + (col + if row == rows { 2 } else { 1 }) % 10;
        }
        if self.selected < rows * 10 && self.selected >= count {
            self.selected = count - 1;
        }
        if controller.trigger_ok() {
            if self.selected < count {
                let mut c = KEYS.chars().nth(self.selected).unwrap();
                if self.upper {
                    c = c.to_uppercase().next().unwrap_or(c);
                }
                self.append(&c.to_string());
            } else {
                match (self.selected - rows * 10) / 2 {
                    0 => self.upper = !self.upper,
                    1 => {
                        self.value.pop();
                    }
                    2 => self.value.clear(),
                    3 => return Some(true),
                    _ => return Some(false),
                }
            }
            state.sound_manager.play_sfx(18);
        }
        // Shoulder buttons insert a space and erase, without leaving the keyboard grid.
        if controller.trigger_shift_left() {
            self.value.pop();
        }
        if controller.trigger_shift_right() {
            self.append(" ");
        }
        None
    }
    pub fn draw(&self, state: &mut SharedGameState, ctx: &mut Context) -> GameResult {
        self.panel.draw(state, ctx)?;
        let x = self.panel.x as f32 + 10.0;
        let y = self.panel.y as f32 + 6.0;
        let mut lines = vec![(self.title.clone(), x, y)];
        let tail: String = self.value.chars().rev().take(60).collect::<Vec<_>>().into_iter().rev().collect();
        let chars: Vec<_> = tail.chars().collect();
        for (line, chunk) in chars.chunks(30).enumerate() {
            lines.push((chunk.iter().collect(), x, y + 18.0 + line as f32 * 14.0));
        }
        let keys: Vec<_> = KEYS.chars().collect();
        for (i, c) in keys.iter().enumerate() {
            let text = if self.upper { c.to_uppercase().collect() } else { c.to_string() };
            let px = x + (i % 10) as f32 * 25.0;
            let py = y + 58.0 + (i / 10) as f32 * 18.0;
            if i == self.selected {
                graphics::draw_rect(
                    ctx,
                    Rect::new(px as isize - 3, py as isize - 3, px as isize + 15, py as isize + 13),
                    Color::from_rgb(64, 88, 144),
                )?;
            }
            lines.push((text, px, py));
        }
        let row = (keys.len() + 9) / 10;
        for (i, text) in ["Aa", "Del", "Clear", "OK", "Back"].iter().enumerate() {
            let px = x + i as f32 * 50.0;
            let py = y + 58.0 + row as f32 * 18.0;
            if self.selected / 10 == row && self.selected % 10 / 2 == i {
                graphics::draw_rect(
                    ctx,
                    Rect::new(px as isize - 3, py as isize - 3, px as isize + 43, py as isize + 13),
                    Color::from_rgb(64, 88, 144),
                )?;
            }
            lines.push((text.to_string(), px, py));
        }
        lines.push(("LB: delete    RB: space".into(), x, self.panel.y as f32 + 182.0));
        for (text, x, y) in lines {
            state.font.builder().position(x, y).draw(&text, ctx, &state.constants, &mut state.texture_set)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn text_editing_preserves_unicode_and_limits_characters() {
        let mut editor = TextEditor::new("Name".into(), "猫".into(), 3);
        editor.append("é\nabc");
        assert_eq!(editor.value, "猫éa");
        editor.key(ScanCode::Backspace, false);
        assert_eq!(editor.value, "猫é");
        editor.key(ScanCode::A, true);
        assert!(editor.value.is_empty());
    }
}
