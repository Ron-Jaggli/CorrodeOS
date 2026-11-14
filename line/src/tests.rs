use super::*;

fn typed(text: &str) -> Editor {
    let mut editor = Editor::new();
    put(&mut editor, text);
    editor
}

fn put(editor: &mut Editor, text: &str) {
    for c in text.chars() {
        editor.key(Key(0), Some(c), 0);
    }
}

fn press(editor: &mut Editor, key: Key) -> Outcome {
    editor.key(key, None, 0)
}

fn ctrl(editor: &mut Editor, c: char) -> Outcome {
    editor.key(Key(0), Some(c), modifier::CTRL)
}

fn enter(editor: &mut Editor) -> Outcome {
    editor.key(Key::ENTER, Some('\n'), 0)
}

#[test]
fn typing_puts_text_at_the_cursor() {
    let mut editor = typed("helo");
    press(&mut editor, Key::LEFT);
    put(&mut editor, "l");
    assert_eq!(editor.line(), "hello");
    assert_eq!(editor.cursor(), 4);
}

#[test]
fn the_cursor_stays_on_the_line() {
    let mut editor = typed("ab");
    for _ in 0..5 {
        press(&mut editor, Key::RIGHT);
    }
    assert_eq!(editor.cursor(), 2);
    for _ in 0..5 {
        press(&mut editor, Key::LEFT);
    }
    assert_eq!(editor.cursor(), 0);
    assert_eq!(press(&mut editor, Key::BACKSPACE), Outcome::Ignored);
    press(&mut editor, Key::END);
    assert_eq!(press(&mut editor, Key::DELETE), Outcome::Ignored);
    assert_eq!(editor.line(), "ab");
}

#[test]
fn home_end_backspace_and_delete() {
    let mut editor = typed("xabcy");
    press(&mut editor, Key::HOME);
    press(&mut editor, Key::DELETE);
    press(&mut editor, Key::END);
    press(&mut editor, Key::BACKSPACE);
    assert_eq!(editor.line(), "abc");
}

#[test]
fn ctrl_chords_move_and_cut() {
    let mut editor = typed("echo one two");
    ctrl(&mut editor, 'w');
    assert_eq!(editor.line(), "echo one ");
    ctrl(&mut editor, 'w');
    assert_eq!(editor.line(), "echo ");
    ctrl(&mut editor, 'a');
    ctrl(&mut editor, 'f');
    ctrl(&mut editor, 'k');
    assert_eq!(editor.line(), "e");
    put(&mut editor, "xit");
    ctrl(&mut editor, 'b');
    ctrl(&mut editor, 'u');
    assert_eq!((editor.line(), editor.cursor()), ("t", 0));
    ctrl(&mut editor, 'd');
    assert_eq!(editor.line(), "");
}

#[test]
fn ctrl_letters_are_not_typed() {
    let mut editor = Editor::new();
    assert_eq!(ctrl(&mut editor, 'z'), Outcome::Ignored);
    assert_eq!(ctrl(&mut editor, 'L'), Outcome::ClearScreen);
    assert_eq!(editor.line(), "");
}

#[test]
fn only_printable_ascii_goes_in() {
    let editor = typed("a\u{1b}\tb\u{e9}");
    assert_eq!(editor.line(), "ab");
}

#[test]
fn the_line_has_a_limit() {
    let editor = typed(&"x".repeat(MAX + 10));
    assert_eq!(editor.line().len(), MAX);
}

#[test]
fn enter_hands_over_the_line_and_remembers_it() {
    let mut editor = typed("  uptime ");
    assert_eq!(enter(&mut editor), Outcome::Submit("  uptime ".into()));
    assert_eq!((editor.line(), editor.cursor()), ("", 0));
    assert_eq!(editor.history(), ["uptime"]);
}

#[test]
fn history_skips_blanks_and_repeats() {
    let mut editor = Editor::new();
    for line in ["a", "", "a", "  ", "b", "a"] {
        put(&mut editor, line);
        enter(&mut editor);
    }
    assert_eq!(editor.history(), ["a", "b", "a"]);
}

#[test]
fn history_is_bounded() {
    let mut editor = Editor::new();
    for n in 0..HISTORY + 5 {
        put(&mut editor, &alloc::format!("{n}"));
        enter(&mut editor);
    }
    assert_eq!(editor.history().len(), HISTORY);
    assert_eq!(editor.history()[0], "5");
}

#[test]
fn up_and_down_walk_history_and_keep_the_draft() {
    let mut editor = Editor::new();
    for line in ["first", "second"] {
        put(&mut editor, line);
        enter(&mut editor);
    }
    put(&mut editor, "dra");
    press(&mut editor, Key::UP);
    assert_eq!((editor.line(), editor.cursor()), ("second", 6));
    press(&mut editor, Key::UP);
    assert_eq!(editor.line(), "first");
    assert_eq!(press(&mut editor, Key::UP), Outcome::Ignored);
    assert_eq!(editor.line(), "first");
    press(&mut editor, Key::DOWN);
    press(&mut editor, Key::DOWN);
    assert_eq!((editor.line(), editor.cursor()), ("dra", 3));
    assert_eq!(press(&mut editor, Key::DOWN), Outcome::Ignored);
}

#[test]
fn up_with_no_history_does_nothing() {
    let mut editor = typed("x");
    assert_eq!(press(&mut editor, Key::UP), Outcome::Ignored);
    assert_eq!(editor.line(), "x");
}

#[test]
fn running_a_recalled_line_starts_fresh() {
    let mut editor = Editor::new();
    put(&mut editor, "one");
    enter(&mut editor);
    press(&mut editor, Key::UP);
    put(&mut editor, "!");
    assert_eq!(enter(&mut editor), Outcome::Submit("one!".into()));
    press(&mut editor, Key::UP);
    assert_eq!(editor.line(), "one!");
    press(&mut editor, Key::DOWN);
    assert_eq!(editor.line(), "");
}

#[test]
fn ctrl_c_abandons_the_line() {
    let mut editor = typed("oops");
    assert_eq!(ctrl(&mut editor, 'c'), Outcome::Cancel("oops".into()));
    assert_eq!(editor.line(), "");
    assert!(editor.history().is_empty());
}

const NAMES: [&str; 5] = ["help", "history", "echo", "uname", "uptime"];

#[test]
fn one_match_completes_with_a_space() {
    let mut editor = typed("ec");
    assert_eq!(press(&mut editor, Key::TAB), Outcome::Complete);
    assert!(editor.complete(&NAMES).is_empty());
    assert_eq!((editor.line(), editor.cursor()), ("echo ", 5));
}

#[test]
fn several_matches_complete_what_they_share() {
    let mut editor = typed("u");
    assert_eq!(editor.complete(&NAMES), ["uname", "uptime"]);
    assert_eq!(editor.line(), "u");
    let mut editor = typed("h");
    assert_eq!(editor.complete(&NAMES), ["help", "history"]);
    assert_eq!(editor.line(), "h");
    let mut editor = typed("hi");
    assert!(editor.complete(&NAMES).is_empty());
    assert_eq!(editor.line(), "history ");
}

#[test]
fn completion_leaves_arguments_and_misses_alone() {
    let mut editor = typed("echo he");
    assert!(editor.complete(&NAMES).is_empty());
    assert_eq!(editor.line(), "echo he");
    let mut editor = typed("zz");
    assert!(editor.complete(&NAMES).is_empty());
    assert_eq!(editor.line(), "zz");
}

#[test]
fn completing_mid_line_does_not_double_the_space() {
    let mut editor = typed("ech hi");
    for _ in 0..3 {
        press(&mut editor, Key::LEFT);
    }
    editor.complete(&NAMES);
    assert_eq!(editor.line(), "echo hi");
}
