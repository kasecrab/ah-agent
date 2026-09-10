//! Background shell jobs.
//!
//! Every `bash` call goes through this table. A foreground call spawns a job
//! and waits for it; a background call, or one that outruns its timeout,
//! leaves the job in the table for the `jobs` tool and the TUI to look at.
//!
//! Nothing here polls. Two threads per job block in `read`, so an idle job
//! costs no CPU, and the thread that sees the last pipe close reaps the child
//! and wakes anyone waiting on the condition variable.

use std::collections::VecDeque;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Who has already been told that a job finished. A job belongs to one agent —
/// the main one is 0, a subagent its own id — so both audiences carry whose
/// news it is, and one bit each is enough for all of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Audience {
    Model(u32),
    /// The screen, for that agent's own view: the main conversation is 0.
    Ui(u32),
}

impl Audience {
    fn bit(self) -> u32 {
        match self {
            Audience::Model(_) => 1,
            Audience::Ui(_) => 2,
        }
    }

    /// Whether this job is any of that audience's business.
    fn covers(self, job: &Job) -> bool {
        match self {
            Audience::Model(owner) | Audience::Ui(owner) => job.owner() == owner,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Running,
    /// Exit code, or -1 when the process was killed by a signal.
    Done(i32),
}

/// Ring of output lines: the first lines of the job, then the most recent ones,
/// with whatever fell out in between counted.
struct Output {
    head: Vec<String>,
    head_bytes: usize,
    tail: VecDeque<String>,
    tail_bytes: usize,
    partial: String,
    /// Line number of the front of `tail`; head lines are 0..head.len().
    tail_start: u64,
    lines: u64,
    dropped: u64,
    cap: usize,
}

impl Output {
    fn new(cap: usize) -> Self {
        Self {
            head: Vec::new(),
            head_bytes: 0,
            tail: VecDeque::new(),
            tail_bytes: 0,
            partial: String::new(),
            tail_start: 0,
            lines: 0,
            dropped: 0,
            cap: cap.max(4096),
        }
    }

    fn push_line(&mut self, line: String) {
        self.lines += 1;
        if self.head_bytes + line.len() <= self.cap / 3 {
            self.head_bytes += line.len() + 1;
            self.head.push(line);
            self.tail_start = self.head.len() as u64;
            return;
        }
        self.tail_bytes += line.len() + 1;
        self.tail.push_back(line);
        while self.tail_bytes > self.cap - self.cap / 3 {
            match self.tail.pop_front() {
                Some(l) => {
                    self.tail_bytes -= l.len() + 1;
                    self.tail_start += 1;
                    self.dropped += 1;
                }
                None => break,
            }
        }
    }

    fn push_bytes(&mut self, bytes: &[u8]) {
        self.partial.push_str(&String::from_utf8_lossy(bytes));
        while let Some(i) = self.partial.find('\n') {
            let line: String = self.partial.drain(..=i).collect();
            self.push_line(line.trim_end_matches(['\n', '\r']).to_string());
        }
        // A line that never ends would grow without bound.
        if self.partial.len() > self.cap {
            let keep = self.partial.split_off(self.partial.len() - self.cap / 2);
            self.partial = keep;
        }
    }

    /// Lines numbered `from` onwards, at most `max`, plus the next line number.
    fn view(&self, from: u64, max: usize) -> (Vec<String>, u64) {
        let mut out = Vec::new();
        let mut n = from;
        while out.len() < max && n < self.lines {
            match self.line(n) {
                Some(l) => out.push(l.to_string()),
                None => n = self.tail_start.max(n + 1) - 1,
            }
            n += 1;
        }
        if !self.partial.is_empty() && out.len() < max && n >= self.lines {
            out.push(self.partial.clone());
        }
        (out, n)
    }

    fn line(&self, n: u64) -> Option<&str> {
        if (n as usize) < self.head.len() {
            return Some(&self.head[n as usize]);
        }
        let i = n.checked_sub(self.tail_start)? as usize;
        self.tail.get(i).map(String::as_str)
    }
}

pub struct Job {
    pub id: u32,
    pub command: String,
    pub started: Instant,
    /// The agent that started it: 0 for the main one, else a subagent id. A
    /// job outliving its agent is handed back to 0 rather than left unheard.
    owner: AtomicU32,
    pid: i32,
    out: Mutex<Output>,
    state: Mutex<State>,
    done: Condvar,
    /// Bumped on every write, so a view can tell "changed" without locking.
    version: AtomicU64,
    reported: AtomicU32,
    /// Whether the end of this job is news. A command run in the foreground is
    /// reported by the call that ran it, so it announces nothing; one started
    /// in the background, or moved there when it outran its timeout, has
    /// nobody waiting on it and says so when it ends.
    announce: AtomicBool,
    child: Mutex<Option<Child>>,
    open_pipes: AtomicU32,
    duration_ms: AtomicU64,
}

impl Job {
    pub fn state(&self) -> State {
        *self.state.lock().unwrap()
    }

    /// Nobody is waiting on this one any more: when it ends, say so.
    pub fn announce(&self) {
        self.announce.store(true, Ordering::Relaxed);
    }

    pub fn owner(&self) -> u32 {
        self.owner.load(Ordering::Relaxed)
    }

    /// Hand the job to another agent, and let that one hear about it even if
    /// the first already did.
    pub fn reparent(&self, to: u32) {
        self.owner.store(to, Ordering::Relaxed);
        self.reported
            .fetch_and(!Audience::Model(to).bit(), Ordering::SeqCst);
    }

    pub fn running(&self) -> bool {
        self.state() == State::Running
    }

    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Relaxed)
    }

    pub fn duration(&self) -> Duration {
        match self.state() {
            State::Running => self.started.elapsed(),
            State::Done(_) => Duration::from_millis(self.duration_ms.load(Ordering::Relaxed)),
        }
    }

    /// Total lines written so far, and how many of the oldest were dropped.
    pub fn counts(&self) -> (u64, u64) {
        let o = self.out.lock().unwrap();
        (o.lines, o.dropped)
    }

    /// Lines from `from` on, at most `max`; returns the next line number.
    pub fn view(&self, from: u64, max: usize) -> (Vec<String>, u64) {
        self.out.lock().unwrap().view(from, max)
    }

    /// The last `max` lines.
    pub fn tail(&self, max: usize) -> (Vec<String>, u64) {
        let o = self.out.lock().unwrap();
        let from = o.lines.saturating_sub(max as u64);
        o.view(from, max)
    }

    /// Everything kept, with a marker where lines were dropped.
    pub fn text(&self) -> String {
        let o = self.out.lock().unwrap();
        let mut parts: Vec<String> = o.head.clone();
        if o.dropped > 0 {
            parts.push(format!("… [{} lines dropped] …", o.dropped));
        }
        parts.extend(o.tail.iter().cloned());
        if !o.partial.is_empty() {
            parts.push(o.partial.clone());
        }
        parts.join("\n")
    }

    /// Block until the job ends or `timeout` passes. True when it ended.
    pub fn wait(&self, timeout: Duration) -> bool {
        let state = self.state.lock().unwrap();
        let (_state, r) = self
            .done
            .wait_timeout_while(state, timeout, |s| *s == State::Running)
            .unwrap();
        !r.timed_out()
    }

    /// Wait, but look at `cancel` every 40 ms. Returns `true` if the job
    /// ended, `false` if the wait ran out or the turn was cancelled.
    pub fn wait_while(&self, timeout: Duration, cancel: &std::sync::atomic::AtomicBool) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return false;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            if self.wait(left.min(Duration::from_millis(40))) {
                return true;
            }
        }
    }

    /// Ask the process group to stop, then make sure of it after `grace`.
    pub fn kill(&self, grace: Duration) {
        if !self.running() {
            return;
        }
        signal(self.pid, SIGTERM);
        let pid = self.pid;
        std::thread::spawn(move || {
            std::thread::sleep(grace);
            signal(pid, SIGKILL);
        });
    }

    /// The polite half of [`Job::kill`], for a caller that does the waiting
    /// itself. Used at exit, where a detached thread would not outlive the
    /// process long enough to fire.
    fn term(&self) {
        if self.running() {
            signal(self.pid, SIGTERM);
        }
    }

    fn hard_kill(&self) {
        if self.running() {
            signal(self.pid, SIGKILL);
        }
    }

    /// True the first time this audience is told the job finished.
    fn claim(&self, who: Audience) -> bool {
        !self.running()
            && self.announce.load(Ordering::Relaxed)
            && self.reported.fetch_or(who.bit(), Ordering::SeqCst) & who.bit() == 0
    }

    fn finish(&self, code: i32) {
        self.duration_ms
            .store(self.started.elapsed().as_millis() as u64, Ordering::Relaxed);
        *self.state.lock().unwrap() = State::Done(code);
        self.version.fetch_add(1, Ordering::Relaxed);
        self.done.notify_all();
    }

    pub fn summary(&self) -> String {
        let (lines, _) = self.counts();
        let secs = self.duration().as_secs_f32();
        match self.state() {
            State::Running => format!("job {} running {secs:.0}s · {lines} lines", self.id),
            State::Done(code) => format!(
                "job {} exited {code} after {secs:.1}s · {lines} lines",
                self.id
            ),
        }
    }
}

type Waker = Box<dyn Fn() + Send + Sync>;

pub struct Jobs {
    jobs: Mutex<Vec<std::sync::Arc<Job>>>,
    next_id: AtomicU32,
    /// Finished jobs kept for inspection.
    keep: usize,
    waker: Mutex<Option<Waker>>,
    /// A wake is already on its way; more output need not send another.
    pending: std::sync::atomic::AtomicBool,
}

/// The table for this process. Jobs are children of `ah`, so there is one.
pub fn table() -> &'static Jobs {
    static JOBS: OnceLock<Jobs> = OnceLock::new();
    JOBS.get_or_init(|| Jobs {
        jobs: Mutex::new(Vec::new()),
        next_id: AtomicU32::new(1),
        keep: 32,
        waker: Mutex::new(None),
        pending: std::sync::atomic::AtomicBool::new(false),
    })
}

impl Jobs {
    /// Start `command` under `shell` and wire its pipes into a job.
    pub fn spawn(
        &self,
        shell: &str,
        command: &str,
        cwd: &std::path::Path,
        buffer_bytes: usize,
        owner: u32,
    ) -> std::io::Result<std::sync::Arc<Job>> {
        let mut cmd = Command::new(shell);
        cmd.arg("-c")
            .arg(command)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            // own process group, so a kill reaches the whole tree
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let mut child = cmd.spawn()?;
        let pid = child.id() as i32;
        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");
        let job = std::sync::Arc::new(Job {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            command: command.to_string(),
            started: Instant::now(),
            owner: AtomicU32::new(owner),
            pid,
            out: Mutex::new(Output::new(buffer_bytes)),
            state: Mutex::new(State::Running),
            done: Condvar::new(),
            version: AtomicU64::new(0),
            reported: AtomicU32::new(0),
            announce: AtomicBool::new(false),
            child: Mutex::new(Some(child)),
            open_pipes: AtomicU32::new(2),
            duration_ms: AtomicU64::new(0),
        });
        pump(job.clone(), stdout);
        pump(job.clone(), stderr);
        let mut jobs = self.jobs.lock().unwrap();
        jobs.push(job.clone());
        self.evict(&mut jobs);
        Ok(job)
    }

    /// Drop the oldest finished jobs once there are too many.
    fn evict(&self, jobs: &mut Vec<std::sync::Arc<Job>>) {
        let finished = jobs.iter().filter(|j| !j.running()).count();
        if finished <= self.keep {
            return;
        }
        let mut over = finished - self.keep;
        // Only once the owning model has heard: the UI reading the news must
        // not throw away a job the agent it belongs to has yet to see.
        let heard = Audience::Model(0).bit();
        jobs.retain(|j| {
            if over > 0 && !j.running() && j.reported.load(Ordering::Relaxed) & heard != 0 {
                over -= 1;
                return false;
            }
            true
        });
    }

    /// Take a job out of the table; foreground calls do this once they finish.
    pub fn remove(&self, id: u32) {
        self.jobs.lock().unwrap().retain(|j| j.id != id);
    }

    pub fn get(&self, id: u32) -> Option<std::sync::Arc<Job>> {
        self.jobs
            .lock()
            .unwrap()
            .iter()
            .find(|j| j.id == id)
            .cloned()
    }

    /// The job as far as one agent is concerned: another agent's job is not
    /// there at all, so nobody reads or stops a command they did not start.
    pub fn get_owned(&self, id: u32, owner: u32) -> Option<std::sync::Arc<Job>> {
        self.get(id).filter(|j| j.owner() == owner)
    }

    pub fn all(&self) -> Vec<std::sync::Arc<Job>> {
        self.jobs.lock().unwrap().clone()
    }

    pub fn owned_by(&self, owner: u32) -> Vec<std::sync::Arc<Job>> {
        self.jobs
            .lock()
            .unwrap()
            .iter()
            .filter(|j| j.owner() == owner)
            .cloned()
            .collect()
    }

    /// Hand what is left of `owner` to `to`. A command still running when its
    /// agent ends goes to the one above, which hears about it when it ends;
    /// one that already finished was that agent's business and is dropped,
    /// rather than announced twice to somebody who never ran it.
    pub fn reparent_all(&self, owner: u32, to: u32) {
        let mut jobs = self.jobs.lock().unwrap();
        jobs.retain(|j| j.owner() != owner || j.running());
        for j in jobs.iter() {
            if j.owner() == owner {
                j.reparent(to);
            }
        }
    }

    pub fn running(&self) -> usize {
        self.jobs
            .lock()
            .unwrap()
            .iter()
            .filter(|j| j.running())
            .count()
    }

    /// One line per job that finished since this audience last asked, and that
    /// belongs to it.
    pub fn notices(&self, who: Audience) -> Vec<String> {
        self.jobs
            .lock()
            .unwrap()
            .iter()
            .filter(|j| who.covers(j) && j.claim(who))
            .map(|j| j.summary())
            .collect()
    }

    /// True when a job has ended that this audience has not been told about,
    /// without claiming the news.
    pub fn unheard(&self, who: Audience) -> bool {
        self.jobs.lock().unwrap().iter().any(|j| {
            who.covers(j)
                && !j.running()
                && j.announce.load(Ordering::Relaxed)
                && j.reported.load(Ordering::Relaxed) & who.bit() == 0
        })
    }

    /// Called when a job prints or ends. Whoever is watching gets one wake
    /// until it says it has caught up, so a chatty job cannot flood the UI.
    pub fn set_waker(&self, waker: Waker) {
        *self.waker.lock().unwrap() = Some(waker);
    }

    fn wake(&self) {
        if self.pending.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(w) = self.waker.lock().unwrap().as_ref() {
            w();
        }
    }

    /// The watcher is about to read; the next change wakes it again.
    pub fn caught_up(&self) {
        self.pending.store(false, Ordering::SeqCst);
    }

    /// Stop every running job and wait for it. Called when ah exits, where a
    /// kill that finishes on a thread of its own would never get the chance:
    /// the process is gone before the thread wakes up.
    pub fn shutdown(&self, grace: Duration) {
        let jobs = self.all();
        for j in &jobs {
            j.term();
        }
        let deadline = Instant::now() + grace;
        for j in &jobs {
            let left = deadline.saturating_duration_since(Instant::now());
            if !left.is_zero() {
                j.wait(left);
            }
        }
        for j in &jobs {
            j.hard_kill();
        }
        // A killed process is only done once its pipes close and the reaper
        // has it, which is what the rest of the table believes.
        for j in &jobs {
            j.wait(Duration::from_millis(200));
        }
    }
}

/// Read one pipe into the job's buffer until it closes, then reap the child if
/// this was the last pipe open.
fn pump(job: std::sync::Arc<Job>, mut pipe: impl Read + Send + 'static) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match pipe.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    job.out.lock().unwrap().push_bytes(&buf[..n]);
                    job.version.fetch_add(1, Ordering::Relaxed);
                    table().wake();
                }
            }
        }
        if job.open_pipes.fetch_sub(1, Ordering::SeqCst) != 1 {
            return;
        }
        let child = job.child.lock().unwrap().take();
        let code = match child {
            Some(mut c) => c.wait().ok().and_then(|s| s.code()).unwrap_or(-1),
            None => -1,
        };
        job.finish(code);
        table().wake();
    });
}

#[cfg(unix)]
const SIGTERM: i32 = 15;
#[cfg(unix)]
const SIGKILL: i32 = 9;
#[cfg(not(unix))]
const SIGTERM: i32 = 0;
#[cfg(not(unix))]
const SIGKILL: i32 = 0;

#[cfg(unix)]
fn signal(pid: i32, sig: i32) {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe {
        kill(-pid, sig);
    }
}

#[cfg(not(unix))]
fn signal(_pid: i32, _sig: i32) {}

/// Job notices are process-wide, and reading them claims them; tests that
/// collect notices queue up behind this.
#[cfg(test)]
pub(crate) fn notice_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cwd() -> std::path::PathBuf {
        std::env::current_dir().unwrap()
    }

    #[test]
    fn runs_and_reports_exit_code() {
        let j = table()
            .spawn("sh", "echo one; echo two 1>&2; exit 3", &cwd(), 65536, 0)
            .unwrap();
        assert!(j.wait(Duration::from_secs(5)));
        assert_eq!(j.state(), State::Done(3));
        let text = j.text();
        assert!(text.contains("one") && text.contains("two"), "{text}");
        assert!(j.duration().as_millis() < 5000);
        table().remove(j.id);
    }

    #[test]
    fn waiting_times_out_then_the_job_can_be_killed() {
        let j = table().spawn("sh", "sleep 30", &cwd(), 65536, 0).unwrap();
        assert!(!j.wait(Duration::from_millis(100)));
        assert!(j.running());
        j.kill(Duration::from_millis(50));
        assert!(j.wait(Duration::from_secs(5)), "kill did not end the job");
        table().remove(j.id);
    }

    #[test]
    fn output_keeps_head_and_tail_and_counts_the_gap() {
        let mut o = Output::new(4096);
        for i in 0..2000 {
            o.push_line(format!("line {i}"));
        }
        assert!(o.dropped > 0);
        assert_eq!(o.head[0], "line 0");
        assert_eq!(o.tail.back().unwrap(), "line 1999");
        let (lines, next) = o.view(1998, 10);
        assert_eq!(lines, vec!["line 1998", "line 1999"]);
        assert_eq!(next, 2000);
    }

    #[test]
    fn partial_lines_show_before_the_newline_arrives() {
        let mut o = Output::new(4096);
        o.push_bytes(b"abc");
        assert_eq!(o.view(0, 10).0, vec!["abc"]);
        o.push_bytes(b"def\nghi");
        assert_eq!(o.view(0, 10).0, vec!["abcdef", "ghi"]);
        assert_eq!(o.lines, 1);
    }

    #[test]
    fn a_finish_is_announced_once_per_audience() {
        let _guard = notice_lock();
        let j = table().spawn("sh", "true", &cwd(), 65536, 0).unwrap();
        j.announce();
        assert!(j.wait(Duration::from_secs(5)));
        assert!(table().unheard(Audience::Model(0)));
        let mine = format!("job {} exited 0", j.id);
        assert!(
            table()
                .notices(Audience::Model(0))
                .iter()
                .any(|n| n.starts_with(&mine))
        );
        assert!(
            !table()
                .notices(Audience::Model(0))
                .iter()
                .any(|n| n.starts_with(&mine))
        );
        assert!(!table().unheard(Audience::Model(0)));
        assert!(
            table()
                .notices(Audience::Ui(0))
                .iter()
                .any(|n| n.starts_with(&mine))
        );
        table().remove(j.id);
    }

    /// A table of this test's own. The process-wide one is shared with every
    /// other test running beside it, and these want to count what is in it.
    fn own_table() -> Jobs {
        Jobs {
            jobs: Mutex::new(Vec::new()),
            next_id: AtomicU32::new(1),
            keep: 32,
            waker: Mutex::new(None),
            pending: std::sync::atomic::AtomicBool::new(false),
        }
    }

    #[test]
    fn news_of_a_job_only_reaches_the_agent_that_started_it() {
        let t = own_table();
        let mine = t.spawn("sh", "true", &cwd(), 65536, 0).unwrap();
        let theirs = t.spawn("sh", "true", &cwd(), 65536, 7).unwrap();
        mine.announce();
        theirs.announce();
        assert!(mine.wait(Duration::from_secs(5)));
        assert!(theirs.wait(Duration::from_secs(5)));

        let heard = t.notices(Audience::Model(0));
        assert_eq!(heard.len(), 1, "{heard:?}");
        assert!(heard[0].starts_with(&format!("job {} exited 0", mine.id)));
        assert!(t.unheard(Audience::Model(7)));

        let theirs_heard = t.notices(Audience::Model(7));
        assert_eq!(theirs_heard.len(), 1, "{theirs_heard:?}");
        assert!(theirs_heard[0].starts_with(&format!("job {} exited 0", theirs.id)));
    }

    #[test]
    fn the_screen_is_told_about_its_own_agents_jobs_only() {
        let t = own_table();
        let mine = t.spawn("sh", "true", &cwd(), 65536, 0).unwrap();
        let theirs = t.spawn("sh", "true", &cwd(), 65536, 7).unwrap();
        mine.announce();
        theirs.announce();
        assert!(mine.wait(Duration::from_secs(5)));
        assert!(theirs.wait(Duration::from_secs(5)));

        let main = t.notices(Audience::Ui(0));
        assert_eq!(main.len(), 1, "{main:?}");
        assert!(main[0].starts_with(&format!("job {} exited 0", mine.id)));
        // The subagent's job is still news to the view of that agent.
        assert!(t.unheard(Audience::Ui(7)));
        assert_eq!(t.notices(Audience::Ui(7)).len(), 1);
    }

    #[test]
    fn a_command_somebody_waited_for_announces_nothing() {
        let t = own_table();
        // A foreground call never calls `announce`: the call itself reports.
        let j = t.spawn("sh", "true", &cwd(), 65536, 0).unwrap();
        assert!(j.wait(Duration::from_secs(5)));
        assert!(!t.unheard(Audience::Ui(0)));
        assert!(t.notices(Audience::Ui(0)).is_empty());
        assert!(t.notices(Audience::Model(0)).is_empty());
    }

    #[test]
    fn one_agent_cannot_reach_anothers_job() {
        let t = own_table();
        let j = t.spawn("sh", "true", &cwd(), 65536, 7).unwrap();
        assert!(j.wait(Duration::from_secs(5)));
        assert!(t.get_owned(j.id, 0).is_none());
        assert!(t.get_owned(j.id, 7).is_some());
        assert!(t.owned_by(0).is_empty());
    }

    #[test]
    fn a_command_still_running_is_handed_back_and_a_finished_one_is_dropped() {
        let t = own_table();
        let over = t.spawn("sh", "true", &cwd(), 65536, 7).unwrap();
        let going = t.spawn("sh", "sleep 30", &cwd(), 65536, 7).unwrap();
        over.announce();
        going.announce();
        assert!(over.wait(Duration::from_secs(5)));
        t.reparent_all(7, 0);

        // What the agent already dealt with is gone, not re-announced.
        assert!(t.get(over.id).is_none());
        assert!(t.notices(Audience::Model(0)).is_empty());

        // What outlived it is the main agent's now, and it hears when it ends.
        assert_eq!(going.owner(), 0);
        going.kill(Duration::from_millis(10));
        assert!(going.wait(Duration::from_secs(5)));
        let heard = t.notices(Audience::Model(0));
        assert!(
            heard
                .iter()
                .any(|n| n.starts_with(&format!("job {} exited", going.id))),
            "{heard:?}"
        );
    }

    #[test]
    fn shutdown_ends_a_job_that_ignores_the_polite_signal() {
        let t = own_table();
        let j = t
            .spawn("sh", "trap '' TERM; sleep 30", &cwd(), 65536, 0)
            .unwrap();
        // Give the shell time to install the trap before the signal lands.
        assert!(!j.wait(Duration::from_millis(200)));
        t.shutdown(Duration::from_millis(200));
        assert!(!j.running(), "shutdown returned with the job still running");
    }
}
