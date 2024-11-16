use nimiq_block::Block;
use nimiq_serde::Deserialize;

use std::io::{self, Read as _};

fn main() {
    let mut buf = Vec::new();
    io::stdin().read_to_end(&mut buf).unwrap();

    let block = Block::deserialize_from_vec(&buf).unwrap();

    println!("{}", block.hash());
    println!("{:#?}", block);
}
