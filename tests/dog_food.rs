use tensile::{console_runner, group, tensile_fn, Options};

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
        .block_on(async move { console_runner(test, &options).await })
        .unwrap_err();
}
