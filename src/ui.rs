use crate::room::{Room, RoomMessage, RoomSpeaker};
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    MouseEventKind,
};
use crossterm::execute;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use std::io;

pub struct TerminalUi {
    terminal: DefaultTerminal,
}

impl TerminalUi {
    pub fn start() -> io::Result<Self> {
        match ratatui::try_init() {
            Ok(terminal) => {
                if let Err(error) = execute!(io::stdout(), EnableMouseCapture) {
                    let _ = ratatui::try_restore();
                    return Err(error);
                }
                Ok(Self { terminal })
            }
            Err(error) => {
                let _ = ratatui::try_restore();
                Err(error)
            }
        }
    }

    pub fn draw(&mut self, state: &mut RoomUi) -> io::Result<()> {
        self.terminal.draw(|frame| render(frame, state))?;
        Ok(())
    }
}

impl Drop for TerminalUi {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), DisableMouseCapture);
        ratatui::restore();
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum InputAction {
    None,
    Submit(String),
    Exit,
}

pub struct RoomUi {
    room_id: String,
    description: String,
    participants: Vec<String>,
    messages: Vec<RoomMessage>,
    input: String,
    cursor: usize,
    completion: Option<CompletionMenu>,
    live_response: Option<LiveResponse>,
    status: String,
    status_is_error: bool,
    scroll: u16,
    maximum_scroll: u16,
    viewport_height: u16,
    follow_output: bool,
}

struct LiveResponse {
    agent: String,
    content: String,
}

struct CompletionMenu {
    start: usize,
    end: usize,
    candidates: Vec<String>,
    selected: usize,
}

impl RoomUi {
    pub fn new(room: &Room, messages: Vec<RoomMessage>) -> Self {
        Self {
            room_id: room.id().to_string(),
            description: room.description().to_owned(),
            participants: room
                .participants()
                .iter()
                .map(|participant| participant.agent_name().to_owned())
                .collect(),
            messages,
            input: String::new(),
            cursor: 0,
            completion: None,
            live_response: None,
            status: "Ready".to_owned(),
            status_is_error: false,
            scroll: 0,
            maximum_scroll: 0,
            viewport_height: 1,
            follow_output: true,
        }
    }

    pub fn handle_event(&mut self, event: Event) -> InputAction {
        match event {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                self.handle_key(key)
            }
            Event::Paste(text) => {
                self.insert(&text.replace(['\r', '\n'], " "));
                InputAction::None
            }
            Event::Mouse(mouse) => {
                self.handle_mouse(mouse.kind);
                InputAction::None
            }
            _ => InputAction::None,
        }
    }

    pub fn handle_passive_event(&mut self, event: &Event) {
        if let Event::Mouse(mouse) = event {
            self.handle_mouse(mouse.kind);
        }
    }

    pub fn push_message(&mut self, message: RoomMessage) {
        self.messages.push(message);
        self.follow_output = true;
    }

    pub fn begin_response(&mut self, agent: &str) {
        self.live_response = Some(LiveResponse {
            agent: agent.to_owned(),
            content: String::new(),
        });
        self.set_status(format!("@{agent} is responding..."));
        self.follow_output = true;
    }

    pub fn push_response_delta(&mut self, delta: &str) {
        if let Some(response) = &mut self.live_response {
            response.content.push_str(delta);
        }
        self.follow_output = true;
    }

    pub fn finish_response(&mut self, agent: &str, content: String) {
        self.live_response = None;
        self.messages.push(RoomMessage::agent(agent, content));
        self.set_status("Ready");
        self.follow_output = true;
    }

    pub fn discard_response(&mut self) {
        self.live_response = None;
    }

    pub fn set_status(&mut self, status: impl Into<String>) {
        self.status = status.into();
        self.status_is_error = false;
    }

    pub fn set_error(&mut self, error: impl Into<String>) {
        self.status = error.into();
        self.status_is_error = true;
    }

    fn handle_key(&mut self, key: KeyEvent) -> InputAction {
        if self.handle_completion_key(key) {
            return InputAction::None;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return match key.code {
                KeyCode::Char('c') => InputAction::Exit,
                KeyCode::Char('a') => {
                    self.cursor = 0;
                    InputAction::None
                }
                KeyCode::Char('e') => {
                    self.cursor = self.input.len();
                    InputAction::None
                }
                KeyCode::Char('u') => {
                    self.input.drain(..self.cursor);
                    self.cursor = 0;
                    InputAction::None
                }
                KeyCode::Char('d') if self.input.is_empty() => InputAction::Exit,
                _ => InputAction::None,
            };
        }

        match key.code {
            KeyCode::Char(character) => self.insert(&character.to_string()),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete(),
            KeyCode::Left => self.move_left(),
            KeyCode::Right => self.move_right(),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.input.len(),
            KeyCode::Esc => {
                self.input.clear();
                self.cursor = 0;
            }
            KeyCode::Tab => self.complete_agent(),
            KeyCode::PageUp => {
                self.scroll_up(self.viewport_height.saturating_sub(1).max(1));
            }
            KeyCode::PageDown => {
                self.scroll_down(self.viewport_height.saturating_sub(1).max(1));
            }
            KeyCode::Enter => {
                let message = self.input.trim().to_owned();
                self.input.clear();
                self.cursor = 0;
                return match message.as_str() {
                    "" => InputAction::None,
                    "/exit" => InputAction::Exit,
                    _ => InputAction::Submit(message),
                };
            }
            _ => {}
        }
        InputAction::None
    }

    fn handle_completion_key(&mut self, key: KeyEvent) -> bool {
        if self.completion.is_none() {
            return false;
        }

        match key.code {
            KeyCode::Tab | KeyCode::Down => self.select_next_completion(),
            KeyCode::BackTab | KeyCode::Up => self.select_previous_completion(),
            KeyCode::Enter => self.accept_completion(),
            KeyCode::Esc => self.completion = None,
            _ => {
                self.completion = None;
                return false;
            }
        }
        true
    }

    fn complete_agent(&mut self) {
        let Some((start, prefix)) = self.agent_prefix_at_cursor() else {
            return;
        };
        let prefix = prefix.to_ascii_lowercase();
        let candidates = self
            .participants
            .iter()
            .filter(|name| name.to_ascii_lowercase().starts_with(&prefix))
            .cloned()
            .collect::<Vec<_>>();

        match candidates.len() {
            0 => self.set_error(format!("No room participant matches `@{prefix}`")),
            1 => self.insert_completion(start, self.cursor, &candidates[0]),
            _ => {
                self.completion = Some(CompletionMenu {
                    start,
                    end: self.cursor,
                    candidates,
                    selected: 0,
                });
            }
        }
    }

    fn agent_prefix_at_cursor(&self) -> Option<(usize, &str)> {
        let before_cursor = &self.input[..self.cursor];
        let start = before_cursor
            .char_indices()
            .rev()
            .find_map(|(index, character)| character.is_whitespace().then_some(index + 1))
            .unwrap_or(0);
        let token = &before_cursor[start..];
        let prefix = token.strip_prefix('@')?;
        if prefix.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '_' || character == '-'
        }) {
            Some((start, prefix))
        } else {
            None
        }
    }

    fn select_next_completion(&mut self) {
        if let Some(completion) = &mut self.completion {
            completion.selected = (completion.selected + 1) % completion.candidates.len();
        }
    }

    fn select_previous_completion(&mut self) {
        if let Some(completion) = &mut self.completion {
            completion.selected = completion
                .selected
                .checked_sub(1)
                .unwrap_or(completion.candidates.len() - 1);
        }
    }

    fn accept_completion(&mut self) {
        let Some(completion) = self.completion.take() else {
            return;
        };
        let candidate = completion.candidates[completion.selected].clone();
        self.insert_completion(completion.start, completion.end, &candidate);
    }

    fn insert_completion(&mut self, start: usize, end: usize, candidate: &str) {
        let replacement = format!("@{candidate}");
        self.input.replace_range(start..end, &replacement);
        self.cursor = start + replacement.len();
        if self.cursor == self.input.len() {
            self.input.push(' ');
            self.cursor += 1;
        }
        self.completion = None;
        self.set_status("Ready");
    }

    fn insert(&mut self, text: &str) {
        self.input.insert_str(self.cursor, text);
        self.cursor += text.len();
    }

    fn backspace(&mut self) {
        let Some(previous) = self.input[..self.cursor].char_indices().next_back() else {
            return;
        };
        self.input.drain(previous.0..self.cursor);
        self.cursor = previous.0;
    }

    fn delete(&mut self) {
        let Some(character) = self.input[self.cursor..].chars().next() else {
            return;
        };
        self.input
            .drain(self.cursor..self.cursor + character.len_utf8());
    }

    fn move_left(&mut self) {
        if let Some(previous) = self.input[..self.cursor].char_indices().next_back() {
            self.cursor = previous.0;
        }
    }

    fn move_right(&mut self) {
        if let Some(character) = self.input[self.cursor..].chars().next() {
            self.cursor += character.len_utf8();
        }
    }

    fn scroll_up(&mut self, lines: u16) {
        self.scroll = self.scroll.saturating_sub(lines);
        self.follow_output = false;
    }

    fn scroll_down(&mut self, lines: u16) {
        self.scroll = self.scroll.saturating_add(lines).min(self.maximum_scroll);
        self.follow_output = self.scroll == self.maximum_scroll;
    }

    fn handle_mouse(&mut self, kind: MouseEventKind) {
        match kind {
            MouseEventKind::ScrollUp => self.scroll_up(3),
            MouseEventKind::ScrollDown => self.scroll_down(3),
            _ => {}
        }
    }
}

pub(crate) fn render(frame: &mut Frame<'_>, state: &mut RoomUi) {
    let completion_height = state
        .completion
        .as_ref()
        .map(|menu| (menu.candidates.len() as u16 + 2).min(6))
        .unwrap_or(0);
    let [
        header_area,
        messages_area,
        completion_area,
        input_area,
        footer_area,
    ] = Layout::vertical([
        Constraint::Length(if state.description.is_empty() { 3 } else { 4 }),
        Constraint::Min(5),
        Constraint::Length(completion_height),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    render_header(frame, state, header_area);
    render_messages(frame, state, messages_area);
    render_completion(frame, state, completion_area);
    render_input(frame, state, input_area);
    render_footer(frame, state, footer_area);
}

fn render_completion(frame: &mut Frame<'_>, state: &RoomUi, area: Rect) {
    let Some(completion) = &state.completion else {
        return;
    };
    let visible_count = area.height.saturating_sub(2).max(1) as usize;
    let maximum_start = completion.candidates.len().saturating_sub(visible_count);
    let start = completion
        .selected
        .saturating_sub(visible_count - 1)
        .min(maximum_start);
    let lines = completion
        .candidates
        .iter()
        .enumerate()
        .skip(start)
        .take(visible_count)
        .map(|(index, name)| {
            let selected = index == completion.selected;
            Line::styled(
                format!("{} @{name}", if selected { "›" } else { " " }),
                if selected {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Cyan)
                },
            )
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Complete @agent "),
        ),
        area,
    );
}

fn render_header(frame: &mut Frame<'_>, state: &RoomUi, area: Rect) {
    let participants = state
        .participants
        .iter()
        .map(|name| format!("@{name}"))
        .collect::<Vec<_>>()
        .join("  ");
    let mut lines = vec![Line::from(vec![
        Span::styled("Participants  ", Style::default().fg(Color::DarkGray)),
        Span::styled(participants, Style::default().fg(Color::Cyan)),
    ])];
    if !state.description.is_empty() {
        lines.push(Line::from(state.description.clone()));
    }

    let header = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" Tapet · {} ", state.room_id))
            .title_style(
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
    );
    frame.render_widget(header, area);
}

fn render_messages(frame: &mut Frame<'_>, state: &mut RoomUi, area: Rect) {
    let mut lines = Vec::new();
    for message in &state.messages {
        push_message_lines(&mut lines, message.speaker(), message.visible_content());
        lines.push(Line::default());
    }
    if let Some(response) = &state.live_response {
        push_speaker_lines(
            &mut lines,
            &response.agent,
            &response.content,
            Color::Magenta,
        );
    }
    if lines.is_empty() {
        lines.push(Line::styled(
            "No messages yet. Start the conversation below.",
            Style::default().fg(Color::DarkGray),
        ));
    }

    let wrapped_width = area.width.saturating_sub(2).max(1) as usize;
    let line_count = lines
        .iter()
        .map(|line| line.width().max(1).div_ceil(wrapped_width))
        .sum::<usize>()
        .min(u16::MAX as usize) as u16;
    let content = Text::from(lines);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Conversation ");
    let inner = block.inner(area);
    let paragraph = Paragraph::new(content)
        .block(block)
        .wrap(Wrap { trim: false });
    let maximum_scroll = line_count.saturating_sub(inner.height);
    state.maximum_scroll = maximum_scroll;
    state.viewport_height = inner.height;
    if state.follow_output {
        state.scroll = maximum_scroll;
    } else {
        state.scroll = state.scroll.min(maximum_scroll);
    }
    frame.render_widget(paragraph.scroll((state.scroll, 0)), area);
}

fn push_message_lines<'a>(lines: &mut Vec<Line<'a>>, speaker: &RoomSpeaker, content: &'a str) {
    match speaker {
        RoomSpeaker::User => push_speaker_lines(lines, "you", content, Color::Green),
        RoomSpeaker::Agent(name) => push_speaker_lines(lines, name, content, Color::Magenta),
    }
}

fn push_speaker_lines<'a>(
    lines: &mut Vec<Line<'a>>,
    speaker: &str,
    content: &'a str,
    color: Color,
) {
    let mut content_lines = content.lines();
    let first = content_lines.next().unwrap_or_default();
    lines.push(Line::from(vec![
        Span::styled(
            format!("{speaker}> "),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::raw(first),
    ]));
    for line in content_lines {
        lines.push(Line::from(line));
    }
}

fn render_input(frame: &mut Frame<'_>, state: &RoomUi, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Message ")
        .border_style(Style::default().fg(Color::Green));
    let inner = block.inner(area);
    let cursor_width = Line::from(&state.input[..state.cursor]).width() as u16;
    let prompt_width = 5;
    let content_cursor = prompt_width + cursor_width;
    let horizontal_scroll = content_cursor.saturating_sub(inner.width.saturating_sub(1));
    let input = Paragraph::new(Line::from(vec![
        Span::styled("you> ", Style::default().fg(Color::Green)),
        Span::raw(state.input.as_str()),
    ]))
    .block(block)
    .scroll((0, horizontal_scroll));
    frame.render_widget(input, area);

    let cursor_x = inner
        .x
        .saturating_add(content_cursor)
        .saturating_sub(horizontal_scroll)
        .min(inner.right().saturating_sub(1));
    frame.set_cursor_position((cursor_x, inner.y));
}

fn render_footer(frame: &mut Frame<'_>, state: &RoomUi, area: Rect) {
    let status_style = if state.status_is_error {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let footer = Line::from(vec![
        Span::styled(state.status.as_str(), status_style),
        Span::raw("  "),
        Span::styled(
            "Tab complete · Enter send · Mouse wheel scroll · Ctrl-C exit",
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    frame.render_widget(Paragraph::new(footer), area);
}

#[cfg(test)]
mod tests {
    use super::{InputAction, RoomUi, render};
    use crate::agent::AgentSnapshot;
    use crate::room::{Room, RoomId, RoomMessage};
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn room() -> Room {
        Room::new(
            RoomId::new(),
            vec![
                AgentSnapshot::fixture_for("explorer", "model", "Explore"),
                AgentSnapshot::fixture_for("doubter", "model", "Doubt"),
            ],
            "Research together".to_owned(),
            String::new(),
        )
    }

    fn ambiguous_room() -> Room {
        Room::new(
            RoomId::new(),
            vec![
                AgentSnapshot::fixture_for("explorer", "model", "Explore"),
                AgentSnapshot::fixture_for("expert", "model", "Explain"),
            ],
            String::new(),
            String::new(),
        )
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::from(code))
    }

    #[test]
    fn edits_and_submits_unicode_input() {
        let mut state = RoomUi::new(&room(), Vec::new());
        assert_eq!(
            state.handle_event(key(KeyCode::Char('h'))),
            InputAction::None
        );
        state.handle_event(key(KeyCode::Char('é')));
        state.handle_event(key(KeyCode::Left));
        state.handle_event(key(KeyCode::Char('!')));

        assert_eq!(
            state.handle_event(key(KeyCode::Enter)),
            InputAction::Submit("h!é".to_owned())
        );
        assert!(state.input.is_empty());
    }

    #[test]
    fn exit_command_and_control_c_exit() {
        let mut state = RoomUi::new(&room(), Vec::new());
        for character in "/exit".chars() {
            state.handle_event(key(KeyCode::Char(character)));
        }
        assert_eq!(state.handle_event(key(KeyCode::Enter)), InputAction::Exit);

        let control_c = KeyEvent::new(KeyCode::Char('c'), crossterm::event::KeyModifiers::CONTROL);
        assert_eq!(state.handle_event(Event::Key(control_c)), InputAction::Exit);
    }

    #[test]
    fn renders_room_messages_and_live_response() {
        let mut state = RoomUi::new(&room(), vec![RoomMessage::user("Hello")]);
        state.begin_response("explorer");
        state.push_response_delta("Thinking aloud");
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|frame| render(frame, &mut state)).unwrap();

        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("Tapet"));
        assert!(rendered.contains("@explorer"));
        assert!(rendered.contains("you> Hello"));
        assert!(rendered.contains("explorer> Thinking aloud"));
    }

    #[test]
    fn mouse_wheel_scrolls_and_resumes_following_at_the_bottom() {
        let mut state = RoomUi::new(&room(), Vec::new());
        state.maximum_scroll = 20;
        state.scroll = 20;

        state.handle_event(mouse(MouseEventKind::ScrollUp));
        assert_eq!(state.scroll, 17);
        assert!(!state.follow_output);

        state.handle_event(mouse(MouseEventKind::ScrollDown));
        assert_eq!(state.scroll, 20);
        assert!(state.follow_output);
    }

    #[test]
    fn tab_completes_a_unique_room_participant() {
        let mut state = RoomUi::new(&room(), Vec::new());
        for character in "ask @ex".chars() {
            state.handle_event(key(KeyCode::Char(character)));
        }

        state.handle_event(key(KeyCode::Tab));

        assert_eq!(state.input, "ask @explorer ");
        assert!(state.completion.is_none());
    }

    #[test]
    fn ambiguous_completion_can_be_selected_with_arrows() {
        let mut state = RoomUi::new(&ambiguous_room(), Vec::new());
        for character in "@ex".chars() {
            state.handle_event(key(KeyCode::Char(character)));
        }

        state.handle_event(key(KeyCode::Tab));
        assert_eq!(state.completion.as_ref().unwrap().selected, 0);
        state.handle_event(key(KeyCode::Down));
        state.handle_event(key(KeyCode::Enter));

        assert_eq!(state.input, "@expert ");
        assert!(state.completion.is_none());
    }

    fn mouse(kind: MouseEventKind) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        })
    }
}
