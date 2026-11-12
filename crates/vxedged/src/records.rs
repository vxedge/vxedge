//! **Live telemetry records** (`ARCHITECTURE.md` §11, decision X-16): the daemon tails the
//! `records.jsonl` the workspace's telemetry service appends as it scrapes, so a live stream
//! carries the same records, in the same order, as the archive built from that folder.
//!
//! A record's `seq` is its line number, from 1. A client that reconnects passes the last `seq` it
//! saw and loses nothing. A line that is not a record this version knows (a future shape, a torn
//! write) still takes its number, so cursors stay stable across versions.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Status;

use vxedge_api::v1::{GapRecord, LogRecord, PhaseRecord, Record, SampleRecord, record};

/// The file the ingest appends records to.
pub const FILE: &str = "records.jsonl";
/// The file the ingest appends log records to (decision X-18).
pub const LOGS: &str = "logs.jsonl";

/// How often a following stream looks for more.
const POLL: Duration = Duration::from_millis(200);

/// What a tail reads.
pub struct Tail {
    pub path: PathBuf,
    /// Only records with a greater `seq`.
    pub after: i64,
    /// Wait for more at the end instead of ending.
    pub follow: bool,
}

/// Stream `tail`'s lines as `parse` reads them, those `keep` accepts, mapped by `map`. The task
/// stops when the client goes away.
pub fn stream<R: Send + 'static, T: Send + 'static>(
    tail: Tail,
    parse: fn(i64, &str) -> Option<R>,
    keep: impl Fn(&R) -> bool + Send + 'static,
    map: impl Fn(R) -> T + Send + 'static,
) -> ReceiverStream<Result<T, Status>> {
    let (tx, rx) = mpsc::channel(256);
    tokio::spawn(async move {
        let file = loop {
            match tokio::fs::File::open(&tail.path).await {
                Ok(f) => break f,
                Err(_) if tail.follow && !tx.is_closed() => tokio::time::sleep(POLL).await,
                Err(e) => {
                    let _ = tx
                        .send(Err(crate::errors::precondition(
                            crate::errors::R::NotFound,
                            format!(
                                "nothing has been ingested: `{}`: {e}. Run the workspace's \
                                 telemetry service first",
                                tail.path.display()
                            ),
                        )))
                        .await;
                    return;
                }
            }
        };
        let mut reader = BufReader::new(file);
        let (mut seq, mut line) = (0i64, String::new());
        loop {
            match reader.read_line(&mut line).await {
                Ok(0) if tail.follow => {
                    // At the end, or inside a line still being written: wait, keep what was read.
                    if tx.is_closed() {
                        return;
                    }
                    tokio::time::sleep(POLL).await;
                }
                Ok(0) => return,
                Ok(_) if !line.ends_with('\n') => {}
                Ok(_) => {
                    seq += 1;
                    if seq > tail.after
                        && let Some(r) = parse(seq, &line)
                        && keep(&r)
                        && tx.send(Ok(map(r))).await.is_err()
                    {
                        return;
                    }
                    line.clear();
                }
                Err(e) => {
                    let _ = tx.send(Err(Status::internal(e.to_string()))).await;
                    return;
                }
            }
        }
    });
    ReceiverStream::new(rx)
}

/// One `records.jsonl` line as a `Record`, or `None` when it is not a record this version knows.
pub fn parse(seq: i64, line: &str) -> Option<Record> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    let s = |k: &str| v[k].as_str().unwrap_or_default().to_string();
    let n = |k: &str| v[k].as_i64().unwrap_or_default();
    let (ts_ns, body) = match v["record"].as_str()? {
        "sample" => (
            n("ts_ns"),
            record::Record::Sample(SampleRecord {
                entity: s("entity"),
                metric: s("metric"),
                labels: v["labels"]
                    .as_object()
                    .map(|m| {
                        m.iter()
                            .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                            .collect()
                    })
                    .unwrap_or_default(),
                value: v["value"].as_f64()?,
            }),
        ),
        "gap" => (
            n("from_ns"),
            record::Record::Gap(GapRecord {
                entity: s("entity"),
                metric: s("metric"),
                from_ns: n("from_ns"),
                to_ns: n("to_ns"),
                reason: s("reason"),
            }),
        ),
        "phase" => (
            n("ts_ns"),
            record::Record::Phase(PhaseRecord {
                trial: s("trial"),
                phase: s("phase"),
                window: s("window"),
            }),
        ),
        _ => return None,
    };
    Some(Record {
        seq,
        ts_ns,
        record: Some(body),
    })
}

/// One `logs.jsonl` line as a `LogRecord`, or `None` when it is not one.
pub fn parse_log(seq: i64, line: &str) -> Option<LogRecord> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    if v["record"].as_str()? != "log" {
        return None;
    }
    let s = |k: &str| v[k].as_str().unwrap_or_default().to_string();
    Some(LogRecord {
        seq,
        ts_ns: v["ts_ns"].as_i64().unwrap_or_default(),
        entity: s("entity"),
        stream: s("stream"),
        line: v["line"].as_str()?.to_string(),
    })
}

/// The entity a record is about; a phase is about none.
pub fn entity(r: &Record) -> Option<&str> {
    match r.record.as_ref()? {
        record::Record::Sample(s) => Some(&s.entity),
        record::Record::Gap(g) => Some(&g.entity),
        record::Record::Phase(_) => None,
    }
}

/// Whether `path` is `prefix` or under it, by whole path segments.
pub fn under(path: &str, prefix: &str) -> bool {
    prefix.is_empty()
        || path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Whether a sample matches a `StreamMetrics` selector: each key a label, or `entity`.
pub fn selected(s: &SampleRecord, selector: &BTreeMap<String, String>) -> bool {
    selector.iter().all(|(k, v)| match k.as_str() {
        "entity" => &s.entity == v,
        _ => s.labels.get(k) == Some(v),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_parse_into_records_with_their_seq() {
        let r = parse(
            3,
            r#"{"record":"sample","ts_ns":5,"entity":"car/front/ecu1","metric":"m","labels":{"q":"0.99"},"value":0.5}"#,
        )
        .unwrap();
        assert_eq!((r.seq, r.ts_ns), (3, 5));
        let Some(record::Record::Sample(s)) = &r.record else {
            panic!("{r:?}")
        };
        assert_eq!(s.labels["q"], "0.99");
        let g = parse(
            4,
            r#"{"record":"gap","entity":"host","from_ns":1,"to_ns":2,"reason":"scrape failed"}"#,
        )
        .unwrap();
        assert_eq!(g.ts_ns, 1);
        assert_eq!(entity(&g), Some("host"));
        let p = parse(5, r#"{"record":"phase","ts_ns":9,"phase":"measure"}"#).unwrap();
        assert_eq!(entity(&p), None);
        let w = parse(
            5,
            r#"{"record":"phase","ts_ns":9,"phase":"measure","window":"cold"}"#,
        )
        .unwrap();
        assert!(
            matches!(w.record, Some(record::Record::Phase(ref ph)) if ph.window == "cold"),
            "{w:?}"
        );
        assert!(parse(6, r#"{"record":"story","ts_ns":1}"#).is_none());
        assert!(parse(7, "{torn").is_none());
        let l = parse_log(
            8,
            r#"{"record":"log","ts_ns":3,"entity":"car/_/hpc1","stream":"serial","line":"login:"}"#,
        )
        .unwrap();
        assert_eq!(
            (l.seq, l.stream.as_str(), l.line.as_str()),
            (8, "serial", "login:")
        );
        assert!(parse_log(9, r#"{"record":"sample","ts_ns":1}"#).is_none());
    }

    #[test]
    fn prefixes_match_whole_segments() {
        assert!(under("car/front/ecu1", "car"));
        assert!(under("car/front/ecu1", "car/front"));
        assert!(under("car", "car"));
        assert!(!under("cart/x", "car"));
        assert!(under("host", ""));
    }

    #[tokio::test]
    async fn a_following_tail_sends_lines_as_they_are_completed() {
        use tokio_stream::StreamExt;
        let dir = std::env::temp_dir().join(format!("vxedged-tail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE);
        let line = |v: f64| {
            format!(
                "{{\"record\":\"sample\",\"ts_ns\":1,\"entity\":\"host\",\"metric\":\"m\",\"value\":{v}}}\n"
            )
        };
        std::fs::write(&path, line(1.0) + &line(2.0)).unwrap();
        let mut s = stream(
            Tail {
                path: path.clone(),
                after: 1,
                follow: true,
            },
            parse,
            |_| true,
            |r| r.seq,
        );
        assert_eq!(s.next().await.unwrap().unwrap(), 2);
        // A line written in two parts arrives once, whole.
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        let third = line(3.0);
        f.write_all(&third.as_bytes()[..10]).unwrap();
        f.flush().unwrap();
        tokio::time::sleep(POLL * 2).await;
        f.write_all(&third.as_bytes()[10..]).unwrap();
        assert_eq!(s.next().await.unwrap().unwrap(), 3);
        drop(s);

        let ended: Vec<_> = stream(
            Tail {
                path: path.clone(),
                after: 0,
                follow: false,
            },
            parse,
            |_| true,
            |r| r.seq,
        )
        .collect()
        .await;
        assert_eq!(ended.len(), 3);
        let missing: Vec<_> = stream(
            Tail {
                path: dir.join("none"),
                after: 0,
                follow: false,
            },
            parse,
            |_| true,
            |r| r.seq,
        )
        .collect()
        .await;
        assert_eq!(
            missing[0].as_ref().unwrap_err().code(),
            tonic::Code::FailedPrecondition
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
