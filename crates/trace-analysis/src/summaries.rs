//! Configurable function models (inter-procedural summaries).
//!
//! A model relates the parameters of one function to each other (and to its
//! return value) so data flows through bodyless callees such as libc's
//! `memcpy_s` or project-specific wrappers. Models are matched by function
//! name at every resolved call site. See `docs/ANALYSIS.md` ("Function
//! models") for semantics and the configuration format.

use rustc_hash::{FxHashMap, FxHashSet};
use std::cell::RefCell;
use trace_ir::Program;

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
        /// A value of this class the callee passes ahead of `args` to a
        /// callback whose first parameter is declared of it
        /// (`std::jthread`'s `std::stop_token`).
        supplies: Option<String>,
        /// Applies only at a call whose argument the test names passes it
        /// (`std::async`'s launch policy).
        when: Option<ArgType>,
    },
    /// The model names a virtual member a framework calls (`Thread::Run`):
    /// the member and every override of it the tree defines is the entry of
    /// an execution context of kind `context`, which no call site starts.
    Entry { context: ContextKind },
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

/// A test on the argument a call passes at `param` (`docs/ANALYSIS.md`,
/// "Function models"): whether it is (`is`) or is not a value of the class
/// or enumeration `class`, as its recorded value's declared type says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgType {
    pub param: u32,
    pub class: String,
    pub is: bool,
}

/// The type [`is_value_of`] tests a value against for the class or
/// enumeration named `class`; built once per test rather than per value.
pub(crate) fn class_type(class: &str) -> trace_ir::TypeDesc {
    trace_ir::TypeDesc::Struct {
        name: class.to_string(),
        fields: Vec::new(),
    }
}

/// Whether a value declared of type `desc` is a value of the class or
/// enumeration `named` ([`class_type`]): `Some(true)` when its type is that
/// class, however qualified (`std::launch`, or `launch` under a `using`),
/// `Some(false)` when it cannot be, and `None` when lowering could not tell
/// (an unresolved type, read as `int`, or a guessed class), which the
/// analysis reads both ways.
pub(crate) fn is_value_of(
    types: &trace_ir::TypeTable,
    desc: &trace_ir::TypeDesc,
    named: &trace_ir::TypeDesc,
) -> Option<bool> {
    if !trace_ir::may_name_same_type_in(types, desc, named) {
        Some(false)
    } else if desc.is_class_like() && trace_ir::may_name_same_type(desc, named) {
        Some(true)
    } else {
        None
    }
}

/// The effects of a model that only runs one callback, in a `context`: one
/// unconditional [`Effect::Invoke`] that supplies no argument of its own.
fn starts(param: u32, args: InvokeArgs, context: ContextKind) -> Vec<Effect> {
    vec![Effect::Invoke {
        param,
        args,
        context,
        supplies: None,
        when: None,
    }]
}

/// Whether the class a function is recorded under, `recorded`, may be the
/// class `model` names (`docs/ANALYSIS.md`, "Model matching"). A class the
/// tree declares is the class its name says. One it never
/// declares is named as written, or, spelled bare, in the innermost namespace
/// it is written in (`undeclared_class_guess` in lowering); C++ lookup from
/// there finds a class of an enclosing namespace too. So `recorded` may be
/// `model` when, past the trailing segments they share (the class name at
/// least), `recorded` keeps no namespace or one nested in `model`'s:
/// `AppExecFwk::EventHandler` and `OHOS::Camera::ThreadPool` may be
/// `OHOS::AppExecFwk::EventHandler` and `OHOS::ThreadPool`;
/// `OHOS::HiviewDFX::EventHandler` is not the former.
pub(crate) fn class_may_be(model: &str, recorded: &str, declared: bool) -> bool {
    let model = model.trim_start_matches("::");
    let recorded = recorded.trim_start_matches("::");
    if model == recorded {
        return true;
    }
    if declared {
        return false;
    }
    let shared = model
        .rsplit("::")
        .zip(recorded.rsplit("::"))
        .take_while(|(m, r)| m == r)
        .count();
    if shared == 0 {
        return false;
    }
    let model_scope = model.split("::").count() - shared;
    let recorded_scope = recorded.split("::").count() - shared;
    recorded_scope == 0
        || (recorded_scope >= model_scope
            && model
                .split("::")
                .zip(recorded.split("::"))
                .take(model_scope)
                .all(|(m, r)| m == r))
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

/// Model set: built-in defaults plus user configuration, keyed by name and
/// matched to functions through [`ModelLookup`]. Later registrations override
/// earlier ones of the same name.
#[derive(Debug, Default, Clone)]
pub struct FnModelSet {
    by_name: FxHashMap<String, FnModel>,
    /// Class → the model registered for its constructor (`C` → `C::C`).
    constructors: FxHashMap<String, String>,
    /// Member name → the qualified models of that name (`PostTask` →
    /// `OHOS::AppExecFwk::EventHandler::PostTask`), sorted, for matching a
    /// member of a class that may be or derive from the model's.
    members: FxHashMap<String, Vec<String>>,
    noise_macros: Vec<String>,
}

impl FnModelSet {
    /// Built-in libc / secure-libc defaults.
    pub fn builtin() -> Self {
        let mut set = Self::default();
        let mut reg = |name: &str, effects: Vec<Effect>| set.register(FnModel::new(name, effects));
        // Task, thread and timer primitives (`docs/ANALYSIS.md`, "Built-in
        // models"): each runs one callback, in the context it states.
        use ContextKind::{PoolTask, SerialTask, Thread};
        // The callback alone.
        for (name, param, context) in [
            ("ffrt::submit", 0, PoolTask),
            ("ffrt::submit_h", 0, PoolTask),
            ("ffrt::queue::submit", 0, SerialTask),
            ("ffrt::queue::submit_h", 0, SerialTask),
            ("ffrt_submit_base", 0, PoolTask),
            ("ffrt_submit_h_base", 0, PoolTask),
            ("ffrt_queue_submit", 1, SerialTask),
            ("ffrt_queue_submit_h", 1, SerialTask),
            ("OHOS::ThreadPool::AddTask", 0, PoolTask),
            ("OHOS::Utils::Timer::Register", 0, SerialTask),
        ] {
            reg(name, starts(param, InvokeArgs::Listed(Vec::new()), context));
        }
        // The callback and the one argument it is called with.
        for (name, param, arg, context) in [
            ("HdfWorkInit", 1, 2, SerialTask),
            ("HdfDelayedWorkInit", 1, 2, SerialTask),
            ("OsalTimerCreate", 2, 3, Thread),
            ("pthread_create", 2, 3, Thread),
        ] {
            reg(name, starts(param, InvokeArgs::Listed(vec![arg]), context));
        }
        // A callable and everything after it; `std::jthread` hands a
        // callable that takes a stop token its own first.
        reg(
            "std::thread::thread",
            starts(0, InvokeArgs::Rest(1), Thread),
        );
        reg(
            "std::jthread::jthread",
            vec![Effect::Invoke {
                param: 0,
                args: InvokeArgs::Rest(1),
                context: Thread,
                supplies: Some("std::stop_token".into()),
                when: None,
            }],
        );
        // An event handler's `PostTask` family: the callback first, then a
        // name, delay or priority.
        for method in [
            "PostTask",
            "PostImmediateTask",
            "PostHighPriorityTask",
            "PostIdleTask",
            "PostSyncTask",
            "PostTimingTask",
            "PostTaskAtFront",
        ] {
            reg(
                &format!("OHOS::AppExecFwk::EventHandler::{method}"),
                starts(0, InvokeArgs::Listed(Vec::new()), SerialTask),
            );
        }
        // The callable first, or after a launch policy.
        reg(
            "std::async",
            [(0, 1, false), (1, 2, true)]
                .into_iter()
                .map(|(param, rest, policy)| Effect::Invoke {
                    param,
                    args: InvokeArgs::Rest(rest),
                    context: Thread,
                    supplies: None,
                    when: Some(ArgType {
                        param: 0,
                        class: "std::launch".into(),
                        is: policy,
                    }),
                })
                .collect(),
        );
        // Virtual members a framework runs on its own thread, queue or pool.
        for (name, context) in [
            ("OHOS::Thread::Run", Thread),
            ("OHOS::AppExecFwk::EventHandler::ProcessEvent", SerialTask),
            (
                "OHOS::IRemoteObject::DeathRecipient::OnRemoteDied",
                ContextKind::IpcHandler,
            ),
        ] {
            reg(name, vec![Effect::Entry { context }]);
        }
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

    /// The model named for the function or call named `name`
    /// (`docs/ANALYSIS.md`, "Model matching", rules 1-3): the exact name;
    /// else the constructor's model for a class name, which is how a
    /// temporary is recorded when the unit never saw the class
    /// (`std::thread(f, x)`); else the last `::` segment, so `::dlsym` /
    /// `ns::dlsym` share the POSIX model. Authoritative for every effect of
    /// the function: an empty one disables. The analysis matches through
    /// [`ModelLookup`], which applies these rules to `dlsym`, `invoke` and
    /// `entry` effects only and falls back to the class rule.
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

    /// The model rules 1-3 name for the function or call named `name`, for
    /// effects of `group` (`docs/ANALYSIS.md`, "Model matching"). Argument
    /// and return effects take only the exact name, written or after a
    /// leading `::` or `std::`, which name the same C library function; the
    /// other groups take [`Self::get_for_callee`]'s rules.
    fn named(&self, name: &str, group: EffectGroup) -> Option<&FnModel> {
        match group {
            EffectGroup::Args | EffectGroup::Return => {
                let global = name.trim_start_matches("::");
                let unstd = global.strip_prefix("std::").unwrap_or(global);
                [name, global, unstd]
                    .into_iter()
                    .find_map(|n| self.by_name.get(n))
            }
            EffectGroup::Dlsym | EffectGroup::Invoke | EffectGroup::Entry => {
                self.get_for_callee(name)
            }
        }
    }

    /// The matcher one analysis of `program` looks every model up through
    /// (`docs/ANALYSIS.md`, "Model matching").
    pub fn lookup<'m>(&'m self, program: &'m Program) -> ModelLookup<'m> {
        ModelLookup {
            models: self,
            program,
            member_models: RefCell::default(),
        }
    }

    pub fn register(&mut self, model: FnModel) {
        if let Some((class, last)) = model.name.rsplit_once("::") {
            if class.rsplit("::").next() == Some(last) {
                self.constructors
                    .insert(class.to_string(), model.name.clone());
            }
            let members = self.members.entry(last.to_string()).or_default();
            if let Err(at) = members.binary_search(&model.name) {
                members.insert(at, model.name.clone());
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
            if !raw.name.contains("::") && effects.iter().any(|e| matches!(e, Effect::Entry { .. }))
            {
                return Err(format!(
                    "model {:?}: an `entry` names a member, `Class::method`",
                    raw.name
                ));
            }
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

/// Which part of the analysis applies an effect. A member model another
/// class's member takes is chosen per group (`docs/ANALYSIS.md`, "Model
/// matching"): the first matching model with an effect of the group, so a
/// nearer model that has none does not hide a farther one that has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectGroup {
    /// `alias`, `mem_copy`, `content_store`, `clears`: at each resolved call.
    Args,
    /// `return_alias`, `return_heap`: into a bodyless callee's call result.
    Return,
    /// `dlsym`.
    Dlsym,
    /// `invoke`: a callback the callee runs.
    Invoke,
    /// `entry`: a member a framework calls.
    Entry,
}

impl Effect {
    /// The part of the analysis that applies this effect.
    fn group(&self) -> EffectGroup {
        match self {
            Self::Alias { .. }
            | Self::MemCopy { .. }
            | Self::ContentStore { .. }
            | Self::Clears { .. } => EffectGroup::Args,
            Self::ReturnAlias { .. } | Self::ReturnHeap => EffectGroup::Return,
            Self::Dlsym { .. } => EffectGroup::Dlsym,
            Self::Invoke { .. } => EffectGroup::Invoke,
            Self::Entry { .. } => EffectGroup::Entry,
        }
    }
}

impl FnModel {
    /// Whether any effect of this model is of `group`.
    fn has(&self, group: EffectGroup) -> bool {
        self.effects.iter().any(|e| e.group() == group)
    }
}

/// The one matcher every model lookup of an analysis goes through
/// (`docs/ANALYSIS.md`, "Model matching"): the model named for the function
/// (`FnModelSet::named`), else the member models of a class the
/// function's class may be or derive from, chosen per [`EffectGroup`]. The class rule's candidates for a name are
/// computed once and kept, so the passes that look the same function up
/// (call sites, callbacks, framework entries) walk its class hierarchy once.
pub struct ModelLookup<'m> {
    models: &'m FnModelSet,
    program: &'m Program,
    /// Function name → the member models the class rule admits for it,
    /// nearest class first, then by model name.
    member_models: RefCell<FxHashMap<String, Vec<&'m FnModel>>>,
}

impl<'m> ModelLookup<'m> {
    /// The model that gives the function named `name` its effects of
    /// `group`, if any.
    pub fn model(&self, name: &str, group: EffectGroup) -> Option<&'m FnModel> {
        self.find(name, group, |model| model.has(group).then_some(()))
            .map(|(model, ())| model)
    }

    /// The `entry` model the function named `name` is the member of or an
    /// override of, with the context it states (`docs/ANALYSIS.md`,
    /// "Execution contexts").
    pub fn entry(&self, name: &str) -> Option<(&'m FnModel, ContextKind)> {
        self.find(name, EffectGroup::Entry, |model| {
            model.effects.iter().find_map(|effect| match effect {
                Effect::Entry { context } => Some(*context),
                _ => None,
            })
        })
    }

    /// The first model for the function named `name`, in matching order
    /// for effects of `group`, that `pick` accepts, with what `pick` took
    /// from it.
    fn find<T>(
        &self,
        name: &str,
        group: EffectGroup,
        pick: impl Fn(&'m FnModel) -> Option<T>,
    ) -> Option<(&'m FnModel, T)> {
        let accept = |model: &'m FnModel| pick(model).map(|t| (model, t));
        if let Some(model) = self.models.named(name, group) {
            return accept(model);
        }
        let (class, member) = name.rsplit_once("::")?;
        let candidates = self.models.members.get(member)?;
        if let Some(matched) = self.member_models.borrow().get(name) {
            return matched.iter().copied().find_map(accept);
        }
        // `ancestor_closure` is breadth-first, so a model's first match is
        // its nearest one.
        let mut ranked: Vec<(usize, &'m str)> = Vec::new();
        let mut seen: FxHashSet<&str> = FxHashSet::default();
        for (class, d) in self.program.ancestor_closure(class) {
            let declared = self.program.types.is_struct_declared(class);
            for model_name in candidates {
                let (model_class, _) = model_name
                    .rsplit_once("::")
                    .expect("a member model is qualified");
                if class_may_be(model_class, class, declared) && seen.insert(model_name) {
                    ranked.push((d, model_name));
                }
            }
        }
        // Nearest class first, then model name: not the order the bases of
        // a class are declared in.
        ranked.sort_unstable();
        let matched: Vec<&'m FnModel> = ranked
            .into_iter()
            .map(|(_, model_name)| &self.models.by_name[model_name])
            .collect();
        let found = matched.iter().copied().find_map(accept);
        self.member_models
            .borrow_mut()
            .insert(name.to_string(), matched);
        found
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
    supplies: Option<String>,
    if_arg: Option<RawArgType>,
    unless_arg: Option<RawArgType>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawArgType {
    param: u32,
    #[serde(rename = "type")]
    class: String,
}

fn effect_from_toml(raw: &RawEffect) -> Result<Effect, String> {
    let need = |v: Option<u32>, field: &str, kind: &str| -> Result<u32, String> {
        v.ok_or_else(|| format!("effect kind {kind:?} requires `{field}`"))
    };
    if raw.kind != "invoke"
        && (raw.args.is_some()
            || raw.rest.is_some()
            || raw.supplies.is_some()
            || raw.if_arg.is_some()
            || raw.unless_arg.is_some())
    {
        return Err(format!(
            "effect kind {:?} takes no `args`, `rest`, `supplies`, `if_arg` or `unless_arg`",
            raw.kind
        ));
    }
    if !matches!(raw.kind.as_str(), "invoke" | "entry") && raw.context.is_some() {
        return Err(format!("effect kind {:?} takes no `context`", raw.kind));
    }
    let context = || match raw.context.as_deref() {
        None => Ok(ContextKind::Unknown),
        Some(spelling) => ContextKind::parse(spelling).ok_or_else(|| {
            let expected: Vec<&str> = ContextKind::ALL.iter().map(|k| k.as_str()).collect();
            format!(
                "unknown context {spelling:?} (expected {})",
                expected.join(", ")
            )
        }),
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
            let class = |spelling: &str, field: &str| {
                let class = spelling.trim().trim_start_matches("::");
                if class.is_empty() || class.contains(['<', '>', '*', '&', ' ']) {
                    Err(format!(
                        "effect kind \"invoke\": `{field}` names a class or enumeration, \
                         not {spelling:?}"
                    ))
                } else {
                    Ok(class.to_string())
                }
            };
            let supplies = raw
                .supplies
                .as_deref()
                .map(|s| class(s, "supplies"))
                .transpose()?;
            let when = match (&raw.if_arg, &raw.unless_arg) {
                (Some(_), Some(_)) => {
                    return Err(
                        "effect kind \"invoke\" takes `if_arg` or `unless_arg`, not both".into(),
                    )
                }
                (Some(test), None) | (None, Some(test)) => Some(ArgType {
                    param: test.param,
                    class: class(&test.class, "type")?,
                    is: raw.if_arg.is_some(),
                }),
                (None, None) => None,
            };
            Ok(Effect::Invoke {
                param,
                args,
                context: context()?,
                supplies,
                when,
            })
        }
        "entry" => {
            let operands = [raw.dst, raw.src, raw.ptr, raw.value, raw.param];
            if operands.iter().any(Option::is_some) {
                return Err(
                    "effect kind \"entry\" takes only `context`: it names no parameter".into(),
                );
            }
            Ok(Effect::Entry {
                context: context()?,
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
             return_alias, return_heap, clears, dlsym, invoke, entry)"
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
    fn toml_invoke_supplies_a_leading_value_and_tests_an_argument() {
        let load = |effect: &str| {
            FnModelSet::from_toml_str(&format!("[[model]]\nname = \"x\"\neffects = [{effect}]\n"))
        };
        let effects = |effect: &str| load(effect).unwrap().get("x").unwrap().effects.clone();
        assert_eq!(
            effects(r#"{ kind = "invoke", param = 0, rest = 1, supplies = "::std::stop_token" }"#),
            vec![Effect::Invoke {
                param: 0,
                args: InvokeArgs::Rest(1),
                context: ContextKind::Unknown,
                supplies: Some("std::stop_token".into()),
                when: None,
            }]
        );
        for (field, is) in [("if_arg", true), ("unless_arg", false)] {
            assert_eq!(
                effects(&format!(
                    r#"{{ kind = "invoke", param = 1, rest = 2, {field} = {{ param = 0, type = "std::launch" }} }}"#
                )),
                vec![Effect::Invoke {
                    param: 1,
                    args: InvokeArgs::Rest(2),
                    context: ContextKind::Unknown,
                    when: Some(ArgType {
                        param: 0,
                        class: "std::launch".into(),
                        is,
                    }),
                    supplies: None,
                }],
                "{field}"
            );
        }
        for effect in [
            r#"{ kind = "alias", dst = 0, src = 1, supplies = "T" }"#,
            r#"{ kind = "alias", dst = 0, src = 1, if_arg = { param = 0, type = "T" } }"#,
            r#"{ kind = "invoke", param = 0, supplies = "" }"#,
            r#"{ kind = "invoke", param = 0, supplies = "std::vector<int>" }"#,
            r#"{ kind = "invoke", param = 1, if_arg = { param = 0, type = "T *" } }"#,
            r#"{ kind = "invoke", param = 1, if_arg = { param = 0 } }"#,
            r#"{ kind = "invoke", param = 1, if_arg = { param = 0, type = "T", is = true } }"#,
            r#"{ kind = "invoke", param = 1, if_arg = { param = 0, type = "T" }, unless_arg = { param = 0, type = "T" } }"#,
        ] {
            assert!(load(effect).is_err(), "{effect}");
        }
    }

    #[test]
    fn a_value_is_of_a_class_by_its_declared_type_or_may_be_when_unresolved() {
        use trace_ir::{TypeDesc, TypeTable};
        let mut types = TypeTable::new();
        let class = |name: &str| TypeDesc::Struct {
            name: name.into(),
            fields: Vec::new(),
        };
        types.note_guessed_class("Policy");
        for (desc, verdict) in [
            (class("std::launch"), Some(true)),
            (class("launch"), Some(true)),
            (class("std::function<void()>"), Some(false)),
            (
                TypeDesc::FnPtr {
                    ret: Box::new(TypeDesc::Void),
                    params: Vec::new(),
                },
                Some(false),
            ),
            (TypeDesc::Ptr(Box::new(class("std::launch"))), Some(false)),
            (TypeDesc::Unknown, None),
            (TypeDesc::Int, None),
            (class("Policy"), None),
        ] {
            assert_eq!(
                is_value_of(&types, &desc, &class_type("std::launch")),
                verdict,
                "{desc:?}"
            );
        }
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
    fn builtin_task_primitives_name_their_callback_and_context() {
        let m = FnModelSet::builtin();
        let effects = |name: &str| {
            m.get(name)
                .unwrap_or_else(|| panic!("{name}"))
                .effects
                .clone()
        };
        let none = || InvokeArgs::Listed(Vec::new());
        for name in ["ffrt::submit", "ffrt::submit_h"] {
            assert_eq!(
                effects(name),
                starts(0, none(), ContextKind::PoolTask),
                "{name}"
            );
        }
        for name in ["ffrt::queue::submit", "ffrt::queue::submit_h"] {
            assert_eq!(
                effects(name),
                starts(0, none(), ContextKind::SerialTask),
                "{name}"
            );
        }
        for name in ["ffrt_submit_base", "ffrt_submit_h_base"] {
            assert_eq!(
                effects(name),
                starts(0, none(), ContextKind::PoolTask),
                "{name}"
            );
        }
        for name in ["ffrt_queue_submit", "ffrt_queue_submit_h"] {
            assert_eq!(
                effects(name),
                starts(1, none(), ContextKind::SerialTask),
                "{name}"
            );
        }
        for method in [
            "PostTask",
            "PostImmediateTask",
            "PostHighPriorityTask",
            "PostIdleTask",
            "PostSyncTask",
            "PostTimingTask",
            "PostTaskAtFront",
        ] {
            let name = format!("OHOS::AppExecFwk::EventHandler::{method}");
            assert_eq!(
                effects(&name),
                starts(0, none(), ContextKind::SerialTask),
                "{name}"
            );
        }
        assert_eq!(
            effects("OHOS::ThreadPool::AddTask"),
            starts(0, none(), ContextKind::PoolTask)
        );
        assert_eq!(
            effects("OHOS::Utils::Timer::Register"),
            starts(0, none(), ContextKind::SerialTask)
        );
        for name in ["HdfWorkInit", "HdfDelayedWorkInit"] {
            assert_eq!(
                effects(name),
                starts(1, InvokeArgs::Listed(vec![2]), ContextKind::SerialTask),
                "{name}"
            );
        }
        assert_eq!(
            effects("OsalTimerCreate"),
            starts(2, InvokeArgs::Listed(vec![3]), ContextKind::Thread)
        );
        let policy = |is: bool| {
            Some(ArgType {
                param: 0,
                class: "std::launch".into(),
                is,
            })
        };
        assert_eq!(
            effects("std::async"),
            vec![
                Effect::Invoke {
                    param: 0,
                    args: InvokeArgs::Rest(1),
                    context: ContextKind::Thread,
                    when: policy(false),
                    supplies: None,
                },
                Effect::Invoke {
                    param: 1,
                    args: InvokeArgs::Rest(2),
                    context: ContextKind::Thread,
                    when: policy(true),
                    supplies: None,
                },
            ],
            "the callable first unless a launch policy is"
        );
        assert_eq!(
            effects("std::jthread::jthread"),
            vec![Effect::Invoke {
                param: 0,
                args: InvokeArgs::Rest(1),
                context: ContextKind::Thread,
                supplies: Some("std::stop_token".into()),
                when: None,
            }]
        );
        for (name, context) in [
            ("OHOS::Thread::Run", ContextKind::Thread),
            (
                "OHOS::AppExecFwk::EventHandler::ProcessEvent",
                ContextKind::SerialTask,
            ),
            (
                "OHOS::IRemoteObject::DeathRecipient::OnRemoteDied",
                ContextKind::IpcHandler,
            ),
        ] {
            assert_eq!(effects(name), vec![Effect::Entry { context }], "{name}");
        }
    }

    #[test]
    fn a_class_name_may_be_the_models_as_c_plus_plus_lookup_finds_it() {
        // Written as declared, or with leading namespaces left out.
        assert!(class_may_be("OHOS::ThreadPool", "OHOS::ThreadPool", true));
        assert!(class_may_be(
            "OHOS::AppExecFwk::EventHandler",
            "AppExecFwk::EventHandler",
            false
        ));
        // A class the tree never declares is named in the innermost
        // namespace it is written in; lookup there finds an outer one.
        assert!(class_may_be(
            "OHOS::ThreadPool",
            "OHOS::Camera::ThreadPool",
            false
        ));
        assert!(class_may_be("OHOS::Thread", "OHOS::Camera::Thread", false));
        assert!(class_may_be("Executor", "OHOS::Camera::Executor", false));
        // Not from a sibling namespace, and never a class the tree declares
        // under another name.
        assert!(!class_may_be(
            "OHOS::AppExecFwk::EventHandler",
            "OHOS::HiviewDFX::EventHandler",
            false
        ));
        assert!(!class_may_be(
            "OHOS::Thread",
            "OHOS::HiviewDFX::Thread",
            true
        ));
        assert!(!class_may_be("OHOS::ThreadPool", "OHOS::Pool", false));
        assert!(!class_may_be(
            "OHOS::ThreadPool",
            "ThreadPool::Inner",
            false
        ));
    }

    #[test]
    fn toml_entry_states_the_context_an_override_runs_in() {
        let load = |name: &str, effect: &str| {
            FnModelSet::from_toml_str(&format!(
                "[[model]]\nname = \"{name}\"\neffects = [{effect}]\n"
            ))
        };
        let set = load("Loop::Run", r#"{ kind = "entry", context = "thread" }"#).unwrap();
        assert_eq!(
            set.get("Loop::Run").unwrap().effects,
            vec![Effect::Entry {
                context: ContextKind::Thread
            }]
        );
        let unstated = load("Loop::Run", r#"{ kind = "entry" }"#).unwrap();
        assert_eq!(
            unstated.get("Loop::Run").unwrap().effects,
            vec![Effect::Entry {
                context: ContextKind::Unknown
            }]
        );
        assert!(
            load("Run", r#"{ kind = "entry", context = "thread" }"#).is_err(),
            "an entry is a member of a class"
        );
        for effect in [
            r#"{ kind = "entry", param = 0 }"#,
            r#"{ kind = "entry", args = [1] }"#,
            r#"{ kind = "entry", rest = 1 }"#,
        ] {
            assert!(load("Loop::Run", effect).is_err(), "{effect}");
        }
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
