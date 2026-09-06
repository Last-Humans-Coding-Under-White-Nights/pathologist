use crate::FnId;

/// A matched proxy→stub IPC bridge, produced by `detect_ipc_pairs`.
///
/// Each bridge represents one concrete IPC call path: the proxy method sends
/// a transaction and the stub handler processes it. At PAG build time the
/// solver emits a synthetic `CallGraphEdge` (marked with the sentinel
/// `SYNTHETIC_CALL_SITE`) from `proxy_method` to `stub_handler`.
#[derive(Debug, Clone)]
pub struct IpcBridge {
    /// The proxy method that initiates the IPC call.
    pub proxy_method: FnId,
    /// The stub handler that processes it (resolved to exact `FnId`).
    pub stub_handler: FnId,
    /// Interface descriptor string. Reserved for v2 (IDL-aware) matching and
    /// diagnostics; always empty in the name-based v1 detection.
    pub descriptor: String,
}
