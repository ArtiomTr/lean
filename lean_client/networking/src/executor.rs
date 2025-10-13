use std::pin::Pin;

use futures::Future;
use libp2p::swarm;

pub trait TaskExecutor: Clone + Send + Sync + 'static {
    fn spawn(&self, task: Pin<Box<dyn Future<Output = ()> + Send>>);
}

#[derive(Clone, Default)]
pub struct TokioTaskExecutor;

impl TaskExecutor for TokioTaskExecutor {
    fn spawn(&self, task: Pin<Box<dyn Future<Output = ()> + Send>>) {
        tokio::spawn(task);
    }
}

#[derive(Clone)]
pub struct Executor<E: TaskExecutor>(pub E);

impl<E: TaskExecutor> swarm::Executor for Executor<E> {
    fn exec(&self, task: Pin<Box<dyn Future<Output = ()> + Send>>) {
        self.0.spawn(task);
    }
}
