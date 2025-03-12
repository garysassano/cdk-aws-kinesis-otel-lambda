use aws_lambda_events::apigw::ApiGatewayV2httpRequest;
use lambda_lw_http_router::{define_router, route};
use lambda_otel_utils::{OpenTelemetryFaasTrigger, OpenTelemetryLayer};
use lambda_runtime::{service_fn, Error, LambdaEvent, Runtime};
use opentelemetry::trace::TracerProvider;
use opentelemetry_sdk::{trace::SdkTracerProvider, Resource};
use custom_stdout_exporter::{OtlpStdoutSpanExporter, OutputFormat};
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
#[route(path = "/hello/{name}")]
async fn handle_hello(ctx: RouteContext) -> Result<Value, Error> {
    let name = ctx.params.get("name").map(|s| s.as_str()).unwrap();
    Ok(json!({
        "message": format!("Hello, {}!", name)
    }))
}

// Lambda function entry point
#[tokio::main]
async fn main() -> Result<(), Error> {
    // Initialize tracer with otlp-stdout-span-exporter
    let exporter = OtlpStdoutSpanExporter::with_format(OutputFormat::ClickHouse);
    let tracer_provider = Arc::new(
        SdkTracerProvider::builder()
            .with_batch_exporter(exporter)
            .with_resource(Resource::builder().with_service_name("hello-world").build())
            .build(),
    );
    let _tracer = tracer_provider.tracer("hello-world");

    let state = Arc::new(AppState {});
    let router = Arc::new(RouterBuilder::from_registry().build());

    let lambda = move |event: LambdaEvent<ApiGatewayV2httpRequest>| {
        let state = state.clone();
        let router = router.clone();
        async move { router.handle_request(event, state).await }
    };

    // Create a reference to the provider for shutdown
    let provider_ref = Arc::clone(&tracer_provider);

    // Use OpenTelemetryLayer for creating the initial span
    // and signal completion by shutting down the provider
    let runtime = Runtime::new(service_fn(lambda)).layer(
        OpenTelemetryLayer::new(move || {
            // Force flush and shutdown the provider when the Lambda execution ends
            let _ = provider_ref.shutdown();
        })
        .with_trigger(OpenTelemetryFaasTrigger::Http),
    );

    runtime.run().await
}
