//! Resource sidebar owns polling and presentation; collectors never borrow UI entities.
mod disks;
mod overview;
mod presentation;

use crate::resource_monitor::{Collector, Probe, Ranking, Request, Snapshot, Target};
use gpui::{AppContext as _, Context, IntoElement, Render, Task, Window};
use std::collections::HashSet;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

/// Focused execution scope; a disconnected SSH pane has identity but no executable target.
#[derive(Clone)]
pub(crate) struct Scope {
    /// Stable host/distribution identity used to reject old results.
    pub key: String,
    /// Explicit host label, never inferred from a working directory.
    pub label: String,
    /// None while an SSH terminal is not authenticated or has disconnected.
    pub target: Option<Target>,
}

/// Independently selectable resource categories.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Category {
    Overview,
    Docker,
}

/// One workspace-owned sidebar, shared across panes targeting the same host.
pub(crate) struct ResourceMonitor {
    /// Scope supplied by the workspace's currently focused terminal.
    focused: Scope,
    /// Effective scope after applying the explicit local-host selection.
    scope: Scope,
    /// Explicit override to inspect the desktop host from an SSH pane.
    local_only: bool,
    /// Whether the workspace currently displays this sidebar.
    visible: bool,
    /// User-requested pause, independent of panel visibility.
    paused: bool,
    /// Active sidebar tab.
    category: Category,
    /// Last complete sample, retained with an error label when a later query fails.
    snapshot: Option<Snapshot>,
    /// Current collection failure; absence does not imply a sample exists.
    error: Option<String>,
    /// Additional data mounts are collapsed when entering a host.
    other_disks_expanded: bool,
    /// Stable container IDs whose port rows are expanded; empty by default.
    expanded: HashSet<String>,
    /// Owns timer and worker completion subscriptions; dropped on hide or scope change.
    polling: Option<Task<()>>,
    /// Shared with blocking workers and subprocess owners to stop work after cancellation.
    cancelled: Arc<AtomicBool>,
}

impl ResourceMonitor {
    /// Resolve collector freshness once for the sidebar and its live process inspector.
    fn collection_status(&self) -> crate::i18n::Message {
        use crate::i18n::Message;
        if self.scope.target.is_none() {
            Message::MonitorDisconnected
        } else if self.paused || !self.visible {
            Message::MonitorPaused
        } else if self.error.is_some() {
            Message::MonitorStale
        } else if self.snapshot.is_none() {
            Message::MonitorLoading
        } else {
            Message::MonitorLive
        }
    }

    /// Create a dormant sidebar; no OS or network work happens during construction.
    pub(crate) fn new(_cx: &mut Context<Self>) -> Self {
        let scope = Scope { key: "local".into(), label: "".into(), target: Some(Target::Native) };
        Self {
            focused: scope.clone(),
            scope,
            local_only: false,
            visible: false,
            paused: false,
            category: Category::Overview,
            snapshot: None,
            error: None,
            other_disks_expanded: false,
            expanded: HashSet::new(),
            polling: None,
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Bind the prepared host identity and panel visibility, cancelling any obsolete worker.
    /// Repeated calls for the same scope are cheap and perform no synchronous I/O.
    pub(crate) fn bind(&mut self, focused: Scope, visible: bool, cx: &mut Context<Self>) {
        self.focused = focused;
        let scope = if self.local_only {
            Scope {
                key: "local".into(),
                label: super::config::ui_language(cx)
                    .text(crate::i18n::Message::MonitorLocal)
                    .into(),
                target: Some(Target::Native),
            }
        } else {
            self.focused.clone()
        };
        let changed = self.scope.key != scope.key;
        let readiness_changed = self.scope.target.is_some() != scope.target.is_some();
        if changed || readiness_changed || self.visible != visible {
            self.stop_polling();
            if changed {
                self.snapshot = None;
                self.error = None;
                self.other_disks_expanded = false;
                self.expanded.clear();
            }
            self.scope = scope;
            self.visible = visible;
            self.start_polling(cx);
            cx.notify();
        } else {
            self.scope.label = scope.label;
        }
    }

    /// Signal cancellation before dropping the UI task so blocking children can be reaped.
    fn stop_polling(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
        self.polling = None;
    }

    /// Poll one collector at a time; a late result cannot cross a scope or visibility boundary.
    fn start_polling(&mut self, cx: &mut Context<Self>) {
        if !self.visible || self.paused || self.polling.is_some() {
            return;
        }
        let Some(target) = self.scope.target.clone() else { return };
        let key = self.scope.key.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        self.cancelled = cancelled.clone();
        let executor = cx.background_executor().clone();
        self.polling = Some(cx.spawn(async move |this, cx| {
            let mut collector: Option<Collector> = None;
            loop {
                let request = match this.update(cx, |view, _| Request {
                    docker: view.category == Category::Docker,
                    processes: false,
                }) {
                    Ok(request) => request,
                    Err(_) => break,
                };
                let target = target.clone();
                let token = cancelled.clone();
                let (next, result) = executor
                    .spawn(async move {
                        let mut collector = collector.unwrap_or_else(|| Collector::new(target));
                        let result = collector.sample(request, &token);
                        (collector, result)
                    })
                    .await;
                collector = Some(next);
                if cancelled.load(Ordering::Relaxed) {
                    break;
                }
                let keep = this
                    .update(cx, |view, cx| {
                        if view.scope.key != key || !view.visible || view.paused {
                            return false;
                        }
                        match result {
                            Ok(snapshot) => {
                                if let Probe::Ready(docker) = &snapshot.docker {
                                    view.expanded.retain(|id| {
                                        docker.containers.iter().any(|item| &item.id == id)
                                    });
                                }
                                view.snapshot = Some(snapshot);
                                view.error = None;
                            },
                            Err(error) => view.error = Some(error),
                        }
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !keep {
                    break;
                }
                executor.timer(Duration::from_secs(2)).await;
            }
        }));
    }
}

impl Drop for ResourceMonitor {
    /// Stop owned probes when the workspace releases the sidebar entity.
    fn drop(&mut self) {
        self.stop_polling();
    }
}

impl Render for ResourceMonitor {
    /// Render prepared values and semantic operation states, never blocking for telemetry.
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_panel(cx)
    }
}
