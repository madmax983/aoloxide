//! Job queues (`nsd/tcljob.c`, `ns_job`).
//!
//! Each queue runs jobs on up to `max_threads` worker threads.  Jobs are
//! engine-agnostic closures: the `ns_job` command wraps a script in a
//! closure that allocates an interpreter for the queue's server on the
//! worker thread.  Non-detached job results are kept until collected with
//! `wait`/`waitany`; detached jobs are discarded when done.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use ns_script::{Code, Exception};

/// The outcome of a job.
pub type JobResult = Result<String, Exception>;
/// Work run by a job.
pub type JobFn = Box<dyn FnOnce() -> JobResult + Send>;
/// Called by each worker thread before it exits (e.g. to release interps).
pub type ThreadExit = Arc<dyn Fn() + Send + Sync>;

/// Job state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    /// Queued, not yet started.
    Scheduled,
    /// Running.
    Running,
    /// Finished.
    Done,
}

impl JobState {
    /// Name reported by `ns_job joblist`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Scheduled => "scheduled",
            Self::Running => "running",
            Self::Done => "done",
        }
    }
}

/// Snapshot of a job (`ns_job joblist`).
#[derive(Debug, Clone)]
pub struct JobInfo {
    /// Job id.
    pub id: String,
    /// State.
    pub state: JobState,
    /// Result so far (empty until done).
    pub results: String,
    /// Script/description.
    pub script: String,
    /// Completion code name (`TCL_OK`, `TCL_ERROR`, ...).
    pub code: String,
    /// `detached` or `nondetached`.
    pub detached: bool,
    /// Pending request: `none`, `wait`, `cancel`.
    pub req: &'static str,
    /// Run time in seconds.
    pub time: f64,
    /// Start (Unix seconds, 0 if not started).
    pub start: i64,
    /// End (Unix seconds, 0 if not done).
    pub end: i64,
}

/// Snapshot of a queue (`ns_job queuelist`).
#[derive(Debug, Clone)]
pub struct QueueInfo {
    /// Name.
    pub name: String,
    /// Description.
    pub desc: String,
    /// Max worker threads.
    pub max_threads: usize,
    /// Running jobs.
    pub running: usize,
    /// Pending request (`none` or `delete`).
    pub req: &'static str,
}

struct Job {
    work: Option<JobFn>,
    state: JobState,
    detached: bool,
    waiting: bool,
    cancelled: bool,
    script: String,
    result: Option<JobResult>,
    started: Option<Instant>,
    elapsed: f64,
    start_unix: i64,
    end_unix: i64,
}

struct QueueInner {
    jobs: BTreeMap<String, Job>,
    pending: VecDeque<String>,
    running: usize,
    threads: usize,
    deleted: bool,
}

/// One job queue.
pub struct Queue {
    /// Queue name.
    pub name: String,
    /// Description.
    pub desc: String,
    /// Max concurrent jobs.
    pub max_threads: usize,
    inner: Mutex<QueueInner>,
    cond: Condvar,
    on_exit: Option<ThreadExit>,
}

impl std::fmt::Debug for Queue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Queue")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

fn code_name(r: &JobResult) -> String {
    match r {
        Ok(_) => "TCL_OK".into(),
        Err(e) => match e.code {
            Code::Error => "TCL_ERROR".into(),
            Code::Return => "TCL_RETURN".into(),
            Code::Break => "TCL_BREAK".into(),
            Code::Continue => "TCL_CONTINUE".into(),
            Code::Other(n) => n.to_string(),
        },
    }
}

impl Queue {
    fn lock(&self) -> MutexGuard<'_, QueueInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn worker(self: Arc<Self>) {
        loop {
            let (id, work) = {
                let mut g = self.lock();
                loop {
                    if g.deleted && g.pending.is_empty() {
                        g.threads -= 1;
                        drop(g);
                        if let Some(f) = &self.on_exit {
                            f();
                        }
                        self.cond.notify_all();
                        return;
                    }
                    if let Some(id) = g.pending.pop_front() {
                        let Some(job) = g.jobs.get_mut(&id) else {
                            continue;
                        };
                        if job.cancelled {
                            g.jobs.remove(&id);
                            continue;
                        }
                        job.state = JobState::Running;
                        job.started = Some(Instant::now());
                        job.start_unix = ns_core::httptime::now();
                        let work = job.work.take();
                        g.running += 1;
                        break (id, work);
                    }
                    let (ng, to) = self
                        .cond
                        .wait_timeout(g, Duration::from_secs(60))
                        .unwrap_or_else(PoisonError::into_inner);
                    g = ng;
                    if to.timed_out() && g.pending.is_empty() {
                        g.threads -= 1;
                        drop(g);
                        if let Some(f) = &self.on_exit {
                            f();
                        }
                        return;
                    }
                }
            };
            let result = work.map_or_else(
                || Ok(String::new()),
                |w| {
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(w))
                        .unwrap_or_else(|_| Err(Exception::error("job panicked")))
                },
            );
            let mut g = self.lock();
            g.running -= 1;
            let remove = g.jobs.get_mut(&id).is_some_and(|job| {
                job.state = JobState::Done;
                job.elapsed = job.started.map_or(0.0, |s| s.elapsed().as_secs_f64());
                job.end_unix = ns_core::httptime::now();
                job.result = Some(result);
                job.detached || job.cancelled
            });
            if remove {
                g.jobs.remove(&id);
            }
            drop(g);
            self.cond.notify_all();
        }
    }

    /// Submit a job.  Returns its id.
    pub fn submit(
        self: &Arc<Self>,
        id: String,
        script: &str,
        detached: bool,
        work: JobFn,
    ) -> Result<String, Exception> {
        let spawn = {
            let mut g = self.lock();
            if g.deleted {
                return Err(Exception::error(format!(
                    "queue \"{}\" is being deleted",
                    self.name
                )));
            }
            g.jobs.insert(
                id.clone(),
                Job {
                    work: Some(work),
                    state: JobState::Scheduled,
                    detached,
                    waiting: false,
                    cancelled: false,
                    script: script.to_owned(),
                    result: None,
                    started: None,
                    elapsed: 0.0,
                    start_unix: 0,
                    end_unix: 0,
                },
            );
            g.pending.push_back(id.clone());
            let busy = g.running + g.pending.len();
            let spawn = g.threads < self.max_threads && busy > g.threads;
            if spawn {
                g.threads += 1;
            }
            spawn
        };
        if spawn {
            let me = Arc::clone(self);
            let name = format!("-ns_job_{}-", self.name);
            if std::thread::Builder::new()
                .name(name)
                .spawn(move || me.worker())
                .is_err()
            {
                self.lock().threads -= 1;
            }
        }
        self.cond.notify_all();
        Ok(id)
    }

    /// Wait for job `id` and take its result.
    pub fn wait(&self, id: &str, timeout: Option<Duration>) -> Result<JobResult, Exception> {
        let deadline = timeout.map(|t| Instant::now() + t);
        let mut g = self.lock();
        match g.jobs.get_mut(id) {
            None => return Err(Exception::error(format!("no such job: {id}"))),
            Some(j) if j.detached => {
                return Err(Exception::error(format!(
                    "can't wait on detached job: {id}"
                )))
            }
            Some(j) if j.waiting => {
                return Err(Exception::error(format!("can't wait on waited job: {id}")))
            }
            Some(j) => j.waiting = true,
        }
        loop {
            if g.jobs.get(id).is_some_and(|j| j.state == JobState::Done) {
                let job = g
                    .jobs
                    .remove(id)
                    .ok_or_else(|| Exception::error("job vanished"))?;
                return Ok(job.result.unwrap_or_else(|| Ok(String::new())));
            }
            g = match deadline {
                None => self.cond.wait(g).unwrap_or_else(PoisonError::into_inner),
                Some(d) => {
                    let now = Instant::now();
                    if now >= d {
                        if let Some(j) = g.jobs.get_mut(id) {
                            j.waiting = false;
                        }
                        return Err(Exception::error("Wait timed out."));
                    }
                    self.cond
                        .wait_timeout(g, d - now)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0
                }
            };
        }
    }

    /// Wait until any job is done (or the queue has no jobs).
    pub fn wait_any(&self, timeout: Option<Duration>) -> Result<(), Exception> {
        let deadline = timeout.map(|t| Instant::now() + t);
        let mut g = self.lock();
        while !g.jobs.is_empty() && !g.jobs.values().any(|j| j.state == JobState::Done) {
            g = match deadline {
                None => self.cond.wait(g).unwrap_or_else(PoisonError::into_inner),
                Some(d) => {
                    let now = Instant::now();
                    if now >= d {
                        return Err(Exception::error("Wait timed out."));
                    }
                    self.cond
                        .wait_timeout(g, d - now)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0
                }
            };
        }
        Ok(())
    }

    /// Cancel a job.  Returns whether it was running.
    pub fn cancel(&self, id: &str) -> Result<bool, Exception> {
        let mut g = self.lock();
        let job = g
            .jobs
            .get_mut(id)
            .ok_or_else(|| Exception::error(format!("no such job: {id}")))?;
        if job.waiting {
            return Err(Exception::error(format!(
                "Can not cancel this job because someone is waiting on it. Job ID: {id}"
            )));
        }
        job.cancelled = true;
        let running = job.state == JobState::Running;
        if job.state == JobState::Done {
            g.jobs.remove(id);
        }
        drop(g);
        self.cond.notify_all();
        Ok(running)
    }

    /// Ids of all jobs.
    pub fn job_ids(&self) -> Vec<String> {
        self.lock().jobs.keys().cloned().collect()
    }

    /// Snapshots of all jobs.
    pub fn jobs(&self) -> Vec<JobInfo> {
        self.lock()
            .jobs
            .iter()
            .map(|(id, j)| JobInfo {
                id: id.clone(),
                state: j.state,
                results: match &j.result {
                    Some(Ok(s)) => s.clone(),
                    Some(Err(e)) => e.message.clone(),
                    None => String::new(),
                },
                script: j.script.clone(),
                code: j.result.as_ref().map_or_else(|| "TCL_OK".into(), code_name),
                detached: j.detached,
                req: if j.cancelled {
                    "cancel"
                } else if j.waiting {
                    "wait"
                } else {
                    "none"
                },
                time: j.elapsed,
                start: j.start_unix,
                end: j.end_unix,
            })
            .collect()
    }

    /// Queue snapshot.
    pub fn info(&self) -> QueueInfo {
        let g = self.lock();
        QueueInfo {
            name: self.name.clone(),
            desc: self.desc.clone(),
            max_threads: self.max_threads,
            running: g.running,
            req: if g.deleted { "delete" } else { "none" },
        }
    }

    fn mark_deleted(&self) {
        self.lock().deleted = true;
        self.cond.notify_all();
    }

    fn wait_threads(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut g = self.lock();
        while g.threads > 0 {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            g = self
                .cond
                .wait_timeout(g, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        true
    }
}

/// All job queues.
#[derive(Default)]
pub struct JobRegistry {
    queues: Mutex<BTreeMap<String, Arc<Queue>>>,
    next_id: Mutex<u64>,
}

impl std::fmt::Debug for JobRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobRegistry").finish_non_exhaustive()
    }
}

/// Default `maxThreads` for `ns_job create`.
pub const DEFAULT_MAX_THREADS: usize = 4;

impl JobRegistry {
    /// Create a queue; errors if it exists.
    pub fn create(
        &self,
        name: &str,
        desc: &str,
        max_threads: usize,
        on_exit: Option<ThreadExit>,
    ) -> Result<Arc<Queue>, Exception> {
        let mut q = self.queues.lock().unwrap_or_else(PoisonError::into_inner);
        if q.contains_key(name) {
            return Err(Exception::error(format!("queue already exists: {name}")));
        }
        let queue = Arc::new(Queue {
            name: name.to_owned(),
            desc: desc.to_owned(),
            max_threads: max_threads.max(1),
            inner: Mutex::new(QueueInner {
                jobs: BTreeMap::new(),
                pending: VecDeque::new(),
                running: 0,
                threads: 0,
                deleted: false,
            }),
            cond: Condvar::new(),
            on_exit,
        });
        q.insert(name.to_owned(), Arc::clone(&queue));
        Ok(queue)
    }

    /// Look up a queue.
    pub fn get(&self, name: &str) -> Result<Arc<Queue>, Exception> {
        self.queues
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
            .cloned()
            .ok_or_else(|| Exception::error(format!("no such queue: {name}")))
    }

    /// Delete a queue once its jobs finish.
    pub fn delete(&self, name: &str) -> Result<(), Exception> {
        let q = self
            .queues
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(name)
            .ok_or_else(|| Exception::error(format!("no such queue: {name}")))?;
        q.mark_deleted();
        Ok(())
    }

    /// Queue names.
    pub fn names(&self) -> Vec<String> {
        self.queues
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .keys()
            .cloned()
            .collect()
    }

    /// All queues.
    pub fn all(&self) -> Vec<Arc<Queue>> {
        self.queues
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .cloned()
            .collect()
    }

    /// A fresh unique job id (`ns_job genid`).
    pub fn gen_id(&self) -> String {
        let mut n = self.next_id.lock().unwrap_or_else(PoisonError::into_inner);
        let id = format!("job{n}");
        *n += 1;
        id
    }

    /// Delete every queue and wait for workers (`NsWaitJobsShutdown`).
    pub fn shutdown(&self, timeout: Duration) -> bool {
        let all: Vec<Arc<Queue>> =
            std::mem::take(&mut *self.queues.lock().unwrap_or_else(PoisonError::into_inner))
                .into_values()
                .collect();
        for q in &all {
            q.mark_deleted();
        }
        all.iter().all(|q| q.wait_threads(timeout))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_wait_and_detached() {
        let r = JobRegistry::default();
        let q = r.create("q", "test", 2, None).expect("create");
        assert!(r.create("q", "", 1, None).is_err());
        let id = q
            .submit(r.gen_id(), "a", false, Box::new(|| Ok("done".into())))
            .expect("submit");
        assert_eq!(q.wait(&id, None).expect("wait"), Ok("done".into()));
        assert!(q.wait(&id, None).is_err(), "result already collected");
        let d = q
            .submit(r.gen_id(), "b", true, Box::new(|| Ok(String::new())))
            .expect("submit");
        assert!(q.wait(&d, None).is_err() || q.job_ids().is_empty());
        let e = q
            .submit(
                r.gen_id(),
                "c",
                false,
                Box::new(|| Err(Exception::error("bad"))),
            )
            .expect("submit");
        q.wait_any(Some(Duration::from_secs(5))).expect("any");
        assert_eq!(
            q.jobs()
                .iter()
                .find(|j| j.id == e)
                .map(|j| j.code.clone())
                .as_deref(),
            Some("TCL_ERROR")
        );
        let res = q.wait(&e, None).expect("wait");
        assert_eq!(res.expect_err("err").message, "bad");
        assert_eq!(q.info().max_threads, 2);
        assert_eq!(r.names(), vec!["q"]);
        r.delete("q").expect("delete");
        assert!(r.get("q").is_err());
        assert!(q
            .submit("x".into(), "", false, Box::new(|| Ok(String::new())))
            .is_err());
    }

    #[test]
    fn timeout_and_cancel() {
        let r = JobRegistry::default();
        let q = r.create("slow", "", 1, None).expect("create");
        let a = q
            .submit(
                r.gen_id(),
                "sleep",
                false,
                Box::new(|| {
                    std::thread::sleep(Duration::from_millis(200));
                    Ok("a".into())
                }),
            )
            .expect("submit");
        let b = q
            .submit(r.gen_id(), "never", false, Box::new(|| Ok("b".into())))
            .expect("submit");
        assert!(!q.cancel(&b).expect("cancel"));
        assert_eq!(
            q.wait(&a, Some(Duration::from_millis(10)))
                .expect_err("timeout")
                .message,
            "Wait timed out."
        );
        assert_eq!(
            q.wait(&a, Some(Duration::from_secs(5))).expect("wait"),
            Ok("a".into())
        );
        assert!(r.shutdown(Duration::from_secs(5)));
        assert_eq!(JobState::Running.as_str(), "running");
    }
}
