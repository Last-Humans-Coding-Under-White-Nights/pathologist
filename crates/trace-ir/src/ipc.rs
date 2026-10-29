use crate::FnId;

/// A detected or modeled IPC bridge, produced during PAG construction.
///
/// Each bridge represents one concrete IPC call path: the proxy method sends
/// a transaction and the stub handler processes it. At PAG build time the
/// solver emits a synthetic `CallGraphEdge` (marked with the sentinel
/// `SYNTHETIC_CALL_SITE`) from `proxy_method` to `stub_handler`.
#[derive(Debug, Clone)]
pub struct IpcBridge {
    /// The sending proxy method or bodyless framework function whose model
    /// summarizes the IPC call (docs/ANALYSIS.md, "OpenHarmony IPC bridges").
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
    /// HDI stubs hold a separate implementation of `interface`; service-IDL
    /// stubs inherit it. Pairing searches interface implementations only for
    /// this composition model (docs/ANALYSIS.md, "IDL-generated interfaces").
    pub composed_stub: bool,
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
