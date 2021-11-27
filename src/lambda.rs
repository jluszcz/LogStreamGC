use lambda_runtime::{handler_fn, Context};
use log_stream_gc::{gc_log_streams, set_up_logger};
use serde_json::{json, Value};
use std::error::Error;

type LambdaError = Box<dyn Error + Send + Sync + 'static>;

#[tokio::main]
async fn main() -> Result<(), LambdaError> {
    let func = handler_fn(function);
    lambda_runtime::run(func).await?;
    Ok(())
}

async fn function(_event: Value, _: Context) -> Result<Value, LambdaError> {
    set_up_logger(module_path!(), false)?;

    gc_log_streams(None, false).await?;

    Ok(json!({}))
}
