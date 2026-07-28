//! What one typed multipart upload costs.
//!
//! Decodes a single `multipart/form-data` POST carrying a five-megabyte part
//! through the `Multipart<T>` extractor and prints the wall time. Run it under
//! a process-level peak-working-set probe to see the resident cost.
//!
//! ```text
//! cargo run --release -p blazingly-executor --example multipart_upload_cost
//! ```

use blazingly_core::{
    ApiModel, ApiSchema, FieldDescriptor, InputSource, ModelDescriptor, Multipart, SchemaKind,
    TypeDescriptor, UploadFile, ValidationErrors,
};
use blazingly_executor::{FromInvocation, HttpRequestParts, InvocationInput};
use serde::Deserialize;
use std::borrow::Cow;
use std::time::Instant;

const UPLOAD_BYTES: usize = 5 * 1024 * 1024;
const BOUNDARY: &str = "blazingly-measure";

#[derive(Deserialize)]
struct CoverUpload {
    title: String,
    attachments: Vec<UploadFile>,
}

impl ApiModel for CoverUpload {
    fn model_descriptor() -> ModelDescriptor {
        ModelDescriptor::new(
            "CoverUpload",
            vec![
                FieldDescriptor::new(
                    "title",
                    true,
                    TypeDescriptor::scalar("String", SchemaKind::String),
                    Vec::new(),
                ),
                FieldDescriptor::new(
                    "attachments",
                    true,
                    <Vec<UploadFile> as ApiSchema>::type_descriptor(),
                    Vec::new(),
                ),
            ],
        )
    }

    fn validate(&self) -> Result<(), ValidationErrors> {
        Ok(())
    }
}

struct Request {
    content_type: String,
    body: Vec<u8>,
}

impl HttpRequestParts for Request {
    fn value(&self, source: InputSource, name: &str, index: usize) -> Option<Cow<'_, str>> {
        if source == InputSource::Header && name.eq_ignore_ascii_case("content-type") && index == 0
        {
            Some(Cow::Borrowed(&self.content_type))
        } else {
            None
        }
    }

    fn body(&self) -> &[u8] {
        &self.body
    }
}

fn request() -> Request {
    let mut body = Vec::with_capacity(UPLOAD_BYTES + 512);
    body.extend_from_slice(
        format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"title\"\r\n\r\ncover\r\n\
             --{BOUNDARY}\r\nContent-Disposition: form-data; name=\"attachments\"; \
             filename=\"cover.png\"\r\nContent-Type: image/png\r\n\r\n"
        )
        .as_bytes(),
    );
    body.resize(body.len() + UPLOAD_BYTES, b'x');
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    Request {
        content_type: format!("multipart/form-data; boundary={BOUNDARY}"),
        body,
    }
}

fn main() {
    let iterations: u32 = std::env::args()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(1);
    let request = request();
    let mut total = std::time::Duration::ZERO;
    let mut observed = 0;
    for _ in 0..iterations {
        let started = Instant::now();
        let Multipart(input) = Multipart::<CoverUpload>::from_invocation(
            &InvocationInput::Http(&request),
            "input",
            true,
        )
        .expect("the typed multipart body decodes");
        total += started.elapsed();
        assert_eq!(input.title, "cover");
        assert_eq!(input.attachments.len(), 1);
        observed = input.attachments[0].bytes.len();
        assert_eq!(observed, UPLOAD_BYTES);
    }
    println!(
        "iterations={iterations} upload_bytes={observed} total_micros={} mean_micros={}",
        total.as_micros(),
        total.as_micros() / u128::from(iterations)
    );
    // Stay alive long enough for an external probe to read the peak working
    // set; the counter is gone once the process has exited.
    if std::env::args().nth(2).as_deref() == Some("hold") {
        std::thread::sleep(std::time::Duration::from_millis(1500));
    }
}
