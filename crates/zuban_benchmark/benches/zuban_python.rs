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

fn bench_unchanged_lsp_edit(c: &mut Criterion) {
    let (mut project, paths) = prepare_independent_files(1000);
    check_prepared_files(&mut project, paths.clone());
    let path = paths[0].clone();
    c.bench_function("unchanged LSP edit among 1000 prepared files", |b| {
        b.iter(|| {
            let change = TextDocumentContentChangeEvent {
                range: Some(Range::new(Position::new(2, 11), Position::new(2, 12))),
                range_length: None,
                text: "x".into(),
            };
            project
                .store_file_with_lsp_changes(path.clone(), vec![change], |pos| {
                    InputPosition::Utf16CodeUnits {
                        line: pos.line as usize,
                        column: pos.character as usize,
                    }
                })
                .unwrap();
            std::hint::black_box(project.code_of_in_memory_file(&path));
        })
    });
}

fn bench_same_length_lsp_edit(c: &mut Criterion, name: &str, code: String) {
    let (mut project, paths) = prepare_independent_files(1);
    let path = paths[0].clone();
    project.store_in_memory_file(path.clone(), code.into_boxed_str());
    let mut use_y = false;
    c.bench_function(name, |b| {
        b.iter(|| {
            let replacement = if use_y { "x" } else { "y" };
            project
                .store_file_with_lsp_changes(
                    path.clone(),
                    vec![TextDocumentContentChangeEvent {
                        range: Some(Range::new(Position::new(0, 0), Position::new(0, 1))),
                        range_length: None,
                        text: replacement.into(),
                    }],
                    |pos| InputPosition::Utf16CodeUnits {
                        line: pos.line as usize,
                        column: pos.character as usize,
                    },
                )
                .unwrap();
            use_y = !use_y;
            std::hint::black_box(project.code_of_in_memory_file(&path));
        })
    });
}

fn bench_large_reference_lsp_edit(c: &mut Criterion, standalone: bool, diagnostics: bool) {
    let (mut project, paths) = prepare_independent_files(1);
    let path = paths[0].clone();
    let mut code = String::from("alpha = 1\ndelta = 2\n");
    for index in 0..16_000 {
        code.push_str(&format!("name_{index} = {index}\n"));
    }
    code.push_str(if standalone { "alpha\n" } else { "alpha + 0\n" });
    project.store_in_memory_file(path.clone(), code.into_boxed_str());
    let name = match (standalone, diagnostics) {
        (true, true) => "standalone reference edit and diagnostics in 16k-unique-name file",
        (true, false) => "standalone reference edit in 16k-unique-name file",
        (false, _) => "compound reference edit in 16k-unique-name file",
    };
    let mut use_delta = false;
    c.bench_function(name, |b| {
        b.iter(|| {
            let replacement = if use_delta { "alpha" } else { "delta" };
            project
                .store_file_with_lsp_changes(
                    path.clone(),
                    vec![TextDocumentContentChangeEvent {
                        range: Some(Range::new(
                            Position::new(16_002, 0),
                            Position::new(16_002, 5),
                        )),
                        range_length: None,
                        text: replacement.into(),
                    }],
                    |pos| InputPosition::Utf16CodeUnits {
                        line: pos.line as usize,
                        column: pos.character as usize,
                    },
                )
                .unwrap();
            use_delta = !use_delta;
            if diagnostics {
                std::hint::black_box(project.document(&path).unwrap().diagnostics());
            }
            std::hint::black_box(project.code_of_in_memory_file(&path));
        })
    });
}

fn bench_dependency_chain_reference_edit(c: &mut Criterion, source_edit: bool) {
    let (mut project, paths) = prepare_independent_files(1);
    let path = paths[0].clone();
    let mut code = if source_edit {
        String::from("alpha = 1\ndelta = 'x'\nname_0 = alpha\n")
    } else {
        String::from("alpha = 1\ndelta = 2\nname_0 = 0\n")
    };
    for index in 1..4_000 {
        code.push_str(&format!("name_{index} = name_{}\n", index - 1));
    }
    code.push_str(if source_edit {
        "name_3999 + 1\n"
    } else {
        "alpha\n"
    });
    project.store_in_memory_file(path.clone(), code.into_boxed_str());
    let mut use_delta = false;
    let (name, line, start_column) = if source_edit {
        (
            "source edit and diagnostics in 4k-name dependency chain",
            2,
            9,
        )
    } else {
        (
            "reference edit and diagnostics in 4k-name dependency chain",
            4_002,
            0,
        )
    };
    c.bench_function(name, |b| {
        b.iter(|| {
            let replacement = if use_delta { "alpha" } else { "delta" };
            project
                .store_file_with_lsp_changes(
                    path.clone(),
                    vec![TextDocumentContentChangeEvent {
                        range: Some(Range::new(
                            Position::new(line, start_column),
                            Position::new(line, start_column + 5),
                        )),
                        range_length: None,
                        text: replacement.into(),
                    }],
                    |pos| InputPosition::Utf16CodeUnits {
                        line: pos.line as usize,
                        column: pos.character as usize,
                    },
                )
                .unwrap();
            use_delta = !use_delta;
            let mut document = project.document(&path).unwrap();
            let diagnostics = document.diagnostics();
            if source_edit {
                assert_eq!(diagnostics.len(), usize::from(use_delta));
            }
            std::hint::black_box(diagnostics);
        })
    });
}

fn bench_conditional_definitions_edit(c: &mut Criterion, count: usize, mixed: bool) {
    let (mut project, paths) = prepare_independent_files(1);
    let path = paths[0].clone();
    let mut code = if mixed {
        String::from(
            "class Box:\n    value: int = 0\nbox = Box()\nguard = bool(1)\nif guard:\n    box.value = 1\n",
        )
    } else {
        String::from("guard = bool(1)\nif guard:\n")
    };
    for index in 0..count {
        code.push_str(&format!("    name_{index} = {index}\n"));
    }
    code.push_str("else:\n");
    if mixed {
        code.push_str("    box.value = 2\n");
    }
    for index in 0..count {
        code.push_str(&format!("    name_{index} = {index}\n"));
    }
    let current = format!("name_{}", count - 1);
    let previous = format!("name_{}", count - 2);
    let line = code.bytes().filter(|&byte| byte == b'\n').count() as u32;
    code.push_str(&format!("{current} + 1\n"));
    project.store_in_memory_file(path.clone(), code.into_boxed_str());
    let mut use_previous = false;
    c.bench_function(
        &format!(
            "reference edit and diagnostics in {}k-name {}conditional",
            count / 1_000,
            if mixed { "mixed " } else { "" },
        ),
        |b| {
            b.iter(|| {
                let replacement = if use_previous { &current } else { &previous };
                project
                    .store_file_with_lsp_changes(
                        path.clone(),
                        vec![TextDocumentContentChangeEvent {
                            range: Some(Range::new(
                                Position::new(line, 0),
                                Position::new(line, current.len() as u32),
                            )),
                            range_length: None,
                            text: replacement.into(),
                        }],
                        |pos| InputPosition::Utf16CodeUnits {
                            line: pos.line as usize,
                            column: pos.character as usize,
                        },
                    )
                    .unwrap();
                use_previous = !use_previous;
                let mut document = project.document(&path).unwrap();
                let diagnostics = document.diagnostics();
                assert!(diagnostics.is_empty());
                std::hint::black_box(diagnostics);
            })
        },
    );
}

fn bench_many_member_conditional_edit(c: &mut Criterion, diagnostics: bool) {
    let count = 400;
    let (mut project, paths) = prepare_independent_files(1);
    let path = paths[0].clone();
    let mut code = String::from("class Box:\n    value: int = 0\n");
    for index in 0..count {
        code.push_str(&format!("box_{index} = Box()\n"));
    }
    code.push_str("guard = bool(1)\nif guard:\n");
    for index in 0..count {
        code.push_str(&format!(
            "    box_{index}.value = {index}\n    name_{index} = {index}\n"
        ));
    }
    code.push_str("else:\n");
    for index in 0..count {
        code.push_str(&format!(
            "    box_{index}.value = {index}\n    name_{index} = {index}\n"
        ));
    }
    let line = code.bytes().filter(|&byte| byte == b'\n').count() as u32;
    code.push_str("name_399 + 1\n");
    project.store_in_memory_file(path.clone(), code.into_boxed_str());
    let mut use_previous = false;
    let name = if diagnostics {
        "reference edit and diagnostics in 400-name 400-member conditional"
    } else {
        "reference edit only in 400-name 400-member conditional"
    };
    c.bench_function(name, |b| {
        b.iter(|| {
            let replacement = if use_previous { "name_399" } else { "name_398" };
            project
                .store_file_with_lsp_changes(
                    path.clone(),
                    vec![TextDocumentContentChangeEvent {
                        range: Some(Range::new(Position::new(line, 0), Position::new(line, 8))),
                        range_length: None,
                        text: replacement.into(),
                    }],
                    |pos| InputPosition::Utf16CodeUnits {
                        line: pos.line as usize,
                        column: pos.character as usize,
                    },
                )
                .unwrap();
            use_previous = !use_previous;
            if diagnostics {
                let mut document = project.document(&path).unwrap();
                let diagnostics = document.diagnostics();
                assert!(diagnostics.is_empty());
                std::hint::black_box(diagnostics);
            } else {
                std::hint::black_box(project.code_of_in_memory_file(&path));
            }
        })
    });
}

fn bench_length_changing_lsp_edit(c: &mut Criterion, name: &str, code: String) {
    let (mut project, paths) = prepare_independent_files(1);
    let path = paths[0].clone();
    project.store_in_memory_file(path.clone(), code.into_boxed_str());
    let mut expanded = false;
    c.bench_function(name, |b| {
        b.iter(|| {
            let replacement = if expanded { "x" } else { "yy" };
            let end = if expanded { 2 } else { 1 };
            project
                .store_file_with_lsp_changes(
                    path.clone(),
                    vec![TextDocumentContentChangeEvent {
                        range: Some(Range::new(Position::new(0, 0), Position::new(0, end))),
                        range_length: None,
                        text: replacement.into(),
                    }],
                    |pos| InputPosition::Utf16CodeUnits {
                        line: pos.line as usize,
                        column: pos.character as usize,
                    },
                )
                .unwrap();
            expanded = !expanded;
            std::hint::black_box(project.code_of_in_memory_file(&path));
        })
    });
}

fn bench_large_lsp_edit_components(c: &mut Criterion) {
    let code = format!("x = 1\n#{}\n", "a".repeat(100_000));
    c.bench_function("construct text for 100 KiB LSP edit", |b| {
        b.iter(|| {
            let mut updated = String::with_capacity(code.len() + 1);
            updated.push_str("yy");
            updated.push_str(&code[1..]);
            std::hint::black_box(updated);
        })
    });
    c.bench_function("parse prepared 100 KiB LSP edit", |b| {
        b.iter_batched(
            || format!("yy{}", &code[1..]).into_boxed_str(),
            |updated| std::hint::black_box(Tree::parse(updated)),
            BatchSize::SmallInput,
        )
    });
    let tree = Tree::parse(code.clone().into_boxed_str());
    c.bench_function("read cached mypy directives in 100 KiB file", |b| {
        b.iter(|| std::hint::black_box(tree.mypy_inline_config_directives().count()))
    });
    c.bench_function("check initial type ignore in 100 KiB file", |b| {
        b.iter(|| std::hint::black_box(tree.has_type_ignore_at_start()))
    });
    let updated = format!("y{}", &code[1..]).into_boxed_str();
    c.bench_function("validate and rebind 100 KiB name edit", |b| {
        b.iter_batched(
            || updated.clone(),
            |code| std::hint::black_box(tree.with_same_length_name_edit(0, 1, code).unwrap()),
            BatchSize::SmallInput,
        )
    });
    let dense_code = "x = 1\n".repeat(16_000);
    c.bench_function("parse prepared 16k-line LSP edit", |b| {
        b.iter_batched(
            || format!("yy{}", &dense_code[1..]).into_boxed_str(),
            |updated| std::hint::black_box(Tree::parse(updated)),
            BatchSize::SmallInput,
        )
    });
    let (mut project, paths) = prepare_independent_files(1);
    let path = paths[0].clone();
    let expanded = format!("yy{}", &dense_code[1..]).into_boxed_str();
    let original = dense_code.into_boxed_str();
    project.store_in_memory_file(path.clone(), original.clone());
    let mut use_expanded = false;
    c.bench_function("store prepared 16k-line file", |b| {
        b.iter(|| {
            use_expanded = !use_expanded;
            project.store_in_memory_file(
                path.clone(),
                if use_expanded { &expanded } else { &original }.clone(),
            );
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

fn bench_large_reverse_lsp_edit(c: &mut Criterion) {
    let (mut project, paths) = prepare_independent_files(1);
    let path = paths[0].clone();
    let code = format!("{}#{}\n", "a\n".repeat(256), "x".repeat(100_000));
    project.store_in_memory_file(path.clone(), code.into_boxed_str());
    let mut expanded = false;
    c.bench_function(
        "256 length-changing reverse LSP edits before 100 KiB tail",
        |b| {
            b.iter(|| {
                let replacement = if expanded { "a" } else { "bb" };
                let end_column = if expanded { 2 } else { 1 };
                let changes = (0..256)
                    .rev()
                    .map(|line| TextDocumentContentChangeEvent {
                        range: Some(Range::new(
                            Position::new(line, 0),
                            Position::new(line, end_column),
                        )),
                        range_length: None,
                        text: replacement.into(),
                    })
                    .collect();
                project
                    .store_file_with_lsp_changes(path.clone(), changes, |pos| {
                        InputPosition::Utf16CodeUnits {
                            line: pos.line as usize,
                            column: pos.character as usize,
                        }
                    })
                    .unwrap();
                expanded = !expanded;
                let code = project.code_of_in_memory_file(&path).unwrap();
                assert!(code.starts_with(if expanded { "bb\n" } else { "a\n" }));
                std::hint::black_box(code);
            })
        },
    );
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
    let small_tree = Tree::parse(small_code.clone());
    c.bench_function("scan small file for type ignores", |b| {
        b.iter(|| {
            std::hint::black_box(
                small_tree.type_ignore_comment_for(0, small_tree.code().len() as u32),
            )
        })
    });
    let ignored_tree = Tree::parse("x = 1 # type: ignore\n".into());
    c.bench_function("scan commented line for type ignore", |b| {
        b.iter(|| std::hint::black_box(ignored_tree.type_ignore_comment_for(0, 5)))
    });
    let many_lines = Tree::parse("x = 1\n".repeat(10_000).into_boxed_str());
    c.bench_function("scan 10000 lines for mypy directives", |b| {
        b.iter(|| std::hint::black_box(many_lines.mypy_inline_config_directives().count()))
    });
    c.bench_function("scan 10000 lines for type ignores", |b| {
        b.iter(|| {
            std::hint::black_box(
                many_lines.type_ignore_comment_for(0, many_lines.code().len() as u32),
            )
        })
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
    c.bench_function("clone unchanged file store inputs", |b| {
        b.iter(|| std::hint::black_box((unchanged_path.clone(), unchanged_code.clone())))
    });
    c.bench_function("store unchanged file among 1000", |b| {
        b.iter(|| {
            unchanged_project.store_in_memory_file(unchanged_path.clone(), unchanged_code.clone())
        })
    });
    let (mut batch_project, batch_paths) = prepare_independent_files(1000);
    let batch_updated = unchanged_code
        .replace("return x", "return 1")
        .into_boxed_str();
    let mut batch_is_updated = false;
    c.bench_function("store changed file and 999 unchanged files", |b| {
        b.iter(|| {
            batch_is_updated = !batch_is_updated;
            batch_project.store_in_memory_file(
                batch_paths[0].clone(),
                if batch_is_updated {
                    &batch_updated
                } else {
                    &unchanged_code
                }
                .clone(),
            );
            for path in &batch_paths[1..] {
                batch_project.store_in_memory_file(path.clone(), unchanged_code.clone());
            }
        })
    });
    c.bench_function("project setup", |b| {
        b.iter(|| {
            let mut options = ProjectOptions::default();
            options.settings.typeshed_path = Some(test_utils::typeshed_path());
            std::hint::black_box(Project::without_watcher(options, RunCause::TypeChecking))
        })
    });
    c.bench_function("project setup without global packages", |b| {
        b.iter(|| {
            let mut options = ProjectOptions::default();
            options.settings.typeshed_path = Some(test_utils::typeshed_path());
            options.settings.add_global_packages_default = false;
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
    let (mut unchanged_project, unchanged_paths) = prepare_independent_files(1000);
    check_prepared_files(&mut unchanged_project, unchanged_paths.clone());
    c.bench_function("recheck 1000 unchanged prepared files", |b| {
        b.iter(|| {
            for path in &unchanged_paths {
                let mut document = unchanged_project.document(path).unwrap();
                std::hint::black_box(document.diagnostics());
            }
        })
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
    bench_unchanged_lsp_edit(c);
    bench_same_length_lsp_edit(
        c,
        "same-length LSP edit in 100 KiB file",
        format!("x = 1\n#{}\n", "a".repeat(100_000)),
    );
    bench_same_length_lsp_edit(
        c,
        "same-length LSP edit in 16k-line file",
        "x = 1\n".repeat(16_000),
    );
    bench_same_length_lsp_edit(
        c,
        "same-length LSP edit in 16k-unique-name file",
        (0..16_000)
            .map(|index| format!("name_{index} = 1\n"))
            .collect(),
    );
    bench_large_reference_lsp_edit(c, true, false);
    bench_large_reference_lsp_edit(c, true, true);
    bench_dependency_chain_reference_edit(c, false);
    bench_dependency_chain_reference_edit(c, true);
    bench_conditional_definitions_edit(c, 1_000, false);
    bench_conditional_definitions_edit(c, 2_000, false);
    bench_conditional_definitions_edit(c, 4_000, false);
    bench_conditional_definitions_edit(c, 2_000, true);
    bench_many_member_conditional_edit(c, false);
    bench_many_member_conditional_edit(c, true);
    bench_large_reference_lsp_edit(c, false, false);
    bench_length_changing_lsp_edit(
        c,
        "length-changing LSP edit in 100 KiB file",
        format!("x = 1\n#{}\n", "a".repeat(100_000)),
    );
    bench_length_changing_lsp_edit(
        c,
        "length-changing LSP edit in 16k-line file",
        "x = 1\n".repeat(16_000),
    );
    bench_large_lsp_edit_components(c);
    bench_warm_multi_lsp_edit(c, 32);
    bench_warm_multi_lsp_edit(c, 256);
    bench_large_reverse_lsp_edit(c);
}

// Register the benchmarks
criterion_group!(benches, bench_type_checking);
criterion_main!(benches);
