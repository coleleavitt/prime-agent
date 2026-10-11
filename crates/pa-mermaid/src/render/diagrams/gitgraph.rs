//! `gitGraph`: commit lanes drawn the way `git log --graph` draws them — newest commit on
//! top, one column per branch, connector rows where history splits. Ported from
//! lovely-mermaid 0.3.3 `diagrams/gitgraph.ts` (Apache-2.0; see `LICENSE-lovely-mermaid`).
//!
//! Connector rows go through the canvas direction bits, so a merge reaching across an
//! active lane crosses it with a `┼` instead of erasing it.

use std::collections::{BTreeSet, HashMap};

use super::super::canvas::{Canvas, D, U, draw_text};
use super::super::graph::MAX_EDGES;
use super::super::labels::{MAX_LABEL, clean_label, fit_label};
use super::super::layout::width_of;
use super::super::statements::{header_kind, statements_of};
use super::super::{Role, js_text};
use super::Drawn;

pub(in crate::render) const HEADERS: &[&str] = &["gitgraph", "gitgraph:"];

struct Commit {
    lane: usize,
    id: String,
    tag: Option<String>,
    /// The lane this commit merged in, if it is a merge commit.
    merge_from: Option<usize>,
}

/// Rows, top-down: commit rows interleaved with connector rows. `Open` hangs a merged
/// lane off its merge commit; `Close` returns a forked lane to its parent at its fork.
enum GitRow {
    Commit(usize),
    Open { parent: usize, lane: usize },
    Close { parent: usize, lane: usize },
}

/// Lanes currently drawn as columns, in the order they joined (JS `Set` iteration).
#[derive(Default)]
struct Live {
    order: Vec<usize>,
    members: BTreeSet<usize>,
}

impl Live {
    fn add(&mut self, lane: usize) {
        if self.members.insert(lane) {
            self.order.push(lane);
        }
    }

    fn has(&self, lane: usize) -> bool {
        self.members.contains(&lane)
    }

    fn delete(&mut self, lane: usize) {
        if self.members.remove(&lane) {
            self.order.retain(|&l| l != lane);
        }
    }
}

pub(in crate::render) fn render(src: &str) -> Option<Drawn> {
    let model = parse_git_graph(src)?;
    let lane_count = model.branches.len();
    let commits = &model.commits;

    // The newest commit of each lane wears the branch name.
    let mut head_of: HashMap<usize, usize> = HashMap::new();
    for (i, c) in commits.iter().enumerate() {
        head_of.insert(c.lane, i);
    }

    let used: Vec<bool> = (0..lane_count)
        .map(|lane| {
            lane == 0
                || commits
                    .iter()
                    .any(|c| c.lane == lane || c.merge_from == Some(lane))
        })
        .collect();
    let mut rows: Vec<GitRow> = Vec::new();
    for i in (0..commits.len()).rev() {
        let c = &commits[i];
        rows.push(GitRow::Commit(i));
        if let Some(from) = c.merge_from {
            rows.push(GitRow::Open {
                parent: c.lane,
                lane: from,
            });
        }
        // Close outer lanes first so an inner close still sees them as columns.
        for lane in (1..lane_count).rev() {
            if i > 0 && model.fork_at[lane] == Some(i - 1) && used[lane] {
                rows.push(GitRow::Close {
                    parent: commits[i - 1].lane,
                    lane,
                });
            }
        }
    }

    let lane_x = |lane: usize| lane as i64 * 2;
    let graph_w = lane_count as i64 * 2;
    let labels: Vec<Option<Vec<(String, Role)>>> = rows
        .iter()
        .map(|row| {
            let GitRow::Commit(at) = *row else {
                return None;
            };
            let c = &commits[at];
            let mut parts: Vec<(String, Role)> = Vec::new();
            if !c.id.is_empty() {
                parts.push((c.id.clone(), Role::Text));
            }
            if head_of.get(&c.lane) == Some(&at) {
                parts.push((format!("({})", model.branches[c.lane]), Role::EdgeLabel));
            }
            if let Some(tag) = &c.tag {
                parts.push((format!("[{tag}]"), Role::EdgeLabel));
            }
            if let Some(from) = c.merge_from {
                parts.push((format!("⇐ {}", model.branches[from]), Role::EdgeLabel));
            }
            Some(parts)
        })
        .collect();
    let label_w = labels
        .iter()
        .map(|parts| {
            parts.as_ref().map_or(0, |parts| {
                parts.iter().map(|(t, _)| width_of(t) + 1).sum::<i64>() - 1
            })
        })
        .fold(1, i64::max);
    let width = graph_w + label_w;
    let mut canvas = Canvas::new(width, rows.len() as i64);

    // Walk top-down with the lanes currently drawn as columns: a lane joins at its newest
    // own commit or its `Open` connector, and leaves at its `Close`.
    let mut live = Live::default();
    for (y, row) in rows.iter().enumerate() {
        let y = y as i64;
        let (parent, lane, open) = match *row {
            GitRow::Commit(at) => {
                let c = &commits[at];
                live.add(c.lane);
                for &l in &live.order {
                    if l != c.lane {
                        canvas.add_bits(lane_x(l), y, U | D, Role::Edge);
                    }
                }
                canvas.set_char(lane_x(c.lane), y, '●', Role::Edge);
                let mut x = graph_w;
                for (text, role) in labels[y as usize].iter().flatten() {
                    draw_text(&mut canvas, text, x, y, *role);
                    x += width_of(text) + 1;
                }
                continue;
            }
            GitRow::Open { parent, lane } => (parent, lane, true),
            GitRow::Close { parent, lane } => (parent, lane, false),
        };
        // The parent keeps its column, the child lane hooks on toward it, and unrelated
        // live lanes cross the horizontal run as `┼` via the bit merge.
        let rejoins = open && live.has(lane);
        if open {
            live.add(lane);
        }
        for &l in &live.order {
            if l != parent && l != lane {
                canvas.add_bits(lane_x(l), y, U | D, Role::Edge);
            }
        }
        // An open hangs off the merge commit directly above; a close bends down into the
        // fork commit below, continuing up only if the parent already had a column here.
        if open {
            canvas.add_bits(lane_x(parent), y, U | D, Role::Edge);
        } else {
            let up = if live.has(parent) { U } else { 0 };
            canvas.add_bits(lane_x(parent), y, D | up, Role::Edge);
        }
        live.add(parent);
        canvas.seg_h(y, lane_x(parent), lane_x(lane));
        let bits = if open { D } else { U } | if rejoins { U } else { 0 };
        canvas.add_bits(lane_x(lane), y, bits, Role::Edge);
        if !open {
            live.delete(lane);
        }
    }

    canvas.finalize_mask();
    Some(Drawn {
        canvas,
        warnings: model.warnings,
    })
}

struct GitModel {
    branches: Vec<String>,
    commits: Vec<Commit>,
    fork_at: Vec<Option<usize>>,
    warnings: Vec<String>,
}

fn parse_git_graph(src: &str) -> Option<GitModel> {
    let statements = statements_of(src);
    let kind = header_kind(&statements)?;
    if !HEADERS.contains(&kind.as_str()) {
        return None;
    }

    let mut branches = vec!["main".to_owned()];
    let mut fork_at: Vec<Option<usize>> = vec![None];
    let mut commits: Vec<Commit> = Vec::new();
    let mut warnings = Vec::new();
    // Newest commit index per lane — the fork point for branches cut from it.
    let mut heads: Vec<Option<usize>> = vec![None];
    let mut cur = 0usize;
    let mut auto = 0usize;
    let mut truncated = false;

    for st in &statements[1..] {
        if commits.len() >= MAX_EDGES {
            truncated = true;
            break;
        }
        let first_raw = js_text::words(st).first().copied().unwrap_or("");
        let first = first_raw.to_lowercase();
        let rest = js_text::trim(&st[first_raw.len()..]);
        let mut unreadable = false;
        match first.as_str() {
            "commit" => {
                let (id, tag) = commit_attrs(rest);
                heads[cur] = Some(commits.len());
                let id = id.unwrap_or_else(|| next_auto(&mut auto));
                commits.push(Commit {
                    lane: cur,
                    id,
                    tag,
                    merge_from: None,
                });
            }
            "branch" => {
                let (name, _) = name_token(rest);
                // The fork point is the current branch's head, not the newest commit.
                let fork = heads[cur].or(fork_at[cur]);
                match (name, fork) {
                    (Some(name), Some(fork)) if !branches.iter().any(|b| b == name) => {
                        branches.push(name.to_owned());
                        fork_at.push(Some(fork));
                        heads.push(None);
                        cur = branches.len() - 1;
                    }
                    _ => unreadable = true,
                }
            }
            "checkout" | "switch" => {
                let name = name_token(rest).0.unwrap_or("");
                match branches.iter().position(|b| b == name) {
                    Some(lane) => cur = lane,
                    None => unreadable = true,
                }
            }
            "merge" => {
                let (name, after) = name_token(rest);
                let name = name.unwrap_or("");
                match branches.iter().position(|b| b == name) {
                    Some(lane) if lane != cur => {
                        // An unnamed merge shows no id — the `⇐ branch` marker says what it is.
                        let (id, tag) = commit_attrs(after);
                        heads[cur] = Some(commits.len());
                        commits.push(Commit {
                            lane: cur,
                            id: id.unwrap_or_default(),
                            tag,
                            merge_from: Some(lane),
                        });
                    }
                    _ => unreadable = true,
                }
            }
            "cherry-pick" => {
                let (id, tag) = commit_attrs(rest);
                heads[cur] = Some(commits.len());
                let id = match id {
                    None => next_auto(&mut auto),
                    Some(id) => format!("⟲ {id}"),
                };
                commits.push(Commit {
                    lane: cur,
                    id,
                    tag,
                    merge_from: None,
                });
            }
            _ => unreadable = true,
        }
        if unreadable {
            warnings.push(format!("dropped, unreadable statement: \"{st}\""));
        }
    }
    if truncated {
        warnings.push(format!(
            "diagram truncated: commit cap ({MAX_EDGES}) reached"
        ));
    }

    (!commits.is_empty()).then_some(GitModel {
        branches,
        commits,
        fork_at,
        warnings,
    })
}

/// The next invented commit id (`c0`, `c1`, …).
fn next_auto(auto: &mut usize) -> String {
    let id = format!("c{auto}");
    *auto += 1;
    id
}

/// The first branch-name token and the text after it; quotes let a name carry spaces.
fn name_token(rest: &str) -> (Option<&str>, &str) {
    if let Some(quoted) = rest.strip_prefix('"') {
        if let Some(close) = quoted.find('"') {
            return (Some(&quoted[..close]), &quoted[close + 1..]);
        }
    }
    match js_text::words(rest).first() {
        Some(&w) => (Some(w), &rest[w.len()..]),
        None => (None, rest),
    }
}

/// JS `/(id|tag)\s*:\s*"([^"]*)"/gi` over the attributes trailing a commit or merge: the
/// last `id` and `tag` values, cleaned and fitted.
fn commit_attrs(rest: &str) -> (Option<String>, Option<String>) {
    let mut id = None;
    let mut tag = None;
    let lower = rest.to_ascii_lowercase();
    let mut from = 0;
    while from < rest.len() {
        let next = ["id", "tag"]
            .into_iter()
            .filter_map(|key| lower[from..].find(key).map(|at| (from + at, key)))
            .min_by_key(|&(at, key)| (at, key.len()));
        let Some((at, key)) = next else {
            break;
        };
        let after_key = at + key.len();
        let value = attr_value(&rest[after_key..]);
        match value {
            Some((value, consumed)) => {
                let cleaned = fit_label(&clean_label(value), MAX_LABEL);
                if key == "id" {
                    id = Some(cleaned);
                } else {
                    tag = Some(cleaned);
                }
                from = after_key + consumed;
            }
            None => from = at + 1,
        }
    }
    (id, tag)
}

/// `\s*:\s*"([^"]*)"` at the start of `s`: the quoted value and the bytes matched.
fn attr_value(s: &str) -> Option<(&str, usize)> {
    let after_space = js_text::trim_start(s);
    let after_colon = js_text::trim_start(after_space.strip_prefix(':')?);
    let quoted = after_colon.strip_prefix('"')?;
    let close = quoted.find('"')?;
    let consumed = s.len() - quoted.len() + close + 1;
    Some((&quoted[..close], consumed))
}
