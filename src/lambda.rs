use jluszcz_rust_utils::lambda;
use lambda_runtime::{LambdaEvent, service_fn};
use log_stream_gc::{APP_NAME, gc_log_streams};
use serde_json::{Value, json};

#[tokio::main]
async fn main() -> Result<(), lambda_runtime::Error> {
    let func = service_fn(function);
    lambda_runtime::run(func).await?;
    Ok(())
}

async fn function(_event: LambdaEvent<Value>) -> Result<Value, lambda_runtime::Error> {
    lambda::init(APP_NAME, module_path!(), false).await?;

    gc_log_streams(false).await?;

    Ok(json!({}))
}
