use lambda_runtime::{LambdaEvent, service_fn};
use lambda_utils::{emit_rustc_metric, set_up_logger};
use log_stream_gc::{APP_NAME, gc_log_streams};
use serde_json::{Value, json};
use std::error::Error;

type LambdaError = Box<dyn Error + Send + Sync + 'static>;

#[tokio::main]
async fn main() -> Result<(), LambdaError> {
    let func = service_fn(function);
    lambda_runtime::run(func).await?;
    Ok(())
}

async fn function(_event: LambdaEvent<Value>) -> Result<Value, LambdaError> {
    set_up_logger(APP_NAME, module_path!(), false)?;
    emit_rustc_metric(APP_NAME).await;

    gc_log_streams(None, false).await?;

    Ok(json!({}))
}
