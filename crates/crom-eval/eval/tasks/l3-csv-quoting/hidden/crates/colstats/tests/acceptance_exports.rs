use colstats::summarize_csv;
use schema::ColumnType;

#[test]
fn summarizes_windows_export_with_quoted_names() {
    let csv = "city,pop,notes\r\n\
\"Portland, OR\",650000,\"largest, by far\"\r\n\
\"Portland, ME\",68000,\"has a \"\"harbor\"\"\"\r\n\
Salem,175000,\"multi\r\nline\"\r\n";
    let s = summarize_csv(csv).unwrap();
    assert_eq!(s.len(), 3);
    assert_eq!(s[0].name, "city");
    assert_eq!(s[0].distinct, 3);
    assert_eq!(s[1].ty, ColumnType::Integer);
    assert_eq!(s[1].min, Some(68000.0));
    assert_eq!(s[1].max, Some(650000.0));
    assert_eq!(s[1].mean, Some(297_666.666_666_666_7));
    assert_eq!(s[2].count, 3);
}
