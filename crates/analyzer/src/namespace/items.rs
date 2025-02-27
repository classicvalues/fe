use crate::context;

use crate::context::{Analysis, Constant};
use crate::errors::{self, IncompleteItem, TypeError};
use crate::namespace::types::FixedSize;
use crate::namespace::types::{self, GenericType};
use crate::traversal::pragma::check_pragma_version;
use crate::AnalyzerDb;
use crate::{builtins, errors::ConstEvalError};
use fe_common::diagnostics::Diagnostic;
use fe_common::files::{common_prefix, Utf8Path};
use fe_common::{impl_intern_key, FileKind, SourceFileId};
use fe_parser::ast;
use fe_parser::node::{Node, Span};
use indexmap::{indexmap, IndexMap, IndexSet};
use smol_str::SmolStr;
use std::ops::Deref;
use std::rc::Rc;
use strum::IntoEnumIterator;

/// A named item. This does not include things inside of
/// a function body.
#[derive(Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Clone, Copy)]
pub enum Item {
    Ingot(IngotId),
    Module(ModuleId),
    Type(TypeDef),
    // GenericType probably shouldn't be a separate category.
    // Any of the items inside TypeDef (struct, alias, etc)
    // could be optionally generic.
    GenericType(GenericType),
    // Events aren't normal types; they *could* be moved into
    // TypeDef, but it would have consequences.
    Event(EventId),
    Function(FunctionId),
    Constant(ModuleConstantId),
    // Needed until we can represent keccak256 as a FunctionId.
    // We can't represent keccak256's arg type yet.
    BuiltinFunction(builtins::GlobalFunction),
    Intrinsic(builtins::Intrinsic),
    // This should go away soon. The globals (block, msg, etc) will be replaced
    // with a context struct that'll appear in the fn parameter list.
    Object(builtins::GlobalObject),
}

impl Item {
    pub fn name(&self, db: &dyn AnalyzerDb) -> SmolStr {
        match self {
            Item::Type(id) => id.name(db),
            Item::GenericType(id) => id.name(),
            Item::Event(id) => id.name(db),
            Item::Function(id) => id.name(db),
            Item::BuiltinFunction(id) => id.as_ref().into(),
            Item::Intrinsic(id) => id.as_ref().into(),
            Item::Object(id) => id.as_ref().into(),
            Item::Constant(id) => id.name(db),
            Item::Ingot(id) => id.name(db),
            Item::Module(id) => id.name(db),
        }
    }

    pub fn name_span(&self, db: &dyn AnalyzerDb) -> Option<Span> {
        match self {
            Item::Type(id) => id.name_span(db),
            Item::GenericType(_) => None,
            Item::Event(id) => Some(id.name_span(db)),
            Item::Function(id) => Some(id.name_span(db)),
            Item::Constant(id) => Some(id.name_span(db)),
            Item::BuiltinFunction(_)
            | Item::Intrinsic(_)
            | Item::Object(_)
            | Item::Ingot(_)
            | Item::Module(_) => None,
        }
    }

    pub fn is_builtin(&self) -> bool {
        match self {
            Item::Type(TypeDef::Primitive(_))
            | Item::GenericType(_)
            | Item::BuiltinFunction(_)
            | Item::Object(_)
            | Item::Intrinsic(_) => true,
            Item::Type(_)
            | Item::Event(_)
            | Item::Function(_)
            | Item::Constant(_)
            | Item::Ingot(_)
            | Item::Module(_) => false,
        }
    }

    pub fn is_struct(&self, val: &StructId) -> bool {
        matches!(self, Item::Type(TypeDef::Struct(current)) if current == val)
    }

    pub fn item_kind_display_name(&self) -> &'static str {
        match self {
            Item::Type(_) | Item::GenericType(_) => "type",
            Item::Event(_) => "event",
            Item::Function(_) | Item::BuiltinFunction(_) => "function",
            Item::Intrinsic(_) => "intrinsic function",
            Item::Object(_) => "object",
            Item::Constant(_) => "constant",
            Item::Ingot(_) => "ingot",
            Item::Module(_) => "module",
        }
    }

    pub fn items(&self, db: &dyn AnalyzerDb) -> Rc<IndexMap<SmolStr, Item>> {
        match self {
            Item::Ingot(ingot) => ingot.items(db),
            Item::Module(module) => module.items(db),
            Item::Type(_) => todo!("cannot access items in types yet"),
            Item::GenericType(_)
            | Item::Event(_)
            | Item::Function(_)
            | Item::Constant(_)
            | Item::BuiltinFunction(_)
            | Item::Intrinsic(_)
            | Item::Object(_) => Rc::new(indexmap! {}),
        }
    }

    pub fn parent(&self, db: &dyn AnalyzerDb) -> Option<Item> {
        match self {
            Item::Type(id) => id.parent(db),
            Item::GenericType(_) => None,
            Item::Event(id) => Some(id.parent(db)),
            Item::Function(id) => Some(id.parent(db)),
            Item::Constant(id) => Some(id.parent(db)),
            Item::Module(id) => Some(id.parent(db)),
            Item::BuiltinFunction(_) | Item::Intrinsic(_) | Item::Object(_) | Item::Ingot(_) => {
                None
            }
        }
    }

    pub fn path(&self, db: &dyn AnalyzerDb) -> Rc<[SmolStr]> {
        // The path is used to generate a yul identifier,
        // eg `foo::Bar::new` becomes `$$foo$Bar$new`.
        // Right now, the ingot name is the os path, so it could
        // be "my project/src".
        // For now, we'll just leave the ingot out of the path,
        // because we can only compile a single ingot anyway.
        match self.parent(db) {
            Some(Item::Ingot(_)) | None => [self.name(db)][..].into(),
            Some(parent) => {
                let mut path = parent.path(db).to_vec();
                path.push(self.name(db));
                path.into()
            }
        }
    }

    pub fn dependency_graph(&self, db: &dyn AnalyzerDb) -> Option<Rc<DepGraph>> {
        match self {
            Item::Type(TypeDef::Contract(id)) => Some(id.dependency_graph(db)),
            Item::Type(TypeDef::Struct(id)) => Some(id.dependency_graph(db)),
            Item::Function(id) => Some(id.dependency_graph(db)),
            _ => None,
        }
    }

    pub fn resolve_path_segments(
        &self,
        db: &dyn AnalyzerDb,
        segments: &[Node<SmolStr>],
    ) -> Analysis<Option<Item>> {
        let mut curr_item = *self;

        for node in segments {
            curr_item = match curr_item.items(db).get(&node.kind) {
                Some(item) => *item,
                None => {
                    return Analysis {
                        value: None,
                        diagnostics: Rc::new([errors::error(
                            "unresolved path item",
                            node.span,
                            "not found",
                        )]),
                    }
                }
            }
        }

        Analysis {
            value: Some(curr_item),
            diagnostics: Rc::new([]),
        }
    }

    /// Downcast utility function
    pub fn as_contract(&self) -> Option<ContractId> {
        match self {
            Item::Type(TypeDef::Contract(id)) => Some(*id),
            _ => None,
        }
    }

    pub fn sink_diagnostics(&self, db: &dyn AnalyzerDb, sink: &mut impl DiagnosticSink) {
        match self {
            Item::Type(id) => id.sink_diagnostics(db, sink),
            Item::Event(id) => id.sink_diagnostics(db, sink),
            Item::Function(id) => id.sink_diagnostics(db, sink),
            Item::GenericType(_)
            | Item::BuiltinFunction(_)
            | Item::Intrinsic(_)
            | Item::Object(_) => {}
            Item::Constant(id) => id.sink_diagnostics(db, sink),
            Item::Ingot(id) => id.sink_diagnostics(db, sink),
            Item::Module(id) => id.sink_diagnostics(db, sink),
        }
    }
}

// Placeholder; someday std::prelude will be a proper module.
pub fn std_prelude_items() -> IndexMap<SmolStr, Item> {
    let mut items = indexmap! {
        SmolStr::new("bool") => Item::Type(TypeDef::Primitive(types::Base::Bool)),
        SmolStr::new("address") => Item::Type(TypeDef::Primitive(types::Base::Address)),
    };
    items.extend(types::Integer::iter().map(|typ| {
        (
            typ.as_ref().into(),
            Item::Type(TypeDef::Primitive(types::Base::Numeric(typ))),
        )
    }));
    items.extend(types::GenericType::iter().map(|typ| (typ.name(), Item::GenericType(typ))));
    items.extend(
        builtins::GlobalFunction::iter()
            .map(|fun| (fun.as_ref().into(), Item::BuiltinFunction(fun))),
    );
    items
        .extend(builtins::Intrinsic::iter().map(|fun| (fun.as_ref().into(), Item::Intrinsic(fun))));
    items
        .extend(builtins::GlobalObject::iter().map(|obj| (obj.as_ref().into(), Item::Object(obj))));
    items
}

#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy)]
pub enum IngotMode {
    /// The target of compilation. Expected to have a main.fe file.
    Main,
    /// A library; expected to have a lib.fe file.
    Lib,
    /// A fake ingot, created to hold a single module with any filename.
    StandaloneModule,
}

/// An `Ingot` is composed of a tree of `Module`s (set via [`AnalyzerDb::set_ingot_module_tree`]),
/// and doesn't have direct knowledge of files.
#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct Ingot {
    pub name: SmolStr,
    // pub version: SmolStr,
    pub mode: IngotMode,
    pub original: Option<IngotId>,

    pub src_dir: SmolStr,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Copy, Clone)]
pub struct IngotId(pub(crate) u32);
impl_intern_key!(IngotId);
impl IngotId {
    pub fn std_lib(db: &mut dyn AnalyzerDb) -> Self {
        IngotId::from_files(
            db,
            "std",
            IngotMode::Lib,
            FileKind::Std,
            &fe_library::std_src_files(),
            indexmap!(),
        )
    }

    pub fn from_files(
        db: &mut dyn AnalyzerDb,
        name: &str,
        mode: IngotMode,
        file_kind: FileKind,
        files: &[(impl AsRef<str>, impl AsRef<str>)],
        deps: IndexMap<SmolStr, IngotId>,
    ) -> Self {
        // The common prefix of all file paths will be stored as the ingot
        // src dir path, and all module file paths will be considered to be
        // relative to this prefix.
        let file_path_prefix = if files.len() == 1 {
            // If there's only one source file, the "common prefix" is everything
            // before the file name.
            Utf8Path::new(files[0].0.as_ref())
                .parent()
                .unwrap_or_else(|| "".into())
                .to_path_buf()
        } else {
            files
                .iter()
                .map(|(path, _)| Utf8Path::new(path).to_path_buf())
                .reduce(|pref, path| common_prefix(&pref, &path))
                .expect("`IngotId::from_files`: empty file list")
        };

        let ingot = db.intern_ingot(Rc::new(Ingot {
            name: name.into(),
            mode,
            original: None,
            src_dir: file_path_prefix.as_str().into(),
        }));

        // Create a module for every .fe source file.
        let file_mods = files
            .iter()
            .map(|(path, content)| {
                let file = SourceFileId::new(
                    db.upcast_mut(),
                    file_kind,
                    path.as_ref(),
                    content.as_ref().into(),
                );
                ModuleId::new(
                    db,
                    Utf8Path::new(path).file_stem().unwrap(),
                    ModuleSource::File(file),
                    ingot,
                )
            })
            .collect();

        // We automatically build a module hierarchy that matches the directory structure.
        // We don't (yet?) require a .fe file for each directory like rust does.
        // (eg `a/b.fe` alongside `a/b/`), but we do allow it (the module's items
        // will be everything inside the .fe file, and the submodules inside the dir.
        //
        // Collect the set of all directories in the file hierarchy
        // (after stripping the common prefix from all paths).
        // eg given ["src/lib.fe", "src/a/b/x.fe", "src/a/c/d/y.fe"],
        // the dir set is {"a", "a/b", "a/c", "a/c/d"}.
        let dirs = files
            .iter()
            .flat_map(|(path, _)| {
                Utf8Path::new(path)
                    .strip_prefix(&file_path_prefix)
                    .unwrap_or_else(|_| Utf8Path::new(path))
                    .ancestors()
                    .skip(1) // first elem of .ancestors() is the path itself
            })
            .collect::<IndexSet<&Utf8Path>>();

        let dir_mods = dirs
            // Skip the dirs that have an associated fe file; eg skip "a/b" if "a/b.fe" exists.
            .difference(
                &files
                    .iter()
                    .map(|(path, _)| {
                        Utf8Path::new(path)
                            .strip_prefix(&file_path_prefix)
                            .unwrap_or_else(|_| Utf8Path::new(path))
                            .as_str()
                            .trim_end_matches(".fe")
                            .into()
                    })
                    .collect::<IndexSet<&Utf8Path>>(),
            )
            .filter_map(|dir| {
                dir.file_name().map(|name| {
                    ModuleId::new(db, name, ModuleSource::Dir(dir.as_str().into()), ingot)
                })
            })
            .collect::<Vec<_>>();

        db.set_ingot_modules(ingot, [file_mods, dir_mods].concat().into());
        db.set_ingot_external_ingots(ingot, Rc::new(deps));
        ingot
    }

    pub fn external_ingots(&self, db: &dyn AnalyzerDb) -> Rc<IndexMap<SmolStr, IngotId>> {
        db.ingot_external_ingots(*self)
    }

    pub fn all_modules(&self, db: &dyn AnalyzerDb) -> Rc<[ModuleId]> {
        db.ingot_modules(*self)
    }

    pub fn data(&self, db: &dyn AnalyzerDb) -> Rc<Ingot> {
        db.lookup_intern_ingot(*self)
    }

    pub fn name(&self, db: &dyn AnalyzerDb) -> SmolStr {
        self.data(db).name.clone()
    }

    /// Returns the `main.fe`, or `lib.fe` module, depending on the ingot "mode" (IngotMode).
    pub fn root_module(&self, db: &dyn AnalyzerDb) -> Option<ModuleId> {
        db.ingot_root_module(*self)
    }

    pub fn items(&self, db: &dyn AnalyzerDb) -> Rc<IndexMap<SmolStr, Item>> {
        self.root_module(db).expect("missing root module").items(db)
    }

    pub fn diagnostics(&self, db: &dyn AnalyzerDb) -> Vec<Diagnostic> {
        let mut diagnostics = vec![];
        self.sink_diagnostics(db, &mut diagnostics);
        diagnostics
    }

    pub fn sink_diagnostics(&self, db: &dyn AnalyzerDb, sink: &mut impl DiagnosticSink) {
        if self.root_module(db).is_none() {
            let file_name = match self.data(db).mode {
                IngotMode::Lib => "lib",
                IngotMode::Main => "main",
                IngotMode::StandaloneModule => unreachable!(), // always has a root module
            };
            sink.push(&Diagnostic::error(format!(
                "The ingot named \"{}\" is missing a `{}` module. \
                 \nPlease add a `src/{}.fe` file to the base directory.",
                self.name(db),
                file_name,
                file_name,
            )));
        }
        for module in self.all_modules(db).iter() {
            module.sink_diagnostics(db, sink)
        }
    }

    pub fn sink_external_ingot_diagnostics(
        &self,
        db: &dyn AnalyzerDb,
        sink: &mut impl DiagnosticSink,
    ) {
        for ingot in self.external_ingots(db).values() {
            ingot.sink_diagnostics(db, sink)
        }
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub enum ModuleSource {
    File(SourceFileId),
    /// For directory modules without a corresponding source file
    /// (which will soon not be allowed, and this variant can go away).
    Dir(SmolStr),
    Lowered {
        original: ModuleId,
        ast: Rc<ast::Module>,
    },
}

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct Module {
    pub name: SmolStr,
    pub ingot: IngotId,

    /// This differentiates between the original `Module` for a Fe source
    /// file (which is parsed in the [`AnalyzerDb::module_parse`] query),
    /// and the lowered `Module`, the ast of which is built in the lowering
    /// phase, and is stored in the `ModulePhase::Lowered` variant.
    // This leaks some knowledge about the existence of the lowering phase
    // into the analyzer, but it seems to be the least bad way to move
    // parsing into a db query instead of needing to parse at Module intern time.
    // Someday we'll likely lower into some new IR, in which case we won't need
    // to allow for lowered versions of `ModuleId`s.
    pub source: ModuleSource,
}

/// Id of a [`Module`], which corresponds to a single Fe source file.
/// The lowering phase will create a separate `Module` & `ModuleId`
/// for the lowered version of the Fe source code.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Copy, Clone)]
pub struct ModuleId(pub(crate) u32);
impl_intern_key!(ModuleId);
impl ModuleId {
    pub fn new_standalone(db: &mut dyn AnalyzerDb, path: &str, content: &str) -> Self {
        let std = IngotId::std_lib(db);
        let ingot = IngotId::from_files(
            db,
            "",
            IngotMode::StandaloneModule,
            FileKind::Local,
            &[(path, content)],
            indexmap! { "std".into() => std },
        );

        ingot
            .root_module(db)
            .expect("ModuleId::new_standalone ingot has no root module")
    }

    pub fn new(db: &dyn AnalyzerDb, name: &str, source: ModuleSource, ingot: IngotId) -> Self {
        db.intern_module(
            Module {
                name: name.into(),
                ingot,
                source,
            }
            .into(),
        )
    }

    pub fn data(&self, db: &dyn AnalyzerDb) -> Rc<Module> {
        db.lookup_intern_module(*self)
    }

    pub fn name(&self, db: &dyn AnalyzerDb) -> SmolStr {
        self.data(db).name.clone()
    }

    pub fn file_path_relative_to_src_dir(&self, db: &dyn AnalyzerDb) -> SmolStr {
        db.module_file_path(*self)
    }

    pub fn ast(&self, db: &dyn AnalyzerDb) -> Rc<ast::Module> {
        match &self.data(db).source {
            ModuleSource::File(_) | ModuleSource::Dir(_) => db.module_parse(*self).value,
            ModuleSource::Lowered { ast, .. } => Rc::clone(ast),
        }
    }

    pub fn ingot(&self, db: &dyn AnalyzerDb) -> IngotId {
        self.data(db).ingot
    }

    pub fn is_incomplete(&self, db: &dyn AnalyzerDb) -> bool {
        db.module_is_incomplete(*self)
    }

    /// Includes duplicate names
    pub fn all_items(&self, db: &dyn AnalyzerDb) -> Rc<[Item]> {
        db.module_all_items(*self)
    }

    /// Returns a map of the named items in the module
    pub fn items(&self, db: &dyn AnalyzerDb) -> Rc<IndexMap<SmolStr, Item>> {
        db.module_item_map(*self).value
    }

    /// Returns a `name -> (name_span, external_item)` map for all `use` statements in a module.
    pub fn used_items(&self, db: &dyn AnalyzerDb) -> Rc<IndexMap<SmolStr, (Span, Item)>> {
        db.module_used_item_map(*self).value
    }

    /// Returns all of the internal items, except for `use`d items. This is used when resolving
    /// `use` statements, as it does not create a query cycle.
    pub fn non_used_internal_items(&self, db: &dyn AnalyzerDb) -> IndexMap<SmolStr, Item> {
        let global_items = self.global_items(db);

        self.submodules(db)
            .iter()
            .map(|module| (module.name(db), Item::Module(*module)))
            .chain(global_items)
            .collect()
    }

    /// Returns all of the internal items. Internal items refers to the set of items visible when
    /// inside of a module.
    pub fn internal_items(&self, db: &dyn AnalyzerDb) -> IndexMap<SmolStr, Item> {
        let global_items = self.global_items(db);
        let defined_items = self.items(db);
        self.submodules(db)
            .iter()
            .map(|module| (module.name(db), Item::Module(*module)))
            .chain(global_items)
            .chain(defined_items.deref().clone())
            .collect()
    }

    /// Resolve a path that starts with an item defined in the module.
    pub fn resolve_path(&self, db: &dyn AnalyzerDb, path: &ast::Path) -> Analysis<Option<Item>> {
        Item::Module(*self).resolve_path_segments(db, &path.segments)
    }

    /// Resolve a path that starts with an internal item. We omit used items to avoid a query cycle.
    pub fn resolve_path_non_used_internal(
        &self,
        db: &dyn AnalyzerDb,
        path: &ast::Path,
    ) -> Analysis<Option<Item>> {
        let segments = &path.segments;
        let first_segment = &segments[0];

        if let Some(curr_item) = self.non_used_internal_items(db).get(&first_segment.kind) {
            curr_item.resolve_path_segments(db, &segments[1..])
        } else {
            Analysis {
                value: None,
                diagnostics: Rc::new([errors::error(
                    "unresolved path item",
                    first_segment.span,
                    "not found",
                )]),
            }
        }
    }

    /// Resolve a path that starts with an internal item.
    pub fn resolve_path_internal(
        &self,
        db: &dyn AnalyzerDb,
        path: &ast::Path,
    ) -> Analysis<Option<Item>> {
        let segments = &path.segments;
        let first_segment = &segments[0];

        if let Some(curr_item) = self.internal_items(db).get(&first_segment.kind) {
            curr_item.resolve_path_segments(db, &segments[1..])
        } else {
            Analysis {
                value: None,
                diagnostics: Rc::new([errors::error(
                    "unresolved path item",
                    first_segment.span,
                    "not found",
                )]),
            }
        }
    }

    /// Returns `Err(IncompleteItem)` if the name could not be resolved, and the module was
    /// not completely parsed (due to a syntax error).
    pub fn resolve_name(
        &self,
        db: &dyn AnalyzerDb,
        name: &str,
    ) -> Result<Option<Item>, IncompleteItem> {
        if let Some(thing) = self.internal_items(db).get(name) {
            Ok(Some(*thing))
        } else if self.is_incomplete(db) {
            Err(IncompleteItem::new())
        } else {
            Ok(None)
        }
    }

    pub fn resolve_constant(
        &self,
        db: &dyn AnalyzerDb,
        name: &str,
    ) -> Result<Option<ModuleConstantId>, IncompleteItem> {
        if let Some(constant) = self
            .all_constants(db)
            .iter()
            .find(|id| id.name(db) == name)
            .copied()
        {
            Ok(Some(constant))
        } else if self.is_incomplete(db) {
            Err(IncompleteItem::new())
        } else {
            Ok(None)
        }
    }
    pub fn submodules(&self, db: &dyn AnalyzerDb) -> Rc<[ModuleId]> {
        db.module_submodules(*self)
    }

    pub fn parent(&self, db: &dyn AnalyzerDb) -> Item {
        self.parent_module(db)
            .map(Item::Module)
            .unwrap_or_else(|| Item::Ingot(self.ingot(db)))
    }

    pub fn parent_module(&self, db: &dyn AnalyzerDb) -> Option<ModuleId> {
        db.module_parent_module(*self)
    }

    /// All contracts, including duplicates
    pub fn all_contracts(&self, db: &dyn AnalyzerDb) -> Rc<[ContractId]> {
        db.module_contracts(*self)
    }

    /// Returns the map of ingot deps, built-ins, and the ingot itself as "ingot".
    pub fn global_items(&self, db: &dyn AnalyzerDb) -> IndexMap<SmolStr, Item> {
        let ingot = self.ingot(db);
        let mut items = ingot
            .external_ingots(db)
            .iter()
            .map(|(name, ingot)| (name.clone(), Item::Ingot(*ingot)))
            .chain(std_prelude_items())
            .collect::<IndexMap<_, _>>();

        if ingot.data(db).mode != IngotMode::StandaloneModule {
            items.insert("ingot".into(), Item::Ingot(ingot));
        }
        items
    }

    /// All structs, including duplicatecrates/analyzer/src/db.rss
    pub fn all_structs(&self, db: &dyn AnalyzerDb) -> Rc<[StructId]> {
        db.module_structs(*self)
    }

    /// All module constants.
    pub fn all_constants(&self, db: &dyn AnalyzerDb) -> Rc<Vec<ModuleConstantId>> {
        db.module_constants(*self)
    }

    pub fn diagnostics(&self, db: &dyn AnalyzerDb) -> Vec<Diagnostic> {
        let mut diagnostics = vec![];
        self.sink_diagnostics(db, &mut diagnostics);
        diagnostics
    }

    pub fn sink_diagnostics(&self, db: &dyn AnalyzerDb, sink: &mut impl DiagnosticSink) {
        let data = self.data(db);
        if let ModuleSource::File(_) = data.source {
            sink.push_all(db.module_parse(*self).diagnostics.iter())
        }
        let ast = self.ast(db);
        for stmt in &ast.body {
            if let ast::ModuleStmt::Pragma(inner) = stmt {
                if let Some(diag) = check_pragma_version(inner) {
                    sink.push(&diag)
                }
            }
        }

        // duplicate item name errors
        sink.push_all(db.module_item_map(*self).diagnostics.iter());

        // errors for each item
        self.all_items(db)
            .iter()
            .for_each(|id| id.sink_diagnostics(db, sink));
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct ModuleConstant {
    pub ast: Node<ast::ConstantDecl>,
    pub module: ModuleId,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Copy, Clone)]
pub struct ModuleConstantId(pub(crate) u32);
impl_intern_key!(ModuleConstantId);

impl ModuleConstantId {
    pub fn data(&self, db: &dyn AnalyzerDb) -> Rc<ModuleConstant> {
        db.lookup_intern_module_const(*self)
    }
    pub fn span(&self, db: &dyn AnalyzerDb) -> Span {
        self.data(db).ast.span
    }
    pub fn typ(&self, db: &dyn AnalyzerDb) -> Result<types::Type, TypeError> {
        db.module_constant_type(*self).value
    }

    pub fn is_base_type(&self, db: &dyn AnalyzerDb) -> bool {
        matches!(self.typ(db), Ok(types::Type::Base(_)))
    }

    pub fn name(&self, db: &dyn AnalyzerDb) -> SmolStr {
        self.data(db).ast.kind.name.kind.clone()
    }

    pub fn constant_value(&self, db: &dyn AnalyzerDb) -> Result<Constant, ConstEvalError> {
        db.module_constant_value(*self).value
    }

    pub fn name_span(&self, db: &dyn AnalyzerDb) -> Span {
        self.data(db).ast.kind.name.span
    }

    pub fn value(&self, db: &dyn AnalyzerDb) -> ast::Expr {
        self.data(db).ast.kind.value.kind.clone()
    }

    pub fn parent(&self, db: &dyn AnalyzerDb) -> Item {
        Item::Module(self.data(db).module)
    }

    pub fn sink_diagnostics(&self, db: &dyn AnalyzerDb, sink: &mut impl DiagnosticSink) {
        db.module_constant_type(*self)
            .diagnostics
            .iter()
            .for_each(|d| sink.push(d));
    }
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Copy, Clone)]
pub enum TypeDef {
    Alias(TypeAliasId),
    Struct(StructId),
    Contract(ContractId),
    Primitive(types::Base),
}
impl TypeDef {
    pub fn name(&self, db: &dyn AnalyzerDb) -> SmolStr {
        match self {
            TypeDef::Alias(id) => id.name(db),
            TypeDef::Struct(id) => id.name(db),
            TypeDef::Contract(id) => id.name(db),
            TypeDef::Primitive(typ) => typ.name(),
        }
    }

    pub fn name_span(&self, db: &dyn AnalyzerDb) -> Option<Span> {
        match self {
            TypeDef::Alias(id) => Some(id.name_span(db)),
            TypeDef::Struct(id) => Some(id.name_span(db)),
            TypeDef::Contract(id) => Some(id.name_span(db)),
            TypeDef::Primitive(_) => None,
        }
    }

    pub fn typ(&self, db: &dyn AnalyzerDb) -> Result<types::Type, TypeError> {
        match self {
            TypeDef::Alias(id) => id.typ(db),
            TypeDef::Struct(id) => Ok(types::Type::Struct(types::Struct {
                id: *id,
                name: id.name(db),
                field_count: id.fields(db).len(), // for the EvmSized trait
            })),
            TypeDef::Contract(id) => Ok(types::Type::Contract(types::Contract {
                id: *id,
                name: id.name(db),
            })),
            TypeDef::Primitive(base) => Ok(types::Type::Base(*base)),
        }
    }

    pub fn parent(&self, db: &dyn AnalyzerDb) -> Option<Item> {
        match self {
            TypeDef::Alias(id) => Some(id.parent(db)),
            TypeDef::Struct(id) => Some(id.parent(db)),
            TypeDef::Contract(id) => Some(id.parent(db)),
            TypeDef::Primitive(_) => None,
        }
    }

    pub fn sink_diagnostics(&self, db: &dyn AnalyzerDb, sink: &mut impl DiagnosticSink) {
        match self {
            TypeDef::Alias(id) => id.sink_diagnostics(db, sink),
            TypeDef::Struct(id) => id.sink_diagnostics(db, sink),
            TypeDef::Contract(id) => id.sink_diagnostics(db, sink),
            TypeDef::Primitive(_) => {}
        }
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct TypeAlias {
    pub ast: Node<ast::TypeAlias>,
    pub module: ModuleId,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Copy, Clone)]
pub struct TypeAliasId(pub(crate) u32);
impl_intern_key!(TypeAliasId);

impl TypeAliasId {
    pub fn data(&self, db: &dyn AnalyzerDb) -> Rc<TypeAlias> {
        db.lookup_intern_type_alias(*self)
    }
    pub fn span(&self, db: &dyn AnalyzerDb) -> Span {
        self.data(db).ast.span
    }
    pub fn name(&self, db: &dyn AnalyzerDb) -> SmolStr {
        self.data(db).ast.name().into()
    }
    pub fn name_span(&self, db: &dyn AnalyzerDb) -> Span {
        self.data(db).ast.kind.name.span
    }
    pub fn typ(&self, db: &dyn AnalyzerDb) -> Result<types::Type, TypeError> {
        db.type_alias_type(*self).value
    }
    pub fn parent(&self, db: &dyn AnalyzerDb) -> Item {
        Item::Module(self.data(db).module)
    }
    pub fn sink_diagnostics(&self, db: &dyn AnalyzerDb, sink: &mut impl DiagnosticSink) {
        db.type_alias_type(*self)
            .diagnostics
            .iter()
            .for_each(|d| sink.push(d))
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct Contract {
    pub name: SmolStr,
    pub ast: Node<ast::Contract>,
    pub module: ModuleId,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Copy, Clone)]
pub struct ContractId(pub(crate) u32);
impl_intern_key!(ContractId);
impl ContractId {
    pub fn data(&self, db: &dyn AnalyzerDb) -> Rc<Contract> {
        db.lookup_intern_contract(*self)
    }
    pub fn span(&self, db: &dyn AnalyzerDb) -> Span {
        self.data(db).ast.span
    }
    pub fn name(&self, db: &dyn AnalyzerDb) -> SmolStr {
        self.data(db).ast.name().into()
    }
    pub fn name_span(&self, db: &dyn AnalyzerDb) -> Span {
        self.data(db).ast.kind.name.span
    }

    pub fn module(&self, db: &dyn AnalyzerDb) -> ModuleId {
        self.data(db).module
    }

    pub fn fields(&self, db: &dyn AnalyzerDb) -> Rc<IndexMap<SmolStr, ContractFieldId>> {
        db.contract_field_map(*self).value
    }

    pub fn field_type(
        &self,
        db: &dyn AnalyzerDb,
        name: &str,
    ) -> Option<(Result<types::Type, TypeError>, usize)> {
        let fields = db.contract_field_map(*self).value;
        let (index, _, field) = fields.get_full(name)?;
        Some((field.typ(db), index))
    }

    pub fn resolve_name(
        &self,
        db: &dyn AnalyzerDb,
        name: &str,
    ) -> Result<Option<Item>, IncompleteItem> {
        if let Some(item) = self
            .function(db, name)
            .filter(|f| !f.takes_self(db))
            .map(Item::Function)
            .or_else(|| self.event(db, name).map(Item::Event))
        {
            Ok(Some(item))
        } else {
            self.module(db).resolve_name(db, name)
        }
    }

    pub fn init_function(&self, db: &dyn AnalyzerDb) -> Option<FunctionId> {
        db.contract_init_function(*self).value
    }

    pub fn call_function(&self, db: &dyn AnalyzerDb) -> Option<FunctionId> {
        db.contract_call_function(*self).value
    }

    /// User functions, public and not. Excludes `__init__` and `__call__`.
    pub fn functions(&self, db: &dyn AnalyzerDb) -> Rc<IndexMap<SmolStr, FunctionId>> {
        db.contract_function_map(*self).value
    }

    /// Lookup a function by name. Searches all user functions, private or not.
    /// Excludes `__init__` and `__call__`.
    pub fn function(&self, db: &dyn AnalyzerDb, name: &str) -> Option<FunctionId> {
        self.functions(db).get(name).copied()
    }

    /// Excludes `__init__` and `__call__`.
    pub fn public_functions(&self, db: &dyn AnalyzerDb) -> Rc<IndexMap<SmolStr, FunctionId>> {
        db.contract_public_function_map(*self)
    }

    /// Get a function that takes self by its name.
    pub fn self_function(&self, db: &dyn AnalyzerDb, name: &str) -> Option<FunctionId> {
        self.function(db, name).filter(|f| f.takes_self(db))
    }

    /// Lookup an event by name.
    pub fn event(&self, db: &dyn AnalyzerDb, name: &str) -> Option<EventId> {
        self.events(db).get(name).copied()
    }

    /// A map of events defined within the contract.
    pub fn events(&self, db: &dyn AnalyzerDb) -> Rc<IndexMap<SmolStr, EventId>> {
        db.contract_event_map(*self).value
    }

    pub fn parent(&self, db: &dyn AnalyzerDb) -> Item {
        Item::Module(self.data(db).module)
    }

    /// Dependency graph of the contract type, which consists of the field types
    /// and the dependencies of those types.
    ///
    /// NOTE: Contract items should *only*
    pub fn dependency_graph(&self, db: &dyn AnalyzerDb) -> Rc<DepGraph> {
        db.contract_dependency_graph(*self).0
    }

    /// Dependency graph of the (imaginary) `__call__` function, which
    /// dispatches to the contract's public functions.
    pub fn runtime_dependency_graph(&self, db: &dyn AnalyzerDb) -> Rc<DepGraph> {
        db.contract_runtime_dependency_graph(*self).0
    }

    pub fn sink_diagnostics(&self, db: &dyn AnalyzerDb, sink: &mut impl DiagnosticSink) {
        // fields
        db.contract_field_map(*self).sink_diagnostics(sink);
        db.contract_all_fields(*self)
            .iter()
            .for_each(|field| field.sink_diagnostics(db, sink));

        // events
        db.contract_event_map(*self).sink_diagnostics(sink);
        db.contract_all_events(*self)
            .iter()
            .for_each(|event| event.sink_diagnostics(db, sink));

        // functions
        db.contract_init_function(*self).sink_diagnostics(sink);
        db.contract_call_function(*self).sink_diagnostics(sink);
        db.contract_function_map(*self).sink_diagnostics(sink);
        db.contract_all_functions(*self)
            .iter()
            .for_each(|id| id.sink_diagnostics(db, sink));
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct ContractField {
    pub ast: Node<ast::Field>,
    pub parent: ContractId,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Copy, Clone)]
pub struct ContractFieldId(pub(crate) u32);
impl_intern_key!(ContractFieldId);
impl ContractFieldId {
    pub fn name(&self, db: &dyn AnalyzerDb) -> SmolStr {
        self.data(db).ast.name().into()
    }
    pub fn data(&self, db: &dyn AnalyzerDb) -> Rc<ContractField> {
        db.lookup_intern_contract_field(*self)
    }
    pub fn typ(&self, db: &dyn AnalyzerDb) -> Result<types::Type, TypeError> {
        db.contract_field_type(*self).value
    }
    pub fn sink_diagnostics(&self, db: &dyn AnalyzerDb, sink: &mut impl DiagnosticSink) {
        sink.push_all(db.contract_field_type(*self).diagnostics.iter())
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct Function {
    pub ast: Node<ast::Function>,
    pub module: ModuleId,
    pub parent: Option<Class>,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Copy, Clone)]
pub struct FunctionId(pub(crate) u32);
impl_intern_key!(FunctionId);
impl FunctionId {
    pub fn data(&self, db: &dyn AnalyzerDb) -> Rc<Function> {
        db.lookup_intern_function(*self)
    }
    pub fn span(&self, db: &dyn AnalyzerDb) -> Span {
        self.data(db).ast.span
    }
    pub fn name(&self, db: &dyn AnalyzerDb) -> SmolStr {
        self.data(db).ast.name().into()
    }
    pub fn name_span(&self, db: &dyn AnalyzerDb) -> Span {
        self.data(db).ast.kind.name.span
    }

    // This should probably be scrapped in favor of `parent()`
    pub fn class(&self, db: &dyn AnalyzerDb) -> Option<Class> {
        self.data(db).parent
    }
    pub fn parent(&self, db: &dyn AnalyzerDb) -> Item {
        let data = self.data(db);
        data.parent
            .map(|class| class.as_item())
            .unwrap_or(Item::Module(data.module))
    }

    pub fn module(&self, db: &dyn AnalyzerDb) -> ModuleId {
        self.data(db).module
    }

    pub fn takes_self(&self, db: &dyn AnalyzerDb) -> bool {
        self.signature(db).self_decl.is_some()
    }
    pub fn self_span(&self, db: &dyn AnalyzerDb) -> Option<Span> {
        if self.takes_self(db) {
            self.data(db)
                .ast
                .kind
                .args
                .iter()
                .find_map(|arg| matches!(arg.kind, ast::FunctionArg::Zelf).then(|| arg.span))
        } else {
            None
        }
    }

    pub fn is_public(&self, db: &dyn AnalyzerDb) -> bool {
        self.pub_span(db).is_some()
    }
    pub fn is_constructor(&self, db: &dyn AnalyzerDb) -> bool {
        self.name(db) == "__init__"
    }
    pub fn pub_span(&self, db: &dyn AnalyzerDb) -> Option<Span> {
        self.data(db).ast.kind.pub_
    }
    pub fn is_unsafe(&self, db: &dyn AnalyzerDb) -> bool {
        self.unsafe_span(db).is_some()
    }
    pub fn unsafe_span(&self, db: &dyn AnalyzerDb) -> Option<Span> {
        self.data(db).ast.kind.unsafe_
    }
    pub fn signature(&self, db: &dyn AnalyzerDb) -> Rc<types::FunctionSignature> {
        db.function_signature(*self).value
    }
    pub fn body(&self, db: &dyn AnalyzerDb) -> Rc<context::FunctionBody> {
        db.function_body(*self).value
    }
    pub fn dependency_graph(&self, db: &dyn AnalyzerDb) -> Rc<DepGraph> {
        db.function_dependency_graph(*self).0
    }
    pub fn sink_diagnostics(&self, db: &dyn AnalyzerDb, sink: &mut impl DiagnosticSink) {
        sink.push_all(db.function_signature(*self).diagnostics.iter());
        sink.push_all(db.function_body(*self).diagnostics.iter());
    }
}

/// A `Class` is an item that can have member functions.
#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy)]
pub enum Class {
    Contract(ContractId),
    Struct(StructId),
}
impl Class {
    pub fn function(&self, db: &dyn AnalyzerDb, name: &str) -> Option<FunctionId> {
        match self {
            Class::Contract(id) => id.function(db, name),
            Class::Struct(id) => id.function(db, name),
        }
    }
    pub fn self_function(&self, db: &dyn AnalyzerDb, name: &str) -> Option<FunctionId> {
        let fun = self.function(db, name)?;
        fun.takes_self(db).then(|| fun)
    }

    pub fn name(&self, db: &dyn AnalyzerDb) -> SmolStr {
        match self {
            Class::Contract(inner) => inner.name(db),
            Class::Struct(inner) => inner.name(db),
        }
    }
    pub fn kind(&self) -> &str {
        match self {
            Class::Contract(_) => "contract",
            Class::Struct(_) => "struct",
        }
    }
    pub fn as_item(&self) -> Item {
        match self {
            Class::Contract(id) => Item::Type(TypeDef::Contract(*id)),
            Class::Struct(id) => Item::Type(TypeDef::Struct(*id)),
        }
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy)]
pub enum MemberFunction {
    BuiltIn(builtins::ValueMethod),
    Function(FunctionId),
}

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct Struct {
    pub ast: Node<ast::Struct>,
    pub module: ModuleId,
}

#[derive(Default, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Copy, Clone)]
pub struct StructId(pub(crate) u32);
impl_intern_key!(StructId);
impl StructId {
    pub fn data(&self, db: &dyn AnalyzerDb) -> Rc<Struct> {
        db.lookup_intern_struct(*self)
    }
    pub fn span(&self, db: &dyn AnalyzerDb) -> Span {
        self.data(db).ast.span
    }
    pub fn name(&self, db: &dyn AnalyzerDb) -> SmolStr {
        self.data(db).ast.name().into()
    }
    pub fn name_span(&self, db: &dyn AnalyzerDb) -> Span {
        self.data(db).ast.kind.name.span
    }
    pub fn module(&self, db: &dyn AnalyzerDb) -> ModuleId {
        self.data(db).module
    }
    pub fn typ(&self, db: &dyn AnalyzerDb) -> Rc<types::Struct> {
        db.struct_type(*self)
    }

    pub fn has_private_field(&self, db: &dyn AnalyzerDb) -> bool {
        self.private_fields(db).iter().count() > 0
    }

    pub fn field(&self, db: &dyn AnalyzerDb, name: &str) -> Option<StructFieldId> {
        self.fields(db).get(name).copied()
    }
    pub fn field_type(
        &self,
        db: &dyn AnalyzerDb,
        name: &str,
    ) -> Option<Result<types::FixedSize, TypeError>> {
        Some(self.field(db, name)?.typ(db))
    }

    pub fn is_base_type(&self, db: &dyn AnalyzerDb, name: &str) -> bool {
        matches!(self.field_type(db, name), Some(Ok(FixedSize::Base(_))))
    }

    pub fn field_index(&self, db: &dyn AnalyzerDb, name: &str) -> Option<usize> {
        self.fields(db).get_index_of(name)
    }

    pub fn has_complex_fields(&self, db: &dyn AnalyzerDb) -> bool {
        self.fields(db)
            .iter()
            .any(|(name, _)| !self.is_base_type(db, name.as_str()))
    }

    pub fn fields(&self, db: &dyn AnalyzerDb) -> Rc<IndexMap<SmolStr, StructFieldId>> {
        db.struct_field_map(*self).value
    }
    pub fn functions(&self, db: &dyn AnalyzerDb) -> Rc<IndexMap<SmolStr, FunctionId>> {
        db.struct_function_map(*self).value
    }
    pub fn function(&self, db: &dyn AnalyzerDb, name: &str) -> Option<FunctionId> {
        self.functions(db).get(name).copied()
    }
    pub fn self_function(&self, db: &dyn AnalyzerDb, name: &str) -> Option<FunctionId> {
        self.function(db, name).filter(|f| f.takes_self(db))
    }
    pub fn parent(&self, db: &dyn AnalyzerDb) -> Item {
        Item::Module(self.data(db).module)
    }
    pub fn private_fields(&self, db: &dyn AnalyzerDb) -> Rc<IndexMap<SmolStr, StructFieldId>> {
        Rc::new(
            self.fields(db)
                .iter()
                .filter_map(|(name, field)| {
                    if field.is_public(db) {
                        None
                    } else {
                        Some((name.clone(), *field))
                    }
                })
                .collect(),
        )
    }
    pub fn dependency_graph(&self, db: &dyn AnalyzerDb) -> Rc<DepGraph> {
        db.struct_dependency_graph(*self).0
    }
    pub fn sink_diagnostics(&self, db: &dyn AnalyzerDb, sink: &mut impl DiagnosticSink) {
        sink.push_all(db.struct_field_map(*self).diagnostics.iter());
        db.struct_all_fields(*self)
            .iter()
            .for_each(|id| id.sink_diagnostics(db, sink));

        db.struct_all_functions(*self)
            .iter()
            .for_each(|id| id.sink_diagnostics(db, sink));
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct StructField {
    pub ast: Node<ast::Field>,
    pub parent: StructId,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Copy, Clone)]
pub struct StructFieldId(pub(crate) u32);
impl_intern_key!(StructFieldId);
impl StructFieldId {
    pub fn name(&self, db: &dyn AnalyzerDb) -> SmolStr {
        self.data(db).ast.name().into()
    }
    pub fn span(&self, db: &dyn AnalyzerDb) -> Span {
        self.data(db).ast.span
    }
    pub fn data(&self, db: &dyn AnalyzerDb) -> Rc<StructField> {
        db.lookup_intern_struct_field(*self)
    }
    pub fn typ(&self, db: &dyn AnalyzerDb) -> Result<types::FixedSize, TypeError> {
        db.struct_field_type(*self).value
    }
    pub fn is_base_type(&self, db: &dyn AnalyzerDb) -> bool {
        matches!(self.typ(db), Ok(FixedSize::Base(_)))
    }
    pub fn is_public(&self, db: &dyn AnalyzerDb) -> bool {
        self.data(db).ast.kind.is_pub
    }

    pub fn sink_diagnostics(&self, db: &dyn AnalyzerDb, sink: &mut impl DiagnosticSink) {
        db.struct_field_type(*self).sink_diagnostics(sink)
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct Event {
    pub ast: Node<ast::Event>,
    pub module: ModuleId,
    pub contract: Option<ContractId>,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Copy, Clone)]
pub struct EventId(pub(crate) u32);
impl_intern_key!(EventId);

impl EventId {
    pub fn name(&self, db: &dyn AnalyzerDb) -> SmolStr {
        self.data(db).ast.name().into()
    }
    pub fn name_span(&self, db: &dyn AnalyzerDb) -> Span {
        self.data(db).ast.kind.name.span
    }
    pub fn data(&self, db: &dyn AnalyzerDb) -> Rc<Event> {
        db.lookup_intern_event(*self)
    }
    pub fn typ(&self, db: &dyn AnalyzerDb) -> Rc<types::Event> {
        db.event_type(*self).value
    }
    pub fn module(&self, db: &dyn AnalyzerDb) -> ModuleId {
        self.data(db).module
    }
    pub fn parent(&self, db: &dyn AnalyzerDb) -> Item {
        if let Some(contract_id) = self.data(db).contract {
            Item::Type(TypeDef::Contract(contract_id))
        } else {
            Item::Module(self.module(db))
        }
    }
    pub fn sink_diagnostics(&self, db: &dyn AnalyzerDb, sink: &mut impl DiagnosticSink) {
        sink.push_all(db.event_type(*self).diagnostics.iter());
    }
}

pub trait DiagnosticSink {
    fn push(&mut self, diag: &Diagnostic);
    fn push_all<'a>(&mut self, iter: impl Iterator<Item = &'a Diagnostic>) {
        iter.for_each(|diag| self.push(diag))
    }
}

impl DiagnosticSink for Vec<Diagnostic> {
    fn push(&mut self, diag: &Diagnostic) {
        self.push(diag.clone())
    }
    fn push_all<'a>(&mut self, iter: impl Iterator<Item = &'a Diagnostic>) {
        self.extend(iter.cloned())
    }
}

pub type DepGraph = petgraph::graphmap::DiGraphMap<Item, DepLocality>;
#[derive(Debug, Clone)]
pub struct DepGraphWrapper(pub Rc<DepGraph>);
impl PartialEq for DepGraphWrapper {
    fn eq(&self, other: &DepGraphWrapper) -> bool {
        self.0.all_edges().eq(other.0.all_edges()) && self.0.nodes().eq(other.0.nodes())
    }
}
impl Eq for DepGraphWrapper {}

/// [`DepGraph`] edge label. "Locality" refers to the deployed state;
/// `Local` dependencies are those that will be compiled together, while
/// `External` dependencies will only be reachable via an evm CALL* op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepLocality {
    Local,
    External,
}

pub fn walk_local_dependencies<F>(graph: &DepGraph, root: Item, mut fun: F)
where
    F: FnMut(Item),
{
    use petgraph::visit::{Bfs, EdgeFiltered};

    let mut bfs = Bfs::new(
        &EdgeFiltered::from_fn(graph, |(_, _, loc)| *loc == DepLocality::Local),
        root,
    );
    while let Some(node) = bfs.next(&graph) {
        fun(node)
    }
}
