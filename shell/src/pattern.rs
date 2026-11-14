//! The patterns `grep` takes: a small regular expression.
//!
//! `c` matches itself, `.` any character, `*` any number of the one before,
//! `^` the start of the line and `$` its end, and `\` makes the next
//! character mean itself. This is the matcher from Kernighan and Pike's
//! *The Practice of Programming*: enough for the searches people type at a
//! prompt, small enough to read in one go.

use alloc::vec::Vec;

/// Whether `pattern` matches anywhere in `line`.
pub fn matches(pattern: &str, line: &str) -> bool {
    let pattern = tokens(pattern);
    let line: Vec<char> = line.chars().collect();
    if let Some((Token::Start, rest)) = pattern.split_first() {
        return here(rest, &line);
    }
    (0..=line.len()).any(|at| here(&pattern, &line[at..]))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Token {
    Char(char),
    Any,
    /// The one before, any number of times.
    Star(Atom),
    Start,
    End,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Atom {
    Char(char),
    Any,
}

impl Atom {
    fn accepts(self, c: char) -> bool {
        match self {
            Atom::Char(want) => want == c,
            Atom::Any => true,
        }
    }
}

fn tokens(pattern: &str) -> Vec<Token> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let atom = match chars[i] {
            '^' if i == 0 => {
                out.push(Token::Start);
                i += 1;
                continue;
            }
            '$' if i == chars.len() - 1 => {
                out.push(Token::End);
                i += 1;
                continue;
            }
            '\\' if i + 1 < chars.len() => {
                i += 1;
                Atom::Char(chars[i])
            }
            '.' => Atom::Any,
            c => Atom::Char(c),
        };
        i += 1;
        if chars.get(i) == Some(&'*') {
            out.push(Token::Star(atom));
            i += 1;
        } else {
            out.push(match atom {
                Atom::Char(c) => Token::Char(c),
                Atom::Any => Token::Any,
            });
        }
    }
    out
}

/// Whether `pattern` matches at the start of `line`.
fn here(pattern: &[Token], line: &[char]) -> bool {
    let Some((first, rest)) = pattern.split_first() else {
        return true;
    };
    match *first {
        Token::End => line.is_empty(),
        Token::Start => false,
        Token::Star(atom) => {
            // Shortest first: a `*` takes as little as lets the rest match.
            let mut at = 0;
            loop {
                if here(rest, &line[at..]) {
                    return true;
                }
                if at < line.len() && atom.accepts(line[at]) {
                    at += 1;
                } else {
                    return false;
                }
            }
        }
        Token::Char(c) => line.first() == Some(&c) && here(rest, &line[1..]),
        Token::Any => !line.is_empty() && here(rest, &line[1..]),
    }
}
