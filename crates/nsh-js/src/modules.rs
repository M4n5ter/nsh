//! The native ES modules apps import: nsh's own (`nsh`,
//! `nsh:rt`, `nsh/device`, `nsh/accessibility`, `nsh/keymap`, ...;
//! [`nsh_core::js::NATIVE_MODULES`])
//! and those of registries (`Registry::specifier`): the UI sets a host
//! serves (`nsh/ui/mobile`) and the runtime's (`nsh/android`, ...), whose
//! components, modules and re-exports become exports (`components.rs`).
//! A module of both kinds (`nsh/accessibility`) has the exports of both.
//!
//! The exports are made when a bundle first imports the module. QuickJS-NG
//! resolves imports when it compiles a module, so the bytecode compiler
//! installs the same loader (a module then only declares its export names;
//! nothing runs).
//!
//! The loader also compiles the chunks of a split bundle (`chunks.rs`): the
//! code the host gave for a name that is not a native module.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    ffi::{CStr, CString, c_char, c_int, c_void},
    sync::Arc,
};

use nsh_core::{
    decl,
    ui::{ComponentSpec, DECLARATIONS, ModuleSpec, Registry, description::SetDescription},
};
use rquickjs::qjs;

use crate::{
    animated, canvas, components, desktop, dom, env, keymap, list, platform_modules,
    reactive::{self, Native},
    touch,
};

type JsValue = qjs::JSValue;
type JsContext = qjs::JSContext;

/// A value nsh's own modules export.
#[derive(Clone, Copy)]
pub(crate) enum Export {
    /// A native function and its `length`.
    Function(Native, c_int),
    /// A value made when the module is first imported (owned).
    Value(fn(*mut JsContext) -> JsValue),
}

/// The TypeScript declaration of an export (`.nsh/types`, assembled by
/// `declarations.rs`): documentation as plain text, the rest as it follows
/// the export's name.
#[derive(Clone, Copy)]
pub(crate) enum Ts {
    /// A function and its signatures (overloads): `<T>(value: T): Signal<T>`.
    Function(&'static str, &'static [&'static str]),
    /// A value and its type.
    Const(&'static str, &'static str),
    /// A class: ` extends Base { ... }` or ` { ... }`.
    Class(&'static str, &'static str),
    /// Not declared: what only compiled code or development bundles import.
    Internal,
}

impl Ts {
    // Constructors for the tables: the text is left out of builds without
    // declarations (`nsh_core::ui::DECLARATIONS`).

    /// [`Ts::Function`].
    pub(crate) const fn function(doc: &'static str, signatures: &'static [&'static str]) -> Ts {
        if DECLARATIONS {
            Ts::Function(doc, signatures)
        } else {
            Ts::Internal
        }
    }

    /// [`Ts::Const`].
    pub(crate) const fn value(doc: &'static str, ty: &'static str) -> Ts {
        if DECLARATIONS {
            Ts::Const(doc, ty)
        } else {
            Ts::Internal
        }
    }

    /// [`Ts::Class`].
    pub(crate) const fn class(doc: &'static str, body: &'static str) -> Ts {
        if DECLARATIONS {
            Ts::Class(doc, body)
        } else {
            Ts::Internal
        }
    }
}

/// An export: its name, how it is made and its declaration.
pub(crate) type Item = (&'static str, Export, Ts);

/// One of nsh's own modules.
pub(crate) struct NativeModule {
    pub(crate) name: &'static str,
    pub(crate) exports: &'static [Item],
    /// The module's own TypeScript types (interfaces, type aliases), next to
    /// the declarations of its exports.
    pub(crate) types: &'static str,
}

/// `nsh`: the API apps write against.
const NSH: &[Item] = &[
    (
        "signal",
        Export::Function(reactive::signal, 2),
        Ts::function(
            "A signal holding `value`.",
            &[
                "<T>(value: T, options?: SignalOptions<T>): Signal<T>",
                "<T = undefined>(): Signal<T | undefined>",
            ],
        ),
    ),
    (
        "computed",
        Export::Function(reactive::computed, 2),
        Ts::function(
            "A value derived from signals, computed again when they change (and read).",
            &["<T>(fn: (previous: T | undefined) => T, options?: SignalOptions<T>): Accessor<T>"],
        ),
    ),
    (
        "effect",
        Export::Function(reactive::effect, 1),
        Ts::function(
            "Runs `fn` after rendering, and again when what it read changes.",
            &["(fn: () => void): void"],
        ),
    ),
    (
        "batch",
        Export::Function(reactive::batch, 1),
        Ts::function(
            "Runs `fn`, applying its writes together at the end.",
            &["<T>(fn: () => T): T"],
        ),
    ),
    (
        "untracked",
        Export::Function(reactive::untrack, 1),
        Ts::function(
            "Runs `fn` without tracking what it reads.",
            &["<T>(fn: () => T): T"],
        ),
    ),
    (
        "cleanup",
        Export::Function(reactive::on_cleanup, 1),
        Ts::function(
            "Runs `fn` before the current effect or component runs again or goes away.",
            &["(fn: () => void): void"],
        ),
    ),
    (
        "context",
        Export::Function(dom::context, 1),
        Ts::function(
            "A context with a default value.",
            &["<T>(defaultValue: T): Context<T>"],
        ),
    ),
    (
        "mount",
        Export::Function(dom::mount, 2),
        Ts::function(
            "Renders `app` into the surface; returns a function that removes it.",
            &["<P extends object>(app: Component<P>, props?: P): () => void"],
        ),
    ),
    (
        "split",
        Export::Function(dom::split, 2),
        Ts::function(
            "`[picked, rest]`: the props named in `keys` and the others, both still\nfollowing what they read (a component passing the rest on: `<Input {...rest} />`).",
            &[
                "<T extends object, K extends readonly (keyof T)[]>(props: T, keys: K): [Pick<T, K[number]>, Omit<T, K[number]>]",
            ],
        ),
    ),
    (
        "View",
        Export::Value(|ctx| dom::host_component(ctx, "view", true)),
        Ts::function(
            "A view: the box layout, styles, touches and accessibility work on\n(UIView / ViewGroup).",
            &["(props: ViewProps): YNode"],
        ),
    ),
    (
        "Text",
        Export::Value(|ctx| dom::host_component(ctx, "text", true)),
        Ts::function(
            "A paragraph of text; a `<Text>` in it is a span of the same paragraph, other\nelements are inline attachments.",
            &["(props: TextProps): YNode"],
        ),
    ),
    (
        "Scroll",
        Export::Value(|ctx| dom::host_component(ctx, "scroll", true)),
        Ts::function(
            "A native scroll view (UIScrollView / ScrollView), zoomable; for long\nlists, `List` (`nsh/list`).",
            &["(props: ScrollProps): YNode"],
        ),
    ),
    (
        "Input",
        Export::Value(|ctx| dom::host_component(ctx, "input", true)),
        Ts::function(
            "A native text input (UITextField / UITextView, EditText); `ref` has\n`focus()`, `blur()` and `setSelection(start, end)`.",
            &["(props: InputProps): YNode"],
        ),
    ),
];

/// `nsh`'s types; its element and style types and `JSX` are `types.d.ts`'s.
const NSH_TYPES: &str = r#"export type * from "./types";

/** A value that changes over time, read by calling it (tracked). */
export interface Accessor<T> {
  (): T;
}

/** A signal: `count()` reads it, `count.set(1)` and `count.update((n) => n + 1)` write it. */
export interface Signal<T> extends Accessor<T> {
  set(value: T): void;
  update(fn: (value: T) => T): void;
}

export interface SignalOptions<T> {
  /** When a new value counts as a change (default `===`); `false`: always. */
  equals?: false | ((previous: T, next: T) => boolean);
}

/** A value components provide to what they render: `Theme()` reads the nearest one. */
export interface Context<T> extends Accessor<T> {
  Provider: (props: { value: T; children?: JSX.Element }) => JSX.Element;
}

/** A component: a function of its props rendering elements. */
export type Component<P = {}> = (props: P) => JSX.Element;
"#;

pub(crate) const MODULES: &[NativeModule] = &[
    NativeModule {
        name: "nsh",
        exports: NSH,
        types: decl!(NSH_TYPES),
    },
    NativeModule {
        name: "nsh:rt",
        exports: dom::RT_EXPORTS,
        types: "",
    },
    NativeModule {
        name: "nsh/device",
        exports: env::DEVICE_EXPORTS,
        types: decl!(env::DEVICE_TYPES),
    },
    NativeModule {
        name: "nsh/accessibility",
        exports: env::ACCESSIBILITY_EXPORTS,
        types: decl!(env::ACCESSIBILITY_TYPES),
    },
    NativeModule {
        name: "nsh/android",
        exports: platform_modules::ANDROID_EXPORTS,
        types: decl!(platform_modules::ANDROID_TYPES),
    },
    NativeModule {
        name: "nsh/ios",
        exports: platform_modules::IOS_EXPORTS,
        types: decl!(platform_modules::IOS_TYPES),
    },
    NativeModule {
        name: "nsh/desktop",
        exports: desktop::EXPORTS,
        types: decl!(desktop::TYPES),
    },
    NativeModule {
        name: "nsh/webview",
        exports: platform_modules::WEBVIEW_EXPORTS,
        types: decl!(platform_modules::WEBVIEW_TYPES),
    },
    NativeModule {
        name: "nsh/video",
        exports: platform_modules::VIDEO_EXPORTS,
        types: decl!(platform_modules::VIDEO_TYPES),
    },
    NativeModule {
        name: "nsh/native",
        exports: platform_modules::NATIVE_EXPORTS,
        types: decl!(platform_modules::NATIVE_TYPES),
    },
    NativeModule {
        name: "nsh/gesture",
        exports: touch::GESTURE_EXPORTS,
        types: decl!(touch::GESTURE_TYPES),
    },
    NativeModule {
        name: "nsh/animated",
        exports: animated::EXPORTS,
        types: decl!(animated::TYPES),
    },
    NativeModule {
        name: "nsh/canvas",
        exports: canvas::EXPORTS,
        types: decl!(canvas::TYPES),
    },
    NativeModule {
        name: "nsh/list",
        exports: list::EXPORTS,
        types: decl!(list::TYPES),
    },
    NativeModule {
        name: "nsh/image",
        exports: IMAGE_EXPORTS,
        types: "",
    },
    NativeModule {
        name: "nsh/keymap",
        exports: keymap::EXPORTS,
        types: decl!(keymap::TYPES),
    },
];

/// `nsh/image`: `Image`, the image element as a component carrying the
/// functions of the runtime's module (`getSize`, `prefetch`).
const IMAGE_EXPORTS: &[Item] = &[(
    "Image",
    Export::Value(|ctx| {
        let image = dom::host_component(ctx, "image", true);
        components::add_runtime_functions(ctx, image, "nsh/image", "Image");
        image
    }),
    Ts::value(
        "An image (a URL or `import logo from \"./logo.png\"`), decoded and cached\nnatively; `Image.getSize(uri)` and `Image.prefetch(uri)` ask about one\nwithout showing it.",
        "((props: ImageProps) => YNode) & ImageModule",
    ),
)];

/// One export of a module, as it is made.
enum Made {
    Own(Export),
    /// A Rust component, with the functions of a module of the same name.
    Component(Arc<ComponentSpec>, Option<Arc<ModuleSpec>>),
    /// A module object.
    Module(Arc<ModuleSpec>),
    /// One function of a flat module.
    Function(Arc<ModuleSpec>, &'static str),
    /// A Rust class (`#[class]`).
    Class(&'static nsh_core::ui::ClassDef),
    /// A value supplied by this VM's embedding host.
    Value(nsh_core::ui::ValueSpec),
    /// Conflicting host bindings must fail instead of choosing an authority.
    DuplicateValue,
    /// A runtime value a registry exports as it is (`safeArea`).
    Reexport(&'static str),
}

/// The modules a VM (or the bytecode compiler) serves: nsh's own, the
/// runtime's registries and the host's UI sets, and the chunks of a split
/// bundle.
pub(crate) struct ModuleTable {
    registries: Vec<Registry>,
    /// Where the host's UI sets start in `registries` (after the
    /// runtime's).
    host: usize,
    /// UI sets known by their exports' names only (the bytecode compiler's:
    /// it links imports by name and never runs the modules).
    declared: Vec<SetDescription>,
    /// Canonical host exports and their public facade, owned by this context.
    materialized: RefCell<HashMap<String, Materialized>>,
    pub(crate) chunks: RefCell<ChunkCodes>,
}

/// Host modules are constructed once, shared by imports and `nativeModule`.
/// The facade may be mutated by JS; imports retain the canonical exports.
enum Materialized {
    Initializing,
    Ready {
        exports: Vec<(String, JsValue)>,
        object: JsValue,
    },
    Failed(JsValue),
}

/// The code of a chunk.
pub(crate) enum ChunkCode {
    Source(String),
    /// From `compile_bytecode`, trusted (as `Host::eval_bytecode`'s).
    Bytecode(Vec<u8>),
}

/// The chunks of a split bundle a VM knows of, by id (module name).
#[derive(Default)]
pub(crate) struct ChunkCodes {
    /// Given, not imported yet: the loader compiles them (and drops the
    /// code).
    pub(crate) code: HashMap<String, ChunkCode>,
    /// Compiled (QuickJS has the modules).
    pub(crate) loaded: HashSet<String>,
}

impl ChunkCodes {
    /// Whether chunk `id` can be imported now.
    pub(crate) fn has(&self, id: &str) -> bool {
        id == nsh_core::js::ENTRY_CHUNK || self.loaded.contains(id) || self.code.contains_key(id)
    }
}

impl ModuleTable {
    pub(crate) fn new(ui_sets: &[Registry]) -> ModuleTable {
        let mut registries: Vec<Registry> = nsh_core::runtime::registries().to_vec();
        let host = registries.len();
        registries.extend(ui_sets.iter().filter(|r| r.specifier().is_some()).cloned());
        ModuleTable {
            registries,
            host,
            declared: Vec::new(),
            materialized: RefCell::default(),
            chunks: RefCell::default(),
        }
    }

    /// The table the bytecode compiler links against: nsh's own modules,
    /// the runtime's registries and `ui_sets` by their exports' names.
    pub(crate) fn compiling(ui_sets: &[SetDescription]) -> ModuleTable {
        let registries = nsh_core::runtime::registries().to_vec();
        ModuleTable {
            // No host sets: `nativeModule` is a run-time call, never an
            // import the compiler links.
            host: registries.len(),
            registries,
            declared: ui_sets.to_vec(),
            materialized: RefCell::default(),
            chunks: RefCell::default(),
        }
    }

    /// Releases every value the cache owns before its context is destroyed.
    pub(crate) fn free_all(&mut self, ctx: *mut JsContext) {
        for (_, module) in self.materialized.get_mut().drain() {
            // SAFETY: owned values; no JS runs after state uninstall.
            unsafe {
                match module {
                    Materialized::Ready { exports, object } => {
                        for (_, value) in exports {
                            qjs::JS_FreeValue(ctx, value);
                        }
                        qjs::JS_FreeValue(ctx, object);
                    }
                    Materialized::Failed(error) => qjs::JS_FreeValue(ctx, error),
                    Materialized::Initializing => {}
                }
            }
        }
    }

    /// The names module `name` exports, if it is one: what the loader
    /// declares (the values are made when it is evaluated, [`init`]).
    fn export_names(&self, name: &str) -> Option<Vec<String>> {
        let own = self.exports(name);
        let mut found = own.is_some();
        let mut names: Vec<String> = own.into_iter().flatten().map(|(name, _)| name).collect();
        for set in self.declared.iter().filter(|s| s.specifier == name) {
            found = true;
            for export in &set.exports {
                if !names.contains(&export.name) {
                    names.push(export.name.clone());
                }
            }
        }
        found.then_some(names)
    }

    /// The exports of module `name`, if it is one.
    fn exports(&self, name: &str) -> Option<Vec<(String, Made)>> {
        let mut out: Vec<(String, Made)> = Vec::new();
        let mut found = false;
        if name == crate::hot::SPECIFIER {
            let own = crate::hot::EXPORTS.iter();
            return Some(own.map(|&(n, e, _)| (n.to_owned(), Made::Own(e))).collect());
        }
        if let Some(m) = MODULES.iter().find(|m| m.name == name) {
            found = true;
            out.extend(
                m.exports
                    .iter()
                    .map(|&(n, e, _)| (n.to_owned(), Made::Own(e))),
            );
        }
        for registry in self
            .registries
            .iter()
            .filter(|r| r.specifier() == Some(name))
        {
            found = true;
            registry_exports(registry, &mut out);
        }
        reject_value_conflicts(&mut out);
        // nsh's own export wins over a registry's of the same name (it
        // builds on it: `AccessibilityInfo`).
        let mut seen = std::collections::HashSet::new();
        out.retain(|(name, _)| seen.insert(name.clone()));
        found.then_some(out)
    }

    /// The exports of the UI set the host registered under `specifier`
    /// (`nativeModule`): only the host's sets, never nsh's own modules or
    /// the runtime's.
    fn host_exports(&self, specifier: &str) -> Option<Vec<(String, Made)>> {
        if MODULES.iter().any(|m| m.name == specifier) {
            return None;
        }
        let mut out = Vec::new();
        let mut found = false;
        for registry in self.registries[self.host..]
            .iter()
            .filter(|r| r.specifier() == Some(specifier))
        {
            found = true;
            registry_exports(registry, &mut out);
        }
        reject_value_conflicts(&mut out);
        let mut seen = std::collections::HashSet::new();
        out.retain(|(name, _)| seen.insert(name.clone()));
        found.then_some(out)
    }
}

/// Marks ambiguous host authority before exports are deduplicated.
fn reject_value_conflicts(exports: &mut [(String, Made)]) {
    let conflicts: HashSet<_> = exports
        .iter()
        .filter(|(_, made)| matches!(made, Made::Value(_)))
        .filter(|(name, _)| exports.iter().filter(|(other, _)| other == name).count() > 1)
        .map(|(name, _)| name.clone())
        .collect();
    for (name, made) in exports {
        if conflicts.contains(name) {
            *made = Made::DuplicateValue;
        }
    }
}

/// The exports of `registry`: its components, modules, classes and
/// re-exports.
fn registry_exports(registry: &Registry, out: &mut Vec<(String, Made)>) {
    let modules = registry.module_specs();
    let specs = registry.specs();
    for spec in &specs {
        let module = modules.iter().find(|m| m.name == spec.export).cloned();
        out.push((spec.export.clone(), Made::Component(spec.clone(), module)));
    }
    for m in &modules {
        if specs.iter().any(|s| s.export == m.name) {
            continue;
        }
        if m.flat {
            for f in &m.functions {
                out.push((f.name.to_owned(), Made::Function(m.clone(), f.name)));
            }
        } else {
            out.push((m.name.to_owned(), Made::Module(m.clone())));
        }
    }
    for class in registry.class_specs() {
        out.push((class.name.to_owned(), Made::Class(class.def)));
    }
    for value in registry.value_specs() {
        out.push((value.name.to_owned(), Made::Value(value.clone())));
    }
    for &value in registry
        .reexports()
        .0
        .iter()
        .chain(registry.native_value_names())
    {
        out.push((value.to_owned(), Made::Reexport(value)));
    }
}

/// Makes runtime `rt` load the modules of `table` (which outlives it).
pub(crate) fn set_loader(rt: *mut qjs::JSRuntime, table: *const ModuleTable) {
    // SAFETY: a plain C loader; the default name normalization keeps bare
    // specifiers as they are.
    unsafe {
        qjs::JS_SetModuleLoaderFunc(rt, None, Some(load), table as *mut c_void);
    }
}

unsafe extern "C" fn load(
    ctx: *mut JsContext,
    name: *const c_char,
    opaque: *mut c_void,
) -> *mut qjs::JSModuleDef {
    // SAFETY: QuickJS passes a NUL-terminated module name; the table is the
    // one `set_loader` was given.
    let (cname, table) = unsafe { (CStr::from_ptr(name), &*(opaque as *const ModuleTable)) };
    let exports = cname.to_str().ok().and_then(|n| table.export_names(n));
    // Taken first: compiling it loads its imports, here again.
    let chunk = match (&exports, cname.to_str()) {
        (None, Ok(id)) => table.chunks.borrow_mut().code.remove(id).map(|c| (id, c)),
        _ => None,
    };
    if let Some((id, code)) = chunk {
        // SAFETY: a live context; `code` is trusted as the host's API
        // requires.
        let m = unsafe { compile_chunk(ctx, name, code) };
        if !m.is_null() {
            table.chunks.borrow_mut().loaded.insert(id.to_owned());
        }
        return m;
    }
    let Some(exports) = exports else {
        // SAFETY: throws on a live context; `%s` keeps the name literal.
        unsafe {
            qjs::JS_ThrowReferenceError(
                ctx,
                c"could not load module '%s' (only nsh's own modules can be imported at run time)"
                    .as_ptr(),
                name,
            )
        };
        return std::ptr::null_mut();
    };
    // SAFETY: a new C module declaring its exports (set by `init`).
    unsafe {
        let m = qjs::JS_NewCModule(ctx, name, Some(init));
        if m.is_null() {
            return m;
        }
        for export in &exports {
            let export = CString::new(export.as_str()).unwrap_or_default();
            qjs::JS_AddModuleExport(ctx, m, export.as_ptr());
        }
        m
    }
}

/// Compiles chunk `name` (its imports resolved by the loader, recursively);
/// null with the exception pending when it does not compile.
///
/// # Safety
///
/// `ctx` is live; bytecode is trusted.
unsafe fn compile_chunk(
    ctx: *mut JsContext,
    name: *const c_char,
    code: ChunkCode,
) -> *mut qjs::JSModuleDef {
    // SAFETY: per the contract; QuickJS reads source up to the NUL it
    // requires after it.
    let module = unsafe {
        match code {
            ChunkCode::Source(mut source) => {
                let len = source.len();
                source.push('\0');
                qjs::JS_Eval(
                    ctx,
                    source.as_ptr() as *const c_char,
                    len as _,
                    name,
                    (qjs::JS_EVAL_TYPE_MODULE | qjs::JS_EVAL_FLAG_COMPILE_ONLY) as c_int,
                )
            }
            ChunkCode::Bytecode(bytes) => qjs::JS_ReadObject(
                ctx,
                bytes.as_ptr(),
                bytes.len() as _,
                qjs::JS_READ_OBJ_BYTECODE as c_int,
            ),
        }
    };
    // SAFETY: inspecting and freeing the value just made.
    unsafe {
        if qjs::JS_IsException(module) {
            return std::ptr::null_mut();
        }
        if qjs::JS_VALUE_GET_TAG(module) != qjs::JS_TAG_MODULE {
            qjs::JS_FreeValue(ctx, module);
            qjs::JS_ThrowTypeError(ctx, c"chunk '%s' is not a module".as_ptr(), name);
            return std::ptr::null_mut();
        }
        let m = qjs::JS_VALUE_GET_PTR(module) as *mut qjs::JSModuleDef;
        // The context keeps the module: the value is one more reference.
        qjs::JS_FreeValue(ctx, module);
        m
    }
}

unsafe extern "C" fn init(ctx: *mut JsContext, m: *mut qjs::JSModuleDef) -> c_int {
    // SAFETY: the module's name atom, freed below; exports are new values
    // the module takes; the state's table lives as long as the context.
    unsafe {
        let atom = qjs::JS_GetModuleName(ctx, m);
        let name = qjs::JS_AtomToCStringLen(ctx, std::ptr::null_mut(), atom);
        qjs::JS_FreeAtom(ctx, atom);
        if name.is_null() {
            return -1;
        }
        let state = crate::state::state(ctx);
        let specifier = CStr::from_ptr(name).to_string_lossy().into_owned();
        qjs::JS_FreeCString(ctx, name);
        let Some(exports) = (*state).modules.exports(&specifier) else {
            return -1;
        };
        if let Some(object) = host_module(ctx, &specifier) {
            if qjs::JS_IsException(object) {
                return -1;
            }
            qjs::JS_FreeValue(ctx, object);
            let canonical = {
                let cached = (*state).modules.materialized.borrow();
                let Some(Materialized::Ready { exports, .. }) = cached.get(&specifier) else {
                    return -1;
                };
                exports.clone()
            };
            // Canonical values stay owned by the cache for the context's life.
            for (export, value) in canonical {
                let cname = CString::new(export.as_str()).unwrap_or_default();
                if qjs::JS_SetModuleExport(ctx, m, cname.as_ptr(), qjs::JS_DupValue(ctx, value))
                    != 0
                {
                    return -1;
                }
            }
            return 0;
        }
        // Reject ambiguous authority before constructing any host values.
        if let Some((name, _)) = exports
            .iter()
            .find(|(_, made)| matches!(made, Made::DuplicateValue))
        {
            let name = CString::new(name.as_str()).unwrap_or_default();
            qjs::JS_ThrowTypeError(
                ctx,
                c"host export '%s' is registered twice".as_ptr(),
                name.as_ptr(),
            );
            return -1;
        }
        for (export, made) in exports {
            let value = make(ctx, &export, made);
            if qjs::JS_IsException(value) {
                return -1;
            }
            let cname = CString::new(export.as_str()).unwrap_or_default();
            if qjs::JS_SetModuleExport(ctx, m, cname.as_ptr(), value) != 0 {
                return -1;
            }
        }
    }
    0
}

/// The value of export `export` (owned; an exception when making it threw).
fn make(ctx: *mut JsContext, export: &str, made: Made) -> JsValue {
    match made {
        Made::Own(Export::Function(f, length)) => {
            let cname = CString::new(export).unwrap_or_default();
            reactive::new_function(ctx, &cname, f, length)
        }
        Made::Own(Export::Value(make)) => make(ctx),
        Made::Component(spec, module) => components::component(ctx, spec, module),
        Made::Module(spec) => components::module(ctx, &spec),
        Made::Function(spec, f) => components::module_function(ctx, &spec, f),
        Made::Class(def) => match crate::classes::constructor(ctx, def) {
            Ok(ctor) => ctor.take(),
            Err(_) => qjs::JS_EXCEPTION,
        },
        Made::Value(spec) => {
            let host = crate::classes::host_outside_calls(ctx);
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                spec.make()
                    .to_js_value(&host)
                    .map(|v| crate::classes::from_raw(v.into_raw()))
            })) {
                Ok(Ok(value)) => value,
                Ok(Err(error)) => {
                    let message = CString::new(error.to_string()).unwrap_or_default();
                    // SAFETY: the context is live and `%s` keeps the message literal.
                    unsafe { qjs::JS_ThrowTypeError(ctx, c"%s".as_ptr(), message.as_ptr()) }
                }
                Err(panic) => {
                    let message =
                        CString::new(crate::classes::panic_message(panic)).unwrap_or_default();
                    // SAFETY: as above.
                    unsafe { qjs::JS_ThrowInternalError(ctx, c"%s".as_ptr(), message.as_ptr()) }
                }
            }
        }
        Made::DuplicateValue => {
            let name = CString::new(export).unwrap_or_default();
            // SAFETY: the context is live and `%s` keeps the name literal.
            unsafe {
                qjs::JS_ThrowTypeError(
                    ctx,
                    c"host export '%s' is registered twice".as_ptr(),
                    name.as_ptr(),
                )
            }
        }
        Made::Reexport(value) => env::reexport(ctx, value),
    }
}

/// The exports of the UI set the host registered under `specifier`, as one
/// object (`nativeModule`; owned): `None` when the host registered none,
/// an exception when making an export threw.
pub(crate) fn host_module(ctx: *mut JsContext, specifier: &str) -> Option<JsValue> {
    // SAFETY: the context owns the table for its entire lifetime.
    let table = unsafe { &(*crate::state::state(ctx)).modules };
    let cached = table.materialized.borrow().get(specifier).map(|module| {
        // SAFETY: duplicating does not run JS; the cache owns each value.
        unsafe {
            match module {
                Materialized::Ready { object, .. } => (Some(qjs::JS_DupValue(ctx, *object)), None),
                Materialized::Failed(error) => (None, Some(qjs::JS_DupValue(ctx, *error))),
                Materialized::Initializing => (None, None),
            }
        }
    });
    if let Some((value, error)) = cached {
        // SAFETY: exceptions consume owned duplicates, after the cache borrow ends.
        return Some(unsafe {
            match (value, error) {
                (Some(value), _) => value,
                (_, Some(error)) => qjs::JS_Throw(ctx, error),
                _ => qjs::JS_ThrowTypeError(ctx, c"host module is already initializing".as_ptr()),
            }
        });
    }
    let public_names: HashSet<_> = table
        .host_exports(specifier)?
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    let exports = table.exports(specifier)?;
    table
        .materialized
        .borrow_mut()
        .insert(specifier.to_owned(), Materialized::Initializing);
    // No cache borrow is held while factories or conversions execute.
    let materialized = materialize(ctx, exports, &public_names);
    let value = match &materialized {
        // SAFETY: new owned values are retained in the cache below.
        Materialized::Ready { object, .. } => unsafe { qjs::JS_DupValue(ctx, *object) },
        // SAFETY: as above; throwing transfers the duplicate to QuickJS.
        Materialized::Failed(error) => unsafe { qjs::JS_Throw(ctx, qjs::JS_DupValue(ctx, *error)) },
        Materialized::Initializing => unreachable!(),
    };
    table
        .materialized
        .borrow_mut()
        .insert(specifier.to_owned(), materialized);
    Some(value)
}

/// Constructs the whole module or retains its first error, before exposing it.
fn materialize(
    ctx: *mut JsContext,
    exports: Vec<(String, Made)>,
    public_names: &HashSet<String>,
) -> Materialized {
    // SAFETY: the context is live; every new value is transferred or freed.
    unsafe {
        if let Some((export, _)) = exports
            .iter()
            .find(|(_, made)| matches!(made, Made::DuplicateValue))
        {
            let name = CString::new(export.as_str()).unwrap_or_default();
            qjs::JS_ThrowTypeError(
                ctx,
                c"host export '%s' is registered twice".as_ptr(),
                name.as_ptr(),
            );
            return Materialized::Failed(qjs::JS_GetException(ctx));
        }
        let object = qjs::JS_NewObject(ctx);
        if qjs::JS_IsException(object) {
            return Materialized::Failed(qjs::JS_GetException(ctx));
        }
        let mut canonical = Vec::new();
        for (export, made) in exports {
            let value = make(ctx, &export, made);
            if !qjs::JS_IsException(value) {
                let name = CString::new(export.as_str()).unwrap_or_default();
                if !public_names.contains(&export)
                    || qjs::JS_DefinePropertyValueStr(
                        ctx,
                        object,
                        name.as_ptr(),
                        qjs::JS_DupValue(ctx, value),
                        qjs::JS_PROP_C_W_E as i32,
                    ) >= 0
                {
                    canonical.push((export, value));
                    continue;
                }
                qjs::JS_FreeValue(ctx, value);
            }
            let error = qjs::JS_GetException(ctx);
            for (_, value) in canonical {
                qjs::JS_FreeValue(ctx, value);
            }
            qjs::JS_FreeValue(ctx, object);
            return Materialized::Failed(error);
        }
        Materialized::Ready {
            exports: canonical,
            object,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use nsh_core::ui::{JsHost, JsOwned, PropError, Registry, Ret, TsTypes};

    use crate::{Host, HostOptions};

    #[test]
    fn host_values_are_lazy_once_per_vm_and_absent_from_other_vms() {
        let calls = Arc::new(AtomicUsize::new(0));
        let called = calls.clone();
        let mut registry = Registry::ui_set("test/host");
        registry.value("identity", move || {
            called.fetch_add(1, Ordering::SeqCst) as u32
        });
        let source = "import { identity } from 'test/host'; if (identity !== EXPECTED) throw Error('wrong owner');";
        let bytecode = crate::compile_bytecode_with(
            &source.replace("EXPECTED", "0"),
            false,
            &[(&registry).into()],
        )
        .unwrap();
        let declarations = crate::type_definitions(&[(&registry).into()]);
        assert!(declarations.iter().any(|(path, body)| {
            path == "test/host.d.ts" && body.contains("export declare const identity: number;")
        }));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "tooling cannot acquire host capabilities"
        );

        let host = Host::with_options(HostOptions {
            ui_sets: vec![registry.clone()],
            ..Default::default()
        })
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0, "VM creation is not import");
        host.eval_bytecode(&bytecode).unwrap();
        host.eval_source(&source.replace("EXPECTED", "0")).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let other = Host::with_options(HostOptions {
            ui_sets: vec![registry],
            ..Default::default()
        })
        .unwrap();
        other.eval_source(&source.replace("EXPECTED", "1")).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[nsh_core::ui::class]
    struct BoundHost {
        identity: u32,
    }

    #[nsh_core::ui::class]
    impl BoundHost {
        pub(crate) fn identity(&self) -> u32 {
            self.identity
        }
    }

    #[test]
    fn native_module_and_import_share_canonical_values_in_both_orders() {
        for native_first in [false, true] {
            let calls = Arc::new(AtomicUsize::new(0));
            let called = calls.clone();
            let mut registry = Registry::ui_set("test/host");
            registry.value("host", move || {
                nsh_core::ui::Instance::new(BoundHost {
                    identity: called.fetch_add(1, Ordering::SeqCst) as u32,
                })
            });
            let host = Host::with_options(HostOptions {
                ui_sets: vec![registry],
                ..Default::default()
            })
            .unwrap();
            let access = r#"
                import { nativeModule } from 'nsh/native';
                const module = nativeModule('test/host');
                globalThis.original = module.host;
                globalThis.constructor = module.BoundHost;
                if (module !== nativeModule('test/host')) throw Error('uncached facade');
                Object.defineProperty(module, 'host', { get() { throw Error('replaced facade'); } });
            "#;
            if native_first {
                host.eval_source(access).unwrap();
            } else {
                host.eval_source("import { host } from 'test/host'; if (host.identity() !== 0) throw Error('wrong owner');").unwrap();
                host.eval_source(access).unwrap();
            }
            host.eval_source(r#"
                import { host, BoundHost } from 'test/host';
                if (host !== globalThis.original || host.identity() !== 0 || BoundHost !== globalThis.constructor)
                    throw Error('import did not retain canonical values');
            "#).unwrap();
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn host_exports_can_extend_a_runtime_registry_without_hiding_its_exports() {
        let mut registry = Registry::ui_set("nsh/clipboard");
        registry.value("extra", || 42u32);
        let host = Host::with_options(HostOptions {
            ui_sets: vec![registry],
            ..Default::default()
        })
        .unwrap();
        host.eval_source(
            r#"
            import { nativeModule } from 'nsh/native';
            const host = nativeModule('nsh/clipboard');
            if (host.extra !== 42 || 'Clipboard' in host) throw Error('wrong host facade');
        "#,
        )
        .unwrap();
        host.eval_source(r#"
            import { Clipboard, extra } from 'nsh/clipboard';
            if (typeof Clipboard.getString !== 'function' || extra !== 42) throw Error('runtime export hidden');
        "#).unwrap();

        let mut registry = Registry::ui_set("nsh/clipboard");
        registry.value("Clipboard", || -> u32 {
            panic!("ambiguous factory must not execute")
        });
        let host = Host::with_options(HostOptions {
            ui_sets: vec![registry],
            ..Default::default()
        })
        .unwrap();
        let error = host
            .eval_source(
                "import { nativeModule } from 'nsh/native'; nativeModule('nsh/clipboard');",
            )
            .unwrap_err();
        assert!(
            error
                .message
                .contains("host export 'Clipboard' is registered twice"),
            "{error}"
        );
    }

    struct Invalid;

    impl Ret for Invalid {
        fn ts_type(_: &mut TsTypes) -> String {
            "never".into()
        }
        fn into_js<'a>(self, _: &'a dyn JsHost) -> Result<JsOwned<'a>, PropError> {
            Err(PropError("invalid host value".into()))
        }
    }

    #[test]
    fn host_value_failures_are_module_errors() {
        for panic in [false, true] {
            let mut registry = Registry::ui_set("test/bad-host");
            registry.value("host", move || {
                assert!(!panic, "host factory failed");
                Invalid
            });
            let host = Host::with_options(HostOptions {
                ui_sets: vec![registry],
                ..Default::default()
            })
            .unwrap();
            let error = host
                .eval_source("import { host } from 'test/bad-host';")
                .unwrap_err();
            assert!(
                error.message.contains(if panic {
                    "host factory failed"
                } else {
                    "invalid host value"
                }),
                "{error}"
            );
        }
    }

    #[test]
    fn failed_host_module_materialization_is_not_retried_in_the_vm() {
        let calls = Arc::new(AtomicUsize::new(0));
        let called = calls.clone();
        let mut registry = Registry::ui_set("test/bad-host");
        registry.value("host", move || {
            called.fetch_add(1, Ordering::SeqCst);
            Invalid
        });
        let host = Host::with_options(HostOptions {
            ui_sets: vec![registry],
            ..Default::default()
        })
        .unwrap();
        for _ in 0..2 {
            let error = host
                .eval_source(
                    "import { nativeModule } from 'nsh/native'; nativeModule('test/bad-host');",
                )
                .unwrap_err();
            assert!(error.message.contains("invalid host value"), "{error}");
        }
        let error = host
            .eval_source("import { host } from 'test/bad-host';")
            .unwrap_err();
        assert!(error.message.contains("invalid host value"), "{error}");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn conflicting_host_registries_do_not_choose_a_factory() {
        let mut first = Registry::ui_set("test/host");
        first.value("unrelated", || -> u32 {
            panic!("module must not acquire host values")
        });
        first.value("host", || -> u32 { panic!("first factory must not run") });
        let mut second = Registry::ui_set("test/host");
        second.value("host", || -> u32 { panic!("second factory must not run") });
        let host = Host::with_options(HostOptions {
            ui_sets: vec![first, second],
            ..Default::default()
        })
        .unwrap();
        let error = host
            .eval_source("import { host } from 'test/host';")
            .unwrap_err();
        assert!(
            error
                .message
                .contains("host export 'host' is registered twice"),
            "{error}"
        );
    }

    #[test]
    fn nshs_own_modules_are_the_ones_the_bundler_knows() {
        let names: Vec<&str> = super::MODULES.iter().map(|m| m.name).collect();
        assert_eq!(names, nsh_core::js::NATIVE_MODULES);
    }

    /// The bytecode compiler, which knows a UI set by its description,
    /// links against the names a VM serving the set exports.
    #[test]
    fn a_described_set_exports_what_its_registry_does() {
        for registry in [nsh_ui_mobile::registry(), crate::classes::tests::registry()] {
            let specifier = registry.specifier().unwrap();
            let served = super::ModuleTable::new(std::slice::from_ref(&registry));
            let compiled = super::ModuleTable::compiling(&[(&registry).into()]);
            let mut served: Vec<String> = served
                .exports(specifier)
                .unwrap()
                .into_iter()
                .map(|(name, _)| name)
                .collect();
            let mut compiled = compiled.export_names(specifier).unwrap();
            assert!(!served.is_empty(), "{specifier}");
            served.sort();
            compiled.sort();
            assert_eq!(compiled, served, "{specifier}");
        }
        let compiling = super::ModuleTable::compiling(&[]);
        assert!(compiling.export_names("nsh/ui/mobile").is_none());
        assert!(compiling.export_names("nsh").is_some());
    }
}
