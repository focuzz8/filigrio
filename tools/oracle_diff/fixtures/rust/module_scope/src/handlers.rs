use crate::api::greet;

fn boot() -> u32 {
    greet() + log()
}

fn log() -> u32 { 3 }
