use sandbox_bench::run::{Resp, malformed_count};

#[test]
fn protojson_strings_absent_fields_and_malformed_values() {
    let r: Resp = serde_json::from_str(
        r#"{"activation":"x","timing":{"totalUs":"1500","hostUs":"1400","acquirePath":"POOL"}}"#,
    )
    .unwrap();
    let t = r.timing.unwrap();
    assert_eq!(
        (t.total_us, t.host_us, t.activate_us, t.exec_us),
        (1500, 1400, 0, 0)
    );
    assert_eq!(t.acquire_path, "POOL");
    let r: Resp = serde_json::from_str("{}").unwrap();
    assert!(r.timing.is_none() && !r.timed_out);

    let before = malformed_count();
    let r: Resp = serde_json::from_str(r#"{"timing":{"totalUs":"abc","execUs":true}}"#).unwrap();
    let t = r.timing.unwrap();
    assert_eq!((t.total_us, t.exec_us), (0, 0));
    assert_eq!(malformed_count() - before, 2);
}
