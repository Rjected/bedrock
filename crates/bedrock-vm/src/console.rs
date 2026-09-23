// SPDX-License-Identifier: GPL-2.0

//! Guest console log format — the host's view of one serial-console line.
//!
//! At runtime the console is one compact journald JSON record per line
//! (`{"SYSLOG_IDENTIFIER":…,"MESSAGE":…}`, via `jq` in `guest/init` — keep the
//! field names in sync). Early boot carries raw printk instead.

use serde::Deserialize;

/// One complete console line, classified by source format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsoleLine {
    /// `source` is `SYSLOG_IDENTIFIER`: a container name, a `systemd-cat -t`
    /// tag, or `kernel`.
    Journal { source: String, message: String },
    /// Anything else, verbatim.
    Raw(String),
}

impl ConsoleLine {
    /// For a null `SYSLOG_IDENTIFIER`: such records are almost always kmsg.
    const DEFAULT_SOURCE: &'static str = "kernel";

    /// A JSON object with a `MESSAGE` field is a journal record; anything else
    /// is [`Raw`](ConsoleLine::Raw).
    pub fn parse(line: &str) -> Self {
        match serde_json::from_str::<JournalRecord>(line.trim()) {
            Ok(rec) if rec.message.is_some() => ConsoleLine::Journal {
                source: rec
                    .syslog_identifier
                    .unwrap_or_else(|| Self::DEFAULT_SOURCE.to_string()),
                message: rec.message.map(|m| m.into_text()).unwrap_or_default(),
            },
            _ => ConsoleLine::Raw(line.to_string()),
        }
    }
}

/// `jq` always emits both keys, so a record without `MESSAGE` isn't ours.
#[derive(Deserialize)]
struct JournalRecord {
    #[serde(rename = "SYSLOG_IDENTIFIER")]
    syslog_identifier: Option<String>,
    #[serde(rename = "MESSAGE")]
    message: Option<JournalMessage>,
}

/// journald renders `MESSAGE` as a byte array when it isn't a clean UTF-8 line.
#[derive(Deserialize)]
#[serde(untagged)]
enum JournalMessage {
    Text(String),
    Bytes(Vec<u8>),
}

impl JournalMessage {
    fn into_text(self) -> String {
        match self {
            JournalMessage::Text(s) => s,
            JournalMessage::Bytes(b) => String::from_utf8_lossy(&b).into_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_tagged_record() {
        let line = r#"{"SYSLOG_IDENTIFIER":"assertions","MESSAGE":"{\"Always\":{}}"}"#;
        assert_eq!(
            ConsoleLine::parse(line),
            ConsoleLine::Journal {
                source: "assertions".to_string(),
                message: r#"{"Always":{}}"#.to_string(),
            }
        );
    }

    #[test]
    fn missing_identifier_defaults_to_kernel() {
        // jq emits the key even when the journal value is null.
        let line = r#"{"SYSLOG_IDENTIFIER":null,"MESSAGE":"oom-killer invoked"}"#;
        assert_eq!(
            ConsoleLine::parse(line),
            ConsoleLine::Journal {
                source: "kernel".to_string(),
                message: "oom-killer invoked".to_string(),
            }
        );
    }

    #[test]
    fn reassembles_a_byte_array_message() {
        // journald renders a multi-line MESSAGE as a byte array; "ab\ncd".
        let line = r#"{"SYSLOG_IDENTIFIER":"idle","MESSAGE":[97,98,10,99,100]}"#;
        assert_eq!(
            ConsoleLine::parse(line),
            ConsoleLine::Journal {
                source: "idle".to_string(),
                message: "ab\ncd".to_string(),
            }
        );
    }

    #[test]
    fn raw_kernel_printk_passes_through() {
        let line = "[    0.123456] Linux version 6.18.0";
        assert_eq!(ConsoleLine::parse(line), ConsoleLine::Raw(line.to_string()));
    }

    #[test]
    fn non_object_json_is_raw() {
        // A bare JSON scalar or array on the console isn't one of our records.
        assert_eq!(ConsoleLine::parse("42"), ConsoleLine::Raw("42".to_string()));
        let obj_without_message = r#"{"SYSLOG_IDENTIFIER":"x"}"#;
        assert_eq!(
            ConsoleLine::parse(obj_without_message),
            ConsoleLine::Raw(obj_without_message.to_string())
        );
    }
}
