//! Bounded text for logs and messages.

use tokio::io::{AsyncRead, AsyncReadExt};

/// Cuts text to at most `max` bytes on a character boundary, keeping the start.
pub fn head(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Cuts text to at most `max` bytes on a character boundary, keeping the end.
pub fn tail(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

/// Reads the stream to its end and returns its last `max` bytes as text.
pub async fn read_tail(mut reader: impl AsyncRead + Unpin, max: usize) -> String {
    let mut kept = Vec::new();
    let mut chunk = vec![0; 64 * 1024];
    while let Ok(read) = reader.read(&mut chunk).await {
        if read == 0 {
            break;
        }
        kept.extend_from_slice(&chunk[..read]);
        if kept.len() > 2 * max {
            kept.drain(..kept.len() - max);
        }
    }
    let text = String::from_utf8_lossy(&kept);
    tail(&text, max).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cuts_on_char_boundaries() {
        assert_eq!(tail("abc", 5), "abc");
        assert_eq!(tail("abcdef", 3), "def");
        assert_eq!(tail("aé", 1), "");
        assert_eq!(tail("aéb", 2), "b");
        assert_eq!(head("abcdef", 3), "abc");
        assert_eq!(head("aéb", 2), "a");
    }

    #[tokio::test]
    async fn read_tail_keeps_the_end_of_long_streams() {
        let input = "x".repeat(200_000) + "end";
        assert_eq!(read_tail(input.as_bytes(), 3).await, "end");
        assert_eq!(read_tail("short".as_bytes(), 100).await, "short");
    }
}
