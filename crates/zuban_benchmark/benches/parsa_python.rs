use criterion::{Criterion, criterion_group, criterion_main};
use std::path::{Path, PathBuf};

use parsa_python_cst::Tree;

fn parse_file(copies: usize) {
    let some_code = utils::dedent(
        r#"
        import os
        def x(a, b, c):
            try:
                return a
            except Exception:
                return [1, 2, 3, 4, 5, 6]
        class C:
            x: int
            y = 1 # type: int
            z: dict[int, list[str]] = {1: ["", r''], 2: [f""" {1} """]}
            def f(self, x: list[str], y: int): ...
    "#,
    );
    assert_eq!(some_code.len(), 262);
    Tree::parse(some_code.repeat(copies).into());
}

fn typeshed_sources() -> Vec<Box<str>> {
    fn collect(dir: &Path, paths: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                collect(&path, paths);
            } else if path.extension().is_some_and(|extension| extension == "pyi") {
                paths.push(path);
            }
        }
    }

    let root = PathBuf::from(test_utils::typeshed_path().to_string());
    let mut paths = Vec::new();
    collect(&root.join("stdlib"), &mut paths);
    collect(&root.join("stubs/mypy-extensions"), &mut paths);
    paths.sort_unstable();
    paths
        .iter()
        .map(|path| std::fs::read_to_string(path).unwrap().into_boxed_str())
        .collect()
}

fn bench_parser(c: &mut Criterion) {
    c.bench_function("parse big file", |b| b.iter(|| parse_file(1000)));
    c.bench_function("parse small file", |b| b.iter(|| parse_file(1)));
    let sources = typeshed_sources();
    c.bench_function("parse typeshed stubs", |b| {
        b.iter(|| {
            for source in &sources {
                std::hint::black_box(Tree::parse(source.clone()));
            }
        })
    });
}

// Register the benchmarks
criterion_group!(benches, bench_parser);
criterion_main!(benches);
