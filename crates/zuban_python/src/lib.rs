#![allow(clippy::nonminimal_bool)] // I don't like this rule
#![allow(clippy::too_many_arguments)] // TODO For now this is easier, but probably enable again

mod arguments;
mod auto_imports;
mod code_actions;
mod completion;
mod database;
mod diagnostics;
mod documentation;
mod file;
mod format_data;
mod getitem;
mod goto;
mod heuristic_infer;
mod imports;
mod inference_state;
mod inferred;
mod inlay_hints;
mod lines;
mod lsp_utils;
mod match_;
mod matcher;
mod name;
mod node_ref;
mod params;
mod prelude;
mod pytest;
mod python_state;
mod result_context;
mod select_files;
mod selection_ranges;
mod semantic_tokens;
mod signatures;
mod sys_path;
mod type_;
mod type_helpers;
mod typeshed;
mod utils;

use std::{cell::OnceCell, path::Path};

use ::utils::FastHashMap;
use anyhow::bail;
use completion::CompletionResolver;
pub use completion::{Completion, CompletionItemKind};
pub use goto::{GotoGoal, ReferencesGoal};
use goto::{GotoResolver, PositionalDocument, ReferencesResolver};
use lsp_types::{FoldingRangeKind, Position};
use name::Range;
use parsa_python_cst::{GotoNode, Scope, Tree};
use rayon::prelude::*;
pub use signatures::{CallSignature, CallSignatures, SignatureParam};
use vfs::{AbsPath, FileIndex, LocalFS, PathWithScheme, VfsHandler};

pub use code_actions::CodeAction;
use config::{IgnoreFileReason, ProjectOptions, PythonVersion, Settings, TypeCheckerFlags};
pub use database::RunCause;
pub use diagnostics::Severity;
pub use documentation::DocumentationResult;
pub use lines::PositionInfos;
pub use name::{Name, NameSymbol, ValueName};
pub use semantic_tokens::{SemanticToken, SemanticTokenProperties};

use crate::{
    file::File, goto::HeuristicDetail, matcher::invalidate_matching_cache, prelude::*,
    select_files::all_typechecked_files,
};

pub struct Project {
    db: Database,
}

impl Project {
    pub fn new(vfs: Box<dyn VfsHandler>, options: ProjectOptions, cause: RunCause) -> Self {
        let db = Database::new(vfs, options, cause);
        Self::new_internal(db)
    }

    fn new_internal(db: Database) -> Self {
        tracing::debug!("Project run cause: {:#?}", &db.run_cause);
        tracing::debug!("Project settings: {:#?}", &db.project);
        Self { db }
    }

    pub fn from_recovery(
        vfs: Box<dyn VfsHandler>,
        options: ProjectOptions,
        recovery: PanicRecovery,
    ) -> Self {
        let db = Database::from_recovery(vfs, options, recovery.run_cause, recovery.vfs);
        Self { db }
    }

    pub fn without_watcher(options: ProjectOptions, cause: RunCause) -> Self {
        let db = Database::new(Box::new(LocalFS::without_watcher()), options, cause);
        Self::new_internal(db)
    }

    pub fn invalidate_path(&mut self, path: &AbsPath) {
        self.db.invalidate_path(path)
    }

    pub fn into_panic_recovery(self) -> PanicRecovery {
        PanicRecovery {
            vfs: self.db.vfs.into_panic_recovery(),
            run_cause: self.db.run_cause,
        }
    }

    pub fn workspace_documents(&self) -> impl ParallelIterator<Item = Document<'_>> {
        invalidate_matching_cache();
        let (known_file_indexes, files_to_be_loaded) = all_typechecked_files(&self.db);
        known_file_indexes
            .into_par_iter()
            .map(|(file_index, _)| file_index)
            // Make sure the file can be loaded, because this is not guaranteed when retrieving a
            // FileIndex.
            .filter(|file_index| self.db.ensure_file_for_file_index(*file_index).is_ok())
            .chain(
                files_to_be_loaded
                    .into_par_iter()
                    .filter_map(|(_, entry, _)| {
                        self.db.load_file_index_from_workspace(&entry, false)
                    }),
            )
            .map(|file_index| Document {
                project: self,
                file_index,
            })
    }

    pub fn store_in_memory_file(&mut self, path: PathWithScheme, code: Box<str>) {
        self.db.store_in_memory_file(path, code, None);
    }

    /// Stores independent files and applies database-wide invalidation once.
    /// Parent-linked files must be stored separately after their parent.
    pub fn store_in_memory_files(&mut self, files: Vec<(PathWithScheme, Box<str>)>) {
        self.db.store_in_memory_files(files);
    }

    pub fn store_file_with_lsp_changes(
        &mut self,
        path: PathWithScheme,
        content_changes: Vec<lsp_types::TextDocumentContentChangeEvent>,
        to_input_position: impl Fn(Position) -> InputPosition,
    ) -> anyhow::Result<()> {
        let Some(index) = self.db.vfs.in_memory_file(&path) else {
            bail!("Missing loaded file for {path:?} while trying to store LSP changes")
        };
        let file = self.db.loaded_python_file(index);
        let old_code = file.tree.code();
        let applied = lsp_utils::apply_document_changes_with_metadata(
            old_code,
            &file.newline_indices,
            content_changes,
            to_input_position,
        )?;
        match applied.code {
            std::borrow::Cow::Owned(code) => {
                let parent = file.super_file.map(|s| s.file);
                let code = code.into_boxed_str();
                let syntax_candidate = applied.same_length_edit.and_then(|(start, end)| {
                    code.get(start..end)
                        .filter(|replacement| {
                            file.tree
                                .can_reuse_syntax_for_name_edit(start, end, replacement)
                        })
                        .map(|_| database::SyntaxReuseCandidate {
                            tree: file.tree.clone(),
                            start,
                            end,
                            source_address: code.as_ptr() as usize,
                        })
                });
                let updated_line_indices = match applied.line_index_update {
                    lsp_utils::LineIndexUpdate::Reuse => Some(file.newline_indices.clone()),
                    lsp_utils::LineIndexUpdate::Shift {
                        end,
                        removed_len,
                        inserted_len,
                    } => {
                        let file = self
                            .db
                            .vfs
                            .file_mut(index)
                            .expect("open in-memory file must remain loaded");
                        let old_indices = std::mem::replace(
                            &mut file.newline_indices,
                            lines::NewlineIndices::new(),
                        );
                        old_indices.shifted_after_edit(
                            file.tree.code(),
                            end,
                            removed_len,
                            inserted_len,
                        )
                    }
                    lsp_utils::LineIndexUpdate::Rebuild => None,
                };
                self.db.store_in_memory_file_with_syntax_candidate(
                    path,
                    code,
                    parent,
                    syntax_candidate,
                );
                if let Some(line_indices) = updated_line_indices {
                    self.db
                        .vfs
                        .file_mut(index)
                        .expect("updated in-memory file must remain loaded")
                        .newline_indices = line_indices;
                }
            }
            std::borrow::Cow::Borrowed(_) => {
                self.db
                    .vfs
                    .handler
                    .on_invalidated_in_memory_file(vfs::InvalidatedInMemoryFile {
                        path,
                        maybe_from_outside_of_in_memory_files: false,
                    });
            }
        }
        Ok(())
    }

    pub fn store_in_memory_file_with_parent(
        &mut self,
        path: PathWithScheme,
        code: Box<str>,
        parent: &PathWithScheme,
    ) -> anyhow::Result<()> {
        let Some(parent) = self.db.vfs.in_memory_file(parent) else {
            bail!(
                "Parent with path {} does not exist when storing an in memory file",
                parent.as_uri()
            );
        };
        self.db.store_in_memory_file(path, code, Some(parent));
        Ok(())
    }

    pub fn code_of_in_memory_file(&mut self, path: &PathWithScheme) -> Option<&str> {
        let file_index = self.db.vfs.in_memory_file(path)?;
        Some(self.db.loaded_python_file(file_index).code())
    }

    pub fn delete_directory_of_in_memory_files(
        &mut self,
        path: &PathWithScheme,
    ) -> Result<(), String> {
        self.db.delete_directory_of_in_memory_files(path)
    }

    pub fn close_in_memory_file(&mut self, path: &PathWithScheme) -> Result<(), &'static str> {
        self.db.close_in_memory_file(path)
    }

    /// Closes the currently open paths and applies database-wide invalidation once.
    /// Paths that are already closed are ignored.
    pub fn close_in_memory_files(&mut self, paths: &[PathWithScheme]) {
        self.db.close_in_memory_files(paths)
    }

    /// Discards in-memory files known to have no backing files on disk.
    /// Use `close_in_memory_files` for normal editor buffers so disk contents are restored.
    pub fn discard_in_memory_files(&mut self, paths: &[PathWithScheme]) {
        self.db.discard_in_memory_files(paths)
    }

    pub fn diagnostics(&mut self) -> anyhow::Result<Diagnostics<'_>> {
        invalidate_matching_cache();
        if self.db.project.settings.mypy_path.len() > 1 {
            debug!(
                "Has complex mypy path: {:?}",
                &self.db.project.settings.mypy_path
            );
        }
        debug!(
            "Checking the following files: {:?}",
            self.db
                .project
                .settings
                .files_or_directories_to_check
                .iter()
                .map(|g| g.as_str())
                .collect::<Vec<_>>()
        );
        let mut checked_files = 0;
        let mut files_with_errors = 0;

        let issues = select_files::diagnostics_for_relevant_files(&self.db, |file| {
            checked_files += 1;
            let mut issues = file.diagnostics(&self.db).into_vec();
            issues.sort_by_key(|issue| issue.start_position().byte_position);
            if !issues.is_empty() {
                files_with_errors += 1;
            }
            issues
        })?;
        tracing::info!("Checked {checked_files} files ({files_with_errors} files had errors)");
        Ok(Diagnostics {
            checked_files,
            files_with_errors,
            issues,
            error_count: Default::default(),
        })
    }

    /// This function is mostly for tests and should therefore not be used for something
    /// stable. We would have to ensure first it works everywhere.
    /// It currently is for example a big issue that HashableRawStr used in the name binder is very
    /// unsafe and will lead to SEGFAULTS if the original project is not kept.
    pub fn try_to_reuse_project_resources_for_tests(&mut self, options: ProjectOptions) -> Self {
        let db = self.db.try_to_reuse_project_resources_for_tests(options);
        Project { db }
    }

    pub fn document(&mut self, path: &PathWithScheme) -> Option<Document<'_>> {
        invalidate_matching_cache();
        let file_index = self.db.file_by_file_path(path)?;
        tracing::debug!("Looking at document #{file_index} for {}", path.as_uri());
        Some(Document {
            project: self,
            file_index,
        })
    }

    pub fn vfs_handler(&self) -> &dyn VfsHandler {
        self.db.vfs.handler.as_ref()
    }

    pub fn mode(&self) -> config::Mode {
        self.db.project.settings.mode
    }
}

impl std::fmt::Debug for Project {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Project")
            .field("settings", &self.db.project.settings)
            .finish()
    }
}

pub struct Document<'project> {
    project: &'project Project,
    file_index: FileIndex,
}

impl<'project> Document<'project> {
    pub fn path(&self) -> &'project PathWithScheme {
        let python_file = self.project.db.loaded_python_file(self.file_index);
        python_file.file_path_with_scheme(&self.project.db)
    }

    pub fn diagnostics(&mut self) -> Box<[diagnostics::Diagnostic<'_>]> {
        let python_file = self.project.db.loaded_python_file(self.file_index);
        python_file.diagnostics(&self.project.db)
    }

    fn positional_document(
        &self,
        position: InputPosition,
    ) -> anyhow::Result<PositionalDocument<'project, GotoNode<'project>>> {
        let db = &self.project.db;
        PositionalDocument::for_goto(db, db.loaded_python_file(self.file_index), position)
    }

    pub fn goto<T>(
        &self,
        position: InputPosition,
        goal: GotoGoal,
        follow_imports: bool,
        on_name: impl for<'a> Fn(Name) -> T,
    ) -> anyhow::Result<Vec<T>> {
        Ok(
            GotoResolver::new(self.positional_document(position)?, goal, on_name)
                .goto(follow_imports),
        )
    }

    pub fn infer_definition<T>(
        &self,
        position: InputPosition,
        goal: GotoGoal,
        use_heuristics: bool,
        on_name: impl for<'a> FnMut(ValueName) -> T,
    ) -> anyhow::Result<Vec<T>> {
        Ok(
            GotoResolver::new(self.positional_document(position)?, goal, on_name)
                .infer_definition(use_heuristics.then_some(HeuristicDetail::Shallow))
                .1,
        )
    }

    pub fn references<T>(
        &self,
        position: InputPosition,
        goal: ReferencesGoal,
        include_declarations: bool,
        on_name: impl for<'a> Fn(Name) -> T,
    ) -> anyhow::Result<Vec<T>> {
        Ok(
            ReferencesResolver::new(self.positional_document(position)?, on_name)
                .references(goal, include_declarations),
        )
    }

    pub fn references_for_rename<'x>(
        &self,
        position: InputPosition,
        new_name: &'x str,
    ) -> anyhow::Result<RenameChanges<'project, 'x>> {
        let document = self.positional_document(position)?;
        let Some(name) = document.node.on_name() else {
            bail!("Could not find a name under the cursor to rename");
        };

        let db = &self.project.db;
        if name.as_code() == new_name {
            // The rename doesn't change anything, because the names stay the same
            return Ok(RenameChanges {
                changes: vec![],
                file_renames: vec![],
                old_name: name.as_code(),
                new_name,
            });
        }

        let mut file_renames: Vec<&'project PathWithScheme> = vec![];
        let mut file_changes = FastHashMap::default();
        let references = ReferencesResolver::new(document, |name| match &name {
            Name::TreeName(tree_name) => {
                let file_index = tree_name.file.file_index;
                file_changes
                    .entry(file_index)
                    .or_insert_with(std::vec::Vec::new)
                    .push(name.name_range());
            }
            Name::ModuleName(module_name) => {
                file_renames.push(module_name.file.file_path_with_scheme(db))
            }
            Name::NodeName(_) => recoverable_error!("Should never reach a node name in rename"),
        })
        .references(ReferencesGoal::OnlyTypeCheckedWorkspaces, true);
        if references.is_empty() {
            bail!(
                "Could not find the definition of {:?} under the cursor",
                name.as_code()
            );
        }
        let changes: Vec<SingleFileRenameChanges<'project>> = file_changes
            .into_iter()
            .map(|(file_index, changes)| {
                //
                SingleFileRenameChanges {
                    path: db.vfs.file_path(file_index),
                    ranges: changes,
                }
            })
            .collect();
        Ok(RenameChanges {
            changes,
            file_renames,
            old_name: name.as_code(),
            new_name,
        })
    }

    pub fn folding_ranges(&self) -> impl Iterator<Item = FoldingRange<'project>> {
        let file = self.project.db.loaded_python_file(self.file_index);
        let to_range = |from, to, kind| FoldingRange {
            start: file.byte_to_position_infos(&self.project.db, from),
            end: file.byte_to_position_infos(&self.project.db, to),
            kind,
        };
        let initial_imports_end = file.tree.initial_imports_end_code_index();
        let has_import_range = file.all_imports.first().is_some_and(|imp| {
            NodeRef::new(file, imp.node_index).node_start_position() < initial_imports_end
        });
        has_import_range
            .then(|| to_range(0, initial_imports_end - 1, FoldingRangeKind::Imports))
            .into_iter()
            .chain(
                file.tree
                    .folding_blocks()
                    .map(move |(start, stop)| to_range(start, stop, FoldingRangeKind::Region)),
            )
    }

    pub fn complete<T>(
        &self,
        position: InputPosition,
        filter_with_name_under_cursor: bool,
        on_completion: impl Fn(Range, &dyn Completion) -> Option<T>,
    ) -> anyhow::Result<Vec<T>> {
        CompletionResolver::complete(
            &self.project.db,
            self.project.db.loaded_python_file(self.file_index),
            position,
            filter_with_name_under_cursor,
            on_completion,
        )
    }

    pub fn call_signatures(
        &self,
        position: InputPosition,
    ) -> anyhow::Result<Option<CallSignatures<'_>>> {
        signatures::SignatureResolver::call_signatures(
            &self.project.db,
            self.project.db.loaded_python_file(self.file_index),
            position,
        )
    }

    pub fn is_valid_rename_location(
        &self,
        position: InputPosition,
    ) -> anyhow::Result<Option<Range<'_>>> {
        let document = self.positional_document(position)?;
        let file = document.file;
        let Some(name) = document.node.on_name() else {
            return Ok(None);
        };
        let resolver = GotoResolver::new(document, GotoGoal::Indifferent, |_: Name| ());
        let is_valid = !resolver.goto(false).is_empty();
        if !is_valid {
            anyhow::bail!(
                "The reference {:?} cannot be resolved; rename is therefore not possible.",
                name.as_code()
            )
        }
        Ok(Some((
            file.byte_to_position_infos(&self.project.db, name.start()),
            file.byte_to_position_infos(&self.project.db, name.end()),
        )))
    }

    pub fn symbols(&self) -> impl ExactSizeIterator<Item = NameSymbol<'_>> {
        let python_file = self.project.db.loaded_python_file(self.file_index);
        NameSymbol::symbol_iterator_from_symbol_table(
            &self.project.db,
            python_file,
            Scope::Module,
            &python_file.symbol_table,
        )
    }

    pub fn ignored_errors_reason(&self) -> Option<IgnoreFileReason> {
        self.project
            .db
            .loaded_python_file(self.file_index)
            .ignore_type_errors
    }
}

#[derive(Debug)]
pub struct SingleFileRenameChanges<'db> {
    pub path: &'db PathWithScheme,
    pub ranges: Vec<Range<'db>>,
}

#[derive(Debug)]
pub struct RenameChanges<'db, 'a> {
    pub changes: Vec<SingleFileRenameChanges<'db>>,
    file_renames: Vec<&'db PathWithScheme>,
    pub old_name: &'db str,
    pub new_name: &'a str,
}

#[derive(Debug)]
pub struct FileRename<'db, 'a> {
    from: &'db PathWithScheme,
    new_name: &'a str,
}

impl<'db> FileRename<'db, '_> {
    pub fn from(&self) -> &'db PathWithScheme {
        self.from
    }

    pub fn from_uri(&self) -> String {
        self.from.as_uri()
    }

    pub fn to_uri(&self) -> String {
        let mut uri = self.from_uri();
        let path = Path::new(&uri);
        let mut parent = path.parent().unwrap();
        let old_name = path.file_stem().unwrap().to_str().unwrap();
        let extension = path.extension().unwrap().to_str().unwrap().to_string();
        let mut maybe_init = "".to_string();
        if old_name == "__init__"
            && let Some(par_parent) = parent.parent()
        {
            parent = par_parent;
            maybe_init = format!("{old_name}/")
        }
        uri.truncate(parent.as_os_str().len());
        uri.push('/');
        uri += self.new_name;
        uri += &maybe_init;
        uri.push('.');
        uri += &extension;
        uri
    }
}

impl<'db> RenameChanges<'db, '_> {
    pub fn has_changes(&self) -> bool {
        !self.changes.is_empty() || !self.file_renames.is_empty()
    }

    pub fn renames(&self) -> impl Iterator<Item = FileRename<'db, '_>> {
        self.file_renames.iter().map(|from| FileRename {
            from,
            new_name: self.new_name,
        })
    }
}

/// All positions are zero based
#[derive(Debug, Clone, Copy)]
pub enum InputPosition {
    NthUTF8Byte(usize),
    Utf8Bytes { line: usize, column: usize },
    Utf16CodeUnits { line: usize, column: usize },
    CodePoints { line: usize, column: usize },
}

/*
impl<'a> Script<'a> {
    pub fn search(&self, _text: String, _all_scopes: bool, _fuzzy: bool) {}
    pub fn complete_search(&self, _text: String, _all_scopes: bool, _fuzzy: bool) {}
    pub fn context(&self, _position: Position) {}
}
*/

pub struct FoldingRange<'db> {
    pub start: PositionInfos<'db>,
    pub end: PositionInfos<'db>,
    pub kind: FoldingRangeKind,
}

pub struct Diagnostics<'a> {
    pub checked_files: usize,
    pub files_with_errors: usize,
    pub issues: Vec<diagnostics::Diagnostic<'a>>,
    error_count: OnceCell<usize>,
}

impl Diagnostics<'_> {
    pub fn summary(&self) -> String {
        let s_if_plural = |n| match n {
            1 => "",
            _ => "s",
        };
        let error_count = self.error_count();
        if error_count == 0 {
            format!(
                "Success: no issues found in {checked} source file{checked_s}",
                checked = self.checked_files,
                checked_s = s_if_plural(self.checked_files),
            )
        } else {
            format!(
                "Found {error_count} error{e_s} in {fwe} file{fwe_s} (checked {checked} source file{checked_s})",
                e_s = s_if_plural(self.issues.len()),
                fwe = self.files_with_errors,
                fwe_s = s_if_plural(self.files_with_errors),
                checked = self.checked_files,
                checked_s = s_if_plural(self.checked_files),
            )
        }
    }

    pub fn error_count(&self) -> usize {
        *self.error_count.get_or_init(|| {
            self.issues
                .iter()
                .filter(|issue| issue.severity() == Severity::Error)
                .count()
        })
    }

    pub fn sort_issues_by_kind(&mut self) {
        self.issues.sort_by_key(|issue| &issue.issue.kind)
    }
}

pub struct PanicRecovery {
    vfs: vfs::VfsPanicRecovery<Tree>,
    run_cause: RunCause,
}

#[cfg(test)]
mod lsp_edit_tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use lsp_types::{Range, TextDocumentContentChangeEvent};

    use super::*;

    #[test]
    fn same_length_name_edit_rebinds_symbols_and_diagnostics() {
        let mut options = ProjectOptions::default();
        options.settings.typeshed_path = Some(test_utils::typeshed_path());
        options.settings.add_global_packages_default = false;
        let mut project = Project::without_watcher(options, RunCause::TypeChecking);
        let vfs = project.vfs_handler();
        let path = PathWithScheme::with_file_scheme(
            vfs.normalize_rc_path(vfs.unchecked_abs_path("/name-edit.py")),
        );
        project.store_in_memory_file(path.clone(), "foo = 1\nfoo\n".into());

        let edit = |text: &str| TextDocumentContentChangeEvent {
            range: Some(Range::new(Position::new(0, 0), Position::new(0, 3))),
            range_length: None,
            text: text.into(),
        };
        let position = |pos: Position| InputPosition::Utf16CodeUnits {
            line: pos.line as usize,
            column: pos.character as usize,
        };
        project
            .store_file_with_lsp_changes(path.clone(), vec![edit("bar")], position)
            .unwrap();
        let index = project.db.vfs.in_memory_file(&path).unwrap();
        let file = project.db.loaded_python_file(index);
        assert!(file.lookup_symbol("bar").is_some());
        assert!(file.lookup_symbol("foo").is_none());
        assert_eq!(project.document(&path).unwrap().diagnostics().len(), 1);

        project
            .store_file_with_lsp_changes(path.clone(), vec![edit("foo")], position)
            .unwrap();
        let file = project.db.loaded_python_file(index);
        assert!(file.lookup_symbol("foo").is_some());
        assert!(file.lookup_symbol("bar").is_none());
        assert!(project.document(&path).unwrap().diagnostics().is_empty());
    }

    #[test]
    fn same_length_name_edit_preserves_inline_directive_semantics() {
        let mut options = ProjectOptions::default();
        options.settings.typeshed_path = Some(test_utils::typeshed_path());
        options.settings.add_global_packages_default = false;
        let mut project = Project::without_watcher(options, RunCause::TypeChecking);
        let vfs = project.vfs_handler();
        let path = PathWithScheme::with_file_scheme(
            vfs.normalize_rc_path(vfs.unchecked_abs_path("/inline-directive-edit.py")),
        );
        project.store_in_memory_file(
            path.clone(),
            "# mypy: ignore_errors = True\nfoo: int = 'bad'\n".into(),
        );
        assert!(project.document(&path).unwrap().diagnostics().is_empty());

        project
            .store_file_with_lsp_changes(
                path.clone(),
                vec![TextDocumentContentChangeEvent {
                    range: Some(Range::new(Position::new(1, 0), Position::new(1, 3))),
                    range_length: None,
                    text: "bar".into(),
                }],
                |pos| InputPosition::Utf16CodeUnits {
                    line: pos.line as usize,
                    column: pos.character as usize,
                },
            )
            .unwrap();
        assert!(project.document(&path).unwrap().diagnostics().is_empty());

        let value_start = "# mypy: ignore_errors = ".len() as u32;
        project
            .store_file_with_lsp_changes(
                path.clone(),
                vec![TextDocumentContentChangeEvent {
                    range: Some(Range::new(
                        Position::new(0, value_start),
                        Position::new(0, value_start + 4),
                    )),
                    range_length: None,
                    text: "False".into(),
                }],
                |pos| InputPosition::Utf16CodeUnits {
                    line: pos.line as usize,
                    column: pos.character as usize,
                },
            )
            .unwrap();
        assert_eq!(project.document(&path).unwrap().diagnostics().len(), 1);
    }

    #[test]
    fn repeated_definitions_form_a_complete_cycle() {
        let mut options = ProjectOptions::default();
        options.settings.typeshed_path = Some(test_utils::typeshed_path());
        options.settings.add_global_packages_default = false;
        let mut project = Project::without_watcher(options, RunCause::TypeChecking);
        let vfs = project.vfs_handler();
        let path = PathWithScheme::with_file_scheme(
            vfs.normalize_rc_path(vfs.unchecked_abs_path("/repeated-definitions.py")),
        );
        const DEFINITION_COUNT: usize = 1_000;
        project.store_in_memory_file(path.clone(), "x = 1\n".repeat(DEFINITION_COUNT).into());

        let index = project.db.vfs.in_memory_file(&path).unwrap();
        let file = project.db.loaded_python_file(index);
        let first = file.symbol_table.lookup_symbol("x").unwrap();
        let mut current = first;
        for definition in 0..DEFINITION_COUNT {
            current = file.points.get(current).node_index();
            assert_eq!(current == first, definition + 1 == DEFINITION_COUNT);
        }
    }

    #[test]
    fn interleaved_definitions_keep_separate_cycles() {
        let mut options = ProjectOptions::default();
        options.settings.typeshed_path = Some(test_utils::typeshed_path());
        options.settings.add_global_packages_default = false;
        let mut project = Project::without_watcher(options, RunCause::TypeChecking);
        let vfs = project.vfs_handler();
        let path = PathWithScheme::with_file_scheme(
            vfs.normalize_rc_path(vfs.unchecked_abs_path("/interleaved-definitions.py")),
        );
        project.store_in_memory_file(path.clone(), "x = 1\ny = 1\nx = 2\ny = 2\nx = 3\n".into());

        let index = project.db.vfs.in_memory_file(&path).unwrap();
        let file = project.db.loaded_python_file(index);
        for (name, count) in [("x", 3), ("y", 2)] {
            let first = file.symbol_table.lookup_symbol(name).unwrap();
            let mut current = first;
            for definition in 0..count {
                current = file.points.get(current).node_index();
                assert_eq!(current == first, definition + 1 == count);
            }
        }
    }

    #[test]
    fn unchanged_lsp_edit_preserves_invalidation_notification() {
        let notifications = Arc::new(AtomicUsize::new(0));
        let notified = notifications.clone();
        let fs = LocalFS::with_watcher(move |_| {
            notified.fetch_add(1, Ordering::Relaxed);
        });
        let mut options = ProjectOptions::default();
        options.settings.typeshed_path = Some(test_utils::typeshed_path());
        options.settings.add_global_packages_default = false;
        let mut project = Project::new(Box::new(fs), options, RunCause::TypeChecking);
        let vfs = project.vfs_handler();
        let path = PathWithScheme::with_file_scheme(
            vfs.normalize_rc_path(vfs.unchecked_abs_path("/lsp-noop-test.py")),
        );
        project.store_in_memory_file(path.clone(), "x = 1\n".into());
        let before = notifications.load(Ordering::Relaxed);
        project.store_in_memory_file(path.clone(), "x = 1\n".into());
        assert_eq!(notifications.load(Ordering::Relaxed), before + 1);
        project
            .store_file_with_lsp_changes(
                path.clone(),
                vec![TextDocumentContentChangeEvent {
                    range: Some(Range::new(Position::new(0, 4), Position::new(0, 5))),
                    range_length: None,
                    text: "1".into(),
                }],
                |pos| InputPosition::Utf16CodeUnits {
                    line: pos.line as usize,
                    column: pos.character as usize,
                },
            )
            .unwrap();
        assert_eq!(project.code_of_in_memory_file(&path), Some("x = 1\n"));
        assert_eq!(notifications.load(Ordering::Relaxed), before + 2);
    }

    #[test]
    fn unchanged_buffer_shortcut_expires_after_workspace_change() {
        let mut options = ProjectOptions::default();
        options.settings.typeshed_path = Some(test_utils::typeshed_path());
        options.settings.add_global_packages_default = false;
        let mut project = Project::without_watcher(options, RunCause::TypeChecking);
        let vfs = project.vfs_handler();
        let path = PathWithScheme::with_file_scheme(
            vfs.normalize_rc_path(vfs.unchecked_abs_path("/epoch-test.py")),
        );
        project.store_in_memory_file(path.clone(), "x = 1\n".into());
        assert!(
            project
                .db
                .vfs
                .unchanged_in_memory_file(&path, "x = 1\n")
                .is_some()
        );

        let workspace_path = project
            .vfs_handler()
            .normalize_rc_path(project.vfs_handler().unchecked_abs_path("/epoch-workspace"));
        assert!(
            project.db.vfs.add_workspace_without_full_access(
                workspace_path,
                vfs::WorkspaceKind::TypeChecking,
            )
        );
        assert!(
            project
                .db
                .vfs
                .unchanged_in_memory_file(&path, "x = 1\n")
                .is_none()
        );

        project.store_in_memory_file(path.clone(), "x = 1\n".into());
        assert!(
            project
                .db
                .vfs
                .unchanged_in_memory_file(&path, "x = 1\n")
                .is_some()
        );
    }

    #[test]
    fn content_edit_preserves_other_validated_buffers_but_new_file_expires_them() {
        let mut options = ProjectOptions::default();
        options.settings.typeshed_path = Some(test_utils::typeshed_path());
        options.settings.add_global_packages_default = false;
        let mut project = Project::without_watcher(options, RunCause::TypeChecking);
        let path = |project: &Project, name: &str| {
            let vfs = project.vfs_handler();
            PathWithScheme::with_file_scheme(vfs.normalize_rc_path(vfs.unchecked_abs_path(name)))
        };
        let a = path(&project, "/epoch-a.py");
        let b = path(&project, "/epoch-b.py");
        let c = path(&project, "/epoch-c.py");
        project.store_in_memory_file(a.clone(), "a = 1".into());
        project.store_in_memory_file(b.clone(), "b = 1".into());
        project.store_in_memory_file(a.clone(), "a = 1".into());
        assert!(
            project
                .db
                .vfs
                .unchanged_in_memory_file(&a, "a = 1")
                .is_some()
        );

        project.store_in_memory_file(b, "b = 2".into());
        assert!(
            project
                .db
                .vfs
                .unchanged_in_memory_file(&a, "a = 1")
                .is_some()
        );

        project.store_in_memory_file(c, "c = 1".into());
        assert!(
            project
                .db
                .vfs
                .unchanged_in_memory_file(&a, "a = 1")
                .is_none()
        );
    }

    #[test]
    fn unchanged_dependent_buffer_uses_updated_import_after_content_edit() {
        let fs = LocalFS::without_watcher();
        let mut options = ProjectOptions::default();
        options.settings.typeshed_path = Some(test_utils::typeshed_path());
        options.settings.add_global_packages_default = false;
        options
            .settings
            .mypy_path
            .push(fs.normalize_rc_path(fs.unchecked_abs_path("/epoch-deps")));
        let mut project = Project::without_watcher(options, RunCause::TypeChecking);
        let path = |project: &Project, name: &str| {
            let vfs = project.vfs_handler();
            PathWithScheme::with_file_scheme(vfs.normalize_rc_path(vfs.unchecked_abs_path(name)))
        };
        let a = path(&project, "/epoch-deps/a.py");
        let b = path(&project, "/epoch-deps/b.py");
        let a_code = "from b import foo\nx: int = foo()\n";
        project.store_in_memory_file(b.clone(), "def foo() -> int: return 1\n".into());
        project.store_in_memory_file(a.clone(), a_code.into());
        assert!(project.document(&a).unwrap().diagnostics().is_empty());

        project.store_in_memory_file(b, "def foo() -> str: return 'wrong'\n".into());
        assert!(
            project
                .db
                .vfs
                .unchanged_in_memory_file(&a, a_code)
                .is_some()
        );
        project.store_in_memory_file(a.clone(), a_code.into());
        assert_eq!(project.document(&a).unwrap().diagnostics().len(), 1);
    }

    #[test]
    fn unchanged_buffer_shortcut_survives_many_prepared_files() {
        let mut options = ProjectOptions::default();
        options.settings.typeshed_path = Some(test_utils::typeshed_path());
        let mut project = Project::without_watcher(options, RunCause::TypeChecking);
        let mut first_path = None;
        for i in 0..1000 {
            let vfs = project.vfs_handler();
            let path = PathWithScheme::with_file_scheme(
                vfs.normalize_rc_path(vfs.unchecked_abs_path(&format!("/prepared/test{i}.py"))),
            );
            project.store_in_memory_file(path.clone(), "x = 1\n".into());
            first_path.get_or_insert(path);
        }
        let first_path = first_path.unwrap();
        assert!(
            project
                .db
                .vfs
                .unchanged_in_memory_file(&first_path, "x = 1\n")
                .is_none()
        );
        project.store_in_memory_file(first_path.clone(), "x = 1\n".into());
        assert!(
            project
                .db
                .vfs
                .unchanged_in_memory_file(&first_path, "x = 1\n")
                .is_some()
        );
    }

    #[test]
    fn parser_added_newlines_do_not_reparse_unchanged_buffers() {
        let mut options = ProjectOptions::default();
        options.settings.typeshed_path = Some(test_utils::typeshed_path());
        options.settings.add_global_packages_default = false;
        let mut project = Project::without_watcher(options, RunCause::TypeChecking);
        for (i, input) in ["x = 1", "x = 1\\", "x = 1\\\n", "x = 1\\\r\n"]
            .into_iter()
            .enumerate()
        {
            let vfs = project.vfs_handler();
            let path = PathWithScheme::with_file_scheme(
                vfs.normalize_rc_path(vfs.unchecked_abs_path(&format!("/normalized-{i}.py"))),
            );
            project.store_in_memory_file(path.clone(), input.into());
            let before = project.code_of_in_memory_file(&path).unwrap().as_ptr();
            assert!(
                project
                    .db
                    .vfs
                    .unchanged_in_memory_file(&path, input)
                    .is_some()
            );
            project.store_in_memory_file(path.clone(), input.into());
            assert_eq!(
                project.code_of_in_memory_file(&path).unwrap().as_ptr(),
                before
            );
        }
    }

    #[test]
    fn sequential_edits_keep_later_utf16_positions_correct() {
        let mut options = ProjectOptions::default();
        options.settings.typeshed_path = Some(test_utils::typeshed_path());
        options.settings.add_global_packages_default = false;
        let mut project = Project::without_watcher(options, RunCause::TypeChecking);
        let vfs = project.vfs_handler();
        let path = PathWithScheme::with_file_scheme(
            vfs.normalize_rc_path(vfs.unchecked_abs_path("/lsp-lines-test.py")),
        );
        project.store_in_memory_file(path.clone(), "x = 1\nemoji = '😀'\ntail = 2\n".into());
        let replace = |project: &mut Project, line, start, end, text: &str| {
            project
                .store_file_with_lsp_changes(
                    path.clone(),
                    vec![TextDocumentContentChangeEvent {
                        range: Some(Range::new(
                            Position::new(line, start),
                            Position::new(line, end),
                        )),
                        range_length: None,
                        text: text.into(),
                    }],
                    |pos| InputPosition::Utf16CodeUnits {
                        line: pos.line as usize,
                        column: pos.character as usize,
                    },
                )
                .unwrap();
        };
        replace(&mut project, 0, 0, 1, "y");
        replace(&mut project, 1, 9, 11, "z");
        replace(&mut project, 2, 7, 8, "3");
        assert_eq!(
            project.code_of_in_memory_file(&path),
            Some("y = 1\nemoji = 'z'\ntail = 3\n")
        );
    }
}
