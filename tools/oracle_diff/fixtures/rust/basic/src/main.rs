use crate::lib::helper;

fn run() -> u32 {
    let a = helper();      // cross-file → resolves to lib::helper
    let b = local();       // same-file → resolves to main::local
    a + b
}

fn local() -> u32 {
    external_thing();      // unresolved → no definition anywhere
    7
}

fn main() {
    let _ = run();
    make_widget();         // cross-file → lib::make_widget
}
