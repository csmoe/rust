# `dotdot_pattern`

The tracking issue for this feature is: [#62254]

[#62254]: https://github.com/rust-lang/rust/issues/62254

------------------------

`dotdot_pattern` makes `..` a pattern rather than a syntactic fragment of some other patterns.

## Examples

```rust
#![feature(dotdot_pattern)]

fn main() {
    let [x, y @ .., z] = [1, 2, 3, 4];
    assert_eq!(y, &[2, 3]);
}
```
