//! Hooks: programs from the user's flake that the controller runs at points
//! of the install, ordered by `after` and `before` against the install's own
//! steps and against each other.
//!
//! The steps are nodes of the same graph, chained in their fixed order, so
//! `after = [ "host-key" ]; before = [ "build" ]` places a hook between the
//! two. Every hook comes after `approve`. Among everything that may run
//! next, hooks go first (by name), so each runs as early as its constraints
//! allow.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use anyhow::{Result, bail};

use crate::metadata::Hook;

/// The install's own steps, in the order they happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Step {
    /// The operator approved the target; its inventory is known.
    Approve,
    /// The facter report is in the repository (if the machine wants one).
    Facter,
    /// The pulled host key is recorded (in pull mode).
    HostKey,
    /// The disks are chosen, and recorded in pinned mode.
    Disks,
    /// The disko script and the system are built.
    Build,
    /// disko formatted the disks; the installed system's root is mounted.
    Format,
    /// The system is in the target's store.
    Copy,
    /// `nixos-install` set the profile and installed the boot loader.
    Install,
    /// Unmounted and exported; the target reboots or powers off.
    Finish,
    /// The installed system answered over SSH, with the expected key.
    Boot,
}

impl Step {
    pub const ALL: [Self; 10] = [
        Self::Approve,
        Self::Facter,
        Self::HostKey,
        Self::Disks,
        Self::Build,
        Self::Format,
        Self::Copy,
        Self::Install,
        Self::Finish,
        Self::Boot,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::Facter => "facter",
            Self::HostKey => "host-key",
            Self::Disks => "disks",
            Self::Build => "build",
            Self::Format => "format",
            Self::Copy => "copy",
            Self::Install => "install",
            Self::Finish => "finish",
            Self::Boot => "boot",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.name() == name)
    }
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Which hooks run right after each step, in order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Schedule {
    after: BTreeMap<Step, Vec<String>>,
}

impl Schedule {
    /// The hooks to run once `step` is done.
    pub fn after(&self, step: Step) -> &[String] {
        self.after.get(&step).map_or(&[], Vec::as_slice)
    }

    /// Every hook with the step it follows, in running order.
    pub fn all(&self) -> impl Iterator<Item = (Step, &str)> {
        self.after
            .iter()
            .flat_map(|(step, hooks)| hooks.iter().map(move |h| (*step, h.as_str())))
    }
}

/// A node of the ordering graph.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Node {
    // Hooks sort before steps: they go first when both may run.
    Hook(String),
    Step(Step),
}

/// Orders `hooks` against the steps and each other. Refuses unknown names,
/// hooks named like steps, hooks before `approve`, and cycles.
pub fn schedule(hooks: &BTreeMap<String, Hook>) -> Result<Schedule> {
    let resolve = |hook: &str, name: &str| -> Result<Node> {
        if let Some(step) = Step::from_name(name) {
            return Ok(Node::Step(step));
        }
        if hooks.contains_key(name) {
            return Ok(Node::Hook(name.to_string()));
        }
        let steps: Vec<&str> = Step::ALL.iter().map(|s| s.name()).collect();
        bail!(
            "hook {hook}: `{name}` is neither a step ({}) nor a hook",
            steps.join(", ")
        )
    };

    let mut edges: BTreeMap<Node, BTreeSet<Node>> = BTreeMap::new();
    let mut edge = |from: Node, to: Node| {
        edges.entry(from).or_default().insert(to);
    };
    for pair in Step::ALL.windows(2) {
        if let [a, b] = pair {
            edge(Node::Step(*a), Node::Step(*b));
        }
    }
    for (name, hook) in hooks {
        if Step::from_name(name).is_some() {
            bail!("hook {name} is named like a step of the install; rename it");
        }
        let me = Node::Hook(name.clone());
        edge(Node::Step(Step::Approve), me.clone());
        for after in &hook.after {
            edge(resolve(name, after)?, me.clone());
        }
        for before in &hook.before {
            let target = resolve(name, before)?;
            if target == Node::Step(Step::Approve) {
                bail!("hook {name} can't run before `approve`: hooks need an approved target");
            }
            edge(me.clone(), target);
        }
    }

    // Kahn's algorithm; the smallest ready node goes next (hooks first).
    let nodes: BTreeSet<Node> = Step::ALL
        .iter()
        .map(|s| Node::Step(*s))
        .chain(hooks.keys().map(|h| Node::Hook(h.clone())))
        .collect();
    let mut incoming: BTreeMap<&Node, usize> = nodes.iter().map(|n| (n, 0)).collect();
    for targets in edges.values() {
        for target in targets {
            if let Some(count) = incoming.get_mut(target) {
                *count += 1;
            }
        }
    }
    let mut ready: BTreeSet<&Node> = incoming
        .iter()
        .filter(|(_, count)| **count == 0)
        .map(|(node, _)| *node)
        .collect();
    let mut schedule = Schedule::default();
    let mut last_step = None;
    let mut done = 0;
    while let Some(node) = ready.pop_first() {
        done += 1;
        match node {
            Node::Step(step) => last_step = Some(*step),
            Node::Hook(hook) => {
                // Every hook follows `approve`, so a step was done before it.
                let step = last_step.unwrap_or(Step::Approve);
                schedule.after.entry(step).or_default().push(hook.clone());
            }
        }
        for target in edges.get(node).into_iter().flatten() {
            if let Some(count) = incoming.get_mut(target) {
                *count -= 1;
                if *count == 0 {
                    ready.insert(target);
                }
            }
        }
    }
    if done < nodes.len() {
        let stuck: Vec<String> = incoming
            .iter()
            .filter(|(_, count)| **count > 0)
            .map(|(node, _)| match node {
                Node::Hook(h) => format!("hook {h}"),
                Node::Step(s) => format!("step {s}"),
            })
            .collect();
        bail!(
            "the hooks' `after` and `before` form a cycle, involving {}",
            stuck.join(", ")
        );
    }
    Ok(schedule)
}
