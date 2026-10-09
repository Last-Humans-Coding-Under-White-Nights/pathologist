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
    /// The callee may invoke the callback in `param[param]`, passing it
    /// `args`, in an execution context of kind `context`. Parameter indexes
    /// count explicit arguments, excluding a member's `this`. Not restricted
    /// to a bodyless callee, unlike the effects above that introduce a value.
    Invoke {
        param: u32,
        args: InvokeArgs,
        context: ContextKind,
    },
}

/// Where a callback an [`Effect::Invoke`] model runs executes, as the model
/// states it (`docs/ANALYSIS.md`, "Execution contexts"). An IPC stub handler
/// is an [`ContextKind::IpcHandler`] context without a model; either way,
/// every IPC handler context is self-concurrent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
///
/// Declared in spelling order, so the derived `Ord` orders kinds as
/// [`ContextKind::as_str`] spells them (`execution_contexts.kind`).
pub enum ContextKind {
    /// A request handler on the IPC worker pool, which may run it twice at
    /// once.
    IpcHandler,
    /// A task on a pool of worker threads.
    PoolTask,
    /// A task on a queue that runs its tasks one at a time.
    SerialTask,
    /// A new thread (`pthread_create`, `std::thread`).
    Thread,
    /// The model does not say, or the callback may run on the caller's own
    /// thread.
    Unknown,
}

impl ContextKind {
    const ALL: [Self; 5] = [
        Self::Thread,
        Self::PoolTask,
        Self::SerialTask,
        Self::IpcHandler,
        Self::Unknown,
    ];

    /// The spelling in a `--models` file and in `execution_contexts.kind`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Thread => "thread",
            Self::PoolTask => "pool_task",
            Self::SerialTask => "serial_task",
            Self::IpcHandler => "ipc_handler",
            Self::Unknown => "unknown",
        }
    }

    fn parse(spelling: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == spelling)
    }
}

/// The arguments an [`Effect::Invoke`] callback is called with, as positions
/// among the modelled callee's own parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InvokeArgs {
    /// Exactly these, in the callback's parameter order; none for a
    /// zero-argument callback.
    Listed(Vec<u32>),
    /// Every parameter from this position on (`std::thread(f, args...)`).
    Rest(u32),
}

impl InvokeArgs {
    /// The callee parameter passed as the callback's `formal`-th parameter.
    pub fn actual_for(&self, formal: u32) -> Option<u32> {
        match self {
            Self::Listed(args) => args.get(formal as usize).copied(),
            Self::Rest(from) => from.checked_add(formal),
        }
    }

    /// Whether these pass callee parameter `param` on.
    fn forwards(&self, param: u32) -> bool {
        match self {
            Self::Listed(args) => args.contains(&param),
            Self::Rest(from) => *from <= param,
        }
    }

    /// Whether any argument is forwarded at all.
    pub fn is_empty(&self) -> bool {
        matches!(self, Self::Listed(args) if args.is_empty())
    }
}

/// The effects of a model that only runs one callback, in a `context`.
fn starts(param: u32, args: InvokeArgs, context: ContextKind) -> Vec<Effect> {
    vec![Effect::Invoke {
        param,
        args,
        context,
    }]
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
    /// Class → the model registered for its constructor (`C` → `C::C`).
    constructors: FxHashMap<String, String>,
    noise_macros: Vec<String>,
}

impl FnModelSet {
    /// Built-in libc / secure-libc defaults.
    pub fn builtin() -> Self {
        let mut set = Self::default();
        let mut reg = |name: &str, effects: Vec<Effect>| set.register(FnModel::new(name, effects));
        reg(
            "ffrt::queue::submit",
            starts(0, InvokeArgs::Listed(Vec::new()), ContextKind::SerialTask),
        );
        reg(
            "pthread_create",
            starts(2, InvokeArgs::Listed(vec![3]), ContextKind::Thread),
        );
        reg(
            "std::thread::thread",
            starts(0, InvokeArgs::Rest(1), ContextKind::Thread),
        );
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

    /// Look up a model by call-site / callee name. Exact match first; then
    /// the constructor's model for a class name, which is how a temporary is
    /// recorded when the unit never saw the class (`std::thread(f, x)`);
    /// then the last `::` segment so `::dlsym` / `ns::dlsym` share the POSIX
    /// model.
    pub fn get_for_callee(&self, name: &str) -> Option<&FnModel> {
        if let Some(m) = self.by_name.get(name) {
            return Some(m);
        }
        if let Some(m) = self
            .constructors
            .get(name)
            .and_then(|c| self.by_name.get(c))
        {
            return Some(m);
        }
        name.rsplit("::")
            .next()
            .filter(|s| !s.is_empty() && *s != name)
            .and_then(|s| self.by_name.get(s))
    }

    pub fn register(&mut self, model: FnModel) {
        if let Some((class, last)) = model.name.rsplit_once("::") {
            if class.rsplit("::").next() == Some(last) {
                self.constructors
                    .insert(class.to_string(), model.name.clone());
            }
        }
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

    pub fn noise_macros(&self) -> &[String] {
        &self.noise_macros
    }

    pub fn add_noise_macro(&mut self, name: impl Into<String>) {
        let name = name.into();
        if !self.noise_macros.contains(&name) {
            self.noise_macros.push(name);
        }
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
        if let Some(noise) = cfg.noise {
            for m in noise.macros {
                self.add_noise_macro(m);
            }
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
    #[serde(default)]
    noise: Option<RawNoise>,
}

#[derive(serde::Deserialize)]
struct RawNoise {
    #[serde(default)]
    macros: Vec<String>,
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
    args: Option<Vec<u32>>,
    rest: Option<u32>,
    context: Option<String>,
}

fn effect_from_toml(raw: &RawEffect) -> Result<Effect, String> {
    let need = |v: Option<u32>, field: &str, kind: &str| -> Result<u32, String> {
        v.ok_or_else(|| format!("effect kind {kind:?} requires `{field}`"))
    };
    if raw.kind != "invoke" && (raw.args.is_some() || raw.rest.is_some() || raw.context.is_some()) {
        return Err(format!(
            "effect kind {:?} takes no `args`, `rest` or `context`",
            raw.kind
        ));
    }
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
        "invoke" => {
            let param = need(raw.param, "param", "invoke")?;
            let args = match (&raw.args, raw.rest) {
                (Some(_), Some(_)) => {
                    return Err("effect kind \"invoke\" takes `args` or `rest`, not both".into())
                }
                (_, Some(from)) => InvokeArgs::Rest(from),
                (args, None) => InvokeArgs::Listed(args.clone().unwrap_or_default()),
            };
            if args.forwards(param) {
                return Err(format!(
                    "effect kind \"invoke\" passes the callback (param {param}) to itself"
                ));
            }
            let context = match raw.context.as_deref() {
                None => ContextKind::Unknown,
                Some(spelling) => ContextKind::parse(spelling).ok_or_else(|| {
                    format!(
                        "unknown invoke context {spelling:?} (expected thread, pool_task, \
                         serial_task, ipc_handler, unknown)"
                    )
                })?,
            };
            Ok(Effect::Invoke {
                param,
                args,
                context,
            })
        }
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
    fn builtin_thread_entries_forward_arguments() {
        let m = FnModelSet::builtin();
        let effects = |name: &str| m.get(name).unwrap().effects.clone();
        assert_eq!(
            effects("pthread_create"),
            starts(2, InvokeArgs::Listed(vec![3]), ContextKind::Thread)
        );
        assert_eq!(
            effects("std::thread::thread"),
            starts(0, InvokeArgs::Rest(1), ContextKind::Thread)
        );
        assert_eq!(
            effects("ffrt::queue::submit"),
            starts(0, InvokeArgs::Listed(Vec::new()), ContextKind::SerialTask)
        );
    }

    #[test]
    fn toml_invoke_states_the_context_it_starts() {
        let load = |effect: &str| {
            FnModelSet::from_toml_str(&format!("[[model]]\nname = \"x\"\neffects = [{effect}]\n"))
        };
        for kind in [
            ContextKind::Thread,
            ContextKind::PoolTask,
            ContextKind::SerialTask,
            ContextKind::IpcHandler,
            ContextKind::Unknown,
        ] {
            let set = load(&format!(
                r#"{{ kind = "invoke", param = 0, context = "{}" }}"#,
                kind.as_str()
            ))
            .unwrap();
            assert_eq!(
                set.get("x").unwrap().effects,
                starts(0, InvokeArgs::Listed(Vec::new()), kind)
            );
        }
        let unstated = load(r#"{ kind = "invoke", param = 0 }"#).unwrap();
        assert_eq!(
            unstated.get("x").unwrap().effects,
            starts(0, InvokeArgs::Listed(Vec::new()), ContextKind::Unknown),
            "a model that does not say what it starts starts an unknown context"
        );
        assert!(load(r#"{ kind = "invoke", param = 0, context = "fiber" }"#).is_err());
        assert!(
            load(r#"{ kind = "alias", dst = 0, src = 1, context = "thread" }"#).is_err(),
            "only `invoke` starts a context"
        );
    }

    #[test]
    fn toml_invoke_takes_listed_or_rest_arguments() {
        let m = FnModelSet::from_toml_str(
            r#"
[[model]]
name = "plain"
effects = [{ kind = "invoke", param = 0 }]
[[model]]
name = "listed"
effects = [{ kind = "invoke", param = 1, args = [2, 0] }]
[[model]]
name = "rest"
effects = [{ kind = "invoke", param = 0, rest = 1 }]
"#,
        )
        .unwrap();
        let effects = |name: &str| m.get(name).unwrap().effects.clone();
        assert_eq!(
            effects("plain"),
            starts(0, InvokeArgs::Listed(Vec::new()), ContextKind::Unknown)
        );
        assert_eq!(
            effects("listed"),
            starts(1, InvokeArgs::Listed(vec![2, 0]), ContextKind::Unknown)
        );
        assert_eq!(
            effects("rest"),
            starts(0, InvokeArgs::Rest(1), ContextKind::Unknown)
        );
        assert!(
            FnModelSet::from_toml_str(
                "[[model]]\nname = \"x\"\neffects = [{ kind = \"invoke\", param = 0, args = [1], rest = 2 }]\n"
            )
            .is_err(),
            "`args` and `rest` are alternatives"
        );
    }

    #[test]
    fn invoke_args_map_formals_to_call_site_positions() {
        let listed = InvokeArgs::Listed(vec![3, 1]);
        assert_eq!(listed.actual_for(0), Some(3));
        assert_eq!(listed.actual_for(1), Some(1));
        assert_eq!(listed.actual_for(2), None);
        let rest = InvokeArgs::Rest(1);
        assert_eq!(rest.actual_for(0), Some(1));
        assert_eq!(rest.actual_for(4), Some(5));
    }

    #[test]
    fn constructor_model_covers_a_temporary_of_its_class() {
        let m = FnModelSet::builtin();
        assert!(m.get("std::thread").is_none(), "one registration");
        assert_eq!(
            m.get_for_callee("std::thread").unwrap().name,
            "std::thread::thread"
        );
        let off =
            FnModelSet::from_toml_str("[[model]]\nname = \"std::thread::thread\"\neffects = []\n")
                .unwrap();
        assert!(off
            .get_for_callee("std::thread")
            .unwrap()
            .effects
            .is_empty());
    }

    #[test]
    fn toml_rejects_misplaced_invoke_arguments() {
        let load = |effect: &str| {
            FnModelSet::from_toml_str(&format!("[[model]]\nname = \"x\"\neffects = [{effect}]\n"))
        };
        for effect in [
            r#"{ kind = "return_alias", param = 0, rest = 1 }"#,
            r#"{ kind = "alias", dst = 0, src = 1, args = [2] }"#,
            r#"{ kind = "invoke", param = 1, args = [1] }"#,
            r#"{ kind = "invoke", param = 2, rest = 1 }"#,
        ] {
            assert!(load(effect).is_err(), "{effect}");
        }
        assert!(load(r#"{ kind = "invoke", param = 0, rest = 1 }"#).is_ok());
    }

    #[test]
    fn empty_model_disables_builtin() {
        let set =
            FnModelSet::from_toml_str("[[model]]\nname = \"memcpy\"\neffects = []\n").unwrap();
        assert!(set.get("memcpy").unwrap().effects.is_empty());
        // Unrelated built-ins stay intact.
        assert!(!set.get("memset_s").unwrap().effects.is_empty());
    }

    #[test]
    fn noise_macros_parsed_from_toml() {
        let cfg = r#"
[noise]
macros = ["TAG_LOG*", "HILOG_*", "LOGD"]
"#;
        let set = FnModelSet::from_toml_str(cfg).unwrap();
        assert_eq!(set.noise_macros(), &["TAG_LOG*", "HILOG_*", "LOGD"]);
    }
}
