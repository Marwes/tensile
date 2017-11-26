extern crate futures;
extern crate futures_cpupool;
extern crate num_cpus;
extern crate tokio_core;
extern crate structopt;
#[macro_use]
extern crate structopt_derive;

use std::fmt;
use std::ops::{Add, AddAssign};

use futures::{Async, AsyncSink, Future, IntoFuture, Poll, Sink, StartSend};
use futures::future::{self, Either};

use tokio_core::reactor::Core;

use futures_cpupool::CpuPool;

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
    Test {
        name: String,
        test: T,
    },
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

impl<E> Testable for Result<(), E>
where
    E: Send + Sync + 'static,
{
    type Error = E;

    fn test(self) -> TestFuture<Self::Error> {
        Box::new(self.into_future())
    }
}

impl<F, T> Testable for F
where
    F: FnOnce() -> T + Send + Sync + ::std::panic::UnwindSafe + 'static,
    T: Testable + 'static,
    T::Error: for<'a> From<&'a str> + From<String> + Send + Sync,
{
    type Error = T::Error;

    fn test(self) -> TestFuture<Self::Error> {
        Box::new(future::lazy(move || {
            let result = ::std::panic::catch_unwind(|| self().test());
            match result {
                Ok(fut) => Either::A(fut),
                Err(err) => Either::B(
                    Err(
                        err.downcast::<T::Error>()
                            .map(|err| *err)
                            .or_else(|err| err.downcast::<String>().map(|s| (*s).into()))
                            .or_else(|err| err.downcast::<&str>().map(|s| (*s).into()))
                            .unwrap_or_else(|_| panic!("Unknown panic type")),
                    ).into_future(),
                ),
            }
        }))
    }
}

struct Starter<'a> {
    cpu_pool: &'a CpuPool,
    options: &'a Options,
    filtered_tests: i32,
}

impl<'a> Starter<'a> {
    fn new(cpu_pool: &'a CpuPool, options: &'a Options) -> Starter<'a> {
        Starter {
            cpu_pool,
            options,
            filtered_tests: 0,
        }
    }
}

impl<Error> Test<Error>
where
    Error: Send + 'static,
{
    fn run_all(self, starter: &mut Starter) -> Option<RunningTest<Error>> {
        self.run_test(starter, "")
    }

    fn run_test(
        self,
        starter: &mut Starter,
        path: &str,
    ) -> Option<RunningTest<Error>> {
        match self {
            Test::Test { name, test } => {
                let test_path = format!("{}/{}", path, name);
                if test_path.contains(&starter.options.filter) {
                    Some(RunTest::Test {
                        name,
                        test: Box::new(starter.cpu_pool.spawn(test.test())),
                    })
                } else {
                    None
                }
            }
            Test::Group { name, tests } => {
                let test_path = format!("{}/{}", path, name);
                let tests: Vec<_> = tests
                    .into_iter()
                    .filter_map(|test| test.run_test(starter, &test_path))
                    .collect();
                if tests.is_empty() {
                    None
                } else {
                    Some(RunTest::Group { name, tests })
                }
            }
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum TestProgress<Error> {
    GroupStart(String),
    GroupEnd,
    Test(String, Result<(), Error>),
}

#[derive(Default)]
struct Stats {
    failed_tests: i32,
    successful_tests: i32,
}

impl AddAssign for Stats {
    fn add_assign(&mut self, other: Stats) {
        self.failed_tests += other.failed_tests;
        self.successful_tests += other.successful_tests;
    }
}

impl Add for Stats {
    type Output = Self;

    fn add(mut self, other: Stats) -> Self {
        self += other;
        self
    }
}


impl<Error> RunningTest<Error>
where
    Error: fmt::Debug + fmt::Display + Send + Sync + 'static,
{
    fn print_all<S>(
        mut tests: Vec<RunningTest<Error>>,
        path: String,
        sink: S,
    ) -> Box<Future<Item = (Stats, S), Error = ()>>
    where
        S: Sink<SinkItem = TestProgress<Error>, SinkError = ()> + 'static,
    {
        match tests.pop() {
            Some(test) => Box::new(test.print(&path, sink).and_then(|(s1, sink)| {
                Self::print_all(tests, path, sink).map(|(s2, sink)| (s1 + s2, sink))
            })),
            None => Box::new(Ok((Stats::default(), sink)).into_future()),
        }
    }

    fn print<S>(self, path: &str, sink: S) -> Box<Future<Item = (Stats, S), Error = ()>>
    where
        S: Sink<SinkItem = TestProgress<Error>, SinkError = ()> + 'static,
    {
        match self {
            RunTest::Test { name, test } => Box::new(test.then(move |result| {
                let stats = Stats {
                    successful_tests: result.is_ok() as i32,
                    failed_tests: result.is_err() as i32,
                };
                sink.send(TestProgress::Test(name, result))
                    .map(|sink| (stats, sink))
            })),
            RunTest::Group { name, mut tests } => {
                tests.reverse();
                let owned_path = if path == "" {
                    name.to_string()
                } else {
                    format!("{}/{}", path, name)
                };
                Box::new(
                    sink.send(TestProgress::GroupStart(name.into()))
                        .and_then(|sink| Self::print_all(tests, owned_path, sink))
                        .and_then(|(stats, sink)| {
                            sink.send(TestProgress::GroupEnd).map(|sink| (stats, sink))
                        }),
                )
            }
        }
    }
}

struct Console<T>(::std::marker::PhantomData<T>);

impl<T> Console<T>
where
    T: fmt::Display,
{
    fn new() -> Self {
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

#[derive(Default, StructOpt)]
pub struct Options {
    #[structopt(short = "j", long = "jobs", help = "Number of tests to run in parallel")]
    jobs: Option<usize>,
    #[structopt(help = "String used to filter out tests")]
    filter: String,
}

impl Options {
    pub fn new() -> Options {
        Options::default()
    }

    pub fn filter<S>(mut self, filter: S) -> Options
    where
        S: Into<String>,
    {
        self.filter = filter.into();
        self
    }

    pub fn jobs(mut self, jobs: Option<usize>) -> Options {
        self.jobs = jobs;
        self
    }
}

struct TestReport {
    failed_tests: i32,
    successful_tests: i32,
    filtered_tests: i32,
}

fn execute_test_runner<S, Error>(sink: S, test: Test<Error>, options: &Options) -> Result<TestReport, ()>
where
    S: Sink<SinkItem = TestProgress<Error>, SinkError = ()> + 'static,
    Error: fmt::Debug + fmt::Display + Send + Sync + 'static,
{

    let pool = futures_cpupool::Builder::new()
        .pool_size(options.jobs.unwrap_or_else(|| num_cpus::get()))
        .name_prefix("tensile-test-pool-")
        .create();

    let mut starter = Starter::new(&pool, options);
    let running_test = test.run_all(&mut starter);

    let mut core = Core::new().unwrap();
    let stats = match running_test {
        Some(running_test) => core.run(running_test.print("", sink))?.0,
        None => Stats::default(),
    };

    Ok(TestReport {
        failed_tests: stats.failed_tests,
        successful_tests: stats.successful_tests,
        filtered_tests: starter.filtered_tests,
    })
}

pub fn console_runner<Error>(test: Test<Error>, options: &Options) -> Result<(), ()>
where
    Error: fmt::Debug + fmt::Display + Send + Sync + 'static,
{
    let mut indent = String::new();
    let sink = Console::new().with(move |progress| {
        Ok(match progress {
            TestProgress::GroupStart(name) => {
                let msg = format!("{}GROUP: {}", indent, name);
                indent.push('\t');
                msg
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

    let report = execute_test_runner(sink, test, options)?;

    let overall_result = if report.failed_tests == 0 {
        "ok"
    } else {
        "FAILED"
    };
    println!(
        "test result: {}. {} passed; {} failed; {} filtered",
        overall_result,
        report.successful_tests,
        report.failed_tests,
        report.filtered_tests
    );
    // Filtering out all tests is almost certainly a mistake so report that as an error
    let filtered_all_tests = report.failed_tests == 0 && report.successful_tests == 0 && report.filtered_tests != 0;
    if report.failed_tests == 0 && !filtered_all_tests {
        Ok(())
    } else {
        Err(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use futures::Stream;
    use futures::sync::mpsc::channel;

    use futures_cpupool::CpuPool;

    fn test_test<Error>(test: Test<Error>) -> Vec<TestProgress<Error>>
    where
        Error: fmt::Debug + fmt::Display + Send + Sync + 'static,
    {
        options_test(test, Options::default())
    }

    fn options_test<Error>(test: Test<Error>, options: Options) -> Vec<TestProgress<Error>>
    where
        Error: fmt::Debug + fmt::Display + Send + Sync + 'static,
    {
        let pool = CpuPool::new(4);
        let running_test = test.run_all(&mut Starter::new(&pool, &options));

        let mut core = Core::new().unwrap();

        let (sender, receiver) = channel(10);
        let collector_future = pool.spawn(receiver.collect());

        match running_test {
            Some(running_test) => {
                core.handle().spawn(
                    running_test
                        .print("", sender.sink_map_err(|_| panic!()))
                        .map(|_| ())
                        .map_err(|_| ()),
                );

                core.run(collector_future).unwrap()
            }
            None => vec![],
        }
    }

    #[test]
    fn simple() {
        let progress = test_test(test("1", true));
        assert_eq!(progress, vec![TestProgress::Test("1".to_string(), Ok(()))]);
    }

    #[test]
    fn grouped_tests() {
        let progress = test_test(group("group", vec![test("1", true), test("2", false)]));
        assert_eq!(
            progress,
            vec![
                TestProgress::GroupStart("group".into()),
                TestProgress::Test("1".into(), Ok(())),
                TestProgress::Test("2".into(), Err("false".to_string())),
                TestProgress::GroupEnd,
            ]
        );
    }

    #[test]
    fn nested_groups() {
        let progress = test_test(group(
            "group",
            vec![test("1", true), group("inner", vec![])],
        ));
        assert_eq!(
            progress,
            vec![
                TestProgress::GroupStart("group".into()),
                TestProgress::Test("1".into(), Ok(())),
                TestProgress::GroupEnd,
            ]
        );
    }

    #[test]
    fn mixed_test_and_groups() {
        let progress = test_test(group(
            "group",
            vec![
                group("inner1", vec![test("test1", true)]),
                test("middle test", true),
                group("inner2", vec![test("test2", false)]),
            ],
        ));
        assert_eq!(
            progress,
            vec![
                TestProgress::GroupStart("group".into()),
                TestProgress::GroupStart("inner1".into()),
                TestProgress::Test("test1".into(), Ok(())),
                TestProgress::GroupEnd,
                TestProgress::Test("middle test".to_string(), Ok(())),
                TestProgress::GroupStart("inner2".into()),
                TestProgress::Test("test2".into(), Err("false".into())),
                TestProgress::GroupEnd,
                TestProgress::GroupEnd,
            ]
        );
    }

    #[test]
    fn handle_panic() {
        fn test1() {
            panic!("fail")
        }
        let progress = test_test(group(
            "group",
            vec![tensile_fn!(test1), test("test2", true)],
        ));
        assert_eq!(
            progress,
            vec![
                TestProgress::GroupStart("group".into()),
                TestProgress::Test("test1".into(), Err("fail".into())),
                TestProgress::Test("test2".into(), Ok(())),
                TestProgress::GroupEnd,
            ]
        );
    }

    #[test]
    fn filter() {
        let progress = options_test(
            group(
                "group",
                vec![
                    group("inner1", vec![test("test1", true)]),
                    test("middle test", true),
                    group("inner2", vec![]),
                ],
            ),
            Options {
                filter: "test1".into(),
                jobs: Some(1),
            },
        );
        assert_eq!(
            progress,
            vec![
                TestProgress::GroupStart("group".into()),
                TestProgress::GroupStart("inner1".into()),
                TestProgress::Test("test1".into(), Ok(())),
                TestProgress::GroupEnd,
                TestProgress::GroupEnd,
            ]
        );
    }
}
