use crate::room::{Room, RoomMessage, RoomSpeaker};
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    MouseEventKind,
};
use crossterm::execute;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use std::io;

const COMMANDS: [(&str, &str); 3] = [
    ("/agents", "Show room participants"),
    ("/exit", "Leave the room"),
    ("/help", "Show room commands"),
];

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
    room_name: String,
    description: String,
    participants: Vec<String>,
    messages: Vec<RoomMessage>,
    input: String,
    cursor: usize,
    input_history: Vec<String>,
    history_position: Option<usize>,
    history_draft: String,
    completion: Option<CompletionMenu>,
    live_response: Option<LiveResponse>,
    tool_approval: Option<ToolApproval>,
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

struct ToolApproval {
    agent: String,
    verb: &'static str,
    path: String,
}

struct CompletionMenu {
    start: usize,
    end: usize,
    title: &'static str,
    candidates: Vec<CompletionCandidate>,
    selected: usize,
}

struct CompletionCandidate {
    value: String,
    description: &'static str,
}

impl RoomUi {
    pub fn new(room: &Room, messages: Vec<RoomMessage>) -> Self {
        let input_history = messages
            .iter()
            .filter(|message| matches!(message.speaker(), RoomSpeaker::User))
            .map(|message| message.content().to_owned())
            .collect();
        Self {
            room_name: room.name().to_string(),
            description: room.description().to_owned(),
            participants: room
                .participants()
                .iter()
                .map(|participant| participant.agent_name().to_owned())
                .collect(),
            messages,
            input: String::new(),
            cursor: 0,
            input_history,
            history_position: None,
            history_draft: String::new(),
            completion: None,
            live_response: None,
            tool_approval: None,
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
                self.leave_history_navigation();
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
        if matches!(message.speaker(), RoomSpeaker::User) {
            self.record_input(message.content());
        }
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

    pub fn reset_response_content(&mut self) {
        if let Some(response) = &mut self.live_response {
            response.content.clear();
        }
        self.follow_output = true;
    }

    pub fn request_tool_approval(&mut self, agent: &str, verb: &'static str, path: &str) {
        self.tool_approval = Some(ToolApproval {
            agent: agent.to_owned(),
            verb,
            path: path.to_owned(),
        });
        self.set_status("Tool approval required");
    }

    pub fn clear_tool_approval(&mut self) {
        self.tool_approval = None;
    }

    pub fn push_tool_notice(&mut self, notice: impl Into<String>) {
        self.messages
            .push(RoomMessage::agent("tapet", notice.into()));
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
                    self.leave_history_navigation();
                    self.input.drain(..self.cursor);
                    self.cursor = 0;
                    InputAction::None
                }
                KeyCode::Char('d') if self.input.is_empty() => InputAction::Exit,
                _ => InputAction::None,
            };
        }

        match key.code {
            KeyCode::Char(character) => {
                self.leave_history_navigation();
                self.insert(&character.to_string());
            }
            KeyCode::Backspace => {
                self.leave_history_navigation();
                self.backspace();
            }
            KeyCode::Delete => {
                self.leave_history_navigation();
                self.delete();
            }
            KeyCode::Left => self.move_left(),
            KeyCode::Right => self.move_right(),
            KeyCode::Up => self.previous_input(),
            KeyCode::Down => self.next_input(),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.input.len(),
            KeyCode::Esc => {
                self.input.clear();
                self.cursor = 0;
            }
            KeyCode::Tab => self.complete_token(),
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
                self.history_position = None;
                self.history_draft.clear();
                return match message.as_str() {
                    "" => InputAction::None,
                    "/exit" => InputAction::Exit,
                    "/help" => {
                        self.set_status("Commands: /agents · /help · /exit");
                        InputAction::None
                    }
                    "/agents" => {
                        self.set_status(format!(
                            "Participants: {}",
                            self.participants
                                .iter()
                                .map(|name| format!("@{name}"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        ));
                        InputAction::None
                    }
                    command if command.starts_with('/') => {
                        self.set_error(format!(
                            "Unknown command `{command}`; use /help for available commands"
                        ));
                        InputAction::None
                    }
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

    fn complete_token(&mut self) {
        if let Some((start, prefix)) = self.agent_prefix_at_cursor() {
            let lowercase = prefix.to_ascii_lowercase();
            let candidates = self
                .participants
                .iter()
                .filter(|name| name.to_ascii_lowercase().starts_with(&lowercase))
                .map(|name| CompletionCandidate {
                    value: format!("@{name}"),
                    description: "",
                })
                .collect::<Vec<_>>();
            self.open_completion(
                start,
                " Complete @agent ",
                candidates,
                format!("No room participant matches `@{lowercase}`"),
            );
        } else if let Some(prefix) = self.command_prefix_at_cursor() {
            let lowercase = prefix.to_ascii_lowercase();
            let candidates = COMMANDS
                .iter()
                .filter(|(command, _)| command.starts_with(&lowercase))
                .map(|(command, description)| CompletionCandidate {
                    value: (*command).to_owned(),
                    description,
                })
                .collect::<Vec<_>>();
            self.open_completion(
                0,
                " Complete /command ",
                candidates,
                format!("No command matches `{lowercase}`"),
            );
        }
    }

    fn open_completion(
        &mut self,
        start: usize,
        title: &'static str,
        candidates: Vec<CompletionCandidate>,
        no_match: String,
    ) {
        match candidates.len() {
            0 => self.set_error(no_match),
            1 => self.insert_completion(start, self.cursor, &candidates[0].value),
            _ => {
                self.completion = Some(CompletionMenu {
                    start,
                    end: self.cursor,
                    title,
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

    fn command_prefix_at_cursor(&self) -> Option<&str> {
        let before_cursor = &self.input[..self.cursor];
        (before_cursor.starts_with('/') && !before_cursor.chars().any(char::is_whitespace))
            .then_some(before_cursor)
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
        let candidate = completion.candidates[completion.selected].value.clone();
        self.insert_completion(completion.start, completion.end, &candidate);
    }

    fn insert_completion(&mut self, start: usize, end: usize, candidate: &str) {
        self.leave_history_navigation();
        self.input.replace_range(start..end, candidate);
        self.cursor = start + candidate.len();
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

    fn previous_input(&mut self) {
        if self.input_history.is_empty() {
            return;
        }
        let position = match self.history_position {
            Some(position) => position.saturating_sub(1),
            None => {
                self.history_draft = self.input.clone();
                self.input_history.len() - 1
            }
        };
        self.history_position = Some(position);
        self.input.clone_from(&self.input_history[position]);
        self.cursor = self.input.len();
    }

    fn next_input(&mut self) {
        let Some(position) = self.history_position else {
            return;
        };
        if position + 1 < self.input_history.len() {
            let next = position + 1;
            self.history_position = Some(next);
            self.input.clone_from(&self.input_history[next]);
        } else {
            self.history_position = None;
            self.input.clone_from(&self.history_draft);
            self.history_draft.clear();
        }
        self.cursor = self.input.len();
    }

    fn leave_history_navigation(&mut self) {
        self.history_position = None;
        self.history_draft.clear();
    }

    fn record_input(&mut self, input: &str) {
        if !input.is_empty() && self.input_history.last().is_none_or(|last| last != input) {
            self.input_history.push(input.to_owned());
        }
        self.history_position = None;
        self.history_draft.clear();
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
    render_tool_approval(frame, state);
}

fn render_tool_approval(frame: &mut Frame<'_>, state: &RoomUi) {
    let Some(approval) = &state.tool_approval else {
        return;
    };
    let frame_area = frame.area();
    let width = frame_area.width.saturating_sub(4).min(72);
    if width < 20 || frame_area.height < 5 {
        return;
    }
    let height = 7.min(frame_area.height);
    let area = Rect::new(
        frame_area.x + frame_area.width.saturating_sub(width) / 2,
        frame_area.y + frame_area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    let caution = match approval.verb {
        "list" => "The directory listing will be sent to the model.",
        _ => "The file contents will be sent to the model.",
    };
    let content = vec![
        Line::from(vec![
            Span::styled(
                format!("@{}", approval.agent),
                Style::default().fg(Color::Magenta),
            ),
            Span::raw(format!(" wants to {} ", approval.verb)),
            Span::styled(&approval.path, Style::default().fg(Color::Cyan)),
        ]),
        Line::default(),
        Line::styled(caution, Style::default().fg(Color::Yellow)),
        Line::default(),
        Line::from(vec![
            Span::styled("[y] Allow once", Style::default().fg(Color::Green)),
            Span::raw("    "),
            Span::styled("[n] Deny", Style::default().fg(Color::Red)),
        ]),
    ];
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(content).block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" {} approval ", approval.verb))
                .border_style(Style::default().fg(Color::Yellow)),
        ),
        area,
    );
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
        .map(|(index, candidate)| {
            let selected = index == completion.selected;
            Line::styled(
                format!(
                    "{} {}{}",
                    if selected { "›" } else { " " },
                    candidate.value,
                    if candidate.description.is_empty() {
                        String::new()
                    } else {
                        format!("  {}", candidate.description)
                    }
                ),
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
                .title(completion.title),
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
            .title(format!(" Tapet · {} ", state.room_name))
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
    if state.tool_approval.is_none() {
        frame.set_cursor_position((cursor_x, inner.y));
    }
}

fn render_footer(frame: &mut Frame<'_>, state: &RoomUi, area: Rect) {
    let status_style = if state.status_is_error {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let controls = if state.tool_approval.is_some() {
        "y allow once · n deny · Mouse wheel scroll · Ctrl-C cancel"
    } else {
        "Tab complete · Enter send · Mouse wheel scroll · Ctrl-C exit"
    };
    let footer = Line::from(vec![
        Span::styled(state.status.as_str(), status_style),
        Span::raw("  "),
        Span::styled(controls, Style::default().fg(Color::DarkGray)),
    ]);
    frame.render_widget(Paragraph::new(footer), area);
}

#[cfg(test)]
mod tests {
    use super::{InputAction, RoomUi, render};
    use crate::agent::AgentSnapshot;
    use crate::room::{Room, RoomId, RoomMessage, RoomName};
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn room() -> Room {
        Room::new(
            RoomId::new(),
            RoomName::generate(),
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
            RoomName::generate(),
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

    #[test]
    fn input_history_uses_persisted_user_messages_and_restores_the_draft() {
        let mut state = RoomUi::new(
            &room(),
            vec![
                RoomMessage::user("first question"),
                RoomMessage::agent("explorer", "answer"),
                RoomMessage::user("second question"),
            ],
        );
        for character in "unfinished".chars() {
            state.handle_event(key(KeyCode::Char(character)));
        }

        state.handle_event(key(KeyCode::Up));
        assert_eq!(state.input, "second question");
        state.handle_event(key(KeyCode::Up));
        assert_eq!(state.input, "first question");
        state.handle_event(key(KeyCode::Down));
        assert_eq!(state.input, "second question");
        state.handle_event(key(KeyCode::Down));
        assert_eq!(state.input, "unfinished");
    }

    #[test]
    fn slash_commands_complete_and_run_locally() {
        let mut state = RoomUi::new(&room(), Vec::new());
        for character in "/a".chars() {
            state.handle_event(key(KeyCode::Char(character)));
        }
        state.handle_event(key(KeyCode::Tab));
        assert_eq!(state.input, "/agents ");

        assert_eq!(state.handle_event(key(KeyCode::Enter)), InputAction::None);
        assert_eq!(state.status, "Participants: @explorer, @doubter");

        state.handle_event(key(KeyCode::Char('/')));
        state.handle_event(key(KeyCode::Tab));
        assert_eq!(state.completion.as_ref().unwrap().candidates.len(), 3);
        assert_eq!(
            state.completion.as_ref().unwrap().title,
            " Complete /command "
        );
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
