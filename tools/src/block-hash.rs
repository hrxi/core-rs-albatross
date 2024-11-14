use nimiq_block::Block;
use nimiq_serde::Deserialize as _;
use std::io;
use std::io::Read as _;

fn main() {
    let mut buf = Vec::new();
    io::stdin().read_to_end(&mut buf).unwrap();
    let mut block = Block::deserialize_from_vec(&buf).unwrap();
    println!("{}", block.hash_cached());
    println!("{:#?}", block);
}
