//! A stand-in for the app's window, for a test of the tray's window handling on Windows (where there are no shell scripts):
//! `fake_window ui <tab> --config <log>` appends `start <tab>` to the log, then `got <line>` for every line sent on stdin.

use std::io::{BufRead, Write};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (tab, log) = (args.get(1).cloned().unwrap_or_default(), args.get(3).cloned().unwrap_or_default());
    let append = |line: String| {
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&log).expect("log");
        writeln!(f, "{line}").expect("write");
    };
    append(format!("start {tab}"));
    for line in std::io::stdin().lock().lines() {
        match line {
            Ok(l) => append(format!("got {l}")),
            Err(_) => break,
        }
    }
}
