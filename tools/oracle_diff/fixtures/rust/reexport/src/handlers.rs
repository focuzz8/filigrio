use crate::prelude::hello;

fn boot() -> u32 {
    hello() + log()
}

fn log() -> u32 { 2 }
