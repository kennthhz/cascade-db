//! Compile-fail: a `submit_to` closure capturing `!Send` state (an
//! `Rc`) must be rejected — captured state crosses reactors (FR-35,
//! R14). Only state CREATED INSIDE the future may be `!Send`.

async fn go() {
    let (init, _rx) = cascade_runtime::submit::channel(8);
    let captured: std::rc::Rc<usize> = std::rc::Rc::new(7);
    let handle = init.connect().await;
    let _submit = handle.submit_to(move || async move { *captured + 1 });
}

fn main() {
    go();
}
