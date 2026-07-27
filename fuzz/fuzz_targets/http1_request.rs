#![no_main]

use blazingly_wire::{
    BodyFraming, ChunkDecoder, Limits, StreamingChunk, StreamingChunkDecoder, parse_request_head,
};
use libfuzzer_sys::fuzz_target;

const MAX_EVENTS: usize = 1_024;

fuzz_target!(|data: &[u8]| {
    let limits = Limits::new()
        .with_max_header_bytes(4 * 1_024)
        .with_max_headers(32)
        .with_max_body_bytes(64 * 1_024)
        .with_max_chunks(256);
    let Ok(Some(head)) = parse_request_head(data, limits) else {
        return;
    };
    if head.target.bytes(data).is_none() {
        panic!("the parser returned an out-of-bounds target");
    }
    for header in head.headers.iter() {
        if header.name.bytes(data).is_none() || header.value.bytes(data).is_none() {
            panic!("the parser returned an out-of-bounds header");
        }
    }

    match head.body {
        BodyFraming::ContentLength(length) => {
            let _ = data.get(head.head_bytes..head.head_bytes.saturating_add(length));
        }
        BodyFraming::Chunked => {
            let _ = ChunkDecoder::new(head.head_bytes, limits).advance(data);
            let mut decoder = StreamingChunkDecoder::new(head.head_bytes, limits);
            for _ in 0..MAX_EVENTS {
                match decoder.advance(data) {
                    Ok(StreamingChunk::Data(range)) => {
                        if range.bytes(data).is_none() {
                            panic!("the decoder returned an out-of-bounds chunk");
                        }
                    }
                    Ok(StreamingChunk::NeedMore | StreamingChunk::Complete { .. }) | Err(_) => {
                        break;
                    }
                }
            }
        }
    }
});
