fn bar<S: Into<String>>(s: S) {
    some_work(); // s: S
    more_work(); // s: S
    let s: String = s.into(); // s: String
    even_more_work(&s); // s: String
    yet_even_more_work(s); // s: String
}

fn some_work() {}
fn more_work() {}
fn even_more_work(s: &String) {}
fn yet_even_more_work(s: String) {}
