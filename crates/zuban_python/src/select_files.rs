use std::sync::{Arc, Mutex, RwLock};

use config::TypeCheckerFlags;
use rayon::prelude::*;
use utils::{FastHashMap, FastHashSet};
use vfs::{
    DirOrFile, Directory, DirectoryEntry, Entries, FileEntry, FileIndex, GitignoreFile,
    GlobAbsPath, LocalFS, PathWithScheme,
};

use crate::{
    diagnostics::Diagnostic,
    imports::ImportResult,
    prelude::*,
    utils::{is_file_with_python_ending, join_with_commas},
};

type ToBeLoaded = (usize, Arc<FileEntry>);
type OrderedFileIndexes = FastHashMap<FileIndex, usize>;

pub(crate) struct SelectionCache {
    epoch: u64,
    files: Vec<FileIndex>,
}

pub(crate) fn diagnostics_for_relevant_files<'db>(
    db: &'db Database,
    cache: &mut Option<SelectionCache>,
    on_file: impl FnMut(&'db PythonFile) -> Vec<Diagnostic<'db>>,
) -> anyhow::Result<Vec<Diagnostic<'db>>> {
    let epoch = db.vfs.structure_epoch();
    if let Some(cache) = cache.as_ref()
        && cache.epoch == epoch
        && let [index] = cache.files.as_slice()
        && let Some(file) = db.vfs.file(*index)
    {
        return Ok(diagnostics_for_selected_files(
            db,
            std::iter::once(file),
            on_file,
        ));
    }
    let mut files = cache
        .as_ref()
        .filter(|cache| cache.epoch == epoch)
        .and_then(|cache| db.vfs.loaded_files(&cache.files));
    if files.is_none() {
        let indexes = FileSelector::find_files(db)?
            .into_iter()
            .map(|item| item.file.file_index)
            .collect();
        *cache = Some(SelectionCache {
            epoch: db.vfs.structure_epoch(),
            files: indexes,
        });
        files = db.vfs.loaded_files(&cache.as_ref().unwrap().files);
    }
    // File-specific flags can change on a content edit without changing the
    // workspace structure, so cache candidates rather than filtered results.
    Ok(diagnostics_for_selected_files(
        db,
        files.expect("selected files must be loaded").into_iter(),
        on_file,
    ))
}

fn diagnostics_for_selected_files<'db>(
    db: &'db Database,
    files: impl Iterator<Item = &'db PythonFile>,
    mut on_file: impl FnMut(&'db PythonFile) -> Vec<Diagnostic<'db>>,
) -> Vec<Diagnostic<'db>> {
    files
        .filter(|file| {
            if let Some(flags) = file.maybe_more_specific_flags(db) {
                let path = file.file_entry(db).relative_path(&*db.vfs.handler);
                !should_skip_file(flags, &path)
            } else {
                true
            }
        })
        .map(&mut on_file)
        .reduce(|mut vec1, vec2| {
            vec1.extend(vec2);
            vec1
        })
        .unwrap_or_default()
}

pub(crate) fn all_typechecked_files(db: &Database) -> (OrderedFileIndexes, Vec<ToBeLoaded>) {
    let mut selector = FileSelector::new(db);
    selector.search_all_typechecked_files();
    (
        selector.file_indexes_to_order_map.into_inner().unwrap(),
        selector.to_be_loaded,
    )
}

fn should_skip_file(flags: &TypeCheckerFlags, rel_path: &str) -> bool {
    if !is_file_with_python_ending(rel_path) {
        return true;
    }
    should_skip_dir_or_file(flags, rel_path)
}

fn should_skip_dir_or_file(flags: &TypeCheckerFlags, rel_path: &str) -> bool {
    flags.excludes.iter().any(|e| e.regex.is_match(rel_path))
}

struct OrderableFile<'db> {
    order_index: usize,
    file: &'db PythonFile,
}

struct FileSelector<'db> {
    db: &'db Database,
    to_be_loaded: Vec<ToBeLoaded>,
    file_indexes_to_order_map: RwLock<OrderedFileIndexes>,
    added_file: bool,
    current_gitignores: Vec<Arc<GitignoreFile>>,
    order_counter: usize,
}

impl<'db> FileSelector<'db> {
    fn new(db: &'db Database) -> Self {
        Self {
            db,
            to_be_loaded: vec![],
            file_indexes_to_order_map: Default::default(),
            added_file: false,
            current_gitignores: vec![],
            order_counter: 0,
        }
    }

    fn find_files(db: &'db Database) -> anyhow::Result<Vec<OrderableFile<'db>>> {
        let mut selector = Self::new(db);
        selector.search_in_workspaces()?;
        let loaded_file_entries: Mutex<FastHashSet<ArcPtrWrapper>> = Mutex::new(
            selector
                .to_be_loaded
                .iter()
                .map(|l| ArcPtrWrapper(Arc::as_ptr(&l.1)))
                .collect(),
        );
        selector
            .to_be_loaded
            .par_iter()
            .for_each(|(order_index, file)| {
                if let Some(file) = db.load_file_from_workspace(file) {
                    selector.add_file_index(*order_index, file.file_index);
                    find_imports_and_preload_files(db, file, &loaded_file_entries)
                }
            });
        let mut vec: Vec<_> = selector
            .file_indexes_to_order_map
            .into_inner()
            .unwrap()
            .into_iter()
            .map(|(file_index, order_index)| OrderableFile {
                order_index,
                file: db.loaded_python_file(file_index),
            })
            .collect();
        vec.sort_by_key(|o| o.order_index);
        Ok(vec)
    }

    fn search_all_typechecked_files(&mut self) {
        for entries in self.db.vfs.workspaces.load().entries_to_type_check() {
            self.handle_entries(entries)
        }
    }

    fn search_in_workspaces(&mut self) -> anyhow::Result<()> {
        // In case there are no files provided we simply scan everything. This might not be
        // efficient in some cases, but people can easily just scan the parts they wish.
        let check_files = &self.db.project.settings.files_or_directories_to_check;
        let vfs_handler = &*self.db.vfs.handler;
        if check_files.is_empty() {
            self.search_all_typechecked_files();
            if !self.added_file {
                anyhow::bail!("No Python files found to check")
            }
        } else {
            // First try to remove the "obvious" parts. This is the much faster code path that does
            // not need to walk the whole tree and simply tries to search for the relevant files.
            // Most people will probably not provide a glob path, they provide an actual path to a
            // file or directory and will land here.
            let not_yet_checked_globs: Vec<&GlobAbsPath> = check_files
                .iter()
                .filter_map(|pattern| {
                    if let Some(path) = pattern.maybe_simple_path() {
                        self.added_file = false;
                        let normalized =
                            LocalFS::without_watcher().normalized_path_from_current_dir(path);
                        let p = PathWithScheme::with_file_scheme(normalized.clone());
                        match self
                            .db
                            .vfs
                            .search_path(self.db.project.flags.case_sensitive, &p)
                        {
                            Some(DirOrFile::Dir(dir)) => self.handle_dir(&dir),
                            Some(DirOrFile::File(file)) => self.add_file(file),
                            None => {
                                // First check the type checked workspaces. If we did not find
                                // something there, the user might explicitly want to typecheck
                                // part of the venv (2). In the case where no files are found,
                                // because the workspaces are subdirs or somewhere else we just
                                // fall back (3) to checking the provided files/directories, but
                                // that will typically lead to import errors.
                                for workspace in self.db.vfs.workspaces.load().iter() {
                                    if workspace.is_type_checked() {
                                        if workspace.root_path_starts_with(&normalized) {
                                            self.handle_entries(&workspace.entries)
                                        }
                                    }
                                }
                                // (2)
                                if !self.added_file {
                                    for workspace in self.db.vfs.workspaces.load().iter() {
                                        if workspace.root_path_starts_with(&normalized) {
                                            self.handle_entries(&workspace.entries)
                                        }
                                    }
                                }
                                // (3)
                                if !self.added_file {
                                    match self.db.vfs.ensure_fallback(p) {
                                        Some(DirectoryEntry::File(file)) => self.add_file(file),
                                        Some(DirectoryEntry::Directory(dir)) => {
                                            self.handle_dir(&dir)
                                        }
                                        _ => (),
                                    }
                                }
                            }
                        }
                        if !self.added_file {
                            Some(Err(anyhow::anyhow!(
                                "No Python files found to check for {path}"
                            )))
                        } else {
                            None
                        }
                    } else {
                        Some(Ok(pattern))
                    }
                })
                .collect::<anyhow::Result<Vec<_>>>()?;

            if !not_yet_checked_globs.is_empty() {
                self.added_file = false;
                for entries in self.db.vfs.workspaces.load().entries_to_type_check() {
                    entries.walk_entries(&self.db.vfs, &mut |in_dir, entry| {
                        let path = match entry {
                            DirectoryEntry::File(file) => file.absolute_path(vfs_handler),
                            DirectoryEntry::Directory(dir) => dir.absolute_path(vfs_handler),
                            _ => {
                                return false;
                            }
                        };
                        if not_yet_checked_globs
                            .iter()
                            .any(|glob| glob.matches(vfs_handler, path.path()))
                        {
                            self.handle_entry(in_dir, entry, true, None, &mut String::new());
                            false
                        } else {
                            true
                        }
                    });
                }
                if !self.added_file {
                    let paths = join_with_commas(not_yet_checked_globs.iter().map(|g| g.as_str()));
                    anyhow::bail!("No Python files found to check in {paths}");
                }
            }
        }
        Ok(())
    }

    fn add_file(&mut self, file: Arc<FileEntry>) {
        self.added_file = true;
        let order_index = self.order_counter;
        self.order_counter += 1;
        if let Some(file_index) = file.get_file_index() {
            self.file_indexes_to_order_map
                .get_mut()
                .unwrap()
                .entry(file_index)
                .or_insert(order_index);
        } else {
            self.to_be_loaded.push((order_index, file));
        }
    }

    fn add_file_index(&self, order_index: usize, file_index: FileIndex) {
        self.file_indexes_to_order_map
            .write()
            .unwrap()
            .entry(file_index)
            .and_modify(|existing| *existing = (*existing).min(order_index))
            .or_insert(order_index);
    }

    fn ignored_by_gitignore(&self, path: impl FnOnce() -> PathWithScheme, is_dir: bool) -> bool {
        if self.current_gitignores.is_empty() {
            return false;
        }
        let path = path();
        self.current_gitignores
            .iter()
            .any(|gitignore| gitignore.is_path_ignored(&path, is_dir))
    }

    fn handle_entry(
        &mut self,
        parent_entries: &Entries,
        entry: &DirectoryEntry,
        parent_has_stub: bool,
        borrowed_entries: Option<&FastHashMap<Arc<str>, DirectoryEntry>>,
        stub_name: &mut String,
    ) {
        let handler = &*self.db.vfs.handler;

        match entry {
            DirectoryEntry::File(file) => {
                let flags = &self.db.project.flags;
                if is_file_with_python_ending(&file.name)
                    && (flags.excludes.is_empty()
                        || !should_skip_dir_or_file(flags, &file.relative_path(handler)))
                    && (!parent_has_stub
                        || !ignore_py_if_overwritten_by_pyi(
                            parent_entries,
                            borrowed_entries,
                            file,
                            stub_name,
                        ))
                    && !self.ignored_by_gitignore(|| file.absolute_path(handler), false)
                {
                    self.add_file(file.clone())
                }
            }
            DirectoryEntry::Directory(dir) => {
                let flags = &self.db.project.flags;
                if (flags.excludes.is_empty()
                    || !should_skip_dir_or_file(flags, &dir.relative_path(handler)))
                    && !self.ignored_by_gitignore(|| dir.absolute_path(handler), true)
                    // Nested workspaces are handled by checking the other workspaces
                    && !dir.is_nested_workspace()
                {
                    self.handle_dir(dir)
                }
            }
            _ => (),
        }
    }

    fn handle_dir(&mut self, dir: &Arc<Directory>) {
        self.handle_entries(Directory::entries(&self.db.vfs, dir))
    }

    fn handle_entries(&mut self, entries: &Entries) {
        let mut need_to_drop_gitignore = false;
        if self.db.project.settings.exclude_gitignore
            && let Some(gitignore) = entries.borrow().iter().find_map(|(_, e)| match e {
                DirectoryEntry::Gitignore(gitignore) => Some(gitignore.clone()),
                _ => None,
            })
        {
            self.current_gitignores.push(gitignore);
            need_to_drop_gitignore = true
        }

        let borrowed = entries.borrow();
        let mut has_stub = false;
        let mut sorted_entries: Vec<_> = borrowed
            .iter()
            .filter_map(|(key, entry)| {
                if entry.is_missing() {
                    return None;
                }
                if let DirectoryEntry::File(file) = entry
                    && file.name.ends_with(".pyi")
                {
                    has_stub = true;
                }
                Some((key, entry))
            })
            .collect();
        // Sorting entries by name to have at least some form of determinism by sorting
        sorted_entries.sort_by_key(|(key, _)| *key);

        let mut stub_name = String::new();
        for (_, entry) in sorted_entries {
            self.handle_entry(entries, entry, has_stub, Some(&borrowed), &mut stub_name)
        }

        if need_to_drop_gitignore {
            self.current_gitignores.pop();
        }
    }
}

fn ignore_py_if_overwritten_by_pyi(
    parent_entries: &Entries,
    borrowed_entries: Option<&FastHashMap<Arc<str>, DirectoryEntry>>,
    file: &FileEntry,
    stub_name: &mut String,
) -> bool {
    if !file.name.ends_with(".py") {
        return false;
    }
    stub_name.clear();
    stub_name.push_str(&file.name);
    stub_name.push('i');
    if let Some(entries) = borrowed_entries {
        matches!(
            entries.get(stub_name.as_str()),
            Some(DirectoryEntry::File(_))
        )
    } else {
        parent_entries
            .search(stub_name)
            .is_some_and(|e| matches!(*e, DirectoryEntry::File(_)))
    }
}

#[derive(Hash, Eq, PartialEq)]
struct ArcPtrWrapper(*const FileEntry);
// We use this only for Pointer equality checks so it's fine
unsafe impl Sync for ArcPtrWrapper {}
unsafe impl Send for ArcPtrWrapper {}

fn find_imports_and_preload_files(
    db: &Database,
    file: &PythonFile,
    loaded_file_entries: &Mutex<FastHashSet<ArcPtrWrapper>>,
) {
    let mut need_to_load_files = FastHashSet::default();
    for imp in &file.all_imports {
        file.find_potential_import_for_import_node_index(
            db,
            imp.node_index,
            |on_file| match on_file {
                ImportResult::File(file_index) => {
                    let ptr = ArcPtrWrapper(Arc::as_ptr(db.vfs.file_entry(file_index)));
                    if loaded_file_entries.lock().unwrap().insert(ptr) {
                        need_to_load_files.insert(file_index);
                    }
                }
                ImportResult::Namespace(_) => (),
                ImportResult::PyTypedMissing(_) | ImportResult::BinaryExtension => (),
            },
        )
    }
    need_to_load_files.into_par_iter().for_each(|file_index| {
        if let Ok(_file) = db.ensure_file_for_file_index(file_index) {
            // TODO should we preload all files? At the moment this seems not necessary
            //find_imports_and_preload_files(db, file, loaded_file_entries)
        }
    });
}

#[cfg(test)]
mod tests {
    use config::{DiagnosticConfig, ExcludeRegex, ProjectOptions};
    use proptest::prelude::*;
    use vfs::{FileIndex, LocalFS, PathWithScheme, VfsHandler};

    use super::FileSelector;
    use crate::{Project, RunCause};

    fn diagnostic_snapshot(project: &mut Project) -> (usize, usize, Vec<String>) {
        let diagnostics = project.diagnostics().unwrap();
        (
            diagnostics.checked_files,
            diagnostics.files_with_errors,
            diagnostics
                .issues
                .iter()
                .map(|issue| issue.as_string(&DiagnosticConfig::default(), None))
                .collect(),
        )
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(32))]
        #[test]
        fn cached_selection_matches_fresh_selection_after_edits(
            actions in proptest::collection::vec(0u8..6, 0..24)
        ) {
            let mut options = ProjectOptions::default();
            options.settings.typeshed_path = Some(test_utils::typeshed_path());
            options.settings.add_global_packages_default = false;
            let fs = LocalFS::without_watcher();
            options.settings.mypy_path.push(fs.normalize_rc_path(
                fs.unchecked_abs_path("/selection-cache-property"),
            ));
            let path = |name: &str| PathWithScheme::with_file_scheme(fs.normalize_rc_path(
                fs.unchecked_abs_path(&format!("/selection-cache-property/{name}")),
            ));
            let a = path("a.py");
            let stub = path("a.pyi");
            let b = path("b.py");
            let mut project = Project::without_watcher(options, RunCause::TypeChecking);
            project.store_in_memory_file(a.clone(), "value: int = 1\n".into());
            let _ = diagnostic_snapshot(&mut project);
            for action in actions {
                match action {
                    0 => project.store_in_memory_file(a.clone(), "value: int = 1\n".into()),
                    1 => project.store_in_memory_file(a.clone(), "value: int = 'bad'\n".into()),
                    2 => project.store_in_memory_file(stub.clone(), "value: int = 'bad'\n".into()),
                    3 => project.discard_in_memory_files(std::slice::from_ref(&stub)),
                    4 => project.store_in_memory_file(b.clone(), "value: int = 'bad'\n".into()),
                    _ => project.discard_in_memory_files(std::slice::from_ref(&b)),
                }
                let cached = diagnostic_snapshot(&mut project);
                project.selection_cache = None;
                let fresh = diagnostic_snapshot(&mut project);
                prop_assert_eq!(cached, fresh);
            }
        }
    }

    #[test]
    fn cached_selection_tracks_edits_and_stub_additions_and_removals() {
        let mut options = ProjectOptions::default();
        options.settings.typeshed_path = Some(test_utils::typeshed_path());
        options.settings.add_global_packages_default = false;
        let fs = LocalFS::without_watcher();
        options
            .settings
            .mypy_path
            .push(fs.normalize_rc_path(fs.unchecked_abs_path("/selection-cache-regression")));
        let mut project = Project::without_watcher(options, RunCause::TypeChecking);
        let make_path = |name: &str| {
            PathWithScheme::with_file_scheme(fs.normalize_rc_path(
                fs.unchecked_abs_path(&format!("/selection-cache-regression/{name}")),
            ))
        };
        let a = make_path("a.py");
        let b = make_path("b.py");
        let stub = make_path("a.pyi");
        project.store_in_memory_files(vec![
            (a.clone(), "value: int = 'bad'\n".into()),
            (b, "value: int = 'bad'\n".into()),
        ]);
        for _ in 0..2 {
            let diagnostics = project.diagnostics().unwrap();
            assert_eq!(diagnostics.checked_files, 2);
            assert_eq!(diagnostics.issues.len(), 2);
        }
        project.store_in_memory_file(a, "value: int = 1\n".into());
        let diagnostics = project.diagnostics().unwrap();
        assert_eq!(diagnostics.checked_files, 2);
        assert_eq!(diagnostics.issues.len(), 1);
        project.store_in_memory_file(stub.clone(), "value: int = 'bad'\n".into());
        let diagnostics = project.diagnostics().unwrap();
        assert_eq!(diagnostics.checked_files, 2);
        assert_eq!(diagnostics.issues.len(), 2);
        project.discard_in_memory_files(&[stub]);
        let diagnostics = project.diagnostics().unwrap();
        assert_eq!(diagnostics.checked_files, 2);
        assert_eq!(diagnostics.issues.len(), 1);
    }

    #[test]
    fn duplicate_file_indices_keep_the_earliest_discovery_order() {
        let mut options = ProjectOptions::default();
        options.settings.typeshed_path = Some(test_utils::typeshed_path());
        options.settings.add_global_packages_default = false;
        let project = Project::without_watcher(options, RunCause::TypeChecking);
        let selector = FileSelector::new(&project.db);
        let file = FileIndex(1);
        selector.add_file_index(9, file);
        selector.add_file_index(2, file);
        selector.add_file_index(5, file);
        assert_eq!(selector.file_indexes_to_order_map.read().unwrap()[&file], 2);
    }

    #[test]
    fn project_selection_respects_exclusions_and_python_file_types() {
        for excluded in [false, true] {
            let mut options = ProjectOptions::default();
            options.settings.typeshed_path = Some(test_utils::typeshed_path());
            options.settings.add_global_packages_default = false;
            let fs = LocalFS::without_watcher();
            options
                .settings
                .mypy_path
                .push(fs.normalize_rc_path(fs.unchecked_abs_path("/selection-regression")));
            if excluded {
                options
                    .flags
                    .excludes
                    .push(ExcludeRegex::new(r"skip\.py$|ignored_dir(?:[/\\]|$)".into()).unwrap());
            }
            let mut project = Project::without_watcher(options, RunCause::TypeChecking);
            let vfs = project.vfs_handler();
            let files = [
                ("keep.py", "value: int = 'bad'\n"),
                ("skip.py", "value: int = 'bad'\n"),
                ("ignored_dir/nested.py", "value: int = 'bad'\n"),
                ("same.py", "value: int = 'bad'\n"),
                ("same.pyi", "value: int\n"),
                ("é.py", "value: int = 'bad'\n"),
                ("é.pyi", "value: int\n"),
                ("longer_module_name.py", "value: int = 'bad'\n"),
                ("notes.txt", "value: int = 'bad'\n"),
            ]
            .into_iter()
            .map(|(name, code)| {
                let path = format!("/selection-regression/{name}");
                (
                    PathWithScheme::with_file_scheme(
                        vfs.normalize_rc_path(vfs.unchecked_abs_path(&path)),
                    ),
                    code.into(),
                )
            })
            .collect();
            project.store_in_memory_files(files);
            let diagnostics = project.diagnostics().unwrap();
            assert_eq!(diagnostics.checked_files, if excluded { 4 } else { 6 });
            assert_eq!(diagnostics.issues.len(), if excluded { 2 } else { 4 });
        }
    }
}
