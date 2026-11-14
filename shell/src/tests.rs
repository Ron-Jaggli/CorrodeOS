use super::*;
use alloc::vec;

/// A ustar archive of `(path, contents)`; a path ending in `/` is a
/// directory.
fn tar(entries: &[(&str, &str)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (path, contents) in entries {
        let mut header = [0u8; 512];
        let dir = path.ends_with('/');
        header[..path.len()].copy_from_slice(path.as_bytes());
        let mode = if dir { "0000755\0" } else { "0000644\0" };
        header[100..108].copy_from_slice(mode.as_bytes());
        let size = format!("{:011o}\0", if dir { 0 } else { contents.len() });
        header[124..136].copy_from_slice(size.as_bytes());
        header[136..148].copy_from_slice(b"00000000000\0");
        header[156] = if dir { b'5' } else { b'0' };
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        header[148..156].copy_from_slice(b"        ");
        let sum: u32 = header.iter().map(|b| *b as u32).sum();
        header[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        out.extend_from_slice(&header);
        if !dir {
            out.extend_from_slice(contents.as_bytes());
            out.resize(out.len().div_ceil(512) * 512, 0);
        }
    }
    out.resize(out.len() + 1024, 0);
    out
}

fn sample() -> Vec<u8> {
    tar(&[
        ("./", ""),
        ("./etc/", ""),
        ("./etc/motd", "Welcome to Corrode.\nRead the docs.\n"),
        ("./docs/a.txt", "apple\nbanana\napple\ncherry\n"),
        ("./docs/b.txt", "one two three\nfour\n"),
        ("./docs/.hidden", "secret\n"),
        ("./home/notes.md", "# notes\n"),
        ("./numbers", "10\n9\n100\n-3\n"),
    ])
}

/// Run `line` in a fresh shell over the sample files, and give back what
/// it printed: output lines as they are, error lines with `!` in front.
fn run(line: &str) -> Vec<String> {
    let bytes = sample();
    let fs = Fs::from_tar(&bytes).unwrap();
    let mut shell = Shell::new();
    printed(&mut shell, &fs, line)
}

fn printed(shell: &mut Shell, fs: &Fs, line: &str) -> Vec<String> {
    shell
        .run(line, fs, &mut NoHost)
        .into_iter()
        .map(|line| {
            if line.error {
                format!("!{}", line.text)
            } else {
                line.text
            }
        })
        .collect()
}

// Parsing.

fn words(line: &str) -> Vec<Vec<String>> {
    let lookup = |name: &str| (name == "X").then(|| String::from("ex"));
    parse::parse(line)
        .unwrap()
        .into_iter()
        .flat_map(|(_, pipeline)| pipeline.stages)
        .map(|stage| {
            stage
                .words
                .iter()
                .filter_map(|word| word.expand(&lookup))
                .collect()
        })
        .collect()
}

#[test]
fn quotes_keep_what_they_hold() {
    assert_eq!(
        words(r#"echo 'a  b' "c $X" d\ e $X ${X}y '$X' "q\"q""#),
        [["echo", "a  b", "c ex", "d e", "ex", "exy", "$X", "q\"q"]]
    );
}

#[test]
fn empty_quotes_are_a_word() {
    assert_eq!(words("echo '' x"), [["echo", "", "x"]]);
}

#[test]
fn unknown_variables_are_empty_and_a_lone_dollar_is_itself() {
    assert_eq!(
        words("echo $NOPE $ 5$ \"$NOPE\""),
        [["echo", "$", "5$", ""]]
    );
}

#[test]
fn pipes_and_lists_split_commands() {
    assert_eq!(
        words("a | b c; d && e || f"),
        [vec!["a"], vec!["b", "c"], vec!["d"], vec!["e"], vec!["f"]]
    );
}

#[test]
fn comments_end_the_line() {
    assert_eq!(words("echo hi # not this"), [["echo", "hi"]]);
    assert_eq!(words("echo a#b"), [["echo", "a#b"]]);
}

#[test]
fn bad_lines_are_errors_not_guesses() {
    for line in [
        "echo 'open",
        "a |",
        "| a",
        "a && ",
        "; a",
        "a > f",
        "a &",
        "a | | b",
        "<",
    ] {
        assert!(parse::parse(line).is_err(), "{line:?}");
    }
    assert!(parse::parse("").unwrap().is_empty());
    assert!(parse::parse("a ;").is_ok());
}

#[test]
fn only_unquoted_stars_glob() {
    let list = parse::parse("ls *.txt '*.md' \\*").unwrap();
    let globs: Vec<bool> = list[0].1.stages[0].words.iter().map(|w| w.glob).collect();
    assert_eq!(globs, [false, true, false, false]);
}

// Files.

#[test]
fn an_archive_becomes_a_tree() {
    let bytes = sample();
    let fs = Fs::from_tar(&bytes).unwrap();
    let names: Vec<&str> = fs.children("/").map(|node| node.name()).collect();
    assert_eq!(names, ["docs", "etc", "home", "numbers"]);
    let motd = fs.get("/etc/motd").unwrap();
    assert_eq!(motd.data, b"Welcome to Corrode.\nRead the docs.\n");
    assert_eq!(motd.mode, 0o644);
    // Made because a file is in it, though the archive never says so.
    assert!(fs.get("/home").unwrap().is_dir());
}

#[test]
fn a_damaged_archive_is_an_error() {
    let mut bytes = sample();
    bytes[0] ^= 1;
    assert_eq!(Fs::from_tar(&bytes).unwrap_err(), fs::Error::Checksum);

    let bytes = sample();
    // Cut inside the first file's contents.
    let cut = &bytes[..512 * 3 + 10];
    assert_eq!(Fs::from_tar(cut).unwrap_err(), fs::Error::Truncated);
    assert!(Fs::from_tar(&[]).unwrap().is_empty());
}

#[test]
fn paths_resolve() {
    assert_eq!(fs::resolve("/a/b", "../c"), "/a/c");
    assert_eq!(fs::resolve("/a", "./b//c/"), "/a/b/c");
    assert_eq!(fs::resolve("/a", "/x/../y"), "/y");
    assert_eq!(fs::resolve("/", "../.."), "/");
    assert_eq!(fs::resolve("/a", ""), "/a");
}

#[test]
fn globs_match_names() {
    for (pattern, name, want) in [
        ("*.txt", "a.txt", true),
        ("*.txt", "a.txt.bak", false),
        ("a?c", "abc", true),
        ("a*b*c", "aXXbYYc", true),
        ("a*b*c", "aXXbYY", false),
        ("*", "", true),
        ("", "x", false),
    ] {
        assert_eq!(fs::glob_match(pattern, name), want, "{pattern} {name}");
    }
}

// Patterns.

#[test]
fn grep_patterns() {
    for (pattern, line, want) in [
        ("an", "banana", true),
        ("^ban", "banana", true),
        ("^an", "banana", false),
        ("na$", "banana", true),
        ("b.n", "banana", true),
        ("ba*n", "bn", true),
        ("x*", "anything", true),
        ("a\\.b", "a.b", true),
        ("a\\.b", "axb", false),
        ("^$", "", true),
    ] {
        assert_eq!(pattern::matches(pattern, line), want, "{pattern} {line}");
    }
}

// Commands.

#[test]
fn echo_and_variables() {
    assert_eq!(run("echo hello   world"), ["hello world"]);
    assert_eq!(
        run("X=1; echo $X; export Y=2; echo $Y; unset Y; echo [$Y]"),
        ["1", "2", "[]"]
    );
    assert_eq!(run("echo $HOME"), ["/home"]);
}

#[test]
fn ls_lists_directories() {
    assert_eq!(run("ls"), ["docs/", "etc/", "home/", "numbers"]);
    assert_eq!(run("ls docs"), ["a.txt", "b.txt"]);
    assert_eq!(run("ls -a docs"), [".hidden", "a.txt", "b.txt"]);
    assert_eq!(run("ls -l etc"), ["-rw-r--r--      35  motd"]);
    assert_eq!(
        run("ls nope"),
        ["!ls: cannot access 'nope': No such file or directory"]
    );
}

#[test]
fn cd_and_pwd() {
    let bytes = sample();
    let fs = Fs::from_tar(&bytes).unwrap();
    let mut shell = Shell::new();
    assert_eq!(
        printed(&mut shell, &fs, "cd docs; pwd; ls"),
        ["/docs", "a.txt", "b.txt"]
    );
    assert_eq!(printed(&mut shell, &fs, "cd ..; cd; pwd"), ["/home"]);
    assert_eq!(
        printed(&mut shell, &fs, "cd /etc/motd"),
        ["!cd: /etc/motd: Not a directory"]
    );
    assert_eq!(shell.status(), 1);
    assert_eq!(shell.cwd(), "/home");
}

#[test]
fn pipes_carry_output() {
    assert_eq!(run("cat docs/a.txt | grep apple | wc -l"), ["2"]);
    assert_eq!(
        run("cat docs/a.txt | sort | uniq -c"),
        ["      2 apple", "      1 banana", "      1 cherry"]
    );
    assert_eq!(run("echo hello | tr a-z A-Z"), ["HELLO"]);
    assert_eq!(run("echo a,b,c | cut -d , -f 2-"), ["b,c"]);
    assert_eq!(run("seq 5 | tail -n 2"), ["4", "5"]);
    assert_eq!(run("seq 3 | tac | head -1"), ["3"]);
}

#[test]
fn input_from_a_file() {
    assert_eq!(run("grep -c an < docs/a.txt"), ["1"]);
    assert_eq!(
        run("wc -l < nope"),
        ["!shell: nope: No such file or directory"]
    );
}

#[test]
fn globs_become_file_names() {
    assert_eq!(run("echo docs/*.txt"), ["docs/a.txt docs/b.txt"]);
    assert_eq!(run("cd docs; echo *"), ["a.txt b.txt"]);
    assert_eq!(run("echo *.none"), ["*.none"]);
    assert_eq!(run("echo '*'"), ["*"]);
}

#[test]
fn grep_flags_and_status() {
    assert_eq!(run("grep -n an docs/a.txt"), ["2:banana"]);
    assert_eq!(run("grep -v three docs/b.txt"), ["four"]);
    assert_eq!(run("grep -i WELCOME etc/motd"), ["Welcome to Corrode."]);
    assert_eq!(
        run("grep -c apple docs/*.txt"),
        ["docs/a.txt:2", "docs/b.txt:0"]
    );
    assert_eq!(run("grep zzz docs/a.txt || echo none"), ["none"]);
    assert_eq!(run("grep -q apple docs/a.txt && echo found"), ["found"]);
    assert_eq!(
        run("grep apple docs/a.txt > /dev/null"),
        ["!shell: cannot write files: the file system is read-only"]
    );
}

#[test]
fn and_or_follow_status() {
    assert_eq!(run("true && echo yes || echo no"), ["yes"]);
    assert_eq!(run("false && echo yes || echo no"), ["no"]);
    assert_eq!(run("false; echo $?"), ["1"]);
    assert_eq!(
        run("nope; echo $?"),
        ["!nope: command not found (try help)", "127"]
    );
}

#[test]
fn sort_numbers_and_words() {
    assert_eq!(run("sort numbers"), ["-3", "10", "100", "9"]);
    assert_eq!(run("sort -n numbers"), ["-3", "9", "10", "100"]);
    assert_eq!(run("sort -rn numbers | head -n 1"), ["100"]);
    assert_eq!(run("sort -u docs/a.txt"), ["apple", "banana", "cherry"]);
}

#[test]
fn wc_counts() {
    assert_eq!(run("wc docs/b.txt"), ["2 4 19 docs/b.txt"]);
    assert_eq!(
        run("wc -w docs/a.txt docs/b.txt"),
        ["4 docs/a.txt", "4 docs/b.txt", "8 total"]
    );
}

#[test]
fn find_and_tree_walk() {
    assert_eq!(
        run("find docs"),
        ["docs", "docs/.hidden", "docs/a.txt", "docs/b.txt"]
    );
    assert_eq!(run("find / -name '*.txt'"), ["/docs/a.txt", "/docs/b.txt"]);
    assert_eq!(run("find . -type d"), [".", "./docs", "./etc", "./home"]);
    assert_eq!(
        run("tree docs"),
        [
            "docs",
            "|-- a.txt",
            "`-- b.txt",
            "",
            "0 directories, 2 files"
        ]
    );
}

#[test]
fn head_tail_and_friends() {
    assert_eq!(
        run("head -n 1 docs/a.txt docs/b.txt"),
        [
            "==> docs/a.txt <==",
            "apple",
            "",
            "==> docs/b.txt <==",
            "one two three"
        ]
    );
    assert_eq!(run("echo abc | rev"), ["cba"]);
    assert_eq!(run("echo x | nl"), ["     1  x"]);
    assert_eq!(
        run("basename /a/b/c.txt; dirname /a/b/c.txt; dirname c"),
        ["c.txt", "/a/b", "."]
    );
    assert_eq!(
        run("file etc/motd docs"),
        ["etc/motd: ASCII text", "docs: directory"]
    );
}

#[test]
fn errors_are_kept_apart_from_output() {
    assert_eq!(
        run("cat etc/motd nope | wc -l"),
        ["!cat: nope: No such file or directory", "2"]
    );
    assert_eq!(run("cat docs"), ["!cat: docs: Is a directory"]);
}

#[test]
fn no_root_here() {
    let printed = run("sudo ls");
    assert!(printed[0].starts_with("!sudo: Corrode has no root"));
    assert!(run("whoami")[0].starts_with("nobody"));
}

#[test]
fn seq_is_bounded() {
    let printed = run("seq 1000000 | wc -l");
    assert_eq!(printed, ["!seq: stopped after 100000 numbers", "100000"]);
}

struct Echoes;

impl Host for Echoes {
    fn commands(&self) -> &[About] {
        &[("shout", "TEXT", "say TEXT loudly")]
    }

    fn run(&mut self, args: &[String], input: &str, out: &mut Output) -> Option<i32> {
        if args[0] != "shout" {
            return None;
        }
        out.line(&args[1..].join(" ").to_uppercase());
        out.text.push_str(&input.to_uppercase());
        Some(0)
    }
}

#[test]
fn a_host_adds_commands_that_pipe() {
    let fs = Fs::empty();
    let mut shell = Shell::new();
    let lines: Vec<String> = shell
        .run("echo low | shout hi | grep -c LOW", &fs, &mut Echoes)
        .into_iter()
        .map(|line| line.text)
        .collect();
    assert_eq!(lines, ["1"]);
    assert!(shell.names(&Echoes).contains(&"shout"));
    assert_eq!(
        shell.run("which shout", &fs, &mut Echoes)[0].text,
        "shout: terminal command"
    );
}
