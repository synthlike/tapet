use std::io::{self, BufRead, Write};

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

fn run(mut input: impl BufRead, mut output: impl Write) -> io::Result<()> {
    let mut line = String::new();

    loop {
        write!(output, "you> ")?;
        output.flush()?;

        line.clear();
        if input.read_line(&mut line)? == 0 {
            return Ok(());
        }

        match process_line(&line) {
            LineAction::Echo(message) => writeln!(output, "tapet> {message}")?,
            LineAction::Ignore => {}
            LineAction::Exit => return Ok(()),
        }
    }
}

fn main() -> io::Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();

    run(stdin.lock(), stdout.lock())
}

#[cfg(test)]
mod tests {
    use super::{LineAction, process_line, run};
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
    fn repl_echoes_messages_and_stops_at_exit() {
        let input = Cursor::new("hello\n\n/exit\nnot read\n");
        let mut output = Vec::new();

        run(input, &mut output).unwrap();

        assert_eq!(
            String::from_utf8(output).unwrap(),
            "you> tapet> hello\nyou> you> "
        );
    }

    #[test]
    fn repl_exits_cleanly_at_end_of_input() {
        let input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::new();

        run(input, &mut output).unwrap();

        assert_eq!(String::from_utf8(output).unwrap(), "you> ");
    }
}
