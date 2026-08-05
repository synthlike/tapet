mod agent;
mod config;
mod message;
mod openai;
mod room;
mod store;
mod stream;
mod terminal;
mod ui;

use agent::AgentSnapshot;
use config::{Agent, Config, ConfigError};
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::{Stream, StreamExt};
use message::Message;
use openai::{OpenAiClient, ProviderError};
use room::{
    Room, RoomError, RoomId, RoomIdError, RoomMessage, RoomSpeaker, room_instructions,
    validate_participants,
};
use std::env;
use std::ffi::OsString;
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::process;
use store::{Store, StoreError};
use stream::{Completion, StreamEvent};
use terminal::InputSuppression;
use thiserror::Error;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader};
use ui::{InputAction, RoomUi, TerminalUi};

const CONFIG_PATH: &str = "tapet.toml";
const DATABASE_PATH: &str = ".tapet/tapet.db";

#[derive(Debug, Eq, PartialEq)]
enum Command {
    Agents,
    Ask { agent: String, message: String },
    Room { source: RoomSource },
    Enter { room: RoomId },
    History { room: RoomId },
}

#[derive(Debug, Eq, PartialEq)]
enum RoomSource {
    With(Vec<String>),
    From(String),
}

fn selected_command(mut args: impl Iterator<Item = OsString>) -> Result<Command, AppError> {
    let subcommand = args
        .next()
        .ok_or(AppError::Usage)?
        .into_string()
        .map_err(|_| AppError::Usage)?;

    match subcommand.as_str() {
        "agents" => {
            if args.next().is_some() {
                return Err(AppError::Usage);
            }
            Ok(Command::Agents)
        }
        "ask" => {
            let agent = next_argument(&mut args)?;
            let message = next_argument(&mut args)?;
            if message.is_empty() || args.next().is_some() {
                return Err(AppError::Usage);
            }
            Ok(Command::Ask { agent, message })
        }
        "room" => {
            let arguments: Result<Vec<_>, _> = args
                .map(|argument| argument.into_string().map_err(|_| AppError::Usage))
                .collect();
            Ok(Command::Room {
                source: parse_room_source(&arguments?)?,
            })
        }
        "enter" => {
            let room = next_argument(&mut args)?.parse()?;
            if args.next().is_some() {
                return Err(AppError::Usage);
            }
            Ok(Command::Enter { room })
        }
        "history" => {
            let room = next_argument(&mut args)?.parse()?;
            if args.next().is_some() {
                return Err(AppError::Usage);
            }
            Ok(Command::History { room })
        }
        _ => Err(AppError::Usage),
    }
}

fn parse_room_source(arguments: &[String]) -> Result<RoomSource, AppError> {
    if let [flag, name] = arguments
        && flag == "--from"
        && !name.is_empty()
    {
        return Ok(RoomSource::From(name.clone()));
    }

    if arguments.is_empty() || !arguments.len().is_multiple_of(2) {
        return Err(AppError::Usage);
    }
    let mut agents = Vec::new();
    for pair in arguments.chunks_exact(2) {
        if pair[0] != "--with" || pair[1].is_empty() || pair[1].starts_with("--") {
            return Err(AppError::Usage);
        }
        agents.push(pair[1].clone());
    }
    Ok(RoomSource::With(agents))
}

fn next_argument(args: &mut impl Iterator<Item = OsString>) -> Result<String, AppError> {
    args.next()
        .ok_or(AppError::Usage)?
        .into_string()
        .map_err(|_| AppError::Usage)
}

async fn try_main() -> Result<(), AppError> {
    let command = selected_command(env::args_os().skip(1))?;

    match command {
        Command::Agents => {
            let config = Config::load(Path::new(CONFIG_PATH))?;
            let stdout = io::stdout();
            write_agents(config.agents(), &mut stdout.lock())?;
        }
        Command::Ask {
            agent: agent_name,
            message,
        } => {
            let config = Config::load(Path::new(CONFIG_PATH))?;
            let agent = config.agent(&agent_name)?;
            let client = OpenAiClient::from_agent(agent)?;
            let messages = [Message::user(message)];
            let stdout = io::stdout();
            render_response(
                agent.name(),
                agent.prompt(),
                &client,
                &messages,
                &mut stdout.lock(),
            )
            .await?;
        }
        Command::Room { source } => {
            let config = Config::load(Path::new(CONFIG_PATH))?;
            let (agent_names, description, prompt) = match source {
                RoomSource::With(agents) => (agents, String::new(), String::new()),
                RoomSource::From(template) => {
                    let template = config.room(&template)?;
                    (
                        template.agents().to_vec(),
                        template.description().to_owned(),
                        template.prompt().to_owned(),
                    )
                }
            };
            let participants = agent_names
                .iter()
                .map(|name| config.agent(name).map(AgentSnapshot::resolve))
                .collect::<Result<Vec<_>, _>>()?;
            validate_participants(&participants)?;
            let store = Store::open(DATABASE_PATH).await?;
            let room = store.create_room(participants, description, prompt).await?;
            if has_interactive_terminal() {
                let id = room.id().to_string();
                run_room_ui(store, room, Vec::new()).await?;
                println!("Room saved: {id}");
            } else {
                let input = BufReader::new(tokio::io::stdin());
                let stdout = io::stdout();
                let mut output = stdout.lock();
                writeln!(output, "Starting new room: {}", room.id())?;
                write_room_ready(&room, &mut output)?;
                run_room(store, room, input, output).await?;
            }
        }
        Command::Enter { room: id } => {
            let store = Store::open(DATABASE_PATH).await?;
            let room = store.load_room(&id).await?;
            let messages = store.room_history(&id).await?;
            if has_interactive_terminal() {
                let id = room.id().to_string();
                run_room_ui(store, room, messages).await?;
                println!("Room saved: {id}");
            } else {
                let input = BufReader::new(tokio::io::stdin());
                let stdout = io::stdout();
                let mut output = stdout.lock();
                writeln!(output, "Room {}", room.id())?;
                write_room_history(&messages, &mut output)?;
                run_room(store, room, input, output).await?;
            }
        }
        Command::History { room: id } => {
            let store = Store::open(DATABASE_PATH).await?;
            let stdout = io::stdout();
            let mut output = stdout.lock();
            store.load_room(&id).await?;
            let messages = store.room_history(&id).await?;
            write_room_history(&messages, &mut output)?;
        }
    }

    Ok(())
}

fn has_interactive_terminal() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

fn write_agents<'a>(
    agents: impl Iterator<Item = &'a Agent>,
    output: &mut impl Write,
) -> io::Result<()> {
    for agent in agents {
        writeln!(
            output,
            "{}\t{}\t{}\t{}",
            agent.name(),
            agent.model_alias(),
            agent.provider_name(),
            agent.model()
        )?;
    }
    Ok(())
}

fn write_room_ready(room: &Room, output: &mut impl Write) -> io::Result<()> {
    let participants = room
        .participants()
        .iter()
        .map(|participant| format!("@{}", participant.agent_name()))
        .collect::<Vec<_>>()
        .join(", ");
    if !room.description().is_empty() {
        writeln!(output, "room> {}", room.description())?;
    }
    writeln!(output, "room> ready ({participants})")
}

fn write_room_history(messages: &[RoomMessage], output: &mut impl Write) -> io::Result<()> {
    for message in messages {
        let speaker = match message.speaker() {
            RoomSpeaker::User => "you",
            RoomSpeaker::Agent(name) => name,
        };
        writeln!(output, "{speaker}> {}", message.visible_content())?;
    }
    Ok(())
}

async fn run_room_ui(store: Store, room: Room, messages: Vec<RoomMessage>) -> Result<(), AppError> {
    let mut terminal = TerminalUi::start()?;
    let mut terminal_events = EventStream::new();
    let mut state = RoomUi::new(&room, messages);

    loop {
        terminal.draw(&mut state)?;
        let event = next_terminal_event(&mut terminal_events).await?;
        match state.handle_event(event) {
            InputAction::None => {}
            InputAction::Exit => return Ok(()),
            InputAction::Submit(message) => {
                if run_room_ui_turn(
                    &store,
                    &room,
                    message,
                    &mut terminal,
                    &mut terminal_events,
                    &mut state,
                )
                .await?
                {
                    return Ok(());
                }
            }
        }
    }
}

async fn run_room_ui_turn(
    store: &Store,
    room: &Room,
    message: String,
    terminal: &mut TerminalUi,
    terminal_events: &mut EventStream,
    state: &mut RoomUi,
) -> Result<bool, AppError> {
    let room_message = RoomMessage::user(message);
    let targets = match room.route(&room_message) {
        Ok(targets) => targets
            .into_iter()
            .map(|participant| participant.agent_name().to_owned())
            .collect::<Vec<_>>(),
        Err(error) => {
            state.set_error(error.to_string());
            return Ok(false);
        }
    };

    state.set_status("Saving message...");
    terminal.draw(state)?;
    let appended = store
        .append_room_user_message(room.id(), room_message.content().to_owned())
        .await?;
    state.push_message(room_message);
    let provider_messages = appended
        .messages
        .iter()
        .map(RoomMessage::as_provider_message)
        .collect::<Vec<_>>();
    let mut failures = Vec::new();

    for target in targets {
        let participant = room
            .participant(&target)
            .expect("room routing only returns participants");
        state.begin_response(&target);
        terminal.draw(state)?;
        let call_id = store
            .begin_room_call(room.id(), &target, appended.message_id)
            .await?;
        let client = match OpenAiClient::from_settings(
            participant.base_url(),
            participant.api_key_env(),
            participant.model(),
        ) {
            Ok(client) => client,
            Err(error) => {
                store.fail_room_call(call_id, error.to_string()).await?;
                state.discard_response();
                failures.push(format!("@{target}: {error}"));
                continue;
            }
        };
        let instructions = room_instructions(room, participant);
        let outcome = stream_room_response_ui(
            &target,
            &instructions,
            &client,
            &provider_messages,
            terminal,
            terminal_events,
            state,
        )
        .await;

        match outcome {
            Ok(Some(response)) => {
                store
                    .complete_room_call(call_id, response.text.clone(), response.completion)
                    .await?;
                state.finish_response(&target, response.text);
            }
            Ok(None) => {
                store
                    .fail_room_call(call_id, "cancelled by user".to_owned())
                    .await?;
                state.discard_response();
                return Ok(true);
            }
            Err(error) => {
                store.fail_room_call(call_id, error.to_string()).await?;
                state.discard_response();
                failures.push(format!("@{target}: {error}"));
            }
        }
    }

    if failures.is_empty() {
        state.set_status("Ready");
    } else {
        state.set_error(failures.join("; "));
    }
    Ok(false)
}

async fn stream_room_response_ui(
    agent_name: &str,
    system_prompt: &str,
    client: &OpenAiClient,
    messages: &[Message],
    terminal: &mut TerminalUi,
    terminal_events: &mut EventStream,
    state: &mut RoomUi,
) -> Result<Option<AssistantResponse>, AppError> {
    let request = client.stream(system_prompt, messages);
    tokio::pin!(request);
    let mut events = loop {
        tokio::select! {
            result = &mut request => break result?,
            event = next_terminal_event(terminal_events) => {
                let event = event?;
                if is_exit_event(&event) {
                    return Ok(None);
                }
                state.handle_passive_event(&event);
                terminal.draw(state)?;
            }
        }
    };
    let mut filter = LeadingAttributionFilter::new(agent_name);
    let mut message = String::new();

    loop {
        tokio::select! {
            event = events.next() => {
                match event {
                    Some(Ok(StreamEvent::TextDelta(delta))) => {
                        let visible = filter.push(&delta);
                        if !visible.is_empty() {
                            message.push_str(&visible);
                            state.push_response_delta(&visible);
                            terminal.draw(state)?;
                        }
                    }
                    Some(Ok(StreamEvent::Completed(completion))) => {
                        let visible = filter.finish();
                        if !visible.is_empty() {
                            message.push_str(&visible);
                            state.push_response_delta(&visible);
                            terminal.draw(state)?;
                        }
                        return Ok(Some(AssistantResponse { text: message, completion }));
                    }
                    Some(Err(error)) => return Err(error.into()),
                    None => return Err(ProviderError::IncompleteStream.into()),
                }
            }
            event = next_terminal_event(terminal_events) => {
                let event = event?;
                if is_exit_event(&event) {
                    return Ok(None);
                }
                state.handle_passive_event(&event);
                terminal.draw(state)?;
            }
        }
    }
}

async fn next_terminal_event(events: &mut EventStream) -> io::Result<Event> {
    match events.next().await {
        Some(event) => event,
        None => Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "terminal event stream ended",
        )),
    }
}

fn is_exit_event(event: &Event) -> bool {
    matches!(
        event,
        Event::Key(key)
            if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
                && key.code == KeyCode::Char('c')
                && key.modifiers.contains(KeyModifiers::CONTROL)
    )
}

async fn run_room(
    store: Store,
    room: Room,
    mut input: impl AsyncBufRead + Unpin,
    mut output: impl Write,
) -> Result<(), AppError> {
    loop {
        let input_action = tokio::select! {
            result = read_user_input(&mut input, &mut output) => result?,
            result = tokio::signal::ctrl_c() => {
                result?;
                writeln!(output)?;
                return Ok(());
            }
        };
        let message = match input_action {
            UserInput::Message(message) => message,
            UserInput::Ignore => continue,
            UserInput::Exit => return Ok(()),
        };

        let room_message = RoomMessage::user(message);
        let targets = match room.route(&room_message) {
            Ok(targets) => targets
                .into_iter()
                .map(|participant| participant.agent_name().to_owned())
                .collect::<Vec<_>>(),
            Err(error) => {
                writeln!(output, "room> {error}")?;
                continue;
            }
        };
        let appended = store
            .append_room_user_message(room.id(), room_message.content().to_owned())
            .await?;
        let provider_messages = appended
            .messages
            .iter()
            .map(RoomMessage::as_provider_message)
            .collect::<Vec<_>>();
        let input_suppression = InputSuppression::start()?;
        let mut failures = Vec::new();

        for target in targets {
            let participant = room
                .participant(&target)
                .expect("room routing only returns participants");
            let call_id = store
                .begin_room_call(room.id(), &target, appended.message_id)
                .await?;
            let client = match OpenAiClient::from_settings(
                participant.base_url(),
                participant.api_key_env(),
                participant.model(),
            ) {
                Ok(client) => client,
                Err(error) => {
                    store.fail_room_call(call_id, error.to_string()).await?;
                    failures.push(format!("@{target}: {error}"));
                    continue;
                }
            };
            let instructions = room_instructions(&room, participant);
            let outcome = tokio::select! {
                result = render_room_response(
                    participant.agent_name(),
                    &instructions,
                    &client,
                    &provider_messages,
                    &mut output,
                ) => TurnOutcome::Response(result),
                result = tokio::signal::ctrl_c() => TurnOutcome::Interrupted(result),
            };

            match outcome {
                TurnOutcome::Response(Ok(response)) => {
                    store
                        .complete_room_call(call_id, response.text, response.completion)
                        .await?;
                }
                TurnOutcome::Response(Err(error)) => {
                    store.fail_room_call(call_id, error.to_string()).await?;
                    failures.push(format!("@{target}: {error}"));
                }
                TurnOutcome::Interrupted(signal) => {
                    let failure = match &signal {
                        Ok(()) => "cancelled by user".to_owned(),
                        Err(error) => format!("failed to listen for Ctrl-C: {error}"),
                    };
                    store.fail_room_call(call_id, failure).await?;
                    drop(input_suppression);
                    signal?;
                    writeln!(output)?;
                    return Ok(());
                }
            }
        }
        drop(input_suppression);

        if !failures.is_empty() {
            return Err(AppError::RoomCalls(failures.join("; ")));
        }
    }
}

enum TurnOutcome {
    Response(Result<AssistantResponse, AppError>),
    Interrupted(io::Result<()>),
}

async fn render_response(
    agent_name: &str,
    system_prompt: &str,
    client: &OpenAiClient,
    messages: &[Message],
    output: &mut impl Write,
) -> Result<AssistantResponse, AppError> {
    let events = client.stream(system_prompt, messages).await?;
    write!(output, "{agent_name}> ")?;
    output.flush()?;

    match render_events(events, output).await {
        Ok(message) => {
            writeln!(output)?;
            Ok(message)
        }
        Err(error) => {
            let _ = writeln!(output);
            Err(error)
        }
    }
}

async fn render_room_response(
    agent_name: &str,
    system_prompt: &str,
    client: &OpenAiClient,
    messages: &[Message],
    output: &mut impl Write,
) -> Result<AssistantResponse, AppError> {
    let events = client.stream(system_prompt, messages).await?;
    write!(output, "{agent_name}> ")?;
    output.flush()?;

    match render_room_events(events, agent_name, output).await {
        Ok(message) => {
            writeln!(output)?;
            Ok(message)
        }
        Err(error) => {
            let _ = writeln!(output);
            Err(error)
        }
    }
}

#[derive(Debug)]
struct AssistantResponse {
    text: String,
    completion: Completion,
}

async fn render_events<S>(
    mut events: S,
    output: &mut impl Write,
) -> Result<AssistantResponse, AppError>
where
    S: Stream<Item = Result<StreamEvent, ProviderError>> + Unpin,
{
    let mut message = String::new();

    while let Some(event) = events.next().await {
        match event? {
            StreamEvent::TextDelta(delta) => {
                output.write_all(delta.as_bytes())?;
                output.flush()?;
                message.push_str(&delta);
            }
            StreamEvent::Completed(completion) => {
                return Ok(AssistantResponse {
                    text: message,
                    completion,
                });
            }
        }
    }

    Err(ProviderError::IncompleteStream.into())
}

async fn render_room_events<S>(
    mut events: S,
    agent_name: &str,
    output: &mut impl Write,
) -> Result<AssistantResponse, AppError>
where
    S: Stream<Item = Result<StreamEvent, ProviderError>> + Unpin,
{
    let mut filter = LeadingAttributionFilter::new(agent_name);
    let mut message = String::new();

    while let Some(event) = events.next().await {
        match event? {
            StreamEvent::TextDelta(delta) => {
                let visible = filter.push(&delta);
                output.write_all(visible.as_bytes())?;
                if !visible.is_empty() {
                    output.flush()?;
                    message.push_str(&visible);
                }
            }
            StreamEvent::Completed(completion) => {
                let visible = filter.finish();
                output.write_all(visible.as_bytes())?;
                if !visible.is_empty() {
                    output.flush()?;
                    message.push_str(&visible);
                }
                return Ok(AssistantResponse {
                    text: message,
                    completion,
                });
            }
        }
    }

    Err(ProviderError::IncompleteStream.into())
}

struct LeadingAttributionFilter {
    prefix: String,
    pending: Option<String>,
}

impl LeadingAttributionFilter {
    fn new(agent_name: &str) -> Self {
        Self {
            prefix: format!("@{}", agent_name.to_ascii_lowercase()),
            pending: Some(String::new()),
        }
    }

    fn push(&mut self, delta: &str) -> String {
        let Some(pending) = &mut self.pending else {
            return delta.to_owned();
        };
        pending.push_str(delta);
        let lowercase = pending.to_ascii_lowercase();

        if pending.len() < self.prefix.len() {
            if self.prefix.starts_with(&lowercase) {
                return String::new();
            }
            return self.pending.take().expect("pending response exists");
        }
        if !lowercase.starts_with(&self.prefix) {
            return self.pending.take().expect("pending response exists");
        }

        let remainder = &pending[self.prefix.len()..];
        if remainder.is_empty() {
            return String::new();
        }
        let remainder = if let Some(remainder) = remainder.strip_prefix(':') {
            remainder
        } else if remainder.starts_with(char::is_whitespace) {
            remainder
        } else {
            return self.pending.take().expect("pending response exists");
        };
        let visible = remainder.trim_start_matches(char::is_whitespace);
        if visible.is_empty() {
            return String::new();
        }

        let visible = visible.to_owned();
        self.pending = None;
        visible
    }

    fn finish(&mut self) -> String {
        let Some(pending) = self.pending.take() else {
            return String::new();
        };
        if pending.len() < self.prefix.len() {
            return pending;
        }
        let lowercase = pending.to_ascii_lowercase();
        if !lowercase.starts_with(&self.prefix) {
            return pending;
        }
        let remainder = &pending[self.prefix.len()..];
        let remainder = remainder.strip_prefix(':').unwrap_or(remainder);
        if remainder.trim().is_empty() {
            String::new()
        } else {
            pending
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
enum UserInput {
    Message(String),
    Ignore,
    Exit,
}

async fn read_user_input(
    input: &mut (impl AsyncBufRead + Unpin),
    output: &mut impl Write,
) -> io::Result<UserInput> {
    write!(output, "you> ")?;
    output.flush()?;

    let mut line = String::new();
    if input.read_line(&mut line).await? == 0 {
        return Ok(UserInput::Exit);
    }

    let line = line.trim_end_matches(['\r', '\n']);
    match line {
        "" => Ok(UserInput::Ignore),
        "/exit" => Ok(UserInput::Exit),
        message => Ok(UserInput::Message(message.to_owned())),
    }
}

#[tokio::main]
async fn main() {
    if let Err(error) = try_main().await {
        eprintln!("tapet: {error}");
        process::exit(1);
    }
}

#[derive(Debug, Error)]
enum AppError {
    #[error(
        "usage: tapet agents\n       tapet ask <agent> <message>\n       tapet room --with <agent> [--with <agent>...]\n       tapet room --from <template>\n       tapet enter <room>\n       tapet history <room>"
    )]
    Usage,
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error(transparent)]
    RoomId(#[from] RoomIdError),
    #[error(transparent)]
    Room(#[from] RoomError),
    #[error("one or more room responses failed: {0}")]
    RoomCalls(String),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}

#[cfg(test)]
mod tests {
    use super::{
        Command, RoomSource, UserInput, read_user_input, render_events, render_room_events,
        selected_command, write_agents, write_room_history,
    };
    use crate::config::Config;
    use crate::openai::{ProviderError, decode_stream};
    use crate::room::RoomMessage;
    use crate::stream::{Completion, StreamEvent};
    use futures_util::stream;
    use std::ffi::OsString;
    use std::fs;
    use std::io::{self, Write};
    use tempfile::TempDir;
    use tokio::io::BufReader;

    #[test]
    fn parses_agent_listing_command() {
        assert_eq!(
            selected_command([OsString::from("agents")].into_iter()).unwrap(),
            Command::Agents
        );
    }

    #[test]
    fn parses_ask_command() {
        assert_eq!(
            selected_command(
                [
                    OsString::from("ask"),
                    OsString::from("explorer"),
                    OsString::from("hello")
                ]
                .into_iter()
            )
            .unwrap(),
            Command::Ask {
                agent: "explorer".to_owned(),
                message: "hello".to_owned()
            }
        );
    }

    #[test]
    fn parses_room_commands_and_room_ids() {
        assert_eq!(
            selected_command(
                [
                    OsString::from("room"),
                    OsString::from("--with"),
                    OsString::from("explorer"),
                ]
                .into_iter()
            )
            .unwrap(),
            Command::Room {
                source: RoomSource::With(vec!["explorer".to_owned()])
            }
        );
        assert_eq!(
            selected_command(
                [
                    OsString::from("room"),
                    OsString::from("--with"),
                    OsString::from("explorer"),
                    OsString::from("--with"),
                    OsString::from("reviewer"),
                ]
                .into_iter(),
            )
            .unwrap(),
            Command::Room {
                source: RoomSource::With(vec!["explorer".to_owned(), "reviewer".to_owned()])
            }
        );
        assert_eq!(
            selected_command(
                [
                    OsString::from("room"),
                    OsString::from("--from"),
                    OsString::from("research"),
                ]
                .into_iter()
            )
            .unwrap(),
            Command::Room {
                source: RoomSource::From("research".to_owned())
            }
        );

        let id = "room_550e8400e29b41d4a716446655440000";
        let room: crate::room::RoomId = id.parse().unwrap();
        assert_eq!(
            selected_command([OsString::from("enter"), OsString::from(id)].into_iter()).unwrap(),
            Command::Enter { room: room.clone() }
        );
        assert_eq!(
            selected_command([OsString::from("history"), OsString::from(id)].into_iter()).unwrap(),
            Command::History { room }
        );
    }

    #[test]
    fn rejects_incomplete_or_extra_arguments() {
        assert!(selected_command(std::iter::empty()).is_err());
        assert!(selected_command([OsString::from("ask")].into_iter()).is_err());
        assert!(selected_command([OsString::from("room")].into_iter()).is_err());
        assert!(
            selected_command([OsString::from("room"), OsString::from("explorer")].into_iter())
                .is_err()
        );
        assert!(
            selected_command(
                [
                    OsString::from("room"),
                    OsString::from("--from"),
                    OsString::from("research"),
                    OsString::from("--with"),
                    OsString::from("explorer"),
                ]
                .into_iter()
            )
            .is_err()
        );
        assert!(
            selected_command([OsString::from("chat"), OsString::from("explorer")].into_iter())
                .is_err()
        );
        assert!(
            selected_command([OsString::from("agents"), OsString::from("extra")].into_iter())
                .is_err()
        );
        assert!(
            selected_command([OsString::from("enter"), OsString::from("not-a-room")].into_iter())
                .is_err()
        );
    }

    #[tokio::test]
    async fn reads_messages_empty_lines_and_exit() {
        let bytes = b"hello\n\n/exit\n";
        let mut input = BufReader::new(&bytes[..]);
        let mut output = Vec::new();

        assert_eq!(
            read_user_input(&mut input, &mut output).await.unwrap(),
            UserInput::Message("hello".to_owned())
        );
        assert_eq!(
            read_user_input(&mut input, &mut output).await.unwrap(),
            UserInput::Ignore
        );
        assert_eq!(
            read_user_input(&mut input, &mut output).await.unwrap(),
            UserInput::Exit
        );
        assert_eq!(String::from_utf8(output).unwrap(), "you> you> you> ");
    }

    #[tokio::test]
    async fn end_of_input_exits_cleanly() {
        let bytes = b"";
        let mut input = BufReader::new(&bytes[..]);
        let mut output = Vec::new();

        assert_eq!(
            read_user_input(&mut input, &mut output).await.unwrap(),
            UserInput::Exit
        );
        assert_eq!(String::from_utf8(output).unwrap(), "you> ");
    }

    #[test]
    fn formats_room_history_with_attributed_speakers() {
        let mut output = Vec::new();

        write_room_history(
            &[
                RoomMessage::user("@explorer Hello"),
                RoomMessage::agent("explorer", "@EXPLORER: Hi"),
                RoomMessage::agent("reviewer", "I agree"),
            ],
            &mut output,
        )
        .unwrap();

        assert_eq!(
            String::from_utf8(output).unwrap(),
            "you> @explorer Hello\nexplorer> Hi\nreviewer> I agree\n"
        );
    }

    #[test]
    fn agent_listing_has_stable_tab_separated_output() {
        let temporary = TempDir::new().unwrap();
        let path = temporary.path().join("tapet.toml");
        fs::write(
            &path,
            concat!(
                "version = 1\n",
                "[providers.openai]\ntype = \"openai\"\napi_key_env = \"KEY\"\n",
                "[models.primary]\nprovider = \"openai\"\nmodel = \"gpt-test\"\n",
                "[agents.reviewer]\nmodel = \"primary\"\nprompt = \"Review\"\n",
                "[agents.explorer]\nmodel = \"primary\"\nprompt = \"Explore\"\n"
            ),
        )
        .unwrap();
        let config = Config::load(path).unwrap();
        let mut output = Vec::new();

        write_agents(config.agents(), &mut output).unwrap();

        assert_eq!(
            String::from_utf8(output).unwrap(),
            concat!(
                "explorer\tprimary\topenai\tgpt-test\n",
                "reviewer\tprimary\topenai\tgpt-test\n"
            )
        );
    }

    #[tokio::test]
    async fn printed_deltas_are_the_stored_assistant_message() {
        let events = stream::iter([
            Ok(StreamEvent::TextDelta("Owner".to_owned())),
            Ok(StreamEvent::TextDelta("ship".to_owned())),
            Ok(StreamEvent::Completed(completion())),
        ]);
        let mut output = TrackingWriter::default();

        let message = render_events(events, &mut output).await.unwrap();

        assert_eq!(message.text, "Ownership");
        assert_eq!(String::from_utf8(output.bytes).unwrap(), message.text);
        assert_eq!(output.flushes, 2);
    }

    #[tokio::test]
    async fn room_responses_hide_and_remove_a_streamed_self_attribution() {
        let events = stream::iter([
            Ok(StreamEvent::TextDelta("@SH".to_owned())),
            Ok(StreamEvent::TextDelta("OUTER: ".to_owned())),
            Ok(StreamEvent::TextDelta("HELLO".to_owned())),
            Ok(StreamEvent::Completed(completion())),
        ]);
        let mut output = TrackingWriter::default();

        let message = render_room_events(events, "shouter", &mut output)
            .await
            .unwrap();

        assert_eq!(message.text, "HELLO");
        assert_eq!(String::from_utf8(output.bytes).unwrap(), "HELLO");
        assert_eq!(output.flushes, 1);
    }

    #[tokio::test]
    async fn room_responses_preserve_text_that_is_not_a_self_attribution() {
        let events = stream::iter([
            Ok(StreamEvent::TextDelta("@exploration".to_owned())),
            Ok(StreamEvent::TextDelta(" continues".to_owned())),
            Ok(StreamEvent::Completed(completion())),
        ]);
        let mut output = Vec::new();

        let message = render_room_events(events, "explorer", &mut output)
            .await
            .unwrap();

        assert_eq!(message.text, "@exploration continues");
        assert_eq!(String::from_utf8(output).unwrap(), message.text);
    }

    #[tokio::test]
    async fn a_stream_without_completion_fails() {
        let fixture = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n"
        );
        let events = decode_stream(stream::iter([Ok::<_, io::Error>(
            fixture.as_bytes().to_vec(),
        )]));
        let mut output = Vec::new();

        let error = render_events(events, &mut output).await.unwrap_err();

        assert!(matches!(
            error,
            super::AppError::Provider(ProviderError::IncompleteStream)
        ));
        assert_eq!(output, b"partial");
    }

    fn completion() -> Completion {
        Completion {
            provider_response_id: Some("resp_test".to_owned()),
            input_tokens: 1,
            output_tokens: 1,
        }
    }

    #[derive(Default)]
    struct TrackingWriter {
        bytes: Vec<u8>,
        flushes: usize,
    }

    impl Write for TrackingWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            Ok(())
        }
    }
}
