//! Golden-file tests for `parse`.
//!
//! The unit tests in `src/parse` assert specific properties; these assert the
//! *whole* output, byte for byte. That is what catches the changes nobody
//! thought to write an assertion for — a field quietly renamed, an ordering
//! that drifted, a normalisation that started escaping something new.
//!
//! To review an intentional change, regenerate and read the diff:
//!
//! ```sh
//! UPDATE_GOLDEN=1 cargo test --test golden
//! git diff tests/golden
//! ```
//!
//! Never regenerate without reading the diff. The whole value of these files
//! is that a human looked at what changed.

use std::path::Path;

use slack2buzz::export::Export;
use slack2buzz::parse::{self, Options};
use slack2buzz::probe;
use slack2buzz::selection::{self, Filter};

const FIXTURE: &str = "fixtures/basic-export";

fn parse_to_string(filter: &Filter, options: &Options) -> String {
    let mut export = Export::open(Path::new(FIXTURE)).expect("fixture export opens");
    let inventory = probe::probe(&mut export, options.keep_joins).expect("fixture probes");
    let resolved = selection::resolve(&inventory, filter).expect("selection resolves");
    let mut out = Vec::new();
    parse::parse(
        &mut export,
        &inventory,
        &resolved.selected,
        options,
        &mut out,
    )
    .expect("parse succeeds");
    String::from_utf8(out).expect("IR is valid UTF-8")
}

/// Compare against `tests/golden/<name>.jsonl`, or rewrite it when
/// `UPDATE_GOLDEN=1`.
fn assert_golden(name: &str, actual: &str) {
    let path = Path::new("tests/golden").join(format!("{name}.jsonl"));

    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, actual).expect("writing golden file");
        return;
    }

    let expected = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "cannot read {}: {e}\nrun `UPDATE_GOLDEN=1 cargo test --test golden` to create it",
            path.display()
        )
    });

    if expected != actual {
        // Report the first differing line rather than dumping both files;
        // these are long and the first divergence is almost always the cause.
        let diff = expected
            .lines()
            .zip(actual.lines())
            .enumerate()
            .find(|(_, (e, a))| e != a);
        match diff {
            Some((i, (e, a))) => panic!(
                "{} differs at line {}\n  expected: {e}\n    actual: {a}\n\n\
                 run `UPDATE_GOLDEN=1 cargo test --test golden` then review `git diff`",
                path.display(),
                i + 1
            ),
            None => panic!(
                "{} differs in length: {} expected lines, {} actual",
                path.display(),
                expected.lines().count(),
                actual.lines().count()
            ),
        }
    }
}

#[test]
fn public_channels_only() {
    let actual = parse_to_string(&Filter::public_only(), &Options::default());
    assert_golden("all-public", &actual);
}

#[test]
fn everything_including_private_and_dms() {
    let actual = parse_to_string(&Filter::everything(), &Options::default());
    assert_golden("everything", &actual);
}

#[test]
fn public_channels_keeping_joins() {
    let actual = parse_to_string(&Filter::public_only(), &Options { keep_joins: true });
    assert_golden("all-public-keep-joins", &actual);
}

/// Selecting one channel by name must produce exactly the same records for it
/// as selecting all public channels does — the selection must not leak into how
/// a message is parsed.
#[test]
fn selection_does_not_change_how_messages_are_parsed() {
    let by_name = parse_to_string(
        &Filter {
            include: vec!["general".to_string()],
            ..Filter::default()
        },
        &Options::default(),
    );
    let by_kind = parse_to_string(&Filter::public_only(), &Options::default());

    let message_lines = |s: &str| -> Vec<String> {
        s.lines()
            .filter(|l| l.contains("\"type\":\"message\""))
            .map(str::to_string)
            .collect()
    };
    assert_eq!(message_lines(&by_name), message_lines(&by_kind));
}
