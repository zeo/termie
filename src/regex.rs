//! tiny thompson-NFA regex for find-in-scrollback: literals, `.`, classes
//! (`[a-z0-9]`, `[^…]`), `\d \w \s` and their negations, `* + ?`, alternation
//! `|`, non-capturing groups `(…)`, and `^ $` anchored to the logical line.
//! matching is case-insensitive with the same fold as the plain find. hand-
//! rolled like the rest of termie's codecs — a regex crate would be the
//! project's largest dependency, bought for one search box

#[derive(Clone)]
enum Inst {
    Char(char),
    Any,
    Class { neg: bool, ranges: Vec<(char, char)> },
    /// try `a` first (priority), then `b`
    Split(usize, usize),
    Jmp(usize),
    Start,
    End,
    Match,
}

pub struct Regex {
    prog: Vec<Inst>,
}

/// one-to-one case folding, identical to the plain find's
fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

struct Parser {
    pat: Vec<char>,
    pos: usize,
    prog: Vec<Inst>,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.pat.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += 1;
        Some(c)
    }

    /// emit a placeholder and return its index for later patching
    fn hole(&mut self) -> usize {
        self.prog.push(Inst::Jmp(usize::MAX));
        self.prog.len() - 1
    }

    fn patch(&mut self, at: usize, inst: Inst) {
        self.prog[at] = inst;
    }

    /// alternation: concat ('|' concat)*
    fn alt(&mut self) -> Option<()> {
        let mut starts: Vec<usize> = Vec::new();
        let mut jumps: Vec<usize> = Vec::new();
        loop {
            let split_at = if self.peek_is_branch_start() { Some(self.hole()) } else { None };
            let branch_start = self.prog.len();
            self.concat()?;
            if let Some(at) = split_at {
                starts.push(at);
                // the split's first arm is the branch we just emitted
                self.patch(at, Inst::Split(branch_start, usize::MAX));
            }
            if self.peek() == Some('|') {
                self.bump();
                jumps.push(self.hole());
                // patch the pending split's second arm to the NEXT branch,
                // which begins right here
                if let Some(&at) = starts.last()
                    && let Inst::Split(a, _) = self.prog[at]
                {
                    self.patch(at, Inst::Split(a, self.prog.len()));
                }
            } else {
                break;
            }
        }
        let end = self.prog.len();
        for j in jumps {
            self.patch(j, Inst::Jmp(end));
        }
        Some(())
    }

    /// whether the upcoming branch needs a split (only when a '|' follows it —
    /// resolved by scanning ahead at this nesting depth)
    fn peek_is_branch_start(&self) -> bool {
        let mut depth = 0usize;
        let mut i = self.pos;
        while let Some(&c) = self.pat.get(i) {
            match c {
                '\\' => i += 1,
                '(' => depth += 1,
                ')' if depth == 0 => return false,
                ')' => depth -= 1,
                '|' if depth == 0 => return true,
                _ => {}
            }
            i += 1;
        }
        false
    }

    /// concatenation: repeat*
    fn concat(&mut self) -> Option<()> {
        while let Some(c) = self.peek() {
            if c == '|' || c == ')' {
                break;
            }
            self.repeat()?;
        }
        Some(())
    }

    /// one atom with an optional * + ? suffix
    fn repeat(&mut self) -> Option<()> {
        let start = self.prog.len();
        // a repeat may need a split BEFORE the atom; reserve the slot lazily
        // by emitting the atom first and rotating when needed
        self.atom()?;
        match self.peek() {
            Some('*') => {
                self.bump();
                // split(atom, out) before the atom, jmp back after it
                self.prog.insert(start, Inst::Jmp(usize::MAX));
                Self::shift(&mut self.prog, start, 1);
                let after = self.prog.len() + 1;
                self.patch(start, Inst::Split(start + 1, after));
                self.prog.push(Inst::Jmp(start));
            }
            Some('+') => {
                self.bump();
                // jmp back via a split: one more round or fall through
                let after = self.prog.len() + 1;
                self.prog.push(Inst::Split(start, after));
            }
            Some('?') => {
                self.bump();
                self.prog.insert(start, Inst::Jmp(usize::MAX));
                Self::shift(&mut self.prog, start, 1);
                let after = self.prog.len();
                self.patch(start, Inst::Split(start + 1, after));
            }
            _ => {}
        }
        Some(())
    }

    /// fix absolute targets after inserting `by` slots at `at`
    fn shift(prog: &mut [Inst], at: usize, by: usize) {
        for inst in prog.iter_mut() {
            match inst {
                Inst::Split(a, b) => {
                    if *a >= at && *a != usize::MAX {
                        *a += by;
                    }
                    if *b >= at && *b != usize::MAX {
                        *b += by;
                    }
                }
                Inst::Jmp(t) if *t >= at && *t != usize::MAX => *t += by,
                _ => {}
            }
        }
    }

    fn atom(&mut self) -> Option<()> {
        match self.bump()? {
            '(' => {
                self.alt()?;
                if self.bump()? != ')' {
                    return None;
                }
            }
            '[' => {
                let mut neg = false;
                let mut ranges: Vec<(char, char)> = Vec::new();
                if self.peek() == Some('^') {
                    self.bump();
                    neg = true;
                }
                loop {
                    let c = self.bump()?;
                    if c == ']' && !ranges.is_empty() {
                        break;
                    }
                    let lo = if c == '\\' { self.bump()? } else { c };
                    if self.peek() == Some('-') && self.pat.get(self.pos + 1) != Some(&']') {
                        self.bump();
                        let hi = self.bump()?;
                        let hi = if hi == '\\' { self.bump()? } else { hi };
                        ranges.push((fold(lo), fold(hi)));
                    } else {
                        ranges.push((fold(lo), fold(lo)));
                    }
                }
                self.prog.push(Inst::Class { neg, ranges });
            }
            '.' => self.prog.push(Inst::Any),
            '^' => self.prog.push(Inst::Start),
            '$' => self.prog.push(Inst::End),
            '\\' => {
                let c = self.bump()?;
                let class = |ranges: Vec<(char, char)>, neg: bool| Inst::Class { neg, ranges };
                let inst = match c {
                    'd' => class(vec![('0', '9')], false),
                    'D' => class(vec![('0', '9')], true),
                    'w' => class(vec![('a', 'z'), ('0', '9'), ('_', '_')], false),
                    'W' => class(vec![('a', 'z'), ('0', '9'), ('_', '_')], true),
                    's' => class(vec![(' ', ' '), ('\t', '\t')], false),
                    'S' => class(vec![(' ', ' '), ('\t', '\t')], true),
                    other => Inst::Char(fold(other)),
                };
                self.prog.push(inst);
            }
            '*' | '+' | '?' | ')' | '|' | ']' => return None,
            c => self.prog.push(Inst::Char(fold(c))),
        }
        Some(())
    }
}

impl Regex {
    /// compile a pattern; None when it is malformed
    pub fn compile(pattern: &str) -> Option<Regex> {
        if pattern.is_empty() {
            return None;
        }
        let mut p = Parser { pat: pattern.chars().collect(), pos: 0, prog: Vec::new() };
        p.alt()?;
        if p.pos != p.pat.len() {
            return None; // trailing garbage (e.g. an unmatched ')')
        }
        p.prog.push(Inst::Match);
        Some(Regex { prog: p.prog })
    }

    fn hit(inst: &Inst, c: char) -> bool {
        let f = fold(c);
        match inst {
            Inst::Char(want) => f == *want,
            Inst::Any => true,
            Inst::Class { neg, ranges } => {
                let inside = ranges.iter().any(|&(lo, hi)| f >= lo && f <= hi);
                inside != *neg
            }
            _ => false,
        }
    }

    /// epsilon-closure add: follow splits/jumps/anchors, dedupe by pc
    fn add(&self, list: &mut Vec<usize>, seen: &mut [bool], pc: usize, pos: usize, len: usize) {
        if seen[pc] {
            return;
        }
        seen[pc] = true;
        match self.prog[pc] {
            Inst::Split(a, b) => {
                self.add(list, seen, a, pos, len);
                self.add(list, seen, b, pos, len);
            }
            Inst::Jmp(t) => self.add(list, seen, t, pos, len),
            Inst::Start => {
                if pos == 0 {
                    self.add(list, seen, pc + 1, pos, len);
                }
            }
            Inst::End => {
                if pos == len {
                    self.add(list, seen, pc + 1, pos, len);
                }
            }
            _ => list.push(pc),
        }
    }

    /// longest match starting exactly at `start`, as the end index; a state-set
    /// walk, so pathological patterns can't blow up exponentially
    fn match_at(&self, hay: &[char], start: usize, budget: &mut usize) -> Option<usize> {
        let n = self.prog.len();
        let mut clist: Vec<usize> = Vec::new();
        let mut seen = vec![false; n];
        let mut best: Option<usize> = None;
        let mut seen0 = vec![false; n];
        self.add(&mut clist, &mut seen0, 0, start, hay.len());
        if clist.iter().any(|&pc| matches!(self.prog[pc], Inst::Match)) {
            best = Some(start);
        }
        let mut pos = start;
        while pos < hay.len() && !clist.is_empty() {
            *budget = budget.saturating_sub(clist.len() + 1);
            if *budget == 0 {
                break;
            }
            let c = hay[pos];
            let mut next: Vec<usize> = Vec::new();
            seen.iter_mut().for_each(|s| *s = false);
            for &pc in &clist {
                if Self::hit(&self.prog[pc], c) {
                    self.add(&mut next, &mut seen, pc + 1, pos + 1, hay.len());
                }
            }
            pos += 1;
            clist = next;
            if clist.iter().any(|&pc| matches!(self.prog[pc], Inst::Match)) {
                best = Some(pos);
            }
        }
        best
    }

    /// all non-overlapping leftmost-longest matches as (start, end) char
    /// indexes. `budget` caps total work so a pathological pattern over a huge
    /// scrollback degrades to partial results instead of freezing the UI
    pub fn find_all(&self, hay: &[char], budget: &mut usize) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        // fast skip: when the program opens with a literal, only attempt
        // starts on that character
        let first = match self.prog.first() {
            Some(Inst::Char(c)) => Some(*c),
            _ => None,
        };
        let mut start = 0;
        while start < hay.len() && *budget > 0 {
            if let Some(f) = first
                && fold(hay[start]) != f
            {
                start += 1;
                continue;
            }
            match self.match_at(hay, start, budget) {
                // zero-length matches (a*, ^) are noise in a find box; skip
                Some(end) if end > start => {
                    out.push((start, end));
                    start = end;
                }
                _ => start += 1,
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(pat: &str, hay: &str) -> Vec<(usize, usize)> {
        let re = Regex::compile(pat).expect("compiles");
        let chars: Vec<char> = hay.chars().collect();
        re.find_all(&chars, &mut 1_000_000)
    }

    #[test]
    fn literals_fold_case() {
        assert_eq!(all("err", "ERROR err"), vec![(0, 3), (6, 9)]);
    }

    #[test]
    fn classes_dot_and_escapes() {
        assert_eq!(all(r"e\d+", "e12 e e9"), vec![(0, 3), (6, 8)]);
        assert_eq!(all("h.t", "hat hot h t"), vec![(0, 3), (4, 7), (8, 11)]);
        assert_eq!(all("[a-c]+", "abcd"), vec![(0, 3)]);
        assert_eq!(all("[^0-9]+", "ab12cd"), vec![(0, 2), (4, 6)]);
        assert_eq!(all(r"\.", "a.b"), vec![(1, 2)]);
    }

    #[test]
    fn repeats_are_greedy_and_optional() {
        assert_eq!(all("ab*c", "ac abc abbbc"), vec![(0, 2), (3, 6), (7, 12)]);
        assert_eq!(all("ab?c", "ac abc"), vec![(0, 2), (3, 6)]);
        assert_eq!(all("a+", "aaa b aa"), vec![(0, 3), (6, 8)]);
    }

    #[test]
    fn alternation_and_groups() {
        assert_eq!(all("cat|dog", "a dog, a cat"), vec![(2, 5), (9, 12)]);
        assert_eq!(all("(ab)+", "ababab x ab"), vec![(0, 6), (9, 11)]);
        assert_eq!(all("gr(a|e)y", "gray grey"), vec![(0, 4), (5, 9)]);
    }

    #[test]
    fn anchors_bind_to_the_line() {
        assert_eq!(all("^err", "err noerr"), vec![(0, 3)]);
        assert_eq!(all("end$", "the end"), vec![(4, 7)]);
        assert_eq!(all("^all$", "all"), vec![(0, 3)]);
        assert!(all("^all$", "not all").is_empty());
    }

    #[test]
    fn malformed_patterns_refuse_to_compile() {
        for bad in ["", "(", "(ab", "a)", "[", "[]", "*a", "+", "a**"] {
            assert!(Regex::compile(bad).is_none(), "{bad:?} should not compile");
        }
    }

    #[test]
    fn budget_bails_instead_of_hanging() {
        let re = Regex::compile("(a+)+$").expect("compiles");
        let hay: Vec<char> = "a".repeat(5000).chars().chain("b".chars()).collect();
        let mut budget = 10_000usize;
        let _ = re.find_all(&hay, &mut budget);
        assert_eq!(budget, 0, "the budget stops the scan");
    }
}
