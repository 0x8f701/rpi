use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use syn::parse::Parser as _;
use syn::punctuated::Punctuated;
use syn::visit::{self, Visit};
use syn::{
    Attribute, Expr, ExprCall, ExprLit, ExprMethodCall, ExprPath, ImplItemFn, Item, ItemFn,
    ItemImpl, ItemMod, Lit, Meta, Stmt, Token, Type, UseTree,
};

const RAW_SPAWN_ALLOWLIST: &[&str] = &[
    "pi_sandbox::launcher::probe_trusted_bwrap",
    "pi_sandbox::launcher::probe_sandbox_exec",
    "pi_sandbox::unix::fork_sandbox_child",
    "pi_sandbox::unix::exec_prepared_wrapper",
    "pi_sandbox::child::exec_child_target",
    "pi_sandbox::unix::launch_raw_host_pipe",
    "pi_sandbox::unix::launch_raw_host_pty",
    "pi_coding::process::backend::spawn_windows_danger_pipe",
    "pi_coding::process::backend::spawn_windows_danger_pty",
    "pi_coding::process::probe::run_trusted_tool_probe",
    "pi_coding::isolate::userns_mount_probe",
    "pi_coding::isolate::mount_kernel",
    "pi_coding::isolate::mount_fuse",
    "pi_coding::isolate::run_command",
    "pi_cli::clipboard::run_command",
    "pi_cli::interactive_commands::spawn_editor_on",
    "pi_cli::tui::open_external_editor",
    "pi_cli::tui::suspend_process",
    "pi_cli::doctor::git_version",
    "pi_cli::terminal_images::run_tmux_version_command",
    "pi_cli::self_update::extract_tar",
    "pi_cli::self_update::extract_zip",
    "pi_cli::self_update::smoke",
    "pi_cli::self_update::WindowsLockHandoff::acquire",
    "pi_cli::code_review::run_git_bounded_os_timeout",
    "pi_coding::process::capture::HermeticRepositoryExecutor::default_executor",
];

const FORBIDDEN_BROWSER_SWITCHES: &[&str] = &[
    concat!("--no-", "sandbox"),
    concat!("--disable-", "sandbox"),
    concat!("--disable-setuid-", "sandbox"),
    concat!("--disable-seccomp-filter-", "sandbox"),
    concat!("--disable-namespace-", "sandbox"),
    concat!("--single-", "process"),
    concat!("--no-", "zygote"),
    concat!("--remote-debugging-", "port"),
    concat!("--remote-allow-", "origins"),
];

const LEGACY_SYMBOLS: &[&str] = &[
    concat!("Sandbox", "Config"),
    concat!("build_", "command"),
    concat!("run_in_", "sandbox"),
    concat!("spawn_", "piped"),
    concat!("SANDBOX_", "SETUP_SCRIPT"),
    concat!("Sandbox", "ConfigFn"),
    concat!("spawn_", "browser"),
    concat!("fetch_json_", "list"),
    concat!("fetch_page_", "websocket"),
];

const AMBIENT_FS_MODULES: &[&str] = &[
    "crates/pi-coding/src/tools.rs",
    "crates/pi-coding/src/tools/paths.rs",
    "crates/pi-coding/src/tools/ast_edit.rs",
    "crates/pi-coding/src/tools/ast_grep.rs",
    "crates/pi-coding/src/tools/lsp.rs",
    "crates/pi-coding/src/tools/lsp_client.rs",
    "crates/pi-coding/src/tools/notebook.rs",
    "crates/pi-coding/src/tools/image.rs",
    "crates/pi-coding/src/tools/image_gen.rs",
    "crates/pi-coding/src/tools/browser.rs",
    "crates/pi-coding/src/tools/doc_convert.rs",
    "crates/pi-coding/src/tools/mime.rs",
    "crates/pi-coding/src/tools/mutation_queue.rs",
    "crates/pi-coding/src/tools/bash/mod.rs",
    "crates/pi-coding/src/tools/glob.rs",
    "crates/pi-coding/src/memory.rs",
    "crates/pi-coding/src/import.rs",
    "crates/pi-coding/src/resources.rs",
    "crates/pi-coding/src/selector.rs",
    "crates/pi-coding/src/session_store.rs",
    "crates/pi-coding/src/export/mod.rs",
    "crates/pi-coding/src/plugin.rs",
    "crates/pi-coding/src/packages.rs",
    "crates/pi-coding/src/auth.rs",
    "crates/pi-coding/src/llama.rs",
    "crates/pi-coding/src/workflow/store.rs",
    "crates/pi-coding/src/workflow/manager.rs",
    "crates/pi-coding/src/application/workflows.rs",
    "crates/pi-coding/src/workflow_worktree/mod.rs",
    "crates/pi-coding/src/workflow_worktree/catalog.rs",
    "crates/pi-coding/src/workflow_worktree/git.rs",
    "crates/pi-coding/src/workflow_worktree/overlay.rs",
    "crates/pi-coding/src/workflow_worktree/tests.rs",
    "crates/pi-coding/src/orchestration/definitions.rs",
    "crates/pi-coding/src/orchestration/persistence.rs",
    "crates/pi-coding/src/orchestration/runtime.rs",
    "crates/pi-cli/src/session_run.rs",
    "crates/pi-cli/src/session_run_blueprint.rs",
    "crates/pi-cli/src/file_args.rs",
    "crates/pi-cli/src/auth_commands.rs",
    "crates/pi-cli/src/modes/acp.rs",
    "crates/pi-cli/src/modes/rpc.rs",
];

#[derive(Clone)]
struct SourceFile {
    display: String,
    crate_name: String,
    module: Vec<String>,
    syntax: syn::File,
}

#[derive(Clone, Debug)]
struct Alias {
    target: Vec<String>,
    origin: Vec<String>,
}

#[derive(Clone, Debug, Default)]
struct Scope {
    aliases: BTreeMap<String, Alias>,
}

#[derive(Debug)]
struct Violation {
    file: String,
    symbol: String,
    construct: String,
}

impl Violation {
    fn render(&self) -> String {
        format!(
            "{}: symbol `{}`: {}",
            self.file, self.symbol, self.construct
        )
    }
}

#[derive(Default)]
struct GlobalAliases {
    aliases: BTreeMap<Vec<String>, Alias>,
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("pi-coding must be inside the workspace")
        .to_path_buf()
}

fn parse_source(path: &Path) -> syn::File {
    let source = fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("reading {}: {error}", path.display()));
    syn::parse_file(&source).unwrap_or_else(|error| panic!("parsing {}: {error}", path.display()))
}

fn production_sources() -> Vec<SourceFile> {
    let root = workspace_root();
    let mut sources = Vec::new();
    for (crate_dir, crate_name, root_files) in [
        ("crates/pi-sandbox", "pi_sandbox", &["lib.rs"][..]),
        ("crates/pi-coding", "pi_coding", &["lib.rs"][..]),
        ("crates/pi-cli", "pi_cli", &["lib.rs", "main.rs"][..]),
    ] {
        for root_file in root_files {
            let path = root.join(crate_dir).join("src").join(root_file);
            let mut visited = BTreeSet::new();
            discover_file(
                &root,
                &path,
                crate_name,
                Vec::new(),
                &mut visited,
                &mut sources,
            );
        }
    }
    sources.sort_by(|left, right| {
        left.crate_name
            .cmp(&right.crate_name)
            .then_with(|| left.module.cmp(&right.module))
            .then_with(|| left.display.cmp(&right.display))
    });
    sources
        .dedup_by(|left, right| left.crate_name == right.crate_name && left.module == right.module);
    sources
}

fn discover_file(
    root: &Path,
    path: &Path,
    crate_name: &str,
    module: Vec<String>,
    visited: &mut BTreeSet<PathBuf>,
    sources: &mut Vec<SourceFile>,
) {
    let path = path.to_path_buf();
    if !visited.insert(path.clone()) {
        return;
    }
    let syntax = parse_source(&path);
    discover_modules(
        root,
        &path,
        crate_name,
        &module,
        &syntax.items,
        visited,
        sources,
    );
    let display = path
        .strip_prefix(root)
        .unwrap_or(&path)
        .to_string_lossy()
        .replace('\\', "/");
    sources.push(SourceFile {
        display,
        crate_name: crate_name.to_owned(),
        module,
        syntax,
    });
}

fn discover_modules(
    root: &Path,
    containing_file: &Path,
    crate_name: &str,
    module: &[String],
    items: &[Item],
    visited: &mut BTreeSet<PathBuf>,
    sources: &mut Vec<SourceFile>,
) {
    for item in items {
        let Item::Mod(item_mod) = item else { continue };
        if is_test_only(&item_mod.attrs) {
            continue;
        }
        let mut child_module = module.to_vec();
        child_module.push(item_mod.ident.to_string());
        if let Some((_, items)) = &item_mod.content {
            discover_modules(
                root,
                containing_file,
                crate_name,
                &child_module,
                items,
                visited,
                sources,
            );
        } else if let Some(path) = module_file(containing_file, item_mod) {
            discover_file(root, &path, crate_name, child_module, visited, sources);
        } else if let Some(path) = known_cfg_module_file(root, crate_name, &child_module) {
            discover_file(root, &path, crate_name, child_module, visited, sources);
        } else if has_non_test_cfg(&item_mod.attrs) {
            continue;
        } else {
            panic!(
                "resolving production module {} from {}",
                item_mod.ident,
                containing_file.display()
            );
        }
    }
}

fn module_file(containing_file: &Path, item_mod: &ItemMod) -> Option<PathBuf> {
    if let Some(explicit) = path_attribute(&item_mod.attrs) {
        return Some(containing_file.parent()?.join(explicit));
    }
    let parent = containing_file.parent()?;
    let stem = containing_file.file_stem()?.to_str()?;
    let base = if matches!(stem, "lib" | "main" | "mod") {
        parent.to_path_buf()
    } else {
        parent.join(stem)
    };
    let name = item_mod.ident.to_string();
    [
        base.join(format!("{name}.rs")),
        base.join(name).join("mod.rs"),
    ]
    .into_iter()
    .find(|candidate| candidate.is_file())
}

fn known_cfg_module_file(root: &Path, crate_name: &str, module: &[String]) -> Option<PathBuf> {
    let crate_dir = match crate_name {
        "pi_sandbox" => "crates/pi-sandbox",
        "pi_coding" => "crates/pi-coding",
        "pi_cli" => "crates/pi-cli",
        _ => return None,
    };
    let relative = module
        .iter()
        .fold(PathBuf::new(), |path, segment| path.join(segment));
    let src = root.join(crate_dir).join("src");
    [
        src.join(relative.with_extension("rs")),
        src.join(relative).join("mod.rs"),
    ]
    .into_iter()
    .find(|candidate| candidate.is_file())
}

fn path_attribute(attrs: &[Attribute]) -> Option<PathBuf> {
    attrs.iter().find_map(|attr| {
        if !attr.path().is_ident("path") {
            return None;
        }
        let Meta::NameValue(value) = &attr.meta else {
            return None;
        };
        let Expr::Lit(ExprLit {
            lit: Lit::Str(path),
            ..
        }) = &value.value
        else {
            return None;
        };
        Some(PathBuf::from(path.value()))
    })
}

fn is_test_only(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        attr.path().is_ident("cfg")
            && attr
                .parse_args::<Meta>()
                .is_ok_and(|predicate| predicate_requires_test(&predicate))
    })
}

fn has_non_test_cfg(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| attr.path().is_ident("cfg")) && !is_test_only(attrs)
}

fn predicate_requires_test(meta: &Meta) -> bool {
    match meta {
        Meta::Path(path) => path.is_ident("test"),
        Meta::List(list) if list.path.is_ident("all") => {
            parse_meta_list(list).is_some_and(|items| items.iter().any(predicate_requires_test))
        }
        Meta::List(list) if list.path.is_ident("any") => parse_meta_list(list)
            .is_some_and(|items| !items.is_empty() && items.iter().all(predicate_requires_test)),
        _ => false,
    }
}

fn parse_meta_list(list: &syn::MetaList) -> Option<Punctuated<Meta, Token![,]>> {
    Punctuated::<Meta, Token![,]>::parse_terminated
        .parse2(list.tokens.clone())
        .ok()
}

fn collect_global_aliases(sources: &[SourceFile]) -> GlobalAliases {
    let mut globals = GlobalAliases::default();
    for source in sources {
        collect_module_aliases(
            &source.crate_name,
            &source.module,
            &source.syntax.items,
            &mut globals,
        );
    }
    globals
}

fn collect_module_aliases(
    crate_name: &str,
    module: &[String],
    items: &[Item],
    globals: &mut GlobalAliases,
) {
    let mut absolute_module = vec![crate_name.to_owned()];
    absolute_module.extend_from_slice(module);
    let scope = scope_from_items(items, module);
    for (name, alias) in scope.aliases {
        let mut key = absolute_module.clone();
        key.push(name);
        globals.aliases.insert(key, alias);
    }
    for item in items {
        let Item::Mod(item_mod) = item else { continue };
        if is_test_only(&item_mod.attrs) {
            continue;
        }
        if let Some((_, nested)) = &item_mod.content {
            let mut nested_module = module.to_vec();
            nested_module.push(item_mod.ident.to_string());
            collect_module_aliases(crate_name, &nested_module, nested, globals);
        }
    }
}

fn scope_from_items(items: &[Item], origin: &[String]) -> Scope {
    let mut scope = Scope::default();
    for item in items {
        if is_test_only(item_attrs(item)) {
            continue;
        }
        match item {
            Item::Use(item_use) => {
                flatten_use(&item_use.tree, Vec::new(), origin, &mut scope.aliases)
            }
            Item::Type(item_type) => {
                if let Some(path) = type_path(&item_type.ty) {
                    scope.aliases.insert(
                        item_type.ident.to_string(),
                        Alias {
                            target: path,
                            origin: origin.to_vec(),
                        },
                    );
                }
            }
            _ => {}
        }
    }
    scope
}

fn scope_from_stmts(stmts: &[Stmt], origin: &[String]) -> Scope {
    let items = stmts
        .iter()
        .filter_map(|stmt| match stmt {
            Stmt::Item(item) => Some(item.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    scope_from_items(&items, origin)
}

fn item_attrs(item: &Item) -> &[Attribute] {
    match item {
        Item::Const(item) => &item.attrs,
        Item::Enum(item) => &item.attrs,
        Item::ExternCrate(item) => &item.attrs,
        Item::Fn(item) => &item.attrs,
        Item::ForeignMod(item) => &item.attrs,
        Item::Impl(item) => &item.attrs,
        Item::Macro(item) => &item.attrs,
        Item::Mod(item) => &item.attrs,
        Item::Static(item) => &item.attrs,
        Item::Struct(item) => &item.attrs,
        Item::Trait(item) => &item.attrs,
        Item::TraitAlias(item) => &item.attrs,
        Item::Type(item) => &item.attrs,
        Item::Union(item) => &item.attrs,
        Item::Use(item) => &item.attrs,
        Item::Verbatim(_) => &[],
        _ => &[],
    }
}

fn flatten_use(
    tree: &UseTree,
    prefix: Vec<String>,
    origin: &[String],
    aliases: &mut BTreeMap<String, Alias>,
) {
    match tree {
        UseTree::Path(path) => {
            let mut next = prefix;
            next.push(path.ident.to_string());
            flatten_use(&path.tree, next, origin, aliases);
        }
        UseTree::Name(name) => {
            let ident = name.ident.to_string();
            if ident == "self" {
                if let Some(binding) = prefix.last().cloned() {
                    aliases.insert(
                        binding,
                        Alias {
                            target: prefix,
                            origin: origin.to_vec(),
                        },
                    );
                }
            } else {
                let mut target = prefix;
                target.push(ident.clone());
                aliases.insert(
                    ident,
                    Alias {
                        target,
                        origin: origin.to_vec(),
                    },
                );
            }
        }
        UseTree::Rename(rename) => {
            let mut target = prefix;
            target.push(rename.ident.to_string());
            aliases.insert(
                rename.rename.to_string(),
                Alias {
                    target,
                    origin: origin.to_vec(),
                },
            );
        }
        UseTree::Glob(_) => {}
        UseTree::Group(group) => {
            for item in &group.items {
                flatten_use(item, prefix.clone(), origin, aliases);
            }
        }
    }
}

fn type_path(ty: &Type) -> Option<Vec<String>> {
    let Type::Path(path) = ty else { return None };
    if path.qself.is_some() {
        return None;
    }
    Some(
        path.path
            .segments
            .iter()
            .map(|segment| segment.ident.to_string())
            .collect(),
    )
}

fn expr_path(expr: &Expr) -> Option<Vec<String>> {
    let Expr::Path(ExprPath {
        qself: None, path, ..
    }) = expr
    else {
        return None;
    };
    Some(
        path.segments
            .iter()
            .map(|segment| segment.ident.to_string())
            .collect(),
    )
}

fn normalize_relative(path: &[String], origin: &[String], crate_name: &str) -> Vec<String> {
    let Some(first) = path.first().map(String::as_str) else {
        return Vec::new();
    };
    match first {
        "crate" => std::iter::once(crate_name.to_owned())
            .chain(path[1..].iter().cloned())
            .collect(),
        "self" => std::iter::once(crate_name.to_owned())
            .chain(origin.iter().cloned())
            .chain(path[1..].iter().cloned())
            .collect(),
        "super" => {
            let mut supers = 0;
            while path.get(supers).is_some_and(|part| part == "super") {
                supers += 1;
            }
            let keep = origin.len().saturating_sub(supers);
            std::iter::once(crate_name.to_owned())
                .chain(origin[..keep].iter().cloned())
                .chain(path[supers..].iter().cloned())
                .collect()
        }
        _ => path.to_vec(),
    }
}

fn resolve_path(
    path: &[String],
    crate_name: &str,
    module: &[String],
    scopes: &[Scope],
    globals: &GlobalAliases,
) -> Vec<String> {
    let mut resolved = path.to_vec();
    for _ in 0..16 {
        let before = resolved.clone();
        if let Some(first) = resolved.first().cloned() {
            if let Some(alias) = scopes
                .iter()
                .rev()
                .find_map(|scope| scope.aliases.get(&first))
            {
                let mut target = normalize_relative(&alias.target, &alias.origin, crate_name);
                target.extend_from_slice(&resolved[1..]);
                resolved = target;
            } else {
                let mut local_key = vec![crate_name.to_owned()];
                local_key.extend_from_slice(module);
                local_key.push(first);
                if let Some(alias) = globals.aliases.get(&local_key) {
                    let mut target = normalize_relative(&alias.target, &alias.origin, crate_name);
                    target.extend_from_slice(&resolved[1..]);
                    resolved = target;
                }
            }
        }
        resolved = normalize_relative(&resolved, module, crate_name);
        let mut expanded = false;
        for end in (1..=resolved.len()).rev() {
            if let Some(alias) = globals.aliases.get(&resolved[..end]) {
                let mut target = normalize_relative(&alias.target, &alias.origin, crate_name);
                target.extend_from_slice(&resolved[end..]);
                resolved = target;
                expanded = true;
                break;
            }
        }
        if resolved == before && !expanded {
            break;
        }
    }
    resolved
}

fn is_raw_unix_path(path: &[String]) -> bool {
    path.first().is_some_and(|part| part == "libc")
        && path.last().is_some_and(|name| {
            name == "fork"
                || name == "forkpty"
                || name.starts_with("posix_spawn")
                || name.starts_with("exec")
                || name == "fexecve"
        })
        || path.first().is_some_and(|part| part == "nix")
            && path
                .last()
                .is_some_and(|name| name == "fork" || name.starts_with("exec"))
}

struct GateVisitor<'a> {
    source: &'a SourceFile,
    globals: &'a GlobalAliases,
    module: Vec<String>,
    scopes: Vec<Scope>,
    enclosing: Vec<String>,
    impl_type: Option<String>,
    violations: Vec<Violation>,
    browser_pipe_switches: usize,
    saw_browser_builder: bool,
    check_repository_contracts: bool,
    check_cli_imports: bool,
}

impl<'a> GateVisitor<'a> {
    fn new(
        source: &'a SourceFile,
        globals: &'a GlobalAliases,
        check_repository_contracts: bool,
        check_cli_imports: bool,
    ) -> Self {
        Self {
            source,
            globals,
            module: source.module.clone(),
            scopes: vec![scope_from_items(&source.syntax.items, &source.module)],
            enclosing: Vec::new(),
            impl_type: None,
            violations: Vec::new(),
            browser_pipe_switches: 0,
            saw_browser_builder: false,
            check_repository_contracts,
            check_cli_imports,
        }
    }

    fn symbol(&self) -> String {
        let mut parts = vec![self.source.crate_name.clone()];
        parts.extend(self.module.iter().cloned());
        parts.extend(self.enclosing.iter().cloned());
        parts.join("::")
    }

    fn push_violation(&mut self, construct: impl Into<String>) {
        self.violations.push(Violation {
            file: self.source.display.clone(),
            symbol: self.symbol(),
            construct: construct.into(),
        });
    }

    fn resolved(&self, path: &[String]) -> Vec<String> {
        resolve_path(
            path,
            &self.source.crate_name,
            &self.module,
            &self.scopes,
            self.globals,
        )
    }

    fn raw_construct(&mut self, construct: &str, portable_pty: bool) {
        let symbol = self.symbol();
        let allowed = RAW_SPAWN_ALLOWLIST.contains(&symbol.as_str());
        let portable_allowed = symbol == "pi_coding::process::backend::spawn_windows_danger_pty";
        if !allowed || (portable_pty && !portable_allowed) {
            self.push_violation(format!("forbidden raw process construct `{construct}`"));
        }
    }

    fn classify_call(&mut self, call: &ExprCall) {
        let Some(path) = expr_path(&call.func) else {
            return;
        };
        let resolved = self.resolved(&path);
        let joined = resolved.join("::");
        let command = matches!(
            joined.as_str(),
            "std::process::Command::new" | "tokio::process::Command::new"
        );
        let portable = matches!(
            joined.as_str(),
            "portable_pty::CommandBuilder::new"
                | "portable_pty::CommandBuilder::from_argv"
                | "portable_pty::native_pty_system"
        );
        let raw_unix = is_raw_unix_path(&resolved);
        if command || portable || raw_unix {
            self.raw_construct(&joined, portable);
        }
        if !command && !portable && !raw_unix {
            for scope in self.scopes.iter().rev() {
                let Some(first) = path.first() else { break };
                let Some(alias) = scope.aliases.get(first) else {
                    continue;
                };
                let mut aliased =
                    normalize_relative(&alias.target, &alias.origin, &self.source.crate_name);
                aliased.extend_from_slice(&path[1..]);
                if is_raw_unix_path(&aliased) {
                    self.raw_construct(&format!("{} via alias", path.join("::")), false);
                    break;
                }
            }
        }
        if self.check_repository_contracts && self.is_ambient_fs_path(&resolved) {
            self.push_violation(format!(
                "ambient filesystem call `{joined}` in a migrated module"
            ));
        }
    }

    fn is_ambient_fs_path(&self, path: &[String]) -> bool {
        if !AMBIENT_FS_MODULES.contains(&self.source.display.as_str()) {
            return false;
        }
        let joined = path.join("::");
        let operation = path.last().map(String::as_str).unwrap_or_default();
        (joined.starts_with("std::fs::") || joined.starts_with("tokio::fs::"))
            && matches!(
                operation,
                "canonicalize"
                    | "copy"
                    | "create_dir"
                    | "create_dir_all"
                    | "hard_link"
                    | "metadata"
                    | "read"
                    | "read_dir"
                    | "read_link"
                    | "read_to_string"
                    | "remove_dir"
                    | "remove_dir_all"
                    | "remove_file"
                    | "rename"
                    | "set_permissions"
                    | "symlink_metadata"
                    | "write"
            )
    }

    fn check_pi_sandbox_reference(&mut self, path: &[String]) {
        if !self.check_cli_imports || self.source.crate_name != "pi_cli" {
            return;
        }
        let direct = path.first().is_some_and(|part| part == "pi_sandbox")
            || self
                .scopes
                .iter()
                .rev()
                .filter_map(|scope| path.first().and_then(|first| scope.aliases.get(first)))
                .map(|alias| {
                    normalize_relative(&alias.target, &alias.origin, &self.source.crate_name)
                })
                .any(|target| target.first().is_some_and(|part| part == "pi_sandbox"));
        if direct && self.symbol() != "pi_cli::sandbox_child_main" {
            self.push_violation(format!(
                "pi-cli imports or directly references `{}` outside hidden-child dispatch",
                path.join("::")
            ));
        }
    }

    fn enter_function(
        &mut self,
        name: String,
        block: &syn::Block,
        visit_block: impl FnOnce(&mut Self),
    ) {
        self.enclosing.push(name);
        self.scopes
            .push(scope_from_stmts(&block.stmts, &self.module));
        visit_block(self);
        self.scopes.pop();
        self.enclosing.pop();
    }

    fn in_browser_argv_builder(&self) -> bool {
        self.check_repository_contracts
            && self.source.display == "crates/pi-coding/src/tools/browser.rs"
            && self.symbol() == "pi_coding::tools::browser::build_browser_argv"
    }

    fn note_browser_literal(&mut self, value: &str) {
        if !self.in_browser_argv_builder() {
            return;
        }
        if FORBIDDEN_BROWSER_SWITCHES
            .iter()
            .any(|switch| value == *switch || value.starts_with(&format!("{switch}=")))
        {
            self.push_violation(format!("forbidden Chromium switch `{value}`"));
        }
        if value == "--remote-debugging-pipe" {
            self.browser_pipe_switches += 1;
        }
    }

    fn inspect_browser_macro(&mut self, mac: &syn::Macro) {
        if !self.in_browser_argv_builder() {
            return;
        }
        let Ok(exprs) = Punctuated::<Expr, Token![,]>::parse_terminated.parse2(mac.tokens.clone())
        else {
            return;
        };
        for expr in exprs {
            self.inspect_browser_expr(&expr);
        }
    }

    fn inspect_browser_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Lit(ExprLit {
                lit: Lit::Str(value),
                ..
            }) => {
                self.note_browser_literal(&value.value());
            }
            Expr::MethodCall(call) => {
                self.inspect_browser_expr(&call.receiver);
                for arg in &call.args {
                    self.inspect_browser_expr(arg);
                }
            }
            Expr::Call(call) => {
                self.inspect_browser_expr(&call.func);
                for arg in &call.args {
                    self.inspect_browser_expr(arg);
                }
            }
            Expr::Macro(mac) => self.inspect_browser_macro(&mac.mac),
            _ => {}
        }
    }
}

impl<'ast> Visit<'ast> for GateVisitor<'_> {
    fn visit_item_mod(&mut self, node: &'ast ItemMod) {
        if is_test_only(&node.attrs) {
            return;
        }
        if let Some((_, items)) = &node.content {
            self.module.push(node.ident.to_string());
            self.scopes.push(scope_from_items(items, &self.module));
            for item in items {
                self.visit_item(item);
            }
            self.scopes.pop();
            self.module.pop();
        }
    }

    fn visit_item_fn(&mut self, node: &'ast ItemFn) {
        if is_test_only(&node.attrs) {
            return;
        }
        let name = node.sig.ident.to_string();
        let symbol = {
            let mut parts = vec![self.source.crate_name.clone()];
            parts.extend(self.module.iter().cloned());
            parts.extend(self.enclosing.iter().cloned());
            parts.push(name.clone());
            parts.join("::")
        };
        if symbol == "pi_coding::tools::browser::build_browser_argv" {
            self.saw_browser_builder = true;
        }
        self.enter_function(name, &node.block, |this| {
            visit::visit_signature(this, &node.sig);
            visit::visit_block(this, &node.block);
        });
    }

    fn visit_item_impl(&mut self, node: &'ast ItemImpl) {
        if is_test_only(&node.attrs) {
            return;
        }
        let previous = self.impl_type.take();
        self.impl_type = type_path(&node.self_ty)
            .map(|path| self.resolved(&path))
            .and_then(|path| path.last().cloned());
        visit::visit_item_impl(self, node);
        self.impl_type = previous;
    }

    fn visit_impl_item_fn(&mut self, node: &'ast ImplItemFn) {
        if is_test_only(&node.attrs) {
            return;
        }
        let mut name = Vec::new();
        if let Some(impl_type) = &self.impl_type {
            name.push(impl_type.clone());
        }
        name.push(node.sig.ident.to_string());
        self.enter_function(name.join("::"), &node.block, |this| {
            visit::visit_signature(this, &node.sig);
            visit::visit_block(this, &node.block);
        });
    }

    fn visit_block(&mut self, node: &'ast syn::Block) {
        self.scopes
            .push(scope_from_stmts(&node.stmts, &self.module));
        visit::visit_block(self, node);
        self.scopes.pop();
    }

    fn visit_expr_call(&mut self, node: &'ast ExprCall) {
        self.classify_call(node);
        visit::visit_expr_call(self, node);
    }
    fn visit_expr_method_call(&mut self, node: &'ast ExprMethodCall) {
        let method = node.method.to_string();
        if method == "exec" {
            self.raw_construct("CommandExt::exec", false);
        } else if matches!(method.as_str(), "openpty" | "spawn_command")
            && (self.source.display == "crates/pi-coding/src/process/backend.rs"
                || self.source.display == "crates/pi-coding/src/tools/bash/pty.rs")
        {
            self.raw_construct(&format!("portable-pty::{method}"), true);
        }
        visit::visit_expr_method_call(self, node);
    }

    fn visit_expr_path(&mut self, node: &'ast ExprPath) {
        if node.qself.is_none() {
            let path = node
                .path
                .segments
                .iter()
                .map(|segment| segment.ident.to_string())
                .collect::<Vec<_>>();
            self.check_pi_sandbox_reference(&path);
        }
        visit::visit_expr_path(self, node);
    }

    fn visit_type_path(&mut self, node: &'ast syn::TypePath) {
        if node.qself.is_none() {
            let path = node
                .path
                .segments
                .iter()
                .map(|segment| segment.ident.to_string())
                .collect::<Vec<_>>();
            self.check_pi_sandbox_reference(&path);
        }
        visit::visit_type_path(self, node);
    }

    fn visit_item_use(&mut self, node: &'ast syn::ItemUse) {
        let mut aliases = BTreeMap::new();
        flatten_use(&node.tree, Vec::new(), &self.module, &mut aliases);
        for alias in aliases.values() {
            self.check_pi_sandbox_reference(&alias.target);
        }
        visit::visit_item_use(self, node);
    }

    fn visit_macro(&mut self, node: &'ast syn::Macro) {
        self.inspect_browser_macro(node);
        visit::visit_macro(self, node);
    }

    fn visit_lit_str(&mut self, node: &'ast syn::LitStr) {
        self.note_browser_literal(&node.value());
        visit::visit_lit_str(self, node);
    }

    fn visit_ident(&mut self, node: &'ast syn::Ident) {
        if self.check_repository_contracts && LEGACY_SYMBOLS.contains(&node.to_string().as_str()) {
            self.push_violation(format!("deleted legacy symbol `{node}` remains"));
        }
        visit::visit_ident(self, node);
    }
}

fn raw_gate_violations(sources: &[SourceFile], globals: &GlobalAliases) -> Vec<Violation> {
    let mut violations = Vec::new();
    let mut saw_browser_builder = false;
    let mut browser_pipe_switches = 0;
    for source in sources {
        let mut visitor = GateVisitor::new(source, globals, true, false);
        visitor.visit_file(&source.syntax);
        saw_browser_builder |= visitor.saw_browser_builder;
        browser_pipe_switches += visitor.browser_pipe_switches;
        violations.extend(visitor.violations);
    }
    if !saw_browser_builder || browser_pipe_switches != 1 {
        violations.push(Violation {
            file: "crates/pi-coding/src/tools/browser.rs".to_owned(),
            symbol: "pi_coding::tools::browser::build_browser_argv".to_owned(),
            construct: format!(
                "browser argv must contain exactly one `--remote-debugging-pipe` literal; found {browser_pipe_switches}"
            ),
        });
    }
    violations
}

fn fixture_violations(source: &str) -> Vec<Violation> {
    let syntax = syn::parse_file(source).expect("fixture must parse");
    let source = SourceFile {
        display: "fixture.rs".to_owned(),
        crate_name: "pi_sandbox".to_owned(),
        module: Vec::new(),
        syntax,
    };
    let sources = vec![source];
    let globals = collect_global_aliases(&sources);
    let mut visitor = GateVisitor::new(&sources[0], &globals, false, false);
    visitor.visit_file(&sources[0].syntax);
    visitor.violations
}

fn assert_no_violations(name: &str, violations: Vec<Violation>) {
    if violations.is_empty() {
        return;
    }
    panic!(
        "{name} failed with {} violation(s):\n{}",
        violations.len(),
        violations
            .iter()
            .map(Violation::render)
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn raw_spawn_symbols_are_exact() {
    let adjacent = fixture_violations(
        r#"
        mod launcher {
            use std::process::Command as ReviewedCommand;
            fn probe_trusted_bwrap() { let _ = ReviewedCommand::new("bwrap"); }
            fn probe_trusted_bwrap_adjacent() { let _ = ReviewedCommand::new("bwrap"); }
        }
        "#,
    );
    assert_eq!(
        adjacent.len(),
        1,
        "the exact allowlist symbol must pass but its adjacent symbol must fail"
    );
    assert_eq!(
        adjacent[0].symbol,
        "pi_sandbox::launcher::probe_trusted_bwrap_adjacent"
    );
    assert!(adjacent[0].construct.contains("std::process::Command::new"));

    let alias_adjacent = fixture_violations(
        r#"
        mod unix {
            type RawCommand = tokio::process::Command;
            fn launch_raw_host_pipe() { let _ = RawCommand::new("ok"); }
            fn launch_raw_host_pipe_neighbor() { let _ = RawCommand::new("no"); }
            #[cfg(test)]
            fn test_only_neighbor() { let _ = RawCommand::new("ignored"); }
        }
        "#,
    );
    assert_eq!(
        alias_adjacent.len(),
        1,
        "type aliases must resolve and cfg(test) must be excluded"
    );
    assert_eq!(
        alias_adjacent[0].symbol,
        "pi_sandbox::unix::launch_raw_host_pipe_neighbor"
    );

    let method_adjacent = fixture_violations(
        r#"
        mod unix {
            struct Raw;
            impl Raw {
                fn launch_raw_host_pty() { let _ = std::process::Command::new("ok"); }
                fn adjacent() { let _ = std::process::Command::new("no"); }
            }
        }
        "#,
    );
    assert_eq!(
        method_adjacent.len(),
        2,
        "methods are not covered by a neighboring free-function allowlist entry"
    );
    assert!(
        method_adjacent
            .iter()
            .any(|violation| violation.symbol == "pi_sandbox::unix::Raw::adjacent")
    );

    let sources = production_sources();
    let globals = collect_global_aliases(&sources);
    assert_no_violations(
        "raw_spawn_symbols_are_exact",
        raw_gate_violations(&sources, &globals),
    );
}

#[derive(Clone, Copy)]
enum ExactCfg<'a> {
    Unix,
    TargetOs(&'a str),
    NotTargetOs(&'a str),
}

fn cfg_matches(attrs: &[Attribute], expected: ExactCfg<'_>) -> bool {
    let predicates = attrs
        .iter()
        .filter(|attr| attr.path().is_ident("cfg"))
        .filter_map(|attr| attr.parse_args::<Meta>().ok())
        .collect::<Vec<_>>();
    predicates.len() == 1 && meta_matches(&predicates[0], expected)
}

fn meta_matches(meta: &Meta, expected: ExactCfg<'_>) -> bool {
    match expected {
        ExactCfg::Unix => matches!(meta, Meta::Path(path) if path.is_ident("unix")),
        ExactCfg::TargetOs(os) => matches!(meta, Meta::NameValue(value)
            if value.path.is_ident("target_os")
                && matches!(&value.value, Expr::Lit(ExprLit { lit: Lit::Str(lit), .. }) if lit.value() == os)),
        ExactCfg::NotTargetOs(os) => {
            let Meta::List(list) = meta else { return false };
            if !list.path.is_ident("not") {
                return false;
            }
            list.parse_args::<Meta>()
                .is_ok_and(|inner| meta_matches(&inner, ExactCfg::TargetOs(os)))
        }
    }
}

fn root_use_targets(item: &syn::ItemUse) -> Vec<String> {
    let mut aliases = BTreeMap::new();
    flatten_use(&item.tree, Vec::new(), &[], &mut aliases);
    aliases
        .into_values()
        .map(|alias| alias.target.join("::"))
        .collect()
}

#[test]
fn pi_sandbox_public_surface_is_cfg_exact() {
    let root = workspace_root();
    let lib_path = root.join("crates/pi-sandbox/src/lib.rs");
    let lib = parse_source(&lib_path);
    let spawn_path = root.join("crates/pi-sandbox/src/spawn.rs");
    let spawn = parse_source(&spawn_path);
    let mut violations = Vec::new();

    let expected_modules = [
        ("child", ExactCfg::Unix),
        ("launcher", ExactCfg::Unix),
        ("prepare", ExactCfg::Unix),
        ("unix", ExactCfg::Unix),
        ("bwrap", ExactCfg::TargetOs("linux")),
        ("seccomp", ExactCfg::TargetOs("linux")),
        ("seatbelt", ExactCfg::TargetOs("macos")),
    ];
    for (name, cfg) in expected_modules {
        let matches = lib
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Mod(item) if item.ident == name => Some(item),
                _ => None,
            })
            .collect::<Vec<_>>();
        if matches.len() != 1
            || !cfg_matches(&matches[0].attrs, cfg)
            || !matches!(matches[0].vis, syn::Visibility::Inherited)
        {
            violations.push(Violation {
                file: "crates/pi-sandbox/src/lib.rs".to_owned(),
                symbol: format!("pi_sandbox::{name}"),
                construct: "sealed platform module must have its exact private cfg".to_owned(),
            });
        }
    }

    let unix_exports = BTreeSet::from(
        [
            "child::exec_child_target",
            "prepare::PreparedSpawn",
            "prepare::prepare_spawn",
            "spawn::MountSource",
            "spawn::PipeStdio",
            "spawn::PreparedSpawnRequest",
            "spawn::RetainedExecutable",
            "spawn::RetainedRevisionRoots",
            "spawn::RetainedWorkingDirectory",
            "spawn::SpawnStdio",
            "spawn::TargetFd",
            "spawn::TargetFdMapping",
            "unix::LaunchedUnixHandle",
            "unix::LaunchedUnixProcess",
            "unix::LaunchedUnixPty",
            "unix::UnixExitStatus",
            "unix::UnixPtyMaster",
            "unix::launch_prepared_unix",
        ]
        .map(str::to_owned),
    );
    let all_target_spawn_exports = BTreeSet::from(
        [
            "spawn::CanonicalTargetFdMapping",
            "spawn::StdioMode",
            "spawn::TargetFdDirection",
            "spawn::TerminalSize",
        ]
        .map(str::to_owned),
    );
    let linux_exports = BTreeSet::from(
        [
            "seccomp::apply_current_thread_restrictions",
            "seccomp::install_seccomp_current_thread",
            "seccomp::set_no_new_privs_current_thread",
        ]
        .map(str::to_owned),
    );
    let mut seen_unix = BTreeSet::new();
    let mut seen_all_target = BTreeSet::new();
    let mut seen_linux = BTreeSet::new();
    for item in &lib.items {
        let Item::Use(item_use) = item else { continue };
        if !matches!(item_use.vis, syn::Visibility::Public(_)) {
            continue;
        }
        for target in root_use_targets(item_use) {
            if unix_exports.contains(target.as_str()) {
                seen_unix.insert(target.clone());
                if !cfg_matches(&item_use.attrs, ExactCfg::Unix) {
                    violations.push(Violation {
                        file: "crates/pi-sandbox/src/lib.rs".to_owned(),
                        symbol: format!("pi_sandbox::{target}"),
                        construct: "descriptor-owning re-export must be exactly cfg(unix)"
                            .to_owned(),
                    });
                }
            } else if all_target_spawn_exports.contains(target.as_str()) {
                seen_all_target.insert(target.clone());
                if item_use
                    .attrs
                    .iter()
                    .any(|attr| attr.path().is_ident("cfg"))
                {
                    violations.push(Violation {
                        file: "crates/pi-sandbox/src/lib.rs".to_owned(),
                        symbol: format!("pi_sandbox::{target}"),
                        construct: "all-target spawn metadata must not be cfg-gated".to_owned(),
                    });
                }
            } else if linux_exports.contains(target.as_str()) {
                seen_linux.insert(target.clone());
                if !cfg_matches(&item_use.attrs, ExactCfg::TargetOs("linux")) {
                    violations.push(Violation {
                        file: "crates/pi-sandbox/src/lib.rs".to_owned(),
                        symbol: format!("pi_sandbox::{target}"),
                        construct: "seccomp re-export must be exactly cfg(target_os = \"linux\")"
                            .to_owned(),
                    });
                }
            } else if target.starts_with("child::")
                || target.starts_with("prepare::")
                || target.starts_with("unix::")
                || target.starts_with("seccomp::")
                || target.starts_with("spawn::")
                    && !target.starts_with("spawn::CanonicalTargetFdMapping")
                    && !target.starts_with("spawn::StdioMode")
                    && !target.starts_with("spawn::TargetFdDirection")
                    && !target.starts_with("spawn::TerminalSize")
            {
                violations.push(Violation {
                    file: "crates/pi-sandbox/src/lib.rs".to_owned(),
                    symbol: format!("pi_sandbox::{target}"),
                    construct:
                        "unexpected public platform/spawn re-export widens the approved surface"
                            .to_owned(),
                });
            }
        }
    }
    for missing in unix_exports.difference(&seen_unix) {
        violations.push(Violation {
            file: "crates/pi-sandbox/src/lib.rs".to_owned(),
            symbol: format!("pi_sandbox::{missing}"),
            construct: "required cfg(unix) root re-export is missing".to_owned(),
        });
    }
    for missing in all_target_spawn_exports.difference(&seen_all_target) {
        violations.push(Violation {
            file: "crates/pi-sandbox/src/lib.rs".to_owned(),
            symbol: format!("pi_sandbox::{missing}"),
            construct: "required all-target metadata re-export is missing".to_owned(),
        });
    }
    for missing in linux_exports.difference(&seen_linux) {
        violations.push(Violation {
            file: "crates/pi-sandbox/src/lib.rs".to_owned(),
            symbol: format!("pi_sandbox::{missing}"),
            construct: "required Linux seccomp re-export is missing".to_owned(),
        });
    }

    for name in [
        "set_no_new_privs_current_thread",
        "install_seccomp_current_thread",
        "apply_current_thread_restrictions",
    ] {
        let functions = lib
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Fn(item) if item.sig.ident == name => Some(item),
                _ => None,
            })
            .collect::<Vec<_>>();
        if functions.len() != 1 || !cfg_matches(&functions[0].attrs, ExactCfg::NotTargetOs("linux"))
        {
            violations.push(Violation {
                file: "crates/pi-sandbox/src/lib.rs".to_owned(),
                symbol: format!("pi_sandbox::{name}"),
                construct: "non-Linux fallback must be exactly cfg(not(target_os = \"linux\"))"
                    .to_owned(),
            });
        }
    }

    let descriptor_types = BTreeSet::from([
        "MountSource",
        "PipeStdio",
        "PreparedSpawnRequest",
        "PreparedSpawnRequestParts",
        "RetainedExecutable",
        "RetainedWorkingDirectory",
        "SpawnStdio",
        "SpawnStdioParts",
        "TargetFd",
        "TargetFdMapping",
    ]);
    let all_target_types = BTreeSet::from([
        "TerminalSize",
        "StdioMode",
        "TargetFdDirection",
        "CanonicalTargetFdMapping",
    ]);
    for item in &spawn.items {
        let named = match item {
            Item::Struct(item) => Some((item.ident.to_string(), &item.attrs)),
            Item::Enum(item) => Some((item.ident.to_string(), &item.attrs)),
            Item::Impl(item) => type_path(&item.self_ty)
                .and_then(|path| path.last().cloned())
                .map(|name| (name, &item.attrs)),
            _ => None,
        };
        let Some((name, attrs)) = named else { continue };
        if descriptor_types.contains(name.as_str()) && !cfg_matches(attrs, ExactCfg::Unix) {
            violations.push(Violation {
                file: "crates/pi-sandbox/src/spawn.rs".to_owned(),
                symbol: format!("pi_sandbox::spawn::{name}"),
                construct: "descriptor-owning declaration/impl must be exactly cfg(unix)"
                    .to_owned(),
            });
        }
        if all_target_types.contains(name.as_str())
            && attrs.iter().any(|attr| attr.path().is_ident("cfg"))
        {
            violations.push(Violation {
                file: "crates/pi-sandbox/src/spawn.rs".to_owned(),
                symbol: format!("pi_sandbox::spawn::{name}"),
                construct: "all-target metadata declaration/impl must not be cfg-gated".to_owned(),
            });
        }
    }
    for item in &spawn.items {
        let Item::Use(item_use) = item else { continue };
        let targets = root_use_targets(item_use);
        if targets.iter().any(|target| {
            target.starts_with("std::collections::")
                || target.starts_with("std::ffi::")
                || target.starts_with("std::os::fd::")
                || target.starts_with("std::os::unix::")
        }) && !cfg_matches(&item_use.attrs, ExactCfg::Unix)
        {
            violations.push(Violation {
                file: "crates/pi-sandbox/src/spawn.rs".to_owned(),
                symbol: "pi_sandbox::spawn".to_owned(),
                construct: format!(
                    "Unix-owned import `{}` must be exactly cfg(unix)",
                    targets.join(", ")
                ),
            });
        }
    }

    assert_no_violations("pi_sandbox_public_surface_is_cfg_exact", violations);
}

#[test]
fn pi_cli_imports_pi_sandbox_only_for_hidden_child_dispatch() {
    let fixture = SourceFile {
        display: "fixture.rs".to_owned(),
        crate_name: "pi_cli".to_owned(),
        module: Vec::new(),
        syntax: syn::parse_file(
            r#"
            fn sandbox_child_main() { let _ = pi_sandbox::exec_child_target; }
            fn adjacent() { let _ = pi_sandbox::SandboxError::ProxyRoutedUnavailable; }
            "#,
        )
        .expect("cli fixture must parse"),
    };
    let fixture_sources = vec![fixture];
    let fixture_globals = collect_global_aliases(&fixture_sources);
    let mut fixture_visitor = GateVisitor::new(&fixture_sources[0], &fixture_globals, false, true);
    fixture_visitor.visit_file(&fixture_sources[0].syntax);
    assert_eq!(
        fixture_visitor.violations.len(),
        1,
        "hidden-child dispatch must be exact, not file-wide"
    );
    assert_eq!(fixture_visitor.violations[0].symbol, "pi_cli::adjacent");

    let sources = production_sources();
    let globals = collect_global_aliases(&sources);
    let mut violations = Vec::new();
    for source in sources
        .iter()
        .filter(|source| source.crate_name == "pi_cli")
    {
        let mut visitor = GateVisitor::new(source, &globals, false, true);
        visitor.visit_file(&source.syntax);
        violations.extend(
            visitor
                .violations
                .into_iter()
                .filter(|violation| violation.construct.starts_with("pi-cli imports")),
        );
    }
    assert_no_violations(
        "pi_cli_imports_pi_sandbox_only_for_hidden_child_dispatch",
        violations,
    );
}
