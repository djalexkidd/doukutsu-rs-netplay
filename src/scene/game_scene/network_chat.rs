//! Local chat presentation and keyboard input, independent of simulation rollback.
use super::*;
use std::time::{Duration, Instant};

const MESSAGE_LIFETIME: Duration = Duration::from_secs(10);

#[derive(Default)]
pub(super) struct NetworkChat {
    cursor: usize,
    scroll: usize,
    outcome: Option<bool>,
    layout: RefCell<Option<Layout>>,
}
struct Row {
    text: String,
    death: bool,
    received: Option<Instant>,
}
struct Layout {
    key: (Option<Instant>, usize, u32),
    rows: Vec<Row>,
}

fn insert(value: &mut String, cursor: &mut usize, text: &str) {
    let remaining = 240usize.saturating_sub(value.chars().count());
    let text: String = text.chars().filter(|ch| !ch.is_control()).take(remaining).collect();
    value.insert_str(*cursor, &text);
    *cursor += text.len();
}
fn previous(value: &str, cursor: usize) -> usize {
    value[..cursor].char_indices().next_back().map_or(0, |(index, _)| index)
}
fn next(value: &str, cursor: usize) -> usize {
    cursor + value[cursor..].chars().next().map_or(0, char::len_utf8)
}
fn recent(received: Option<Instant>, now: Instant) -> bool {
    received.is_some_and(|time| now.saturating_duration_since(time) < MESSAGE_LIFETIME)
}
fn wrap(text: &str, width: f32, measure: impl Fn(char) -> f32) -> Vec<String> {
    let mut rows = Vec::new();
    let mut row = String::new();
    let mut length = 0.0;
    for ch in text.chars() {
        let advance = measure(ch);
        if !row.is_empty() && length + advance > width {
            rows.push(std::mem::take(&mut row));
            length = 0.0;
        }
        row.push(ch);
        length += advance;
    }
    if !row.is_empty() {
        rows.push(row);
    }
    rows
}
impl NetworkChat {
    fn take_text(&mut self, state: &mut SharedGameState, ctx: &mut Context) {
        let session = state.network.as_mut().unwrap();
        self.cursor = self.cursor.min(session.chat_draft.len());
        insert(&mut session.chat_draft, &mut self.cursor, &ctx.keyboard_context.take_text_input());
    }
    pub fn key(&mut self, state: &mut SharedGameState, ctx: &mut Context, key: ScanCode) {
        let session = state.network.as_mut().unwrap();
        if !session.chat_open {
            if matches!(key, ScanCode::Return | ScanCode::NumpadEnter)
                && !session.options_open
                && !ctx.keyboard_context.active_mods().alt()
            {
                session.chat_open = true;
                self.cursor = session.chat_draft.len();
                self.scroll = 0;
                ctx.keyboard_context.native_text_input = true;
                ctx.keyboard_context.take_text_input();
            }
            return;
        }
        self.take_text(state, ctx);
        let draft = &mut state.network.as_mut().unwrap().chat_draft;
        match key {
            ScanCode::Return | ScanCode::NumpadEnter => self.outcome = Some(true),
            ScanCode::Escape => self.outcome = Some(false),
            ScanCode::Backspace if self.cursor > 0 => {
                let start = previous(draft, self.cursor);
                draft.replace_range(start..self.cursor, "");
                self.cursor = start;
            }
            ScanCode::Delete if self.cursor < draft.len() => {
                draft.replace_range(self.cursor..next(draft, self.cursor), "");
            }
            ScanCode::Left => self.cursor = previous(draft, self.cursor),
            ScanCode::Right => self.cursor = next(draft, self.cursor),
            ScanCode::Home => self.cursor = 0,
            ScanCode::End => self.cursor = draft.len(),
            ScanCode::A if ctx.keyboard_context.active_mods().ctrl() => {
                draft.clear();
                self.cursor = 0;
            }
            ScanCode::PageUp => self.scroll = self.scroll.saturating_add(6),
            ScanCode::PageDown => self.scroll = self.scroll.saturating_sub(6),
            _ => (),
        }
    }
    pub fn tick(&mut self, state: &mut SharedGameState, ctx: &mut Context) -> GameResult {
        if !state.network.as_ref().unwrap().chat_open {
            return Ok(());
        }
        self.take_text(state, ctx);
        if let Some(send) = self.outcome.take() {
            let session = state.network.as_mut().unwrap();
            if send && !session.chat_draft.trim().is_empty() {
                let draft = session.chat_draft.clone();
                session.send_chat(&draft)?;
                session.chat_draft.clear();
                self.cursor = 0;
            }
            session.chat_open = false;
            ctx.keyboard_context.native_text_input = false;
            ctx.keyboard_context.take_text_input();
        }
        Ok(())
    }
    pub fn draw(&self, state: &mut SharedGameState, ctx: &mut Context) -> GameResult {
        let Some(session) = &state.network else { return Ok(()) };
        let width = (state.canvas_size.0 - 16.0).min(300.0).max(40.0);
        let key = (session.chat.back().and_then(|line| line.received_at), session.chat.len(), width.to_bits());
        let mut cache = self.layout.borrow_mut();
        if cache.as_ref().map(|layout| layout.key) != Some(key) {
            let mut rows = Vec::new();
            for message in &session.chat {
                let text = format!("{}: {}", message.author, message.text);
                for text in wrap(&text, width - 8.0, |ch| state.font.builder().compute_width(&ch.to_string())) {
                    rows.push(Row { text, death: message.death, received: message.received_at });
                }
            }
            *cache = Some(Layout { key, rows });
        }
        let layout = cache.as_ref().unwrap();
        let now = Instant::now();
        let rows: Vec<_> = layout.rows.iter().filter(|row| session.chat_open || recent(row.received, now)).collect();
        let line_height = state.font.line_height() + 2.0;
        let capacity = if session.chat_open { 10 } else { 5 };
        let capacity = capacity.min(((state.canvas_size.1 - 32.0) / line_height).max(1.0) as usize);
        let scroll = if session.chat_open { self.scroll.min(rows.len().saturating_sub(capacity)) } else { 0 };
        let end = rows.len().saturating_sub(scroll);
        let start = end.saturating_sub(capacity);
        if end == start && !session.chat_open {
            return Ok(());
        }
        let bottom = state.canvas_size.1 - 8.0;
        let input_height = if session.chat_open { line_height } else { 0.0 };
        let top = bottom - input_height - (end - start) as f32 * line_height;
        graphics::draw_rect(
            ctx,
            Rect::new(
                (4.0 * state.scale) as isize,
                ((top - 3.0) * state.scale) as isize,
                ((width + 12.0) * state.scale) as isize,
                ((bottom + 3.0) * state.scale) as isize,
            ),
            Color::new(0.0, 0.0, 0.0, 0.65),
        )?;
        for (index, row) in rows[start..end].iter().enumerate() {
            state
                .font
                .builder()
                .position(8.0, top + index as f32 * line_height)
                .color(if row.death { (255, 80, 80, 255) } else { (255, 255, 255, 255) })
                .shadow(true)
                .draw(&row.text, ctx, &state.constants, &mut state.texture_set)?;
        }
        if session.chat_open {
            let draft = &session.chat_draft;
            let cursor = self.cursor.min(draft.len());
            let mut offset = 0;
            while offset < cursor && state.font.builder().compute_width(&draft[offset..cursor]) > width - 20.0 {
                offset = next(draft, offset);
            }
            let mut end = cursor;
            while end < draft.len() {
                let following = next(draft, end);
                if state.font.builder().compute_width(&draft[offset..following]) > width - 20.0 {
                    break;
                }
                end = following;
            }
            let text = format!("> {}", &draft[offset..end]);
            let caret = state.font.builder().compute_width(&format!("> {}", &draft[offset..cursor]));
            let y = bottom - line_height;
            state.font.builder().position(8.0, y).color((255, 255, 160, 255)).shadow(true).draw(
                &text,
                ctx,
                &state.constants,
                &mut state.texture_set,
            )?;
            state.font.builder().position(8.0 + caret, y).color((255, 255, 160, 255)).draw(
                "|",
                ctx,
                &state.constants,
                &mut state.texture_set,
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn expiration_keeps_old_messages_available_only_in_history() {
        let now = Instant::now();
        assert!(recent(Some(now - Duration::from_secs(9)), now));
        assert!(!recent(Some(now - MESSAGE_LIFETIME), now));
        assert!(!recent(None, now)); // Welcome history never flashes as new messages.
    }
    #[test]
    fn typing_preserves_unicode_boundaries_and_limits_messages() {
        let mut value = "été".to_owned();
        let mut cursor = previous(&value, value.len());
        insert(&mut value, &mut cursor, "東京\n");
        assert_eq!(value, "ét東京é");
        assert_eq!(next(&value, previous(&value, cursor)), cursor);
        insert(&mut value, &mut cursor, &"é".repeat(300));
        assert_eq!(value.chars().count(), 240);
    }
    #[test]
    fn long_messages_wrap_without_dropping_unicode_characters() {
        let rows = wrap("été東京", 2.0, |_| 1.0);
        assert_eq!(rows, ["ét", "é東", "京"]);
    }
}
