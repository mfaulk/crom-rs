/// Run-length encode `input`.
///
/// Each maximal run of one repeated character is written as that character
/// followed by the run length in decimal. A run of length 1 is written as the
/// bare character, with no count. Characters are Unicode scalar values (`char`),
/// so a multi-byte character counts as one.
///
/// ```
/// assert_eq!(rle::encode("aaabcc"), "a3bc2");
/// assert_eq!(rle::encode(""), "");
/// ```
pub fn encode(input: &str) -> String {
    let _ = input;
    todo!("run-length encode as documented above")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_characters_have_no_count() {
        assert_eq!(encode("abc"), "abc");
    }

    #[test]
    fn runs_are_counted() {
        assert_eq!(encode("zzzz"), "z4");
    }
}
