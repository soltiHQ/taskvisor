use std::{borrow::Cow, future::Future, sync::Mutex};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::error::TaskError;

pub type TaskRef = std::sync::Arc<dyn Task>;

#[async_trait]
pub trait Task: Send + Sync + 'static {
    fn name(&self) -> &str;
    async fn run(&self, ctx: CancellationToken) -> Result<(), TaskError>;
}

#[derive(Debug)]
pub struct TaskFn<Fnc, Fut>
where
    Fnc: FnMut(CancellationToken) -> Fut + Send + 'static,
    Fut: Future<Output = Result<(), TaskError>> + Send + 'static,
{
    name: Cow<'static, str>,
    func: Mutex<Fnc>,
}

impl<Fnc, Fut> TaskFn<Fnc, Fut>
where
    Fnc: FnMut(CancellationToken) -> Fut + Send + 'static,
    Fut: Future<Output = Result<(), TaskError>> + Send + 'static,
{
    pub fn new(name: impl Into<Cow<'static, str>>, func: Fnc) -> Self {
        Self {
            name: name.into(),
            func: Mutex::new(func),
        }
    }

    pub fn arc(name: impl Into<Cow<'static, str>>, func: Fnc) -> TaskRef {
        std::sync::Arc::new(Self::new(name, func))
    }
}

#[async_trait]
impl<Fnc, Fut> Task for TaskFn<Fnc, Fut>
where
    Fnc: FnMut(CancellationToken) -> Fut + Send + 'static,
    Fut: Future<Output = Result<(), TaskError>> + Send + 'static,
{
    fn name(&self) -> &str {
        &self.name
    }

    async fn run(&self, ctx: CancellationToken) -> Result<(), TaskError> {
        let fut = {
            let mut f = self.func.lock().map_err(|_| TaskError::Fatal {
                error: "mutex poisoned".into(),
            })?;
            (f)(ctx)
        };
        fut.await
    }
}
