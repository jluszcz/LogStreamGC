use jluszcz_rust_utils::lambda;
use lambda_runtime::LambdaEvent;
use log_stream_gc::{APP_NAME, Config, gc_log_streams};
use serde_json::{Value, json};

#[tokio::main]
async fn main() -> Result<(), lambda_runtime::Error> {
    lambda::run(APP_NAME, module_path!(), false, function).await
}

async fn function(_event: LambdaEvent<Value>) -> Result<Value, lambda_runtime::Error> {
    gc_log_streams(None, Config::default(), false).await?;

    Ok(json!({}))
}
