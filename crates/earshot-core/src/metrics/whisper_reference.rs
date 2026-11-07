//! Reference cases for [`normalize_english`] against OpenAI Whisper's own normaliser.
//!
//! Every expected string below is hand-copied from the output of upstream
//! `whisper.normalizers.EnglishTextNormalizer` (openai/whisper commit
//! 86098128c0b4f24f0e2aa2994de830614b474227, `english.py` + `basic.py` + `english.json`),
//! run once under CPython 3.14 outside the build. They were not produced by this crate.
//!
//! Our only intended difference is that the result is trimmed (Whisper can leave one
//! leading or trailing space), so the expected value is trimmed before comparing.

use super::normalize::{american_spelling, normalize_english, spelling_table_len};

#[track_caller]
fn whisper(input: &str, upstream: &str) {
    assert_eq!(
        normalize_english(input),
        upstream.trim(),
        "input: {input:?}"
    );
}

// ---------------------------------------------------------------- spellings

#[test]
fn spelling_table_is_the_full_upstream_file() {
    let raw = include_str!("data/english.json");
    let parsed: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(raw).expect("english.json is valid JSON");
    assert_eq!(parsed.len(), 1739, "upstream english.json has 1739 entries");
    assert_eq!(spelling_table_len(), parsed.len());
    for (british, american) in &parsed {
        let american = american.as_str().expect("string value");
        assert_eq!(american_spelling(british), Some(american), "{british}");
    }
}

#[test]
fn whisper_british_spellings() {
    whisper(
        "The colour of the centre was marvellous.",
        "the color of the center was marvelous",
    );
    // "learnt" and "civilisation" are not in Whisper's table, so they stay as they are.
    whisper(
        "She learnt about civilisation in the theatre.",
        "she learnt about civilisation in the theater",
    );
    whisper(
        "I apologise, the programme was cancelled.",
        "i apologize the program was canceled",
    );
    whisper(
        "Aluminium tyres and grey cheques.",
        "aluminum tires and gray checks",
    );
}

// ---------------------------------------------------------------- percent

#[test]
fn whisper_percent_digits_and_words_agree() {
    whisper("25 percent", "25%");
    whisper("twenty five percent", "25%");
    whisper("twenty-five per cent", "25%");
    whisper("25 per cent", "25%");
    whisper(
        "I'm 25 per cent sure it costs twenty five per cent more.",
        "i am 25% sure it costs 25% more",
    );
    whisper("It rose 3 per cent this year.", "it rose 3% this year");
    whisper("three and a half percent", "3.5%");
    whisper("3.0 percent", "3%");
    whisper("fifty percent", "50%");
}

// ---------------------------------------------------------------- currency

#[test]
fn whisper_currency() {
    whisper("five dollars", "$5");
    whisper("$5", "$5");
    whisper("5 dollars", "$5");
    whisper("$20 million", "$20000000");
    whisper("twenty million dollars", "$20000000");
    whisper("two dollars and seven cents", "$2.07");
    whisper("two dollars seven cents", "$2.07");
    whisper("$2.07", "$2.07");
    whisper("ten thousand dollars and fifty cents", "$10000.50");
    whisper("fifty cents", "¢50");
    whisper("$0.50", "¢50");
    whisper("$0.05 and $0.5", "¢5 and ¢5");
    whisper("three pounds fifty", "£3 50");
    whisper("£3.50", "£3.50");
    whisper("5 pounds and 20 pence", "£5 and 20 pence");
    whisper("ten euros", "€10");
    whisper("€10", "€10");
    whisper("5 euros", "€5");
    whisper("pounds", "pounds");
    whisper("it costs $", "it costs $");
    whisper("$3.50 and 20%", "$3.50 and 20%");
}

// ---------------------------------------------------------------- plurals, oh, double

#[test]
fn whisper_plurals() {
    whisper("the 1960s", "the 1960s");
    whisper("the nineteen sixties", "the 1960s");
    whisper("the twenties", "the 20s");
    whisper("in the 90s", "in the 90s");
    whisper("the eighties and nineties", "the 80s and 90s");
    whisper("twos and threes", "2s and 3s");
    whisper("sixes and sevens", "6s and 7s");
    whisper("five hundreds", "500s");
    whisper("a 1s and 0s", "a ones and 0s");
}

#[test]
fn whisper_oh_double_triple() {
    whisper("one oh one", "101");
    whisper("one oh oh", "100");
    whisper("double oh seven", "007");
    whisper("double oh", "00");
    whisper("triple five", "555");
    whisper("one double two", "122");
    whisper("double you", "double you");
    whisper("oh no", "0 no");
    whisper("o", "0");
    whisper("0 oh", "0");
    whisper("four o'clock", "40 clock");
}

// ---------------------------------------------------------------- ordinals

#[test]
fn whisper_ordinals() {
    whisper("the first time", "the 1st time");
    whisper("the twenty first century", "the 21st century");
    whisper("the 21st century", "the 21st century");
    whisper("wait a second", "wait a 2nd");
    whisper("the second one", "the 2nd one");
    whisper("he was first, she was second.", "he was 1st she was 2nd");
    whisper("one hundred and first", "101st");
    // Whisper builds "nine" + "th" = "nineth", so the real spelling is left alone.
    whisper("the ninth", "the ninth");
    whisper("nineteenth", "19th");
    whisper("the fifth and twelfth", "the 5th and 12th");
    whisper("the hundredth", "the 100th");
    whisper("two thousandths", "2 thousandths");
    whisper("the 3rd and 4th", "the 3rd and 4th");
    whisper("2nd", "2nd");
}

// ---------------------------------------------------------------- years and cardinals

#[test]
fn whisper_years_and_cardinals() {
    whisper("in nineteen ninety nine", "in 1999");
    whisper("in 1999", "in 1999");
    whisper("two thousand and twenty four", "2024");
    whisper("twenty twenty four", "2024");
    whisper("fifty fifty", "5050");
    whisper("twelve thirty", "1230");
    whisper("one two three four", "1234");
    whisper("twenty five thousand three hundred and forty two", "25342");
    whisper("nine hundred ninety nine thousand", "999000");
    whisper("seven billion people", "7000000000 people");
    whisper("a million thousand", "a 1000000");
    whisper("hundred thousand", "100000");
    whisper("a hundred", "a 100");
    whisper("12 3 hundred", "12 300");
    whisper("ten to five", "10 to 5");
    whisper("it's twenty past three", "it is 20 past 3");
    whisper("the 2 of us", "the 2 of us");
    whisper("zero", "0");
}

#[test]
fn whisper_one_is_written_out_when_alone() {
    whisper("one", "one");
    whisper("one of the ones", "one of the ones");
    whisper("1 apple", "one apple");
}

// ---------------------------------------------------------------- decimals, halves, signs

#[test]
fn whisper_decimals_halves_and_signs() {
    whisper("one point five", "1.5");
    whisper("three point one four", "3.14");
    whisper("one point five million people", "1500000 people");
    whisper("two point five billion", "2500000000");
    whisper("1.5 million", "1500000");
    whisper("one point two point three million", "1.2.3 1000000");
    whisper("point", "point");
    whisper("point five", ".5");
    whisper("point point five", ".5");
    whisper("two point", "2 point");
    whisper("one and a half", "1.5");
    whisper("two and a half million", "2500000");
    whisper("a dozen and a half eggs", "a dozen and a half eggs");
    whisper("and a half", "");
    whisper("minus five", "-5");
    whisper("negative 3", "-3");
    whisper("plus two", "+2");
    whisper("minus and", "-and");
    whisper("version 2.0 and 1.50", "version 2 and 1.50");
    whisper("007", "7");
    whisper("05", "5");
    whisper("2.00", "2");
    whisper(".5", ".5");
}

// ---------------------------------------------------------------- digits and letters

#[test]
fn whisper_digit_letter_boundaries_and_commas() {
    whisper("it's 9am at 5pm", "it is 9 am at 5 pm");
    whisper("mp3 player", "mp 3 player");
    whisper("1,000 people", "1000 people");
    whisper("1,234,567", "1234567");
    // non-overlapping `(\d),(\d)`: the middle digit is consumed by the first match
    whisper("1,2,3", "12 3");
    whisper("...5 and ..5", " .5 and .5");
}

// ---------------------------------------------------------------- contractions, symbols

#[test]
fn whisper_contractions_and_titles() {
    whisper(
        "I won't, I can't and I'm sure they've gone.",
        "i will not i can not and i am sure they have gone",
    );
    whisper(
        "Mr. Smith's dog's been barking.",
        "mister smith is dog has been barking",
    );
    whisper(
        "I'd been there and he'd done it, she'd go.",
        "i had been there and he had done it she would go",
    );
}

#[test]
fn whisper_unattached_symbols() {
    whisper("a $ sign and % alone", "a sign and alone");
    whisper("50%%", "50% ");
    whisper("100 %", "100 ");
    whisper("a%% b", "a % b");
    whisper("..a", " a");
    whisper("caf&eacute; naïve résumé", "caf eacute naive resume");
}
