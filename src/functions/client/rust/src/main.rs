use aws_lambda_events::apigw::ApiGatewayV2httpRequest;
use lambda_lw_http_router::{define_router, route};
use lambda_otel_utils::{
    OpenTelemetryFaasTrigger, OpenTelemetryLayer, OpenTelemetrySubscriberBuilder,
};
use lambda_runtime::{service_fn, Error, LambdaEvent, Runtime};
use opentelemetry::trace::TracerProvider;
use opentelemetry_sdk::{trace::SdkTracerProvider, Resource};
use otlp_stdout_span_exporter::{OtlpStdoutSpanExporter, OutputFormat};
use serde_json::{json, Value};
use std::sync::Arc;

// Define your application state
#[derive(Clone)]
struct AppState {
    // your state fields here
}

// Set up the router
define_router!(event = ApiGatewayV2httpRequest, state = AppState);

// Define route handlers
#[route(path = "/")]
async fn handle_root(_ctx: RouteContext) -> Result<Value, Error> {
    Ok(json!({
        "statusCode": 200,
        "headers": {"Content-Type": "application/json"},
        "body": json!({
            "message": "Hello from Lambda!"
        })
    }))
}
#[route(path = "/hello/{name}")]
async fn handle_hello(ctx: RouteContext) -> Result<Value, Error> {
    let name = ctx.params.get("name").map(|s| s.as_str()).unwrap();
    Ok(json!({
        "statusCode": 200,
        "headers": {"Content-Type": "application/json"},
        "body": json!({
            "message": format!("Hello, {}!", name)
        })
    }))
}

// Lambda function entrypoint
#[tokio::main]
async fn main() -> Result<(), Error> {
    // Initialize tracer with otlp-stdout-span-exporter
    let exporter = OtlpStdoutSpanExporter::with_format(OutputFormat::ClickHouse);

    // Create a new tracer provider with batch export
    let tracer_provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(Resource::builder().with_service_name("hello-world").build())
        .build();

    // Get a tracer from the provider
    let _tracer = tracer_provider.tracer("hello-world");

    // Initialize the OpenTelemetry subscriber
    OpenTelemetrySubscriberBuilder::new()
        .with_env_filter(true)
        .with_service_name("hello-world")
        .with_json_format(true)
        .init()?;

    let state = Arc::new(AppState {});
    let router = Arc::new(RouterBuilder::from_registry().build());

    let lambda = move |event: LambdaEvent<ApiGatewayV2httpRequest>| {
        let state = Arc::clone(&state);
        let router = Arc::clone(&router);
        async move { router.handle_request(event, state).await }
    };

    // Create a reference to the provider for the OpenTelemetryLayer
    let provider_ref = Arc::new(tracer_provider);

    let runtime = Runtime::new(service_fn(lambda)).layer(
        OpenTelemetryLayer::new(move || {
            // Force flush the provider when the Lambda execution ends
            let provider = Arc::clone(&provider_ref);
            let _ = provider.shutdown();
        })
        .with_trigger(OpenTelemetryFaasTrigger::Http),
    );

    runtime.run().await
}
