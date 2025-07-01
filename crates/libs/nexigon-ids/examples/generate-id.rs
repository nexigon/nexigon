//! Generates a random id of with a given tag.

use std::str::FromStr;

use nexigon_ids::Id;
use nexigon_ids::Tag;

pub fn main() {
    let Some(tag) = std::env::args().skip(1).next() else {
        eprintln!("usage: generate-id <tag>");
        std::process::exit(1);
    };
    let Ok(tag) = Tag::from_str(&tag) else {
        eprintln!("{tag:?} is not a valid tag");
        std::process::exit(1);
    };
    let id = tag.generate();
    println!("{}", id.stringify());
}
