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

    async fn send_record(&self, record: String) -> Result<(), Error> {
        // Add detailed debug logging to understand the record structure
        tracing::debug!(
            "Raw record (first 200 chars): {}",
            record.chars().take(200).collect::<String>()
        );

        // Check if the record is too large
        if record.len() > MAX_RECORD_SIZE_BYTES {
            tracing::warn!("Record too large ({} bytes), skipping", record.len());
            return Ok(());
        }

        // First try to process as JSONEachRow format (multiple JSON objects, one per line)
        if record.contains("\n") {
            let lines: Vec<&str> = record.trim().split('\n').collect();
            if !lines.is_empty() {
                tracing::debug!("Processing {} lines in JSONEachRow format", lines.len());
                let mut spans_sent = 0;

                for line in lines {
                    if line.trim().is_empty() {
                        continue;
                    }

                    // Only process lines that look like OpenTelemetry spans
                    if line.contains("\"TraceId\":") && line.contains("\"SpanId\":") {
                        if let Ok(json) = serde_json::from_str::<Value>(line) {
                            if let (Some(trace_id), Some(span_id)) = (
                                json.get("TraceId").and_then(|v| v.as_str()),
                                json.get("SpanId").and_then(|v| v.as_str()),
                            ) {
                                tracing::info!(
                                    "Found OpenTelemetry span: TraceId={}, SpanId={}",
                                    trace_id,
                                    span_id
                                );

                                // Send to Kinesis
                                if let Err(e) = self.send_to_kinesis(line.to_string()).await {
                                    tracing::error!("Failed to send span: {}", e);
                                    return Err(e);
                                }
                                spans_sent += 1;
                            }
                        }
                    }
                }

                if spans_sent > 0 {
                    tracing::info!("Sent {} OpenTelemetry spans to Kinesis", spans_sent);
                    return Ok(());
                }
            }
        }

        // Try to parse as a single JSON object
        if let Ok(json) = serde_json::from_str::<Value>(&record) {
            // CASE 1: Direct OpenTelemetry span with TraceId and SpanId at root level
            if let (Some(trace_id), Some(span_id)) = (
                json.get("TraceId").and_then(|v| v.as_str()),
                json.get("SpanId").and_then(|v| v.as_str()),
            ) {
                tracing::info!(
                    "Found OpenTelemetry span: TraceId={}, SpanId={}",
                    trace_id,
                    span_id
                );
                return self.send_to_kinesis(record).await;
            }

            // CASE 2: Structured log with embedded span in message field
            if json.get("fields").is_some() {
                if let Some(message) = json
                    .get("fields")
                    .and_then(|f| f.get("message"))
                    .and_then(|m| m.as_str())
                {
                    if message.contains("\"TraceId\":") && message.contains("\"SpanId\":") {
                        if let Ok(message_json) = serde_json::from_str::<Value>(message) {
                            if let (Some(trace_id), Some(span_id)) = (
                                message_json.get("TraceId").and_then(|v| v.as_str()),
                                message_json.get("SpanId").and_then(|v| v.as_str()),
                            ) {
                                tracing::info!("Extracted OpenTelemetry span from message: TraceId={}, SpanId={}", trace_id, span_id);
                                return self.send_to_kinesis(message.to_string()).await;
                            }
                        }
                    }
                }
            }
        }

        // If we get here, the record is not an OpenTelemetry span - skip it
        tracing::debug!("Skipping record (not an OpenTelemetry span)");
        Ok(())
    }

    // Helper method to send data to Kinesis
    async fn send_to_kinesis(&self, data: String) -> Result<(), Error> {
        match self
            .kinesis_client
            .put_record()
            .stream_name(&self.stream_name)
            .data(Blob::new(data))
            .partition_key(Uuid::new_v4().to_string())
            .send()
            .await
        {
            Ok(_) => {
                tracing::info!("Successfully sent span to Kinesis");
                Ok(())
            }
            Err(e) => {
                tracing::error!("Failed to send span to Kinesis: {}", e);
                Err(Error::from(format!(
                    "Failed to send span to Kinesis: {}",
                    e
                )))
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    // Set up logging with info level for our code, but warn for AWS libraries
    std::env::set_var("RUST_LOG", "info,aws_config=warn,aws_smithy_http=warn");
    tracing::init_default_subscriber();

    tracing::info!("Starting OpenTelemetry span exporter extension - only processing spans, ignoring regular logs");

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
                            Ok(_) => {
                                tracing::debug!("Successfully processed telemetry record");
                            }
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

    tracing::info!("Extension initialized, starting run");

    Extension::new()
        .with_telemetry_processor(telemetry_processor)
        .with_telemetry_types(&["function"])
        .run()
        .await?;

    tracing::info!("Extension run completed");

    Ok(())
}
