//! A line `--ears` prints reaches a pipe reader while the process still runs.
//!
//! The tray reads the capture's stdout through a pipe, and its Pause is a `SIGKILL`, which flushes nothing: a line
//! still in a userspace buffer when the kill comes is lost. The test runs itself again as the child, with stdout a
//! pipe: the child writes one short line through [`tulpa_hear::ears::Stdout`] (the `--ears` sink), far under any
//! buffer's size, and then stays alive, waiting on its stdin; the parent must read the line before it lets the child
//! go. A process that exits flushes on the way out, so only a reader that sees the line while the writer lives proves
//! the line was not left in a buffer.

use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use tulpa_hear::ears::{Sink, Stdout};

/// Set in the child's environment: write the line and wait.
const CHILD: &str = "TULPA_HEAR_STDOUT_PIPE_CHILD";
const LINE: &str = "TULPA-EARS-LINE 1795000000000 1795000001000 12:00:00 [me] да";

#[test]
fn a_line_reaches_a_pipe_reader_while_the_writer_lives() {
    if std::env::var_os(CHILD).is_some() {
        assert!(Stdout::default().line(LINE, 0).is_ok());
        // Alive until the parent closes our stdin.
        let _all = std::io::stdin().read_to_end(&mut Vec::new());
        return;
    }
    let exe = std::env::current_exe();
    assert!(exe.is_ok(), "{exe:?}");
    let Ok(exe) = exe else { return };
    let child = Command::new(exe)
        .args([
            "a_line_reaches_a_pipe_reader_while_the_writer_lives",
            "--exact",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    assert!(child.is_ok(), "{child:?}");
    let Ok(mut child) = child else { return };
    let (stdin, stdout) = (child.stdin.take(), child.stdout.take());
    assert!(stdin.is_some() && stdout.is_some(), "the pipes");
    let (Some(stdin), Some(stdout)) = (stdin, stdout) else {
        let _killed = child.kill();
        return;
    };
    let (tx, rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if line == LINE {
                let _sent = tx.send(());
                break;
            }
        }
    });
    // A guard against a wedge, not a timing claim: the line is written at the child's start.
    let got = rx.recv_timeout(Duration::from_secs(60));
    let alive = child.try_wait().map(|s| s.is_none());
    drop(stdin);
    let _waited = child.wait();
    let _joined = reader.join();
    assert_eq!(got, Ok(()), "the line, while the child runs");
    assert!(
        matches!(alive, Ok(true)),
        "the child still ran when the line came: {alive:?}"
    );
}
