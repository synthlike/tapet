mod config;
mod conversation;
mod message;
mod openai;
mod stream;
mod terminal;

use config::{Agent, Config, ConfigError};
use conversation::Conversation;
use futures_util::{Stream, StreamExt};
use message::Message;
use openai::{OpenAiClient, ProviderError};
use std::env;
use std::ffi::OsString;
use std::io::{self, Write};
use std::path::Path;
use std::process;
use stream::StreamEvent;
use terminal::InputSuppression;
use thiserror::Error;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader};

const CONFIG_PATH: &str = "tapet.toml";

#[derive(Debug, Eq, PartialEq)]
enum Command {
    Ask { agent: String, message: String },
    Chat { agent: String },
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
    let config = Config::load(Path::new(CONFIG_PATH))?;

    match command {
        Command::Ask {
            agent: agent_name,
            message,
        } => {
            let agent = config.agent(&agent_name)?;
            let client = OpenAiClient::from_config(config.openai())?;
            let messages = [Message::user(message)];
            let stdout = io::stdout();
            render_response(agent, &client, &messages, &mut stdout.lock()).await?;
        }
        Command::Chat { agent: agent_name } => {
            let agent = config.agent(&agent_name)?;
            let client = OpenAiClient::from_config(config.openai())?;
            let input = BufReader::new(tokio::io::stdin());
            let stdout = io::stdout();
            run_chat(agent, &client, input, stdout.lock()).await?;
        }
    }

    Ok(())
}

async fn run_chat(
    agent: &Agent,
    client: &OpenAiClient,
    mut input: impl AsyncBufRead + Unpin,
    mut output: impl Write,
) -> Result<(), AppError> {
    writeln!(output, "{}> ready", agent.name())?;
    let mut conversation = Conversation::new();

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
        let interrupted = tokio::select! {
            result = conversation.turn(message, |messages| {
                let output = &mut output;
                async move {
                    render_response(agent, client, &messages, output).await
                }
            }) => {
                result?;
                false
            },
            result = tokio::signal::ctrl_c() => {
                result?;
                true
            }
        };
        drop(input_suppression);

        if interrupted {
            writeln!(output)?;
            return Ok(());
        }
    }
}

async fn render_response(
    agent: &Agent,
    client: &OpenAiClient,
    messages: &[Message],
    output: &mut impl Write,
) -> Result<String, AppError> {
    let events = client.stream(agent.prompt(), messages).await?;
    write!(output, "{}> ", agent.name())?;
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

async fn render_events<S>(mut events: S, output: &mut impl Write) -> Result<String, AppError>
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
            StreamEvent::Completed(_) => return Ok(message),
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
    #[error("usage: tapet ask <agent> <message>\n       tapet chat <agent>")]
    Usage,
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}

#[cfg(test)]
mod tests {
    use super::{Command, UserInput, read_user_input, render_events, selected_command};
    use crate::openai::{ProviderError, decode_stream};
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
    fn rejects_incomplete_or_extra_arguments() {
        assert!(selected_command(std::iter::empty()).is_err());
        assert!(selected_command([OsString::from("ask")].into_iter()).is_err());
        assert!(selected_command([OsString::from("chat")].into_iter()).is_err());
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

    #[tokio::test]
    async fn printed_deltas_are_the_stored_assistant_message() {
        let events = stream::iter([
            Ok(StreamEvent::TextDelta("Owner".to_owned())),
            Ok(StreamEvent::TextDelta("ship".to_owned())),
            Ok(StreamEvent::Completed(completion())),
        ]);
        let mut output = TrackingWriter::default();

        let message = render_events(events, &mut output).await.unwrap();

        assert_eq!(message, "Ownership");
        assert_eq!(String::from_utf8(output.bytes).unwrap(), message);
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
