//! Test-only lifecycle observations; never records credentials or HTTP data.
use agentcpm_iface::{BoxFuture, env::http_pull::*};
use std::{
    io::Write,
    path::PathBuf,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

#[derive(Clone)]
pub struct Trace(Arc<std::sync::Mutex<PathBuf>>);
impl Trace {
    pub fn new(path: PathBuf) -> Self {
        Self(Arc::new(std::sync::Mutex::new(path)))
    }
    fn write(&self, job: &str, stage: &str, detail: &str) {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|t| t.as_millis())
            .unwrap_or_default();
        if let Ok(path) = self.0.lock()
            && let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&*path)
        {
            let _ = writeln!(
                file,
                "{}",
                serde_json::json!({"time_ms":stamp,"job":job,"stage":stage,"detail":detail})
            );
        }
    }
}
struct Guard {
    trace: Trace,
    job: String,
    stage: &'static str,
}
impl Drop for Guard {
    fn drop(&mut self) {
        self.trace.write(&self.job, self.stage, "drop");
    }
}
pub struct Port {
    pub inner: Arc<dyn HttpPullTransport>,
    pub trace: Trace,
}
impl HttpPullTransport for Port {
    fn start(
        &self,
        request: HttpPullRequest,
        context: HttpPullContext,
    ) -> Result<HttpExchange, HttpFailure> {
        let job = context.job_id.clone();
        self.trace.write(&job, "start", "enter");
        let exchange = match self.inner.start(request, context) {
            Ok(v) => v,
            Err(e) => {
                self.trace.write(&job, "start", "error");
                return Err(e);
            }
        };
        self.trace.write(&job, "start", "ok");
        let trace = self.trace.clone();
        let head_job = job.clone();
        let guard = Guard {
            trace: trace.clone(),
            job: head_job.clone(),
            stage: "head_future",
        };
        let response = Box::pin(async move {
            let _guard = guard;
            trace.write(&head_job, "head_future", "poll/start");
            match exchange.response.await {
                Ok(response) => {
                    trace.write(&head_job, "head_future", "ok");
                    Ok(HttpPullResponse {
                        head: response.head,
                        body: Box::pin(Body {
                            inner: response.body,
                            trace,
                            job: head_job,
                        }),
                    })
                }
                Err(error) => {
                    trace.write(&head_job, "head_future", "error");
                    Err(error)
                }
            }
        });
        Ok(HttpExchange {
            response,
            control: Arc::new(Control {
                inner: exchange.control,
                trace: self.trace.clone(),
                job,
            }),
        })
    }
    fn close_scope(
        &self,
        context: HttpPullContext,
    ) -> Result<Arc<dyn HttpScopeCleanup>, HttpFailure> {
        let job = context.job_id.clone();
        self.trace.write(&job, "close_scope", "enter");
        match self.inner.close_scope(context) {
            Ok(inner) => {
                self.trace.write(&job, "close_scope", "ok");
                Ok(Arc::new(Cleanup {
                    inner,
                    trace: self.trace.clone(),
                    job,
                }))
            }
            Err(e) => {
                self.trace.write(&job, "close_scope", "error");
                Err(e)
            }
        }
    }
}
struct Body {
    inner: Pin<Box<dyn HttpBody>>,
    trace: Trace,
    job: String,
}
impl HttpBody for Body {
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<HttpFrame, HttpFailure>>> {
        let result = self.inner.as_mut().poll_frame(cx);
        if let Poll::Ready(ref item) = result {
            self.trace.write(
                &self.job,
                "body_poll",
                if item.is_none() {
                    "eof"
                } else if item.as_ref().is_some_and(Result::is_err) {
                    "error"
                } else {
                    "frame"
                },
            );
        }
        result
    }
}
impl Drop for Body {
    fn drop(&mut self) {
        self.trace.write(&self.job, "body", "drop");
    }
}
struct Control {
    inner: Arc<dyn HttpExchangeControl>,
    trace: Trace,
    job: String,
}
impl HttpExchangeControl for Control {
    fn stop_local(&self) {
        self.trace.write(&self.job, "control_stop", "enter");
        self.inner.stop_local();
        self.trace.write(&self.job, "control_stop", "exit");
    }
    fn snapshot(&self) -> HttpExchangeReceipt {
        self.inner.snapshot()
    }
    fn released(&self) -> BoxFuture<'_, HttpExchangeReceipt> {
        let inner = self.inner.released();
        let guard = Guard {
            trace: self.trace.clone(),
            job: self.job.clone(),
            stage: "control_released",
        };
        Box::pin(async move {
            let _guard = guard;
            self.trace
                .write(&self.job, "control_released", "poll/start");
            let r = inner.await;
            self.trace
                .write(&self.job, "control_released", &format!("{:?}", r.resources));
            r
        })
    }
}
impl Drop for Control {
    fn drop(&mut self) {
        self.trace.write(&self.job, "control", "drop");
    }
}
struct Cleanup {
    inner: Arc<dyn HttpScopeCleanup>,
    trace: Trace,
    job: String,
}
impl HttpScopeCleanup for Cleanup {
    fn resources(&self) -> HttpResources {
        self.inner.resources()
    }
    fn released(&self) -> BoxFuture<'_, HttpResources> {
        let inner = self.inner.released();
        let guard = Guard {
            trace: self.trace.clone(),
            job: self.job.clone(),
            stage: "scope_released",
        };
        Box::pin(async move {
            let _guard = guard;
            self.trace.write(&self.job, "scope_released", "poll/start");
            let r = inner.await;
            self.trace
                .write(&self.job, "scope_released", &format!("{:?}", r));
            r
        })
    }
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        self.trace.write(&self.job, "scope_cleanup", "drop");
    }
}
