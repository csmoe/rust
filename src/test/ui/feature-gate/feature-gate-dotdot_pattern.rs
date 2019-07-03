// gate-test-dotdot_pattern

#![feature(dotdot_pattern)]

fn main() {
    struct TupleStruct(i32, i32, i32);
    let x = vec![1, 2, 3];
    let y = TupleStruct(1, 2, 3);

    if let TupleStruct(1, .., 3) = y {}
    if let [1, .., 3] = x {}
}
