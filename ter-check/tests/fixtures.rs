//! Recorded runs with known outcomes: each folder under tests/fixtures
//! holds an `events.jsonl`, the `check.yaml` it was judged against and the
//! `expected.toml` report. The evaluator must reproduce the report exactly,
//! with no simulator or board.

use std::path::Path;

use ter_telemetry::Recording;

#[test]
fn every_fixture_gives_its_expected_report() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut seen = 0;
    for entry in std::fs::read_dir(&root).unwrap() {
        let dir = entry.unwrap().path();
        if !dir.is_dir() {
            continue;
        }
        let check = std::fs::read_to_string(dir.join("check.yaml")).unwrap();
        let expected = std::fs::read_to_string(dir.join("expected.toml")).unwrap();
        let rec = Recording::read(&dir).unwrap();
        let report = ter_check::check(&check, &rec).unwrap();
        assert_eq!(report.to_toml(), expected, "{}", dir.display());
        seen += 1;
    }
    assert!(seen >= 5, "the fixtures are there: {seen}");
}
