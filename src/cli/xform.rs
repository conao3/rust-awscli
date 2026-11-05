pub(crate) fn xform_name_uncached(name: &str, sep: &str) -> String {
    let bytes = name.as_bytes();
    let mut name = name.to_string();
    let end = match bytes {
        [.., b's', b'\n'] => Some(bytes.len() - 2),
        [.., b's'] => Some(bytes.len() - 1),
        _ => None,
    };
    if let Some(end) = end {
        let start = bytes[..end]
            .iter()
            .rposition(|b| !b.is_ascii_uppercase())
            .map_or(0, |i| i + 1);
        if end - start >= 2 {
            let matched = &bytes[start..=end];
            name = format!(
                "{}{sep}{}",
                &name[..name.len() - matched.len()],
                std::str::from_utf8(matched).unwrap().to_lowercase()
            );
        }
    }
    let mut s1 = String::with_capacity(name.len() + 8);
    let mut chars = name.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let rest = &name.as_bytes()[i + c.len_utf8()..];
        let lower = rest
            .iter()
            .skip(1)
            .take_while(|b| b.is_ascii_lowercase())
            .count();
        s1.push(c);
        if c != '\n' && rest.first().is_some_and(u8::is_ascii_uppercase) && lower > 0 {
            s1.push_str(sep);
            s1.push_str(std::str::from_utf8(&rest[..1 + lower]).unwrap());
            for _ in 0..1 + lower {
                chars.next();
            }
        }
    }
    let s1 = s1.as_bytes();
    let mut out = String::with_capacity(s1.len() + 8);
    let mut i = 0;
    while i < s1.len() {
        let b = s1[i];
        if (b.is_ascii_lowercase() || b.is_ascii_digit())
            && s1.get(i + 1).is_some_and(u8::is_ascii_uppercase)
        {
            out.push(b as char);
            out.push_str(sep);
            out.push(s1[i + 1] as char);
            i += 2;
        } else {
            let len = utf8_len(b);
            out.push_str(std::str::from_utf8(&s1[i..i + len]).unwrap());
            i += len;
        }
    }
    out.to_lowercase()
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}
