//! The agent's task list: what it means to do, in what order, and what each
//! step waits for.
//!
//! The list is flat and every task carries an optional `parent`, so subtasks
//! are ordinary tasks pointing at the task they belong to, and `needs` edges
//! record which tasks have to finish first. Nothing here schedules anything:
//! the model still chooses what to work on. The plan only refuses the moves
//! that are plainly wrong — starting a task whose dependencies are unfinished,
//! finishing a parent whose children are not — so a long plan cannot quietly
//! drift out of order while the model is busy elsewhere.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    #[default]
    Todo,
    Doing,
    Done,
    Dropped,
}

impl Status {
    /// The names other harnesses use, so a model trained on them is understood.
    pub fn parse(s: &str) -> Option<Status> {
        match s.trim().to_ascii_lowercase().as_str() {
            "todo" | "pending" | "open" | "not_started" => Some(Status::Todo),
            "doing" | "in_progress" | "started" | "active" => Some(Status::Doing),
            "done" | "completed" | "complete" | "finished" => Some(Status::Done),
            "dropped" | "cancelled" | "canceled" | "skipped" => Some(Status::Dropped),
            _ => None,
        }
    }

    /// True when this task no longer blocks the ones that wait for it.
    fn settled(self) -> bool {
        matches!(self, Status::Done | Status::Dropped)
    }

    pub fn box_(self) -> &'static str {
        match self {
            Status::Todo => "[ ]",
            Status::Doing => "[>]",
            Status::Done => "[x]",
            Status::Dropped => "[-]",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Status::Todo => "todo",
            Status::Doing => "doing",
            Status::Done => "done",
            Status::Dropped => "dropped",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub id: u16,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<u16>,
    /// Tasks that must be done (or dropped) before this one may start.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub needs: Vec<u16>,
    #[serde(default)]
    pub status: Status,
    /// A short line about the outcome or the blocker.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

/// A task as the model submits it: ids are assigned by the plan.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct NewTask {
    pub title: String,
    #[serde(default)]
    pub parent: Option<u16>,
    #[serde(default)]
    pub needs: Vec<u16>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub tasks: Vec<Task>,
    next_id: u16,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    pub fn get(&self, id: u16) -> Option<&Task> {
        self.tasks.iter().find(|t| t.id == id)
    }

    fn get_mut(&mut self, id: u16) -> Option<&mut Task> {
        self.tasks.iter_mut().find(|t| t.id == id)
    }

    /// `(done or dropped, total)`.
    pub fn counts(&self) -> (usize, usize) {
        (
            self.tasks.iter().filter(|t| t.status.settled()).count(),
            self.tasks.len(),
        )
    }

    fn children(&self, id: u16) -> Vec<u16> {
        self.tasks
            .iter()
            .filter(|t| t.parent == Some(id))
            .map(|t| t.id)
            .collect()
    }

    /// Everything under `id`, however deep. Dropping a task drops the work
    /// below it, and that work is not always one level down.
    fn descendants(&self, id: u16) -> Vec<u16> {
        let mut out = Vec::new();
        let mut frontier = self.children(id);
        let mut depth = 0;
        while !frontier.is_empty() && depth <= 8 {
            let mut next = Vec::new();
            for c in frontier {
                if c == id || out.contains(&c) {
                    continue;
                }
                out.push(c);
                next.extend(self.children(c));
            }
            frontier = next;
            depth += 1;
        }
        out
    }

    /// Dependencies of `id` that are not finished yet.
    pub fn waiting_for(&self, id: u16) -> Vec<u16> {
        let Some(t) = self.get(id) else {
            return Vec::new();
        };
        t.needs
            .iter()
            .copied()
            .filter(|n| self.get(*n).is_some_and(|d| !d.status.settled()))
            .collect()
    }

    /// The first task that could be started now: nothing unfinished before it.
    pub fn next_ready(&self) -> Option<&Task> {
        self.tasks
            .iter()
            .find(|t| t.status == Status::Todo && self.waiting_for(t.id).is_empty())
    }

    pub fn doing(&self) -> Vec<&Task> {
        self.tasks
            .iter()
            .filter(|t| t.status == Status::Doing)
            .collect()
    }

    /// Replace the whole plan. Ids are handed out in submission order, so a
    /// `needs` or `parent` of 2 means the second task in the list.
    pub fn set(&mut self, items: &[NewTask]) -> Result<(), String> {
        let mut tasks = Vec::with_capacity(items.len());
        for (i, item) in items.iter().enumerate() {
            tasks.push(build(i as u16 + 1, item)?);
        }
        let plan = Plan {
            next_id: tasks.len() as u16 + 1,
            tasks,
        };
        plan.check()?;
        *self = plan;
        Ok(())
    }

    /// Append tasks, keeping the ids already in the plan.
    pub fn add(&mut self, items: &[NewTask]) -> Result<Vec<u16>, String> {
        if self.next_id == 0 {
            self.next_id = 1;
        }
        let mut plan = self.clone();
        let mut ids = Vec::with_capacity(items.len());
        for item in items {
            let id = plan.next_id;
            plan.next_id += 1;
            plan.tasks.push(build(id, item)?);
            ids.push(id);
        }
        plan.check()?;
        *self = plan;
        Ok(ids)
    }

    /// Move tasks to `status`. `note` is attached to each of them. What comes
    /// back are the tasks this actually moved, the subtasks a drop took with
    /// it included — empty when every one of them was already there.
    pub fn set_status(
        &mut self,
        ids: &[u16],
        status: Status,
        note: &str,
    ) -> Result<Vec<u16>, String> {
        for id in ids {
            if self.get(*id).is_none() {
                return Err(format!("no task {id}"));
            }
        }
        for id in ids {
            if status == Status::Doing {
                let waiting = self.waiting_for(*id);
                if !waiting.is_empty() {
                    return Err(format!(
                        "task {id} waits for {}; finish or drop {} first",
                        list(&waiting),
                        if waiting.len() == 1 { "it" } else { "them" }
                    ));
                }
            }
            if status == Status::Done {
                // A subtask finished in this same call is not an open one:
                // closing a parent and its children together is one move.
                let open: Vec<u16> = self
                    .children(*id)
                    .into_iter()
                    .filter(|c| !ids.contains(c))
                    .filter(|c| self.get(*c).is_some_and(|t| !t.status.settled()))
                    .collect();
                if !open.is_empty() {
                    return Err(format!(
                        "task {id} still has open subtasks: {}",
                        list(&open)
                    ));
                }
            }
        }
        let mut changed = Vec::new();
        for id in ids {
            let dropping = status == Status::Dropped;
            let kids = if dropping {
                self.descendants(*id)
            } else {
                vec![]
            };
            if let Some(t) = self.get_mut(*id) {
                let moved = t.status != status || (!note.is_empty() && t.note != note);
                t.status = status;
                if !note.is_empty() {
                    t.note = note.to_string();
                }
                if moved && !changed.contains(id) {
                    changed.push(*id);
                }
            }
            for c in kids {
                if let Some(t) = self.get_mut(c)
                    && !t.status.settled()
                {
                    t.status = Status::Dropped;
                    if !changed.contains(&c) {
                        changed.push(c);
                    }
                }
            }
        }
        Ok(changed)
    }

    /// Change one task in place. `None` leaves a field alone; the answer says
    /// whether any of it landed on something different from what was there.
    pub fn update(
        &mut self,
        id: u16,
        title: Option<&str>,
        parent: Option<Option<u16>>,
        needs: Option<Vec<u16>>,
        note: Option<&str>,
    ) -> Result<bool, String> {
        let mut plan = self.clone();
        let Some(t) = plan.get_mut(id) else {
            return Err(format!("no task {id}"));
        };
        if let Some(title) = title {
            if title.trim().is_empty() {
                return Err("title is empty".into());
            }
            t.title = title.trim().to_string();
        }
        if let Some(p) = parent {
            t.parent = p;
        }
        if let Some(n) = needs {
            t.needs = n;
        }
        if let Some(n) = note {
            t.note = n.trim().to_string();
        }
        plan.check()?;
        let changed = plan != *self;
        *self = plan;
        Ok(changed)
    }

    /// Every id refers to a real task, and neither parents nor dependencies
    /// form a loop.
    fn check(&self) -> Result<(), String> {
        for t in &self.tasks {
            if let Some(p) = t.parent {
                if p == t.id {
                    return Err(format!("task {} cannot be its own parent", t.id));
                }
                if self.get(p).is_none() {
                    return Err(format!("task {}: no parent {p}", t.id));
                }
            }
            for n in &t.needs {
                if *n == t.id {
                    return Err(format!("task {} cannot wait for itself", t.id));
                }
                if self.get(*n).is_none() {
                    return Err(format!("task {}: no task {n} to wait for", t.id));
                }
            }
        }
        for t in &self.tasks {
            if let Some(cycle) = self.cycle_from(t.id, |x| x.parent.into_iter().collect()) {
                return Err(format!("parents loop: {}", list(&cycle)));
            }
            if let Some(cycle) = self.cycle_from(t.id, |x| x.needs.clone()) {
                return Err(format!("dependencies loop: {}", list(&cycle)));
            }
        }
        Ok(())
    }

    /// Walks `edges` from `start`; returns the visited ids if it comes back.
    fn cycle_from(&self, start: u16, edges: impl Fn(&Task) -> Vec<u16>) -> Option<Vec<u16>> {
        let mut seen = vec![start];
        let mut frontier = edges(self.get(start)?);
        while let Some(id) = frontier.pop() {
            if id == start {
                seen.push(id);
                return Some(seen);
            }
            if seen.contains(&id) {
                continue;
            }
            seen.push(id);
            if let Some(t) = self.get(id) {
                frontier.extend(edges(t));
            }
        }
        None
    }

    /// One line: how far along, what is in flight, what to pick up next.
    pub fn summary(&self) -> String {
        if self.tasks.is_empty() {
            return "plan · empty".into();
        }
        let (done, total) = self.counts();
        let mut s = format!("plan · {done}/{total} done");
        let doing = self.doing();
        if let Some(t) = doing.first() {
            s.push_str(&format!(" · doing: {} {}", t.id, t.title));
            if doing.len() > 1 {
                s.push_str(&format!(" (+{})", doing.len() - 1));
            }
        }
        if done == total {
            s.push_str(" · complete");
        } else if doing.is_empty() {
            match self.next_ready() {
                Some(t) => s.push_str(&format!(" · next: {} {}", t.id, t.title)),
                None => s.push_str(" · nothing ready: every task waits for another"),
            }
        }
        s
    }

    /// The summary followed by the tasks, subtasks under their parent.
    pub fn render(&self) -> String {
        if self.tasks.is_empty() {
            return "plan · empty; set one with the plan tool".into();
        }
        let mut out = self.summary();
        for (id, depth) in self.ordered() {
            out.push('\n');
            out.push_str(&self.line(id, depth));
        }
        out
    }

    /// The summary followed by only the tasks named. A reply about a move
    /// that touched two tasks has no reason to spell out the other twenty:
    /// the summary already says how far the work has got and what is next.
    /// Order and depth come from the whole plan, so a line reads the same
    /// here as it does in `render`.
    pub fn render_some(&self, ids: &[u16]) -> String {
        if self.tasks.is_empty() {
            return "plan · empty; set one with the plan tool".into();
        }
        let mut out = self.summary();
        for (id, depth) in self.ordered() {
            if ids.contains(&id) {
                out.push('\n');
                out.push_str(&self.line(id, depth));
            }
        }
        out
    }

    /// Task ids in reading order, each with how deep it sits.
    pub fn ordered(&self) -> Vec<(u16, usize)> {
        let mut out = Vec::with_capacity(self.tasks.len());
        let roots: Vec<u16> = self
            .tasks
            .iter()
            .filter(|t| t.parent.is_none_or(|p| self.get(p).is_none()))
            .map(|t| t.id)
            .collect();
        for id in roots {
            self.walk(&mut out, id, 0);
        }
        out
    }

    fn walk(&self, out: &mut Vec<(u16, usize)>, id: u16, depth: usize) {
        if depth > 8 || out.iter().any(|(x, _)| *x == id) {
            return;
        }
        out.push((id, depth));
        for c in self.children(id) {
            self.walk(out, c, depth + 1);
        }
    }

    /// One task as a line: id, box, title, then what holds it up or how it went.
    pub fn line(&self, id: u16, depth: usize) -> String {
        let Some(t) = self.get(id) else {
            return String::new();
        };
        let mut s = format!(
            "{:>3} {}{} {}",
            t.id,
            "  ".repeat(depth),
            t.status.box_(),
            t.title
        );
        let waiting = self.waiting_for(id);
        if !waiting.is_empty() && t.status == Status::Todo {
            s.push_str(&format!(" · waits for {}", list(&waiting)));
        }
        if !t.note.is_empty() {
            s.push_str(&format!(" · {}", t.note));
        }
        s
    }
}

fn build(id: u16, item: &NewTask) -> Result<Task, String> {
    let title = item.title.trim();
    if title.is_empty() {
        return Err(format!("task {id}: title is empty"));
    }
    let status = match item.status.as_deref() {
        None | Some("") => Status::Todo,
        Some(s) => Status::parse(s).ok_or_else(|| {
            format!("task {id}: unknown status `{s}`; use todo, doing, done or dropped")
        })?,
    };
    Ok(Task {
        id,
        title: title.to_string(),
        parent: item.parent.filter(|p| *p != 0),
        needs: item.needs.iter().copied().filter(|n| *n != 0).collect(),
        status,
        note: item.note.trim().to_string(),
    })
}

fn list(ids: &[u16]) -> String {
    ids.iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

type Waker = Box<dyn Fn() + Send + Sync>;

/// The plan for one session, shared by the tool, the loop and the UI.
pub struct Store {
    plan: Mutex<Plan>,
    /// Bumped on every change; the loop uses it to decide when to remind the
    /// model of the plan, and the UI to decide when to redraw.
    version: AtomicU64,
    waker: Mutex<Option<Waker>>,
}

thread_local! {
    /// Which session's plan this thread means. Empty for a process running one
    /// session, which is every window.
    static CURRENT: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

/// Say which session's plan this thread is working on.
///
/// Called once on each engine thread, and once on the UI thread of a window.
/// The daemon runs several engines in one process and they had one plan
/// between them, so a session could overwrite another's tasks and the model
/// was told about work it had never been given.
///
/// A thread-local rather than an argument because the plan is read from
/// sixteen places, most of them drawing code. It does not reach threads spawned
/// underneath — the plan tool never runs on one, being barred from subagents
/// and not read-only, so nothing that touches the plan runs anywhere else.
pub fn use_session(id: &str) {
    CURRENT.with(|c| *c.borrow_mut() = id.to_string());
}

/// The plan for whatever session this thread is working on.
pub fn store() -> &'static Store {
    CURRENT.with(|c| store_for(&c.borrow()))
}

/// The plan for a named session, made the first time it is asked for.
///
/// Leaked on purpose: one per session, at most `remote.max_sessions` of them,
/// and every caller wants a `'static` reference into drawing code that
/// outlives any borrow this could hand out instead.
pub fn store_for(session: &str) -> &'static Store {
    static STORES: OnceLock<Mutex<std::collections::HashMap<String, &'static Store>>> =
        OnceLock::new();
    let mut stores = STORES
        .get_or_init(|| Mutex::new(std::collections::HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(found) = stores.get(session) {
        return found;
    }
    let made: &'static Store = Box::leak(Box::new(Store {
        plan: Mutex::new(Plan::default()),
        version: AtomicU64::new(0),
        waker: Mutex::new(None),
    }));
    stores.insert(session.to_string(), made);
    made
}

impl Store {
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Relaxed)
    }

    /// A copy of the plan, for whoever needs to keep one. Every caller that
    /// only means to read it wants `with` instead.
    pub fn snapshot(&self) -> Plan {
        self.plan.lock().unwrap().clone()
    }

    /// Read the plan where it lies. The drawing code runs this on every
    /// frame, so it copies nothing.
    pub fn with<T>(&self, f: impl FnOnce(&Plan) -> T) -> T {
        f(&self.plan.lock().unwrap())
    }

    pub fn is_empty(&self) -> bool {
        self.plan.lock().unwrap().is_empty()
    }

    /// Run `f` on the plan. It answers with its result and whether the plan
    /// moved; a move bumps the version and wakes the UI. The mutators know
    /// what they did, so nothing here has to copy the plan to find out.
    pub fn edit<T>(
        &self,
        f: impl FnOnce(&mut Plan) -> Result<(T, bool), String>,
    ) -> Result<T, String> {
        let mut guard = self.plan.lock().unwrap();
        let out = f(&mut guard);
        drop(guard);
        match out {
            Ok((value, changed)) => {
                if changed {
                    self.version.fetch_add(1, Ordering::Relaxed);
                    self.wake();
                }
                Ok(value)
            }
            Err(e) => Err(e),
        }
    }

    /// Replace the plan wholesale, as when a session is resumed.
    pub fn load(&self, plan: Plan) {
        *self.plan.lock().unwrap() = plan;
        self.version.fetch_add(1, Ordering::Relaxed);
        self.wake();
    }

    pub fn clear(&self) {
        self.load(Plan::default());
    }

    pub fn set_waker(&self, waker: Waker) {
        *self.waker.lock().unwrap() = Some(waker);
    }

    fn wake(&self) {
        if let Some(w) = self.waker.lock().unwrap().as_ref() {
            w();
        }
    }
}

/// Tests share one process-wide plan; this keeps them out of each other's way.
#[cfg(test)]
mod stores {
    use super::*;

    #[test]
    fn two_sessions_do_not_share_one_plan() {
        // The daemon runs several engines in one process. Sharing a store
        // meant one session could overwrite another's tasks, and the model be
        // reminded of work it had never been given.
        let alpha = store_for("alpha");
        let beta = store_for("beta");
        alpha.load(Plan {
            tasks: vec![Task {
                id: 1,
                title: "alpha's work".into(),
                parent: None,
                needs: Vec::new(),
                status: Status::default(),
                note: String::new(),
            }],
            ..Default::default()
        });
        assert_eq!(alpha.with(|p| p.tasks.len()), 1);
        assert_eq!(beta.with(|p| p.tasks.len()), 0, "beta saw alpha's plan");
        // And asking again is the same store, not a fresh one.
        assert_eq!(store_for("alpha").with(|p| p.tasks.len()), 1);
    }

    #[test]
    fn a_thread_that_named_no_session_gets_the_shared_one() {
        // Which is every window: one engine, one UI thread, one plan.
        assert!(std::ptr::eq(store(), store_for("")));
    }
}

#[cfg(test)]
pub(crate) fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    store().clear();
    guard
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new(title: &str, parent: Option<u16>, needs: &[u16]) -> NewTask {
        NewTask {
            title: title.into(),
            parent,
            needs: needs.to_vec(),
            ..NewTask::default()
        }
    }

    fn plan() -> Plan {
        let mut p = Plan::default();
        p.set(&[
            new("api", None, &[]),
            new("routes", Some(1), &[]),
            new("handlers", Some(1), &[]),
            new("tests", None, &[1]),
        ])
        .unwrap();
        p
    }

    #[test]
    fn a_task_cannot_start_before_what_it_waits_for() {
        let mut p = plan();
        let e = p.set_status(&[4], Status::Doing, "").unwrap_err();
        assert!(e.contains("waits for 1"), "{e}");
        p.set_status(&[2, 3], Status::Done, "").unwrap();
        p.set_status(&[1], Status::Done, "").unwrap();
        p.set_status(&[4], Status::Doing, "").unwrap();
        assert_eq!(p.get(4).unwrap().status, Status::Doing);
    }

    #[test]
    fn a_parent_waits_for_its_subtasks() {
        let mut p = plan();
        let e = p.set_status(&[1], Status::Done, "").unwrap_err();
        assert!(e.contains("open subtasks: 2, 3"), "{e}");
        p.set_status(&[2], Status::Done, "shipped").unwrap();
        p.set_status(&[3], Status::Dropped, "not needed").unwrap();
        p.set_status(&[1], Status::Done, "").unwrap();
        assert_eq!(p.counts(), (3, 4));
    }

    #[test]
    fn dropping_a_task_drops_its_subtasks() {
        let mut p = plan();
        p.set_status(&[1], Status::Dropped, "cut from scope")
            .unwrap();
        assert_eq!(p.get(2).unwrap().status, Status::Dropped);
        assert_eq!(p.get(3).unwrap().status, Status::Dropped);
        // A dropped dependency no longer blocks what waited for it.
        assert!(p.waiting_for(4).is_empty());
        assert_eq!(p.next_ready().map(|t| t.id), Some(4));
    }

    #[test]
    fn dropping_a_task_drops_the_work_below_it() {
        let mut p = Plan::default();
        p.set(&[
            new("api", None, &[]),
            new("routes", Some(1), &[]),
            new("the get", Some(2), &[]),
            new("the post", Some(3), &[]),
        ])
        .unwrap();
        assert_eq!(
            p.set_status(&[1], Status::Dropped, "cut").unwrap(),
            vec![1, 2, 3, 4]
        );
        assert!(p.tasks.iter().all(|t| t.status == Status::Dropped));
    }

    #[test]
    fn a_parent_and_its_subtasks_finish_in_one_call() {
        let mut p = plan();
        assert_eq!(
            p.set_status(&[1, 2, 3], Status::Done, "").unwrap(),
            vec![1, 2, 3]
        );
        assert_eq!(p.counts(), (3, 4));
    }

    #[test]
    fn moving_a_task_where_it_already_is_moves_nothing() {
        let mut p = plan();
        assert_eq!(p.set_status(&[2], Status::Done, "").unwrap(), vec![2]);
        assert!(p.set_status(&[2], Status::Done, "").unwrap().is_empty());
        // A note is a change of its own, even when the status is not.
        assert_eq!(
            p.set_status(&[2], Status::Done, "12 routes").unwrap(),
            vec![2]
        );
        assert!(!p.update(2, Some("routes"), None, None, None).unwrap());
        assert!(p.update(2, Some("the routes"), None, None, None).unwrap());
    }

    #[test]
    fn a_reply_about_two_tasks_leaves_the_others_out() {
        let mut p = plan();
        p.set_status(&[2], Status::Done, "12 routes").unwrap();
        assert_eq!(
            p.render_some(&[2, 4]),
            "plan · 1/4 done · next: 1 api\n\
             \x20 2   [x] routes · 12 routes\n\
             \x20 4 [ ] tests · waits for 1"
        );
    }

    #[test]
    fn loops_and_dangling_ids_are_refused() {
        let mut p = Plan::default();
        assert!(
            p.set(&[new("a", None, &[2])])
                .unwrap_err()
                .contains("no task 2")
        );
        assert!(
            p.set(&[new("a", None, &[2]), new("b", None, &[1])])
                .unwrap_err()
                .contains("dependencies loop")
        );
        p.set(&[new("a", None, &[]), new("b", Some(1), &[])])
            .unwrap();
        assert!(
            p.update(1, None, Some(Some(2)), None, None)
                .unwrap_err()
                .contains("parents loop")
        );
    }

    #[test]
    fn adding_keeps_the_ids_already_handed_out() {
        let mut p = plan();
        p.set_status(&[2], Status::Done, "").unwrap();
        let ids = p.add(&[new("docs", None, &[4])]).unwrap();
        assert_eq!(ids, vec![5]);
        assert_eq!(p.get(2).unwrap().status, Status::Done);
        assert_eq!(p.get(5).unwrap().needs, vec![4]);
    }

    #[test]
    fn the_render_shows_the_shape_of_the_work() {
        let mut p = plan();
        p.set_status(&[2], Status::Done, "12 routes").unwrap();
        p.set_status(&[3], Status::Doing, "").unwrap();
        let text = p.render();
        assert_eq!(
            text,
            "plan · 1/4 done · doing: 3 handlers\n\
             \x20 1 [ ] api\n\
             \x20 2   [x] routes · 12 routes\n\
             \x20 3   [>] handlers\n\
             \x20 4 [ ] tests · waits for 1"
        );
    }

    #[test]
    fn statuses_other_harnesses_use_are_understood() {
        assert_eq!(Status::parse("in_progress"), Some(Status::Doing));
        assert_eq!(Status::parse("Completed"), Some(Status::Done));
        assert_eq!(Status::parse("cancelled"), Some(Status::Dropped));
        assert_eq!(Status::parse("nope"), None);
    }

    #[test]
    fn the_store_bumps_its_version_only_when_something_changed() {
        let s = Store {
            plan: Mutex::new(Plan::default()),
            version: AtomicU64::new(0),
            waker: Mutex::new(None),
        };
        s.edit(|p| Ok((p.set(&[new("a", None, &[])])?, true)))
            .unwrap();
        let v = s.version();
        assert!(
            s.edit(|p| {
                let moved = p.set_status(&[9], Status::Done, "")?;
                Ok((moved, true))
            })
            .is_err()
        );
        assert_eq!(s.version(), v);
        let done = |s: &Store| {
            s.edit(|p| {
                let moved = p.set_status(&[1], Status::Done, "")?;
                let changed = !moved.is_empty();
                Ok((moved, changed))
            })
            .unwrap()
        };
        assert_eq!(done(&s), vec![1]);
        assert_eq!(s.version(), v + 1);
        // Finishing a finished task moves nothing, so nothing redraws.
        assert!(done(&s).is_empty());
        assert_eq!(s.version(), v + 1);
    }
}
