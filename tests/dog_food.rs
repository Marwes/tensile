#[macro_use]
extern crate tensile;
extern crate futures;

use futures::Future;
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
    console_runner(test, &options).wait().unwrap_err();
}
