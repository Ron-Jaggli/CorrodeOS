//! Arguments as a Windows command line carries them.
//!
//! A Windows program is given one string, not a list, and its C runtime
//! splits it again (`CommandLineToArgvW` and the Microsoft C runtime agree
//! on the rule). So the list must be joined in the form that splits back
//! into exactly the same arguments:
//!
//! - an argument with no space, tab, newline, vertical tab or quote, and
//!   not empty, goes as it is: backslashes are only special before a quote;
//! - any other goes in quotes, where `n` backslashes before a quote become
//!   `2n + 1` and the quote, `n` backslashes at the end become `2n` (the
//!   closing quote follows them), and any other backslashes stay as they
//!   are.

use alloc::string::String;

/// One argument, quoted if it must be.
pub fn quoted(argument: &str) -> String {
    if !argument.is_empty() && !argument.contains([' ', '\t', '\n', '\x0b', '"']) {
        return String::from(argument);
    }
    let mut out = String::from("\"");
    let mut backslashes = 0;
    for character in argument.chars() {
        match character {
            '\\' => backslashes += 1,
            '"' => {
                // Each backslash before it doubled, and one more for it.
                out.extend(core::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.extend(core::iter::repeat_n('\\', backslashes));
                out.push(character);
                backslashes = 0;
            }
        }
    }
    // Before the closing quote, doubled, so it stays a closing quote.
    out.extend(core::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

/// Arguments joined into a command line's tail, one space between each.
pub fn joined<S: AsRef<str>>(arguments: &[S]) -> String {
    let mut line = String::new();
    for (index, argument) in arguments.iter().enumerate() {
        if index > 0 {
            line.push(' ');
        }
        line.push_str(&quoted(argument.as_ref()));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    /// The Microsoft C runtime's split of everything after the program's
    /// name, written here from its documented rules, apart from
    /// Verdigris's msvcrt so each checks the other.
    fn split(line: &str) -> Vec<String> {
        let chars: Vec<char> = line.chars().collect();
        let mut arguments = Vec::new();
        let mut at = 0;
        loop {
            while at < chars.len() && (chars[at] == ' ' || chars[at] == '\t') {
                at += 1;
            }
            if at == chars.len() {
                return arguments;
            }
            let mut argument = String::new();
            let mut quoted = false;
            while at < chars.len() {
                let c = chars[at];
                if !quoted && (c == ' ' || c == '\t') {
                    break;
                }
                if c == '\\' {
                    let run = chars[at..].iter().take_while(|c| **c == '\\').count();
                    at += run;
                    if chars.get(at) == Some(&'"') {
                        argument.extend(core::iter::repeat_n('\\', run / 2));
                        if run % 2 == 1 {
                            argument.push('"');
                            at += 1;
                        }
                    } else {
                        argument.extend(core::iter::repeat_n('\\', run));
                    }
                    continue;
                }
                if c == '"' {
                    if quoted && chars.get(at + 1) == Some(&'"') {
                        argument.push('"');
                        at += 2;
                        continue;
                    }
                    quoted = !quoted;
                    at += 1;
                    continue;
                }
                argument.push(c);
                at += 1;
            }
            arguments.push(argument);
        }
    }

    #[test]
    fn plain_arguments_go_as_they_are() {
        assert_eq!(quoted("one"), "one");
        assert_eq!(quoted(r"C:\dir\file"), r"C:\dir\file");
        assert_eq!(quoted(r"trailing\"), r"trailing\");
        assert_eq!(joined(&["one", "two"]), "one two");
    }

    #[test]
    fn the_rest_is_quoted_by_windows_rule() {
        assert_eq!(quoted(""), r#""""#);
        assert_eq!(quoted("two three"), r#""two three""#);
        assert_eq!(quoted(r#"say "hi""#), r#""say \"hi\"""#);
        // A backslash before a quote is doubled, then the quote escaped.
        assert_eq!(quoted(r#"a\"b"#), r#""a\\\"b""#);
        // Backslashes before the closing quote are doubled.
        assert_eq!(quoted(r"C:\a b\"), r#""C:\a b\\""#);
        // Elsewhere they stay single.
        assert_eq!(quoted(r"a\b c"), r#""a\b c""#);
        assert_eq!(quoted("tab\there"), "\"tab\there\"");
        assert_eq!(quoted("line\nfeed"), "\"line\nfeed\"");
    }

    /// What the terminal did before (section 88): a backslash before a
    /// quote, or ending a quoted argument, came back different.
    #[test]
    fn the_old_form_lost_backslashes() {
        let old = |argument: &str| {
            let mut out = String::from("\"");
            for c in argument.chars() {
                if c == '"' {
                    out.push('\\');
                }
                out.push(c);
            }
            out.push('"');
            out
        };
        assert_ne!(split(&old(r"C:\a b\")), [r"C:\a b\"]);
        assert_ne!(split(&old(r#"a\"b"#)), [r#"a\"b"#]);
    }

    #[test]
    fn every_list_splits_back_to_itself() {
        let pieces = ["", " ", "\t", "\"", "\\", "\\\\", "a", "b c", "\\\"", "\"\"", "é", "x\\", "\\ "];
        // Every list of up to three arguments, each up to three pieces.
        let mut words: Vec<String> = Vec::new();
        for a in pieces {
            words.push(String::from(a));
            for b in pieces {
                words.push(String::from(a) + b);
                for c in pieces {
                    words.push(String::from(a) + b + c);
                }
            }
        }
        let mut checked = 0;
        for (i, first) in words.iter().enumerate() {
            let one = [first.as_str()];
            assert_eq!(split(&joined(&one)), one, "{first:?}");
            let second = &words[(i * 7 + 3) % words.len()];
            let third = &words[(i * 13 + 5) % words.len()];
            let three = [first.as_str(), second.as_str(), third.as_str()];
            assert_eq!(split(&joined(&three)), three, "{three:?}");
            checked += 1;
        }
        assert!(checked > 2000);
    }
}
