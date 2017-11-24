#[macro_use]
extern crate tensile;

use tensile::{console_runner, group, Options};


fn test1() {
    assert!(true);
}

fn test2() -> bool {
    false
}

fn main() {
    let test = group("group1", vec![tensile_fn!(test1), tensile_fn!(test2)]);
    console_runner(test, &Options::new()).unwrap_err();
}
