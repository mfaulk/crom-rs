use argparse::parse;

fn argv(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| s.to_string()).collect()
}

#[test]
fn output_before_input() {
    let a = parse(&argv(&["--output", "out.txt", "in.txt"])).unwrap();
    assert_eq!(a.input.as_deref(), Some("in.txt"));
    assert_eq!(a.output.as_deref(), Some("out.txt"));
}

#[test]
fn output_after_input() {
    // This is the failing case: flags must be accepted in any order.
    let a = parse(&argv(&["in.txt", "--output", "out.txt"])).unwrap();
    assert_eq!(a.input.as_deref(), Some("in.txt"));
    assert_eq!(a.output.as_deref(), Some("out.txt"));
}

#[test]
fn verbose_after_input() {
    let a = parse(&argv(&["in.txt", "--verbose"])).unwrap();
    assert_eq!(a.input.as_deref(), Some("in.txt"));
    assert!(a.verbose);
}
