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
    strict_json_array_only: bool,
}

impl ExtensionConfig {
    fn from_env() -> Result<Self, Error> {
        // Default to strict JSON array only mode unless explicitly disabled
        let strict_json_array_only = env::var("DISABLE_STRICT_JSON_ARRAY_ONLY")
            .map(|val| val.to_lowercase() != "true")
            .unwrap_or(true);

        Ok(Self {
            kinesis_stream_name: env::var("OTLP_STDOUT_KINESIS_STREAM_NAME")
                .map_err(|e| Error::from(format!("Failed to get stream name: {}", e)))?,
            strict_json_array_only,
        })
    }
}

struct TelemetryHandler {
    kinesis_client: Arc<KinesisClient>,
    stream_name: String,
    strict_json_array_only: bool,
}

impl TelemetryHandler {
    async fn new() -> Result<Self, Error> {
        let config = ExtensionConfig::from_env()?;
        let aws_config = aws_config::from_env().load().await;
        let kinesis_client = Arc::new(KinesisClient::new(&aws_config));

        Ok(Self {
            kinesis_client,
            stream_name: config.kinesis_stream_name,
            strict_json_array_only: config.strict_json_array_only,
        })
    }

    async fn send_record(&self, record: String) -> Result<(), Error> {
        tracing::info!("==== RECEIVED NEW RECORD ====");
        tracing::info!("Record length: {} bytes", record.len());

        // If strict mode is enabled (default), only process JSON arrays
        if self.strict_json_array_only {
            // Try to parse as JSON array - this is the ONLY format we accept in strict mode
            match serde_json::from_str::<Vec<Value>>(&record) {
                Ok(json_array) => {
                    tracing::info!("==== JSON ARRAY DETECTED ====");
                    tracing::info!("JSON array has {} elements", json_array.len());

                    // Process the JSON array
                    self.process_json_array_elements(json_array).await
                }
                Err(e) => {
                    // Not a JSON array, skip it
                    tracing::info!("==== NOT A JSON ARRAY, SKIPPING (STRICT MODE) ====");
                    tracing::info!("Parse error: {:?}", e);
                    tracing::info!(
                        "First few characters: {}",
                        &record[..std::cmp::min(100, record.len())]
                    );
                    tracing::info!("Skipping record because it's not a JSON array");
                    Ok(())
                }
            }
        } else {
            // This branch should never be taken with the default configuration
            tracing::warn!(
                "STRICT MODE DISABLED: This is not recommended and may process unwanted logs"
            );

            // Try to parse as JSON array first
            if let Ok(json_array) = serde_json::from_str::<Vec<Value>>(&record) {
                tracing::info!("==== JSON ARRAY DETECTED ====");
                tracing::info!("JSON array has {} elements", json_array.len());
                return self.process_json_array_elements(json_array).await;
            }

            // If we get here, it's not a JSON array and strict mode is disabled
            // We'll just log and skip it anyway for safety
            tracing::warn!("Record is not a JSON array, skipping for safety even though strict mode is disabled");
            Ok(())
        }
    }

    /// Process JSON array elements and send each valid object to Kinesis
    async fn process_json_array_elements(&self, json_array: Vec<Value>) -> Result<(), Error> {
        tracing::info!("Processing JSON array with {} objects", json_array.len());

        // Send each object in the array to Kinesis
        for (index, json_obj) in json_array.iter().enumerate() {
            let json_str = json_obj.to_string();
            tracing::info!("Object {}: {} bytes", index, json_str.len());

            if index < 2 {
                tracing::info!(
                    "Object {} sample: {}",
                    index,
                    &json_str[..std::cmp::min(100, json_str.len())]
                );
            }

            // Check size limit
            if json_str.len() > MAX_RECORD_SIZE_BYTES {
                tracing::warn!(
                    "JSON object at index {} size {} bytes exceeds maximum size of {} bytes, skipping",
                    index,
                    json_str.len(),
                    MAX_RECORD_SIZE_BYTES
                );
                continue;
            }

            // Send to Kinesis
            match self
                .kinesis_client
                .put_record()
                .stream_name(&self.stream_name)
                .data(Blob::new(json_str))
                .partition_key(Uuid::new_v4().to_string())
                .send()
                .await
            {
                Ok(_) => {
                    tracing::info!(
                        "Successfully sent JSON object at index {} to Kinesis",
                        index
                    );
                }
                Err(e) => {
                    tracing::error!(
                        "Failed to send JSON object at index {} to Kinesis: {}",
                        index,
                        e
                    );
                }
            }
        }

        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    tracing::init_default_subscriber();
    tracing::info!("Starting Rust extension for processing ONLY JSON arrays");

    let handler = Arc::new(TelemetryHandler::new().await?);
    let handler_clone = handler.clone();

    // Log the Kinesis stream name at startup
    tracing::info!("Using Kinesis stream: {}", handler.stream_name);

    // Log strict mode configuration
    if handler.strict_json_array_only {
        tracing::info!(
            "STRICT MODE: Only processing valid JSON arrays, all other formats will be skipped"
        );
    } else {
        tracing::info!("WARNING: Strict JSON array only mode is DISABLED");
    }

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
