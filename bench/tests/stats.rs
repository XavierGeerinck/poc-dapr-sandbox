use sandbox_bench::stats::percentile;

#[test]
fn nearest_rank() {
    let v: Vec<u64> = (1..=100).collect();
    assert_eq!(percentile(&v, 50.0), 50);
    assert_eq!(percentile(&v, 99.0), 99);
    assert_eq!(percentile(&v, 100.0), 100);
    assert_eq!(percentile(&v, 0.0), 1);
}

#[test]
#[should_panic(expected = "empty sample")]
fn empty_panics() {
    percentile(&[], 50.0);
}
