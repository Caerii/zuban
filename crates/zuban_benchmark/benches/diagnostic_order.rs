use config::ProjectOptions;
use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use vfs::PathWithScheme;
use zuban_python::{Project, RunCause};

fn diagnostic_order(c: &mut Criterion) {
    const ERRORS: usize = 1_000;
    let mut options = ProjectOptions::default();
    options.settings.typeshed_path = Some(test_utils::typeshed_path());
    options.settings.add_global_packages_default = false;
    let mut project = Project::without_watcher(options, RunCause::TypeChecking);
    let vfs = project.vfs_handler();
    let path = PathWithScheme::with_file_scheme(
        vfs.normalize_rc_path(vfs.unchecked_abs_path("/diagnostic-order-benchmark.py")),
    );
    let mut code = String::new();
    for index in 0..ERRORS {
        code.push_str(&format!("value_{index}: int = 'not an integer'\n"));
    }
    project.store_in_memory_file(path, code.into_boxed_str());
    assert_eq!(project.diagnostics().unwrap().issues.len(), ERRORS);

    let mut group = c.benchmark_group("cached project diagnostics");
    group.throughput(Throughput::Elements(ERRORS as u64));
    group.bench_function("1000 errors", |b| {
        b.iter(|| {
            let diagnostics = project.diagnostics().unwrap();
            assert_eq!(diagnostics.issues.len(), ERRORS);
            std::hint::black_box(diagnostics);
        });
    });
    group.finish();
}

criterion_group!(benches, diagnostic_order);
criterion_main!(benches);
