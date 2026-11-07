//! Test-only GBNF recogniser for the subset [`crate::grammar`] emits, so grammar tests can
//! check which texts a grammar accepts without loading a model: rules `name ::= body`, one
//! per line; alternatives `|`; groups `( )`; double-quoted literals with `\"` `\\` `\n` `\r`
//! `\t` escapes; character classes `[...]` with `^`, ranges and `\xHH` / `\\` / `\"` / `\t`
//! / `\n` escapes; postfix `*`, `+`, `?`, `{m}`, `{m,}`, `{m,n}`; rule references.
//! Semantics follow llama.cpp's grammar (a set of end positions per node; the whole input
//! must be consumed by `root`).

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};

#[derive(Debug, Clone)]
enum Atom {
    Lit(Vec<char>),
    Class {
        neg: bool,
        ranges: Vec<(char, char)>,
    },
    Ref(String),
    Group(Vec<Vec<Item>>),
}

#[derive(Debug, Clone)]
struct Item {
    atom: Atom,
    min: usize,
    max: Option<usize>,
}

struct Parser<'a> {
    s: &'a [char],
    i: usize,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while self.i < self.s.len() && self.s[self.i].is_whitespace() {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<char> {
        self.s.get(self.i).copied()
    }

    fn escape(&mut self) -> char {
        // At a backslash.
        self.i += 1;
        let c = self.s[self.i];
        self.i += 1;
        match c {
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            'x' => {
                let h: String = self.s[self.i..self.i + 2].iter().collect();
                self.i += 2;
                char::from_u32(u32::from_str_radix(&h, 16).expect("hex escape")).expect("char")
            }
            other => other,
        }
    }

    fn alternatives(&mut self) -> Vec<Vec<Item>> {
        let mut alts = vec![self.sequence()];
        loop {
            self.ws();
            if self.peek() == Some('|') {
                self.i += 1;
                alts.push(self.sequence());
            } else {
                return alts;
            }
        }
    }

    fn sequence(&mut self) -> Vec<Item> {
        let mut items = Vec::new();
        loop {
            self.ws();
            let atom = match self.peek() {
                None | Some('|' | ')') => return items,
                Some('"') => {
                    self.i += 1;
                    let mut lit = Vec::new();
                    while self.peek() != Some('"') {
                        if self.peek() == Some('\\') {
                            lit.push(self.escape());
                        } else {
                            lit.push(self.s[self.i]);
                            self.i += 1;
                        }
                    }
                    self.i += 1;
                    Atom::Lit(lit)
                }
                Some('[') => {
                    self.i += 1;
                    let neg = self.peek() == Some('^');
                    if neg {
                        self.i += 1;
                    }
                    let mut ranges = Vec::new();
                    while self.peek() != Some(']') {
                        let a = if self.peek() == Some('\\') {
                            self.escape()
                        } else {
                            self.i += 1;
                            self.s[self.i - 1]
                        };
                        let b = if self.peek() == Some('-') && self.s.get(self.i + 1) != Some(&']')
                        {
                            self.i += 1;
                            if self.peek() == Some('\\') {
                                self.escape()
                            } else {
                                self.i += 1;
                                self.s[self.i - 1]
                            }
                        } else {
                            a
                        };
                        ranges.push((a, b));
                    }
                    self.i += 1;
                    Atom::Class { neg, ranges }
                }
                Some('(') => {
                    self.i += 1;
                    let alts = self.alternatives();
                    self.ws();
                    assert_eq!(self.peek(), Some(')'), "unclosed group");
                    self.i += 1;
                    Atom::Group(alts)
                }
                Some(c) if c.is_ascii_alphanumeric() || c == '-' => {
                    let start = self.i;
                    while self
                        .peek()
                        .is_some_and(|c| c.is_ascii_alphanumeric() || c == '-')
                    {
                        self.i += 1;
                    }
                    Atom::Ref(self.s[start..self.i].iter().collect())
                }
                Some(c) => panic!("unexpected {c:?} in grammar"),
            };
            let (min, max) = match self.peek() {
                Some('*') => {
                    self.i += 1;
                    (0, None)
                }
                Some('+') => {
                    self.i += 1;
                    (1, None)
                }
                Some('?') => {
                    self.i += 1;
                    (0, Some(1))
                }
                Some('{') => {
                    self.i += 1;
                    let end = self.i + self.s[self.i..].iter().position(|&c| c == '}').unwrap();
                    let body: String = self.s[self.i..end].iter().collect();
                    self.i = end + 1;
                    match body.split_once(',') {
                        None => {
                            let n = body.trim().parse().unwrap();
                            (n, Some(n))
                        }
                        Some((a, b)) => (
                            a.trim().parse().unwrap(),
                            if b.trim().is_empty() {
                                None
                            } else {
                                Some(b.trim().parse().unwrap())
                            },
                        ),
                    }
                }
                _ => (1, Some(1)),
            };
            items.push(Item { atom, min, max });
        }
    }
}

/// A parsed grammar.
pub struct Gbnf {
    rules: HashMap<String, Vec<Vec<Item>>>,
}

impl Gbnf {
    /// Parses `text` (one rule per line).
    pub fn parse(text: &str) -> Self {
        let mut rules = HashMap::new();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let (name, body) = line.split_once("::=").expect("rule line");
            let chars: Vec<char> = body.chars().collect();
            let mut p = Parser { s: &chars, i: 0 };
            let alts = p.alternatives();
            p.ws();
            assert_eq!(p.i, chars.len(), "trailing input in rule {name}");
            rules.insert(name.trim().to_string(), alts);
        }
        Gbnf { rules }
    }

    /// Whether `root` matches the whole of `input`.
    pub fn accepts(&self, input: &str) -> bool {
        let chars: Vec<char> = input.chars().collect();
        let m = Matcher {
            g: self,
            input: &chars,
            memo: RefCell::new(HashMap::new()),
        };
        m.rule("root", 0).contains(&chars.len())
    }
}

struct Matcher<'a> {
    g: &'a Gbnf,
    input: &'a [char],
    memo: RefCell<HashMap<(String, usize), BTreeSet<usize>>>,
}

impl Matcher<'_> {
    fn rule(&self, name: &str, pos: usize) -> BTreeSet<usize> {
        let key = (name.to_string(), pos);
        if let Some(v) = self.memo.borrow().get(&key) {
            return v.clone();
        }
        let alts = self
            .g
            .rules
            .get(name)
            .unwrap_or_else(|| panic!("undefined rule {name}"));
        let out = self.alternatives(alts, pos);
        self.memo.borrow_mut().insert(key, out.clone());
        out
    }

    fn alternatives(&self, alts: &[Vec<Item>], pos: usize) -> BTreeSet<usize> {
        let mut out = BTreeSet::new();
        for seq in alts {
            out.extend(self.sequence(seq, pos));
        }
        out
    }

    fn sequence(&self, seq: &[Item], pos: usize) -> BTreeSet<usize> {
        let mut cur = BTreeSet::from([pos]);
        for item in seq {
            let mut next = BTreeSet::new();
            for &p in &cur {
                next.extend(self.repeat(item, p));
            }
            if next.is_empty() {
                return next;
            }
            cur = next;
        }
        cur
    }

    fn repeat(&self, item: &Item, pos: usize) -> BTreeSet<usize> {
        let mut out = BTreeSet::new();
        let mut frontier = BTreeSet::from([pos]);
        let mut seen = BTreeSet::from([pos]);
        if item.min == 0 {
            out.insert(pos);
        }
        let mut n = 0;
        while !frontier.is_empty() && item.max.is_none_or(|m| n < m) {
            n += 1;
            let mut next = BTreeSet::new();
            for &p in &frontier {
                next.extend(self.atom(&item.atom, p));
            }
            if n >= item.min {
                out.extend(next.iter().copied());
            }
            // Unbounded repetition stops when no new position appears.
            frontier = if item.max.is_none() {
                next.into_iter().filter(|p| seen.insert(*p)).collect()
            } else {
                next
            };
        }
        out
    }

    fn atom(&self, atom: &Atom, pos: usize) -> BTreeSet<usize> {
        match atom {
            Atom::Lit(l) => {
                let end = pos + l.len();
                if self.input.get(pos..end) == Some(l.as_slice()) {
                    BTreeSet::from([end])
                } else {
                    BTreeSet::new()
                }
            }
            Atom::Class { neg, ranges } => match self.input.get(pos) {
                Some(&c) if ranges.iter().any(|&(a, b)| a <= c && c <= b) != *neg => {
                    BTreeSet::from([pos + 1])
                }
                _ => BTreeSet::new(),
            },
            Atom::Ref(name) => self.rule(name, pos),
            Atom::Group(alts) => self.alternatives(alts, pos),
        }
    }
}

#[test]
fn gbnf_match_recognises_the_subset() {
    let g = Gbnf::parse(concat!(
        "root ::= \"[\" item ( \",\" space item ){0,2} \"]\"\n",
        "item ::= [0-9a-f]{2} | \"\\\"x\\\"\" | [^\\x00-\\x20,\\]]+\n",
        "space ::= | \" \"\n",
    ));
    for ok in ["[0a]", "[0a, ff]", "[\"x\",zz,9b]", "[hello]"] {
        assert!(g.accepts(ok), "{ok}");
    }
    for bad in ["[]", "[0a,0b,0c,0d]", "[0a,  0b]", "[\n]", "0a", "[0a]x"] {
        assert!(!g.accepts(bad), "{bad:?}");
    }
}
