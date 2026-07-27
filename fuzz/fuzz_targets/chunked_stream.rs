#![no_main]

use blazingly_wire::{Limits, StreamingChunk, StreamingChunkDecoder};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let limits = Limits::new()
        .with_max_header_bytes(4 * 1_024)
        .with_max_body_bytes(64 * 1_024)
        .with_max_chunks(256);
    let mut decoder = StreamingChunkDecoder::new(0, limits);

    for _ in 0..1_024 {
        match decoder.advance(data) {
            Ok(StreamingChunk::Data(range)) => {
                if range.bytes(data).is_none() {
                    panic!("the decoder returned an out-of-bounds chunk");
                }
            }
            Ok(StreamingChunk::NeedMore | StreamingChunk::Complete { .. }) | Err(_) => break,
        }
    }
});
