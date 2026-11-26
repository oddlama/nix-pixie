//! What the operator at the target sees. The agent's messages go to its
//! stdout (the journal, and the console where the unit sends them there)
//! and into a [`Status`] that `nix-pixie console` shows on the installer's
//! tty1, by way of the control socket.

use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use tokio::sync::watch;

/// The most messages a [`Status`] keeps.
const MAX_MESSAGES: usize = 50;

/// Everything the target's screen shows.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    /// The pairing code shown, while one is, and what to run on the
    /// controller for it.
    pub code: Option<Shown>,
    /// How the relay stands, with relays.
    pub relay: Option<String>,
    /// The latest messages, oldest first.
    pub messages: Vec<String>,
    /// How many messages were said in all, kept or not: the last
    /// `said - previous said` of `messages` are new.
    pub said: u64,
}

/// A pairing code on the screen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shown {
    pub code: String,
    /// The controller's command line for it.
    pub command: String,
}

fn status() -> &'static watch::Sender<Status> {
    static STATUS: OnceLock<watch::Sender<Status>> = OnceLock::new();
    STATUS.get_or_init(|| watch::Sender::new(Status::default()))
}

/// Tells the operator `text`.
pub fn say(text: impl Into<String>) {
    let text = text.into();
    println!("\n    {text}\n");
    status().send_modify(|status| {
        status.said += 1;
        status.messages.push(text);
        let excess = status.messages.len().saturating_sub(MAX_MESSAGES);
        status.messages.drain(..excess);
    });
}

/// Shows a code, or none (`None`) once it is no use any more.
pub fn code(shown: Option<Shown>) {
    if let Some(shown) = &shown {
        println!(
            "\n    nix-pixie pairing code:  {}\n\n    On the controller: {}\n",
            shown.code, shown.command
        );
    }
    status().send_modify(|status| status.code = shown);
}

/// How the relay stands, one line (`None`: no relays).
pub fn relay(line: Option<String>) {
    status().send_modify(|status| status.relay = line);
}

/// The screen as it is now, and as it changes.
pub fn watch() -> watch::Receiver<Status> {
    status().subscribe()
}
