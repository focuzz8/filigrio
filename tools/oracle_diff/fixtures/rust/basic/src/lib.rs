pub fn helper() -> u32 { 42 }

pub struct Widget { pub n: u32 }

pub fn make_widget() -> Widget {
    let x = helper();
    Widget { n: x }
}
