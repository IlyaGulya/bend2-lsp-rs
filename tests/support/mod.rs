use std::fmt::Debug;

pub(crate) trait Must<T> {
    fn must_be(self, context: &str) -> T;
}

impl<T, E: Debug> Must<T> for Result<T, E> {
    #[track_caller]
    fn must_be(self, context: &str) -> T {
        match self {
            Ok(value) => value,
            Err(error) => panic!("{context}: {error:?}"),
        }
    }
}

impl<T> Must<T> for Option<T> {
    #[track_caller]
    fn must_be(self, context: &str) -> T {
        match self {
            Some(value) => value,
            None => panic!("{context}"),
        }
    }
}
