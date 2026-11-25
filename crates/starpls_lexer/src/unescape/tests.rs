use std::fmt::Write;
use std::str;

use expect_test::expect;
use expect_test::Expect;

use super::*;

#[test]
fn test_unescape_string() {
    fn check(input: &str, raw: bool, expect: Expect) {
        let mut actual = String::new();
        unescape_string(input, raw, false, &mut |range, res| {
            write!(&mut actual, "{:?} ", range).unwrap();
            match res {
                Ok(c) => {
                    writeln!(&mut actual, "{:?}", c).unwrap();
                }
                Err(err) => {
                    writeln!(&mut actual, "{:?}", err).unwrap();
                }
            }
        });
        expect.assert_eq(&actual);
    }
    check(
        "\\0",
        false,
        expect![[r#"
        0..2 '\0'
    "#]],
    );
    check(
        "\\世a",
        true,
        expect![[r#"
            0..1 '\\'
            1..4 '世'
            4..5 'a'
        "#]],
    );
    check(
        "\\\"",
        true,
        expect![[r#"
        0..2 '"'
    "#]],
    );
    check(
        "\\'",
        true,
        expect![[r#"
        0..2 '\''
    "#]],
    );
    check(
        "\\\n",
        true,
        expect![[r#"
        0..1 '\\'
        1..2 '\n'
    "#]],
    );
}

#[test]
fn test_unescape_byte_string() {
    fn check(input: &str, expect: Expect) {
        let mut actual = String::new();
        unescape_byte_string(input, false, &mut |range, res| {
            write!(&mut actual, "{:?} ", range).unwrap();
            match res {
                Ok(b) => {
                    if let Ok(s) = str::from_utf8(b) {
                        writeln!(&mut actual, "{:?}", s).unwrap();
                    } else {
                        writeln!(&mut actual, "{:x?}", b).unwrap();
                    }
                }
                Err(err) => {
                    writeln!(&mut actual, "{:?}", err).unwrap();
                }
            }
        });
        expect.assert_eq(&actual);
    }

    check(
        "\\0",
        expect![[r#"
            0..2 "\0"
        "#]],
    );
    check(
        r#"AЀ世😿"#,
        expect![[r#"
            0..1 "A"
            1..3 "Ѐ"
            3..6 "世"
            6..10 "😿"
        "#]],
    );
    check(
        r#"\x41\u0400\u4e16\U0001F63F"#,
        expect![[r#"
        0..4 "A"
        4..10 "Ѐ"
        10..16 "世"
        16..26 "😿"
    "#]],
    );
    check(
        r#"\377\378\x80\xff\xff"#,
        expect![[r#"
        0..4 [ff]
        4..7 "\u{1f}"
        7..8 "8"
        8..12 [80]
        12..16 [ff]
        16..20 [ff]
    "#]],
    );
}

#[test]
fn test_hexadecimal_escape_takes_two_digits() {
    fn check(input: &str, expect: Expect) {
        let mut actual = String::new();
        unescape_string(input, false, false, &mut |range, res| {
            writeln!(&mut actual, "{:?} {:?}", range, res).unwrap();
        });
        expect.assert_eq(&actual);
    }

    // `\x41A` is `A` followed by a literal `A`, not a three-digit escape.
    check(
        r"\x41A",
        expect![[r#"
        0..4 Ok('A')
        4..5 Ok('A')
    "#]],
    );
    check(
        r"\x4",
        expect![[r#"
        0..3 Err(TooShortHexadecimalEscape)
    "#]],
    );
}

#[test]
fn test_raw_byte_string_has_no_escapes() {
    fn check(input: &str, expect: Expect) {
        let mut actual = String::new();
        unescape_byte_string(input, true, &mut |range, res| {
            writeln!(
                &mut actual,
                "{:?} {:?}",
                range,
                res.map(|b| str::from_utf8(b).unwrap().to_string())
            )
            .unwrap();
        });
        expect.assert_eq(&actual);
    }

    check(
        r"\d+",
        expect![[r#"
        0..1 Ok("\\")
        1..2 Ok("d")
        2..3 Ok("+")
    "#]],
    );
    check(
        r#"a\"b"#,
        expect![[r#"
        0..1 Ok("a")
        1..3 Ok("\"")
        3..4 Ok("b")
    "#]],
    );
}
