//! Comparison-only identities for models reached through local imports.
//!
//! Keep source paths intact: an accessible reexport can name a model whose
//! defining module is private. Unsupported or ambiguous bindings retain the
//! previous structural type equality rather than guessing an equivalence.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use syn::ext::IdentExt;

use super::{ModelInSource, SourceFile};

type ModulePath = Vec<String>;

// Missing names and unknown bindings differ: an unknown glob export must not
// be discarded in favor of another glob.
type Lookup = Result<Option<Definition>, ()>;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum ModelKey {
    Local(ModulePath),
    Unresolved(Box<syn::Type>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Definition {
    Module(ModulePath),
    Struct(ModulePath),
}

#[derive(Debug)]
enum Target {
    Definition(Definition),
    Import(ModulePath),
    Unknown,
}

#[derive(Debug)]
struct Binding {
    target: Target,
    // A private/restricted binding is visible in this module and descendants.
    // The empty prefix also represents crate-wide/public visibility.
    visible_from: Option<ModulePath>,
}

#[derive(Debug)]
struct Glob {
    path: Option<ModulePath>,
    visible_from: Option<ModulePath>,
}

#[derive(Debug, Default)]
struct Module {
    bindings: HashMap<String, Binding>,
    globs: Vec<Glob>,
    has_unexpanded_items: bool,
    conventional_macro_names: HashSet<String>,
}

impl Module {
    fn insert(&mut self, name: String, binding: Binding) {
        self.bindings
            .entry(name)
            .and_modify(|existing| {
                existing.target = Target::Unknown;
                // Duplicate/cfg-selected declarations may have different
                // visibility.
                existing.visible_from = Some(Vec::new());
            })
            .or_insert(binding);
    }
}

#[derive(Debug, Default)]
pub(super) struct ModelIdentities {
    modules: HashMap<ModulePath, Module>,
    models: HashSet<ModulePath>,
}

impl ModelIdentities {
    pub(super) fn from_files(files: &[SourceFile]) -> Self {
        let roots: Vec<_> = files
            .iter()
            .filter(|file| matches!(file.path.to_str(), Some("main.rs" | "lib.rs")))
            .collect();
        // File discovery does not distinguish separate Cargo targets. Do not
        // combine their namespaces or infer a root for synthetic SourceFiles.
        let [root] = roots.as_slice() else {
            return Self::default();
        };
        if conditional(&root.content.attrs) {
            return Self::default();
        }
        let mut result = Self::default();
        result.read_module(files, root, Vec::new());
        result.check_conventional_macro_names();
        result
    }

    fn check_conventional_macro_names(&mut self) {
        // A builtin-looking derive or conventional framework qualifier can be
        // shadowed through a glob. Trust its spelling only when the complete
        // local context proves that no such binding is visible. Barriers only
        // grow, so repeat to a fixed point independent of file/hash-map order.
        loop {
            let blocked = self
                .modules
                .iter()
                .filter_map(|(path, module)| {
                    if module.has_unexpanded_items {
                        return None;
                    }
                    let uncertain = module.conventional_macro_names.iter().any(|name| {
                        !matches!(
                            self.resolve_name(path, name, &mut HashSet::new(), None),
                            Ok(None)
                        )
                    });
                    uncertain.then(|| path.clone())
                })
                .collect::<Vec<_>>();
            if blocked.is_empty() {
                break;
            }
            for path in blocked {
                self.modules
                    .get_mut(&path)
                    .expect("module was just inspected")
                    .has_unexpanded_items = true;
            }
        }
    }

    pub(super) fn register_models(&mut self, models: &[ModelInSource]) {
        for model in models {
            if let Some(Definition::Struct(path)) = self.resolve_type(&model.model.resolved_ty) {
                self.models.insert(path);
            }
        }
    }

    pub(super) fn key(&self, ty: &syn::Type) -> ModelKey {
        if let Some(Definition::Struct(path)) = self.resolve_type(ty)
            && self.models.contains(&path)
        {
            ModelKey::Local(path)
        } else {
            ModelKey::Unresolved(Box::new(ty.clone()))
        }
    }

    fn resolve_type(&self, ty: &syn::Type) -> Option<Definition> {
        let syn::Type::Path(ty) = ty else {
            return None;
        };
        if ty.qself.is_some()
            || ty.path.leading_colon.is_some()
            || ty
                .path
                .segments
                .iter()
                .any(|part| !part.arguments.is_none())
        {
            return None;
        }
        let path = ty
            .path
            .segments
            .iter()
            .map(|part| part.ident.to_string())
            .collect::<Vec<_>>();
        self.resolve_path(&path, &mut HashSet::new()).ok().flatten()
    }

    fn resolve_path(
        &self,
        path: &[String],
        visiting: &mut HashSet<(ModulePath, String)>,
    ) -> Lookup {
        if path.first().map(String::as_str) != Some("crate") {
            return Err(());
        }
        let mut definition = Definition::Module(Vec::new());
        for name in &path[1..] {
            let Definition::Module(module) = definition else {
                return Err(());
            };
            let Some(next) = self.resolve_name(&module, name, visiting, None)? else {
                return Ok(None);
            };
            definition = next;
        }
        Ok(Some(definition))
    }

    fn resolve_name(
        &self,
        path: &ModulePath,
        name: &str,
        visiting: &mut HashSet<(ModulePath, String)>,
        importer: Option<&ModulePath>,
    ) -> Lookup {
        let query = (path.clone(), name.to_owned());
        if !visiting.insert(query.clone()) {
            return Err(());
        }
        let result = self.resolve_name_inner(path, name, visiting, importer);
        visiting.remove(&query);
        result
    }

    fn resolve_name_inner(
        &self,
        path: &ModulePath,
        name: &str,
        visiting: &mut HashSet<(ModulePath, String)>,
        importer: Option<&ModulePath>,
    ) -> Lookup {
        let module = self.modules.get(path).ok_or(())?;
        // Explicit names shadow globs even when we cannot resolve the name.
        if let Some(binding) = module.bindings.get(name) {
            if importer.is_some_and(|scope| !visible(binding.visible_from.as_ref(), scope)) {
                return Ok(None);
            }
            return match &binding.target {
                Target::Definition(definition) => Ok(Some(definition.clone())),
                Target::Import(import) => self.resolve_path(import, visiting)?.ok_or(()).map(Some),
                Target::Unknown => Err(()),
            };
        }
        // Item macros can introduce explicit imports that shadow a glob. We do
        // not expand them, so no equivalence inferred through globs is proven.
        if module.has_unexpanded_items {
            return Err(());
        }
        let mut found = None;
        for glob in &module.globs {
            if importer.is_some_and(|scope| !visible(glob.visible_from.as_ref(), scope)) {
                continue;
            }
            let Some(Definition::Module(source)) =
                self.resolve_path(glob.path.as_ref().ok_or(())?, visiting)?
            else {
                return Err(());
            };
            if let Some(definition) = self.resolve_name(&source, name, visiting, Some(path))? {
                if found
                    .as_ref()
                    .is_some_and(|previous| previous != &definition)
                {
                    return Err(());
                }
                found = Some(definition);
            }
        }
        Ok(found)
    }

    fn read_module(&mut self, files: &[SourceFile], file: &SourceFile, path: ModulePath) {
        let mut module = Module::default();
        let imported_names = explicit_import_names(&file.content);
        for item in &file.content.items {
            let attributes = item_attribute_effects(item, &imported_names);
            module.has_unexpanded_items |= attributes.introduces_items;
            module
                .conventional_macro_names
                .extend(attributes.conventional_macro_names.iter().cloned());
            match item {
                syn::Item::Mod(item) => {
                    let name = item.ident.to_string();
                    let mut child_path = path.clone();
                    child_path.push(name.clone());
                    let mut stem = PathBuf::new();
                    for part in &child_path {
                        stem.push(part);
                    }
                    let flat = stem.with_extension("rs");
                    let nested = stem.join("mod.rs");
                    let children = files
                        .iter()
                        .filter(|file| file.path == flat || file.path == nested)
                        .collect::<Vec<_>>();
                    let target = if item.content.is_none()
                        && !attributes.rewrites_item
                        && !conditional(&item.attrs)
                        && !item.attrs.iter().any(|attr| attr.path().is_ident("path"))
                        && let [child] = children.as_slice()
                        && !conditional(&child.content.attrs)
                    {
                        self.read_module(files, child, child_path.clone());
                        Target::Definition(Definition::Module(child_path))
                    } else {
                        Target::Unknown
                    };
                    module.insert(
                        name,
                        Binding {
                            target,
                            visible_from: visibility(&item.vis, &path),
                        },
                    );
                }
                syn::Item::Struct(item) => {
                    let name = item.ident.to_string();
                    let mut definition = path.clone();
                    definition.push(name.clone());
                    module.insert(
                        name,
                        Binding {
                            target: if conditional(&item.attrs) || attributes.rewrites_item {
                                Target::Unknown
                            } else {
                                Target::Definition(Definition::Struct(definition))
                            },
                            visible_from: visibility(&item.vis, &path),
                        },
                    );
                    if item.attrs.iter().any(super::is_model_attr) {
                        // Cot's model macro also defines this non-model helper.
                        module.insert(
                            format!("{}Fields", item.ident.unraw()),
                            Binding {
                                target: Target::Unknown,
                                visible_from: visibility(&item.vis, &path),
                            },
                        );
                    }
                }
                syn::Item::Use(item) => {
                    read_import(
                        &mut module,
                        &item.tree,
                        Vec::new(),
                        &path,
                        visibility(&item.vis, &path),
                        item.leading_colon.is_some()
                            || conditional(&item.attrs)
                            || attributes.rewrites_item,
                    );
                }
                // These occupy the type namespace too. Do not let a glob
                // incorrectly win over a local name we do not interpret.
                syn::Item::Enum(item) => block(&mut module, &item.ident, &item.vis, &path),
                syn::Item::Union(item) => block(&mut module, &item.ident, &item.vis, &path),
                syn::Item::Trait(item) => block(&mut module, &item.ident, &item.vis, &path),
                syn::Item::TraitAlias(item) => block(&mut module, &item.ident, &item.vis, &path),
                syn::Item::Type(item) => block(&mut module, &item.ident, &item.vis, &path),
                syn::Item::ExternCrate(item) => block(
                    &mut module,
                    item.rename.as_ref().map_or(&item.ident, |(_, name)| name),
                    &item.vis,
                    &path,
                ),
                syn::Item::Macro(_) => module.has_unexpanded_items = true,
                _ => {}
            }
        }
        self.modules.insert(path, module);
    }
}

fn explicit_import_names(file: &syn::File) -> HashSet<String> {
    let mut imports = Module::default();
    for item in &file.items {
        if let syn::Item::Use(item) = item {
            read_import(&mut imports, &item.tree, Vec::new(), &[], None, true);
        }
        if let syn::Item::Mod(item) = item {
            block(&mut imports, &item.ident, &item.vis, &[]);
        }
        if let syn::Item::ExternCrate(item) = item {
            block(
                &mut imports,
                item.rename.as_ref().map_or(&item.ident, |(_, alias)| alias),
                &item.vis,
                &[],
            );
        }
    }
    imports.bindings.into_keys().collect()
}

#[derive(Default)]
struct AttributeEffects {
    rewrites_item: bool,
    introduces_items: bool,
    conventional_macro_names: Vec<String>,
}

fn item_attribute_effects(item: &syn::Item, imported_names: &HashSet<String>) -> AttributeEffects {
    let attrs = match item {
        syn::Item::Const(item) => &item.attrs,
        syn::Item::Enum(item) => &item.attrs,
        syn::Item::ExternCrate(item) => &item.attrs,
        syn::Item::Fn(item) => &item.attrs,
        syn::Item::ForeignMod(item) => &item.attrs,
        syn::Item::Impl(item) => &item.attrs,
        syn::Item::Macro(item) => &item.attrs,
        syn::Item::Mod(item) => &item.attrs,
        syn::Item::Static(item) => &item.attrs,
        syn::Item::Struct(item) => &item.attrs,
        syn::Item::Trait(item) => &item.attrs,
        syn::Item::TraitAlias(item) => &item.attrs,
        syn::Item::Type(item) => &item.attrs,
        syn::Item::Union(item) => &item.attrs,
        syn::Item::Use(item) => &item.attrs,
        _ => return AttributeEffects::default(),
    };
    let mut effects = AttributeEffects::default();
    for attr in attrs {
        let path = attr.path();
        if [
            "cfg",
            "doc",
            "allow",
            "expect",
            "warn",
            "deny",
            "forbid",
            "repr",
            "deprecated",
            "must_use",
            "non_exhaustive",
            "inline",
            "cold",
            "track_caller",
            "automatically_derived",
        ]
        .iter()
        .any(|name| path.is_ident(name))
        {
            continue;
        }
        if matches!(item, syn::Item::Struct(_)) && super::is_model_attr(attr) {
            continue;
        }
        // These framework wrappers re-emit one function. Do not guess aliases
        // for their names or trust arbitrary procedural attributes.
        if matches!(item, syn::Item::Fn(_))
            && path.leading_colon.is_none()
            && path.segments.len() == 2
            && path.segments[1].ident == "main"
            && (path.segments[0].ident == "cot" || path.segments[0].ident == "tokio")
            && !imported_names.contains(&path.segments[0].ident.to_string())
        {
            effects
                .conventional_macro_names
                .push(path.segments[0].ident.to_string());
            continue;
        }
        if path.is_ident("derive") {
            let derives = attr.parse_args_with(
                syn::punctuated::Punctuated::<syn::Path, syn::Token![,]>::parse_terminated,
            );
            if let Ok(derives) = derives
                && derives.iter().all(|derive| {
                    [
                        "Debug",
                        "Clone",
                        "Copy",
                        "Default",
                        "Eq",
                        "PartialEq",
                        "Ord",
                        "PartialOrd",
                        "Hash",
                    ]
                    .iter()
                    .any(|name| derive.is_ident(name) && !imported_names.contains(*name))
                })
            {
                effects.conventional_macro_names.extend(
                    derives
                        .iter()
                        .map(|derive| derive.segments[0].ident.to_string()),
                );
            } else {
                effects.introduces_items = true;
            }
        } else {
            // Includes cfg_attr: its payload might select a procedural macro.
            effects.rewrites_item = true;
            effects.introduces_items = true;
        }
    }
    effects
}

fn conditional(attrs: &[syn::Attribute]) -> bool {
    attrs
        .iter()
        .any(|attr| attr.path().is_ident("cfg") || attr.path().is_ident("cfg_attr"))
}

fn visible(visibility: Option<&ModulePath>, scope: &[String]) -> bool {
    visibility.is_some_and(|prefix| scope.starts_with(prefix))
}

fn block(module: &mut Module, name: &syn::Ident, vis: &syn::Visibility, path: &[String]) {
    module.insert(
        name.to_string(),
        Binding {
            target: Target::Unknown,
            visible_from: visibility(vis, path),
        },
    );
}

fn absolute(module: &[String], path: &[String]) -> Option<ModulePath> {
    let mut result = vec!["crate".to_owned()];
    let mut remaining = path;
    match remaining.first().map(String::as_str) {
        Some("crate") => return Some(remaining.to_vec()),
        Some("self") => {
            result.extend_from_slice(module);
            remaining = &remaining[1..];
        }
        Some("super") => {
            result.extend_from_slice(module);
            while remaining.first().map(String::as_str) == Some("super") {
                if result.len() == 1 {
                    return None;
                }
                result.pop();
                remaining = &remaining[1..];
            }
        }
        _ => result.extend_from_slice(module),
    }
    result.extend_from_slice(remaining);
    Some(result)
}

fn visibility(vis: &syn::Visibility, module: &[String]) -> Option<ModulePath> {
    match vis {
        syn::Visibility::Public(_) => Some(Vec::new()),
        syn::Visibility::Inherited => Some(module.to_vec()),
        syn::Visibility::Restricted(vis) => {
            let parts = vis
                .path
                .segments
                .iter()
                .map(|part| part.ident.to_string())
                .collect::<Vec<_>>();
            let path = absolute(module, &parts)?;
            Some(path[1..].to_vec())
        }
    }
}

fn read_import(
    module: &mut Module,
    tree: &syn::UseTree,
    mut prefix: ModulePath,
    scope: &[String],
    visible_from: Option<ModulePath>,
    unsupported: bool,
) {
    match tree {
        syn::UseTree::Path(path) => {
            prefix.push(path.ident.to_string());
            read_import(module, &path.tree, prefix, scope, visible_from, unsupported);
        }
        syn::UseTree::Group(group) => {
            for item in &group.items {
                read_import(
                    module,
                    item,
                    prefix.clone(),
                    scope,
                    visible_from.clone(),
                    unsupported,
                );
            }
        }
        syn::UseTree::Glob(_) => module.globs.push(Glob {
            path: if unsupported {
                None
            } else {
                absolute(scope, &prefix)
            },
            visible_from,
        }),
        syn::UseTree::Name(name) => {
            let alias = if name.ident == "self" {
                prefix.last().cloned()
            } else {
                prefix.push(name.ident.to_string());
                Some(name.ident.to_string())
            };
            if let Some(alias) = alias {
                import_binding(module, alias, &prefix, scope, visible_from, unsupported);
            }
        }
        syn::UseTree::Rename(rename) => {
            if rename.ident != "self" {
                prefix.push(rename.ident.to_string());
            }
            if rename.rename != "_" {
                import_binding(
                    module,
                    rename.rename.to_string(),
                    &prefix,
                    scope,
                    visible_from,
                    unsupported,
                );
            }
        }
    }
}

fn import_binding(
    module: &mut Module,
    alias: String,
    prefix: &[String],
    scope: &[String],
    visible_from: Option<ModulePath>,
    unsupported: bool,
) {
    let target = if unsupported {
        Target::Unknown
    } else {
        absolute(scope, prefix).map_or(Target::Unknown, Target::Import)
    };
    module.insert(
        alias,
        Binding {
            target,
            visible_from,
        },
    );
}

#[cfg(test)]
mod tests {
    use syn::parse_quote;

    use super::*;
    use crate::migration_generator::{MigrationGenerator, MigrationGeneratorOptions};

    const MODEL: &str = "use cot::db::{Auto, model}; #[model] pub struct Target { #[model(primary_key)] pub id: Auto<i64> }";

    fn unresolved(ty: syn::Type) -> ModelKey {
        ModelKey::Unresolved(Box::new(ty))
    }

    fn context(sources: &[(&str, &str)]) -> ModelIdentities {
        let files = sources
            .iter()
            .map(|(path, source)| SourceFile::parse(PathBuf::from(path), source).unwrap())
            .collect::<Vec<_>>();
        let mut identities = ModelIdentities::from_files(&files);
        let generator = MigrationGenerator::new(
            PathBuf::from("Cargo.toml"),
            "test".into(),
            MigrationGeneratorOptions::default(),
        );
        let state = generator.process_source_files(files).unwrap();
        identities.register_models(&state.models);
        identities
    }

    #[test]
    fn private_root_glob_and_direct_path_identify_the_same_model() {
        let identities = context(&[
            ("main.rs", "mod models; use models::*;"),
            ("models/mod.rs", "pub mod tags;"),
            ("models/tags.rs", MODEL),
        ]);
        assert_eq!(
            identities.key(&parse_quote!(crate::tags::Target)),
            identities.key(&parse_quote!(crate::models::tags::Target))
        );
        assert!(matches!(
            identities.key(&parse_quote!(crate::tags::Target)),
            ModelKey::Local(_)
        ));
    }

    #[test]
    fn grouped_module_and_struct_reexports_preserve_identity() {
        let identities = context(&[
            (
                "main.rs",
                "mod models; use models::{self as facade, Target as Alias};",
            ),
            (
                "models/mod.rs",
                "mod implementation; pub use implementation::Target;",
            ),
            ("models/implementation.rs", MODEL),
        ]);
        let expected = identities.key(&parse_quote!(crate::models::implementation::Target));
        assert_eq!(
            identities.key(&parse_quote!(crate::facade::Target)),
            expected
        );
        assert_eq!(identities.key(&parse_quote!(crate::Alias)), expected);
    }

    #[test]
    fn explicit_binding_shadows_glob_without_collapsing_same_named_models() {
        let identities = context(&[
            (
                "main.rs",
                "mod first; mod second; use first::*; use second::Target;",
            ),
            ("first.rs", MODEL),
            ("second.rs", MODEL),
        ]);
        assert_eq!(
            identities.key(&parse_quote!(crate::Target)),
            identities.key(&parse_quote!(crate::second::Target))
        );
        assert_ne!(
            identities.key(&parse_quote!(crate::Target)),
            identities.key(&parse_quote!(crate::first::Target))
        );
    }

    #[test]
    fn multiple_paths_to_one_definition_are_not_ambiguous() {
        let identities = context(&[
            (
                "main.rs",
                "mod original; mod facade; use original::*; use facade::*;",
            ),
            ("original.rs", MODEL),
            ("facade.rs", "pub use crate::original::Target;"),
        ]);
        assert_eq!(
            identities.key(&parse_quote!(crate::Target)),
            identities.key(&parse_quote!(crate::original::Target))
        );
    }

    // These parsed inputs characterize conservative fallback, not valid-app
    // compilation. In particular ambiguous imports need not compile in Rust.
    #[test]
    fn ambiguous_glob_cannot_be_discarded_in_favor_of_another_glob() {
        let identities = context(&[
            (
                "main.rs",
                "mod first; mod second; mod ambiguous; use ambiguous::*; use first::*;",
            ),
            ("first.rs", MODEL),
            ("second.rs", MODEL),
            (
                "ambiguous.rs",
                "pub use crate::first::*; pub use crate::second::*;",
            ),
        ]);
        assert_eq!(
            identities.key(&parse_quote!(crate::Target)),
            unresolved(parse_quote!(crate::Target))
        );
    }

    #[test]
    fn unknown_visible_type_cannot_be_discarded_in_favor_of_a_glob() {
        let identities = context(&[
            (
                "main.rs",
                "mod first; mod other; use other::*; use first::*;",
            ),
            ("first.rs", MODEL),
            ("other.rs", "pub type Target = external::Model;"),
        ]);
        assert_eq!(
            identities.key(&parse_quote!(crate::Target)),
            unresolved(parse_quote!(crate::Target))
        );
    }

    #[test]
    fn unexpanded_macro_can_shadow_a_glob() {
        let identities = context(&[
            ("main.rs", "mod models; use models::*; make_import!();"),
            ("models/mod.rs", "pub mod tags;"),
            ("models/tags.rs", MODEL),
        ]);
        assert_eq!(
            identities.key(&parse_quote!(crate::tags::Target)),
            unresolved(parse_quote!(crate::tags::Target))
        );
        assert!(matches!(
            identities.key(&parse_quote!(crate::models::tags::Target)),
            ModelKey::Local(_)
        ));
    }

    #[test]
    fn private_child_import_is_not_reexported_by_a_glob() {
        let identities = context(&[
            ("main.rs", "mod first; mod facade; use facade::*;"),
            ("first.rs", MODEL),
            ("facade.rs", "use crate::first::*;"),
        ]);
        assert_eq!(
            identities.key(&parse_quote!(crate::Target)),
            unresolved(parse_quote!(crate::Target))
        );
    }

    #[test]
    fn restricted_visibility_and_relative_imports_are_respected() {
        let identities = context(&[
            ("main.rs", "mod models; use models::*;"),
            (
                "models/mod.rs",
                "mod inner; pub(crate) use self::inner::Target;",
            ),
            ("models/inner.rs", MODEL),
        ]);
        assert_eq!(
            identities.key(&parse_quote!(crate::Target)),
            identities.key(&parse_quote!(crate::models::inner::Target))
        );
    }

    #[test]
    fn cyclic_imports_terminate_and_external_types_remain_opaque() {
        let identities = context(&[
            ("main.rs", "mod model; use self::B as A; use self::A as B;"),
            ("model.rs", MODEL),
        ]);
        for ty in [
            parse_quote!(crate::A),
            parse_quote!(external::Target),
            parse_quote!(crate::missing::Target),
        ] {
            assert_eq!(identities.key(&ty), unresolved(ty));
        }
    }

    #[test]
    fn unsupported_modules_and_multiple_roots_do_not_create_equivalences() {
        for root in [
            "#[path = \"elsewhere.rs\"] mod model; use model::Target;",
            "mod model { } use model::Target;",
            "#[cfg(feature = \"x\")] mod model; use model::Target;",
        ] {
            let identities = context(&[("main.rs", root), ("model.rs", MODEL)]);
            assert_eq!(
                identities.key(&parse_quote!(crate::Target)),
                unresolved(parse_quote!(crate::Target))
            );
        }
        let identities = context(&[
            ("main.rs", "mod model; use model::Target;"),
            ("lib.rs", "mod other;"),
            ("model.rs", MODEL),
        ]);
        assert_eq!(
            identities.key(&parse_quote!(crate::Target)),
            unresolved(parse_quote!(crate::Target))
        );
    }
    #[test]
    fn absent_name_in_another_glob_does_not_hide_a_known_model() {
        let identities = context(&[
            (
                "main.rs",
                "mod first; mod empty; use empty::*; use first::*;",
            ),
            ("first.rs", MODEL),
            ("empty.rs", ""),
        ]);
        assert_eq!(
            identities.key(&parse_quote!(crate::Target)),
            identities.key(&parse_quote!(crate::first::Target))
        );
    }

    #[test]
    fn conditional_child_file_does_not_create_equivalences() {
        let source = format!("#![cfg(feature = \"x\")]\n{MODEL}");
        let identities = context(&[
            ("main.rs", "mod model; use model::Target;"),
            ("model.rs", &source),
        ]);
        assert_eq!(
            identities.key(&parse_quote!(crate::Target)),
            unresolved(parse_quote!(crate::Target))
        );
    }
    #[test]
    fn unknown_module_and_import_attributes_do_not_create_equivalences() {
        for root in [
            "#[custom] mod model; use model::Target;",
            "mod model; #[custom] use model::Target;",
        ] {
            let identities = context(&[("main.rs", root), ("model.rs", MODEL)]);
            assert_eq!(
                identities.key(&parse_quote!(crate::Target)),
                unresolved(parse_quote!(crate::Target))
            );
        }
    }

    #[test]
    fn unknown_transforming_struct_attribute_blocks_its_definition() {
        let model = format!("#[custom] {MODEL}");
        // Put the attribute on the struct, not on its preceding import.
        let model = model
            .replace("#[custom] use", "use")
            .replace("#[model]", "#[custom] #[model]");
        let identities = context(&[
            ("main.rs", "mod model; use model::*;"),
            ("model.rs", &model),
        ]);
        assert_eq!(
            identities.key(&parse_quote!(crate::model::Target)),
            unresolved(parse_quote!(crate::model::Target))
        );
    }

    #[test]
    fn custom_derive_and_function_attributes_block_glob_inference() {
        for extra in [
            "#[derive(Custom)] struct Other;",
            "#[custom] fn other() {}",
            "#[cfg_attr(feature = \"x\", custom)] fn other() {}",
        ] {
            let root = format!("mod model; use model::*; {extra}");
            let identities = context(&[("main.rs", &root), ("model.rs", MODEL)]);
            assert_eq!(
                identities.key(&parse_quote!(crate::Target)),
                unresolved(parse_quote!(crate::Target))
            );
        }
    }

    #[test]
    fn standard_derives_and_framework_main_wrappers_preserve_globs() {
        for wrapper in ["cot", "tokio"] {
            let root = format!(
                "mod model; use model::*; #[derive(Debug, Clone)] struct Other; #[cfg(feature = \"runtime\")] #[{wrapper}::main] async fn main() {{}}"
            );
            let identities = context(&[("main.rs", &root), ("model.rs", MODEL)]);
            assert_eq!(
                identities.key(&parse_quote!(crate::Target)),
                identities.key(&parse_quote!(crate::model::Target))
            );
        }
    }
    #[test]
    fn imported_derive_name_is_not_assumed_to_be_a_builtin() {
        let identities = context(&[
            (
                "main.rs",
                "mod model; use model::*; use custom::Debug; #[derive(Debug)] struct Other;",
            ),
            ("model.rs", MODEL),
        ]);
        assert_eq!(
            identities.key(&parse_quote!(crate::Target)),
            unresolved(parse_quote!(crate::Target))
        );
    }
    #[test]
    fn glob_imported_derive_name_is_not_assumed_to_be_a_builtin() {
        let identities = context(&[
            (
                "main.rs",
                "mod model; mod derives; use model::*; use derives::*; #[derive(Debug)] struct Other;",
            ),
            ("model.rs", MODEL),
            ("derives.rs", "pub use custom::Debug;"),
        ]);
        assert_eq!(
            identities.key(&parse_quote!(crate::Target)),
            unresolved(parse_quote!(crate::Target))
        );
    }

    #[test]
    fn glob_imported_framework_qualifier_is_not_assumed_to_be_external() {
        let identities = context(&[
            (
                "main.rs",
                "mod model; mod aliases; use model::*; use aliases::*; #[tokio::main] async fn main() {}",
            ),
            ("model.rs", MODEL),
            ("aliases.rs", "pub use custom as tokio;"),
        ]);
        assert_eq!(
            identities.key(&parse_quote!(crate::Target)),
            unresolved(parse_quote!(crate::Target))
        );
    }
    #[test]
    fn renamed_extern_crate_is_not_a_trusted_framework_wrapper() {
        for alias in ["cot", "tokio"] {
            let root = format!(
                "extern crate custom as {alias}; mod model; use model::*; #[{alias}::main] async fn main() {{}}"
            );
            let identities = context(&[("main.rs", &root), ("model.rs", MODEL)]);
            assert_eq!(
                identities.key(&parse_quote!(crate::Target)),
                unresolved(parse_quote!(crate::Target))
            );
        }
    }
}
