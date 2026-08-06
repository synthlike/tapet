mod agent;
mod config;
mod message;
mod openai;
mod room;
mod store;
mod stream;
mod terminal;
mod tool;
mod ui;

use agent::AgentSnapshot;
use config::{Agent, Config, ConfigError, RoomTemplate};
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::{Stream, StreamExt};
use message::Message;
use openai::{OpenAiClient, ProviderError, ToolInput};
use room::{
    Room, RoomError, RoomMessage, RoomName, RoomNameError, RoomSpeaker, room_instructions,
    validate_participants,
};
use std::env;
use std::ffi::OsString;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process;
use store::{RoomSummary, Store, StoreError};
use stream::{Completion, ResponseRound, StreamEvent, ToolCall};
use terminal::InputSuppression;
use thiserror::Error;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader};
use tool::{MAX_TOOL_CALLS_PER_TURN, ToolRequest, error_output};
use ui::{InputAction, RoomUi, TerminalUi};

const CONFIG_PATH: &str = "tapet.toml";
const DATABASE_PATH: &str = ".tapet/tapet.db";

#[derive(Debug, Eq, PartialEq)]
enum Command {
    Agents,
    Templates,
    Rooms,
    Ask {
        agent: String,
        message: String,
    },
    Room {
        source: RoomSource,
        name: Option<RoomName>,
    },
    Enter {
        room: RoomName,
    },
    History {
        room: RoomName,
    },
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
        "templates" => {
            if args.next().is_some() {
                return Err(AppError::Usage);
            }
            Ok(Command::Templates)
        }
        "rooms" => {
            if args.next().is_some() {
                return Err(AppError::Usage);
            }
            Ok(Command::Rooms)
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
            let (source, name) = parse_room_options(&arguments?)?;
            Ok(Command::Room { source, name })
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

fn parse_room_options(arguments: &[String]) -> Result<(RoomSource, Option<RoomName>), AppError> {
    if arguments.is_empty() || !arguments.len().is_multiple_of(2) {
        return Err(AppError::Usage);
    }
    let mut agents = Vec::new();
    let mut template = None;
    let mut name = None;
    for pair in arguments.chunks_exact(2) {
        if pair[1].is_empty() || pair[1].starts_with("--") {
            return Err(AppError::Usage);
        }
        match pair[0].as_str() {
            "--with" if template.is_none() => agents.push(pair[1].clone()),
            "--from" if template.is_none() && agents.is_empty() => {
                template = Some(pair[1].clone());
            }
            "--name" if name.is_none() => name = Some(pair[1].parse()?),
            _ => return Err(AppError::Usage),
        }
    }
    let source = match (template, agents.is_empty()) {
        (Some(template), true) => RoomSource::From(template),
        (None, false) => RoomSource::With(agents),
        _ => return Err(AppError::Usage),
    };
    Ok((source, name))
}

fn next_argument(args: &mut impl Iterator<Item = OsString>) -> Result<String, AppError> {
    args.next()
        .ok_or(AppError::Usage)?
        .into_string()
        .map_err(|_| AppError::Usage)
}

fn extract_config_path(
    args: impl Iterator<Item = OsString>,
) -> Result<(PathBuf, Vec<OsString>), AppError> {
    let mut config_path = None;
    let mut remaining = Vec::new();
    let mut args = args;
    while let Some(argument) = args.next() {
        if argument == "--config" {
            if config_path.is_some() {
                return Err(AppError::Usage);
            }
            config_path = Some(PathBuf::from(args.next().ok_or(AppError::Usage)?));
        } else {
            remaining.push(argument);
        }
    }
    Ok((
        config_path.unwrap_or_else(|| PathBuf::from(CONFIG_PATH)),
        remaining,
    ))
}

async fn try_main() -> Result<(), AppError> {
    let (config_path, remaining) = extract_config_path(env::args_os().skip(1))?;
    let command = selected_command(remaining.into_iter())?;

    match command {
        Command::Agents => {
            let config = Config::load(&config_path)?;
            let stdout = io::stdout();
            write_agents(config.agents(), &mut stdout.lock())?;
        }
        Command::Templates => {
            let config = Config::load(&config_path)?;
            let stdout = io::stdout();
            write_templates(config.rooms(), &mut stdout.lock())?;
        }
        Command::Rooms => {
            let store = Store::open(DATABASE_PATH).await?;
            let rooms = store.list_rooms().await?;
            let stdout = io::stdout();
            write_room_list(&rooms, &mut stdout.lock())?;
        }
        Command::Ask {
            agent: agent_name,
            message,
        } => {
            let config = Config::load(&config_path)?;
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
        Command::Room { source, name } => {
            let config = Config::load(&config_path)?;
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
            let room = store
                .create_room(name, participants, description, prompt)
                .await?;
            if has_interactive_terminal() {
                let name = room.name().to_string();
                run_room_ui(store, room, Vec::new(), &config_path).await?;
                println!("Room saved: {name}");
            } else {
                let input = BufReader::new(tokio::io::stdin());
                let stdout = io::stdout();
                let mut output = stdout.lock();
                writeln!(output, "Starting new room: {}", room.name())?;
                write_room_ready(&room, &mut output)?;
                run_room(store, room, input, output).await?;
            }
        }
        Command::Enter { room: name } => {
            let store = Store::open(DATABASE_PATH).await?;
            let room = store.load_room(&name).await?;
            let messages = store.room_history(room.id()).await?;
            if has_interactive_terminal() {
                let name = room.name().to_string();
                run_room_ui(store, room, messages, &config_path).await?;
                println!("Room saved: {name}");
            } else {
                let input = BufReader::new(tokio::io::stdin());
                let stdout = io::stdout();
                let mut output = stdout.lock();
                writeln!(output, "Room {}", room.name())?;
                write_room_history(&messages, &mut output)?;
                run_room(store, room, input, output).await?;
            }
        }
        Command::History { room: name } => {
            let store = Store::open(DATABASE_PATH).await?;
            let stdout = io::stdout();
            let mut output = stdout.lock();
            let room = store.load_room(&name).await?;
            let messages = store.room_history(room.id()).await?;
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
    let rows: Vec<_> = agents
        .map(|agent| (agent.name(), agent.provider_name(), agent.model()))
        .collect();
    let name_width = rows.iter().map(|(name, ..)| name.len()).max().unwrap_or(0);
    let provider_width = rows
        .iter()
        .map(|(_, provider, _)| provider.len())
        .max()
        .unwrap_or(0);
    for (name, provider, model) in rows {
        writeln!(
            output,
            "{name:name_width$}  {provider:provider_width$}  {model}"
        )?;
    }
    Ok(())
}

fn write_templates<'a>(
    rooms: impl Iterator<Item = &'a RoomTemplate>,
    output: &mut impl Write,
) -> io::Result<()> {
    let rows: Vec<_> = rooms
        .map(|room| {
            let default = room.agents().first().map_or("", String::as_str);
            (room.name(), default, room.agents().join(", "))
        })
        .collect();
    let name_width = rows.iter().map(|(name, ..)| name.len()).max().unwrap_or(0);
    let default_width = rows
        .iter()
        .map(|(_, default, _)| default.len())
        .max()
        .unwrap_or(0);
    for (name, default, agents) in rows {
        writeln!(
            output,
            "{name:name_width$}  {default:default_width$}  {agents}"
        )?;
    }
    Ok(())
}

fn write_room_list(rooms: &[RoomSummary], output: &mut impl Write) -> io::Result<()> {
    let rows: Vec<_> = rooms
        .iter()
        .map(|room| (room.name().to_string(), room.participants().join(", ")))
        .collect();
    let name_width = rows.iter().map(|(name, _)| name.len()).max().unwrap_or(0);
    for (name, agents) in rows {
        writeln!(output, "{name:name_width$}  {agents}")?;
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

async fn run_room_ui(
    store: Store,
    mut room: Room,
    messages: Vec<RoomMessage>,
    config_path: &Path,
) -> Result<(), AppError> {
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
            InputAction::AddAgent(name) => {
                add_room_participant_ui(&store, &mut room, &name, &mut state, config_path).await?;
            }
        }
    }
}

async fn add_room_participant_ui(
    store: &Store,
    room: &mut Room,
    agent_name: &str,
    state: &mut RoomUi,
    config_path: &Path,
) -> Result<(), AppError> {
    if room.participant(agent_name).is_some() {
        state.set_error(format!("@{agent_name} is already in this room"));
        return Ok(());
    }
    let config = match Config::load(config_path) {
        Ok(config) => config,
        Err(error) => {
            state.set_error(error.to_string());
            return Ok(());
        }
    };
    let agent = match config.agent(agent_name) {
        Ok(agent) => agent,
        Err(error) => {
            state.set_error(error.to_string());
            return Ok(());
        }
    };
    let participant = AgentSnapshot::resolve(agent);

    match store
        .add_room_participant(room.id(), participant.clone())
        .await
    {
        Ok(()) => {
            room.add_participant(participant);
            state.add_known_participant(agent_name);
            state.set_status(format!("@{agent_name} joined the room"));
        }
        Err(error) => state.set_error(error.to_string()),
    }
    Ok(())
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
        let outcome = {
            let mut active_ui = ActiveRoomUi {
                terminal,
                terminal_events,
                state,
            };
            stream_room_response_ui(
                &target,
                &instructions,
                &client,
                &provider_messages,
                store,
                call_id,
                &mut active_ui,
            )
            .await
        };

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

struct ActiveRoomUi<'a> {
    terminal: &'a mut TerminalUi,
    terminal_events: &'a mut EventStream,
    state: &'a mut RoomUi,
}

async fn stream_room_response_ui(
    agent_name: &str,
    system_prompt: &str,
    client: &OpenAiClient,
    messages: &[Message],
    store: &Store,
    room_call_id: i64,
    ui: &mut ActiveRoomUi<'_>,
) -> Result<Option<AssistantResponse>, AppError> {
    let mut input = ToolInput::from_messages(messages)?;
    let mut usage = Completion {
        provider_response_id: None,
        input_tokens: 0,
        output_tokens: 0,
    };
    let mut tool_calls = 0;
    loop {
        let tools_enabled = tool_calls < MAX_TOOL_CALLS_PER_TURN;
        let events = {
            let request = client.stream_with_tools(system_prompt, &input, tools_enabled);
            tokio::pin!(request);
            loop {
                tokio::select! {
                    result = &mut request => break result?,
                    event = next_terminal_event(ui.terminal_events) => {
                        let event = event?;
                        if is_exit_event(&event) {
                            return Ok(None);
                        }
                        ui.state.handle_passive_event(&event);
                        ui.terminal.draw(ui.state)?;
                    }
                }
            }
        };
        let round = stream_room_round_ui(agent_name, events, ui).await?;
        let Some(round) = round else {
            return Ok(None);
        };
        add_usage(&mut usage, &round.response.completion);
        input.append_output_items(round.response.output_items);

        if round.tool_calls.is_empty() {
            return Ok(Some(AssistantResponse {
                text: round.text,
                completion: usage,
            }));
        }

        ui.state.reset_response_content();
        ui.terminal.draw(ui.state)?;
        for call in round.tool_calls {
            let record_id = store.record_tool_proposal(room_call_id, &call).await?;
            if tool_calls >= MAX_TOOL_CALLS_PER_TURN {
                let error =
                    format!("tool-call limit of {MAX_TOOL_CALLS_PER_TURN} reached for this turn");
                store.fail_tool_call(record_id, error.clone()).await?;
                ui.state
                    .push_tool_notice(format!("✗ {} · {error}", call.name));
                input.append_function_output(&call.call_id, error_output(&error));
                continue;
            }
            tool_calls += 1;
            let Some(output) = run_tool_ui(agent_name, &call, record_id, store, ui).await? else {
                return Ok(None);
            };
            input.append_function_output(&call.call_id, output);
        }
        ui.state
            .set_status(format!("@{agent_name} is responding..."));
    }
}

struct RoomResponseRound {
    text: String,
    tool_calls: Vec<ToolCall>,
    response: ResponseRound,
}

async fn stream_room_round_ui<S>(
    agent_name: &str,
    mut events: S,
    ui: &mut ActiveRoomUi<'_>,
) -> Result<Option<RoomResponseRound>, AppError>
where
    S: Stream<Item = Result<StreamEvent, ProviderError>> + Unpin,
{
    let mut filter = LeadingAttributionFilter::new(agent_name);
    let mut text = String::new();
    let mut tool_calls = Vec::new();

    loop {
        tokio::select! {
            event = events.next() => match event {
                Some(Ok(StreamEvent::TextDelta(delta))) => {
                    let visible = filter.push(&delta);
                    if !visible.is_empty() {
                        text.push_str(&visible);
                        ui.state.push_response_delta(&visible);
                        ui.terminal.draw(ui.state)?;
                    }
                }
                Some(Ok(StreamEvent::ToolCallProposed(call))) => tool_calls.push(call),
                Some(Ok(StreamEvent::Completed(response))) => {
                    let visible = filter.finish();
                    if !visible.is_empty() {
                        text.push_str(&visible);
                        ui.state.push_response_delta(&visible);
                        ui.terminal.draw(ui.state)?;
                    }
                    return Ok(Some(RoomResponseRound { text, tool_calls, response }));
                }
                Some(Err(error)) => return Err(error.into()),
                None => return Err(ProviderError::IncompleteStream.into()),
            },
            event = next_terminal_event(ui.terminal_events) => {
                let event = event?;
                if is_exit_event(&event) {
                    return Ok(None);
                }
                ui.state.handle_passive_event(&event);
                ui.terminal.draw(ui.state)?;
            }
        }
    }
}

async fn run_tool_ui(
    agent_name: &str,
    call: &ToolCall,
    record_id: i64,
    store: &Store,
    ui: &mut ActiveRoomUi<'_>,
) -> Result<Option<String>, AppError> {
    let request = match ToolRequest::parse(&call.name, &call.arguments) {
        Ok(request) => request,
        Err(error) => {
            store.fail_tool_call(record_id, error.to_string()).await?;
            ui.state
                .push_tool_notice(format!("✗ {} · {error}", call.name));
            return Ok(Some(error_output(&error)));
        }
    };
    let verb = request.verb();
    let path = request.display_path();
    let workspace = env::current_dir()?;
    let preview = match request.approval_preview(&workspace) {
        Ok(preview) => preview,
        Err(error) => {
            store.fail_tool_call(record_id, error.to_string()).await?;
            ui.state
                .push_tool_notice(format!("✗ @{agent_name} {verb} {path} · {error}"));
            return Ok(Some(error_output(&error)));
        }
    };
    ui.state.request_tool_approval(agent_name, preview);
    ui.terminal.draw(ui.state)?;

    let approved = loop {
        let event = next_terminal_event(ui.terminal_events).await?;
        if is_exit_event(&event) {
            ui.state.clear_tool_approval();
            store
                .fail_tool_call(record_id, "cancelled by user".to_owned())
                .await?;
            return Ok(None);
        }
        match &event {
            Event::Key(key)
                if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
                    && matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) =>
            {
                break true;
            }
            Event::Key(key)
                if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
                    && matches!(key.code, KeyCode::Char('n') | KeyCode::Char('N')) =>
            {
                break false;
            }
            _ => {
                ui.state.handle_passive_event(&event);
                ui.terminal.draw(ui.state)?;
            }
        }
    };
    ui.state.clear_tool_approval();

    if !approved {
        store.deny_tool_call(record_id).await?;
        ui.state
            .push_tool_notice(format!("○ @{agent_name} {verb} {path} · denied"));
        ui.terminal.draw(ui.state)?;
        return Ok(Some(error_output(&"user denied file access")));
    }

    store.approve_tool_call(record_id).await?;
    store.start_tool_call(record_id).await?;
    ui.state
        .set_status(format!("@{agent_name} is {verb}ing {path}..."));
    ui.terminal.draw(ui.state)?;
    let request_for_label = request.clone();
    let task = tokio::task::spawn_blocking(move || request.execute(&workspace));
    tokio::pin!(task);
    let result = loop {
        tokio::select! {
            result = &mut task => match result {
                Ok(result) => break result,
                Err(error) => {
                    store.fail_tool_call(record_id, error.to_string()).await?;
                    return Err(AppError::ToolTask(error));
                }
            },
            event = next_terminal_event(ui.terminal_events) => {
                let event = event?;
                if is_exit_event(&event) {
                    store.fail_tool_call(record_id, "cancelled by user".to_owned()).await?;
                    return Ok(None);
                }
                ui.state.handle_passive_event(&event);
                ui.terminal.draw(ui.state)?;
            }
        }
    };

    match result {
        Ok(outcome) => {
            store
                .complete_tool_call(record_id, outcome.bytes, outcome.lines)
                .await?;
            ui.state.push_tool_notice(format!(
                "✓ @{agent_name} {verb} {path} · {} {} · {}",
                outcome.lines,
                request_for_label.count_label(outcome.lines),
                format_bytes(outcome.bytes)
            ));
            ui.terminal.draw(ui.state)?;
            Ok(Some(outcome.json))
        }
        Err(error) => {
            store.fail_tool_call(record_id, error.to_string()).await?;
            ui.state
                .push_tool_notice(format!("✗ @{agent_name} {verb} {path} · {error}"));
            ui.terminal.draw(ui.state)?;
            Ok(Some(error_output(&error)))
        }
    }
}

fn add_usage(total: &mut Completion, round: &Completion) {
    total
        .provider_response_id
        .clone_from(&round.provider_response_id);
    total.input_tokens = total.input_tokens.saturating_add(round.input_tokens);
    total.output_tokens = total.output_tokens.saturating_add(round.output_tokens);
}

fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
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
                    &store,
                    call_id,
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
    store: &Store,
    room_call_id: i64,
) -> Result<AssistantResponse, AppError> {
    write!(output, "{agent_name}> ")?;
    output.flush()?;
    let mut input = ToolInput::from_messages(messages)?;
    let mut usage = Completion {
        provider_response_id: None,
        input_tokens: 0,
        output_tokens: 0,
    };
    let mut tool_calls = 0;

    loop {
        let events = client
            .stream_with_tools(system_prompt, &input, tool_calls < MAX_TOOL_CALLS_PER_TURN)
            .await?;
        let round = match render_room_events(events, agent_name, output).await {
            Ok(round) => round,
            Err(error) => {
                let _ = writeln!(output);
                return Err(error);
            }
        };
        add_usage(&mut usage, &round.response.completion);
        input.append_output_items(round.response.output_items);
        if round.tool_calls.is_empty() {
            writeln!(output)?;
            return Ok(AssistantResponse {
                text: round.text,
                completion: usage,
            });
        }

        for call in round.tool_calls {
            let record_id = store.record_tool_proposal(room_call_id, &call).await?;
            store.deny_tool_call(record_id).await?;
            tool_calls += 1;
            let reason = "interactive approval is unavailable when input or output is redirected";
            input.append_function_output(&call.call_id, error_output(&reason));
            writeln!(
                output,
                "\ntapet> denied {} · {reason}\n{agent_name}> ",
                call.name
            )?;
            output.flush()?;
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
            StreamEvent::ToolCallProposed(_) => {}
            StreamEvent::Completed(response) => {
                return Ok(AssistantResponse {
                    text: message,
                    completion: response.completion,
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
) -> Result<RoomResponseRound, AppError>
where
    S: Stream<Item = Result<StreamEvent, ProviderError>> + Unpin,
{
    let mut filter = LeadingAttributionFilter::new(agent_name);
    let mut message = String::new();
    let mut tool_calls = Vec::new();

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
            StreamEvent::ToolCallProposed(call) => tool_calls.push(call),
            StreamEvent::Completed(response) => {
                let visible = filter.finish();
                output.write_all(visible.as_bytes())?;
                if !visible.is_empty() {
                    output.flush()?;
                    message.push_str(&visible);
                }
                return Ok(RoomResponseRound {
                    text: message,
                    tool_calls,
                    response,
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
        "usage: tapet [--config <path>] agents\n       tapet [--config <path>] templates\n       tapet rooms\n       tapet [--config <path>] ask <agent> <message>\n       tapet [--config <path>] room [--name <name>] --with <agent> [--with <agent>...]\n       tapet [--config <path>] room [--name <name>] --from <template>\n       tapet enter <room>\n       tapet history <room>"
    )]
    Usage,
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error(transparent)]
    RoomName(#[from] RoomNameError),
    #[error(transparent)]
    Room(#[from] RoomError),
    #[error("one or more room responses failed: {0}")]
    RoomCalls(String),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("tool worker failed: {0}")]
    ToolTask(#[source] tokio::task::JoinError),
    #[error("could not serialize a tool result: {0}")]
    ToolJson(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::{
        Command, RoomSource, UserInput, extract_config_path, read_user_input, render_events,
        render_room_events, selected_command, write_agents, write_room_history, write_room_list,
        write_templates,
    };
    use crate::config::Config;
    use crate::openai::{ProviderError, decode_stream};
    use crate::room::RoomMessage;
    use crate::store::RoomSummary;
    use crate::stream::{Completion, ResponseRound, StreamEvent, ToolCall};
    use futures_util::stream;
    use std::ffi::OsString;
    use std::fs;
    use std::io::{self, Write};
    use tempfile::TempDir;
    use tokio::io::BufReader;

    #[test]
    fn extracts_the_config_flag_from_anywhere_in_the_arguments() {
        let (path, remaining) = extract_config_path(
            [
                OsString::from("--config"),
                OsString::from("custom.toml"),
                OsString::from("agents"),
            ]
            .into_iter(),
        )
        .unwrap();
        assert_eq!(path, std::path::PathBuf::from("custom.toml"));
        assert_eq!(remaining, [OsString::from("agents")]);

        let (path, remaining) = extract_config_path(
            [
                OsString::from("ask"),
                OsString::from("explorer"),
                OsString::from("--config"),
                OsString::from("custom.toml"),
                OsString::from("hello"),
            ]
            .into_iter(),
        )
        .unwrap();
        assert_eq!(path, std::path::PathBuf::from("custom.toml"));
        assert_eq!(
            remaining,
            [
                OsString::from("ask"),
                OsString::from("explorer"),
                OsString::from("hello")
            ]
        );
    }

    #[test]
    fn defaults_the_config_path_when_the_flag_is_absent() {
        let (path, remaining) =
            extract_config_path([OsString::from("agents")].into_iter()).unwrap();
        assert_eq!(path, std::path::PathBuf::from("tapet.toml"));
        assert_eq!(remaining, [OsString::from("agents")]);
    }

    #[test]
    fn rejects_a_dangling_or_repeated_config_flag() {
        assert!(extract_config_path([OsString::from("--config")].into_iter()).is_err());
        assert!(
            extract_config_path(
                [
                    OsString::from("--config"),
                    OsString::from("one.toml"),
                    OsString::from("--config"),
                    OsString::from("two.toml"),
                ]
                .into_iter()
            )
            .is_err()
        );
    }

    #[test]
    fn parses_agent_listing_command() {
        assert_eq!(
            selected_command([OsString::from("agents")].into_iter()).unwrap(),
            Command::Agents
        );
    }

    #[test]
    fn parses_template_and_room_listing_commands() {
        assert_eq!(
            selected_command([OsString::from("templates")].into_iter()).unwrap(),
            Command::Templates
        );
        assert!(
            selected_command([OsString::from("templates"), OsString::from("extra")].into_iter())
                .is_err()
        );
        assert_eq!(
            selected_command([OsString::from("rooms")].into_iter()).unwrap(),
            Command::Rooms
        );
        assert!(
            selected_command([OsString::from("rooms"), OsString::from("extra")].into_iter())
                .is_err()
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
                source: RoomSource::With(vec!["explorer".to_owned()]),
                name: None,
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
                source: RoomSource::With(vec!["explorer".to_owned(), "reviewer".to_owned()]),
                name: None,
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
                source: RoomSource::From("research".to_owned()),
                name: None,
            }
        );

        let named = "sweaty-warroom".parse().unwrap();
        assert_eq!(
            selected_command(
                [
                    OsString::from("room"),
                    OsString::from("--name"),
                    OsString::from("sweaty-warroom"),
                    OsString::from("--from"),
                    OsString::from("research"),
                ]
                .into_iter()
            )
            .unwrap(),
            Command::Room {
                source: RoomSource::From("research".to_owned()),
                name: Some(named),
            }
        );

        let name = "sweaty-warroom";
        let room: crate::room::RoomName = name.parse().unwrap();
        assert_eq!(
            selected_command([OsString::from("enter"), OsString::from(name)].into_iter()).unwrap(),
            Command::Enter { room: room.clone() }
        );
        assert_eq!(
            selected_command([OsString::from("history"), OsString::from(name)].into_iter())
                .unwrap(),
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
            selected_command([OsString::from("enter"), OsString::from("Not_A_Room")].into_iter())
                .is_err()
        );
        assert!(
            selected_command(
                [
                    OsString::from("room"),
                    OsString::from("--name"),
                    OsString::from("Bad Name"),
                    OsString::from("--with"),
                    OsString::from("explorer"),
                ]
                .into_iter()
            )
            .is_err()
        );
        assert!(
            selected_command(
                [
                    OsString::from("room"),
                    OsString::from("--name"),
                    OsString::from("one"),
                    OsString::from("--name"),
                    OsString::from("two"),
                    OsString::from("--with"),
                    OsString::from("explorer"),
                ]
                .into_iter()
            )
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
    fn agent_listing_is_column_aligned_by_name_and_provider() {
        let temporary = TempDir::new().unwrap();
        let path = temporary.path().join("tapet.toml");
        fs::write(
            &path,
            concat!(
                "version = 1\n",
                "[providers.openai]\ntype = \"openai\"\napi_key_env = \"KEY\"\n",
                "[models.primary]\nprovider = \"openai\"\nmodel = \"gpt-test\"\n",
                "[agents.ex]\nmodel = \"primary\"\nprompt = \"Explore\"\n",
                "[agents.explorer]\nmodel = \"primary\"\nprompt = \"Explore\"\n"
            ),
        )
        .unwrap();
        let config = Config::load(path).unwrap();
        let mut output = Vec::new();

        write_agents(config.agents(), &mut output).unwrap();

        let expected = format!(
            "{:<8}  {:<6}  {}\n{:<8}  {:<6}  {}\n",
            "ex", "openai", "gpt-test", "explorer", "openai", "gpt-test"
        );
        assert_eq!(String::from_utf8(output).unwrap(), expected);
    }

    #[test]
    fn template_listing_is_column_aligned_with_default_agent_first() {
        let temporary = TempDir::new().unwrap();
        let path = temporary.path().join("tapet.toml");
        fs::write(
            &path,
            concat!(
                "version = 1\n",
                "[providers.openai]\ntype = \"openai\"\napi_key_env = \"KEY\"\n",
                "[models.primary]\nprovider = \"openai\"\nmodel = \"gpt-test\"\n",
                "[agents.explorer]\nmodel = \"primary\"\nprompt = \"Explore\"\n",
                "[agents.doubter]\nmodel = \"primary\"\nprompt = \"Doubt\"\n",
                "[rooms.research]\nagents = [\"explorer\", \"doubter\"]\n",
                "default = \"explorer\"\ndescription = \"Research\"\nprompt = \"Cite evidence\"\n"
            ),
        )
        .unwrap();
        let config = Config::load(path).unwrap();
        let mut output = Vec::new();

        write_templates(config.rooms(), &mut output).unwrap();

        let expected = format!(
            "{:<8}  {:<8}  {}\n",
            "research", "explorer", "explorer, doubter"
        );
        assert_eq!(String::from_utf8(output).unwrap(), expected);
    }

    #[test]
    fn room_listing_is_column_aligned() {
        let rooms = [
            RoomSummary::fixture("sweaty-warroom", &["explorer", "doubter"]),
            RoomSummary::fixture("haunted-basement", &["duck"]),
        ];
        let mut output = Vec::new();

        write_room_list(&rooms, &mut output).unwrap();

        let expected = format!(
            "{:<16}  {}\n{:<16}  {}\n",
            "sweaty-warroom", "explorer, doubter", "haunted-basement", "duck"
        );
        assert_eq!(String::from_utf8(output).unwrap(), expected);
    }

    #[tokio::test]
    async fn printed_deltas_are_the_stored_assistant_message() {
        let events = stream::iter([
            Ok(StreamEvent::TextDelta("Owner".to_owned())),
            Ok(StreamEvent::TextDelta("ship".to_owned())),
            Ok(StreamEvent::Completed(response_round())),
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
            Ok(StreamEvent::Completed(response_round())),
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
            Ok(StreamEvent::Completed(response_round())),
        ]);
        let mut output = Vec::new();

        let message = render_room_events(events, "explorer", &mut output)
            .await
            .unwrap();

        assert_eq!(message.text, "@exploration continues");
        assert_eq!(String::from_utf8(output).unwrap(), message.text);
    }

    #[tokio::test]
    async fn room_tool_proposals_are_returned_for_the_execution_loop() {
        let events = stream::iter([
            Ok(StreamEvent::ToolCallProposed(ToolCall {
                call_id: "call_1".to_owned(),
                name: "read_file".to_owned(),
                arguments: "{\"path\":\"Cargo.toml\"}".to_owned(),
            })),
            Ok(StreamEvent::Completed(response_round())),
        ]);
        let mut output = Vec::new();

        let message = render_room_events(events, "explorer", &mut output)
            .await
            .unwrap();

        assert!(String::from_utf8(output).unwrap().is_empty());
        assert!(message.text.is_empty());
        assert_eq!(message.tool_calls.len(), 1);
        assert_eq!(message.tool_calls[0].name, "read_file");
        assert_eq!(message.tool_calls[0].arguments, "{\"path\":\"Cargo.toml\"}");
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

    fn response_round() -> ResponseRound {
        ResponseRound {
            completion: Completion {
                provider_response_id: Some("resp_test".to_owned()),
                input_tokens: 1,
                output_tokens: 1,
            },
            output_items: Vec::new(),
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
