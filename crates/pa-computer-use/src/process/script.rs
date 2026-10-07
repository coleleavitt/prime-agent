//! The scripted process environment (the skill's `fakes_linux.X11Script`):
//! records every argv and answers with canned results; a PNG rule writes a
//! minimal PNG to the argv's last element, like the real capture tools.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use super::{CommandOutput, RunError, Tools};

/// One canned answer for argvs whose tail starts with `prefix`.
#[derive(Clone)]
struct Rule {
    prefix: Vec<String>,
    answer: Answer,
}

#[derive(Clone)]
enum Answer {
    Output(CommandOutput, Option<(u32, u32)>),
    Error(RunError),
}

#[derive(Default)]
struct State {
    calls: Vec<Vec<String>>,
    rules: Vec<Rule>,
    tools: Vec<String>,
    files: Vec<String>,
    sockets: Vec<String>,
    env: HashMap<String, String>,
}

/// The scripted environment; clones share state.
#[derive(Clone, Default)]
pub(crate) struct Script {
    state: Arc<Mutex<State>>,
}

/// A minimal PNG header with the given IHDR dimensions.
pub(crate) fn png_bytes(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR".to_vec();
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes
}

impl Script {
    /// A script whose `PATH` holds `tools` (as `/usr/bin/<name>`).
    pub(crate) fn with_tools(tools: &[&str]) -> Self {
        let script = Self::default();
        script.lock().tools = tools.iter().map(ToString::to_string).collect();
        script
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn push(&self, prefix: &[&str], answer: Answer, first: bool) {
        let rule = Rule {
            prefix: prefix.iter().map(ToString::to_string).collect(),
            answer,
        };
        let mut state = self.lock();
        if first {
            state.rules.insert(0, rule);
        } else {
            state.rules.push(rule);
        }
    }

    /// Queue one canned result (the first matching rule answers).
    pub(crate) fn on(&self, prefix: &[&str], code: i32, stdout: &[u8], stderr: &[u8]) {
        self.push(
            prefix,
            Answer::Output(
                CommandOutput {
                    code,
                    stdout: stdout.to_vec(),
                    stderr: stderr.to_vec(),
                },
                None,
            ),
            false,
        );
    }

    /// Answer with success after writing a PNG of `size` to the argv's last element.
    pub(crate) fn on_png(&self, prefix: &[&str], size: (u32, u32)) {
        self.push(
            prefix,
            Answer::Output(CommandOutput::default(), Some(size)),
            false,
        );
    }

    pub(crate) fn on_error(&self, prefix: &[&str], error: RunError) {
        self.push(prefix, Answer::Error(error), false);
    }

    /// Serve `stdout` for `prefix` ahead of every earlier rule (a later
    /// call replaces the served output).
    pub(crate) fn serve_first(&self, prefix: &[&str], stdout: &str) {
        self.push(
            prefix,
            Answer::Output(
                CommandOutput {
                    code: 0,
                    stdout: stdout.as_bytes().to_vec(),
                    stderr: Vec::new(),
                },
                None,
            ),
            true,
        );
    }

    pub(crate) fn set_env(&self, key: &str, value: &str) {
        self.lock().env.insert(key.to_string(), value.to_string());
    }

    pub(crate) fn add_file(&self, path: &str) {
        self.lock().files.push(path.to_string());
    }

    pub(crate) fn add_socket(&self, path: &str) {
        self.lock().sockets.push(path.to_string());
    }

    /// Every recorded argv.
    pub(crate) fn calls(&self) -> Vec<Vec<String>> {
        self.lock().calls.clone()
    }

    /// The recorded argvs run through `tool` (by file name).
    pub(crate) fn tool_calls(&self, tool: &str) -> Vec<Vec<String>> {
        self.calls()
            .into_iter()
            .filter(|argv| {
                std::path::Path::new(&argv[0])
                    .file_name()
                    .is_some_and(|name| name == tool)
            })
            .collect()
    }
}

impl Tools for Script {
    fn run(&self, argv: &[String], _timeout: Duration) -> Result<CommandOutput, RunError> {
        let answer = {
            let mut state = self.lock();
            state.calls.push(argv.to_vec());
            state
                .rules
                .iter()
                .find(|rule| {
                    argv.get(1..)
                        .is_some_and(|tail| tail.starts_with(&rule.prefix))
                })
                .map(|rule| rule.answer.clone())
        };
        match answer {
            None => Ok(CommandOutput::default()),
            Some(Answer::Error(error)) => Err(error),
            Some(Answer::Output(output, png)) => {
                if let Some((width, height)) = png {
                    let target = std::path::Path::new(argv.last().expect("an argv"));
                    if let Some(parent) = target.parent() {
                        std::fs::create_dir_all(parent).expect("create the capture parent");
                    }
                    std::fs::write(target, png_bytes(width, height)).expect("write the fake PNG");
                }
                Ok(output)
            }
        }
    }

    fn which(&self, name: &str) -> Option<String> {
        let state = self.lock();
        state
            .tools
            .iter()
            .any(|tool| tool == name)
            .then(|| format!("/usr/bin/{name}"))
    }

    fn is_file(&self, path: &str) -> bool {
        self.lock().files.iter().any(|file| file == path)
    }

    fn is_socket(&self, path: &str) -> bool {
        self.lock().sockets.iter().any(|socket| socket == path)
    }

    fn env(&self, key: &str) -> Option<String> {
        self.lock().env.get(key).cloned()
    }
}
