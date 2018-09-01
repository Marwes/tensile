extern crate failure;
extern crate futures;
extern crate futures_cpupool;
extern crate num_cpus;
extern crate structopt;
#[macro_use]
extern crate structopt_derive;
extern crate termcolor;

use std::fmt;
use std::io::{self, Write};
use std::ops::{Add, AddAssign};

use termcolor::WriteColor;

use futures::future::{self, Either};
use futures::{stream, Async, AsyncSink, Future, IntoFuture, Poll, Sink, StartSend};

use futures_cpupool::CpuPool;

pub type TestFuture<E> = Box<Future<Item = (), Error = E> + Send + 'static>;

pub trait Testable: Send {
    type Error;
    fn test(self) -> TestFuture<Self::Error>;
}

pub trait BoxTestable: Send {
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
    T: Testable + Send + 'static,
{
    Test::Test {
        name: name.into(),
        test: Box::new(testable),
    }
}

#[macro_export]
macro_rules! tensile_fn {
    ($name:ident) => {
        $crate::test(stringify!($name), $name as fn() -> _)
    };
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
    E: Send + 'static,
{
    type Error = E;

    fn test(self) -> TestFuture<Self::Error> {
        Box::new(self.into_future())
    }
}

impl<F, T> Testable for F
where
    F: FnOnce() -> T + Send + ::std::panic::UnwindSafe + 'static,
    T: Testable + 'static,
    T::Error: for<'a> From<&'a str> + From<String> + Send,
{
    type Error = T::Error;

    fn test(self) -> TestFuture<Self::Error> {
        Box::new(future::lazy(move || {
            let result = ::std::panic::catch_unwind(|| self().test());
            match result {
                Ok(fut) => Either::A(fut),
                Err(err) => Either::B(
                    Err(err
                        .downcast::<T::Error>()
                        .map(|err| *err)
                        .or_else(|err| err.downcast::<String>().map(|s| (*s).into()))
                        .or_else(|err| err.downcast::<&str>().map(|s| (*s).into()))
                        .unwrap_or_else(|_| panic!("Unknown panic type"))).into_future(),
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

    fn run_test(self, starter: &mut Starter, path: &str) -> Option<RunningTest<Error>> {
        match self {
            Test::Test { name, test } => {
                let test_path = format!("{}/{}", path, name);
                if test_path.contains(&starter.options.filter) {
                    Some(RunTest::Test {
                        name,
                        test: Box::new(starter.cpu_pool.spawn(test.test())),
                    })
                } else {
                    starter.filtered_tests += 1;
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
    Error: fmt::Debug + fmt::Display + Send + 'static,
{
    fn print_all<S>(
        mut tests: Vec<RunningTest<Error>>,
        path: String,
        sink: S,
    ) -> Box<Future<Item = (Stats, S), Error = S::SinkError> + Send>
    where
        S: Sink<SinkItem = TestProgress<Error>> + Send + 'static,
        S::SinkError: Send,
    {
        match tests.pop() {
            Some(test) => Box::new(test.print(&path, sink).and_then(|(s1, sink)| {
                Self::print_all(tests, path, sink).map(|(s2, sink)| (s1 + s2, sink))
            })),
            None => Box::new(future::ok((Stats::default(), sink))),
        }
    }

    fn print<S>(
        self,
        path: &str,
        sink: S,
    ) -> Box<Future<Item = (Stats, S), Error = S::SinkError> + Send>
    where
        S: Sink<SinkItem = TestProgress<Error>> + Send + 'static,
        S::SinkError: Send,
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

struct Console<W, T> {
    writer: W,
    _marker: ::std::marker::PhantomData<T>,
}

impl<W, T> Console<W, T>
where
    W: termcolor::WriteColor,
    T: fmt::Display,
{
    fn new(writer: W) -> Self {
        Console {
            writer,
            _marker: ::std::marker::PhantomData,
        }
    }
}

struct Colored<T> {
    color: Option<termcolor::ColorSpec>,
    msg: T,
}

impl<T> From<T> for Colored<T> {
    fn from(msg: T) -> Self {
        Colored { color: None, msg }
    }
}

impl<W, T> Sink for Console<W, T>
where
    W: termcolor::WriteColor,
    T: fmt::Display,
{
    type SinkItem = Colored<T>;
    type SinkError = io::Error;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        match item.color {
            Some(color) => self.writer.set_color(&color)?,
            None => self.writer.reset()?,
        }
        write!(self.writer, "{}", item.msg)?;
        Ok(AsyncSink::Ready)
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        Ok(Async::Ready(()))
    }
}

#[derive(Default, StructOpt)]
pub struct Options {
    #[structopt(
        short = "j",
        long = "jobs",
        help = "Number of tests to run in parallel"
    )]
    jobs: Option<usize>,
    #[structopt(help = "String used to filter out tests")]
    filter: String,
    #[structopt(
        help = "Coloring: auto, always, always-ansi, never",
        parse(try_from_str)
    )]
    color: Color,
}

struct Color(termcolor::ColorChoice);

impl Default for Color {
    fn default() -> Color {
        Color(termcolor::ColorChoice::Auto)
    }
}

impl ::std::str::FromStr for Color {
    type Err = &'static str;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        use termcolor::ColorChoice::*;
        Ok(Color(match s {
            "auto" => Auto,
            "always" => Always,
            "always-ansi" => AlwaysAnsi,
            "never" => Never,
            _ => return Err("Expected on of auto, always, always-ansi, never"),
        }))
    }
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

    pub fn color(mut self, color: termcolor::ColorChoice) -> Options {
        self.color = Color(color);
        self
    }
}

struct TestReport {
    failed_tests: i32,
    successful_tests: i32,
    filtered_tests: i32,
}

fn execute_test_runner<S, Error>(
    sink: S,
    test: Test<Error>,
    options: &Options,
) -> impl Future<Item = TestReport, Error = S::SinkError>
where
    S: Sink<SinkItem = TestProgress<Error>> + Send + 'static,
    Error: fmt::Debug + fmt::Display + Send + 'static,
    S::SinkError: Send,
{
    let pool = futures_cpupool::Builder::new()
        .pool_size(options.jobs.unwrap_or_else(|| num_cpus::get()))
        .name_prefix("tensile-test-pool-")
        .create();

    let mut starter = Starter::new(&pool, options);
    let running_test = test.run_all(&mut starter);

    let stats = match running_test {
        Some(running_test) => Either::A(running_test.print("", sink).from_err().map(|t| t.0)),
        None => Either::B(future::ok(Stats::default())),
    };
    let filtered_tests = starter.filtered_tests;
    stats.map(move |stats| TestReport {
        failed_tests: stats.failed_tests,
        successful_tests: stats.successful_tests,
        filtered_tests,
    })
}

pub fn console_runner<Error>(
    test: Test<Error>,
    options: &Options,
) -> impl Future<Item = (), Error = failure::Error>
where
    Error: fmt::Debug + fmt::Display + Send + 'static,
{
    let color = options.color.0;
    let mut indent = String::new();

    let sink =
        Console::new(termcolor::StandardStream::stdout(color)).with_flat_map(move |progress| {
            let cmds = match progress {
                TestProgress::GroupStart(name) => {
                    let msg = format!("{}{}\n", indent, name);
                    indent.push('\t');
                    vec![Colored {
                        color: Some(termcolor::ColorSpec::new().set_bold(true).clone()),
                        msg,
                    }]
                }
                TestProgress::GroupEnd => {
                    indent.pop();
                    vec![Colored::from("\n".to_string())]
                }
                TestProgress::Test(name, result) => {
                    let name = Colored::from(format!("{}{} ... ", indent, name));
                    match result {
                        Ok(()) => vec![
                            name,
                            Colored {
                                color: Some(
                                    termcolor::ColorSpec::new()
                                        .set_fg(Some(termcolor::Color::Green))
                                        .clone(),
                                ),
                                msg: "ok".to_string(),
                            },
                            Colored::from("\n".to_string()),
                        ],
                        Err(err) => vec![
                            name,
                            Colored {
                                color: Some(
                                    termcolor::ColorSpec::new()
                                        .set_fg(Some(termcolor::Color::Red))
                                        .clone(),
                                ),
                                msg: "FAILED".to_string(),
                            },
                            Colored::from(format!("\n{}", err)),
                        ],
                    }
                }
            };
            stream::iter_ok(cmds)
        });

    execute_test_runner(sink, test, options)
        .from_err()
        .and_then(move |report| -> Result<_, failure::Error> {
            let mut writer = termcolor::StandardStream::stdout(color);
            write!(writer, "test result: ")?;
            if report.failed_tests == 0 {
                writer
                    .set_color(termcolor::ColorSpec::new().set_fg(Some(termcolor::Color::Green)))?;
                write!(writer, "ok")?;
            } else {
                writer
                    .set_color(termcolor::ColorSpec::new().set_fg(Some(termcolor::Color::Red)))?;
                write!(writer, "FAILED")?;
            }
            writer.reset()?;
            writeln!(
                writer,
                ". {} passed; {} failed; {} filtered",
                report.successful_tests, report.failed_tests, report.filtered_tests
            )?;
            // Filtering out all tests is almost certainly a mistake so report that as an error
            let filtered_all_tests = report.failed_tests == 0
                && report.successful_tests == 0
                && report.filtered_tests != 0;
            if report.failed_tests == 0 && !filtered_all_tests {
                Ok(())
            } else {
                Err(failure::err_msg("One or more tests failed"))
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    extern crate tokio;

    use futures::sync::mpsc::channel;
    use futures::Stream;

    use futures_cpupool::CpuPool;

    use self::tokio::runtime::current_thread::Runtime;

    fn test_test<Error>(test: Test<Error>) -> Vec<TestProgress<Error>>
    where
        Error: fmt::Debug + fmt::Display + Send + 'static,
    {
        options_test(test, Options::default())
    }

    fn options_test<Error>(test: Test<Error>, options: Options) -> Vec<TestProgress<Error>>
    where
        Error: fmt::Debug + fmt::Display + Send + 'static,
    {
        let pool = CpuPool::new(4);
        let running_test = test.run_all(&mut Starter::new(&pool, &options));

        let mut core = Runtime::new().unwrap();

        let (sender, receiver) = channel(10);
        let collector_future = pool.spawn(receiver.collect());

        match running_test {
            Some(running_test) => {
                core.spawn(
                    running_test
                        .print("", sender.sink_map_err(|_| panic!()))
                        .map(|_| ())
                        .map_err(|_| ()),
                );

                core.block_on(collector_future).unwrap()
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
                color: Color(termcolor::ColorChoice::Never),
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
