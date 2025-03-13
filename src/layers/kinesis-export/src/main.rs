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

    /// Check if a single JSON object is a valid span
    fn is_valid_span_json(&self, json: &Value) -> bool {
        // Check for required fields in the ClickHouse span format
        // These are the essential fields from the ClickhouseSpan struct
        let has_timestamp = json.get("Timestamp").is_some();
        let has_trace_id = json.get("TraceId").is_some();
        let has_span_id = json.get("SpanId").is_some();
        
        // Additional fields that should be present in a complete span
        let has_span_name = json.get("SpanName").is_some();
        let has_span_kind = json.get("SpanKind").is_some();
        let has_service_name = json.get("ServiceName").is_some();
        
        // Check if it has the minimum required fields for a valid span
        if has_timestamp && has_trace_id && has_span_id {
            // Log more details about the span for debugging
            if has_span_name && has_service_name {
                let span_name = json.get("SpanName").and_then(|v| v.as_str()).unwrap_or("unknown");
                let service_name = json.get("ServiceName").and_then(|v| v.as_str()).unwrap_or("unknown");
                tracing::debug!("Found valid ClickHouse span: '{}' from service '{}'", span_name, service_name);
            } else {
                tracing::debug!("Found valid ClickHouse span with essential fields");
            }
            return true;
        }
        
        false
    }
    
    /// Process and send spans from JSONEachRow format
    async fn process_json_each_row(&self, record: &str) -> Result<bool, Error> {
        let lines: Vec<&str> = record.trim().split('\n').collect();
        if lines.is_empty() {
            return Ok(false);
        }
        
        let mut valid_spans_found = false;
        
        for line in lines {
            if line.trim().is_empty() {
                continue;
            }
            
            if let Ok(json) = serde_json::from_str::<Value>(line) {
                if self.is_valid_span_json(&json) {
                    valid_spans_found = true;
                    
                    // Send this individual span to Kinesis
                    if line.len() > MAX_RECORD_SIZE_BYTES {
                        tracing::warn!(
                            "Span size {} bytes exceeds maximum size of {} bytes, skipping",
                            line.len(),
                            MAX_RECORD_SIZE_BYTES
                        );
                        continue;
                    }
                    
                    // Extract span details for better logging
                    let span_id = json.get("SpanId").and_then(|v| v.as_str()).unwrap_or("unknown");
                    let trace_id = json.get("TraceId").and_then(|v| v.as_str()).unwrap_or("unknown");
                    
                    tracing::info!(
                        "Processing span: ID={}, TraceID={}",
                        span_id,
                        trace_id
                    );
                    
                    self.kinesis_client
                        .put_record()
                        .stream_name(&self.stream_name)
                        .data(Blob::new(line))
                        .partition_key(Uuid::new_v4().to_string())
                        .send()
                        .await
                        .map_err(|e| Error::from(format!("Failed to send record to Kinesis: {}", e)))?;
                        
                    tracing::info!("Successfully sent span to Kinesis: ID={}", span_id);
                }
            } else {
                tracing::debug!("Failed to parse line as JSON: {}", line);
            }
        }
        
        Ok(valid_spans_found)
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
        // First check if it contains valid spans in JSONEachRow format
        if self.contains_valid_spans(record) {
            return true;
        }
        
        // If not, try parsing as a single JSON object for OTLP format
        match serde_json::from_str::<Value>(record) {
            Ok(json) => {
                if self.is_otlp_format(&json) {
                    return true;
                }
                
                // Also check if the single JSON object itself is a valid span
                if self.is_valid_span_json(&json) {
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
        // First, try to process as JSONEachRow format (multiple JSON objects, one per line)
        if let Ok(true) = self.process_json_each_row(&record).await {
            return Ok(());
        }
        
        // If not JSONEachRow or no valid spans found, try as a single JSON object
        if let Ok(json) = serde_json::from_str::<Value>(&record) {
            // Check if it's a single valid span
            if self.is_valid_span_json(&json) {
                if record.len() > MAX_RECORD_SIZE_BYTES {
                    tracing::warn!(
                        "Record size {} bytes exceeds maximum size of {} bytes, skipping",
                        record.len(),
                        MAX_RECORD_SIZE_BYTES
                    );
                    return Ok(());
                }
                
                let span_id = json.get("SpanId").and_then(|v| v.as_str()).unwrap_or("unknown");
                tracing::info!("Processing single span: ID={}", span_id);
                
                self.kinesis_client
                    .put_record()
                    .stream_name(&self.stream_name)
                    .data(Blob::new(record))
                    .partition_key(Uuid::new_v4().to_string())
                    .send()
                    .await
                    .map_err(|e| Error::from(format!("Failed to send record to Kinesis: {}", e)))?;
                    
                tracing::info!("Successfully sent span to Kinesis: ID={}", span_id);
                return Ok(());
            }
            
            // Check if it's OTLP format
            if self.is_otlp_format(&json) {
                if record.len() > MAX_RECORD_SIZE_BYTES {
                    tracing::warn!(
                        "Record size {} bytes exceeds maximum size of {} bytes, skipping",
                        record.len(),
                        MAX_RECORD_SIZE_BYTES
                    );
                    return Ok(());
                }
                
                tracing::info!("Processing span in OTLP format");
                
                self.kinesis_client
                    .put_record()
                    .stream_name(&self.stream_name)
                    .data(Blob::new(record))
                    .partition_key(Uuid::new_v4().to_string())
                    .send()
                    .await
                    .map_err(|e| Error::from(format!("Failed to send record to Kinesis: {}", e)))?;
                    
                tracing::info!("Successfully sent OTLP format span to Kinesis");
                return Ok(());
            }
        }
        
        // If we get here, the record doesn't match any known format
        tracing::debug!("Record does not contain valid OpenTelemetry spans, skipping");
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
