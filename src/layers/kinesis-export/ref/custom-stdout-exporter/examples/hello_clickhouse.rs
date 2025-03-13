use opentelemetry::trace::{Tracer, TracerProvider};
use opentelemetry_sdk::{trace::SdkTracerProvider, Resource};
use custom_stdout_exporter::{CustomStdoutSpanExporter, OutputFormat};

#[tokio::main]
async fn main() {
    // Create a new stdout exporter with ClickHouse format
    let exporter = CustomStdoutSpanExporter::with_format(OutputFormat::ClickHouse);

    // Create a new tracer provider with batch export
    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(Resource::builder().with_service_name("hello-world").build())
        .build();

    // Create a tracer
    let tracer = provider.tracer("hello-world");

    // Create spans
    tracer.in_span("parent-operation", |_cx| {
        // Create child spans
        tracer.in_span("child1", |_| {});
        tracer.in_span("child2", |_| {});
    });

    // Shut down the provider to ensure all spans are exported
    let _ = provider.shutdown();
}
