// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use crate::protocol::{Bootstrap, Finish, Hello, Message, TestRecord};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::Mutex;
use std::time::Duration;

const BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(180);
const FINISH_TIMEOUT: Duration = Duration::from_secs(120);

/// Where test results go and where feature data comes from.
pub trait Backend: Send + Sync {
    fn bootstrap(&self, hello: &Hello) -> Result<Bootstrap, String>;
    fn send(&self, record: TestRecord);
    /// Blocks until every record sent so far is handled.
    fn finish(&self, finish: Finish);
}

/// Talks to `<agent> agent` over the child's stdin and stdout. Messages are
/// written to the pipe before `send` returns, so they reach the agent even if
/// the test binary exits right after.
pub struct AgentClient {
    stdin: Mutex<Option<ChildStdin>>,
    replies: Mutex<Receiver<Message>>,
    bootstrap_timeout: Duration,
    /// Not waited for: after `finished`, the agent only flushes telemetry.
    _child: Child,
}

impl AgentClient {
    pub fn spawn(program: &Path) -> Result<AgentClient, String> {
        let mut child = Command::new(program)
            .arg("agent")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("starting {}: {e}", program.display()))?;
        let stdin = child.stdin.take().ok_or("agent has no stdin")?;
        let stdout = child.stdout.take().ok_or("agent has no stdout")?;

        let (replies_tx, replies) = mpsc::channel();
        std::thread::Builder::new()
            .name("dd-test-agent-reader".into())
            .spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let Ok(line) = line else { return };
                    match Message::decode(&line) {
                        Ok(message) => {
                            if replies_tx.send(message).is_err() {
                                return;
                            }
                        }
                        Err(error) => warn!("invalid message from the Datadog test agent: {error}"),
                    }
                }
            })
            .map_err(|e| e.to_string())?;

        Ok(AgentClient {
            stdin: Mutex::new(Some(stdin)),
            replies: Mutex::new(replies),
            bootstrap_timeout: BOOTSTRAP_TIMEOUT,
            _child: child,
        })
    }

    fn write(&self, message: &Message) {
        let mut stdin = self.stdin.lock().unwrap_or_else(|e| e.into_inner());
        let Some(pipe) = stdin.as_mut() else { return };
        let mut line = message.encode();
        line.push('\n');
        if pipe.write_all(line.as_bytes()).is_err() {
            warn!("lost the connection to the Datadog test agent");
            *stdin = None;
        }
    }

    fn reply(&self, timeout: Duration) -> Result<Message, RecvTimeoutError> {
        self.replies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .recv_timeout(timeout)
    }
}

impl Backend for AgentClient {
    fn bootstrap(&self, hello: &Hello) -> Result<Bootstrap, String> {
        self.write(&Message::Hello(hello.clone()));
        match self.reply(self.bootstrap_timeout) {
            Ok(Message::Bootstrap(bootstrap)) => Ok(bootstrap),
            Ok(Message::Error(error)) => Err(error),
            Ok(other) => Err(format!("unexpected agent reply {other:?}")),
            Err(error) => {
                if error == RecvTimeoutError::Timeout {
                    self.write(&Message::Cancel);
                }
                Err(reply_error(error, self.bootstrap_timeout))
            }
        }
    }

    fn send(&self, record: TestRecord) {
        self.write(&Message::Test(record));
    }

    fn finish(&self, finish: Finish) {
        self.write(&Message::Finish(finish));
        drop(self.stdin.lock().unwrap_or_else(|e| e.into_inner()).take());
        match self.reply(FINISH_TIMEOUT) {
            Ok(Message::Finished) => {}
            Ok(other) => warn!("unexpected agent reply {other:?}"),
            Err(error) => warn!("test results may be incomplete: {}", reply_error(error, FINISH_TIMEOUT)),
        }
    }
}

fn reply_error(error: RecvTimeoutError, timeout: Duration) -> String {
    match error {
        RecvTimeoutError::Timeout => format!("no answer from the agent after {}s", timeout.as_secs()),
        RecvTimeoutError::Disconnected => "the agent exited".to_string(),
    }
}

/// The agent executable: `DD_TEST_AGENT`, then the wrapper that built the
/// harness, then `dd-test-wrapper` on `PATH`.
pub fn locate_agent(built_by: Option<&str>) -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("DD_TEST_AGENT").filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(path));
    }
    if let Some(path) = built_by.map(PathBuf::from).filter(|p| p.is_file()) {
        return Some(path);
    }
    let name = if cfg!(windows) {
        "dd-test-wrapper.exe"
    } else {
        "dd-test-wrapper"
    };
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join(name))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
        .into_iter()
        .find(|candidate| candidate.is_file())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Instant;

    #[test]
    fn a_late_bootstrap_cancels_the_session() {
        let dir = std::env::temp_dir().join(format!("dd-test-backend-{:x}", crate::ids::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let agent = dir.join("agent");
        let received = dir.join("stdin");
        std::fs::write(&agent, format!("#!/bin/sh\ncat > '{}'\n", received.display())).unwrap();
        std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut client = AgentClient::spawn(&agent).unwrap();
        client.bootstrap_timeout = Duration::from_millis(100);
        let hello = Hello {
            module: "m".into(),
            crate_name: "c".into(),
            binary: "b".into(),
            rustc_version: "1".into(),
            library_version: "0".into(),
            command: "cargo test".into(),
            working_dir: ".".into(),
            parent_pid: None,
        };
        assert!(client.bootstrap(&hello).unwrap_err().contains("no answer"));
        drop(client);

        let expected = format!("{}\n{}\n", Message::Hello(hello).encode(), Message::Cancel.encode());
        let deadline = Instant::now() + Duration::from_secs(10);
        while std::fs::read_to_string(&received).unwrap_or_default() != expected {
            assert!(Instant::now() < deadline, "the agent receives hello, then cancel");
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
