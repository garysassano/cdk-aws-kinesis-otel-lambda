use aws_sdk_kinesis::primitives::Blob;
use aws_sdk_kinesis::Client as KinesisClient;
use jsonschema::JSONSchema;
use lambda_extension::{
    service_fn, tracing, Error, Extension, LambdaTelemetry, LambdaTelemetryRecord, SharedService,
};
use serde_json::{json, Value};
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

#[derive(Debug, PartialEq)]
enum SchemaType {
    Direct,  // First schema with direct TraceId, SpanId
    Nested,  // Second schema with nested record
    Unknown, // For cases where validation passes but we can't determine which schema
}

struct TelemetryHandler {
    kinesis_client: Arc<KinesisClient>,
    stream_name: String,
    compiled_schema: Arc<JSONSchema>,
    // Add individual schemas for testing
    direct_schema: Arc<JSONSchema>,
    nested_schema: Arc<JSONSchema>,
}

impl TelemetryHandler {
    async fn new() -> Result<Self, Error> {
        let config = ExtensionConfig::from_env()?;

        let aws_config = aws_config::from_env().load().await;

        let kinesis_client = Arc::new(KinesisClient::new(&aws_config));

        // Get the schema components
        let (direct_schema_value, nested_schema_value) = Self::get_schema_components();

        // Compile the individual schemas
        let direct_schema =
            JSONSchema::compile(&direct_schema_value).expect("Invalid direct schema definition");

        let nested_schema =
            JSONSchema::compile(&nested_schema_value).expect("Invalid nested schema definition");

        // Create the combined schema with oneOf
        let combined_schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "oneOf": [direct_schema_value, nested_schema_value]
        });

        let compiled_schema =
            JSONSchema::compile(&combined_schema).expect("Invalid JSON Schema definition");

        Ok(Self {
            kinesis_client,
            stream_name: config.kinesis_stream_name,
            compiled_schema: Arc::new(compiled_schema),
            direct_schema: Arc::new(direct_schema),
            nested_schema: Arc::new(nested_schema),
        })
    }

    // Return the two schema components as a tuple
    fn get_schema_components() -> (serde_json::Value, serde_json::Value) {
        // Common properties that should be validated in both formats
        let direct_schema = json!({
            "type": "object",
            "required": ["TraceId", "SpanId"],
            "properties": {
                "TraceId": {"type": "string", "minLength": 1},
                "SpanId": {"type": "string", "minLength": 1},
                "ParentSpanId": {"type": "string"},
                "SpanName": {"type": "string"},
                "Duration": {"type": "number"},
                // Additional properties that might be present in this format
                "Timestamp": {"type": "string"},
                "TraceState": {"type": "string"},
                "SpanKind": {"type": "string"},
                "ServiceName": {"type": "string"},
                "ResourceAttributes": {"type": "object"},
                "ScopeName": {"type": "string"},
                "SpanAttributes": {"type": "object"},
                "StatusCode": {"type": "string"},
                "StatusMessage": {"type": "string"}
            }
        });

        let nested_schema = json!({
            "type": "object",
            "required": ["record"],
            "properties": {
                "record": {
                    "type": "object",
                    "required": ["traceId", "spanId"],
                    "properties": {
                        "traceId": {"type": "string", "minLength": 1},
                        "spanId": {"type": "string", "minLength": 1},
                        "parentSpanId": {"type": "string"},
                        "name": {"type": "string"},
                        "duration": {"type": "number"},
                        // Additional properties that might be present in this format
                        "timestamp": {"type": "string"},
                        "traceState": {"type": "string"},
                        "kind": {"type": "string"},
                        "serviceName": {"type": "string"},
                        "attributes": {"type": "object"},
                        "status": {"type": "object"}
                    }
                },
                // The nested format might have additional metadata at the top level
                "timestamp": {"type": "string"},
                "source": {"type": "string"}
            }
        });

        (direct_schema, nested_schema)
    }

    fn is_valid_span_json(&self, json: &Value) -> (bool, SchemaType) {
        match self.compiled_schema.validate(json) {
            Ok(_) => {
                // Determine which schema matched
                if self.direct_schema.is_valid(json) {
                    tracing::debug!("JSON validated with Direct schema");
                    (true, SchemaType::Direct)
                } else if self.nested_schema.is_valid(json) {
                    tracing::debug!("JSON validated with Nested schema");
                    (true, SchemaType::Nested)
                } else {
                    tracing::debug!("JSON validated but schema type unknown");
                    (true, SchemaType::Unknown)
                }
            }
            Err(errors) => {
                tracing::debug!("JSON validation failed: {:?}", errors.collect::<Vec<_>>());
                (false, SchemaType::Unknown)
            }
        }
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
                let (is_valid, schema_type) = self.is_valid_span_json(&json);
                if is_valid {
                    valid_spans_found = true;
                    tracing::info!("Valid span found with schema type: {:?}", schema_type);

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
                    let (span_id, trace_id) = match schema_type {
                        SchemaType::Direct => {
                            let span_id = json
                                .get("SpanId")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown");
                            let trace_id = json
                                .get("TraceId")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown");
                            (span_id, trace_id)
                        }
                        SchemaType::Nested => {
                            if let Some(record) = json.get("record") {
                                let span_id = record
                                    .get("spanId")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("unknown");
                                let trace_id = record
                                    .get("traceId")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("unknown");
                                (span_id, trace_id)
                            } else {
                                ("unknown", "unknown")
                            }
                        }
                        SchemaType::Unknown => {
                            // Try both formats
                            let span_id = json
                                .get("SpanId")
                                .and_then(|v| v.as_str())
                                .or_else(|| {
                                    json.get("record")
                                        .and_then(|r| r.get("spanId"))
                                        .and_then(|v| v.as_str())
                                })
                                .unwrap_or("unknown");
                            let trace_id = json
                                .get("TraceId")
                                .and_then(|v| v.as_str())
                                .or_else(|| {
                                    json.get("record")
                                        .and_then(|r| r.get("traceId"))
                                        .and_then(|v| v.as_str())
                                })
                                .unwrap_or("unknown");
                            (span_id, trace_id)
                        }
                    };

                    let duration = match schema_type {
                        SchemaType::Direct => {
                            json.get("Duration").and_then(|v| v.as_u64()).unwrap_or(0)
                        }
                        _ => 0, // Duration might not be available in other formats
                    };

                    tracing::info!(
                        "Processing span: ID={}, TraceID={}, Duration={} ns, Schema={:?}",
                        span_id,
                        trace_id,
                        duration,
                        schema_type
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
