extern crate futures;
extern crate tokio_core;

use std::fmt;

use futures::{Async, AsyncSink, Future, IntoFuture, Poll, Sink, StartSend};
use tokio_core::reactor::Core;

pub type TestFuture<E> = Box<Future<Item = (), Error = E> + Send + Sync + 'static>;


pub trait Testable {
    type Error;
    fn test(self) -> TestFuture<Self::Error>;
}

pub trait BoxTestable {
    type Error;
    fn test_box(self: Box<Self>) -> TestFuture<Self::Error>;
}

impl<Error> Testable for Box<BoxTestable<Error = Error>> {
    type Error = Error;

    fn test(self) -> TestFuture<Self::Error> {
        self.test_box()
    }
}

impl<T> BoxTestable for T
where
    T: Testable,
{
    type Error = T::Error;

    fn test_box(self: Box<Self>) -> TestFuture<Self::Error> {
        (*self).test()
    }
}

pub enum Test<Error> {
    Test {
        name: String,
        test: Box<BoxTestable<Error = Error>>,
    },
    Group {
        name: String,
        tests: Vec<Test<Error>>,
    },
}

pub fn test<S, T>(name: S, testable: T) -> Test<T::Error>
where
    S: Into<String>,
    T: Testable + Send + Sync + 'static,
{
    Test::Test {
        name: name.into(),
        test: Box::new(testable),
    }
}

#[macro_export]
macro_rules! tensile_fn {
    ($name: ident) => {
        $crate::test(stringify!($name), $name as fn() -> _)
    }
}

pub fn group<S, Error>(name: S, tests: Vec<Test<Error>>) -> Test<Error>
where
    S: Into<String>,
{
    Test::Group {
        name: name.into(),
        tests,
    }
}

pub enum RunTest<T> {
    Test { name: String, test: T },
    Group {
        name: String,
        tests: Vec<RunTest<T>>,
    },
}

pub type RunningTest<Error> = RunTest<TestFuture<Error>>;
pub type FinishedTest = RunTest<(String, bool)>;

impl Testable for bool {
    type Error = String;

    fn test(self) -> TestFuture<Self::Error> {
        Box::new(
            if self {
                Ok(())
            } else {
                Err("false".to_string())
            }.into_future(),
        )
    }
}

impl Testable for () {
    type Error = String;

    fn test(self) -> TestFuture<Self::Error> {
        Box::new(Ok(()).into_future())
    }
}

impl<T> Testable for fn() -> T
where
    T: Testable,
{
    type Error = T::Error;

    fn test(self) -> TestFuture<Self::Error> {
        self().test()
    }
}

impl<Error> Test<Error> {
    fn run_all(self) -> RunningTest<Error> {
        self.run_test("")
    }

    fn run_test(self, path: &str) -> RunningTest<Error> {
        match self {
            Test::Test { name, test } => RunTest::Test {
                name,
                test: test.test(),
            },
            Test::Group { name, tests } => {
                let test_path = format!("{}/{}", path, name);

                RunTest::Group {
                    name: name,
                    tests: tests
                        .into_iter()
                        .map(|test| test.run_test(&test_path))
                        .collect(),
                }
            }
        }
    }
}

pub enum TestProgress<Error> {
    GroupStart(String),
    GroupEnd,
    Test(String, Result<(), Error>),
}

impl<Error> RunningTest<Error>
where
    Error: fmt::Debug + fmt::Display + Send + Sync + 'static,
{
    fn print_all<S>(
        mut tests: Vec<RunningTest<Error>>,
        path: String,
        sink: S,
    ) -> Box<Future<Item = S, Error = ()>>
    where
        S: Sink<SinkItem = TestProgress<Error>, SinkError = ()> + 'static,
    {
        match tests.pop() {
            Some(test) => Box::new(
                test.print(&path, sink)
                    .and_then(|sink| Self::print_all(tests, path, sink)),
            ),
            None => Box::new(Ok(sink).into_future()),
        }
    }

    fn print<S>(self, path: &str, sink: S) -> Box<Future<Item = S, Error = ()>>
    where
        S: Sink<SinkItem = TestProgress<Error>, SinkError = ()> + 'static,
    {
        match self {
            RunTest::Test { name, test } => {
                Box::new(test.then(move |result| sink.send(TestProgress::Test(name, result))))
            }
            RunTest::Group { name, mut tests } => {
                tests.reverse();
                let owned_path = if path == "" {
                    name.to_string()
                } else {
                    format!("{}/{}", path, name)
                };
                Box::new(
                    sink.send(TestProgress::GroupStart(owned_path.clone()))
                        .and_then(|sink| Self::print_all(tests, owned_path, sink)),
                )
            }
        }
    }
}

struct Console<T>(::std::marker::PhantomData<T>);
impl<T> Default for Console<T>
where
    T: fmt::Display,
{
    fn default() -> Self {
        Console(::std::marker::PhantomData)
    }
}
impl<T> Sink for Console<T>
where
    T: fmt::Display,
{
    type SinkItem = T;
    type SinkError = ();

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        println!("{}", item);
        Ok(AsyncSink::Ready)
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        Ok(Async::Ready(()))
    }
}


pub fn console_runner<Error>(test: Test<Error>) -> Result<(), ()>
where
    Error: fmt::Debug + fmt::Display + Send + Sync + 'static,
{
    let mut indent = String::new();
    let sink = Console::default().with(move |progress| {
        Ok(match progress {
            TestProgress::GroupStart(name) => {
                indent.push('\t');
                format!("GROUP: {}", name)
            }
            TestProgress::GroupEnd => {
                indent.pop();
                "".to_string()
            }
            TestProgress::Test(name, result) => match result {
                Ok(()) => format!("{}PASSED: {}", indent, name),
                Err(err) => format!("{}FAILED: {}\n{}", indent, name, err),
            },
        })
    });

    let running_test = test.run_all();

    let mut core = Core::new().unwrap();
    core.run(running_test.print("", sink).map(|_| ()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        assert_eq!(2 + 2, 4);
    }
}
