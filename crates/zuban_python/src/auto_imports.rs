use std::{
    collections::hash_map::{DefaultHasher, Entry},
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
};

use config::ProjectOptions;
use parsa_python_cst::{
    CodeIndex, DottedImportName, DottedImportNameContent, LevelWithDottedName, Name,
    NameImportParent, Scope,
};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use utils::FastHashMap;
use vfs::{
    AbsPath, Directory, DirectoryEntry, Entries, FileEntry, NormalizedPath, Parent, PathWithScheme,
    WorkspaceKind,
};

use crate::{
    CodeAction, Project, RunCause,
    file::{
        File as _, FileImport, StarImportResolutionKind, dotted_path_from_dir,
        is_private_import_and_not_in_dunder_all,
    },
    imports::ImportResult,
    prelude::*,
    utils::is_file_with_python_ending,
};

const FILE_LOAD_LIMIT: usize = 2000;

pub(crate) struct ImportFinder<'db> {
    db: &'db Database,
    name: &'db str,
    found: Mutex<Vec<PotentialImport<'db>>>,
    files_loaded: AtomicUsize,
}

#[derive(Clone, Copy)]
pub(crate) struct PotentialImport<'db> {
    file: &'db PythonFile,
    needs_additional_name: bool,
}

impl PotentialImport<'_> {
    pub(crate) fn title(&self, db: &Database, name: &str) -> String {
        let (dot, rest) = if self.needs_additional_name {
            (".", name)
        } else {
            ("", "")
        };
        format!("Import `{}{dot}{rest}`", self.file.qualified_name(db))
    }
}

impl<'db> ImportFinder<'db> {
    pub(crate) fn find_importable_name(
        db: &'db Database,
        name: &'db str,
    ) -> Vec<PotentialImport<'db>> {
        let slf = ImportFinder {
            db,
            name,
            found: Default::default(),
            files_loaded: Default::default(),
        };
        for workspace in db.vfs.workspaces.load().iter() {
            match &workspace.kind {
                WorkspaceKind::TypeChecking => {
                    slf.find_importable_name_in_entries(&workspace.entries, false, true, 0)
                }
                WorkspaceKind::SitePackages => {
                    slf.find_importable_name_in_entries(&workspace.entries, false, false, 0)
                }
                WorkspaceKind::PythonStdLib => (), // Already added as part of typeshed
                WorkspaceKind::Typeshed => {
                    let symbols = TypeshedSymbols::cached(db);
                    let mut found = slf.found.lock().unwrap();
                    let mut try_to_add = |typeshed_file: &TypeshedFile, needs_additional_name| {
                        let path = db
                            .vfs
                            .handler
                            .normalize_unchecked_abs_path(&typeshed_file.path);
                        if let Some(file_index) =
                            db.file_by_file_path(&PathWithScheme::with_file_scheme(path))
                        {
                            found.push(PotentialImport {
                                file: db.loaded_python_file(file_index),
                                needs_additional_name,
                            });
                        }
                    };
                    for typeshed_file in symbols.lookup(name) {
                        try_to_add(typeshed_file, true)
                    }
                    if let Some(typeshed_file) = symbols.lookup_top_level_file(name) {
                        try_to_add(typeshed_file, false)
                    }
                }
                // These are not reachable via normal sys path and we should therefore not add this
                // to the auto imports
                WorkspaceKind::Fallback => (),
            };
        }
        if slf.has_reached_limits() {
            tracing::warn!(
                "The auto-import limit of {FILE_LOAD_LIMIT} loaded files has been reached"
            );
        }
        slf.found.into_inner().unwrap()
    }

    fn find_importable_name_in_entries(
        &self,
        entries: &Entries,
        in_package: bool,
        add_submodules: bool,
        non_py_dir_depth: usize,
    ) {
        if self.has_reached_limits() {
            return;
        }
        if in_package {
            if let Some(entry) = entries
                .search("__init__.pyi")
                .filter(|x| !matches!(&**x, DirectoryEntry::MissingEntry(_)))
                .or_else(|| entries.search("__init__.py"))
                && let DirectoryEntry::File(__init__) = &*entry
            {
                // We have to make sure to drop the entry otherwise star import finding can cause
                // deadlocks, because the entries are modified there (missing entries are added).
                let __init__ = __init__.clone();
                drop(entry);

                if self.find_importable_name_in_file_entry(&__init__, !add_submodules) {
                    // If we find a name in __init__.py, we should probably not be looking up the other
                    // imports.
                    return;
                }
            }
            if !add_submodules {
                return;
            }
        }
        let entries: Vec<_> = entries
            .borrow()
            .iter()
            .filter_map(|(_, dir_entry)| match dir_entry {
                DirectoryEntry::File(_) | DirectoryEntry::Directory(_) => Some(dir_entry.clone()),
                _ => None,
            })
            .collect();
        let mut is_different_project = false;
        let has_python_files_in_dir = entries.iter().any(|e| match e {
            DirectoryEntry::File(f) => {
                if &*f.name == "pyproject.toml" {
                    is_different_project = true;
                }
                f.name.ends_with(".py") || f.name.ends_with(".pyi")
            }
            /*
             * Theoretically we would like to filter something like this, but it's currently
             * impossible, because .git is not part of the workspace, it gets filtered out.
            DirectoryEntry::Directory(dir) if &*dir.name == ".git" => {
                is_different_project = true;
                false
            }
            */
            _ => false,
        });
        if is_different_project && in_package {
            // We probably do not want to check a different project
            return;
        }
        entries.into_par_iter().for_each(|entry| match entry {
            DirectoryEntry::File(entry) => {
                // Only find importable files like foo.py that have importable file endings and
                // don't have symbols in there like dashes and spaces.
                // TODO there are a lot of other symbols that are invalid
                if is_file_with_python_ending(&entry.name)
                    && let Some(prefix) = Path::new(&*entry.name).file_prefix()
                    && might_be_python_identifier(prefix.to_str().unwrap())
                {
                    self.find_importable_name_in_file_entry(&entry, false);
                }
            }
            DirectoryEntry::Directory(dir) => {
                if might_be_python_identifier(&dir.name) {
                    // In cases where the project is equal to something like $HOME, which typically
                    // contains millions of files (sometimes multiplied by symlinks), we need to
                    // avoid following every directory. This also makes sense in a different way:
                    // If people don't have Python files in the directories the dirs are probably
                    // not something they want to import.
                    const MAX_NON_PY_DIR_AUTO_IMPORTS: usize = 2;
                    let new_non_py_dir_depth = if has_python_files_in_dir {
                        0
                    } else {
                        non_py_dir_depth + 1
                    };
                    if new_non_py_dir_depth < MAX_NON_PY_DIR_AUTO_IMPORTS {
                        self.find_importable_name_in_entries(
                            Directory::entries(&self.db.vfs, &dir),
                            true,
                            add_submodules,
                            new_non_py_dir_depth,
                        )
                    }
                }
            }
            _ => {
                unreachable!("Removed above")
            }
        })
    }

    fn has_reached_limits(&self) -> bool {
        self.files_loaded.load(Ordering::Relaxed) >= FILE_LOAD_LIMIT
    }

    fn find_importable_name_in_file_entry(
        &self,
        entry: &Arc<FileEntry>,
        add_star_imports: bool,
    ) -> bool {
        if !reserve_file_load(&self.files_loaded) {
            return false;
        }
        let Some(file) = self.db.load_file_from_workspace(entry) else {
            return false;
        };
        if file.name(self.db) == self.name {
            self.found.lock().unwrap().push(PotentialImport {
                file,
                needs_additional_name: false,
            })
        }
        if let Some(symbol) = file.lookup_symbol(self.name) {
            if is_private_import_and_not_in_dunder_all(self.db, symbol, |imp| match imp {
                NameImportParent::ImportFromAsName(from_as_name) => from_as_name
                    .import_from()
                    .is_some_and(|import_from| match import_from.level_with_dotted_name() {
                        LevelWithDottedName {
                            level: 0,
                            names: Some(imp),
                            ..
                        } => {
                            let (_, is_package) = file.file_entry_and_is_package(self.db);
                            !(is_package || has_import_of_file(self.db, file, imp))
                        }
                        LevelWithDottedName { level: 1, .. } => false, // Imports from the same package are not private
                        // Levels bigger than two should not be public
                        _ => true,
                    }),
                NameImportParent::DottedAsName(_) => true,
            }) {
                return false;
            }
            self.found.lock().unwrap().push(PotentialImport {
                file,
                needs_additional_name: true,
            });
            return true;
        } else if add_star_imports {
            if file
                .name_resolution_for_types(&InferenceState::new(self.db, file))
                .lookup_from_star_import(self.name, StarImportResolutionKind::Global)
                .is_ok()
            {
                self.found.lock().unwrap().push(PotentialImport {
                    file,
                    needs_additional_name: true,
                });
                return true;
            }
        }
        false
    }
}

fn reserve_file_load(files_loaded: &AtomicUsize) -> bool {
    files_loaded.fetch_add(1, Ordering::Relaxed) < FILE_LOAD_LIMIT
}

fn all_recursive_typeshed_file_entries(
    db: &Database,
    entries: &Entries,
    include_private: bool,
) -> Vec<Arc<FileEntry>> {
    fn recurse(
        db: &Database,
        found: &mut Vec<Arc<FileEntry>>,
        entries: &Entries,
        include_private: bool,
    ) {
        entries.borrow().iter().for_each(|(_, entry)| {
            match entry {
                DirectoryEntry::File(entry) => {
                    // Underscored modules are private, while dunder modules are not like
                    // __init__.pyi
                    if !include_private
                        && entry.name.starts_with('_')
                        && !entry.name.starts_with("__")
                    {
                        return;
                    }
                    found.push(entry.clone())
                }
                DirectoryEntry::Directory(dir) => {
                    // Underscored packages are private
                    // There's a directory in stdlib called @tests
                    if !include_private && (dir.name.starts_with('_') || dir.name.starts_with('@'))
                    {
                        return;
                    }
                    recurse(db, found, Directory::entries(&db.vfs, dir), include_private)
                }
                _ => (),
            }
        })
    }
    let mut found = vec![];
    recurse(db, &mut found, entries, include_private);
    found
}

pub(crate) fn create_import_code_action<'db>(
    db: &'db Database,
    from_file: &'db PythonFile,
    potential: PotentialImport,
    title: String,
    name: Name,
) -> CodeAction<'db> {
    if potential.needs_additional_name {
        // Try to find an import that matches
        for imp in &from_file.all_imports {
            let is_reachable = || {
                imp.node_index < name.index() || matches!(name.parent_scope(), Scope::Function(_))
            };
            if imp.in_global_scope
                && is_reachable()
                && let Some(imp) = NodeRef::new(from_file, imp.node_index).maybe_import_from()
                && matches!(
                    from_file.import_from_first_part_without_loading_file(db, imp),
                    Some(ImportResult::File(i)) if i == potential.file.file_index
                )
            {
                let insertion = imp.insertion_point_for_new_name(name.as_code());
                let pos = from_file.byte_to_position_infos(db, insertion.insertion_code_index);
                return CodeAction {
                    title,
                    start_of_change: pos,
                    end_of_change: pos,
                    replacement: insertion.addition,
                };
            }
        }
    }

    let mut newlines_at_start = String::default();
    let pos = position_for_import(db, from_file, potential, &mut newlines_at_start);
    let additional_newline_needed = if matches!(
        from_file.tree.code().as_bytes().get(pos as usize),
        Some(b'\n' | b'\r')
    ) {
        ""
    } else {
        "\n"
    };
    let replacement = if potential.needs_additional_name {
        format!(
            "{newlines_at_start}from {} import {}\n{additional_newline_needed}",
            potential.file.qualified_name(db),
            name.as_code()
        )
    } else if let (_, Some(parent_dir)) = potential.file.name_and_parent_dir(db) {
        format!(
            "{newlines_at_start}from {} import {}\n{additional_newline_needed}",
            dotted_path_from_dir(&parent_dir),
            name.as_code()
        )
    } else {
        format!(
            "{newlines_at_start}import {}\n{additional_newline_needed}",
            potential.file.qualified_name(db)
        )
    };
    let pos = from_file.byte_to_position_infos(db, pos);
    CodeAction {
        title,
        start_of_change: pos,
        end_of_change: pos,
        replacement,
    }
}

fn position_for_import<'db>(
    db: &'db Database,
    from_file: &'db PythonFile,
    potential: PotentialImport,
    newlines_at_start: &mut String,
) -> CodeIndex {
    let end_of_imports = from_file.tree.initial_imports_end_code_index();
    let auto_import_kind = file_to_kind(db, potential.file);
    let mut previous_match = None;
    for imp in from_file.all_imports.iter() {
        let node_ref = NodeRef::new(from_file, imp.node_index);
        if node_ref.node_start_position() >= end_of_imports {
            break;
        }
        if let Some(kind) = imp.kind_for_auto_imports(db, from_file) {
            if kind > auto_import_kind {
                return if let Some((previous_kind, prev)) = previous_match {
                    if previous_kind < auto_import_kind {
                        newlines_at_start.push('\n');
                    }
                    prev
                } else {
                    node_ref.node_start_position()
                };
            }
            let newline_end_after_import = || {
                let end = node_ref.node_end_position();
                if let Some(newline_index) = from_file.tree.code()[end as usize..].find('\n') {
                    end + newline_index as CodeIndex + 1
                } else {
                    recoverable_error!("An import should always have a newline afterwards");
                    end
                }
            };
            previous_match = Some((kind, newline_end_after_import()));
        }
    }
    if let Some((kind, prev)) = previous_match {
        if kind < auto_import_kind {
            newlines_at_start.push('\n');
        }
        prev
    } else {
        end_of_imports
    }
}

#[derive(PartialOrd, PartialEq)]
enum ImportKind {
    StdLib,
    ThirdParty,
    Project,
}

fn file_to_kind(db: &Database, file: &PythonFile) -> ImportKind {
    match &file.file_entry(db).parent.workspace().kind {
        vfs::WorkspaceKind::TypeChecking | vfs::WorkspaceKind::Fallback => ImportKind::Project,
        vfs::WorkspaceKind::SitePackages => ImportKind::ThirdParty,
        vfs::WorkspaceKind::Typeshed | vfs::WorkspaceKind::PythonStdLib => ImportKind::StdLib,
    }
}

impl FileImport {
    fn kind_for_auto_imports(&self, db: &Database, from_file: &PythonFile) -> Option<ImportKind> {
        let check = |imp_result: Option<ImportResult>| match imp_result? {
            ImportResult::File(file_index) => {
                Some(file_to_kind(db, db.loaded_python_file(file_index)))
            }
            ImportResult::PyTypedMissing(_) | ImportResult::BinaryExtension => {
                Some(ImportKind::ThirdParty)
            }
            ImportResult::Namespace(_) => None,
        };
        let node_ref = NodeRef::new(from_file, self.node_index);
        if let Some(import_from) = node_ref.maybe_import_from() {
            check(from_file.import_from_first_part_without_loading_file(db, import_from))
        } else {
            // We just use the first file that can be loaded, because this is a heuristic anyway.
            for dotted in node_ref.expect_import_name().iter_dotted_as_names() {
                if let Some(result) = check(from_file.cache_dotted_as_name_import(db, dotted)) {
                    return Some(result);
                }
            }
            None
        }
    }
}

#[derive(Serialize, Deserialize, Default, Debug)]
struct TypeshedFile {
    // This should probably be an Arc<NormalizedPath>, but I'm not sure how to
    // deserialize that.
    path: String,
}

type TypeshedFileIndex = u32;

#[derive(Serialize, Deserialize, Default, Debug)]
pub(crate) struct TypeshedSymbols {
    files: Vec<TypeshedFile>,
    toplevel_import_names: FastHashMap<String, TypeshedFileIndex>,
    // The value is the vec of a file index in the typeshed symbols.
    // It's a linked list not a Vec, because most of the time there's only one definition for a
    // name.
    symbols_to_files: FastHashMap<String, SingleLinkedList<TypeshedFileIndex>>,
}

#[derive(Serialize, Deserialize)]
struct VersionedTypeshedSymbols {
    version: usize,
    typeshed_path: String,
    source_fingerprint: u64,
    symbols: TypeshedSymbols,
}

static TYPESHED_INDEXES: OnceLock<
    Mutex<FastHashMap<Arc<NormalizedPath>, Arc<OnceLock<Arc<TypeshedSymbols>>>>>,
> = OnceLock::new();

impl TypeshedSymbols {
    fn source_fingerprint(db: &Database) -> Option<u64> {
        let mut paths: Vec<_> = db
            .vfs
            .workspaces
            .load()
            .iter()
            .filter(|workspace| matches!(workspace.kind, WorkspaceKind::Typeshed))
            .flat_map(|workspace| all_recursive_typeshed_file_entries(db, &workspace.entries, true))
            .map(|entry| entry.absolute_path(&*db.vfs.handler).path().to_string())
            .collect();
        paths.sort_unstable();
        let file_hashes: Vec<_> = paths
            .par_iter()
            .map(|path| {
                let mut hasher = DefaultHasher::new();
                path.hash(&mut hasher);
                std::fs::read(path).ok()?.hash(&mut hasher);
                Some(hasher.finish())
            })
            .collect();
        let mut hasher = DefaultHasher::new();
        for file_hash in file_hashes {
            file_hash?.hash(&mut hasher);
        }
        Some(hasher.finish())
    }

    fn cache_file(cache_dir: &Path, typeshed_path: &NormalizedPath) -> PathBuf {
        let mut hasher = DefaultHasher::new();
        typeshed_path.hash(&mut hasher);
        cache_dir.join(format!("typeshed-{:016x}.cache", hasher.finish()))
    }

    fn cached(db: &Database) -> Arc<Self> {
        if db.typeshed_has_overlay {
            return db
                .typeshed_overlay_index
                .get_or_init(|| Arc::new(Self::generate_typeshed_symbols(db)))
                .clone();
        }
        const TYPESHED_CACHE_VERSION: usize = 2;
        let typeshed_path = db
            .project
            .settings
            .typeshed_path
            .clone()
            .unwrap_or_else(|| {
                db.vfs
                    .workspaces
                    .load()
                    .iter()
                    .find(|workspace| matches!(workspace.kind, WorkspaceKind::Typeshed))
                    .expect("There should always be a typeshed workspace kind")
                    .root_path
                    .clone()
            });
        let cell = {
            let mut cells = TYPESHED_INDEXES
                .get_or_init(Default::default)
                .lock()
                .unwrap();
            cells.entry(typeshed_path.clone()).or_default().clone()
        };
        cell.get_or_init(|| {
            let cache = || {
                let mut options = ProjectOptions::default();
                options.settings.typeshed_path = db.project.settings.typeshed_path.clone();
                let project = Project::without_watcher(options, RunCause::LanguageServer);
                Self::generate_typeshed_symbols(&project.db)
            };

            let cache_dir = dirs::cache_dir().map(|c| c.join("zuban"));
            if let Some(cache_dir) = cache_dir
                && std::fs::create_dir_all(&cache_dir).is_ok()
            {
                let file = Self::cache_file(&cache_dir, &typeshed_path);
                let source_fingerprint = Self::source_fingerprint(db);
                if let Some(cached) = load_cache::<VersionedTypeshedSymbols>(&file)
                    && cached.version == TYPESHED_CACHE_VERSION
                    && *cached.typeshed_path.as_str() == ***typeshed_path
                    && Some(cached.source_fingerprint) == source_fingerprint
                {
                    return Arc::new(cached.symbols);
                }
                let symbols = cache();
                if let Some(source_fingerprint) = source_fingerprint {
                    let result = VersionedTypeshedSymbols {
                        version: TYPESHED_CACHE_VERSION,
                        typeshed_path: typeshed_path.to_string(),
                        source_fingerprint,
                        symbols,
                    };
                    match utils::serialize_binary(&result) {
                        Ok(bytes) => {
                            if let Err(err) = std::fs::write(file, bytes) {
                                tracing::error!("Could not save typeshed.cache: {err:?}");
                            }
                        }
                        Err(err) => tracing::error!("Could not serialize typeshed.cache: {err:?}"),
                    };
                    return Arc::new(result.symbols);
                }
                return Arc::new(symbols);
            }
            Arc::new(cache())
        })
        .clone()
    }

    pub(crate) fn contains_path(db: &Database, path: &AbsPath) -> bool {
        db.vfs.workspaces.load().iter().any(|workspace| {
            matches!(workspace.kind, WorkspaceKind::Typeshed)
                && workspace.contains_file_path(
                    &*db.vfs.handler,
                    db.project.flags.case_sensitive,
                    path,
                )
        })
    }

    pub(crate) fn invalidate_path(db: &Database, path: &AbsPath) -> bool {
        if !Self::contains_path(db, path) {
            return false;
        }
        let workspaces = db.vfs.workspaces.load();
        let typeshed_path = db
            .project
            .settings
            .typeshed_path
            .clone()
            .unwrap_or_else(|| {
                workspaces
                    .iter()
                    .find(|workspace| matches!(workspace.kind, WorkspaceKind::Typeshed))
                    .unwrap()
                    .root_path
                    .clone()
            });
        if let Some(indexes) = TYPESHED_INDEXES.get() {
            indexes.lock().unwrap().remove(&typeshed_path);
        }
        if let Some(cache_dir) = dirs::cache_dir() {
            let file = Self::cache_file(&cache_dir.join("zuban"), &typeshed_path);
            if let Err(err) = std::fs::remove_file(file)
                && err.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!("Could not remove stale typeshed index: {err:?}");
            }
        }
        true
    }

    fn generate_typeshed_symbols(db: &Database) -> Self {
        let found: Mutex<Self> = Default::default();
        for workspace in db.vfs.workspaces.load().iter() {
            if matches!(&workspace.kind, WorkspaceKind::Typeshed) {
                all_recursive_typeshed_file_entries(db, &workspace.entries, false)
                    .par_iter()
                    .for_each(|entry| {
                        let file = db
                            .load_file_from_workspace(entry)
                            .expect("Expected there to be all typeshed files");

                        let mut found = found.lock().unwrap();
                        let index = found.files.len() as u32;
                        found.files.push(TypeshedFile {
                            path: (**file.file_path(db)).to_string(),
                        });
                        let insert_symbol = |found: &mut Self, name: &str| match found
                            .symbols_to_files
                            .entry(name.to_string())
                        {
                            Entry::Occupied(mut occupied) => occupied.get_mut().insert_last(index),
                            Entry::Vacant(vacant) => {
                                vacant.insert_entry(SingleLinkedList::new(index));
                            }
                        };
                        match &entry.parent {
                            Parent::Directory(dir) => {
                                let dir = dir.upgrade().unwrap();
                                if entry.name.as_ref() == "__init__.pyi" {
                                    Directory::entries(&db.vfs, &dir).borrow().iter().for_each(
                                        |(_, dir_entry)| {
                                            let name = dir_entry.name();
                                            if name != "__init__.pyi" {
                                                insert_symbol(
                                                    &mut found,
                                                    name.trim_end_matches(".pyi"),
                                                )
                                            }
                                        },
                                    )
                                }
                            }
                            Parent::Workspace(_) => {
                                let result = found
                                    .toplevel_import_names
                                    .insert(file.name(db).to_string(), index);
                                debug_assert!(result.is_none());
                            }
                        }
                        // Builtins are already reachable
                        if file.file_index == db.python_state.builtins().file_index
                            // For now disable typing_extensions, because it essentially contains
                            // the almost exact same items as typing.pyi
                            || entry.name.as_ref() == "typing_extensions.pyi"
                        {
                            return;
                        }
                        for (name, &node_index) in file.symbol_table.iter() {
                            if is_private_import_and_not_in_dunder_all(
                                db,
                                NodeRef::new(file, node_index),
                                |_| true,
                            ) {
                                continue;
                            }
                            insert_symbol(&mut found, name)
                        }
                    })
            }
        }
        found.into_inner().unwrap()
    }

    fn lookup(&self, name: &str) -> impl Iterator<Item = &'_ TypeshedFile> {
        self.symbols_to_files
            .get(name)
            .map(|lst| lst.iter().map(|&index| &self.files[index as usize]))
            .into_iter()
            .flatten()
    }

    fn lookup_top_level_file(&self, name: &str) -> Option<&'_ TypeshedFile> {
        let index = self.toplevel_import_names.get(name)?;
        Some(&self.files[*index as usize])
    }
}

fn load_cache<T: for<'a> Deserialize<'a>>(path: &Path) -> Option<T> {
    match std::fs::read(path) {
        //Some(result),
        Ok(bytes) => match utils::deserialize_binary(&bytes) {
            Ok(result) => Some(result),
            Err(err) => {
                tracing::warn!("Tried to deserialize the typeshed cache, but got: {err:?}");
                None
            }
        },
        Err(err) => {
            tracing::info!("Tried reading typeshed cache, got: {err:?}");
            None
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct SingleLinkedList<T> {
    value: T,
    next: Option<Box<Self>>,
}

impl<T> SingleLinkedList<T> {
    fn new(value: T) -> Self {
        Self { value, next: None }
    }

    fn insert_last(&mut self, value: T) {
        let mut current = self;
        while let Some(ref mut next) = current.next {
            current = next;
        }
        current.next = Some(Box::new(Self { value, next: None }));
    }

    fn iter(&self) -> SingleLinkedListIter<'_, T> {
        SingleLinkedListIter { next: Some(self) }
    }
}

pub struct SingleLinkedListIter<'a, T> {
    next: Option<&'a SingleLinkedList<T>>,
}

impl<'a, T> Iterator for SingleLinkedListIter<'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<Self::Item> {
        self.next.map(|node| {
            self.next = node.next.as_deref();
            &node.value
        })
    }
}

fn has_import_of_file(db: &Database, file: &PythonFile, dotted: DottedImportName) -> bool {
    if let DottedImportNameContent::DottedName(dotted_inner, _) = dotted.unpack()
        && has_import_of_file(db, file, dotted_inner)
    {
        return true;
    };
    if let Some(result) = file.cache_import_dotted_name(db, dotted, None) {
        match result {
            ImportResult::File(file_index) => file_index == file.file_index,
            ImportResult::Namespace(_) => false,
            ImportResult::PyTypedMissing(_) | ImportResult::BinaryExtension => false,
        }
    } else {
        false
    }
}

fn might_be_python_identifier(s: &str) -> bool {
    let mut chars = s.chars();

    // The first char needs to be part of [A-Za-z_]
    match chars.next() {
        Some('_') => {}
        Some(c) if c.is_alphabetic() => {}
        _ => return false,
    }

    // After that numbers are allowed as well
    chars.all(|c| c == '_' || c.is_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::{FILE_LOAD_LIMIT, TypeshedSymbols, might_be_python_identifier, reserve_file_load};
    use crate::{Project, RunCause};
    use config::ProjectOptions;
    use rayon::prelude::*;
    use std::{
        fs,
        path::PathBuf,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };
    use vfs::{LocalFS, PathWithScheme};

    #[test]
    fn typeshed_index_is_scoped_to_its_root() {
        let source = PathBuf::from(test_utils::typeshed_path().to_string());
        let other = test_utils::TestDir::new(false);
        for path in [
            "stdlib/VERSIONS",
            "stdlib/builtins.pyi",
            "stdlib/typing.pyi",
            "stdlib/_typeshed/__init__.pyi",
            "stdlib/types.pyi",
            "stdlib/abc.pyi",
            "stdlib/functools.pyi",
            "stdlib/enum.pyi",
            "stdlib/dataclasses.pyi",
            "stdlib/typing_extensions.pyi",
            "stdlib/collections/__init__.pyi",
            "stdlib/_collections_abc.pyi",
            "stdlib/warnings.pyi",
            "stubs/mypy-extensions/mypy_extensions.pyi",
        ] {
            let destination = PathBuf::from(other.path()).join(path);
            fs::create_dir_all(destination.parent().unwrap()).unwrap();
            fs::copy(source.join(path), destination).unwrap();
        }
        other.write_file(
            "stdlib/zuban_cache_other_root.pyi",
            "class OnlyInOtherTypeshedRoot: ...\n",
        );

        let mut options = ProjectOptions::default();
        options.settings.typeshed_path = Some(test_utils::typeshed_path());
        let first_project = Project::without_watcher(options, RunCause::LanguageServer);
        let first = TypeshedSymbols::cached(&first_project.db);
        assert!(first.lookup("OnlyInOtherTypeshedRoot").next().is_none());

        let mut options = ProjectOptions::default();
        let other_root = LocalFS::without_watcher().normalized_path_from_current_dir(other.path());
        options.settings.typeshed_path = Some(other_root.clone());
        let mut second_project = Project::without_watcher(options, RunCause::LanguageServer);
        let second = TypeshedSymbols::cached(&second_project.db);
        assert_eq!(second.lookup("OnlyInOtherTypeshedRoot").count(), 1);
        assert!(!Arc::ptr_eq(&first, &second));
        assert!(Arc::ptr_eq(
            &first,
            &TypeshedSymbols::cached(&first_project.db)
        ));

        let stub_path = PathBuf::from(other.path()).join("stdlib/zuban_cache_other_root.pyi");
        let overlay_path = PathWithScheme::with_file_scheme(
            second_project
                .vfs_handler()
                .normalize_unchecked_abs_path(stub_path.to_str().unwrap()),
        );
        second_project.store_in_memory_file(
            overlay_path.clone(),
            "class OnlyInMemoryTypeshedRoot: ...\n".into(),
        );
        let overlay = TypeshedSymbols::cached(&second_project.db);
        assert_eq!(overlay.lookup("OnlyInMemoryTypeshedRoot").count(), 1);
        assert!(overlay.lookup("OnlyInOtherTypeshedRoot").next().is_none());
        assert!(!Arc::ptr_eq(&second, &overlay));
        let mut options = ProjectOptions::default();
        options.settings.typeshed_path = Some(other_root.clone());
        let peer_project = Project::without_watcher(options, RunCause::LanguageServer);
        assert!(Arc::ptr_eq(
            &second,
            &TypeshedSymbols::cached(&peer_project.db)
        ));
        second_project.store_in_memory_file(
            overlay_path.clone(),
            "class UpdatedInMemoryTypeshedRoot: ...\n".into(),
        );
        let updated_overlay = TypeshedSymbols::cached(&second_project.db);
        assert!(
            updated_overlay
                .lookup("OnlyInMemoryTypeshedRoot")
                .next()
                .is_none()
        );
        assert_eq!(
            updated_overlay
                .lookup("UpdatedInMemoryTypeshedRoot")
                .count(),
            1
        );
        second_project.close_in_memory_file(&overlay_path).unwrap();
        let disk_after_close = TypeshedSymbols::cached(&second_project.db);
        assert_eq!(
            disk_after_close.lookup("OnlyInOtherTypeshedRoot").count(),
            1
        );
        assert!(
            disk_after_close
                .lookup("OnlyInMemoryTypeshedRoot")
                .next()
                .is_none()
        );

        let unrelated_path = second_project
            .vfs_handler()
            .unchecked_abs_path(&format!("{}/unrelated.py", other.path()));
        second_project.invalidate_path(&unrelated_path);
        assert!(Arc::ptr_eq(
            &disk_after_close,
            &TypeshedSymbols::cached(&second_project.db)
        ));

        other.write_file(
            "stdlib/zuban_cache_other_root.pyi",
            "class UpdatedInOtherTypeshedRoot: ...\n",
        );
        let changed_path = PathBuf::from(other.path()).join("stdlib/zuban_cache_other_root.pyi");
        let changed_path = second_project
            .vfs_handler()
            .unchecked_abs_path(changed_path.to_str().unwrap());
        second_project.invalidate_path(&changed_path);
        let updated = TypeshedSymbols::cached(&second_project.db);
        assert!(!Arc::ptr_eq(&second, &updated));
        assert!(updated.lookup("OnlyInOtherTypeshedRoot").next().is_none());
        assert_eq!(updated.lookup("UpdatedInOtherTypeshedRoot").count(), 1);
        assert!(Arc::ptr_eq(
            &first,
            &TypeshedSymbols::cached(&first_project.db)
        ));

        other.write_file(
            "stdlib/zuban_cache_other_root.pyi",
            "class ChangedWhileZubanWasStopped: ...\n",
        );
        other.write_file(
            "stdlib/zuban_new_while_stopped.pyi",
            "class AddedWhileZubanWasStopped: ...\n",
        );
        super::TYPESHED_INDEXES
            .get()
            .unwrap()
            .lock()
            .unwrap()
            .remove(&other_root);
        let mut options = ProjectOptions::default();
        options.settings.typeshed_path = Some(other_root.clone());
        let restarted_project = Project::without_watcher(options, RunCause::LanguageServer);
        let restarted = TypeshedSymbols::cached(&restarted_project.db);
        assert!(
            restarted
                .lookup("UpdatedInOtherTypeshedRoot")
                .next()
                .is_none()
        );
        assert_eq!(restarted.lookup("ChangedWhileZubanWasStopped").count(), 1);
        assert_eq!(restarted.lookup("AddedWhileZubanWasStopped").count(), 1);

        if let Some(cache_dir) = dirs::cache_dir() {
            let cache_file = TypeshedSymbols::cache_file(&cache_dir.join("zuban"), &other_root);
            if cache_file.exists() {
                fs::remove_file(cache_file).unwrap();
            }
        }
    }

    #[test]
    fn parallel_auto_import_scans_respect_file_load_limit() {
        let attempts = AtomicUsize::new(0);
        let reserved = (0..FILE_LOAD_LIMIT * 2)
            .into_par_iter()
            .filter(|_| reserve_file_load(&attempts))
            .count();
        assert_eq!(reserved, FILE_LOAD_LIMIT);
        assert!(attempts.load(Ordering::Relaxed) >= FILE_LOAD_LIMIT);
    }

    #[test]
    fn identifiers() {
        for s in ["a", "_", "_foo", "foo123", "foo_bar", "café", "变量", "é2"] {
            assert!(might_be_python_identifier(s), "{s:?}");
        }
        for s in ["", "123foo", "123", "foo-bar", "foo bar", "foo.bar", "-foo"] {
            assert!(!might_be_python_identifier(s), "{s:?}");
        }
    }
}
