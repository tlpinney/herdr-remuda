//! GitHub as a channel: when CI fails on an agent's PR, or someone comments on
//! or reviews it, the agent gets a nudge to look, never the content itself.
//!
//! An agent's PR is the one it tagged its pane with (`remuda_pr`), or else the
//! open PR for the branch checked out in its working directory, as long as no
//! other agent works in the same worktree. The board's GitHub thread checks
//! each linked PR with `gh` on this machine about once a minute. The first
//! check of a PR only records what is there; later ones nudge the agent about
//! anything new, through `remuda send`'s delivery: typed once the agent is
//! idle, prefixed `[from github]`, and logged.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::herdr;
use crate::herds::Seen;
use crate::remote;
use crate::send;

/// How often linked PRs are checked.
const CHECK_EVERY: Duration = Duration::from_secs(60);
/// How long a PR found by branch is trusted before looking it up again.
const BRANCH_LOOKUP_FOR: Duration = Duration::from_secs(300);
/// How long a nudge waits for a busy agent.
const NUDGE_WAIT: Duration = Duration::from_secs(1800);
/// Check conclusions that count as a failure.
const FAILED: [&str; 4] = ["FAILURE", "TIMED_OUT", "ACTION_REQUIRED", "ERROR"];

/// An agent the board watches: its herd, machine, and what it last saw.
pub type Target = (String, String, Seen);

/// What the GitHub thread tells the board about an agent.
pub struct Status {
    pub machine: String,
    pub pane: String,
    /// e.g. `PR #12 · CI failing`; empty when the agent has no PR.
    pub text: String,
}

/// What GitHub said about a PR on one check.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PrState {
    pub number: u64,
    pub sha: String,
    /// Names of failed checks on `sha`.
    pub failed: BTreeSet<String>,
    pub pending: bool,
    pub comments: HashSet<String>,
    pub reviews: HashSet<String>,
    /// Authors of each comment and review, by id.
    pub authors: HashMap<String, String>,
}

impl PrState {
    pub fn from_json(pr: &Value) -> PrState {
        let mut state = PrState {
            number: pr["number"].as_u64().unwrap_or_default(),
            sha: pr["headRefOid"].as_str().unwrap_or_default().to_string(),
            ..PrState::default()
        };
        for check in pr["statusCheckRollup"].as_array().into_iter().flatten() {
            // Check runs have a name and conclusion, status contexts a
            // context and state.
            let name = check["name"]
                .as_str()
                .or_else(|| check["context"].as_str())
                .unwrap_or("check");
            let outcome = check["conclusion"]
                .as_str()
                .filter(|c| !c.is_empty())
                .or_else(|| check["state"].as_str())
                .unwrap_or_default();
            if FAILED.contains(&outcome) {
                state.failed.insert(name.to_string());
            } else if outcome.is_empty() || outcome == "PENDING" {
                state.pending = true;
            }
        }
        for (key, set) in [
            ("comments", &mut state.comments),
            ("reviews", &mut state.reviews),
        ] {
            for item in pr[key].as_array().into_iter().flatten() {
                let Some(id) = item["id"].as_str() else {
                    continue;
                };
                set.insert(id.to_string());
                let author = item["author"]["login"].as_str().unwrap_or("someone");
                state.authors.insert(id.to_string(), author.to_string());
            }
        }
        state
    }

    /// `PR #12 · CI failing`, for the board.
    fn summary(&self) -> String {
        let ci = if !self.failed.is_empty() {
            "CI failing"
        } else if self.pending {
            "CI running"
        } else {
            "CI passing"
        };
        format!("PR #{} · {ci}", self.number)
    }
}

/// The nudge for what changed between two checks of a PR, if anything did.
/// The first check (`old` is `None`) only records the PR.
pub fn nudge(old: Option<&PrState>, new: &PrState) -> Option<String> {
    let old = old?;
    let n = new.number;
    let mut parts = Vec::new();
    let new_failures: Vec<&str> = new
        .failed
        .iter()
        .filter(|name| new.sha != old.sha || !old.failed.contains(*name))
        .map(String::as_str)
        .collect();
    if !new_failures.is_empty() {
        parts.push(format!(
            "CI failed on PR #{n} ({}). Look at the failure with `gh pr checks {n}` and fix it.",
            new_failures.join(", ")
        ));
    }
    let fresh = |now: &HashSet<String>, before: &HashSet<String>| -> BTreeSet<String> {
        now.difference(before)
            .filter_map(|id| new.authors.get(id).map(|author| format!("@{author}")))
            .collect()
    };
    let commenters = fresh(&new.comments, &old.comments);
    let reviewers = fresh(&new.reviews, &old.reviews);
    if !commenters.is_empty() || !reviewers.is_empty() {
        let mut what = Vec::new();
        if !commenters.is_empty() {
            what.push("comments");
        }
        if !reviewers.is_empty() {
            what.push("reviews");
        }
        let who: BTreeSet<&String> = commenters.iter().chain(&reviewers).collect();
        parts.push(format!(
            "There are new {} on PR #{n} from {}. Read them with `gh pr view {n} --comments` and follow up.",
            what.join(" and "),
            who.into_iter().cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

/// `owner/repo` from a GitHub remote URL.
pub fn repo_of(remote: &str) -> Option<String> {
    let rest = remote
        .trim()
        .strip_prefix("git@github.com:")
        .or_else(|| remote.trim().strip_prefix("https://github.com/"))
        .or_else(|| remote.trim().strip_prefix("ssh://git@github.com/"))?;
    let rest = rest.trim_end_matches('/').trim_end_matches(".git");
    let (owner, repo) = rest.split_once('/')?;
    (!owner.is_empty() && !repo.is_empty() && !repo.contains('/'))
        .then(|| format!("{owner}/{repo}"))
}

/// Where an agent works: its worktree, branch and GitHub repo.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Checkout {
    toplevel: String,
    branch: String,
    repo: String,
}

/// Reads the checkout in `cwd` on `machine` with one ssh command.
fn checkout(machine: &str, cwd: &str) -> Option<Checkout> {
    let dir = remote::shell_quote(cwd);
    let script = format!(
        "git -C {dir} rev-parse --show-toplevel --abbrev-ref HEAD && git -C {dir} remote get-url origin"
    );
    let output = herdr::shell_on(machine, &script).ok()?;
    let mut lines = output.lines().map(str::trim);
    let toplevel = lines.next()?.to_string();
    let branch = lines.next()?.to_string();
    let repo = repo_of(lines.next()?)?;
    (branch != "HEAD").then_some(Checkout {
        toplevel,
        branch,
        repo,
    })
}

fn gh(args: &[&str]) -> Option<Value> {
    let output = Command::new("gh")
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| serde_json::from_slice(&output.stdout).ok())?
}

/// The open PR for `branch` in `repo`.
fn pr_for_branch(repo: &str, branch: &str) -> Option<String> {
    let found = gh(&[
        "pr", "list", "-R", repo, "--head", branch, "--state", "open", "--json", "url", "--limit",
        "1",
    ])?;
    found[0]["url"].as_str().map(str::to_string)
}

fn pr_state(url: &str) -> Option<PrState> {
    let pr = gh(&[
        "pr",
        "view",
        url,
        "--json",
        "number,headRefOid,statusCheckRollup,comments,reviews",
    ])?;
    Some(PrState::from_json(&pr))
}

/// Links PRs to agents: a tag wins; a branch counts only for an agent alone
/// in its worktree. Returns PR URLs by `(machine, pane)`.
fn links(
    targets: &[Target],
    checkouts: &HashMap<(String, String), (Instant, Option<Checkout>)>,
    branches: &HashMap<(String, String), Option<String>>,
) -> HashMap<(String, String), String> {
    let mut by_worktree: HashMap<(String, String), usize> = HashMap::new();
    for ((machine, _), (_, checkout)) in checkouts {
        if let Some(checkout) = checkout {
            *by_worktree
                .entry((machine.clone(), checkout.toplevel.clone()))
                .or_default() += 1;
        }
    }
    let mut links = HashMap::new();
    for (_, machine, seen) in targets {
        let key = (machine.clone(), seen.pane.clone());
        if let Some(pr) = &seen.pr {
            links.insert(key, pr.clone());
            continue;
        }
        let Some((_, Some(checkout))) = checkouts.get(&key) else {
            continue;
        };
        let alone = by_worktree
            .get(&(machine.clone(), checkout.toplevel.clone()))
            .is_some_and(|count| *count == 1);
        let pr = branches.get(&(checkout.repo.clone(), checkout.branch.clone()));
        if let (true, Some(Some(url))) = (alone, pr) {
            links.insert(key, url.clone());
        }
    }
    links
}

/// Runs the GitHub channel: takes the board's agents from `targets` and
/// reports each agent's PR status to `statuses`.
pub fn spawn(targets: Receiver<Vec<Target>>, statuses: Sender<Vec<Status>>) {
    thread::spawn(move || {
        let mut latest: Vec<Target> = Vec::new();
        let mut checkouts: HashMap<(String, String), (Instant, Option<Checkout>)> = HashMap::new();
        let mut states: HashMap<String, PrState> = HashMap::new();
        // PRs found by branch, by (repo, branch); `None` when there is none.
        let mut branches: HashMap<(String, String), Option<String>> = HashMap::new();
        let mut next = Instant::now();
        loop {
            match targets.recv_timeout(next.saturating_duration_since(Instant::now())) {
                Ok(update) => {
                    // Check as soon as the board's first agents arrive.
                    if latest.is_empty() && !update.is_empty() {
                        next = Instant::now();
                    }
                    latest = update;
                    continue;
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            next = Instant::now() + CHECK_EVERY;

            // Look up branches for untagged agents now and then, one ssh
            // command per working directory.
            let mut by_dir: HashMap<(String, String), Option<Checkout>> = HashMap::new();
            for (_, machine, seen) in &latest {
                if seen.pr.is_some() || seen.cwd.is_empty() {
                    continue;
                }
                let key = (machine.clone(), seen.pane.clone());
                let stale = checkouts
                    .get(&key)
                    .is_none_or(|(at, _)| at.elapsed() >= BRANCH_LOOKUP_FOR);
                if !stale {
                    continue;
                }
                let found = by_dir
                    .entry((machine.clone(), seen.cwd.clone()))
                    .or_insert_with(|| {
                        let found = checkout(machine, &seen.cwd);
                        if let Some(found) = &found {
                            let pr = pr_for_branch(&found.repo, &found.branch);
                            branches.insert((found.repo.clone(), found.branch.clone()), pr);
                        }
                        found
                    })
                    .clone();
                checkouts.insert(key, (Instant::now(), found));
            }
            let live: HashSet<(String, String)> = latest
                .iter()
                .map(|(_, machine, seen)| (machine.clone(), seen.pane.clone()))
                .collect();
            checkouts.retain(|key, _| live.contains(key));

            let linked = links(&latest, &checkouts, &branches);
            let mut report = Vec::new();
            for (_, machine, seen) in &latest {
                let key = (machine.clone(), seen.pane.clone());
                let Some(url) = linked.get(&key) else {
                    report.push(Status {
                        machine: machine.clone(),
                        pane: seen.pane.clone(),
                        text: String::new(),
                    });
                    continue;
                };
                let Some(state) = pr_state(url) else {
                    continue;
                };
                if let Some(text) = nudge(states.get(url), &state) {
                    let to = format!("{}@{machine}", seen.name);
                    let (machine, pane) = (machine.clone(), seen.pane.clone());
                    thread::spawn(move || {
                        let _ = send::post("github", &to, &pane, Some(&machine), NUDGE_WAIT, &text);
                    });
                }
                report.push(Status {
                    machine: machine.clone(),
                    pane: seen.pane.clone(),
                    text: state.summary(),
                });
                states.insert(url.clone(), state);
            }
            if statuses.send(report).is_err() {
                return;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pr(checks: Value, comments: Value, reviews: Value) -> PrState {
        PrState::from_json(&json!({
            "number": 12, "headRefOid": "abc",
            "statusCheckRollup": checks, "comments": comments, "reviews": reviews,
        }))
    }

    fn comment(id: &str, author: &str) -> Value {
        json!({"id": id, "author": {"login": author}})
    }

    #[test]
    fn checks_and_contexts_read_as_failed_pending_or_passing() {
        let state = pr(
            json!([
                {"name": "test", "conclusion": "FAILURE", "status": "COMPLETED"},
                {"name": "lint", "conclusion": "SUCCESS", "status": "COMPLETED"},
                {"name": "build", "conclusion": "", "status": "IN_PROGRESS"},
                {"context": "ci/legacy", "state": "ERROR"},
            ]),
            json!([]),
            json!([]),
        );
        assert_eq!(
            state.failed,
            BTreeSet::from(["test".to_string(), "ci/legacy".to_string()])
        );
        assert!(state.pending);
        assert_eq!(state.summary(), "PR #12 · CI failing");
    }

    #[test]
    fn the_first_check_only_records_the_pr() {
        let failing = pr(
            json!([{"name": "test", "conclusion": "FAILURE"}]),
            json!([]),
            json!([]),
        );
        assert_eq!(nudge(None, &failing), None);
    }

    #[test]
    fn a_new_failure_nudges_once() {
        let passing = pr(
            json!([{"name": "test", "conclusion": ""}]),
            json!([]),
            json!([]),
        );
        let failing = pr(
            json!([{"name": "test", "conclusion": "FAILURE"}]),
            json!([]),
            json!([]),
        );
        let text = nudge(Some(&passing), &failing).unwrap();
        assert_eq!(
            text,
            "CI failed on PR #12 (test). Look at the failure with `gh pr checks 12` and fix it."
        );
        assert_eq!(nudge(Some(&failing), &failing), None);
        // A new push that fails again is news again.
        let mut pushed = failing.clone();
        pushed.sha = "def".into();
        assert!(nudge(Some(&failing), &pushed).is_some());
    }

    #[test]
    fn new_comments_and_reviews_nudge_without_their_text() {
        let before = pr(json!([]), json!([comment("c1", "travis")]), json!([]));
        let after = pr(
            json!([]),
            json!([comment("c1", "travis"), comment("c2", "alice")]),
            json!([{"id": "r1", "author": {"login": "bob"}, "state": "CHANGES_REQUESTED", "body": "do X"}]),
        );
        let text = nudge(Some(&before), &after).unwrap();
        assert_eq!(
            text,
            "There are new comments and reviews on PR #12 from @alice, @bob. Read them with `gh pr view 12 --comments` and follow up."
        );
        assert!(!text.contains("do X"));
    }

    #[test]
    fn github_remotes_name_their_repo() {
        for url in [
            "git@github.com:tlpinney/remuda.git",
            "https://github.com/tlpinney/remuda",
            "https://github.com/tlpinney/remuda.git\n",
            "ssh://git@github.com/tlpinney/remuda.git",
        ] {
            assert_eq!(repo_of(url).as_deref(), Some("tlpinney/remuda"), "{url}");
        }
        assert_eq!(repo_of("https://gitlab.com/a/b"), None);
        assert_eq!(repo_of("git@github.com:a"), None);
    }

    fn target(pane: &str, pr: Option<&str>) -> Target {
        (
            "ops@ziggy".into(),
            "ziggy".into(),
            Seen {
                pane: pane.into(),
                name: pane.into(),
                status: "idle".into(),
                launched: None,
                title: String::new(),
                cwd: "/w".into(),
                pr: pr.map(str::to_string),
            },
        )
    }

    #[test]
    fn tags_win_and_branches_count_only_for_an_agent_alone_in_its_worktree() {
        let checkout = |top: &str, branch: &str| {
            (
                Instant::now(),
                Some(Checkout {
                    toplevel: top.into(),
                    branch: branch.into(),
                    repo: "o/r".into(),
                }),
            )
        };
        let branches = HashMap::from([
            (
                ("o/r".into(), "fix-a".into()),
                Some("https://github.com/o/r/pull/1".into()),
            ),
            (
                ("o/r".into(), "fix-b".into()),
                Some("https://github.com/o/r/pull/2".into()),
            ),
        ]);
        let targets = [
            target("p1", Some("https://github.com/o/r/pull/9")),
            target("p2", None),
            target("p3", None),
            target("p4", None),
        ];
        let checkouts = HashMap::from([
            (("ziggy".into(), "p2".into()), checkout("/wt/a", "fix-a")),
            // p3 and p4 share a worktree: neither is linked.
            (("ziggy".into(), "p3".into()), checkout("/wt/b", "fix-b")),
            (("ziggy".into(), "p4".into()), checkout("/wt/b", "fix-b")),
        ]);
        let linked = links(&targets, &checkouts, &branches);
        let get = |pane: &str| {
            linked
                .get(&("ziggy".to_string(), pane.to_string()))
                .cloned()
        };
        assert_eq!(get("p1").as_deref(), Some("https://github.com/o/r/pull/9"));
        assert_eq!(get("p2").as_deref(), Some("https://github.com/o/r/pull/1"));
        assert_eq!(get("p3"), None);
        assert_eq!(get("p4"), None);
    }
}
