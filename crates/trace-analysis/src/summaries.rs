//! Configurable function models (inter-procedural summaries).
//!
//! A model relates the parameters of one function to each other (and to its
//! return value) so data flows through bodyless callees such as libc's
//! `memcpy_s` or project-specific wrappers. Models are matched by function
//! name at every resolved call site. See `docs/ANALYSIS.md` ("Function
//! models") for semantics and the configuration format.

use rustc_hash::FxHashMap;

/// One parameter relation of a function model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// `pts(param[dst]) ⊇ pts(param[src])` after the call.
    Alias { dst: u32, src: u32 },
    /// Bulk content copy `*dst <- *src` (memcpy family); realized as
    /// [`Effect::Alias`] over-approximation (see docs/ANALYSIS.md).
    MemCopy { dst: u32, src: u32 },
    /// `*param[ptr] = param[value]`.
    ContentStore { ptr: u32, value: u32 },
    /// Return value may be `param[param]` (bodyless callees only).
    ReturnAlias { param: u32 },
    /// Returns a fresh storage location (malloc family; bodyless callees).
    ReturnHeap,
    /// Terminator: memory reachable via `param[param]` is zeroed by this
    /// call. Introduces no values; kills are not modeled (flow-insensitive).
    Clears { param: u32 },
    /// Return value may be the address of an in-tree function whose name
    /// equals a string constant in `param[name_param]` (`dlsym` family).
    Dlsym { name_param: u32 },
    /// The callee may invoke this zero-argument callback. Parameter indexes
    /// count explicit arguments, excluding a member's `this`. Not restricted
    /// to a bodyless callee, unlike the effects above that introduce a value.
    Invoke { param: u32 },
}

/// A per-function summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FnModel {
    pub name: String,
    pub effects: Vec<Effect>,
}

impl FnModel {
    fn new(name: &str, effects: Vec<Effect>) -> Self {
        Self {
            name: name.to_string(),
            effects,
        }
    }
}

/// Model set: built-in defaults plus user configuration, matched by exact
/// function name. Later registrations override earlier ones.
#[derive(Debug, Default, Clone)]
pub struct FnModelSet {
    by_name: FxHashMap<String, FnModel>,
}

impl FnModelSet {
    /// Built-in libc / secure-libc defaults.
    pub fn builtin() -> Self {
        let mut set = Self::default();
        let mut reg = |name: &str, effects: Vec<Effect>| set.register(FnModel::new(name, effects));
        reg("ffrt::queue::submit", vec![Effect::Invoke { param: 0 }]);
        for n in ["memcpy", "memmove", "strcpy", "strncpy"] {
            reg(n, vec![Effect::MemCopy { dst: 0, src: 1 }]);
        }
        // Secure variants carry an extra destMax argument in slot 1.
        for n in [
            "memcpy_s",
            "memmove_s",
            "strcpy_s",
            "strncpy_s",
            "strcat_s",
            "strncat_s",
        ] {
            reg(n, vec![Effect::MemCopy { dst: 0, src: 2 }]);
        }
        for n in ["memset", "memset_s"] {
            reg(n, vec![Effect::Clears { param: 0 }]);
        }
        for n in [
            "malloc",
            "calloc",
            "zalloc",
            "kmalloc",
            "OsalMemAlloc",
            "OsalMemCalloc",
        ] {
            reg(n, vec![Effect::ReturnHeap]);
        }
        reg(
            "realloc",
            vec![Effect::ReturnAlias { param: 0 }, Effect::ReturnHeap],
        );
        for n in ["dlsym", "dlvsym", "GetProcAddress"] {
            reg(n, vec![Effect::Dlsym { name_param: 1 }]);
        }
        set
    }

    /// Look up a model by call-site / callee name. Exact match first, then
    /// the last `::` segment so `::dlsym` / `ns::dlsym` share the POSIX model.
    pub fn get_for_callee(&self, name: &str) -> Option<&FnModel> {
        if let Some(m) = self.by_name.get(name) {
            return Some(m);
        }
        name.rsplit("::")
            .next()
            .filter(|s| !s.is_empty() && *s != name)
            .and_then(|s| self.by_name.get(s))
    }

    pub fn register(&mut self, model: FnModel) {
        self.by_name.insert(model.name.clone(), model);
    }

    pub fn get(&self, name: &str) -> Option<&FnModel> {
        self.by_name.get(name)
    }

    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &FnModel> {
        self.by_name.values()
    }

    /// Parse a TOML configuration string (the contents of one `--models`
    /// file). Entries override same-name models already in `self`.
    pub fn merge_toml_str(&mut self, s: &str) -> Result<(), String> {
        let cfg: ModelsConfig = toml::from_str(s).map_err(|e| e.to_string())?;
        for raw in cfg.model {
            let effects = raw
                .effects
                .iter()
                .map(effect_from_toml)
                .collect::<Result<Vec<_>, _>>()?;
            // An explicitly empty model is allowed: it overrides (and thus
            // disables) a same-name built-in.
            self.register(FnModel {
                name: raw.name,
                effects,
            });
        }
        Ok(())
    }

    /// Parse a TOML configuration into a fresh set on top of the built-ins.
    pub fn from_toml_str(s: &str) -> Result<Self, String> {
        let mut set = Self::builtin();
        set.merge_toml_str(s)?;
        Ok(set)
    }
}

#[derive(serde::Deserialize)]
struct ModelsConfig {
    #[serde(default)]
    #[allow(dead_code)]
    version: Option<u64>,
    #[serde(rename = "model", default)]
    model: Vec<RawModel>,
}

#[derive(serde::Deserialize)]
struct RawModel {
    name: String,
    #[serde(default)]
    effects: Vec<RawEffect>,
}

#[derive(serde::Deserialize)]
struct RawEffect {
    kind: String,
    dst: Option<u32>,
    src: Option<u32>,
    ptr: Option<u32>,
    value: Option<u32>,
    param: Option<u32>,
}

fn effect_from_toml(raw: &RawEffect) -> Result<Effect, String> {
    let need = |v: Option<u32>, field: &str, kind: &str| -> Result<u32, String> {
        v.ok_or_else(|| format!("effect kind {kind:?} requires `{field}`"))
    };
    match raw.kind.as_str() {
        "alias" => Ok(Effect::Alias {
            dst: need(raw.dst, "dst", "alias")?,
            src: need(raw.src, "src", "alias")?,
        }),
        "mem_copy" => Ok(Effect::MemCopy {
            dst: need(raw.dst, "dst", "mem_copy")?,
            src: need(raw.src, "src", "mem_copy")?,
        }),
        "content_store" => Ok(Effect::ContentStore {
            ptr: need(raw.ptr, "ptr", "content_store")?,
            value: need(raw.value, "value", "content_store")?,
        }),
        "return_alias" => Ok(Effect::ReturnAlias {
            param: need(raw.param, "param", "return_alias")?,
        }),
        "return_heap" => Ok(Effect::ReturnHeap),
        "invoke" => Ok(Effect::Invoke {
            param: need(raw.param, "param", "invoke")?,
        }),
        "clears" => Ok(Effect::Clears {
            param: need(raw.param, "param", "clears")?,
        }),
        "dlsym" => Ok(Effect::Dlsym {
            name_param: need(raw.param, "param", "dlsym")?,
        }),
        other => Err(format!(
            "unknown effect kind {other:?} (expected alias, mem_copy, content_store, \
             return_alias, return_heap, clears, dlsym, invoke)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_covers_secure_variants_with_shifted_src() {
        let m = FnModelSet::builtin();
        assert_eq!(
            m.get("memcpy").unwrap().effects,
            vec![Effect::MemCopy { dst: 0, src: 1 }]
        );
        assert_eq!(
            m.get("memcpy_s").unwrap().effects,
            vec![Effect::MemCopy { dst: 0, src: 2 }]
        );
        assert_eq!(
            m.get("memset_s").unwrap().effects,
            vec![Effect::Clears { param: 0 }]
        );
        assert!(m.get("realloc").is_some());
        assert_eq!(
            m.get("dlsym").unwrap().effects,
            vec![Effect::Dlsym { name_param: 1 }]
        );
        assert_eq!(
            m.get_for_callee("::dlsym").unwrap().effects,
            vec![Effect::Dlsym { name_param: 1 }]
        );
        assert!(m.get("nope").is_none());
    }

    #[test]
    fn toml_config_parses_and_overrides_builtin() {
        let cfg = r#"
version = 1

[[model]]
name = "memcpy_s"
effects = [ { kind = "clears", param = 0 } ]

[[model]]
name = "MyWrapper"
effects = [
    { kind = "mem_copy", dst = 1, src = 0 },
    { kind = "content_store", ptr = 2, value = 3 },
]

[[model]]
name = "MyDlsym"
effects = [ { kind = "dlsym", param = 1 } ]
"#;
        let m = FnModelSet::from_toml_str(cfg).unwrap();
        assert_eq!(
            m.get("memcpy_s").unwrap().effects,
            vec![Effect::Clears { param: 0 }],
            "user config overrides builtin"
        );
        assert_eq!(
            m.get("MyWrapper").unwrap().effects,
            vec![
                Effect::MemCopy { dst: 1, src: 0 },
                Effect::ContentStore { ptr: 2, value: 3 }
            ]
        );
        assert_eq!(
            m.get("MyDlsym").unwrap().effects,
            vec![Effect::Dlsym { name_param: 1 }]
        );
        // Untouched built-ins survive.
        assert_eq!(
            m.get("memcpy").unwrap().effects,
            vec![Effect::MemCopy { dst: 0, src: 1 }]
        );
    }

    #[test]
    fn toml_rejects_unknown_kind_and_missing_fields() {
        assert!(FnModelSet::from_toml_str(
            "[[model]]\nname = \"x\"\neffects = [{ kind = \"wat\" }]\n"
        )
        .is_err());
        assert!(FnModelSet::from_toml_str(
            "[[model]]\nname = \"x\"\neffects = [{ kind = \"alias\", dst = 0 }]\n"
        )
        .is_err());
    }

    #[test]
    fn empty_model_disables_builtin() {
        let set =
            FnModelSet::from_toml_str("[[model]]\nname = \"memcpy\"\neffects = []\n").unwrap();
        assert!(set.get("memcpy").unwrap().effects.is_empty());
        // Unrelated built-ins stay intact.
        assert!(!set.get("memset_s").unwrap().effects.is_empty());
    }
}
