use config::ProjectOptions;
use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use vfs::PathWithScheme;
use zuban_python::{Project, RunCause};

fn diagnostic_order_case(c: &mut Criterion, embedded: bool, errors: usize) {
    let mut options = ProjectOptions::default();
    options.settings.typeshed_path = Some(test_utils::typeshed_path());
    options.settings.add_global_packages_default = false;
    let mut project = Project::without_watcher(options, RunCause::TypeChecking);
    let vfs = project.vfs_handler();
    let path = PathWithScheme::with_file_scheme(
        vfs.normalize_rc_path(vfs.unchecked_abs_path("/diagnostic-order-benchmark.py")),
    );
    let mut code = String::new();
    for index in 0..errors {
        if embedded {
            code.push_str(&format!("value_{index}: 'MissingType{index}' = 1\n"));
        } else {
            code.push_str(&format!("value_{index}: int = 'not an integer'\n"));
        }
    }
    project.store_in_memory_file(path, code.into_boxed_str());
    assert_eq!(project.diagnostics().unwrap().issues.len(), errors);

    let mut group = c.benchmark_group("cached project diagnostics");
    group.throughput(Throughput::Elements(errors as u64));
    let name = if embedded {
        format!("{errors} embedded annotation errors")
    } else {
        format!("{errors} errors")
    };
    group.bench_function(&name, |b| {
        b.iter(|| {
            let diagnostics = project.diagnostics().unwrap();
            assert_eq!(diagnostics.issues.len(), errors);
            std::hint::black_box(diagnostics);
        });
    });
    group.finish();
}

fn diagnostic_order(c: &mut Criterion) {
    diagnostic_order_case(c, false, 1_000);
    for errors in [32, 128, 1_000] {
        diagnostic_order_case(c, true, errors);
    }
}

criterion_group!(benches, diagnostic_order);
criterion_main!(benches);
