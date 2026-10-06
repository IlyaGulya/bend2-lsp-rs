mod benchmarks;
mod consumers;
mod fixtures;
mod probe;
mod stages;

trait Must<T> {
    fn must_be(self, message: &str) -> T;
}

impl<T> Must<T> for Option<T> {
    fn must_be(self, message: &str) -> T {
        match self {
            Some(value) => value,
            None => panic!("{message}"),
        }
    }
}

impl<T, E: std::fmt::Debug> Must<T> for Result<T, E> {
    fn must_be(self, message: &str) -> T {
        match self {
            Ok(value) => value,
            Err(error) => panic!("{message}: {error:?}"),
        }
    }
}

pub(super) fn run() {
    benchmarks::run();
}

pub(super) fn probe(path: &std::path::Path) {
    probe::write(path);
}
