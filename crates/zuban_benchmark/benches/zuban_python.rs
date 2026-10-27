use criterion::{BatchSize, Criterion, criterion_group, criterion_main};

use config::ProjectOptions;
use vfs::PathWithScheme;
use zuban_python::{Project, RunCause};

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

fn bench_type_checking(c: &mut Criterion) {
    c.bench_function("project setup", |b| {
        b.iter(|| {
            let mut options = ProjectOptions::default();
            options.settings.typeshed_path = Some(test_utils::typeshed_path());
            std::hint::black_box(Project::without_watcher(options, RunCause::TypeChecking))
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
}

// Register the benchmarks
criterion_group!(benches, bench_type_checking);
criterion_main!(benches);
