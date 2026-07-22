mod config;
mod conversation;
mod message;
mod openai;
mod session;
mod store;
mod stream;
mod terminal;

use config::{Config, ConfigError};
use conversation::Conversation;
use futures_util::{Stream, StreamExt};
use message::{Message, MessageRole};
use openai::{OpenAiClient, ProviderError};
use session::{AgentSnapshot, SessionId, SessionIdError};
use std::env;
use std::ffi::OsString;
use std::io::{self, Write};
use std::path::Path;
use std::process;
use store::{Store, StoreError};
use stream::{Completion, StreamEvent};
use terminal::InputSuppression;
use thiserror::Error;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader};

const CONFIG_PATH: &str = "tapet.toml";
const DATABASE_PATH: &str = ".tapet/tapet.db";

#[derive(Debug, Eq, PartialEq)]
enum Command {
    Ask { agent: String, message: String },
    Chat { agent: String },
    Attach { session: SessionId },
    History { session: SessionId },
}

fn selected_command(mut args: impl Iterator<Item = OsString>) -> Result<Command, AppError> {
    let subcommand = args
        .next()
        .ok_or(AppError::Usage)?
        .into_string()
        .map_err(|_| AppError::Usage)?;

    match subcommand.as_str() {
        "ask" => {
            let agent = next_argument(&mut args)?;
            let message = next_argument(&mut args)?;
            if message.is_empty() || args.next().is_some() {
                return Err(AppError::Usage);
            }
            Ok(Command::Ask { agent, message })
        }
        "chat" => {
            let agent = next_argument(&mut args)?;
            if args.next().is_some() {
                return Err(AppError::Usage);
            }
            Ok(Command::Chat { agent })
        }
        "attach" => {
            let session = next_argument(&mut args)?.parse()?;
            if args.next().is_some() {
                return Err(AppError::Usage);
            }
            Ok(Command::Attach { session })
        }
        "history" => {
            let session = next_argument(&mut args)?.parse()?;
            if args.next().is_some() {
                return Err(AppError::Usage);
            }
            Ok(Command::History { session })
        }
        _ => Err(AppError::Usage),
    }
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
        Command::Ask {
            agent: agent_name,
            message,
        } => {
            let config = Config::load(Path::new(CONFIG_PATH))?;
            let agent = config.agent(&agent_name)?;
            let client = OpenAiClient::from_config(config.openai())?;
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
        Command::Chat { agent: agent_name } => {
            let config = Config::load(Path::new(CONFIG_PATH))?;
            let agent = config.agent(&agent_name)?;
            let snapshot = AgentSnapshot::resolve(agent, config.openai());
            let store = Store::open(DATABASE_PATH).await?;
            let session = store.create_session(snapshot).await?;
            let client = client_for_session(&session)?;
            let input = BufReader::new(tokio::io::stdin());
            let stdout = io::stdout();
            let mut output = stdout.lock();
            writeln!(output, "Session {}", session.id())?;
            run_chat(Conversation::new(store, session), &client, input, output).await?;
        }
        Command::Attach { session: id } => {
            let store = Store::open(DATABASE_PATH).await?;
            let session = store.load_session(&id).await?;
            let client = client_for_session(&session)?;
            let input = BufReader::new(tokio::io::stdin());
            let stdout = io::stdout();
            run_chat(
                Conversation::new(store, session),
                &client,
                input,
                stdout.lock(),
            )
            .await?;
        }
        Command::History { session: id } => {
            let store = Store::open(DATABASE_PATH).await?;
            let session = store.load_session(&id).await?;
            let messages = store.history(&id).await?;
            let stdout = io::stdout();
            let mut output = stdout.lock();
            write_history(session.agent().agent_name(), &messages, &mut output)?;
        }
    }

    Ok(())
}

fn client_for_session(session: &session::Session) -> Result<OpenAiClient, ProviderError> {
    let agent = session.agent();
    OpenAiClient::from_settings(agent.base_url(), agent.api_key_env(), agent.model())
}

fn write_history(
    agent_name: &str,
    messages: &[Message],
    output: &mut impl Write,
) -> io::Result<()> {
    for message in messages {
        let speaker = match message.role() {
            MessageRole::User => "you",
            MessageRole::Assistant => agent_name,
        };
        writeln!(output, "{speaker}> {}", message.content())?;
    }
    Ok(())
}

async fn run_chat(
    conversation: Conversation,
    client: &OpenAiClient,
    mut input: impl AsyncBufRead + Unpin,
    mut output: impl Write,
) -> Result<(), AppError> {
    let agent = conversation.session().agent();
    writeln!(output, "{}> ready", agent.agent_name())?;

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

        let input_suppression = InputSuppression::start()?;
        let turn = conversation.begin_turn(message).await?;
        let outcome = tokio::select! {
            result = render_response(
                agent.agent_name(),
                agent.system_prompt(),
                client,
                turn.messages(),
                &mut output,
            ) => TurnOutcome::Response(result),
            result = tokio::signal::ctrl_c() => TurnOutcome::Interrupted(result),
        };
        drop(input_suppression);

        match outcome {
            TurnOutcome::Response(Ok(response)) => {
                turn.complete(response.text, response.completion).await?;
            }
            TurnOutcome::Response(Err(error)) => {
                turn.fail(error.to_string()).await?;
                return Err(error);
            }
            TurnOutcome::Interrupted(signal) => {
                let failure = match &signal {
                    Ok(()) => "cancelled by user".to_owned(),
                    Err(error) => format!("failed to listen for Ctrl-C: {error}"),
                };
                turn.fail(failure).await?;
                signal?;
                writeln!(output)?;
                return Ok(());
            }
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
        "usage: tapet ask <agent> <message>\n       tapet chat <agent>\n       tapet attach <session>\n       tapet history <session>"
    )]
    Usage,
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error(transparent)]
    SessionId(#[from] SessionIdError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}

#[cfg(test)]
mod tests {
    use super::{
        Command, UserInput, read_user_input, render_events, selected_command, write_history,
    };
    use crate::message::Message;
    use crate::openai::{ProviderError, decode_stream};
    use crate::session::SessionId;
    use crate::stream::{Completion, StreamEvent};
    use futures_util::stream;
    use std::ffi::OsString;
    use std::io::{self, Write};
    use tokio::io::BufReader;

    #[test]
    fn parses_ask_and_chat_commands() {
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
        assert_eq!(
            selected_command([OsString::from("chat"), OsString::from("explorer")].into_iter())
                .unwrap(),
            Command::Chat {
                agent: "explorer".to_owned()
            }
        );
    }

    #[test]
    fn parses_attach_and_history_commands() {
        let id = "ses_550e8400e29b41d4a716446655440000";
        let session: SessionId = id.parse().unwrap();

        assert_eq!(
            selected_command([OsString::from("attach"), OsString::from(id)].into_iter()).unwrap(),
            Command::Attach {
                session: session.clone()
            }
        );
        assert_eq!(
            selected_command([OsString::from("history"), OsString::from(id)].into_iter()).unwrap(),
            Command::History { session }
        );
    }

    #[test]
    fn rejects_incomplete_or_extra_arguments() {
        assert!(selected_command(std::iter::empty()).is_err());
        assert!(selected_command([OsString::from("ask")].into_iter()).is_err());
        assert!(selected_command([OsString::from("chat")].into_iter()).is_err());
        assert!(
            selected_command(
                [OsString::from("attach"), OsString::from("not-a-session")].into_iter()
            )
            .is_err()
        );
        assert!(
            selected_command(
                [
                    OsString::from("chat"),
                    OsString::from("explorer"),
                    OsString::from("extra")
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
    fn formats_session_history_with_speaker_names() {
        let mut output = Vec::new();

        write_history(
            "explorer",
            &[Message::user("Hello"), Message::assistant("Hi")],
            &mut output,
        )
        .unwrap();

        assert_eq!(
            String::from_utf8(output).unwrap(),
            "you> Hello\nexplorer> Hi\n"
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
