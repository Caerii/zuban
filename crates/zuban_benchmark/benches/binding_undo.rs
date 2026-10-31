use config::ProjectOptions;
use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use lsp_types::{Position, Range, TextDocumentContentChangeEvent};
use vfs::PathWithScheme;
use zuban_python::{InputPosition, Project, RunCause};

// Each iteration changes and restores 64 distinct references. The file and
// project stay warm, as they do during a long editing session.
fn binding_edit_undo(c: &mut Criterion) {
    const DEFINITIONS: u32 = 16_000;
    const REFERENCES: u32 = 64;
    let mut options = ProjectOptions::default();
    options.settings.typeshed_path = Some(test_utils::typeshed_path());
    options.settings.add_global_packages_default = false;
    let mut project = Project::without_watcher(options, RunCause::TypeChecking);
    let vfs = project.vfs_handler();
    let path = PathWithScheme::with_file_scheme(
        vfs.normalize_rc_path(vfs.unchecked_abs_path("/binding-undo-benchmark.py")),
    );
    let mut code = String::from("alpha = 1\ndelta = 2\n");
    for index in 0..DEFINITIONS {
        code.push_str(&format!("name_{index} = {index}\n"));
    }
    for _ in 0..REFERENCES {
        code.push_str("alpha\n");
    }
    project.store_in_memory_file(path.clone(), code.clone().into_boxed_str());
    assert!(project.document(&path).unwrap().diagnostics().is_empty());

    let mut group = c.benchmark_group("binding edit and undo");
    group.throughput(Throughput::Elements(u64::from(REFERENCES) * 2));
    group.bench_function("64 references in 16k-definition file", |b| {
        b.iter(|| {
            for slot in 0..REFERENCES {
                let line = DEFINITIONS + 2 + slot;
                for replacement in ["delta", "alpha"] {
                    project
                        .store_file_with_lsp_changes(
                            path.clone(),
                            vec![TextDocumentContentChangeEvent {
                                range: Some(Range::new(
                                    Position::new(line, 0),
                                    Position::new(line, 5),
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
                }
            }
            assert_eq!(project.code_of_in_memory_file(&path).unwrap(), code);
        });
    });
    group.finish();
    assert!(project.document(&path).unwrap().diagnostics().is_empty());
}

criterion_group!(benches, binding_edit_undo);
criterion_main!(benches);
