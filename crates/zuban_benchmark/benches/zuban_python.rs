use std::time::{Duration, Instant};

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};

use config::ProjectOptions;
use lsp_types::{Position, Range, TextDocumentContentChangeEvent};
use parsa_python_cst::Tree;
use vfs::{LocalFS, PathWithScheme, VfsHandler};
use zuban_python::{InputPosition, Project, RunCause};

fn prepare_independent_files(file_count: usize) -> (Project, Vec<PathWithScheme>) {
    let some_code = utils::dedent(
        r#"
        def foo(x: int) -> int:
            return x
    "#,
    );
    let mut po = ProjectOptions::default();
    po.settings.typeshed_path = Some(test_utils::typeshed_path());
    let mut project = Project::without_watcher(po, RunCause::TypeChecking);
    let mut paths = Vec::with_capacity(file_count);
    for i in 0..file_count {
        let vfs = project.vfs_handler();
        let path = PathWithScheme::with_file_scheme(
            vfs.normalize_rc_path(vfs.unchecked_abs_path(&format!("/bench-test/test{i}.py"))),
        );
        project.store_in_memory_file(path.clone(), some_code.clone().into());
        paths.push(path);
    }
    (project, paths)
}

fn prepare_deep_files(file_count: usize) -> (Project, Vec<PathWithScheme>) {
    let code = utils::dedent(
        r#"
        def foo(x: int) -> int:
            return x
    "#,
    );
    let fs = LocalFS::without_watcher();
    let mut options = ProjectOptions::default();
    options.settings.typeshed_path = Some(test_utils::typeshed_path());
    options
        .settings
        .mypy_path
        .push(fs.normalize_rc_path(fs.unchecked_abs_path("/bench-deep")));
    let mut project = Project::without_watcher(options, RunCause::TypeChecking);
    let mut paths = Vec::with_capacity(file_count);
    for i in 0..file_count {
        let vfs = project.vfs_handler();
        let path = PathWithScheme::with_file_scheme(vfs.normalize_rc_path(
            vfs.unchecked_abs_path(&format!("/bench-deep/a/b/c/d/e/test{i}.py")),
        ));
        project.store_in_memory_file(path.clone(), code.clone().into());
        paths.push(path);
    }
    (project, paths)
}

fn prepare_independent_files_batched(file_count: usize) -> (Project, Vec<PathWithScheme>) {
    let some_code = utils::dedent(
        r#"
        def foo(x: int) -> int:
            return x
    "#,
    );
    let mut po = ProjectOptions::default();
    po.settings.typeshed_path = Some(test_utils::typeshed_path());
    let mut project = Project::without_watcher(po, RunCause::TypeChecking);
    let mut paths = Vec::with_capacity(file_count);
    let mut files = Vec::with_capacity(file_count);
    for i in 0..file_count {
        let vfs = project.vfs_handler();
        let path = PathWithScheme::with_file_scheme(
            vfs.normalize_rc_path(vfs.unchecked_abs_path(&format!("/bench-test/test{i}.py"))),
        );
        files.push((path.clone(), some_code.clone().into()));
        paths.push(path);
    }
    project.store_in_memory_files(files);
    (project, paths)
}

fn check_independent_files(file_count: usize) {
    let (mut project, paths) = prepare_independent_files(file_count);
    check_prepared_files(&mut project, paths);
}

fn check_prepared_files(project: &mut Project, paths: Vec<PathWithScheme>) {
    for path in paths {
        let mut document = project
            .document(&path)
            .expect("stored file must be available");
        std::hint::black_box(document.diagnostics());
    }
}

fn prepare_reusable_project() -> (Project, ProjectOptions) {
    let mut options = ProjectOptions::mypy_default();
    options.settings.typeshed_path = Some(test_utils::typeshed_path());
    options.settings.add_global_packages_default = false;
    let fs = LocalFS::without_watcher();
    let base_path = if cfg!(windows) {
        r"C:\mypylike\"
    } else {
        "/mypylike/"
    };
    options
        .settings
        .mypy_path
        .push(fs.normalize_rc_path(fs.unchecked_abs_path(base_path)));
    let base = Project::without_watcher(options.clone(), RunCause::TypeChecking);
    (base, options)
}

fn bench_warm_edit_stage(c: &mut Criterion, name: &str, measure_update: bool, deep: bool) {
    let (mut project, paths) = if deep {
        prepare_deep_files(1000)
    } else {
        prepare_independent_files(1000)
    };
    check_prepared_files(&mut project, paths.clone());
    let edited_path = paths[0].clone();
    let original = utils::dedent(
        r#"
        def foo(x: int) -> int:
            return x
    "#,
    )
    .into_boxed_str();
    let updated = utils::dedent(
        r#"
        def foo(x: int) -> int:
            return "wrong"
    "#,
    )
    .into_boxed_str();
    let mut use_updated = false;
    c.bench_function(name, |b| {
        b.iter_custom(|iters| {
            let mut measured = Duration::ZERO;
            for _ in 0..iters {
                use_updated = !use_updated;
                let code = if use_updated { &updated } else { &original };
                let update_start = Instant::now();
                project.store_in_memory_file(edited_path.clone(), code.clone());
                let update_time = update_start.elapsed();
                let mut document = project.document(&edited_path).unwrap();
                let check_start = Instant::now();
                let diagnostics = document.diagnostics();
                let check_time = check_start.elapsed();
                assert_eq!(diagnostics.len(), usize::from(use_updated));
                std::hint::black_box(diagnostics);
                measured += if measure_update {
                    update_time
                } else {
                    check_time
                };
            }
            measured
        })
    });
}

fn bench_warm_lsp_edit(c: &mut Criterion) {
    let (mut project, paths) = prepare_independent_files(1000);
    check_prepared_files(&mut project, paths.clone());
    let edited_path = paths[0].clone();
    let original = utils::dedent(
        r#"
        def foo(x: int) -> int:
            return x
    "#,
    );
    let updated = original.replacen("return x", "return \"wrong\"", 1);
    let mut use_updated = false;
    c.bench_function("LSP edit and check 1 of 1000 prepared files", |b| {
        b.iter(|| {
            let range = if use_updated { 18 } else { 12 };
            let text = if use_updated { "x" } else { "\"wrong\"" };
            let change = TextDocumentContentChangeEvent {
                range: Some(Range::new(Position::new(2, 11), Position::new(2, range))),
                range_length: None,
                text: text.into(),
            };
            project
                .store_file_with_lsp_changes(edited_path.clone(), vec![change], |pos| {
                    InputPosition::Utf16CodeUnits {
                        line: pos.line as usize,
                        column: pos.character as usize,
                    }
                })
                .unwrap();
            use_updated = !use_updated;
            assert_eq!(
                project
                    .code_of_in_memory_file(&edited_path)
                    .unwrap()
                    .trim_end(),
                if use_updated { &updated } else { &original }.trim_end()
            );
            let mut document = project.document(&edited_path).unwrap();
            let diagnostics = document.diagnostics();
            assert_eq!(diagnostics.len(), usize::from(use_updated));
            std::hint::black_box(diagnostics);
        })
    });
}

fn bench_warm_multi_lsp_edit(c: &mut Criterion, edit_count: u32) {
    let (mut project, paths) = prepare_independent_files(1000);
    let edited_path = paths[0].clone();
    let mut code = String::from("def foo(x: int) -> int:\n    return x\n");
    for _ in 0..edit_count {
        code.push_str("# a\n");
    }
    project.store_in_memory_file(edited_path.clone(), code.into_boxed_str());
    check_prepared_files(&mut project, paths);
    let mut use_b = false;
    let name = format!("{edit_count} reverse LSP edits and check prepared file");
    c.bench_function(&name, |b| {
        b.iter(|| {
            let replacement = if use_b { "a" } else { "b" };
            let changes = (2..2 + edit_count)
                .rev()
                .map(|line| TextDocumentContentChangeEvent {
                    range: Some(Range::new(Position::new(line, 2), Position::new(line, 3))),
                    range_length: None,
                    text: replacement.into(),
                })
                .collect();
            project
                .store_file_with_lsp_changes(edited_path.clone(), changes, |pos| {
                    InputPosition::Utf16CodeUnits {
                        line: pos.line as usize,
                        column: pos.character as usize,
                    }
                })
                .unwrap();
            use_b = !use_b;
            let code = project.code_of_in_memory_file(&edited_path).unwrap();
            assert_eq!(
                code.as_bytes()[code.len() - 2],
                if use_b { b'b' } else { b'a' }
            );
            let mut document = project.document(&edited_path).unwrap();
            let diagnostics = document.diagnostics();
            assert!(diagnostics.is_empty());
            std::hint::black_box(diagnostics);
        })
    });
}

fn bench_type_checking(c: &mut Criterion) {
    let small_code = utils::dedent(
        r#"
        def foo(x: int) -> int:
            return "wrong"
    "#,
    )
    .into_boxed_str();
    c.bench_function("parse edited small file", |b| {
        b.iter(|| std::hint::black_box(Tree::parse(small_code.clone())))
    });
    let assignments = (0..100)
        .map(|i| format!("x{i} = {i}\n"))
        .collect::<Vec<_>>()
        .concat()
        .into_boxed_str();
    c.bench_function("parse 100 assignments", |b| {
        b.iter(|| std::hint::black_box(Tree::parse(assignments.clone())))
    });
    let chained_assignments = (0..100)
        .map(|i| format!("x{i} = y{i} = z{i}\n"))
        .collect::<Vec<_>>()
        .concat()
        .into_boxed_str();
    c.bench_function("parse 100 chained assignments", |b| {
        b.iter(|| std::hint::black_box(Tree::parse(chained_assignments.clone())))
    });
    let comments = "# a comment\n".repeat(100).into_boxed_str();
    c.bench_function("parse 100 comments", |b| {
        b.iter(|| std::hint::black_box(Tree::parse(comments.clone())))
    });
    for depth in [10, 25, 50] {
        let nested = format!("x = {}1{}\n", "(".repeat(depth), ")".repeat(depth)).into_boxed_str();
        let name = format!("parse {depth} nested parentheses");
        c.bench_function(&name, |b| {
            b.iter(|| std::hint::black_box(Tree::parse(nested.clone())))
        });
    }
    let (mut unchanged_project, unchanged_paths) = prepare_independent_files(1000);
    let unchanged_path = unchanged_paths[0].clone();
    let unchanged_code = utils::dedent(
        r#"
        def foo(x: int) -> int:
            return x
    "#,
    )
    .into_boxed_str();
    c.bench_function("store unchanged file among 1000", |b| {
        b.iter(|| {
            unchanged_project.store_in_memory_file(unchanged_path.clone(), unchanged_code.clone())
        })
    });
    c.bench_function("project setup", |b| {
        b.iter(|| {
            let mut options = ProjectOptions::default();
            options.settings.typeshed_path = Some(test_utils::typeshed_path());
            std::hint::black_box(Project::without_watcher(options, RunCause::TypeChecking))
        })
    });
    let (mut base_project, reuse_options) = prepare_reusable_project();
    c.bench_function("project reuse for tests", |b| {
        b.iter(|| {
            std::hint::black_box(
                base_project.try_to_reuse_project_resources_for_tests(reuse_options.clone()),
            )
        })
    });
    c.bench_function("insert 1000 files", |b| {
        b.iter(|| std::hint::black_box(prepare_independent_files(1000)))
    });
    c.bench_function("insert 1000 files batched", |b| {
        b.iter(|| std::hint::black_box(prepare_independent_files_batched(1000)))
    });
    c.bench_function("type check 1 file", |b| {
        b.iter(|| check_independent_files(1))
    });
    c.bench_function("type check 1000 files", |b| {
        b.iter(|| check_independent_files(1000))
    });
    c.bench_function("type check 1000 prepared files", |b| {
        b.iter_batched(
            || prepare_independent_files(1000),
            |(mut project, paths)| check_prepared_files(&mut project, paths),
            BatchSize::SmallInput,
        )
    });
    let (mut project, paths) = prepare_independent_files(1000);
    check_prepared_files(&mut project, paths.clone());
    let edited_path = paths[0].clone();
    let original = utils::dedent(
        r#"
        def foo(x: int) -> int:
            return x
    "#,
    )
    .into_boxed_str();
    let updated = utils::dedent(
        r#"
        def foo(x: int) -> int:
            return "wrong"
    "#,
    )
    .into_boxed_str();
    let mut use_updated = false;
    c.bench_function("edit and check 1 of 1000 prepared files", |b| {
        b.iter(|| {
            use_updated = !use_updated;
            let code = if use_updated { &updated } else { &original };
            project.store_in_memory_file(edited_path.clone(), code.clone());
            let mut document = project.document(&edited_path).unwrap();
            let diagnostics = document.diagnostics();
            assert_eq!(diagnostics.len(), usize::from(use_updated));
            std::hint::black_box(diagnostics);
        })
    });
    bench_warm_edit_stage(c, "edit stage of 1 of 1000 prepared files", true, false);
    bench_warm_edit_stage(c, "check stage of 1 of 1000 prepared files", false, false);
    bench_warm_edit_stage(c, "edit stage of deep prepared file", true, true);
    bench_warm_edit_stage(c, "check stage of deep prepared file", false, true);
    bench_warm_lsp_edit(c);
    bench_warm_multi_lsp_edit(c, 32);
    bench_warm_multi_lsp_edit(c, 256);
}

// Register the benchmarks
criterion_group!(benches, bench_type_checking);
criterion_main!(benches);
