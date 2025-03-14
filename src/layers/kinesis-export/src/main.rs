use aws_sdk_kinesis::primitives::Blob;
use aws_sdk_kinesis::Client as KinesisClient;
use lambda_extension::{
    service_fn, tracing, Error, Extension, LambdaTelemetry, LambdaTelemetryRecord, SharedService,
};
use serde_json::Value;
use std::env;
use std::sync::Arc;
use uuid::Uuid;

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
        // Simplified validation - just check for the essential trace fields
        // Check both uppercase and lowercase versions to be more flexible
        let has_trace_id = json.get("TraceId").is_some() || json.get("traceId").is_some();
        let has_span_id = json.get("SpanId").is_some() || json.get("spanId").is_some();

        // Log the field presence for debugging
        tracing::debug!(
            "Field presence: TraceId={}, SpanId={}",
            has_trace_id,
            has_span_id
        );

        // If it has both trace ID and span ID, consider it a valid span
        if has_trace_id && has_span_id {
            tracing::debug!("Found valid span with TraceId and SpanId");
            return true;
        }

        // Also check for fields in the Lambda platform format
        let has_time = json.get("time").is_some();
        let has_type = json.get("type").is_some();
        let has_record = json.get("record").is_some();

        // If it's a Lambda platform event, check if it contains trace information
        if has_time && has_type && has_record {
            tracing::debug!(
                "Found Lambda platform event: type={}",
                json.get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
            );

            // Check if the record contains trace information
            if let Some(record) = json.get("record") {
                if record.get("traceId").is_some() || record.get("TraceId").is_some() {
                    tracing::debug!("Found trace information in Lambda platform event record");
                    return true;
                }
            }
        }

        false
    }

    /// Process and send spans from JSONEachRow format
    async fn process_json_each_row(&self, record: &str) -> Result<bool, Error> {
        let lines: Vec<&str> = record.trim().split('\n').collect();
        if lines.is_empty() {
            return Ok(false);
        }

        tracing::debug!("Processing {} lines in JSONEachRow format", lines.len());
        let mut valid_spans_found = false;

        for line in lines {
            if line.trim().is_empty() {
                continue;
            }

            tracing::debug!("Processing line: {}", line);
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
                    let span_id = json
                        .get("SpanId")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    let trace_id = json
                        .get("TraceId")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    let duration = json.get("Duration").and_then(|v| v.as_u64()).unwrap_or(0);

                    tracing::info!(
                        "Processing span: ID={}, TraceID={}, Duration={} ns",
                        span_id,
                        trace_id,
                        duration
                    );

                    // Log the Kinesis stream name for debugging
                    tracing::debug!("Sending to Kinesis stream: {}", self.stream_name);

                    match self
                        .kinesis_client
                        .put_record()
                        .stream_name(&self.stream_name)
                        .data(Blob::new(line))
                        .partition_key(Uuid::new_v4().to_string())
                        .send()
                        .await
                    {
                        Ok(_) => {
                            tracing::info!("Successfully sent span to Kinesis: ID={}", span_id);
                        }
                        Err(e) => {
                            tracing::error!("Failed to send record to Kinesis: {}", e);
                            return Err(Error::from(format!(
                                "Failed to send record to Kinesis: {}",
                                e
                            )));
                        }
                    }
                } else {
                    tracing::debug!("Line is not a valid span");
                }
            } else {
                tracing::debug!("Failed to parse line as JSON: {}", line);
            }
        }

        Ok(valid_spans_found)
    }

    async fn send_record(&self, record: String) -> Result<(), Error> {
        // Log the raw record for debugging
        tracing::debug!("Received record: {}", record);

        // Check if the record is too large
        if record.len() > MAX_RECORD_SIZE_BYTES {
            tracing::warn!(
                "Record size {} bytes exceeds maximum size of {} bytes, skipping",
                record.len(),
                MAX_RECORD_SIZE_BYTES
            );
            return Ok(());
        }

        // First try to process as JSONEachRow format (multiple JSON objects, one per line)
        if let Ok(true) = self.process_json_each_row(&record).await {
            tracing::info!("Successfully processed record as JSONEachRow format");
            return Ok(());
        }

        // If not JSONEachRow, try as a single JSON object
        if let Ok(json) = serde_json::from_str::<Value>(&record) {
            // Try to extract a span ID for logging
            let span_id = json
                .get("SpanId")
                .or_else(|| json.get("spanId"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");

            let trace_id = json
                .get("TraceId")
                .or_else(|| json.get("traceId"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");

            tracing::info!(
                "Processing record with SpanId={}, TraceId={}",
                span_id,
                trace_id
            );

            // Log the Kinesis stream name for debugging
            tracing::debug!("Sending to Kinesis stream: {}", self.stream_name);

            // Send the record to Kinesis
            match self
                .kinesis_client
                .put_record()
                .stream_name(&self.stream_name)
                .data(Blob::new(record))
                .partition_key(Uuid::new_v4().to_string())
                .send()
                .await
            {
                Ok(_) => {
                    tracing::info!("Successfully sent record to Kinesis");
                    return Ok(());
                }
                Err(e) => {
                    tracing::error!("Failed to send record to Kinesis: {}", e);
                    return Err(Error::from(format!(
                        "Failed to send record to Kinesis: {}",
                        e
                    )));
                }
            }
        } else {
            // If it's not valid JSON, log it and try to send it anyway
            tracing::warn!("Record is not valid JSON, but will try to send it anyway");

            // Log the Kinesis stream name for debugging
            tracing::debug!("Sending to Kinesis stream: {}", self.stream_name);

            // Send the record to Kinesis
            match self
                .kinesis_client
                .put_record()
                .stream_name(&self.stream_name)
                .data(Blob::new(record))
                .partition_key(Uuid::new_v4().to_string())
                .send()
                .await
            {
                Ok(_) => {
                    tracing::info!("Successfully sent non-JSON record to Kinesis");
                    return Ok(());
                }
                Err(e) => {
                    tracing::error!("Failed to send record to Kinesis: {}", e);
                    return Err(Error::from(format!(
                        "Failed to send record to Kinesis: {}",
                        e
                    )));
                }
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    tracing::init_default_subscriber();
    tracing::info!("Starting Rust extension for OpenTelemetry spans");

    let handler = Arc::new(TelemetryHandler::new().await?);
    let handler_clone = handler.clone();

    // Log the Kinesis stream name at startup
    tracing::info!("Using Kinesis stream: {}", handler.stream_name);

    let telemetry_processor =
        SharedService::new(service_fn(move |events: Vec<LambdaTelemetry>| {
            let handler = handler_clone.clone();
            async move {
                tracing::debug!("Received {} telemetry events", events.len());
                for event in events {
                    if let LambdaTelemetryRecord::Function(record) = event.record {
                        match handler.send_record(record).await {
                            Ok(_) => {}
                            Err(e) => {
                                tracing::error!("Error processing record: {}", e);
                            }
                        }
                    } else {
                        tracing::debug!("Skipping non-function telemetry record");
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
