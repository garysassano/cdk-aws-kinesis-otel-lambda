use aws_sdk_kinesis::primitives::Blob;
use aws_sdk_kinesis::Client as KinesisClient;
use lambda_extension::{
    service_fn, tracing, Error, Extension, LambdaTelemetry, LambdaTelemetryRecord, SharedService,
};
use std::env;
use std::sync::Arc;
use uuid::Uuid;
use serde_json::Value;

// Kinesis limit for a single record
const MAX_RECORD_SIZE_BYTES: usize = 1_048_576; // 1MB per record

#[derive(Debug)]
struct ExtensionConfig {
    kinesis_stream_name: String,
}

impl ExtensionConfig {
    fn from_env() -> Result<Self, Error> {
        Ok(Self {
            kinesis_stream_name: env::var("OTLP_STDOUT_KINESIS_STREAM_NAME")
                .map_err(|e| Error::from(format!("Failed to get stream name: {}", e)))?,
        })
    }
}

struct TelemetryHandler {
    kinesis_client: Arc<KinesisClient>,
    stream_name: String,
}

impl TelemetryHandler {
    async fn new() -> Result<Self, Error> {
        let config = ExtensionConfig::from_env()?;

        let aws_config = aws_config::from_env().load().await;

        let kinesis_client = Arc::new(KinesisClient::new(&aws_config));

        Ok(Self {
            kinesis_client,
            stream_name: config.kinesis_stream_name,
        })
    }

    /// Check if the record is in ClickHouse format
    fn is_clickhouse_format(&self, json: &Value) -> bool {
        // Check if it's an array
        if let Some(array) = json.as_array() {
            if array.is_empty() {
                return false;
            }
            
            // Check for required fields in the first element
            if let Some(first_item) = array.get(0) {
                // Check for essential ClickHouse format fields
                let has_trace_id = first_item.get("TraceId").is_some();
                let has_span_id = first_item.get("SpanId").is_some();
                let has_span_name = first_item.get("SpanName").is_some();
                let has_service_name = first_item.get("ServiceName").is_some();
                
                if has_trace_id && has_span_id && has_span_name && has_service_name {
                    tracing::debug!("Found valid ClickHouse format span data");
                    return true;
                }
            }
        }
        false
    }

    /// Check if the record is in OTLP/JSON format
    fn is_otlp_format(&self, json: &Value) -> bool {
        // Check if it has the resourceSpans field which is specific to OpenTelemetry
        if let Some(resource_spans) = json.get("resourceSpans") {
            // Validate it's an array with at least one element
            if let Some(resource_spans_array) = resource_spans.as_array() {
                if resource_spans_array.is_empty() {
                    return false;
                }
                
                // Check for required fields in the first resourceSpan
                if let Some(first_resource_span) = resource_spans_array.get(0) {
                    // Check for resource field
                    if !first_resource_span.get("resource").is_some() {
                        return false;
                    }
                    
                    // Check for scopeSpans field
                    if let Some(scope_spans) = first_resource_span.get("scopeSpans") {
                        if scope_spans.is_array() && !scope_spans.as_array().unwrap().is_empty() {
                            // Check for spans field in the first scopeSpan
                            if let Some(first_scope_span) = scope_spans.as_array().unwrap().get(0) {
                                if let Some(spans) = first_scope_span.get("spans") {
                                    if spans.is_array() {
                                        tracing::debug!("Found valid OTLP/JSON format span data");
                                        return true;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        false
    }

    /// Check if the record is a valid OpenTelemetry span in either format
    fn is_valid_otel_span(&self, record: &str) -> bool {
        match serde_json::from_str::<Value>(record) {
            Ok(json) => {
                // Check for either OTLP/JSON or ClickHouse format
                if self.is_otlp_format(&json) || self.is_clickhouse_format(&json) {
                    return true;
                }
                
                tracing::debug!("JSON doesn't match any known OpenTelemetry span format");
                false
            }
            Err(e) => {
                tracing::debug!("Failed to parse record as JSON: {}", e);
                false
            }
        }
    }

    async fn send_record(&self, record: String) -> Result<(), Error> {
        // Only process valid OpenTelemetry spans
        if !self.is_valid_otel_span(&record) {
            return Ok(());
        }

        tracing::info!("Processing OpenTelemetry span");

        if record.len() > MAX_RECORD_SIZE_BYTES {
            tracing::warn!(
                "Record size {} bytes exceeds maximum size of {} bytes, skipping",
                record.len(),
                MAX_RECORD_SIZE_BYTES
            );
            return Ok(());
        }

        self.kinesis_client
            .put_record()
            .stream_name(&self.stream_name)
            .data(Blob::new(record))
            .partition_key(Uuid::new_v4().to_string())
            .send()
            .await
            .map_err(|e| Error::from(format!("Failed to send record to Kinesis: {}", e)))?;

        tracing::info!("Successfully sent OpenTelemetry span to Kinesis");
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    tracing::init_default_subscriber();
    tracing::info!("Starting Rust extension for OpenTelemetry spans");

    let handler = Arc::new(TelemetryHandler::new().await?);
    let handler_clone = handler.clone();

    let telemetry_processor =
        SharedService::new(service_fn(move |events: Vec<LambdaTelemetry>| {
            let handler = handler_clone.clone();
            async move {
                for event in events {
                    if let LambdaTelemetryRecord::Function(record) = event.record {
                        handler.send_record(record).await?;
                    }
                }
                Ok::<(), Error>(())
            }
        }));

    Extension::new()
        .with_telemetry_processor(telemetry_processor)
        .with_telemetry_types(&["function"])
        .run()
        .await?;

    Ok(())
}
