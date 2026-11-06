use crate::FnId;

/// Whether a callee spelling names an OpenHarmony IPC send operation.
/// Shared by bridge detection and exported linking facts; see
/// docs/ANALYSIS.md, "OpenHarmony IPC bridges".
pub fn is_ipc_send(name: &str) -> bool {
    let mut name = name.trim();
    // Explicit template arguments may contain qualified or nested types.
    // Remove the whole trailing argument list before splitting the receiver.
    if name.ends_with('>') {
        let mut depth = 0;
        for (offset, ch) in name.char_indices().rev() {
            match ch {
                '>' => depth += 1,
                '<' => {
                    depth -= 1;
                    if depth == 0 {
                        name = name[..offset].trim_end();
                        break;
                    }
                }
                _ => {}
            }
        }
    }
    // An unresolved receiver may retain its member-expression spelling.
    let callable = name.rsplit([':', '>', '.']).next().unwrap_or("").trim();
    let callable = callable
        .strip_prefix("template")
        .filter(|rest| rest.starts_with(char::is_whitespace))
        .map(str::trim_start)
        .unwrap_or(callable);
    matches!(callable, "SendRequest" | "SendTransactCmd")
}

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
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IdlMethod {
    pub name: String,
    pub ipccode: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::is_ipc_send;

    #[test]
    fn send_match_uses_exact_final_callable_name() {
        for method in ["SendRequest", "SendTransactCmd"] {
            for prefix in ["", "OHOS::FooProxy::", "remote->", "remote."] {
                assert!(is_ipc_send(&format!("{prefix}{method}")));
                assert!(!is_ipc_send(&format!("{prefix}{method}Async")));
                assert!(!is_ipc_send(&format!("{prefix}Log{method}Result")));
            }
        }
        assert!(!is_ipc_send(""));
    }

    #[test]
    fn send_match_accepts_template_keyword_and_arguments() {
        for method in ["SendRequest", "SendTransactCmd"] {
            for prefix in [
                "",
                "OHOS::FooProxy<int>::",
                "this->",
                "helper_->",
                "helper_.",
            ] {
                for keyword in ["", "template ", "template\t"] {
                    for args in ["< int>", "<OHOS::FooCode>", "<Code<int>>"] {
                        assert!(is_ipc_send(&format!("{prefix}{keyword}{method}{args}")));
                        assert!(!is_ipc_send(&format!(
                            "{prefix}{keyword}{method}Async{args}"
                        )));
                        assert!(!is_ipc_send(&format!(
                            "{prefix}{keyword}Log{method}Result{args}"
                        )));
                    }
                }
            }
        }
        assert!(!is_ipc_send("helper_->Send<SendRequest>"));
        assert!(!is_ipc_send("helper_->templateSendRequest<int>"));
    }
}
