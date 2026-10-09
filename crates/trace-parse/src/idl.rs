//! OpenHarmony `.idl` interface definitions: the grammar, the idl-tool naming
//! convention and the declarations idl-tool would generate
//! (docs/ANALYSIS.md, "IDL-generated interfaces").

use crate::deps::VirtualHeaders;
use crate::discover::DiscoveredFiles;
use rustc_hash::{FxHashMap, FxHashSet};
use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use trace_ir::TestPartition;
use trace_preproc::PreprocessOptions;

/// One parsed `.idl` file.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct File {
    pub interfaces: Vec<Interface>,
    /// `sequenceable a.b.C;` and `rawdata a.b.C;`, as dotted segments.
    pub sequenceables: Vec<Vec<String>>,
    /// Forward `interface a.b.IFoo;` declarations.
    pub interface_refs: Vec<Vec<String>>,
    pub enums: Vec<String>,
    /// `interface_token a.b.C;`: the descriptor the file's interfaces use
    /// instead of their own names.
    pub interface_token: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Interface {
    /// Dotted name with its package: `["OHOS", "Security", "IAtm"]`.
    pub qualified: Vec<String>,
    /// `extends a.b.IBase`, qualified like `qualified`.
    pub extends: Option<Vec<String>>,
    pub methods: Vec<Method>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Method {
    pub name: String,
    pub ipccode: Option<u32>,
    pub ret: Type,
    pub params: Vec<Param>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Param {
    pub dir: Direction,
    pub ty: Type,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    In,
    Out,
    InOut,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Type {
    /// Dotted segments; `unsigned int` is the single segment `"unsigned int"`.
    Named(Vec<String>),
    Generic(String, Vec<Type>),
    Array(Box<Type>),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ParseError {
    pub line: u32,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    Int(u64),
    Str,
    Punct(char),
}

fn lex(text: &str) -> Result<Vec<(Tok, u32)>, ParseError> {
    let b = text.as_bytes();
    let (mut i, mut line, mut out) = (0usize, 1u32, Vec::new());
    while i < b.len() {
        let c = b[i] as char;
        match c {
            '\n' => {
                line += 1;
                i += 1;
            }
            c if c.is_ascii_whitespace() => i += 1,
            '/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            '/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i < b.len() && !(b[i] == b'*' && b.get(i + 1) == Some(&b'/')) {
                    if b[i] == b'\n' {
                        line += 1;
                    }
                    i += 1;
                }
                if i >= b.len() {
                    return Err(ParseError {
                        line,
                        message: "unterminated comment".into(),
                    });
                }
                i += 2;
            }
            '"' => {
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    if b.get(i) == Some(&b'\n') {
                        line += 1;
                    }
                    i += 1;
                }
                if i >= b.len() {
                    return Err(ParseError {
                        line,
                        message: "unterminated string".into(),
                    });
                }
                i += 1;
                out.push((Tok::Str, line));
            }
            c if c.is_ascii_digit() => {
                let start = i;
                while i < b.len() && (b[i] as char).is_ascii_alphanumeric() {
                    i += 1;
                }
                let s = &text[start..i];
                let v = if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
                    u64::from_str_radix(h, 16)
                } else {
                    s.parse()
                };
                let v = v.map_err(|_| ParseError {
                    line,
                    message: format!("bad number `{s}`"),
                })?;
                out.push((Tok::Int(v), line));
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let start = i;
                while i < b.len() && ((b[i] as char).is_ascii_alphanumeric() || b[i] == b'_') {
                    i += 1;
                }
                out.push((Tok::Ident(text[start..i].to_string()), line));
            }
            // `b[i] as char` is only the right char for ASCII: take a non-ASCII
            // code point from the text so `i` stays on a UTF-8 boundary.
            _ => {
                let c = text[i..].chars().next().expect("i is on a char boundary");
                out.push((Tok::Punct(c), line));
                i += c.len_utf8();
            }
        }
    }
    Ok(out)
}

struct Parser {
    toks: Vec<(Tok, u32)>,
    pos: usize,
    /// Types open around the one being parsed (see [`MAX_TYPE_DEPTH`]).
    type_depth: usize,
}

/// Deepest nesting of generic arguments and array suffixes a type may have.
/// Real interfaces nest a few levels; the bound keeps a hostile file from
/// exhausting the stack, here and wherever the type is walked afterwards.
const MAX_TYPE_DEPTH: usize = 64;

/// One bracketed attribute: `ipccode 1`, `oneway`, `in`.
struct Attr {
    name: String,
    value: Option<u64>,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos).map(|t| &t.0)
    }
    fn line(&self) -> u32 {
        self.toks
            .get(self.pos)
            .or(self.toks.last())
            .map_or(1, |t| t.1)
    }
    fn error(&self, message: impl Into<String>) -> ParseError {
        ParseError {
            line: self.line(),
            message: message.into(),
        }
    }
    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(&Tok::Punct(c)) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
    fn expect(&mut self, c: char) -> Result<(), ParseError> {
        if self.eat(c) {
            Ok(())
        } else {
            Err(self.error(format!("expected `{c}`")))
        }
    }
    fn ident(&mut self) -> Result<String, ParseError> {
        match self.peek().cloned() {
            Some(Tok::Ident(s)) => {
                self.pos += 1;
                Ok(s)
            }
            _ => Err(self.error("expected an identifier")),
        }
    }
    fn peek_ident(&self) -> Option<&str> {
        match self.peek() {
            Some(Tok::Ident(s)) => Some(s.as_str()),
            _ => None,
        }
    }
    /// `a.b.C`
    fn dotted(&mut self) -> Result<Vec<String>, ParseError> {
        let mut segs = vec![self.ident()?];
        while self.eat('.') {
            segs.push(self.ident()?);
        }
        Ok(segs)
    }
    /// `[a, b 1]*`; zero or more bracket groups.
    fn attributes(&mut self) -> Result<Vec<Attr>, ParseError> {
        let mut attrs = Vec::new();
        while self.eat('[') {
            loop {
                let name = self.ident()?;
                let value = match self.peek().cloned() {
                    Some(Tok::Int(v)) => {
                        self.pos += 1;
                        Some(v)
                    }
                    _ => None,
                };
                // Any other value is skipped: `macrodef NAME`,
                // `flags=MessageOption::TF_IMAGE`, over brackets of its own.
                let mut depth = 0usize;
                loop {
                    match self.peek() {
                        None => return Err(self.error("unexpected end of file")),
                        Some(Tok::Punct(',' | ']')) if depth == 0 => break,
                        Some(Tok::Punct('[')) => depth += 1,
                        Some(Tok::Punct(']')) => depth -= 1,
                        Some(_) => {}
                    }
                    self.pos += 1;
                }
                attrs.push(Attr { name, value });
                if self.eat(']') {
                    break;
                }
                self.expect(',')?;
            }
        }
        Ok(attrs)
    }
    /// Skips to the `;` ending this declaration, over balanced braces.
    fn skip_declaration(&mut self) -> Result<(), ParseError> {
        let mut depth = 0i32;
        while let Some(t) = self.peek().cloned() {
            self.pos += 1;
            match t {
                Tok::Punct('{') => depth += 1,
                Tok::Punct('}') => depth -= 1,
                Tok::Punct(';') if depth == 0 => return Ok(()),
                _ => {}
            }
        }
        Err(self.error("unexpected end of file"))
    }
    /// The name a declaration gives, and whether it is spelled from the
    /// global namespace: `a.b.C`, or `header..a.b.C`, where the part after
    /// `..` is the name and the part before it the header declaring it. A
    /// `.` right after the separator (`header...C`) is the global qualifier.
    fn declared_name(&mut self) -> Result<(Vec<String>, bool), ParseError> {
        let mut segs = vec![self.ident()?];
        let mut global = false;
        while self.eat('.') {
            if self.eat('.') {
                segs.clear();
                global = self.eat('.');
            }
            segs.push(self.ident()?);
        }
        Ok((segs, global))
    }
    fn ty(&mut self) -> Result<Type, ParseError> {
        self.type_depth += 1;
        let ty = self.ty_at_depth();
        self.type_depth -= 1;
        ty
    }
    fn ty_at_depth(&mut self) -> Result<Type, ParseError> {
        if self.type_depth > MAX_TYPE_DEPTH {
            return Err(self.error("type nested too deeply"));
        }
        let mut segs = self.dotted()?;
        if segs == ["unsigned"] {
            segs = vec![format!("unsigned {}", self.ident()?)];
        }
        let mut ty = if self.eat('<') {
            let mut args = vec![self.ty()?];
            while self.eat(',') {
                args.push(self.ty()?);
            }
            self.expect('>')?;
            Type::Generic(segs.join("."), args)
        } else {
            Type::Named(segs)
        };
        let mut depth = self.type_depth;
        while self.eat('[') {
            self.expect(']')?;
            depth += 1;
            if depth > MAX_TYPE_DEPTH {
                return Err(self.error("type nested too deeply"));
            }
            ty = Type::Array(Box::new(ty));
        }
        Ok(ty)
    }
    fn method(&mut self) -> Result<Method, ParseError> {
        let attrs = self.attributes()?;
        // A code that does not fit is not recorded; the method is still declared.
        let ipccode = attrs
            .iter()
            .find(|a| a.name == "ipccode")
            .and_then(|a| a.value)
            .and_then(|v| u32::try_from(v).ok());
        let ret = self.ty()?;
        let name = self.ident()?;
        self.expect('(')?;
        let mut params = Vec::new();
        if !self.eat(')') {
            loop {
                let attrs = self.attributes()?;
                let dir = attrs
                    .iter()
                    .find_map(|a| match a.name.as_str() {
                        "in" => Some(Direction::In),
                        "out" => Some(Direction::Out),
                        "inout" => Some(Direction::InOut),
                        _ => None,
                    })
                    .unwrap_or(Direction::In);
                let ty = self.ty()?;
                let name = self.ident()?;
                params.push(Param { dir, ty, name });
                if self.eat(')') {
                    break;
                }
                self.expect(',')?;
            }
        }
        self.expect(';')?;
        Ok(Method {
            name,
            ipccode,
            ret,
            params,
        })
    }
}

/// Parses one `.idl` file (docs/ANALYSIS.md, "IDL-generated interfaces").
fn parse(text: &str) -> Result<File, ParseError> {
    let mut p = Parser {
        toks: lex(text)?,
        pos: 0,
        type_depth: 0,
    };
    let mut file = File::default();
    let mut package: Vec<String> = Vec::new();
    while p.peek().is_some() {
        // Interface-level attributes (`[oneway]`) do not change the declarations.
        p.attributes()?;
        match p.peek_ident() {
            Some("package") => {
                p.pos += 1;
                package = p.dotted()?;
                p.expect(';')?;
            }
            Some("import") => p.skip_declaration()?,
            // `rawdata` names a parcelable type the same way.
            Some("sequenceable" | "rawdata") => {
                p.pos += 1;
                file.sequenceables.push(p.declared_name()?.0);
                p.expect(';')?;
            }
            Some("enum") => {
                p.pos += 1;
                file.enums.push(p.ident()?);
                p.skip_declaration()?;
            }
            Some("struct" | "union") => p.skip_declaration()?,
            // `option_stub_hooks on;`, `option_parcel_hooks on;`
            Some(option) if option.starts_with("option_") => p.skip_declaration()?,
            Some("interface_token") => {
                p.pos += 1;
                file.interface_token = Some(p.dotted()?.join("."));
                p.expect(';')?;
            }
            Some("interface") => {
                p.pos += 1;
                let in_package = |name: Vec<String>| -> Vec<String> {
                    if name.len() == 1 {
                        package.iter().cloned().chain(name).collect()
                    } else {
                        name
                    }
                };
                let (name, global) = p.declared_name()?;
                let qualified = if global { name } else { in_package(name) };
                if p.eat(';') {
                    file.interface_refs.push(qualified);
                    continue;
                }
                let extends = if p.peek_ident() == Some("extends") {
                    p.pos += 1;
                    Some(in_package(p.dotted()?))
                } else {
                    None
                };
                p.expect('{')?;
                let mut methods = Vec::new();
                while !p.eat('}') {
                    if p.peek().is_none() {
                        return Err(p.error("unexpected end of file"));
                    }
                    methods.push(p.method()?);
                }
                p.eat(';');
                file.interfaces.push(Interface {
                    qualified,
                    extends,
                    methods,
                });
            }
            _ => return Err(p.error("expected a declaration")),
        }
    }
    Ok(file)
}

/// The names idl-tool gives an interface's generated classes and headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Names {
    /// `["OHOS", "Security"]`
    pub namespace: Vec<String>,
    /// `IAtm`
    pub interface: String,
    /// `AtmProxy`
    pub proxy: String,
    /// `AtmStub`
    pub stub: String,
    /// `iatm.h`
    pub interface_header: String,
    /// `atm_proxy.h`
    pub proxy_header: String,
    /// `atm_stub.h`
    pub stub_header: String,
    /// `OHOS.Security.IAtm`
    pub descriptor: String,
}

/// The file a generated class is written to: lower case, with an underscore
/// before every capital past the second character (the generators'
/// `CodeEmitter::FileName`), so `IAtm` is `iatm` and `IHDIFoo` is
/// `ih_d_i_foo`.
fn file_name(class: &str) -> String {
    let mut out = String::with_capacity(class.len() + 4);
    for (i, ch) in class.chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if i > 1 {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

impl Names {
    /// The idl-tool naming convention for the interface `qualified` names;
    /// the one place it is computed.
    fn of(qualified: &[String]) -> Names {
        let (interface, namespace) = qualified
            .split_last()
            .expect("parser yields a non-empty name");
        let stripped = interface
            .strip_prefix('I')
            .filter(|r| r.starts_with(|c: char| c.is_ascii_uppercase()));
        let base = stripped.unwrap_or(interface);
        let (proxy, stub) = (format!("{base}Proxy"), format!("{base}Stub"));
        Names {
            namespace: namespace.to_vec(),
            interface: interface.clone(),
            interface_header: format!("{}.h", file_name(interface)),
            proxy_header: format!("{}.h", file_name(&proxy)),
            stub_header: format!("{}.h", file_name(&stub)),
            proxy,
            stub,
            descriptor: qualified.join("."),
        }
    }
    /// `class` in the interface's namespace: `OHOS::Security::AtmProxy`.
    fn qualify(&self, class: &str) -> String {
        self.namespace
            .iter()
            .map(String::as_str)
            .chain([class])
            .collect::<Vec<_>>()
            .join("::")
    }
}

const PRIMITIVES: &[(&str, &str)] = &[
    ("void", "void"),
    ("boolean", "bool"),
    ("byte", "int8_t"),
    ("short", "int16_t"),
    ("int", "int32_t"),
    ("long", "int64_t"),
    ("float", "float"),
    ("double", "double"),
    ("char", "char"),
    ("unsigned char", "uint8_t"),
    ("unsigned short", "uint16_t"),
    ("unsigned int", "uint32_t"),
    ("unsigned long", "uint64_t"),
    ("FileDescriptor", "int"),
    // C++ spellings some OpenHarmony IDL files use directly; scalars all the same.
    ("bool", "bool"),
    ("int8_t", "int8_t"),
    ("int16_t", "int16_t"),
    ("int32_t", "int32_t"),
    ("int64_t", "int64_t"),
    ("uint8_t", "uint8_t"),
    ("uint16_t", "uint16_t"),
    ("uint32_t", "uint32_t"),
    ("uint64_t", "uint64_t"),
];

fn primitive(name: &str) -> Option<&'static str> {
    PRIMITIVES
        .iter()
        .find(|(idl, _)| *idl == name)
        .map(|(_, cpp)| *cpp)
}

/// Passed by value when `[in]`: primitives and IDL enums.
fn is_scalar(ty: &Type, file: &File) -> bool {
    matches!(ty, Type::Named(s) if s.len() == 1
        && (primitive(&s[0]).is_some() || file.enums.contains(&s[0])))
}

/// The IDL → C++ type mapping; the one place it is computed.
fn cpp_type(ty: &Type, file: &File) -> String {
    match ty {
        Type::Array(inner) => format!("std::vector<{}>", cpp_type(inner, file)),
        Type::Generic(name, args) => {
            let args: Vec<String> = args.iter().map(|a| cpp_type(a, file)).collect();
            match name.as_str() {
                "List" => format!("std::vector<{}>", args.join(", ")),
                "Map" => format!("std::unordered_map<{}>", args.join(", ")),
                other => format!("{}<{}>", other.replace('.', "::"), args.join(", ")),
            }
        }
        Type::Named(segs) => {
            if let [one] = segs.as_slice() {
                if let Some(cpp) = primitive(one) {
                    return cpp.into();
                }
                match one.as_str() {
                    "String" => return "std::string".into(),
                    "IRemoteObject" => return "sptr<IRemoteObject>".into(),
                    _ => {}
                }
            }
            let declared_interface = file
                .interfaces
                .iter()
                .map(|i| &i.qualified)
                .chain(&file.interface_refs)
                .find(|q| q.ends_with(segs));
            if let Some(q) = declared_interface {
                return format!("sptr<{}>", q.join("::"));
            }
            if let Some(q) = file.sequenceables.iter().find(|q| q.ends_with(segs)) {
                return q.join("::");
            }
            segs.join("::")
        }
    }
}

fn param(p: &Param, file: &File) -> String {
    let t = cpp_type(&p.ty, file);
    match p.dir {
        Direction::In if is_scalar(&p.ty, file) => format!("{t} {}", p.name),
        Direction::In => format!("const {t}& {}", p.name),
        Direction::Out | Direction::InOut => format!("{t}& {}", p.name),
    }
}

fn signature(m: &Method, file: &File) -> String {
    let mut params: Vec<String> = m.params.iter().map(|p| param(p, file)).collect();
    if !matches!(&m.ret, Type::Named(segs) if *segs == ["void"]) {
        params.push(format!("{}& funcResult", cpp_type(&m.ret, file)));
    }
    format!("int32_t {}({})", m.name, params.join(", "))
}

/// `dir` is the directory below the generated root the header is served
/// from. A header of the same name is served from the root itself and from
/// each such directory, and a unit may include more than one of them, so the
/// guard names the directory with the file.
fn header(
    source_name: &str,
    dir: Option<&OsStr>,
    file_name: &str,
    include: Option<&str>,
    ns: &[String],
    body: &str,
) -> String {
    let spelled: String = dir
        .map(|dir| format!("{}_", dir.to_string_lossy()))
        .into_iter()
        .chain([file_name.to_string()])
        .collect();
    let guard: String = "TRACE_IDL_"
        .chars()
        .chain(spelled.chars().map(|c| match c {
            c if c.is_ascii_alphanumeric() => c.to_ascii_uppercase(),
            _ => '_',
        }))
        .collect();
    let mut s = format!(
        "// Synthesized by trace from {source_name} (docs/ANALYSIS.md, \"IDL-generated interfaces\").\n#ifndef {guard}\n#define {guard}\n"
    );
    if let Some(inc) = include {
        s += &format!("#include \"{inc}\"\n");
    }
    for n in ns {
        s += &format!("namespace {n} {{\n");
    }
    s += body;
    for n in ns.iter().rev() {
        s += &format!("}} // namespace {n}\n");
    }
    s += &format!("#endif // {guard}\n");
    s
}

/// The base of an interface that extends none.
const BROKER: &str = "OHOS::IRemoteBroker";

/// What `extends` brings to an interface, resolved over the tree's IDL files.
#[derive(Debug, Default)]
pub(crate) struct Base<'a> {
    /// The base's header, when it can be included: it is synthesized too, or
    /// on disk.
    pub header: Option<String>,
    /// The methods the interface inherits and does not declare again,
    /// nearest base first, each with the file declaring it (whose
    /// declarations spell its types).
    pub methods: Vec<(&'a Method, &'a File)>,
}

/// (file name, text) for the three headers, in the order interface, proxy,
/// stub.
fn render(
    i: &Interface,
    file: &File,
    source_name: &str,
    dir: Option<&OsStr>,
    base: &Base,
) -> [(String, String); 3] {
    let n = Names::of(&i.qualified);
    let own = |prefix: &str, suffix: &str| -> String {
        i.methods
            .iter()
            .map(|m| format!("    {prefix}{}{suffix};\n", signature(m, file)))
            .collect()
    };
    // The interface inherits its base's methods; the proxy is the concrete
    // class and implements them all.
    let inherited: String = base
        .methods
        .iter()
        .map(|(m, declared_in)| format!("    {} override;\n", signature(m, declared_in)))
        .collect();
    // Without `extends` the interface derives from the broker (`AsObject`),
    // as every generated interface does.
    let extends = i.extends.as_ref().map(|qualified| qualified.join("::"));
    let interface = format!(
        "class {} : public {} {{\npublic:\n{}}};\n",
        n.interface,
        extends.as_deref().unwrap_or(BROKER),
        own("virtual ", " = 0")
    );
    let proxy = format!(
        "class {} : public IRemoteProxy<{}> {{\npublic:\n{}{}}};\n",
        n.proxy,
        n.interface,
        own("", " override"),
        inherited
    );
    let stub = format!(
        "class {} : public IRemoteStub<{}> {{\npublic:\n    int32_t OnRemoteRequest(uint32_t code, MessageParcel& data, MessageParcel& reply, MessageOption& option) override;\n}};\n",
        n.stub, n.interface
    );
    let interface_header = n.interface_header.as_str();
    [
        (interface_header, base.header.as_deref(), interface),
        (n.proxy_header.as_str(), Some(interface_header), proxy),
        (n.stub_header.as_str(), Some(interface_header), stub),
    ]
    .map(|(name, include, body)| {
        (
            name.to_string(),
            header(source_name, dir, name, include, &n.namespace, &body),
        )
    })
}

/// Directory under the analysis root that the synthesized headers are served from.
const GENERATED_DIR: &str = ".trace-idl-generated";

/// Whether `path` is a header synthesized from an `.idl` file: one served
/// from [`GENERATED_DIR`], a name reserved for them.
pub(crate) fn is_generated(path: &Path) -> bool {
    path.components()
        .any(|c| matches!(c, Component::Normal(name) if name == GENERATED_DIR))
}

/// The headers synthesized for one tree, held in memory.
#[derive(Debug, Default)]
pub(crate) struct Synthesized {
    /// `<canonical root>/.trace-idl-generated`
    pub root: PathBuf,
    pub headers: VirtualHeaders,
    /// Production interfaces before those of the test partition, each in
    /// sorted IDL path order, then declaration order.
    pub interfaces: Vec<trace_ir::IdlInterface>,
    pub warnings: Vec<String>,
}

/// The test directory an IDL file is under, `None` for a production one.
type TestDir = Option<OsString>;

/// The first directory on the way to `path` that the test partition names,
/// below the first of `roots` that gives one: the analysis root, then the
/// dependency roots. An IDL file in a test directory of a dependency root is
/// test IDL as one of the analysis root is, and what it renders goes where
/// production code does not look.
fn test_directory(roots: &[&Path], path: &Path, partition: &TestPartition) -> TestDir {
    roots.iter().find_map(|root| {
        let parent = path.strip_prefix(root).ok()?.parent()?;
        parent.components().find_map(|part| match part {
            Component::Normal(name) if partition.matches(name) => Some(name.to_os_string()),
            _ => None,
        })
    })
}

/// [`synthesize`] over what discovery found under the analysis root and the
/// dependency roots. A tree without `.idl` files costs nothing here.
pub(crate) fn synthesize_discovered(
    root: &Path,
    opts: &PreprocessOptions,
    found: &[&DiscoveredFiles],
) -> Synthesized {
    let canonical = |paths: &[PathBuf]| -> Vec<PathBuf> {
        paths.iter().map(|p| trace_ir::canonicalize(p)).collect()
    };
    let idl_files: Vec<PathBuf> = found.iter().flat_map(|f| canonical(&f.idl)).collect();
    if idl_files.is_empty() {
        return Synthesized::default();
    }
    let headers: Vec<PathBuf> = found.iter().flat_map(|f| canonical(&f.headers)).collect();
    // A header in a directory the command line names is on disk too.
    let search_dirs: Vec<PathBuf> = opts
        .quote_include_paths
        .iter()
        .chain(&opts.include_paths)
        .chain(&opts.system_include_paths)
        .cloned()
        .collect();
    synthesize(
        root,
        &canonical(&opts.dep_roots),
        &idl_files,
        &headers,
        &search_dirs,
        &opts.test_partition,
    )
}

/// Parses every `.idl` in `idl_files` and renders its headers under
/// `<root>/.trace-idl-generated`; those of an IDL file in the test partition
/// go one directory down, named after its test directory, so they stay in
/// the partition. Production files come first, each group in sorted path
/// order (docs/ANALYSIS.md, "IDL-generated interfaces").
///
/// A header is not rendered when a header of its name is already there for
/// its includers — among `on_disk_headers` (for a production interface,
/// those outside the test partition) or in one of `search_dirs` — or was
/// rendered from an earlier IDL path. An interface whose own header was is
/// not declared: nothing of it is rendered, and nothing extends it.
/// An interface records its `IdlInterface` fact only when it lost none of its
/// headers to an earlier IDL, so a fact always describes the interface, proxy
/// and stub classes that are actually declared.
fn synthesize(
    root: &Path,
    dep_roots: &[PathBuf],
    idl_files: &[PathBuf],
    on_disk_headers: &[PathBuf],
    search_dirs: &[PathBuf],
    partition: &TestPartition,
) -> Synthesized {
    let gen_root = root.join(GENERATED_DIR);
    let mut out = Synthesized {
        root: gen_root.clone(),
        ..Default::default()
    };
    let mut on_disk: FxHashSet<&OsStr> = FxHashSet::default();
    let mut on_disk_production: FxHashSet<&OsStr> = FxHashSet::default();
    for path in on_disk_headers {
        let Some(name) = path.file_name() else {
            continue;
        };
        on_disk.insert(name);
        // As include resolution decides it, which finds a header for a
        // production includer by this test: relative to the analysis root.
        if !trace_ir::is_test_path(root, path, partition) {
            on_disk_production.insert(name);
        }
    }
    // What an includer beside the IDL in `dir` finds without synthesis.
    let exists = |dir: &TestDir, name: &str| {
        let seen = if dir.is_some() {
            &on_disk
        } else {
            &on_disk_production
        };
        seen.contains(OsStr::new(name))
            || search_dirs
                .iter()
                .any(|d| trace_ir::is_file_cached(&d.join(name)))
    };

    let roots: Vec<&Path> = std::iter::once(root)
        .chain(dep_roots.iter().map(PathBuf::as_path))
        .collect();
    let mut idl_files: Vec<(TestDir, &PathBuf)> = idl_files
        .iter()
        .map(|path| (test_directory(&roots, path, partition), path))
        .collect();
    idl_files.sort();
    idl_files.dedup();
    let mut parsed: Vec<(TestDir, &PathBuf, File)> = Vec::new();
    for (dir, path) in idl_files {
        let Ok(text) = std::fs::read_to_string(path) else {
            out.warnings
                .push(format!("{}: unreadable IDL file", path.display()));
            continue;
        };
        match parse(&text) {
            Ok(mut file) => {
                for iface in &mut file.interfaces {
                    if iface.extends.as_ref() == Some(&iface.qualified) {
                        out.warnings.push(format!(
                            "{}: interface {} extends itself",
                            path.display(),
                            iface.qualified.join(".")
                        ));
                        iface.extends = None;
                    }
                }
                parsed.push((dir, path, file));
            }
            Err(e) => out
                .warnings
                .push(format!("{}:{}: {}", path.display(), e.line, e.message)),
        }
    }
    // The interface each rendered interface header declares: the first in
    // processing order of those named for it in its directory.
    let mut header_of: FxHashMap<(&TestDir, String), &[String]> = FxHashMap::default();
    for (dir, _, file) in &parsed {
        for iface in &file.interfaces {
            header_of
                .entry((dir, Names::of(&iface.qualified).interface_header))
                .or_insert(iface.qualified.as_slice());
        }
    }
    // Whether the interface `qualified`, of IDL in `dir`, is declared: its
    // header is on disk, or is the one rendered under that name.
    let is_declared = |dir: &TestDir, qualified: &[String]| {
        let header = Names::of(qualified).interface_header;
        exists(dir, &header) || header_of.get(&(dir, header)).copied() == Some(qualified)
    };

    // Interfaces by directory and name, the first in processing order under
    // each. An interface is visible beside its own directory's IDL, and a
    // production one to test code too.
    let mut declared: FxHashMap<(&TestDir, &[String]), (&Interface, &File)> = FxHashMap::default();
    for (dir, _, file) in &parsed {
        for iface in &file.interfaces {
            if is_declared(dir, &iface.qualified) {
                declared
                    .entry((dir, iface.qualified.as_slice()))
                    .or_insert((iface, file));
            }
        }
    }
    let visible = |dir: &TestDir, name: &[String]| {
        declared
            .get(&(dir, name))
            .or_else(|| declared.get(&(&None, name)))
            .copied()
    };
    // What `iface`, declared in `dir`, inherits and does not declare again,
    // nearest base first: a method of a nearer base stands for those of its
    // name further up.
    let inherited = |dir: &TestDir, iface: &Interface| -> Vec<(&Method, &File)> {
        let mut methods: Vec<(&Method, &File)> = Vec::new();
        let mut seen: FxHashSet<&[String]> = FxHashSet::default();
        seen.insert(iface.qualified.as_slice());
        let mut next = iface.extends.as_deref();
        while let Some((base, file)) = next
            .filter(|name| seen.insert(name))
            .and_then(|name| visible(dir, name))
        {
            let new: Vec<(&Method, &File)> = base
                .methods
                .iter()
                .filter(|m| {
                    iface.methods.iter().all(|own| own.name != m.name)
                        && methods.iter().all(|(nearer, _)| nearer.name != m.name)
                })
                .map(|m| (m, file))
                .collect();
            methods.extend(new);
            next = base.extends.as_deref();
        }
        methods
    };

    let mut owner: FxHashMap<PathBuf, &Path> = FxHashMap::default();
    let mut claimed: FxHashSet<String> = FxHashSet::default();
    for (dir, path, file) in &parsed {
        let target = match dir {
            Some(dir) => gen_root.join(dir),
            None => gen_root.clone(),
        };
        let source_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        for iface in &file.interfaces {
            let n = Names::of(&iface.qualified);
            if !is_declared(dir, &iface.qualified) {
                let first = owner.get(&target.join(&n.interface_header));
                out.warnings.push(format!(
                    "{}: rendered from both {} and {}; keeping the first",
                    n.interface_header,
                    first.map_or_else(String::new, |first| first.display().to_string()),
                    path.display()
                ));
                continue;
            }
            let base = Base {
                header: iface
                    .extends
                    .as_ref()
                    .filter(|qualified| {
                        visible(dir, qualified).is_some()
                            || exists(dir, &Names::of(qualified).interface_header)
                    })
                    .map(|qualified| Names::of(qualified).interface_header),
                methods: inherited(dir, iface),
            };
            let mut lost_header = false;
            for (name, text) in render(iface, file, &source_name, dir.as_deref(), &base) {
                if exists(dir, &name) {
                    continue;
                }
                let generated = target.join(&name);
                if let Some(first) = owner.get(&generated) {
                    out.warnings.push(format!(
                        "{name}: rendered from both {} and {}; keeping the first",
                        first.display(),
                        path.display()
                    ));
                    lost_header = true;
                    continue;
                }
                owner.insert(generated.clone(), path);
                out.headers.insert(generated, Arc::from(text));
            }
            // The surviving proxy/stub classes belong to the earlier IDL; a fact
            // here would give them this interface's descriptor and methods.
            // `claimed` also covers pairs whose headers are both on disk, and
            // a test interface naming a production one's classes.
            let (proxy, stub) = (n.qualify(&n.proxy), n.qualify(&n.stub));
            if lost_header || claimed.contains(&proxy) || claimed.contains(&stub) {
                continue;
            }
            claimed.insert(proxy.clone());
            claimed.insert(stub.clone());
            out.interfaces.push(trace_ir::IdlInterface {
                interface: n.qualify(&n.interface),
                descriptor: file.interface_token.clone().unwrap_or(n.descriptor),
                proxy,
                stub,
                // As the proxy declares them: its own, then those inherited.
                methods: iface
                    .methods
                    .iter()
                    .chain(base.methods.iter().map(|(m, _)| *m))
                    .map(|m| trace_ir::IdlMethod {
                        name: m.name.clone(),
                        ipccode: m.ipccode,
                    })
                    .collect(),
            });
        }
    }
    // The directory becomes a dependency root, whatever is in it.
    if !out.headers.is_empty() && gen_root.exists() {
        out.warnings.push(format!(
            "{}: exists on disk; its files are indexed as dependency headers",
            gen_root.display()
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(s: &[&str]) -> Type {
        Type::Named(s.iter().map(|x| x.to_string()).collect())
    }

    #[test]
    fn parses_issue_interface() {
        let f = parse(
            "// comment\n/* block */\n\
             interface OHOS.Security.IAtm {\n\
               [ipccode 1] void VerifyAccessToken([in] unsigned int tokenID, [in] String permissionName, [out] int state);\n\
             }\n",
        )
        .unwrap();
        assert_eq!(f.interfaces.len(), 1);
        let i = &f.interfaces[0];
        assert_eq!(i.qualified, vec!["OHOS", "Security", "IAtm"]);
        let m = &i.methods[0];
        assert_eq!(m.name, "VerifyAccessToken");
        assert_eq!(m.ipccode, Some(1));
        assert_eq!(m.ret, named(&["void"]));
        assert_eq!(
            m.params,
            vec![
                Param {
                    dir: Direction::In,
                    ty: named(&["unsigned int"]),
                    name: "tokenID".into()
                },
                Param {
                    dir: Direction::In,
                    ty: named(&["String"]),
                    name: "permissionName".into()
                },
                Param {
                    dir: Direction::Out,
                    ty: named(&["int"]),
                    name: "state".into()
                },
            ]
        );
    }

    #[test]
    fn package_qualifies_a_bare_interface() {
        let f = parse("package OHOS.AAFwk;\ninterface IFoo { void Ping(); }").unwrap();
        assert_eq!(f.interfaces[0].qualified, vec!["OHOS", "AAFwk", "IFoo"]);
        assert_eq!(f.interfaces[0].methods[0].ipccode, None);
    }

    #[test]
    fn generics_arrays_and_hex_ipccode() {
        let f = parse(
            "interface a.IFoo { [ipccode 0x10, oneway] void Put([in] List<Map<String, int>> xs, [inout] long[] ys); }",
        )
        .unwrap();
        let m = &f.interfaces[0].methods[0];
        assert_eq!(m.ipccode, Some(16));
        assert_eq!(
            m.params[0].ty,
            Type::Generic(
                "List".into(),
                vec![Type::Generic(
                    "Map".into(),
                    vec![named(&["String"]), named(&["int"])]
                ),]
            )
        );
        assert_eq!(m.params[1].dir, Direction::InOut);
        assert_eq!(m.params[1].ty, Type::Array(Box::new(named(&["long"]))));
    }

    #[test]
    fn tolerates_declarations_it_does_not_model() {
        let f = parse(
            "import IdlCommon;\n\
             sequenceable OHOS.Security.AccessToken.HapInfoParcel;\n\
             sequenceable want..OHOS.AAFwk.Want;\n\
             interface OHOS.Security.ICallback;\n\
             enum Mode { A = 1, B, };\n\
             struct S { int x; };\n\
             [oneway, cacheable 100] interface OHOS.Security.IAtm { [ipcincapacity 50] int Get([in] ICallback cb); }\n",
        )
        .unwrap();
        assert_eq!(
            f.sequenceables,
            vec![
                vec!["OHOS", "Security", "AccessToken", "HapInfoParcel"],
                vec!["OHOS", "AAFwk", "Want"],
            ]
        );
        assert_eq!(
            f.interface_refs,
            vec![vec!["OHOS", "Security", "ICallback"]]
        );
        assert_eq!(f.enums, vec!["Mode"]);
        assert_eq!(f.interfaces.len(), 1);
        assert_eq!(f.interfaces[0].methods[0].ret, named(&["int"]));
    }

    #[test]
    fn syntax_error_reports_its_line() {
        let err = parse("interface a.IFoo {\n  void Ping(\n}\n").unwrap_err();
        assert_eq!(err.line, 3);
        assert!(!err.message.is_empty());
    }

    #[test]
    fn only_enums_is_an_empty_interface_list() {
        assert!(parse("enum E { A };").unwrap().interfaces.is_empty());
    }

    #[test]
    fn non_ascii_outside_comments_keeps_following_tokens() {
        // `’` is three bytes, and the `;` after it starts at the next boundary.
        let toks: Vec<Tok> = lex("’;x").unwrap().into_iter().map(|(t, _)| t).collect();
        assert_eq!(
            toks,
            vec![Tok::Punct('’'), Tok::Punct(';'), Tok::Ident("x".into())]
        );
        let f = parse("interface a.IFoo { void A(); } // café ’\n").unwrap();
        assert_eq!(f.interfaces[0].methods[0].name, "A");
    }

    #[test]
    fn unterminated_string_is_an_error() {
        // The escaped quote does not end the first string; the last one never ends.
        let err = parse("interface a.IFoo {\n  [x \"y\\\" z\"] void A();\n}\n\"open").unwrap_err();
        assert_eq!(err.message, "unterminated string");
    }

    fn iface(src: &str) -> (File, Interface) {
        let f = parse(src).unwrap();
        let i = f.interfaces[0].clone();
        (f, i)
    }

    #[test]
    fn names_follow_idl_tool() {
        let (_, i) = iface("interface OHOS.Security.IAccessTokenManager { void A(); }");
        let n = Names::of(&i.qualified);
        assert_eq!(n.namespace, vec!["OHOS", "Security"]);
        assert_eq!(n.interface, "IAccessTokenManager");
        assert_eq!(n.proxy, "AccessTokenManagerProxy");
        assert_eq!(n.stub, "AccessTokenManagerStub");
        assert_eq!(n.interface_header, "iaccess_token_manager.h");
        assert_eq!(n.proxy_header, "access_token_manager_proxy.h");
        assert_eq!(n.stub_header, "access_token_manager_stub.h");
        assert_eq!(n.descriptor, "OHOS.Security.IAccessTokenManager");
        assert_eq!(
            n.qualify(&n.proxy),
            "OHOS::Security::AccessTokenManagerProxy"
        );
    }

    #[test]
    fn names_without_i_prefix_and_without_namespace() {
        let (_, i) = iface("interface El5FilekeyManagerInterface { void A(); }");
        let n = Names::of(&i.qualified);
        assert!(n.namespace.is_empty());
        assert_eq!(n.interface_header, "el5_filekey_manager_interface.h");
        assert_eq!(n.proxy, "El5FilekeyManagerInterfaceProxy");
        assert_eq!(n.qualify(&n.stub), "El5FilekeyManagerInterfaceStub");
        let (_, i) = iface("interface IAtm { void A(); }");
        assert_eq!(Names::of(&i.qualified).stub_header, "atm_stub.h");
        assert_eq!(Names::of(&i.qualified).interface_header, "iatm.h");
    }

    #[test]
    fn cpp_types() {
        let f = parse(
            "sequenceable a.b.S;\ninterface a.ICb;\nenum E { X };\ninterface a.IFoo { void A(); }",
        )
        .unwrap();
        let t = |s: &str| {
            let p = parse(&format!("interface Z {{ void M([in] {s} x); }}")).unwrap();
            cpp_type(&p.interfaces[0].methods[0].params[0].ty, &f)
        };
        assert_eq!(t("unsigned int"), "uint32_t");
        assert_eq!(t("long"), "int64_t");
        assert_eq!(t("boolean"), "bool");
        assert_eq!(t("String"), "std::string");
        assert_eq!(t("List<String>"), "std::vector<std::string>");
        assert_eq!(t("int[]"), "std::vector<int32_t>");
        assert_eq!(
            t("Map<String, long>"),
            "std::unordered_map<std::string, int64_t>"
        );
        assert_eq!(t("S"), "a::b::S");
        assert_eq!(t("ICb"), "sptr<a::ICb>");
        assert_eq!(t("IRemoteObject"), "sptr<IRemoteObject>");
        assert_eq!(t("E"), "E");
        assert_eq!(t("uint32_t"), "uint32_t");
    }

    #[test]
    fn fixed_width_in_param_is_by_value() {
        let f =
            parse("interface a.IFoo { void A([in] uint32_t id, [in] bool on, [out] int64_t n); }")
                .unwrap();
        let ps: Vec<String> = f.interfaces[0].methods[0]
            .params
            .iter()
            .map(|p| param(p, &f))
            .collect();
        assert_eq!(ps, vec!["uint32_t id", "bool on", "int64_t& n"]);
    }

    #[test]
    fn renders_the_three_headers() {
        let (f, i) = iface(
            "interface OHOS.Security.IAtm {\n\
               [ipccode 1] void VerifyAccessToken([in] unsigned int tokenID, [in] String name, [out] int state);\n\
               [ipccode 2] String GetName([in] List<int> ids);\n\
             }",
        );
        let [(ih, itext), (ph, ptext), (sh, stext)] =
            render(&i, &f, "IAtm.idl", None, &Base::default());
        assert_eq!(
            (ih.as_str(), ph.as_str(), sh.as_str()),
            ("iatm.h", "atm_proxy.h", "atm_stub.h")
        );
        assert_eq!(itext, "\
// Synthesized by trace from IAtm.idl (docs/ANALYSIS.md, \"IDL-generated interfaces\").
#ifndef TRACE_IDL_IATM_H
#define TRACE_IDL_IATM_H
namespace OHOS {
namespace Security {
class IAtm : public OHOS::IRemoteBroker {
public:
    virtual int32_t VerifyAccessToken(uint32_t tokenID, const std::string& name, int32_t& state) = 0;
    virtual int32_t GetName(const std::vector<int32_t>& ids, std::string& funcResult) = 0;
};
} // namespace Security
} // namespace OHOS
#endif // TRACE_IDL_IATM_H
");
        assert_eq!(
            ptext,
            "\
// Synthesized by trace from IAtm.idl (docs/ANALYSIS.md, \"IDL-generated interfaces\").
#ifndef TRACE_IDL_ATM_PROXY_H
#define TRACE_IDL_ATM_PROXY_H
#include \"iatm.h\"
namespace OHOS {
namespace Security {
class AtmProxy : public IRemoteProxy<IAtm> {
public:
    int32_t VerifyAccessToken(uint32_t tokenID, const std::string& name, int32_t& state) override;
    int32_t GetName(const std::vector<int32_t>& ids, std::string& funcResult) override;
};
} // namespace Security
} // namespace OHOS
#endif // TRACE_IDL_ATM_PROXY_H
"
        );
        assert_eq!(stext, "\
// Synthesized by trace from IAtm.idl (docs/ANALYSIS.md, \"IDL-generated interfaces\").
#ifndef TRACE_IDL_ATM_STUB_H
#define TRACE_IDL_ATM_STUB_H
#include \"iatm.h\"
namespace OHOS {
namespace Security {
class AtmStub : public IRemoteStub<IAtm> {
public:
    int32_t OnRemoteRequest(uint32_t code, MessageParcel& data, MessageParcel& reply, MessageOption& option) override;
};
} // namespace Security
} // namespace OHOS
#endif // TRACE_IDL_ATM_STUB_H
");
    }

    #[test]
    fn global_interface_renders_without_namespace() {
        let (f, i) = iface("interface IAtm { void Ping(); }");
        let [(_, itext), _, _] = render(&i, &f, "IAtm.idl", None, &Base::default());
        assert!(
            itext.contains("\nclass IAtm : public OHOS::IRemoteBroker {\npublic:\n    virtual int32_t Ping() = 0;\n};\n#endif")
        );
    }

    #[test]
    fn option_declarations_are_skipped() {
        let f = parse(
            "package a;\noption_stub_hooks on;\noption_parcel_hooks on;\ninterface IFoo { void A(); }",
        )
        .unwrap();
        assert_eq!(f.interfaces[0].qualified, vec!["a", "IFoo"]);
    }

    #[test]
    fn attribute_values_of_any_shape_are_skipped() {
        let f = parse(
            "interface a.IFoo { [customMsgOption flags=MessageOption::TF_IMAGE, ipccode 7] void A([in] int x); }",
        )
        .unwrap();
        let m = &f.interfaces[0].methods[0];
        assert_eq!(m.name, "A");
        assert_eq!(m.ipccode, Some(7));
        let err = parse("interface a.IFoo { [flags=").unwrap_err();
        assert_eq!(err.message, "unexpected end of file");
    }

    #[test]
    fn extends_names_the_base_interface() {
        let f = parse("package a;\ninterface IFoo extends IBase { void A(); }").unwrap();
        assert_eq!(
            f.interfaces[0].extends,
            Some(vec!["a".to_string(), "IBase".to_string()])
        );
        let f = parse("package a;\ninterface IFoo extends b.c.IBase { void A(); }").unwrap();
        assert_eq!(
            f.interfaces[0].extends,
            Some(vec!["b".to_string(), "c".to_string(), "IBase".to_string()])
        );
        let f = parse("interface a.IFoo { void A(); }").unwrap();
        assert_eq!(f.interfaces[0].extends, None);
    }

    #[test]
    fn extending_interface_derives_from_its_base() {
        let (f, i) = iface("package a;\ninterface IFoo extends IBase { void A(); }");
        let with_header = Base {
            header: Some("ibase.h".into()),
            ..Default::default()
        };
        let [(_, itext), (_, ptext), _] = render(&i, &f, "IFoo.idl", None, &with_header);
        assert_eq!(
            itext,
            "\
// Synthesized by trace from IFoo.idl (docs/ANALYSIS.md, \"IDL-generated interfaces\").
#ifndef TRACE_IDL_IFOO_H
#define TRACE_IDL_IFOO_H
#include \"ibase.h\"
namespace a {
class IFoo : public a::IBase {
public:
    virtual int32_t A() = 0;
};
} // namespace a
#endif // TRACE_IDL_IFOO_H
"
        );
        assert!(ptext.contains("class FooProxy : public IRemoteProxy<IFoo> {"));
    }

    #[test]
    fn rawdata_declares_a_type_like_sequenceable() {
        let f = parse(
            "rawdata OHOS.AAFwk.UriPermissionRawData;\n\
             rawdata ToolInfo..OHOS.CliTool.ToolsRawData;\n\
             interface a.IFoo { void A([in] UriPermissionRawData d, [out] ToolsRawData t); }",
        )
        .unwrap();
        assert_eq!(
            f.sequenceables,
            vec![
                vec!["OHOS", "AAFwk", "UriPermissionRawData"],
                vec!["OHOS", "CliTool", "ToolsRawData"],
            ]
        );
        let ps: Vec<String> = f.interfaces[0].methods[0]
            .params
            .iter()
            .map(|p| param(p, &f))
            .collect();
        assert_eq!(
            ps,
            vec![
                "const OHOS::AAFwk::UriPermissionRawData& d",
                "OHOS::CliTool::ToolsRawData& t"
            ]
        );
    }

    #[test]
    fn type_nested_past_the_limit_is_an_error() {
        let generic = format!(
            "interface a.IFoo {{ void A([in] {}int{} x); }}",
            "List<".repeat(1000),
            ">".repeat(1000)
        );
        assert_eq!(
            parse(&generic).unwrap_err().message,
            "type nested too deeply"
        );
        let array = format!(
            "interface a.IFoo {{ void A([in] int{} x); }}",
            "[]".repeat(1000)
        );
        assert_eq!(parse(&array).unwrap_err().message, "type nested too deeply");
        // Real interfaces nest a few levels.
        let ok = "interface a.IFoo { void A([in] Map<String, List<Map<String, int[]>>> x); }";
        assert!(parse(ok).is_ok());
    }

    #[test]
    fn file_names_follow_the_generators_rule() {
        // An underscore before every capital past the second character,
        // runs of capitals included.
        let (_, i) = iface("interface IHDIFoo { void A(); }");
        let n = Names::of(&i.qualified);
        assert_eq!(n.interface_header, "ih_d_i_foo.h");
        assert_eq!(n.proxy, "HDIFooProxy");
        assert_eq!(n.proxy_header, "hd_i_foo_proxy.h");
        assert_eq!(n.stub_header, "hd_i_foo_stub.h");
        let (_, i) = iface("interface IDCameraProvider { void A(); }");
        assert_eq!(
            Names::of(&i.qualified).interface_header,
            "id_camera_provider.h"
        );
    }

    #[test]
    fn attribute_value_may_hold_brackets() {
        let f = parse("interface a.IFoo { [a b[1], ipccode 3] void A([in] int[] x); }").unwrap();
        let m = &f.interfaces[0].methods[0];
        assert_eq!(m.ipccode, Some(3));
        assert_eq!(m.params[0].ty, Type::Array(Box::new(named(&["int"]))));
    }

    #[test]
    fn base_header_is_included_only_when_it_exists() {
        let (f, i) = iface("package a;\ninterface IFoo extends IBase { void A(); }");
        let [(_, itext), _, _] = render(&i, &f, "IFoo.idl", None, &Base::default());
        assert!(!itext.contains("#include"), "{itext}");
        assert!(itext.contains("class IFoo : public a::IBase {"), "{itext}");
    }

    #[test]
    fn proxy_declares_the_methods_its_interface_inherits() {
        let (bf, b) =
            iface("package a;\nsequenceable x.S;\ninterface IBase { void Ping([in] S s); }");
        let (f, i) = iface("package a;\ninterface IFoo extends IBase { void A(); }");
        let base = Base {
            header: Some("ibase.h".into()),
            methods: vec![(&b.methods[0], &bf)],
        };
        let [(_, itext), (_, ptext), _] = render(&i, &f, "IFoo.idl", None, &base);
        // The interface inherits them; only the proxy has to implement them.
        assert!(!itext.contains("Ping"), "{itext}");
        assert!(
            ptext.contains(
                "public:\n    int32_t A() override;\n    int32_t Ping(const x::S& s) override;\n};"
            ),
            "{ptext}"
        );
    }

    #[test]
    fn forward_interface_may_name_its_header() {
        let f = parse(
            "interface CallbackHeader..a.ICallback;\n\
             interface a.IFoo { void Set([in] ICallback cb); }",
        )
        .unwrap();
        assert_eq!(f.interface_refs, vec![vec!["a", "ICallback"]]);
        assert_eq!(f.interfaces.len(), 1);
        assert_eq!(
            param(&f.interfaces[0].methods[0].params[0], &f),
            "const sptr<a::ICallback>& cb"
        );
    }

    #[test]
    fn interface_token_is_the_files_descriptor() {
        let f = parse("interface_token other.Foo;\ninterface a.IFoo { void A(); }").unwrap();
        assert_eq!(f.interface_token.as_deref(), Some("other.Foo"));
        assert_eq!(f.interfaces.len(), 1);
        assert_eq!(
            parse("interface a.IFoo { void A(); }")
                .unwrap()
                .interface_token,
            None
        );
    }

    #[test]
    fn ipccode_past_u32_is_not_recorded() {
        let f = parse(
            "interface a.IFoo { [ipccode 4294967296] void A(); [ipccode 4294967295] void B(); }",
        )
        .unwrap();
        let codes: Vec<Option<u32>> = f.interfaces[0].methods.iter().map(|m| m.ipccode).collect();
        assert_eq!(codes, vec![None, Some(u32::MAX)]);
    }

    #[test]
    fn name_after_the_header_may_be_globally_qualified() {
        let f = parse(
            "package a;\n\
             interface IncludeDir...myinterface2;\n\
             sequenceable Hdr...GlobalParcel;\n\
             interface IFoo { void Set([in] myinterface2 cb, [in] GlobalParcel p); }",
        )
        .unwrap();
        // `.` after the separator: the global namespace, not the package.
        assert_eq!(f.interface_refs, vec![vec!["myinterface2"]]);
        assert_eq!(f.sequenceables, vec![vec!["GlobalParcel"]]);
        assert_eq!(f.interfaces[0].qualified, vec!["a", "IFoo"]);
        let ps: Vec<String> = f.interfaces[0].methods[0]
            .params
            .iter()
            .map(|p| param(p, &f))
            .collect();
        assert_eq!(
            ps,
            vec!["const sptr<myinterface2>& cb", "const GlobalParcel& p"]
        );
    }

    fn write(dir: &Path, rel: &str, text: &str) {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// What indexing synthesizes for the tree at `root` with `deps` as its
    /// dependency roots.
    fn synthesize_tree(root: &Path, deps: &[&Path]) -> Synthesized {
        let root = trace_ir::canonicalize(root);
        let opts = deps.iter().fold(PreprocessOptions::new(), |opts, dep| {
            opts.with_dep(trace_ir::canonicalize(dep))
        });
        let discovered: Vec<DiscoveredFiles> = std::iter::once(&root)
            .chain(&opts.dep_roots)
            .map(|dir| crate::discover::discover_files(dir))
            .collect();
        let found: Vec<&DiscoveredFiles> = discovered.iter().collect();
        synthesize_discovered(&root, &opts, &found)
    }

    fn generated<'a>(synthesized: &'a Synthesized, rel: &str) -> Option<&'a str> {
        synthesized
            .headers
            .get(&synthesized.root.join(rel))
            .map(|text| &**text)
    }

    fn method_names(interface: &trace_ir::IdlInterface) -> Vec<&str> {
        interface.methods.iter().map(|m| m.name.as_str()).collect()
    }

    /// `IC` extends `IB`, which extends `IA` and declares `M` again: the
    /// nearest declaration is the one `IC` inherits, once.
    #[test]
    fn method_redeclared_along_a_chain_is_inherited_once() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "idl/IA.idl",
            "interface p.IA { void M(); void OnlyA(); }\n",
        );
        write(
            dir.path(),
            "idl/IB.idl",
            "interface p.IB extends p.IA { void M(); void M([in] int overload); }\n",
        );
        write(
            dir.path(),
            "idl/IC.idl",
            "interface p.IC extends p.IB { void Own(); }\n",
        );
        let synthesized = synthesize_tree(dir.path(), &[]);
        let ic = synthesized
            .interfaces
            .iter()
            .find(|i| i.interface == "p::IC")
            .expect("IC is an interface");
        // `IB`'s two overloads of `M`, not `IA`'s third declaration of it.
        assert_eq!(method_names(ic), vec!["Own", "M", "M", "OnlyA"]);
        let proxy = generated(&synthesized, "cproxy.h").expect("IC has a proxy");
        assert_eq!(proxy.matches("int32_t M()").count(), 1, "{proxy}");
    }

    /// `IFoo` and `Ifoo` are both declared in `ifoo.h`, and their proxies and
    /// stubs are not in one file. The second has no interface header, so
    /// nothing that would include it is rendered and no fact names it.
    #[test]
    fn interface_that_lost_its_header_is_not_rendered() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a/IFoo.idl", "interface p.IFoo { void A(); }\n");
        write(dir.path(), "b/Ifoo.idl", "interface p.Ifoo { void B(); }\n");
        let synthesized = synthesize_tree(dir.path(), &[]);
        let interfaces: Vec<&str> = synthesized
            .interfaces
            .iter()
            .map(|i| i.interface.as_str())
            .collect();
        assert_eq!(interfaces, vec!["p::IFoo"]);
        let names: Vec<String> = synthesized
            .headers
            .keys()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["foo_proxy.h", "foo_stub.h", "ifoo.h"]);
        assert_eq!(synthesized.warnings.len(), 1, "{:?}", synthesized.warnings);
    }

    fn guard(header: &str) -> &str {
        header
            .lines()
            .find_map(|line| line.strip_prefix("#ifndef "))
            .expect("a synthesized header has a guard")
    }

    /// A production interface and one of the test partition are rendered
    /// under one file name, a directory apart. A unit may include both, so
    /// each has a guard of its own.
    #[test]
    fn headers_of_one_name_in_two_directories_have_their_own_guards() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "src/IFoo.idl",
            "interface p.IFoo { void A(); }\n",
        );
        write(
            dir.path(),
            "test/IFoo.idl",
            "interface q.IFoo { void B(); }\n",
        );
        let synthesized = synthesize_tree(dir.path(), &[]);
        for name in ["ifoo.h", "foo_proxy.h", "foo_stub.h"] {
            let production = generated(&synthesized, name).expect("production header");
            let test = generated(&synthesized, &format!("test/{name}")).expect("test header");
            assert_ne!(guard(production), guard(test), "{name}");
        }
    }

    /// `q.IBase` loses `ibase.h` to `p.IBase`. The header there declares
    /// another class, so `q.IFoo` does not include it for its base, and
    /// inherits nothing from an interface that is not declared.
    #[test]
    fn base_that_lost_its_header_is_not_included() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "a/IBase.idl",
            "interface p.IBase { void P(); }\n",
        );
        write(
            dir.path(),
            "b/IBase.idl",
            "interface q.IBase { void Q(); }\n",
        );
        write(
            dir.path(),
            "c/IFoo.idl",
            "interface q.IFoo extends q.IBase { void A(); }\n",
        );
        let synthesized = synthesize_tree(dir.path(), &[]);
        let foo = generated(&synthesized, "ifoo.h").expect("IFoo has a header");
        assert!(!foo.contains("#include \"ibase.h\""), "{foo}");
        let fact = synthesized
            .interfaces
            .iter()
            .find(|i| i.interface == "q::IFoo")
            .expect("IFoo is an interface");
        assert_eq!(method_names(fact), vec!["A"]);
    }

    /// A header in a test directory of a dependency root is one include
    /// resolution finds for production code, so it stands in for a
    /// generated header of its name as any on-disk header does; the
    /// partition of the analysis root does not reach into a dependency root
    /// (docs/ANALYSIS.md, "IDL-generated interfaces").
    #[test]
    fn mock_header_of_a_dependency_root_stands_in_for_a_generated_one() {
        let dir = tempfile::tempdir().unwrap();
        let (root, dep) = (dir.path().join("target"), dir.path().join("dep"));
        write(&root, "idl/IAtm.idl", "interface p.IAtm { void Real(); }\n");
        write(&dep, "mock/iatm.h", "namespace p { class IAtm {}; }\n");
        let synthesized = synthesize_tree(&root, &[&dep]);
        assert!(generated(&synthesized, "iatm.h").is_none());
        assert!(generated(&synthesized, "atm_proxy.h").is_some());
        assert_eq!(synthesized.interfaces.len(), 1);
    }

    /// A dependency root has test directories of its own. IDL in one is
    /// rendered below the generated directory of that name, as IDL in a test
    /// directory of the analysis root is, and after production IDL.
    #[test]
    fn idl_in_a_test_directory_of_a_dependency_root_stays_in_the_partition() {
        let dir = tempfile::tempdir().unwrap();
        let (root, dep) = (dir.path().join("target"), dir.path().join("dep"));
        write(&root, "main.cpp", "int main() { return 0; }\n");
        write(
            &dep,
            "mock/IAtm.idl",
            "interface p.IAtm { void Mocked(); }\n",
        );
        write(&dep, "src/IAtm.idl", "interface p.IAtm { void Real(); }\n");
        let synthesized = synthesize_tree(&root, &[&dep]);
        let production = generated(&synthesized, "iatm.h").expect("production header");
        assert!(production.contains("Real"), "{production}");
        let mock = generated(&synthesized, "mock/iatm.h").expect("header of the partition");
        assert!(mock.contains("Mocked"), "{mock}");
        assert!(
            synthesized.warnings.is_empty(),
            "{:?}",
            synthesized.warnings
        );
        assert_eq!(synthesized.interfaces.len(), 1);
        assert_eq!(method_names(&synthesized.interfaces[0]), vec!["Real"]);
    }
}
