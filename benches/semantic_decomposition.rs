mod decomposition;

fn main() {
    let mut arguments = std::env::args_os().skip(1);
    if arguments
        .next()
        .is_some_and(|argument| argument == "--probe-output")
    {
        let path = arguments
            .next()
            .unwrap_or_else(|| panic!("--probe-output requires a path"));
        assert!(arguments.next().is_none(), "unexpected probe arguments");
        decomposition::probe(std::path::Path::new(&path));
        return;
    }
    decomposition::run();
}
