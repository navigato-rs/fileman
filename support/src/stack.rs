//! Address-only backtraces from the executable, never other loaded modules.
use std::{path, sync};

const MAX_FRAMES: usize = 48;
const MAX_WALK: usize = 128;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Trace {
    debug_id: String,
    code_id: Option<String>,
    image_size: u64,
    image_vmaddr: u64,
    /// Oldest to newest, relative to the executable's load address (not ASLR addresses).
    offsets: Vec<u64>,
}

struct Image {
    base: u64,
    trace: Trace,
}

impl Image {
    fn load() -> Option<Self> {
        let executable = std::env::current_exe().ok()?;
        sentry_debug_images::debug_images()
            .into_iter()
            .find_map(|image| {
                let sentry_core::protocol::DebugImage::Symbolic(image) = image else {
                    return None;
                };
                if path::Path::new(&image.name) != executable {
                    return None;
                }
                Some(Self {
                    base: image.image_addr.0,
                    trace: Trace {
                        debug_id: image.id.to_string(),
                        code_id: image.code_id.map(|id| id.to_string()),
                        image_size: image.image_size,
                        image_vmaddr: image.image_vmaddr.0,
                        offsets: Vec::new(),
                    },
                })
            })
    }
}

impl Trace {
    pub(crate) fn capture() -> Option<Self> {
        // Called only after checking local backtrace consent. Do not resolve symbols
        // in the failing process; the matching release symbols belong in Sentry.
        static IMAGE: sync::OnceLock<Option<Image>> = sync::OnceLock::new();
        let image = IMAGE.get_or_init(Image::load).as_ref()?;
        let mut trace = image.trace.clone();
        trace.offsets.reserve(MAX_FRAMES);
        let mut visited = 0;
        backtrace::trace(|frame| {
            visited += 1;
            if let Some(offset) = (frame.ip() as u64).checked_sub(image.base)
                && offset < trace.image_size
            {
                trace.offsets.push(offset);
            }
            visited < MAX_WALK && trace.offsets.len() < MAX_FRAMES
        });
        trace.offsets.reverse();
        trace.valid().then_some(trace)
    }

    pub(crate) fn valid(&self) -> bool {
        self.debug_id.len() <= 45
            && self.debug_id.parse::<sentry_core::types::DebugId>().is_ok()
            && self.code_id.as_ref().is_none_or(|id| {
                !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_hexdigit())
            })
            && self.image_size > 0
            && self.image_size <= 1 << 40
            && !self.offsets.is_empty()
            && self.offsets.len() <= MAX_FRAMES
            && self.offsets.iter().all(|offset| *offset < self.image_size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_bounded_executable_offsets_without_paths() {
        let trace = Trace::capture().expect("test executable has debug information");
        assert!(trace.valid());
        let json = serde_json::to_string(&trace).unwrap();
        assert!(json.len() < 4096);
        assert!(!json.contains(std::env::current_exe().unwrap().to_str().unwrap()));
        assert!(!json.contains("name"));
        assert!(!json.contains("image_addr"));
    }

    #[test]
    fn rejects_untrusted_or_out_of_range_metadata() {
        let mut trace = Trace::capture().unwrap();
        trace.offsets.push(trace.image_size);
        assert!(!trace.valid());
        trace.offsets.pop();
        trace.code_id = Some("/home/CANARY".into());
        assert!(!trace.valid());
        trace.code_id = None;
        trace.debug_id = "CANARY".into();
        assert!(!trace.valid());
    }
}
