//! The operator's answers on stdin, read without ever blocking.
//!
//! A blocking read can't be cancelled: it would keep the process from
//! exiting, and take the next line from a hook that reads the terminal
//! itself. So stdin is only read once `poll` says a line is there (on a
//! terminal in canonical mode, readable means a whole line), and only while
//! somebody waits for one, or to drop what was typed ahead on the terminal.

use std::collections::VecDeque;
use std::io::IsTerminal;

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::io::Errno;
use rustix::termios::{QueueSelector, tcflush, tcgetpgrp};

/// Lines read from stdin that nobody took yet, and whether it ended.
#[derive(Debug)]
pub struct Input {
    /// Whether stdin is a terminal, which echoes what is typed.
    pub tty: bool,
    partial: Vec<u8>,
    /// `None` for a Ctrl-D on a terminal: no answer to one question.
    lines: VecDeque<Option<String>>,
    /// Stdin ended for good: a pipe or file at its end, or a terminal that
    /// hung up.
    eof: bool,
    /// How many are waiting for a line; stdin is only read for them.
    pub waiting: usize,
}

/// What [`Input::service`] found.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Serviced {
    /// Line breaks read. On a terminal that echoes, each moved the cursor
    /// down a line.
    pub newlines: usize,
    /// Whether a waiting line or the end of input is there now.
    pub answered: bool,
}

impl Default for Input {
    fn default() -> Self {
        Self::new(std::io::stdin().is_terminal())
    }
}

impl Input {
    pub fn new(tty: bool) -> Self {
        Self {
            tty,
            partial: Vec::new(),
            lines: VecDeque::new(),
            eof: false,
            waiting: 0,
        }
    }

    /// Takes whatever stdin has right now, without blocking. Complete lines
    /// are kept if `keep`, and dropped otherwise (input nobody asked for).
    pub fn service(&mut self, keep: bool) -> Serviced {
        if self.eof {
            return Serviced::default();
        }
        let stdin = std::io::stdin();
        let mut fds = [PollFd::new(&stdin, PollFlags::IN)];
        let now = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        if !matches!(poll(&mut fds, Some(&now)), Ok(n) if n > 0) {
            return Serviced::default();
        }
        let hangup = fds
            .first()
            .is_some_and(|fd| fd.revents().intersects(PollFlags::HUP | PollFlags::ERR));
        let mut buf = [0_u8; 4096];
        match rustix::io::read(&stdin, &mut buf[..]) {
            Ok(0) => self.nothing(keep, hangup),
            Ok(n) => self.take(buf.get(..n).unwrap_or_default(), keep),
            Err(Errno::INTR | Errno::AGAIN) => Serviced::default(),
            // The terminal hung up, or stdin is unreadable: no more answers.
            Err(_) => {
                self.end(keep);
                Serviced {
                    newlines: 0,
                    answered: true,
                }
            }
        }
    }

    /// Splits `data` into lines.
    pub fn take(&mut self, data: &[u8], keep: bool) -> Serviced {
        let mut newlines = 0;
        for &byte in data {
            if byte == b'\n' {
                newlines += 1;
                let line = std::mem::take(&mut self.partial);
                if keep {
                    self.lines.push_back(Some(decode(&line)));
                }
            } else {
                self.partial.push(byte);
            }
        }
        Serviced {
            newlines,
            answered: keep && !self.lines.is_empty(),
        }
    }

    /// A read of nothing. Ctrl-D on a terminal ends one answer; a pipe or
    /// file at its end, or a terminal that hung up, ends all input.
    pub fn nothing(&mut self, keep: bool, hangup: bool) -> Serviced {
        if self.tty && !hangup {
            if keep {
                self.lines.push_back(None);
            }
            return Serviced {
                newlines: 0,
                answered: keep,
            };
        }
        self.end(keep);
        Serviced {
            newlines: 0,
            answered: true,
        }
    }

    /// Whether input ended for good.
    pub fn ended(&self) -> bool {
        self.eof
    }

    fn end(&mut self, keep: bool) {
        self.eof = true;
        let rest = std::mem::take(&mut self.partial);
        if keep && !rest.is_empty() {
            self.lines.push_back(Some(decode(&rest)));
        }
    }

    /// The next line; `Err(())` for no answer (Ctrl-D on a terminal, or
    /// the end of input once every line is taken); `None` while nothing is
    /// there yet.
    pub fn next_line(&mut self) -> Option<Result<String, ()>> {
        match self.lines.pop_front() {
            Some(Some(line)) => Some(Ok(line)),
            Some(None) => Some(Err(())),
            None if self.eof => Some(Err(())),
            None => None,
        }
    }

    /// Forgets what was typed ahead on a terminal, before a question whose
    /// answer should be typed after reading it. Input from a pipe or a file
    /// is a script's answers in order, and kept.
    pub fn drop_typeahead(&mut self) {
        if !self.tty {
            return;
        }
        let _ = tcflush(std::io::stdin(), QueueSelector::IFlush);
        self.lines.clear();
        self.partial.clear();
    }
}

fn decode(line: &[u8]) -> String {
    let line = String::from_utf8_lossy(line);
    line.strip_suffix('\r').unwrap_or(&line).to_string()
}

/// Whether the terminal on stdin echoes what is typed.
pub fn echoes() -> bool {
    rustix::termios::tcgetattr(std::io::stdin())
        .is_ok_and(|t| t.local_modes.contains(rustix::termios::LocalModes::ECHO))
}

/// Whether this process may read the terminal on stdin: reading it from the
/// background stops the process (SIGTTIN).
pub fn foreground() -> bool {
    tcgetpgrp(std::io::stdin()).is_ok_and(|group| group == rustix::process::getpgrp())
}
