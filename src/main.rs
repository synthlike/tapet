mod config;
mod conversation;
mod message;
mod openai;

use config::{Agent, Config, ConfigError};
use conversation::Conversation;
use message::Message;
use openai::{OpenAiClient, ProviderError};
use std::env;
use std::ffi::OsString;
use std::io::{self, Write};
use std::path::Path;
use std::process;
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
            let response = client.complete(agent.prompt(), &messages).await?;

            let stdout = io::stdout();
            writeln!(stdout.lock(), "{}> {response}", agent.name())?;
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

        let prompt = agent.prompt();
        let response = tokio::select! {
            result = conversation.turn(message, |messages| async move {
                client.complete(prompt, &messages).await
            }) => result?,
            result = tokio::signal::ctrl_c() => {
                result?;
                writeln!(output)?;
                return Ok(());
            }
        };

        writeln!(output, "{}> {response}", agent.name())?;
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
    use super::{Command, UserInput, read_user_input, selected_command};
    use std::ffi::OsString;
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
}
