use std::ffi::c_char;

pub fn copy_utf8(source: &str, target: &mut [c_char]) {
    if target.is_empty() {
        return;
    }
    let mut length = source.len().min(target.len() - 1);
    while !source.is_char_boundary(length) {
        length -= 1;
    }
    for (slot, byte) in target.iter_mut().zip(&source.as_bytes()[..length]) {
        *slot = *byte as c_char;
    }
    target[length] = 0;
}

pub fn copy_utf16(source: &str, target: &mut [u16]) {
    if target.is_empty() {
        return;
    }
    let capacity = target.len() - 1;
    let mut length = 0;
    for (slot, unit) in target[..capacity].iter_mut().zip(source.encode_utf16()) {
        *slot = unit;
        length += 1;
    }
    target[length] = 0;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn as_bytes(chars: &[c_char]) -> Vec<u8> {
        chars.iter().take_while(|c| **c != 0).map(|c| *c as u8).collect()
    }

    #[test]
    fn short_text_is_copied_whole() {
        let mut target = [127 as c_char; 8];
        copy_utf8("OTT", &mut target);
        assert_eq!(as_bytes(&target), b"OTT");
    }

    #[test]
    fn long_text_is_cut_and_still_terminated() {
        let mut target = [127 as c_char; 4];
        copy_utf8("abcdefgh", &mut target);
        assert_eq!(as_bytes(&target), b"abc");
        assert_eq!(target[3], 0);
    }

    #[test]
    fn multibyte_characters_are_never_split() {
        let mut target = [127 as c_char; 4];
        copy_utf8("яяя", &mut target);
        assert_eq!(as_bytes(&target), "я".as_bytes());
    }

    #[test]
    fn empty_targets_are_left_alone() {
        copy_utf8("abc", &mut []);
        copy_utf16("abc", &mut []);
    }

    #[test]
    fn utf16_is_cut_and_terminated() {
        let mut target = [9u16; 3];
        copy_utf16("abc", &mut target);
        assert_eq!(target, [97, 98, 0]);
    }
}
