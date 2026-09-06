#![allow(clippy::expect_used)]
use super::JsonWriter;
use serde::{Serialize, Serializer};
use std::io::{self, Write};

#[test]
fn destinations_preserve_exact_json_bytes_and_preallocated_storage() {
    let value = serde_json::json!({
        "text": "Unicode 🦀 λ\n\r\t\0\u{1f}\\\"",
        "numbers": [i64::MIN, i64::MAX, 0],
        "decimal": 1.25e-12,
        "nested": [true, false, null, {"array": ["", "last"]}],
    });
    let expected = serde_json::to_vec(&value).expect("reference bytes");
    let mut count = JsonWriter::count(expected.len());
    count.serialize(&value).expect("exact byte count");
    assert_eq!(count.written(), expected.len());
    assert!(!count.exceeded());
    let mut bytes = Vec::with_capacity(expected.len());
    let original = bytes.as_ptr();
    JsonWriter::buffer(&mut bytes, expected.len(), 4096)
        .expect("admitted preallocation")
        .serialize(&value)
        .expect("exact encoding");
    assert_eq!(bytes, expected);
    assert_eq!(bytes.as_ptr(), original);
    assert_eq!(bytes.capacity(), expected.len());
    let mut stream = Vec::new();
    JsonWriter::stream(&mut stream, expected.len())
        .serialize(&value)
        .expect("stream bytes");
    assert_eq!(stream, expected);
}

#[test]
fn escaped_bytes_and_appended_delimiters_obey_the_same_ceiling() {
    let expected = serde_json::to_vec("\0\n🦀").expect("escaped bytes");
    let limit = expected.len() - 1;
    let mut count = JsonWriter::count(limit);
    assert!(
        count
            .serialize("\0\n🦀")
            .expect_err("one byte short")
            .is_io()
    );
    assert!(count.exceeded());
    assert!(count.written() <= limit);
    let mut bytes = Vec::new();
    {
        let mut writer = JsonWriter::buffer(&mut bytes, limit, 4096).expect("buffer");
        assert!(writer.serialize("\0\n🦀").is_err());
        assert!(writer.exceeded());
    }
    assert!(bytes.len() <= limit);
    assert!(bytes.capacity() <= limit);
    let mut framed = expected.clone();
    let limit = framed.capacity() + 1;
    let mut writer = JsonWriter::buffer(&mut framed, limit, 0).expect("append buffer");
    writer.write_all(b"\n").expect("delimiter");
    assert_eq!(writer.written(), expected.len() + 1);
    assert!(writer.write_all(&vec![0; limit]).is_err());
}

#[test]
fn retained_capacity_and_arithmetic_overflow_are_rejected() {
    let mut oversized = Vec::with_capacity(16);
    assert!(JsonWriter::buffer(&mut oversized, 15, 0).is_err());
    let mut count = JsonWriter::count(usize::MAX);
    count.written = usize::MAX;
    assert!(count.write_all(b"x").is_err());
    assert!(count.exceeded());
    assert_eq!(count.written(), usize::MAX);
}

#[test]
fn semantic_and_stream_failures_preserve_error_classification() {
    struct Invalid;
    impl Serialize for Invalid {
        fn serialize<S: Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom("invalid semantic value"))
        }
    }
    let mut count = JsonWriter::count(1024);
    assert!(
        count
            .serialize(&Invalid)
            .expect_err("semantic rejection")
            .is_data()
    );
    assert!(!count.exceeded());
    struct Broken;
    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "closed transport",
            ))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut sink = Broken;
    let mut stream = JsonWriter::stream(&mut sink, 1024);
    let error = stream.serialize(&true).expect_err("transport rejection");
    assert_eq!(error.io_error_kind(), Some(io::ErrorKind::BrokenPipe));
    assert_eq!(stream.written(), 0);
    assert!(!stream.exceeded());
}
