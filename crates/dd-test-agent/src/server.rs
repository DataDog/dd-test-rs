// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use crate::config::Config;
use crate::session::AgentSession;
use dd_test_core::protocol::Message;
use dd_test_core::{debug, warn};
use std::io::{BufRead, Write};

/// Serves one test binary: `hello`, then `test` records, then `finish`.
/// After `cancel`, nothing is reported. Returns the process exit code.
pub fn serve(input: impl BufRead, mut output: impl Write) -> i32 {
    let mut lines = input.lines();
    let reply = |output: &mut dyn Write, message: Message| {
        let _ = writeln!(output, "{}", message.encode()).and_then(|_| output.flush());
    };

    let hello = match lines
        .next()
        .map(|line| line.map_err(|e| e.to_string()).and_then(|l| Message::decode(&l)))
    {
        Some(Ok(Message::Hello(hello))) => hello,
        Some(Ok(other)) => {
            reply(&mut output, Message::Error(format!("expected hello, got {other:?}")));
            return 2;
        }
        Some(Err(error)) => {
            reply(&mut output, Message::Error(error));
            return 2;
        }
        None => return 0,
    };

    let config = Config::from_env();
    dd_test_core::log::set_debug(config.debug);
    if config.api_key.is_empty() {
        reply(&mut output, Message::Error("DD_API_KEY is not set".to_string()));
        return 0;
    }
    let (mut session, bootstrap) = AgentSession::start(&hello, config);
    reply(&mut output, Message::Bootstrap(bootstrap));

    for line in lines {
        let Ok(line) = line else { break };
        match Message::decode(&line) {
            Ok(Message::Test(record)) => session.record(record),
            Ok(Message::Finish(finish)) => {
                session.finish(Some(finish));
                detach_stderr();
                reply(&mut output, Message::Finished);
                session.close();
                return 0;
            }
            Ok(Message::Cancel) => {
                debug!("the test binary runs without Datadog, nothing is reported");
                detach_stderr();
                session.close();
                return 0;
            }
            Ok(other) => warn!("unexpected message from the test binary: {other:?}"),
            Err(error) => warn!("invalid message from the test binary: {error}"),
        }
    }
    detach_stderr();
    session.finish(None);
    session.close();
    0
}

/// Releases the stderr inherited from the test binary. Test runners such as
/// nextest wait for it to close and report a leak while the agent still runs.
#[cfg(unix)]
fn detach_stderr() {
    use std::os::fd::AsRawFd;
    if let Ok(null) = std::fs::OpenOptions::new().write(true).open("/dev/null") {
        unsafe { libc::dup2(null.as_raw_fd(), libc::STDERR_FILENO) };
    }
}

#[cfg(not(unix))]
fn detach_stderr() {}
