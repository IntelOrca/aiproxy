//! Minimal prefixed logging helpers (parity with the Deno version's
//! `[aiproxy]`-prefixed console output: log/error to stdout/stderr).

use std::fmt::Display;

#[allow(dead_code)]
pub fn info(message: impl Display) {
    println!("[aiproxy] {}", message);
}

#[allow(dead_code)]
pub fn warn(message: impl Display) {
    eprintln!("[aiproxy] {}", message);
}

#[allow(dead_code)]
pub fn error(message: impl Display) {
    eprintln!("[aiproxy] error: {}", message);
}
