// edition: 2018
// compile-flags: -O --crate-type=lib

#[derive(Clone)]
pub enum Foo {
    A(u8),
    B(bool),
}

pub fn clone_foo(f: &Foo) -> Foo {
    f.clone()
}
