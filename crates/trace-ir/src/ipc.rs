use crate::FnId;

/// A matched proxy→stub IPC bridge, produced by `detect_ipc_pairs`.
///
/// Each bridge represents one concrete IPC call path: the proxy method sends
/// a transaction and the stub handler processes it. At PAG build time the
/// solver emits a synthetic `CallGraphEdge` (marked with the sentinel
/// `SYNTHETIC_CALL_SITE`) from `proxy_method` to `stub_handler`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IpcBridge {
    /// The proxy method that initiates the IPC call.
    pub proxy_method: FnId,
    /// The stub handler that processes it (resolved to exact `FnId`).
    pub stub_handler: FnId,
    /// Interface descriptor string. Reserved for v2 (IDL-aware) matching and
    /// diagnostics; always empty in the name-based v1 detection.
    pub descriptor: String,
}

/// An IPC transaction initiated by a proxy method via `SendRequest`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct IpcSend {
    /// The proxy method executing `SendRequest`.
    pub proxy_method: FnId,
    /// Raw textual expression for the opcode argument.
    pub opcode_expr: String,
    /// Normalized opcode expression (casts and outer parentheses stripped).
    pub opcode_normalized: String,
    /// Constant integer value if directly evaluatable at lowering time.
    pub opcode_val: Option<u64>,
}

/// An IPC dispatch entry inside a stub (e.g., `switch (code)` or `if (code == ...)` in `OnRemoteRequest`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct IpcDispatch {
    /// The stub method (e.g. `FooStub::OnRemoteRequest`).
    pub stub_fn: FnId,
    /// Raw textual expression for the case or comparison opcode.
    pub opcode_expr: String,
    /// Normalized opcode expression (casts and outer parentheses stripped).
    pub opcode_normalized: String,
    /// Constant integer value if directly evaluatable at lowering time.
    pub opcode_val: Option<u64>,
    /// Candidates for the handler method called within this dispatch arm.
    pub callee_names: Vec<String>,
}

