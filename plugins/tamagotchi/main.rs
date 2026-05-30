//! termie reference plugin: a tiny tamagotchi pet.
//!
//! demonstrates the v1 plugin protocol end to end with zero dependencies:
//! - declares a Tier-1 widget, then updates it on a timer (the pet gets
//!   hungrier / sleepier over time)
//! - reacts to host events: a `bell` startles it happy; `focus_changed` pets it
//! - exits cleanly when stdin closes or a `shutdown` event arrives
//!
//! protocol: newline-delimited json. host events arrive on stdin; commands go
//! out on stdout. see plugins/README.md for the full contract.

use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// minimal json string escaper (the only json we emit is widget text)
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

/// the pet's mood, derived from its stats, as a little face + status lines
fn render(hunger: u8, joy: u8) -> (String, Vec<String>) {
    let face = if joy >= 70 {
        ">  w  <"
    } else if hunger >= 80 {
        ">  n  <"
    } else if joy <= 25 {
        ">  ;  <"
    } else {
        ">  -  <"
    };
    let bar = |v: u8| {
        let filled = (v as usize + 9) / 10;
        let mut s = String::new();
        for i in 0..10 {
            s.push(if i < filled { '#' } else { '.' });
        }
        s
    };
    (
        "tama".to_string(),
        vec![
            face.to_string(),
            format!("joy   {}", bar(joy)),
            format!("food  {}", bar(100u8.saturating_sub(hunger))),
        ],
    )
}

fn emit_widget(out: &mut impl Write, hunger: u8, joy: u8) {
    let (title, lines) = render(hunger, joy);
    let lines_json: Vec<String> = lines.iter().map(|l| format!("\"{}\"", esc(l))).collect();
    let _ = writeln!(
        out,
        "{{\"t\":\"update_widget\",\"widget\":{{\"id\":\"pet\",\"title\":\"{}\",\"lines\":[{}]}}}}",
        esc(&title),
        lines_json.join(",")
    );
    let _ = out.flush();
}

fn main() {
    let stdout = Arc::new(Mutex::new(std::io::stdout()));

    // shared stats (0..=100), nudged by both the tick thread and host events
    let hunger = Arc::new(AtomicU8::new(20));
    let joy = Arc::new(AtomicU8::new(80));

    // announce ourselves and declare the widget once
    {
        let mut o = stdout.lock().unwrap();
        let _ = writeln!(o, "{{\"t\":\"ready\",\"name\":\"tamagotchi\",\"api_version\":1}}");
        let _ = writeln!(
            o,
            "{{\"t\":\"declare_widget\",\"widget\":{{\"id\":\"pet\",\"title\":\"tama\",\"lines\":[]}}}}"
        );
        let _ = o.flush();
        emit_widget(&mut *o, hunger.load(Ordering::Relaxed), joy.load(Ordering::Relaxed));
    }

    // tick thread: the pet slowly gets hungrier and a touch less joyful, and we
    // repaint the widget every couple of seconds
    {
        let (stdout, hunger, joy) = (stdout.clone(), hunger.clone(), joy.clone());
        thread::spawn(move || loop {
            thread::sleep(Duration::from_secs(2));
            let h = (hunger.load(Ordering::Relaxed) + 2).min(100);
            hunger.store(h, Ordering::Relaxed);
            let mut j = joy.load(Ordering::Relaxed).saturating_sub(1);
            if h >= 80 {
                j = j.saturating_sub(2); // hungry pets sulk
            }
            joy.store(j, Ordering::Relaxed);
            let mut o = stdout.lock().unwrap();
            emit_widget(&mut *o, h, j);
        });
    }

    // main thread: read host events line by line until stdin closes
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // dependency-free: match on the event type substring. the protocol
        // guarantees one compact json object per line with a "t" tag
        if line.contains("\"shutdown\"") {
            break;
        } else if line.contains("\"bell\"") {
            // a bell startles the pet into delight and shakes off hunger a bit
            joy.store(100, Ordering::Relaxed);
            hunger.store(hunger.load(Ordering::Relaxed).saturating_sub(15), Ordering::Relaxed);
            let mut o = stdout.lock().unwrap();
            emit_widget(&mut *o, hunger.load(Ordering::Relaxed), joy.load(Ordering::Relaxed));
        } else if line.contains("\"focus_changed\"") {
            // attention cheers it up slightly
            let j = (joy.load(Ordering::Relaxed) + 5).min(100);
            joy.store(j, Ordering::Relaxed);
            let mut o = stdout.lock().unwrap();
            emit_widget(&mut *o, hunger.load(Ordering::Relaxed), j);
        }
    }
}
