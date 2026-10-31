use config::{ExcludeRegex, ProjectOptions};
use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use vfs::{LocalFS, PathWithScheme, VfsHandler};
use zuban_python::{Project, RunCause};

fn selection_case(c: &mut Criterion, count: usize, deep: bool, exclude: bool) {
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
            if exclude { " with exclude" } else { "" }
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
        selection_case(c, count, false, false);
        selection_case(c, count, true, false);
    }
    selection_case(c, 1_000, true, true);
}

criterion_group!(benches, project_selection);
criterion_main!(benches);
