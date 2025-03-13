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

    /// Process a JSON array and send each object as a separate record to Kinesis
    async fn process_json_array(&self, record: &str) -> Result<(), Error> {
        // Try to parse as a JSON array first
        if let Ok(json_array) = serde_json::from_str::<Vec<Value>>(record) {
            tracing::info!("Processing JSON array with {} objects", json_array.len());

            // Convert each object to a newline-delimited JSON (JSONEachRow/NDJSON)
            for (index, json_obj) in json_array.iter().enumerate() {
                let json_str = json_obj.to_string();

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

            return Ok(());
        }

        // If not a JSON array, try to process as JSONEachRow (NDJSON)
        tracing::info!("Input is not a JSON array, trying JSONEachRow format");
        self.process_json_each_row(record).await
    }

    /// Process and send records from JSONEachRow format (NDJSON)
    async fn process_json_each_row(&self, record: &str) -> Result<(), Error> {
        let lines: Vec<&str> = record.trim().split('\n').collect();
        if lines.is_empty() {
            tracing::warn!("No lines found in the input");
            return Ok(());
        }

        tracing::info!("Processing {} lines in JSONEachRow format", lines.len());

        for (index, line) in lines.iter().enumerate() {
            if line.trim().is_empty() {
                continue;
            }

            // Check if it's valid JSON
            if let Ok(_json) = serde_json::from_str::<Value>(line) {
                // Check size limit
                if line.len() > MAX_RECORD_SIZE_BYTES {
                    tracing::warn!(
                        "Line {} size {} bytes exceeds maximum size of {} bytes, skipping",
                        index,
                        line.len(),
                        MAX_RECORD_SIZE_BYTES
                    );
                    continue;
                }

                // Send to Kinesis
                match self
                    .kinesis_client
                    .put_record()
                    .stream_name(&self.stream_name)
                    .data(Blob::new(line.to_string()))
                    .partition_key(Uuid::new_v4().to_string())
                    .send()
                    .await
                {
                    Ok(_) => {
                        tracing::info!("Successfully sent line {} to Kinesis", index);
                    }
                    Err(e) => {
                        tracing::error!("Failed to send line {} to Kinesis: {}", index, e);
                    }
                }
            } else {
                tracing::warn!("Line {} is not valid JSON, skipping: {}", index, line);
            }
        }

        Ok(())
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

        // Process the record - first try as JSON array, then as JSONEachRow
        self.process_json_array(&record).await
    }
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    tracing::init_default_subscriber();
    tracing::info!("Starting Rust extension for ClickHouse JSONEachRow format");

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
