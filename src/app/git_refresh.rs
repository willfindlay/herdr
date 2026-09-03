use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use super::{App, GIT_REMOTE_STATUS_REFRESH_INTERVAL, GIT_REPO_DISCOVERY_REFRESH_INTERVAL};
use crate::events::AppEvent;
use crate::terminal::TerminalId;
use crate::workspace::{
    GitStatusCacheEntry, GitStatusRefreshDemand, TerminalGitStatus, WorkspaceGitStatus,
};

/// Who receives the git facts for one checkout.
#[derive(Clone, Debug, PartialEq, Eq)]
enum GitRefreshTarget {
    Workspace {
        workspace_id: String,
        resolved_identity_cwd: PathBuf,
    },
    Terminal {
        terminal_id: TerminalId,
        cwd: PathBuf,
        /// The pane shell's cwd, tried when `cwd` is outside any checkout. The
        /// foreground group can hand back a helper's directory (a language
        /// server started by the agent) when the leader's cwd is unreadable.
        shell_cwd: Option<PathBuf>,
    },
}

impl GitRefreshTarget {
    fn cwd(&self) -> &PathBuf {
        match self {
            Self::Workspace {
                resolved_identity_cwd,
                ..
            } => resolved_identity_cwd,
            Self::Terminal { cwd, .. } => cwd,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct GitRefreshItem {
    target: GitRefreshTarget,
    cache_key_hint: Option<PathBuf>,
    demand: GitStatusRefreshDemand,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct GitRefreshJob {
    cache_key: PathBuf,
    cached: Option<GitStatusCacheEntry>,
    targets: Vec<(GitRefreshTarget, GitStatusRefreshDemand)>,
    demand: GitStatusRefreshDemand,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct GitRefreshOutput {
    results: Vec<WorkspaceGitStatus>,
    terminal_results: Vec<TerminalGitStatus>,
    cache_updates: Vec<(PathBuf, GitStatusCacheEntry)>,
}

fn union_demand(
    left: GitStatusRefreshDemand,
    right: GitStatusRefreshDemand,
) -> GitStatusRefreshDemand {
    GitStatusRefreshDemand {
        branch: left.branch || right.branch,
        ahead_behind: left.ahead_behind || right.ahead_behind,
        dirty: left.dirty || right.dirty,
    }
}

impl App {
    pub(crate) fn start_git_status_refresh_if_due(&mut self, now: Instant) {
        let Some(deadline) = self.git_refresh_deadline() else {
            return;
        };

        if now < deadline {
            return;
        }

        let refresh_repo_discovery = self.git_identity_refresh_requested
            || now.saturating_duration_since(self.last_git_repo_discovery_refresh)
                >= GIT_REPO_DISCOVERY_REFRESH_INTERVAL;
        let mut workspace_demand = self.git_refresh_demand();
        if self.git_identity_refresh_requested {
            workspace_demand.branch = true;
        }
        let mut items = self.workspace_git_refresh_items(refresh_repo_discovery, workspace_demand);
        items.extend(self.terminal_git_refresh_items(self.agent_git_refresh_demand()));

        if items.is_empty() {
            self.last_git_remote_status_refresh = now;
            self.git_identity_refresh_requested = false;
            return;
        }

        self.git_refresh_in_flight = true;
        let event_tx = self.event_tx.clone();
        let cache = self.git_status_cache.clone();
        self.git_identity_refresh_requested = false;
        if refresh_repo_discovery {
            self.last_git_repo_discovery_refresh = now;
        }
        std::thread::spawn(move || {
            let output = refresh_git_statuses_with_cache(items, &cache);
            let _ = event_tx.blocking_send(AppEvent::GitStatusRefreshed {
                results: output.results,
                terminal_results: output.terminal_results,
                cache_updates: output.cache_updates,
            });
        });
    }

    pub(crate) fn request_git_identity_refresh(&mut self, now: Instant) {
        self.git_identity_refresh_requested = true;
        self.mark_git_status_refresh_due(now);
    }

    pub(crate) fn mark_git_status_refresh_due(&mut self, now: Instant) {
        self.git_status_cache
            .retain(|_, entry| entry.fingerprint.is_some());
        if self.git_refresh_in_flight {
            self.git_refresh_due_after_in_flight = true;
            return;
        }
        self.last_git_remote_status_refresh = now
            .checked_sub(GIT_REMOTE_STATUS_REFRESH_INTERVAL)
            .unwrap_or(now);
        self.git_refresh_due_after_in_flight = false;
    }

    pub(crate) fn git_refresh_deadline(&self) -> Option<Instant> {
        let agent_consumer = !self.agent_git_refresh_demand().is_empty();
        (!self.git_refresh_in_flight
            && !self.state.workspaces.is_empty()
            && (self.git_identity_refresh_requested
                || !self.git_refresh_demand().is_empty()
                || agent_consumer))
            .then_some(self.last_git_remote_status_refresh + GIT_REMOTE_STATUS_REFRESH_INTERVAL)
    }

    fn git_refresh_demand(&self) -> GitStatusRefreshDemand {
        let mut demand = GitStatusRefreshDemand::default();
        for token in self.state.sidebar_spaces.rows.iter().flatten() {
            match token.parts().0 {
                crate::config::SpaceSidebarToken::Branch => demand.branch = true,
                crate::config::SpaceSidebarToken::GitStatus => demand.ahead_behind = true,
                _ => {}
            }
        }
        demand
    }

    /// Demand raised by agent rows: both counts, for as long as some open
    /// agent pane renders the `git_status` token.
    fn agent_git_refresh_demand(&self) -> GitStatusRefreshDemand {
        let wants_git_status = self.state.terminals.values().any(|terminal| {
            terminal.is_agent_terminal() && self.state.terminal_wants_git_status(terminal)
        });
        GitStatusRefreshDemand {
            branch: false,
            ahead_behind: wants_git_status,
            dirty: wants_git_status,
        }
    }

    fn workspace_git_refresh_items(
        &self,
        refresh_repo_discovery: bool,
        demand: GitStatusRefreshDemand,
    ) -> Vec<GitRefreshItem> {
        if demand.is_empty() {
            return Vec::new();
        }
        self.state
            .workspaces
            .iter()
            .filter_map(|ws| {
                let cwd =
                    ws.resolved_identity_cwd_from(&self.state.terminals, &self.terminal_runtimes)?;
                let cache_key_hint = (!refresh_repo_discovery && ws.cached_identity_cwd == cwd)
                    .then(|| ws.cached_git_status_key.clone());
                Some(GitRefreshItem {
                    target: GitRefreshTarget::Workspace {
                        workspace_id: ws.id.clone(),
                        resolved_identity_cwd: cwd,
                    },
                    cache_key_hint,
                    demand,
                })
            })
            .collect()
    }

    /// One item per agent pane, at the foreground cwd the agent poll recorded,
    /// so an agent that moved into a linked worktree reports that worktree
    /// rather than the shell's or the workspace's directory.
    fn terminal_git_refresh_items(&self, demand: GitStatusRefreshDemand) -> Vec<GitRefreshItem> {
        if demand.is_empty() {
            return Vec::new();
        }
        let mut items = Vec::new();
        for ws in &self.state.workspaces {
            for tab in &ws.tabs {
                for pane_id in tab.layout.pane_ids() {
                    let Some(pane) = tab.panes.get(&pane_id) else {
                        continue;
                    };
                    let Some(terminal) = self.state.terminals.get(&pane.attached_terminal_id)
                    else {
                        continue;
                    };
                    if !terminal.is_agent_terminal()
                        || !self.state.terminal_wants_git_status(terminal)
                    {
                        continue;
                    }
                    let shell_cwd =
                        tab.cwd_for_pane(pane_id, &self.state.terminals, &self.terminal_runtimes);
                    let Some(cwd) = terminal.agent_cwd.clone().or_else(|| shell_cwd.clone()) else {
                        continue;
                    };
                    items.push(GitRefreshItem {
                        target: GitRefreshTarget::Terminal {
                            terminal_id: terminal.id.clone(),
                            shell_cwd: shell_cwd.filter(|shell_cwd| *shell_cwd != cwd),
                            cwd,
                        },
                        cache_key_hint: None,
                        demand,
                    });
                }
            }
        }
        items
    }
}

/// Picks the checkout a terminal target reports on: its foreground cwd when
/// that sits in a repository, else the pane shell's cwd when that one does.
fn resolve_terminal_checkout(target: GitRefreshTarget) -> GitRefreshTarget {
    let GitRefreshTarget::Terminal {
        terminal_id,
        cwd,
        shell_cwd,
    } = target
    else {
        return target;
    };
    let cwd = match shell_cwd {
        Some(shell_cwd)
            if crate::workspace::git_status_cache_key(&cwd).is_none()
                && crate::workspace::git_status_cache_key(&shell_cwd).is_some() =>
        {
            shell_cwd
        }
        _ => cwd,
    };
    GitRefreshTarget::Terminal {
        terminal_id,
        cwd,
        shell_cwd: None,
    }
}

fn deduplicate_git_refresh_items(
    items: Vec<GitRefreshItem>,
    cache: &HashMap<PathBuf, GitStatusCacheEntry>,
) -> Vec<GitRefreshJob> {
    let mut indexes = HashMap::<PathBuf, usize>::new();
    let mut jobs = Vec::<GitRefreshJob>::new();

    for item in items {
        let item = GitRefreshItem {
            target: resolve_terminal_checkout(item.target),
            ..item
        };
        // A workspace arrives without a hint when it is re-checking its
        // identity, so its old entry must not be reused. A terminal resolves
        // its key from the cwd on every tick and has no stored key to distrust.
        let reconcile = item.cache_key_hint.is_none()
            && matches!(item.target, GitRefreshTarget::Workspace { .. });
        let cache_key = item.cache_key_hint.unwrap_or_else(|| {
            crate::workspace::git_status_cache_key(item.target.cwd())
                .unwrap_or_else(|| item.target.cwd().clone())
        });
        if let Some(&index) = indexes.get(&cache_key) {
            jobs[index].cached = jobs[index].cached.take().filter(|_| !reconcile);
            jobs[index].targets.push((item.target, item.demand));
            jobs[index].demand = union_demand(jobs[index].demand, item.demand);
            continue;
        }

        let cached = cache.get(&cache_key).filter(|_| !reconcile).cloned();
        indexes.insert(cache_key.clone(), jobs.len());
        jobs.push(GitRefreshJob {
            cache_key,
            cached,
            targets: vec![(item.target, item.demand)],
            demand: item.demand,
        });
    }

    jobs
}

fn refresh_git_statuses_with_cache(
    items: Vec<GitRefreshItem>,
    cache: &HashMap<PathBuf, GitStatusCacheEntry>,
) -> GitRefreshOutput {
    let mut results = Vec::new();
    let mut terminal_results = Vec::new();
    let mut cache_updates = Vec::new();

    for job in deduplicate_git_refresh_items(items, cache) {
        let (snapshot, cache_entry) = crate::workspace::git_status_snapshot_for_cwd_with_demand(
            &job.cache_key,
            job.cached.as_ref(),
            job.demand,
        );
        if let Some(cache_entry) = cache_entry {
            cache_updates.push((job.cache_key.clone(), cache_entry));
        }
        for (target, demand) in job.targets {
            match target {
                GitRefreshTarget::Workspace {
                    workspace_id,
                    resolved_identity_cwd,
                } => results.push(snapshot.clone().into_workspace_status(
                    workspace_id,
                    resolved_identity_cwd,
                    job.cache_key.clone(),
                    demand,
                )),
                GitRefreshTarget::Terminal {
                    terminal_id, cwd, ..
                } => terminal_results.push(TerminalGitStatus {
                    terminal_id,
                    cwd,
                    demand,
                    ahead_behind: snapshot.ahead_behind,
                    dirty: snapshot.dirty,
                }),
            }
        }
    }

    GitRefreshOutput {
        results,
        terminal_results,
        cache_updates,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::Workspace;

    #[test]
    fn git_refresh_deduplicates_workspaces_with_same_cache_key() {
        let repo =
            std::env::temp_dir().join(format!("herdr-git-refresh-dedupe-{}", std::process::id()));
        let nested = repo.join("nested");
        let other = repo.join("other");
        std::fs::create_dir_all(&nested).expect("create nested dir");
        std::fs::create_dir_all(&other).expect("create other dir");
        std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .arg("init")
            .output()
            .expect("run git init");

        let output = refresh_git_statuses_with_cache(
            vec![
                GitRefreshItem {
                    target: GitRefreshTarget::Workspace {
                        workspace_id: "one".into(),
                        resolved_identity_cwd: nested.clone(),
                    },
                    cache_key_hint: None,
                    demand: GitStatusRefreshDemand::ALL,
                },
                GitRefreshItem {
                    target: GitRefreshTarget::Workspace {
                        workspace_id: "two".into(),
                        resolved_identity_cwd: other.clone(),
                    },
                    cache_key_hint: None,
                    demand: GitStatusRefreshDemand::ALL,
                },
            ],
            &HashMap::new(),
        );

        assert_eq!(output.cache_updates.len(), 1);
        assert_eq!(
            output.cache_updates[0].0,
            std::fs::canonicalize(&repo).expect("canonical repo path")
        );
        assert_eq!(output.results.len(), 2);
        assert_eq!(output.results[0].workspace_id, "one");
        assert_eq!(output.results[0].resolved_identity_cwd, nested);
        assert_eq!(output.results[1].workspace_id, "two");
        assert_eq!(output.results[1].resolved_identity_cwd, other);

        let _ = std::fs::remove_dir_all(repo);
    }

    #[test]
    fn shared_root_repo_refresh_keeps_workspace_specific_fallback_labels() {
        let cache_key = PathBuf::from("/");
        let cached = GitStatusCacheEntry {
            fingerprint: None,
            retry_after: Some(Instant::now() + std::time::Duration::from_secs(30)),
            snapshot: crate::workspace::WorkspaceGitStatusSnapshot {
                auto_label: "/".into(),
                branch: Some("main".into()),
                ahead_behind: None,
                dirty: None,
                space: Some(crate::workspace::GitSpaceMetadata {
                    key: "/.git".into(),
                    checkout_key: "/".into(),
                    repo_name: "repo".into(),
                    repo_root: cache_key.clone(),
                    is_linked_worktree: false,
                }),
            },
            dirty_refreshed_at: None,
        };
        let items = ["alpha", "beta"]
            .into_iter()
            .map(|name| GitRefreshItem {
                target: GitRefreshTarget::Workspace {
                    workspace_id: name.into(),
                    resolved_identity_cwd: cache_key.join(name),
                },
                cache_key_hint: Some(cache_key.clone()),
                demand: GitStatusRefreshDemand::ALL,
            })
            .collect();

        let output = refresh_git_statuses_with_cache(items, &HashMap::from([(cache_key, cached)]));

        assert_eq!(output.cache_updates.len(), 1);
        assert_eq!(output.results.len(), 2);
        assert_eq!(output.results[0].auto_label, "alpha");
        assert_eq!(output.results[1].auto_label, "beta");
        assert_eq!(output.results[0].branch.as_deref(), Some("main"));
        assert_eq!(output.results[1].branch.as_deref(), Some("main"));
    }

    #[test]
    fn git_refresh_item_collection_does_not_discover_uncached_cwd() {
        let mut app = test_app(&crate::config::Config::default());
        let cwd = std::env::temp_dir().join(format!("herdr-uncached-cwd-{}", std::process::id()));
        let mut ws = Workspace::test_new("test");
        ws.identity_cwd = cwd.clone();
        ws.tabs.clear();
        app.state.workspaces.push(ws);

        let items = app.workspace_git_refresh_items(false, GitStatusRefreshDemand::ALL);

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].target.cwd(), &cwd);
        assert_eq!(items[0].cache_key_hint, None);
    }

    #[test]
    fn git_refresh_item_collection_reuses_matching_cached_key() {
        let mut app = test_app(&crate::config::Config::default());
        let cwd = PathBuf::from("/repo/deep/nested");
        let cache_key = PathBuf::from("/repo");
        let mut ws = Workspace::test_new("test");
        ws.identity_cwd = cwd.clone();
        ws.cached_identity_cwd = cwd;
        ws.cached_git_status_key = cache_key.clone();
        ws.tabs.clear();
        app.state.workspaces.push(ws);

        let items = app.workspace_git_refresh_items(false, GitStatusRefreshDemand::ALL);

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].cache_key_hint, Some(cache_key));
    }

    #[test]
    fn periodic_repo_discovery_ignores_cached_key_hints() {
        let mut app = test_app(&crate::config::Config::default());
        let cwd = PathBuf::from("/repo/deep/nested");
        let mut ws = Workspace::test_new("test");
        ws.identity_cwd = cwd.clone();
        ws.cached_identity_cwd = cwd;
        ws.cached_git_status_key = PathBuf::from("/repo");
        ws.tabs.clear();
        app.state.workspaces.push(ws);

        let items = app.workspace_git_refresh_items(true, GitStatusRefreshDemand::ALL);

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].cache_key_hint, None);
        let cache_key = items[0].target.cwd().clone();
        let cached = GitStatusCacheEntry {
            fingerprint: None,
            retry_after: None,
            snapshot: crate::workspace::WorkspaceGitStatusSnapshot {
                auto_label: "stale".into(),
                branch: None,
                ahead_behind: None,
                dirty: None,
                space: None,
            },
            dirty_refreshed_at: None,
        };
        let jobs = deduplicate_git_refresh_items(items, &HashMap::from([(cache_key, cached)]));
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].cached, None);
    }

    #[test]
    fn cwd_identity_refresh_runs_once_without_sidebar_git_tokens() {
        let mut config = crate::config::Config::default();
        config.ui.sidebar.spaces.rows = vec![vec![crate::config::SpaceSidebarToken::Workspace]];
        let mut app = test_app(&config);
        app.state.workspaces.push(Workspace::test_new("test"));
        let now = Instant::now();

        app.request_git_identity_refresh(now);

        assert!(app.git_refresh_deadline().is_some());
        app.start_git_status_refresh_if_due(now);
        assert!(app.git_refresh_in_flight);
        assert!(!app.git_identity_refresh_requested);
    }

    #[test]
    fn due_git_refresh_does_not_start_without_sidebar_consumer() {
        let mut config = crate::config::Config::default();
        config.ui.sidebar.spaces.rows = vec![vec![crate::config::SpaceSidebarToken::Workspace]];
        let mut app = test_app(&config);
        app.state.workspaces.push(Workspace::test_new("test"));
        let now = Instant::now();
        app.last_git_remote_status_refresh = now - GIT_REMOTE_STATUS_REFRESH_INTERVAL;

        app.start_git_status_refresh_if_due(now);

        assert!(!app.git_refresh_in_flight);
        assert!(app.event_rx.try_recv().is_err());
    }

    #[test]
    fn git_refresh_demand_matches_sidebar_rows() {
        let cases = [
            (
                crate::config::SpaceSidebarToken::Workspace,
                GitStatusRefreshDemand::default(),
            ),
            (
                crate::config::SpaceSidebarToken::Branch,
                GitStatusRefreshDemand {
                    branch: true,
                    ahead_behind: false,
                    dirty: false,
                },
            ),
            (
                crate::config::SpaceSidebarToken::GitStatus,
                GitStatusRefreshDemand {
                    branch: false,
                    ahead_behind: true,
                    dirty: false,
                },
            ),
        ];

        for (token, expected) in cases {
            let mut config = crate::config::Config::default();
            config.ui.sidebar.spaces.rows = vec![vec![token.clone()]];
            let mut app = test_app(&config);
            app.state.workspaces.push(Workspace::test_new("test"));

            assert_eq!(app.git_refresh_demand(), expected, "token: {token:?}");
            assert_eq!(
                app.git_refresh_deadline().is_some(),
                !expected.is_empty(),
                "token: {token:?}"
            );
        }
    }

    #[test]
    fn unnamed_linked_worktree_does_not_force_periodic_branch_refresh() {
        let mut config = crate::config::Config::default();
        config.ui.sidebar.spaces.rows = vec![vec![crate::config::SpaceSidebarToken::Workspace]];
        let mut app = test_app(&config);
        let mut child = Workspace::test_new("test");
        child.custom_name = None;
        child.worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: "repo".into(),
            label: "repo".into(),
            repo_root: "/repo".into(),
            checkout_path: "/repo-worktree".into(),
            is_linked_worktree: true,
        });
        app.state.workspaces.push(child);

        assert_eq!(app.git_refresh_deadline(), None);
    }

    #[test]
    fn custom_named_linked_worktree_does_not_require_branch_refresh() {
        let mut config = crate::config::Config::default();
        config.ui.sidebar.spaces.rows = vec![vec![crate::config::SpaceSidebarToken::Workspace]];
        let mut app = test_app(&config);
        let mut child = Workspace::test_new("custom");
        child.worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: "repo".into(),
            label: "repo".into(),
            repo_root: "/repo".into(),
            checkout_path: "/repo-worktree".into(),
            is_linked_worktree: true,
        });
        app.state.workspaces.push(child);

        assert_eq!(app.git_refresh_deadline(), None);
    }

    #[test]
    fn headless_deadline_can_suppress_git_refresh_timer() {
        let mut app = test_app(&crate::config::Config::default());
        app.state.workspaces.push(Workspace::test_new("test"));
        let now = Instant::now();
        app.last_git_remote_status_refresh = now - GIT_REMOTE_STATUS_REFRESH_INTERVAL;

        assert_eq!(
            app.next_headless_loop_deadline_with_git_refresh(now, false, false),
            None
        );
        assert_eq!(
            app.next_headless_loop_deadline_with_git_refresh(now, false, true),
            Some(now)
        );
    }

    #[test]
    fn explicit_git_refresh_invalidates_cached_non_git_results() {
        let mut app = test_app(&crate::config::Config::default());
        let cwd = std::env::temp_dir().join(format!("herdr-git-miss-{}", std::process::id()));
        std::fs::create_dir_all(&cwd).unwrap();
        let (_, entry) = crate::workspace::git_status_snapshot_for_cwd_with_demand(
            &cwd,
            None,
            GitStatusRefreshDemand::ALL,
        );
        app.git_status_cache
            .insert(cwd.clone(), entry.expect("non-Git cache entry"));

        app.mark_git_status_refresh_due(Instant::now());

        assert!(app.git_status_cache.is_empty());
        std::fs::remove_dir_all(cwd).unwrap();
    }

    #[test]
    fn git_refresh_due_request_survives_in_flight_refresh() {
        let mut app = test_app(&crate::config::Config::default());
        let now = Instant::now();
        app.git_refresh_in_flight = true;

        app.mark_git_status_refresh_due(now);
        assert!(app.git_refresh_due_after_in_flight);

        app.handle_internal_event(AppEvent::GitStatusRefreshed {
            results: Vec::new(),
            terminal_results: Vec::new(),
            cache_updates: Vec::new(),
        });

        assert!(!app.git_refresh_in_flight);
        assert!(!app.git_refresh_due_after_in_flight);
        assert_eq!(app.git_refresh_deadline(), None);

        app.state.workspaces.push(Workspace::test_new("test"));
        let deadline = app
            .git_refresh_deadline()
            .expect("refresh should be due once a workspace exists");
        assert!(deadline <= Instant::now());
    }

    fn agent_terminal(app: &mut super::super::App, ws_idx: usize, cwd: &str) -> TerminalId {
        let ws = &app.state.workspaces[ws_idx];
        let terminal_id = ws.terminal_id(ws.tabs[0].root_pane).unwrap().clone();
        let mut terminal =
            crate::terminal::TerminalState::new(terminal_id.clone(), PathBuf::from(cwd));
        terminal.agent_name = Some("claude".into());
        app.state.terminals.insert(terminal_id.clone(), terminal);
        terminal_id
    }

    fn agent_git_status_config() -> crate::config::Config {
        let mut config = crate::config::Config::default();
        config.ui.sidebar.agents.git_status.enabled = true;
        config
    }

    #[test]
    fn terminal_git_refresh_items_cover_agent_panes_at_their_cwd() {
        let mut app = test_app(&agent_git_status_config());
        app.state.workspaces.push(Workspace::test_new("agent"));
        app.state.workspaces.push(Workspace::test_new("shell"));
        let agent_id = agent_terminal(&mut app, 0, "/repo/agent-worktree");
        let shell_ws = &app.state.workspaces[1];
        let shell_id = shell_ws
            .terminal_id(shell_ws.tabs[0].root_pane)
            .unwrap()
            .clone();
        app.state.terminals.insert(
            shell_id.clone(),
            crate::terminal::TerminalState::new(shell_id, PathBuf::from("/repo")),
        );
        let demand = GitStatusRefreshDemand {
            branch: false,
            ahead_behind: true,
            dirty: true,
        };

        let items = app.terminal_git_refresh_items(demand);

        assert_eq!(
            items,
            vec![GitRefreshItem {
                target: GitRefreshTarget::Terminal {
                    terminal_id: agent_id,
                    cwd: PathBuf::from("/repo/agent-worktree"),
                    shell_cwd: None,
                },
                cache_key_hint: None,
                demand,
            }]
        );
        assert!(app
            .terminal_git_refresh_items(GitStatusRefreshDemand::default())
            .is_empty());
    }

    #[test]
    fn terminal_git_refresh_items_prefer_the_recorded_agent_cwd() {
        let mut app = test_app(&agent_git_status_config());
        app.state.workspaces.push(Workspace::test_new("agent"));
        let agent_id = agent_terminal(&mut app, 0, "/repo");
        app.state
            .terminals
            .get_mut(&agent_id)
            .expect("agent terminal")
            .agent_cwd = Some(PathBuf::from("/repo-worktrees/feature"));
        let demand = GitStatusRefreshDemand {
            branch: false,
            ahead_behind: true,
            dirty: true,
        };

        let items = app.terminal_git_refresh_items(demand);

        assert_eq!(
            items[0].target,
            GitRefreshTarget::Terminal {
                terminal_id: agent_id,
                cwd: PathBuf::from("/repo-worktrees/feature"),
                shell_cwd: Some(PathBuf::from("/repo")),
            }
        );
    }

    #[test]
    fn deduplicate_unions_demand_for_targets_sharing_a_checkout() {
        let cache_key = PathBuf::from("/repo");
        let workspace_demand = GitStatusRefreshDemand {
            branch: true,
            ahead_behind: false,
            dirty: false,
        };
        let terminal_demand = GitStatusRefreshDemand {
            branch: false,
            ahead_behind: true,
            dirty: true,
        };
        let terminal_id = TerminalId::alloc();
        let items = vec![
            GitRefreshItem {
                target: GitRefreshTarget::Workspace {
                    workspace_id: "one".into(),
                    resolved_identity_cwd: cache_key.clone(),
                },
                cache_key_hint: Some(cache_key.clone()),
                demand: workspace_demand,
            },
            GitRefreshItem {
                target: GitRefreshTarget::Terminal {
                    terminal_id: terminal_id.clone(),
                    cwd: cache_key.join("nested"),
                    shell_cwd: None,
                },
                cache_key_hint: Some(cache_key.clone()),
                demand: terminal_demand,
            },
        ];

        let jobs = deduplicate_git_refresh_items(items, &HashMap::new());

        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].cache_key, cache_key);
        assert_eq!(jobs[0].demand, GitStatusRefreshDemand::ALL);
        assert_eq!(jobs[0].targets.len(), 2);
        assert_eq!(jobs[0].targets[0].1, workspace_demand);
        assert_eq!(jobs[0].targets[1].1, terminal_demand);
    }

    #[test]
    fn refresh_emits_terminal_results_for_terminal_targets() {
        let repo =
            std::env::temp_dir().join(format!("herdr-git-refresh-terminal-{}", std::process::id()));
        std::fs::create_dir_all(&repo).expect("create repo dir");
        std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .arg("init")
            .output()
            .expect("run git init");
        let terminal_id = TerminalId::alloc();
        let demand = GitStatusRefreshDemand {
            branch: false,
            ahead_behind: true,
            dirty: true,
        };

        let output = refresh_git_statuses_with_cache(
            vec![GitRefreshItem {
                target: GitRefreshTarget::Terminal {
                    terminal_id: terminal_id.clone(),
                    cwd: repo.clone(),
                    shell_cwd: None,
                },
                cache_key_hint: None,
                demand,
            }],
            &HashMap::new(),
        );

        assert!(output.results.is_empty());
        assert_eq!(output.terminal_results.len(), 1);
        assert_eq!(output.terminal_results[0].terminal_id, terminal_id);
        assert_eq!(output.terminal_results[0].cwd, repo);
        assert_eq!(output.terminal_results[0].demand, demand);

        let _ = std::fs::remove_dir_all(repo);
    }

    #[test]
    fn terminal_target_falls_back_to_shell_cwd_outside_a_checkout() {
        let base =
            std::env::temp_dir().join(format!("herdr-git-refresh-fallback-{}", std::process::id()));
        let repo = base.join("repo");
        let helper_dir = base.join("helper");
        std::fs::create_dir_all(&repo).expect("create repo dir");
        std::fs::create_dir_all(&helper_dir).expect("create helper dir");
        std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .arg("init")
            .output()
            .expect("run git init");
        let terminal_id = TerminalId::alloc();

        let jobs = deduplicate_git_refresh_items(
            vec![GitRefreshItem {
                target: GitRefreshTarget::Terminal {
                    terminal_id: terminal_id.clone(),
                    cwd: helper_dir.clone(),
                    shell_cwd: Some(repo.clone()),
                },
                cache_key_hint: None,
                demand: GitStatusRefreshDemand::ALL,
            }],
            &HashMap::new(),
        );

        assert_eq!(jobs.len(), 1);
        assert_eq!(
            jobs[0].cache_key,
            std::fs::canonicalize(&repo).expect("canonical repo path")
        );
        assert_eq!(
            jobs[0].targets[0].0,
            GitRefreshTarget::Terminal {
                terminal_id,
                cwd: repo,
                shell_cwd: None,
            }
        );

        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn agent_git_status_demand_needs_an_agent_terminal() {
        let mut config = crate::config::Config::default();
        config.ui.sidebar.spaces.rows = vec![vec![crate::config::SpaceSidebarToken::Workspace]];
        config.ui.sidebar.agents.rows = vec![vec![crate::config::AgentSidebarToken::GitStatus]];
        config.ui.sidebar.agents.git_status.enabled = true;
        let mut app = test_app(&config);
        app.state.workspaces.push(Workspace::test_new("test"));

        assert!(app.agent_git_refresh_demand().is_empty());
        assert_eq!(app.git_refresh_deadline(), None);

        agent_terminal(&mut app, 0, "/repo");

        assert_eq!(
            app.agent_git_refresh_demand(),
            GitStatusRefreshDemand {
                branch: false,
                ahead_behind: true,
                dirty: true,
            }
        );
        assert!(app.git_refresh_deadline().is_some());
    }

    #[test]
    fn agent_git_status_demand_follows_the_rows_of_the_open_panes() {
        let mut config = crate::config::Config::default();
        config.ui.sidebar.spaces.rows = vec![vec![crate::config::SpaceSidebarToken::Workspace]];
        config.ui.sidebar.agents.rows = vec![vec![crate::config::AgentSidebarToken::Agent]];
        config.ui.sidebar.agents.rows_by_agent.insert(
            "claude".into(),
            vec![vec![crate::config::AgentSidebarToken::GitStatus]],
        );
        config.ui.sidebar.agents.git_status.enabled = true;
        let mut app = test_app(&config);
        app.state.workspaces.push(Workspace::test_new("test"));
        let terminal_id = agent_terminal(&mut app, 0, "/repo");
        let terminal = app
            .state
            .terminals
            .get_mut(&terminal_id)
            .expect("agent terminal");
        terminal.agent_name = None;
        terminal.detected_agent = Some(crate::detect::Agent::Codex);

        assert!(app.agent_git_refresh_demand().is_empty());
        assert_eq!(app.git_refresh_deadline(), None);

        app.state
            .terminals
            .get_mut(&terminal_id)
            .expect("agent terminal")
            .detected_agent = Some(crate::detect::Agent::Claude);

        assert!(app.agent_git_refresh_demand().ahead_behind);
        assert!(app.agent_git_refresh_demand().dirty);
        assert!(app.git_refresh_deadline().is_some());
    }

    #[test]
    fn agent_git_status_demand_is_off_until_enabled() {
        let mut app = test_app(&crate::config::Config::default());
        app.state.workspaces.push(Workspace::test_new("test"));
        agent_terminal(&mut app, 0, "/repo");

        assert!(app.agent_git_refresh_demand().is_empty());
        assert!(app
            .terminal_git_refresh_items(app.agent_git_refresh_demand())
            .is_empty());
    }

    #[test]
    fn terminal_git_refresh_items_follow_the_rows_each_agent_resolves() {
        let mut config = crate::config::Config::default();
        config.ui.sidebar.agents.rows = vec![vec![crate::config::AgentSidebarToken::Agent]];
        config.ui.sidebar.agents.rows_by_agent.insert(
            "claude".into(),
            vec![vec![crate::config::AgentSidebarToken::GitStatus]],
        );
        config.ui.sidebar.agents.git_status.enabled = true;
        let mut app = test_app(&config);
        app.state.workspaces.push(Workspace::test_new("claude"));
        app.state.workspaces.push(Workspace::test_new("codex"));
        let claude_id = agent_terminal(&mut app, 0, "/repo/claude");
        let codex_id = agent_terminal(&mut app, 1, "/repo/codex");
        for (id, agent) in [
            (&claude_id, crate::detect::Agent::Claude),
            (&codex_id, crate::detect::Agent::Codex),
        ] {
            let terminal = app.state.terminals.get_mut(id).expect("agent terminal");
            terminal.agent_name = None;
            terminal.detected_agent = Some(agent);
        }

        let items = app.terminal_git_refresh_items(app.agent_git_refresh_demand());

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].target.cwd(), &PathBuf::from("/repo/claude"));
    }

    #[test]
    fn deduplicate_keeps_the_cache_for_a_terminal_target() {
        let cwd =
            std::env::temp_dir().join(format!("herdr-git-refresh-cache-{}", std::process::id()));
        std::fs::create_dir_all(&cwd).expect("create cwd");
        let cached = GitStatusCacheEntry {
            fingerprint: None,
            retry_after: None,
            snapshot: crate::workspace::WorkspaceGitStatusSnapshot {
                auto_label: "cache".into(),
                branch: None,
                ahead_behind: None,
                dirty: None,
                space: None,
            },
            dirty_refreshed_at: None,
        };
        let cache = HashMap::from([(cwd.clone(), cached.clone())]);
        let items = vec![GitRefreshItem {
            target: GitRefreshTarget::Terminal {
                terminal_id: TerminalId::alloc(),
                cwd: cwd.clone(),
                shell_cwd: None,
            },
            cache_key_hint: None,
            demand: GitStatusRefreshDemand::ALL,
        }];

        let jobs = deduplicate_git_refresh_items(items, &cache);

        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].cached.as_ref(), Some(&cached));

        let _ = std::fs::remove_dir_all(cwd);
    }

    #[test]
    fn terminal_target_reuses_the_dirty_count_within_the_refresh_interval() {
        let repo =
            std::env::temp_dir().join(format!("herdr-git-refresh-throttle-{}", std::process::id()));
        std::fs::create_dir_all(&repo).expect("create repo dir");
        for args in [
            vec!["init", "--quiet"],
            vec![
                "-c",
                "user.name=herdr",
                "-c",
                "user.email=herdr@example.com",
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                "init",
            ],
        ] {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .status()
                .expect("run git");
            assert!(status.success());
        }
        let terminal_id = TerminalId::alloc();
        let item = || GitRefreshItem {
            target: GitRefreshTarget::Terminal {
                terminal_id: terminal_id.clone(),
                cwd: repo.clone(),
                shell_cwd: None,
            },
            cache_key_hint: None,
            demand: GitStatusRefreshDemand {
                branch: false,
                ahead_behind: true,
                dirty: true,
            },
        };

        let first = refresh_git_statuses_with_cache(vec![item()], &HashMap::new());
        assert_eq!(first.terminal_results[0].dirty, Some(0));
        let mut cache: HashMap<_, _> = first.cache_updates.into_iter().collect();
        std::fs::write(repo.join("untracked.txt"), "x").expect("write untracked file");

        let second = refresh_git_statuses_with_cache(vec![item()], &cache);
        assert_eq!(second.terminal_results[0].dirty, Some(0));

        for entry in cache.values_mut() {
            entry.dirty_refreshed_at = Some(Instant::now() - std::time::Duration::from_secs(6));
        }
        let third = refresh_git_statuses_with_cache(vec![item()], &cache);
        assert_eq!(third.terminal_results[0].dirty, Some(1));

        let _ = std::fs::remove_dir_all(repo);
    }

    fn test_app(config: &crate::config::Config) -> super::super::App {
        super::super::App::new(
            config,
            crate::app::AppPolicy::TEST,
            None,
            tokio::sync::mpsc::unbounded_channel().1,
            crate::api::EventHub::default(),
        )
    }
}
