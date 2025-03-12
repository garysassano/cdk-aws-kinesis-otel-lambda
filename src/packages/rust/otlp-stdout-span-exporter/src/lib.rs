//! A span exporter that writes OpenTelemetry spans to stdout in OTLP format as plain JSON.
//!
//! This crate provides an implementation of OpenTelemetry's [`SpanExporter`] that writes spans to stdout
//! in OTLP (OpenTelemetry Protocol) format as plain JSON. It is particularly useful in serverless environments like
//! AWS Lambda where writing to stdout is a common pattern for exporting telemetry data.
//!
//! # Features
//!
//! - Outputs OTLP data directly as plain JSON
//! - Optionally outputs in ClickHouse-compatible format
//! - Simple, lightweight implementation
//! - No compression or encoding overhead
//!
//! # Example
//!
//! ```rust,no_run
//! use opentelemetry::trace::{Tracer, TracerProvider};
//! use opentelemetry_sdk::{trace::SdkTracerProvider, Resource};
//! use otlp_stdout_span_exporter::OtlpStdoutSpanExporter;
//!
//! #[tokio::main]
//! async fn main() {
//!     // Create a new stdout exporter
//!     let exporter = OtlpStdoutSpanExporter::new();
//!
//!     // Create a new tracer provider with batch export
//!     let provider = SdkTracerProvider::builder()
//!         .with_batch_exporter(exporter)
//!         .with_resource(Resource::builder().build())
//!         .build();
//!
//!     // Create a tracer
//!     let tracer = provider.tracer("my-service");
//!
//!     // Create spans
//!     tracer.in_span("parent-operation", |_cx| {
//!         println!("Doing work...");
//!         
//!         // Create nested spans
//!         tracer.in_span("child-operation", |_cx| {
//!             println!("Doing more work...");
//!         });
//!     });
//!     
//!     // Shut down the provider
//!     let _ = provider.shutdown();
//! }
//! ```
//!
//! # Output Format
//!
//! The exporter outputs the OTLP data directly as JSON:
//!
//! ```json
//! {
//!   "resourceSpans": [
//!     {
//!       "resource": {
//!         "attributes": [
//!           {
//!             "key": "service.name",
//!             "value": {
//!               "stringValue": "my-service"
//!             }
//!           }
//!         ]
//!       },
//!       "scopeSpans": [
//!         {
//!           "scope": {
//!             "name": "my-library",
//!             "version": "1.0.0"
//!           },
//!           "spans": [
//!             {
//!               "traceId": "...",
//!               "spanId": "...",
//!               "name": "my-span",
//!               "kind": "SPAN_KIND_INTERNAL",
//!               "startTimeUnixNano": "...",
//!               "endTimeUnixNano": "..."
//!             }
//!           ]
//!         }
//!       ]
//!     }
//!   ]
//! }
//! ```

mod clickhouse_formatter;

use async_trait::async_trait;
use futures_util::future::BoxFuture;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::transform::common::tonic::ResourceAttributesWithSchema;
use opentelemetry_proto::transform::trace::tonic::group_spans_by_resource_and_scope;
use opentelemetry_sdk::resource::Resource;
use opentelemetry_sdk::{
    error::OTelSdkError,
    trace::{SpanData, SpanExporter},
};
#[cfg(test)]
use std::sync::Mutex;
use std::{result::Result, sync::Arc};

/// Output format options for the exporter
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// Standard OTLP JSON format
    OtlpJson,
    /// ClickHouse-compatible format
    ClickHouse,
}

impl Default for OutputFormat {
    fn default() -> Self {
        Self::OtlpJson
    }
}

/// Trait for output handling
///
/// This trait defines the interface for writing output lines. It is implemented
/// by both the standard output handler and test output handler.
trait Output: Send + Sync + std::fmt::Debug {
    /// Writes a single line of output
    ///
    /// # Arguments
    ///
    /// * `line` - The line to write
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` if the write was successful, or a `TraceError` if it failed
    fn write_line(&self, line: &str) -> Result<(), OTelSdkError>;
}

/// Standard output implementation that writes to stdout
#[derive(Debug, Default)]
struct StdOutput;

impl Output for StdOutput {
    fn write_line(&self, line: &str) -> Result<(), OTelSdkError> {
        println!("{}", line);
        Ok(())
    }
}

/// Test output implementation that captures to a buffer
#[cfg(test)]
#[derive(Debug, Default)]
struct TestOutput {
    buffer: Arc<Mutex<Vec<String>>>,
}

#[cfg(test)]
impl TestOutput {
    fn new() -> Self {
        Self {
            buffer: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn get_output(&self) -> Vec<String> {
        self.buffer.lock().unwrap().clone()
    }
}

#[cfg(test)]
impl Output for TestOutput {
    fn write_line(&self, line: &str) -> Result<(), OTelSdkError> {
        self.buffer.lock().unwrap().push(line.to_string());
        Ok(())
    }
}

/// A span exporter that writes spans to stdout in OTLP format as plain JSON
///
/// This exporter implements the OpenTelemetry [`SpanExporter`] trait and writes spans
/// to stdout in OTLP format as plain JSON.
///
/// # Example
///
/// ```rust,no_run
/// use opentelemetry_sdk::runtime;
/// use otlp_stdout_span_exporter::{OtlpStdoutSpanExporter, OutputFormat};
///
/// // Create a new exporter with default OTLP format
/// let exporter = OtlpStdoutSpanExporter::new();
///
/// // Or create with ClickHouse format
/// let clickhouse_exporter = OtlpStdoutSpanExporter::with_format(OutputFormat::ClickHouse);
/// ```
#[derive(Debug)]
pub struct OtlpStdoutSpanExporter {
    /// Optional resource to be included with all spans
    resource: Option<Resource>,
    /// Output implementation (stdout or test buffer)
    output: Arc<dyn Output>,
    /// Output format
    format: OutputFormat,
}

impl Default for OtlpStdoutSpanExporter {
    fn default() -> Self {
        Self::new()
    }
}

impl OtlpStdoutSpanExporter {
    /// Creates a new exporter with default OTLP format
    ///
    /// # Example
    ///
    /// ```rust
    /// use otlp_stdout_span_exporter::OtlpStdoutSpanExporter;
    ///
    /// let exporter = OtlpStdoutSpanExporter::new();
    /// ```
    pub fn new() -> Self {
        Self {
            resource: None,
            output: Arc::new(StdOutput),
            format: OutputFormat::OtlpJson,
        }
    }

    /// Creates a new exporter with the specified output format
    ///
    /// # Example
    ///
    /// ```rust
    /// use otlp_stdout_span_exporter::{OtlpStdoutSpanExporter, OutputFormat};
    ///
    /// let exporter = OtlpStdoutSpanExporter::with_format(OutputFormat::ClickHouse);
    /// ```
    pub fn with_format(format: OutputFormat) -> Self {
        Self {
            resource: None,
            output: Arc::new(StdOutput),
            format,
        }
    }

    #[cfg(test)]
    fn with_test_output() -> (Self, Arc<TestOutput>) {
        let output = Arc::new(TestOutput::new());
        let exporter = Self {
            resource: None,
            output: output.clone() as Arc<dyn Output>,
            format: OutputFormat::OtlpJson,
        };
        (exporter, output)
    }

    #[cfg(test)]
    fn with_test_output_and_format(format: OutputFormat) -> (Self, Arc<TestOutput>) {
        let output = Arc::new(TestOutput::new());
        let exporter = Self {
            resource: None,
            output: output.clone() as Arc<dyn Output>,
            format,
        };
        (exporter, output)
    }
}

#[async_trait]
impl SpanExporter for OtlpStdoutSpanExporter {
    /// Export spans to stdout in the configured format
    ///
    /// This function converts spans to the specified format and outputs them directly as JSON.
    ///
    /// # Arguments
    ///
    /// * `batch` - A vector of spans to export
    ///
    /// # Returns
    ///
    /// Returns a resolved future with `Ok(())` if the export was successful, or a `TraceError` if it failed
    fn export(&mut self, batch: Vec<SpanData>) -> BoxFuture<'static, Result<(), OTelSdkError>> {
        // Do all work synchronously
        let result = (|| {
            // Convert spans to OTLP format
            let resource = self
                .resource
                .clone()
                .unwrap_or_else(|| opentelemetry_sdk::Resource::builder_empty().build());
            let resource_attrs = ResourceAttributesWithSchema::from(&resource);
            let resource_spans = group_spans_by_resource_and_scope(batch, &resource_attrs);
            let request = ExportTraceServiceRequest { resource_spans };

            // Convert request to JSON
            let json_str = serde_json::to_string(&request)
                .map_err(|e| OTelSdkError::InternalFailure(e.to_string()))?;

            // Transform to ClickHouse format if needed
            match self.format {
                OutputFormat::OtlpJson => {
                    // Write OTLP JSON directly
                    self.output.write_line(&json_str)?;
                }
                OutputFormat::ClickHouse => {
                    // Transform to ClickHouse format and write
                    let clickhouse_json =
                        clickhouse_formatter::transform_otlp_to_clickhouse(&json_str)
                            .map_err(|e| OTelSdkError::InternalFailure(e.to_string()))?;
                    self.output.write_line(&clickhouse_json)?;
                }
            }

            Ok(())
        })();

        // Return a resolved future with the result
        Box::pin(std::future::ready(result))
    }

    /// Shuts down the exporter
    ///
    /// This is a no-op for stdout export as no cleanup is needed.
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` as there is nothing to clean up.
    fn shutdown(&mut self) -> Result<(), OTelSdkError> {
        Ok(())
    }

    /// Force flushes any pending spans
    ///
    /// This is a no-op for stdout export as spans are written immediately.
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` as there is nothing to flush.
    fn force_flush(&mut self) -> Result<(), OTelSdkError> {
        Ok(())
    }

    /// Sets the resource for this exporter.
    ///
    /// This method stores a clone of the provided resource to be used when exporting spans.
    /// The resource represents the entity producing telemetry and will be included in the
    /// exported trace data.
    ///
    /// # Arguments
    ///
    /// * `resource` - The resource to associate with this exporter
    fn set_resource(&mut self, resource: &opentelemetry_sdk::Resource) {
        self.resource = Some(<opentelemetry_sdk::Resource as Into<Resource>>::into(
            resource.clone(),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::trace::{SpanKind, Status};
    use opentelemetry::{InstrumentationScope, KeyValue};
    use opentelemetry_sdk::trace::{SpanEvents, SpanLinks};
    use serde_json::Value;
    use std::time::SystemTime;

    fn create_test_span() -> SpanData {
        SpanData {
            span_context: opentelemetry::trace::SpanContext::empty_context(),
            parent_span_id: opentelemetry::trace::SpanId::INVALID,
            span_kind: SpanKind::Client,
            name: "test-span".into(),
            start_time: SystemTime::UNIX_EPOCH,
            end_time: SystemTime::UNIX_EPOCH,
            attributes: vec![KeyValue::new("test.key", "test-value")],
            dropped_attributes_count: 0,
            events: SpanEvents::default(),
            links: SpanLinks::default(),
            status: Status::Ok,
            instrumentation_scope: InstrumentationScope::builder("test-library")
                .with_version("1.0.0")
                .with_schema_url("https://opentelemetry.io/schema/1.0.0")
                .build(),
        }
    }

    #[tokio::test]
    async fn test_export() {
        let (mut exporter, output) = OtlpStdoutSpanExporter::with_test_output();
        let span = create_test_span();

        let result = exporter.export(vec![span]).await;
        assert!(result.is_ok());

        let output = output.get_output();
        assert_eq!(output.len(), 1);

        // Parse and verify it's valid JSON
        let json: Value = serde_json::from_str(&output[0]).unwrap();

        // Verify it contains the expected fields for OTLP JSON format
        assert!(json.get("resourceSpans").is_some());

        // Resource spans should be an array
        let resource_spans = json.get("resourceSpans").unwrap().as_array().unwrap();
        assert!(resource_spans.len() > 0);
    }

    #[tokio::test]
    async fn test_export_clickhouse_format() {
        let (mut exporter, output) =
            OtlpStdoutSpanExporter::with_test_output_and_format(OutputFormat::ClickHouse);
        let span = create_test_span();

        let result = exporter.export(vec![span]).await;
        assert!(result.is_ok());

        let output = output.get_output();
        assert_eq!(output.len(), 1);

        // Parse and verify it's valid JSON
        let json: Value = serde_json::from_str(&output[0]).unwrap();

        // Verify it's an array of spans in ClickHouse format
        assert!(json.is_array());

        if let Some(first_span) = json.as_array().unwrap().first() {
            // Check for ClickHouse format fields
            assert!(first_span.get("Timestamp").is_some());
            assert!(first_span.get("TraceId").is_some());
            assert!(first_span.get("SpanId").is_some());
            assert!(first_span.get("SpanName").is_some());
            assert!(first_span.get("SpanKind").is_some());
            assert!(first_span.get("ServiceName").is_some());
        }
    }

    #[tokio::test]
    async fn test_export_plain_json() {
        let (mut exporter, output) = OtlpStdoutSpanExporter::with_test_output();
        let span = create_test_span();

        let result = exporter.export(vec![span]).await;
        assert!(result.is_ok());

        let output = output.get_output();
        assert_eq!(output.len(), 1);

        // Parse and verify it's valid JSON
        let json: Value = serde_json::from_str(&output[0]).unwrap();

        // Verify it contains the expected fields for OTLP JSON format
        assert!(json.get("resourceSpans").is_some());

        // Resource spans should be an array
        let resource_spans = json.get("resourceSpans").unwrap().as_array().unwrap();
        assert!(resource_spans.len() > 0);

        // Verify it doesn't contain any of the wrapper fields
        assert!(json.get("__otel_otlp_stdout").is_none());
        assert!(json.get("source").is_none());
        assert!(json.get("endpoint").is_none());
        assert!(json.get("method").is_none());
        assert!(json.get("content-type").is_none());
        assert!(json.get("content-encoding").is_none());
        assert!(json.get("base64").is_none());
    }
}
