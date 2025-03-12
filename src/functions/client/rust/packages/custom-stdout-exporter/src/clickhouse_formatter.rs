use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::time::UNIX_EPOCH;

/// Represents a span in the ClickHouse format
#[derive(Debug, Serialize, Deserialize)]
#[allow(non_snake_case)]
pub struct ClickhouseSpan {
    pub Timestamp: String,
    pub TraceId: String,
    pub SpanId: String,
    pub ParentSpanId: String,
    pub TraceState: String,
    pub SpanName: String,
    pub SpanKind: String,
    pub ServiceName: String,
    pub ResourceAttributes: HashMap<String, String>,
    pub ScopeName: String,
    pub ScopeVersion: String,
    pub SpanAttributes: HashMap<String, String>,
    pub Duration: u64,
    pub StatusCode: String,
    pub StatusMessage: String,
    pub Events: ClickhouseEvents,
    pub Links: ClickhouseLinks,
}

#[derive(Debug, Serialize, Deserialize)]
#[allow(non_snake_case)]
pub struct ClickhouseEvents {
    pub Timestamp: Vec<String>,
    pub Name: Vec<String>,
    pub Attributes: Vec<HashMap<String, String>>,
}

#[derive(Debug, Serialize, Deserialize)]
#[allow(non_snake_case)]
pub struct ClickhouseLinks {
    pub TraceId: Vec<String>,
    pub SpanId: Vec<String>,
    pub TraceState: Vec<String>,
    pub Attributes: Vec<HashMap<String, String>>,
}

/// Converts a span kind integer to a string representation
fn span_kind_to_string(kind: i32) -> String {
    match kind {
        1 => "Internal".to_string(),
        2 => "Server".to_string(),
        3 => "Client".to_string(),
        4 => "Producer".to_string(),
        5 => "Consumer".to_string(),
        _ => "Unspecified".to_string(),
    }
}

/// Converts a status code integer to a string representation
fn status_code_to_string(code: i32) -> String {
    match code {
        1 => "Ok".to_string(),
        2 => "Error".to_string(),
        _ => "Unset".to_string(),
    }
}

/// Formats a timestamp in nanoseconds to a datetime string
fn format_timestamp(timestamp_nanos: u64) -> String {
    let seconds = timestamp_nanos / 1_000_000_000;
    let nanos = timestamp_nanos % 1_000_000_000;

    // Convert to SystemTime
    let system_time = UNIX_EPOCH + std::time::Duration::new(seconds, nanos as u32);

    // Format as ISO 8601 with microsecond precision
    let datetime = chrono::DateTime::<chrono::Utc>::from(system_time);
    datetime.format("%Y-%m-%d %H:%M:%S.%f").to_string()
}

/// Converts attributes from OTLP format to a simple key-value map
fn convert_attributes(attributes: &[Value]) -> HashMap<String, String> {
    let mut result = HashMap::new();

    for attr in attributes {
        if let (Some(key), Some(value)) = (attr.get("key"), attr.get("value")) {
            let key_str = key.as_str().unwrap_or_default().to_string();

            // Extract the value based on its type
            let value_str = if let Some(string_value) = value.get("stringValue") {
                string_value.as_str().unwrap_or_default().to_string()
            } else if let Some(int_value) = value.get("intValue") {
                int_value.to_string()
            } else if let Some(double_value) = value.get("doubleValue") {
                double_value.to_string()
            } else if let Some(bool_value) = value.get("boolValue") {
                bool_value.to_string()
            } else {
                "".to_string()
            };

            result.insert(key_str, value_str);
        }
    }

    result
}

/// Converts events from OTLP format to ClickHouse format
fn convert_events(events: &[Value]) -> ClickhouseEvents {
    let mut timestamps = Vec::new();
    let mut names = Vec::new();
    let mut attributes_list = Vec::new();

    for event in events {
        if let Some(timestamp) = event.get("timeUnixNano") {
            if let Some(timestamp_str) = timestamp.as_str() {
                if let Ok(timestamp_nanos) = timestamp_str.parse::<u64>() {
                    timestamps.push(format_timestamp(timestamp_nanos));
                }
            }
        }

        if let Some(name) = event.get("name") {
            names.push(name.as_str().unwrap_or_default().to_string());
        }

        if let Some(attributes) = event.get("attributes") {
            if let Some(attrs_array) = attributes.as_array() {
                attributes_list.push(convert_attributes(attrs_array));
            }
        }
    }

    ClickhouseEvents {
        Timestamp: timestamps,
        Name: names,
        Attributes: attributes_list,
    }
}

/// Converts links from OTLP format to ClickHouse format
fn convert_links(links: &[Value]) -> ClickhouseLinks {
    let mut trace_ids = Vec::new();
    let mut span_ids = Vec::new();
    let mut trace_states = Vec::new();
    let mut attributes_list = Vec::new();

    for link in links {
        if let Some(trace_id) = link.get("traceId") {
            trace_ids.push(trace_id.as_str().unwrap_or_default().to_string());
        }

        if let Some(span_id) = link.get("spanId") {
            span_ids.push(span_id.as_str().unwrap_or_default().to_string());
        }

        if let Some(trace_state) = link.get("traceState") {
            trace_states.push(trace_state.as_str().unwrap_or_default().to_string());
        }

        if let Some(attributes) = link.get("attributes") {
            if let Some(attrs_array) = attributes.as_array() {
                attributes_list.push(convert_attributes(attrs_array));
            }
        }
    }

    ClickhouseLinks {
        TraceId: trace_ids,
        SpanId: span_ids,
        TraceState: trace_states,
        Attributes: attributes_list,
    }
}

/// Transforms OTLP JSON to ClickHouse format
pub fn transform_otlp_to_clickhouse(otlp_json: &str) -> Result<String, serde_json::Error> {
    let otlp_value: Value = serde_json::from_str(otlp_json)?;
    let mut clickhouse_spans = Vec::new();

    // Process resource spans
    if let Some(resource_spans) = otlp_value.get("resourceSpans").and_then(|v| v.as_array()) {
        for resource_span in resource_spans {
            // Extract resource attributes
            let resource_attributes = if let Some(resource) = resource_span.get("resource") {
                if let Some(attributes) = resource.get("attributes").and_then(|v| v.as_array()) {
                    convert_attributes(attributes)
                } else {
                    HashMap::new()
                }
            } else {
                HashMap::new()
            };

            // Get service name from resource attributes
            let service_name = resource_attributes
                .get("service.name")
                .cloned()
                .unwrap_or_else(|| "unknown_service".to_string());

            // Process scope spans
            if let Some(scope_spans) = resource_span.get("scopeSpans").and_then(|v| v.as_array()) {
                for scope_span in scope_spans {
                    // Extract scope information
                    let (scope_name, scope_version) = if let Some(scope) = scope_span.get("scope") {
                        (
                            scope
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string(),
                            scope
                                .get("version")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string(),
                        )
                    } else {
                        ("".to_string(), "".to_string())
                    };

                    // Process spans
                    if let Some(spans) = scope_span.get("spans").and_then(|v| v.as_array()) {
                        for span in spans {
                            // Extract basic span information
                            let trace_id = span
                                .get("traceId")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string();
                            let span_id = span
                                .get("spanId")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string();
                            let parent_span_id = span
                                .get("parentSpanId")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string();
                            let trace_state = span
                                .get("traceState")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string();
                            let span_name = span
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string();

                            // Convert span kind
                            let span_kind =
                                if let Some(kind) = span.get("kind").and_then(|v| v.as_i64()) {
                                    span_kind_to_string(kind as i32)
                                } else {
                                    "Unspecified".to_string()
                                };

                            // Extract timestamps
                            let start_time_nanos = if let Some(start_time) =
                                span.get("startTimeUnixNano").and_then(|v| v.as_str())
                            {
                                start_time.parse::<u64>().unwrap_or_default()
                            } else {
                                0
                            };

                            let end_time_nanos = if let Some(end_time) =
                                span.get("endTimeUnixNano").and_then(|v| v.as_str())
                            {
                                end_time.parse::<u64>().unwrap_or_default()
                            } else {
                                0
                            };

                            // Calculate duration in nanoseconds
                            let duration = end_time_nanos.saturating_sub(start_time_nanos);

                            // Format timestamp
                            let timestamp = format_timestamp(start_time_nanos);

                            // Extract span attributes
                            let span_attributes = if let Some(attributes) =
                                span.get("attributes").and_then(|v| v.as_array())
                            {
                                convert_attributes(attributes)
                            } else {
                                HashMap::new()
                            };

                            // Extract status
                            let (status_code, status_message) =
                                if let Some(status) = span.get("status") {
                                    let code = status
                                        .get("code")
                                        .and_then(|v| v.as_i64())
                                        .unwrap_or_default()
                                        as i32;
                                    let message = status
                                        .get("message")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or_default()
                                        .to_string();
                                    (status_code_to_string(code), message)
                                } else {
                                    ("Unset".to_string(), "".to_string())
                                };

                            // Extract events
                            let events = if let Some(events_array) =
                                span.get("events").and_then(|v| v.as_array())
                            {
                                convert_events(events_array)
                            } else {
                                ClickhouseEvents {
                                    Timestamp: Vec::new(),
                                    Name: Vec::new(),
                                    Attributes: Vec::new(),
                                }
                            };

                            // Extract links
                            let links = if let Some(links_array) =
                                span.get("links").and_then(|v| v.as_array())
                            {
                                convert_links(links_array)
                            } else {
                                ClickhouseLinks {
                                    TraceId: Vec::new(),
                                    SpanId: Vec::new(),
                                    TraceState: Vec::new(),
                                    Attributes: Vec::new(),
                                }
                            };

                            // Create ClickHouse span
                            let clickhouse_span = ClickhouseSpan {
                                Timestamp: timestamp,
                                TraceId: trace_id,
                                SpanId: span_id,
                                ParentSpanId: parent_span_id,
                                TraceState: trace_state,
                                SpanName: span_name,
                                SpanKind: span_kind,
                                ServiceName: service_name.clone(),
                                ResourceAttributes: resource_attributes.clone(),
                                ScopeName: scope_name.clone(),
                                ScopeVersion: scope_version.clone(),
                                SpanAttributes: span_attributes,
                                Duration: duration / 1_000_000, // Convert to microseconds
                                StatusCode: status_code,
                                StatusMessage: status_message,
                                Events: events,
                                Links: links,
                            };

                            clickhouse_spans.push(clickhouse_span);
                        }
                    }
                }
            }
        }
    }

    // Serialize to JSON
    serde_json::to_string_pretty(&clickhouse_spans)
}
