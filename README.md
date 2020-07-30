# Zeebe Rust Client

A rust client for defining, orchestrating, and monitoring business processes
across microservices using [Zeebe].

## What is Zeebe?

[Zeebe] is a workflow engine for microservices orchestration. Zeebe ensures
that, once started, flows are always carried out fully, retrying steps in case
of failures. Along the way, Zeebe maintains a complete audit log so that the
progress of flows can be monitored. Zeebe is fault tolerant and scales
seamlessly to handle growing transaction volumes.

[Zeebe]: https://zeebe.io

## Example

```rust
use serde_json::json;
use zeebe::{Client, Job};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Create a zeebe client
    let client = Client::default();

    // Deploy a workflow
    client
        .deploy_workflow()
        .with_resource_file("examples/workflows/order-process.bpmn")
        .send()
        .await?;

    // Create a new workflow instance
    client
        .create_workflow_instance()
        .with_bpmn_process_id("order-process")
        .with_latest_version()
        .with_variables(json!({"orderId": 1234}))
        .send()
        .await?;

    // Process a job type within the workflow
    client
        .job_worker()
        .with_job_type("payment-service")
        .with_handler(handle_job)
        .run()
        .await?;

    Ok(())
}

async fn handle_job(client: Client, job: Job) {
    /// your job processing logic...

    let _ = client.complete_job().with_job_key(job.key()).send().await;
}
```

Or with job success and failure reported for you automatically from your
function result:

```rust
use serde::{Deserialize, Serialize};
use thiserror::Error;
use zeebe::Client;
use futures::future;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::new();

    // Given an app-specific error
    #[derive(Error, Debug)]
    enum MyError {
        #[error("unknown error occurred")]
        Unknown,
    }

    // And app-specific job data
    #[derive(Deserialize)]
    struct MyJobData {
        my_property: String,
        my_other_property: String,
    }

    // And app-specific job result
    #[derive(Serialize)]
    struct MyJobResult {
        result: u32,
    }

    // Async job handler function
    async fn handle_job(client: Client, data: MyJobData) -> Result<MyJobResult, MyError> {
       Ok(MyJobResult { result: 42 })
    }

    // You can run a worker from your function with results auto reported
    let job = client
        .job_worker()
        .with_job_type("my-job-type")
        .with_auto_handler(handle_job)
        .run()
        .await?;

    // OR you can run a closure and have the results auto reported
    let job = client
        .job_worker()
        .with_job_type("my-job-type")
        .with_auto_handler(|client: Client, my_job_data: MyJobData| {
            future::ok::<_, MyError>(MyJobResult { result: 42 })
        })
        .run()
        .await?;

    Ok(())
}
```
