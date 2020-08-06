extern crate rand;

// TODO: respect resource order preference for scheduled tasks.

use ::lazy_static::lazy_static;

use self::task_ident::TaskIdent;
use fs2::FileExt;
use log::debug;
use rand::distributions::Alphanumeric;
use rand::{thread_rng, Rng};
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fs::{self, create_dir_all, File};
use std::io::Error;
use std::iter;
use std::marker::PhantomData;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

use self::{resource_lock::ResourceLock, task_file::TaskFile};

mod resource_lock;
mod task_file;
mod task_ident;

/// How often, in milliseconds, should we poll by default?
const POLL_INTERVAL_MS: u64 = 100;
const LOCK_NAME: &str = "resource.lock";

/// Lower values have 'higher' priority.
type Priority = usize;

lazy_static! {
    static ref PROCESS_ID: String = iter::repeat(())
        .map(|()| thread_rng().sample(Alphanumeric))
        .take(10)
        .collect();
}

pub trait Resource {
    /// `dir_id` uniquely identifies the directory associated with the resource.
    fn dir_id(&self) -> String;
    /// `name` is the descriptive name of the resource and defaults to wrapping `dir_id`.
    fn name(&self) -> String {
        format!("Resource #{}", self.dir_id())
    }
}

/// Implementers of `Executable` act as a callback which executes the job associated with a task.
pub trait Executable<R: Resource + Clone> {
    /// `execute` executes a task's job. `preempt.should_preempt_now()` should be polled as appropriate,
    /// and execution should terminate if it returns true. Tasks which are not preemptible need not
    /// ever check for preemption.
    fn execute(&self, preempt: &dyn Preemption<R>);

    /// Returns true if the job associated with this `Executable` can be preempted. `Executable`s
    /// which return `true` should periodically poll for preemption while executing.
    fn is_preemptible(&self) -> bool {
        false
    }
}

pub trait Preemption<R: Resource + Clone> {
    // Return true if task should be preempted now.
    // `Executable`s which are preemptible, must call this method.
    fn should_preempt_now(&self, _task: &Task<R>) -> bool;
}

impl<'a, R: Resource + Clone> Preemption<R> for ResourceScheduler<'a, R> {
    /// The current `Task` should be preempted if the high-priority lock has been acquired
    /// by another `Task`.
    fn should_preempt_now(&self, _task: &Task<R>) -> bool {
        todo!();
    }
}

pub struct Task<'a, R: Resource> {
    /// These are the resources for which the `Task` has been requested to be scheduled,
    /// in order of preference. It is guaranteed that the `Task` will be scheduled on only one of these.
    executable: Box<&'a dyn Executable<R>>,
}

impl<'a, R: Resource> Task<'a, R> {
    pub fn new(executable: Box<&'a dyn Executable<R>>) -> Self {
        Self { executable }
    }

    fn clone_task(&self) -> Self {
        Self {
            executable: Box::new(*self.executable),
        }
    }
}

pub struct Scheduler<'a, R: Resource + Clone> {
    scheduler_root: Arc<Mutex<SchedulerRoot<'a, R>>>,
    resource_schedulers: Vec<ResourceScheduler<'a, R>>,
    control_chan: Option<mpsc::Sender<()>>,
    poll_interval: u64,
}

pub struct ScheduleData {}

impl<'a, R: 'a + Resource + Copy + Send> Scheduler<'a, R> {
    pub fn new(root: PathBuf) -> Result<Self, Error> {
        Self::new_with_poll_interval(root, POLL_INTERVAL_MS)
    }

    pub fn new_with_poll_interval(root: PathBuf, poll_interval: u64) -> Result<Self, Error> {
        let scheduler = SchedulerRoot::new(root)?;
        Ok(Self {
            scheduler_root: Arc::new(Mutex::new(scheduler)),
            resource_schedulers: Default::default(),
            control_chan: None,
            poll_interval,
        })
    }

    pub fn start(scheduler: &'static Mutex<Self>) -> Result<(), Error> {
        let (control_tx, control_rx) = mpsc::channel();
        thread::spawn(move || {
            let should_stop = || !control_rx.try_recv().is_err();
            let poll_interval = scheduler.lock().unwrap().poll_interval;
            loop {
                if should_stop() {
                    break;
                };
                for s in scheduler.lock().unwrap().resource_schedulers.iter_mut() {
                    if should_stop() {
                        break;
                    };

                    s.handle_next().expect("failed in handle_next"); // FIXME
                }
                thread::sleep(Duration::from_millis(poll_interval));
            }
        });
        scheduler.lock().unwrap().control_chan = Some(control_tx);
        Ok(())
    }

    pub fn schedule(
        &mut self,
        priority: usize,
        name: &str,
        task: &'a dyn Executable<R>,
        resources: &[R],
    ) -> Result<(), Error> {
        resources.iter().for_each(|r| {
            self.ensure_resource_scheduler(*r);
        });
        let task_ident = self
            .scheduler_root
            .lock()
            .unwrap()
            .new_ident(priority, name);
        let task = Task::new(Box::new(task));
        self.scheduler_root
            .lock()
            .unwrap()
            .schedule(task_ident, task, resources)
    }

    pub fn stop(&self) -> Result<(), mpsc::SendError<()>> {
        if let Some(c) = self.control_chan.as_ref() {
            c.send(())?
        };
        Ok(())
    }

    fn ensure_resource_scheduler(&mut self, resource: R) {
        let dir = self
            .scheduler_root
            .lock()
            .unwrap()
            .root
            .join(resource.dir_id());

        let rs = ResourceScheduler::new(self.scheduler_root.clone(), dir, resource);
        // FIXME: only add if needed.
        self.resource_schedulers.push(rs);
    }
}

struct SchedulerRoot<'a, R: Resource + Clone> {
    root: PathBuf,
    /// A given `Task` (identified uniquely by a `TaskIdent`) may have multiple `TaskFile`s associated,
    /// one per `Resource` for which it is currently scheduled (but only one `Resource` will eventually be assigned).
    task_files: HashMap<TaskIdent, HashSet<TaskFile>>,
    /// Each `Task` (identified uniquely by a `TaskIdent`) is protected by a `Mutex`.
    own_tasks: HashMap<TaskIdent, Mutex<Task<'a, R>>>,
    children: Mutex<HashMap<String, ResourceScheduler<'a, R>>>,
    ident_counter: usize,
}

unsafe impl<'a, R: Resource + Clone> Send for SchedulerRoot<'a, R> {}

impl<'a, R: Resource + Clone> SchedulerRoot<'a, R> {
    fn new(root: PathBuf) -> Result<Self, Error> {
        create_dir_all(&root)?;
        Ok(Self {
            root,
            task_files: Default::default(),
            own_tasks: Default::default(),
            children: Default::default(),
            ident_counter: 0,
        })
    }
    fn new_ident(&mut self, priority: Priority, name: &str) -> TaskIdent {
        let id = self.ident_counter;
        self.ident_counter += 1;
        TaskIdent::new(priority, name, id)
    }
    fn schedule(
        &mut self,
        task_ident: TaskIdent,
        task: Task<'a, R>,
        resources: &[R],
    ) -> Result<(), Error> {
        for resource in resources.iter() {
            let dir = self.root.join(resource.dir_id());
            create_dir_all(&dir)?;
            let task_file = task_ident.enqueue_in_dir(&dir)?;

            self.task_files
                .entry(task_ident.clone())
                .or_insert(Default::default())
                .insert(task_file);
            self.own_tasks
                .insert(task_ident.clone(), Mutex::new(task.clone_task()));
        }
        Ok(())
    }
}

struct ResourceScheduler<'a, R: Resource + Clone> {
    root_scheduler: Arc<Mutex<SchedulerRoot<'a, R>>>,
    dir: PathBuf,
    resource: R,
    /// The previous 'next', and a count of how many times we have seen it as such.
    previous: Option<(TaskIdent, usize)>,
}

impl<'a, R: Resource + Clone> ResourceScheduler<'a, R> {
    fn new(root_scheduler: Arc<Mutex<SchedulerRoot<'a, R>>>, dir: PathBuf, resource: R) -> Self {
        Self {
            root_scheduler,
            dir,
            resource,
            previous: None,
        }
    }
    fn lock(&self) -> Result<ResourceLock, Error> {
        ResourceLock::acquire(&self.dir, &self.resource)
    }

    fn handle_next(&mut self) -> Result<(), Error> {
        assert!(self.dir.is_dir(), "scheduler dir is not a directory.");
        let mut ident_data = Vec::new();
        let _ = fs::read_dir(&self.dir)?
            .map(|res| {
                res.map(|e| {
                    // FIXME: unwraps
                    let metadata = e.metadata().unwrap();
                    let task_ident = TaskIdent::from_str(
                        &e.file_name()
                            .to_str()
                            .expect("failed to create TaskIdent from string"),
                    )
                    .ok();
                    let file = File::open(e.path()).unwrap();
                    let locked = file.try_lock_exclusive().is_err();
                    if let Some(ident) = task_ident {
                        ident_data.push((
                            ident,
                            metadata.created().expect("failed to create metadata"),
                            locked,
                        ))
                    };
                })
            })
            .collect::<Result<Vec<_>, Error>>()?;
        ident_data.sort_by(|(a_ident, a_create_date, _), (b_ident, b_create_date, _)| {
            // Sort first by (priority, creation date).
            let priority_ordering = a_ident.priority.partial_cmp(&b_ident.priority).unwrap();
            match priority_ordering {
                Ordering::Equal => a_create_date.partial_cmp(&b_create_date).unwrap(),
                _ => priority_ordering,
            }
        });
        let (ident, locked) = if let Some((ident, _, locked)) = ident_data.get(0) {
            (ident, *locked)
        } else {
            // If there was no `TaskIdent` found, nothing to do.
            // Forget about anything we saw before.
            self.previous = None;
            return Ok(());
        };
        let is_own = self
            .root_scheduler
            .lock()
            .unwrap()
            .own_tasks
            .get(ident)
            .is_some();

        if is_own {
            // Task is owned by this process.

            let mut performed_task = false;
            {
                let root_scheduler = self.root_scheduler.lock().unwrap();
                // Lock the task so a sibling won't remove it.
                let mut guard_result = root_scheduler
                    .own_tasks
                    .get(ident)
                    .expect("own task missing")
                    .try_lock();

                if let Ok(ref mut guard) = guard_result {
                    let task = &*guard;
                    self.previous = None;

                    let mut to_destroy_later = None;

                    // We have the lock for this task, so we may destroy the sibling TaskFiles.
                    if let Some(all_task_files) = root_scheduler.task_files.get(ident) {
                        // FIXME: unwrap
                        all_task_files.iter().for_each(|task_file| {
                            // Don't destroy this directory's task file until we are done performing the task
                            if !task_file.path.starts_with(self.dir.clone()) {
                                // We already hold the lock for all of our task files, so this is okay.
                                task_file.destroy().unwrap();
                            // TODO: check that destroy fails gracefully if already gone.
                            } else {
                                to_destroy_later = Some(task_file);
                            }
                        });
                    }

                    self.perform_task(&task)?;
                    // NOTE: We must defer removing from `self.own_tasks` because the map is borrowed in this scope above.
                    performed_task = true;

                    // Finally, destroy this `TaskFile`, too — assuming it is necessary.
                    if let Some(task_file) = to_destroy_later {
                        // We already hold the lock for this task file, so this is okay.
                        task_file.destroy().unwrap()
                    };
                } else {
                    // Task `Mutex` was already locked, which means this process has already assigned it to a different resource.
                    // Do nothing and allow it to be cleaned up (removed from this queue) as part of that assignment.
                }

                // lock is dropped here
            }

            if performed_task {
                // Now we can remove (see NOTE above).
                self.root_scheduler.lock().unwrap().own_tasks.remove(&ident);
            }
        } else {
            // Task is owned by another process.
            if locked {
                self.previous = None;
            } else {
                self.previous = match &self.previous {
                    // The same unlocked task has been 'next up' for turns threevmx, so it has forfeited its turn.
                    // Since we discovered this, it is our job to destroy it.
                    // We need to see it three times, since different processes will be on different schedules.
                    // Worst-case behavior of out-of-sync schedules gives no time for the actual winner to act.
                    Some((previous, n)) if previous == ident && *n >= 2 => {
                        // If this fails, someone else may have seized the lock and done it for us.
                        previous.try_destroy(&self.dir)?;
                        None
                    }

                    // Increment the count, so we can destroy this if we see it on top next time we check.
                    Some((previous, n)) if previous == ident => Some((previous.clone(), n + 1)),

                    // No match, forget.
                    Some(_) => None,

                    // Remember this ident,
                    None => Some((ident.clone(), 1)),
                }
            }
        }
        Ok(())
    }

    fn perform_task(&self, task: &Task<R>) -> Result<(), Error> {
        let _lock = self.lock()?;
        // Pass `self` so `Executable` can call `should_preempt_now` on it if needed.
        task.executable.execute(self);
        Ok(())
        // Lock is dropped, and therefore released here, at end of scope.
    }
}

mod test {
    use super::*;

    /// `Scheduler` requires that resources be `Copy`.
    #[derive(Copy, Clone, Debug)]
    struct Rsrc {
        id: usize,
    }

    impl Resource for Rsrc {
        fn dir_id(&self) -> String {
            self.id.to_string()
        }
    }

    struct Dummy<R> {
        _r: PhantomData<R>,
    }
    impl<R: Resource + Clone> Preemption<R> for Dummy<R> {
        fn should_preempt_now(&self, _task: &Task<R>) -> bool {
            false
        }
    }

    #[derive(Debug)]
    struct Task1 {
        id: usize,
    }

    impl<R: Resource + Clone> Executable<R> for Task1 {
        fn execute(&self, _p: &dyn Preemption<R>) {
            (*RESULT_STATE).lock().unwrap().push(self.id);
        }
    }

    lazy_static! {
        static ref RESULT_STATE: Mutex<Vec<usize>> = Mutex::new(Vec::new());
        static ref SCHEDULER: Mutex<Scheduler::<'static, Rsrc>> = Mutex::new(
            Scheduler::<Rsrc>::new(tempfile::tempdir().unwrap().into_path())
                .expect("Failed to create scheduler")
        );
        static ref TASKS: Vec<Task1> = (0..5).map(|id| Task1 { id }).collect::<Vec<_>>();
    }

    #[test]
    fn test_scheduler() {
        let scheduler = &*SCHEDULER;
        let root_dir = scheduler
            .lock()
            .unwrap()
            .scheduler_root
            .lock()
            .unwrap()
            .root
            .clone();

        let resources = (0..3).map(|id| Rsrc { id }).collect::<Vec<_>>();

        let mut expected = Vec::new();

        let control_chan = Scheduler::start(scheduler).expect("Failed to start scheduler.");
        for (i, task) in TASKS.iter().enumerate() {
            // When tasks are added very quickly (relative to the poll interval),
            // they should be performed in order of their priority.
            // In this group, we set priority to be the 'inverse' of task id.
            // So task 0 has a high-numbered priority and should be performed last.
            // Therefore, we push the highest id onto `expected` first.
            let priority = TASKS.len() - i - 1;
            expected.push(priority);
            scheduler
                .lock()
                .unwrap()
                .schedule(priority, &format!("{:?}", task), task, &resources);
        }
        thread::sleep(Duration::from_millis(1000));
        for (i, task) in TASKS.iter().enumerate() {
            // This example is like the previous, except that we sleep for twice the length of the poll interval
            // between each call to `schedule`. TODO: set the poll interval explicitly in the test.
            // Because each task is fully processed, they are performed in the order scheduled.
            let priority = TASKS.len() - i - 1;
            expected.push(i);
            thread::sleep(Duration::from_millis(200));
            scheduler
                .lock()
                .unwrap()
                .schedule(priority, &format!("{:?}", task), task, &resources);
        }
        thread::sleep(Duration::from_millis(1000));
        for (i, task) in TASKS.iter().enumerate() {
            // In this example, tasks are added quickly and with priority matching id.
            // We therefore expect them to be performed in the order scheduled.
            // This case is somewhat trivial.
            expected.push(i);
            scheduler
                .lock()
                .unwrap()
                .schedule(i, &format!("{:?}", task), task, &resources);
        }
        thread::sleep(Duration::from_millis(1000));
        scheduler.lock().unwrap().stop();

        assert_eq!(TASKS.len() * 3, expected.len());

        assert_eq!(expected, *RESULT_STATE.lock().unwrap());
    }
}