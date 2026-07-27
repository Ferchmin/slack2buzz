//! `slack2buzz` — import Slack workspace history into a Buzz community.
//!
//! Exit codes follow Buzz's own CLI discipline so this composes in scripts:
//! `0` ok, `1` bad input, `2` network/relay, `3` auth, `4` other, `5` write
//! conflict. `probe` and `parse` touch no network, so they only ever return
//! `0`, `1` or `4`.

use std::io::{IsTerminal, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use slack2buzz::export::Export;
use slack2buzz::ir::ChannelKind;
use slack2buzz::{fmt, parse, probe, selection};

/// Exit codes, mirroring Buzz's CLI.
mod exit {
    pub const OK: i32 = 0;
    pub const INPUT: i32 = 1;
    pub const OTHER: i32 = 4;
}

#[derive(Parser)]
#[command(
    name = "slack2buzz",
    version,
    about = "Import Slack history into a Buzz community as a signed archive",
    long_about = "Imports Slack history into Buzz as an ARCHIVE, not an identity \
migration. Messages are published under per-user archive keys that are not the \
original people and are not meant to be adopted. See the README before using."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Report what a Slack export actually contains. Reads only; sends nothing.
    Probe {
        /// Path to the export `.zip` or an unzipped export directory.
        export: PathBuf,
        /// Count join/leave messages as importable.
        #[arg(long)]
        keep_joins: bool,
    },

    /// Convert a Slack export into `import.jsonl`. No network access.
    Parse {
        /// Path to the export `.zip` or an unzipped export directory.
        export: PathBuf,

        /// Where to write the IR. `-` writes to stdout.
        #[arg(short, long, default_value = "import.jsonl")]
        out: PathBuf,

        /// Every conversation in the export, including private channels and DMs.
        #[arg(long)]
        all: bool,
        /// All public channels.
        #[arg(long)]
        all_public: bool,
        /// All private channels.
        #[arg(long)]
        all_private: bool,
        /// All DMs and group DMs.
        #[arg(long)]
        all_dms: bool,

        /// Comma-separated channel names or Slack ids. Repeatable.
        #[arg(long, value_name = "NAMES")]
        channels: Vec<String>,
        /// File with one channel name or id per line; `#` comments allowed.
        #[arg(long, value_name = "PATH")]
        channels_file: Option<PathBuf>,
        /// Comma-separated channels to drop from whatever was selected.
        #[arg(long, value_name = "NAMES")]
        exclude: Vec<String>,

        /// Include channels Slack has archived.
        #[arg(long)]
        include_archived: bool,
        /// Include conversations that have no messages.
        #[arg(long)]
        include_empty: bool,
        /// Keep join/leave messages instead of dropping them.
        #[arg(long)]
        keep_joins: bool,

        /// Never prompt. Requires an explicit selection flag.
        #[arg(long)]
        no_input: bool,
    },
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .without_time()
        .init();

    let cli = Cli::parse();
    let code = match run(cli) {
        Ok(()) => exit::OK,
        Err(e) => {
            eprintln!("error: {e:#}");
            classify(&e)
        }
    };
    std::process::exit(code);
}

/// Map an error to an exit code. Anything about the export or the operator's
/// flags is input; everything else is `OTHER` until there is a network stage to
/// distinguish.
fn classify(error: &anyhow::Error) -> i32 {
    let text = error.to_string();
    let input_shaped = [
        "no channels selected",
        "no channel in this export matches",
        "matched no conversations",
        "nothing selected",
        "contains no conversations",
    ];
    if input_shaped.iter().any(|m| text.contains(m))
        || error.downcast_ref::<std::io::Error>().is_some()
    {
        exit::INPUT
    } else {
        exit::OTHER
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Probe { export, keep_joins } => {
            let mut export = Export::open(&export)?;
            let inventory = probe::probe(&mut export, keep_joins)?;
            print_inventory(&inventory);
            Ok(())
        }

        Command::Parse {
            export: export_path,
            out,
            all,
            all_public,
            all_private,
            all_dms,
            channels,
            channels_file,
            exclude,
            include_archived,
            include_empty,
            keep_joins,
            no_input,
        } => {
            let mut export = Export::open(&export_path)?;
            let inventory = probe::probe(&mut export, keep_joins)?;

            let mut include: Vec<String> = channels
                .iter()
                .flat_map(|c| selection::parse_list(c))
                .collect();
            if let Some(path) = &channels_file {
                include.extend(
                    selection::read_list_file(path)
                        .with_context(|| format!("reading {}", path.display()))?,
                );
            }

            let mut filter = selection::Filter {
                all,
                all_public,
                all_private,
                all_dms,
                include,
                exclude: exclude
                    .iter()
                    .flat_map(|e| selection::parse_list(e))
                    .collect(),
                include_archived,
                include_empty,
            };

            // Nothing asked for: prompt when we can, refuse when we cannot.
            // Never fall back to "everything" — see the selection module docs.
            if filter.is_empty() {
                if no_input || !std::io::stdin().is_terminal() {
                    anyhow::bail!(
                        "no channels selected: pass --all, --all-public or --channels <names>"
                    );
                }
                print_inventory(&inventory);
                filter = selection::prompt(&inventory)?;
            }

            let resolved = selection::resolve(&inventory, &filter)?;
            report_selection(&inventory, &resolved);

            let options = parse::Options { keep_joins };
            let counts = if out.as_os_str() == "-" {
                let stdout = std::io::stdout();
                let mut w = std::io::BufWriter::new(stdout.lock());
                parse::parse(
                    &mut export,
                    &inventory,
                    &resolved.selected,
                    &options,
                    &mut w,
                )?
            } else {
                let file = std::fs::File::create(&out)
                    .with_context(|| format!("creating {}", out.display()))?;
                let mut w = std::io::BufWriter::new(file);
                parse::parse(
                    &mut export,
                    &inventory,
                    &resolved.selected,
                    &options,
                    &mut w,
                )?
            };

            print_counts(&counts, &out);
            Ok(())
        }
    }
}

fn print_inventory(inventory: &probe::Inventory) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "Export contains: {}", inventory.scope.describe());
    let _ = writeln!(
        out,
        "{}, {}, {}, {} custom emoji\n",
        fmt::plural(inventory.users.len(), "user"),
        fmt::plural(inventory.conversations.len(), "conversation"),
        fmt::plural(inventory.total_messages(), "message"),
        inventory.emoji_count,
    );

    for kind in [
        ChannelKind::Public,
        ChannelKind::Private,
        ChannelKind::GroupDm,
        ChannelKind::Dm,
    ] {
        let group: Vec<_> = inventory.of_kind(kind).collect();
        if group.is_empty() {
            continue;
        }
        let _ = writeln!(out, "{} ({}):", kind.as_str(), group.len());
        for c in group {
            let range = match (c.first_ts, c.last_ts) {
                (Some(f), Some(l)) => format!("{} → {}", fmt::date(f), fmt::date(l)),
                _ => "empty".to_string(),
            };
            let _ = writeln!(
                out,
                "  {:<26} {:>7} msgs {:>5} thr {:>5} rxn {:>4} files  {}{}",
                c.label(),
                c.message_count,
                c.thread_reply_count,
                c.reaction_count,
                c.file_count,
                range,
                if c.is_archived { "  [archived]" } else { "" },
            );
        }
        let _ = writeln!(out);
    }

    if !inventory.warnings.is_empty() {
        let _ = writeln!(out, "Notes:");
        for warning in &inventory.warnings {
            let _ = writeln!(out, "  - {warning}");
        }
        let _ = writeln!(out);
    }

    if inventory.scope != probe::ExportScope::PublicOnly {
        let _ = writeln!(
            out,
            "This export includes conversations people had a reasonable \
             expectation of privacy about.\nNothing beyond public channels is \
             selected by default."
        );
        let _ = writeln!(out);
    }
}

fn report_selection(inventory: &probe::Inventory, resolved: &selection::Resolved) {
    let selected: Vec<_> = inventory
        .conversations
        .iter()
        .filter(|c| resolved.selected.contains(&c.slack_id))
        .collect();
    let private = selected.iter().filter(|c| c.kind.is_private()).count();

    eprintln!(
        "Selected {} ({} private), skipping {}.",
        fmt::plural(selected.len(), "conversation"),
        private,
        fmt::plural(resolved.skipped.len(), "conversation"),
    );
}

fn print_counts(counts: &slack2buzz::ir::Counts, out: &std::path::Path) {
    let target = if out.as_os_str() == "-" {
        "stdout".to_string()
    } else {
        out.display().to_string()
    };
    eprintln!(
        "Wrote {target}: {} messages ({} thread replies), {} reactions, \
         {} file references, {} users, {} custom emoji.",
        counts.messages,
        counts.thread_replies,
        counts.reactions,
        counts.files,
        counts.users,
        counts.emoji,
    );
    if counts.dropped_joins > 0 {
        eprintln!(
            "Dropped {} join/leave messages (--keep-joins to keep them).",
            counts.dropped_joins
        );
    }
    if counts.skipped_unparseable > 0 {
        eprintln!(
            "WARNING: skipped {} unparseable records — this is fidelity loss. \
             Re-run with RUST_LOG=warn for detail.",
            counts.skipped_unparseable
        );
    }
}
