#[macro_use]
extern crate tensile;

use tokio;

use futures::{future};
use tensile::{console_runner, group, Options};

fn test1() {
    assert!(true);
}

fn test2() -> bool {
    false
}

fn main() {
    let test = group("group1", vec![tensile_fn!(test1), tensile_fn!(test2)]);
    let options = Options::new();

    let mut runtime = tokio::runtime::current_thread::Runtime::new().unwrap();
    runtime
        .block_on(future::lazy(|| console_runner(test, &options)))
        .unwrap_err();
}
