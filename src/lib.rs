use std::{
    fmt,
    io::{self, Write},
    ops::{Add, AddAssign},
    pin::Pin,
};

use termcolor::WriteColor;

use futures::{
    future::{self, BoxFuture},
    pin_mut,
    prelude::*,
    stream,
    task::{self, SpawnExt},
};

pub type TestFuture<E> = BoxFuture<'static, Result<(), E>>;

pub trait Testable: Send {
    type Error;
    fn test(self) -> TestFuture<Self::Error>;
}

pub trait BoxTestable: Send {
    type Error;
    fn test_box(self: Box<Self>) -> TestFuture<Self::Error>;
}

impl<Error> Testable for Box<dyn BoxTestable<Error = Error>> {
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
        test: Box<dyn BoxTestable<Error = Error>>,
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
        Box::pin(async move {
            if self {
                Ok(())
            } else {
                Err("false".to_string())
            }
        })
    }
}

impl Testable for () {
    type Error = String;

    fn test(self) -> TestFuture<Self::Error> {
        Box::pin(async { Ok(()) })
    }
}

impl<E> Testable for Result<(), E>
where
    E: Send + 'static,
{
    type Error = E;

    fn test(self) -> TestFuture<Self::Error> {
        Box::pin(async { self })
    }
}

pub struct Future<F>(pub F);

impl<F> Testable for Future<F>
where
    F: std::future::Future + Send + std::panic::UnwindSafe + 'static,
    F::Output: Testable + 'static,
    <F::Output as Testable>::Error: for<'a> From<&'a str> + From<String> + Send,
{
    type Error = <F::Output as Testable>::Error;

    fn test(self) -> TestFuture<Self::Error> {
        Box::pin(async move {
            match self.0.catch_unwind().await {
                Ok(out) => out.test().await,
                Err(err) => Err(err
                    .downcast::<<F::Output as Testable>::Error>()
                    .map(|err| *err)
                    .or_else(|err| err.downcast::<String>().map(|s| (*s).into()))
                    .or_else(|err| err.downcast::<&str>().map(|s| (*s).into()))
                    .unwrap_or_else(|_| panic!("Unknown panic type"))),
            }
        })
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
        Box::pin(async move {
            let result = ::std::panic::catch_unwind(|| self().test());
            match result {
                Ok(fut) => fut.await,
                Err(err) => Err(err
                    .downcast::<T::Error>()
                    .map(|err| *err)
                    .or_else(|err| err.downcast::<String>().map(|s| (*s).into()))
                    .or_else(|err| err.downcast::<&str>().map(|s| (*s).into()))
                    .unwrap_or_else(|_| panic!("Unknown panic type"))),
            }
        })
    }
}

struct Starter<'a, E> {
    options: &'a Options,
    filtered_tests: i32,
    executor: Option<E>,
}

struct DummyExecutor;
impl task::Spawn for DummyExecutor {
    fn spawn_obj(&self, _future: future::FutureObj<'static, ()>) -> Result<(), task::SpawnError> {
        unreachable!()
    }
}

#[cfg(test)]
impl<'a> Starter<'a, DummyExecutor> {
    fn new(options: &'a Options) -> Self {
        Self::with_executor(options, None)
    }
}

impl<'a, E> Starter<'a, E> {
    fn with_executor(options: &'a Options, executor: Option<E>) -> Self {
        Starter {
            options,
            filtered_tests: 0,
            executor,
        }
    }
}

impl<Error> Test<Error>
where
    Error: Send + 'static,
{
    fn run_all<E>(self, starter: &mut Starter<'_, E>) -> Option<RunningTest<Error>>
    where
        E: task::Spawn,
    {
        self.run_test(starter, "")
    }

    fn run_test<E>(self, starter: &mut Starter<'_, E>, path: &str) -> Option<RunningTest<Error>>
    where
        E: task::Spawn,
    {
        match self {
            Test::Test { name, test } => {
                let test_path = format!("{}/{}", path, name);
                if test_path.contains(&starter.options.filter) {
                    let test = test.test();
                    let f = match &mut starter.executor {
                        Some(executor) => {
                            let (tx, rx) = futures::channel::oneshot::channel();
                            executor
                                .spawn(test.map(move |result| {
                                    tx.send(result).ok().unwrap();
                                }))
                                .unwrap();
                            rx.map(|result| result.unwrap()).boxed()
                        }
                        None => Box::pin(async move { test.await }),
                    };
                    Some(RunTest::Test { name, test: f })
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
    async fn print<'s, S>(self, sink: Pin<&'s mut S>) -> Result<Stats, S::Error>
    where
        S: Sink<TestProgress<Error>> + Send + 'static,
        S::Error: Send,
    {
        self.print_path(&mut String::new(), sink).await
    }

    fn print_path<'s, S>(
        self,
        path: &'s mut String,
        mut sink: Pin<&'s mut S>,
    ) -> BoxFuture<'s, Result<Stats, S::Error>>
    where
        S: Sink<TestProgress<Error>> + Send + 'static,
        S::Error: Send,
    {
        Box::pin(async move {
            match self {
                RunTest::Test { name, test } => {
                    let result = test.await;
                    let stats = Stats {
                        successful_tests: result.is_ok() as i32,
                        failed_tests: result.is_err() as i32,
                    };
                    sink.send(TestProgress::Test(name, result)).await?;
                    Ok(stats)
                }
                RunTest::Group { name, tests } => {
                    let before = path.len();
                    if path != "" {
                        path.push('/');
                    }
                    path.push_str(&name);

                    sink.as_mut()
                        .send(TestProgress::GroupStart(name.into()))
                        .await?;

                    let mut stats = Stats::default();
                    for test in tests {
                        stats += test.print_path(path, sink.as_mut()).await?;
                    }

                    path.truncate(before);

                    sink.as_mut().send(TestProgress::GroupEnd).await?;

                    Ok(stats)
                }
            }
        })
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

#[derive(Default, structopt_derive::StructOpt)]
pub struct Options {
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

async fn execute_test_runner<S, Error, E>(
    sink: Pin<&mut S>,
    test: Test<Error>,
    options: &Options,
    executor: Option<E>,
) -> Result<TestReport, S::Error>
where
    S: Sink<TestProgress<Error>> + Send + 'static,
    Error: fmt::Debug + fmt::Display + Send + 'static,
    S::Error: Send,
    E: task::Spawn,
{
    let mut starter = Starter::with_executor(options, executor);
    let running_test = test.run_all(&mut starter);

    let stats = match running_test {
        Some(running_test) => running_test.print(sink).err_into().await?,
        None => Stats::default(),
    };
    let filtered_tests = starter.filtered_tests;
    Ok(TestReport {
        failed_tests: stats.failed_tests,
        successful_tests: stats.successful_tests,
        filtered_tests,
    })
}

#[cfg(feature = "tokio")]
pub async fn tokio_console_runner<Error>(
    test: Test<Error>,
    options: &Options,
) -> Result<(), failure::Error>
where
    Error: fmt::Debug + fmt::Display + Send + 'static,
{
    struct TokioExecutor;
    impl task::Spawn for TokioExecutor {
        fn spawn_obj(
            &self,
            future: future::FutureObj<'static, ()>,
        ) -> Result<(), task::SpawnError> {
            tokio::spawn(future);
            Ok(())
        }
    }
    executor_console_runner(test, options, Some(TokioExecutor)).await
}

pub async fn console_runner<Error>(
    test: Test<Error>,
    options: &Options,
) -> Result<(), failure::Error>
where
    Error: fmt::Debug + fmt::Display + Send + 'static,
{
    executor_console_runner(test, options, None::<DummyExecutor>).await
}

fn console_sink<T>(mut writer: impl WriteColor) -> impl Sink<Colored<T>, Error = io::Error>
where
    T: fmt::Display,
{
    futures::sink::drain()
        .sink_map_err::<io::Error, _>(|_| unreachable!())
        .with(move |item: Colored<T>| {
            future::ready((|| -> io::Result<_> {
                match item.color {
                    Some(color) => writer.set_color(&color)?,
                    None => writer.reset()?,
                }
                write!(writer, "{}", item.msg)?;
                Ok(())
            })())
        })
}

pub async fn executor_console_runner<Error, E>(
    test: Test<Error>,
    options: &Options,
    executor: Option<E>,
) -> Result<(), failure::Error>
where
    Error: fmt::Debug + fmt::Display + Send + 'static,
    E: task::Spawn,
{
    let color = options.color.0;
    let mut indent = String::new();

    let writer = termcolor::StandardStream::stdout(color);
    let sink = console_sink(writer).with_flat_map(move |progress| {
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
        stream::iter(cmds.into_iter().map(Ok))
    });
    pin_mut!(sink);

    let report = execute_test_runner(sink, test, options, executor).await?;
    let mut writer = termcolor::StandardStream::stdout(color);
    write!(writer, "test result: ")?;
    if report.failed_tests == 0 {
        writer.set_color(termcolor::ColorSpec::new().set_fg(Some(termcolor::Color::Green)))?;
        write!(writer, "ok")?;
    } else {
        writer.set_color(termcolor::ColorSpec::new().set_fg(Some(termcolor::Color::Red)))?;
        write!(writer, "FAILED")?;
    }
    writer.reset()?;
    writeln!(
        writer,
        ". {} passed; {} failed; {} filtered",
        report.successful_tests, report.failed_tests, report.filtered_tests
    )?;
    // Filtering out all tests is almost certainly a mistake so report that as an error
    let filtered_all_tests =
        report.failed_tests == 0 && report.successful_tests == 0 && report.filtered_tests != 0;
    if report.failed_tests == 0 && !filtered_all_tests {
        Ok(())
    } else {
        Err(failure::err_msg("One or more tests failed"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tokio;

    use futures::channel::mpsc::channel;

    async fn test_test<Error>(test: Test<Error>) -> Vec<TestProgress<Error>>
    where
        Error: fmt::Debug + fmt::Display + Send + 'static,
    {
        options_test(test, Options::default()).await
    }

    async fn options_test<Error>(test: Test<Error>, options: Options) -> Vec<TestProgress<Error>>
    where
        Error: fmt::Debug + fmt::Display + Send + 'static,
    {
        let running_test = test.run_all(&mut Starter::new(&options));

        let (sender, receiver) = channel(10);

        match running_test {
            Some(running_test) => {
                tokio::spawn(async move {
                    let sender = sender.sink_map_err(|_| panic!());
                    pin_mut!(sender);
                    running_test.print(sender).map(|_| ()).await
                });

                receiver.collect().await
            }
            None => vec![],
        }
    }

    #[tokio::test]
    async fn simple() {
        let progress = test_test(test("1", true)).await;
        assert_eq!(progress, vec![TestProgress::Test("1".to_string(), Ok(()))]);
    }

    #[tokio::test]
    async fn grouped_tests() {
        let progress = test_test(group("group", vec![test("1", true), test("2", false)])).await;
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

    #[tokio::test]
    async fn nested_groups() {
        let progress = test_test(group(
            "group",
            vec![test("1", true), group("inner", vec![])],
        ))
        .await;
        assert_eq!(
            progress,
            vec![
                TestProgress::GroupStart("group".into()),
                TestProgress::Test("1".into(), Ok(())),
                TestProgress::GroupEnd,
            ]
        );
    }

    #[tokio::test]
    async fn mixed_test_and_groups() {
        let progress = test_test(group(
            "group",
            vec![
                group("inner1", vec![test("test1", true)]),
                test("middle test", true),
                group("inner2", vec![test("test2", false)]),
            ],
        ))
        .await;
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

    #[tokio::test]
    async fn handle_panic() {
        fn test1() {
            panic!("fail")
        }
        let progress = test_test(group(
            "group",
            vec![tensile_fn!(test1), test("test2", true)],
        ))
        .await;
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

    #[tokio::test]
    async fn filter() {
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
                color: Color(termcolor::ColorChoice::Never),
            },
        )
        .await;
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
