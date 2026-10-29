mod ide;

use std::{
    cell::{OnceCell, RefCell},
    cmp::Ordering,
    collections::BinaryHeap,
    env,
    fs::{read_dir, read_to_string},
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

use clap::{Command, CommandFactory as _, FromArgMatches as _, Parser};

use config::{
    DiagnosticConfig, Mode, ModeChoice, ModeChoiceArg, ProjectOptions, PythonVersion, Settings,
    TypeCheckerFlags,
};
use ide::find_and_check_ide_tests;
use parsa_python_cst::Tree;
use regex::{Captures, Regex, Replacer};
use test_utils::{Step, Steps, calculate_steps};
use utils::{FastHashMap, FastHashSet};
use vfs::{NormalizedPath, PathWithScheme, SimpleLocalFS, VfsHandler};
use zuban_python::{Project, RunCause};

const SKIP_MYPY_TEST_FILES: [&str; 31] = [
    // --allow-redefinition tests
    "check-redefine.test",
    // Python special features
    "check-formatting.test",
    "check-plugin-attrs.test",
    // Mypy flag checking
    "cmdline.test",
    "cmdline.pyproject.test",
    // Maybe some day
    "check-reports.test",
    "reports.test",
    // Unfortunately probably not possible
    "check-custom-plugin.test",
    "check-plugin-error-codes.test",
    // Probably not relevant, because additional almost unrelated mypy features
    "stubgen.test",
    "typexport-basic.test",
    // dmypy suggest feature,
    // see https://mypy.readthedocs.io/en/stable/mypy_daemon.html#static-inference-of-annotations
    "fine-grained-suggest.test",
    // Inspect feature, see https://mypy.readthedocs.io/en/stable/mypy_daemon.html#static-inference-of-annotations
    "fine-grained-inspect.test",
    // Won't do, because they test mypy internals
    "native-parser.test",
    "native-parser-imports.test",
    "check-incomplete-fixture.test",
    "check-native-int.test",
    "semanal-symtable.test",
    "daemon.test",
    "deps-classes.test",
    "deps-expressions.test",
    "deps-generics.test",
    "deps-statements.test",
    "deps.test",
    "deps-types.test",
    "diff.test",
    "outputjson.test",
    "exportjson.test",
    "errorstream.test",
    "merge.test",
    "ref-info.test",
];

#[cfg(not(target_os = "windows"))]
const BASE_PATH_STR: &str = "/mypylike/";
#[cfg(target_os = "windows")]
const BASE_PATH_STR: &str = r"C:\\mypylike\";

static FIXTURE_ROOT_HAS_DISK_BACKING: OnceLock<bool> = OnceLock::new();

thread_local! {
    static BASE_PATH: Arc<NormalizedPath> = {
        let local_fs = SimpleLocalFS::without_watcher();
        local_fs.normalize_rc_path(local_fs.unchecked_abs_path(BASE_PATH_STR))
    };
}
static CURRENT_TEST: Mutex<Option<(String, String)>> = Mutex::new(None);

struct CurrentTestGuard(Option<(String, String)>);

impl CurrentTestGuard {
    fn new(name: &str, file_name: &str) -> Self {
        let context = Some((name.to_owned(), file_name.to_owned()));
        Self(std::mem::replace(
            &mut *CURRENT_TEST.lock().unwrap(),
            context,
        ))
    }
}

impl Drop for CurrentTestGuard {
    fn drop(&mut self) {
        *CURRENT_TEST.lock().unwrap() = self.0.take();
    }
}

const MYPY_TEST_DATA_PACKAGES_FOLDER: &str = "tests/mypylike/mypy/test-data/packages/";
const OUR_TEST_DATA_PACKAGES_FOLDER: &str = "tests/mypylike/packages/";

lazy_static::lazy_static! {
    static ref CASE: Regex = Regex::new(r"(?m)^\[case ([a-zA-Z_0-9-]+)\][ \t]*\r?\n").unwrap();
    static ref REPLACE_COMMENTS: Regex = Regex::new(r"(?m)^--.*$\r?\n").unwrap();
    static ref REPLACE_TUPLE: Regex = Regex::new(r"\bTuple\b").unwrap();
    static ref REPLACE_MYPY: Regex = Regex::new(r"`-?\d+").unwrap();
    static ref REPLACE_MYPY_ELLIPSIS: Regex = Regex::new(r#""ellipsis""#).unwrap();
    static ref REPLACE_MYPY_NO_RETURN: Regex = Regex::new(r"\bNoReturn\b").unwrap();
    static ref REPLACE_MYPY_SUBTYPE_NUMBERS: Regex = Regex::new(r"(<subclass[^<]+>)\d+").unwrap();
    static ref REPLACE_MYPY_SUBTYPE: Regex = Regex::new(r"\w+\.(<subclass|<callable subtype)").unwrap();
    static ref REPLACE_ESCAPES: Regex = Regex::new(r"(?m)^\\\[").unwrap();
    static ref REPLACE_MAIN: Regex = Regex::new(r"^main:").unwrap();
}

/// Simple program to greet a person
#[derive(Parser, Debug)]
#[command(about, long_about = None)]
struct CliArgs {
    /// Will not reuse the database, which disables a hack, but is slower.
    #[arg(long)]
    no_reuse_db: bool,

    /// Filter files or test names like "check-classes" or "testFooBar"
    #[arg(num_args = 0..)]
    filters: Vec<String>,

    #[arg(long)]
    start_at: Option<String>,

    /// Runs only the tests for typechecking and not language server
    #[arg(long)]
    only_typecheck: bool,
    #[arg(long)]
    only_language_server: bool,

    #[arg(short = 'x')]
    stop_after_first_error: bool,

    #[arg(long)]
    slowest: Option<usize>,
}

#[derive(Parser, Clone, Default)]
struct PerTestFlags {
    #[command(flatten)]
    cli: cli_args::Cli,

    // Maybe implement?
    #[arg(long)]
    local_partial_types: bool,
    #[arg(long)]
    no_local_partial_types: bool,
    #[arg(long)]
    disable_memoryview_promotion: bool,
    #[arg(long)]
    disable_bytearray_promotion: bool,
    #[arg(long)]
    warn_unused_ignores: bool,
    #[arg(long)]
    namespace_packages: bool,
    #[arg(long)]
    no_namespace_packages: bool,
    #[arg(long)]
    no_strict_bytes: bool,
    #[arg(long)]
    follow_imports: Option<String>,

    // Won't implement, Mypy internals
    #[arg(long)]
    no_sqlite_cache: bool,
    #[arg(long)]
    soft_error_limit: Option<isize>,
    #[arg(long)]
    fast_module_lookup: bool,
    #[arg(long)]
    force_union_syntax: bool,
    #[arg(long)]
    no_force_union_syntax: bool,
    #[arg(long)]
    cache_fine_grained: bool,
    #[arg(long)]
    no_incremental: bool,
    #[arg(long)]
    bazel: bool,
    #[arg(long)]
    enable_incomplete_feature: Option<String>,
    #[arg(long)]
    verbose: bool,
    #[arg(short)]
    package: Option<String>,
    #[arg(long)]
    debug_serialize: bool,

    // Not public
    #[arg(long)]
    allow_redefinition_old: bool,
    #[arg(long)]
    disallow_redefinition_old: bool,

    // Our own
    #[arg(long)]
    no_typecheck: bool,
    #[arg(long)]
    only_language_server: bool,
    #[arg(long)]
    no_windows: bool,
    #[arg(long)]
    use_joins: bool,
    #[arg(long)]
    no_use_joins: bool,
    #[arg(long)]
    disallow_empty_bodies: bool,
}

struct TestCase<'name, 'code> {
    file_name: &'name str,
    name: String,
    code: &'code str,
    from_mypy_test_suite: bool,
    steps: OnceCell<Steps<'code>>,
    fixture_data: OnceCell<FixtureData<'code>>,
    parsed_flags: OnceCell<PerTestFlags>,
}

struct FixtureData<'code> {
    parsed_files: Vec<Vec<(&'code str, PathWithScheme, Tree)>>,
    flat_files: Vec<PathWithScheme>,
    directories: Vec<PathWithScheme>,
}

#[derive(Default)]
struct PhaseTimings {
    setup: Duration,
    project_setup: Duration,
    fixture: Duration,
    ide: Duration,
    wanted: Duration,
    inline_errors: Duration,
    file_store: Duration,
    wanted_normalization: Duration,
    diagnostics: Duration,
    diagnostic_compute: Duration,
    comparison: Duration,
    cleanup: Duration,
    project_reuse: Duration,
    project_reuse_count: usize,
    project_fresh: Duration,
    project_fresh_count: usize,
}

thread_local! {
    static PHASE_TIMINGS: RefCell<PhaseTimings> = RefCell::new(PhaseTimings::default());
}
static PROFILE_PHASES: OnceLock<bool> = OnceLock::new();

fn profile_phases() -> bool {
    *PROFILE_PHASES.get_or_init(|| env::var_os("ZUBAN_PROFILE_TEST_PHASES").is_some())
}

#[derive(Clone, Copy)]
struct CommonProject {
    index: usize,
    diagnostics: DiagnosticConfig,
}

#[derive(Eq, Hash, PartialEq)]
struct FlagProjectKey {
    fixture_kind: FixtureConfigKind,
    mypy_compatible: bool,
    no_empty: bool,
    flags: Vec<String>,
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
enum FixtureConfigKind {
    Normal,
    ErrorCodes,
    Columns,
    RecursiveTypes,
    CaseInsensitive,
    FineGrained,
    Abstract,
}

impl TestCase<'_, '_> {
    fn fixture_config_kind(&self) -> FixtureConfigKind {
        match self.file_name {
            "check-errorcodes" => FixtureConfigKind::ErrorCodes,
            "check-columns" => FixtureConfigKind::Columns,
            "check-recursive-types" => FixtureConfigKind::RecursiveTypes,
            "check-modules-case" => FixtureConfigKind::CaseInsensitive,
            "check-abstract" => FixtureConfigKind::Abstract,
            name if name.starts_with("fine-grained") => FixtureConfigKind::FineGrained,
            _ => FixtureConfigKind::Normal,
        }
    }

    fn initialize_flags<'p>(
        &self,
        projects: &'p mut ProjectsCache,
        local_fs: &SimpleLocalFS,
        mode: ModeChoice,
        mut flags: PerTestFlags,
        steps: &[Step],
        common_slot: Option<usize>,
        flag_project_key: Option<FlagProjectKey>,
    ) -> (OwnedOrMut<'p, Project>, DiagnosticConfig) {
        if let Some(slot) = common_slot
            && let Some(entry) = projects
                .common_projects
                .get(self.file_name)
                .and_then(|slots| slots[slot])
        {
            return (
                OwnedOrMut::Mut(&mut projects.projects[entry.index]),
                entry.diagnostics,
            );
        }
        let mut diagnostic_config = DiagnosticConfig {
            show_error_codes: false,
            ..Default::default()
        };
        let po = ProjectOptions::default_for_mode(mode.into());
        let mut config = po.flags;
        // TODO This appears to cause issues, because Mypy uses a custom typing.pyi that has
        // different argument types.
        //config.python_version = self.default_python_version();
        let mut settings = po.settings;
        let mut project_options = None;

        if let Some(mypy_ini_config) = steps[0].files.get("mypy.ini") {
            println!("Loading mypy.ini for {} ({})", self.name, self.file_name);
            let ini = cleanup_mypy_issues(mypy_ini_config).unwrap();
            let mut new = BASE_PATH.with(|base_path| {
                ProjectOptions::from_mypy_ini(
                    local_fs,
                    base_path,
                    &local_fs.join(&base_path, "mypy.ini"),
                    &ini,
                    &mut diagnostic_config,
                    mode,
                )
                .expect("Expected there to be no errors in the mypy.ini")
                .unwrap_or_else(ProjectOptions::mypy_default)
            });
            set_mypy_path(&mut new);
            config = std::mem::replace(&mut new.flags, config);
            settings = std::mem::replace(&mut new.settings, settings);
            project_options = Some(new);
        } else if let Some(pyproject_toml) = steps[0].files.get("pyproject.toml") {
            println!(
                "Loading pyproject.toml for {} ({})",
                self.name, self.file_name
            );
            let ini = cleanup_mypy_issues(pyproject_toml).unwrap();
            let mut new = BASE_PATH.with(|base_path| {
                ProjectOptions::from_pyproject_toml_only(
                    local_fs,
                    base_path,
                    &local_fs.join(&base_path, "pyproject.toml"),
                    &ini,
                    &mut diagnostic_config,
                    mode,
                )
                .expect("Expected there to be no errors in the pyproject.toml")
                .unwrap_or_else(|| ProjectOptions::default_for_mode(settings.mode))
            });
            set_mypy_path(&mut new);
            config = std::mem::replace(&mut new.flags, config);
            settings = std::mem::replace(&mut new.settings, settings);
            project_options = Some(new);
        }

        // Appears mostly in pep561.test
        let first_line = self.code.split('\n').next().unwrap();
        if let Some(suffix) = first_line.strip_prefix("# pkgs:") {
            let current_dir = local_fs.current_dir();
            let mypy_folder = local_fs.join(&current_dir, MYPY_TEST_DATA_PACKAGES_FOLDER);
            let our_folder = local_fs.join(&current_dir, OUR_TEST_DATA_PACKAGES_FOLDER);
            settings.prepended_site_packages.extend(
                suffix
                    .split([';', ','])
                    .map(|s| {
                        [
                            local_fs.normalize_rc_path(local_fs.join(&mypy_folder, s.trim())),
                            local_fs.normalize_rc_path(local_fs.join(&our_folder, s.trim())),
                        ]
                        .into_iter()
                    })
                    .flatten(),
            );
        };

        match self.fixture_config_kind() {
            FixtureConfigKind::ErrorCodes => diagnostic_config.show_error_codes = true,
            FixtureConfigKind::Columns => diagnostic_config.show_column_numbers = true,
            FixtureConfigKind::RecursiveTypes => {
                // This feels very broken, but for now we disable these errors, because they don't feel
                // wrong, but Mypy does not have them.
                config.disabled_error_codes.push("used-before-def".into())
            }
            FixtureConfigKind::CaseInsensitive => {
                // These tests are checking for case insensitive file systems like macOS, Windows
                config.case_sensitive = false;
            }
            FixtureConfigKind::FineGrained => config.local_partial_types = true,
            FixtureConfigKind::Normal | FixtureConfigKind::Abstract => {}
        }
        // This is simply for testing and mirrors how mypy does it.
        config.allow_empty_bodies = !self.name.ends_with("_no_empty")
            && self.fixture_config_kind() != FixtureConfigKind::Abstract;

        flags.cli.mypy_options.allow_redefinition |= flags.allow_redefinition_old;
        flags.cli.mypy_options.allow_redefinition |= flags.allow_redefinition_old;

        BASE_PATH.with(|base_path| {
            let current_dir = NormalizedPath::arc_to_abs_path(base_path.clone());
            cli_args::apply_flags_detailed(
                local_fs,
                &mut settings,
                &mut config,
                &mut diagnostic_config,
                current_dir.clone(),
                flags.cli,
                current_dir,
                None,
            );
        });

        macro_rules! set_flag {
            ($name:ident) => {
                if flags.$name {
                    config.$name = true;
                }
            };
        }

        set_flag!(local_partial_types);
        set_flag!(disable_bytearray_promotion);
        set_flag!(disable_memoryview_promotion);
        set_flag!(use_joins);
        if flags.no_use_joins {
            config.use_joins = false;
        }
        if flags.no_local_partial_types {
            config.local_partial_types = false;
        }
        if flags.disallow_empty_bodies {
            config.allow_empty_bodies = false;
        }

        let project = if let Some(mut project_options) = project_options {
            project_options.settings = settings;
            project_options.flags = config;
            OwnedOrMut::Owned(projects.try_to_reuse_project_parts(project_options))
        } else {
            let index = projects.get_or_create_index(settings, config);
            if let Some(key) = flag_project_key {
                projects.flag_projects.insert(
                    key,
                    CommonProject {
                        index,
                        diagnostics: diagnostic_config,
                    },
                );
            }
            if let Some(slot) = common_slot {
                projects
                    .common_projects
                    .entry(self.file_name.to_owned())
                    .or_insert([None; 4])[slot] = Some(CommonProject {
                    index,
                    diagnostics: diagnostic_config,
                });
            }
            OwnedOrMut::Mut(&mut projects.projects[index])
        };
        (project, diagnostic_config)
    }

    fn run(&self, projects: &mut ProjectsCache, mode: Mode) -> anyhow::Result<bool> {
        let profile = profile_phases();
        let setup_start = profile.then(Instant::now);
        let steps = self
            .steps
            .get_or_init(|| calculate_steps(Some(self.file_name), self.code));
        let common_slot = (steps.flags.is_empty()
            && !steps.steps[0].files.contains_key("mypy.ini")
            && !steps.steps[0].files.contains_key("pyproject.toml")
            && !self.code.starts_with("# pkgs:"))
        .then_some((mode == Mode::Mypy) as usize * 2 + self.name.ends_with("_no_empty") as usize);
        // Avoid parsing, because it's pretty slow and let's not do it if not strictly necessary.
        let flags_key: Vec<String> = steps.flags.iter().map(|flag| (*flag).to_owned()).collect();
        let flags = if flags_key.is_empty() {
            PerTestFlags::default()
        } else if let Some(parsed) = self.parsed_flags.get() {
            // The fixture flags are independent of the project run cause. Clone because
            // initialize_flags may change the per-run CLI options.
            parsed.clone()
        } else {
            let parsed = if let Some(cached) = projects.flags_cache.get(&flags_key) {
                cached.clone()
            } else {
                // Reuse the Command, then cache the parsed value for identical fixture flags.
                let matches = projects.command.try_get_matches_from_mut(
                    std::iter::once("").chain(steps.flags.iter().copied()),
                )?;
                let parsed = PerTestFlags::from_arg_matches(&matches)?;
                projects
                    .flags_cache
                    .insert(flags_key.clone(), parsed.clone());
                parsed
            };
            let _ = self.parsed_flags.set(parsed.clone());
            parsed
        };
        let steps = &steps.steps;
        if flags.only_language_server && !matches!(projects.run_cause, RunCause::LanguageServer)
            || flags.no_windows && cfg!(windows)
            || match flags.cli.mode {
                Some(ModeChoiceArg::Default) => mode != Mode::Default,
                Some(ModeChoiceArg::Mypy) => mode != Mode::Mypy,
                Some(ModeChoiceArg::Auto) => {
                    // We skip these, because we only want to run these tests once and that's
                    // already done in default mode.
                    mode == Mode::Mypy
                }
                None => false,
            }
        {
            return Ok(false);
        }
        let _current_test = CurrentTestGuard::new(&self.name, self.file_name);
        let local_fs = SimpleLocalFS::without_watcher();
        let fixture_data = self.fixture_data.get_or_init(|| {
            let parsed_files = steps
                .iter()
                .map(|step| {
                    let mut files: Vec<_> = step
                        .files
                        .iter()
                        .filter(|(path, _)| {
                            !["mypy.ini", "pyproject.toml"].contains(path)
                                && !path.starts_with("../")
                        })
                        .map(|(&path, &code)| {
                            (
                                path,
                                base_path_join(&local_fs, path),
                                Tree::parse(code.into()),
                            )
                        })
                        .collect();
                    files.sort_by_key(|(path, _, _)| *path);
                    files
                })
                .collect::<Vec<_>>();
            let mut flat_files = Vec::new();
            let mut seen_flat_files = FastHashSet::default();
            let mut directories = Vec::new();
            let mut seen_directories = FastHashSet::default();
            for step in steps {
                for path in step.files.keys() {
                    if path.contains('/') {
                        let before_slash = path.split('/').next().unwrap();
                        if seen_directories.insert(before_slash) {
                            directories.push(base_path_join(&local_fs, before_slash));
                        }
                    } else if seen_flat_files.insert(*path) {
                        flat_files.push(base_path_join(&local_fs, path));
                    }
                }
            }
            FixtureData {
                parsed_files,
                flat_files,
                directories,
            }
        });
        let no_typecheck = flags.no_typecheck;
        // Flag-bearing cases with the same effective fixture settings can share the resolved
        // project and diagnostic config. Config files and package directives vary per case.
        let flag_project_key = (!flags_key.is_empty()
            && !steps[0].files.contains_key("mypy.ini")
            && !steps[0].files.contains_key("pyproject.toml")
            && !self.code.starts_with("# pkgs:"))
        .then(|| FlagProjectKey {
            fixture_kind: self.fixture_config_kind(),
            mypy_compatible: mode == Mode::Mypy,
            no_empty: self.name.ends_with("_no_empty"),
            flags: flags_key,
        });
        let cached_flag_project = flag_project_key
            .as_ref()
            .and_then(|key| projects.flag_projects.get(key).copied());
        let project_setup_start = profile.then(Instant::now);
        let (mut project, diagnostic_config) = if let Some(cached) = cached_flag_project {
            (
                OwnedOrMut::Mut(&mut projects.projects[cached.index]),
                cached.diagnostics,
            )
        } else {
            self.initialize_flags(
                projects,
                &local_fs,
                match flags.cli.mode {
                    Some(mode) => mode.into(),
                    None => ModeChoice::Explicit(mode),
                },
                flags,
                &steps,
                common_slot,
                flag_project_key,
            )
        };
        if let Some(start) = project_setup_start {
            PHASE_TIMINGS.with_borrow_mut(|times| times.project_setup += start.elapsed());
        }

        let is_parse_test = self.file_name.starts_with("parse");
        let is_semanal_test = self.file_name.starts_with("semanal-");

        let mut result = Ok(true);
        let project = project.as_mut();
        if let Some(start) = setup_start {
            PHASE_TIMINGS.with_borrow_mut(|times| times.setup += start.elapsed());
        }
        for (i, (step, parsed_files)) in steps.iter().zip(&fixture_data.parsed_files).enumerate() {
            if cfg!(feature = "zuban_debug") {
                println!(
                    "\nTest: {}: Step {}/{}",
                    self.format(mode),
                    i + 1,
                    steps.len()
                );
            }
            let fixture_start = profile.then(Instant::now);
            let mut wanted = initialize_and_return_wanted_output(
                &local_fs,
                project,
                step,
                parsed_files,
                self.from_mypy_test_suite,
            );
            let mut ide_test_results: Vec<String> = vec![];
            let ide_start = profile.then(Instant::now);
            if !self.from_mypy_test_suite {
                BASE_PATH.with(|base_path| {
                    let mut previously_checked = FastHashSet::default();
                    for step in steps.iter().take(i + 1).rev() {
                        for (path, code) in &step.files {
                            if !previously_checked.insert(path) {
                                continue;
                            }
                            find_and_check_ide_tests(
                                project,
                                base_path,
                                path,
                                code,
                                &mut ide_test_results,
                            );
                        }
                    }
                });
            }
            if let Some(start) = ide_start {
                PHASE_TIMINGS.with_borrow_mut(|times| times.ide += start.elapsed());
            }

            for path in &step.deletions {
                project
                    .close_in_memory_file(&base_path_join(&local_fs, path))
                    .unwrap_or_else(|_| {
                        project
                            .delete_directory_of_in_memory_files(&base_path_join(&local_fs, path))
                            .unwrap()
                    });
            }
            if let Some(start) = fixture_start {
                PHASE_TIMINGS.with_borrow_mut(|times| times.fixture += start.elapsed());
            }
            let diagnostics_start = profile.then(Instant::now);
            let mut diagnostics: Vec<_> = if no_typecheck {
                vec![]
            } else {
                let computed = project.diagnostics().unwrap();
                if let Some(start) = diagnostics_start {
                    PHASE_TIMINGS
                        .with_borrow_mut(|times| times.diagnostic_compute += start.elapsed());
                }
                computed
                    .issues
                    .iter()
                    .filter_map(|d| {
                        if is_semanal_test && !d.is_mypy_semanal_error() {
                            return None;
                        }
                        (!is_parse_test || d.mypy_error_code() == "syntax").then(|| {
                            let mut s = d.as_string(&diagnostic_config, None);
                            if s.starts_with("__main__.py:") {
                                s = s.replace("__main__.py:", "__main__:");
                            }
                            if cfg!(target_os = "windows") {
                                // Safety: OK because we only modify ASCII
                                let bytes = unsafe { s.as_bytes_mut() };
                                for line in bytes.split_mut(|c| *c == b'\n') {
                                    if let Some(colon) = line.iter().position(|c| *c == b':') {
                                        let to_change = &mut line[..colon];
                                        for b in to_change {
                                            if *b == b'\\' {
                                                *b = b'/'
                                            }
                                        }
                                    }
                                }
                            }
                            s
                        })
                    })
                    .collect()
            };
            if let Some(start) = diagnostics_start {
                PHASE_TIMINGS.with_borrow_mut(|times| times.diagnostics += start.elapsed());
            }
            let comparison_start = profile.then(Instant::now);
            if self.from_mypy_test_suite {
                // Remove notes that only exist in Zuban
                diagnostics.retain(|x| !x.contains("note: TypedDicts can only be assigned to"))
            }

            let mut actual = replace_annoyances(diagnostics.join("\n"));
            if is_parse_test && self.file_name == "parse-errors" {
                // For whatever reason mypy uses a different file name for these tests
                actual = actual.replace("__main__:", "file:")
            }
            if self.file_name.starts_with("pythoneval") {
                // pythoneval tests use different file names, because in mypy these are actually
                // created as files.
                // It uses two different ways for convenience...
                let name = self.name.strip_suffix("-xfail").unwrap_or(&self.name);
                let file_name_dot_py = format!("_{}.py:", name);
                let file_name_qualified = format!("_{}.", name);
                for line in wanted.iter_mut() {
                    *line = line.replace("_program.py:", "__main__:");
                    *line = line.replace(&file_name_dot_py, "__main__:");
                    // Since the file name can be in a reveal_type like _fooTest.A, but it should
                    // actually be __main__.A to be closer to what we do.
                    *line = line.replace(&file_name_qualified, "__main__.");
                }
            } else if self.file_name == "pep561" {
                let file_name_dot_py = format!("{}.py:", self.name);
                for line in wanted.iter_mut() {
                    *line = line.replace(&file_name_dot_py, "__main__:");
                }
            }
            let mut actual_lines = actual
                .trim()
                .split('\n')
                .map(|s| {
                    if self.from_mypy_test_suite {
                        s.to_lowercase()
                    } else {
                        s.to_string()
                    }
                })
                .filter_map(temporarily_skip)
                .collect::<Vec<_>>();
            if actual_lines == [""] {
                actual_lines.pop();
            }
            // Add ide_test_results
            for r in &ide_test_results {
                if !actual.is_empty() && !actual.ends_with('\n') {
                    actual.push('\n');
                }
                actual.push_str(r);
                actual.push('\n');
            }
            // Generally we should not sort any of the tests we wrote ourselves, but this might be
            // tricky for now due to determinism, so get closer slowly.
            let should_sort = !ide_test_results.iter().any(|s| s.contains("Symbols:"));
            actual_lines.extend(ide_test_results);

            if should_sort {
                actual_lines.sort();
            }

            let mut wanted_cleaned_up: Vec<_> = wanted
                .iter()
                .map(|s| {
                    if self.from_mypy_test_suite {
                        // For now we want to compare lower cases, because mypy mixes up list[] and
                        // List[]
                        s.to_lowercase()
                    } else {
                        s.to_string()
                    }
                })
                .filter_map(temporarily_skip)
                .collect();
            if should_sort {
                wanted_cleaned_up.sort();
            }

            if wanted_cleaned_up != actual_lines {
                if should_sort {
                    // To check output only sort by filenames, which should be enough.
                    wanted.sort_by_key(|line| line.split(':').next().unwrap().to_owned());
                }

                let wanted = wanted.iter().fold(String::new(), |a, b| a + b + "\n");
                result = Err(anyhow::anyhow!(
                    "\nMismatch:\n\
                     Wanted lines: {wanted_cleaned_up:?}\n\n\
                     Actual lines: {actual_lines:?}\n\n\
                     Wanted:\n\
                     {wanted}\n\
                     Actual:\n\
                     {actual}\n\
                     in {} ({}): Step {}/{}",
                    &self.name,
                    self.file_name,
                    i + 1,
                    steps.len(),
                ));
                break;
            }
            if let Some(start) = comparison_start {
                PHASE_TIMINGS.with_borrow_mut(|times| times.comparison += start.elapsed());
            }
        }
        let cleanup_start = profile.then(Instant::now);
        // The fixture paths are fixed across checker modes and run causes. Keep the same
        // directory cleanup so namespace packages cannot leak into the next case.
        let has_disk_backing =
            *FIXTURE_ROOT_HAS_DISK_BACKING.get_or_init(|| Path::new(BASE_PATH_STR).exists());
        if has_disk_backing {
            project.close_in_memory_files(&fixture_data.flat_files);
        } else {
            project.discard_in_memory_files(&fixture_data.flat_files);
        }
        for directory in &fixture_data.directories {
            let _ = if has_disk_backing {
                project.delete_directory_of_in_memory_files(directory)
            } else {
                project.discard_directory_of_in_memory_files(directory)
            };
        }
        if let Some(start) = cleanup_start {
            PHASE_TIMINGS.with_borrow_mut(|times| times.cleanup += start.elapsed());
        }
        result
    }

    fn format(&self, mode: Mode) -> String {
        format!(
            "{} ({}, {})",
            self.name,
            self.file_name,
            match mode {
                Mode::Mypy => "mypy-compatible",
                Mode::Default => "default",
            }
        )
    }

    #[allow(dead_code)]
    fn default_python_version(&self) -> PythonVersion {
        match self.file_name {
            "check-python312" => PythonVersion::new(3, 12),
            "check-python311" => PythonVersion::new(3, 11),
            "check-python310" => PythonVersion::new(3, 10),
            "check-python39" => PythonVersion::new(3, 9),
            "check-python38" => PythonVersion::new(3, 8),
            _ => PythonVersion::new(3, 8),
        }
    }
}

fn replace_annoyances(s: String) -> String {
    s.replace("builtins.", "")
}

fn temporarily_skip(s: String) -> Option<String> {
    if s.contains(" overlap with incompatible return types")
        && s.contains("overloaded function signatures")
    {
        return None;
    }
    // This callable note is very weird and I'm confused when it appears and when not.
    if s.contains(".__call__\" has type") {
        return None;
    }
    if s.contains("note: types from \"numbers\" aren't supported for static type checking")
        || s.contains("note: consider using a protocol instead, such as typing.supportsfloat")
        || s.contains("https://peps.python.org/pep-0484/#the-numeric-tower")
        // Full note: This is likely because "one" has named arguments: "x". Consider marking them positional-only
        || s.contains("consider marking them positional-only")
    {
        return None;
    }
    Some(s)
}

fn initialize_and_return_wanted_output(
    local_fs: &SimpleLocalFS,
    project: &mut Project,
    step: &Step,
    parsed_files: &[(&str, PathWithScheme, Tree)],
    from_mypy_test_suite: bool,
) -> Vec<String> {
    let profile = profile_phases();
    let wanted_start = profile.then(Instant::now);
    let mut wanted = step
        .out
        .trim()
        .split('\n')
        .filter_map(|mut s| {
            if from_mypy_test_suite {
                cleanup_mypy_issues(s)
            } else {
                if cfg!(target_os = "windows") {
                    s = s.trim_end_matches('\r')
                }
                Some(s.to_string())
            }
        })
        .collect::<Vec<_>>();

    if wanted == [""] {
        wanted.pop();
    }
    let wanted_duration = wanted_start.map_or(Duration::ZERO, |start| start.elapsed());

    let mut independent_files = Vec::new();
    let mut inline_duration = Duration::ZERO;
    let mut store_duration = Duration::ZERO;
    for (path, file_path, tree) in parsed_files {
        let code = tree.code();
        let p = if *path == "__main__.py" {
            "__main__"
        } else {
            path
        };
        let inline_start = profile.then(Instant::now);
        add_inline_errors(&mut wanted, p, code, from_mypy_test_suite);
        if let Some(start) = inline_start {
            inline_duration += start.elapsed();
        }
        // testAbstractClassSubclasses
        if let Some(parent) = maybe_parent(local_fs, code) {
            let store_start = profile.then(Instant::now);
            project
                .store_in_memory_files_preparsed_for_tests(std::mem::take(&mut independent_files));
            project
                .store_in_memory_file_with_parent(file_path.clone(), code.into(), &parent)
                .unwrap();
            if let Some(start) = store_start {
                store_duration += start.elapsed();
            }
        } else {
            independent_files.push((file_path.clone(), tree.clone()));
        }
    }
    let store_start = profile.then(Instant::now);
    project.store_in_memory_files_preparsed_for_tests(independent_files);
    if let Some(start) = store_start {
        store_duration += start.elapsed();
    }
    let normalize_start = profile.then(Instant::now);
    for line in &mut wanted {
        replace_unions(line);
        replace_optional(line);
    }
    if let Some(start) = normalize_start {
        PHASE_TIMINGS.with_borrow_mut(|times| {
            times.wanted += wanted_duration;
            times.inline_errors += inline_duration;
            times.file_store += store_duration;
            times.wanted_normalization += start.elapsed();
        });
    }
    wanted
}

fn maybe_parent(local_fs: &SimpleLocalFS, code: &str) -> Option<PathWithScheme> {
    let parent = code.strip_prefix("# parent: ")?;
    let path = parent.split(['\n', '\r']).next()?;
    Some(base_path_join(local_fs, path))
}

fn add_inline_errors(wanted: &mut Vec<String>, path: &str, code: &str, from_mypy_test_suite: bool) {
    if !code.contains("# E:") && !code.contains("# N:") {
        return;
    }
    let lines: Box<_> = code.split('\n').collect();
    for (line_nr, column, mut type_, comment) in
        ErrorCommentsOnCode(&lines, lines.iter().enumerate())
    {
        for comment in comment.split(" # E: ") {
            for (i, mut comment) in comment.split(" # N: ").enumerate() {
                if i != 0 {
                    type_ = "note";
                }
                if comment == "\\" || comment == "\\\r" {
                    // Stuff like:
                    // # E: \
                    continue;
                }
                let mut add_comment = |comment: &str| {
                    if let Some(column) = column {
                        wanted.push(format!(
                            "{path}:{line_nr}:{column}: {type_}: {}",
                            comment.trim_end()
                        ))
                    } else {
                        wanted.push(format!("{path}:{line_nr}: {type_}: {}", comment.trim_end()))
                    }
                };
                if from_mypy_test_suite {
                    if let Some(comment) = cleanup_mypy_issues(comment) {
                        add_comment(&comment)
                    }
                } else {
                    if cfg!(target_os = "windows") {
                        comment = comment.trim_end_matches('\r')
                    }
                    if comment.ends_with(" \\") {
                        comment = &comment[..comment.len() - 2];
                    }
                    add_comment(comment)
                }
            }
        }
    }
}

fn replace_unions(line: &mut String) {
    // Replaces Union[int, str] with int | str
    while let Some(index) = line.rfind("Union[") {
        let (end, commas) = find_end_bracket(&line[index..]);
        line.replace_range(index + end..index + end + 1, "");
        for i in commas.iter().rev() {
            line.replace_range(index + i..index + i + 1, " |");
        }
        line.replace_range(index..index + "Union[".len(), "");
    }
}

fn replace_optional(line: &mut String) {
    // Replaces Optional[int] -> int | None
    while let Some(index) = line.rfind("Optional[") {
        let s = &line[index..];
        if s.starts_with("Optional[...] must have exactly one type argument") {
            return;
        }
        let end = find_end_bracket(s).0;
        line.replace_range(index + end..index + end + 1, " | None");
        line.replace_range(index..index + "Optional[".len(), "");
    }
}

fn find_end_bracket(s: &str) -> (usize, Vec<usize>) {
    let mut brackets = 0;
    let mut commas = vec![];
    let mut end = 0;
    for (i, chr) in s.char_indices() {
        match chr {
            '[' | '(' | '{' => brackets += 1,
            ']' | ')' | '}' => {
                brackets -= 1;
                if brackets == 0 {
                    end = i;
                    break;
                }
            }
            ',' => {
                if brackets == 1 {
                    commas.push(i);
                }
            }
            _ => (),
        }
    }
    assert_eq!(brackets, 0);
    assert_ne!(end, 0);
    (end, commas)
}

struct ErrorCommentsOnCode<'a>(
    &'a [&'a str],
    std::iter::Enumerate<std::slice::Iter<'a, &'a str>>,
);

impl Iterator for ErrorCommentsOnCode<'_> {
    type Item = (usize, Option<usize>, &'static str, String);
    fn next(&mut self) -> Option<Self::Item> {
        for (i, line) in &mut self.1 {
            let was_exception = line.find("# E:");
            if let Some(pos) = was_exception.or_else(|| line.find("# N:")) {
                let mut backslashes = 0;
                if line.trim_start().starts_with("#") {
                    for i in (0..i).rev() {
                        if !(self.0[i].trim_end_matches('\r').ends_with('\\')) {
                            break;
                        }
                        backslashes += 1;
                    }
                }
                // Get rid of # E:25: Foo
                let mut rest = &line[pos + 4..];
                let first = rest.split(':').next().unwrap();
                let column: Option<usize> = first.parse().ok();
                if column.is_some() {
                    rest = &rest[first.len() + 1..];
                }

                return Some((
                    i + 1 - backslashes,
                    column,
                    match was_exception {
                        Some(_) => "error",
                        None => "note",
                    },
                    rest.get(1..).unwrap_or(rest).to_string(),
                ));
            }
        }
        None
    }
}

fn cleanup_mypy_issues(mut s: &str) -> Option<String> {
    if s.contains("See https://mypy.readthedocs.io/en/stable/running_mypy.html#missing-imports") {
        return None;
    }
    if s.contains("\" defined in ") && s.trim_end().ends_with('"')
        || s.contains("Flipping the order of overloads will fix this error")
        || s.contains("Maybe you forgot to use \"await\"?")
    {
        // TODO we might not want to skip this note in the future.
        return None;
    }
    if cfg!(target_os = "windows") {
        s = s.trim_end_matches('\r')
    }

    if s.ends_with(" \\") {
        s = &s[..s.len() - 2];
    }
    let s = REPLACE_TUPLE.replace_all(s, TypeStuffReplacer());
    let s = REPLACE_MYPY.replace_all(&s, "");
    // Mypy has a bit of a different handling for ellipsis when reading it from typeshed.
    let s = REPLACE_MYPY_ELLIPSIS.replace_all(&s, "\"EllipsisType\"");
    let s = REPLACE_MYPY_NO_RETURN.replace_all(&s, "Never");
    let s = REPLACE_MYPY_SUBTYPE_NUMBERS.replace_all(&s, "$1");
    let s = REPLACE_MYPY_SUBTYPE.replace_all(&s, "$1");
    let s = REPLACE_ESCAPES.replace_all(&s, "[");
    let s = REPLACE_MAIN.replace(&s, "__main__:");
    Some(replace_annoyances(s.replace("tmp/", "")))
}

struct TypeStuffReplacer();

impl Replacer for TypeStuffReplacer {
    fn replace_append(&mut self, _caps: &Captures<'_>, dst: &mut String) {
        if dst.ends_with("(got \"")
            || dst.ends_with(", expected \"")
            || dst.ends_with("has type \"")
        {
            dst.push_str("tuple")
        } else {
            dst.push_str("builtins.tuple")
        }
    }
}

fn calculate_filters(args: &[String]) -> Vec<&str> {
    let mut filters = vec![];
    for s in args.iter() {
        filters.push(s.as_str())
    }
    filters
}

struct ProjectsCache {
    command: Command,
    base_projects: Option<FastHashMap<BaseProjectKey, Project>>,
    map: FastHashMap<(Settings, TypeCheckerFlags), usize>,
    projects: Vec<Project>,
    common_projects: FastHashMap<String, [Option<CommonProject>; 4]>,
    flag_projects: FastHashMap<FlagProjectKey, CommonProject>,
    run_cause: RunCause,
    flags_cache: FastHashMap<Vec<String>, PerTestFlags>,
}

type BaseProjectKey = (Option<PythonVersion>, Option<Arc<NormalizedPath>>);

fn set_mypy_path(options: &mut ProjectOptions) {
    options.settings.add_global_packages_default = false;
    BASE_PATH.with(|base_path| {
        options.settings.mypy_path.push(base_path.clone());
    })
}

fn base_path_join(local_fs: &dyn VfsHandler, other: &str) -> PathWithScheme {
    PathWithScheme::with_file_scheme(
        BASE_PATH.with(|base_path| local_fs.normalize_rc_path(local_fs.join(base_path, other))),
    )
}

impl ProjectsCache {
    fn new(reuse_db: bool, run_cause: RunCause) -> Self {
        let mut po = ProjectOptions::mypy_default();
        po.settings.typeshed_path = Some(test_utils::typeshed_path());
        set_mypy_path(&mut po);
        let base_projects = reuse_db.then(|| {
            let mut bases = FastHashMap::default();
            let key = (
                po.settings.python_version,
                po.settings.typeshed_path.clone(),
            );
            bases.insert(key, Project::without_watcher(po, run_cause));
            bases
        });
        Self {
            command: PerTestFlags::command(),
            base_projects,
            run_cause,
            flags_cache: FastHashMap::default(),
            map: Default::default(),
            projects: Vec::new(),
            common_projects: FastHashMap::default(),
            flag_projects: FastHashMap::default(),
        }
    }

    fn get_or_create_index(&mut self, mut settings: Settings, flags: TypeCheckerFlags) -> usize {
        settings.typeshed_path = Some(test_utils::typeshed_path());
        let key = (settings, flags);
        if let Some(&index) = self.map.get(&key) {
            return index;
        }
        let mut options = ProjectOptions::new(key.0.clone(), key.1.clone());
        set_mypy_path(&mut options);
        let project = self.try_to_reuse_project_parts(options);
        let index = self.projects.len();
        self.projects.push(project);
        self.map.insert(key, index);
        index
    }

    fn try_to_reuse_project_parts(&mut self, mut options: ProjectOptions) -> Project {
        let start = profile_phases().then(Instant::now);
        options
            .settings
            .typeshed_path
            .get_or_insert_with(test_utils::typeshed_path);
        let reused = self.base_projects.is_some();
        let project = if let Some(base_projects) = self.base_projects.as_mut() {
            // Parsed stubs depend on both Python version and typeshed root.
            let key = (
                options.settings.python_version,
                options.settings.typeshed_path.clone(),
            );
            let base_project = base_projects.entry(key).or_insert_with(|| {
                let mut base_options = ProjectOptions::mypy_default();
                base_options.settings.python_version = options.settings.python_version;
                base_options.settings.typeshed_path = options.settings.typeshed_path.clone();
                set_mypy_path(&mut base_options);
                Project::without_watcher(base_options, self.run_cause)
            });
            base_project.try_to_reuse_project_resources_for_tests(options)
        } else {
            Project::without_watcher(options, self.run_cause)
        };
        if let Some(start) = start {
            PHASE_TIMINGS.with_borrow_mut(|times| {
                if reused {
                    times.project_reuse += start.elapsed();
                    times.project_reuse_count += 1;
                } else {
                    times.project_fresh += start.elapsed();
                    times.project_fresh_count += 1;
                }
            });
        }
        project
    }
}

enum OwnedOrMut<'a, T> {
    Owned(T),
    Mut(&'a mut T),
}

impl<'a, T> OwnedOrMut<'a, T> {
    pub fn as_mut(&mut self) -> &mut T {
        match self {
            OwnedOrMut::Owned(t) => t,
            OwnedOrMut::Mut(t) => t,
        }
    }
}

fn main() -> ExitCode {
    logging_config::setup_logging(None).unwrap();
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Ok(current) = CURRENT_TEST.try_lock()
            && let Some((name, file_name)) = current.as_ref()
        {
            println!("Panic in {name} ({file_name})");
        }
        default_panic(info);
    }));

    // Avoid the --, because that's the only way how we can accept flags like --foo like
    // cargo test mypy -- -- --foo. Otherwise libtest? complains, if we just use -- once.
    let cli_args = CliArgs::parse_from(env::args().filter(|arg| arg != "--"));
    let filters = calculate_filters(&cli_args.filters);

    let skipped = skipped();

    let files: Vec<_> = find_mypy_style_files()
        .into_iter()
        .map(|(from_mypy_test_suite, file)| {
            let code = read_to_string(&file).unwrap();
            let code = REPLACE_COMMENTS.replace_all(&code, "").into_owned();
            let file_name = file.file_stem().unwrap().to_str().unwrap().to_owned();
            (from_mypy_test_suite, file_name, code)
        })
        .collect();
    let cases_by_file: Vec<_> = files
        .iter()
        .map(|(from_mypy_test_suite, file_name, code)| {
            (
                file_name.as_str(),
                mypy_style_cases(file_name, code, *from_mypy_test_suite),
            )
        })
        .collect();

    let mut error_count = 0;
    if !cli_args.only_language_server {
        error_count += run(
            &cli_args,
            RunCause::TypeChecking,
            &cases_by_file,
            &skipped,
            &filters,
        );
    }
    if !cli_args.only_typecheck && error_count == 0 {
        error_count += run(
            &cli_args,
            RunCause::LanguageServer,
            &cases_by_file,
            &skipped,
            &filters,
        )
    }

    ExitCode::from((error_count > 0) as u8)
}

fn run(
    cli_args: &CliArgs,
    run_cause: RunCause,
    cases_by_file: &[(&str, Vec<TestCase<'_, '_>>)],
    skipped: &SkippedIndex,
    filters: &[&str],
) -> usize {
    let start = Instant::now();
    if profile_phases() {
        PHASE_TIMINGS.with_borrow_mut(|times| *times = PhaseTimings::default());
    }
    let mut projects = ProjectsCache::new(!cli_args.no_reuse_db, run_cause);
    let mut full_count = 0;
    let mut passed_count = 0;
    let mut error_count = 0;
    let file_count = cases_by_file.len();
    let mut error_summary = String::new();
    let mut allowed_to_run_when_start_at = false;
    let mut timings = Timings::new(cli_args.slowest.unwrap_or_default());
    for (file_name, cases) in cases_by_file {
        for case in cases {
            if !filters.is_empty()
                && !filters.contains(&&*case.name)
                && !filters.contains(file_name)
            {
                continue;
            }
            if let Some(name) = &cli_args.start_at {
                if case.name == *name || file_name == name {
                    allowed_to_run_when_start_at = true;
                } else if !allowed_to_run_when_start_at {
                    continue;
                }
            }
            if skipped.is_skip(case.file_name, &case.name) && !filters.contains(&&*case.name) {
                //println!("Skipped: {}", case.name);
                full_count += 1;
                continue;
            }
            let mut check = |result, mode| match result {
                Ok(ran) => {
                    passed_count += ran as usize;
                    full_count += ran as usize;
                }
                Err(err) => {
                    full_count += 1;
                    if cli_args.stop_after_first_error {
                        panic!("{err}")
                    } else {
                        error_summary += &case.format(mode);
                        error_summary += "\n";
                        error_count += 1;
                        println!("{err}")
                    }
                }
            };
            if !case.from_mypy_test_suite {
                timings.run(case.file_name, &case.name, || {
                    // Run our own tests both with mypy-compatible and without it.
                    check(case.run(&mut projects, Mode::Default), Mode::Default)
                })
            }
            timings.run(case.file_name, &case.name, || {
                check(case.run(&mut projects, Mode::Mypy), Mode::Mypy)
            })
        }
    }
    if error_count > 0 {
        println!("\nError summary:");
        println!("{error_summary}");
    }
    let error_s = match error_count {
        1 => "",
        _ => "s",
    };
    println!(
        "Passed {passed_count} of {full_count} ({error_count} error{error_s}) \
         mypy-like tests in {file_count} files; finished in {:.2}s (cause={})",
        start.elapsed().as_secs_f32(),
        match run_cause {
            RunCause::TypeChecking => "type-checking",
            RunCause::LanguageServer => "language-server",
        }
    );
    timings.print_slowest_if_wanted();
    if profile_phases() {
        PHASE_TIMINGS.with_borrow(|times| {
            println!(
                "Phase timings (s): setup={:.3} project_setup={:.3} fixture={:.3} ide={:.3} diagnostics={:.3} comparison={:.3} cleanup={:.3}; projects={} bases={} flagged={} common={}",
                times.setup.as_secs_f64(),
                times.project_setup.as_secs_f64(),
                times.fixture.as_secs_f64(),
                times.ide.as_secs_f64(),
                times.diagnostics.as_secs_f64(),
                times.comparison.as_secs_f64(),
                times.cleanup.as_secs_f64(),
                projects.projects.len(),
                projects.base_projects.as_ref().map_or(0, |bases| bases.len()),
                projects.flag_projects.len(),
                projects.common_projects.len(),
            );
            println!(
                "Project construction (s): reused={:.3}/{} fresh={:.3}/{}",
                times.project_reuse.as_secs_f64(),
                times.project_reuse_count,
                times.project_fresh.as_secs_f64(),
                times.project_fresh_count,
            );
            println!(
                "Fixture detail (s): wanted={:.3} inline={:.3} store={:.3} normalize={:.3}",
                times.wanted.as_secs_f64(),
                times.inline_errors.as_secs_f64(),
                times.file_store.as_secs_f64(),
                times.wanted_normalization.as_secs_f64(),
            );
            println!(
                "Diagnostic compute (s): {:.3} of {:.3} total",
                times.diagnostic_compute.as_secs_f64(),
                times.diagnostics.as_secs_f64(),
            );
        });
    }
    error_count
}

fn mypy_style_cases<'a, 'b>(
    file_name: &'a str,
    code: &'b str,
    from_mypy_test_suite: bool,
) -> Vec<TestCase<'a, 'b>> {
    let mut cases = vec![];

    let mut add = |name, start, end| {
        cases.push(TestCase {
            file_name,
            name,
            code: &code[start..end],
            from_mypy_test_suite,
            steps: OnceCell::new(),
            fixture_data: OnceCell::new(),
            parsed_flags: OnceCell::new(),
        });
    };

    let mut start = None;
    let mut next_name = None;
    for capture in CASE.captures_iter(code) {
        if let Some(start) = start {
            add(
                next_name.take().unwrap(),
                start,
                capture.get(0).unwrap().start(),
            );
        }
        next_name = Some(capture.get(1).unwrap().as_str().to_owned());
        start = Some(capture.get(0).unwrap().end())
    }

    add(
        next_name.unwrap_or_else(|| panic!("File without test cases: {:?}", file_name)),
        start.unwrap(),
        code.len(),
    );
    cases
}

fn get_base() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("mypylike")
}

fn find_mypy_style_files() -> Vec<(bool, PathBuf)> {
    let base = get_base();

    // Include local tests
    let mut path = base.clone();
    path.push("tests");

    let mut our_own_tests: Vec<_> = read_dir(path)
        .unwrap()
        .map(|res| (false, res.unwrap().path()))
        .filter(|(_, p)| {
            p.extension()
                .is_some_and(|ext| ext.to_str().unwrap() == "test")
        })
        .collect();

    our_own_tests.sort();

    let mut mypy_test_path = base;
    mypy_test_path.extend(["mypy", "test-data", "unit"]);
    // Include mypy tests
    let mut entries: Vec<_> = read_dir(mypy_test_path)
        .unwrap()
        .filter_map(|res| {
            let entry = res.unwrap();
            let name = entry.file_name().into_string().unwrap();
            if !name.ends_with(".test") || SKIP_MYPY_TEST_FILES.contains(&name.as_str()) {
                None
            } else {
                Some((true, entry.path()))
            }
        })
        .collect();

    entries.sort();
    entries.extend(our_own_tests);
    entries
}

#[derive(Debug)]
struct Skipped {
    name: String,
    only_for_file: Option<String>,
    start_star: bool,
    end_star: bool,
}

struct SkippedIndex {
    exact: FastHashSet<String>,
    exact_for_file: FastHashMap<String, FastHashSet<String>>,
    patterns: Vec<Skipped>,
}

impl SkippedIndex {
    fn is_skip(&self, file_name: &str, name: &str) -> bool {
        self.exact.contains(name)
            || self
                .exact_for_file
                .get(file_name)
                .is_some_and(|names| names.contains(name))
            || self
                .patterns
                .iter()
                .any(|skip| skip.is_skip(file_name, name))
    }
}

impl Skipped {
    fn is_skip(&self, file_name: &str, name: &str) -> bool {
        if let Some(only_for_file) = self.only_for_file.as_ref()
            && file_name != only_for_file
        {
            return false;
        }
        if self.start_star && self.end_star {
            name.contains(&self.name)
        } else if self.start_star {
            name.ends_with(&self.name)
        } else if self.end_star {
            name.starts_with(&self.name)
        } else {
            self.name == name
        }
    }
}

fn skipped() -> SkippedIndex {
    let mut skipped_path = get_base();
    skipped_path.push("skipped");
    let file = read_to_string(skipped_path).unwrap();

    let mut index = SkippedIndex {
        exact: FastHashSet::default(),
        exact_for_file: FastHashMap::default(),
        patterns: Vec::new(),
    };
    for skip in file
        .trim()
        .split('\n')
        .filter(|line| !line.starts_with('#'))
        .map(|mut x| {
            if cfg!(target_os = "windows") {
                x = x.trim_end_matches('\r')
            }
            let start_star = x.starts_with('*');
            let end_star = x.ends_with('*');
            if start_star {
                x = &x[1..];
            }
            if end_star {
                x = &x[..x.len() - 1]
            }
            let mut split_by_colon = x.split(':');
            let first = split_by_colon.next().unwrap();
            let mut only_for_file = None;
            if let Some(rest) = split_by_colon.next() {
                assert_eq!(split_by_colon.next(), None);
                only_for_file = Some(first.into());
                x = rest;
            }

            Skipped {
                name: x.to_owned(),
                only_for_file,
                start_star,
                end_star,
            }
        })
    {
        if skip.start_star || skip.end_star {
            index.patterns.push(skip);
        } else if let Some(file_name) = skip.only_for_file {
            index
                .exact_for_file
                .entry(file_name)
                .or_default()
                .insert(skip.name);
        } else {
            index.exact.insert(skip.name);
        }
    }
    index
}

struct Timings {
    limit_count: usize,
    slowest: BinaryHeap<Timing>,
}

struct Timing {
    duration: f64,
    file_name: String,
    test_name: String,
}

impl Eq for Timing {}

impl PartialEq for Timing {
    fn eq(&self, other: &Self) -> bool {
        self.duration == other.duration
    }
}

impl Ord for Timing {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse comparison -> min-heap.
        other.duration.total_cmp(&self.duration)
    }
}

impl PartialOrd for Timing {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Timings {
    fn new(limit_count: usize) -> Self {
        Self {
            limit_count,
            slowest: Default::default(),
        }
    }

    fn run(&mut self, file_name: &str, test_name: &str, callback: impl FnOnce()) {
        let start = Instant::now();
        callback();

        let duration = start.elapsed().as_secs_f64();
        let create_timing = || Timing {
            duration,
            file_name: file_name.into(),
            test_name: test_name.into(),
        };

        if self.limit_count == 0 {
            return;
        }

        if self.slowest.len() < self.limit_count {
            self.slowest.push(create_timing());
        } else if duration > self.slowest.peek().unwrap().duration {
            self.slowest.pop();
            self.slowest.push(create_timing());
        }
    }

    fn print_slowest_if_wanted(self) {
        if self.limit_count > 0 {
            println!("\nSlowest tests:");
            for timing in self.slowest.into_sorted_vec() {
                println!(
                    "{:.3}s\t{} ({})",
                    timing.duration, timing.test_name, timing.file_name
                )
            }
        }
    }
}
