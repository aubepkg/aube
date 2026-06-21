use std::sync::{Arc, OnceLock, RwLock};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgressUnit {
    None,
    Count,
    Bytes,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgressOutcome {
    Success,
    Failure,
}

pub trait ProgressSink: Send + Sync + 'static {
    fn start(&self, _message: &str, _total: Option<u64>) {}

    fn update(&self, _position: Option<u64>, _total: Option<u64>, _message: Option<&str>) {}

    fn finish(&self, _outcome: ProgressOutcome, _message: &str) {}

    fn start_task(
        &self,
        _id: u64,
        _parent: Option<u64>,
        _message: &str,
        _total: Option<u64>,
        _unit: ProgressUnit,
    ) {
    }

    fn update_task(
        &self,
        _id: u64,
        _position: Option<u64>,
        _total: Option<u64>,
        _message: Option<&str>,
        _detail: Option<&str>,
    ) {
    }

    fn finish_task(&self, _id: u64, _outcome: ProgressOutcome, _message: &str) {}

    fn message(&self, _message: &str) {}
}

static ACTIVE: OnceLock<RwLock<Option<Arc<dyn ProgressSink>>>> = OnceLock::new();

fn active() -> &'static RwLock<Option<Arc<dyn ProgressSink>>> {
    ACTIVE.get_or_init(|| RwLock::new(None))
}

pub struct ProgressSinkGuard {
    previous: Option<Arc<dyn ProgressSink>>,
}

impl Drop for ProgressSinkGuard {
    fn drop(&mut self) {
        if let Ok(mut guard) = active().write() {
            *guard = self.previous.take();
        }
    }
}

pub fn install_progress_sink(sink: Arc<dyn ProgressSink>) -> ProgressSinkGuard {
    let previous = active()
        .write()
        .ok()
        .and_then(|mut guard| guard.replace(sink));
    ProgressSinkGuard { previous }
}

pub fn has_progress_sink() -> bool {
    active().read().is_ok_and(|guard| guard.is_some())
}

pub fn with_progress_sink<R>(f: impl FnOnce(&dyn ProgressSink) -> R) -> Option<R> {
    let sink = active().read().ok().and_then(|guard| guard.clone())?;
    Some(f(sink.as_ref()))
}
