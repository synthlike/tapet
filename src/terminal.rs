use std::io;

pub struct InputSuppression {
    #[cfg(unix)]
    _terminal: Option<UnixTerminal>,
}

impl InputSuppression {
    pub fn start() -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::io::IsTerminal;

            let terminal = if io::stdin().is_terminal() {
                Some(UnixTerminal::suppress_input()?)
            } else {
                None
            };
            Ok(Self {
                _terminal: terminal,
            })
        }

        #[cfg(not(unix))]
        {
            Ok(Self {})
        }
    }
}

#[cfg(unix)]
struct UnixTerminal {
    original: libc::termios,
}

#[cfg(unix)]
impl UnixTerminal {
    fn suppress_input() -> io::Result<Self> {
        let mut original = std::mem::MaybeUninit::uninit();
        // SAFETY: `original` points to writable storage for a `termios` value.
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, original.as_mut_ptr()) } == -1 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `tcgetattr` succeeded, so it initialized `original`.
        let original = unsafe { original.assume_init() };
        let mut suppressed = original;
        suppressed.c_lflag &= !(libc::ECHO | libc::ECHONL);

        // SAFETY: `suppressed` is initialized and the file descriptor is stdin.
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &suppressed) } == -1 {
            return Err(io::Error::last_os_error());
        }

        Ok(Self { original })
    }
}

#[cfg(unix)]
impl Drop for UnixTerminal {
    fn drop(&mut self) {
        // SAFETY: both calls receive stdin and a live, initialized `termios` value.
        unsafe {
            libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH);
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.original);
        }
    }
}
