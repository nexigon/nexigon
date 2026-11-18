//! Command that returns the most recent log records of an app's workload.
//!
//! [`read_logs`] reads the records through Rugix Ctrl and returns them as
//! [`RugixAppLogs`]. Nexigon Hub stores the output of an operation's command
//! step only up to a size limit, 64 KiB by default, so the command bounds its
//! output: it cuts long messages, drops the fields of records that are still
//! large, and omits the oldest records that do not fit.

use nexigon_api::types::rugix_apps::RugixAppLogRecord;
use nexigon_api::types::rugix_apps::RugixAppLogs;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;

use super::commands::check_app_name;
use super::rugix::RugixApps;

/// Device command that returns the most recent log records of an app.
pub(crate) const LOGS_COMMAND: &str = "nexigon.rugix-apps.logs";

/// Number of records that the command requests by default.
const DEFAULT_LINES: u32 = 100;
/// Maximum number of records that the command requests, which bounds the work
/// of Rugix Ctrl and the memory of the agent.
const MAX_LINES: u32 = 1000;
/// Maximum size of the output as compact JSON, the default limit of Nexigon
/// Hub for the output of an operation's command step.
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
/// Length after which the command cuts a message.
const MAX_MESSAGE_BYTES: usize = 2 * 1024;
/// Size of a record as compact JSON above which the command drops its fields.
const MAX_RECORD_BYTES: usize = 4 * 1024;

/// JSON Schema of the input, which names the app and the number of records.
pub(crate) fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "app": { "type": "string", "description": "Name of the app." },
            "lines": {
                "type": "integer",
                "minimum": 1,
                "maximum": MAX_LINES,
                "default": DEFAULT_LINES,
                "description": "Maximum number of records, keeping the most recent ones."
            }
        },
        "required": ["app"]
    })
}

/// Read the most recent log records of an app and return them as
/// [`RugixAppLogs`], or an error message if reading fails.
pub(crate) async fn read_logs(input: Value) -> Result<Value, String> {
    let Input { app, lines } =
        serde_json::from_value(input).map_err(|error| format!("invalid input: {error}"))?;
    check_app_name(&app)?;
    let lines = lines.unwrap_or(DEFAULT_LINES);
    if !(1..=MAX_LINES).contains(&lines) {
        return Err(format!("lines must be between 1 and {MAX_LINES}"));
    }
    let records = RugixApps
        .logs(&app, lines)
        .await
        .map_err(|error| format!("{error:#}"))?;
    let logs = bound_logs(records, MAX_OUTPUT_BYTES);
    Ok(serde_json::to_value(logs).expect("log records serialize to JSON"))
}

/// Input of the logs command.
#[derive(Deserialize)]
struct Input {
    app: String,
    lines: Option<u32>,
}

/// Keep the most recent records that fit into `max_bytes` as compact JSON,
/// after cutting messages and dropping the fields of large records.
fn bound_logs(records: Vec<RugixAppLogRecord>, max_bytes: usize) -> RugixAppLogs {
    let total = records.len();
    // Reserve the space of the largest count of omitted records.
    let mut size = json_size(&RugixAppLogs::new(Vec::new().into()).with_omitted(Some(u32::MAX)));
    let mut kept = Vec::new();
    for record in records.into_iter().rev() {
        let record = shrink_record(record);
        // Records after the first one are preceded by a comma.
        let record_size = json_size(&record) + usize::from(!kept.is_empty());
        if size + record_size > max_bytes {
            break;
        }
        size += record_size;
        kept.push(record);
    }
    kept.reverse();
    let omitted = total - kept.len();
    RugixAppLogs::new(kept.into()).with_omitted(
        (omitted > 0).then(|| u32::try_from(omitted).expect("at most `MAX_LINES` records")),
    )
}

/// Cut the message of a record and drop its fields if it is still large.
fn shrink_record(mut record: RugixAppLogRecord) -> RugixAppLogRecord {
    if record.message.len() > MAX_MESSAGE_BYTES {
        let end = record.message.floor_char_boundary(MAX_MESSAGE_BYTES);
        record.message = record.message[..end].into();
        record.truncated = Some(true);
    }
    if record.fields.is_some() && json_size(&record) > MAX_RECORD_BYTES {
        record.fields = None;
        record.truncated = Some(true);
    }
    record
}

/// Size of a value as compact JSON.
fn json_size(value: &impl serde::Serialize) -> usize {
    serde_json::to_vec(value)
        .expect("log records serialize to JSON")
        .len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(message: &str) -> RugixAppLogRecord {
        RugixAppLogRecord::new(message.to_owned())
    }

    /// Long messages are cut at a character boundary, and large records lose
    /// their fields, which both mark the record as truncated.
    #[test]
    fn records_are_shrunk() {
        let short = shrink_record(record("started"));
        assert_eq!(short.message, "started");
        assert_eq!(short.truncated, None);

        let long = shrink_record(record(&"ä".repeat(MAX_MESSAGE_BYTES)));
        assert_eq!(long.message.len(), MAX_MESSAGE_BYTES);
        assert_eq!(long.truncated, Some(true));

        let fields =
            serde_json::from_value(json!({ "payload": "x".repeat(MAX_RECORD_BYTES) })).unwrap();
        let large = shrink_record(record("request").with_fields(Some(fields)));
        assert!(large.fields.is_none());
        assert_eq!(large.truncated, Some(true));
    }

    /// The output keeps the most recent records in chronological order and
    /// stays within the limit, counting the omitted older records.
    #[test]
    fn logs_keep_the_most_recent_records_within_the_limit() {
        let records = (0..1000)
            .map(|index| record(&format!("{index:0>100}")))
            .collect::<Vec<_>>();

        let logs = bound_logs(records.clone(), MAX_OUTPUT_BYTES);
        let size = json_size(&logs);
        assert!(size <= MAX_OUTPUT_BYTES, "{size}");
        assert!(MAX_OUTPUT_BYTES - size < 200, "{size}");
        let kept = logs.records.len();
        assert_eq!(logs.omitted, Some(u32::try_from(1000 - kept).unwrap()));
        assert_eq!(logs.records.last().unwrap().message, records[999].message);
        assert_eq!(logs.records[0].message, records[1000 - kept].message);

        let logs = bound_logs(records[..10].to_vec(), MAX_OUTPUT_BYTES);
        assert_eq!(logs.records.len(), 10);
        assert_eq!(logs.omitted, None);
    }
}
