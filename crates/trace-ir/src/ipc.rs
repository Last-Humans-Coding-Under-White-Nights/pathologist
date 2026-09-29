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
    /// Interface descriptor from the `.idl` naming the stub the sender
    /// pairs with (`OHOS.Security.IAtm`); empty for pairs detected by name
    /// alone.
    pub descriptor: String,
}

/// One interface declared by an `.idl` file, with the classes synthesized
/// for it (docs/ANALYSIS.md, "IDL-generated interfaces").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdlInterface {
    /// Dotted IDL name as written (`OHOS.Security.IAtm`), idl-tool's interface descriptor.
    pub descriptor: String,
    /// `OHOS::Security::IAtm`
    pub interface: String,
    /// `OHOS::Security::AtmProxy`
    pub proxy: String,
    /// `OHOS::Security::AtmStub`
    pub stub: String,
    pub methods: Vec<IdlMethod>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdlMethod {
    pub name: String,
    pub ipccode: Option<u32>,
}
