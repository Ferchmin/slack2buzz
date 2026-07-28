//! The `.zip` reader must agree with the directory reader, exactly.
//!
//! Every other test in this repo reads `fixtures/basic-export/` as a directory,
//! because a directory stays reviewable in a diff. But the documented first
//! command is `slack2buzz probe export.zip`, so the zip path is what most people
//! actually exercise — and it is a genuinely different code path: different
//! entry enumeration, a wrapper-prefix to strip, and no filesystem to lean on.
//!
//! Rather than assert zip behaviour independently (which would just re-encode
//! whatever the implementation happens to do), these tests zip the fixture and
//! require the result to be **byte-identical** to the directory read. The
//! directory path is already pinned by the golden files, so agreement with it
//! transitively pins the zip path to the same spec.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write;
use std::path::{Path, PathBuf};

use slack2buzz::export::Export;
use slack2buzz::parse::{self, Options};
use slack2buzz::probe;
use slack2buzz::selection::{self, Filter};

const FIXTURE: &str = "fixtures/basic-export";

/// Zip the fixture into a temp file. `prefix` emulates Slack's habit of
/// sometimes wrapping everything in one top-level directory and sometimes not.
fn zip_fixture(name: &str, prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("s2b-zip-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("export.zip");

    let file = std::fs::File::create(&path).unwrap();
    let mut writer = zip::ZipWriter::new(file);
    let options: zip::write::FileOptions<'_, ()> =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    for entry in walk(Path::new(FIXTURE)) {
        let relative = entry
            .strip_prefix(FIXTURE)
            .unwrap()
            .to_string_lossy()
            .to_string();
        writer
            .start_file(format!("{prefix}{relative}"), options)
            .unwrap();
        writer.write_all(&std::fs::read(&entry).unwrap()).unwrap();
    }
    writer.finish().unwrap();
    path
}

fn walk(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

/// Run probe+parse over an already-opened export and return the IR.
fn ir_from(export: &mut Export, filter: &Filter) -> String {
    let inventory = probe::probe(export, false).unwrap();
    let resolved = selection::resolve(&inventory, filter).unwrap();
    let mut out = Vec::new();
    parse::parse(
        export,
        &inventory,
        &resolved.selected,
        &Options::default(),
        &mut out,
    )
    .unwrap();
    String::from_utf8(out).unwrap()
}

fn ir_from_dir(filter: &Filter) -> String {
    let mut export = Export::open(Path::new(FIXTURE)).unwrap();
    ir_from(&mut export, filter)
}

fn ir_from_zip(path: &Path, filter: &Filter) -> String {
    let mut export = Export::open(path).unwrap();
    ir_from(&mut export, filter)
}

#[test]
fn a_zip_with_no_wrapper_directory_parses_identically_to_the_directory() {
    let zip = zip_fixture("flat", "");
    assert_eq!(
        ir_from_zip(&zip, &Filter::everything()),
        ir_from_dir(&Filter::everything())
    );
    let _ = std::fs::remove_dir_all(zip.parent().unwrap());
}

/// Slack often wraps the whole export in one directory. Stripping it must be
/// invisible to everything downstream.
#[test]
fn a_zip_with_a_wrapper_directory_parses_identically_too() {
    let zip = zip_fixture("wrapped", "acme-slack-export-2024/");
    assert_eq!(
        ir_from_zip(&zip, &Filter::everything()),
        ir_from_dir(&Filter::everything())
    );
    let _ = std::fs::remove_dir_all(zip.parent().unwrap());
}

#[test]
fn probe_reports_the_same_inventory_from_a_zip() {
    let zip = zip_fixture("probe", "");
    let mut from_zip = Export::open(&zip).unwrap();
    let zipped = probe::probe(&mut from_zip, false).unwrap();

    let mut from_dir = Export::open(Path::new(FIXTURE)).unwrap();
    let dir = probe::probe(&mut from_dir, false).unwrap();

    assert_eq!(zipped.scope, dir.scope);
    assert_eq!(zipped.total_messages(), dir.total_messages());
    assert_eq!(zipped.total_reactions(), dir.total_reactions());
    assert_eq!(zipped.total_files(), dir.total_files());
    assert_eq!(zipped.total_thread_replies(), dir.total_thread_replies());
    assert_eq!(zipped.emoji_count, dir.emoji_count);
    assert_eq!(zipped.conversations.len(), dir.conversations.len());
    assert_eq!(zipped.users.len(), dir.users.len());

    // Including the derived per-conversation detail, not just the totals.
    for (z, d) in zipped.conversations.iter().zip(dir.conversations.iter()) {
        assert_eq!(z.slack_id, d.slack_id);
        assert_eq!(z.dir, d.dir, "directory resolution must match");
        assert_eq!(z.message_count, d.message_count);
        assert_eq!(z.first_ts, d.first_ts);
        assert_eq!(z.last_ts, d.last_ts);
    }

    let _ = std::fs::remove_dir_all(zip.parent().unwrap());
}

/// Channel selection resolves against names and ids that came out of the zip's
/// manifests, so it must work the same way there.
#[test]
fn channel_selection_works_against_a_zip() {
    let zip = zip_fixture("select", "");
    let by_name = ir_from_zip(
        &zip,
        &Filter {
            include: vec!["general".to_string()],
            ..Filter::default()
        },
    );
    assert_eq!(by_name, ir_from_dir(&Filter::public_only()));
    let _ = std::fs::remove_dir_all(zip.parent().unwrap());
}

#[test]
fn a_file_that_is_not_a_zip_fails_with_a_clear_error() {
    let dir = std::env::temp_dir().join(format!("s2b-notzip-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("not-an-export.zip");
    std::fs::write(&path, b"this is not a zip archive").unwrap();

    let err = Export::open(&path).unwrap_err().to_string();
    assert!(
        err.contains("as a zip archive"),
        "error should name the problem: {err}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_missing_path_fails_with_a_clear_error() {
    let err = Export::open(Path::new("fixtures/does-not-exist.zip"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("opening export"), "{err}");
}
