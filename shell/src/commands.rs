//! The shell's own commands: files, text, and the shell's state.
//!
//! Each takes its arguments, the output of whatever was piped into it, and
//! somewhere to write, and says how it went: 0 for success, 1 for "no" (as
//! `grep` finding nothing, or `false`), 2 for being called wrongly.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::fs::{self, Fs, Kind, Node};
use crate::{pattern, About, Host, Output, Shell};

pub const BUILTINS: &[About] = &[
    ("help", "", "this list"),
    ("echo", "[-n] TEXT", "say TEXT back"),
    ("pwd", "", "where you are"),
    ("cd", "[DIR]", "go to DIR, or home"),
    ("ls", "[-al] [PATH]", "what is in a directory"),
    ("cat", "[FILE]", "print files, or what was piped in"),
    ("head", "[-n N] [FILE]", "the first lines"),
    ("tail", "[-n N] [FILE]", "the last lines"),
    ("wc", "[-lwc] [FILE]", "count lines, words and bytes"),
    ("grep", "[-ivncqF] PATTERN [FILE]", "lines that match"),
    ("sort", "[-rnu] [FILE]", "lines in order"),
    ("uniq", "[-c] [FILE]", "drop repeated lines"),
    (
        "cut",
        "-d D -f N | -c N",
        "fields or characters of each line",
    ),
    ("tr", "[-d] SET1 [SET2]", "change or delete characters"),
    ("rev", "[FILE]", "each line backwards"),
    ("tac", "[FILE]", "lines last to first"),
    ("nl", "[FILE]", "number the lines"),
    ("seq", "[FIRST [STEP]] LAST", "count"),
    ("find", "[PATH] [-name P] [-type f|d]", "files by name"),
    ("tree", "[PATH]", "a directory and all under it"),
    ("stat", "FILE", "what a file is"),
    ("file", "FILE", "what kind of contents a file has"),
    ("basename", "PATH", "the last part of a path"),
    ("dirname", "PATH", "all but the last part"),
    ("env", "", "the variables"),
    ("export", "NAME=VALUE", "set a variable"),
    ("unset", "NAME", "forget a variable"),
    ("which", "NAME", "what a command is"),
    ("true", "", "succeed"),
    ("false", "", "fail"),
    ("whoami", "", "who you are, here"),
    ("sudo", "", "there is no root"),
];

pub fn run(
    shell: &mut Shell,
    args: &[String],
    input: &str,
    fs: &Fs,
    host: &mut dyn Host,
    out: &mut Output,
) -> Option<i32> {
    let name = args[0].as_str();
    let rest = &args[1..];
    let mut ctx = Ctx {
        name,
        cwd: shell.cwd.clone(),
        fs,
        input,
        out,
    };
    Some(match name {
        "help" => help(host, ctx.out),
        "echo" => echo(rest, ctx.out),
        "pwd" => {
            ctx.out.line(&shell.cwd);
            0
        }
        "cd" => cd(shell, rest, fs, ctx.out),
        "ls" => ls(&mut ctx, rest),
        "cat" => cat(&mut ctx, rest),
        "head" => ends(&mut ctx, rest, true),
        "tail" => ends(&mut ctx, rest, false),
        "wc" => wc(&mut ctx, rest),
        "grep" => grep(&mut ctx, rest),
        "sort" => sort(&mut ctx, rest),
        "uniq" => uniq(&mut ctx, rest),
        "cut" => cut(&mut ctx, rest),
        "tr" => tr(&mut ctx, rest),
        "rev" => each_text(&mut ctx, rest, |text, out| {
            for line in text.lines() {
                out.line(&line.chars().rev().collect::<String>());
            }
        }),
        "tac" => each_text(&mut ctx, rest, |text, out| {
            for line in text.lines().rev() {
                out.line(line);
            }
        }),
        "nl" => each_text(&mut ctx, rest, |text, out| {
            for (number, line) in text.lines().enumerate() {
                out.line(&format!("{:>6}  {line}", number + 1));
            }
        }),
        "seq" => seq(rest, ctx.out),
        "find" => find(&mut ctx, rest),
        "tree" => tree(&mut ctx, rest),
        "stat" => stat(&mut ctx, rest),
        "file" => file(&mut ctx, rest),
        "basename" => match rest.first() {
            Some(path) => {
                ctx.out.line(fs::name_of(path));
                0
            }
            None => usage(ctx.out, "basename PATH"),
        },
        "dirname" => match rest.first() {
            Some(path) => {
                let trimmed = path.trim_end_matches('/');
                let dir = match trimmed.rfind('/') {
                    Some(0) => "/",
                    Some(at) => &trimmed[..at],
                    None if path.starts_with('/') => "/",
                    None => ".",
                };
                ctx.out.line(dir);
                0
            }
            None => usage(ctx.out, "dirname PATH"),
        },
        "env" => {
            ctx.out.line(&format!("PWD={}", shell.cwd));
            for (key, value) in &shell.vars {
                ctx.out.line(&format!("{key}={value}"));
            }
            0
        }
        "export" => {
            for arg in rest {
                match crate::assignment(arg) {
                    Some((key, value)) => shell.set_var(key, value),
                    None if shell.var(arg).is_some() => {}
                    None => {
                        ctx.out
                            .error(&format!("export: {arg}: give it as NAME=VALUE"));
                        return Some(1);
                    }
                }
            }
            0
        }
        "unset" => {
            shell.vars.retain(|(key, _)| !rest.contains(key));
            0
        }
        "which" | "type" => which(shell, host, rest, ctx.out),
        "true" => 0,
        "false" => 1,
        "whoami" => {
            ctx.out.line("nobody: Corrode has no users.");
            ctx.out
                .line("What you may do is what this terminal holds; caps lists it.");
            0
        }
        "sudo" | "su" | "doas" => {
            ctx.out.error(&format!(
                "{name}: Corrode has no root to become. To run a command that needs more"
            ));
            ctx.out
                .error("than this terminal holds, ask for just that: grant COMMAND");
            1
        }
        _ => return None,
    })
}

/// What every file-reading command shares.
struct Ctx<'c, 'f> {
    name: &'c str,
    cwd: String,
    fs: &'c Fs<'f>,
    input: &'c str,
    out: &'c mut Output,
}

impl<'f> Ctx<'_, 'f> {
    fn path(&self, path: &str) -> String {
        fs::resolve(&self.cwd, path)
    }

    fn missing(&mut self, path: &str) {
        let message = format!("{}: {path}: No such file or directory", self.name);
        self.out.error(&message);
    }

    /// The text of each file named, or of the input if none are: a label
    /// for each (`None` for the input) and the text. Files that cannot be
    /// read are reported and left out.
    fn texts(&mut self, files: &[String]) -> (Vec<(Option<String>, String)>, bool) {
        if files.is_empty() {
            return (alloc::vec![(None, self.input.to_string())], true);
        }
        let mut texts = Vec::new();
        let mut ok = true;
        for file in files {
            if file == "-" {
                texts.push((None, self.input.to_string()));
                continue;
            }
            match self.fs.get(&self.path(file)) {
                Some(node) if node.is_dir() => {
                    let message = format!("{}: {file}: Is a directory", self.name);
                    self.out.error(&message);
                    ok = false;
                }
                Some(node) => {
                    let text = String::from_utf8_lossy(node.data).into_owned();
                    texts.push((Some(file.clone()), text));
                }
                None => {
                    self.missing(file);
                    ok = false;
                }
            }
        }
        (texts, ok)
    }
}

/// Split arguments into single-letter flags and the rest. `--` ends flags,
/// and a lone `-` is not one.
fn flags(args: &[String]) -> (Vec<char>, Vec<String>) {
    let mut letters = Vec::new();
    let mut rest = Vec::new();
    let mut done = false;
    for arg in args {
        if done || arg == "-" || !arg.starts_with('-') {
            rest.push(arg.clone());
        } else if arg == "--" {
            done = true;
        } else {
            letters.extend(arg[1..].chars());
        }
    }
    (letters, rest)
}

fn usage(out: &mut Output, how: &str) -> i32 {
    out.error(&format!("usage: {how}"));
    2
}

fn status(ok: bool) -> i32 {
    if ok {
        0
    } else {
        1
    }
}

fn help(host: &dyn Host, out: &mut Output) -> i32 {
    out.line("Commands; most work in pipes, as in: cat FILE | grep WORD | wc -l");
    for (name, args, what) in BUILTINS.iter().chain(host.commands()) {
        out.line(&format!("  {name} {args}  -  {what}"));
    }
    out.line("Keys: arrows, Home/End, Tab to complete, Ctrl+A/E/U/K/W/C/L");
    0
}

fn echo(args: &[String], out: &mut Output) -> i32 {
    let (newline, words) = match args.first().map(String::as_str) {
        Some("-n") => (false, &args[1..]),
        _ => (true, args),
    };
    out.text.push_str(&words.join(" "));
    if newline {
        out.text.push('\n');
    }
    0
}

fn cd(shell: &mut Shell, args: &[String], fs: &Fs, out: &mut Output) -> i32 {
    let home = shell.var("HOME").unwrap_or_else(|| String::from("/"));
    let target = args.first().cloned().unwrap_or(home);
    let path = fs::resolve(&shell.cwd, &target);
    match fs.get(&path) {
        Some(node) if node.is_dir() => {
            shell.cwd = path;
            0
        }
        Some(_) => {
            out.error(&format!("cd: {target}: Not a directory"));
            1
        }
        None => {
            out.error(&format!("cd: {target}: No such file or directory"));
            1
        }
    }
}

fn ls(ctx: &mut Ctx, args: &[String]) -> i32 {
    let (letters, mut paths) = flags(args);
    let all = letters.contains(&'a');
    let long = letters.contains(&'l');
    if let Some(bad) = letters.iter().find(|c| !"al1F".contains(**c)) {
        let message = format!("ls: unknown option -{bad}");
        ctx.out.error(&message);
        return 2;
    }
    if paths.is_empty() {
        paths.push(String::from("."));
    }
    let entry = |node: &Node, name: &str| {
        let shown = if node.is_dir() {
            format!("{name}/")
        } else {
            name.to_string()
        };
        if long {
            format!(
                "{} {:>7}  {shown}",
                fs::mode_string(node.kind, node.mode),
                node.data.len()
            )
        } else {
            shown
        }
    };
    let mut ok = true;
    let several = paths.len() > 1;
    for (index, path) in paths.iter().enumerate() {
        let full = ctx.path(path);
        let Some(node) = ctx.fs.get(&full) else {
            let message = format!("ls: cannot access '{path}': No such file or directory");
            ctx.out.error(&message);
            ok = false;
            continue;
        };
        if !node.is_dir() {
            let line = entry(node, path);
            ctx.out.line(&line);
            continue;
        }
        if several {
            if index > 0 {
                ctx.out.line("");
            }
            ctx.out.line(&format!("{path}:"));
        }
        let lines: Vec<String> = ctx
            .fs
            .children(&full)
            .filter(|child| all || !child.name().starts_with('.'))
            .map(|child| entry(child, child.name()))
            .collect();
        for line in lines {
            ctx.out.line(&line);
        }
    }
    if ok {
        0
    } else {
        2
    }
}

fn cat(ctx: &mut Ctx, args: &[String]) -> i32 {
    let (texts, ok) = ctx.texts(args);
    for (_, text) in texts {
        ctx.out.text.push_str(&text);
        if !text.is_empty() && !text.ends_with('\n') {
            ctx.out.text.push('\n');
        }
    }
    status(ok)
}

/// `head` and `tail`: `-n N`, `-nN` or `-N` lines, ten if not said.
fn ends(ctx: &mut Ctx, args: &[String], head: bool) -> i32 {
    let mut count = 10usize;
    let mut files = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        let number = if arg == "-n" {
            i += 1;
            args.get(i).map(String::as_str)
        } else if let Some(n) = arg.strip_prefix("-n") {
            Some(n)
        } else if arg.len() > 1 && arg.starts_with('-') {
            Some(&arg[1..])
        } else {
            files.push(arg.clone());
            i += 1;
            continue;
        };
        match number.and_then(|n| n.parse().ok()) {
            Some(n) => count = n,
            None => {
                return usage(
                    ctx.out,
                    if head {
                        "head [-n N] [FILE]"
                    } else {
                        "tail [-n N] [FILE]"
                    },
                )
            }
        }
        i += 1;
    }
    let (texts, ok) = ctx.texts(&files);
    let several = texts.len() > 1;
    for (index, (label, text)) in texts.iter().enumerate() {
        if several {
            if index > 0 {
                ctx.out.line("");
            }
            ctx.out
                .line(&format!("==> {} <==", label.as_deref().unwrap_or("-")));
        }
        let lines: Vec<&str> = text.lines().collect();
        let picked = if head {
            &lines[..count.min(lines.len())]
        } else {
            &lines[lines.len().saturating_sub(count)..]
        };
        for line in picked {
            ctx.out.line(line);
        }
    }
    status(ok)
}

fn wc(ctx: &mut Ctx, args: &[String]) -> i32 {
    let (letters, files) = flags(args);
    let mut want = [
        letters.contains(&'l'),
        letters.contains(&'w'),
        letters.contains(&'c'),
    ];
    if want == [false; 3] {
        want = [true; 3];
    }
    let (texts, ok) = ctx.texts(&files);
    let mut total = [0usize; 3];
    let several = texts.len() > 1;
    for (label, text) in &texts {
        let counts = [
            text.matches('\n').count(),
            text.split_whitespace().count(),
            text.len(),
        ];
        for (sum, count) in total.iter_mut().zip(counts) {
            *sum += count;
        }
        let line = counted(want, counts, label.as_deref());
        ctx.out.line(&line);
    }
    if several {
        let line = counted(want, total, Some("total"));
        ctx.out.line(&line);
    }
    status(ok)
}

fn counted(want: [bool; 3], counts: [usize; 3], label: Option<&str>) -> String {
    let mut parts: Vec<String> = want
        .iter()
        .zip(counts)
        .filter(|(on, _)| **on)
        .map(|(_, count)| count.to_string())
        .collect();
    parts.extend(label.map(str::to_string));
    parts.join(" ")
}

fn grep(ctx: &mut Ctx, args: &[String]) -> i32 {
    let (letters, rest) = flags(args);
    let ignore = letters.contains(&'i');
    let invert = letters.contains(&'v');
    let numbers = letters.contains(&'n');
    let count = letters.contains(&'c');
    let fixed = letters.contains(&'F');
    let quiet = letters.contains(&'q');
    if let Some(bad) = letters.iter().find(|c| !"ivncFq".contains(**c)) {
        let message = format!("grep: unknown option -{bad}");
        ctx.out.error(&message);
        return 2;
    }
    let Some((wanted, files)) = rest.split_first() else {
        return usage(ctx.out, "grep [-ivncqF] PATTERN [FILE...]");
    };
    let wanted = if ignore {
        wanted.to_lowercase()
    } else {
        wanted.clone()
    };
    let (texts, ok) = ctx.texts(files);
    let several = texts.len() > 1;
    let mut found = false;
    for (label, text) in &texts {
        let mut matched = 0;
        for (number, line) in text.lines().enumerate() {
            let subject = if ignore {
                line.to_lowercase()
            } else {
                line.to_string()
            };
            let hit = if fixed {
                subject.contains(wanted.as_str())
            } else {
                pattern::matches(&wanted, &subject)
            };
            if hit == invert {
                continue;
            }
            matched += 1;
            if count || quiet {
                continue;
            }
            let mut shown = String::new();
            if several {
                shown.push_str(label.as_deref().unwrap_or("-"));
                shown.push(':');
            }
            if numbers {
                shown.push_str(&format!("{}:", number + 1));
            }
            shown.push_str(line);
            ctx.out.line(&shown);
        }
        if count && !quiet {
            let line = match (several, label) {
                (true, Some(label)) => format!("{label}:{matched}"),
                _ => matched.to_string(),
            };
            ctx.out.line(&line);
        }
        found |= matched > 0;
    }
    match (ok, found) {
        (false, _) => 2,
        (true, true) => 0,
        (true, false) => 1,
    }
}

fn sort(ctx: &mut Ctx, args: &[String]) -> i32 {
    let (letters, files) = flags(args);
    let (texts, ok) = ctx.texts(&files);
    let mut lines: Vec<String> = texts
        .iter()
        .flat_map(|(_, text)| text.lines().map(str::to_string))
        .collect();
    if letters.contains(&'n') {
        let key = |line: &str| -> i64 {
            let digits: String = line
                .trim_start()
                .chars()
                .enumerate()
                .take_while(|(i, c)| c.is_ascii_digit() || (*i == 0 && *c == '-'))
                .map(|(_, c)| c)
                .collect();
            digits.parse().unwrap_or(0)
        };
        lines.sort_by(|a, b| key(a).cmp(&key(b)).then_with(|| a.cmp(b)));
    } else {
        lines.sort();
    }
    if letters.contains(&'u') {
        lines.dedup();
    }
    if letters.contains(&'r') {
        lines.reverse();
    }
    for line in lines {
        ctx.out.line(&line);
    }
    status(ok)
}

fn uniq(ctx: &mut Ctx, args: &[String]) -> i32 {
    let (letters, files) = flags(args);
    let (texts, ok) = ctx.texts(&files);
    let mut runs: Vec<(usize, String)> = Vec::new();
    for (_, text) in &texts {
        for line in text.lines() {
            match runs.last_mut() {
                Some((count, last)) if last == line => *count += 1,
                _ => runs.push((1, line.to_string())),
            }
        }
    }
    for (count, line) in runs {
        if letters.contains(&'c') {
            ctx.out.line(&format!("{count:>7} {line}"));
        } else {
            ctx.out.line(&line);
        }
    }
    status(ok)
}

/// `1`, `2-4`, `3-`, `-2`, joined by commas: which fields to keep,
/// counting from one.
fn ranges(list: &str) -> Option<Vec<(usize, usize)>> {
    list.split(',')
        .map(|part| {
            let (from, to) = match part.split_once('-') {
                Some((from, to)) => (
                    if from.is_empty() {
                        1
                    } else {
                        from.parse().ok()?
                    },
                    if to.is_empty() {
                        usize::MAX
                    } else {
                        to.parse().ok()?
                    },
                ),
                None => {
                    let n = part.parse().ok()?;
                    (n, n)
                }
            };
            (from >= 1 && from <= to).then_some((from, to))
        })
        .collect()
}

fn cut(ctx: &mut Ctx, args: &[String]) -> i32 {
    let mut delimiter = '\t';
    let mut fields = None;
    let mut chars = None;
    let mut files = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        let mut value = |flag: &str| -> Option<String> {
            if arg == flag {
                i += 1;
                args.get(i).cloned()
            } else {
                arg.strip_prefix(flag).map(str::to_string)
            }
        };
        if let Some(d) = value("-d") {
            let mut d = d.chars();
            match (d.next(), d.next()) {
                (Some(c), None) => delimiter = c,
                _ => return usage(ctx.out, "cut: the delimiter must be one character"),
            }
        } else if let Some(list) = value("-f") {
            fields = ranges(&list);
            if fields.is_none() {
                return usage(ctx.out, "cut -f LIST, as 1,3 or 2-4");
            }
        } else if let Some(list) = value("-c") {
            chars = ranges(&list);
            if chars.is_none() {
                return usage(ctx.out, "cut -c LIST, as 1,3 or 2-4");
            }
        } else {
            files.push(arg.to_string());
        }
        i += 1;
    }
    let keep = |list: &[(usize, usize)], n: usize| list.iter().any(|(a, b)| (*a..=*b).contains(&n));
    let (texts, ok) = ctx.texts(&files);
    for (_, text) in &texts {
        for line in text.lines() {
            let picked = match (&fields, &chars) {
                (Some(list), _) => {
                    if !line.contains(delimiter) {
                        line.to_string()
                    } else {
                        let parts: Vec<&str> = line
                            .split(delimiter)
                            .enumerate()
                            .filter(|(n, _)| keep(list, n + 1))
                            .map(|(_, part)| part)
                            .collect();
                        parts.join(&delimiter.to_string())
                    }
                }
                (None, Some(list)) => line
                    .chars()
                    .enumerate()
                    .filter(|(n, _)| keep(list, n + 1))
                    .map(|(_, c)| c)
                    .collect(),
                (None, None) => return usage(ctx.out, "cut -d D -f LIST | -c LIST"),
            };
            ctx.out.line(&picked);
        }
    }
    status(ok)
}

/// `a-z` and the like, spelled out.
fn char_set(spec: &str) -> Vec<char> {
    let chars: Vec<char> = spec.chars().collect();
    let mut set = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if i + 2 < chars.len() && chars[i + 1] == '-' && chars[i] <= chars[i + 2] {
            set.extend(chars[i]..=chars[i + 2]);
            i += 3;
        } else {
            set.push(chars[i]);
            i += 1;
        }
    }
    set
}

fn tr(ctx: &mut Ctx, args: &[String]) -> i32 {
    let text = ctx.input;
    let translated: String = match args {
        [flag, set] if flag == "-d" => {
            let set = char_set(set);
            text.chars().filter(|c| !set.contains(c)).collect()
        }
        [from, to] if !from.starts_with('-') => {
            let (from, to) = (char_set(from), char_set(to));
            let Some(last) = to.last().copied() else {
                return usage(ctx.out, "tr SET1 SET2");
            };
            text.chars()
                .map(|c| match from.iter().position(|f| *f == c) {
                    Some(at) => to.get(at).copied().unwrap_or(last),
                    None => c,
                })
                .collect()
        }
        _ => return usage(ctx.out, "tr SET1 SET2, or tr -d SET"),
    };
    ctx.out.text.push_str(&translated);
    0
}

fn each_text(ctx: &mut Ctx, args: &[String], each: impl Fn(&str, &mut Output)) -> i32 {
    let (texts, ok) = ctx.texts(args);
    for (_, text) in &texts {
        each(text, ctx.out);
    }
    status(ok)
}

/// Most numbers `seq` will print, so a slip cannot fill memory.
const SEQ_MAX: i64 = 100_000;

fn seq(args: &[String], out: &mut Output) -> i32 {
    let numbers: Option<Vec<i64>> = args.iter().map(|arg| arg.parse().ok()).collect();
    let (first, step, last) = match numbers.as_deref() {
        Some([last]) => (1, 1, *last),
        Some([first, last]) => (*first, 1, *last),
        Some([first, step, last]) if *step != 0 => (*first, *step, *last),
        _ => return usage(out, "seq [FIRST [STEP]] LAST, in whole numbers"),
    };
    let mut n = first;
    let mut printed = 0;
    while (step > 0 && n <= last) || (step < 0 && n >= last) {
        if printed == SEQ_MAX {
            out.error(&format!("seq: stopped after {SEQ_MAX} numbers"));
            return 1;
        }
        out.line(&n.to_string());
        printed += 1;
        let Some(next) = n.checked_add(step) else {
            break;
        };
        n = next;
    }
    0
}

fn find(ctx: &mut Ctx, args: &[String]) -> i32 {
    let mut start = String::from(".");
    let mut name: Option<String> = None;
    let mut kind: Option<Kind> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-name" => {
                i += 1;
                match args.get(i) {
                    Some(pattern) => name = Some(pattern.clone()),
                    None => return usage(ctx.out, "find [PATH] [-name PATTERN] [-type f|d]"),
                }
            }
            "-type" => {
                i += 1;
                kind = match args.get(i).map(String::as_str) {
                    Some("f") => Some(Kind::File),
                    Some("d") => Some(Kind::Dir),
                    _ => return usage(ctx.out, "find [PATH] [-name PATTERN] [-type f|d]"),
                };
            }
            arg if arg.starts_with('-') => {
                let message = format!("find: unknown option {arg}");
                ctx.out.error(&message);
                return 2;
            }
            arg if i == 0 => start = arg.to_string(),
            arg => {
                let message = format!("find: {arg}: give the path first");
                ctx.out.error(&message);
                return 2;
            }
        }
        i += 1;
    }
    let root = ctx.path(&start);
    let Some(top) = ctx.fs.get(&root) else {
        ctx.missing(&start);
        return 1;
    };
    let wanted = |node: &Node| {
        kind.is_none_or(|kind| node.kind == kind)
            && name
                .as_deref()
                .is_none_or(|pattern| fs::glob_match(pattern, node.name()))
    };
    let mut lines = Vec::new();
    if wanted(top) {
        lines.push(start.clone());
    }
    let prefix = start.trim_end_matches('/');
    for node in ctx.fs.under(&root).filter(|node| wanted(node)) {
        let relative = if root == "/" {
            &node.path[1..]
        } else {
            &node.path[root.len() + 1..]
        };
        lines.push(if prefix.is_empty() {
            format!("/{relative}")
        } else {
            format!("{prefix}/{relative}")
        });
    }
    for line in lines {
        ctx.out.line(&line);
    }
    0
}

fn tree(ctx: &mut Ctx, args: &[String]) -> i32 {
    let start = args.first().cloned().unwrap_or_else(|| String::from("."));
    let root = ctx.path(&start);
    match ctx.fs.get(&root) {
        Some(node) if node.is_dir() => {}
        Some(_) => {
            let message = format!("tree: {start}: Not a directory");
            ctx.out.error(&message);
            return 1;
        }
        None => {
            ctx.missing(&start);
            return 1;
        }
    }
    ctx.out.line(&start);
    let (mut dirs, mut files) = (0, 0);
    let mut lines = Vec::new();
    branch(ctx.fs, &root, "", &mut lines, &mut dirs, &mut files);
    for line in lines {
        ctx.out.line(&line);
    }
    ctx.out.line("");
    ctx.out.line(&format!(
        "{dirs} director{}, {files} file{}",
        if dirs == 1 { "y" } else { "ies" },
        if files == 1 { "" } else { "s" }
    ));
    0
}

fn branch(
    fs: &Fs,
    dir: &str,
    indent: &str,
    lines: &mut Vec<String>,
    dirs: &mut usize,
    files: &mut usize,
) {
    let children: Vec<&Node> = fs
        .children(dir)
        .filter(|node| !node.name().starts_with('.'))
        .collect();
    for (index, child) in children.iter().enumerate() {
        let last = index + 1 == children.len();
        let (stem, more) = if last {
            ("`-- ", "    ")
        } else {
            ("|-- ", "|   ")
        };
        lines.push(format!("{indent}{stem}{}", child.name()));
        if child.is_dir() {
            *dirs += 1;
            branch(
                fs,
                &child.path,
                &format!("{indent}{more}"),
                lines,
                dirs,
                files,
            );
        } else {
            *files += 1;
        }
    }
}

fn stat(ctx: &mut Ctx, args: &[String]) -> i32 {
    if args.is_empty() {
        return usage(ctx.out, "stat FILE...");
    }
    let mut ok = true;
    for arg in args {
        let Some(node) = ctx.fs.get(&ctx.path(arg)).cloned() else {
            ctx.missing(arg);
            ok = false;
            continue;
        };
        ctx.out.line(&format!("  File: {}", node.path));
        ctx.out.line(&format!(
            "  Size: {}  Type: {}",
            node.data.len(),
            if node.is_dir() {
                "directory"
            } else {
                "regular file"
            }
        ));
        ctx.out.line(&format!(
            "Access: {:04o} ({})  read-only: loaded from the boot medium",
            node.mode & 0o7777,
            fs::mode_string(node.kind, node.mode)
        ));
    }
    status(ok)
}

fn file(ctx: &mut Ctx, args: &[String]) -> i32 {
    if args.is_empty() {
        return usage(ctx.out, "file FILE...");
    }
    let mut ok = true;
    for arg in args {
        let Some(node) = ctx.fs.get(&ctx.path(arg)) else {
            ctx.missing(arg);
            ok = false;
            continue;
        };
        let data = node.data;
        let what = if node.is_dir() {
            "directory"
        } else if data.is_empty() {
            "empty"
        } else if data.starts_with(b"\x7fELF") {
            "ELF executable"
        } else if data.starts_with(b"\x89PNG") {
            "PNG image"
        } else if data.starts_with(b"#!") {
            "script, text"
        } else if data
            .iter()
            .all(|b| b.is_ascii() && (*b >= 0x20 || b"\n\t\r".contains(b)))
        {
            "ASCII text"
        } else if core::str::from_utf8(data).is_ok() {
            "UTF-8 text"
        } else {
            "data"
        };
        ctx.out.line(&format!("{arg}: {what}"));
    }
    status(ok)
}

fn which(shell: &Shell, host: &dyn Host, args: &[String], out: &mut Output) -> i32 {
    if args.is_empty() {
        return usage(out, "which NAME...");
    }
    let mut ok = true;
    for arg in args {
        if BUILTINS.iter().any(|(name, _, _)| name == arg) {
            out.line(&format!("{arg}: shell builtin"));
        } else if host.commands().iter().any(|(name, _, _)| name == arg) {
            out.line(&format!("{arg}: terminal command"));
        } else if shell.var(arg).is_some() {
            out.line(&format!("{arg}: a variable, not a command"));
            ok = false;
        } else {
            out.error(&format!("which: no {arg}"));
            ok = false;
        }
    }
    status(ok)
}
