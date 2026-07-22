mod config;

use config::{Config, ConfigError};
use std::env;
use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::io::{self, BufRead, Write};
use std::path::Path;
use std::process;

const CONFIG_PATH: &str = "tapet.toml";
const USAGE: &str = "usage: tapet <agent>";

#[derive(Debug, PartialEq, Eq)]
enum LineAction<'a> {
    Echo(&'a str),
    Ignore,
    Exit,
}

fn process_line(line: &str) -> LineAction<'_> {
    let line = line.trim_end_matches(['\r', '\n']);

    match line {
        "" => LineAction::Ignore,
        "/exit" => LineAction::Exit,
        message => LineAction::Echo(message),
    }
}

fn run(agent_name: &str, mut input: impl BufRead, mut output: impl Write) -> io::Result<()> {
    writeln!(output, "{agent_name}> ready")?;

    let mut line = String::new();
    loop {
        write!(output, "you> ")?;
        output.flush()?;

        line.clear();
        if input.read_line(&mut line)? == 0 {
            return Ok(());
        }

        match process_line(&line) {
            LineAction::Echo(message) => writeln!(output, "{agent_name}> {message}")?,
            LineAction::Ignore => {}
            LineAction::Exit => return Ok(()),
        }
    }
}

fn selected_agent_name(mut args: impl Iterator<Item = OsString>) -> Result<String, AppError> {
    let name = args.next().ok_or(AppError::Usage)?;
    if args.next().is_some() {
        return Err(AppError::Usage);
    }

    name.into_string().map_err(|_| AppError::Usage)
}

fn try_main() -> Result<(), AppError> {
    let agent_name = selected_agent_name(env::args_os().skip(1))?;
    let config = Config::load(Path::new(CONFIG_PATH))?;
    let agent = config.agent(&agent_name)?;

    // Stage 2 resolves the prompt but intentionally does not send it anywhere.
    let _initial_prompt = agent.prompt();

    let stdin = io::stdin();
    let stdout = io::stdout();
    run(agent.name(), stdin.lock(), stdout.lock())?;
    Ok(())
}

fn main() {
    if let Err(error) = try_main() {
        eprintln!("tapet: {error}");
        process::exit(1);
    }
}

#[derive(Debug)]
enum AppError {
    Usage,
    Config(ConfigError),
    Io(io::Error),
}

impl fmt::Display for AppError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage => formatter.write_str(USAGE),
            Self::Config(error) => error.fmt(formatter),
            Self::Io(error) => error.fmt(formatter),
        }
    }
}

impl Error for AppError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Usage => None,
            Self::Config(error) => Some(error),
            Self::Io(error) => Some(error),
        }
    }
}

impl From<ConfigError> for AppError {
    fn from(error: ConfigError) -> Self {
        Self::Config(error)
    }
}

impl From<io::Error> for AppError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[cfg(test)]
mod tests {
    use super::{LineAction, process_line, run, selected_agent_name};
    use std::ffi::OsString;
    use std::io::Cursor;

    #[test]
    fn messages_are_echoed_without_the_line_ending() {
        assert_eq!(process_line("hello\n"), LineAction::Echo("hello"));
        assert_eq!(process_line("hello\r\n"), LineAction::Echo("hello"));
    }

    #[test]
    fn empty_lines_are_ignored() {
        assert_eq!(process_line("\n"), LineAction::Ignore);
        assert_eq!(process_line("\r\n"), LineAction::Ignore);
    }

    #[test]
    fn exit_command_stops_the_conversation() {
        assert_eq!(process_line("/exit\n"), LineAction::Exit);
    }

    #[test]
    fn repl_echoes_messages_with_the_agent_name_and_stops_at_exit() {
        let input = Cursor::new("hello\n\n/exit\nnot read\n");
        let mut output = Vec::new();

        run("explorer", input, &mut output).unwrap();

        assert_eq!(
            String::from_utf8(output).unwrap(),
            "explorer> ready\nyou> explorer> hello\nyou> you> "
        );
    }

    #[test]
    fn repl_exits_cleanly_at_end_of_input() {
        let input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::new();

        run("explorer", input, &mut output).unwrap();

        assert_eq!(String::from_utf8(output).unwrap(), "explorer> ready\nyou> ");
    }

    #[test]
    fn exactly_one_agent_name_is_required() {
        assert!(selected_agent_name(std::iter::empty()).is_err());
        assert!(
            selected_agent_name([OsString::from("explorer"), OsString::from("extra")].into_iter())
                .is_err()
        );
        assert_eq!(
            selected_agent_name([OsString::from("explorer")].into_iter()).unwrap(),
            "explorer"
        );
    }
}
