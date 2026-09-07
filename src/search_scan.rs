//! Bounded, cancellable content scanning. The buffers are reused between reads.

use std::{io, path, sync::atomic};

use crate::core::SearchCase;

const CHUNK: usize = 64 * 1024;

fn check_cancelled(cancel: &atomic::AtomicBool) -> io::Result<()> {
    if cancel.load(atomic::Ordering::Relaxed) {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "search cancelled",
        ))
    } else {
        Ok(())
    }
}

pub(super) fn file_contains(
    path: &path::Path,
    needle: &str,
    case: SearchCase,
    cancel: &atomic::AtomicBool,
) -> io::Result<bool> {
    check_cancelled(cancel)?;
    let mut file = std::fs::File::open(path)?;
    reader_contains(&mut file, needle, case, cancel)
}

fn reader_contains(
    reader: &mut impl io::Read,
    needle: &str,
    case: SearchCase,
    cancel: &atomic::AtomicBool,
) -> io::Result<bool> {
    if needle.is_empty() {
        return Ok(false);
    }
    let needle = match case {
        SearchCase::Sensitive => needle.as_bytes().to_vec(),
        SearchCase::Insensitive => needle.to_ascii_lowercase().into_bytes(),
    };
    let mut chunk = vec![0; CHUNK];
    let mut window = Vec::new();
    loop {
        check_cancelled(cancel)?;
        let read = match reader.read(&mut chunk) {
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        check_cancelled(cancel)?;
        if read == 0 {
            return Ok(false);
        }
        window.extend_from_slice(&chunk[..read]);
        if case == SearchCase::Insensitive {
            window.make_ascii_lowercase();
        }
        if memchr::memmem::find(&window, &needle).is_some() {
            return Ok(true);
        }
        // Keep enough overlap for matches crossing an arbitrary read boundary.
        let keep = needle.len().saturating_sub(1).min(window.len());
        let start = window.len() - keep;
        window.copy_within(start.., 0);
        window.truncate(keep);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct CancellingReader<'a> {
        cancel: &'a atomic::AtomicBool,
        reads: usize,
    }

    impl io::Read for CancellingReader<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.reads += 1;
            buffer.fill(b'x');
            self.cancel.store(true, atomic::Ordering::Relaxed);
            Ok(buffer.len())
        }
    }

    #[test]
    fn cancellation_stops_before_the_next_read() {
        let cancel = atomic::AtomicBool::new(false);
        let mut reader = CancellingReader {
            cancel: &cancel,
            reads: 0,
        };
        let error =
            reader_contains(&mut reader, "absent", SearchCase::Sensitive, &cancel).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(reader.reads, 1);
    }

    #[test]
    fn a_cancelled_request_does_not_read() {
        let cancel = atomic::AtomicBool::new(true);
        let mut reader = CancellingReader {
            cancel: &cancel,
            reads: 0,
        };
        assert!(reader_contains(&mut reader, "x", SearchCase::Sensitive, &cancel).is_err());
        assert_eq!(reader.reads, 0);
    }

    #[test]
    fn matches_across_chunk_boundaries() {
        let cancel = atomic::AtomicBool::new(false);
        let mut bytes = vec![b'x'; CHUNK - 2];
        bytes.extend_from_slice(b"NeEdLe");
        let mut reader = io::Cursor::new(bytes);
        assert!(reader_contains(&mut reader, "needle", SearchCase::Insensitive, &cancel).unwrap());
        reader.set_position(0);
        assert!(!reader_contains(&mut reader, "needle", SearchCase::Sensitive, &cancel).unwrap());
    }

    #[test]
    fn supports_needles_larger_than_a_chunk() {
        let cancel = atomic::AtomicBool::new(false);
        let needle = "a".repeat(CHUNK + 7);
        let mut reader = io::Cursor::new(needle.as_bytes());
        assert!(reader_contains(&mut reader, &needle, SearchCase::Sensitive, &cancel).unwrap());
    }

    #[test]
    fn empty_needle_does_not_read() {
        let cancel = atomic::AtomicBool::new(false);
        let mut reader = CancellingReader {
            cancel: &cancel,
            reads: 0,
        };
        assert!(!reader_contains(&mut reader, "", SearchCase::Sensitive, &cancel).unwrap());
        assert_eq!(reader.reads, 0);
    }
}
