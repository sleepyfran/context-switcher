//! Display-agnostic progress reporting.
//!
//! Long-running operations (`csw start`, `csw done`) call into a
//! [`Reporter`] at meaningful checkpoints. The CLI binds this to a real
//! progress UI (indicatif spinners); tests bind it to a no-op or a
//! recording stub so they can assert on what events fired without
//! coupling to any particular renderer.

/// A reporter scoped to a whole operation. Each repo gets its own
/// [`RepoProgress`] handle via [`Reporter::begin`].
pub trait Reporter: Send + Sync {
    /// Open progress for a repo. `action` is a verb-phrase like "creating"
    /// or "deleting" that the renderer can use as the initial label.
    fn begin(&self, repo: &str, action: &str) -> Box<dyn RepoProgress>;
}

/// A handle scoped to one repo's progress. The renderer decides what
/// `step`/`ok`/`err` look like (spinner update, finish with check mark,
/// finish with cross, etc.).
pub trait RepoProgress: Send {
    fn step(&self, message: &str);
    fn ok(&self, message: &str);
    fn err(&self, message: &str);
}

/// Reporter that drops every event on the floor — used by tests and by
/// the CLI in non-interactive (`--quiet` or non-TTY) contexts.
pub struct NullReporter;

impl Reporter for NullReporter {
    fn begin(&self, _repo: &str, _action: &str) -> Box<dyn RepoProgress> {
        Box::new(NullProgress)
    }
}

struct NullProgress;

impl RepoProgress for NullProgress {
    fn step(&self, _message: &str) {}
    fn ok(&self, _message: &str) {}
    fn err(&self, _message: &str) {}
}

#[cfg(test)]
pub mod testing {
    //! Recording reporter used by core tests to assert which events fired.

    use super::*;
    use std::sync::Mutex;

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Event {
        Begin { repo: String, action: String },
        Step { repo: String, message: String },
        Ok { repo: String, message: String },
        Err { repo: String, message: String },
    }

    #[derive(Default)]
    pub struct RecordingReporter {
        events: Mutex<Vec<Event>>,
    }

    impl RecordingReporter {
        pub fn new() -> Self {
            Self::default()
        }
        pub fn events(&self) -> Vec<Event> {
            self.events.lock().unwrap().clone()
        }
        fn push(&self, e: Event) {
            self.events.lock().unwrap().push(e);
        }
    }

    impl Reporter for RecordingReporter {
        fn begin(&self, repo: &str, action: &str) -> Box<dyn RepoProgress> {
            self.push(Event::Begin {
                repo: repo.to_string(),
                action: action.to_string(),
            });
            Box::new(RecordingProgress {
                repo: repo.to_string(),
                // Pointer to the parent's vec via a leaked Arc would be fancier;
                // for tests we just stash a clone of the parent reference using
                // a static-friendly wrapper.
                events: self as *const _,
            })
        }
    }

    struct RecordingProgress {
        repo: String,
        events: *const RecordingReporter,
    }

    // Safety: tests run single-threaded against the same RecordingReporter
    // for the duration of the operation; we don't move the pointer across
    // threads, and the reporter outlives every progress handle it produces.
    unsafe impl Send for RecordingProgress {}

    impl RecordingProgress {
        fn parent(&self) -> &RecordingReporter {
            // Safety: tied to the lifetime of the test scope by construction.
            unsafe { &*self.events }
        }
    }

    impl RepoProgress for RecordingProgress {
        fn step(&self, message: &str) {
            self.parent().push(Event::Step {
                repo: self.repo.clone(),
                message: message.to_string(),
            });
        }
        fn ok(&self, message: &str) {
            self.parent().push(Event::Ok {
                repo: self.repo.clone(),
                message: message.to_string(),
            });
        }
        fn err(&self, message: &str) {
            self.parent().push(Event::Err {
                repo: self.repo.clone(),
                message: message.to_string(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_reporter_is_a_silent_noop() {
        let r = NullReporter;
        let p = r.begin("frontend", "creating");
        p.step("cloning");
        p.ok("done");
        p.err("nope");
        // Reaching here is the assertion: nothing panicked, nothing observable.
    }

    #[test]
    fn recording_reporter_captures_event_sequence() {
        use testing::{Event, RecordingReporter};

        let r = RecordingReporter::new();
        {
            let p = r.begin("frontend", "creating");
            p.step("cloning");
            p.step("fetching");
            p.ok("ready");
        }

        let events = r.events();
        assert_eq!(
            events,
            vec![
                Event::Begin {
                    repo: "frontend".into(),
                    action: "creating".into()
                },
                Event::Step {
                    repo: "frontend".into(),
                    message: "cloning".into()
                },
                Event::Step {
                    repo: "frontend".into(),
                    message: "fetching".into()
                },
                Event::Ok {
                    repo: "frontend".into(),
                    message: "ready".into()
                },
            ]
        );
    }
}
