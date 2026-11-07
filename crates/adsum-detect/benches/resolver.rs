use adsum_detect::resolver::resolve_box64;
use adsum_detect::types::Exec;
use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;

fn exec_box64() -> Exec {
  Exec {
    pid: 1234,
    path: "box64".into(),
    arguments: Some("wine ./game.exe -windowed".into()),
    argument_spans: Vec::new(),
  }
}

fn bench_resolve_box64(c: &mut Criterion) {
  let e = exec_box64();
  c.bench_function("resolve_box64", |b| {
    b.iter(|| black_box(resolve_box64(black_box(&e))))
  });
}

fn bench_arguments_from(c: &mut Criterion) {
  let e = exec_box64();
  c.bench_function("arguments_from", |b| {
    b.iter(|| black_box(e.arguments_from(black_box(1))))
  });
}

criterion_group!(benches, bench_resolve_box64, bench_arguments_from);
criterion_main!(benches);
