use rustc_data_structures::fx::FxHashMap;
use std::collections::hash_map::Entry;
use std::fs;
use std::iter::Peekable;
use std::path::Path;
use std::str::Chars;

use rustc_errors::Handler;

#[cfg(test)]
mod tests;

#[derive(Debug)]
pub(crate) struct CssPath {
    pub(crate) rules: FxHashMap<String, String>,
    pub(crate) children: FxHashMap<String, CssPath>,
}

/// When encountering a `"` or a `'`, returns the whole string, including the quote characters.
fn get_string(iter: &mut Peekable<Chars<'_>>, string_start: char) -> String {
    let mut s = String::with_capacity(2);

    s.push(string_start);
    while let Some(c) = iter.next() {
        s.push(c);
        if c == '\\' {
            iter.next();
        } else if c == string_start {
            break;
        }
    }
    s
}

/// Skips a `/*` comment.
fn skip_comment(iter: &mut Peekable<Chars<'_>>) {
    while let Some(c) = iter.next() {
        if c == '*' && iter.next() == Some('/') {
            break;
        }
    }
}

/// Skips a line comment (`//`).
fn skip_line_comment(iter: &mut Peekable<Chars<'_>>) {
    while let Some(c) = iter.next() {
        if c == '\n' {
            break;
        }
    }
}

fn handle_common_chars(c: char, buffer: &mut String, iter: &mut Peekable<Chars<'_>>) {
    match c {
        '"' | '\'' => buffer.push_str(&get_string(iter, c)),
        '/' if iter.peek() == Some(&'*') => skip_comment(iter),
        '/' if iter.peek() == Some(&'/') => skip_line_comment(iter),
        _ => buffer.push(c),
    }
}

/// Returns a CSS property name. Ends when encountering a `:` character.
///
/// If the `:` character isn't found, returns `None`.
///
/// If a `{` character is encountered, returns an error.
fn parse_property_name(iter: &mut Peekable<Chars<'_>>) -> Result<Option<String>, String> {
    let mut content = String::new();

    while let Some(c) = iter.next() {
        match c {
            ':' => return Ok(Some(content.trim().to_owned())),
            '{' => return Err("Unexpected `{` in a `{}` block".to_owned()),
            '}' => break,
            _ => handle_common_chars(c, &mut content, iter),
        }
    }
    Ok(None)
}

/// Try to get the value of a CSS property (the `#fff` in `color: #fff`). It'll stop when it
/// encounters a `{` or a `;` character.
///
/// It returns the value string and a boolean set to `true` if the value is ended with a `}` because
/// it means that the parent block is done and that we should notify the parent caller.
fn parse_property_value(iter: &mut Peekable<Chars<'_>>) -> (String, bool) {
    let mut value = String::new();
    let mut out_block = false;

    while let Some(c) = iter.next() {
        match c {
            ';' => break,
            '}' => {
                out_block = true;
                break;
            }
            _ => handle_common_chars(c, &mut value, iter),
        }
    }
    (value.trim().to_owned(), out_block)
}

/// This is used to parse inside a CSS `{}` block. If we encounter a new `{` inside it, we consider
/// it as a new block and therefore recurse into `parse_rules`.
fn parse_rules(
    content: &str,
    selector: String,
    iter: &mut Peekable<Chars<'_>>,
    paths: &mut FxHashMap<String, CssPath>,
) -> Result<(), String> {
    let mut rules = FxHashMap::default();
    let mut children = FxHashMap::default();

    loop {
        // If the parent isn't a "normal" CSS selector, we only expect sub-selectors and not CSS
        // properties.
        if selector.starts_with('@') {
            parse_selectors(content, iter, &mut children)?;
            break;
        }
        let rule = match parse_property_name(iter)? {
            Some(r) => {
                if r.is_empty() {
                    return Err(format!("Found empty rule in selector `{selector}`"));
                }
                r
            }
            None => break,
        };
        let (value, out_block) = parse_property_value(iter);
        if value.is_empty() {
            return Err(format!("Found empty value for rule `{rule}` in selector `{selector}`"));
        }
        match rules.entry(rule) {
            Entry::Occupied(mut o) => {
                eprintln!("Duplicated rule `{}` in CSS selector `{selector}`", o.key());
                *o.get_mut() = value;
            }
            Entry::Vacant(v) => {
                v.insert(value);
            }
        }
        if out_block {
            break;
        }
    }

    match paths.entry(selector) {
        Entry::Occupied(mut o) => {
            eprintln!("Duplicated CSS selector: `{}`", o.key());
            let v = o.get_mut();
            for (key, value) in rules.into_iter() {
                v.rules.insert(key, value);
            }
            for (sel, child) in children.into_iter() {
                v.children.insert(sel, child);
            }
        }
        Entry::Vacant(v) => {
            v.insert(CssPath { rules, children });
        }
    }
    Ok(())
}

pub(crate) fn parse_selectors(
    content: &str,
    iter: &mut Peekable<Chars<'_>>,
    paths: &mut FxHashMap<String, CssPath>,
) -> Result<(), String> {
    let mut selector = String::new();

    while let Some(c) = iter.next() {
        match c {
            '{' => {
                let s = minifier::css::minify(selector.trim()).map(|s| s.to_string())?;
                parse_rules(content, s, iter, paths)?;
                selector.clear();
            }
            '}' => break,
            ';' => selector.clear(), // We don't handle inline selectors like `@import`.
            _ => handle_common_chars(c, &mut selector, iter),
        }
    }
    Ok(())
}

/// The entry point to parse the CSS rules. Every time we encounter a `{`, we then parse the rules
/// inside it.
pub(crate) fn load_css_paths(content: &str) -> Result<FxHashMap<String, CssPath>, String> {
    let mut iter = content.chars().peekable();
    let mut paths = FxHashMap::default();

    parse_selectors(content, &mut iter, &mut paths)?;
    Ok(paths)
}

pub(crate) fn get_differences(
    origin: &FxHashMap<String, CssPath>,
    against: &FxHashMap<String, CssPath>,
    v: &mut Vec<String>,
) {
    for (selector, entry) in origin.iter() {
        match against.get(selector) {
            Some(a) => {
                get_differences(&entry.children, &a.children, v);
                if selector == ":root" {
                    // We need to check that all variables have been set.
                    for rule in entry.rules.keys() {
                        if !a.rules.contains_key(rule) {
                            v.push(format!("  Missing CSS variable `{rule}` in `:root`"));
                        }
                    }
                }
            }
            None => v.push(format!("  Missing rule `{selector}`")),
        }
    }
}

pub(crate) fn test_theme_against<P: AsRef<Path>>(
    f: &P,
    origin: &FxHashMap<String, CssPath>,
    diag: &Handler,
) -> (bool, Vec<String>) {
    let against = match fs::read_to_string(f)
        .map_err(|e| e.to_string())
        .and_then(|data| load_css_paths(&data))
    {
        Ok(c) => c,
        Err(e) => {
            diag.struct_err(&e).emit();
            return (false, vec![]);
        }
    };

    let mut ret = vec![];
    get_differences(origin, &against, &mut ret);
    (true, ret)
}
