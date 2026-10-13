//! Serving FETCH_DATA from the host through the engine's transport.

use std::sync::Arc;

use medialake_cloudapi::{CloudError, CloudResult, FetchRequest, Provider, Transfer, plan_chunks};
use medialake_sync::member::transport::{RangeFetch, Transport};

/// Largest single request to the host (and single TRANSFER_DATA).
pub const CHUNK: u64 = 4 << 20;

/// A transport error that means "the host cannot be reached" (Explorer then fails at once with
/// "network unavailable" instead of waiting for a time-out).
pub fn looks_offline(msg: &str) -> bool {
    let m = msg.to_lowercase();
    ["cannot connect", "cannot resolve", "connection refused", "connection reset", "timed out", "broken pipe", "is this host a medialake host"]
        .iter()
        .any(|p| m.contains(p))
}

pub struct HostProvider {
    tr: Arc<dyn Transport>,
}

impl HostProvider {
    pub fn new(tr: Arc<dyn Transport>) -> HostProvider {
        HostProvider { tr }
    }
}

impl Provider for HostProvider {
    fn fetch(&self, req: &FetchRequest, out: &dyn Transfer) -> CloudResult<()> {
        // without the identity there is no revision to ask the host for
        let Some(id) = &req.identity else {
            return Err(CloudError::Failed(format!("{}: the placeholder has no revision", req.rel)));
        };
        let pieces = plan_chunks(req.offset, req.length, req.file_size, CHUNK);
        let total: u64 = pieces.iter().map(|(_, l)| l).sum();
        let mut done = 0u64;
        for (offset, len) in pieces {
            match self.tr.read_range(&req.rel, id.rev, offset, len) {
                Ok(RangeFetch::Data(d)) if d.len() as u64 == len => {
                    out.data(offset, &d)?;
                    done += len;
                    out.progress(total, done);
                }
                // shorter or longer than the placeholder says: the host's file is another one by now
                Ok(RangeFetch::Data(_)) | Ok(RangeFetch::Stale) => {
                    return Err(CloudError::Failed(format!("{} changed on the host; it is being updated", req.rel)));
                }
                Ok(RangeFetch::NotFound) => return Err(CloudError::NotFound(format!("{} is no longer on the host", req.rel))),
                Err(e) => {
                    let msg = format!("{e:#}");
                    return Err(if looks_offline(&msg) { CloudError::Offline(msg) } else { CloudError::Failed(msg) });
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_errors_are_told_apart() {
        assert!(looks_offline("cannot connect to 10.0.0.2:47900: Connection refused"));
        assert!(looks_offline("download failed: operation Timed Out"));
        assert!(!looks_offline("access denied: no access to sync x"));
        assert!(!looks_offline("a.txt: rejected by the host: ignored name"));
    }
}
