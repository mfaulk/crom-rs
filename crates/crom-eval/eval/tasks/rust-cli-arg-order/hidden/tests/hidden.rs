use argparse::{parse, Args};

fn argv(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| s.to_string()).collect()
}

/// Every ordering of input, `--output`, and `--verbose` parses to the same result.
#[test]
fn all_flag_orders_agree() {
    let want = Args {
        input: Some("in.txt".into()),
        output: Some("out.txt".into()),
        verbose: true,
    };
    for order in [
        &["in.txt", "--output", "out.txt", "--verbose"][..],
        &["--verbose", "in.txt", "--output", "out.txt"],
        &["--output", "out.txt", "--verbose", "in.txt"],
        &["in.txt", "--verbose", "--output", "out.txt"],
    ] {
        assert_eq!(parse(&argv(order)).unwrap(), want, "{order:?}");
    }
}

/// A trailing `--output` with no value is still an error after the input path.
#[test]
fn trailing_output_without_value_errors() {
    assert!(parse(&argv(&["in.txt", "--output"])).is_err());
}

/// Input alone and no arguments at all still parse.
#[test]
fn minimal_invocations() {
    let a = parse(&argv(&["in.txt"])).unwrap();
    assert_eq!(a.input.as_deref(), Some("in.txt"));
    assert_eq!(a.output, None);
    assert!(!a.verbose);
    assert_eq!(parse(&argv(&[])).unwrap(), Args::default());
}
