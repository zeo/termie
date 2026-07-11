//! kitty graphics protocol scanner. vte 0.15 has no APC callback, so we split
//! the pty byte stream ourselves into terminal data (handed to vte) and complete
//! kitty APC payloads (`ESC _ G ... ESC \`), buffering partial sequences across
//! pty reads. non-kitty APC is dropped (vte swallows it anyway)

/// hard cap on a single buffered APC sequence so a hostile/garbled stream can't
/// grow the buffer without bound — on overflow we resync to normal scanning
const MAX_APC: usize = 16 * 1024 * 1024;

#[derive(Default, PartialEq)]
enum State {
    #[default]
    Normal,
    Esc,
    Apc,
    ApcEsc,
}

#[derive(Default)]
pub struct ApcScanner {
    state: State,
    apc: Vec<u8>,
    // reused across feeds so the hot pty path never allocates per chunk
    pass: Vec<u8>,
    kitty: Vec<Vec<u8>>,
}

impl ApcScanner {
    /// split a chunk into (bytes for vte, completed kitty payloads). a kitty
    /// payload is the bytes between `ESC _` and `ESC \`, including the leading
    /// `G`. spans split across calls are buffered until complete. the returned
    /// slices borrow reused internal buffers — consume them before the next feed.
    /// runs between escapes are bulk-copied, so a no-graphics chunk costs one
    /// scan for ESC plus one extend (no per-byte work, no allocation)
    pub fn feed(&mut self, chunk: &[u8]) -> (&[u8], &[Vec<u8>]) {
        self.pass.clear();
        self.kitty.clear();
        let mut i = 0;
        while i < chunk.len() {
            match self.state {
                State::Normal => match chunk[i..].iter().position(|&b| b == 0x1b) {
                    Some(off) => {
                        self.pass.extend_from_slice(&chunk[i..i + off]);
                        self.state = State::Esc;
                        i += off + 1;
                    }
                    None => {
                        self.pass.extend_from_slice(&chunk[i..]);
                        break;
                    }
                },
                State::Esc => {
                    let b = chunk[i];
                    i += 1;
                    if b == 0x5f {
                        // ESC _ : APC start
                        self.state = State::Apc;
                        self.apc.clear();
                    } else {
                        // not APC: replay the ESC and resume (ESC ESC stays armed)
                        self.pass.push(0x1b);
                        if b == 0x1b {
                            self.state = State::Esc;
                        } else {
                            self.pass.push(b);
                            self.state = State::Normal;
                        }
                    }
                }
                State::Apc => match chunk[i..].iter().position(|&b| b == 0x1b) {
                    Some(off) => {
                        self.apc.extend_from_slice(&chunk[i..i + off]);
                        self.state = State::ApcEsc;
                        i += off + 1;
                        if self.apc.len() > MAX_APC {
                            self.apc.clear();
                            self.state = State::Normal;
                        }
                    }
                    None => {
                        self.apc.extend_from_slice(&chunk[i..]);
                        if self.apc.len() > MAX_APC {
                            self.apc.clear();
                            self.state = State::Normal;
                        }
                        break;
                    }
                },
                State::ApcEsc => {
                    let b = chunk[i];
                    i += 1;
                    if b == 0x5c {
                        // ESC \ : string terminator — emit if this is kitty (G…)
                        let payload = std::mem::take(&mut self.apc);
                        if payload.first() == Some(&b'G') {
                            self.kitty.push(payload);
                        }
                        self.state = State::Normal;
                    } else if b == 0x1b {
                        // a literal ESC inside the payload; the next byte decides
                        self.apc.push(0x1b);
                    } else {
                        self.apc.push(0x1b);
                        self.apc.push(b);
                        self.state = State::Apc;
                    }
                }
            }
        }
        (&self.pass, &self.kitty)
    }
}

/// a parsed kitty graphics command (the control keys this v1 understands)
pub struct KittyCmd {
    /// 't' transmit, 'T' transmit+display, 'p' put/display, 'q' query, 'd' delete
    pub action: u8,
    /// 24 = RGB, 32 = RGBA (100 = PNG, deferred)
    pub format: u32,
    pub width: u32,
    pub height: u32,
    pub id: u32,
    /// c=/r=: display the image scaled to this many cell columns/rows
    /// (0 = natural pixel size)
    pub cols: u32,
    pub rows: u32,
    /// z=: stacking order; negative draws beneath the pane's text
    pub z: i32,
    /// m=1: more chunks of this image follow
    pub more: bool,
    /// C=1: leave the cursor where it is instead of stepping past the placement
    pub no_cursor_move: bool,
    /// q: 0 = all responses, 1 = errors only, 2 = silent
    pub quiet: u8,
    /// the base64-decoded image bytes for this chunk
    pub payload: Vec<u8>,
}

impl KittyCmd {
    /// parse a kitty payload `G<key=val,...>;<base64>` (leading `G` already
    /// confirmed by the scanner). None on malformed control data
    pub fn parse(apc: &[u8]) -> Option<KittyCmd> {
        let body = apc.strip_prefix(b"G")?;
        let (control, data) = match body.iter().position(|&b| b == b';') {
            Some(i) => (&body[..i], &body[i + 1..]),
            None => (body, &b""[..]),
        };
        let mut cmd = KittyCmd {
            action: b't',
            // 0 = unspecified: a continuation chunk carries no f= key, and a
            // 32 default here clobbered a chunked RGB transfer's format so its
            // byte count never matched and the image silently vanished. the
            // store owns the RGBA default instead
            format: 0,
            width: 0,
            height: 0,
            id: 0,
            cols: 0,
            rows: 0,
            z: 0,
            more: false,
            no_cursor_move: false,
            quiet: 0,
            payload: Vec::new(),
        };
        for kv in control.split(|&b| b == b',') {
            if kv.is_empty() {
                continue;
            }
            let mut it = kv.splitn(2, |&b| b == b'=');
            let key = it.next()?;
            let val = it.next().unwrap_or(b"");
            let vs = std::str::from_utf8(val).ok()?;
            match key {
                b"a" => cmd.action = val.first().copied().unwrap_or(b't'),
                b"f" => cmd.format = vs.parse().ok()?,
                b"s" => cmd.width = vs.parse().ok()?,
                b"v" => cmd.height = vs.parse().ok()?,
                b"i" => cmd.id = vs.parse().ok()?,
                b"c" => cmd.cols = vs.parse().ok()?,
                b"r" => cmd.rows = vs.parse().ok()?,
                b"z" => cmd.z = vs.parse().ok()?,
                b"m" => cmd.more = vs == "1",
                b"C" => cmd.no_cursor_move = vs == "1",
                b"q" => cmd.quiet = vs.parse().unwrap_or(0),
                _ => {}
            }
        }
        cmd.payload = crate::term::base64_decode(data).unwrap_or_default();
        Some(cmd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_passthrough_from_kitty_apc() {
        let mut s = ApcScanner::default();
        // "hi" + a kitty APC + "bye"
        let (pass, kitty) = s.feed(b"hi\x1b_Ga=T,f=24,s=1,v=1;AAAA\x1b\\bye");
        assert_eq!(pass, b"hibye");
        assert_eq!(kitty.len(), 1);
        assert_eq!(&kitty[0][..1], b"G");
    }

    #[test]
    fn buffers_apc_split_across_feeds() {
        let mut s = ApcScanner::default();
        let (p1, k1) = s.feed(b"x\x1b_Ga=T,f=24,s=1,v=1");
        assert_eq!(p1, b"x");
        assert!(k1.is_empty()); // incomplete: no payload yet
        let (p2, k2) = s.feed(b";QUJD\x1b\\y");
        assert_eq!(p2, b"y");
        assert_eq!(k2.len(), 1);
        let cmd = KittyCmd::parse(&k2[0]).expect("parse");
        assert_eq!(cmd.action, b'T');
        assert_eq!(cmd.format, 24);
        assert_eq!(cmd.width, 1);
        assert_eq!(cmd.payload, b"ABC"); // base64 QUJD
    }

    #[test]
    fn csi_passes_through_untouched() {
        let mut s = ApcScanner::default();
        // a CSI sequence (ESC [) must not be mistaken for APC (ESC _)
        let (pass, kitty) = s.feed(b"\x1b[31mred\x1b[0m");
        assert_eq!(pass, b"\x1b[31mred\x1b[0m");
        assert!(kitty.is_empty());
    }

    #[test]
    fn non_kitty_apc_is_dropped() {
        let mut s = ApcScanner::default();
        let (pass, kitty) = s.feed(b"a\x1b_Zsomething\x1b\\b");
        assert_eq!(pass, b"ab");
        assert!(kitty.is_empty()); // not a 'G' payload
    }

    // the bulk-copy rewrite (extend runs between escapes) must be byte-identical
    // to scanning one byte at a time
    #[test]
    fn bulk_copy_matches_byte_at_a_time() {
        let stream: &[u8] =
            b"hello\x1b[31mworld\x1b\x1b[0m\x1b_Znope\x1b\\tail\x1b_Ga=T,f=24,s=1,v=1;QUJD\x1b\\done";
        let mut a = ApcScanner::default();
        let (pa, ka) = a.feed(stream);
        let (pass_one, kitty_one) = (pa.to_vec(), ka.to_vec());

        let mut b = ApcScanner::default();
        let mut pass_bytes = Vec::new();
        let mut kitty_bytes: Vec<Vec<u8>> = Vec::new();
        for &byte in stream {
            let (p, k) = b.feed(&[byte]);
            pass_bytes.extend_from_slice(p);
            kitty_bytes.extend(k.iter().cloned());
        }
        assert_eq!(pass_one, pass_bytes);
        assert_eq!(kitty_one, kitty_bytes);
    }

    // feed() clears but REUSES its buffers — a later feed must not see stale bytes
    #[test]
    fn reused_buffers_dont_leak_across_feeds() {
        let mut s = ApcScanner::default();
        let (p1, _) = s.feed(b"\x1b_Ga=T;QUJD\x1b\\lots of passthrough text here");
        assert_eq!(p1.to_vec(), b"lots of passthrough text here");
        let (p2, k2) = s.feed(b"x");
        assert_eq!(p2, b"x"); // not "x" appended to the prior pass buffer
        assert!(k2.is_empty());
    }

    // the Esc/Apc/ApcEsc state must survive a feed boundary at ANY byte offset
    #[test]
    fn apc_split_exactly_at_chunk_boundary() {
        let seq: &[u8] = b"\x1b_Ga=T,f=24,s=1,v=1;QUJD\x1b\\";
        for i in 1..seq.len() {
            let mut s = ApcScanner::default();
            let (p1, k1) = s.feed(&seq[..i]);
            let mut pass = p1.to_vec();
            let mut kitty: Vec<Vec<u8>> = k1.to_vec();
            let (p2, k2) = s.feed(&seq[i..]);
            pass.extend_from_slice(p2);
            kitty.extend(k2.iter().cloned());
            assert!(pass.is_empty(), "split at {i}: passthrough not empty");
            assert_eq!(kitty.len(), 1, "split at {i}: expected one payload");
            let cmd = KittyCmd::parse(&kitty[0]).expect("parse");
            assert_eq!(cmd.action, b'T');
            assert_eq!(cmd.payload, b"ABC");
        }
    }

    // a doubled ESC outside an APC stays armed (so ESC ESC _ still opens it), and
    // a literal ESC inside the payload is preserved
    #[test]
    fn esc_esc_and_literal_esc_in_payload() {
        let mut s = ApcScanner::default();
        let (pass, kitty) = s.feed(b"\x1b\x1b_Ga=T;QUJD\x1b\\");
        // ESC ESC is not the APC introducer: the first ESC is replayed to vte
        // (the scanner only intercepts ESC _), the second ESC + _ opens the APC
        assert_eq!(pass, b"\x1b");
        assert_eq!(kitty.len(), 1);
        assert_eq!(&kitty[0][..], b"Ga=T;QUJD");

        let mut s = ApcScanner::default();
        let (_p, k) = s.feed(b"\x1b_Ga=T;AA\x1bBB\x1b\\");
        assert_eq!(k.len(), 1);
        assert_eq!(&k[0][..], b"Ga=T;AA\x1bBB"); // embedded ESC kept
    }

    // an APC that overruns MAX_APC with no terminator resyncs back to Normal
    #[test]
    fn max_apc_overflow_resyncs_to_normal() {
        let mut s = ApcScanner::default();
        s.feed(b"\x1b_G"); // open an APC
        let junk = vec![b'A'; MAX_APC + 1]; // overrun the cap with no ESC \
        let (_p, k) = s.feed(&junk);
        assert!(k.is_empty());
        // resync'd to Normal: the now-orphaned ESC \ (string terminator) just
        // passes through to vte, and no kitty payload came from the oversized junk
        let (pass, kitty) = s.feed(b"\x1b\\after");
        assert_eq!(pass, b"\x1b\\after");
        assert!(kitty.is_empty());
    }

    // KittyCmd::parse default-fill + malformed/None branches
    #[test]
    fn kittycmd_parse_defaults_and_malformed() {
        // format defaults to 0 = unspecified: a continuation chunk has no f=
        // and must not clobber the pending transfer's format (the store owns
        // the RGBA default)
        let d = KittyCmd::parse(b"G").expect("bare G");
        assert_eq!((d.action, d.format, d.width, d.id, d.more, d.quiet), (b't', 0, 0, 0, false, 0));
        assert!(!d.no_cursor_move); // cursor movement is the default policy
        let q = KittyCmd::parse(b"Ga=q,q=2;").expect("query");
        assert_eq!((q.action, q.quiet), (b'q', 2));
        let still = KittyCmd::parse(b"Ga=T,C=1;AAAA").expect("C=1");
        assert!(still.no_cursor_move);
        let under = KittyCmd::parse(b"Ga=T,z=-7;AAAA").expect("z=-7");
        assert_eq!(under.z, -7);
        assert_eq!(d.z, 0); // unspecified stays at the text layer
        assert!(KittyCmd::parse(b"Gf=notanumber;AAAA").is_none()); // bad int -> None
        assert!(KittyCmd::parse(b"Zfoo").is_none()); // not a G payload
    }
}
