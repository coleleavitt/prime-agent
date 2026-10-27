//! `prime-agent prune <session> [verb]`: offline, reversible prune of a saved
//! session's oversized entries (pasted images, giant text), so a bloated or
//! wedged session (one too big for any model to compact) opens again.
//!
//! Verbs: none / `list` shows the biggest entries; a list of indices prunes
//! those; `biggest N` prunes the N largest; `restore [id...]` puts pruned
//! entries back. The file is backed up before every change and originals are
//! kept in a sidecar for restore. Intended for a session that is not currently
//! open in the daemon.

use std::path::{Path, PathBuf};

use pa_core::prune;

use crate::config::get_agent_dir;

const USAGE: &str = "Usage:\n  \
    prime-agent prune <session> [list]            show the biggest entries\n  \
    prime-agent prune <session> <index> [index…]  prune those entries\n  \
    prime-agent prune <session> biggest <N>       prune the N largest entries\n  \
    prime-agent prune <session> restore [id…]     restore pruned entries (all if none given)\n\n\
<session> is a session id (or prefix) under the agent's sessions dir, or a path to a .jsonl file.\n\
Prune a session that is NOT currently open. Every change is backed up first and is reversible with restore.";

/// Run the `prune` command; returns the process exit code.
#[must_use]
pub fn run(args: &[String]) -> i32 {
    if matches!(
        args.first().map(String::as_str),
        None | Some("-h" | "--help" | "help")
    ) {
        println!("{USAGE}");
        return i32::from(args.is_empty());
    }
    let agent_dir = get_agent_dir();
    let selector = &args[0];
    let path = match resolve_session(selector, &agent_dir) {
        Ok(path) => path,
        Err(error) => {
            eprintln!("Error: {error}");
            return 1;
        }
    };
    let rest = &args[1..];
    match rest.first().map(String::as_str) {
        None | Some("list") => run_list(&path),
        Some("restore") => run_restore(&path, &rest[1..]),
        Some("biggest") => run_biggest(&path, rest.get(1).map(String::as_str)),
        _ => run_prune_indices(&path, rest),
    }
}

fn run_list(path: &Path) -> i32 {
    match prune::list(path) {
        Ok(infos) if infos.is_empty() => {
            println!("No message entries found.");
            0
        }
        Ok(infos) => {
            println!("{:>5}  {:>9}  {:<10}  content", "index", "size", "role");
            for info in &infos {
                let mark = if info.pruned { "  [pruned]" } else { "" };
                println!(
                    "{:>5}  {:>8.2}M  {:<10}  {}{mark}",
                    info.index,
                    info.bytes as f64 / 1e6,
                    info.role,
                    info.descriptor,
                );
            }
            println!(
                "\nPrune with: prime-agent prune {} <index>…  (or `biggest N`)",
                path.display()
            );
            0
        }
        Err(error) => {
            eprintln!("Error: {error}");
            1
        }
    }
}

fn run_prune_indices(path: &Path, verbs: &[String]) -> i32 {
    let mut indices = Vec::with_capacity(verbs.len());
    for verb in verbs {
        let Ok(index) = verb.parse::<usize>() else {
            eprintln!("Error: unknown argument {verb:?}\n\n{USAGE}");
            return 1;
        };
        indices.push(index);
    }
    report_prune(prune::prune(path, &indices))
}

fn run_biggest(path: &Path, count: Option<&str>) -> i32 {
    let Some(count) = count.and_then(|count| count.parse::<usize>().ok()) else {
        eprintln!("Error: `biggest` needs a count, e.g. `biggest 2`");
        return 1;
    };
    report_prune(prune::prune_biggest(path, count))
}

fn report_prune(result: Result<prune::PruneReport, String>) -> i32 {
    match result {
        Ok(report) => {
            println!(
                "Pruned {} entr{} ({} image(s), ~{:.1} MB removed): {:.2} MB -> {:.2} MB",
                report.pruned_indices.len(),
                if report.pruned_indices.len() == 1 {
                    "y"
                } else {
                    "ies"
                },
                report.removed_images,
                report.removed_bytes as f64 / 1e6,
                report.before_bytes as f64 / 1e6,
                report.after_bytes as f64 / 1e6,
            );
            println!("Backup: {}", report.backup.display());
            println!("Restore with: prime-agent prune <session> restore");
            0
        }
        Err(error) => {
            eprintln!("Error: {error}");
            1
        }
    }
}

fn run_restore(path: &Path, ids: &[String]) -> i32 {
    let filter = if ids.is_empty() { None } else { Some(ids) };
    match prune::restore(path, filter) {
        Ok(report) => {
            println!(
                "Restored {} entr{}.",
                report.restored_ids.len(),
                if report.restored_ids.len() == 1 {
                    "y"
                } else {
                    "ies"
                }
            );
            println!("Backup: {}", report.backup.display());
            0
        }
        Err(error) => {
            eprintln!("Error: {error}");
            1
        }
    }
}

/// Resolve a session selector to a `.jsonl` path: an existing file, else
/// `<agent dir>/sessions/<id>.jsonl`, else a unique id-prefix match there.
fn resolve_session(selector: &str, agent_dir: &Path) -> Result<PathBuf, String> {
    let direct = Path::new(selector);
    if direct.is_file() {
        return Ok(direct.to_path_buf());
    }
    let sessions = agent_dir.join("sessions");
    let by_id = sessions.join(format!("{selector}.jsonl"));
    if by_id.is_file() {
        return Ok(by_id);
    }
    let mut matches: Vec<PathBuf> = Vec::new();
    if let Ok(dir) = std::fs::read_dir(&sessions) {
        for entry in dir.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(selector) && name.ends_with(".jsonl") {
                matches.push(entry.path());
            }
        }
    }
    match matches.len() {
        0 => Err(format!(
            "no session matching {selector:?} (looked in {})",
            sessions.display()
        )),
        1 => Ok(matches.remove(0)),
        n => Err(format!(
            "{n} sessions match {selector:?}; use a longer id or a full path"
        )),
    }
}
