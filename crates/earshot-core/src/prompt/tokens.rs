//! Token estimate without a tokenizer.

/// Tokens added per chat message for the role and separators (the OpenAI cookbook counts
/// 3 to 4 per message for its chat models; the larger figure is used).
pub const TOKENS_PER_MESSAGE: usize = 4;

/// Tokens added once per request for priming the reply.
pub const TOKENS_PER_REQUEST: usize = 3;

/// A deliberately pessimistic estimate of the number of tokens `text` takes in a BPE
/// tokenizer of the GPT-2 / cl100k family (also close for Llama-style SentencePiece
/// vocabularies on English text).
///
/// Rules, applied to maximal runs of characters:
/// - ASCII letters: one token per started group of 4 (common English words are one token,
///   so English prose is overestimated by roughly a third);
/// - ASCII digits: one token per started group of 3 (cl100k splits numbers into groups of
///   at most three digits);
/// - whitespace: free (BPE merges a leading space into the next word), except newlines,
///   one token each;
/// - any other character (punctuation, symbols, non-ASCII): one token each.
///
/// Hex-heavy text such as UUIDs comes out at about 20 tokens per id, close to what cl100k
/// produces. The estimate is subadditive: `estimate(a + b) <= estimate(a) + estimate(b)`,
/// because joining two strings can only merge runs, so a sum of part estimates is an upper
/// bound for the joined text. The prompt builder relies on that when it fills a context
/// window.
#[must_use]
pub fn estimate_tokens(text: &str) -> usize {
    let mut tokens = 0;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_alphabetic() || c.is_ascii_digit() {
            let digit = c.is_ascii_digit();
            let mut n = 1usize;
            while chars.peek().is_some_and(|&d| {
                if digit {
                    d.is_ascii_digit()
                } else {
                    d.is_ascii_alphabetic()
                }
            }) {
                chars.next();
                n += 1;
            }
            tokens += n.div_ceil(if digit { 3 } else { 4 });
        } else if c == '\n' || !c.is_whitespace() {
            tokens += 1;
        }
    }
    tokens
}
