use crate::client::Client;
use crate::error::{Error, Result};
use crate::job::Job;
use crate::proto;
use crate::worker::{job_dispatcher, JobPoller, PollMessage};
use futures::future::BoxFuture;
use futures::{FutureExt, StreamExt};
use serde::Serialize;
use serde_json::json;
use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::{sync::mpsc, time::interval};

static DEFAULT_JOB_TIMEOUT: Duration = Duration::from_secs(5 * 60);
static DEFAULT_JOB_TIMEOUT_IN_MS: i64 = DEFAULT_JOB_TIMEOUT.as_millis() as i64;

static DEFAULT_JOB_WORKER_MAX_JOB_ACTIVE: u32 = 32;
static DEFAULT_JOB_WORKER_CONCURRENCY: u32 = 4;
static DEFAULT_JOB_WORKER_POLL_INTERVAL: Duration = Duration::from_millis(100);
static DEFAULT_JOB_WORKER_POLL_THRESHOLD: f32 = 0.3;
static REQUEST_TIMEOUT_OFFSET: Duration = Duration::from_secs(10);
static DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub(crate) struct JobHandler(Arc<dyn Fn(Client, Job) -> BoxFuture<'static, ()> + Send + Sync>);

impl JobHandler {
    pub(crate) fn call(&self, client: Client, job: Job) -> BoxFuture<'static, ()> {
        self.0(client, job)
    }
}

impl fmt::Debug for JobHandler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "JobHandler")
    }
}

/// Configuration for an asynchronous worker process.
#[derive(Debug)]
pub struct JobWorkerBuilder {
    client: Client,
    handler: Option<JobHandler>,
    concurrency: u32,
    poll_interval: Duration,
    poll_threshold: f32,
    request: proto::ActivateJobsRequest,
    request_timeout: Duration,
}

impl JobWorkerBuilder {
    /// Create a new job worker builder.
    pub fn new(client: Client) -> Self {
        JobWorkerBuilder {
            client,
            handler: None,
            concurrency: DEFAULT_JOB_WORKER_CONCURRENCY,
            poll_interval: DEFAULT_JOB_WORKER_POLL_INTERVAL,
            poll_threshold: DEFAULT_JOB_WORKER_POLL_THRESHOLD,
            request: proto::ActivateJobsRequest {
                r#type: String::new(),
                worker: String::from("default"),
                timeout: DEFAULT_JOB_TIMEOUT_IN_MS,
                max_jobs_to_activate: DEFAULT_JOB_WORKER_MAX_JOB_ACTIVE as i32,
                fetch_variable: Vec::new(),
                request_timeout: DEFAULT_REQUEST_TIMEOUT.as_millis() as i64,
            },
            request_timeout: DEFAULT_REQUEST_TIMEOUT + REQUEST_TIMEOUT_OFFSET,
        }
    }

    /// Set the job type of the worker.
    pub fn with_job_type<T: Into<String>>(mut self, job_type: T) -> Self {
        self.request.r#type = job_type.into();
        self
    }

    /// Set the worker name (mostly used for logging)
    pub fn with_worker_name<T: Into<String>>(mut self, worker: T) -> Self {
        self.request.worker = worker.into();
        self
    }

    /// Set the worker job timeout.
    ///
    /// See [the requesting jobs docs] for more details.
    ///
    /// [the requesting jobs docs]: https://docs.zeebe.io/basics/job-workers.html#requesting-jobs-from-the-broker
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.request.timeout = timeout.as_millis() as i64;
        self
    }

    /// Set the worker request timeout.
    ///
    /// See [the requesting jobs docs] for more details.
    ///
    /// [the requesting jobs docs]: https://docs.zeebe.io/basics/job-workers.html#requesting-jobs-from-the-broker
    pub fn with_request_timeout(mut self, request_timeout: Duration) -> Self {
        self.request.request_timeout = request_timeout.as_millis() as i64;
        self.request_timeout = request_timeout + REQUEST_TIMEOUT_OFFSET;
        self
    }

    /// Set the maximum jobs to activate at a time by the worker.
    pub fn with_max_jobs_active(mut self, max_jobs_active: u32) -> Self {
        self.request.max_jobs_to_activate = max_jobs_active as i32;
        self
    }

    /// Set the max number of jobs to run concurrently.
    pub fn with_concurrency(self, concurrency: u32) -> Self {
        JobWorkerBuilder {
            concurrency,
            ..self
        }
    }

    /// Set the handler function for the worker.
    pub fn with_handler<T, R>(self, handler: T) -> Self
    where
        T: Fn(Client, Job) -> R + Send + Sync + 'static,
        R: Future<Output = ()> + Send + 'static,
    {
        JobWorkerBuilder {
            handler: Some(JobHandler(Arc::new(move |client, job| {
                Box::pin(handler(client, job))
            }))),
            ..self
        }
    }

    /// Set a handler function that completes or fails the job based on the result
    /// rather than having to explicitly use the client to report job status.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use serde::{Deserialize, Serialize};
    /// use thiserror::Error;
    /// use zeebe::Client;
    /// use futures::future;
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let client = Client::new();
    ///
    /// // Given an app-specific error
    /// #[derive(Error, Debug)]
    /// enum MyError {
    ///     #[error("unknown error occurred")]
    ///     Unknown,
    /// }
    ///
    /// // And app-specific job data
    /// #[derive(Deserialize)]
    /// struct MyJobData {
    ///     my_property: String,
    ///     my_other_property: String,
    /// }
    ///
    /// // And app-specific job result
    /// #[derive(Serialize)]
    /// struct MyJobResult {
    ///     result: u32,
    /// }
    ///
    /// let job = client
    ///     .job_worker()
    ///     .with_job_type("my-job-type")
    ///     .with_auto_handler(move |_client: Client, my_job_data: MyJobData| {
    ///         future::ok::<_, MyError>(MyJobResult { result: 42 })
    ///     })
    ///     .spawn()
    ///     .await?;
    ///
    /// # Ok(())
    /// # }
    ///
    /// ```
    pub fn with_auto_handler<F, R, E, T, J>(self, handler: F) -> Self
    where
        F: Fn(Client, J) -> R + Send + Sync + 'static,
        R: Future<Output = std::result::Result<T, E>> + Send + 'static,
        E: std::error::Error,
        T: Serialize,
        J: serde::de::DeserializeOwned,
    {
        self.with_handler(move |client, job| {
            let job_key = job.key();
            match serde_json::from_str(job.variables_str()) {
                Ok(typed_job) => handler(client.clone(), typed_job)
                    .then(move |result| match result {
                        Ok(variables) => client
                            .complete_job()
                            .with_job_key(job_key)
                            .with_variables(json!(variables))
                            .send()
                            .map(|_| ())
                            .left_future(),
                        Err(err) => client
                            .fail_job()
                            .with_job_key(job_key)
                            .with_error_message(err.to_string())
                            .send()
                            .map(|_| ())
                            .right_future(),
                    })
                    .left_future(),
                Err(err) => client
                    .fail_job()
                    .with_job_key(job_key)
                    .with_error_message(format!(
                        "variables do not deserialize to expected type: {:?}",
                        err
                    ))
                    .send()
                    .map(|_| ())
                    .right_future(),
            }
        })
    }

    /// Set the list of variables to fetch as the job variables.
    ///
    /// By default all visible variables at the time of activation for the scope of
    /// the job will be returned.
    pub fn with_fetch_variables(mut self, fetch_variables: Vec<String>) -> Self {
        self.request.fetch_variable = fetch_variables;
        self
    }

    /// Start the worker as a future. To stop the worker, simply drop the future.
    pub async fn spawn(self) -> Result<()> {
        if self.request.r#type.is_empty() || self.handler.is_none() {
            return Err(Error::InvalidParameters(
                "`job_type` and `handler` must be set",
            ));
        }

        let (job_queue, job_queue_rx) = mpsc::channel(self.request.max_jobs_to_activate as usize);
        let (poll_queue, poll_rx) = mpsc::channel(32);
        let poll_interval = interval(self.poll_interval).map(|_| PollMessage::FetchJobs);
        let worker_name = self.request.worker.clone();
        let job_poller = JobPoller {
            client: self.client.clone(),
            request_timeout: Duration::from_millis(self.request.request_timeout as u64),
            request_in_progress: false,
            max_jobs_active: self.request.max_jobs_to_activate as u32,
            job_queue,
            message_sender: poll_queue.clone(),
            messages: Box::pin(futures::stream::select(poll_rx, poll_interval)),
            remaining: 0,
            threshold: (self.request.max_jobs_to_activate as f32 * self.poll_threshold).floor()
                as u32,
            request: self.request,
        };

        // Process work
        futures::join!(
            job_poller,
            job_dispatcher::run(
                job_queue_rx,
                poll_queue,
                self.concurrency as usize,
                self.handler.unwrap(),
                self.client.clone(),
                worker_name,
            )
        );

        Ok(())
    }
}
