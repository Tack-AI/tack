//! stdout printing for CLI/streaming paths that must not panic on EPIPE.
//!
//! The `print!`/`println!` macros panic when stdout is a broken pipe
//! (dead pty, `| head` exiting early). Mid-run that panic escalated into
//! a hard abort (SIGABRT): the panic hook's own `eprintln!` hit the same
//! dead pipe and panicked inside the hook. Centralize a best-effort
//! printer here: broken pipe = downstream gone = terminate quietly like
//! any well-behaved CLI; other errors are ignored (nothing useful can be
//! written anyway).

use std::io::Write as _;

/// Write `text` to stdout and flush. Never panics.
pub fn print_out(text: &str) {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    match out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
            std::process::exit(0);
        }
        Err(_) => {}
    }
}
