//! termie reference plugin: a session relay over the in-process plugin bus.
//!
//! demonstrates the Phase-3 bus: this plugin `subscribe`s to a topic, and
//! republishes anything notable as a bus `message` other plugins can react to.
//! it also surfaces received messages in a Tier-1 widget so the bus is visible.
//!
//! within one termie, plugins talk via the bus (publish/subscribe). ACROSS
//! machines/instances the bus does not reach — a real cross-session plugin
//! would open its own socket here and bridge it onto the bus. this reference
//! keeps to the in-process bus so it has zero dependencies and zero config.
//!
//! protocol: newline-delimited json on stdin (host events) / stdout (commands).
//! see plugins/README.md for the full contract.

use std::io::{BufRead, Write};

fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out
}

/// pull the value of a top-level string field out of one compact protocol line.
/// the host emits one object per line with no nested quotes in these fields, so
/// a dependency-free scan is enough for a reference plugin
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("\"{key}\":\"");
    let start = line.find(&pat)? + pat.len();
    let rest = &line[start..];
    let end = rest.find('"')?;
    Some(&rest[..end])
}

fn emit_widget(out: &mut impl Write, lines: &[String]) {
    let shown: Vec<String> = lines.iter().rev().take(5).rev().cloned().collect();
    let json: Vec<String> = shown.iter().map(|l| format!("\"{}\"", esc(l))).collect();
    let _ = writeln!(
        out,
        "{{\"t\":\"update_widget\",\"widget\":{{\"id\":\"relay\",\"title\":\"relay\",\"lines\":[{}]}}}}",
        json.join(",")
    );
    let _ = out.flush();
}

fn main() {
    let mut out = std::io::stdout();

    // announce, subscribe to the "chat" topic, and declare a log widget
    let _ = writeln!(out, "{{\"t\":\"ready\",\"name\":\"relay\",\"api_version\":1}}");
    let _ = writeln!(out, "{{\"t\":\"subscribe\",\"topic\":\"chat\"}}");
    let _ = writeln!(
        out,
        "{{\"t\":\"declare_widget\",\"widget\":{{\"id\":\"relay\",\"title\":\"relay\",\"lines\":[\"listening on #chat\"]}}}}"
    );
    let _ = out.flush();

    let mut log: Vec<String> = vec!["listening on #chat".to_string()];

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line.contains("\"shutdown\"") {
            break;
        }
        // a bus message from another plugin: log who said what
        if line.contains("\"t\":\"message\"") {
            let from = field(line, "from").unwrap_or("?");
            let body = field(line, "body").unwrap_or("");
            log.push(format!("{from}: {body}"));
            emit_widget(&mut out, &log);
        }
        // when a pane rings, announce it on the bus so other sessions' plugins
        // (subscribed to "chat") can react — demonstrates publish
        else if line.contains("\"t\":\"bell\"") {
            let _ = writeln!(
                out,
                "{{\"t\":\"publish\",\"topic\":\"chat\",\"body\":\"bell rang\"}}"
            );
            let _ = out.flush();
        }
    }
}
