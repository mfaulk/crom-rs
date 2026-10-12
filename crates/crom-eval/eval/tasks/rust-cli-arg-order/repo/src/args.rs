/// Parsed command-line arguments.
#[derive(Debug, PartialEq, Eq, Default)]
pub struct Args {
    pub input: Option<String>,
    pub output: Option<String>,
    pub verbose: bool,
}

/// Parse arguments (excluding argv[0]).
///
/// Bug: once a positional (the input path) is seen, parsing stops, so any
/// `--output` that appears *after* the input path is silently dropped.
pub fn parse(argv: &[String]) -> Result<Args, String> {
    let mut args = Args::default();
    let mut i = 0;
    while i < argv.len() {
        let a = &argv[i];
        match a.as_str() {
            "--output" => {
                i += 1;
                let v = argv.get(i).ok_or("--output requires a value")?;
                args.output = Some(v.clone());
            }
            "--verbose" => args.verbose = true,
            _ => {
                args.input = Some(a.clone());
                // Bug: stop parsing once the positional is found.
                break;
            }
        }
        i += 1;
    }
    Ok(args)
}
