use config::{ExcludeRegex, ProjectOptions};
use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use vfs::{LocalFS, PathWithScheme, VfsHandler};
use zuban_python::{Project, RunCause};

fn selection_case(c: &mut Criterion, count: usize, deep: bool, exclude: bool, with_stub: bool) {
    let mut options = ProjectOptions::default();
    options.settings.typeshed_path = Some(test_utils::typeshed_path());
    options.settings.add_global_packages_default = false;
    if exclude {
        options
            .flags
            .excludes
            .push(ExcludeRegex::new("never-match-this-file".into()).unwrap());
    }
    let fs = LocalFS::without_watcher();
    options
        .settings
        .mypy_path
        .push(fs.normalize_rc_path(fs.unchecked_abs_path("/selection-benchmark")));
    let mut project = Project::without_watcher(options, RunCause::TypeChecking);
    let dir = if deep {
        "/selection-benchmark/a/b/c/d/e/f"
    } else {
        "/selection-benchmark"
    };
    let mut files = Vec::with_capacity(count);
    for index in 0..count {
        let vfs = project.vfs_handler();
        let path = PathWithScheme::with_file_scheme(
            vfs.normalize_rc_path(vfs.unchecked_abs_path(&format!("{dir}/file_{index}.py"))),
        );
        files.push((path, "value: int = 1\n".into()));
    }
    if with_stub {
        let vfs = project.vfs_handler();
        let path = PathWithScheme::with_file_scheme(
            vfs.normalize_rc_path(vfs.unchecked_abs_path(&format!("{dir}/file_0.pyi"))),
        );
        files.push((path, "value: int\n".into()));
    }
    project.store_in_memory_files(files);
    let first = project.diagnostics().unwrap();
    assert_eq!(first.checked_files, count);
    assert!(first.issues.is_empty());

    let mut group = c.benchmark_group("cached project selection");
    group.throughput(Throughput::Elements(count as u64));
    group.bench_function(
        format!(
            "{count} files {}{}",
            if deep { "deep" } else { "shallow" },
            if exclude {
                " with exclude"
            } else if with_stub {
                " with stub"
            } else {
                ""
            }
        ),
        |b| {
            b.iter(|| {
                let diagnostics = project.diagnostics().unwrap();
                assert_eq!(diagnostics.checked_files, count);
                assert!(diagnostics.issues.is_empty());
                std::hint::black_box(diagnostics);
            });
        },
    );
    group.finish();
}

fn project_selection(c: &mut Criterion) {
    for count in [1, 1_000] {
        selection_case(c, count, false, false, false);
        selection_case(c, count, true, false, false);
    }
    selection_case(c, 1_000, true, true, false);
    selection_case(c, 1_000, true, false, true);
    cold_project_check(c);
}

fn cold_project_check(c: &mut Criterion) {
    const FILES: usize = 100;
    let temp = tempfile::tempdir().expect("create project fixture");
    for index in 0..FILES {
        let dir = temp.path().join(format!("package_{}/nested", index / 10));
        std::fs::create_dir_all(&dir).expect("create fixture directory");
        std::fs::write(dir.join(format!("file_{index}.py")), "value: int = 1\n")
            .expect("write fixture file");
    }

    let fs = LocalFS::without_watcher();
    let mut options = ProjectOptions::default();
    options.settings.typeshed_path = Some(test_utils::typeshed_path());
    options.settings.add_global_packages_default = false;
    options
        .settings
        .mypy_path
        .push(fs.normalize_rc_path(fs.unchecked_abs_path(temp.path().to_str().unwrap())));

    let root = fs.unchecked_abs_path(temp.path().to_str().unwrap());
    let mut group = c.benchmark_group("first project check");
    group.throughput(Throughput::Elements(FILES as u64));
    for overlap in [false, true] {
        let mut case_options = options.clone();
        if overlap {
            case_options
                .settings
                .set_files_or_directories_to_check(
                    &fs,
                    &root,
                    None,
                    [".".into(), "package_0".into()],
                )
                .expect("configure overlapping paths");
        }
        let name = if overlap {
            "100 files with overlap"
        } else {
            "100 files on disk"
        };
        group.bench_function(name, |b| {
            b.iter_batched(
                || Project::without_watcher(case_options.clone(), RunCause::TypeChecking),
                |mut project| {
                    let diagnostics = project.diagnostics().expect("check fixture");
                    assert_eq!(diagnostics.checked_files, FILES);
                    assert!(diagnostics.issues.is_empty());
                    std::hint::black_box(diagnostics.checked_files);
                },
                BatchSize::PerIteration,
            );
        });
    }
    group.finish();

    let edited_file = temp.path().join("package_0/nested/file_0.py");
    let invalidated_path = fs.unchecked_abs_path(edited_file.to_str().unwrap());
    let mut project = Project::without_watcher(options, RunCause::TypeChecking);
    let first = project.diagnostics().expect("check fixture before edits");
    assert_eq!(first.checked_files, FILES);
    assert!(first.issues.is_empty());
    let mut has_error = false;
    c.bench_function("disk edit and recheck 100 files", |b| {
        b.iter(|| {
            has_error = !has_error;
            let code = if has_error {
                "value: int = 'bad'\n"
            } else {
                "value: int = 1\n"
            };
            std::fs::write(&edited_file, code).expect("edit fixture file");
            project.invalidate_path(&invalidated_path);
            let diagnostics = project.diagnostics().expect("recheck fixture");
            assert_eq!(diagnostics.checked_files, FILES);
            assert_eq!(diagnostics.issues.len(), usize::from(has_error));
            std::hint::black_box(diagnostics.checked_files);
        });
    });
}

criterion_group!(benches, project_selection);
criterion_main!(benches);
